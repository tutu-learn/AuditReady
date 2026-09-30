//! WebSocket client for the action push channel.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::Message, Error as WsError},
    MaybeTlsStream, WebSocketStream,
};

use super::protocol::ActionMessage;
use super::{poll, push_action, remove_pending, PendingAction, SharedActions};

/// Maximum size of a single action message in bytes.
const MAX_MESSAGE_SIZE: usize = 256 * 1024;
/// Interval between WebSocket pings.
const PING_INTERVAL_SECONDS: u64 = 30;
/// Max silence from the server before reconnecting.
const SERVER_TIMEOUT_SECONDS: u64 = 120;
/// Initial reconnect delay.
const RECONNECT_BASE_SECONDS: u64 = 1;
/// Maximum reconnect delay.
const RECONNECT_MAX_SECONDS: u64 = 60;

/// Action push WebSocket client. Connects to the server, authenticates, and
/// waits for `ActionPush` messages. Results produced by the UI/executor flow
/// back through `result_rx` and are forwarded over the websocket.
pub struct ActionWebSocketClient {
    url: String,
    token: String,
    actions: SharedActions,
    result_rx: Option<mpsc::UnboundedReceiver<ActionMessage>>,
}

impl ActionWebSocketClient {
    pub fn new(
        url: String,
        token: String,
        actions: SharedActions,
        result_rx: mpsc::UnboundedReceiver<ActionMessage>,
    ) -> Self {
        Self {
            url,
            token,
            actions,
            result_rx: Some(result_rx),
        }
    }

    /// Run the action websocket forever, reconnecting on failure.
    pub async fn run(mut self) {
        // The result receiver is long-lived. It forwards into whichever
        // outbound websocket writer is currently attached; on disconnect
        // messages are dropped until a new writer is attached.
        let current_writer: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedSender<ActionMessage>>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let mut result_rx = self
            .result_rx
            .take()
            .expect("result_rx initialized in ActionWebSocketClient::new");
        let _forwarder = tokio::spawn({
            let current_writer = current_writer.clone();
            async move {
                while let Some(msg) = result_rx.recv().await {
                    let tx = current_writer.lock().await.clone();
                    if let Some(tx) = tx {
                        if tx.send(msg).is_err() {
                            // Writer is gone; next message will try the new
                            // writer once reconnect completes.
                        }
                    }
                }
            }
        });

        let mut attempt: u32 = 0;
        loop {
            match self.connect_and_serve(current_writer.clone()).await {
                Ok(()) => {
                    tracing::info!("action websocket closed cleanly; reconnecting...");
                }
                Err(e) => {
                    tracing::warn!("action websocket error: {}; reconnecting...", e);
                }
            }
            {
                let mut guard = current_writer.lock().await;
                *guard = None;
            }
            let delay = backoff_seconds(attempt);
            attempt = attempt.saturating_add(1);
            tracing::info!("waiting {}s before reconnect", delay);
            sleep(Duration::from_secs(delay)).await;
        }

    }

    async fn connect_and_serve(
        &self,
        current_writer: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedSender<ActionMessage>>>>,
    ) -> Result<()> {
        tracing::info!(url = %self.url, "connecting to action websocket");
        let (mut ws, _) = connect_async(&self.url)
            .await
            .context("connect to action websocket")?;

        let hello = ActionMessage::AgentHello {
            token: self.token.clone(),
        };
        ws.send(Message::Text(serde_json::to_string(&hello)?))
            .await?;

        let accepted = wait_for_hello_ack(&mut ws).await?;
        if !accepted {
            anyhow::bail!("server rejected action websocket authentication");
        }
        tracing::info!("action websocket authenticated");

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ActionMessage>();
        {
            let mut guard = current_writer.lock().await;
            *guard = Some(outbound_tx);
        }

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
                                tracing::warn!("failed to serialize action message: {}", e);
                                continue;
                            }
                        };
                        if text.len() > MAX_MESSAGE_SIZE {
                            tracing::warn!("action message exceeds size limit; dropping");
                            continue;
                        }
                        if let Err(e) = ws_tx.send(Message::Text(text)).await {
                            tracing::warn!("action websocket send error: {}", e);
                            break;
                        }
                    }
                    _ = ping.tick() => {
                        if let Err(e) = ws_tx.send(Message::Ping(Vec::new())).await {
                            tracing::warn!("action websocket ping error: {}", e);
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
                        "no traffic on action websocket for {}s; reconnecting",
                        SERVER_TIMEOUT_SECONDS
                    ));
                }
            };

            match frame {
                Some(Ok(Message::Text(text))) => {
                    if text.len() > MAX_MESSAGE_SIZE {
                        tracing::warn!("action message exceeds size limit; dropping");
                        continue;
                    }
                    let msg = match serde_json::from_str::<ActionMessage>(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!("invalid action message: {}", e);
                            continue;
                        }
                    };
                    if let Err(e) = self.handle_message(msg).await {
                        tracing::warn!("handle action message error: {}", e);
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    tracing::info!("action websocket closed by server");
                    break Ok(());
                }
                Some(Ok(_)) => continue,
                Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                    tracing::info!("action websocket connection closed");
                    break Ok(());
                }
                Some(Err(e)) => break Err(anyhow::Error::from(e)),
                None => {
                    tracing::info!("action websocket stream ended");
                    break Ok(());
                }
            }
        };

        writer.abort();
        result?;

        // On reconnect, trigger one HTTP poll cycle to catch missed actions.
        if let Err(e) = self.http_poll_fallback().await {
            tracing::warn!("action websocket HTTP fallback poll failed: {}", e);
        }

        Ok(())
    }

    async fn handle_message(&self, msg: ActionMessage) -> Result<()> {
        match msg {
            ActionMessage::ActionPush { action } => {
                push_action(&self.actions, PendingAction::from_item(action));
            }
            ActionMessage::ActionAck { name } => {
                remove_pending(&self.actions, &name);
            }
            ActionMessage::Error { message } => {
                tracing::warn!("action websocket server error: {}", message);
            }
            ActionMessage::HelloAck { .. } => {
                // Already handled during handshake.
            }
            other => {
                tracing::warn!("unexpected action message: {:?}", other);
            }
        }
        Ok(())
    }

    async fn http_poll_fallback(&self) -> anyhow::Result<()> {
        let trimmed = self.url.trim();
        let (scheme, rest) = if trimmed.starts_with("wss://") {
            ("https", trimmed.trim_start_matches("wss://"))
        } else if trimmed.starts_with("ws://") {
            ("http", trimmed.trim_start_matches("ws://"))
        } else {
            ("https", trimmed)
        };
        let domain = rest.split('/').next().unwrap_or(rest);
        let poll_url = format!("{}://{}/audit_ready/actions/poll", scheme, domain);

        let token = self.token.clone();
        let actions = match tokio::task::spawn_blocking(move || poll(&poll_url, &token)).await {
            Ok(Ok(actions)) => actions,
            Ok(Err(e)) => return Err(e),
            Err(e) => anyhow::bail!("HTTP poll task failed: {}", e),
        };

        for action in actions {
            push_action(&self.actions, PendingAction::from_item(action));
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
                    if let Ok(ActionMessage::HelloAck { accepted, .. }) =
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
        Err(_) => anyhow::bail!("timeout waiting for action hello ack"),
    }
}

/// Public entry point used by `main.rs`.
pub async fn run(
    url: String,
    token: String,
    actions: SharedActions,
    result_rx: mpsc::UnboundedReceiver<ActionMessage>,
) {
    let client = ActionWebSocketClient::new(url, token, actions, result_rx);
    client.run().await;
}
