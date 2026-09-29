//! Action polling, WebSocket push, and execution for client mode.
//!
//! The primary path is a WebSocket connection to
//! `/audit_ready/actions/ws` that pushes actions and receives results.
//! The older HTTP poll/result endpoints remain as a fallback.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod protocol;
pub mod ws;

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

/// A completed or failed action recorded for the history tab.
#[derive(Debug, Clone)]
pub struct HistoryAction {
    pub item: ActionItem,
    pub status: String,
    pub result: Value,
    pub error_message: String,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}

/// Shared action state used by the poller, WebSocket client, and UI.
#[derive(Clone)]
pub struct ActionState {
    pub pending: Vec<PendingAction>,
    pub history: Vec<HistoryAction>,
    pub result_tx: Option<tokio::sync::mpsc::UnboundedSender<protocol::ActionMessage>>,
}

/// Shared action queue/state used by the poller/WebSocket client and the UI.
pub type SharedActions = Arc<Mutex<ActionState>>;

pub fn new_shared() -> SharedActions {
    Arc::new(Mutex::new(ActionState {
        pending: vec![],
        history: vec![],
        result_tx: None,
    }))
}

/// Add a pending action unless an action with the same name is already queued.
pub fn push_pending(actions: &SharedActions, pending: PendingAction) {
    let mut state = actions.lock().unwrap();
    if !state.pending.iter().any(|a| a.item.name == pending.item.name) {
        state.pending.push(pending);
    }
}

/// Remove a pending action by name.
pub fn remove_pending(actions: &SharedActions, name: &str) {
    let mut state = actions.lock().unwrap();
    state.pending.retain(|a| a.item.name != name);
}

/// Move an action from pending to history, recording its outcome.
pub fn record_history(
    actions: &SharedActions,
    item: ActionItem,
    status: String,
    result: Value,
    error_message: String,
) {
    let mut state = actions.lock().unwrap();
    state.pending.retain(|a| a.item.name != item.name);
    state.history.push(HistoryAction {
        item,
        status,
        result,
        error_message,
        completed_at: Utc::now(),
    });
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
                    tracing::info!(
                        "actions: received {} pending action(s) via HTTP",
                        new_count
                    );
                    last_count = new_count;
                }

                let mut state = shared.lock().unwrap();
                let mut merged: Vec<PendingAction> = actions
                    .into_iter()
                    .map(PendingAction::from_item)
                    .collect();
                for existing in state.pending.drain(..) {
                    if !merged.iter().any(|a| a.item.name == existing.item.name) {
                        merged.push(existing);
                    }
                }
                state.pending = merged;
                drop(state);

                // Mark connection healthy through the stats handle.
                let _ = super::stats::record_client_report(&stats, Utc::now(), 0, 0, 0, 0);
            }
            Err(e) => {
                tracing::warn!("actions HTTP poll failed: {}", e);
                let _ = super::stats::record_failure(&stats, format!("actions HTTP poll: {}", e));
            }
        }

        std::thread::sleep(Duration::from_secs(poll_interval_seconds));
    }
}

/// Low-level HTTP poll used by both the blocking poller and the websocket
/// fallback path.
pub fn poll(url: &str, token: &str) -> anyhow::Result<Vec<ActionItem>> {
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
///
/// Results are sent over the websocket result channel when available, falling
/// back to the HTTP result endpoint. On success the action is moved from
/// pending to history.
pub fn execute_and_report(
    domain: &str,
    token: &str,
    actions: SharedActions,
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
                            format!("exit code {:?}: {}", output.status.code(), stderr),
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

    let status = status.to_string();

    // Try the websocket result channel first.
    let result_tx = {
        let state = actions.lock().unwrap();
        state.result_tx.clone()
    };
    if let Some(tx) = result_tx {
        let msg = protocol::ActionMessage::action_result(
            action.name.clone(),
            status.clone(),
            result.clone(),
            error_message.clone(),
        );
        if tx.send(msg).is_ok() {
            record_history(
                &actions,
                action.clone(),
                status,
                result,
                error_message,
            );
            return Ok(());
        }
    }

    report_result(domain, token, &action.name, &status, &result, &error_message)?;
    record_history(
        &actions,
        action.clone(),
        status,
        result,
        error_message,
    );
    Ok(())
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
