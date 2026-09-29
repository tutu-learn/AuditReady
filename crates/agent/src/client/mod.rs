//! Client mode: user file-change + clipboard/mouse-activity monitoring with
//! sensitive-data flagging, reported to `POST /audit_ready/client-report`.

pub(crate) mod actions;
pub(crate) mod alerts;
mod clipboard;
mod file_scan;
mod mouse;
pub(crate) mod report;
mod runner;
mod sensitive;
pub mod frontend;
pub mod stats;

pub use runner::run;
