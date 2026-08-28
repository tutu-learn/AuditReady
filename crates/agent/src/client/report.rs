use super::file_scan::{ChangedFile, FolderWrite};
use super::sensitive::SensitiveFinding;
use crate::models::DiskEntry;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::time::Duration;

/// Report payload sent to the AuditReady server.
///
/// Matches the schema expected by `POST /audit_ready/client-report`.
#[derive(Debug, Serialize)]
pub struct ClientReport {
    pub hostname: String,
    pub username: String,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    /// Mounted disks with total/available space at report time.
    pub disks: Vec<DiskEntry>,
    pub changed_files: Vec<ChangedFile>,
    pub clipboard_events: Vec<ClipboardEventReport>,
    /// Processes running at report time, busiest by CPU first (capped).
    pub running_processes: Vec<RunningProcessReport>,
    /// Per-folder write activity over the period, busiest first.
    pub folder_writes: Vec<FolderWrite>,
    pub total_copy_bytes: u64,
    pub sensitive_hits: u64,
}

/// One running process in the wire format. `cpu_percent`/`memory_bytes` are
/// null when the platform cannot provide them.
#[derive(Debug, Serialize)]
pub struct RunningProcessReport {
    pub name: String,
    pub pid: u32,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

/// One clipboard event in the wire format. `content` is `null` unless the
/// event was at or above the configured size threshold.
#[derive(Debug, Serialize)]
pub struct ClipboardEventReport {
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub app: Option<String>,
    pub size_bytes: u64,
    pub sensitive: Vec<SensitiveFinding>,
    pub content: Option<String>,
}

/// POST the report to `{scheme}://{domain}/audit_ready/client-report`.
///
/// If `token` is supplied, it is sent as `Authorization: Bearer <token>`.
/// Any 2xx response counts as success; non-2xx is an error (the caller
/// retries at the next interval, there is no local queue).
pub fn post(endpoint: &str, token: Option<&str>, payload: &ClientReport) -> anyhow::Result<()> {
    let body = serde_json::to_string(payload)?;
    let mut request = ureq::post(endpoint)
        .timeout(Duration::from_secs(60))
        .set("Content-Type", "application/json")
        .set("User-Agent", "AuditReady/0.1");

    if let Some(t) = token.filter(|t| !t.is_empty()) {
        request = request.set("Authorization", &format!("Bearer {}", t));
    }

    let response = request.send_string(&body)?;

    let status = response.status();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(anyhow::anyhow!("endpoint returned HTTP {}", status))
    }
}

/// Same scheme/local rules as `publisher::build_endpoint` (kept as a small
/// local copy; publisher's stays private): HTTP for localhost/loopback,
/// HTTPS otherwise, path appended.
pub fn build_endpoint(domain: &str) -> String {
    let domain = domain.trim();
    if domain.starts_with("http://") || domain.starts_with("https://") {
        let base = domain.trim_end_matches('/');
        format!("{}/audit_ready/client-report", base)
    } else {
        let is_local =
            domain.starts_with("localhost") || domain.starts_with("127.") || domain == "::1";
        let scheme = if is_local { "http" } else { "https" };
        format!("{}://{}/audit_ready/client-report", scheme, domain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_endpoint_from_domain() {
        assert_eq!(
            build_endpoint("localhost:8000"),
            "http://localhost:8000/audit_ready/client-report"
        );
        assert_eq!(
            build_endpoint("api.example.com"),
            "https://api.example.com/audit_ready/client-report"
        );
        assert_eq!(
            build_endpoint("https://api.example.com/"),
            "https://api.example.com/audit_ready/client-report"
        );
    }

    #[test]
    fn payload_serializes_wire_format() {
        let period_start = DateTime::parse_from_rfc3339("2026-08-21T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let period_end = DateTime::parse_from_rfc3339("2026-08-21T08:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let event_ts = DateTime::parse_from_rfc3339("2026-08-21T08:05:12Z")
            .unwrap()
            .with_timezone(&Utc);

        let payload = ClientReport {
            hostname: "DESKTOP-ABC123".to_string(),
            username: "johann".to_string(),
            period_start,
            period_end,
            disks: vec![DiskEntry {
                mount_point: "C:\\".to_string(),
                name: "Windows".to_string(),
                total_bytes: 512_110_190_592,
                available_bytes: 220_200_960_000,
            }],
            changed_files: vec![ChangedFile {
                path: "C:\\Users\\johann\\Documents\\report.docx".to_string(),
                size_bytes: 48211,
                modified_at: event_ts,
            }],
            clipboard_events: vec![
                // Below the threshold: content is null.
                ClipboardEventReport {
                    ts: event_ts,
                    kind: "copy".to_string(),
                    app: Some("chrome.exe".to_string()),
                    size_bytes: 342,
                    sensitive: vec![SensitiveFinding {
                        kind: "credit_card".to_string(),
                        masked: "41**********11".to_string(),
                    }],
                    content: None,
                },
                // Above the threshold: content is present.
                ClipboardEventReport {
                    ts: event_ts,
                    kind: "copy".to_string(),
                    app: Some("EXCEL.EXE".to_string()),
                    size_bytes: 61204,
                    sensitive: vec![],
                    content: Some("big clipboard text".to_string()),
                },
            ],
            total_copy_bytes: 61546,
            sensitive_hits: 1,
            running_processes: vec![RunningProcessReport {
                name: "chrome.exe".to_string(),
                pid: 1234,
                cpu_percent: Some(2.5),
                memory_bytes: Some(524_288_000),
            }],
            folder_writes: vec![FolderWrite {
                folder: "C:\\Users\\johann\\Documents".to_string(),
                write_count: 42,
                last_write_at: event_ts,
            }],
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["hostname"], "DESKTOP-ABC123");
        assert_eq!(json["username"], "johann");
        assert_eq!(json["period_start"], "2026-08-21T08:00:00Z");
        assert_eq!(json["period_end"], "2026-08-21T08:30:00Z");
        assert_eq!(json["disks"][0]["mount_point"], "C:\\");
        assert_eq!(json["disks"][0]["total_bytes"], 512_110_190_592i64);
        assert_eq!(json["disks"][0]["available_bytes"], 220_200_960_000i64);
        assert_eq!(json["changed_files"][0]["size_bytes"], 48211);
        assert_eq!(
            json["changed_files"][0]["modified_at"],
            "2026-08-21T08:05:12Z"
        );

        let small = &json["clipboard_events"][0];
        assert_eq!(small["kind"], "copy");
        assert_eq!(small["app"], "chrome.exe");
        assert_eq!(small["sensitive"][0]["kind"], "credit_card");
        assert_eq!(small["sensitive"][0]["masked"], "41**********11");
        // Present in the JSON, explicitly null (content below threshold).
        assert!(small.get("content").is_some());
        assert!(small["content"].is_null());

        let big = &json["clipboard_events"][1];
        assert_eq!(big["content"], "big clipboard text");

        assert_eq!(json["total_copy_bytes"], 61546);
        assert_eq!(json["sensitive_hits"], 1);

        assert_eq!(json["running_processes"][0]["name"], "chrome.exe");
        assert_eq!(json["running_processes"][0]["pid"], 1234);
        assert_eq!(json["running_processes"][0]["cpu_percent"], 2.5);
        assert_eq!(
            json["running_processes"][0]["memory_bytes"],
            524_288_000i64
        );
        assert_eq!(
            json["folder_writes"][0]["folder"],
            "C:\\Users\\johann\\Documents"
        );
        assert_eq!(json["folder_writes"][0]["write_count"], 42);
        assert_eq!(
            json["folder_writes"][0]["last_write_at"],
            "2026-08-21T08:05:12Z"
        );
    }
}
