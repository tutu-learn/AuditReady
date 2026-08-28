//! Client mode: user file-change + clipboard monitoring with sensitive-data
//! flagging, reported to `POST /audit_ready/client-report`.

mod clipboard;
mod file_scan;
pub(crate) mod report;
mod runner;
mod sensitive;

pub use runner::run;
