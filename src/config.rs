use serde::Deserialize;
use std::path::Path;

/// Top-level application settings loaded from `appsettings.json`.
#[derive(Debug, Deserialize, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub push: PushSettings,
}

/// Settings controlling how audit snapshots are pushed to a remote endpoint.
///
/// `domain` is just the host (and optional port), e.g. `localhost:8000` or
/// `api.example.com`. The full URL is built as `https://<domain>/audit_ready/telemetry`.
#[derive(Debug, Deserialize)]
pub struct PushSettings {
    #[serde(default)]
    pub enabled: bool,
    pub domain: Option<String>,
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    /// Optional Bearer token sent as an `Authorization` header.
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for PushSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            domain: None,
            interval_seconds: default_interval(),
            token: None,
        }
    }
}

fn default_interval() -> u64 {
    30
}

impl AppSettings {
    /// Override settings from environment variables.
    ///
    /// Supported variables (all optional):
    /// - `AUDITREADY_PUSH_ENABLED`
    /// - `AUDITREADY_PUSH_DOMAIN`
    /// - `AUDITREADY_PUSH_INTERVAL_SECONDS`
    /// - `AUDITREADY_PUSH_TOKEN`
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("AUDITREADY_PUSH_ENABLED") {
            self.push.enabled = v.parse().unwrap_or(self.push.enabled);
        }
        if let Ok(v) = std::env::var("AUDITREADY_PUSH_DOMAIN") {
            if !v.is_empty() {
                self.push.domain = Some(v);
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_PUSH_INTERVAL_SECONDS") {
            if let Ok(n) = v.parse() {
                self.push.interval_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_PUSH_TOKEN") {
            if !v.is_empty() {
                self.push.token = Some(v);
            }
        }
    }
}

/// Load settings from the given JSON file.
///
/// Callers should fall back to [`AppSettings::default`] when the file is missing
/// so the application can run without a configuration file.
pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<AppSettings> {
    let content = std::fs::read_to_string(path)?;
    let settings = serde_json::from_str(&content)?;
    Ok(settings)
}
