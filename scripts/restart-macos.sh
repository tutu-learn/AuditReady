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
