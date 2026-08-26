use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SoftwareEntry {
    pub name: String,
    pub version: Option<String>,
    pub source: String,
}

/// One mounted disk/volume and its space usage.
#[derive(Debug, Clone, Serialize)]
pub struct DiskEntry {
    pub mount_point: String,
    pub name: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub scanned_at: DateTime<Utc>,
    pub software_count: usize,
    pub software: Vec<SoftwareEntry>,
    pub disks: Vec<DiskEntry>,
}
