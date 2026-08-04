//! Best-effort collection of IIS website inventory for telemetry (Windows).
//!
//! The fleet-map node panel shows IIS sites only when the agent reports IIS
//! as installed, so the full current site list is reported with every
//! telemetry snapshot once collected. Collection shells out to PowerShell's
//! WebAdministration module, so it runs on a dedicated background thread and
//! the telemetry loop only reads the cached snapshot — a failed or slow
//! check never delays or breaks telemetry.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the cached IIS inventory is refreshed. Site states should pick
/// up stop/start actions reasonably fast; collection is one short-lived
/// PowerShell process.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// One IIS website, as reported in telemetry `iis.sites`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IisSite {
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub app_pool: String,
    #[serde(default)]
    pub app_pool_state: String,
    #[serde(default)]
    pub physical_path: String,
    #[serde(default)]
    pub bindings: Vec<String>,
}

/// The `iis` telemetry section: whether IIS is installed, and every website
/// on the host when it is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IisSection {
    pub installed: bool,
    #[serde(default)]
    pub sites: Vec<IisSite>,
}

/// Shared cache holding the most recent successful collection.
///
/// `None` means no successful collection has happened yet (or the platform
/// is not Windows); the telemetry payload omits the field in that case, so
/// the server keeps whatever snapshot it already has.
pub struct IisCache {
    inner: Mutex<Option<Arc<IisSection>>>,
}

impl IisCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Current snapshot, if any collection has ever succeeded.
    pub fn snapshot(&self) -> Option<Arc<IisSection>> {
        self.inner.lock().ok().and_then(|g| g.clone())
    }

    fn store(&self, section: IisSection) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(Arc::new(section));
        }
    }
}

impl Default for IisCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect the current IIS inventory once. `Ok(None)` means the platform has
/// no IIS concept (non-Windows) and the payload should omit the section.
pub fn collect() -> anyhow::Result<Option<IisSection>> {
    platform::collect()
}

/// Background refresher loop: recollect the IIS inventory every
/// `REFRESH_INTERVAL`. Runs forever; a failed collection only delays the
/// next snapshot and is logged, never fatal.
pub fn run_refresher(cache: Arc<IisCache>) {
    loop {
        match collect() {
            Ok(Some(section)) => cache.store(section),
            Ok(None) => {}
            Err(e) => tracing::warn!("IIS inventory collection failed: {}", e),
        }
        std::thread::sleep(REFRESH_INTERVAL);
    }
}

// ── Parser (compiled on every platform so tests run anywhere) ───────────────

/// Parse the collector script's stdout into a section. The script prints
/// either the marker `NOT_INSTALLED` or a JSON array of sites (`null` when
/// IIS has no sites).
pub(crate) fn parse(output: &str) -> Option<IisSection> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("NOT_INSTALLED") {
        return Some(IisSection {
            installed: false,
            sites: Vec::new(),
        });
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if value.is_null() {
        return Some(IisSection {
            installed: true,
            sites: Vec::new(),
        });
    }
    let sites: Vec<IisSite> = serde_json::from_value(value).ok()?;
    Some(IisSection {
        installed: true,
        sites,
    })
}

// ── Platform collectors ─────────────────────────────────────────────────────

#[cfg(windows)]
mod platform {
    use super::{parse, IisSection};
    use anyhow::{bail, Context};
    use std::process::{Command, Stdio};

    /// WebAdministration dump (fixed literal; takes no external input).
    /// Prints `NOT_INSTALLED` when the module is missing, else the sites as
    /// a JSON array. `@(...)` keeps even a single site an array.
    const IIS_LIST_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
if (-not (Get-Module -ListAvailable -Name WebAdministration)) {
    Write-Output 'NOT_INSTALLED'
    exit 0
}
Import-Module WebAdministration
$sites = @(Get-Website | ForEach-Object {
    $pool = $_.applicationPool
    $poolState = ''
    try { $poolState = "$((Get-WebAppPoolState -Name $pool).Value)" } catch {}
    $bindings = @($_.bindings.Collection | ForEach-Object { "$($_.bindingInformation)" })
    [PSCustomObject]@{
        name = $_.name
        state = "$($_.state)"
        app_pool = $pool
        app_pool_state = $poolState
        physical_path = $_.physicalPath
        bindings = $bindings
    }
})
ConvertTo-Json -Compress -Depth 4 -InputObject $sites
"#;

    pub fn collect() -> anyhow::Result<Option<IisSection>> {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                IIS_LIST_SCRIPT,
            ])
            .stdin(Stdio::null())
            .output()
            .context("failed to run IIS inventory script")?;
        if !out.status.success() {
            bail!(
                "IIS inventory script failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(parse(&String::from_utf8_lossy(&out.stdout)))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::IisSection;

    pub fn collect() -> anyhow::Result<Option<IisSection>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sites_array() {
        let out = r#"[{"name":"billing-web","state":"Started","app_pool":"billing-web","app_pool_state":"Started","physical_path":"C:\\inetpub\\sites\\billing-web","bindings":["*:80:billing.example.com"]},{"name":"Default Web Site","state":"Stopped","app_pool":"DefaultAppPool","app_pool_state":"Stopped","physical_path":"%SystemDrive%\\inetpub\\wwwroot","bindings":["*:80:"]}]"#;
        let section = parse(out).unwrap();
        assert!(section.installed);
        assert_eq!(section.sites.len(), 2);
        assert_eq!(section.sites[0].name, "billing-web");
        assert_eq!(section.sites[0].state, "Started");
        assert_eq!(section.sites[0].app_pool, "billing-web");
        assert_eq!(section.sites[0].bindings, vec!["*:80:billing.example.com"]);
        assert_eq!(section.sites[1].name, "Default Web Site");
    }

    #[test]
    fn parses_empty_and_null_site_lists() {
        let section = parse("[]").unwrap();
        assert!(section.installed);
        assert!(section.sites.is_empty());

        let section = parse("null").unwrap();
        assert!(section.installed);
        assert!(section.sites.is_empty());
    }

    #[test]
    fn parses_not_installed_marker() {
        let section = parse("NOT_INSTALLED\n").unwrap();
        assert!(!section.installed);
        assert!(section.sites.is_empty());
    }

    #[test]
    fn rejects_garbage_and_empty_output() {
        assert!(parse("").is_none());
        assert!(parse("   \n").is_none());
        assert!(parse("this is not json").is_none());
        assert!(parse(r#"{"unexpected":"object"}"#).is_none());
    }

    #[test]
    fn cache_stores_and_returns_snapshot() {
        let cache = IisCache::new();
        assert!(cache.snapshot().is_none());
        cache.store(IisSection {
            installed: true,
            sites: vec![IisSite {
                name: "billing-web".into(),
                state: "Started".into(),
                app_pool: "billing-web".into(),
                app_pool_state: "Started".into(),
                physical_path: String::new(),
                bindings: vec![],
            }],
        });
        let snap = cache.snapshot().unwrap();
        assert!(snap.installed);
        assert_eq!(snap.sites.len(), 1);
    }
}
