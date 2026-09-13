//! Reusable script execution helper used by patch jobs and deployment pushes.
//!
//! Runs a bash/PowerShell script in a temp file, captures combined stdout+stderr,
//! and reports progress pings with a live output tail while the script runs.

use anyhow::Context;
use chrono::Utc;
use std::time::Duration;

/// Max wall-clock time for a script before it is killed.
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Interval between progress pings while a script runs.
pub const SCRIPT_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Cap for the script output tail sent in terminal reports.
pub const OUTPUT_CAP: usize = 60000;
/// Cap for the live output tail riding progress pings.
pub const LIVE_TAIL_CAP: usize = 16000;

/// Outcome of a script run. `error` is set when the agent itself failed to run
/// the script (temp file, spawn); `timed_out` when the script was killed after
/// `SCRIPT_TIMEOUT`. `output_tail` always carries the last chunk of combined
/// stdout/stderr.
pub struct ScriptOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub error: Option<String>,
    pub output_tail: String,
}

/// Write `script` to a temp file and run it — `bash` on unix, `powershell.exe
/// -File` on Windows — capturing combined stdout+stderr. `ping` is called every
/// `ping_interval` while the script runs, with the live tail of the output
/// captured so far (capped at `LIVE_TAIL_CAP`). The temp files are always
/// deleted afterwards.
pub fn run_script(
    script: &str,
    ping_interval: Duration,
    mut ping: impl FnMut(&str),
) -> ScriptOutcome {
    let fail = |outcome: &mut ScriptOutcome, e: anyhow::Error| {
        tracing::error!("script run failed: {:#}", e);
        outcome.error = Some(format!("{:#}", e));
    };

    let mut outcome = ScriptOutcome {
        exit_code: None,
        timed_out: false,
        error: None,
        output_tail: String::new(),
    };

    let dir = std::env::temp_dir().join("auditready");
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = format!(
        "{}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    #[cfg(windows)]
    let script_path = dir.join(format!("script-{}.ps1", stamp));
    #[cfg(not(windows))]
    let script_path = dir.join(format!("script-{}.sh", stamp));
    let log_path = dir.join(format!("script-{}.log", stamp));

    'run: {
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&script_path, script_file_bytes(script)))
            .context("failed to write script temp file")
        {
            fail(&mut outcome, e);
            break 'run;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(
                &script_path,
                std::fs::Permissions::from_mode(0o700),
            )
            .context("failed to chmod script temp file")
            {
                fail(&mut outcome, e);
                break 'run;
            }
        }

        let log = match std::fs::File::create(&log_path)
            .and_then(|f| f.try_clone().map(|f2| (f, f2)))
            .context("failed to create script log file")
        {
            Ok((out, err)) => (out, err),
            Err(e) => {
                fail(&mut outcome, e);
                break 'run;
            }
        };

        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("powershell.exe");
            c.arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(&script_path);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = std::process::Command::new("bash");
            c.arg(&script_path);
            c
        };
        cmd.stdin(std::process::Stdio::null())
            .stdout(log.0)
            .stderr(log.1)
            .env("DEBIAN_FRONTEND", "noninteractive");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = match cmd.spawn().context("failed to spawn script interpreter") {
            Ok(c) => c,
            Err(e) => {
                fail(&mut outcome, e);
                break 'run;
            }
        };

        let start = std::time::Instant::now();
        let mut last_ping = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    outcome.exit_code = status.code();
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    fail(&mut outcome, anyhow::Error::new(e).context("failed to poll script"));
                    break;
                }
            }
            if start.elapsed() > SCRIPT_TIMEOUT {
                tracing::error!("script exceeded {:?}; killing it", SCRIPT_TIMEOUT);
                kill_script(&mut child);
                let _ = child.wait();
                outcome.timed_out = true;
                break;
            }
            if last_ping.elapsed() >= ping_interval {
                let live_tail = std::fs::read(&log_path)
                    .map(|bytes| tail_chars(&String::from_utf8_lossy(&bytes), LIVE_TAIL_CAP))
                    .unwrap_or_default();
                ping(&live_tail);
                last_ping = std::time::Instant::now();
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    if let Ok(bytes) = std::fs::read(&log_path) {
        outcome.output_tail = tail_chars(&String::from_utf8_lossy(&bytes), OUTPUT_CAP);
    }
    if let Err(e) = std::fs::remove_file(&script_path) {
        tracing::warn!("failed to delete {}: {}", script_path.display(), e);
    }
    if let Err(e) = std::fs::remove_file(&log_path) {
        tracing::warn!("failed to delete {}: {}", log_path.display(), e);
    }
    outcome
}

/// Bytes written to the script temp file. Windows PowerShell 5.1 reads a
/// BOM-less .ps1 as ANSI, mangling any non-ASCII; prefix a UTF-8 BOM so both
/// 5.1 and 7+ decode the file as UTF-8.
#[cfg(windows)]
fn script_file_bytes(script: &str) -> Vec<u8> {
    let mut bytes = b"\xef\xbb\xbf".to_vec();
    bytes.extend_from_slice(script.as_bytes());
    bytes
}

#[cfg(not(windows))]
fn script_file_bytes(script: &str) -> &[u8] {
    script.as_bytes()
}

/// Kill a timed-out script. On unix the script runs in its own process group,
/// so kill the whole group.
#[cfg(unix)]
fn kill_script(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_script(child: &mut std::process::Child) {
    let _ = child.kill();
}

/// Keep the last `cap` chars of `text` (char-boundary safe), with a "..."
/// prefix when truncated.
pub fn tail_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .rev()
        .take(cap)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{}", tail)
}

