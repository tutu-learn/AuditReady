//! WebSocket client for the dedicated deployment push channel.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::Message, Error as WsError},
    MaybeTlsStream, WebSocketStream,
};

use super::protocol::DeploymentMessage;
use super::runner::run_deployment;

/// Maximum size of a single deployment message in bytes.
const MAX_MESSAGE_SIZE: usize = 256 * 1024;
/// Interval between WebSocket pings.
const PING_INTERVAL_SECONDS: u64 = 30;
/// Max silence from the server before reconnecting.
const SERVER_TIMEOUT_SECONDS: u64 = 120;
/// Initial reconnect delay.
const RECONNECT_BASE_SECONDS: u64 = 1;
/// Maximum reconnect delay.
const RECONNECT_MAX_SECONDS: u64 = 60;

/// Deployment channel client. Connects to the server, authenticates, and waits
/// for `Deployment` push messages.
pub struct DeploymentClient {
    url: String,
    token: String,
}

impl DeploymentClient {
    pub fn new(url: String, token: String) -> Self {
        Self { url, token }
    }

    /// Run the deployment channel forever, reconnecting on failure.
    pub async fn run(self) {
        let mut attempt: u32 = 0;
        loop {
            match self.connect_and_serve().await {
                Ok(()) => {
                    tracing::info!("deployment channel closed cleanly; reconnecting...");
                }
                Err(e) => {
                    tracing::warn!("deployment channel error: {}; reconnecting...", e);
                }
            }
            let delay = backoff_seconds(attempt);
            attempt = attempt.saturating_add(1);
            tracing::info!("waiting {}s before reconnect", delay);
            sleep(Duration::from_secs(delay)).await;
        }
    }

    async fn connect_and_serve(&self) -> Result<()> {
        tracing::info!(url = %self.url, "connecting to deployment channel");
        let (mut ws, _) = connect_async(&self.url)
            .await
            .context("connect to deployment channel")?;

        let hello = DeploymentMessage::AgentHello {
            token: self.token.clone(),
        };
        ws.send(Message::Text(serde_json::to_string(&hello)?))
            .await?;

        let accepted = wait_for_hello_ack(&mut ws).await?;
        if !accepted {
            anyhow::bail!("server rejected deployment channel authentication");
        }
        tracing::info!("deployment channel authenticated");

        let (outbound_tx, mut outbound_rx) =
            mpsc::unbounded_channel::<DeploymentMessage>();

        let (mut ws_tx, mut ws_rx) = ws.split();

        // Writer: serialize outbound reports and send over WS, plus keepalives.
        let writer = tokio::spawn(async move {
            let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECONDS));
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ping.tick().await; // skip immediate first tick
            loop {
                tokio::select! {
                    msg = outbound_rx.recv() => {
                        let Some(msg) = msg else { break };
                        let text = match serde_json::to_string(&msg) {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!("failed to serialize deployment report: {}", e);
                                continue;
                            }
                        };
                        if text.len() > MAX_MESSAGE_SIZE {
                            tracing::warn!("deployment report exceeds size limit; dropping");
                            continue;
                        }
                        if let Err(e) = ws_tx.send(Message::Text(text)).await {
                            tracing::warn!("deployment websocket send error: {}", e);
                            break;
                        }
                    }
                    _ = ping.tick() => {
                        if let Err(e) = ws_tx.send(Message::Ping(Vec::new())).await {
                            tracing::warn!("deployment websocket ping error: {}", e);
                            break;
                        }
                    }
                }
            }
        });

        // Reader: dispatch inbound messages.
        let result = loop {
            let frame = match tokio::time::timeout(
                Duration::from_secs(SERVER_TIMEOUT_SECONDS),
                ws_rx.next(),
            )
            .await
            {
                Ok(frame) => frame,
                Err(_) => {
                    break Err(anyhow::anyhow!(
                        "no traffic on deployment channel for {}s; reconnecting",
                        SERVER_TIMEOUT_SECONDS
                    ));
                }
            };

            match frame {
                Some(Ok(Message::Text(text))) => {
                    if text.len() > MAX_MESSAGE_SIZE {
                        tracing::warn!("deployment message exceeds size limit; dropping");
                        continue;
                    }
                    let msg = match serde_json::from_str::<DeploymentMessage>(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!("invalid deployment message: {}", e);
                            continue;
                        }
                    };
                    if let Err(e) = self.handle_message(msg, &outbound_tx).await {
                        tracing::warn!("handle deployment message error: {}", e);
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    tracing::info!("deployment channel closed by server");
                    break Ok(());
                }
                Some(Ok(_)) => continue,
                Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                    tracing::info!("deployment websocket connection closed");
                    break Ok(());
                }
                Some(Err(e)) => break Err(anyhow::Error::from(e)),
                None => {
                    tracing::info!("deployment websocket stream ended");
                    break Ok(());
                }
            }
        };

        writer.abort();
        result?;
        Ok(())
    }

    async fn handle_message(
        &self,
        msg: DeploymentMessage,
        outbound: &mpsc::UnboundedSender<DeploymentMessage>,
    ) -> Result<()> {
        match msg {
            DeploymentMessage::Deployment {
                deployment_id,
                name,
                script,
            } => {
                // Spawn the runner on a blocking task so the async reader stays
                // responsive. Reports flow back through the outbound channel.
                let outbound = outbound.clone();
                tokio::task::spawn_blocking(move || {
                    run_deployment(deployment_id, name, script, &outbound);
                });
            }
            DeploymentMessage::HelloAck { .. } => {
                // Already handled during handshake.
            }
            DeploymentMessage::Error { message } => {
                tracing::error!("deployment channel server error: {}", message);
            }
            other => {
                tracing::warn!("unexpected deployment message: {:?}", other);
            }
        }
        Ok(())
    }
}

fn backoff_seconds(attempt: u32) -> u64 {
    let base = RECONNECT_BASE_SECONDS.saturating_mul(2u64.saturating_pow(attempt));
    base.clamp(RECONNECT_BASE_SECONDS, RECONNECT_MAX_SECONDS)
}

async fn wait_for_hello_ack(
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<bool> {
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(DeploymentMessage::HelloAck { accepted, .. }) =
                        serde_json::from_str(&text)
                    {
                        return Ok(accepted);
                    }
                }
                Some(Ok(Message::Close(_))) => return Ok(false),
                Some(Err(e)) => return Err(e),
                None => return Ok(false),
                _ => continue,
            }
        }
    })
    .await;

    match outcome {
        Ok(inner) => inner.map_err(|e| anyhow::anyhow!("websocket error: {}", e)),
        Err(_) => anyhow::bail!("timeout waiting for deployment hello ack"),
    }
}

/// Public entry point used by `main.rs`.
pub async fn run(url: String, token: String) {
    let client = DeploymentClient::new(url, token);
    client.run().await;
}
