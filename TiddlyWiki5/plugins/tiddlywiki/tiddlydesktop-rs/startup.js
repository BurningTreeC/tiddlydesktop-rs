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
		isMobile: false, // Will be updated on startup
		// Async dialog methods for TiddlyWiki to use
		alert: function(message) {
			return invoke("show_alert", { message: String(message || "") });
		},
		confirm: function(message) {
			return invoke("show_confirm", { message: String(message || "") });
		}
	};

	// Detect if running on mobile
	invoke("is_mobile").then(function(mobile) {
		$tw.tiddlydesktoprs.isMobile = mobile;
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/is-mobile", "text", null, mobile ? "yes" : "no");
		console.log("Platform detected: " + (mobile ? "mobile" : "desktop"));
	});

	// Set main wiki flag
	$tw.wiki.setText("$:/temp/tiddlydesktop-rs/is-main-wiki", "text", null, isMainWiki ? "yes" : "no");

	// Add body class for main wiki (used by CSS to hide notifications)
	if (isMainWiki) {
		document.body.classList.add("td-main-wiki");
	}

	// Non-main wikis don't need the wiki list management - external attachments
	// and drag-drop are handled by the protocol handler script injection
	if (!isMainWiki) {
		console.log("Not main wiki, skipping wiki list management");
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
		// Find existing entry to preserve backups_enabled setting
		var existingEntry = null;
		for (var i = 0; i < entries.length; i++) {
			if (entries[i].path === entry.path) {
				existingEntry = entries[i];
				break;
			}
		}
		if (existingEntry && existingEntry.backups_enabled !== undefined) {
			entry.backups_enabled = existingEntry.backups_enabled;
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

		// Populate temp tiddlers for UI
		entries.forEach(function(entry, index) {
			$tw.wiki.addTiddler({
				title: "$:/temp/tiddlydesktop-rs/wikis/" + index,
				path: entry.path,
				filename: entry.filename,
				favicon: entry.favicon || "",
				is_folder: entry.is_folder ? "true" : "false",
				backups_enabled: entry.backups_enabled ? "true" : "false",
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
		console.log("tm-tiddlydesktop-rs-open-wiki triggered");
		openDialog({
			multiple: false,
			filters: [{
				name: "TiddlyWiki",
				extensions: ["html", "htm"]
			}]
		}).then(function(file) {
			console.log("Dialog returned file:", file);
			if (file) {
				invoke("open_wiki_window", { path: file }).then(function(entry) {
					console.log("open_wiki_window completed, entry:", entry);
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
		console.log("tm-tiddlydesktop-rs-open-folder triggered");
		openDialog({
			directory: true,
			multiple: false
		}).then(function(folder) {
			console.log("Dialog returned folder:", folder);
			if (folder) {
				// Check folder status first
				invoke("check_folder_status", { path: folder }).then(function(status) {
					console.log("Folder status:", status);
					if (status.is_wiki) {
						// Already a wiki folder, open it directly
						invoke("open_wiki_folder", { path: folder }).then(function(entry) {
							console.log("open_wiki_folder completed, entry:", entry);
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

			console.log("Available editions:", editions);
			console.log("Available plugins:", plugins);

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
		console.log("tm-tiddlydesktop-rs-create-wiki triggered");
		var saveDialog = window.__TAURI__.dialog.save;
		saveDialog({
			filters: [{
				name: "TiddlyWiki",
				extensions: ["html"]
			}],
			defaultPath: "wiki.html"
		}).then(function(filePath) {
			console.log("Save dialog returned:", filePath);
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

		console.log("Creating wiki file:", filePath, "edition:", editionId, "plugins:", selectedPlugins);

		if (!filePath) {
			alert("Missing file path");
			return;
		}

		// Hide the selector and show loading state
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-edition-selector", "text", null, "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "yes");

		invoke("create_wiki_file", { path: filePath, edition: editionId, plugins: selectedPlugins }).then(function() {
			console.log("Wiki file created successfully");
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
		console.log("All plugin tiddlers:", allPluginTiddlers);

		allPluginTiddlers.forEach(function(title) {
			var tiddler = $tw.wiki.getTiddler(title);
			console.log("Plugin tiddler:", title, "selected:", tiddler ? tiddler.fields.selected : "no tiddler");
			if (tiddler && tiddler.fields.selected === "yes" && tiddler.fields.id) {
				selectedPlugins.push(tiddler.fields.id);
			}
		});

		console.log("Initializing folder:", folderPath, "with edition:", editionId, "plugins:", selectedPlugins);

		if (!folderPath || !editionId) {
			alert("Missing folder path or edition");
			return;
		}

		// Hide the selector and show loading state
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/show-edition-selector", "text", null, "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/init-loading", "text", null, "yes");

		invoke("init_wiki_folder", { path: folderPath, edition: editionId, plugins: selectedPlugins }).then(function() {
			console.log("Wiki folder initialized successfully");
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
		console.log("set-backups handler called:", path, "enabled:", enabled);
		if (path) {
			// Update the Rust backend
			invoke("set_wiki_backups", { path: path, enabled: enabled }).then(function() {
				console.log("set_wiki_backups invoke succeeded");
				// Update the local wiki list entry
				var entries = getWikiListEntries();
				for (var i = 0; i < entries.length; i++) {
					if (entries[i].path === path) {
						entries[i].backups_enabled = enabled;
						// Directly update the temp tiddler field for immediate UI update
						var tempTitle = "$:/temp/tiddlydesktop-rs/wikis/" + i;
						$tw.wiki.setText(tempTitle, "backups_enabled", null, enabled ? "true" : "false");
						console.log("Updated temp tiddler", tempTitle, "backups_enabled to", enabled ? "true" : "false");
						break;
					}
				}
				saveWikiList(entries);
				console.log("saveWikiList completed");
			}).catch(function(err) {
				console.error("Failed to set backups:", err);
			});
		}
	});

	// Set up drag-drop listeners
	listen("tauri://drag-drop", function(event) {
		var paths = event.payload.paths;
		if (paths && paths.length > 0) {
			paths.forEach(function(path) {
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
		}
	});

	listen("tauri://drag-enter", function() {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "yes");
	});

	listen("tauri://drag-leave", function() {
		$tw.wiki.setText("$:/temp/tiddlydesktop-rs/drag-over", "text", null, "no");
	});

	// Initial load of wiki list from tiddler
	refreshWikiList();

	callback();
};

})();
