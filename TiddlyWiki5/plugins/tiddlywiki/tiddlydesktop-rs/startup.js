/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/startup.js
type: application/javascript
module-type: startup

Tauri integration startup module

\*/
(function() {

"use strict";

exports.name = "tiddlydesktop-rs";
exports.platforms = ["browser"];
exports.after = ["startup"];
exports.synchronous = false;

exports.startup = function(callback) {
	// Only run in Tauri environment
	if (typeof window === "undefined" || !window.__TAURI__) {
		callback();
		return;
	}

	var invoke = window.__TAURI__.core.invoke;
	var listen = window.__TAURI__.event.listen;
	var openDialog = window.__TAURI__.dialog.open;

	// Detect if running on Android
	var isAndroid = /android/i.test(navigator.userAgent);


	// Convert Android SAF content:// URIs to human-readable paths for display.
	// Handles both plain URIs and JSON-wrapped URIs ({"uri":"content://..."}).
	// On desktop, returns the path unchanged.
	function getDisplayPath(pathStr) {
		if (!pathStr) return "";
		// Extract URI from JSON if needed
		var uri = pathStr;
		if (pathStr.charAt(0) === '{') {
			try {
				var parsed = JSON.parse(pathStr);
				uri = parsed.uri || pathStr;
			} catch(e) {}
		}
		if (!uri.startsWith("content://")) {
			return uri;
		}
		try {
			var decoded = decodeURIComponent(uri);
			// document/storage:path (single file) — storage can be primary, home, SD card ID, etc.
			// Skip opaque numeric IDs (e.g. msf:12345 from recents picker)
			var docMatch = decoded.match(/\/document\/[^/:]+:(.+)$/);
			if (docMatch && !/^\d+$/.test(docMatch[1])) return docMatch[1];
			// tree/.../document/storage:path (file inside tree)
			var treeDocMatch = decoded.match(/\/tree\/[^/]+\/document\/[^/:]+:(.+)$/);
			if (treeDocMatch && !/^\d+$/.test(treeDocMatch[1])) return treeDocMatch[1];
			// tree/storage:path (folder)
			var treeMatch = decoded.match(/\/tree\/[^/:]+:(.+)$/);
			if (treeMatch && !/^\d+$/.test(treeMatch[1])) return treeMatch[1];
			// Downloads provider
			if (decoded.indexOf("downloads") !== -1) {
				var dlMatch = decoded.match(/\/document\/(.+)$/);
				if (dlMatch) return "Downloads/" + dlMatch[1];
			}
		} catch(e) {}
		return uri.length > 50 ? "..." + uri.substring(uri.length - 40) : uri;
	}

	// Check if this is the main wiki
	var isMainWiki = window.__IS_MAIN_WIKI__ === true;

	// Store references globally for other modules
	$tw.tiddlydesktoprs = {
		invoke: invoke,
		listen: listen,
		openDialog: openDialog,
		isMainWiki: isMainWiki,
		// Async dialog methods for TiddlyWiki to use
		alert: function(message) {
			return invoke("show_alert", { message: String(message || "") });
		},
		confirm: function(message) {
			return invoke("show_confirm", { message: String(message || "") });
		}
	};

	// Check if terms and conditions have been accepted (one-time gate for sync features)
	function checkTermsAccepted() {
		var accepted = $tw.wiki.getTiddlerText("$:/config/TiddlyDesktopRS/TermsAccepted");
		if(accepted) return true;
		var msg = $tw.wiki.renderText("text/plain", "text/vnd.tiddlywiki", "<<td-lingo Terms/AcceptPrompt>>");
		msg += "\nhttps://burningtreec.github.io/TiddlyDesktopRust/";
		if(confirm(msg)) {
			$tw.wiki.setText("$:/config/TiddlyDesktopRS/TermsAccepted", "text", null, new Date().toISOString());
			return true;
		}
		return false;
	}

	// Desktop app - always set mobile to no
	$tw.wiki.setText("$:/temp/tiddlydesktop-rs/is-mobile", "text", null, "no");

	// Set Android flag for UI to show/hide Android-specific elements
	$tw.wiki.setText("$:/temp/tiddlydesktop-rs/is-android", "text", null, isAndroid ? "yes" : "no");

	// Set main wiki flag
	$tw.wiki.setText("$:/temp/tiddlydesktop-rs/is-main-wiki", "text", null, isMainWiki ? "yes" : "no");

	// Add body class for main wiki (used by CSS to hide notifications)
	if (isMainWiki) {
		document.body.classList.add("td-main-wiki");
	}

	// Add body class for Android (used by CSS for Android-specific styling)
	// Also add to html element since :has() selector isn't supported in Android WebView
	if (isAndroid) {
		document.body.classList.add("td-is-android");
		document.documentElement.classList.add("td-is-android");
	}

	// Add class to html element for main wiki on Android (for scroll fix)
	if (isAndroid && isMainWiki) {
		document.documentElement.classList.add("td-android-main-wiki");
	}

	// Scroll focused inputs into view when Android keyboard opens
	if (isAndroid) {
		document.addEventListener("focusin", function(e) {
			var el = e.target;
			if (el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT")) {
				setTimeout(function() {
					el.scrollIntoView({block: "center", behavior: "smooth"});
				}, 300);
			}
		});
	}

	// ========================================
	// Android System Bar Color Sync
	// ========================================
	if (isAndroid) {
		// Function to get a color from TiddlyWiki's current palette (with recursive resolution)
		function getColour(name, fallback, depth) {
			depth = depth || 0;
			if (depth > 10) return fallback; // Prevent infinite recursion

			try {
				// Get the current palette title
				var paletteName = $tw.wiki.getTiddlerText("$:/palette");
				if (paletteName) {
					paletteName = paletteName.trim();
					var paletteTiddler = $tw.wiki.getTiddler(paletteName);
					if (paletteTiddler) {
						// Colors are in the tiddler text (one per line: name: value)
						var text = paletteTiddler.fields.text || "";
						var lines = text.split("\n");
						for (var i = 0; i < lines.length; i++) {
							var line = lines[i].trim();
							var colonIndex = line.indexOf(":");
							if (colonIndex > 0) {
								var colorName = line.substring(0, colonIndex).trim();
								var colorValue = line.substring(colonIndex + 1).trim();
								if (colorName === name && colorValue) {
									// Handle references to other colors like <<colour background>>
									var match = colorValue.match(/<<colour\s+([^>]+)>>/);
									if (match) {
										return getColour(match[1].trim(), fallback, depth + 1);
									}
									return colorValue;
								}
							}
						}
					}
				}
			} catch (e) {
				console.error('[TiddlyDesktop-Android] getColour error:', e);
			}
			return fallback;
		}

		// Resolve any CSS color to #rrggbb or rgba(r,g,b,a) using a canvas context
		var _colorCtx = null;
		function resolveCssColor(color, fallback) {
			try {
				if (!_colorCtx) _colorCtx = document.createElement("canvas").getContext("2d");
				_colorCtx.fillStyle = "#000";
				_colorCtx.fillStyle = color;
				var resolved = _colorCtx.fillStyle;
				if (resolved === "#000000" && color.trim().toLowerCase() !== "#000000" && color.trim().toLowerCase() !== "#000" && color.trim().toLowerCase() !== "black") {
					return fallback || color;
				}
				return resolved;
			} catch (e) {
				return fallback || color;
			}
		}

		// Dark mode fallback colors (used when palette has no color defined)
		var _isDarkMode = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
		var _defaultBg = _isDarkMode ? "#333333" : "#ffffff";
		var _defaultFg = _isDarkMode ? "#cccccc" : "#333333";

		// Function to update system bar colors
		function updateSystemBarColors() {
			// Use page-background for status bar, tiddler-background for nav bar
			var statusBarColor = resolveCssColor(getColour("page-background", _defaultBg), _defaultBg);
			var navBarColor = resolveCssColor(getColour("tiddler-background", statusBarColor), statusBarColor);
			// Get foreground color to determine if icons should be light or dark
			var foregroundColor = resolveCssColor(getColour("foreground", _defaultFg), _defaultFg);

			invoke("android_set_system_bar_colors", {
				statusBarColor: statusBarColor,
				navBarColor: navBarColor,
				foregroundColor: foregroundColor
			}).catch(function(err) {
				console.error("[TiddlyDesktop-Android] Failed to set system bar colors:", err);
			});
		}

		// Wait for palette to be ready before updating colors
		function waitForPaletteAndUpdate(retries) {
			retries = retries || 0;
			if (retries > 100) {
				// Give up after 5 seconds, use defaults
				updateSystemBarColors();
				return;
			}
			var paletteName = ($tw.wiki.getTiddlerText("$:/palette") || "").trim();
			if (paletteName) {
				var paletteTiddler = $tw.wiki.getTiddler(paletteName);
				// Check that palette exists AND has color content
				if (paletteTiddler && paletteTiddler.fields.text && paletteTiddler.fields.text.indexOf(":") > 0) {
					// Palette is ready with color definitions, update colors
					updateSystemBarColors();
					return;
				}
			}
			// Palette not ready yet, try again
			setTimeout(function() { waitForPaletteAndUpdate(retries + 1); }, 50);
		}

		// Start checking for palette on startup
		waitForPaletteAndUpdate(0);

		// Listen for palette changes
		$tw.wiki.addEventListener("change", function(changes) {
			// Check if the palette reference changed
			if (changes["$:/palette"]) {
				updateSystemBarColors();
				return;
			}
			// Check if the current palette tiddler itself changed
			var paletteName = ($tw.wiki.getTiddlerText("$:/palette") || "").trim();
			if (paletteName && changes[paletteName]) {
				updateSystemBarColors();
			}
		});
	}

	// Non-main wikis don't need the wiki list management - external attachments
	// and drag-drop are handled by the protocol handler script injection
	if (!isMainWiki) {
		// ========================================
		// tm-open-window handler (for opened wikis only, not landing page)
		// Opens tiddlers in new windows using Tauri
		// ========================================
		// Remove TiddlyWiki's built-in handlers (from core windows.js) which use
		// window.open() — that doesn't work in Tauri webviews.
		$tw.rootWidget.eventListeners["tm-open-window"] = [];
		$tw.rootWidget.eventListeners["tm-close-window"] = [];
		$tw.rootWidget.eventListeners["tm-close-all-windows"] = [];
		$tw.rootWidget.addEventListener("tm-open-window", function(event) {
			var title = event.param || event.tiddlerTitle;
			var paramObject = event.paramObject || {};
			var windowTitle = paramObject.windowTitle || title;
			var windowID = paramObject.windowID || title;
			var template = paramObject.template || "$:/core/templates/single.tiddler.window";
			var width = paramObject.width ? parseFloat(paramObject.width) : null;
			var height = paramObject.height ? parseFloat(paramObject.height) : null;
			var left = paramObject.left ? parseFloat(paramObject.left) : null;
			var top = paramObject.top ? parseFloat(paramObject.top) : null;

			// Collect any additional variables (any params not in the known list)
			var knownParams = ["windowTitle", "windowID", "template", "width", "height", "left", "top"];
			var extraVariables = {};
			for (var key in paramObject) {
				if (paramObject.hasOwnProperty(key) && knownParams.indexOf(key) === -1) {
					extraVariables[key] = paramObject[key];
				}
			}
			// Always include currentTiddler and tv-window-id
			extraVariables.currentTiddler = title;
			extraVariables["tv-window-id"] = windowID;

			// Get the current window label
			var currentLabel = window.__WINDOW_LABEL__ || "main";

			// Store references to opened Tauri windows separately from TiddlyWiki's $tw.windows
			// TiddlyWiki expects $tw.windows entries to be actual Window objects with document.body
			window.__tiddlyDesktopWindows = window.__tiddlyDesktopWindows || {};

			// Call Tauri command to open tiddler window
			invoke("open_tiddler_window", {
				parentLabel: currentLabel,
				tiddlerTitle: title,
				template: template,
				windowTitle: windowTitle,
				width: width,
				height: height,
				left: left,
				top: top,
				variables: JSON.stringify(extraVariables)
			}).then(function(newLabel) {
				// Store reference in our own tracking (not $tw.windows)
				window.__tiddlyDesktopWindows[windowID] = { label: newLabel, title: title };
			}).catch(function(err) {
				console.error("Failed to open tiddler window:", err);
			});

			// Prevent default TiddlyWiki handler
			return false;
		});

		// tm-close-window handler
		$tw.rootWidget.addEventListener("tm-close-window", function(event) {
			var windowID = event.param;
			var windows = window.__tiddlyDesktopWindows || {};
			if (windows[windowID]) {
				var windowInfo = windows[windowID];
				invoke("close_window_by_label", { label: windowInfo.label }).catch(function(err) {
					console.error("Failed to close window:", err);
				});
				delete windows[windowID];
			}
			return false;
		});

		// tm-close-all-windows handler
		$tw.rootWidget.addEventListener("tm-close-all-windows", function(event) {
			var windows = window.__tiddlyDesktopWindows || {};
			Object.keys(windows).forEach(function(windowID) {
				var windowInfo = windows[windowID];
				invoke("close_window_by_label", { label: windowInfo.label }).catch(function() {});
			});
			window.__tiddlyDesktopWindows = {};
			return false;
		});

		// tm-open-external-window handler - opens URL in default browser
		$tw.rootWidget.addEventListener("tm-open-external-window", function(event) {
			var url = event.param || "https://tiddlywiki.com/";
			// Use Tauri's opener plugin to open in default browser
			if (window.__TAURI__ && window.__TAURI__.opener) {
				window.__TAURI__.opener.openUrl(url).catch(function(err) {
					console.error("Failed to open external URL:", err);
				});
			}
			return false;
		});

		callback();
		return;
	}

	// ========================================
	// Wiki List Storage Functions (Tiddler-based)
	// ========================================

	// Get wiki list entries from the persistent tiddler
	function getWikiListEntries() {
		var tiddler = $tw.wiki.getTiddler("$:/TiddlyDesktop/WikiList");
		if (tiddler && tiddler.fields.text) {
			try {
				return JSON.parse(tiddler.fields.text);
			} catch(e) {
				console.error("Failed to parse wiki list:", e);
			}
		}
		return [];
	}

	// Get the list of collapsed group names from the persistent tiddler
	function getCollapsedGroups() {
		var text = $tw.wiki.getTiddlerText("$:/TiddlyDesktop/CollapsedGroups", "");
		if (!text.trim()) return [];
		try { return JSON.parse(text); } catch(e) { return []; }
	}

	// Save collapsed groups and trigger autosave
	function saveCollapsedGroups(collapsed) {
		$tw.wiki.addTiddler({
			title: "$:/TiddlyDesktop/CollapsedGroups",
			type: "application/json",
			text: JSON.stringify(collapsed)
		});
		$tw.rootWidget.dispatchEvent({type: "tm-auto-save-wiki"});
	}

	// Restore collapsed group state tiddlers from persistent storage
	function restoreCollapsedGroups() {
		var collapsed = getCollapsedGroups();
		for (var i = 0; i < collapsed.length; i++) {
			$tw.wiki.setText("$:/state/tiddlydesktop-rs/group-collapsed/" + collapsed[i], "text", null, "yes");
		}
	}

	// Save wiki list to the persistent tiddler and trigger autosave
	function saveWikiList(entries) {
		$tw.wiki.addTiddler({
			title: "$:/TiddlyDesktop/WikiList",
			type: "application/json",
			text: JSON.stringify(entries, null, 2)
		});
		// Trigger autosave so the main wiki file is saved
		$tw.rootWidget.dispatchEvent({type: "tm-auto-save-wiki"});
		// Reconcile: ensure Rust JSON only contains wikis that are in the WikiList.
		// This prevents stale entries from being broadcast to sync peers.
		var paths = entries.map(function(e) { return e.path; });
		invoke("reconcile_recent_files", { paths: paths }).catch(function(err) {
			console.error("[TiddlyDesktop] Failed to reconcile recent files:", err);
		});
	}

	// Add an entry to the wiki list
	function addToWikiList(entry) {
		var entries = getWikiListEntries();
		// Find existing entry to preserve backup settings and group
		var existingEntry = null;
		for (var i = 0; i < entries.length; i++) {
			if (entries[i].path === entry.path) {
				existingEntry = entries[i];
				break;
			}
		}
		if (existingEntry) {
			if (existingEntry.backups_enabled !== undefined) {
				entry.backups_enabled = existingEntry.backups_enabled;
			}
			if (existingEntry.backup_dir) {
				entry.backup_dir = existingEntry.backup_dir;
			}
			if (existingEntry.group) {
				entry.group = existingEntry.group;
			}
			if (existingEntry.backup_count !== undefined) {
				entry.backup_count = existingEntry.backup_count;
			}
			// Preserve favicon if the new entry doesn't have one
			if (!entry.favicon && existingEntry.favicon) {
				entry.favicon = existingEntry.favicon;
			}
			// Preserve LAN sync settings
			if (existingEntry.sync_enabled) {
				entry.sync_enabled = existingEntry.sync_enabled;
			}
			if (existingEntry.sync_id) {
				entry.sync_id = existingEntry.sync_id;
			}
			if (existingEntry.sync_peers && existingEntry.sync_peers.length > 0 && (!entry.sync_peers || entry.sync_peers.length === 0)) {
				entry.sync_peers = existingEntry.sync_peers;
			}
			// Preserve relay room assignment
			if (existingEntry.relay_room && !entry.relay_room) {
				entry.relay_room = existingEntry.relay_room;
			}
		}
		// Remove if already exists
		entries = entries.filter(function(e) { return e.path !== entry.path; });
		// Add to front
		entries.unshift(entry);
		// Limit to 50 entries
		if (entries.length > 50) {
			entries = entries.slice(0, 50);
		}
		saveWikiList(entries);
	}

	// Remove an entry from the wiki list
	function removeFromWikiList(path) {
		var entries = getWikiListEntries();
		entries = entries.filter(function(e) { return e.path !== path; });
		saveWikiList(entries);
		// Also remove from the Rust backend (updates widget data file)
		invoke("remove_recent_file", { path: path }).then(function() {
			// Broadcast updated manifest so peers no longer see this wiki
			invoke("lan_sync_broadcast_manifest").catch(function() {});
		}).catch(function(err) {
			console.error("[TiddlyDesktop] Failed to remove from recent files:", err);
		});
	}

	// Refresh the wiki list UI from the tiddler
	function refreshWikiList() {
		var entries = getWikiListEntries();

		// Clear existing temp entries
		$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/wikis/]]").forEach(function(title) {
			$tw.wiki.deleteTiddler(title);
		});

		// Collect unique groups and update the groups tiddler
		var groups = [];
		entries.forEach(function(entry) {
			if (entry.group && groups.indexOf(entry.group) === -1) {
				groups.push(entry.group);
			}
		});
		groups.sort();
		$tw.wiki.addTiddler({
			title: "$:/temp/tiddlydesktop-rs/groups",
			list: groups.join(" "),
			text: groups.join("\n")
		});

		// Restore persisted collapsed states
		restoreCollapsedGroups();

		// Populate temp tiddlers for UI
		entries.forEach(function(entry, index) {
			$tw.wiki.addTiddler({
				title: "$:/temp/tiddlydesktop-rs/wikis/" + index,
				path: entry.path,
				display_path: entry.display_path || entry.path,
				filename: entry.filename,
				favicon: entry.favicon || "",
				is_folder: entry.is_folder ? "true" : "false",
				backups_enabled: entry.backups_enabled ? "true" : "false",
				backup_dir: entry.backup_dir || "",
				backup_dir_display: entry.backup_dir ? getDisplayPath(entry.backup_dir) : "",
				backup_count: entry.backup_count !== undefined ? String(entry.backup_count) : "",
				group: entry.group || "",
				sync_enabled: entry.sync_enabled ? "true" : "false",
				sync_id: entry.sync_id || "",
				sync_peers: entry.sync_peers ? JSON.stringify(entry.sync_peers) : "[]",
				relay_room: entry.relay_room || "",
				needs_reauth: "checking", // Will be updated by permission check on Android
				text: ""
			});
		});

		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/wiki-count", "text", null, String(entries.length));

		// On Android, check permissions for each wiki
		// On desktop, permissions are always OK (regular file paths)
		if (isAndroid) {
			checkWikiPermissions(entries);
		} else {
			entries.forEach(function(entry, index) {
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/wikis/" + index, "needs_reauth", null, "no");
			});
		}

		// Check which wikis are currently open (for disabling Plugins button etc.)
		entries.forEach(function(entry, index) {
			invoke("is_wiki_open", { path: entry.path }).then(function(isOpen) {
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/wikis/" + index, "is_open", null, isOpen ? "yes" : "no");
			}).catch(function() {
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/wikis/" + index, "is_open", null, "no");
			});
		});
	}

	// Check permissions for all wiki entries (Android only)
	function checkWikiPermissions(entries) {
		entries.forEach(function(entry, index) {
			var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + index;
			// Extract the URI from the path (which is JSON for Android)
			var uri = null;
			try {
				var pathData = JSON.parse(entry.path);
				uri = pathData.uri;
			} catch (e) {
				// Not JSON, use path directly (legacy format)
				uri = entry.path;
			}

			if (uri && uri.startsWith("content://")) {
				// Check if we have permission for this URI
				invoke("android_has_permission", { uri: uri }).then(function(hasPermission) {
					$tw.wiki.setText(tempTitle, "needs_reauth", null, hasPermission ? "no" : "yes");
				}).catch(function(err) {
					console.error("Permission check failed for " + uri + ":", err);
					$tw.wiki.setText(tempTitle, "needs_reauth", null, "yes");
				});
			} else {
				// Not a content:// URI, no permission check needed
				$tw.wiki.setText(tempTitle, "needs_reauth", null, "no");
			}
		});
	}

	// ========================================
	// Message Handlers
	// ========================================

	// Message handler: open wiki file dialog
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-open-wiki", function(event) {
		if (isAndroid) {
			// Android: Use SAF file picker
			invoke("android_pick_wiki_file").then(function(uri) {
				if (uri) {
					invoke("open_wiki_window", { path: uri }).then(function(entry) {
						addToWikiList(entry);
						refreshWikiList();
					}).catch(function(err) {
						console.error("open_wiki_window error:", err);
					});
				}
			}).catch(function(err) {
				console.error("android_pick_wiki_file error:", err);
			});
		} else {
			// Desktop: Use native file dialog
			openDialog({
				multiple: false,
				filters: [{
					name: "TiddlyWiki",
					extensions: ["html", "htm"]
				}]
			}).then(function(file) {
				if (file) {
					invoke("open_wiki_window", { path: file }).then(function(entry) {
						addToWikiList(entry);
						refreshWikiList();
					}).catch(function(err) {
						console.error("open_wiki_window error:", err);
					});
				}
			}).catch(function(err) {
				console.error("openDialog error:", err);
			});
		}
	});

	// Message handler: open wiki folder dialog
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-open-folder", function(event) {
		if (isAndroid) {
			// Android: Use SAF directory picker, then check folder status
			invoke("android_pick_folder_for_wiki_creation").then(function(result) {
				if (result) {
					var uri = result[0];
					var isWiki = result[1];
					var isEmpty = result[2];
					var folderName = result[3];

					if (isWiki) {
						// Already a wiki folder, open it directly
						invoke("open_wiki_folder", { path: uri }).then(function(entry) {
							addToWikiList(entry);
							refreshWikiList();
						}).catch(function(err) {
							console.error("open_wiki_folder error:", err);
							alert("Failed to open wiki folder: " + err);
						});
					} else {
						// Not a wiki folder, show edition selection
						showEditionSelector(uri, { name: folderName, is_empty: isEmpty });
					}
				}
			}).catch(function(err) {
				console.error("android_pick_folder_for_wiki_creation error:", err);
				alert("Failed to pick folder: " + err);
			});
			return;
		}
		openDialog({
			directory: true,
			multiple: false
		}).then(function(folder) {
			if (folder) {
				// Check folder status first
				invoke("check_folder_status", { path: folder }).then(function(status) {
					if (status.is_wiki) {
						// Already a wiki folder, open it directly
						invoke("open_wiki_folder", { path: folder }).then(function(entry) {
							addToWikiList(entry);
							refreshWikiList();
						}).catch(function(err) {
							console.error("open_wiki_folder error:", err);
							alert("Failed to open wiki folder: " + err);
						});
					} else {
						// Not a wiki folder, show edition selection
						showEditionSelector(folder, status);
					}
				}).catch(function(err) {
					console.error("check_folder_status error:", err);
					alert("Failed to check folder: " + err);
				});
			}
		}).catch(function(err) {
			console.error("openDialog error:", err);
		});
	});

	// Function to show edition selector for initializing a new wiki folder
	function showEditionSelector(folderPath, folderStatus) {
		// Store the folder path for later use
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-folder-path", "text", null, folderPath);
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-folder-name", "text", null, folderStatus.name);
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-folder-empty", "text", null, folderStatus.is_empty ? "yes" : "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/selected-edition", "text", null, "empty");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/selected-plugins", "text", null, "");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/create-mode", "text", null, "folder");

		loadEditionsAndPlugins();
	}

	// Function to show edition selector for creating a new wiki file
	function showFileCreator(filePath) {
		// Store the file path for later use
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-file-path", "text", null, filePath);
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-file-path-display", "text", null, getDisplayPath(filePath));
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/selected-edition", "text", null, "empty");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/selected-plugins", "text", null, "");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/create-mode", "text", null, "file");

		loadEditionsAndPlugins();
	}

	// Function to load editions and plugins and show the modal
	function loadEditionsAndPlugins() {
		// Load available editions and plugins in parallel
		Promise.all([
			invoke("get_available_editions"),
			invoke("get_available_plugins")
		]).then(function(results) {
			var editions = results[0];
			var plugins = results[1];


			// Clear existing entries
			$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/editions/]]").forEach(function(title) {
				$tw.wiki.deleteTiddler(title);
			});
			$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/plugins/]]").forEach(function(title) {
				$tw.wiki.deleteTiddler(title);
			});

			// Add edition entries
			editions.forEach(function(edition, index) {
				$tw.wiki.addTiddler({
					title: "$:/temp/tiddlydesktop-rs/editions/" + index,
					id: edition.id,
					name: edition.name,
					description: edition.description,
					text: ""
				});
			});

			// Add plugin entries
			plugins.forEach(function(plugin, index) {
				$tw.wiki.addTiddler({
					title: "$:/temp/tiddlydesktop-rs/plugins/" + plugin.id,
					id: plugin.id,
					name: plugin.name,
					description: plugin.description,
					category: plugin.category,
					selected: "no",
					text: ""
				});
			});

			// Show the edition selector modal
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-edition-selector", "text", null, "yes");
		}).catch(function(err) {
			console.error("Failed to load editions/plugins:", err);
			alert("Failed to load editions: " + err);
		});
	}

	// Message handler: create new wiki file (shows save dialog then edition selector)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-create-wiki", function(event) {
		if (isAndroid) {
			// Android: Use SAF save dialog
			invoke("android_create_wiki_file", { suggestedName: "wiki.html" }).then(function(uri) {
				if (uri) {
					showFileCreator(uri);
				}
			}).catch(function(err) {
				console.error("android_create_wiki_file error:", err);
			});
		} else {
			// Desktop: Use native save dialog
			var saveDialog = window.__TAURI__.dialog.save;
			saveDialog({
				filters: [{
					name: "TiddlyWiki",
					extensions: ["html"]
				}],
				defaultPath: "wiki.html"
			}).then(function(filePath) {
				if (filePath) {
					showFileCreator(filePath);
				}
			}).catch(function(err) {
				console.error("Save dialog error:", err);
			});
		}
	});

	// Message handler: create wiki file with selected edition and plugins
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-create-file", function(event) {
		var filePath = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/init-file-path");
		var editionId = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/selected-edition") || "empty";

		// Collect selected plugins
		var selectedPlugins = [];
		$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/plugins/]]").forEach(function(title) {
			var tiddler = $tw.wiki.getTiddler(title);
			if (tiddler && tiddler.fields.selected === "yes" && tiddler.fields.id) {
				selectedPlugins.push(tiddler.fields.id);
			}
		});


		if (!filePath) {
			alert("Missing file path");
			return;
		}

		// Hide the selector and show loading state
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-edition-selector", "text", null, "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "yes");

		invoke("create_wiki_file", { path: filePath, edition: editionId, plugins: selectedPlugins }).then(function() {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "no");

			// Open the newly created wiki file
			invoke("open_wiki_window", { path: filePath }).then(function(entry) {
				addToWikiList(entry);
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to open created wiki:", err);
				alert("Wiki created but failed to open: " + err);
			});
		}).catch(function(err) {
			console.error("create_wiki_file error:", err);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "no");
			alert("Failed to create wiki file: " + err);
		});
	});

	// Message handler: toggle plugin selection
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-toggle-plugin", function(event) {
		var pluginId = event.param || (event.paramObject && event.paramObject.plugin);
		if (pluginId) {
			var tiddler = $tw.wiki.getTiddler("$:/temp/tiddlydesktop-rs/plugins/" + pluginId);
			if (tiddler) {
				var isSelected = tiddler.fields.selected === "yes";
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/plugins/" + pluginId, "selected", null, isSelected ? "no" : "yes");
			}
		}
	});

	// Message handler: initialize wiki folder with selected edition and plugins
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-init-folder", function(event) {
		var editionId = event.param || (event.paramObject && event.paramObject.edition);
		var folderPath = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/init-folder-path");

		// If no edition passed, use the selected one
		if (!editionId) {
			editionId = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/selected-edition") || "server";
		}

		// Collect selected plugins
		var selectedPlugins = [];
		var allPluginTiddlers = $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/plugins/]]");

		allPluginTiddlers.forEach(function(title) {
			var tiddler = $tw.wiki.getTiddler(title);
			if (tiddler && tiddler.fields.selected === "yes" && tiddler.fields.id) {
				selectedPlugins.push(tiddler.fields.id);
			}
		});


		if (!folderPath || !editionId) {
			alert("Missing folder path or edition");
			return;
		}

		// Hide the selector and show loading state
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-edition-selector", "text", null, "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "yes");

		invoke("init_wiki_folder", { path: folderPath, edition: editionId, plugins: selectedPlugins }).then(function(entry) {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "no");

			// On Android, init_wiki_folder also opens the wiki and returns the entry
			// On desktop, we need to call open_wiki_folder separately
			if (isAndroid) {
				// Entry returned from init_wiki_folder on Android
				if (entry) {
					addToWikiList(entry);
					refreshWikiList();
				}
			} else {
				// Desktop: open the newly initialized folder
				invoke("open_wiki_folder", { path: folderPath }).then(function(entry) {
					addToWikiList(entry);
					refreshWikiList();
				}).catch(function(err) {
					console.error("Failed to open initialized folder:", err);
					alert("Wiki initialized but failed to open: " + err);
				});
			}
		}).catch(function(err) {
			console.error("init_wiki_folder error:", err);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "no");
			alert("Failed to initialize wiki folder: " + err);
		});
	});

	// Message handler: cancel edition selection
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-cancel-init", function(event) {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-edition-selector", "text", null, "no");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/init-folder-path");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/init-folder-name");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/init-file-path");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/create-mode");
	});

	// Message handler: show plugin installer modal for a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-show-plugin-installer", function(event) {
		var path = event.paramObject && event.paramObject.path;
		var isFolder = event.paramObject && event.paramObject.isFolder === "true";
		var filename = event.paramObject && event.paramObject.filename;
		if (!path) return;

		// Store wiki info in temp tiddlers
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/plugin-install-wiki-path", "text", null, path);
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/plugin-install-wiki-name", "text", null, filename || path);
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/plugin-install-is-folder", "text", null, isFolder ? "true" : "false");

		// Load available plugins and installed plugins in parallel
		Promise.all([
			invoke("get_available_plugins"),
			invoke("get_wiki_installed_plugins", { path: path, isFolder: isFolder })
		]).then(function(results) {
			var availablePlugins = results[0];
			var installedPlugins = results[1];

			// Clear existing install-plugins entries
			$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/install-plugins/]]").forEach(function(title) {
				$tw.wiki.deleteTiddler(title);
			});

			// Add plugin entries
			availablePlugins.forEach(function(plugin) {
				var isInstalled = installedPlugins.indexOf(plugin.id) !== -1 ||
					installedPlugins.indexOf("tiddlywiki/" + plugin.id) !== -1;
				// Also check if the short name matches (e.g. "markdown" vs "tiddlywiki/markdown")
				var shortId = plugin.id.indexOf("/") !== -1 ? plugin.id.split("/").pop() : plugin.id;
				if (!isInstalled) {
					isInstalled = installedPlugins.some(function(ip) {
						var ipShort = ip.indexOf("/") !== -1 ? ip.split("/").pop() : ip;
						return ipShort === shortId;
					});
				}
				$tw.wiki.addTiddler({
					title: "$:/temp/tiddlydesktop-rs/install-plugins/" + plugin.id,
					id: plugin.id,
					name: plugin.name,
					description: plugin.description,
					category: plugin.category,
					selected: isInstalled ? "yes" : "no",
					installed: isInstalled ? "yes" : "no",
					text: ""
				});
			});

			// Show the modal
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-plugin-installer", "text", null, "yes");
		}).catch(function(err) {
			console.error("Failed to load plugins:", err);
			alert("Failed to load plugins: " + err);
		});
	});

	// Message handler: apply plugin changes to a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-do-install-plugins", function(event) {
		var path = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/plugin-install-wiki-path");
		var isFolder = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/plugin-install-is-folder") === "true";
		if (!path) return;

		// Collect all selected plugins
		var selectedPlugins = [];
		$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/install-plugins/]]").forEach(function(title) {
			var tiddler = $tw.wiki.getTiddler(title);
			if (tiddler && tiddler.fields.selected === "yes" && tiddler.fields.id) {
				selectedPlugins.push(tiddler.fields.id);
			}
		});

		// Hide modal, show loading spinner
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-plugin-installer", "text", null, "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/plugin-install-loading", "text", null, "yes");

		invoke("install_plugins_to_wiki", { path: path, isFolder: isFolder, plugins: selectedPlugins }).then(function() {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/plugin-install-loading", "text", null, "no");
			// Clean up temp tiddlers
			$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/install-plugins/]]").forEach(function(title) {
				$tw.wiki.deleteTiddler(title);
			});
			$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/plugin-install-wiki-path");
			$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/plugin-install-wiki-name");
			$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/plugin-install-is-folder");
		}).catch(function(err) {
			console.error("install_plugins_to_wiki error:", err);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/plugin-install-loading", "text", null, "no");
			alert("Failed to update plugins: " + err);
		});
	});

	// Message handler: cancel plugin installer
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-cancel-plugin-install", function(event) {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-plugin-installer", "text", null, "no");
		// Clean up temp tiddlers
		$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/install-plugins/]]").forEach(function(title) {
			$tw.wiki.deleteTiddler(title);
		});
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/plugin-install-wiki-path");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/plugin-install-wiki-name");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/plugin-install-is-folder");
	});

	// Message handler: open a specific wiki path (auto-detect file vs folder)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-open-path", function(event) {
		var path = event.param || event.paramObject.path;
		var isFolder = event.paramObject && event.paramObject.isFolder === "true";
		if (path) {
			// Look up the entry to get backup settings
			var entries = getWikiListEntries();
			var entry = null;
			for (var i = 0; i < entries.length; i++) {
				if (entries[i].path === path) {
					entry = entries[i];
					break;
				}
			}

			var command = isFolder ? "open_wiki_folder" : "open_wiki_window";
			var params = { path: path };

			// Pass backup settings if we have them (single-file wikis only)
			if (!isFolder && entry) {
				params.backupsEnabled = entry.backups_enabled !== false; // Default true
				params.backupCount = entry.backup_count !== undefined ? entry.backup_count : null;
			}

			invoke(command, params).then(function(resultEntry) {
				addToWikiList(resultEntry);
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to open wiki:", err);
				alert("Failed to open: " + err);
			});
		}
	});

	// Message handler: reveal wiki in file manager
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-reveal", function(event) {
		var path = event.param || event.paramObject.path;
		if (path) {
			invoke("reveal_in_folder", { path: path }).catch(function(err) {
				console.error("Failed to reveal:", err);
			});
		}
	});

	// Message handler: remove wiki from recent list
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-remove", function(event) {
		var path = event.param || event.paramObject.path;
		if (path) {
			removeFromWikiList(path);
			refreshWikiList();
		}
	});

	// Message handler: re-authorize a wiki (Android only - permission expired)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-reauthorize", function(event) {
		var oldPath = event.param || (event.paramObject && event.paramObject.path);
		var isFolder = event.paramObject && event.paramObject.isFolder === "true";

		if (!oldPath || !isAndroid) return;

		// Show file picker to re-select the file
		var pickPromise = isFolder
			? invoke("android_pick_directory")
			: invoke("android_pick_wiki_file");

		pickPromise.then(function(newUri) {
			if (!newUri) return; // User cancelled

			// Update the wiki list entry with the new URI
			var entries = getWikiListEntries();
			var entryIndex = -1;

			for (var i = 0; i < entries.length; i++) {
				if (entries[i].path === oldPath) {
					entryIndex = i;
					break;
				}
			}

			if (entryIndex >= 0) {
				// Update the path but preserve all other settings
				entries[entryIndex].path = newUri;
				saveWikiList(entries);
				refreshWikiList();
				console.log("[TiddlyDesktop] Re-authorized wiki: " + oldPath + " -> " + newUri);
			}
		}).catch(function(err) {
			console.error("Re-authorization failed:", err);
			alert("Failed to re-authorize: " + err);
		});
	});

	// Message handler: toggle backups for a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-backups", function(event) {
		var path = event.paramObject && event.paramObject.path;
		var enabled = event.paramObject && event.paramObject.enabled === "true";
		if (path) {
			// Update the Rust backend
			invoke("set_wiki_backups", { path: path, enabled: enabled }).then(function() {
				// Update the local wiki list entry
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						entries[i].backups_enabled = enabled;
						// Directly update the temp tiddler field for immediate UI update
						var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
						$tw.wiki.setText(tempTitle, "backups_enabled", null, enabled ? "true" : "false");
						break;
					}
				}
				saveWikiList(entries);
			}).catch(function(err) {
				console.error("Failed to set backups:", err);
			});
		}
	});

	// Message handler: toggle LAN sync for a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-wiki-sync", function(event) {
		var path = event.paramObject && event.paramObject.path;
		var enabled = event.paramObject && event.paramObject.enabled === "true";
		if(enabled && !checkTermsAccepted()) return;
		if (path) {
			invoke("set_wiki_sync", { path: path, enabled: enabled }).then(function(syncId) {
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						entries[i].sync_enabled = enabled;
						if (syncId) entries[i].sync_id = syncId;
						var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
						$tw.wiki.setText(tempTitle, "sync_enabled", null, enabled ? "true" : "false");
						$tw.wiki.setText(tempTitle, "sync_id", null, syncId || "");
						break;
					}
				}
				saveWikiList(entries);
			}).catch(function(err) {
				console.error("Failed to set wiki sync:", err);
			});
		}
	});

	// Message handler: set custom backup directory for a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-backup-dir", function(event) {
		var path = event.paramObject && event.paramObject.path;
		if (path) {
			if (isAndroid) {
				// Android: Use SAF directory picker for backup directory
				invoke("android_pick_backup_directory").then(function(folder) {
					if (folder) {
						// Update the Rust backend
						invoke("set_wiki_backup_dir", { path: path, backupDir: folder }).then(function() {
							// Update the local wiki list entry
							var entries = getWikiListEntries();
							for (var i = 0; i < entries.length; i++) {
								if (entries[i].path === path) {
									entries[i].backup_dir = folder;
									// Directly update the temp tiddler field for immediate UI update
									var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
									$tw.wiki.setText(tempTitle, "backup_dir", null, folder);
									$tw.wiki.setText(tempTitle, "backup_dir_display", null, getDisplayPath(folder));
									break;
								}
							}
							saveWikiList(entries);
						}).catch(function(err) {
							console.error("Failed to set backup directory:", err);
							alert("Failed to set backup directory: " + err);
						});
					}
				}).catch(function(err) {
					console.error("android_pick_backup_directory error:", err);
					alert("Failed to pick backup directory: " + err);
				});
				return;
			}
			// Desktop: Open folder picker dialog
			openDialog({
				directory: true,
				multiple: false,
				title: "Select Backup Directory"
			}).then(function(folder) {
				if (folder) {
					// Update the Rust backend
					invoke("set_wiki_backup_dir", { path: path, backupDir: folder }).then(function() {
						// Update the local wiki list entry
						var entries = getWikiListEntries();
						for (var i = 0; i < entries.length; i++) {
							if (entries[i].path === path) {
								entries[i].backup_dir = folder;
								// Directly update the temp tiddler field for immediate UI update
								var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
								$tw.wiki.setText(tempTitle, "backup_dir", null, folder);
								$tw.wiki.setText(tempTitle, "backup_dir_display", null, getDisplayPath(folder));
								break;
							}
						}
						saveWikiList(entries);
					}).catch(function(err) {
						console.error("Failed to set backup directory:", err);
					});
				}
			}).catch(function(err) {
				console.error("openDialog error:", err);
			});
		}
	});

	// Message handler: clear/reset backup directory to default
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-clear-backup-dir", function(event) {
		var path = event.paramObject && event.paramObject.path;
		if (path) {
			// Update the Rust backend with null to reset to default
			invoke("set_wiki_backup_dir", { path: path, backupDir: null }).then(function() {
				// Update the local wiki list entry
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						delete entries[i].backup_dir;
						// Directly update the temp tiddler field for immediate UI update
						var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
						$tw.wiki.setText(tempTitle, "backup_dir", null, "");
						$tw.wiki.setText(tempTitle, "backup_dir_display", null, "");
						break;
					}
				}
				saveWikiList(entries);
			}).catch(function(err) {
				console.error("Failed to clear backup directory:", err);
			});
		}
	});

	// Message handler: set backup count for a wiki (max backups to keep)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-backup-count", function(event) {
		var path = event.paramObject && event.paramObject.path;
		var countStr = event.paramObject && event.paramObject.count;
		if (path) {
			// Parse count: empty string or null = default (20), "0" = unlimited, number = that count
			var count = countStr === "" || countStr === null || countStr === undefined ? null : parseInt(countStr, 10);
			if (count !== null && isNaN(count)) count = null;

			// Update the Rust backend
			invoke("set_wiki_backup_count", { path: path, count: count }).then(function() {
				// Update the local wiki list entry
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						if (count === null) {
							delete entries[i].backup_count;
						} else {
							entries[i].backup_count = count;
						}
						// Directly update the temp tiddler field for immediate UI update
						var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
						$tw.wiki.setText(tempTitle, "backup_count", null, count !== null ? String(count) : "");
						break;
					}
				}
				saveWikiList(entries);
			}).catch(function(err) {
				console.error("Failed to set backup count:", err);
			});
		}
	});

	// Message handler: set group for a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-group", function(event) {
		var path = event.paramObject && event.paramObject.path;
		var group = event.paramObject && event.paramObject.group;
		if (path) {
			// Treat empty string as null (ungrouped)
			var groupValue = group && group.trim() ? group.trim() : null;
			// Update the Rust backend
			invoke("set_wiki_group", { path: path, group: groupValue }).then(function() {
				// Update the local wiki list entry
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						if (groupValue) {
							entries[i].group = groupValue;
						} else {
							delete entries[i].group;
						}
						break;
					}
				}
				saveWikiList(entries);
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to set wiki group:", err);
			});
		}
	});

	// Message handler: rename a group
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-rename-group", function(event) {
		var oldName = event.paramObject && event.paramObject.oldName;
		var newName = event.paramObject && event.paramObject.newName;
		if (oldName && newName && newName.trim()) {
			invoke("rename_wiki_group", { oldName: oldName, newName: newName.trim() }).then(function() {
				// Update local entries
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].group === oldName) {
						entries[i].group = newName.trim();
					}
				}
				// Transfer collapsed state to the new group name
				var collapsed = getCollapsedGroups();
				var idx = collapsed.indexOf(oldName);
				if (idx !== -1) {
					collapsed[idx] = newName.trim();
					saveCollapsedGroups(collapsed);
				}
				saveWikiList(entries);
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to rename group:", err);
			});
		}
	});

	// Message handler: delete a group (moves wikis to ungrouped)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-delete-group", function(event) {
		var groupName = event.param || (event.paramObject && event.paramObject.group);
		if (groupName) {
			invoke("delete_wiki_group", { groupName: groupName }).then(function() {
				// Update local entries
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].group === groupName) {
						delete entries[i].group;
					}
				}
				// Remove from collapsed groups
				var collapsed = getCollapsedGroups();
				collapsed = collapsed.filter(function(g) { return g !== groupName; });
				saveCollapsedGroups(collapsed);
				saveWikiList(entries);
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to delete group:", err);
			});
		}
	});

	// Message handler: toggle group collapse state (persistent)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-toggle-group", function(event) {
		var groupName = event.param || (event.paramObject && event.paramObject.group);
		var key = groupName || "Ungrouped";
		var stateTitle = "$:/state/tiddlydesktop-rs/group-collapsed/" + key;
		var currentState = $tw.wiki.getTiddlerText(stateTitle);
		var newState = currentState === "yes" ? "no" : "yes";
		// Update ephemeral state for instant UI response
		$tw.wiki.setText(stateTitle, "text", null, newState);
		// Persist collapsed groups to a saved tiddler
		var collapsed = getCollapsedGroups();
		if (newState === "yes") {
			if (collapsed.indexOf(key) === -1) collapsed.push(key);
		} else {
			collapsed = collapsed.filter(function(g) { return g !== key; });
		}
		saveCollapsedGroups(collapsed);
	});

	// Message handler: show group manager modal
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-manage-groups", function(event) {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-group-manager", "text", null, "yes");
	});

	// Message handler: close group manager modal
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-close-group-manager", function(event) {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-group-manager", "text", null, "no");
	});

	// Set up drag-drop listeners
	// Track recently processed drops to prevent duplicate opens if both events fire
	var recentDrops = {};
	var DROP_DEDUPE_MS = 500;

	// Handle dropped files/folders by opening them as wikis
	function handleDroppedPaths(paths) {
		if (!paths || paths.length === 0) return;
		var now = Date.now();
		paths.forEach(function(path) {
			// Deduplicate: skip if this path was processed recently
			if (recentDrops[path] && (now - recentDrops[path]) < DROP_DEDUPE_MS) {
				return;
			}
			recentDrops[path] = now;

			// Check if it's an HTML file or potentially a folder
			if (path.endsWith(".html") || path.endsWith(".htm")) {
				invoke("open_wiki_window", { path: path }).then(function(entry) {
					addToWikiList(entry);
					refreshWikiList();
				});
			} else {
				// Try to open as a folder - backend will verify if it's a valid wiki folder
				invoke("check_is_wiki_folder", { path: path }).then(function(isFolder) {
					if (isFolder) {
						invoke("open_wiki_folder", { path: path }).then(function(entry) {
							addToWikiList(entry);
							refreshWikiList();
						}).catch(function(err) {
							console.error("Failed to open wiki folder:", err);
						});
					}
				});
			}
		});
		// Clean up old entries periodically
		setTimeout(function() {
			var cutoff = Date.now() - DROP_DEDUPE_MS * 2;
			for (var p in recentDrops) {
				if (recentDrops[p] < cutoff) {
					delete recentDrops[p];
				}
			}
		}, DROP_DEDUPE_MS * 2);
	}

	// Listen for tauri://drag-drop (Windows, and Tauri's built-in drag-drop)
	listen("tauri://drag-drop", function(event) {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "no");
		handleDroppedPaths(event.payload && event.payload.paths);
	});

	// Listen for td-file-drop (Linux/macOS custom GTK/Cocoa drag-drop)
	listen("td-file-drop", function(event) {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "no");
		handleDroppedPaths(event.payload && event.payload.paths);
	});

	// Visual feedback for drag-over state (Tauri built-in events - Windows)
	listen("tauri://drag-enter", function() {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "yes");
	});

	listen("tauri://drag-leave", function() {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "no");
	});

	// Visual feedback for drag-over state (custom GTK/Cocoa events - Linux/macOS)
	listen("td-drag-motion", function() {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "yes");
	});

	listen("td-drag-leave", function() {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "no");
	});

	// Fix for stale tc-dragover class when dragging between droppable widgets
	// The droppable widget's currentlyEntered tracking can get out of sync
	// Ensure only one droppable has the highlight at a time
	document.addEventListener("dragenter", function(event) {
		var target = event.target.closest(".tc-droppable");
		var elements = document.querySelectorAll(".tc-dragover");
		for(var i = 0; i < elements.length; i++) {
			if(elements[i] !== target) {
				elements[i].classList.remove("tc-dragover");
			}
		}
	});

	// Listen for wiki list changes (e.g., when a wiki is opened from file explorer)
	listen("wiki-list-changed", function(event) {
		if(event.payload) {
			addToWikiList(event.payload);
		}
		refreshWikiList();
	});

	// Listen for wiki process closed events to update open/closed state (desktop)
	listen("wiki-process-closed", function() {
		refreshWikiList();
	});

	// Expose a global function for Android to call from Kotlin when a WikiActivity closes.
	// MainActivity's BroadcastReceiver calls evaluateJavascript() with this.
	window.__tdWikiClosed = function(wikiPath) {
		var entries = getWikiListEntries();
		entries.forEach(function(entry, index) {
			if (entry.path === wikiPath) {
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/wikis/" + index, "is_open", null, "no");
			}
		});
	};

	// Listen for favicon updates from wiki windows (updates in real-time without full refresh)
	window.addEventListener("td-favicon-updated", function(event) {
		var path = event.detail && event.detail.path;
		var favicon = event.detail && event.detail.favicon;
		if (!path) return;

		// Update the persistent wiki list entry
		var entries = getWikiListEntries();
		var updated = false;
		for (var i = 0; i < entries.length; i++) {
			if (entries[i].path === path) {
				entries[i].favicon = favicon;
				// Also update the temp tiddler directly for immediate UI update
				var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
				$tw.wiki.setText(tempTitle, "favicon", null, favicon || "");
				updated = true;
				break;
			}
		}
		if (updated) {
			saveWikiList(entries);
		}
	});

	// Clean up stale $:/state/ and $:/temp/ tiddlers from previous sessions.
	// These get persisted into the wiki HTML on save and cause UI glitches
	// on next launch (e.g. popup dropdowns appearing open).
	// NOTE: Do NOT delete all $:/temp/tiddlydesktop-rs/ — that would wipe
	// is-mobile, is-android, relay-sync-url, etc. Only delete specific stale prefixes.
	$tw.wiki.each(function(tiddler, title) {
		if (title.indexOf("$:/state/relay-room-popup/") === 0 ||
			title.indexOf("$:/state/relay-room-details/") === 0 ||
			title.indexOf("$:/state/group-popup/") === 0 ||
			title.indexOf("$:/state/backup-count-popup/") === 0 ||
			title.indexOf("$:/state/link-wiki-popup") === 0 ||
			title.indexOf("$:/state/tiddlydesktop-rs/") === 0 ||
			title.indexOf("$:/temp/tiddlydesktop-rs/wikis/") === 0 ||
			title.indexOf("$:/temp/tiddlydesktop-rs/relay-room/") === 0 ||
			title.indexOf("$:/temp/tiddlydesktop-rs/relay-room-details/") === 0 ||
			title.indexOf("$:/temp/tiddlydesktop-rs/relay-room-password/") === 0 ||
			title.indexOf("$:/temp/tiddlydesktop-rs/relay-room-name-edit/") === 0 ||
			title.indexOf("$:/temp/tiddlydesktop-rs/relay-server-room/") === 0 ||
			title.indexOf("$:/temp/new-group-name/") === 0 ||
			title.indexOf("$:/temp/backup-count-input/") === 0) {
			$tw.wiki.deleteTiddler(title);
		}
	});

	// Initial load of wiki list from tiddler
	refreshWikiList();

	// If the WikiList tiddler is empty (e.g. migration reset, autosave didn't fire,
	// or HTML was rebuilt), restore entries from the Rust JSON config on disk.
	// Reconcile runs after the fallback resolves so it sees the restored entries.
	function reconcileStartupPaths() {
		var paths = getWikiListEntries().map(function(e) { return e.path; });
		invoke("reconcile_recent_files", { paths: paths }).then(function(removedCount) {
			if (removedCount > 0) {
				console.log("[TiddlyDesktop] Startup reconciliation removed " + removedCount + " stale entries from Rust config");
			}
		}).catch(function(err) {
			console.error("[TiddlyDesktop] Failed to reconcile on startup:", err);
		});
	}

	var initialEntries = getWikiListEntries();
	if (initialEntries.length === 0) {
		// WikiList tiddler is empty — try restoring from Rust JSON backup
		invoke("get_recent_files").then(function(jsonEntries) {
			if (jsonEntries && jsonEntries.length > 0) {
				console.log("[TiddlyDesktop] WikiList tiddler empty — restoring " +
					jsonEntries.length + " entries from recent_wikis.json");
				saveWikiList(jsonEntries);
				refreshWikiList();
			}
			reconcileStartupPaths();
		}).catch(function(err) {
			console.error("[TiddlyDesktop] Failed to load recent files for fallback:", err);
			reconcileStartupPaths();
		});
	} else {
		reconcileStartupPaths();
	}

	// On Android, merge favicons from disk files into wiki list entries.
	// WikiActivity saves favicons to files in the :wiki process, but can't send
	// Tauri events to the main process. So we load them via get_recent_files
	// (which reads favicon files on Android) and merge into the wiki list tiddler.
	function mergeFaviconsFromDisk() {
		if (!isAndroid) return;
		invoke("get_recent_files").then(function(rustEntries) {
			if (!rustEntries || !rustEntries.length) return;
			var faviconMap = {};
			rustEntries.forEach(function(e) {
				if (e.favicon) {
					faviconMap[e.path] = e.favicon;
				}
			});
			var entries = getWikiListEntries();
			var updated = false;
			entries.forEach(function(entry, index) {
				if (faviconMap[entry.path] && entry.favicon !== faviconMap[entry.path]) {
					entry.favicon = faviconMap[entry.path];
					var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + index;
					$tw.wiki.setText(tempTitle, "favicon", null, entry.favicon);
					updated = true;
				}
			});
			if (updated) {
				saveWikiList(entries);
				console.log("[TiddlyDesktop] Merged favicons from disk files");
			}
		}).catch(function(err) {
			console.log("[TiddlyDesktop] Failed to load favicons from Rust:", err);
		});
	}
	mergeFaviconsFromDisk();

	// Re-merge favicons when returning to the landing page (e.g., after closing a wiki).
	// WikiActivity extracts favicons in the :wiki process and saves them to disk,
	// so we need to re-check when the user comes back.
	if (isAndroid) {
		document.addEventListener("visibilitychange", function() {
			if (document.visibilityState === "visible") {
				mergeFaviconsFromDisk();
				checkPendingWikiOpen();
			}
		});
	}

	// Check if a pending wiki open was requested (from widget or Quick Capture).
	// MainActivity writes a pending file, and we consume it here.
	function checkPendingWikiOpen() {
		invoke("get_pending_widget_wiki").then(function(pendingWiki) {
			if (pendingWiki && pendingWiki.path) {
				console.log("[TiddlyDesktop] Opening pending wiki: " + pendingWiki.path);
				var path = pendingWiki.path;
				var isFolder = !!pendingWiki.is_folder;
				var navigateToTiddler = pendingWiki.navigate_to_tiddler || null;

				// Look up backup settings from the wiki list
				var entries = getWikiListEntries();
				var entry = null;
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						entry = entries[i];
						break;
					}
				}

				var command = isFolder ? "open_wiki_folder" : "open_wiki_window";
				var params = { path: path };
				if (!isFolder && entry) {
					params.backupsEnabled = entry.backups_enabled !== false;
					params.backupCount = entry.backup_count !== undefined ? entry.backup_count : null;
				}
				if (navigateToTiddler) {
					params.tiddlerTitle = navigateToTiddler;
				}

				invoke(command, params).then(function(resultEntry) {
					addToWikiList(resultEntry);
					refreshWikiList();
				}).catch(function(err) {
					console.error("[TiddlyDesktop] Failed to open pending wiki:", err);
				});
			}
		}).catch(function(err) {
			// No pending wiki — normal case
		});
	}
	if (isAndroid) {
		checkPendingWikiOpen();
	}

	// ========================================
	// Custom Plugin/Edition Path Handlers (Android only)
	// ========================================
	if (isAndroid) {
		// Load and display current custom paths on startup
		function refreshCustomPaths() {
			invoke("get_custom_plugin_path").then(function(uri) {
				if (uri) {
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-plugin-path", "text", null, uri);
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-plugin-path-display", "text", null, getDisplayPath(uri));
				} else {
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-plugin-path", "text", null, "");
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-plugin-path-display", "text", null, "");
				}
			}).catch(function() {});
			invoke("get_custom_edition_path").then(function(uri) {
				if (uri) {
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-edition-path", "text", null, uri);
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-edition-path-display", "text", null, getDisplayPath(uri));
				} else {
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-edition-path", "text", null, "");
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/custom-edition-path-display", "text", null, "");
				}
			}).catch(function() {});
		}
		refreshCustomPaths();

		// Message handler: set custom plugin path via SAF folder picker
		$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-plugin-path", function(event) {
			invoke("android_pick_directory").then(function(uri) {
				if (uri) {
					invoke("set_custom_plugin_path", { uri: uri }).then(function() {
						refreshCustomPaths();
					}).catch(function(err) {
						console.error("Failed to set custom plugin path:", err);
						alert("Failed to set plugin folder: " + err);
					});
				}
			}).catch(function(err) {
				console.error("android_pick_directory error:", err);
			});
		});

		// Message handler: clear custom plugin path
		$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-clear-plugin-path", function(event) {
			invoke("set_custom_plugin_path", { uri: "" }).then(function() {
				refreshCustomPaths();
			}).catch(function(err) {
				console.error("Failed to clear custom plugin path:", err);
			});
		});

		// Message handler: set custom edition path via SAF folder picker
		$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-edition-path", function(event) {
			invoke("android_pick_directory").then(function(uri) {
				if (uri) {
					invoke("set_custom_edition_path", { uri: uri }).then(function() {
						refreshCustomPaths();
					}).catch(function(err) {
						console.error("Failed to set custom edition path:", err);
						alert("Failed to set edition folder: " + err);
					});
				}
			}).catch(function(err) {
				console.error("android_pick_directory error:", err);
			});
		});

		// Message handler: clear custom edition path
		$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-clear-edition-path", function(event) {
			invoke("set_custom_edition_path", { uri: "" }).then(function() {
				refreshCustomPaths();
			}).catch(function(err) {
				console.error("Failed to clear custom edition path:", err);
			});
		});
	}

	// Check for application updates
	invoke("check_for_updates").then(function(result) {
		if (result.update_available && result.latest_version) {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/update-available", "text", null, "yes");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/latest-version", "text", null, result.latest_version);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/releases-url", "text", null, result.releases_url);
			console.log("Update available: v" + result.latest_version + " (current: v" + result.current_version + ")");
		} else {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/update-available", "text", null, "no");
			console.log("TiddlyDesktop-RS is up to date (v" + result.current_version + ")");
		}
	}).catch(function(err) {
		console.warn("Failed to check for updates:", err);
		// Don't show update button on error
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/update-available", "text", null, "no");
	});

	// ── Sync message handlers ─────────────────────────────────────────

	// Request a wiki from a peer
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-request-wiki", function(event) {
		var wikiId = event.paramObject ? event.paramObject.wikiId : "";
		var fromDeviceId = event.paramObject ? event.paramObject.fromDeviceId : "";
		var wikiName = event.paramObject ? event.paramObject.wikiName : "";
		var roomCode = event.paramObject ? event.paramObject.roomCode : "";
		if (!wikiId || !fromDeviceId) return;

		function doRequest(targetDir) {
			if (!targetDir) return; // User cancelled
			invoke("lan_sync_request_wiki", {
				wikiId: wikiId,
				fromDeviceId: fromDeviceId,
				targetDir: targetDir,
				roomCode: roomCode || null
			}).then(function() {
				console.log("[LAN Sync] Requested wiki " + wikiName + " from peer");
			}).catch(function(err) {
				console.error("Failed to request wiki:", err);
				alert("Failed to request wiki: " + err);
			});
		}

		// Open folder picker for target directory
		if (isAndroid) {
			invoke("android_pick_directory").then(function(targetDir) {
				doRequest(targetDir);
			}).catch(function(err) {
				console.error("Failed to open folder picker:", err);
			});
		} else {
			openDialog({
				directory: true,
				multiple: false,
				title: "Choose save location for " + wikiName
			}).then(function(targetDir) {
				doRequest(targetDir);
			}).catch(function(err) {
				console.error("Failed to open folder picker:", err);
			});
		}
	});

	// Populate candidate tiddlers for the link-wiki dropdown
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-prepare-link-wiki", function() {
		// Clear old candidates
		$tw.wiki.each(function(t, title) {
			if (title.indexOf("$:/temp/tiddlydesktop-rs/link-candidate/") === 0) {
				$tw.wiki.deleteTiddler(title);
			}
		});

		// Load from Rust backend (source of truth) to ensure paths match recent_files.json
		invoke("get_recent_files").then(function(rustEntries) {
			var candidates = (rustEntries || []).filter(function(e) { return !e.sync_enabled; });
			candidates.forEach(function(e, i) {
				$tw.wiki.addTiddler({
					title: "$:/temp/tiddlydesktop-rs/link-candidate/" + i,
					"candidate-path": e.path,
					text: e.filename + (e.display_path ? " (" + e.display_path + ")" : "")
				});
			});
		}).catch(function(err) {
			console.error("[LAN Sync] Failed to load recent files for link candidates:", err);
		});
	});

	// Link a remote wiki to an existing local wiki (called from dropdown selection)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-do-link-wiki", function(event) {
		var wikiId = event.paramObject ? event.paramObject.wikiId : "";
		var path = event.paramObject ? event.paramObject.path : "";
		var fromDeviceId = event.paramObject ? event.paramObject.fromDeviceId : "";
		var roomCode = event.paramObject ? event.paramObject.roomCode : "";
		if (!wikiId || !path) {
			console.error("[LAN Sync] do-link-wiki: missing wikiId=" + wikiId + " or path=" + path);
			return;
		}

		invoke("lan_sync_link_wiki", { path: path, syncId: wikiId, fromDeviceId: fromDeviceId || null, roomCode: roomCode || null }).then(function(resolvedRoom) {
			console.log("[LAN Sync] Linked wiki to sync ID " + wikiId + " (room: " + resolvedRoom + ")");
			try {
				// Update the wiki list tiddler to reflect the new sync state
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						entries[i].sync_enabled = true;
						entries[i].sync_id = wikiId;
						if (resolvedRoom) {
							entries[i].relay_room = resolvedRoom;
						}
						break;
					}
				}
				saveWikiList(entries);
				refreshWikiList();
			} catch(e) {
				console.error("[LAN Sync] Error updating wiki list after link:", e);
			}
			invoke("lan_sync_get_available_wikis").then(function(wikis) {
				refreshRemoteWikis(wikis);
			}).catch(function() {});
			invoke("lan_sync_broadcast_manifest").catch(function() {});
		}).catch(function(err) {
			console.error("Failed to link wiki:", err);
			alert("Failed to link wiki: " + err);
		});
	});

	// ── Relay Sync message handlers ─────────────────────────────────────

	// Sign in — fetch providers, then start auth flow
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-signin", function(event) {
		if(!checkTermsAccepted()) return;
		invoke("relay_sync_fetch_providers").then(function(providers) {
			if (!providers || providers.length === 0) {
				console.error("[Relay] No auth providers available");
				return;
			}
			if (providers.length === 1) {
				// Only one provider — go directly to auth
				startLoginForProvider(providers[0]);
			} else {
				// Multiple providers — show picker
				showProviderPicker(providers);
			}
		}).catch(function(err) {
			console.error("[Relay] Failed to fetch providers:", err);
		});
	});

	// Login with a specific provider (from picker or direct)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-login", function(event) {
		var p = event.paramObject || {};
		if (!p.provider || !p.clientId) return;
		startLoginForProvider({
			name: p.provider,
			client_id: p.clientId,
			url: p.authUrl || null,
			discovery_url: p.discoveryUrl || null,
			display_name: p.displayName || p.provider,
			scope: p.scope || null
		});
	});

	function startLoginForProvider(provider) {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-status", "text", null, "logging-in");
		// Also set legacy tiddler
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-auth-status", "text", null, "logging-in");

		// Resolve auth URL for the provider
		var authUrl = null;
		if (provider.name === "github") {
			authUrl = "https://github.com/login/oauth/authorize";
		} else if (provider.name === "gitlab") {
			authUrl = (provider.url || "https://gitlab.com") + "/oauth/authorize";
		}
		// For OIDC, discovery_url is used instead

		invoke("relay_sync_login", {
			provider: provider.name,
			clientId: provider.client_id,
			authUrl: authUrl,
			discoveryUrl: provider.discovery_url || null,
			scope: provider.scope || null
		}).then(function(result) {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-username", "text", null, result.username || "");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-provider", "text", null, result.provider || "");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-status", "text", null, "authenticated");
			// Legacy tiddlers
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-login", "text", null, result.username || "");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-auth-status", "text", null, "authenticated");
			refreshSyncStatus();
			fetchServerRooms();
		}).catch(function(err) {
			console.error("[Relay] Login failed:", err);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-status", "text", null, "");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-auth-status", "text", null, "");
		});
	}

	function showProviderPicker(providers) {
		// Store providers as tiddlers for the UI to enumerate
		var providerTitles = [];
		providers.forEach(function(prov) {
			var title = "$:/temp/tiddlydesktop-rs/relay-provider/" + prov.name;
			providerTitles.push(title);
			$tw.wiki.addTiddler(new $tw.Tiddler({
				title: title,
				name: prov.name,
				client_id: prov.client_id,
				url: prov.url || "",
				discovery_url: prov.discovery_url || "",
				display_name: prov.display_name || {"github":"GitHub","gitlab":"GitLab","oidc":"SSO"}[prov.name] || prov.name,
				auth_url: "",
				scope: ""
			}));
		});
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-provider-list", "text", null, providerTitles.join(" "));
		// Show the provider picker inline
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-status", "text", null, "picking");
	}

	// Fetch and display server rooms
	function fetchServerRooms() {
		invoke("relay_sync_list_server_rooms").then(function(serverRooms) {
			updateServerRoomList(serverRooms);
		}).catch(function(err) {
			console.error("[Relay] Failed to fetch server rooms:", err);
		});
	}

	function updateServerRoomList(serverRooms) {
		var titles = [];
		var currentTitles = {};
		(serverRooms || []).forEach(function(sr) {
			var title = "$:/temp/tiddlydesktop-rs/relay-server-room/" + sr.room_hash;
			titles.push(title);
			currentTitles[title] = true;
			$tw.wiki.addTiddler(new $tw.Tiddler({
				title: title,
				room_hash: sr.room_hash,
				role: sr.role,
				member_count: "" + (sr.member_count || 0),
				owner_username: sr.owner_username || "",
				local_room_code: sr.local_room_code || "",
				local_room_name: sr.local_room_name || "",
				decrypted_room_code: sr.decrypted_room_code || "",
				decrypted_password: sr.decrypted_password || ""
			}));
		});
		// Clean stale tiddlers
		$tw.wiki.each(function(tiddler, title) {
			if(title.indexOf("$:/temp/tiddlydesktop-rs/relay-server-room/") === 0 && !currentTitles[title]) {
				$tw.wiki.deleteTiddler(title);
			}
		});
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-server-room-list", "text", null, titles.join(" "));
	}

	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-fetch-server-rooms", function() {
		fetchServerRooms();
	});

	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-delete-server-room", function(event) {
		var roomHash = event.paramObject && event.paramObject.roomHash;
		if(!roomHash) return;
		if(!confirm($tw.wiki.renderText("text/plain", "text/vnd.tiddlywiki", "<<td-lingo RelaySync/ConfirmDeleteServerRoom>>") || "Are you sure you want to delete this room from the server? This cannot be undone.")) return;
		invoke("relay_sync_delete_server_room_by_hash", { roomHash: roomHash }).then(function() {
			// Immediately remove the tiddler from UI (don't wait for re-fetch)
			var roomTitle = "$:/temp/tiddlydesktop-rs/relay-server-room/" + roomHash;
			$tw.wiki.deleteTiddler(roomTitle);
			var currentList = ($tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/relay-server-room-list") || "").split(" ").filter(function(t) { return t && t !== roomTitle; });
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-server-room-list", "text", null, currentList.join(" "));
			// Then re-fetch to get authoritative state
			fetchServerRooms();
		}).catch(function(err) {
			console.error("[Relay] Failed to delete server room:", err);
			alert("Failed to delete room: " + err);
		});
	});

	// Sign out
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-logout", function(event) {
		invoke("relay_sync_logout").then(function() {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-username", "text", null, "");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-provider", "text", null, "");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-status", "text", null, "");
			// Legacy tiddlers
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-login", "text", null, "");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-auth-status", "text", null, "");
			refreshSyncStatus();
		});
	});

	// Legacy message handlers for backward compat
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-github-login", function(event) {
		$tw.rootWidget.dispatchEvent({type: "tm-tiddlydesktop-rs-relay-signin"});
	});
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-github-logout", function(event) {
		$tw.rootWidget.dispatchEvent({type: "tm-tiddlydesktop-rs-relay-logout"});
	});

	// Register an existing local room on the relay server (enables relay sync for that room)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-register-room", function(event) {
		var p = event.paramObject || {};
		if (!p.roomCode || !p.name) return;
		var statusTiddler = "$:/temp/tiddlydesktop-rs/relay-room-register-status/" + p.roomCode;
		$tw.wiki.setText(statusTiddler, "text", null, "registering");
		invoke("relay_sync_create_room", { name: p.name, roomCode: p.roomCode }).then(function(result) {
			console.log("[Relay] Room registered on server:", result.room_code);
			$tw.wiki.setText(statusTiddler, "text", null, "registered");
			refreshSyncStatus();
		}).catch(function(err) {
			var errStr = "" + err;
			if (errStr.indexOf("409") !== -1 || errStr.indexOf("already exists") !== -1) {
				console.log("[Relay] Room already registered on server:", p.roomCode);
				$tw.wiki.setText(statusTiddler, "text", null, "already-registered");
			} else {
				console.error("[Relay] Register room failed:", err);
				$tw.wiki.setText(statusTiddler, "text", null, "error");
			}
		});
	});

	// Add a member to a server room (also used for unblocking)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-add-member", function(event) {
		var p = event.paramObject || {};
		var memberName = p.username || p.githubLogin;
		if (!p.roomCode || !memberName) return;
		invoke("relay_sync_add_member", { roomCode: p.roomCode, username: memberName, provider: p.provider || null, userId: p.userId || null }).then(function() {
			// Refresh member list
			$tw.rootWidget.dispatchEvent({
				type: "tm-tiddlydesktop-rs-relay-load-members",
				paramObject: { roomCode: p.roomCode }
			});
		}).catch(function(err) {
			console.error("[Relay] Add member failed:", err);
		});
	});

	// Remove a member from a server room
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-remove-member", function(event) {
		var p = event.paramObject || {};
		var memberId = p.userId || p.githubLogin;
		if (!p.roomCode || !memberId) return;
		invoke("relay_sync_remove_member", { roomCode: p.roomCode, userId: memberId }).then(function() {
			// Refresh member list
			$tw.rootWidget.dispatchEvent({
				type: "tm-tiddlydesktop-rs-relay-load-members",
				paramObject: { roomCode: p.roomCode }
			});
		}).catch(function(err) {
			console.error("[Relay] Remove member failed:", err);
		});
	});

	// Load members of a server room
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-load-members", function(event) {
		var p = event.paramObject || {};
		if (!p.roomCode) return;
		invoke("relay_sync_list_members", { roomCode: p.roomCode }).then(function(members) {
			var tiddler = "$:/temp/tiddlydesktop-rs/relay-room-members/" + p.roomCode;
			$tw.wiki.setText(tiddler, "text", null, JSON.stringify(members));
			$tw.wiki.setText(tiddler, "type", null, "application/json");
		}).catch(function(err) {
			console.error("[Relay] Load members failed:", err);
		});
	});

	// Generate a unique speakable room name that doesn't conflict with existing rooms
	function generateUniqueRoomName() {
		var adjectives = [
			"Amber", "Blue", "Coral", "Dawn", "Echo", "Frost", "Gold", "Haze",
			"Iris", "Jade", "Kelp", "Luna", "Mint", "Nova", "Opal", "Pine",
			"Quill", "Rose", "Sage", "Tide", "Vale", "Wind", "Zen", "Arc",
			"Birch", "Cloud", "Dusk", "Elm", "Fern", "Glen", "Husk", "Ivy"
		];
		var nouns = [
			"Brook", "Cove", "Drift", "Edge", "Forge", "Grove", "Haven", "Isle",
			"Keep", "Lake", "Mesa", "Nest", "Oak", "Peak", "Ridge", "Shore",
			"Trail", "Vault", "Wharf", "Yard", "Bluff", "Creek", "Dell", "Field",
			"Gate", "Heath", "Knoll", "Ledge", "Mill", "Nook", "Pond", "Reef"
		];
		// Collect existing room names
		var existing = {};
		$tw.wiki.each(function(tiddler, title) {
			if(title.indexOf("$:/temp/tiddlydesktop-rs/relay-room/") === 0) {
				var name = tiddler.fields.room_name;
				if(name) existing[name.toLowerCase()] = true;
			}
		});
		// Try random combinations (adjective + noun)
		for(var attempt = 0; attempt < 100; attempt++) {
			var adj = adjectives[Math.floor(Math.random() * adjectives.length)];
			var noun = nouns[Math.floor(Math.random() * nouns.length)];
			var candidate = adj + " " + noun;
			if(!existing[candidate.toLowerCase()]) return candidate;
		}
		// Fallback: append a number
		for(var n = 1; n < 1000; n++) {
			var fallback = "Room " + n;
			if(!existing[fallback.toLowerCase()]) return fallback;
		}
		return "Room";
	}

	// Add a relay room
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-add-room", function(event) {
		var p = event.paramObject || {};
		if (!p.roomCode || !p.password) return;
		var roomName = p.name || generateUniqueRoomName();
		invoke("relay_sync_add_room", {
			name: roomName, roomCode: p.roomCode, password: p.password, autoConnect: true
		}).then(function() {
			// Auto-connect the new room, then refresh UI once
			invoke("relay_sync_connect_room", { roomCode: p.roomCode }).then(function() {
				refreshSyncStatus();
			}).catch(function() {
				// Still refresh even if connect fails — room was added
				refreshSyncStatus();
			});
		}).catch(function(err) {
			console.error("Failed to add relay room:", err);
			alert("Failed to add room: " + err);
		});
	});

	// Remove a relay room
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-remove-room", function(event) {
		var roomCode = event.paramObject ? event.paramObject.roomCode : "";
		if (!roomCode) return;
		// Tell Rust to remove the room first, then clean up UI
		invoke("relay_sync_remove_room", { roomCode: roomCode }).then(function() {
			// Clean up all temp tiddlers for this room
			var roomTitle = "$:/temp/tiddlydesktop-rs/relay-room/" + roomCode;
			$tw.wiki.deleteTiddler(roomTitle);
			$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/relay-room-details/" + roomCode);
			$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/relay-room-password/" + roomCode);
			$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/relay-room-name-edit/" + roomCode);
			$tw.wiki.deleteTiddler("$:/state/relay-room-details/" + roomCode);
			$tw.wiki.each(function(tiddler, title) {
				if (title.indexOf("$:/temp/tiddlydesktop-rs/relay-room-peer/" + roomCode + "/") === 0) {
					$tw.wiki.deleteTiddler(title);
				}
			});
			refreshSyncStatus();
		}).catch(function(err) {
			console.error("Failed to remove relay room:", err);
			refreshSyncStatus();
		});
	});

	// Connect to a relay room
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-connect-room", function(event) {
		var roomCode = event.paramObject ? event.paramObject.roomCode : "";
		if (!roomCode) return;
		invoke("relay_sync_connect_room", { roomCode: roomCode }).then(function() {
			refreshSyncStatus();
		}).catch(function(err) {
			console.error("Failed to connect relay room:", err);
			refreshSyncStatus();
		});
	});

	// Disconnect from a relay room
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-disconnect-room", function(event) {
		var roomCode = event.paramObject ? event.paramObject.roomCode : "";
		if (!roomCode) return;
		invoke("relay_sync_disconnect_room", { roomCode: roomCode }).then(function() {
			refreshSyncStatus();
		}).catch(function(err) {
			console.error("Failed to disconnect relay room:", err);
		});
	});

	// Set auto-connect for a relay room
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-set-room-auto-connect", function(event) {
		var p = event.paramObject || {};
		var roomCode = p.roomCode;
		var enabled = p.enabled === "true";
		if (!roomCode) return;
		invoke("relay_sync_set_room_auto_connect", { roomCode: roomCode, enabled: enabled }).then(function() {
			refreshSyncStatus();
		}).catch(function(err) {
			console.error("Failed to set room auto-connect:", err);
		});
	});

	// Set Relay room password
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-set-room-password", function(event) {
		var p = event.paramObject || {};
		var roomCode = p.roomCode;
		var password = p.password;
		if (!roomCode || !password) return;
		invoke("relay_sync_set_room_password", { roomCode: roomCode, password: password }).then(function() {
			refreshSyncStatus();
		}).catch(function(err) {
			console.error("Failed to set room password:", err);
		});
	});

	// Set Relay room name
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-set-room-name", function(event) {
		var p = event.paramObject || {};
		var roomCode = p.roomCode;
		var name = p.name;
		if (!roomCode || !name) return;
		invoke("relay_sync_set_room_name", { roomCode: roomCode, name: name }).then(function() {
			refreshSyncStatus();
		}).catch(function(err) {
			console.error("Failed to set room name:", err);
		});
	});

	// Load Relay room details (credentials) into temp tiddler
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-load-room-details", function(event) {
		var p = event.paramObject || {};
		var roomCode = p.roomCode;
		if (!roomCode) return;
		invoke("relay_sync_get_room_credentials", { roomCode: roomCode }).then(function(creds) {
			var detailsTiddler = "$:/temp/tiddlydesktop-rs/relay-room-details/" + roomCode;
			$tw.wiki.addTiddler(new $tw.Tiddler({
				title: detailsTiddler,
				password: creds.password,
				room_code: creds.room_code,
				room_name: creds.name
			}));
			// Pre-fill edit fields
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-room-password/" + roomCode, "text", null, creds.password);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-room-name-edit/" + roomCode, "text", null, creds.name);
		}).catch(function(err) {
			console.error("Failed to load room credentials:", err);
		});
	});

	// Set Relay server URL
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-sync-set-url", function(event) {
		var url = event.paramObject && event.paramObject.url;
		if (url) {
			invoke("relay_sync_set_url", { url: url }).then(function() {
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-sync-url", "text", null, url);
			}).catch(function(err) {
				console.error("Failed to set relay URL:", err);
			});
		}
	});

	// Set device display name
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-display-name", function(event) {
		var name = event.paramObject && event.paramObject.name || "";
		invoke("lan_sync_set_display_name", { name: name }).then(function() {
			refreshSyncStatus();
		}).catch(function(err) {
			console.error("Failed to set display name:", err);
		});
	});

	// Prepare add room form with auto-generated credentials
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-prepare-add-room", function(event) {
		invoke("relay_sync_generate_credentials").then(function(creds) {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-add-room-code", "text", null, creds.room_code);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-add-room-password", "text", null, creds.password);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-add-room-name", "text", null, generateUniqueRoomName());
		}).catch(function(err) {
			console.error("Failed to generate room credentials:", err);
		});
	});

	// Set wiki relay room assignment
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-relay-set-wiki-room", function(event) {
		var p = event.paramObject || {};
		var path = p.path;
		var roomCode = p.roomCode || null;
		if (!path) return;
		// Update the local wiki list entry (persistent tiddler)
		var entries = getWikiListEntries();
		for (var i = 0; i < entries.length; i++) {
			if (entries[i].path === path) {
				if (roomCode) {
					entries[i].relay_room = roomCode;
				} else {
					delete entries[i].relay_room;
				}
				// Directly update the temp tiddler for immediate UI update
				var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
				$tw.wiki.setText(tempTitle, "relay_room", null, roomCode || "");
				break;
			}
		}
		saveWikiList(entries);
		// Update the Rust backend
		invoke("set_wiki_relay_room", { path: path, roomCode: roomCode || null }).catch(function(err) {
			console.error("Failed to set wiki relay room:", err);
		});
		// Auto-connect the room if assigning (and not already connected)
		if (roomCode) {
			invoke("relay_sync_connect_room", { roomCode: roomCode }).catch(function() {});
		}
	});

	// Track auth state transitions to fetch server rooms once on initial detection
	var _lastKnownAuthState = false;

	// Refresh sync status and update tiddlers (debounced to avoid races)
	var _refreshSyncTimer = null;
	function refreshSyncStatus() {
		if (_refreshSyncTimer) clearTimeout(_refreshSyncTimer);
		_refreshSyncTimer = setTimeout(_doRefreshSyncStatus, 100);
	}
	function _doRefreshSyncStatus() {
		invoke("lan_sync_get_status").then(function(status) {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/lan-sync-running", "text", null, status.running ? "yes" : "no");
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/lan-sync-device-name", "text", null, status.device_name);
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/lan-sync-device-id", "text", null, status.device_id);

			// Fetch relay status to update rooms and determine any-sync-running
			invoke("relay_sync_get_status").then(function(relayStatus) {
				var anyRoomConnected = false;
				var rooms = relayStatus.rooms || [];
				rooms.forEach(function(room) {
					if (room.connected) anyRoomConnected = true;
				});
				var anyRunning = (status.running || anyRoomConnected) ? "yes" : "no";
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/any-sync-running", "text", null, anyRunning);
				if (relayStatus.relay_url) {
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-sync-url", "text", null, relayStatus.relay_url);
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-sync-url-input", "text", null, relayStatus.relay_url);
				}
				// Update auth status tiddlers
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-username", "text", null, relayStatus.username || relayStatus.github_login || "");
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-provider", "text", null, relayStatus.auth_provider || "");
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-status", "text", null, (relayStatus.authenticated || relayStatus.github_authenticated) ? "authenticated" : "");
				// Legacy tiddlers
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-login", "text", null, relayStatus.username || relayStatus.github_login || "");
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-auth-status", "text", null, (relayStatus.authenticated || relayStatus.github_authenticated) ? "authenticated" : "");
				// Fetch server rooms on auth transition
				var isAuth = !!(relayStatus.authenticated || relayStatus.github_authenticated);
				if(isAuth && !_lastKnownAuthState) {
					fetchServerRooms();
				}
				_lastKnownAuthState = isAuth;
				updateRelayRoomList(rooms);
			}).catch(function(err) {
				console.error("relay_sync_get_status failed (inner):", err);
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/any-sync-running", "text", null, status.running ? "yes" : "no");
			});
		}).catch(function(err) {
			// Sync not initialized yet — set defaults
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/lan-sync-running", "text", null, "no");
			invoke("relay_sync_get_status").then(function(relayStatus) {
				var anyRoomConnected = false;
				var rooms = relayStatus.rooms || [];
				rooms.forEach(function(room) {
					if (room.connected) anyRoomConnected = true;
				});
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/any-sync-running", "text", null, anyRoomConnected ? "yes" : "no");
				if (relayStatus.relay_url) {
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-sync-url", "text", null, relayStatus.relay_url);
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-sync-url-input", "text", null, relayStatus.relay_url);
				}
				// Update auth status tiddlers
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-username", "text", null, relayStatus.username || relayStatus.github_login || "");
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-provider", "text", null, relayStatus.auth_provider || "");
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/auth-status", "text", null, (relayStatus.authenticated || relayStatus.github_authenticated) ? "authenticated" : "");
				// Legacy tiddlers
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-login", "text", null, relayStatus.username || relayStatus.github_login || "");
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/github-auth-status", "text", null, (relayStatus.authenticated || relayStatus.github_authenticated) ? "authenticated" : "");
				// Fetch server rooms on auth transition
				var isAuth2 = !!(relayStatus.authenticated || relayStatus.github_authenticated);
				if(isAuth2 && !_lastKnownAuthState) {
					fetchServerRooms();
				}
				_lastKnownAuthState = isAuth2;
				updateRelayRoomList(rooms);
			}).catch(function(err) {
				console.error("relay_sync_get_status failed (outer):", err);
				$tw.wiki.setText("$:/temp/tiddlydesktop-rs/any-sync-running", "text", null, "no");
			});
		});
	}

	// Update relay room list tiddlers for UI display
	function updateRelayRoomList(rooms) {
		// Build set of current room codes and peer titles
		var currentRoomTitles = {};
		var currentPeerTitles = {};
		var roomListTitles = [];
		if (rooms && rooms.length > 0) {
			rooms.forEach(function(room) {
				var roomTitle = "$:/temp/tiddlydesktop-rs/relay-room/" + room.room_code;
				currentRoomTitles[roomTitle] = true;
				roomListTitles.push(roomTitle);
				// Create/update peer tiddlers for this room
				var peerTitles = [];
				if (room.connected_peers) {
					room.connected_peers.forEach(function(peer) {
						var peerTitle = "$:/temp/tiddlydesktop-rs/relay-room-peer/" + room.room_code + "/" + peer.device_id;
						currentPeerTitles[peerTitle] = true;
						peerTitles.push(peerTitle);
						$tw.wiki.addTiddler(new $tw.Tiddler({
							title: peerTitle,
							device_name: peer.device_name,
							device_id: peer.device_id,
							room_code: room.room_code
						}));
					});
				}
				$tw.wiki.addTiddler(new $tw.Tiddler({
					title: roomTitle,
					room_name: room.name,
					room_code: room.room_code,
					auto_connect: room.auto_connect ? "yes" : "no",
					connected: room.connected ? "yes" : "no",
					peer_count: String(room.connected_peers ? room.connected_peers.length : 0),
					peer_list: peerTitles.join(" ")
				}));
				// Keep password and details tiddlers in sync
				if (room.password) {
					var detailsTiddler = "$:/temp/tiddlydesktop-rs/relay-room-details/" + room.room_code;
					$tw.wiki.addTiddler(new $tw.Tiddler({
						title: detailsTiddler,
						password: room.password,
						room_code: room.room_code,
						room_name: room.name
					}));
					$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-room-password/" + room.room_code, "text", null, room.password);
				}
			});
		}
		// Remove only stale room and peer tiddlers (ones no longer in the list)
		$tw.wiki.each(function(tiddler, title) {
			if (title.indexOf("$:/temp/tiddlydesktop-rs/relay-room/") === 0 && !currentRoomTitles[title]) {
				$tw.wiki.deleteTiddler(title);
			} else if (title.indexOf("$:/temp/tiddlydesktop-rs/relay-room-peer/") === 0 && !currentPeerTitles[title]) {
				$tw.wiki.deleteTiddler(title);
			}
		});
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/relay-room-list", "text", null, roomListTitles.join(" "));
	}

	function refreshRemoteWikis(wikis) {
		// Remove old remote wiki tiddlers
		$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/remote-wikis/]]").forEach(function(title) {
			$tw.wiki.deleteTiddler(title);
		});

		if (wikis && wikis.length > 0) {
			wikis.forEach(function(wiki, index) {
				var tiddlerTitle = "$:/temp/tiddlydesktop-rs/remote-wikis/" + wiki.wiki_id;
				$tw.wiki.addTiddler(new $tw.Tiddler({
					title: tiddlerTitle,
					wiki_id: wiki.wiki_id,
					wiki_name: wiki.wiki_name,
					is_folder: wiki.is_folder ? "true" : "false",
					from_device_id: wiki.from_device_id,
					from_device_name: wiki.from_device_name,
					room_code: wiki.room_code || ""
				}));
			});
		}
	}

	// Listen for peer connection/disconnection events
	if (listen) {
		listen("lan-sync-peer-connected", function(event) {
			refreshSyncStatus();
			// Send our wiki manifest to the newly connected peer
			invoke("lan_sync_broadcast_manifest").catch(function() {});
			// Also refresh available wikis after a short delay (manifest takes time to arrive)
			setTimeout(function() {
				invoke("lan_sync_get_available_wikis").then(function(wikis) {
					refreshRemoteWikis(wikis);
				}).catch(function() {});
			}, 1500);
		});
		listen("lan-sync-peer-disconnected", function(event) {
			refreshSyncStatus();
		});
		listen("lan-sync-peers-updated", function() {
			refreshSyncStatus();
		});
		listen("lan-sync-remote-wikis-updated", function(event) {
			refreshRemoteWikis(event.payload);
		});
		listen("lan-sync-wiki-received", function(event) {
			var data = event.payload;
			console.log("[LAN Sync] Wiki received:", data.wiki_name, "at", data.wiki_path);
			// Open the received wiki
			var command = data.is_folder ? "open_wiki_folder" : "open_wiki_window";
			invoke(command, { path: data.wiki_path }).then(function(entry) {
				// Carry over sync settings from the transfer
				entry.sync_enabled = true;
				entry.sync_id = data.wiki_id;
				if (data.relay_room) {
					entry.relay_room = data.relay_room;
				}
				addToWikiList(entry);
				refreshWikiList();
			}).catch(function(err) {
				console.error("[LAN Sync] Failed to open received wiki:", err);
				// Still refresh wiki list even if open failed
				refreshWikiList();
			});
			// Also refresh remote wikis (it should be removed from available list now)
			invoke("lan_sync_get_available_wikis").then(function(wikis) {
				refreshRemoteWikis(wikis);
			}).catch(function() {});
		});
		// Refresh UI when a relay room connects or disconnects
		listen("relay-room-connected", function() {
			refreshSyncStatus();
			// Delayed re-check: session_init handshake may take a moment
			setTimeout(refreshSyncStatus, 2000);
			// Also refresh available wikis once peer manifests arrive
			setTimeout(function() {
				invoke("lan_sync_get_available_wikis").then(function(wikis) {
					refreshRemoteWikis(wikis);
				}).catch(function() {});
			}, 3000);
		});
		listen("relay-room-disconnected", function() {
			refreshSyncStatus();
		});
		// Refresh relay status after start_background() completes (may clear auth on
		// token validation failure, or successfully connect rooms after initial boot)
		listen("relay-sync-config-updated", function() {
			refreshSyncStatus();
		});
	}

	// Initial status check
	refreshSyncStatus();

	// Pre-populate display name input from saved setting
	invoke("lan_sync_get_display_name_setting").then(function(name) {
		if (name) {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/device-display-name-input", "text", null, name);
		}
	}).catch(function() {});

	callback();
};

})();
