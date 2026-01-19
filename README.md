# TiddlyDesktop-RS

A modern, cross-platform desktop application for [TiddlyWiki](https://tiddlywiki.com/), built with [Tauri](https://tauri.app/) and Rust.

## Features

- **Single-file wikis**: Open and edit standalone TiddlyWiki HTML files with automatic saving and backups
- **Wiki folders**: Full Node.js-powered wiki folder support with real-time syncing
- **Create new wikis**: Initialize new single-file or folder wikis from any TiddlyWiki edition
- **Cross-platform**: Windows, macOS, and Linux support
- **Lightweight**: Small download size and low resource usage thanks to Tauri
- **Native experience**: System tray, native file dialogs, and platform-specific installers

## Download

Download the latest release for your platform from the [Releases page](../../releases).

| Platform | Download |
|----------|----------|
| Windows | `.msi` (installer) or `.exe` (NSIS installer) |
| macOS | `.dmg` (disk image) or `.app.zip` |
| Linux | `.deb` (Debian/Ubuntu), `.rpm` (Fedora/RHEL), or `.AppImage` |

## Installation

### Windows

1. Download the `.msi` or `.exe` installer
2. Run the installer
3. **Security warning**: Windows SmartScreen may show "Windows protected your PC"
   - Click **"More info"**
   - Click **"Run anyway"**

### macOS

1. Download the `.dmg` file
2. Open the disk image and drag the app to Applications
3. **Security warning**: macOS will show "app is damaged" or "unidentified developer"
   - **Option A**: Right-click the app → **Open** → **Open**
   - **Option B**: Run in Terminal: `xattr -cr /Applications/TiddlyDesktopRS.app`

### Linux

**Requirements**:
- **Node.js 18+**: Required for wiki folder support. Install via your package manager, [NodeSource](https://github.com/nodesource/distributions), or [nvm](https://github.com/nvm-sh/nvm). Single-file wikis work without Node.js.
- **libayatana-appindicator**: Required for system tray support.
  - Debian/Ubuntu: `sudo apt install libayatana-appindicator3-1`
  - Fedora: `sudo dnf install libayatana-appindicator-gtk3`

**Debian/Ubuntu (.deb)**:
```bash
sudo dpkg -i tiddlydesktop-rs_*.deb
```

**Fedora/RHEL (.rpm)**:
```bash
sudo rpm -i tiddlydesktop-rs-*.rpm
```

**AppImage**:
```bash
chmod +x TiddlyDesktopRS-*.AppImage
./TiddlyDesktopRS-*.AppImage
```

## Verifying Downloads

Each release includes a `CHECKSUMS-SHA256.txt` file containing SHA256 checksums for all downloads.

**Linux/macOS**:
```bash
# Download CHECKSUMS-SHA256.txt and the file you want to verify
sha256sum -c CHECKSUMS-SHA256.txt --ignore-missing
```

**Windows (PowerShell)**:
```powershell
# Get the checksum of your downloaded file
Get-FileHash .\TiddlyDesktopRS_x64-setup.exe -Algorithm SHA256

# Compare with the value in CHECKSUMS-SHA256.txt
```

## Building from Source

### Prerequisites

- [Node.js](https://nodejs.org/) 20+
- [Rust](https://rustup.rs/) (stable)
- Platform-specific dependencies (see [Tauri prerequisites](https://tauri.app/start/prerequisites/))

### Build Steps

```bash
# Clone the repository
git clone https://github.com/BurningTreeC/tiddlydesktop-rs.git
cd tiddlydesktop-rs

# Clone TiddlyWiki5 (required for building)
git clone https://github.com/TiddlyWiki/TiddlyWiki5.git ../TiddlyWiki5

# Copy plugins
cp -r TiddlyWiki5/plugins/tiddlywiki/tiddlydesktop-rs ../TiddlyWiki5/plugins/tiddlywiki/
cp -r TiddlyWiki5/editions/tiddlydesktop ../TiddlyWiki5/editions/

# Install dependencies
npm install

# Build TiddlyWiki
cd ../TiddlyWiki5
node tiddlywiki.js editions/tiddlydesktop --output ../tiddlydesktop-rs/src --render '$:/core/save/all' 'index.html' 'text/plain'
cd ../tiddlydesktop-rs

# Bundle TiddlyWiki for the app
mkdir -p src-tauri/resources
cp -r ../TiddlyWiki5 src-tauri/resources/tiddlywiki

# Download Node.js binary for wiki folder support (desktop only)
# See .github/workflows/release.yml for platform-specific instructions

# Build the application
npm run tauri build
```

## Usage

### Opening Wikis

- **Single-file wiki**: Click "Open Wiki File" or drag-and-drop an HTML file
- **Wiki folder**: Click "Open Wiki Folder" and select a folder containing `tiddlywiki.info`

### Creating New Wikis

1. Click "New Wiki File" or "New Wiki Folder"
2. Select an edition (e.g., "empty", "full")
3. Optionally select additional plugins
4. Choose the save location

### Wiki Folders vs Single Files

| Feature | Single File | Wiki Folder |
|---------|-------------|-------------|
| Portability | Single HTML file | Directory with multiple files |
| Saving | Manual save (Ctrl+S) with backups | Auto-save on every change |
| Performance | Can be slow with large wikis | Better for large wikis |
| Plugins | Embedded in file | External plugin folders |
| Node.js required | No | Yes |

## Known Limitations

### macOS: Orphaned Node.js Processes

When using wiki folders, TiddlyDesktop-RS spawns a Node.js server process. On **Linux** and **Windows**, if the app is forcefully terminated (e.g., via Task Manager or `kill -9`), these Node.js processes are automatically killed.

On **macOS**, due to OS limitations, forcefully killing the app may leave orphaned Node.js processes running. To clean them up manually:

```bash
# Find orphaned Node.js processes
ps aux | grep "node.*tiddlywiki"

# Kill them (replace PID with actual process ID)
kill PID
```

This only affects wiki folders, not single-file wikis. Normal app closure (quit via menu/tray) always cleans up properly on all platforms.

## Why the Security Warnings?

The application is not code-signed, which means your operating system can't verify the publisher. Code signing certificates cost $100-400+ per year, which isn't feasible for this free, open-source project.

**The app is safe to use** - you can:
- Review the source code in this repository
- Verify downloads using the SHA256 checksums
- Build from source yourself

## License

MIT License - see [LICENSE](LICENSE) for details.

## Acknowledgments

- [TiddlyWiki](https://tiddlywiki.com/) - The amazing non-linear personal web notebook
- [Tauri](https://tauri.app/) - Build smaller, faster, and more secure desktop applications
- [Original TiddlyDesktop](https://github.com/TiddlyWiki/TiddlyDesktop) - Inspiration for this project
