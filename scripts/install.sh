#!/usr/bin/env bash
set -euo pipefail

# Install AuditReady agent as a systemd service on Linux.
#
# Download first, then run (recommended so prompts work interactively):
#   wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install.sh
#   chmod +x install.sh
#   sudo ./install.sh
#
# Or pipe directly (also supported):
#   curl -fsSL https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install.sh | sudo bash
#   wget -qO- https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install.sh | sudo bash
#
# Non-interactive / automated installs:
#   sudo DOMAIN=api.example.com TOKEN=abc123 ./install.sh

REPO="tutu-learn/AuditReady"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="/etc/auditready"
SERVICE_USER="${SERVICE_USER:-root}"

# Ensure we can install system files.
if [ "$EUID" -ne 0 ]; then
    echo "This installer must be run as root (try with sudo)." >&2
    exit 1
fi

# Detect architecture.
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        TARGET="x86_64-unknown-linux-gnu"
        ;;
    aarch64 | arm64)
        TARGET="aarch64-unknown-linux-gnu"
        ;;
    *)
        echo "Unsupported architecture: $ARCH" >&2
        exit 1
        ;;
esac

# Resolve version.
if [ "$VERSION" = "latest" ]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$VERSION" ]; then
        echo "Failed to determine latest version" >&2
        exit 1
    fi
fi

ASSET="auditready-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

echo "Installing AuditReady ${VERSION} for ${TARGET}..."

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$URL" -o "$TMP_DIR/$ASSET"
tar xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR"

# Install binary.
install -m 755 "$TMP_DIR/auditready/auditready" "$INSTALL_DIR/auditready"
echo "Installed auditready to ${INSTALL_DIR}/auditready"

# Install helper scripts if present in the release archive.
if [ -f "$TMP_DIR/auditready/restart.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/restart.sh" "$INSTALL_DIR/auditready-restart"
    echo "Installed auditready-restart to ${INSTALL_DIR}/auditready-restart"
fi

# Prepare config directory.
mkdir -p "$CONFIG_DIR"

# Interactive configuration.
echo ""
echo "Configure the agent (press Enter to keep suggested value):"
echo ""

if [ -z "${DOMAIN:-}" ]; then
    read -rp "Backend domain or URL (e.g. api.example.com or localhost:8000): " DOMAIN < /dev/tty
    if [ -z "$DOMAIN" ]; then
        echo "A backend domain is required." >&2
        exit 1
    fi
fi

# Strip scheme if the user pasted a full URL; the agent builds ws/wss from the domain.
DOMAIN=$(echo "$DOMAIN" | sed -E 's|^https?://||' | sed -E 's|/$||')

if [ -z "${TOKEN:-}" ]; then
    read -rsp "Agent token: " TOKEN < /dev/tty
    echo "" > /dev/tty
    if [ -z "$TOKEN" ]; then
        echo "An agent token is required." >&2
        exit 1
    fi
fi

# Determine the home directory for the service user so the remote shell starts there.
if [ "$SERVICE_USER" = "root" ]; then
    SHELL_HOME="/root"
else
    SHELL_HOME=$(getent passwd "$SERVICE_USER" | cut -d: -f6)
    SHELL_HOME="${SHELL_HOME:-/home/$SERVICE_USER}"
fi

cat > "$CONFIG_DIR/appsettings.json" <<EOF
{
  "server": {
    "domain": "${DOMAIN}",
    "token": "${TOKEN}",
    "interval_seconds": 10,
    "tunnel_enabled": true,
    "tunnel_shell": null,
    "tunnel_cwd": "${SHELL_HOME}"
  }
}
EOF
chmod 600 "$CONFIG_DIR/appsettings.json"
echo "Wrote configuration to ${CONFIG_DIR}/appsettings.json"

# Create systemd service.
SERVICE_FILE="/etc/systemd/system/auditready.service"
cat > "$SERVICE_FILE" <<EOF
[Unit]
Description=AuditReady Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_USER}
WorkingDirectory=${CONFIG_DIR}
ExecStart=${INSTALL_DIR}/auditready
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
chmod 644 "$SERVICE_FILE"
echo "Created systemd service at ${SERVICE_FILE}"

# Reload, enable and start.
systemctl daemon-reload
systemctl enable auditready.service
if systemctl start auditready.service; then
    echo ""
    echo "AuditReady is installed and running."
    echo "  Status:  systemctl status auditready"
    echo "  Logs:    journalctl -u auditready -f"
    echo "  Restart: auditready-restart"
else
    echo ""
    echo "AuditReady is installed but failed to start. Check the logs:"
    echo "  journalctl -u auditready -n 50"
    exit 1
fi
