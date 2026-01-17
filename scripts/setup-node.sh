#!/bin/bash
# Download Node.js for bundling with TiddlyDesktopRS

set -e

NODE_VERSION="v20.11.0"
BINARIES_DIR="src-tauri/binaries"

mkdir -p "$BINARIES_DIR"

# Detect platform
case "$(uname -s)" in
    Linux*)
        PLATFORM="linux"
        case "$(uname -m)" in
            x86_64) ARCH="x64"; TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64) ARCH="arm64"; TARGET="aarch64-unknown-linux-gnu" ;;
            *) echo "Unsupported architecture"; exit 1 ;;
        esac
        EXT="tar.xz"
        ;;
    Darwin*)
        PLATFORM="darwin"
        case "$(uname -m)" in
            x86_64) ARCH="x64"; TARGET="x86_64-apple-darwin" ;;
            arm64) ARCH="arm64"; TARGET="aarch64-apple-darwin" ;;
            *) echo "Unsupported architecture"; exit 1 ;;
        esac
        EXT="tar.gz"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        PLATFORM="win"
        ARCH="x64"
        TARGET="x86_64-pc-windows-msvc"
        EXT="zip"
        ;;
    *)
        echo "Unsupported platform"
        exit 1
        ;;
esac

FILENAME="node-${NODE_VERSION}-${PLATFORM}-${ARCH}"
URL="https://nodejs.org/dist/${NODE_VERSION}/${FILENAME}.${EXT}"

echo "Downloading Node.js ${NODE_VERSION} for ${PLATFORM}-${ARCH}..."
echo "URL: $URL"

cd "$BINARIES_DIR"

# Download
if [ "$EXT" = "zip" ]; then
    curl -L -o node.zip "$URL"
    unzip -q node.zip
    mv "${FILENAME}/node.exe" "node-${TARGET}.exe"
    rm -rf node.zip "$FILENAME"
else
    curl -L -o node.tar.xz "$URL"
    tar -xf node.tar.xz
    mv "${FILENAME}/bin/node" "node-${TARGET}"
    rm -rf node.tar.xz "$FILENAME"
fi

echo "Node.js downloaded to ${BINARIES_DIR}/node-${TARGET}"
ls -la
