#!/usr/bin/env bash
set -euo pipefail

# Install AuditReady agent as a macOS LaunchDaemon running as root.
#
# Download first, then run (recommended so prompts work interactively):
#   wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install-macos.sh
#   chmod +x install-macos.sh
#   sudo ./install-macos.sh
#
# Or pipe directly:
#   curl -fsSL https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/install-macos.sh | sudo bash
#
# Non-interactive / automated installs:
#   sudo DOMAIN=api.example.com TOKEN=abc123 ./install-macos.sh
#
# Running as root is required for DNS traffic capture (tcpdump on port 53).

REPO="tutu-learn/AuditReady"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="/etc/auditready"
PLIST_LABEL="com.auditready.agent"
PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"

# Ensure we can install system files.
if [ "$EUID" -ne 0 ]; then
    echo "This installer must be run as root (try with sudo)." >&2
    exit 1
fi

# Detect architecture.
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        TARGET="x86_64-apple-darwin"
        ;;
    arm64)
        TARGET="aarch64-apple-darwin"
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
if [ -f "$TMP_DIR/auditready/restart-macos.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/restart-macos.sh" "$INSTALL_DIR/auditready-restart"
    echo "Installed auditready-restart to ${INSTALL_DIR}/auditready-restart"
fi
if [ -f "$TMP_DIR/auditready/update.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/update.sh" "$INSTALL_DIR/auditready-update"
    echo "Installed auditready-update to ${INSTALL_DIR}/auditready-update"
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

ROOT_HOME="/var/root"

cat > "$CONFIG_DIR/appsettings.json" <<EOF
{
  "server": {
    "domain": "${DOMAIN}",
    "token": "${TOKEN}",
    "interval_seconds": 10,
    "tunnel_enabled": true,
    "tunnel_shell": null,
    "tunnel_cwd": "${ROOT_HOME}"
  }
}
EOF
chmod 600 "$CONFIG_DIR/appsettings.json"
echo "Wrote configuration to ${CONFIG_DIR}/appsettings.json"

# Create LaunchDaemon plist.
cat > "$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${INSTALL_DIR}/auditready</string>
    </array>
    <key>WorkingDirectory</key>
    <string>${CONFIG_DIR}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${CONFIG_DIR}/auditready.log</string>
    <key>StandardErrorPath</key>
    <string>${CONFIG_DIR}/auditready.log</string>
</dict>
</plist>
EOF
chmod 644 "$PLIST_PATH"
echo "Created LaunchDaemon at ${PLIST_PATH}"

# Load and start the service.
if launchctl list "$PLIST_LABEL" >/dev/null 2>&1; then
    launchctl unload "$PLIST_PATH" >/dev/null 2>&1 || true
fi
launchctl load -w "$PLIST_PATH"

if launchctl start "$PLIST_LABEL" 2>/dev/null; then
    echo ""
    echo "AuditReady is installed and running as root."
    echo "  Status:  sudo launchctl list ${PLIST_LABEL}"
    echo "  Logs:    sudo tail -f ${CONFIG_DIR}/auditready.log"
    echo "  Restart: sudo auditready-restart"
else
    echo ""
    echo "AuditReady is installed but failed to start. Check the logs:"
    echo "  sudo tail -n 50 ${CONFIG_DIR}/auditready.log" >&2
    exit 1
fi
