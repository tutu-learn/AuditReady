//! Wire protocol for the action push WebSocket.
//!
//! Must stay in sync with the server-side actions WebSocket protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ActionItem;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionMessage {
    AgentHello { token: String },
    HelloAck {
        accepted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    ActionPush { action: ActionItem },
    ActionResult {
        name: String,
        status: String,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        error_message: String,
    },
    ActionAck { name: String },
    Error {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
    },
}

impl ActionMessage {
    #[allow(dead_code)]
    pub fn hello_ack(accepted: bool, message: impl Into<String>) -> Self {
        let msg = message.into();
        ActionMessage::HelloAck {
            accepted,
            message: if msg.is_empty() { None } else { Some(msg) },
        }
    }

    pub fn action_result(
        name: String,
        status: String,
        result: Value,
        error_message: String,
    ) -> Self {
        ActionMessage::ActionResult {
            name,
            status,
            result,
            error_message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_hello_round_trips() {
        let msg = ActionMessage::AgentHello {
            token: "infra-token".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"agent_hello\""));
        let decoded: ActionMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, ActionMessage::AgentHello { token } if token == "infra-token")
        );
    }

    #[test]
    fn hello_ack_omits_empty_message() {
        let msg = ActionMessage::hello_ack(true, "");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("message"));
    }

    #[test]
    fn action_result_round_trips() {
        let msg = ActionMessage::action_result(
            "action-1".into(),
            "Completed".into(),
            serde_json::json!({ "ok": true }),
            String::new(),
        );
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ActionMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ActionMessage::ActionResult {
                name,
                status,
                ..
            } if name == "action-1" && status == "Completed"
        ));
    }
}
