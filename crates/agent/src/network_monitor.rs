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

/// One captured DNS query. Serialized as an entry of
/// `network_traffic.dns_queries` in the telemetry payload.
#[derive(Debug, Clone, Serialize)]
pub struct DnsQuery {
    /// Capture time as epoch milliseconds (UTC).
    pub ts_ms: i64,
    /// Query name (qname) without the trailing dot.
    pub domain: String,
    /// Record type string ("A", "AAAA", "CNAME", ...).
    pub qtype: String,
    /// Resolved IPs from the response (empty if none, NXDOMAIN, or timed out).
    pub answers: Vec<String>,
    /// IP of the DNS server the query was sent to.
    pub resolver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// Max buffered DNS queries; the server keeps the newest 200 per snapshot.
const MAX_DNS_QUERIES: usize = 200;

/// Best-effort live DNS query capture.
///
/// - Linux: reads UDP port-53 traffic from an AF_PACKET socket (requires root).
/// - macOS: runs `tcpdump` on port 53 (requires root).
/// - Windows: polls `Get-DnsClientCache` for recently resolved names.
///
/// If capture cannot start, a warning is logged once and `dns_queries` stays
/// empty; the agent never fails over DNS capture.
#[derive(Clone)]
pub struct DnsCapture {
    queries: Arc<Mutex<Vec<DnsQuery>>>,
}

impl DnsCapture {
    pub fn start() -> Self {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let queries_clone = Arc::clone(&queries);
        std::thread::spawn(move || {
            if let Err(e) = platform::run_dns_capture(queries_clone) {
                tracing::warn!("DNS capture unavailable: {:#}; dns_queries will be empty", e);
            }
        });
        Self { queries }
    }

    /// Take all buffered queries (oldest first, newest last), clearing the buffer.
    pub fn drain(&self) -> Vec<DnsQuery> {
        let mut guard = self.queries.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    }
}

/// Push onto the bounded buffer, dropping the oldest entries on overflow.
fn push_query(queries: &Arc<Mutex<Vec<DnsQuery>>>, q: DnsQuery) {
    let mut guard = queries.lock().unwrap_or_else(|p| p.into_inner());
    if guard.len() >= MAX_DNS_QUERIES {
        let overflow = guard.len() + 1 - MAX_DNS_QUERIES;
        guard.drain(..overflow);
    }
    guard.push(q);
}

pub fn snapshot(dns: Option<&DnsCapture>) -> NetworkSnapshot {
    NetworkSnapshot {
        interfaces: platform::interfaces(),
        connections: platform::connections(),
        dns_servers: platform::dns_servers(),
        dns_queries: dns.map(|d| d.drain()).unwrap_or_default(),
        captured_at: Utc::now(),
    }
}

/// DNS record type number → display string.
#[cfg(any(target_os = "linux", windows, test))]
fn qtype_str(n: u16) -> String {
    match n {
        1 => "A".to_string(),
        2 => "NS".to_string(),
        5 => "CNAME".to_string(),
        6 => "SOA".to_string(),
        12 => "PTR".to_string(),
        15 => "MX".to_string(),
        16 => "TXT".to_string(),
        28 => "AAAA".to_string(),
        33 => "SRV".to_string(),
        65 => "HTTPS".to_string(),
        other => other.to_string(),
    }
}

/// Match a socket-table local address (e.g. `192.168.1.5:1234`,
/// `[fe80::1]:1234`, `0.0.0.0:1234`, `*:1234`) against a packet's client
/// ip:port. Wildcard listeners match any ip on the same port.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn endpoint_matches(local: &str, ip: &str, port: u16) -> bool {
    let (lip, lport) = match local.rfind(':') {
        Some(i) => (&local[..i], &local[i + 1..]),
        None => return false,
    };
    if lport.parse::<u16>().ok() != Some(port) {
        return false;
    }
    let lip = lip.trim_matches(|c| c == '[' || c == ']');
    lip == ip || lip == "*" || lip == "0.0.0.0" || lip == "::"
}

// ── Shared DNS query/response pairing ─────────────────────────────────────────

/// Pairs queries with responses by (dns id, client endpoint) so answers can be
/// attached to the query that produced them. Used by the AF_PACKET (Linux) and
/// tcpdump (macOS) capture loops.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
mod dns_pairing {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// How long a query waits for its response before being reported with
    /// empty answers.
    pub const PAIR_TIMEOUT: Duration = Duration::from_secs(5);

    /// One endpoint of a DNS exchange.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Endpoint {
        pub ip: String,
        pub port: u16,
    }

    /// A parsed DNS packet, either direction.
    #[derive(Debug)]
    pub enum DnsPacketEvent {
        Query {
            id: u16,
            client: Endpoint,
            server_ip: String,
            domain: String,
            qtype: String,
        },
        Response {
            id: u16,
            client: Endpoint,
            answers: Vec<String>,
        },
    }

    struct Pending {
        query: DnsQuery,
        inserted: Instant,
    }

    /// Queries seen but not yet answered, keyed by (dns id, client endpoint).
    #[derive(Default)]
    pub struct PendingQueries {
        map: HashMap<(u16, Endpoint), Pending>,
    }

    impl PendingQueries {
        pub fn insert(&mut self, id: u16, client: Endpoint, query: DnsQuery) {
            self.map.insert(
                (id, client),
                Pending {
                    query,
                    inserted: Instant::now(),
                },
            );
        }

        pub fn complete(
            &mut self,
            id: u16,
            client: &Endpoint,
            answers: Vec<String>,
        ) -> Option<DnsQuery> {
            let key = (id, client.clone());
            self.map.remove(&key).map(|mut p| {
                p.query.answers = answers;
                p.query
            })
        }

        /// Drain entries that waited longer than PAIR_TIMEOUT (reported with
        /// empty answers).
        pub fn expire(&mut self) -> Vec<DnsQuery> {
            let stale: Vec<(u16, Endpoint)> = self
                .map
                .iter()
                .filter(|(_, p)| p.inserted.elapsed() >= PAIR_TIMEOUT)
                .map(|(k, _)| k.clone())
                .collect();
            stale
                .into_iter()
                .filter_map(|k| self.map.remove(&k).map(|p| p.query))
                .collect()
        }
    }

    /// Feed one parsed packet into the pairing state machine. `attribute`
    /// resolves a client ip:port to its owning process (best effort).
    pub fn handle_event(
        ev: DnsPacketEvent,
        pending: &mut PendingQueries,
        queries: &Arc<Mutex<Vec<DnsQuery>>>,
        attribute: &mut dyn FnMut(&str, u16) -> (Option<u32>, Option<String>),
    ) {
        match ev {
            DnsPacketEvent::Query {
                id,
                client,
                server_ip,
                domain,
                qtype,
            } => {
                let (pid, process) = attribute(&client.ip, client.port);
                pending.insert(
                    id,
                    client,
                    DnsQuery {
                        ts_ms: Utc::now().timestamp_millis(),
                        domain,
                        qtype,
                        answers: vec![],
                        resolver: server_ip,
                        pid,
                        process,
                    },
                );
            }
            DnsPacketEvent::Response {
                id,
                client,
                answers,
            } => {
                if let Some(q) = pending.complete(id, &client, answers) {
                    super::push_query(queries, q);
                }
            }
        }
    }

    /// Flush expired pending queries into the buffer.
    pub fn flush_expired(pending: &mut PendingQueries, queries: &Arc<Mutex<Vec<DnsQuery>>>) {
        for q in pending.expire() {
            super::push_query(queries, q);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn ep() -> Endpoint {
            Endpoint {
                ip: "192.168.1.5".to_string(),
                port: 12345,
            }
        }

        fn query() -> DnsQuery {
            DnsQuery {
                ts_ms: 0,
                domain: "example.com".to_string(),
                qtype: "A".to_string(),
                answers: vec![],
                resolver: "192.168.1.1".to_string(),
                pid: None,
                process: None,
            }
        }

        #[test]
        fn response_completes_pending_query() {
            let mut pending = PendingQueries::default();
            pending.insert(7, ep(), query());
            let q = pending
                .complete(7, &ep(), vec!["93.184.216.34".to_string()])
                .unwrap();
            assert_eq!(q.answers, vec!["93.184.216.34"]);
            assert!(pending.complete(7, &ep(), vec![]).is_none());
        }

        #[test]
        fn different_client_does_not_complete() {
            let mut pending = PendingQueries::default();
            pending.insert(7, ep(), query());
            let other = Endpoint {
                ip: "192.168.1.5".to_string(),
                port: 9999,
            };
            assert!(pending.complete(7, &other, vec![]).is_none());
        }

        #[test]
        fn handle_event_pairs_and_pushes() {
            let queries = Arc::new(Mutex::new(Vec::new()));
            let mut pending = PendingQueries::default();
            let mut no_attr = |_: &str, _: u16| (None, None);

            handle_event(
                DnsPacketEvent::Query {
                    id: 1,
                    client: ep(),
                    server_ip: "192.168.1.1".to_string(),
                    domain: "example.com".to_string(),
                    qtype: "A".to_string(),
                },
                &mut pending,
                &queries,
                &mut no_attr,
            );
            assert!(queries.lock().unwrap().is_empty());

            handle_event(
                DnsPacketEvent::Response {
                    id: 1,
                    client: ep(),
                    answers: vec!["93.184.216.34".to_string()],
                },
                &mut pending,
                &queries,
                &mut no_attr,
            );
            let guard = queries.lock().unwrap();
            assert_eq!(guard.len(), 1);
            assert_eq!(guard[0].domain, "example.com");
            assert_eq!(guard[0].answers, vec!["93.184.216.34"]);
        }
    }
}

// ── tcpdump DNS capture (macOS) ───────────────────────────────────────────────

#[cfg(any(target_os = "macos", test))]
mod tcpdump_dns {
    use super::dns_pairing::{
        flush_expired, handle_event, DnsPacketEvent, Endpoint, PendingQueries,
    };
    use super::{platform, DnsQuery};
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    pub fn run_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        let mut child = Command::new("tcpdump")
            .args(["-i", "any", "-n", "-l", "-Q", "inout", "port", "53"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stderr"))?;

        let mut pending = PendingQueries::default();
        let mut attribute = |ip: &str, port: u16| platform::attribute_dns_client(ip, port);

        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(ev) = parse_tcpdump_line(&line) {
                handle_event(ev, &mut pending, &queries, &mut attribute);
            }
            flush_expired(&mut pending, &queries);
        }

        // tcpdump exited (e.g. permission denied, interface gone): surface why.
        let status = child.wait()?;
        let mut err_text = String::new();
        let _ = stderr.read_to_string(&mut err_text);
        Err(anyhow::anyhow!(
            "tcpdump exited ({}): {}",
            status,
            err_text.trim()
        ))
    }

    /// Parse a single `tcpdump` line into a query or response event.
    ///
    /// Query:    `12:34:56.789123 IP 192.168.1.5.12345 > 192.168.1.1.53: 12345+ A? example.com. (32)`
    /// Response: `12:34:56.790456 IP 192.168.1.1.53 > 192.168.1.5.12345: 12345 1/0/0 A 93.184.216.34 (48)`
    /// NXDOMAIN: `... 12345 NXDomain 0/1/0 (104)`
    fn parse_tcpdump_line(line: &str) -> Option<DnsPacketEvent> {
        let arrow = line.find(" > ")?;
        let src = line[..arrow].rsplit(' ').next()?;
        let after_arrow = &line[arrow + 3..];
        let colon = after_arrow.find(": ")?;
        let dst = &after_arrow[..colon];
        let payload = after_arrow[colon + 2..].trim();

        let id_token = payload.split_whitespace().next()?;
        let id: u16 = id_token
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()?;

        if payload.contains('?') {
            // Query: client is the source, resolver is the destination.
            let qmark = payload.find('?')?;
            let qtype = payload[..qmark].split_whitespace().next_back()?.to_string();
            let domain = payload[qmark + 1..]
                .split_whitespace()
                .next()?
                .trim_end_matches('.')
                .to_string();
            if domain.is_empty() {
                return None;
            }
            Some(DnsPacketEvent::Query {
                id,
                client: parse_endpoint(src)?,
                server_ip: parse_endpoint(dst)?.ip,
                domain,
                qtype,
            })
        } else {
            // Response: client is the destination. Answers are A/AAAA records
            // after the counts token, or empty for NXDomain/ServFail/...
            let mut answers = vec![];
            let after_id = payload[id_token.len()..].trim_start();
            if after_id
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            {
                let records = after_id.splitn(2, ' ').nth(1).unwrap_or("");
                let records = records.split(" (").next().unwrap_or(records);
                for rec in records.split(',') {
                    let mut parts = rec.split_whitespace();
                    if let (Some(t), Some(rdata)) = (parts.next(), parts.next()) {
                        if t == "A" || t == "AAAA" {
                            answers.push(rdata.to_string());
                        }
                    }
                }
            }
            Some(DnsPacketEvent::Response {
                id,
                client: parse_endpoint(dst)?,
                answers,
            })
        }
    }

    /// tcpdump prints endpoints as `ip.port` (also for IPv6, e.g. `fe80::1.53`).
    fn parse_endpoint(s: &str) -> Option<Endpoint> {
        let i = s.rfind('.')?;
        let port = s[i + 1..].parse().ok()?;
        Some(Endpoint {
            ip: s[..i].to_string(),
            port,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_tcpdump_query_line() {
            let line = "12:34:56.789123 IP 192.168.1.5.12345 > 192.168.1.1.53: 12345+ A? example.com. (32)";
            match parse_tcpdump_line(line).unwrap() {
                DnsPacketEvent::Query {
                    id,
                    client,
                    server_ip,
                    domain,
                    qtype,
                } => {
                    assert_eq!(id, 12345);
                    assert_eq!(client.ip, "192.168.1.5");
                    assert_eq!(client.port, 12345);
                    assert_eq!(server_ip, "192.168.1.1");
                    assert_eq!(domain, "example.com");
                    assert_eq!(qtype, "A");
                }
                _ => panic!("expected query"),
            }
        }

        #[test]
        fn parse_tcpdump_response_line() {
            let line = "12:34:56.790456 IP 192.168.1.1.53 > 192.168.1.5.12345: 12345 1/0/0 A 93.184.216.34 (48)";
            match parse_tcpdump_line(line).unwrap() {
                DnsPacketEvent::Response {
                    id,
                    client,
                    answers,
                } => {
                    assert_eq!(id, 12345);
                    assert_eq!(client.ip, "192.168.1.5");
                    assert_eq!(client.port, 12345);
                    assert_eq!(answers, vec!["93.184.216.34"]);
                }
                _ => panic!("expected response"),
            }
        }

        #[test]
        fn parse_tcpdump_multi_answer_response() {
            let line = "12:34:56.790456 IP 192.168.1.1.53 > 192.168.1.5.12345: 12345 3/0/0 CNAME www.example.com., A 93.184.216.34, AAAA 2606:2800:220:1:248:1893:25c8:1946 (96)";
            match parse_tcpdump_line(line).unwrap() {
                DnsPacketEvent::Response { answers, .. } => {
                    assert_eq!(
                        answers,
                        vec![
                            "93.184.216.34",
                            "2606:2800:220:1:248:1893:25c8:1946"
                        ]
                    );
                }
                _ => panic!("expected response"),
            }
        }

        #[test]
        fn parse_tcpdump_nxdomain_response() {
            let line = "12:34:56.790456 IP 192.168.1.1.53 > 192.168.1.5.12345: 12345 NXDomain 0/1/0 (104)";
            match parse_tcpdump_line(line).unwrap() {
                DnsPacketEvent::Response { answers, .. } => assert!(answers.is_empty()),
                _ => panic!("expected response"),
            }
        }

        #[test]
        fn parse_ipv6_endpoint() {
            let ep = parse_endpoint("fe80::1234.53").unwrap();
            assert_eq!(ep.ip, "fe80::1234");
            assert_eq!(ep.port, 53);
        }
    }
}

// ── DNS wire-format parsing (Linux AF_PACKET, unit-tested everywhere) ─────────

#[cfg(any(target_os = "linux", test))]
mod dns_wire {
    use super::dns_pairing::{DnsPacketEvent, Endpoint};

    /// Parse a cooked-capture (SOCK_DGRAM) frame: Linux SLL header, then
    /// IPv4/IPv6, UDP, and a DNS payload on port 53.
    pub fn parse_frame(buf: &[u8]) -> Option<DnsPacketEvent> {
        if buf.len() < 16 {
            return None;
        }
        let eth_proto = u16::from_be_bytes([buf[14], buf[15]]);
        let l3 = &buf[16..];
        match eth_proto {
            0x0800 => parse_ipv4(l3),
            0x86dd => parse_ipv6(l3),
            _ => None,
        }
    }

    fn parse_ipv4(p: &[u8]) -> Option<DnsPacketEvent> {
        if p.len() < 20 {
            return None;
        }
        let ihl = (p[0] & 0x0f) as usize * 4;
        if ihl < 20 || p.len() < ihl || p[9] != 17 {
            return None;
        }
        let src = format!("{}.{}.{}.{}", p[12], p[13], p[14], p[15]);
        let dst = format!("{}.{}.{}.{}", p[16], p[17], p[18], p[19]);
        parse_udp(&p[ihl..], src, dst)
    }

    fn parse_ipv6(p: &[u8]) -> Option<DnsPacketEvent> {
        // No extension-header walking: plain next-header UDP only.
        if p.len() < 40 || p[6] != 17 {
            return None;
        }
        let src = ipv6_str(&p[8..24]);
        let dst = ipv6_str(&p[24..40]);
        parse_udp(&p[40..], src, dst)
    }

    fn ipv6_str(b: &[u8]) -> String {
        let mut groups = [0u16; 8];
        for (i, g) in groups.iter_mut().enumerate() {
            *g = u16::from_be_bytes([b[i * 2], b[i * 2 + 1]]);
        }
        std::net::Ipv6Addr::from(groups).to_string()
    }

    fn parse_udp(p: &[u8], src: String, dst: String) -> Option<DnsPacketEvent> {
        if p.len() < 8 {
            return None;
        }
        let sport = u16::from_be_bytes([p[0], p[1]]);
        let dport = u16::from_be_bytes([p[2], p[3]]);
        let ulen = u16::from_be_bytes([p[4], p[5]]) as usize;
        if ulen < 8 || p.len() < ulen {
            return None;
        }
        let dns = &p[8..ulen];
        if dport == 53 {
            let (id, domain, qtype) = parse_dns_query(dns)?;
            Some(DnsPacketEvent::Query {
                id,
                client: Endpoint {
                    ip: src,
                    port: sport,
                },
                server_ip: dst,
                domain,
                qtype,
            })
        } else if sport == 53 {
            let (id, answers) = parse_dns_response(dns)?;
            Some(DnsPacketEvent::Response {
                id,
                client: Endpoint {
                    ip: dst,
                    port: dport,
                },
                answers,
            })
        } else {
            None
        }
    }

    fn parse_dns_query(dns: &[u8]) -> Option<(u16, String, String)> {
        if dns.len() < 12 {
            return None;
        }
        let flags = u16::from_be_bytes([dns[2], dns[3]]);
        if flags & 0x8000 != 0 {
            return None; // not a query
        }
        let qdcount = u16::from_be_bytes([dns[4], dns[5]]);
        if qdcount < 1 {
            return None;
        }
        let id = u16::from_be_bytes([dns[0], dns[1]]);
        let (name, off) = read_name(dns, 12)?;
        if name.is_empty() || dns.len() < off + 4 {
            return None;
        }
        let qtype = u16::from_be_bytes([dns[off], dns[off + 1]]);
        Some((id, name, super::qtype_str(qtype)))
    }

    fn parse_dns_response(dns: &[u8]) -> Option<(u16, Vec<String>)> {
        if dns.len() < 12 {
            return None;
        }
        let flags = u16::from_be_bytes([dns[2], dns[3]]);
        if flags & 0x8000 == 0 {
            return None; // not a response
        }
        let id = u16::from_be_bytes([dns[0], dns[1]]);
        let rcode = flags & 0x000f;
        let qdcount = u16::from_be_bytes([dns[4], dns[5]]) as usize;
        let ancount = u16::from_be_bytes([dns[6], dns[7]]) as usize;
        if rcode != 0 {
            // NXDOMAIN / ServFail / ...: completes the query with no answers.
            return Some((id, vec![]));
        }

        let mut off = 12;
        for _ in 0..qdcount {
            let (_, o) = read_name(dns, off)?;
            off = o.checked_add(4)?;
            if dns.len() < off {
                return None;
            }
        }

        let mut answers = vec![];
        for _ in 0..ancount {
            let (_, o) = read_name(dns, off)?;
            if dns.len() < o + 10 {
                break;
            }
            let rtype = u16::from_be_bytes([dns[o], dns[o + 1]]);
            let rdlen = u16::from_be_bytes([dns[o + 8], dns[o + 9]]) as usize;
            let rdata_start = o + 10;
            if dns.len() < rdata_start + rdlen {
                break;
            }
            let rdata = &dns[rdata_start..rdata_start + rdlen];
            match (rtype, rdlen) {
                (1, 4) => answers.push(format!(
                    "{}.{}.{}.{}",
                    rdata[0], rdata[1], rdata[2], rdata[3]
                )),
                (28, 16) => answers.push(ipv6_str(rdata)),
                _ => {}
            }
            off = rdata_start + rdlen;
        }
        Some((id, answers))
    }

    /// Read a (possibly compressed) DNS name. Returns the name without a
    /// trailing dot and the offset just after the name at `start`.
    fn read_name(dns: &[u8], start: usize) -> Option<(String, usize)> {
        let mut labels = vec![];
        let mut off = start;
        let mut end = None;
        let mut jumps = 0;
        loop {
            if off >= dns.len() {
                return None;
            }
            let len = dns[off] as usize;
            if len & 0xc0 == 0xc0 {
                if off + 1 >= dns.len() {
                    return None;
                }
                let ptr = ((len & 0x3f) << 8) | dns[off + 1] as usize;
                if end.is_none() {
                    end = Some(off + 2);
                }
                off = ptr;
                jumps += 1;
                if jumps > 10 {
                    return None;
                }
            } else if len == 0 {
                if end.is_none() {
                    end = Some(off + 1);
                }
                break;
            } else {
                if off + 1 + len > dns.len() {
                    return None;
                }
                labels.push(String::from_utf8_lossy(&dns[off + 1..off + 1 + len]).to_string());
                off += 1 + len;
            }
        }
        Some((labels.join("."), end?))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Build SLL + IPv4 + UDP + DNS frame bytes for tests.
        fn frame(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, dns: &[u8]) -> Vec<u8> {
            let mut buf = vec![0u8; 16];
            buf[14] = 0x08;
            buf[15] = 0x00;
            // IPv4 header (20 bytes, no options)
            let mut ip = vec![
                0x45, 0x00, 0x00, 0x00, // ver/ihl, tos, total len (filled below)
                0x00, 0x00, 0x00, 0x00, // id, flags/frag
                0x40, 17, 0x00, 0x00, // ttl, proto=UDP, checksum
            ];
            ip.extend_from_slice(&src);
            ip.extend_from_slice(&dst);
            let total = (20 + 8 + dns.len()) as u16;
            ip[2] = (total >> 8) as u8;
            ip[3] = total as u8;
            buf.extend_from_slice(&ip);
            // UDP header
            let ulen = (8 + dns.len()) as u16;
            buf.extend_from_slice(&sport.to_be_bytes());
            buf.extend_from_slice(&dport.to_be_bytes());
            buf.extend_from_slice(&ulen.to_be_bytes());
            buf.extend_from_slice(&[0, 0]);
            buf.extend_from_slice(dns);
            buf
        }

        fn query_dns(id: u16, name: &str, qtype: u16) -> Vec<u8> {
            let mut dns = vec![];
            dns.extend_from_slice(&id.to_be_bytes());
            dns.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
            dns.extend_from_slice(&1u16.to_be_bytes()); // qd
            dns.extend_from_slice(&0u16.to_be_bytes()); // an
            dns.extend_from_slice(&0u16.to_be_bytes()); // ns
            dns.extend_from_slice(&0u16.to_be_bytes()); // ar
            for label in name.split('.') {
                dns.push(label.len() as u8);
                dns.extend_from_slice(label.as_bytes());
            }
            dns.push(0);
            dns.extend_from_slice(&qtype.to_be_bytes());
            dns.extend_from_slice(&1u16.to_be_bytes()); // class IN
            dns
        }

        fn response_dns(id: u16, name: &str, ips: &[[u8; 4]]) -> Vec<u8> {
            let mut dns = query_dns(id, name, 1);
            dns[2] = 0x81;
            dns[3] = 0x80; // QR + RD + RA, rcode 0
            dns[6] = 0x00;
            dns[7] = ips.len() as u8; // ancount
            for ip in ips {
                dns.extend_from_slice(&[0xc0, 0x0c]); // name ptr to question
                dns.extend_from_slice(&1u16.to_be_bytes()); // type A
                dns.extend_from_slice(&1u16.to_be_bytes()); // class IN
                dns.extend_from_slice(&60u32.to_be_bytes()); // ttl
                dns.extend_from_slice(&4u16.to_be_bytes()); // rdlen
                dns.extend_from_slice(ip);
            }
            dns
        }

        #[test]
        fn parses_udp_query_frame() {
            let dns = query_dns(0x1234, "example.com", 1);
            let f = frame([192, 168, 1, 5], [192, 168, 1, 1], 51234, 53, &dns);
            match parse_frame(&f).unwrap() {
                DnsPacketEvent::Query {
                    id,
                    client,
                    server_ip,
                    domain,
                    qtype,
                } => {
                    assert_eq!(id, 0x1234);
                    assert_eq!(client.ip, "192.168.1.5");
                    assert_eq!(client.port, 51234);
                    assert_eq!(server_ip, "192.168.1.1");
                    assert_eq!(domain, "example.com");
                    assert_eq!(qtype, "A");
                }
                _ => panic!("expected query"),
            }
        }

        #[test]
        fn parses_udp_response_frame() {
            let dns = response_dns(0x1234, "example.com", &[[93, 184, 216, 34], [93, 184, 216, 35]]);
            let f = frame([192, 168, 1, 1], [192, 168, 1, 5], 53, 51234, &dns);
            match parse_frame(&f).unwrap() {
                DnsPacketEvent::Response {
                    id,
                    client,
                    answers,
                } => {
                    assert_eq!(id, 0x1234);
                    assert_eq!(client.ip, "192.168.1.5");
                    assert_eq!(client.port, 51234);
                    assert_eq!(answers, vec!["93.184.216.34", "93.184.216.35"]);
                }
                _ => panic!("expected response"),
            }
        }

        #[test]
        fn ignores_non_dns_traffic() {
            let dns = query_dns(0x1234, "example.com", 1);
            let f = frame([10, 0, 0, 1], [10, 0, 0, 2], 1234, 443, &dns);
            assert!(parse_frame(&f).is_none());
        }

        #[test]
        fn rejects_truncated_frames() {
            assert!(parse_frame(&[0u8; 10]).is_none());
        }
    }
}

// ── AF_PACKET capture (Linux) ─────────────────────────────────────────────────

/// Captures UDP port-53 traffic from a cooked AF_PACKET socket (root
/// required). No external tools needed; TCP DNS (rare: zone transfers,
/// oversized responses) is not covered.
#[cfg(target_os = "linux")]
mod afpacket_dns {
    use super::dns_pairing::{flush_expired, handle_event, PendingQueries};
    use super::{dns_wire, platform, DnsQuery};
    use std::sync::{Arc, Mutex};

    pub fn run_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        // socket() takes the protocol in network byte order.
        let proto = (libc::ETH_P_ALL as u16).to_be() as libc::c_int;
        let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM, proto) };
        if fd < 0 {
            return Err(anyhow::anyhow!(
                "opening AF_PACKET socket failed (needs root): {}",
                std::io::Error::last_os_error()
            ));
        }

        // 1s receive timeout so unanswered queries expire even when the
        // network is silent.
        let tv = libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        };
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(anyhow::anyhow!("setsockopt(SO_RCVTIMEO) failed: {}", e));
        }

        let mut pending = PendingQueries::default();
        let mut attribute = |ip: &str, port: u16| platform::attribute_dns_client(ip, port);
        let mut buf = vec![0u8; 65536];

        loop {
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                match e.kind() {
                    std::io::ErrorKind::WouldBlock => {
                        flush_expired(&mut pending, &queries);
                    }
                    std::io::ErrorKind::Interrupted => {}
                    _ => {
                        unsafe { libc::close(fd) };
                        return Err(anyhow::anyhow!("AF_PACKET recv failed: {}", e));
                    }
                }
                continue;
            }
            if let Some(ev) = dns_wire::parse_frame(&buf[..n as usize]) {
                handle_event(ev, &mut pending, &queries, &mut attribute);
            }
            flush_expired(&mut pending, &queries);
        }
    }
}

// ── `ss` output parsing (Linux, unit-tested everywhere) ──────────────────────

#[cfg(any(target_os = "linux", test))]
mod ss_parse {
    use super::NetworkConnection;

    /// Parse `ss -tanp` / `ss -uanp` output.
    ///
    /// Columns: Netid State Recv-Q Send-Q Local Address:Port Peer Address:Port Process
    pub fn parse_connections(text: &str, proto: &str) -> Vec<NetworkConnection> {
        let mut connections = vec![];
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 5 {
                continue;
            }
            let local = cols.get(4).copied().unwrap_or("").to_string();
            let remote = cols
                .get(5)
                .copied()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let state = cols.get(1).copied().map(|s| s.to_string());
            let (pid, process_name) = if cols.len() > 6 {
                parse_process(&cols[6..].join(" "))
            } else {
                (None, None)
            };

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

    /// Parse the `ss -p` process column, e.g. `users:(("firefox",pid=4821,fd=64))`.
    pub fn parse_process(text: &str) -> (Option<u32>, Option<String>) {
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

    /// (local ip:port, pid, name) rows from `ss -uanp`, for DNS client
    /// attribution.
    pub fn parse_udp_table(text: &str) -> Vec<(String, u32, String)> {
        let mut rows = vec![];
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 7 {
                continue;
            }
            let (pid, name) = parse_process(&cols[6..].join(" "));
            if let (Some(pid), Some(name)) = (pid, name) {
                rows.push((cols[4].to_string(), pid, name));
            }
        }
        rows
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_ss_with_process() {
            let text = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
                        tcp   ESTAB  0      0      192.168.1.5:51234    142.250.72.14:443 users:((\"firefox\",pid=4821,fd=64))\n";
            let c = parse_connections(text, "TCP");
            assert_eq!(c.len(), 1);
            assert_eq!(c[0].local_addr, "192.168.1.5:51234");
            assert_eq!(c[0].remote_addr.as_deref(), Some("142.250.72.14:443"));
            assert_eq!(c[0].state.as_deref(), Some("ESTAB"));
            assert_eq!(c[0].pid, Some(4821));
            assert_eq!(c[0].process_name.as_deref(), Some("firefox"));
        }

        #[test]
        fn parses_ss_without_process_column() {
            let text = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port\n\
                        tcp   ESTAB  0      0      192.168.1.5:51234    142.250.72.14:443\n";
            let c = parse_connections(text, "TCP");
            assert_eq!(c.len(), 1);
            assert_eq!(c[0].pid, None);
            assert_eq!(c[0].process_name, None);
        }

        #[test]
        fn parses_udp_table() {
            let text = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
                        udp   UNCONN 0      0      192.168.1.5:55321    192.168.1.1:53   users:((\"firefox\",pid=4821,fd=72))\n\
                        udp   UNCONN 0      0      127.0.0.53:53         0.0.0.0:*        users:((\"systemd-resolve\",pid=301,fd=12))\n";
            let rows = parse_udp_table(text);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0], ("192.168.1.5:55321".to_string(), 4821, "firefox".to_string()));
        }
    }
}

// ── Windows PowerShell output parsing (unit-tested everywhere) ───────────────

#[cfg(any(windows, test))]
mod windows_parse {
    use super::{DnsQuery, NetworkConnection};
    use chrono::Utc;
    use std::collections::HashMap;

    /// Parse `Get-NetTCPConnection` / `Get-NetUDPEndpoint` CSV output and join
    /// OwningProcess with the pid→name map. UDP rows have no remote/state.
    pub fn parse_connections(
        text: &str,
        proto: &str,
        names: &HashMap<u32, String>,
    ) -> Vec<NetworkConnection> {
        let mut connections = vec![];
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        let headers: Vec<&str> = header.split(',').map(|s| s.trim().trim_matches('"')).collect();

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split(',').map(|s| s.trim().trim_matches('"')).collect();
            let get = |name: &str| {
                headers
                    .iter()
                    .position(|h| *h == name)
                    .and_then(|i| cols.get(i))
                    .copied()
            };

            let local = format!(
                "{}:{}",
                get("LocalAddress").unwrap_or(""),
                get("LocalPort").unwrap_or("")
            );
            let remote = get("RemoteAddress").and_then(|a| {
                get("RemotePort").map(|p| format!("{}:{}", a, p))
            });
            let pid = get("OwningProcess").and_then(|s| s.parse().ok());

            connections.push(NetworkConnection {
                protocol: proto.to_string(),
                local_addr: local,
                remote_addr: remote,
                state: get("State").map(|s| s.to_string()),
                pid,
                process_name: pid.and_then(|p| names.get(&p).cloned()),
            });
        }
        connections
    }

    /// Parse `Get-Process | Select Id, ProcessName` CSV into a pid→name map.
    pub fn parse_process_names(text: &str) -> HashMap<u32, String> {
        let mut map = HashMap::new();
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        let headers: Vec<&str> = header.split(',').map(|s| s.trim().trim_matches('"')).collect();

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split(',').map(|s| s.trim().trim_matches('"')).collect();
            let get = |name: &str| {
                headers
                    .iter()
                    .position(|h| *h == name)
                    .and_then(|i| cols.get(i))
                    .copied()
            };
            if let (Some(id), Some(name)) = (get("Id"), get("ProcessName")) {
                if let Ok(pid) = id.parse::<u32>() {
                    map.insert(pid, name.to_string());
                }
            }
        }
        map
    }

    /// Parse `Get-DnsClientCache | Select Entry, Type, Data` CSV rows into
    /// query entries. `answers` holds Data only when it is an IP address.
    pub fn parse_dns_cache(text: &str) -> Vec<DnsQuery> {
        let mut queries = vec![];
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        let headers: Vec<&str> = header.split(',').map(|s| s.trim().trim_matches('"')).collect();

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split(',').map(|s| s.trim().trim_matches('"')).collect();
            let get = |name: &str| {
                headers
                    .iter()
                    .position(|h| *h == name)
                    .and_then(|i| cols.get(i))
                    .copied()
            };

            let name = get("Entry").unwrap_or("").trim_end_matches('.');
            if name.is_empty() {
                continue;
            }
            let qtype = get("Type")
                .and_then(|t| t.parse::<u16>().ok())
                .map(super::qtype_str)
                .unwrap_or_default();
            let answers = get("Data")
                .filter(|d| d.parse::<std::net::IpAddr>().is_ok())
                .map(|d| vec![d.to_string()])
                .unwrap_or_default();

            queries.push(DnsQuery {
                ts_ms: Utc::now().timestamp_millis(),
                domain: name.to_string(),
                qtype,
                answers,
                resolver: String::new(),
                pid: None,
                process: None,
            });
        }
        queries
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_tcp_connections_with_names() {
            let text = "\"LocalAddress\",\"LocalPort\",\"RemoteAddress\",\"RemotePort\",\"State\",\"OwningProcess\"\n\
                        \"192.168.1.5\",\"51234\",\"142.250.72.14\",\"443\",\"Established\",\"4821\"\n";
            let names = HashMap::from([(4821u32, "firefox".to_string())]);
            let c = parse_connections(text, "TCP", &names);
            assert_eq!(c.len(), 1);
            assert_eq!(c[0].local_addr, "192.168.1.5:51234");
            assert_eq!(c[0].remote_addr.as_deref(), Some("142.250.72.14:443"));
            assert_eq!(c[0].state.as_deref(), Some("Established"));
            assert_eq!(c[0].pid, Some(4821));
            assert_eq!(c[0].process_name.as_deref(), Some("firefox"));
        }

        #[test]
        fn parses_udp_endpoints_without_remote_or_state() {
            let text = "\"LocalAddress\",\"LocalPort\",\"OwningProcess\"\n\
                        \"192.168.1.5\",\"55321\",\"4821\"\n";
            let names = HashMap::from([(4821u32, "firefox".to_string())]);
            let c = parse_connections(text, "UDP", &names);
            assert_eq!(c.len(), 1);
            assert_eq!(c[0].remote_addr, None);
            assert_eq!(c[0].state, None);
            assert_eq!(c[0].pid, Some(4821));
        }

        #[test]
        fn unknown_pid_yields_no_name() {
            let text = "\"LocalAddress\",\"LocalPort\",\"OwningProcess\"\n\
                        \"0.0.0.0\",\"53\",\"999\"\n";
            let names = HashMap::new();
            let c = parse_connections(text, "UDP", &names);
            assert_eq!(c[0].pid, Some(999));
            assert_eq!(c[0].process_name, None);
        }

        #[test]
        fn parses_process_names() {
            let text = "\"Id\",\"ProcessName\"\n\"4821\",\"firefox\"\n\"301\",\"svchost\"\n";
            let names = parse_process_names(text);
            assert_eq!(names.get(&4821).map(String::as_str), Some("firefox"));
            assert_eq!(names.get(&301).map(String::as_str), Some("svchost"));
        }

        #[test]
        fn parses_dns_cache_rows() {
            let text = "\"Entry\",\"Type\",\"Data\"\n\
                        \"example.com\",\"1\",\"93.184.216.34\"\n\
                        \"mail.example.com\",\"15\",\"mail.example.com\"\n";
            let q = parse_dns_cache(text);
            assert_eq!(q.len(), 2);
            assert_eq!(q[0].domain, "example.com");
            assert_eq!(q[0].qtype, "A");
            assert_eq!(q[0].answers, vec!["93.184.216.34"]);
            assert_eq!(q[1].qtype, "MX");
            // MX Data is a hostname, not an IP → no answers.
            assert!(q[1].answers.is_empty());
        }
    }
}

// ── macOS implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::cell::RefCell;
    use std::process::Command;
    use std::time::Instant;

    pub fn run_dns_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        tcpdump_dns::run_capture(queries)
    }

    /// Best-effort: map a DNS client's local ip:port to its owning process via
    /// the UDP socket table (`lsof`), cached for 1s (queries arrive in bursts).
    pub fn attribute_dns_client(ip: &str, port: u16) -> (Option<u32>, Option<String>) {
        thread_local! {
            static TABLE: RefCell<(Option<Instant>, Vec<(String, u32, String)>)> =
                RefCell::new((None, Vec::new()));
        }
        TABLE.with(|t| {
            let mut t = t.borrow_mut();
            if t.0.map(|i| i.elapsed().as_secs() >= 1).unwrap_or(true) {
                t.1 = udp_socket_table();
                t.0 = Some(Instant::now());
            }
            t.1.iter()
                .find(|(local, _, _)| endpoint_matches(local, ip, port))
                .map(|(_, pid, name)| (Some(*pid), Some(name.clone())))
                .unwrap_or((None, None))
        })
    }

    /// (local ip:port, pid, name) rows from `lsof -iUDP`.
    fn udp_socket_table() -> Vec<(String, u32, String)> {
        let out = Command::new("lsof")
            .args(["-iUDP", "-n", "-P", "-F", "pcn"])
            .output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return vec![],
        };
        parse_lsof_udp_table(&text)
    }

    fn parse_lsof_udp_table(text: &str) -> Vec<(String, u32, String)> {
        let mut rows = vec![];
        let mut current_pid: Option<u32> = None;
        let mut current_name: Option<String> = None;
        for line in text.lines() {
            match line.chars().next() {
                Some('p') => current_pid = line[1..].parse().ok(),
                Some('c') => current_name = Some(line[1..].to_string()),
                Some('n') => {
                    if let (Some(pid), Some(name)) = (current_pid, current_name.clone()) {
                        rows.push((line[1..].to_string(), pid, name));
                    }
                }
                _ => {}
            }
        }
        rows
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
    use std::collections::HashMap;
    use std::process::Command;
    use std::thread;
    use std::time::Instant;

    /// Poll the Windows DNS client cache every few seconds for recently resolved
    /// names. Entries seen in the last 10 minutes are not re-reported.
    pub fn run_dns_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        let mut seen: HashMap<(String, String), Instant> = HashMap::new();
        let mut warned = false;
        loop {
            match poll_dns_client_cache() {
                Ok(rows) => {
                    let now = Instant::now();
                    seen.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
                    for q in rows {
                        let key = (q.domain.clone(), q.qtype.clone());
                        if seen.contains_key(&key) {
                            continue;
                        }
                        seen.insert(key, now);
                        super::push_query(&queries, q);
                    }
                }
                Err(e) => {
                    if !warned {
                        tracing::warn!("DNS cache polling failed: {:#}; dns_queries will stay empty", e);
                        warned = true;
                    }
                }
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
                "Get-DnsClientCache | Select-Object Entry, Type, Data | ConvertTo-Csv -NoTypeInformation",
            ])
            .output()?;

        if !out.status.success() {
            return Err(anyhow::anyhow!("Get-DnsClientCache failed"));
        }
        Ok(windows_parse::parse_dns_cache(
            &String::from_utf8_lossy(&out.stdout),
        ))
    }

    /// No packet-level capture on Windows: DNS attribution is unavailable.
    #[allow(dead_code)]
    pub fn attribute_dns_client(_: &str, _: u16) -> (Option<u32>, Option<String>) {
        (None, None)
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
        let names = windows_parse::parse_process_names(&run_ps(
            "Get-Process | Select-Object Id, ProcessName | ConvertTo-Csv -NoTypeInformation",
        ));
        let mut connections = windows_parse::parse_connections(
            &run_ps("Get-NetTCPConnection | Select-Object LocalAddress, LocalPort, RemoteAddress, RemotePort, State, OwningProcess | ConvertTo-Csv -NoTypeInformation"),
            "TCP",
            &names,
        );
        connections.extend(windows_parse::parse_connections(
            &run_ps("Get-NetUDPEndpoint | Select-Object LocalAddress, LocalPort, OwningProcess | ConvertTo-Csv -NoTypeInformation"),
            "UDP",
            &names,
        ));
        connections
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

    fn run_ps(script: &str) -> String {
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
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
}

// ── Linux implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::cell::RefCell;
    use std::process::Command;
    use std::time::Instant;

    pub fn run_dns_capture(queries: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        afpacket_dns::run_capture(queries)
    }

    /// Best-effort: map a DNS client's local ip:port to its owning process via
    /// the UDP socket table (`ss -uanp`), cached for 1s (queries arrive in
    /// bursts).
    pub fn attribute_dns_client(ip: &str, port: u16) -> (Option<u32>, Option<String>) {
        thread_local! {
            static TABLE: RefCell<(Option<Instant>, Vec<(String, u32, String)>)> =
                RefCell::new((None, Vec::new()));
        }
        TABLE.with(|t| {
            let mut t = t.borrow_mut();
            if t.0.map(|i| i.elapsed().as_secs() >= 1).unwrap_or(true) {
                t.1 = ss_parse::parse_udp_table(&run_ss("-uanp"));
                t.0 = Some(Instant::now());
            }
            t.1.iter()
                .find(|(local, _, _)| endpoint_matches(local, ip, port))
                .map(|(_, pid, name)| (Some(*pid), Some(name.clone())))
                .unwrap_or((None, None))
        })
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
        connections.extend(ss_parse::parse_connections(&run_ss("-tanp"), "TCP"));
        connections.extend(ss_parse::parse_connections(&run_ss("-uanp"), "UDP"));
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
}

// ── Other Unix stub (no network collection support) ───────────────────────────

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod platform {
    use super::*;

    pub fn run_dns_capture(_: Arc<Mutex<Vec<DnsQuery>>>) -> anyhow::Result<()> {
        anyhow::bail!("DNS capture is not supported on this platform")
    }

    #[allow(dead_code)]
    pub fn attribute_dns_client(_: &str, _: u16) -> (Option<u32>, Option<String>) {
        (None, None)
    }

    pub fn interfaces() -> Vec<NetworkInterface> {
        vec![]
    }

    pub fn connections() -> Vec<NetworkConnection> {
        vec![]
    }

    pub fn dns_servers() -> Vec<String> {
        vec![]
    }
}

// ── Live capture smoke test (macOS, requires root for tcpdump) ───────────────

#[cfg(all(test, target_os = "macos"))]
mod live_tests {
    use super::*;

    /// End-to-end DNS capture. Run with:
    ///   sudo env CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" \
    ///     CARGO_TARGET_DIR=/tmp/ar-target cargo test -p auditready -- --ignored
    #[test]
    #[ignore]
    fn captures_live_dns_query() {
        let capture = DnsCapture::start();
        // Give tcpdump a moment to attach before generating traffic.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let _ = std::process::Command::new("dig")
            .args(["example.com", "+time=3", "+tries=1"])
            .output();
        std::thread::sleep(std::time::Duration::from_secs(2));

        let queries = capture.drain();
        assert!(
            queries.iter().any(|q| q.domain == "example.com"),
            "expected example.com in captured queries: {:?}",
            queries.iter().map(|q| &q.domain).collect::<Vec<_>>()
        );
        let q = queries
            .iter()
            .find(|q| q.domain == "example.com")
            .unwrap();
        assert_eq!(q.qtype, "A");
        assert!(!q.resolver.is_empty());
        assert!(q.ts_ms > 0);
    }
}
