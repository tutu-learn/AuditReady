use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;
use sysinfo::{Pid, Process, System};

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ProcessEvent {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub exe: Option<String>,
    /// Full argv — captured from ETW (Windows) / ESF (macOS) kernel source
    pub cmdline: Vec<String>,
    pub is_elevated: bool,
    pub captured_at: DateTime<Utc>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Allowed,
    /// Elevated process not on the whitelist — flagged for review
    Flagged,
    /// On the blacklist — killed immediately
    Blacklisted,
}

impl Serialize for Verdict {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Verdict::Allowed => "allowed",
            Verdict::Flagged => "flagged",
            Verdict::Blacklisted => "blacklisted",
        })
    }
}

// ── Rules engine ──────────────────────────────────────────────────────────────

pub struct RulesEngine {
    whitelist: HashSet<String>,
    blacklist: HashSet<String>,
}

impl RulesEngine {
    pub fn new(whitelist: Vec<String>, blacklist: Vec<String>) -> Self {
        Self {
            whitelist: whitelist.into_iter().map(|s| s.to_lowercase()).collect(),
            blacklist: blacklist.into_iter().map(|s| s.to_lowercase()).collect(),
        }
    }

    pub fn verdict(&self, name: &str, is_elevated: bool) -> Verdict {
        let lower = name.to_lowercase();
        if self.blacklist.contains(&lower) {
            return Verdict::Blacklisted;
        }
        if is_elevated && !self.whitelist.contains(&lower) {
            return Verdict::Flagged;
        }
        Verdict::Allowed
    }
}

// ── Snapshot ──────────────────────────────────────────────────────────────────

/// Point-in-time process snapshot.
/// Killing on blacklist match is reactive (poll-based).
/// True on-launch interception requires ETW (Windows) or ESF (macOS) daemons
/// with the appropriate kernel entitlements / provider subscriptions.
pub fn snapshot(engine: &RulesEngine) -> Vec<ProcessEvent> {
    let mut sys = System::new_all();
    sys.refresh_all();
    let now = Utc::now();

    let mut events: Vec<ProcessEvent> = sys
        .processes()
        .iter()
        .map(|(&pid, proc)| build_event(pid, proc, engine, now))
        .collect();

    for event in &events {
        if event.verdict == Verdict::Blacklisted {
            platform::kill(event.pid);
        }
    }

    events.sort_by_key(|e| e.pid);
    events
}

fn build_event(pid: Pid, proc: &Process, engine: &RulesEngine, now: DateTime<Utc>) -> ProcessEvent {
    let name = proc.name().to_string_lossy().to_string();
    let exe = proc.exe().map(|p| p.display().to_string());
    let cmdline: Vec<String> = proc
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy().to_string())
        .collect();
    let ppid = proc.parent().map(|p| p.as_u32());
    let is_elevated = platform::is_elevated(pid.as_u32(), proc);
    let verdict = engine.verdict(&name, is_elevated);

    ProcessEvent {
        pid: pid.as_u32(),
        ppid,
        name,
        exe,
        cmdline,
        is_elevated,
        captured_at: now,
        verdict,
    }
}

// ── Platform implementations ──────────────────────────────────────────────────

/// macOS / Linux: elevation via ESF / procfs UID (effective user id == root)
#[cfg(unix)]
mod platform {
    use sysinfo::Process;

    pub fn is_elevated(_pid: u32, proc: &Process) -> bool {
        proc.user_id()
            .map(|uid| {
                let v: u32 = **uid;
                v == 0
            })
            .unwrap_or(false)
    }

    /// SIGKILL via libc — mirrors the `kill` crate approach
    pub fn kill(pid: u32) {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// Windows: elevation via WMI token query, kill via TerminateProcess
#[cfg(windows)]
mod platform {
    use sysinfo::Process;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, TerminateProcess, PROCESS_QUERY_INFORMATION, PROCESS_TERMINATE,
    };

    pub fn is_elevated(pid: u32, _proc: &Process) -> bool {
        unsafe {
            let Ok(proc_handle) = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) else {
                return false;
            };
            let mut token = HANDLE::default();
            if OpenProcessToken(proc_handle, TOKEN_QUERY, &mut token).is_err() {
                let _ = CloseHandle(proc_handle);
                return false;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut ret_len = 0u32;
            let elevated = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
            .is_ok()
                && elevation.TokenIsElevated != 0;
            let _ = CloseHandle(token);
            let _ = CloseHandle(proc_handle);
            elevated
        }
    }

    pub fn kill(pid: u32) {
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use sysinfo::Process;

    pub fn is_elevated(_pid: u32, _proc: &Process) -> bool {
        false
    }

    pub fn kill(_pid: u32) {}
}
