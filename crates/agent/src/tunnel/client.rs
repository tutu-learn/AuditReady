use anyhow::{Context, Result};
use auditready_protocol::{ChannelId, TunnelMessage};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use portable_pty::PtySize;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::Message, Error as WsError},
    MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;

use super::pty::ChannelPty;

/// Maximum size of an inbound WebSocket text frame or decoded ChannelData payload.
const MAX_MESSAGE_SIZE: usize = 256 * 1024;
/// Maximum number of concurrent shell channels per agent.
const MAX_CHANNELS: usize = 64;
/// Outbound message queue capacity (towards the WebSocket writer).
const OUTBOUND_CAPACITY: usize = 256;
/// Initial reconnect delay in seconds.
const RECONNECT_BASE_SECONDS: u64 = 1;
/// Maximum reconnect delay in seconds.
const RECONNECT_MAX_SECONDS: u64 = 60;

/// Handle to a connected agent tunnel.
pub struct TunnelClient {
    broker_url: String,
    token: String,
    shell: Option<String>,
    cwd: Option<String>,
}

struct ChannelHandle {
    pty: ChannelPty,
}

impl TunnelClient {
    pub fn new(
        broker_url: String,
        token: String,
        shell: Option<String>,
        cwd: Option<String>,
    ) -> Self {
        Self {
            broker_url,
            token,
            shell,
            cwd,
        }
    }

    /// Run the tunnel forever, reconnecting on failure with exponential backoff.
    pub async fn run(self) {
        let mut attempt: u32 = 0;
        loop {
            match self.connect_and_serve().await {
                Ok(()) => {
                    tracing::info!("tunnel closed cleanly; reconnecting...");
                }
                Err(e) => {
                    tracing::warn!("tunnel error: {}; reconnecting...", e);
                }
            }
            let delay = backoff_seconds(attempt);
            attempt = attempt.saturating_add(1);
            tracing::info!("waiting {}s before reconnect", delay);
            sleep(Duration::from_secs(delay)).await;
        }
    }

    async fn connect_and_serve(&self) -> Result<()> {
        tracing::info!(url = %self.broker_url, "connecting to broker");
        // native-tls verifies server certificates by default. Do not disable
        // certificate verification in production — that would allow trivial MITM.
        let (mut ws, _) = connect_async(&self.broker_url)
            .await
            .context("connect to broker")?;

        // Authenticate. agent_id is intentionally omitted; the broker assigns
        // one based on the token and returns it in BrokerHello.
        // Token is intentionally not logged.
        let hello = TunnelMessage::AgentHello {
            agent_id: None,
            token: self.token.clone(),
        };
        ws.send(Message::Text(serde_json::to_string(&hello)?))
            .await?;

        // Wait for broker hello.
        let accepted = wait_for_broker_hello(&mut ws).await?;
        if !accepted {
            anyhow::bail!("broker rejected authentication");
        }
        tracing::info!("broker accepted authentication");

        let (broker_tx, mut broker_rx) = mpsc::channel::<TunnelMessage>(OUTBOUND_CAPACITY);
        let channels: DashMap<ChannelId, ChannelHandle> = DashMap::new();

        let (mut ws_tx, mut ws_rx) = ws.split();

        // Task: serialize outbound tunnel messages and send over WS.
        let writer = tokio::spawn(async move {
            while let Some(msg) = broker_rx.recv().await {
                let text = match serde_json::to_string(&msg) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("failed to serialize tunnel message: {}", e);
                        continue;
                    }
                };
                if text.len() > MAX_MESSAGE_SIZE {
                    tracing::warn!("outbound tunnel message exceeds size limit; dropping");
                    continue;
                }
                if let Err(e) = ws_tx.send(Message::Text(text)).await {
                    tracing::warn!("websocket send error: {}", e);
                    break;
                }
            }
        });

        // Main loop: read websocket frames and dispatch.
        let result = loop {
            match ws_rx.next().await {
                Some(Ok(Message::Text(text))) => {
                    if text.len() > MAX_MESSAGE_SIZE {
                        tracing::warn!(
                            "inbound websocket frame exceeds {} bytes; dropping",
                            MAX_MESSAGE_SIZE
                        );
                        continue;
                    }
                    let msg = match serde_json::from_str::<TunnelMessage>(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!("invalid tunnel message: {}", e);
                            continue;
                        }
                    };
                    if !is_allowed_from_broker(&msg) {
                        tracing::warn!("dropping unexpected message type from broker");
                        continue;
                    }
                    if let Err(e) = self.handle_message(msg, &broker_tx, &channels).await {
                        tracing::warn!("handle message error: {}", e);
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    tracing::info!("broker closed websocket");
                    break Ok(());
                }
                Some(Ok(_)) => {
                    // Ignore binary/ping/pong frames.
                    continue;
                }
                Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                    tracing::info!("websocket connection closed");
                    break Ok(());
                }
                Some(Err(e)) => {
                    break Err(e);
                }
                None => {
                    tracing::info!("websocket stream ended");
                    break Ok(());
                }
            }
        };

        // Clean up all channels on disconnect.
        for entry in channels.iter() {
            entry.value().pty.close();
        }
        channels.clear();

        writer.abort();

        result?;
        Ok(())
    }

    async fn handle_message(
        &self,
        msg: TunnelMessage,
        broker_tx: &mpsc::Sender<TunnelMessage>,
        channels: &DashMap<ChannelId, ChannelHandle>,
    ) -> Result<()> {
        match msg {
            TunnelMessage::BrokerHello { .. } => {
                // Already handled during handshake.
            }
            TunnelMessage::ChannelOpen {
                channel_id,
                command,
            } => {
                if !is_valid_channel_id(&channel_id) {
                    tracing::warn!("rejecting ChannelOpen with invalid channel id");
                    let _ = broker_tx
                        .send(TunnelMessage::Error {
                            message: "invalid channel id".to_string(),
                        })
                        .await;
                    return Ok(());
                }
                if channels.contains_key(&channel_id) {
                    tracing::warn!(channel_id = %channel_id.0, "rejecting ChannelOpen for existing channel");
                    let _ = broker_tx
                        .send(TunnelMessage::Error {
                            message: "channel id already exists".to_string(),
                        })
                        .await;
                    return Ok(());
                }
                if channels.len() >= MAX_CHANNELS {
                    tracing::warn!("rejecting ChannelOpen: channel limit reached");
                    let _ = broker_tx
                        .send(TunnelMessage::Error {
                            message: "channel limit reached".to_string(),
                        })
                        .await;
                    return Ok(());
                }
                tracing::info!(channel_id = %channel_id.0, "opening channel");
                let shell = command.or_else(|| self.shell.clone());
                let cwd = self.cwd.clone();
                let size = PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                match super::pty::spawn(channel_id, shell, cwd, size, broker_tx.clone()) {
                    Ok(pty) => {
                        channels.insert(channel_id, ChannelHandle { pty });
                    }
                    Err(e) => {
                        tracing::error!(channel_id = %channel_id.0, "failed to open channel: {}", e);
                        let _ = broker_tx
                            .send(TunnelMessage::Error {
                                message: format!("failed to open channel: {}", e),
                            })
                            .await;
                    }
                }
            }
            TunnelMessage::ChannelData { channel_id, data } => {
                if data.len() > MAX_MESSAGE_SIZE {
                    tracing::warn!(
                        channel_id = %channel_id.0,
                        "ChannelData payload exceeds size limit; dropping"
                    );
                    return Ok(());
                }
                if let Some(handle) = channels.get(&channel_id) {
                    handle.pty.send(data);
                }
            }
            TunnelMessage::ChannelResize {
                channel_id,
                rows,
                cols,
            } => {
                if let Some(handle) = channels.get(&channel_id) {
                    handle.pty.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            TunnelMessage::ChannelClose { channel_id } => {
                tracing::info!(channel_id = %channel_id.0, "closing channel");
                if let Some((_, handle)) = channels.remove(&channel_id) {
                    handle.pty.close();
                }
            }
            TunnelMessage::Error { message } => {
                tracing::error!("broker error: {}", message);
            }
            TunnelMessage::AgentHello { .. } => {
                // Handled by is_allowed_from_broker; unreachable here.
            }
        }
        Ok(())
    }
}

fn is_allowed_from_broker(msg: &TunnelMessage) -> bool {
    matches!(
        msg,
        TunnelMessage::BrokerHello { .. }
            | TunnelMessage::ChannelOpen { .. }
            | TunnelMessage::ChannelData { .. }
            | TunnelMessage::ChannelResize { .. }
            | TunnelMessage::ChannelClose { .. }
            | TunnelMessage::Error { .. }
    )
}

fn is_valid_channel_id(channel_id: &ChannelId) -> bool {
    channel_id.0 != Uuid::nil()
}

fn backoff_seconds(attempt: u32) -> u64 {
    let base = RECONNECT_BASE_SECONDS.saturating_mul(2u64.saturating_pow(attempt));
    base.clamp(RECONNECT_BASE_SECONDS, RECONNECT_MAX_SECONDS)
}

async fn wait_for_broker_hello(
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<bool> {
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(TunnelMessage::BrokerHello { accepted, .. }) =
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
        Err(_) => anyhow::bail!("timeout waiting for broker hello"),
    }
}

/// Public entry point used by `main.rs`.
pub async fn run(
    broker_url: String,
    token: String,
    shell: Option<String>,
    cwd: Option<String>,
) {
    let client = TunnelClient::new(broker_url, token, shell, cwd);
    client.run().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_hello_from_broker_is_dropped() {
        let msg = TunnelMessage::AgentHello {
            agent_id: None,
            token: "x".to_string(),
        };
        assert!(!is_allowed_from_broker(&msg));
    }

    #[test]
    fn broker_hello_is_allowed() {
        let msg = TunnelMessage::BrokerHello {
            accepted: true,
            agent_id: None,
        };
        assert!(is_allowed_from_broker(&msg));
    }

    #[test]
    fn nil_channel_id_is_invalid() {
        assert!(!is_valid_channel_id(&ChannelId(Uuid::nil())));
    }

    #[test]
    fn non_nil_channel_id_is_valid() {
        assert!(is_valid_channel_id(&ChannelId::new()));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_seconds(0), 1);
        assert_eq!(backoff_seconds(1), 2);
        assert_eq!(backoff_seconds(2), 4);
        assert_eq!(backoff_seconds(10), 60);
    }

    #[tokio::test]
    async fn nil_channel_id_open_is_rejected() {
        let client = TunnelClient::new(
            "ws://localhost:8000/audit_ready/tunnel/agent".to_string(),
            "token".to_string(),
            None,
            None,
        );
        let (broker_tx, mut broker_rx) = mpsc::channel(8);
        let channels: DashMap<ChannelId, ChannelHandle> = DashMap::new();

        let msg = TunnelMessage::ChannelOpen {
            channel_id: ChannelId(Uuid::nil()),
            command: Some("/bin/sh".to_string()),
        };
        client
            .handle_message(msg, &broker_tx, &channels)
            .await
            .unwrap();

        let reply = broker_rx.recv().await;
        assert!(
            matches!(reply, Some(TunnelMessage::Error { ref message }) if message.contains("invalid")),
            "expected invalid-channel-id error, got {:?}",
            reply
        );
    }

    #[tokio::test]
    async fn oversized_channel_data_is_dropped() {
        let client = TunnelClient::new(
            "ws://localhost:8000/audit_ready/tunnel/agent".to_string(),
            "token".to_string(),
            None,
            None,
        );
        let (broker_tx, _broker_rx) = mpsc::channel(8);
        let channels: DashMap<ChannelId, ChannelHandle> = DashMap::new();

        let msg = TunnelMessage::ChannelData {
            channel_id: ChannelId::new(),
            data: vec![0u8; MAX_MESSAGE_SIZE + 1],
        };
        // Should return Ok without panicking even though the channel does not exist.
        client.handle_message(msg, &broker_tx, &channels).await.unwrap();
    }
}
