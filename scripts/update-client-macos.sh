#!/usr/bin/env bash
set -euo pipefail

# Update only the AuditReady client-mode LaunchAgent on macOS.
# This script does NOT touch the root LaunchDaemon; it only replaces the
# shared binary and restarts the per-user client agent.
#
# Download first, then run:
#   wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update-client-macos.sh
#   chmod +x update-client-macos.sh
#   sudo ./update-client-macos.sh
#
# Pin a specific version:
#   sudo VERSION=nightly-2026-08-28-120000 ./update-client-macos.sh

REPO="tutu-learn/AuditReady"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
CLIENT_PLIST_LABEL="com.auditready.client"
CLIENT_PLIST_PATH="/Library/LaunchAgents/${CLIENT_PLIST_LABEL}.plist"

# Updating the binary requires root.
if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (try with sudo)." >&2
    exit 1
fi

if [ ! -f "${INSTALL_DIR}/auditready" ]; then
    echo "No existing installation at ${INSTALL_DIR}/auditready." >&2
    echo "Use install-macos.sh with MODE=client for a fresh install." >&2
    exit 1
fi

if [ ! -f "$CLIENT_PLIST_PATH" ]; then
    echo "Client mode is not installed (${CLIENT_PLIST_PATH} missing)." >&2
    echo "Run install-macos.sh with MODE=client first." >&2
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

# Releases package macOS builds as .zip.
ASSET="auditready-${TARGET}.zip"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

echo "Updating AuditReady client to ${VERSION} for ${TARGET}..."

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$URL" -o "$TMP_DIR/$ASSET"
unzip -q "$TMP_DIR/$ASSET" -d "$TMP_DIR"

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
if [ -f "$TMP_DIR/auditready/update-client-macos.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/update-client-macos.sh" "$INSTALL_DIR/auditready-update-client"
    echo "Updated ${INSTALL_DIR}/auditready-update-client"
fi

# Enforce a 5-minute client reporting interval, preserving other settings.
CONFIG_FILE="/etc/auditready/appsettings.json"
if [ -f "$CONFIG_FILE" ]; then
    REWRITE_OK=0
    if command -v jq > /dev/null 2>&1; then
        if jq '.client.report_interval_seconds = 300' "$CONFIG_FILE" > "${CONFIG_FILE}.tmp" 2>/dev/null; then
            mv "${CONFIG_FILE}.tmp" "$CONFIG_FILE"
            REWRITE_OK=1
        else
            rm -f "${CONFIG_FILE}.tmp"
        fi
    elif command -v python3 > /dev/null 2>&1; then
        if python3 - "$CONFIG_FILE" <<'PY'
import json, sys
path = sys.argv[1]
with open(path) as f:
    data = json.load(f)
data.setdefault("client", {})["report_interval_seconds"] = 300
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
PY
        then
            REWRITE_OK=1
        fi
    fi
    if [ "$REWRITE_OK" = "1" ]; then
        echo "Set client.report_interval_seconds = 300 (5 minutes) in ${CONFIG_FILE}"
    else
        echo "Config rewrite failed; keeping ${CONFIG_FILE} as-is." >&2
        echo "Set \"client\": { \"report_interval_seconds\": 300 } manually if needed." >&2
    fi
    # The client-mode LaunchAgent runs as the logged-in user and must be able
    # to read this file. The rewrite replaces the file wholesale, so repair
    # permissions here or the client agent dies with EACCES.
    chgrp staff "$CONFIG_FILE"
    chmod 640 "$CONFIG_FILE"
fi

# The client runs as the logged-in user and must be able to write its log
# in the root-owned config dir.
touch /etc/auditready/auditready-client.log
chgrp staff /etc/auditready/auditready-client.log
chmod 664 /etc/auditready/auditready-client.log

CONSOLE_USER=$(stat -f '%Su' /dev/console 2>/dev/null || true)
if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ]; then
    CONSOLE_UID=$(id -u "$CONSOLE_USER")
    # kickstart only works on a loaded service; bootstrap it back if needed.
    launchctl kickstart -k "gui/${CONSOLE_UID}/${CLIENT_PLIST_LABEL}" 2>/dev/null \
        || launchctl bootstrap "gui/${CONSOLE_UID}" "$CLIENT_PLIST_PATH" 2>/dev/null \
        || true
    echo "Client agent updated and restarted for user ${CONSOLE_USER}."
    echo "Its tray icon should reappear in the menu bar within a few seconds."
else
    echo "Client agent binary updated, but no user is logged in; it will start at next login."
fi

echo ""
echo "Logs: tail -f /etc/auditready/auditready-client.log"
