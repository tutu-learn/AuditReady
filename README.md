# AuditReady

AuditReady is a lightweight agent written in Rust that audits installed software
on endpoints and reports to a central backend over a WebSocket connection. It
supports Linux, Windows, and macOS, and can optionally expose a remote shell
tunnel for the backend.

## Installation

### Linux (systemd service)

DNS traffic capture on Linux uses an `AF_PACKET` socket and requires the agent
to run as root. The installer creates the systemd service as `root` by default.

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
- Create, enable, and start the `auditready` systemd service as `root`.

For non-interactive / automated installs, pass the values as environment
variables:

```bash
sudo DOMAIN=api.example.com TOKEN=abc123 ./install.sh
```

Optional environment variables: `VERSION` (release tag, default `latest`),
`INSTALL_DIR` (default `/usr/local/bin`), `SERVICE_USER` (default `root`).

> Do not change `SERVICE_USER` away from `root` unless you do not need DNS
> traffic capture; the `AF_PACKET` socket requires root privileges.

Verify the service after installation:

```bash
systemctl status auditready
journalctl -u auditready -f
```

### Windows

The installer must run as Administrator because it creates a Windows service.

Download and run the installer from an elevated PowerShell window:

```powershell
Invoke-WebRequest `
  -Uri https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install-windows.ps1 `
  -OutFile install-windows.ps1
.\install-windows.ps1 -Domain api.example.com -Token abc123
```

`Domain` also accepts a full URL (`-Url https://api.example.com`) and is
normalized to a bare host. The installer will:

- Download the latest `auditready-x86_64-pc-windows-msvc.zip` release.
- Install `auditready.exe` to `C:\Program Files\AuditReady`.
- Write configuration to `C:\ProgramData\AuditReady\appsettings.json`.
- Create and start the `AuditReady` Windows service.

To install a specific release:

```powershell
.\install-windows.ps1 -Domain api.example.com -Token abc123 -Version v1.2.3
```

### macOS

DNS traffic capture on macOS uses `tcpdump` and requires the agent to run as
root. The easiest way is to install it as a LaunchDaemon:

```bash
wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install-macos.sh
chmod +x install-macos.sh
sudo ./install-macos.sh
```

Or pipe it directly:

```bash
curl -fsSL https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install-macos.sh | sudo bash
```

The installer will:

- Detect your architecture (`x86_64` or `arm64`) and download the latest
  release binary.
- Install `auditready` and the `auditready-restart` helper to `/usr/local/bin`.
- Prompt for the backend domain and agent token, and write the configuration
  to `/etc/auditready/appsettings.json` (mode `600`).
- Create, load, and start the `com.auditready.agent` LaunchDaemon as root.

For non-interactive / automated installs, pass the values as environment
variables:

```bash
sudo DOMAIN=api.example.com TOKEN=abc123 ./install-macos.sh
```

Verify the service after installation:

```bash
sudo launchctl list com.auditready.agent
sudo tail -f /etc/auditready/auditready.log
```

Restart the agent:

```bash
sudo auditready-restart
```

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

# Update to the latest release (keeps your config, restarts the service)
sudo auditready-update
# or fetch the script directly:
wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update.sh
chmod +x update.sh
sudo ./update.sh            # or: sudo VERSION=<tag> ./update.sh

# Update the agent token
wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update-token.sh
chmod +x update-token.sh
sudo ./update-token.sh            # prompts for the new token
sudo ./update-token.sh abc123     # or pass it directly
```

## Managing the agent (Windows)

The helper scripts are installed next to the binary at
`C:\Program Files\AuditReady`. Run them from an elevated PowerShell window.

```powershell
# Restart
& "C:\Program Files\AuditReady\restart-windows.ps1"

# Update to the latest release (keeps your config, restarts the service)
& "C:\Program Files\AuditReady\update-windows.ps1"
# or fetch the script directly:
Invoke-WebRequest `
  -Uri https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update-windows.ps1 `
  -OutFile update-windows.ps1
.\update-windows.ps1            # or: .\update-windows.ps1 -Version v1.2.3

# Update token (prompts interactively)
& "C:\Program Files\AuditReady\update-token-windows.ps1"

# Update token directly
& "C:\Program Files\AuditReady\update-token-windows.ps1" abc123
```

## Uninstall (Linux)

```bash
sudo systemctl disable --now auditready.service
sudo rm /etc/systemd/system/auditready.service
sudo systemctl daemon-reload
sudo rm -f /usr/local/bin/auditready /usr/local/bin/auditready-restart
sudo rm -rf /etc/auditready
```

## Uninstall (Windows)

Run from an elevated PowerShell window:

```powershell
Stop-Service -Name AuditReady -Force
& sc.exe delete AuditReady
Remove-Item -Path "C:\Program Files\AuditReady" -Recurse -Force
Remove-Item -Path "C:\ProgramData\AuditReady" -Recurse -Force
```

## License

See [LICENSE](LICENSE).
