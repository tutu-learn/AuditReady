use std::sync::atomic::AtomicU64;
use std::sync::Arc;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// At most one mouse-movement event is counted per this interval, so a drag
/// (hundreds of raw move events) stays a small, meaningful activity signal.
pub(crate) const MIN_EVENT_INTERVAL_MS: u64 = 100;

/// Start the platform mouse-movement monitor and return a shared counter.
///
/// Spawns a background hook thread that increments the counter (throttled to
/// [`MIN_EVENT_INTERVAL_MS`]). The report loop reads it with `swap(0, ..)` so
/// each report carries a per-period count. On platforms without mouse support
/// the counter simply stays 0.
pub fn start() -> Arc<AtomicU64> {
    let counter = Arc::new(AtomicU64::new(0));
    #[cfg(target_os = "macos")]
    {
        macos::start(Arc::clone(&counter));
    }
    #[cfg(windows)]
    {
        windows::start(Arc::clone(&counter));
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        tracing::warn!("mouse activity monitoring is not supported on this platform");
    }
    counter
}
