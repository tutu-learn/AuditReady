# AuditReady

AuditReady is a lightweight agent written in Rust that audits installed software
on endpoints and reports to a central backend over a WebSocket connection. It
supports Linux, Windows, and macOS, and can optionally expose a remote shell
tunnel for the backend.

## Installation

### Linux (systemd service)

Download the installer and run it (recommended, so prompts work interactively):

```bash
wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install.sh
chmod +x install.sh
sudo ./install.sh
```

Or pipe it directly:

```bash
curl -fsSL https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install.sh | sudo bash
```

The installer will:

- Detect your architecture (`x86_64` or `aarch64`) and download the latest
  release binary. The Linux binary is statically linked (musl), so it runs on
  any distribution regardless of glibc version. Prebuilt Linux releases
  currently ship `x86_64` only.
- Install `auditready` and the `auditready-restart` helper to `/usr/local/bin`.
- Prompt for the backend domain and agent token, and write the configuration
  to `/etc/auditready/appsettings.json` (mode `600`).
- Create, enable, and start the `auditready` systemd service.

For non-interactive / automated installs, pass the values as environment
variables:

```bash
sudo DOMAIN=api.example.com TOKEN=abc123 ./install.sh
```

Optional environment variables: `VERSION` (release tag, default `latest`),
`INSTALL_DIR` (default `/usr/local/bin`), `SERVICE_USER` (default `root`).

Verify the service after installation:

```bash
systemctl status auditready
journalctl -u auditready -f
```

### Windows

1. Download `auditready-x86_64-pc-windows-msvc.zip` (or the `.msi` installer)
   from the [Releases](https://github.com/tutu-learn/AuditReady/releases) page.
2. Extract the archive.
3. Rename `appsettings.example.json` to `appsettings.json` and set your backend
   `domain` and `token`.
4. Run `auditready.exe` from that directory (the agent reads `appsettings.json`
   from its working directory).

### macOS

1. Download `auditready-x86_64-apple-darwin.zip` (Intel) or
   `auditready-aarch64-apple-darwin.zip` (Apple Silicon) from the
   [Releases](https://github.com/tutu-learn/AuditReady/releases) page.
2. Extract the archive.
3. Rename `appsettings.example.json` to `appsettings.json` and set your backend
   `domain` and `token`.
4. Run `./auditready` from that directory.

### Build from source

Requires a stable Rust toolchain (see `rust-toolchain.toml`):

```bash
cargo build --release
```

The binary is at `target/release/auditready`. Copy `appsettings.json` next to
it (or into your working directory), set `domain` and `token`, and run it.

## Configuration

The agent reads `appsettings.json` from its working directory. On Linux
installs this is `/etc/auditready/appsettings.json`:

```json
{
  "server": {
    "domain": "api.example.com",
    "token": "your-agent-token",
    "interval_seconds": 10,
    "tunnel_enabled": true,
    "tunnel_shell": null,
    "tunnel_cwd": "/root"
  }
}
```

- `domain` — backend host (with optional port); no `http(s)://` prefix.
- `token` — agent token used to authenticate with the backend.
- `interval_seconds` — how often the agent reports.
- `tunnel_enabled` — allow the backend to open a remote shell tunnel.
- `tunnel_shell` — shell to use for the tunnel (`null` = system default).
- `tunnel_cwd` — working directory for the tunnel shell.

## Managing the agent (Linux)

```bash
# Status and logs
systemctl status auditready
journalctl -u auditready -f

# Restart
sudo auditready-restart

# Update the agent token
wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update-token.sh
chmod +x update-token.sh
sudo ./update-token.sh            # prompts for the new token
sudo ./update-token.sh abc123     # or pass it directly
```

## Uninstall (Linux)

```bash
sudo systemctl disable --now auditready.service
sudo rm /etc/systemd/system/auditready.service
sudo systemctl daemon-reload
sudo rm -f /usr/local/bin/auditready /usr/local/bin/auditready-restart
sudo rm -rf /etc/auditready
```

## License

See [LICENSE](LICENSE).
