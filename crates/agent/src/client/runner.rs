use super::clipboard::{self, EventKind};
use super::report::{self, ClientReport, ClipboardEventReport};
use super::{file_scan, sensitive};
use crate::config::ClientSettings;
use chrono::Utc;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime};

/// Run client mode: monitor the clipboard, scan the user's files, and POST a
/// report every `report_interval_seconds`. Loops forever; errors are logged
/// and the loop continues.
pub fn run(settings: &ClientSettings, domain: &str, token: &str) -> anyhow::Result<()> {
    let events = clipboard::start();
    let endpoint = report::build_endpoint(domain);
    let scan_root = settings
        .scan_root
        .as_deref()
        .map(PathBuf::from)
        .or_else(home_dir);
    if scan_root.is_none() {
        tracing::warn!("client mode: no scan_root configured and no home directory found");
    }

    let mut watermark = SystemTime::now();
    let mut period_start = Utc::now();

    loop {
        thread::sleep(Duration::from_secs(settings.report_interval_seconds));

        let period_end = Utc::now();

        // Drain clipboard events collected during the period.
        let mut total_copy_bytes = 0u64;
        let mut sensitive_hits = 0u64;
        let mut clipboard_events = Vec::new();
        for event in events.try_iter() {
            let findings = event
                .text
                .as_deref()
                .map(sensitive::scan)
                .unwrap_or_default();
            sensitive_hits += findings.len() as u64;
            if event.kind == EventKind::Copy {
                total_copy_bytes += event.size_bytes;
            }
            // Content only leaves the machine at or above the size threshold.
            let content = if event.size_bytes >= settings.clipboard_content_threshold_bytes {
                event
                    .text
                    .map(|t| truncate_utf8(&t, settings.clipboard_content_max_bytes as usize))
            } else {
                None
            };
            clipboard_events.push(ClipboardEventReport {
                ts: event.ts,
                kind: match event.kind {
                    EventKind::Copy => "copy".to_string(),
                    EventKind::Paste => "paste".to_string(),
                },
                app: event.app,
                size_bytes: event.size_bytes,
                sensitive: findings,
                content,
            });
        }

        // Files changed since the last report.
        let changed_files = scan_root
            .as_deref()
            .map(|root| file_scan::changed_since(root, watermark, &settings.excluded_dirs))
            .unwrap_or_default();
        watermark = SystemTime::now();

        let payload = ClientReport {
            hostname: hostname(),
            username: username(),
            period_start,
            period_end,
            disks: crate::collector::disks(),
            changed_files,
            clipboard_events,
            total_copy_bytes,
            sensitive_hits,
        };
        period_start = period_end;

        match report::post(&endpoint, Some(token), &payload) {
            Ok(()) => println!(
                "[{}] Client report posted",
                Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
            ),
            Err(e) => eprintln!(
                "[{}] Client report failed: {}",
                Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                e
            ),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn username() -> String {
    for var in ["USER", "LOGNAME", "USERNAME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    "unknown".to_string()
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string())
        .trim()
        .to_string()
}

fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
