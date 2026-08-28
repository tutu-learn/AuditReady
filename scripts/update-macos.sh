#!/usr/bin/env bash
set -euo pipefail

# Update an existing AuditReady installation on macOS to the latest (or a
# given) release. Keeps the existing configuration and LaunchDaemon; only
# replaces the binary and helper scripts.
#
# Download first, then run:
#   wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update-macos.sh
#   chmod +x update-macos.sh
#   sudo ./update-macos.sh
#
# Pin a specific version:
#   sudo VERSION=nightly-2026-08-28-120000 ./update-macos.sh

REPO="tutu-learn/AuditReady"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
PLIST_LABEL="com.auditready.agent"
PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"
CLIENT_PLIST_LABEL="com.auditready.client"
CLIENT_PLIST_PATH="/Library/LaunchAgents/${CLIENT_PLIST_LABEL}.plist"

# Updating system files requires root.
if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (try with sudo)." >&2
    exit 1
fi

if [ ! -f "${INSTALL_DIR}/auditready" ]; then
    echo "No existing installation at ${INSTALL_DIR}/auditready." >&2
    echo "Use install-macos.sh for a fresh install." >&2
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

echo "Updating AuditReady to ${VERSION} for ${TARGET}..."

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$URL" -o "$TMP_DIR/$ASSET"
unzip -q "$TMP_DIR/$ASSET" -d "$TMP_DIR"

# Unload the daemon before replacing the binary.
DAEMON_LOADED=0
if launchctl list "$PLIST_LABEL" >/dev/null 2>&1; then
    DAEMON_LOADED=1
    launchctl unload "$PLIST_PATH" >/dev/null 2>&1 || true
fi

install -m 755 "$TMP_DIR/auditready/auditready" "$INSTALL_DIR/auditready"
echo "Updated ${INSTALL_DIR}/auditready"

# Update helper scripts if present in the release archive.
if [ -f "$TMP_DIR/auditready/restart-macos.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/restart-macos.sh" "$INSTALL_DIR/auditready-restart"
    echo "Updated ${INSTALL_DIR}/auditready-restart"
fi
if [ -f "$TMP_DIR/auditready/update-macos.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/update-macos.sh" "$INSTALL_DIR/auditready-update"
    echo "Updated ${INSTALL_DIR}/auditready-update"
fi

# Enforce a 5-minute client reporting interval. Updating the binary alone
# never touches the config, so an install with a slower interval would keep
# it forever. Rewrites client.report_interval_seconds, preserving all other
# settings.
CONFIG_FILE="/etc/auditready/appsettings.json"
if [ -f "$CONFIG_FILE" ]; then
    if command -v jq > /dev/null 2>&1; then
        jq '.client.report_interval_seconds = 300' "$CONFIG_FILE" > "${CONFIG_FILE}.tmp"
        mv "${CONFIG_FILE}.tmp" "$CONFIG_FILE"
    elif command -v python3 > /dev/null 2>&1; then
        # macOS ships python3 via the CLT; fall back to it when jq is absent.
        python3 - "$CONFIG_FILE" <<'PY'
import json, sys
path = sys.argv[1]
with open(path) as f:
    data = json.load(f)
data.setdefault("client", {})["report_interval_seconds"] = 300
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
PY
    else
        # Non-fatal: the binary is already swapped; don't strand the daemon.
        echo "Neither jq nor python3 available; skipping client interval update." >&2
        echo "Set \"client\": { \"report_interval_seconds\": 300 } in ${CONFIG_FILE} manually." >&2
    fi
    chmod 600 "$CONFIG_FILE"
    echo "Set client.report_interval_seconds = 300 (5 minutes) in ${CONFIG_FILE}"
fi

if [ "$DAEMON_LOADED" = "1" ]; then
    if launchctl load -w "$PLIST_PATH"; then
        echo ""
        echo "AuditReady ${VERSION} is installed and running."
        echo "  Status:  sudo launchctl list ${PLIST_LABEL}"
        echo "  Logs:    sudo tail -f /etc/auditready/auditready.log"
        echo "  Restart: sudo auditready-restart"
    else
        echo ""
        echo "Updated, but the daemon failed to load. Check the logs:" >&2
        echo "  sudo tail -n 50 /etc/auditready/auditready.log" >&2
        exit 1
    fi
else
    echo "No ${PLIST_LABEL} daemon loaded; binary updated, start the agent manually."
fi

# The client LaunchAgent shares the same binary; kick it so it picks up the
# new version (KeepAlive restarts it, but only after it exits on its own).
if [ -f "$CLIENT_PLIST_PATH" ]; then
    CONSOLE_USER=$(stat -f '%Su' /dev/console 2>/dev/null || true)
    if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ]; then
        CONSOLE_UID=$(id -u "$CONSOLE_USER")
        launchctl kickstart -k "gui/${CONSOLE_UID}/${CLIENT_PLIST_LABEL}" 2>/dev/null || true
        echo "Client agent restarted for user ${CONSOLE_USER}."
    fi
fi
