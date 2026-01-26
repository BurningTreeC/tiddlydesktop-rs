# TiddlyDesktop-RS

A modern, cross-platform desktop application for [TiddlyWiki](https://tiddlywiki.com/), built with [Tauri](https://tauri.app/) and Rust.

## Features

- **Single-file wikis**: Open and edit standalone TiddlyWiki HTML files with automatic saving and backups
- **Wiki folders**: Full Node.js-powered wiki folder support with real-time syncing
- **Create new wikis**: Initialize new single-file or folder wikis from any TiddlyWiki edition
- **Drag and drop**: Drop wiki files onto the app or landing page to open them
- **Cross-platform**: Windows, macOS, and Linux support
- **Lightweight**: Small download size (~15MB) and low resource usage thanks to Tauri
- **Native experience**: System tray, native file dialogs, and platform-specific installers

## Quick Start

1. Download the installer for your platform from the [Releases page](../../releases)
2. Install and launch TiddlyDesktopRS
3. Click **"Open Wiki File"** to open an existing TiddlyWiki HTML file, or
4. Click **"New Wiki File"** to create a new wiki

## Download

| Platform | Recommended | Alternative |
|----------|-------------|-------------|
| **Windows** | `.msi` installer | `.exe` (NSIS installer) |
| **macOS** | `.dmg` disk image | `.app.zip` |
| **Linux** | `.deb` / `.rpm` / `.pkg.tar.zst` | `.tar.gz` (portable) |

### Which Build Should I Choose?

**Standard builds** (recommended): Include a bundled Node.js binary for wiki folder support. Everything works out of the box.

**`PROVIDE_YOUR_OWN_NODEJS_` builds**: Smaller downloads without bundled Node.js. Use these if:
- You already have Node.js 18+ installed system-wide
- You only use single-file wikis (Node.js not required)
- You want smaller download size

**Note**: Linux builds never bundle Node.js - install it via your package manager if you need wiki folder support.

## Installation

### Windows

1. Download the `.msi` or `.exe` installer
2. Run the installer
3. **Security warning**: Windows SmartScreen may show "Windows protected your PC"
   - Click **"More info"** → **"Run anyway"**

### macOS

1. Download the `.dmg` file
2. Open the disk image and drag the app to Applications
3. **Security warning**: macOS may show "app is damaged" or "unidentified developer"
   - **Option A**: Right-click the app → **Open** → **Open**
   - **Option B**: Run in Terminal: `xattr -cr /Applications/TiddlyDesktopRS.app`

### Linux

**Install required system libraries first:**

| Distribution | Command |
|--------------|---------|
| Debian/Ubuntu | `sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0 libayatana-appindicator3-1` |
| Fedora | `sudo dnf install webkit2gtk4.1 gtk3 libayatana-appindicator-gtk3` |
| Arch | `sudo pacman -S webkit2gtk-4.1 gtk3 libayatana-appindicator` |

**For wiki folder support**, also install Node.js 18+ via your package manager, [NodeSource](https://github.com/nodesource/distributions), or [nvm](https://github.com/nvm-sh/nvm).

**Then install the package:**

```bash
# Debian/Ubuntu
sudo dpkg -i tiddlydesktop-rs_*.deb

# Fedora/RHEL
sudo rpm -i tiddlydesktop-rs-*.rpm

# Arch Linux
sudo pacman -U tiddlydesktop-rs-*.pkg.tar.zst

# Portable (any distro)
tar -xzf tiddlydesktop-rs-*.tar.gz
./tiddlydesktop-rs/tiddlydesktop-rs
```

## Usage

### Opening Wikis

- **Click** "Open Wiki File" or "Open Wiki Folder" from the landing page
- **Drag and drop** a `.html` wiki file onto the app window
- **Double-click** a `.html` file (if file associations are set up)

### Saving Wikis

| Wiki Type | How to Save |
|-----------|-------------|
| Single-file | Press **Ctrl+S** (or **Cmd+S** on macOS), or click the save button in TiddlyWiki |
| Wiki folder | Saves automatically on every change |

Single-file wikis create timestamped backups in the same directory when saved.

### Creating New Wikis

1. Click **"New Wiki File"** or **"New Wiki Folder"**
2. Select an edition (e.g., "empty", "full")
3. Optionally select additional plugins
4. Choose the save location

### Wiki Folders vs Single Files

| Feature | Single File | Wiki Folder |
|---------|-------------|-------------|
| Format | Single `.html` file | Directory with multiple files |
| Saving | Manual (Ctrl+S) with backups | Auto-save on every change |
| Performance | Can slow down with large wikis | Better for large wikis |
| Plugins | Embedded in file | External plugin folders |
| Node.js required | No | Yes |
| Portability | Easy to share/backup | Requires folder copy |

### Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| Ctrl+S / Cmd+S | Save wiki (single-file wikis) |
| Ctrl+F / Cmd+F | Find in page (wiki windows only, not landing page) |
| F3 / Shift+F3 | Find next / previous |
| Escape | Close find bar |

### Custom Editions

Add custom TiddlyWiki editions to the user editions directory:

| Platform | Path |
|----------|------|
| Linux | `~/.local/share/tiddlydesktop-rs/editions/` |
| macOS | `~/Library/Application Support/tiddlydesktop-rs/editions/` |
| Windows | `%APPDATA%\tiddlydesktop-rs\editions\` |

Each edition must be a directory containing a valid `tiddlywiki.info` file.

### Data Storage Locations

TiddlyDesktopRS stores settings and wiki metadata in:

| Platform | Path |
|----------|------|
| Linux | `~/.local/share/tiddlydesktop-rs/` |
| macOS | `~/Library/Application Support/tiddlydesktop-rs/` |
| Windows | `%APPDATA%\tiddlydesktop-rs\` |

This includes:
- `settings.json` - Application preferences
- `wikis.json` - Recently opened wikis and window positions
- `editions/` - Custom TiddlyWiki editions

## Running Shell Commands

TiddlyDesktopRS supports running shell commands from within your wikis via the **TiddlyDesktop-RS Commands** plugin.

### Installing the Plugin

1. Drag and drop the plugin folder from `TiddlyWiki5/plugins/tiddlywiki/tiddlydesktop-rs-commands/` into your wiki
2. Save and reload your wiki

### Basic Usage

```html
<$button>
  Open File Manager
  <$action-run-command $command="xdg-open" $args="."/>
</$button>
```

### Widget Attributes

| Attribute | Description | Default |
|-----------|-------------|---------|
| `$command` | Command or script path to run (required) | - |
| `$args` | Arguments (space-separated, supports quotes) | - |
| `$workingDir` | Working directory | Wiki directory |
| `$wait` | Wait for completion (`yes`/`no`) | `no` |
| `$confirm` | Show confirmation dialog (`yes`/`no`) | `yes` |
| `$outputTiddler` | Tiddler to store output (requires `$wait="yes"`) | - |
| `$outputField` | Field for stdout | `text` |
| `$exitCodeField` | Field for exit code | - |
| `$stderrField` | Field for stderr | - |

### Capturing Output

```html
<$button>
  Get Current Date
  <$action-run-command
    $command="date"
    $wait="yes"
    $outputTiddler="$:/temp/command-output"
    $exitCodeField="exit_code"
  />
</$button>

<$list filter="[[$:/temp/command-output]has[text]]">
  Output: {{$:/temp/command-output}}
</$list>
```

### Security Notes

- By default, a confirmation dialog appears before each command runs
- Use `$confirm="no"` only for trusted commands
- Commands run with your user permissions
- This widget only works in TiddlyDesktopRS (no effect in browsers)

## Troubleshooting

### Linux: Performance Issues

If you experience slow performance, laggy scrolling, or high CPU usage, try these environment variables:

```bash
# Often fixes issues on KDE Plasma
WEBKIT_DISABLE_DMABUF_RENDERER=1 ./tiddlydesktop-rs

# Alternative fix
WEBKIT_DISABLE_COMPOSITING_MODE=1 ./tiddlydesktop-rs

# Maximum compatibility (software rendering)
TIDDLYDESKTOP_DISABLE_GPU=1 ./tiddlydesktop-rs
```

**KDE Plasma specific:**
```bash
GTK_USE_PORTAL=0 ./tiddlydesktop-rs
# Or on Wayland:
GDK_BACKEND=x11 ./tiddlydesktop-rs
```

**Make permanent** by adding to `~/.bashrc`:
```bash
export WEBKIT_DISABLE_DMABUF_RENDERER=1
```

### Linux: Graphics Glitches

Blank windows or rendering artifacts (common with nouveau driver):
```bash
TIDDLYDESKTOP_DISABLE_GPU=1 ./tiddlydesktop-rs
```

### Linux: Missing Libraries

If the app fails to start, ensure all dependencies are installed:
```bash
# Debian/Ubuntu
sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0 libayatana-appindicator3-1

# Fedora
sudo dnf install webkit2gtk4.1 gtk3 libayatana-appindicator-gtk3

# Arch
sudo pacman -S webkit2gtk-4.1 gtk3 libayatana-appindicator
```

## Known Limitations

### Linux: Window Title

The window title doesn't update to reflect the wiki name on Linux due to a WebKitGTK limitation. Works correctly on Windows and macOS.

### macOS: Orphaned Node.js Processes

If you force-kill the app (e.g., `kill -9`) while using wiki folders, Node.js processes may remain running. Clean up manually:
```bash
ps aux | grep "node.*tiddlywiki"
kill <PID>
```

Normal quit (menu/tray) always cleans up properly.

## Verifying Downloads

Each release includes `CHECKSUMS-SHA256.txt` with SHA256 checksums.

**Linux/macOS:**
```bash
sha256sum -c CHECKSUMS-SHA256.txt --ignore-missing
```

**Windows (PowerShell):**
```powershell
Get-FileHash .\TiddlyDesktopRS_x64-setup.exe -Algorithm SHA256
```

## Building from Source

### Prerequisites

- [Node.js](https://nodejs.org/) 20+
- [Rust](https://rustup.rs/) (stable)
- Platform-specific dependencies (see below)

### Build Dependencies

**Linux (Debian/Ubuntu):**
```bash
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
```

**Linux (Fedora):**
```bash
sudo dnf install gtk3-devel webkit2gtk4.1-devel libayatana-appindicator-gtk3-devel librsvg2-devel
```

**Linux (Arch):**
```bash
sudo pacman -S gtk3 webkit2gtk-4.1 libayatana-appindicator librsvg
```

**Windows:**
- Visual Studio Build Tools with C++ workload (comes with Rust)
- [WiX Toolset](https://wixtoolset.org/) v3 for `.msi` builds: `choco install wixtoolset`

**macOS:**
- Xcode Command Line Tools: `xcode-select --install`

### Build Steps

```bash
# Clone repositories
git clone https://github.com/BurningTreeC/tiddlydesktop-rs.git
git clone https://github.com/TiddlyWiki/TiddlyWiki5.git

# Copy plugins and editions to TiddlyWiki5
cp -r tiddlydesktop-rs/TiddlyWiki5/plugins/tiddlywiki/tiddlydesktop-rs TiddlyWiki5/plugins/tiddlywiki/
cp -r tiddlydesktop-rs/TiddlyWiki5/plugins/tiddlywiki/tiddlydesktop-rs-commands TiddlyWiki5/plugins/tiddlywiki/
cp -r tiddlydesktop-rs/TiddlyWiki5/editions/tiddlydesktop-rs TiddlyWiki5/editions/

# Build TiddlyWiki landing page
cd TiddlyWiki5
node tiddlywiki.js editions/tiddlydesktop-rs --output ../tiddlydesktop-rs/src --render '$:/core/save/all' 'index.html' 'text/plain'

# Bundle TiddlyWiki into the app
cd ../tiddlydesktop-rs
npm install
mkdir -p src-tauri/resources
cp -r ../TiddlyWiki5 src-tauri/resources/tiddlywiki
cp src/index.html src-tauri/resources/index.html

# Build the application
npm run tauri build
```

Built artifacts will be in `src-tauri/target/release/bundle/`.

## Why the Security Warnings?

The application is not code-signed because certificates cost $100-400+/year, which isn't feasible for this free, open-source project.

**The app is safe** - you can:
- Review the [source code](https://github.com/BurningTreeC/tiddlydesktop-rs)
- Verify downloads using SHA256 checksums
- Build from source yourself

## License

MIT License - see [LICENSE](LICENSE) for details.

## Acknowledgments

- [TiddlyWiki](https://tiddlywiki.com/) - The non-linear personal web notebook
- [Tauri](https://tauri.app/) - Framework for building desktop apps
- [Original TiddlyDesktop](https://github.com/TiddlyWiki/TiddlyDesktop) - Inspiration for this project
