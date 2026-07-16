#!/usr/bin/env bash
set -euo pipefail

# Restart the AuditReady agent systemd service.
#
# Usage:
#   sudo auditready-restart
#   auditready-restart    (if root)

SERVICE_NAME="auditready"

# Ensure we can control systemd units.
if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (try with sudo)." >&2
    exit 1
fi

if ! systemctl is-active --quiet "${SERVICE_NAME}.service" 2>/dev/null; then
    echo "Service ${SERVICE_NAME} is not currently running; starting it..."
fi

if systemctl restart "${SERVICE_NAME}.service"; then
    echo "AuditReady restarted successfully."
    echo "  Status: systemctl status ${SERVICE_NAME}"
    echo "  Logs:   journalctl -u ${SERVICE_NAME} -f"
else
    echo "Failed to restart AuditReady. Recent logs:" >&2
    journalctl -u "${SERVICE_NAME}" -n 50 --no-pager >&2 || true
    exit 1
fi
