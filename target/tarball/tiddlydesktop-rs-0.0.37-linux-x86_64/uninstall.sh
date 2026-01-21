#!/bin/bash
# Uninstaller for TiddlyDesktop
# Run as root or with sudo

set -e

PREFIX="${1:-/usr/local}"

echo "Uninstalling TiddlyDesktop from ${PREFIX}..."

rm -f "${PREFIX}/bin/tiddlydesktop-rs"
rm -rf "${PREFIX}/share/tiddlydesktop-rs"
rm -f "${PREFIX}/share/applications/tiddlydesktop-rs.desktop"
for size in 32x32 128x128 256x256; do
    rm -f "${PREFIX}/share/icons/hicolor/${size}/apps/tiddlydesktop-rs.png"
done

echo "Uninstallation complete!"
