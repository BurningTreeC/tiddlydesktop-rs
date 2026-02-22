# TiddlyDesktop-RS

A modern, cross-platform application for [TiddlyWiki](https://tiddlywiki.com/), built with [Tauri](https://tauri.app/) and Rust. Available on **Windows**, **macOS**, **Linux**, and **Android**.

## Features

- **Single-file wikis**: Open and edit standalone TiddlyWiki HTML files with automatic saving and configurable backups
- **Wiki folders**: Full Node.js-powered wiki folder support with real-time syncing (desktop only)
- **Android app**: Full-featured Android app available on the [Google Play Store](https://play.google.com/store/apps/details?id=com.burningtreec.tiddlydesktop_rs)
- **LAN Sync**: Synchronize tiddlers between devices on your local network with PIN-based pairing and conflict resolution
- **Cloud saver chaining**: Automatically trigger GitHub, GitLab, Gitea, or Tiddlyhost savers after each local save
- **External attachments**: Store files as external references to keep wiki size small
- **Cross-wiki drag and drop**: Drag tiddlers between wiki windows using native platform drag APIs
- **Native PDF rendering**: View PDFs inline with zoom, page navigation, and text selection
- **Create new wikis**: Initialize new single-file or folder wikis from any TiddlyWiki edition with optional plugin pre-installation
- **Portable mode**: Run from a USB drive with all data stored alongside the executable
- **30+ languages**: Full internationalization with auto-detect
- **Lightweight**: Small download size and low resource usage thanks to Tauri

## Screenshots

![Landing Page](docs/screenshots/screenshot1.png)
*Landing page - Open or create wikis*

![Create Wiki - Edition Selection](docs/screenshots/screenshot2.png)
*Create a new wiki - Choose from available editions*

![Create Wiki - Plugin Selection](docs/screenshots/screenshot3.png)
*Create a new wiki - Select additional plugins*

![Wiki Window](docs/screenshots/screenshot4.png)
*TiddlyWiki running in its own window*

![Dark Theme](docs/screenshots/screenshot5.png)
*Landing page with dark palette and wiki listed*

## Quick Start

1. Download the installer for your platform from the [Releases page](../../releases), or install from [Google Play](https://play.google.com/store/apps/details?id=com.burningtreec.tiddlydesktop_rs) on Android
2. Install and launch TiddlyDesktopRS
3. Click **"Open Wiki File"** to open an existing TiddlyWiki HTML file, or
4. Click **"New Wiki File"** to create a new wiki

## Download

### Desktop

| Platform | Recommended | Alternative |
|----------|-------------|-------------|
| **Windows** | `.msi` installer | `.exe` (NSIS installer) |
| **macOS** | `.dmg` disk image | `.app.zip` |
| **Linux** | `.deb` / `.rpm` / `.pkg.tar.zst` | `.tar.gz` (portable) |

#### Which Build Should I Choose?

**Standard builds** (recommended): Include a bundled Node.js binary for wiki folder support. Everything works out of the box.

**`PROVIDE_YOUR_OWN_NODEJS_` builds**: Smaller downloads without bundled Node.js. Use these if:
- You already have Node.js 18+ installed system-wide
- You only use single-file wikis (Node.js not required)
- You want smaller download size

**Note**: Linux builds never bundle Node.js - install it via your package manager if you need wiki folder support.

### Android

Install from the [Google Play Store](https://play.google.com/store/apps/details?id=com.burningtreec.tiddlydesktop_rs).

## Installation

### Windows

1. Download the `.msi` or `.exe` installer
2. Run the installer - choose between **Install mode** (with shortcuts and uninstaller) or **Portable mode** (run from any folder)
3. **Security warning**: Windows SmartScreen may show "Windows protected your PC"
   - Click **"More info"** then **"Run anyway"**

### macOS

1. Download the `.dmg` file
2. Open the disk image and drag the app to Applications
3. **Security warning**: macOS may show "app is damaged" or "unidentified developer"
   - **Option A**: Right-click the app, then **Open**, then **Open**
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

---

## Usage

### Opening Wikis

- **Click** "Open Wiki File" or "Open Wiki Folder" from the landing page
- **Drag and drop** a `.html` wiki file onto the app window
- **Double-click** a `.html` file (if file associations are set up)
- **Android**: Use "Open with" from your file manager to open HTML files in TiddlyDesktopRS

### Saving Wikis

| Wiki Type | How to Save |
|-----------|-------------|
| Single-file | Press **Ctrl+S** (or **Cmd+S** on macOS), or click the save button in TiddlyWiki |
| Wiki folder | Saves automatically on every change |

Single-file wikis create timestamped backups when saved. See [How to Configure Backups](#how-to-configure-backups) for details.

### Creating New Wikis

1. Click **"New Wiki File"** or **"New Wiki Folder"**
2. Select an edition (e.g., "empty", "full")
3. Optionally select additional plugins to pre-install
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
| Conversion | Can convert to folder | Can convert to single file |

### Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| Ctrl+S / Cmd+S | Save wiki (single-file wikis) |
| Ctrl+F / Cmd+F | Find in page |
| F3 / Shift+F3 | Find next / previous |
| Ctrl+G / Cmd+G | Find next |
| Ctrl+Plus / Cmd+Plus | Zoom in |
| Ctrl+Minus / Cmd+Minus | Zoom out |
| Ctrl+0 / Cmd+0 | Reset zoom to 100% |
| Escape | Close find bar |

---

## Features in Detail

### Cloud Saver Chaining

After each local save, TiddlyDesktopRS automatically triggers any configured cloud savers in your wiki. This works with:

- **GitHub Saver** - save to a GitHub repository
- **GitLab Saver** - save to a GitLab repository
- **Gitea Saver** - save to a Gitea instance
- **Tiddlyhost** - save to Tiddlyhost

No extra configuration is needed in TiddlyDesktopRS -- just set up the cloud saver within TiddlyWiki as you normally would, and TiddlyDesktopRS will chain the cloud save after every local file save.

### External Attachments

Store files as external references (`_canonical_uri`) instead of embedding them in the wiki, keeping your wiki file small:

1. Open a wiki window
2. Go to the TiddlyWiki control panel (gear icon)
3. Find the **TiddlyDesktop Settings** tab
4. Enable **External Attachments**
5. Files dropped into the wiki will now be saved alongside it and referenced by path

Options include relative paths (default) and absolute paths for files outside the wiki's directory.

### Native PDF Rendering

PDFs embedded in tiddlers are rendered natively using PDFium -- no browser plugin required:

- Multi-page scrollable view with lazy page rendering
- Zoom in/out and fit-to-width controls
- Page navigation
- Text selection and copy
- Resizable viewer that adapts to container width

### Cross-Wiki Tiddler Drag and Drop

Drag tiddlers between wiki windows to copy them:

1. Start dragging a tiddler link in one wiki window
2. Drop it onto another wiki window
3. The tiddler (with all fields) is imported into the target wiki

This uses native platform drag APIs (GTK on Linux, OLE on Windows, AppKit on macOS) for true cross-window drag-and-drop.

You can also drop files from your file manager into a wiki to import them as tiddlers.

### Wiki Grouping

Organize your wikis into collapsible groups on the landing page:

1. Click the group dropdown next to any wiki in the list
2. Select an existing group or type a new group name
3. Wikis in the same group appear together in a collapsible section

Use the group manager (accessible from the toolbar) to rename or delete groups.

### Wiki Conversion

Convert between wiki formats from the landing page:

- **Single-file to folder**: Right-click a wiki in the list and choose "Convert to Folder Wiki" (requires Node.js)
- **Folder to single-file**: Right-click a folder wiki and choose "Convert to Single File"

### Plugin Installer

Install or remove bundled TiddlyWiki plugins to/from any wiki directly from the landing page:

1. Click the plugin icon next to a wiki in the list
2. Browse available plugins -- already-installed plugins are marked
3. Toggle plugins on/off and save

### Palette and Language

- **Palette selector**: Click the palette button in the landing page toolbar to choose from all available TiddlyWiki color palettes. Changes apply immediately.
- **Language selector**: Click the language button to choose from 30+ languages, or use auto-detect mode.

### Portable Mode

Run TiddlyDesktopRS from a USB drive or any folder without installing:

1. Place a file named `portable` (or `portable.txt`) next to the application executable
2. All data (wiki list, settings, backups) will be stored alongside the executable instead of in the system app data directory

The Windows NSIS installer also offers a Portable mode option during installation.

### System Tray (Desktop)

A system tray icon provides quick access:
- **Double-click** the tray icon to show/focus the landing page
- **Right-click** for a context menu with Show and Quit options

### Window State Persistence

Each wiki window's size, position, and monitor placement are saved and restored the next time you open that wiki.

### Automatic Updates

TiddlyDesktopRS checks for updates on startup:
- **Desktop**: Shows a toolbar button linking to GitHub Releases when a new version is available
- **Android**: Shows a banner linking to the Google Play Store

---

## LAN Sync

Synchronize tiddlers between devices on your local network. LAN Sync works across all platforms (Windows, macOS, Linux, Android).

### Setting Up LAN Sync

1. **Pair devices**: On one device, go to the landing page settings and find the LAN Sync section. Note the 6-digit PIN displayed. On the other device, enter that PIN to pair.

2. **Enable sync per wiki**: On the landing page, click the sync icon next to a wiki to enable LAN Sync for that wiki. Both devices must have sync enabled for the same wiki.

3. **Sync happens automatically**: Once paired and enabled, tiddler changes are sent to paired devices in real-time whenever a tiddler is modified.

### Wiki Transfer

Request an entire wiki from a paired device:
1. On the receiving device, use the "Request Wiki" option in LAN Sync settings
2. Choose which wiki to request from the remote device
3. Select a local save location
4. The wiki file is transferred over the LAN

### Linking Existing Wikis

If you have the same wiki on two devices but they were opened independently (not transferred), you can link them:
1. Enable LAN Sync for the wiki on both devices
2. Use the "Link to Remote Wiki" option to connect the local wiki to the remote wiki's sync ID

### Conflict Resolution

When LAN Sync detects concurrent edits to the same tiddler on different devices:
1. A notification banner appears at the top of the wiki window
2. Click **Resolve** to open the conflict resolution modal
3. Review field-by-field diffs (color-coded: red = local, green = remote)
4. Choose **Keep Local** or **Keep Remote** for each conflict, or use **Resolve All** for batch resolution

### Network Requirements

LAN Sync uses:
- **UDP port 45699** for device discovery (broadcast)
- **TCP ports 45700-45710** for sync connections

If you use a firewall, you may need to allow these ports. On Linux:

```bash
# UFW (Ubuntu, Linux Mint)
sudo ufw allow 45699/udp
sudo ufw allow 45700:45710/tcp

# firewalld (Fedora)
sudo firewall-cmd --permanent --add-port=45699/udp
sudo firewall-cmd --permanent --add-port=45700-45710/tcp
sudo firewall-cmd --reload
```

All sync data is encrypted using SPAKE2 key exchange and ChaCha20-Poly1305 authenticated encryption.

---

## Android

The Android app supports single-file wikis with most of the same features as the desktop app:

- **Open wikis** from any storage location using Android's file picker
- **Create new wikis** from available editions with optional plugins
- **LAN Sync** with desktop and other Android devices
- **Cloud saver chaining** (GitHub, GitLab, Gitea, Tiddlyhost)
- **Open With** support - open HTML files shared from other apps
- **Share to wiki** - share text, images, and files from any Android app into a wiki
- **Native PDF rendering** with zoom, page navigation, and text selection
- **Video poster extraction** - video thumbnails are extracted natively and cached
- **Image format conversion** - HEIC, TIFF, and AVIF images are automatically converted to JPEG
- **System bar colors** sync with the wiki's TiddlyWiki palette
- **Folder wikis** via embedded Node.js (with local file sync back to SAF storage)

---

## How-To Guides

### How to Configure Backups

Each single-file wiki can have its own backup settings:

1. On the landing page, click the **settings icon** (gear) next to a wiki
2. **Enable/disable backups** - toggle automatic backup creation on save
3. **Backup directory** - set a custom directory for backups (default: `.backups` folder next to the wiki file)
4. **Backup count** - set the maximum number of backups to keep (default: 20). Options include 5, 10, 20, 50, unlimited, or a custom number. Oldest backups are automatically deleted when the limit is exceeded.

Backups are named with timestamps (e.g., `MyWiki_20260220143005.html`) so you can easily find a specific version.

### How to Use External Attachments

External attachments let you reference files by path instead of embedding them, which keeps your wiki file small:

1. Open your wiki
2. Open the TiddlyWiki control panel (gear icon in the sidebar)
3. Go to the **TiddlyDesktop Settings** tab
4. Enable **External Attachments**
5. Now when you drag a file into the wiki, it will be saved alongside the wiki file and referenced via `_canonical_uri`

**Path modes:**
- **Relative paths** (default): Files are referenced relative to the wiki location. Best for wikis you might move around.
- **Absolute paths**: For files outside the wiki's directory tree. Enable "Use absolute path for files outside wiki directory" in the settings.

### How to Use Cloud Savers

TiddlyDesktopRS automatically triggers cloud saves after every local save. To set this up:

1. **GitHub Saver**: In your wiki, go to Control Panel > Saving > GitHub Saver. Enter your GitHub token, username, repo, branch, and file path.
2. **GitLab Saver**: Control Panel > Saving > GitLab Saver. Enter your GitLab token, project ID, branch, and file path.
3. **Gitea Saver**: Control Panel > Saving > Gitea Saver. Enter your server URL, token, repo, and file path.
4. **Tiddlyhost**: Control Panel > Saving > Tiddlyhost. Enter your credentials.

Once configured in TiddlyWiki, TiddlyDesktopRS handles the rest -- every time you save locally (Ctrl+S), the cloud saver runs automatically in the background.

### How to Drag Tiddlers Between Wikis

1. Open two or more wiki windows
2. In the source wiki, find the tiddler you want to copy
3. Start dragging the tiddler's title link
4. Move your mouse to the other wiki window and drop
5. The tiddler (with all fields and content) is imported

This also works for dropping files from your file manager into a wiki window.

### How to Set Up LAN Sync

**On Device A:**
1. Open the landing page
2. Open Settings (gear icon in toolbar)
3. Under LAN Sync, note the displayed 6-digit PIN

**On Device B:**
1. Open the landing page
2. Open Settings
3. Under LAN Sync, enter Device A's PIN and click Pair

**Enable sync for a wiki:**
1. On the landing page, click the sync icon next to the wiki you want to sync
2. Do the same on the other device for the same wiki
3. Changes now sync automatically between the two devices

### How to Import Files into a Wiki

**From file manager:**
- Drag files directly from your file manager into a wiki window
- `.tid`, `.json`, `.csv` files are imported as TiddlyWiki tiddlers
- Images and other files are imported as binary tiddlers (or external attachments if enabled)

**From clipboard:**
- Copy an image, then paste into a wiki window to import it

**On Android:**
- Use the Share button in any app to share content to TiddlyDesktopRS
- Choose which wiki to import into

### How to Use Portable Mode

**Option 1 - Manual setup:**
1. Copy the application executable to your desired location (e.g., USB drive)
2. Create an empty file named `portable` (no extension) in the same directory
3. Launch the application -- all data will be stored alongside it

**Option 2 - Windows NSIS installer:**
1. Run the `.exe` installer
2. On the installation mode page, choose **"Portable"**
3. Select your destination folder

In portable mode, the following are stored next to the executable:
- `settings.json` - Application preferences
- `wikis.json` - Wiki list and window positions
- `editions/` - Custom TiddlyWiki editions

### How to Add Custom Editions

Add your own TiddlyWiki editions to the edition selector:

1. Create your edition directory with a valid `tiddlywiki.info` file
2. Copy it to the editions directory:

| Platform | Path |
|----------|------|
| Linux | `~/.local/share/tiddlydesktop-rs/editions/` |
| macOS | `~/Library/Application Support/tiddlydesktop-rs/editions/` |
| Windows | `%APPDATA%\tiddlydesktop-rs\editions\` |
| Portable | `editions/` next to executable |

3. Restart the app -- your edition will appear in the "New Wiki" dialog

### How to Convert Between Wiki Types

**Single-file to folder wiki:**
1. On the landing page, find the wiki in the list
2. Click the convert icon or use the context menu
3. Choose "Convert to Folder Wiki"
4. Select a destination directory
5. The wiki is converted and the new folder wiki opens

**Folder wiki to single-file:**
1. Find the folder wiki in the list
2. Click the convert icon
3. Choose "Convert to Single File"
4. Choose where to save the HTML file

Note: Wiki folder conversion requires Node.js.

---

## Data Storage Locations

TiddlyDesktopRS stores settings and wiki metadata in:

| Platform | Path |
|----------|------|
| Linux | `~/.local/share/tiddlydesktop-rs/` |
| macOS | `~/Library/Application Support/tiddlydesktop-rs/` |
| Windows | `%APPDATA%\tiddlydesktop-rs\` |
| Portable | Next to executable |

This includes:
- `settings.json` - Application preferences
- `wikis.json` - Recently opened wikis and window positions
- `editions/` - Custom TiddlyWiki editions

---

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

### Linux: Scrollbar and Scrolling Tweaks

```bash
# Disable overlay scrollbars (always show classic scrollbars)
GTK_OVERLAY_SCROLLING=0 ./tiddlydesktop-rs

# Use older input handling (can fix some scrolling issues)
GDK_CORE_DEVICE_EVENTS=1 ./tiddlydesktop-rs
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

### Linux: LAN Sync Firewall

If LAN Sync can't discover devices, your firewall may be blocking the required ports:

```bash
# UFW (Ubuntu, Linux Mint)
sudo ufw allow 45699/udp
sudo ufw allow 45700:45710/tcp

# firewalld (Fedora)
sudo firewall-cmd --permanent --add-port=45699/udp
sudo firewall-cmd --permanent --add-port=45700-45710/tcp
sudo firewall-cmd --reload

# iptables (manual)
sudo iptables -A INPUT -p udp --dport 45699 -j ACCEPT
sudo iptables -A INPUT -p tcp --dport 45700:45710 -j ACCEPT
```

### Android: Wiki Permission Lost

If a wiki shows a "re-authorize" button on the landing page, the Storage Access Framework permission has expired. Click the button to re-grant access via the file picker.

---

## Known Limitations

### macOS: Orphaned Node.js Processes

If you force-kill the app (e.g., `kill -9`) while using wiki folders, Node.js processes may remain running. Clean up manually:
```bash
ps aux | grep "node.*tiddlywiki"
kill <PID>
```

Normal quit (menu/tray) always cleans up properly. On Linux and Windows, child processes are automatically killed when the parent exits.

### Android: Background Suspension

Android may suspend the app when it's in the background. Wiki servers continue running via a foreground service notification, but very long background periods may still interrupt connections.

### LAN Sync: Same Network Required

LAN Sync only works when devices are on the same local network (same Wi-Fi, same subnet). It does not work across the internet.

---

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

---

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

---

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
- [Tauri](https://tauri.app/) - Framework for building desktop and mobile apps
- [Original TiddlyDesktop](https://github.com/TiddlyWiki/TiddlyDesktop) - Inspiration for this project
