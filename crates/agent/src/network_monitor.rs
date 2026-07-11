use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::Duration;

/// Point-in-time view of network state.
///
/// Live DNS query capture generally requires elevated privileges (root /
/// kernel-level packet capture, or access to system DNS logs). This module
/// captures what is available with the privileges it has and lets the remote
/// endpoint diff consecutive connection snapshots to detect new connections.
#[derive(Debug, Clone, Serialize)]
pub struct NetworkSnapshot {
    pub interfaces: Vec<NetworkInterface>,
    pub connections: Vec<NetworkConnection>,
    pub dns_servers: Vec<String>,
    pub dns_queries: Vec<DnsQuery>,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkInterface {
    pub name: String,
    pub ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkConnection {
    pub protocol: String,
    pub local_addr: String,
    pub remote_addr: Option<String>,
    pub state: Option<String>,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DnsQuery {
    pub timestamp: DateTime<Utc>,
    pub client: String,
    pub server: String,
    pub query: String,
    pub query_type: String,
    pub answers: Vec<String>,
}

/// Best-effort live DNS query capture.
///
/// - macOS / Linux: tries to run `tcpdump` on port 53 (requires root).
/// - Windows: polls `Get-DnsClientCache` for recently resolved names.
///
/// If capture cannot start, DNS queries are simply left empty. The capture keeps
/// a rolling window of the most recent queries.
#[derive(Clone)]
pub struct DnsCapture {
    queries: Arc<Mutex<Vec<DnsQuery>>>,
}

impl DnsCapture {
    pub fn start() -> Self {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let queries_clone = Arc::clone(&queries);
        std::thread::spawn(move || {
            let _ = platform::run_dns_capture(queries_clone);
        });
        Self { queries }
    }

    pub fn recent(&self) -> Vec<DnsQuery> {
        self.queries.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

pub fn snapshot(dns: Option<&DnsCapture>) -> NetworkSnapshot {
    NetworkSnapshot {
        interfaces: platform::interfaces(),
        connections: platform::connections(),
        dns_servers: platform::dns_servers(),
        dns_queries: dns.map(|d| d.recent()).unwrap_or_default(),
        captured_at: Utc::now(),
    }
}

// ── Shared DNS capture helpers ────────────────────────────────────────────────

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod tcpdump_dns {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    pub fn run_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        let mut child = Command::new("tcpdump")
            .args([
                "-i", "any",
                "-n",
                "-l",
                "-Q", "inout",
                "port", "53",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let reader = BufReader::new(stdout);

        for line in reader.lines().map_while(Result::ok) {
            if let Some(q) = parse_tcpdump_line(&line) {
                let mut guard = queries.lock().unwrap_or_else(|p| p.into_inner());
                guard.push(q);
                const MAX_QUERIES: usize = 1000;
                if guard.len() > MAX_QUERIES {
                    guard.remove(0);
                }
            }
        }

        Ok(())
    }

    /// Parse a single `tcpdump` line looking for DNS question records.
    ///
    /// Example:
    ///   `12:34:56.789123 IP 192.168.1.5.12345 > 192.168.1.1.53: 12345+ A? example.com. (32)`
    fn parse_tcpdump_line(line: &str) -> Option<DnsQuery> {
        if !line.contains(" > ") || !line.contains('?') {
            return None;
        }

        let query_marker = line.find('?')?;
        let before_query = &line[..query_marker];

        let arrow_idx = before_query.find(" > ")?;
        let client_part = before_query[..arrow_idx].rsplit(' ').next()?;
        let server_part = before_query[arrow_idx + 3..]
            .split(' ')
            .next()?
            .trim_end_matches(':')
            .to_string();

        let after_query = &line[query_marker + 1..];
        let name_end = after_query.find('(')?;
        let name = after_query[..name_end].trim().trim_end_matches('.').to_string();

        let qtype = before_query.split_whitespace().next_back()?.to_string();

        Some(DnsQuery {
            timestamp: Utc::now(),
            client: client_part.to_string(),
            server: server_part,
            query: name,
            query_type: qtype,
            answers: vec![],
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_tcpdump_query_line() {
            let line = "12:34:56.789123 IP 192.168.1.5.12345 > 192.168.1.1.53: 12345+ A? example.com. (32)";
            let q = parse_tcpdump_line(line).unwrap();
            assert_eq!(q.client, "192.168.1.5.12345");
            assert_eq!(q.server, "192.168.1.1.53");
            assert_eq!(q.query, "example.com");
            assert_eq!(q.query_type, "A");
        }

        #[test]
        fn parse_tcpdump_response_line_is_ignored() {
            let line = "12:34:56.790456 IP 192.168.1.1.53 > 192.168.1.5.12345: 12345 1/0/0 A 93.184.216.34 (48)";
            assert!(parse_tcpdump_line(line).is_none());
        }
    }
}

// ── macOS implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::process::Command;

    pub fn run_dns_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        tcpdump_dns::run_capture(queries)
    }

    pub fn interfaces() -> Vec<NetworkInterface> {
        let out = Command::new("ifconfig").output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return vec![],
        };
        parse_ifconfig(&text)
    }

    pub fn connections() -> Vec<NetworkConnection> {
        let mut connections = vec![];
        connections.extend(parse_lsof_fields(&run_lsof_fields("TCP"), "TCP"));
        connections.extend(parse_lsof_fields(&run_lsof_fields("UDP"), "UDP"));
        connections
    }

    pub fn dns_servers() -> Vec<String> {
        let out = Command::new("scutil").arg("--dns").output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return vec![],
        };
        parse_scutil_dns(&text)
    }

    fn run_lsof_fields(proto: &str) -> String {
        Command::new("lsof")
            .args([
                format!("-i{}", proto),
                "-n".into(),
                "-P".into(),
                "-F".into(),
                "pcnT".into(),
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    }

    fn parse_ifconfig(text: &str) -> Vec<NetworkInterface> {
        let mut interfaces = vec![];
        let mut current: Option<NetworkInterface> = None;

        for line in text.lines() {
            if line.starts_with(' ') || line.starts_with('\t') {
                let trimmed = line.trim();
                if let Some(ref mut iface) = current {
                    if trimmed.starts_with("inet ") {
                        if let Some(ip) = trimmed.split_whitespace().nth(1) {
                            iface.ips.push(ip.to_string());
                        }
                    } else if trimmed.starts_with("inet6 ") {
                        if let Some(ip) = trimmed.split_whitespace().nth(1) {
                            let ip = ip.split('%').next().unwrap_or(ip);
                            iface.ips.push(ip.to_string());
                        }
                    }
                }
            } else {
                if let Some(iface) = current.take() {
                    if !iface.ips.is_empty() {
                        interfaces.push(iface);
                    }
                }
                if let Some(name) = line.split(':').next() {
                    current = Some(NetworkInterface {
                        name: name.trim().to_string(),
                        ips: vec![],
                    });
                }
            }
        }

        if let Some(iface) = current {
            if !iface.ips.is_empty() {
                interfaces.push(iface);
            }
        }

        interfaces
    }

    fn parse_lsof_fields(text: &str, proto: &str) -> Vec<NetworkConnection> {
        let mut connections = vec![];
        let mut current_pid: Option<u32> = None;
        let mut current_command: Option<String> = None;
        let mut pending_name: Option<String> = None;

        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            match line.chars().next() {
                Some('p') => {
                    current_pid = line[1..].parse().ok();
                    current_command = None;
                    pending_name = None;
                }
                Some('c') => {
                    current_command = Some(line[1..].to_string());
                    pending_name = None;
                }
                Some('n') => {
                    pending_name = Some(line[1..].to_string());
                }
                Some('T') if pending_name.is_some() => {
                    if let Some(name) = pending_name.take() {
                        let state = line.strip_prefix("TST=").map(|s| s.to_string());
                        let (local, remote) = parse_lsof_name(&name);
                        connections.push(NetworkConnection {
                            protocol: proto.to_string(),
                            local_addr: local,
                            remote_addr: remote,
                            state,
                            pid: current_pid,
                            process_name: current_command.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        if let Some(name) = pending_name {
            let (local, remote) = parse_lsof_name(&name);
            connections.push(NetworkConnection {
                protocol: proto.to_string(),
                local_addr: local,
                remote_addr: remote,
                state: None,
                pid: current_pid,
                process_name: current_command.clone(),
            });
        }

        connections
    }

    fn parse_lsof_name(name: &str) -> (String, Option<String>) {
        if let Some(idx) = name.find("->") {
            (name[..idx].to_string(), Some(name[idx + 2..].to_string()))
        } else {
            (name.to_string(), None)
        }
    }

    fn parse_scutil_dns(text: &str) -> Vec<String> {
        let mut servers = vec![];
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("nameserver") {
                if let Some(ip) = trimmed.split(':').nth(1) {
                    let ip = ip.trim();
                    if !servers.contains(&ip.to_string()) {
                        servers.push(ip.to_string());
                    }
                }
            }
        }
        servers
    }
}

// ── Windows implementation ────────────────────────────────────────────────────

#[cfg(windows)]
mod platform {
    use super::*;
    use std::process::Command;
    use std::thread;

    /// Poll the Windows DNS client cache every few seconds for recently resolved names.
    pub fn run_dns_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        loop {
            match poll_dns_client_cache() {
                Ok(new_queries) => {
                    let mut guard = queries.lock().unwrap_or_else(|p| p.into_inner());
                    for q in new_queries {
                        if !guard.iter().any(|existing| existing.query == q.query && existing.query_type == q.query_type) {
                            guard.push(q);
                        }
                    }
                    const MAX_QUERIES: usize = 1000;
                    while guard.len() > MAX_QUERIES {
                        guard.remove(0);
                    }
                }
                Err(_) => {}
            }
            thread::sleep(Duration::from_secs(5));
        }
    }

    fn poll_dns_client_cache() -> anyhow::Result<Vec<DnsQuery>> {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-DnsClientCache | Select-Object Entry, Type | ConvertTo-Csv -NoTypeInformation",
            ])
            .output()?;

        if !out.status.success() {
            return Err(anyhow::anyhow!("Get-DnsClientCache failed"));
        }

        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        let headers: Vec<&str> = header.split(',').map(|s| s.trim()).collect();

        let mut queries = vec![];
        for line in lines {
            let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            let get = |name: &str| headers.iter().position(|h| h == name).and_then(|i| cols.get(i)).copied();

            let name = get("Entry").unwrap_or("").trim_matches('"').to_string();
            let qtype = get("Type").unwrap_or("").trim_matches('"').to_string();
            if name.is_empty() {
                continue;
            }

            queries.push(DnsQuery {
                timestamp: Utc::now(),
                client: String::new(),
                server: String::new(),
                query: name,
                query_type: qtype,
                answers: vec![],
            });
        }
        Ok(queries)
    }

    pub fn interfaces() -> Vec<NetworkInterface> {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-NetIPAddress | Select-Object InterfaceAlias, IPAddress | Format-Table -HideTableHeaders",
            ])
            .output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return vec![],
        };
        parse_windows_ip_table(&text)
    }

    pub fn connections() -> Vec<NetworkConnection> {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-NetTCPConnection | Select-Object LocalAddress, LocalPort, RemoteAddress, RemotePort, State, OwningProcess | ConvertTo-Csv -NoTypeInformation",
            ])
            .output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return vec![],
        };
        parse_windows_connections(&text)
    }

    pub fn dns_servers() -> Vec<String> {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-DnsClientServerAddress -AddressFamily IPv4).ServerAddresses | Sort-Object -Unique",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            _ => vec![],
        }
    }

    fn parse_windows_ip_table(text: &str) -> Vec<NetworkInterface> {
        let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                map.entry(parts[0].to_string())
                    .or_default()
                    .push(parts[1].to_string());
            }
        }
        map.into_iter()
            .map(|(name, ips)| NetworkInterface { name, ips })
            .collect()
    }

    fn parse_windows_connections(text: &str) -> Vec<NetworkConnection> {
        let mut connections = vec![];
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        let headers: Vec<&str> = header.split(',').map(|s| s.trim()).collect();

        for line in lines {
            let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            let get = |name: &str| headers.iter().position(|h| h == name).and_then(|i| cols.get(i)).copied();

            let local = format!("{}:{}", get("LocalAddress").unwrap_or(""), get("LocalPort").unwrap_or(""));
            let remote_addr = get("RemoteAddress");
            let remote_port = get("RemotePort");
            let remote = remote_addr.and_then(|a| remote_port.map(|p| format!("{}:{}", a, p)));

            connections.push(NetworkConnection {
                protocol: "TCP".to_string(),
                local_addr: local,
                remote_addr: remote,
                state: get("State").map(|s| s.to_string()),
                pid: get("OwningProcess").and_then(|s| s.parse().ok()),
                process_name: None,
            });
        }
        connections
    }
}

// ── Linux / other implementation ──────────────────────────────────────────────

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    use std::process::Command;

    pub fn run_dns_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        tcpdump_dns::run_capture(queries)
    }

    pub fn interfaces() -> Vec<NetworkInterface> {
        let out = Command::new("ip").args(["addr", "show", "up"]).output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return vec![],
        };
        parse_ip_addr(&text)
    }

    pub fn connections() -> Vec<NetworkConnection> {
        let mut connections = vec![];
        connections.extend(parse_ss(&run_ss("-tan"), "TCP"));
        connections.extend(parse_ss(&run_ss("-uan"), "UDP"));
        connections
    }

    pub fn dns_servers() -> Vec<String> {
        std::fs::read_to_string("/etc/resolv.conf")
            .ok()
            .map(|s| {
                s.lines()
                    .filter(|l| l.trim_start().starts_with("nameserver "))
                    .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn run_ss(flags: &str) -> String {
        Command::new("ss")
            .args(flags.split_whitespace())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    }

    fn parse_ip_addr(text: &str) -> Vec<NetworkInterface> {
        let mut interfaces = vec![];
        let mut current: Option<NetworkInterface> = None;

        for line in text.lines() {
            if !line.starts_with(' ') {
                if let Some(iface) = current.take() {
                    if !iface.ips.is_empty() {
                        interfaces.push(iface);
                    }
                }
                if let Some(name) = line.split(':').nth(1) {
                    current = Some(NetworkInterface {
                        name: name.trim().to_string(),
                        ips: vec![],
                    });
                }
            } else if let Some(ref mut iface) = current {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(ip) = trimmed.split_whitespace().nth(1) {
                        iface.ips.push(ip.split('/').next().unwrap_or(ip).to_string());
                    }
                } else if trimmed.starts_with("inet6 ") {
                    if let Some(ip) = trimmed.split_whitespace().nth(1) {
                        iface.ips.push(ip.split('/').next().unwrap_or(ip).to_string());
                    }
                }
            }
        }

        if let Some(iface) = current {
            if !iface.ips.is_empty() {
                interfaces.push(iface);
            }
        }
        interfaces
    }

    fn parse_ss(text: &str, proto: &str) -> Vec<NetworkConnection> {
        let mut connections = vec![];
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 5 {
                continue;
            }
            let local = cols.get(4).copied().unwrap_or("").to_string();
            let remote = cols.get(5).copied().filter(|s| !s.is_empty()).map(|s| s.to_string());
            let state = cols.get(1).copied().map(|s| s.to_string());
            let process_col = cols.get(6).copied().unwrap_or("");
            let (pid, process_name) = parse_ss_process(process_col);

            connections.push(NetworkConnection {
                protocol: proto.to_string(),
                local_addr: local,
                remote_addr: remote,
                state,
                pid,
                process_name,
            });
        }
        connections
    }

    fn parse_ss_process(text: &str) -> (Option<u32>, Option<String>) {
        let text = text.trim();
        if text.is_empty() || text == "-" {
            return (None, None);
        }
        let pid = text
            .split("pid=")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.parse().ok());
        let name = text.split('"').nth(1).map(|s| s.to_string());
        (pid, name)
    }
}
