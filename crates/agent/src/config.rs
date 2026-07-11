use serde::Deserialize;
use std::path::Path;

/// Top-level application settings loaded from `appsettings.json`.
#[derive(Debug, Deserialize, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub server: ServerSettings,
}

/// Settings for the single backend server.
///
/// The same domain, token and interval are used for both telemetry pushes and
/// the remote shell tunnel. The tunnel WebSocket URL is derived from `domain`:
///
/// - `localhost:8000` → `ws://localhost:8000/ws`
/// - `api.example.com` → `wss://api.example.com/ws`
#[derive(Debug, Deserialize)]
pub struct ServerSettings {
    /// Host (and optional port) of the backend, e.g. `localhost:8000`.
    pub domain: Option<String>,
    /// Shared secret used for telemetry auth and tunnel auth.
    pub token: Option<String>,
    /// Seconds between telemetry snapshots.
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    /// Enable the outbound remote shell tunnel.
    #[serde(default)]
    pub tunnel_enabled: bool,
    /// Optional shell command for tunnel channels. Defaults to `$SHELL` or `/bin/sh`.
    #[serde(default)]
    pub tunnel_shell: Option<String>,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            domain: None,
            token: None,
            interval_seconds: default_interval(),
            tunnel_enabled: false,
            tunnel_shell: None,
        }
    }
}

fn default_interval() -> u64 {
    30
}

impl AppSettings {
    /// Return the WebSocket URL the tunnel should use.
    ///
    /// Derived from `server.domain`:
    ///   - `localhost:8000` → `ws://localhost:8000/audit_ready/tunnel/agent`
    ///   - `api.example.com` → `wss://api.example.com/audit_ready/tunnel/agent`
    pub fn broker_url(&self) -> Option<String> {
        self.server.domain.as_deref().map(build_broker_url)
    }

    /// Override settings from environment variables.
    ///
    /// Supported variables (all optional):
    /// - `AUDITREADY_DOMAIN`
    /// - `AUDITREADY_TOKEN`
    /// - `AUDITREADY_INTERVAL_SECONDS`
    /// - `AUDITREADY_TUNNEL_ENABLED`
    /// - `AUDITREADY_TUNNEL_SHELL`
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("AUDITREADY_DOMAIN") {
            if !v.is_empty() {
                self.server.domain = Some(v);
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_TOKEN") {
            if !v.is_empty() {
                self.server.token = Some(v);
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_INTERVAL_SECONDS") {
            if let Ok(n) = v.parse() {
                self.server.interval_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_TUNNEL_ENABLED") {
            self.server.tunnel_enabled = v.parse().unwrap_or(self.server.tunnel_enabled);
        }
        if let Ok(v) = std::env::var("AUDITREADY_TUNNEL_SHELL") {
            if !v.is_empty() {
                self.server.tunnel_shell = Some(v);
            }
        }
    }
}

fn build_broker_url(domain: &str) -> String {
    let domain = domain.trim();
    if domain.starts_with("ws://") || domain.starts_with("wss://") {
        return domain.trim_end_matches('/').to_string();
    }
    let is_local = domain.starts_with("localhost")
        || domain.starts_with("127.")
        || domain == "::1";
    let scheme = if is_local { "ws" } else { "wss" };
    format!("{}://{}/audit_ready/tunnel/agent", scheme, domain)
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
