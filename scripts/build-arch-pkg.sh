#!/bin/bash
# Build Arch Linux package from the Tauri build output
# Run this after: cargo tauri build

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/src-tauri/target/release/bundle"

# Package info
PKGNAME="tiddlydesktop-rs"
PKGVER=$(grep '"version"' "$PROJECT_DIR/src-tauri/tauri.conf.json" | head -1 | sed 's/.*: *"\([^"]*\)".*/\1/')
PKGREL=1
ARCH="x86_64"

echo "Building Arch Linux package for $PKGNAME $PKGVER..."

# Find the built deb
DEB_FILE=$(find "$BUILD_DIR/deb" -name "*.deb" 2>/dev/null | head -1)

if [ -z "$DEB_FILE" ]; then
    echo "Error: No .deb file found in $BUILD_DIR/deb"
    echo "Run 'cargo tauri build' first"
    exit 1
fi

echo "Found deb: $DEB_FILE"

# Create temp directory for package construction
WORK_DIR=$(mktemp -d)
PKG_DIR="$WORK_DIR/pkg"
mkdir -p "$PKG_DIR"

echo "Extracting deb..."
# Extract deb (it's an ar archive containing data.tar.* and control.tar.*)
cd "$WORK_DIR"
ar x "$DEB_FILE"

# Extract data tarball
if [ -f data.tar.gz ]; then
    tar xzf data.tar.gz -C "$PKG_DIR"
elif [ -f data.tar.xz ]; then
    tar xJf data.tar.xz -C "$PKG_DIR"
elif [ -f data.tar.zst ]; then
    tar --zstd -xf data.tar.zst -C "$PKG_DIR"
else
    echo "Error: Could not find data tarball in deb"
    exit 1
fi

# Get installed size (in KB)
INSTALLED_SIZE=$(du -sk "$PKG_DIR" | cut -f1)

# Create .PKGINFO
echo "Creating .PKGINFO..."
cat > "$PKG_DIR/.PKGINFO" << EOF
pkgname = $PKGNAME
pkgbase = $PKGNAME
pkgver = $PKGVER-$PKGREL
pkgdesc = A desktop application for TiddlyWiki built with Tauri
url = https://github.com/BurningTreeC/tiddlydesktop-rs
builddate = $(date +%s)
packager = Unknown Packager
size = $((INSTALLED_SIZE * 1024))
arch = $ARCH
license = MIT
depend = webkit2gtk-4.1
depend = gtk3
depend = libayatana-appindicator
optdepend = nodejs: Required for wiki folder support
EOF

# Create .MTREE (file metadata)
echo "Creating .MTREE..."
cd "$PKG_DIR"
find . -type f -o -type l | sed 's|^\./||' | LANG=C sort | while read -r file; do
    if [ -L "$file" ]; then
        echo "$file type=link"
    elif [ -f "$file" ]; then
        MODE=$(stat -c %a "$file")
        SIZE=$(stat -c %s "$file")
        echo "$file mode=$MODE size=$SIZE"
    fi
done | gzip -c > .MTREE 2>/dev/null || true

# Create the package archive
OUTPUT_DIR="$BUILD_DIR/arch"
mkdir -p "$OUTPUT_DIR"
OUTPUT_FILE="$OUTPUT_DIR/$PKGNAME-$PKGVER-$PKGREL-$ARCH.pkg.tar.zst"

echo "Creating package archive..."
cd "$PKG_DIR"

# Create package with zstd compression
tar --zstd -cf "$OUTPUT_FILE" .PKGINFO .MTREE * 2>/dev/null || \
tar -I 'zstd -19' -cf "$OUTPUT_FILE" .PKGINFO .MTREE *

# Cleanup
rm -rf "$WORK_DIR"

echo ""
echo "✓ Arch Linux package created:"
echo "  $OUTPUT_FILE"
echo ""
echo "Install with: sudo pacman -U $OUTPUT_FILE"
