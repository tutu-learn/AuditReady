//! Tray icon + stats dashboard window for client mode.
//!
//! Runs on the process's main thread (required for the tray icon on macOS,
//! and simplest cross-platform). Background report loops run on their own
//! threads/tasks and publish into `stats::SharedStats`, which this UI polls
//! on a timer — there is no other coupling between the two.
//!
//! The tray icon alone answers "is it running"; clicking it (or its "Open
//! Dashboard" item) opens a small window with connection status and the
//! activity counters from `stats::ClientStats`.
//!
//! Supported on Windows and macOS only, matching where client mode's
//! clipboard/mouse monitoring already works (see `super::clipboard`,
//! `super::mouse`). `tray-icon`/`muda` pull in GTK + libappindicator +
//! libxdo on Linux, which don't link against the static-musl target the
//! Linux release build uses — rather than break that build, Linux keeps
//! running headless, same as clipboard/mouse already do there.

#[cfg(any(target_os = "macos", windows))]
pub use dashboard::run;

#[cfg(not(any(target_os = "macos", windows)))]
pub fn run(_stats: super::stats::SharedStats) -> anyhow::Result<()> {
    tracing::warn!(
        "client tray/dashboard UI is not supported on this platform (Windows or macOS only); running headless"
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(any(target_os = "macos", windows))]
mod dashboard;
#[cfg(any(target_os = "macos", windows))]
mod icon;
#[cfg(any(target_os = "macos", windows))]
mod theme;
#[cfg(any(target_os = "macos", windows))]
mod tray;
