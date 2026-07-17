#!/usr/bin/env bash
set -euo pipefail

# Update an existing AuditReady installation to the latest (or a given) release.
# Keeps the existing configuration and systemd unit; only replaces the binary
# and helper scripts.
#
# Download first, then run:
#   wget -q https://raw.githubusercontent.com/tutu-learn/AuditReady/main/scripts/update.sh
#   chmod +x update.sh
#   sudo ./update.sh
#
# Pin a specific version:
#   sudo VERSION=nightly-2026-07-17-075306 ./update.sh

REPO="tutu-learn/AuditReady"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
SERVICE_NAME="auditready.service"

# Updating system files requires root.
if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (try with sudo)." >&2
    exit 1
fi

if [ ! -f "${INSTALL_DIR}/auditready" ]; then
    echo "No existing installation at ${INSTALL_DIR}/auditready." >&2
    echo "Use install.sh for a fresh install." >&2
    exit 1
fi

# Detect architecture.
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        TARGET="x86_64-unknown-linux-musl"
        ;;
    aarch64 | arm64)
        TARGET="aarch64-unknown-linux-musl"
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

echo "Updating AuditReady to ${VERSION} for ${TARGET}..."

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$URL" -o "$TMP_DIR/$ASSET"
tar xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR"

# Stop the service before replacing the binary; a running executable cannot
# be overwritten (ETXTBSY).
UNIT_EXISTS=0
if systemctl list-unit-files "$SERVICE_NAME" 2>/dev/null | grep -q "$SERVICE_NAME"; then
    UNIT_EXISTS=1
    systemctl stop "$SERVICE_NAME"
fi

install -m 755 "$TMP_DIR/auditready/auditready" "$INSTALL_DIR/auditready"
echo "Updated ${INSTALL_DIR}/auditready"

# Update helper scripts if present in the release archive.
if [ -f "$TMP_DIR/auditready/restart.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/restart.sh" "$INSTALL_DIR/auditready-restart"
    echo "Updated ${INSTALL_DIR}/auditready-restart"
fi
if [ -f "$TMP_DIR/auditready/update.sh" ]; then
    install -m 755 "$TMP_DIR/auditready/update.sh" "$INSTALL_DIR/auditready-update"
    echo "Updated ${INSTALL_DIR}/auditready-update"
fi

if [ "$UNIT_EXISTS" = "1" ]; then
    if systemctl start "$SERVICE_NAME"; then
        echo ""
        echo "AuditReady ${VERSION} is installed and running."
        echo "  Status: systemctl status auditready"
        echo "  Logs:   journalctl -u auditready -f"
    else
        echo ""
        echo "Updated, but the service failed to start. Check the logs:" >&2
        echo "  journalctl -u auditready -n 50" >&2
        exit 1
    fi
else
    echo "No ${SERVICE_NAME} unit found; binary updated, start the agent manually."
fi
