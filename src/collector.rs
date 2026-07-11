use crate::models::{AuditReport, SoftwareEntry};
use anyhow::Result;
use chrono::Utc;

pub fn collect() -> Result<AuditReport> {
    let software = platform::installed_software()?;
    let hostname = hostname();
    let (os, os_version) = os_info();

    Ok(AuditReport {
        hostname,
        os,
        os_version,
        scanned_at: Utc::now(),
        software_count: software.len(),
        software,
    })
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string())
        .trim()
        .to_string()
}

fn os_info() -> (String, String) {
    let os = std::env::consts::OS.to_string();
    let version = platform::os_version().unwrap_or_else(|| "unknown".to_string());
    (os, version)
}

// ── Platform-specific implementations ────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::process::Command;

    pub fn os_version() -> Option<String> {
        let out = Command::new("sw_vers").arg("-productVersion").output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn installed_software() -> Result<Vec<SoftwareEntry>> {
        let mut entries: Vec<SoftwareEntry> = Vec::new();

        entries.extend(scan_applications_folder("/Applications"));
        entries.extend(scan_applications_folder(&format!(
            "{}/Applications",
            std::env::var("HOME").unwrap_or_default()
        )));
        entries.extend(homebrew_formulae());
        entries.extend(homebrew_casks());
        entries.extend(mas_apps());

        entries.dedup_by(|a, b| a.name == b.name);
        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(entries)
    }

    fn scan_applications_folder(folder: &str) -> Vec<SoftwareEntry> {
        let mut apps = Vec::new();
        let read = match std::fs::read_dir(folder) {
            Ok(r) => r,
            Err(_) => return apps,
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("app") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown")
                .to_string();
            let version = read_plist_value(
                &path.join("Contents/Info.plist"),
                "CFBundleShortVersionString",
            );
            apps.push(SoftwareEntry {
                name,
                version,
                source: "Applications".to_string(),
            });
        }
        apps
    }

    fn read_plist_value(plist_path: &std::path::Path, key: &str) -> Option<String> {
        let out = Command::new("defaults")
            .arg("read")
            .arg(plist_path.to_string_lossy().trim_end_matches(".plist"))
            .arg(key)
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    }

    fn homebrew_formulae() -> Vec<SoftwareEntry> {
        let out = match Command::new("brew")
            .args(["list", "--formula", "--versions"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return vec![],
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let mut parts = line.splitn(2, ' ');
                let name = parts.next().unwrap_or("").to_string();
                let version = parts.next().map(|v| v.trim().to_string());
                SoftwareEntry { name, version, source: "Homebrew formula".to_string() }
            })
            .collect()
    }

    fn homebrew_casks() -> Vec<SoftwareEntry> {
        let out = match Command::new("brew")
            .args(["list", "--cask", "--versions"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return vec![],
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let mut parts = line.splitn(2, ' ');
                let name = parts.next().unwrap_or("").to_string();
                let version = parts.next().map(|v| v.trim().to_string());
                SoftwareEntry { name, version, source: "Homebrew cask".to_string() }
            })
            .collect()
    }

    fn mas_apps() -> Vec<SoftwareEntry> {
        let out = match Command::new("mas").arg("list").output() {
            Ok(o) => o,
            Err(_) => return vec![],
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let without_id = line.trim_start_matches(|c: char| c.is_ascii_digit()).trim();
                let (name, version) = if let Some(idx) = without_id.rfind('(') {
                    let n = without_id[..idx].trim().to_string();
                    let v = without_id[idx + 1..].trim_end_matches(')').trim().to_string();
                    (n, Some(v))
                } else {
                    (without_id.to_string(), None)
                };
                SoftwareEntry { name, version, source: "Mac App Store".to_string() }
            })
            .collect()
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use winreg::enums::*;
    use winreg::RegKey;

    pub fn os_version() -> Option<String> {
        let key = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
            .ok()?;
        let product: String = key.get_value("ProductName").ok()?;
        let build: String = key.get_value("CurrentBuildNumber").ok().unwrap_or_default();
        Some(format!("{product} (build {build})"))
    }

    pub fn installed_software() -> Result<Vec<SoftwareEntry>> {
        let mut entries = Vec::new();

        let hives = [
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
            (HKEY_CURRENT_USER,  "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
        ];

        for (root, path) in &hives {
            let root_key = RegKey::predef(*root);
            if let Ok(uninstall) = root_key.open_subkey(path) {
                for key_name in uninstall.enum_keys().flatten() {
                    if let Ok(sub) = uninstall.open_subkey(&key_name) {
                        let display_name: Option<String> = sub.get_value("DisplayName").ok();
                        let name = match display_name {
                            Some(n) if !n.trim().is_empty() => n,
                            _ => continue,
                        };
                        let version: Option<String> = sub.get_value("DisplayVersion").ok();
                        entries.push(SoftwareEntry {
                            name,
                            version,
                            source: "Windows Registry".to_string(),
                        });
                    }
                }
            }
        }

        entries.dedup_by(|a, b| a.name == b.name && a.version == b.version);
        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(entries)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn os_version() -> Option<String> {
        std::fs::read_to_string("/etc/os-release").ok().and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
        })
    }

    pub fn installed_software() -> Result<Vec<SoftwareEntry>> {
        let out = std::process::Command::new("dpkg-query")
            .args(["-W", "-f=${Package}\t${Version}\n"])
            .output();
        if let Ok(out) = out {
            return Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    let mut parts = line.splitn(2, '\t');
                    SoftwareEntry {
                        name: parts.next().unwrap_or("").to_string(),
                        version: parts.next().map(|s| s.to_string()),
                        source: "dpkg".to_string(),
                    }
                })
                .collect());
        }
        Ok(vec![])
    }
}
