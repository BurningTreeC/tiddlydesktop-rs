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

	// Desktop app - always set mobile to no
	$tw.wiki.setText("$:/temp/tiddlydesktop-rs/is-mobile", "text", null, "no");

	// Set main wiki flag
	$tw.wiki.setText("$:/temp/tiddlydesktop-rs/is-main-wiki", "text", null, isMainWiki ? "yes" : "no");

	// Add body class for main wiki (used by CSS to hide notifications)
	if (isMainWiki) {
		document.body.classList.add("td-main-wiki");
	}

	// Non-main wikis don't need the wiki list management - external attachments
	// and drag-drop are handled by the protocol handler script injection
	if (!isMainWiki) {
		// ========================================
		// tm-open-window handler (for opened wikis only, not landing page)
		// Opens tiddlers in new windows using Tauri
		// ========================================
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

	// Save wiki list to the persistent tiddler and trigger autosave
	function saveWikiList(entries) {
		$tw.wiki.addTiddler({
			title: "$:/TiddlyDesktop/WikiList",
			type: "application/json",
			text: JSON.stringify(entries, null, 2)
		});
		// Trigger autosave so the main wiki file is saved
		$tw.rootWidget.dispatchEvent({type: "tm-auto-save-wiki"});
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

		// Populate temp tiddlers for UI
		entries.forEach(function(entry, index) {
			$tw.wiki.addTiddler({
				title: "$:/temp/tiddlydesktop-rs/wikis/" + index,
				path: entry.path,
				filename: entry.filename,
				favicon: entry.favicon || "",
				is_folder: entry.is_folder ? "true" : "false",
				backups_enabled: entry.backups_enabled ? "true" : "false",
				backup_dir: entry.backup_dir || "",
				group: entry.group || "",
				text: ""
			});
		});

		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/wiki-count", "text", null, String(entries.length));
	}

	// ========================================
	// Message Handlers
	// ========================================

	// Message handler: open wiki file dialog
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-open-wiki", function(event) {
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
	});

	// Message handler: open wiki folder dialog
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-open-folder", function(event) {
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

		invoke("init_wiki_folder", { path: folderPath, edition: editionId, plugins: selectedPlugins }).then(function() {
			$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "no");

			// Now open the newly initialized folder
			invoke("open_wiki_folder", { path: folderPath }).then(function(entry) {
				addToWikiList(entry);
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to open initialized folder:", err);
				alert("Wiki initialized but failed to open: " + err);
			});
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

	// Message handler: open a specific wiki path (auto-detect file vs folder)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-open-path", function(event) {
		var path = event.param || event.paramObject.path;
		var isFolder = event.paramObject && event.paramObject.isFolder === "true";
		if (path) {
			var command = isFolder ? "open_wiki_folder" : "open_wiki_window";
			invoke(command, { path: path }).then(function(entry) {
				addToWikiList(entry);
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

	// Message handler: set custom backup directory for a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-backup-dir", function(event) {
		var path = event.paramObject && event.paramObject.path;
		if (path) {
			// Open folder picker dialog
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
						break;
					}
				}
				saveWikiList(entries);
			}).catch(function(err) {
				console.error("Failed to clear backup directory:", err);
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
				saveWikiList(entries);
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to delete group:", err);
			});
		}
	});

	// Message handler: toggle group collapse state
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-toggle-group", function(event) {
		var groupName = event.param || (event.paramObject && event.paramObject.group);
		var stateTitle = "$:/state/tiddlydesktop-rs/group-collapsed/" + (groupName || "Ungrouped");
		var currentState = $tw.wiki.getTiddlerText(stateTitle);
		$tw.wiki.setText(stateTitle, "text", null, currentState === "yes" ? "no" : "yes");
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

	// Initial load of wiki list from tiddler
	refreshWikiList();

	callback();
};

})();
