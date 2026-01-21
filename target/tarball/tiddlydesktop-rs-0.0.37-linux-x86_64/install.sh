#!/bin/bash
# Simple installer for TiddlyDesktop
# Run as root or with sudo

set -e

PREFIX="${1:-/usr/local}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Installing TiddlyDesktop to ${PREFIX}..."

# Install binary
install -Dm755 "${SCRIPT_DIR}/bin/tiddlydesktop-rs" "${PREFIX}/bin/tiddlydesktop-rs"

# Install resources
mkdir -p "${PREFIX}/share/tiddlydesktop-rs"
cp -r "${SCRIPT_DIR}/resources/"* "${PREFIX}/share/tiddlydesktop-rs/"

# Install icons
for size in 32x32 128x128 256x256; do
    install -Dm644 "${SCRIPT_DIR}/share/icons/hicolor/${size}/apps/tiddlydesktop-rs.png" \
        "${PREFIX}/share/icons/hicolor/${size}/apps/tiddlydesktop-rs.png"
done

# Install desktop file
install -Dm644 "${SCRIPT_DIR}/share/applications/tiddlydesktop-rs.desktop" \
    "${PREFIX}/share/applications/tiddlydesktop-rs.desktop"

# Update icon cache if available
if command -v gtk-update-icon-cache &> /dev/null; then
    gtk-update-icon-cache -f "${PREFIX}/share/icons/hicolor" 2>/dev/null || true
fi

echo "Installation complete!"
echo ""
echo "You can now run: tiddlydesktop-rs"
