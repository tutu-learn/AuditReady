#!/usr/bin/env bash
set -euo pipefail

# Grant the per-user AuditReady client agent permission to capture DNS
# traffic. tcpdump needs read/write access to the /dev/bpf* devices, which
# are root:wheel 0600 by default. This is the Wireshark "ChmodBPF" approach:
#
#   1. a dedicated `access_bpf` group containing the user,
#   2. a LaunchDaemon that chgrps/chmods /dev/bpf* at every boot
#      (/dev is rebuilt on reboot, so a one-off chmod does not survive).
#
# Usage:
#   sudo ./fix-dns-capture-macos.sh            # grants the logged-in console user
#   sudo ./fix-dns-capture-macos.sh someuser   # grants a specific user
#
# Security note: any process running as a member of access_bpf can sniff ALL
# network traffic on this machine. That is exactly what the monitoring agent
# is for, but only add users that should have that capability.

GROUP="access_bpf"
PLIST_LABEL="com.auditready.chmodbpf"
PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"
HELPER="/usr/local/bin/auditready-chmodbpf"

if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (try with sudo)." >&2
    exit 1
fi

# User to grant: explicit argument, else the currently logged-in console user.
TARGET_USER="${1:-$(stat -f '%Su' /dev/console 2>/dev/null || true)}"
if [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = "root" ]; then
    echo "No console user found; pass the user explicitly:" >&2
    echo "  sudo $0 <username>" >&2
    exit 1
fi

# 1. Create the access_bpf group (first free GID from 400 up) if missing.
if ! dscl . -read "/Groups/${GROUP}" >/dev/null 2>&1; then
    GID=400
    while dscl . -list /Groups PrimaryGroupID | awk '{print $2}' | grep -qx "$GID"; do
        GID=$((GID + 1))
    done
    dscl . -create "/Groups/${GROUP}"
    dscl . -create "/Groups/${GROUP}" PrimaryGroupID "$GID"
    dscl . -create "/Groups/${GROUP}" RealName "BPF device access"
    echo "Created group ${GROUP} (gid ${GID})."
fi

# 2. Add the user to the group.
if dscl . -read "/Groups/${GROUP}" GroupMembership 2>/dev/null | grep -qw "$TARGET_USER"; then
    echo "${TARGET_USER} is already in ${GROUP}."
else
    dscl . -append "/Groups/${GROUP}" GroupMembership "$TARGET_USER"
    echo "Added ${TARGET_USER} to ${GROUP}."
fi

# 3. Helper that fixes the BPF device permissions.
cat > "$HELPER" <<EOF
#!/bin/bash
chgrp ${GROUP} /dev/bpf*
chmod g+rw /dev/bpf*
EOF
chmod 755 "$HELPER"

# 4. LaunchDaemon to run the helper at every boot.
cat > "$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${HELPER}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
EOF
chmod 644 "$PLIST_PATH"

# 5. Apply the permissions now and load the daemon for future boots.
"$HELPER"
if launchctl list "$PLIST_LABEL" >/dev/null 2>&1; then
    launchctl unload "$PLIST_PATH" 2>/dev/null || true
fi
launchctl load -w "$PLIST_PATH"

# 6. Restart the client agent so the new group membership and device
#    permissions take effect. Group changes only apply to newly spawned
#    processes; if the warning persists, log out and back in once.
launchctl kickstart -k "gui/$(id -u "$TARGET_USER")/com.auditready.client" 2>/dev/null || true

echo ""
echo "Done. ${TARGET_USER} can now capture DNS traffic."
echo "Verify: tail -f /etc/auditready/auditready-client.log"
echo "        (the 'tcpdump ... Operation not permitted' warning should be gone)"
