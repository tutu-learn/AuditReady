use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SoftwareEntry {
    pub name: String,
    pub version: Option<String>,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub scanned_at: DateTime<Utc>,
    pub software_count: usize,
    pub software: Vec<SoftwareEntry>,
}
