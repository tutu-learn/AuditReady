mod client;
mod cmd;
mod collector;
mod config;
mod iis;
mod jobs;
mod models;
mod network_monitor;
mod pending_updates;
mod process_monitor;
mod publisher;
mod tunnel;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();

    // Parse a subset of CLI arguments manually to avoid adding a dependency.
    let mut config_path: Option<String> = None;
    let mut mode_flag: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--config" && i + 1 < args.len() {
            config_path = Some(args[i + 1].clone());
            i += 2;
        } else if args[i] == "--mode" && i + 1 < args.len() {
            mode_flag = Some(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }

    // Load appsettings.json if present; otherwise use defaults.
    // A --config argument overrides the default file path.
    // Environment variables override config file values.
    let mut settings = match config_path {
        Some(path) if std::path::Path::new(&path).exists() => config::load(&path)?,
        Some(_) => config::AppSettings::default(),
        None if std::path::Path::new("appsettings.json").exists() => {
            config::load("appsettings.json")?
        }
        None => config::AppSettings::default(),
    };
    settings.apply_env_overrides();
    // Mode precedence: --mode flag beats AUDITREADY_MODE beats the config file.
    let mode = mode_flag
        .or(settings.mode.clone())
        .unwrap_or_else(|| "agent".to_string());
    if mode != "agent" && mode != "client" {
        tracing::warn!("unknown mode '{}', expected 'agent' or 'client'", mode);
    }

    // --print-network: collect and print the network snapshot as JSON, then exit
    if args.iter().any(|a| a == "--print-network") {
        let snapshot = network_monitor::snapshot(None);
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    // --print-dns: capture live DNS traffic for a few seconds, print the
    // captured queries as JSON, then exit. Requires root (packet capture).
    if args.iter().any(|a| a == "--print-dns") {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(async {
            let capture = network_monitor::DnsCapture::start();
            // Give the capture thread a moment to attach before traffic arrives.
            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("Capturing DNS traffic for 10 seconds (generate some lookups)...");
            tokio::time::sleep(Duration::from_secs(10)).await;
            let queries = capture.drain();
            println!("{}", serde_json::to_string_pretty(&queries)?);
            Ok(())
        });
    }

    // --software: print the full inventory and exit
    if args.iter().any(|a| a == "--software") {
        let report = collector::collect()?;
        println!("AuditReady — Installed Software");
        println!("Host    : {}", report.hostname);
        println!("OS      : {} {}", report.os, report.os_version);
        println!("Scanned : {}", report.scanned_at.format("%Y-%m-%d %H:%M:%S UTC"));
        println!("Total   : {} packages", report.software_count);
        println!("{}", "─".repeat(72));
        println!("Disks");
        for d in &report.disks {
            println!(
                "{:<30} {:>10.1} GiB free of {:.1} GiB  {}",
                truncate(&d.mount_point, 29),
                d.available_bytes as f64 / (1 << 30) as f64,
                d.total_bytes as f64 / (1 << 30) as f64,
                d.name
            );
        }
        println!("{}", "─".repeat(72));
        println!("{:<45} {:<20} Source", "Name", "Version");
        println!("{}", "─".repeat(72));
        for sw in &report.software {
            println!(
                "{:<45} {:<20} {}",
                truncate(&sw.name, 44),
                sw.version.as_deref().unwrap_or("—"),
                sw.source
            );
        }
        println!("{}", "─".repeat(72));
        return Ok(());
    }

    // --print-updates: collect and print pending OS updates as JSON, then exit
    if args.iter().any(|a| a == "--print-updates") {
        let updates = pending_updates::collect()?;
        println!("{}", serde_json::to_string_pretty(&updates)?);
        return Ok(());
    }

    // Client mode on Windows runs as a per-user background monitor started
    // by Task Scheduler, which gives the console app its own window on the
    // user's desktop. Closing that window kills the agent (CTRL_CLOSE_EVENT),
    // silently stopping monitoring. Detach when we own the console so the
    // window closes itself at startup; when started from an existing terminal
    // (shared console) stay attached so debug output remains visible.
    #[cfg(windows)]
    if mode == "client" {
        detach_owned_console();
    }

    // Everything from here on needs an async runtime to spawn tasks, but the
    // tray icon (below) must run on the real OS main thread with no Tokio
    // runtime entered on it — iced's own (tokio-backed) executor panics with
    // "Cannot start a runtime from within a runtime" otherwise. So: drive
    // setup and all background tasks to completion inside `block_on`, then
    // drop back to this bare thread for the UI. `rt` is kept alive across
    // that so the already-spawned background tasks keep running.
    let rt = tokio::runtime::Runtime::new()?;
    let client_stats = rt.block_on(async_setup(settings, mode))?;

    // Client mode: run the tray icon + stats dashboard on this thread (the
    // real process main thread, required by the tray icon on macOS). This
    // blocks until the user quits from the tray menu.
    if let Some(client_stats) = client_stats {
        if let Err(e) = client::ui::run(client_stats) {
            tracing::error!("client tray/dashboard UI failed: {}", e);
        }
    }

    Ok(())
}

/// Resolves config, spawns every background task (telemetry, patch jobs,
/// tunnel, client-mode monitors, network refresh), and returns the client
/// stats handle for the tray UI — or blocks forever for agent mode, which
/// has no UI of its own. Must run inside a Tokio runtime.
async fn async_setup(
    settings: config::AppSettings,
    mode: String,
) -> Result<Option<client::stats::SharedStats>> {
    // Shared backend config is required for either push or tunnel.
    let domain = settings
        .server
        .domain
        .clone()
        .ok_or_else(|| anyhow::anyhow!("server.domain is not configured"))?;
    let token = settings
        .server
        .token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("server.token is not configured"))?;

    // Shared cache of pending OS updates, refreshed in the background and
    // reported with every telemetry snapshot.
    let pending_cache = Arc::new(pending_updates::PendingUpdatesCache::new());
    {
        let cache = pending_cache.clone();
        std::thread::spawn(move || pending_updates::run_refresher(cache));
    }

    // Shared cache of the IIS website inventory (Windows only), refreshed in
    // the background and reported with every telemetry snapshot.
    let iis_cache = Arc::new(iis::IisCache::new());
    {
        let cache = iis_cache.clone();
        std::thread::spawn(move || iis::run_refresher(cache));
    }

    // Client mode gets a shared stats handle feeding the tray/dashboard UI
    // and connection-lost/restored desktop notifications; agent mode has no
    // UI to feed, so it stays None.
    let client_stats = if mode == "client" {
        Some(client::stats::new_shared())
    } else {
        None
    };

    // Push telemetry. Runs in a blocking task because publisher::run is
    // synchronous and loops forever.
    let push_interval = settings.server.interval_seconds;
    let push_domain = domain.clone();
    let push_token = token.clone();
    let push_cache = pending_cache.clone();
    let push_iis = iis_cache.clone();
    let push_stats = client_stats.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = publisher::run(&push_domain, push_interval, Some(&push_token), push_cache, push_iis, push_stats) {
            tracing::error!("telemetry publisher failed: {}", e);
        }
    });

    // Poll for and execute patch jobs (package/OS updates queued by the
    // server). Synchronous loop on its own blocking task.
    let job_domain = domain.clone();
    let job_token = token.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = jobs::run(&job_domain, &job_token) {
            tracing::error!("patch job poller failed: {}", e);
        }
    });

    // Remote shell tunnel if enabled. Dials out to the broker and serves channels.
    if settings.server.tunnel_enabled {
        let broker_url = settings
            .broker_url()
            .ok_or_else(|| anyhow::anyhow!("tunnel is enabled but server.domain is not configured"))?;
        tokio::spawn(tunnel::run(
            broker_url,
            token.clone(),
            settings.server.tunnel_shell,
            settings.server.tunnel_cwd,
        ));
    }

    // Client mode: additionally monitor user file changes and clipboard
    // activity, reporting to /audit_ready/client-report. Runs as a separate
    // per-user instance (logon task / LaunchAgent); telemetry keeps working.
    if mode == "client" {
        let client_settings = settings.client.clone();
        let client_domain = domain.clone();
        let client_token = token.clone();
        let client_stats = client_stats.clone().expect("client_stats set for client mode");
        tokio::task::spawn_blocking(move || {
            if let Err(e) = client::run(&client_settings, &client_domain, &client_token, client_stats) {
                tracing::error!("client mode failed: {}", e);
            }
        });
    }

    // Background network-state refresh, common to both modes.
    tokio::spawn(async {
        let dns_capture = network_monitor::DnsCapture::start();
        loop {
            let _ = network_monitor::snapshot(Some(&dns_capture));
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // Client mode: the tray icon + stats dashboard is run by the caller, on
    // the real process main thread (required on macOS). Hand back the stats
    // handle instead of driving it here.
    if mode == "client" {
        return Ok(client_stats);
    }

    // Agent mode: no UI, just keep the process (and this runtime) alive.
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// Detach from the console only when this process is the sole process
/// attached to it — i.e. the console was created for us (Task Scheduler
/// launch) and its window would otherwise sit on the user's desktop.
#[cfg(windows)]
fn detach_owned_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    unsafe {
        let mut list = [0u32; 2];
        let count = GetConsoleProcessList(&mut list);
        if count == 1 {
            let _ = FreeConsole();
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
