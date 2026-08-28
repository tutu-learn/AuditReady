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
# Additionally install the per-user client-mode agent (file-change and
# clipboard monitoring) as a LaunchAgent:
#   sudo DOMAIN=api.example.com TOKEN=abc123 MODE=client ./install-macos.sh
#
# Running as root is required for DNS traffic capture (tcpdump on port 53).

REPO="tutu-learn/AuditReady"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="/etc/auditready"
PLIST_LABEL="com.auditready.agent"
PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"
CLIENT_PLIST_LABEL="com.auditready.client"
CLIENT_PLIST_PATH="/Library/LaunchAgents/${CLIENT_PLIST_LABEL}.plist"

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

# Releases package macOS builds as .zip (see release.yml).
ASSET="auditready-${TARGET}.zip"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

echo "Installing AuditReady ${VERSION} for ${TARGET}..."

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$URL" -o "$TMP_DIR/$ASSET"
unzip -q "$TMP_DIR/$ASSET" -d "$TMP_DIR"

# Install binary.
install -m 755 "$TMP_DIR/auditready/auditready" "$INSTALL_DIR/auditready"
echo "Installed auditready to ${INSTALL_DIR}/auditready"

# Install helper scripts if present in the release archive.
# (update.sh is Linux-only — systemd and .tar.gz assets — so it is never
# bundled into or installed from the macOS zip.)
if [ -f "$TMP_DIR/auditready/restart-macos.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/restart-macos.sh" "$INSTALL_DIR/auditready-restart"
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

# Client mode runs as a second, per-user instance: clipboard and frontmost-app
# access require the user's GUI session, which the root LaunchDaemon cannot
# do. LaunchAgents run inside the GUI session of the logged-in user.
if [ "${MODE:-agent}" = "client" ]; then
    cat > "$CLIENT_PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${CLIENT_PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${INSTALL_DIR}/auditready</string>
        <string>--config</string>
        <string>${CONFIG_DIR}/appsettings.json</string>
        <string>--mode</string>
        <string>client</string>
    </array>
    <key>WorkingDirectory</key>
    <string>${CONFIG_DIR}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${CONFIG_DIR}/auditready-client.log</string>
    <key>StandardErrorPath</key>
    <string>${CONFIG_DIR}/auditready-client.log</string>
</dict>
</plist>
EOF
    chmod 644 "$CLIENT_PLIST_PATH"
    echo "Created LaunchAgent at ${CLIENT_PLIST_PATH}"

    # Load it into the currently logged-in user's GUI session, if any.
    CONSOLE_USER=$(stat -f '%Su' /dev/console 2>/dev/null || true)
    if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ]; then
        CONSOLE_UID=$(id -u "$CONSOLE_USER")
        launchctl bootstrap "gui/${CONSOLE_UID}" "$CLIENT_PLIST_PATH" 2>/dev/null || true
        echo "Started client agent for user ${CONSOLE_USER}"
    else
        echo "No user logged in; the client agent will start at next login."
    fi

    echo ""
    echo "NOTE: paste capture (Cmd+V) requires the Accessibility permission."
    echo "  Grant it to ${INSTALL_DIR}/auditready under"
    echo "  System Settings > Privacy & Security > Accessibility."
    echo "  Without it, only clipboard copy events are reported."
fi

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
