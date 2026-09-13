//! Deployment script runner.
//!
//! Receives `Deployment` messages from the deployment WebSocket client, runs
//! the script using the shared `script` helper, and sends progress/terminal
//! reports back to the client for forwarding to the server.

use std::time::Duration;

use super::protocol::DeploymentMessage;

/// Interval between progress pings while a deployment script runs.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);

/// Run a single deployment to completion, sending `Report` messages into
/// `outbound`. This is intentionally synchronous so it can be driven from a
/// dedicated blocking task without tying up the async websocket reader.
pub fn run_deployment(
    deployment_id: String,
    name: String,
    script: String,
    outbound: &tokio::sync::mpsc::UnboundedSender<DeploymentMessage>,
) {
    tracing::info!(
        deployment_id = %deployment_id,
        name = %name,
        "running deployment"
    );

    if script.trim().is_empty() {
        let _ = outbound.send(DeploymentMessage::report(
            deployment_id,
            "Failed".into(),
            None,
            String::new(),
            "Deployment has no script".into(),
            String::new(),
        ));
        return;
    }

    let mut pings: u32 = 0;
    let outcome = crate::script::run_script(&script, PROGRESS_INTERVAL, |live_tail| {
        pings += 1;
        // Ramp 10 → 90 over the first pings, then hold.
        let progress = (10 + pings * 10).min(90);
        let _ = outbound.send(DeploymentMessage::report(
            deployment_id.clone(),
            "Running".into(),
            Some(progress),
            live_tail.to_string(),
            String::new(),
            String::new(),
        ));
    });

    let (status, result, error) = if let Some(e) = &outcome.error {
        ("Failed", String::new(), e.clone())
    } else if outcome.timed_out {
        (
            "Failed",
            String::new(),
            format!(
                "deployment timed out after {} minutes",
                crate::script::SCRIPT_TIMEOUT.as_secs() / 60
            ),
        )
    } else if outcome.exit_code == Some(0) {
        ("Done", "deployment completed".to_string(), String::new())
    } else {
        let error = match outcome.exit_code {
            Some(code) => format!("deployment script exited {}", code),
            None => "deployment script killed by signal".to_string(),
        };
        ("Failed", String::new(), error)
    };

    let _ = outbound.send(DeploymentMessage::report(
        deployment_id,
        status.into(),
        Some(100),
        outcome.output_tail,
        error,
        result,
    ));
}
