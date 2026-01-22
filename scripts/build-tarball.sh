#!/bin/bash
# Build a simple tarball for Slackware and other Linux distributions
# Requires: system webkit2gtk, libayatana-appindicator (or libappindicator)

set -e

cd "$(dirname "$0")/.."

# Get version from Cargo.toml
VERSION=$(grep '^version' src-tauri/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
ARCH=$(uname -m)
NAME="tiddlydesktop-rs"
TARBALL_NAME="${NAME}-${VERSION}-linux-${ARCH}"
BUILD_DIR="target/tarball/${TARBALL_NAME}"

echo "Building ${NAME} v${VERSION} for ${ARCH}..."

# Build the release binary (skip if already built, e.g., in CI)
if [ ! -f "src-tauri/target/release/${NAME}" ]; then
    echo "Compiling release binary..."
    cd src-tauri
    cargo build --release
    cd ..
else
    echo "Release binary already exists, skipping build..."
fi

# Create tarball directory structure
# Tauri looks for resources in ../lib/<app-name>/ relative to binary
echo "Creating tarball structure..."
rm -rf "target/tarball"
mkdir -p "${BUILD_DIR}/bin"
mkdir -p "${BUILD_DIR}/lib/${NAME}"
mkdir -p "${BUILD_DIR}/share/applications"
mkdir -p "${BUILD_DIR}/share/icons/hicolor/32x32/apps"
mkdir -p "${BUILD_DIR}/share/icons/hicolor/128x128/apps"
mkdir -p "${BUILD_DIR}/share/icons/hicolor/256x256/apps"

# Copy binary
cp "src-tauri/target/release/${NAME}" "${BUILD_DIR}/bin/"

# Create portable marker (enables local data storage when running directly from tarball)
# This file is NOT copied by install.sh, so installed versions use standard app data dirs
touch "${BUILD_DIR}/bin/portable"

# Copy resources to lib/<app-name>/ (where Tauri expects them)
cp -r src-tauri/resources/tiddlywiki "${BUILD_DIR}/lib/${NAME}/"
cp src/index.html "${BUILD_DIR}/lib/${NAME}/"

# Copy icons
cp src-tauri/icons/32x32.png "${BUILD_DIR}/share/icons/hicolor/32x32/apps/${NAME}.png"
cp src-tauri/icons/128x128.png "${BUILD_DIR}/share/icons/hicolor/128x128/apps/${NAME}.png"
cp src-tauri/icons/128x128@2x.png "${BUILD_DIR}/share/icons/hicolor/256x256/apps/${NAME}.png"

# Create .desktop file
cat > "${BUILD_DIR}/share/applications/${NAME}.desktop" << EOF
[Desktop Entry]
Name=TiddlyDesktop
Comment=TiddlyWiki desktop application
Exec=${NAME}
Icon=${NAME}
Terminal=false
Type=Application
Categories=Office;Utility;
MimeType=text/html;
EOF

# Create install script
cat > "${BUILD_DIR}/install.sh" << 'EOF'
#!/bin/bash
# Simple installer for TiddlyDesktop
# Run as root or with sudo

set -e

PREFIX="${1:-/usr/local}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Installing TiddlyDesktop to ${PREFIX}..."

# Install binary
install -Dm755 "${SCRIPT_DIR}/bin/tiddlydesktop-rs" "${PREFIX}/bin/tiddlydesktop-rs"

# Install resources (Tauri looks in ../lib/<app-name>/ relative to binary)
mkdir -p "${PREFIX}/lib/tiddlydesktop-rs"
cp -r "${SCRIPT_DIR}/lib/tiddlydesktop-rs/"* "${PREFIX}/lib/tiddlydesktop-rs/"

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
EOF
chmod +x "${BUILD_DIR}/install.sh"

# Create uninstall script
cat > "${BUILD_DIR}/uninstall.sh" << 'EOF'
#!/bin/bash
# Uninstaller for TiddlyDesktop
# Run as root or with sudo

set -e

PREFIX="${1:-/usr/local}"

echo "Uninstalling TiddlyDesktop from ${PREFIX}..."

rm -f "${PREFIX}/bin/tiddlydesktop-rs"
rm -rf "${PREFIX}/lib/tiddlydesktop-rs"
rm -f "${PREFIX}/share/applications/tiddlydesktop-rs.desktop"
for size in 32x32 128x128 256x256; do
    rm -f "${PREFIX}/share/icons/hicolor/${size}/apps/tiddlydesktop-rs.png"
done

echo "Uninstallation complete!"
EOF
chmod +x "${BUILD_DIR}/uninstall.sh"

# Create README
cat > "${BUILD_DIR}/README.txt" << EOF
TiddlyDesktop v${VERSION}
========================

A desktop application for TiddlyWiki.

REQUIREMENTS
------------
- webkit2gtk (webkit2gtk-4.1 or webkit2gtk-4.0)
- libayatana-appindicator3 (or libappindicator-gtk3)
- GTK 3

On Slackware, install:
  sbopkg -i webkit2gtk libappindicator

On other distros, the package names may vary.

INSTALLATION
------------
Option 1: System-wide (as root)
  ./install.sh

Option 2: Custom prefix
  ./install.sh /opt/tiddlydesktop

Option 3: Run directly without installing (portable mode)
  ./bin/tiddlydesktop-rs

  In portable mode, all data (settings, wiki sessions) is stored
  locally in the tarball directory instead of ~/.local/share/.

UNINSTALLATION
--------------
  ./uninstall.sh

or with custom prefix:
  ./uninstall.sh /opt/tiddlydesktop

EOF

# Create the tarball
echo "Creating tarball..."
cd target/tarball
tar -czvf "${TARBALL_NAME}.tar.gz" "${TARBALL_NAME}"
cd ../..

echo ""
echo "Done! Tarball created at:"
echo "  target/tarball/${TARBALL_NAME}.tar.gz"
echo ""
echo "Size: $(du -h "target/tarball/${TARBALL_NAME}.tar.gz" | cut -f1)"
