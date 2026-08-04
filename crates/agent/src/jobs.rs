//! Patch job polling and execution.
//!
//! Polls `POST /audit_ready/agent/poll`, executes patch jobs one at a time
//! — Package Update / OS Update via the platform package manager, Script jobs
//! by running the script carried in `package` (bash on unix, PowerShell on
//! Windows) — and reports
//! progress/results to `POST /audit_ready/agent/patch-result`.
//!
//! Job delivery is at-least-once: a server restart can re-deliver a completed
//! job. A small on-disk state file (`patch-state.json`) records completed
//! jobs (re-delivered jobs get their terminal status re-reported, never a
//! reinstall) and any in-flight job, so a reboot or crash mid-job loses
//! nothing: after the agent comes back it verifies the outcome and sends the
//! terminal report that closes the job on the server.

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Seconds between job polls when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// How many completed job names to remember for at-least-once dedup.
const MAX_COMPLETED: usize = 200;
/// Cap for package-manager stderr/stdout summaries in failure reports
/// (the server itself caps `error` at 5000 chars).
const SUMMARY_CAP: usize = 1000;
/// If a reboot doesn't happen within this long after being requested
/// (e.g. it was cancelled), verify and close the job anyway.
const REBOOT_STALL: Duration = Duration::from_secs(900);
/// Max wall-clock time for a Script job before it is killed
/// (kubeadm upgrades download packages, so keep this generous).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Interval between progress pings while a Script job runs.
const SCRIPT_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Cap for the script output tail sent as `output` (the server caps at 64000).
const OUTPUT_CAP: usize = 60000;

// ── Wire types ──────────────────────────────────────────────────────────────

/// A patch job as returned by the poll endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub from_version: String,
    #[serde(default)]
    pub to_version: String,
    #[serde(default)]
    pub job_type: String,
    #[serde(default)]
    pub scheduled_at: String,
    #[serde(default)]
    pub reboot_required: bool,
    #[serde(default)]
    pub update_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    #[serde(default)]
    jobs: Vec<Job>,
}

// ── Persistent state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletedJob {
    name: String,
    status: String,
    result: String,
    error: String,
    /// Output tail for Script jobs; empty for package/OS jobs.
    #[serde(default)]
    output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InFlightJob {
    job: Job,
    /// True after updates were installed and a reboot was requested; the
    /// terminal report is sent once the machine is back and verified.
    reboot_pending: bool,
    /// Unix seconds when the reboot was requested (stall detection).
    reboot_marked_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    in_flight: Option<InFlightJob>,
    completed: Vec<CompletedJob>,
}

impl State {
    fn load(path: &PathBuf) -> State {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &PathBuf) {
        let tmp = path.with_extension("json.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, path))
                {
                    tracing::error!("failed to persist job state: {}", e);
                }
            }
            Err(e) => tracing::error!("failed to serialize job state: {}", e),
        }
    }

    fn record_completed(&mut self, name: &str, status: &str, result: &str, error: &str, output: &str) {
        self.completed.retain(|c| c.name != name);
        self.completed.push(CompletedJob {
            name: name.to_string(),
            status: status.to_string(),
            result: result.to_string(),
            error: error.to_string(),
            output: output.to_string(),
        });
        if self.completed.len() > MAX_COMPLETED {
            let excess = self.completed.len() - MAX_COMPLETED;
            self.completed.drain(..excess);
        }
    }

    fn completed_job(&self, name: &str) -> Option<&CompletedJob> {
        self.completed.iter().find(|c| c.name == name)
    }
}

/// Directory for `patch-state.json`. Prefers a system location (the agent
/// service usually runs privileged); falls back to the user home and finally
/// the temp dir. `AUDITREADY_STATE_DIR` overrides everything.
fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AUDITREADY_STATE_DIR") {
        if !dir.is_empty() && std::fs::create_dir_all(&dir).is_ok() {
            return PathBuf::from(dir);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        if let Ok(pd) = std::env::var("ProgramData") {
            candidates.push(PathBuf::from(pd).join("AuditReady"));
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            candidates.push(PathBuf::from(profile).join(".auditready"));
        }
    }
    #[cfg(not(windows))]
    {
        candidates.push(PathBuf::from("/var/lib/auditready"));
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(home).join(".auditready"));
        }
    }
    candidates.push(std::env::temp_dir().join("auditready"));

    for dir in &candidates {
        if std::fs::create_dir_all(dir).is_ok() {
            return dir.clone();
        }
    }
    // Should be unreachable (temp dir is last), but never panic here.
    PathBuf::from(".")
}

// ── HTTP client ─────────────────────────────────────────────────────────────

struct Client {
    base: String,
    token: String,
}

impl Client {
    fn new(domain: &str, token: &str) -> Client {
        Client {
            base: build_base(domain),
            token: token.to_string(),
        }
    }

    fn request(&self, path: &str) -> ureq::Request {
        let mut req = ureq::post(&format!("{}/audit_ready/{}", self.base, path))
            .set("Content-Type", "application/json")
            .set("User-Agent", "AuditReady/0.1")
            .timeout(Duration::from_secs(60));
        if !self.token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", self.token));
        }
        req
    }

    fn poll(&self) -> anyhow::Result<Vec<Job>> {
        let body = serde_json::to_string(&serde_json::json!({ "limit": 10 }))?;
        let resp = self.request("agent/poll").send_string(&body)?;
        let text = resp.into_string().context("poll response not UTF-8")?;
        let parsed: PollResponse =
            serde_json::from_str(&text).context("invalid poll response")?;
        Ok(parsed.jobs)
    }

    /// Send a patch-result report, retrying on transport errors and 5xx
    /// (result reporting is idempotent on the server).
    fn report(
        &self,
        name: &str,
        status: &str,
        progress: Option<u32>,
        result: Option<&str>,
        error: Option<&str>,
        output: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({ "name": name, "status": status });
        if let Some(p) = progress {
            body["progress"] = p.into();
        }
        if let Some(r) = result {
            body["result"] = r.into();
        }
        if let Some(e) = error {
            body["error"] = e.into();
        }
        if let Some(o) = output.filter(|o| !o.is_empty()) {
            body["output"] = o.into();
        }

        let mut last_err = anyhow!("no attempts made");
        let body = serde_json::to_string(&body)?;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1 << attempt)); // 2s, 4s, 8s, 16s
            }
            match self.request("agent/patch-result").send_string(&body) {
                Ok(_) => return Ok(()),
                Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) => {
                    // 4xx is a request problem; retrying won't help.
                    return Err(anyhow!("patch-result rejected with HTTP {}", code));
                }
                Err(e) => {
                    tracing::warn!("patch-result attempt {} failed: {}", attempt + 1, e);
                    last_err = anyhow!("{}", e);
                }
            }
        }
        Err(last_err.context("patch-result failed after retries"))
    }

    /// Best-effort progress ping; failures are logged, never fatal.
    fn progress(&self, name: &str, progress: u32) {
        if let Err(e) = self.report(name, "Running", Some(progress), None, None, None) {
            tracing::warn!("progress ping for {} failed: {}", name, e);
        }
    }

    /// Terminal report; retried internally, and once more by the caller's
    /// caller via the completed-record re-report path if it ultimately fails.
    /// `output` is the Script-job output tail; `None`/empty for other types.
    fn terminal(
        &self,
        name: &str,
        status: &str,
        result: &str,
        error: &str,
        output: Option<&str>,
    ) -> anyhow::Result<()> {
        self.report(name, status, None, Some(result), Some(error), output)
    }
}

fn build_base(domain: &str) -> String {
    let domain = domain.trim();
    if domain.starts_with("http://") || domain.starts_with("https://") {
        return domain.trim_end_matches('/').to_string();
    }
    let is_local = domain.starts_with("localhost") || domain.starts_with("127.") || domain == "::1";
    let scheme = if is_local { "http" } else { "https" };
    format!("{}://{}", scheme, domain)
}

// ── Scheduling ──────────────────────────────────────────────────────────────

/// How long to wait before a scheduled job is due. `None` means run now
/// (empty, unparsable, or already in the past).
fn scheduled_delay(scheduled_at: &str) -> Option<Duration> {
    let s = scheduled_at.trim();
    if s.is_empty() {
        return None;
    }
    let dt: Option<DateTime<Utc>> = DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|d| d.and_utc())
        })
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|d| d.and_utc())
        });
    dt.and_then(|d| (d - Utc::now()).to_std().ok())
}

// ── Main loop ───────────────────────────────────────────────────────────────

/// Run the job polling loop forever.
pub fn run(domain: &str, token: &str) -> anyhow::Result<()> {
    let client = Client::new(domain, token);
    let state_path = state_dir().join("patch-state.json");
    let mut state = State::load(&state_path);

    // Resume an in-flight job from before a reboot/crash/restart.
    if let Some(in_flight) = state.in_flight.clone() {
        if in_flight.reboot_pending {
            finalize_after_reboot(&client, &mut state, &state_path, &in_flight.job);
        } else {
            tracing::info!("resuming interrupted job {}", in_flight.job.name);
            execute_job(&client, &mut state, &state_path, in_flight.job);
        }
    }

    loop {
        // Waiting for a reboot: don't pick up new work. If the reboot never
        // happened (cancelled/failed silently), close the job after a stall.
        if let Some(in_flight) = state.in_flight.clone() {
            if in_flight.reboot_pending {
                let marked = DateTime::from_timestamp(in_flight.reboot_marked_at, 0)
                    .unwrap_or_else(Utc::now);
                if (Utc::now() - marked).to_std().unwrap_or_default() > REBOOT_STALL {
                    tracing::warn!(
                        "reboot for {} did not happen; verifying and closing job",
                        in_flight.job.name
                    );
                    finalize_after_reboot(&client, &mut state, &state_path, &in_flight.job);
                }
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
        }

        match client.poll() {
            Ok(jobs) => {
                for job in jobs {
                    handle_job(&client, &mut state, &state_path, job);
                }
            }
            Err(e) => tracing::warn!("job poll failed: {}", e),
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn handle_job(client: &Client, state: &mut State, state_path: &PathBuf, job: Job) {
    // At-least-once dedup: already finished → just re-report the terminal
    // status (idempotent on the server), never reinstall.
    if let Some(done) = state.completed_job(&job.name).cloned() {
        tracing::info!("job {} re-delivered after completion; re-reporting", job.name);
        if let Err(e) = client.terminal(
            &done.name,
            &done.status,
            &done.result,
            &done.error,
            Some(&done.output),
        ) {
            tracing::error!("re-report for {} failed: {}", done.name, e);
        }
        return;
    }
    execute_job(client, state, state_path, job);
}

fn execute_job(client: &Client, state: &mut State, state_path: &PathBuf, job: Job) {
    tracing::info!(
        "executing job {} ({} {})",
        job.name,
        job.job_type,
        job.package
    );

    // Persist before doing anything so a crash/restart can resume.
    if state.in_flight.as_ref().map(|f| &f.job.name) != Some(&job.name) {
        state.in_flight = Some(InFlightJob {
            job: job.clone(),
            reboot_pending: false,
            reboot_marked_at: 0,
        });
        state.save(state_path);
    }

    // Honor scheduled_at: wait until the scheduled time before executing.
    if let Some(delay) = scheduled_delay(&job.scheduled_at) {
        tracing::info!("job {} scheduled in {:?}; waiting", job.name, delay);
        client.progress(&job.name, 5);
        std::thread::sleep(delay);
    }

    client.progress(&job.name, 10);

    // Script jobs run the bash script in `package` and report by exit code;
    // they don't go through package verification or reboot handling.
    if job.job_type == "Script" {
        execute_script_job(client, state, state_path, &job);
        return;
    }

    let install = match job.job_type.as_str() {
        "Package Update" => exec::package_upgrade(&job.package, &job.to_version),
        "OS Update" => {
            let total = job.update_ids.len().max(1) as u32;
            let mut result = Ok(());
            for (i, id) in job.update_ids.iter().enumerate() {
                if let Err(e) = exec::os_update_one(id) {
                    result = Err(e.context(format!("failed to install update {}", id)));
                    break;
                }
                client.progress(&job.name, 10 + ((i as u32 + 1) * 60 / total));
            }
            if job.update_ids.is_empty() {
                result = Err(anyhow!("OS Update job has no update_ids"));
            }
            result
        }
        other => Err(anyhow!("unsupported job type: {}", other)),
    };

    if let Err(e) = install {
        finish(client, state, state_path, &job, "Failed", "", &format!("{:#}", e), None);
        return;
    }
    client.progress(&job.name, 70);

    if job.reboot_required {
        // Install done; reboot, then verify + terminal report after we're back.
        client.progress(&job.name, 95);
        state.in_flight = Some(InFlightJob {
            job: job.clone(),
            reboot_pending: true,
            reboot_marked_at: Utc::now().timestamp(),
        });
        state.save(state_path);
        match exec::trigger_reboot() {
            Ok(()) => {
                tracing::info!("reboot requested to finish job {}", job.name);
            }
            Err(e) => {
                finish(
                    client,
                    state,
                    state_path,
                    &job,
                    "Failed",
                    "",
                    &format!("updates installed but reboot failed: {:#}", e),
                    None,
                );
            }
        }
        return;
    }

    client.progress(&job.name, 90);
    match verify_outcome(&job) {
        Ok(summary) => finish(client, state, state_path, &job, "Success", &summary, "", None),
        Err(e) => finish(client, state, state_path, &job, "Failed", "", &format!("{:#}", e), None),
    }
}

/// After a reboot (or a stalled reboot): verify the outcome and close the job.
fn finalize_after_reboot(client: &Client, state: &mut State, state_path: &PathBuf, job: &Job) {
    match verify_outcome(job) {
        Ok(summary) => finish(client, state, state_path, job, "Success", &summary, "", None),
        Err(e) => finish(
            client,
            state,
            state_path,
            job,
            "Failed",
            "",
            &format!("post-reboot verification failed: {:#}", e),
            None,
        ),
    }
}

/// Verify that the job's changes actually landed on the machine.
fn verify_outcome(job: &Job) -> anyhow::Result<String> {
    match job.job_type.as_str() {
        "Package Update" => {
            let installed = exec::verify_package(&job.package, &job.from_version, &job.to_version)?;
            Ok(if job.from_version.is_empty() {
                format!("{} installed at {}", job.package, installed)
            } else {
                format!("{} {} -> {}", job.package, job.from_version, installed)
            })
        }
        "OS Update" => exec::verify_os_updates(&job.update_ids),
        other => Err(anyhow!("cannot verify job type: {}", other)),
    }
}

/// Send the terminal report, record the job as completed, clear in-flight.
/// `output` is the Script-job output tail; `None` for package/OS jobs.
#[allow(clippy::too_many_arguments)]
fn finish(
    client: &Client,
    state: &mut State,
    state_path: &PathBuf,
    job: &Job,
    status: &str,
    result: &str,
    error: &str,
    output: Option<&str>,
) {
    match client.terminal(&job.name, status, result, error, output) {
        Ok(()) => {
            state.record_completed(&job.name, status, result, error, output.unwrap_or(""));
            state.in_flight = None;
            state.save(state_path);
            tracing::info!("job {} finished: {}", job.name, status);
        }
        Err(e) => {
            // Keep the job in flight; the next agent restart will resume it
            // and re-report. The server never expires a Running job.
            tracing::error!("terminal report for {} failed: {}", job.name, e);
        }
    }
}

// ── Script jobs ─────────────────────────────────────────────────────────────

/// Outcome of a Script job run. `error` is set when the agent itself failed
/// to run the script (temp file, spawn); `timed_out` when the script was
/// killed after SCRIPT_TIMEOUT. `output_tail` always carries the last chunk
/// of combined stdout/stderr — it is the only console visibility operators
/// have, so it is sent on success and failure alike.
struct ScriptOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    error: Option<String>,
    output_tail: String,
}

/// Run a Script job: the script rides in `job.package` — a bash script on
/// unix, a PowerShell script on Windows (e.g. the server-rendered IIS deploy
/// and config scripts). Reports Done on exit 0, Failed otherwise; the
/// terminal report always carries the output tail.
fn execute_script_job(client: &Client, state: &mut State, state_path: &PathBuf, job: &Job) {
    if job.package.trim().is_empty() {
        finish(
            client,
            state,
            state_path,
            job,
            "Failed",
            "",
            "Script job has no script in `package`",
            None,
        );
        return;
    }

    let mut pings: u32 = 0;
    let outcome = run_script(&job.package, || {
        pings += 1;
        // Ramp 20 → 90 over the first pings, then hold.
        client.progress(&job.name, (20 + pings * 10).min(90));
    });

    let (status, result, error) = if let Some(e) = &outcome.error {
        ("Failed", String::new(), e.clone())
    } else if outcome.timed_out {
        (
            "Failed",
            String::new(),
            format!("script timed out after {} minutes", SCRIPT_TIMEOUT.as_secs() / 60),
        )
    } else if outcome.exit_code == Some(0) {
        ("Done", "script completed".to_string(), String::new())
    } else {
        let error = match outcome.exit_code {
            Some(code) => format!("script exited {}", code),
            None => "script killed by signal".to_string(),
        };
        ("Failed", String::new(), error)
    };
    finish(
        client,
        state,
        state_path,
        job,
        status,
        &result,
        &error,
        Some(&outcome.output_tail),
    );
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

/// Write `script` to a temp file (0700 on unix) and run it — `bash` on unix,
/// `powershell.exe -File` on Windows — capturing combined stdout+stderr.
/// `ping` is called every SCRIPT_PING_INTERVAL while the script runs. The
/// temp files are always deleted afterwards.
fn run_script(script: &str, mut ping: impl FnMut()) -> ScriptOutcome {
    let fail = |outcome: &mut ScriptOutcome, e: anyhow::Error| {
        tracing::error!("script job failed to run: {:#}", e);
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
            // The unix scripts run apt; never let debconf prompt on a dead stdin.
            .env("DEBIAN_FRONTEND", "noninteractive");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group, so a timeout kills apt/kubeadm children too.
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
            if last_ping.elapsed() >= SCRIPT_PING_INTERVAL {
                ping();
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

/// Kill a timed-out script. On unix the script runs in its own process group,
/// so kill the whole group (bash may have spawned apt, kubeadm, …).
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
fn tail_chars(text: &str, cap: usize) -> String {
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

// ── Platform execution ──────────────────────────────────────────────────────

/// Truncate a package-manager log dump to a short summary for `error`.
fn summarize(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.chars().count() <= SUMMARY_CAP {
        text.to_string()
    } else {
        // Keep the tail: the actual error is usually at the end.
        let tail: String = text
            .chars()
            .rev()
            .take(SUMMARY_CAP)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("...{}", tail)
    }
}

fn run_checked(program: &str, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("failed to run {}", program))?;
    if !out.status.success() {
        let stderr = summarize(&out.stderr);
        let detail = if stderr.is_empty() {
            summarize(&out.stdout)
        } else {
            stderr
        };
        bail!("{} exited with {}: {}", program, out.status, detail);
    }
    Ok(out)
}

#[cfg(all(unix, not(target_os = "macos")))]
mod exec {
    use super::{bail, run_checked, summarize, Context};
    use std::process::{Command, Stdio};

    #[derive(Clone, Copy, PartialEq)]
    enum Pm {
        Apt,
        Dnf,
    }

    fn package_manager() -> anyhow::Result<Pm> {
        if command_exists("apt-get") {
            Ok(Pm::Apt)
        } else if command_exists("dnf") {
            Ok(Pm::Dnf)
        } else {
            bail!("no supported package manager (apt-get or dnf) found")
        }
    }

    fn command_exists(name: &str) -> bool {
        Command::new(name)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn package_upgrade(package: &str, _to_version: &str) -> anyhow::Result<()> {
        if package.is_empty() {
            bail!("Package Update job has no package name");
        }
        match package_manager()? {
            Pm::Apt => {
                Command::new("apt-get")
                    .args(["install", "--only-upgrade", "-y", package])
                    .env("DEBIAN_FRONTEND", "noninteractive")
                    .stdin(Stdio::null())
                    .output()
                    .context("failed to run apt-get")
                    .and_then(|out| {
                        if out.status.success() {
                            Ok(())
                        } else {
                            bail!("apt-get failed: {}", summarize(&out.stderr))
                        }
                    })
            }
            Pm::Dnf => {
                run_checked("dnf", &["upgrade", "-y", package]).map(|_| ())
            }
        }
    }

    pub fn os_update_one(id: &str) -> anyhow::Result<()> {
        package_upgrade(id, "")
    }

    pub fn verify_package(
        package: &str,
        from_version: &str,
        to_version: &str,
    ) -> anyhow::Result<String> {
        let installed = installed_version(package)
            .ok_or_else(|| anyhow::anyhow!("{} is not installed after upgrade", package))?;
        if !to_version.is_empty() && installed == to_version {
            return Ok(installed);
        }
        if !from_version.is_empty() && !to_version.is_empty() && installed == from_version {
            bail!(
                "{} is still at {} after upgrade (target {})",
                package,
                installed,
                to_version
            );
        }
        // The repo may only offer a newer version than the target; that is
        // acceptable — report what was actually installed.
        Ok(installed)
    }

    fn installed_version(package: &str) -> Option<String> {
        match package_manager().ok()? {
            Pm::Apt => {
                let out = Command::new("dpkg-query")
                    .args(["-W", "-f=${Version}", package])
                    .stdin(Stdio::null())
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .filter(|v| !v.is_empty())
            }
            Pm::Dnf => {
                let out = Command::new("rpm")
                    .args(["-q", "--qf", "%{VERSION}-%{RELEASE}", package])
                    .stdin(Stdio::null())
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .filter(|v| !v.is_empty())
            }
        }
    }

    pub fn verify_os_updates(ids: &[String]) -> anyhow::Result<String> {
        match package_manager()? {
            Pm::Apt => {
                // An id is verified once apt no longer lists it as upgradable.
                let out = Command::new("apt")
                    .args(["list", "--upgradable"])
                    .stdin(Stdio::null())
                    .output()
                    .context("failed to run apt list --upgradable")?;
                let stdout = String::from_utf8_lossy(&out.stdout);
                for id in ids {
                    let prefix = format!("{}/", id);
                    if stdout.lines().any(|l| l.trim_start().starts_with(&prefix)) {
                        bail!("{} is still upgradable after install", id);
                    }
                }
                Ok(format!("installed updates: {}", ids.join(", ")))
            }
            Pm::Dnf => {
                for id in ids {
                    let out = Command::new("dnf")
                        .args(["check-update", "-q", id])
                        .stdin(Stdio::null())
                        .output()
                        .context("failed to run dnf check-update")?;
                    if out.status.code() == Some(100) {
                        bail!("{} is still upgradable after install", id);
                    }
                }
                Ok(format!("installed updates: {}", ids.join(", ")))
            }
        }
    }

    pub fn trigger_reboot() -> anyhow::Result<()> {
        run_checked("shutdown", &["-r", "+1", "AuditReady: reboot to finish update installation"])
            .map(|_| ())
    }
}

#[cfg(target_os = "macos")]
mod exec {
    use super::{bail, run_checked, Context};
    use std::process::{Command, Stdio};

    pub fn package_upgrade(package: &str, _to_version: &str) -> anyhow::Result<()> {
        if package.is_empty() {
            bail!("Package Update job has no package name");
        }
        run_checked("brew", &["upgrade", package]).map(|_| ())
    }

    pub fn os_update_one(id: &str) -> anyhow::Result<()> {
        run_checked("softwareupdate", &["-i", id]).map(|_| ())
    }

    pub fn verify_package(
        package: &str,
        from_version: &str,
        to_version: &str,
    ) -> anyhow::Result<String> {
        let out = Command::new("brew")
            .args(["list", "--versions", package])
            .stdin(Stdio::null())
            .output()
            .context("failed to run brew list")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let installed = stdout
            .split_whitespace()
            .nth(1)
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("{} is not installed after upgrade", package))?;
        if !to_version.is_empty() && installed == to_version {
            return Ok(installed);
        }
        if !from_version.is_empty() && !to_version.is_empty() && installed == from_version {
            bail!(
                "{} is still at {} after upgrade (target {})",
                package,
                installed,
                to_version
            );
        }
        Ok(installed)
    }

    pub fn verify_os_updates(ids: &[String]) -> anyhow::Result<String> {
        // Best-effort: the update labels should no longer be listed.
        let out = Command::new("softwareupdate")
            .arg("-l")
            .stdin(Stdio::null())
            .output()
            .context("failed to run softwareupdate -l")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        for id in ids {
            let still_listed = stdout.lines().any(|l| {
                l.trim()
                    .strip_prefix("* ")
                    .map(|rest| rest.trim_start_matches("Label:").trim() == id)
                    .unwrap_or(false)
            });
            if still_listed {
                bail!("{} is still listed as available after install", id);
            }
        }
        Ok(format!("installed updates: {}", ids.join(", ")))
    }

    pub fn trigger_reboot() -> anyhow::Result<()> {
        run_checked("shutdown", &["-r", "+1", "AuditReady: reboot to finish update installation"])
            .map(|_| ())
    }
}

#[cfg(windows)]
mod exec {
    use super::{bail, run_checked, summarize, Context};
    use std::process::{Command, Stdio};

    fn command_exists(name: &str) -> bool {
        Command::new(name)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn package_upgrade(package: &str, _to_version: &str) -> anyhow::Result<()> {
        if package.is_empty() {
            bail!("Package Update job has no package name");
        }
        if command_exists("winget") {
            run_checked(
                "winget",
                &[
                    "upgrade",
                    "--id",
                    package,
                    "--exact",
                    "--accept-source-agreements",
                    "--accept-package-agreements",
                    "--disable-interactivity",
                ],
            )
            .map(|_| ())
        } else if command_exists("choco") {
            run_checked("choco", &["upgrade", package, "-y"]).map(|_| ())
        } else {
            bail!("no supported package manager (winget or choco) found")
        }
    }

    /// Install one KB. Prefers PSWindowsUpdate's Install-WindowsUpdate when
    /// the module is present; otherwise drives WUA via COM. The KB arrives as
    /// an argv element ($args[0]), never interpolated into the script.
    /// Idempotent: a KB that is already installed (e.g. Windows updated in
    /// the background) exits 0, so the job finishes Success, not Failed
    /// (agent contract: already-installed is Done).
    const WUA_INSTALL_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$kb = $args[0] -replace '^KB', ''
if (Get-Module -ListAvailable -Name PSWindowsUpdate) {
    Import-Module PSWindowsUpdate
    Install-WindowsUpdate -KBArticleID $kb -AcceptAll -IgnoreReboot
    exit 0
}
$session = New-Object -ComObject Microsoft.Update.Session
$searcher = $session.CreateUpdateSearcher()
$result = $searcher.Search("IsInstalled=0 and Type='Software'")
$toInstall = New-Object -ComObject Microsoft.Update.UpdateColl
foreach ($u in $result.Updates) {
    foreach ($id in $u.KBArticleIDs) {
        if ("$id" -eq "$kb") { [void]$toInstall.Add($u) }
    }
}
if ($toInstall.Count -eq 0) {
    if (Get-HotFix -Id "KB$kb" -ErrorAction SilentlyContinue) { exit 0 }
    # Not offered anymore usually means Windows already installed it in the
    # background — that is success, not failure (contract: report Done, not
    # Failed). Get-HotFix only covers CBS, so also search WUA's installed
    # history before giving up.
    $installed = $session.CreateUpdateSearcher().Search("IsInstalled=1 and Type='Software'")
    foreach ($u in $installed.Updates) {
        foreach ($id in $u.KBArticleIDs) {
            if ("$id" -eq "$kb") { exit 0 }
        }
    }
    if ($kb -eq '2267602') {
        # Defender Security Intelligence updates ship through Defender's own
        # channel, not CBS: they never appear in Get-HotFix, and once Defender
        # has self-updated, WUA stops offering them. Trigger a signature
        # update directly and accept fresh signatures as success.
        $mpcmd = Join-Path $env:ProgramFiles 'Windows Defender\MpCmdRun.exe'
        if (Test-Path $mpcmd) { & $mpcmd -SignatureUpdate | Out-Null }
        $mp = Get-MpComputerStatus
        if ($mp.AntivirusSignatureLastUpdated -and $mp.AntivirusSignatureLastUpdated -gt (Get-Date).AddDays(-2)) { exit 0 }
        Write-Error "Defender signature update failed (last updated $($mp.AntivirusSignatureLastUpdated))"
        exit 1
    }
    Write-Error "update KB$kb not found"
    exit 1
}
$downloader = $session.CreateUpdateDownloader()
$downloader.Updates = $toInstall
[void]$downloader.Download()
$installer = $session.CreateUpdateInstaller()
$installer.Updates = $toInstall
$r = $installer.Install()
if ($r.ResultCode -eq 2) { exit 0 } else { Write-Error "install result code $($r.ResultCode), hresult $($r.HResult)"; exit 1 }
"#;

    pub fn os_update_one(id: &str) -> anyhow::Result<()> {
        run_checked(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                WUA_INSTALL_SCRIPT,
                id,
            ],
        )
        .map(|_| ())
    }

    pub fn verify_package(
        package: &str,
        _from_version: &str,
        to_version: &str,
    ) -> anyhow::Result<String> {
        if command_exists("winget") {
            let out = Command::new("winget")
                .args(["list", "--id", package, "--exact", "--accept-source-agreements"])
                .stdin(Stdio::null())
                .output()
                .context("failed to run winget list")?;
            if out.status.success() {
                // winget doesn't hand us a clean machine-readable version here;
                // presence is what we verify.
                return Ok(if to_version.is_empty() {
                    "installed".to_string()
                } else {
                    to_version.to_string()
                });
            }
            bail!("{} not found by winget after upgrade", package);
        }
        if command_exists("choco") {
            let out = Command::new("choco")
                .args(["list", "--local-only", "--exact", package])
                .stdin(Stdio::null())
                .output()
                .context("failed to run choco list")?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let mut parts = line.split_whitespace();
                if parts.next() == Some(package) {
                    if let Some(v) = parts.next() {
                        return Ok(v.to_string());
                    }
                }
            }
            bail!("{} not found by choco after upgrade", package);
        }
        bail!("no supported package manager (winget or choco) found")
    }

    /// Verify a KB is installed: Get-HotFix first, then a WUA installed-search.
    /// Defender Security Intelligence updates (KB2267602) bypass CBS entirely,
    /// so for those any present Defender signature counts as installed.
    const WUA_VERIFY_SCRIPT: &str = r#"
$kb = $args[0] -replace '^KB', ''
if (Get-HotFix -Id "KB$kb" -ErrorAction SilentlyContinue) { exit 0 }
if ($kb -eq '2267602') {
    $mp = Get-MpComputerStatus
    if ($mp.AntivirusSignatureLastUpdated) { exit 0 }
    exit 1
}
$session = New-Object -ComObject Microsoft.Update.Session
$result = $session.CreateUpdateSearcher().Search("IsInstalled=1 and Type='Software'")
foreach ($u in $result.Updates) {
    foreach ($id in $u.KBArticleIDs) {
        if ("$id" -eq "$kb") { exit 0 }
    }
}
exit 1
"#;

    pub fn verify_os_updates(ids: &[String]) -> anyhow::Result<String> {
        for id in ids {
            let out = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    WUA_VERIFY_SCRIPT,
                    id,
                ])
                .stdin(Stdio::null())
                .output()
                .context("failed to run KB verification")?;
            if !out.status.success() {
                bail!(
                    "{} not installed after update: {}",
                    id,
                    summarize(&out.stderr)
                );
            }
        }
        Ok(format!("installed updates: {}", ids.join(", ")))
    }

    pub fn trigger_reboot() -> anyhow::Result<()> {
        run_checked(
            "shutdown",
            &["/r", "/t", "30", "/c", "AuditReady: reboot to finish update installation"],
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_poll_response() {
        let json = serde_json::json!({
            "ok": true,
            "jobs": [{
                "name": "patch-20260721093000-123456789",
                "host": "web-01",
                "package": "openssl",
                "from_version": "3.0.2",
                "to_version": "3.0.3",
                "severity": "Critical",
                "status": "Running",
                "job_type": "Package Update",
                "vulnerability": "alert-20260721-0001",
                "scheduled_at": "",
                "reboot_required": false,
                "progress": 0,
                "update_ids": []
            }]
        });
        let parsed: PollResponse = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.jobs.len(), 1);
        let job = &parsed.jobs[0];
        assert_eq!(job.name, "patch-20260721093000-123456789");
        assert_eq!(job.job_type, "Package Update");
        assert_eq!(job.to_version, "3.0.3");
        assert!(!job.reboot_required);
        assert!(job.update_ids.is_empty());
    }

    #[test]
    fn scheduled_delay_empty_or_past_runs_now() {
        assert!(scheduled_delay("").is_none());
        assert!(scheduled_delay("2000-01-01T00:00:00Z").is_none());
        assert!(scheduled_delay("not a date").is_none());
    }

    #[test]
    fn scheduled_delay_future_is_some() {
        let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let delay = scheduled_delay(&future).unwrap();
        assert!(delay > Duration::from_secs(3500));

        let future_naive = (Utc::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(scheduled_delay(&future_naive).is_some());

        let future_naive_t = (Utc::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert!(scheduled_delay(&future_naive_t).is_some());
    }

    #[test]
    fn completed_record_dedups_and_stays_bounded() {
        let mut state = State::default();
        for i in 0..250 {
            state.record_completed(&format!("job-{}", i), "Success", "ok", "", "");
        }
        assert_eq!(state.completed.len(), MAX_COMPLETED);
        // Oldest entries were dropped, newest retained.
        assert!(state.completed_job("job-0").is_none());
        assert!(state.completed_job("job-249").is_some());

        state.record_completed("job-249", "Failed", "", "boom", "tail of output");
        assert_eq!(state.completed.len(), MAX_COMPLETED);
        let done = state.completed_job("job-249").unwrap();
        assert_eq!(done.status, "Failed");
        assert_eq!(done.output, "tail of output");
    }

    #[test]
    fn summarize_caps_long_output() {
        let long = "x".repeat(5000);
        let summary = summarize(long.as_bytes());
        assert!(summary.chars().count() <= SUMMARY_CAP + 4); // "..." prefix
    }

    #[test]
    fn tail_chars_keeps_last_chars() {
        let short = "hello";
        assert_eq!(tail_chars(short, 100), "hello");

        let long = "x".repeat(OUTPUT_CAP + 100);
        let tail = tail_chars(&long, OUTPUT_CAP);
        assert!(tail.starts_with("..."));
        assert_eq!(tail.chars().count(), OUTPUT_CAP + 3);

        // Multibyte characters must not be split.
        let wide = "é".repeat(OUTPUT_CAP + 1);
        let tail = tail_chars(&wide, OUTPUT_CAP);
        assert_eq!(tail.chars().count(), OUTPUT_CAP + 3);
    }

    #[cfg(unix)]
    #[test]
    fn script_success_reports_done_and_output() {
        let outcome = run_script("echo hello-from-script", || {});
        assert_eq!(outcome.error, None);
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.output_tail.contains("hello-from-script"));
    }

    #[cfg(unix)]
    #[test]
    fn script_failure_reports_exit_code_and_partial_output() {
        let outcome = run_script("echo before-failure\necho oops >&2\nexit 3", || {});
        assert_eq!(outcome.error, None);
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(3));
        // Combined stdout+stderr is captured.
        assert!(outcome.output_tail.contains("before-failure"));
        assert!(outcome.output_tail.contains("oops"));
    }

    #[test]
    fn state_roundtrips_through_json() {
        let mut state = State::default();
        state.in_flight = Some(InFlightJob {
            job: Job {
                name: "patch-1".to_string(),
                host: "web-01".to_string(),
                package: "openssl".to_string(),
                from_version: "3.0.0".to_string(),
                to_version: "3.0.2".to_string(),
                job_type: "Package Update".to_string(),
                scheduled_at: String::new(),
                reboot_required: true,
                update_ids: vec![],
            },
            reboot_pending: true,
            reboot_marked_at: 1784307600,
        });
        state.record_completed("patch-0", "Success", "openssl 3.0.0 -> 3.0.2", "", "");

        let dir = std::env::temp_dir().join(format!("auditready-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patch-state.json");
        state.save(&path);

        let loaded = State::load(&path);
        let in_flight = loaded.in_flight.as_ref().unwrap();
        assert_eq!(in_flight.job.name, "patch-1");
        assert!(in_flight.reboot_pending);
        assert_eq!(loaded.completed_job("patch-0").unwrap().status, "Success");

        std::fs::remove_dir_all(&dir).ok();
    }
}
