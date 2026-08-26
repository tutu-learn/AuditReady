use serde::Deserialize;
use std::path::Path;

/// Top-level application settings loaded from `appsettings.json`.
#[derive(Debug, Deserialize, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub server: ServerSettings,
    /// Operating mode: `"agent"` (default) or `"client"`. Client mode
    /// additionally reports changed user files and clipboard activity.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub client: ClientSettings,
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
    /// Working directory for tunnel channels. Defaults to the process current directory.
    #[serde(default)]
    pub tunnel_cwd: Option<String>,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            domain: None,
            token: None,
            interval_seconds: default_interval(),
            tunnel_enabled: false,
            tunnel_shell: None,
            tunnel_cwd: None,
        }
    }
}

fn default_interval() -> u64 {
    30
}

/// Settings for client mode (user file-change + clipboard monitoring).
#[derive(Debug, Clone, Deserialize)]
pub struct ClientSettings {
    /// Seconds between client reports.
    #[serde(default = "default_client_report_interval")]
    pub report_interval_seconds: u64,
    /// Clipboard events at or above this size include their text content.
    #[serde(default = "default_clipboard_threshold")]
    pub clipboard_content_threshold_bytes: u64,
    /// Clipboard text is truncated to this many bytes in reports.
    #[serde(default = "default_clipboard_max")]
    pub clipboard_content_max_bytes: u64,
    /// Root directory scanned for changed files. Defaults to the current
    /// user's home directory.
    #[serde(default)]
    pub scan_root: Option<String>,
    /// Directory names (or slash-separated relative paths) skipped by the
    /// changed-file scan.
    #[serde(default = "default_excluded_dirs")]
    pub excluded_dirs: Vec<String>,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            report_interval_seconds: default_client_report_interval(),
            clipboard_content_threshold_bytes: default_clipboard_threshold(),
            clipboard_content_max_bytes: default_clipboard_max(),
            scan_root: None,
            excluded_dirs: default_excluded_dirs(),
        }
    }
}

fn default_client_report_interval() -> u64 {
    1800
}

fn default_clipboard_threshold() -> u64 {
    51200
}

fn default_clipboard_max() -> u64 {
    102400
}

fn default_excluded_dirs() -> Vec<String> {
    vec![
        "AppData/Local/Temp".to_string(),
        "AppData/Local/Microsoft".to_string(),
        "Library/Caches".to_string(),
        "node_modules".to_string(),
        ".git".to_string(),
        ".Trash".to_string(),
    ]
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
    /// - `AUDITREADY_TUNNEL_CWD`
    /// - `AUDITREADY_MODE`
    /// - `AUDITREADY_CLIENT_REPORT_INTERVAL_SECONDS`
    /// - `AUDITREADY_CLIENT_CLIPBOARD_THRESHOLD_BYTES`
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
        if let Ok(v) = std::env::var("AUDITREADY_TUNNEL_CWD") {
            if !v.is_empty() {
                self.server.tunnel_cwd = Some(v);
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_MODE") {
            if !v.is_empty() {
                self.mode = Some(v);
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_CLIENT_REPORT_INTERVAL_SECONDS") {
            if let Ok(n) = v.parse() {
                self.client.report_interval_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("AUDITREADY_CLIENT_CLIPBOARD_THRESHOLD_BYTES") {
            if let Ok(n) = v.parse() {
                self.client.clipboard_content_threshold_bytes = n;
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
    // Windows PowerShell 5.1 writes UTF-8 with a BOM by default; strip it
    // because serde_json rejects a leading BOM.
    let content = content.trim_start_matches('\u{feff}');
    let settings = serde_json::from_str(content)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn loads_config_with_utf8_bom() {
        let mut path = std::env::temp_dir();
        path.push(format!("auditready-bom-test-{}.json", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"\xef\xbb\xbf{\"server\":{\"domain\":\"api.example.com\",\"token\":\"t\"}}")
            .unwrap();
        drop(file);

        let settings = load(&path).unwrap();
        assert_eq!(settings.server.domain.as_deref(), Some("api.example.com"));
        assert_eq!(settings.server.token.as_deref(), Some("t"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parses_client_section_with_defaults() {
        // Full client section.
        let settings: AppSettings = serde_json::from_str(
            r#"{
                "mode": "client",
                "client": {
                    "report_interval_seconds": 900,
                    "clipboard_content_threshold_bytes": 1024,
                    "clipboard_content_max_bytes": 4096,
                    "scan_root": "/tmp/scan",
                    "excluded_dirs": ["skipme"]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(settings.mode.as_deref(), Some("client"));
        assert_eq!(settings.client.report_interval_seconds, 900);
        assert_eq!(settings.client.clipboard_content_threshold_bytes, 1024);
        assert_eq!(settings.client.clipboard_content_max_bytes, 4096);
        assert_eq!(settings.client.scan_root.as_deref(), Some("/tmp/scan"));
        assert_eq!(settings.client.excluded_dirs, vec!["skipme".to_string()]);

        // Missing client section gets defaults.
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.mode.is_none());
        assert_eq!(settings.client.report_interval_seconds, 1800);
        assert_eq!(settings.client.clipboard_content_threshold_bytes, 51200);
        assert_eq!(settings.client.clipboard_content_max_bytes, 102400);
        assert!(settings.client.scan_root.is_none());
        assert!(settings
            .client
            .excluded_dirs
            .contains(&"node_modules".to_string()));
        assert!(settings.client.excluded_dirs.contains(&".git".to_string()));
    }

    #[test]
    fn env_overrides_mode_and_client_settings() {
        std::env::set_var("AUDITREADY_MODE", "client");
        std::env::set_var("AUDITREADY_CLIENT_REPORT_INTERVAL_SECONDS", "60");
        std::env::set_var("AUDITREADY_CLIENT_CLIPBOARD_THRESHOLD_BYTES", "2048");

        let mut settings = AppSettings::default();
        settings.apply_env_overrides();
        assert_eq!(settings.mode.as_deref(), Some("client"));
        assert_eq!(settings.client.report_interval_seconds, 60);
        assert_eq!(settings.client.clipboard_content_threshold_bytes, 2048);

        std::env::remove_var("AUDITREADY_MODE");
        std::env::remove_var("AUDITREADY_CLIENT_REPORT_INTERVAL_SECONDS");
        std::env::remove_var("AUDITREADY_CLIENT_CLIPBOARD_THRESHOLD_BYTES");
    }
}
