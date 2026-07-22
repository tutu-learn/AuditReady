//! Best-effort collection of pending OS updates for telemetry.
//!
//! The server keeps only the latest `pending_updates` snapshot per host, so
//! the full current list is reported on every telemetry cycle. Collection can
//! be slow (Windows Update search, `softwareupdate -l`), so it runs on a
//! dedicated background thread and the telemetry loop only reads the cached
//! snapshot — a failed or slow check never delays or breaks telemetry.

use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the cached pending-update list is refreshed.
const REFRESH_INTERVAL: Duration = Duration::from_secs(600);

/// One pending OS-level update, as reported in telemetry `pending_updates`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PendingUpdate {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub source: String,
    pub kb: String,
}

/// Shared cache holding the most recent successful collection.
///
/// `None` means no successful collection has happened yet (or the platform is
/// unsupported); the telemetry payload omits the field in that case, so the
/// server keeps whatever snapshot it already has.
pub struct PendingUpdatesCache {
    inner: Mutex<Option<Arc<Vec<PendingUpdate>>>>,
}

impl PendingUpdatesCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Current snapshot, if any collection has ever succeeded.
    pub fn snapshot(&self) -> Option<Arc<Vec<PendingUpdate>>> {
        self.inner.lock().ok().and_then(|g| g.clone())
    }

    fn store(&self, updates: Vec<PendingUpdate>) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(Arc::new(updates));
        }
    }
}

impl Default for PendingUpdatesCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect the current pending-update list once. Used by the background
/// refresher and the `--print-updates` debug flag.
pub fn collect() -> anyhow::Result<Vec<PendingUpdate>> {
    platform::collect()
}

/// Background refresher loop: recollect pending updates every
/// `REFRESH_INTERVAL`. Runs forever; a failed collection only delays the next
/// snapshot and is logged, never fatal.
pub fn run_refresher(cache: Arc<PendingUpdatesCache>) {
    loop {
        match collect() {
            Ok(list) => {
                tracing::debug!("pending updates: {} available", list.len());
                cache.store(list);
            }
            Err(e) => tracing::warn!("pending update collection failed: {}", e),
        }
        std::thread::sleep(REFRESH_INTERVAL);
    }
}

// ── Pure parsers (compiled on every platform so tests run anywhere) ──────────

// Each parser is only called by one platform's collector, so on any given
// build some of them are unused outside tests.
#[allow(dead_code)]
pub(crate) mod parse {
    use super::PendingUpdate;

    /// Parse `apt list --upgradable` output.
    ///
    /// Lines look like:
    /// `openssl/jammy-updates,jammy-security 3.0.2-0ubuntu1.25 amd64 [upgradable from: 3.0.2]`
    pub fn apt(output: &str) -> Vec<PendingUpdate> {
        output
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let (name, rest) = line.split_once('/')?;
                if name.is_empty() || name.contains(char::is_whitespace) {
                    return None;
                }
                let mut parts = rest.split_whitespace();
                let _suites = parts.next()?;
                let version = parts.next()?;
                Some(PendingUpdate {
                    id: name.to_string(),
                    title: format!("{} {}", name, version),
                    severity: String::new(),
                    source: "apt".to_string(),
                    kb: String::new(),
                })
            })
            .collect()
    }

    /// Parse `dnf check-update` output.
    ///
    /// Package lines look like: `openssl.x86_64   1:3.0.7-28.el9   updates`
    pub fn dnf(output: &str) -> Vec<PendingUpdate> {
        output
            .lines()
            .filter_map(|line| {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() < 3 {
                    return None;
                }
                let (name, version) = (cols[0], cols[1]);
                // Header/junk lines don't have an arch suffix on the name.
                let id = strip_rpm_arch(name)?;
                Some(PendingUpdate {
                    title: format!("{} {}", id, version),
                    id,
                    severity: String::new(),
                    source: "dnf".to_string(),
                    kb: String::new(),
                })
            })
            .collect()
    }

    fn strip_rpm_arch(name: &str) -> Option<String> {
        let (base, arch) = name.rsplit_once('.')?;
        const ARCHES: [&str; 7] = [
            "x86_64", "noarch", "aarch64", "i686", "i386", "armv7hl", "ppc64le",
        ];
        if ARCHES.contains(&arch) && !base.is_empty() {
            Some(base.to_string())
        } else {
            None
        }
    }

    /// Parse `softwareupdate -l` output.
    ///
    /// Modern format: `* Label: macOS Sonoma 14.4-23E214`
    /// Older format:  `   * macOS 12.3-21E230`
    pub fn softwareupdate(output: &str) -> Vec<PendingUpdate> {
        output
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let label = if let Some(idx) = line.find("Label:") {
                    line[idx + "Label:".len()..].trim()
                } else if let Some(rest) = line.strip_prefix("* ") {
                    rest.trim()
                } else {
                    return None;
                };
                if label.is_empty() {
                    return None;
                }
                Some(PendingUpdate {
                    id: label.to_string(),
                    title: label.to_string(),
                    severity: String::new(),
                    source: "softwareupdate".to_string(),
                    kb: String::new(),
                })
            })
            .collect()
    }

    /// Parse the tab-separated output of the Windows WUA collector script:
    /// `id\ttitle\tkb` per line.
    pub fn windows(output: &str) -> Vec<PendingUpdate> {
        output
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let id = parts.next()?.trim();
                if id.is_empty() {
                    return None;
                }
                let title = parts.next().unwrap_or("").trim().to_string();
                let kb = parts.next().unwrap_or("").trim().to_string();
                Some(PendingUpdate {
                    id: id.to_string(),
                    title,
                    severity: String::new(),
                    source: "windows_update".to_string(),
                    kb,
                })
            })
            .collect()
    }
}

// ── Platform collectors ─────────────────────────────────────────────────────

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::{parse, PendingUpdate};
    use anyhow::{bail, Context};
    use std::process::{Command, Stdio};

    pub fn collect() -> anyhow::Result<Vec<PendingUpdate>> {
        if command_exists("apt") {
            collect_apt()
        } else if command_exists("dnf") {
            collect_dnf()
        } else {
            bail!("no supported package manager (apt or dnf) found")
        }
    }

    pub fn command_exists(name: &str) -> bool {
        Command::new(name)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn collect_apt() -> anyhow::Result<Vec<PendingUpdate>> {
        let out = Command::new("apt")
            .args(["list", "--upgradable"])
            .stdin(Stdio::null())
            .output()
            .context("failed to run `apt list --upgradable`")?;
        if !out.status.success() {
            bail!("apt list --upgradable exited with {}", out.status);
        }
        Ok(parse::apt(&String::from_utf8_lossy(&out.stdout)))
    }

    fn collect_dnf() -> anyhow::Result<Vec<PendingUpdate>> {
        let out = Command::new("dnf")
            .args(["check-update", "-q"])
            .stdin(Stdio::null())
            .output()
            .context("failed to run `dnf check-update`")?;
        // dnf check-update exits 100 when updates are available, 0 when none.
        match out.status.code() {
            Some(0) | Some(100) => Ok(parse::dnf(&String::from_utf8_lossy(&out.stdout))),
            _ => bail!("dnf check-update exited with {}", out.status),
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{parse, PendingUpdate};
    use anyhow::Context;
    use std::process::{Command, Stdio};

    pub fn collect() -> anyhow::Result<Vec<PendingUpdate>> {
        let out = Command::new("softwareupdate")
            .arg("-l")
            .stdin(Stdio::null())
            .output()
            .context("failed to run `softwareupdate -l`")?;
        // `softwareupdate -l` can exit non-zero when no updates are available;
        // whatever it printed on stdout is still parseable either way.
        Ok(parse::softwareupdate(&String::from_utf8_lossy(&out.stdout)))
    }
}

#[cfg(windows)]
mod platform {
    use super::{parse, PendingUpdate};
    use anyhow::{bail, Context};
    use std::process::{Command, Stdio};

    /// WUA COM script (fixed literal; takes no external input). Prints
    /// `id\ttitle\tkb` per pending software update.
    const WUA_LIST_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$session = New-Object -ComObject Microsoft.Update.Session
$result = $session.CreateUpdateSearcher().Search("IsInstalled=0 and Type='Software'")
foreach ($u in $result.Updates) {
    $kb = ''
    if ($u.KBArticleIDs -and $u.KBArticleIDs.Count -gt 0) { $kb = 'KB' + $u.KBArticleIDs.Item(0) }
    $id = if ($kb) { $kb } else { $u.Title }
    Write-Output ($id + "`t" + $u.Title + "`t" + $kb)
}
"#;

    pub fn collect() -> anyhow::Result<Vec<PendingUpdate>> {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                WUA_LIST_SCRIPT,
            ])
            .stdin(Stdio::null())
            .output()
            .context("failed to run Windows Update search")?;
        if !out.status.success() {
            bail!(
                "Windows Update search failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(parse::windows(&String::from_utf8_lossy(&out.stdout)))
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_apt_upgradable() {
        let out = "Listing... Done\n\
                   openssl/jammy-updates,jammy-security 3.0.2-0ubuntu1.25 amd64 [upgradable from: 3.0.2]\n\
                   libssl3/jammy-updates 3.0.2-0ubuntu1.25 amd64 [upgradable from: 3.0.2]\n";
        let updates = parse::apt(out);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].id, "openssl");
        assert_eq!(updates[0].title, "openssl 3.0.2-0ubuntu1.25");
        assert_eq!(updates[0].source, "apt");
        assert_eq!(updates[0].kb, "");
        assert_eq!(updates[1].id, "libssl3");
    }

    #[test]
    fn parses_apt_empty() {
        assert!(parse::apt("Listing... Done\n").is_empty());
    }

    #[test]
    fn parses_dnf_check_update() {
        let out = "\nLast metadata expiration check: 0:12:34 ago on Mon 21 Jul 2026.\n\
                   \n\
                   openssl.x86_64                    1:3.0.7-28.el9_0        updates\n\
                   krb5-libs.noarch                  1.20.1-2.el9            updates\n";
        let updates = parse::dnf(out);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].id, "openssl");
        assert_eq!(updates[0].title, "openssl 1:3.0.7-28.el9_0");
        assert_eq!(updates[0].source, "dnf");
        assert_eq!(updates[1].id, "krb5-libs");
    }

    #[test]
    fn parses_softwareupdate_modern_format() {
        let out = "Software Update Tool\n\n\
                   Finding available software\n\
                   Software Update found the following new or updated software:\n\
                   * Label: macOS Sonoma 14.4-23E214\n\
                   \tTitle: macOS Sonoma 14.4, Version: 14.4, Size: 1000K, Recommended: YES, Action: restart,\n\
                   * Label: Command Line Tools for Xcode-15.3\n\
                   \tTitle: Command Line Tools for Xcode, Version: 15.3, Size: 1000K, Recommended: YES,\n";
        let updates = parse::softwareupdate(out);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].id, "macOS Sonoma 14.4-23E214");
        assert_eq!(updates[0].source, "softwareupdate");
        assert_eq!(updates[1].id, "Command Line Tools for Xcode-15.3");
    }

    #[test]
    fn parses_softwareupdate_legacy_format() {
        let out = "Software Update found the following new or updated software:\n   * macOS 12.3-21E230\n";
        let updates = parse::softwareupdate(out);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id, "macOS 12.3-21E230");
    }

    #[test]
    fn parses_softwareupdate_none_available() {
        assert!(parse::softwareupdate("No new software available.\n").is_empty());
    }

    #[test]
    fn parses_windows_wua_output() {
        let out = "KB5034441\t2024-01 Security Update for Windows 11\tKB5034441\n\
                   Some driver update\tVendor driver update\t\n";
        let updates = parse::windows(out);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].id, "KB5034441");
        assert_eq!(updates[0].kb, "KB5034441");
        assert_eq!(updates[0].source, "windows_update");
        assert_eq!(updates[1].id, "Some driver update");
        assert_eq!(updates[1].kb, "");
    }
}
