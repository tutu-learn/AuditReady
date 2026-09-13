//! Wire protocol for the deployment push WebSocket.
//!
//! Must stay in sync with the server-side `deployments/protocol.rs` in the
//! Portal repo.

use serde::{Deserialize, Serialize};

/// Messages exchanged between the AuditReady agent and the deployment broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeploymentMessage {
    /// Sent by the agent immediately after the WebSocket opens.
    AgentHello { token: String },
    /// Sent by the broker in response to `AgentHello`.
    HelloAck {
        accepted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Server → agent: run this deployment script.
    Deployment {
        deployment_id: String,
        name: String,
        script: String,
    },
    /// Agent → server: progress or terminal report for a deployment.
    Report {
        deployment_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        progress: Option<u32>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        output: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        error: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        result: String,
    },
    /// Generic error message in either direction.
    Error {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_hello_round_trips() {
        let msg = DeploymentMessage::AgentHello {
            token: "infra-token".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"agent_hello\""));
        let decoded: DeploymentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, DeploymentMessage::AgentHello { token } if token == "infra-token")
        );
    }

    #[test]
    fn deployment_round_trips() {
        let msg = DeploymentMessage::Deployment {
            deployment_id: "dep-1".into(),
            name: "my-app".into(),
            script: "echo hi".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DeploymentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DeploymentMessage::Deployment {
                deployment_id,
                name,
                script,
            } if deployment_id == "dep-1" && name == "my-app" && script == "echo hi"
        ));
    }

    #[test]
    fn report_round_trips() {
        let msg = DeploymentMessage::report(
            "dep-1".into(),
            "Running".into(),
            Some(42),
            "output tail".into(),
            String::new(),
            String::new(),
        );
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("error"));
        let decoded: DeploymentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DeploymentMessage::Report {
                deployment_id,
                status,
                progress: Some(42),
                output,
                ..
            } if deployment_id == "dep-1" && status == "Running" && output == "output tail"
        ));
    }
}

impl DeploymentMessage {
    #[allow(dead_code)]
    pub fn hello_ack(accepted: bool, message: impl Into<String>) -> Self {
        let msg = message.into();
        DeploymentMessage::HelloAck {
            accepted,
            message: if msg.is_empty() { None } else { Some(msg) },
        }
    }

    #[allow(dead_code)]
    pub fn deployment(deployment_id: String, name: String, script: String) -> Self {
        DeploymentMessage::Deployment {
            deployment_id,
            name,
            script,
        }
    }

    pub fn report(
        deployment_id: String,
        status: String,
        progress: Option<u32>,
        output: String,
        error: String,
        result: String,
    ) -> Self {
        DeploymentMessage::Report {
            deployment_id,
            status,
            progress,
            output,
            error,
            result,
        }
    }
}
