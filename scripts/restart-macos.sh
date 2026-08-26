#!/usr/bin/env bash
set -euo pipefail

# Restart the AuditReady agent LaunchDaemon on macOS.
#
# Usage:
#   sudo auditready-restart
#   auditready-restart    (if root)

PLIST_LABEL="com.auditready.agent"
PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"

# Controlling system LaunchDaemons requires root.
if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (try with sudo)." >&2
    exit 1
fi

if [ ! -f "$PLIST_PATH" ]; then
    echo "LaunchDaemon not found at ${PLIST_PATH}. Is the agent installed?" >&2
    exit 1
fi

if launchctl list "$PLIST_LABEL" >/dev/null 2>&1; then
    launchctl stop "$PLIST_LABEL" >/dev/null 2>&1 || true
    launchctl unload "$PLIST_PATH" >/dev/null 2>&1 || true
fi

launchctl load -w "$PLIST_PATH"
launchctl start "$PLIST_LABEL"

echo "AuditReady restarted successfully."
echo "  Status: sudo launchctl list ${PLIST_LABEL}"
echo "  Logs:   sudo tail -f /etc/auditready/auditready.log"

# Also restart the per-user client-mode LaunchAgent when it is installed.
CLIENT_PLIST_LABEL="com.auditready.client"
CLIENT_PLIST_PATH="/Library/LaunchAgents/${CLIENT_PLIST_LABEL}.plist"
if [ -f "$CLIENT_PLIST_PATH" ]; then
    CONSOLE_USER=$(stat -f '%Su' /dev/console 2>/dev/null || true)
    if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ]; then
        CONSOLE_UID=$(id -u "$CONSOLE_USER")
        launchctl kickstart -k "gui/${CONSOLE_UID}/${CLIENT_PLIST_LABEL}" 2>/dev/null || true
        echo "AuditReady client agent restarted for user ${CONSOLE_USER}."
    else
        echo "Client agent installed but no user is logged in; it will start at next login."
    fi
fi
