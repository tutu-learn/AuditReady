use crate::collector;
use crate::models::AuditReport;
use crate::network_monitor::{self, NetworkSnapshot};
use crate::process_monitor::{self, ProcessEvent, RulesEngine, Verdict};
use chrono::Utc;
use serde::Serialize;
use std::thread;
use std::time::Duration;

/// Telemetry payload sent to the AuditReady server.
///
/// Matches the schema expected by `POST /audit_ready/telemetry`.
#[derive(Serialize)]
pub struct TelemetryPayload {
    pub hostname: String,
    pub os_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_software: Option<InstalledSoftware>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compliance: Option<CompliancePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_processes: Option<RunningProcessesPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_traffic: Option<NetworkTrafficPayload>,
}

#[derive(Serialize)]
pub struct InstalledSoftware {
    pub packages: Vec<SoftwarePackage>,
}

#[derive(Serialize)]
pub struct SoftwarePackage {
    pub name: String,
    pub version: String,
    pub source: String,
}

#[derive(Serialize)]
pub struct CompliancePayload {
    pub pass: usize,
    pub fail: usize,
    pub warn: usize,
    pub checks: Vec<ComplianceCheckPayload>,
}

#[derive(Serialize)]
pub struct ComplianceCheckPayload {
    pub name: String,
    pub status: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct RunningProcessesPayload {
    pub total: usize,
    pub flagged: usize,
    pub killed: usize,
    pub processes: Vec<ProcessPayload>,
}

#[derive(Serialize)]
pub struct ProcessPayload {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub elevated: bool,
    pub verdict: String,
    pub command: String,
}

#[derive(Serialize)]
pub struct NetworkTrafficPayload {
    pub interfaces: Vec<NetworkInterfacePayload>,
    pub connections: Vec<NetworkConnectionPayload>,
    pub dns_servers: Vec<String>,
}

#[derive(Serialize)]
pub struct NetworkInterfacePayload {
    pub name: String,
    pub addresses: Vec<String>,
}

#[derive(Serialize)]
pub struct NetworkConnectionPayload {
    pub proto: String,
    pub local: String,
    pub remote: String,
    pub state: String,
}

/// Continuously collect audit data and POST it to the configured endpoint every `interval_secs`.
///
/// `domain` is the host (and optional port) only; the full path `/audit_ready/telemetry` is appended.
/// If `token` is supplied, it is sent as `Authorization: Bearer <token>`.
pub fn run(domain: &str, interval_secs: u64, token: Option<&str>) -> anyhow::Result<()> {
    let engine = RulesEngine::new(
        vec![
            "launchd".into(),
            "kernel_task".into(),
            "svchost.exe".into(),
        ],
        vec![],
    );

    let dns_capture = network_monitor::DnsCapture::start();
    let endpoint = build_endpoint(domain);

    loop {
        match push_snapshot(&engine, &endpoint, token, &dns_capture) {
            Ok(()) => println!("[{}] Telemetry posted", Utc::now().format("%Y-%m-%d %H:%M:%S UTC")),
            Err(e) => eprintln!("[{}] Telemetry failed: {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"), e),
        }
        thread::sleep(Duration::from_secs(interval_secs));
    }
}

fn build_endpoint(domain: &str) -> String {
    let domain = domain.trim();
    if domain.starts_with("http://") || domain.starts_with("https://") {
        let base = domain.trim_end_matches('/');
        format!("{}/audit_ready/telemetry", base)
    } else {
        // Use HTTP for localhost/loopback to match the documented dev endpoint;
        // HTTPS otherwise.
        let is_local = domain.starts_with("localhost")
            || domain.starts_with("127.")
            || domain == "::1";
        let scheme = if is_local { "http" } else { "https" };
        format!("{}://{}/audit_ready/telemetry", scheme, domain)
    }
}

fn push_snapshot(
    engine: &RulesEngine,
    endpoint: &str,
    token: Option<&str>,
    dns_capture: &network_monitor::DnsCapture,
) -> anyhow::Result<()> {
    let report = collector::collect()?;
    let processes = process_monitor::snapshot(engine);
    let network = network_monitor::snapshot(Some(dns_capture));

    let payload = build_payload(report, processes, network);
    let body = serde_json::to_string(&payload)?;
    let mut request = ureq::post(endpoint)
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

fn build_payload(
    report: AuditReport,
    processes: Vec<ProcessEvent>,
    network: NetworkSnapshot,
) -> TelemetryPayload {
    let total = processes.len();
    let flagged = processes.iter().filter(|p| p.verdict == Verdict::Flagged).count();
    let killed = processes.iter().filter(|p| p.verdict == Verdict::Blacklisted).count();

    TelemetryPayload {
        hostname: report.hostname,
        os_version: report.os_version,
        installed_software: Some(InstalledSoftware {
            packages: report
                .software
                .into_iter()
                .map(|s| SoftwarePackage {
                    name: s.name,
                    version: s.version.unwrap_or_default(),
                    source: s.source,
                })
                .collect(),
        }),
        compliance: None,
        running_processes: Some(RunningProcessesPayload {
            total,
            flagged,
            killed,
            processes: processes
                .into_iter()
                .filter(|p| p.verdict == Verdict::Flagged)
                .map(|p| ProcessPayload {
                    pid: p.pid,
                    ppid: p.ppid.unwrap_or(0),
                    name: p.name,
                    elevated: p.is_elevated,
                    verdict: format_verdict(p.verdict),
                    command: p.cmdline.join(" "),
                })
                .collect(),
        }),
        network_traffic: Some(NetworkTrafficPayload {
            interfaces: network
                .interfaces
                .into_iter()
                .map(|i| NetworkInterfacePayload {
                    name: i.name,
                    addresses: i.ips,
                })
                .collect(),
            connections: network
                .connections
                .into_iter()
                .map(|c| NetworkConnectionPayload {
                    proto: c.protocol,
                    local: c.local_addr,
                    remote: c.remote_addr.unwrap_or_default(),
                    state: c.state.unwrap_or_default(),
                })
                .collect(),
            dns_servers: network.dns_servers,
        }),
    }
}

fn format_verdict(verdict: Verdict) -> String {
    match verdict {
        Verdict::Allowed => "OK".to_string(),
        Verdict::Flagged => "FLAGGED".to_string(),
        Verdict::Blacklisted => "KILLED".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_endpoint_from_domain() {
        assert_eq!(
            build_endpoint("localhost:8000"),
            "http://localhost:8000/audit_ready/telemetry"
        );
        assert_eq!(
            build_endpoint("api.example.com"),
            "https://api.example.com/audit_ready/telemetry"
        );
    }

    #[test]
    fn build_endpoint_from_full_url() {
        assert_eq!(
            build_endpoint("https://api.example.com/"),
            "https://api.example.com/audit_ready/telemetry"
        );
        assert_eq!(
            build_endpoint("http://localhost:8000/other"),
            "http://localhost:8000/other/audit_ready/telemetry"
        );
    }
}
