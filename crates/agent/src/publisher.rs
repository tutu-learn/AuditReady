use crate::collector;
use crate::models::AuditReport;
use crate::network_monitor::{self, NetworkSnapshot};
use crate::pending_updates::{PendingUpdate, PendingUpdatesCache};
use crate::process_monitor::{self, ProcessEvent, RulesEngine, Verdict};
use chrono::Utc;
use serde::Serialize;
use std::sync::Arc;
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
    pub pending_updates: Option<Vec<PendingUpdate>>,
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
    pub dns_queries: Vec<network_monitor::DnsQuery>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// Max connections reported per snapshot; the server keeps the newest 200.
const MAX_CONNECTIONS: usize = 200;

/// Continuously collect audit data and POST it to the configured endpoint every `interval_secs`.
///
/// `domain` is the host (and optional port) only; the full path `/audit_ready/telemetry` is appended.
/// If `token` is supplied, it is sent as `Authorization: Bearer <token>`.
/// `pending_updates` carries the cached pending-update snapshot; until the
/// first successful collection it holds `None` and the field is omitted.
pub fn run(
    domain: &str,
    interval_secs: u64,
    token: Option<&str>,
    pending_updates: Arc<PendingUpdatesCache>,
) -> anyhow::Result<()> {
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
        match push_snapshot(&engine, &endpoint, token, &dns_capture, &pending_updates) {
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
    pending_updates: &PendingUpdatesCache,
) -> anyhow::Result<()> {
    let report = collector::collect()?;
    let processes = process_monitor::snapshot(engine);
    let network = network_monitor::snapshot(Some(dns_capture));

    let mut payload = build_payload(report, processes, network);
    // Present (even empty) once the first collection succeeded; absent before
    // that, so the server keeps whatever snapshot it already has.
    payload.pending_updates = pending_updates.snapshot().map(|u| (*u).clone());
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
        pending_updates: None,
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
                .take(MAX_CONNECTIONS)
                .map(|c| {
                    let proto = c.protocol.to_lowercase();
                    // UDP is connectionless: report an empty state.
                    let state = if proto == "udp" {
                        String::new()
                    } else {
                        c.state.unwrap_or_default()
                    };
                    NetworkConnectionPayload {
                        proto,
                        local: c.local_addr,
                        remote: c.remote_addr.unwrap_or_default(),
                        state,
                        pid: c.pid,
                        process: c.process_name,
                    }
                })
                .collect(),
            dns_queries: network.dns_queries,
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

    #[test]
    fn payload_serializes_connection_attribution_and_dns_queries() {
        let report = AuditReport {
            hostname: "host1".to_string(),
            os: "linux".to_string(),
            os_version: "Ubuntu 22.04".to_string(),
            scanned_at: Utc::now(),
            software_count: 0,
            software: vec![],
        };
        let network = NetworkSnapshot {
            interfaces: vec![],
            connections: vec![
                network_monitor::NetworkConnection {
                    protocol: "TCP".to_string(),
                    local_addr: "192.168.1.5:51234".to_string(),
                    remote_addr: Some("142.250.72.14:443".to_string()),
                    state: Some("ESTAB".to_string()),
                    pid: Some(4821),
                    process_name: Some("firefox".to_string()),
                },
                network_monitor::NetworkConnection {
                    protocol: "UDP".to_string(),
                    local_addr: "192.168.1.5:55321".to_string(),
                    remote_addr: Some("192.168.1.1:53".to_string()),
                    state: Some("UNCONN".to_string()),
                    pid: None,
                    process_name: None,
                },
            ],
            dns_servers: vec!["192.168.1.1".to_string()],
            dns_queries: vec![network_monitor::DnsQuery {
                ts_ms: 1784307600000,
                domain: "example.com".to_string(),
                qtype: "A".to_string(),
                answers: vec!["93.184.216.34".to_string()],
                resolver: "192.168.1.1".to_string(),
                pid: None,
                process: None,
            }],
            captured_at: Utc::now(),
        };

        let payload = build_payload(report, vec![], network);
        let json = serde_json::to_value(&payload).unwrap();
        let nt = &json["network_traffic"];

        // TCP entry: lowercase proto, attribution present.
        let tcp = &nt["connections"][0];
        assert_eq!(tcp["proto"], "tcp");
        assert_eq!(tcp["state"], "ESTAB");
        assert_eq!(tcp["pid"], 4821);
        assert_eq!(tcp["process"], "firefox");

        // UDP entry: empty state, pid/process omitted entirely.
        let udp = &nt["connections"][1];
        assert_eq!(udp["proto"], "udp");
        assert_eq!(udp["state"], "");
        assert!(udp.get("pid").is_none());
        assert!(udp.get("process").is_none());

        // DNS query entry matches the wire schema.
        let q = &nt["dns_queries"][0];
        assert_eq!(q["ts_ms"], 1784307600000i64);
        assert_eq!(q["domain"], "example.com");
        assert_eq!(q["qtype"], "A");
        assert_eq!(q["answers"][0], "93.184.216.34");
        assert_eq!(q["resolver"], "192.168.1.1");
        assert!(q.get("pid").is_none());

        assert_eq!(nt["dns_servers"][0], "192.168.1.1");
    }

    #[test]
    fn payload_serializes_empty_dns_queries_when_capture_unavailable() {
        let report = AuditReport {
            hostname: "host1".to_string(),
            os: "linux".to_string(),
            os_version: "Ubuntu 22.04".to_string(),
            scanned_at: Utc::now(),
            software_count: 0,
            software: vec![],
        };
        let network = NetworkSnapshot {
            interfaces: vec![],
            connections: vec![],
            dns_servers: vec![],
            dns_queries: vec![],
            captured_at: Utc::now(),
        };
        let payload = build_payload(report, vec![], network);
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["network_traffic"]["dns_queries"], serde_json::json!([]));
    }

    #[test]
    fn payload_omits_pending_updates_until_first_collection() {
        let report = AuditReport {
            hostname: "host1".to_string(),
            os: "linux".to_string(),
            os_version: "Ubuntu 22.04".to_string(),
            scanned_at: Utc::now(),
            software_count: 0,
            software: vec![],
        };
        let network = NetworkSnapshot {
            interfaces: vec![],
            connections: vec![],
            dns_servers: vec![],
            dns_queries: vec![],
            captured_at: Utc::now(),
        };
        let mut payload = build_payload(report, vec![], network);
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("pending_updates").is_none());

        payload.pending_updates = Some(vec![PendingUpdate {
            id: "openssl".to_string(),
            title: "openssl 3.0.2-0ubuntu1.25".to_string(),
            severity: String::new(),
            source: "apt".to_string(),
            kb: String::new(),
        }]);
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["pending_updates"][0]["id"], "openssl");
        assert_eq!(json["pending_updates"][0]["source"], "apt");

        // An empty list is still serialized — it clears the server snapshot.
        payload.pending_updates = Some(vec![]);
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["pending_updates"], serde_json::json!([]));
    }

    #[test]
    fn payload_caps_connections_at_200() {
        let report = AuditReport {
            hostname: "host1".to_string(),
            os: "linux".to_string(),
            os_version: "Ubuntu 22.04".to_string(),
            scanned_at: Utc::now(),
            software_count: 0,
            software: vec![],
        };
        let connections = (0..250)
            .map(|i| network_monitor::NetworkConnection {
                protocol: "TCP".to_string(),
                local_addr: format!("10.0.0.1:{}", 10000 + i),
                remote_addr: None,
                state: Some("ESTAB".to_string()),
                pid: Some(1),
                process_name: Some("init".to_string()),
            })
            .collect();
        let network = NetworkSnapshot {
            interfaces: vec![],
            connections,
            dns_servers: vec![],
            dns_queries: vec![],
            captured_at: Utc::now(),
        };
        let payload = build_payload(report, vec![], network);
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json["network_traffic"]["connections"].as_array().unwrap().len(),
            MAX_CONNECTIONS
        );
    }
}
