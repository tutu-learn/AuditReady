use chrono::{DateTime, Utc};
use std::sync::mpsc::Receiver;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Clipboard text is read up to this many bytes at capture time (no
/// unbounded reads). The report applies the configured threshold/truncation
/// on top of this capture.
pub(crate) const MAX_CAPTURE_BYTES: usize = 256 * 1024;

/// One clipboard activity event.
#[derive(Debug)]
pub struct ClipboardEvent {
    pub ts: DateTime<Utc>,
    pub kind: EventKind,
    /// Source program for copies, destination program for pastes.
    pub app: Option<String>,
    pub size_bytes: u64,
    /// Clipboard text, capped at [`MAX_CAPTURE_BYTES`]. `None` when the
    /// clipboard held no text.
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Clipboard content changed.
    Copy,
    /// Ctrl+V / Cmd+V observed.
    Paste,
}

/// Start the platform clipboard monitor and return the event receiver.
///
/// Spawns background threads: one polling for copy events, one watching for
/// paste shortcuts. On platforms without clipboard support the returned
/// receiver simply never yields events.
pub fn start() -> Receiver<ClipboardEvent> {
    #[cfg(target_os = "macos")]
    {
        macos::start()
    }
    #[cfg(windows)]
    {
        windows::start()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        tracing::warn!("clipboard monitoring is not supported on this platform");
        let (_tx, rx) = std::sync::mpsc::channel();
        rx
    }
}
