//! Terminal client for Audit Ready Actions.
//!
//! Polls the server for pending actions and renders each one as a TUI card.
//! Button presses execute the action plan and report the result back.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    crossterm::{
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        ExecutableCommand,
    },
    Terminal,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

mod ui;

/// CLI for the Audit Ready Action client.
#[derive(Parser, Debug)]
#[command(name = "action-client")]
#[command(about = "Terminal client for Audit Ready Actions")]
struct Cli {
    /// Server base URL, e.g. https://audit.example.com
    #[arg(long, env = "AUDIT_READY_SERVER")]
    server: String,

    /// Bearer token for a Client Machine.
    #[arg(long, env = "AUDIT_READY_TOKEN")]
    token: String,

    /// Poll interval in seconds.
    #[arg(long, default_value_t = 30)]
    poll_interval: u64,
}

/// Action item as returned by the server.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ActionItem {
    name: String,
    title: String,
    action_type: String,
    status: String,
    payload: Value,
    action_plan: Value,
    assigned_user: String,
    due_at: String,
}

/// A button in an action plan.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ActionButton {
    label: String,
    command: String,
    #[serde(default)]
    style: String,
}

#[derive(Debug, Clone)]
struct ActionCard {
    action: ActionItem,
    buttons: Vec<ActionButton>,
}

impl ActionCard {
    fn from_action(action: ActionItem) -> Self {
        let buttons = match &action.action_plan {
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect(),
            _ => Vec::new(),
        };
        Self { action, buttons }
    }
}

#[derive(Debug, Clone, Default)]
struct AppState {
    cards: Vec<ActionCard>,
    selected_card: usize,
    selected_button: usize,
    status: String,
    last_poll: Option<String>,
}

impl AppState {
    fn selected_card(&self) -> Option<&ActionCard> {
        self.cards.get(self.selected_card)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!("starting action client for {}", cli.server);

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let state = Arc::new(tokio::sync::Mutex::new(AppState::default()));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<ActionItem>>(16);

    // Background poll loop.
    let poll_http = http.clone();
    let poll_state = Arc::clone(&state);
    let poll_server = cli.server.clone();
    let poll_token = cli.token.clone();
    let poll_interval = cli.poll_interval;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(poll_interval));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match poll_actions(&poll_http, &poll_server, &poll_token).await {
                Ok(actions) => {
                    let _ = tx.send(actions).await;
                }
                Err(e) => {
                    warn!("poll failed: {}", e);
                    let mut s = poll_state.lock().await;
                    s.status = format!("poll failed: {}", e);
                }
            }
        }
    });

    // Prime the first poll immediately.
    let initial_actions = poll_actions(&http, &cli.server, &cli.token).await?;
    {
        let mut s = state.lock().await;
        s.cards = initial_actions.into_iter().map(ActionCard::from_action).collect();
        s.status = format!("{} action(s) pending", s.cards.len());
        s.last_poll = Some(Utc::now().to_rfc3339());
    }

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, state, &http, &cli.server, &cli.token, &mut rx).await;

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn poll_actions(http: &reqwest::Client, server: &str, token: &str) -> Result<Vec<ActionItem>> {
    let url = format!("{}/audit_ready/actions/poll", server.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&json!({ "limit": 50 }))
        .send()
        .await
        .context("failed to send poll request")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("failed to parse poll response")?;

    if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let message = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("poll failed");
        anyhow::bail!("{} (HTTP {})", message, status);
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

async fn report_result(
    http: &reqwest::Client,
    server: &str,
    token: &str,
    name: &str,
    status: &str,
    result: &Value,
    error_message: &str,
) -> Result<()> {
    let url = format!("{}/audit_ready/actions/result", server.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&json!({
            "name": name,
            "status": status,
            "result": result,
            "error_message": error_message,
        }))
        .send()
        .await
        .context("failed to send result")?;

    let http_status = resp.status();
    let body: Value = resp.json().await.context("failed to parse result response")?;

    if !http_status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let message = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("result upload failed");
        anyhow::bail!("{} (HTTP {})", message, http_status);
    }
    Ok(())
}

fn execute_action_plan(action: &ActionItem, button: &ActionButton) -> (String, Value, String) {
    let now = Utc::now().to_rfc3339();
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());

    let (status, result, error) = match button.command.as_str() {
        "clock_in" => (
            "Completed".into(),
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
                    "Failed".into(),
                    Value::Null,
                    "No task_number provided in payload.".into(),
                )
            } else {
                (
                    "Completed".into(),
                    json!({ "task_number": task, "started_at": now, "user": username }),
                    String::new(),
                )
            }
        }
        "accept" => (
            "Completed".into(),
            json!({ "accepted_at": now, "user": username }),
            String::new(),
        ),
        "snooze" => (
            "Completed".into(),
            json!({ "snoozed_at": now, "user": username }),
            String::new(),
        ),
        cmd if cmd.starts_with('!') => {
            let shell_cmd = &cmd[1..];
            match std::process::Command::new("sh").arg("-c").arg(shell_cmd).output() {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    if output.status.success() {
                        (
                            "Completed".into(),
                            json!({ "stdout": stdout, "stderr": stderr }),
                            String::new(),
                        )
                    } else {
                        (
                            "Failed".into(),
                            json!({ "stdout": stdout }),
                            format!("exit code {:?}: {}", output.status.code(), stderr),
                        )
                    }
                }
                Err(e) => (
                    "Failed".into(),
                    Value::Null,
                    format!("failed to run command: {}", e),
                ),
            }
        }
        _ => (
            "Completed".into(),
            json!({ "button": button.label, "command": button.command, "pressed_at": now }),
            String::new(),
        ),
    };

    (status, result, error)
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    state: Arc<tokio::sync::Mutex<AppState>>,
    http: &reqwest::Client,
    server: &str,
    token: &str,
    rx: &mut tokio::sync::mpsc::Receiver<Vec<ActionItem>>,
) -> Result<()> {
    let mut reader = EventStream::new();
    let mut last_tick = tokio::time::Instant::now();
    let tick_rate = Duration::from_millis(250);

    loop {
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        let mut should_draw = false;

        tokio::select! {
            Some(actions) = rx.recv() => {
                let mut s = state.lock().await;
                let old_selected = s.cards.get(s.selected_card).map(|c| c.action.name.clone());
                s.cards = actions.into_iter().map(ActionCard::from_action).collect();
                if let Some(ref old_name) = old_selected {
                    if let Some(idx) = s.cards.iter().position(|c| c.action.name == *old_name) {
                        s.selected_card = idx;
                    } else {
                        s.selected_card = s.selected_card.min(s.cards.len().saturating_sub(1));
                    }
                } else {
                    s.selected_card = 0;
                }
                s.selected_button = s.selected_button.min(
                    s.selected_card().map(|c| c.buttons.len().saturating_sub(1)).unwrap_or(0)
                );
                s.status = format!("{} action(s) pending", s.cards.len());
                s.last_poll = Some(Utc::now().to_rfc3339());
                should_draw = true;
            }
            Some(Ok(event)) = reader.next() => {
                if let Event::Key(key) = event {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let mut s = state.lock().await;
                    match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Char('r') => {
                            drop(s);
                            match poll_actions(http, server, token).await {
                                Ok(actions) => {
                                    let mut s = state.lock().await;
                                    s.cards = actions.into_iter().map(ActionCard::from_action).collect();
                                    s.selected_card = s.selected_card.min(s.cards.len().saturating_sub(1));
                                    s.selected_button = 0;
                                    s.status = format!("{} action(s) pending", s.cards.len());
                                    s.last_poll = Some(Utc::now().to_rfc3339());
                                }
                                Err(e) => {
                                    let mut s = state.lock().await;
                                    s.status = format!("refresh failed: {}", e);
                                }
                            }
                            continue;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if !s.cards.is_empty() {
                                s.selected_card = s.selected_card.saturating_sub(1);
                                s.selected_button = 0;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if !s.cards.is_empty() {
                                s.selected_card = (s.selected_card + 1).min(s.cards.len() - 1);
                                s.selected_button = 0;
                            }
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            if s.selected_card().is_some() {
                                s.selected_button = s.selected_button.saturating_sub(1);
                            }
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            if let Some(card) = s.selected_card() {
                                s.selected_button =
                                    (s.selected_button + 1).min(card.buttons.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(card) = s.cards.get(s.selected_card).cloned() {
                                if let Some(button) = card.buttons.get(s.selected_button).cloned() {
                                    let action = card.action.clone();
                                    let (status, result, error) = execute_action_plan(&action, &button);
                                    s.status = format!("Reporting {} ...", action.name);
                                    drop(s);
                                    match report_result(http, server, token, &action.name, &status, &result, &error).await {
                                        Ok(()) => {
                                            let mut s = state.lock().await;
                                            s.status = format!("{} {}.", action.name, status);
                                            // Remove the completed card locally so the UI feels responsive.
                                            s.cards.retain(|c| c.action.name != action.name);
                                            s.selected_card = s.selected_card.min(s.cards.len().saturating_sub(1));
                                            s.selected_button = 0;
                                        }
                                        Err(e) => {
                                            let mut s = state.lock().await;
                                            s.status = format!("failed to report {}: {}", action.name, e);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    should_draw = true;
                }
            }
            _ = tokio::time::sleep(timeout) => {
                if last_tick.elapsed() >= tick_rate {
                    should_draw = true;
                    last_tick = tokio::time::Instant::now();
                }
            }
        }

        if should_draw {
            let s = state.lock().await.clone();
            terminal.draw(|f| ui::draw(f, &s))?;
        }
    }
}
