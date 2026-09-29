//! Action polling and execution for client mode.
//!
//! A background task polls `POST /audit_ready/actions/poll` and pushes incoming
//! `Audit Ready Action` items into the shared stats state. The iced dashboard
//! renders them and reports the result back when the user clicks a button.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::stats::SharedStats;

/// Action item as returned by the server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActionItem {
    pub name: String,
    pub title: String,
    pub action_type: String,
    pub status: String,
    pub payload: Value,
    pub action_plan: Value,
    pub assigned_user: String,
    pub due_at: String,
}

/// A button in an action plan.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActionButton {
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub style: String,
}

/// A pending action prepared for the UI.
#[derive(Debug, Clone)]
pub struct PendingAction {
    pub item: ActionItem,
    pub buttons: Vec<ActionButton>,
}

impl PendingAction {
    pub fn from_item(item: ActionItem) -> Self {
        let buttons = match &item.action_plan {
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect(),
            _ => Vec::new(),
        };
        Self { item, buttons }
    }
}

/// Shared action queue used by the poller and the UI.
pub type SharedActions = Arc<Mutex<Vec<PendingAction>>>;

pub fn new_shared() -> SharedActions {
    Arc::new(Mutex::new(Vec::new()))
}

/// Poll the server for pending actions and merge them into the shared queue.
/// Runs until the process exits; errors are logged and retried after the
/// poll interval.
pub fn run(
    domain: &str,
    token: &str,
    shared: SharedActions,
    stats: SharedStats,
    poll_interval_seconds: u64,
) {
    let url = format!("{}/audit_ready/actions/poll", domain.trim_end_matches('/'));
    let mut last_count = 0usize;

    loop {
        match poll(&url, token) {
            Ok(actions) => {
                let new_count = actions.len();
                if new_count != last_count {
                    tracing::info!("actions: received {} pending action(s)", new_count);
                    last_count = new_count;
                }

                let mut queue = shared.lock().unwrap();
                // Replace the queue with the server's current view, preserving
                // any actions the user is already interacting with.
                let mut merged: Vec<PendingAction> = actions
                    .into_iter()
                    .map(PendingAction::from_item)
                    .collect();
                for existing in queue.drain(..) {
                    if !merged.iter().any(|a| a.item.name == existing.item.name) {
                        merged.push(existing);
                    }
                }
                *queue = merged;
                drop(queue);

                // Mark connection healthy through the stats handle.
                let _ = super::stats::record_client_report(
                    &stats,
                    Utc::now(),
                    0,
                    0,
                    0,
                    0,
                );
            }
            Err(e) => {
                tracing::warn!("actions poll failed: {}", e);
                let _ = super::stats::record_failure(&stats, format!("actions poll: {}", e));
            }
        }

        std::thread::sleep(Duration::from_secs(poll_interval_seconds));
    }
}

fn poll(url: &str, token: &str) -> anyhow::Result<Vec<ActionItem>> {
    let body = serde_json::to_string(&json!({ "limit": 50 }))?;
    let resp = ureq::post(url)
        .set("Authorization", &format!("Bearer {}", token))
        .set("Content-Type", "application/json")
        .send_string(&body)?;

    let body_text = resp.into_string()?;
    let body: Value = serde_json::from_str(&body_text)?;
    if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let message = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("poll failed");
        anyhow::bail!("{}", message);
    }

    let actions = body
        .get("actions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(actions)
}

/// Execute the action plan for the chosen button and report the result.
pub fn execute_and_report(
    domain: &str,
    token: &str,
    action: &ActionItem,
    button: &ActionButton,
) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());

    let (status, result, error_message) = match button.command.as_str() {
        "clock_in" => (
            "Completed",
            json!({ "clock_in_at": now, "user": username }),
            String::new(),
        ),
        "start_timesheet" => {
            let task = action
                .payload
                .get("task_number")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if task.is_empty() {
                (
                    "Failed",
                    Value::Null,
                    "No task_number provided in payload.".into(),
                )
            } else {
                (
                    "Completed",
                    json!({ "task_number": task, "started_at": now, "user": username }),
                    String::new(),
                )
            }
        }
        "accept" => (
            "Completed",
            json!({ "accepted_at": now, "user": username }),
            String::new(),
        ),
        "snooze" => (
            "Completed",
            json!({ "snoozed_at": now, "user": username }),
            String::new(),
        ),
        cmd if cmd.starts_with('!') => {
            let shell_cmd = &cmd[1..];
            match std::process::Command::new("sh")
                .arg("-c")
                .arg(shell_cmd)
                .output()
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    if output.status.success() {
                        (
                            "Completed",
                            json!({ "stdout": stdout, "stderr": stderr }),
                            String::new(),
                        )
                    } else {
                        (
                            "Failed",
                            json!({ "stdout": stdout }),
                            format!(
                                "exit code {:?}: {}",
                                output.status.code(),
                                stderr
                            ),
                        )
                    }
                }
                Err(e) => ("Failed", Value::Null, format!("failed to run command: {}", e)),
            }
        }
        _ => (
            "Completed",
            json!({
                "button": button.label,
                "command": button.command,
                "pressed_at": now,
            }),
            String::new(),
        ),
    };

    report_result(domain, token, &action.name, status, &result, &error_message)
}

fn report_result(
    domain: &str,
    token: &str,
    name: &str,
    status: &str,
    result: &Value,
    error_message: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/audit_ready/actions/result", domain.trim_end_matches('/'));
    let body = serde_json::to_string(&json!({
        "name": name,
        "status": status,
        "result": result,
        "error_message": error_message,
    }))?;
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", token))
        .set("Content-Type", "application/json")
        .send_string(&body)?;

    let body_text = resp.into_string()?;
    let body: Value = serde_json::from_str(&body_text)?;
    if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let message = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("result upload failed");
        anyhow::bail!("{}", message);
    }
    Ok(())
}
