use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identity for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

/// Identity for a single multiplexed shell channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub Uuid);

impl ChannelId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ChannelId {
    fn default() -> Self {
        Self::new()
    }
}

/// Wire messages exchanged between agent and broker over the tunnel WebSocket.
///
/// A single WebSocket carries many logical channels tagged by `channel_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TunnelMessage {
    /// Sent by the agent immediately after the WebSocket opens.
    /// `agent_id` is optional; if omitted, the broker assigns one and returns it
    /// in `BrokerHello`.
    AgentHello {
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<AgentId>,
        token: String,
    },
    /// Sent by the broker in response to `AgentHello`.
    BrokerHello {
        accepted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<AgentId>,
    },
    /// Broker asks the agent to open a new channel.
    ChannelOpen {
        channel_id: ChannelId,
        #[serde(default)]
        command: Option<String>,
    },
    /// Close a channel from either side.
    ChannelClose {
        channel_id: ChannelId,
    },
    /// PTY data in either direction. Bytes are base64-encoded for JSON transport.
    ChannelData {
        channel_id: ChannelId,
        #[serde(with = "serde_bytes_base64")]
        data: Vec<u8>,
    },
    /// Resize the PTY for a channel.
    ChannelResize {
        channel_id: ChannelId,
        rows: u16,
        cols: u16,
    },
    /// Generic error message.
    Error {
        message: String,
    },
}

mod serde_bytes_base64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        base64::engine::general_purpose::STANDARD
            .encode(bytes)
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_channel_data() {
        let msg = TunnelMessage::ChannelData {
            channel_id: ChannelId::new(),
            data: b"hello world\n\x00\x01".to_vec(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: TunnelMessage = serde_json::from_str(&json).unwrap();
        match (msg, decoded) {
            (TunnelMessage::ChannelData { data: a, .. }, TunnelMessage::ChannelData { data: b, .. }) => {
                assert_eq!(a, b);
            }
            _ => panic!("message variant mismatch"),
        }
    }

    #[test]
    fn agent_hello_serializes_with_type_tag() {
        let msg = TunnelMessage::AgentHello {
            agent_id: Some(AgentId::new()),
            token: "secret".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"agent_hello\""), "{json}");
    }

    #[test]
    fn agent_hello_without_id_omits_it() {
        let msg = TunnelMessage::AgentHello {
            agent_id: None,
            token: "secret".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("agent_id"), "{json}");
    }
}
