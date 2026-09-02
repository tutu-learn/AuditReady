//! Shared, live view of client-mode activity: connection health and running
//! totals for the tray/dashboard UI (`super::ui`) to poll and render.
//!
//! Updated from two independent report loops (`client::runner` for the
//! client report, `publisher` for telemetry) via [`record_client_report`] /
//! [`record_telemetry`] / [`record_failure`], which return an [`Alert`]
//! telling the caller whether to raise (or clear) a modal popup.
//!
//! A disconnect only becomes an [`Alert::Disconnected`] once the outage has
//! lasted [`DISCONNECT_ALERT_AFTER`] — short blips (a Wi-Fi hiccup, a server
//! restart) recover before that and never surface a popup at all. A
//! reconnect only becomes an [`Alert::Reconnected`] if the disconnect was
//! actually alerted on, so a blip that never crossed the threshold doesn't
//! get a "connection restored" popup either.

use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};

/// How long a report/telemetry failure must persist, continuously, before
/// the connection-lost popup fires.
const DISCONNECT_ALERT_AFTER_MINUTES: i64 = 10;

#[derive(Debug, Clone)]
pub struct ClientStats {
    pub connected: bool,
    /// When the current outage started; `None` while connected.
    pub disconnected_since: Option<DateTime<Utc>>,
    /// Whether the current outage has already crossed the alert threshold
    /// (so we don't re-alert every cycle while it continues).
    alerted: bool,

    pub last_report_at: Option<DateTime<Utc>>,
    pub next_report_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,

    /// Cumulative totals since this process started, incremented by each
    /// client report cycle.
    pub clipboard_events: u64,
    pub mouse_events: u64,
    pub files_scanned: u64,
    pub sensitive_hits: u64,

    /// Latest telemetry snapshot (from `publisher`), replaced each cycle
    /// rather than accumulated.
    pub total_processes: usize,
    pub flagged_processes: usize,
    pub network_connections: usize,
}

impl Default for ClientStats {
    fn default() -> Self {
        Self {
            connected: true,
            disconnected_since: None,
            alerted: false,
            last_report_at: None,
            next_report_at: None,
            last_error: None,
            clipboard_events: 0,
            mouse_events: 0,
            files_scanned: 0,
            sensitive_hits: 0,
            total_processes: 0,
            flagged_processes: 0,
            network_connections: 0,
        }
    }
}

pub type SharedStats = Arc<Mutex<ClientStats>>;

pub fn new_shared() -> SharedStats {
    Arc::new(Mutex::new(ClientStats::default()))
}

/// What the caller should do after a stats update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    None,
    /// The outage has just crossed the alert threshold; show the
    /// connection-lost popup with this error text.
    Disconnected(String),
    /// A previously-alerted outage just cleared; show the connection-restored
    /// popup.
    Reconnected,
}

/// Marks the connection healthy and returns whether that clears a standing
/// alert. Shared by both report loops' success paths.
fn mark_connected(s: &mut ClientStats) -> Alert {
    let alert = if s.alerted {
        Alert::Reconnected
    } else {
        Alert::None
    };
    s.connected = true;
    s.disconnected_since = None;
    s.alerted = false;
    s.last_error = None;
    alert
}

/// Record a successful client-report cycle.
pub fn record_client_report(
    shared: &SharedStats,
    next_report_at: DateTime<Utc>,
    clipboard_events: u64,
    mouse_events: u64,
    files_scanned: u64,
    sensitive_hits: u64,
) -> Alert {
    let mut s = shared.lock().unwrap();
    let alert = mark_connected(&mut s);
    s.last_report_at = Some(Utc::now());
    s.next_report_at = Some(next_report_at);
    s.clipboard_events += clipboard_events;
    s.mouse_events += mouse_events;
    s.files_scanned += files_scanned;
    s.sensitive_hits += sensitive_hits;
    alert
}

/// Record a successful telemetry cycle (process/network counts only; does
/// not touch the client-report fields).
pub fn record_telemetry(
    shared: &SharedStats,
    total_processes: usize,
    flagged_processes: usize,
    network_connections: usize,
) -> Alert {
    let mut s = shared.lock().unwrap();
    let alert = mark_connected(&mut s);
    s.total_processes = total_processes;
    s.flagged_processes = flagged_processes;
    s.network_connections = network_connections;
    alert
}

/// Record a failed report/telemetry cycle. Returns `Alert::Disconnected`
/// only the first time the ongoing outage crosses
/// `DISCONNECT_ALERT_AFTER_MINUTES`; every other failed cycle (before or
/// after that point) returns `Alert::None`.
pub fn record_failure(shared: &SharedStats, error: String) -> Alert {
    let mut s = shared.lock().unwrap();
    s.last_error = Some(error.clone());
    let now = Utc::now();

    if s.connected {
        // First failure of a fresh outage: start the clock, don't alert yet.
        s.connected = false;
        s.disconnected_since = Some(now);
        s.alerted = false;
        return Alert::None;
    }

    if !s.alerted {
        if let Some(since) = s.disconnected_since {
            if now - since >= chrono::Duration::minutes(DISCONNECT_ALERT_AFTER_MINUTES) {
                s.alerted = true;
                return Alert::Disconnected(error);
            }
        }
    }
    Alert::None
}

pub fn snapshot(shared: &SharedStats) -> ClientStats {
    shared.lock().unwrap().clone()
}
