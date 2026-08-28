//! Helper to spawn child processes without popping a console window on
//! Windows. In client mode the agent detaches from its console; without
//! CREATE_NO_WINDOW each console child would allocate a fresh visible window.

/// Spawn the child without allocating a console window (Windows only;
/// no-op elsewhere).
pub trait CommandExtNoWindow {
    fn no_window(&mut self) -> &mut Self;
}

impl CommandExtNoWindow for std::process::Command {
    #[cfg(windows)]
    fn no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }

    #[cfg(not(windows))]
    fn no_window(&mut self) -> &mut Self {
        self
    }
}
