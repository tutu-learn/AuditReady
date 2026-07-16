#!/usr/bin/env bash
set -euo pipefail

# Update the agent token of an existing AuditReady installation.
#
# Download first, then run (recommended so the prompt works interactively):
#   wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update-token.sh
#   chmod +x update-token.sh
#   sudo ./update-token.sh
#
# Non-interactive:
#   sudo ./update-token.sh abc123
#   sudo TOKEN=abc123 ./update-token.sh

CONFIG_DIR="/etc/auditready"
CONFIG_FILE="${CONFIG_DIR}/appsettings.json"
SERVICE_NAME="auditready.service"

# Updating system config requires root.
if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (try with sudo)." >&2
    exit 1
fi

if [ ! -f "$CONFIG_FILE" ]; then
    echo "Config not found at ${CONFIG_FILE}. Is the agent installed?" >&2
    exit 1
fi

# Token from argument, environment, or interactive prompt.
TOKEN="${1:-${TOKEN:-}}"
if [ -z "$TOKEN" ]; then
    read -rsp "New agent token: " TOKEN < /dev/tty
    echo "" > /dev/tty
    if [ -z "$TOKEN" ]; then
        echo "A token is required." >&2
        exit 1
    fi
fi

cp "$CONFIG_FILE" "${CONFIG_FILE}.bak"

# Rewrite server.token, preserving all other settings.
if command -v jq > /dev/null 2>&1; then
    jq --arg token "$TOKEN" '.server.token = $token' "$CONFIG_FILE" > "${CONFIG_FILE}.tmp"
    mv "${CONFIG_FILE}.tmp" "$CONFIG_FILE"
elif command -v python3 > /dev/null 2>&1; then
    python3 - "$CONFIG_FILE" "$TOKEN" <<'PY'
import json, sys
path, token = sys.argv[1], sys.argv[2]
with open(path) as f:
    data = json.load(f)
data.setdefault("server", {})["token"] = token
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
PY
else
    echo "Neither jq nor python3 is available; cannot safely update ${CONFIG_FILE}." >&2
    echo "Install jq, or edit the \"token\" field in ${CONFIG_FILE} manually." >&2
    exit 1
fi
chmod 600 "$CONFIG_FILE"
echo "Updated token in ${CONFIG_FILE} (backup at ${CONFIG_FILE}.bak)"

# Restart the agent so it picks up the new token.
if command -v systemctl > /dev/null 2>&1 && systemctl list-unit-files "$SERVICE_NAME" | grep -q "$SERVICE_NAME"; then
    systemctl restart "$SERVICE_NAME"
    echo "Restarted ${SERVICE_NAME}."
    echo "  Status: systemctl status auditready"
    echo "  Logs:   journalctl -u auditready -f"
else
    echo "Restart the agent manually to apply the new token."
fi
