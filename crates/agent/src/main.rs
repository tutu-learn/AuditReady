mod collector;
mod config;
mod models;
mod network_monitor;
mod process_monitor;
mod publisher;
mod tunnel;

use anyhow::Result;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();

    // Load appsettings.json if present; otherwise use defaults.
    // Environment variables override config file values.
    let mut settings = if std::path::Path::new("appsettings.json").exists() {
        config::load("appsettings.json")?
    } else {
        config::AppSettings::default()
    };
    settings.apply_env_overrides();

    // --print-network: collect and print the network snapshot as JSON, then exit
    if args.iter().any(|a| a == "--print-network") {
        let snapshot = network_monitor::snapshot(None);
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
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

    // Push telemetry. Runs in a blocking task because publisher::run is
    // synchronous and loops forever.
    let push_interval = settings.server.interval_seconds;
    let push_domain = domain.clone();
    let push_token = token.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = publisher::run(&push_domain, push_interval, Some(&push_token)) {
            tracing::error!("telemetry publisher failed: {}", e);
        }
    });

    // Remote shell tunnel if enabled. Dials out to the broker and serves channels.
    if settings.server.tunnel_enabled {
        let broker_url = settings
            .broker_url()
            .ok_or_else(|| anyhow::anyhow!("tunnel is enabled but server.domain is not configured"))?;
        tokio::spawn(tunnel::run(broker_url, token, settings.server.tunnel_shell));
    }

    // Default: silent mode. Keep the process alive and refresh network state.
    let dns_capture = network_monitor::DnsCapture::start();
    loop {
        let _ = network_monitor::snapshot(Some(&dns_capture));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
