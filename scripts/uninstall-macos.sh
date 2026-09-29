#!/usr/bin/env bash
set -euo pipefail

# Uninstall AuditReady from macOS.
# Removes the LaunchDaemon, LaunchAgent, binary, helper scripts, and config.
#
# Download and run:
#   wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/uninstall-macos.sh
#   chmod +x uninstall-macos.sh
#   sudo ./uninstall-macos.sh

INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="/etc/auditready"
PLIST_LABEL="com.auditready.agent"
PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"
CLIENT_PLIST_LABEL="com.auditready.client"
CLIENT_PLIST_PATH="/Library/LaunchAgents/${CLIENT_PLIST_LABEL}.plist"

if [ "$EUID" -ne 0 ]; then
    echo "This uninstaller must be run as root (try with sudo)." >&2
    exit 1
fi

# Stop and remove the root LaunchDaemon.
if [ -f "$PLIST_PATH" ]; then
    if launchctl list "$PLIST_LABEL" >/dev/null 2>&1; then
        launchctl unload "$PLIST_PATH" >/dev/null 2>&1 || true
    fi
    rm -f "$PLIST_PATH"
    echo "Removed ${PLIST_PATH}"
fi

# Stop and remove the per-user LaunchAgent.
if [ -f "$CLIENT_PLIST_PATH" ]; then
    CONSOLE_USER=$(stat -f '%Su' /dev/console 2>/dev/null || true)
    if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ]; then
        CONSOLE_UID=$(id -u "$CONSOLE_USER")
        launchctl bootout "gui/${CONSOLE_UID}" "$CLIENT_PLIST_PATH" >/dev/null 2>&1 || true
    fi
    rm -f "$CLIENT_PLIST_PATH"
    echo "Removed ${CLIENT_PLIST_PATH}"
fi

# Remove binary and helper scripts.
rm -f "${INSTALL_DIR}/auditready"
rm -f "${INSTALL_DIR}/auditready-restart"
rm -f "${INSTALL_DIR}/auditready-update"
echo "Removed binaries from ${INSTALL_DIR}"

# Remove config and logs.
rm -rf "$CONFIG_DIR"
echo "Removed ${CONFIG_DIR}"

echo ""
echo "AuditReady has been uninstalled."
