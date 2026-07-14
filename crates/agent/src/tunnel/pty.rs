use anyhow::{Context, Result};
use auditready_protocol::{ChannelId, TunnelMessage};
use portable_pty::{CommandBuilder, NativePtySystem, PtyPair, PtySize, PtySystem};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const READ_BUF_SIZE: usize = 4096;
/// Maximum number of outstanding control messages per PTY channel.
const PTY_CONTROL_CAPACITY: usize = 256;

/// Handle to a running channel PTY.
pub struct ChannelPty {
    ctrl_tx: mpsc::Sender<PtyControl>,
    _reader_handle: JoinHandle<()>,
    _writer_handle: JoinHandle<()>,
}

enum PtyControl {
    Data(Vec<u8>),
    Resize(PtySize),
    Kill,
}

impl ChannelPty {
    /// Send raw bytes to the PTY stdin.
    pub fn send(&self, data: Vec<u8>) {
        if let Err(e) = self.ctrl_tx.try_send(PtyControl::Data(data)) {
            tracing::warn!("pty control queue full; dropping input: {}", e);
        }
    }

    /// Resize the PTY.
    pub fn resize(&self, size: PtySize) {
        let _ = self.ctrl_tx.try_send(PtyControl::Resize(size));
    }

    /// Signal the shell to exit and tear down the channel.
    pub fn close(&self) {
        let _ = self.ctrl_tx.try_send(PtyControl::Kill);
    }
}

impl Drop for ChannelPty {
    fn drop(&mut self) {
        self.close();
    }
}

/// Spawn a PTY running the configured shell for this channel.
pub fn spawn(
    channel_id: ChannelId,
    shell: Option<String>,
    cwd: Option<String>,
    initial_size: PtySize,
    broker_tx: mpsc::Sender<TunnelMessage>,
) -> Result<ChannelPty> {
    let shell = shell.unwrap_or_else(default_shell);
    validate_shell_command(&shell)?;

    let pty_system = NativePtySystem::default();
    let pair = pty_system.openpty(initial_size).context("open pty")?;

    let mut cmd = CommandBuilder::new(&shell);
    let cwd = cwd.map(PathBuf::from).or_else(|| std::env::home_dir());
    cmd.cwd(cwd.unwrap_or_else(|| std::path::PathBuf::from("/")));

    // Terminal setup so colors and interactive apps behave as expected.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if std::env::var_os("PATH").is_none() {
        cmd.env(
            "PATH",
            "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        );
    }

    let child = pair.slave.spawn_command(cmd).context("spawn shell")?;

    let reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let writer = pair.master.take_writer().context("take pty writer")?;

    let (ctrl_tx, ctrl_rx) = mpsc::channel(PTY_CONTROL_CAPACITY);

    let reader_handle = spawn_reader(channel_id, reader, broker_tx);
    let writer_handle = spawn_writer(pair, writer, child, ctrl_rx);

    Ok(ChannelPty {
        ctrl_tx,
        _reader_handle: reader_handle,
        _writer_handle: writer_handle,
    })
}

fn validate_shell_command(command: &str) -> Result<()> {
    if command.is_empty() {
        anyhow::bail!("shell command is empty");
    }
    if command.bytes().any(|b| b == b'\n' || b == b'\0') {
        anyhow::bail!("shell command contains invalid characters");
    }
    Ok(())
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

fn spawn_reader(
    channel_id: ChannelId,
    mut reader: Box<dyn std::io::Read + Send>,
    broker_tx: mpsc::Sender<TunnelMessage>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; READ_BUF_SIZE];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    let msg = TunnelMessage::ChannelData { channel_id, data };
                    if broker_tx.blocking_send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(channel_id = %channel_id.0, "pty read error: {}", e);
                    break;
                }
            }
        }
        tracing::debug!(channel_id = %channel_id.0, "pty reader exited");
    })
}

fn spawn_writer(
    pair: PtyPair,
    mut writer: Box<dyn std::io::Write + Send>,
    mut child: Box<dyn portable_pty::Child + Send>,
    mut ctrl_rx: mpsc::Receiver<PtyControl>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        while let Some(msg) = ctrl_rx.blocking_recv() {
            match msg {
                PtyControl::Data(data) => {
                    if let Err(e) = writer.write_all(&data) {
                        tracing::debug!("pty write error: {}", e);
                        break;
                    }
                    if let Err(e) = writer.flush() {
                        tracing::debug!("pty flush error: {}", e);
                        break;
                    }
                }
                PtyControl::Resize(size) => {
                    if let Err(e) = pair.master.resize(size) {
                        tracing::debug!("pty resize error: {}", e);
                    }
                }
                PtyControl::Kill => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        tracing::debug!("pty writer exited");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn spawn_and_echo() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let (tx, mut rx) = mpsc::channel(64);
        let pty = spawn(
            ChannelId::new(),
            Some("/bin/sh".to_string()),
            None,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            tx,
        )
        .expect("spawn pty");

        pty.send(b"echo auditready_pty_test\n".to_vec());

        // Wait for the echoed string to come back.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut found = false;
        while std::time::Instant::now() < deadline && !found {
            match rx.try_recv() {
                Ok(TunnelMessage::ChannelData { data, .. }) => {
                    let text = String::from_utf8_lossy(&data);
                    if text.contains("auditready_pty_test") {
                        found = true;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }

        pty.close();
        assert!(found, "did not receive echo from PTY");
    }

    #[test]
    fn rejects_command_with_newline() {
        let result = validate_shell_command("/bin/sh\n/bin/bash");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_empty_command() {
        let result = validate_shell_command("");
        assert!(result.is_err());
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn pty_has_xterm_environment() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let (tx, mut rx) = mpsc::channel(64);
        let pty = spawn(
            ChannelId::new(),
            Some("/bin/sh".to_string()),
            None,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            tx,
        )
        .expect("spawn pty");

        pty.send(b"echo TERM=$TERM\n".to_vec());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut found = false;
        while std::time::Instant::now() < deadline && !found {
            match rx.try_recv() {
                Ok(TunnelMessage::ChannelData { data, .. }) => {
                    let text = String::from_utf8_lossy(&data);
                    if text.contains("TERM=xterm-256color") {
                        found = true;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }

        pty.close();
        assert!(found, "did not see TERM=xterm-256color in PTY output");
    }
}
