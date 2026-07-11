mod collector;
mod config;
mod models;
mod network_monitor;
mod process_monitor;
mod publisher;

use anyhow::Result;
use std::time::Duration;

fn main() -> Result<()> {
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

    // Push telemetry if enabled in appsettings.json. Domain, interval, and token
    // are all read from config; no CLI flags override these values.
    if settings.push.enabled {
        let domain = settings
            .push
            .domain
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("push is enabled but push.domain is missing"))?;
        return publisher::run(domain, settings.push.interval_seconds, settings.push.token.as_deref());
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
        println!("{:<45} {:<20} {}", "Name", "Version", "Source");
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

    // Default: silent mode. Telemetry is only emitted when push.enabled is true.
    let dns_capture = network_monitor::DnsCapture::start();
    loop {
        let _ = network_monitor::snapshot(Some(&dns_capture));
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
