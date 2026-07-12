#!/usr/bin/env bash
set -euo pipefail

# Install AuditReady agent from a GitHub release.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/YOUR_ORG/YOUR_REPO/main/scripts/install.sh | bash
# Or with a specific version:
#   VERSION=v1.2.0 ./install.sh

REPO="YOUR_ORG/YOUR_REPO"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# Detect architecture.
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        TARGET="x86_64-unknown-linux-gnu"
        ;;
    aarch64 | arm64)
        TARGET="aarch64-unknown-linux-gnu"
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

echo "Installing AuditReady ${VERSION} for ${TARGET}..."

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$URL" -o "$TMP_DIR/$ASSET"
tar xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR"

# Install binary.
if [ -w "$INSTALL_DIR" ]; then
    install -m 755 "$TMP_DIR/auditready/auditready" "$INSTALL_DIR/auditready"
else
    echo "Need sudo to install to $INSTALL_DIR"
    sudo install -m 755 "$TMP_DIR/auditready/auditready" "$INSTALL_DIR/auditready"
fi

echo "Installed auditready to ${INSTALL_DIR}/auditready"

# Copy example config if none exists.
if [ ! -f "appsettings.json" ]; then
    cp "$TMP_DIR/auditready/appsettings.example.json" ./appsettings.json
    echo "Created appsettings.json in the current directory. Edit it before running the agent."
else
    echo "appsettings.json already exists in the current directory."
fi

echo ""
echo "Next steps:"
echo "  1. Edit appsettings.json with your server domain and token."
echo "  2. Run: auditready"
