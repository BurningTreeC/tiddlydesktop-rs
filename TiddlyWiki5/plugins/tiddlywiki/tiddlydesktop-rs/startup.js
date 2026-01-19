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

	// Store references globally for other modules
	$tw.tiddlydesktop = {
		invoke: invoke,
		listen: listen,
		openDialog: openDialog,
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
		$tw.tiddlydesktop.isMobile = mobile;
		$tw.wiki.setText("$:/temp/tiddlydesktop/is-mobile", "text", null, mobile ? "yes" : "no");
		console.log("Platform detected: " + (mobile ? "mobile" : "desktop"));
	});

	// Message handler: open wiki file dialog
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-open-wiki", function(event) {
		console.log("tm-tiddlydesktop-open-wiki triggered");
		openDialog({
			multiple: false,
			filters: [{
				name: "TiddlyWiki",
				extensions: ["html", "htm"]
			}]
		}).then(function(file) {
			console.log("Dialog returned file:", file);
			if (file) {
				invoke("open_wiki_window", { path: file }).then(function() {
					console.log("open_wiki_window completed, refreshing list");
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
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-open-folder", function(event) {
		console.log("tm-tiddlydesktop-open-folder triggered");
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
						invoke("open_wiki_folder", { path: folder }).then(function() {
							console.log("open_wiki_folder completed, refreshing list");
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
		$tw.wiki.setText("$:/temp/tiddlydesktop/init-folder-path", "text", null, folderPath);
		$tw.wiki.setText("$:/temp/tiddlydesktop/init-folder-name", "text", null, folderStatus.name);
		$tw.wiki.setText("$:/temp/tiddlydesktop/init-folder-empty", "text", null, folderStatus.is_empty ? "yes" : "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop/selected-edition", "text", null, "empty");
		$tw.wiki.setText("$:/temp/tiddlydesktop/selected-plugins", "text", null, "");
		$tw.wiki.setText("$:/temp/tiddlydesktop/create-mode", "text", null, "folder");

		loadEditionsAndPlugins();
	}

	// Function to show edition selector for creating a new wiki file
	function showFileCreator(filePath) {
		// Store the file path for later use
		$tw.wiki.setText("$:/temp/tiddlydesktop/init-file-path", "text", null, filePath);
		$tw.wiki.setText("$:/temp/tiddlydesktop/selected-edition", "text", null, "empty");
		$tw.wiki.setText("$:/temp/tiddlydesktop/selected-plugins", "text", null, "");
		$tw.wiki.setText("$:/temp/tiddlydesktop/create-mode", "text", null, "file");

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
			$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop/editions/]]").forEach(function(title) {
				$tw.wiki.deleteTiddler(title);
			});
			$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop/plugins/]]").forEach(function(title) {
				$tw.wiki.deleteTiddler(title);
			});

			// Add edition entries
			editions.forEach(function(edition, index) {
				$tw.wiki.addTiddler({
					title: "$:/temp/tiddlydesktop/editions/" + index,
					id: edition.id,
					name: edition.name,
					description: edition.description,
					text: ""
				});
			});

			// Add plugin entries
			plugins.forEach(function(plugin, index) {
				$tw.wiki.addTiddler({
					title: "$:/temp/tiddlydesktop/plugins/" + plugin.id,
					id: plugin.id,
					name: plugin.name,
					description: plugin.description,
					category: plugin.category,
					selected: "no",
					text: ""
				});
			});

			// Show the edition selector modal
			$tw.wiki.setText("$:/temp/tiddlydesktop/show-edition-selector", "text", null, "yes");
		}).catch(function(err) {
			console.error("Failed to load editions/plugins:", err);
			alert("Failed to load editions: " + err);
		});
	}

	// Message handler: create new wiki file (shows save dialog then edition selector)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-create-wiki", function(event) {
		console.log("tm-tiddlydesktop-create-wiki triggered");
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
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-create-file", function(event) {
		var filePath = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop/init-file-path");
		var editionId = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop/selected-edition") || "empty";

		// Collect selected plugins
		var selectedPlugins = [];
		$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop/plugins/]]").forEach(function(title) {
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
		$tw.wiki.setText("$:/temp/tiddlydesktop/show-edition-selector", "text", null, "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop/init-loading", "text", null, "yes");

		invoke("create_wiki_file", { path: filePath, edition: editionId, plugins: selectedPlugins }).then(function() {
			console.log("Wiki file created successfully");
			$tw.wiki.setText("$:/temp/tiddlydesktop/init-loading", "text", null, "no");

			// Open the newly created wiki file
			invoke("open_wiki_window", { path: filePath }).then(function() {
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to open created wiki:", err);
				alert("Wiki created but failed to open: " + err);
			});
		}).catch(function(err) {
			console.error("create_wiki_file error:", err);
			$tw.wiki.setText("$:/temp/tiddlydesktop/init-loading", "text", null, "no");
			alert("Failed to create wiki file: " + err);
		});
	});

	// Message handler: toggle plugin selection
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-toggle-plugin", function(event) {
		var pluginId = event.param || (event.paramObject && event.paramObject.plugin);
		if (pluginId) {
			var tiddler = $tw.wiki.getTiddler("$:/temp/tiddlydesktop/plugins/" + pluginId);
			if (tiddler) {
				var isSelected = tiddler.fields.selected === "yes";
				$tw.wiki.setText("$:/temp/tiddlydesktop/plugins/" + pluginId, "selected", null, isSelected ? "no" : "yes");
			}
		}
	});

	// Message handler: initialize wiki folder with selected edition and plugins
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-init-folder", function(event) {
		var editionId = event.param || (event.paramObject && event.paramObject.edition);
		var folderPath = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop/init-folder-path");

		// If no edition passed, use the selected one
		if (!editionId) {
			editionId = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop/selected-edition") || "server";
		}

		// Collect selected plugins
		var selectedPlugins = [];
		var allPluginTiddlers = $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop/plugins/]]");
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
		$tw.wiki.setText("$:/temp/tiddlydesktop/show-edition-selector", "text", null, "no");
		$tw.wiki.setText("$:/temp/tiddlydesktop/init-loading", "text", null, "yes");

		invoke("init_wiki_folder", { path: folderPath, edition: editionId, plugins: selectedPlugins }).then(function() {
			console.log("Wiki folder initialized successfully");
			$tw.wiki.setText("$:/temp/tiddlydesktop/init-loading", "text", null, "no");

			// Now open the newly initialized folder
			invoke("open_wiki_folder", { path: folderPath }).then(function() {
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to open initialized folder:", err);
				alert("Wiki initialized but failed to open: " + err);
			});
		}).catch(function(err) {
			console.error("init_wiki_folder error:", err);
			$tw.wiki.setText("$:/temp/tiddlydesktop/init-loading", "text", null, "no");
			alert("Failed to initialize wiki folder: " + err);
		});
	});

	// Message handler: cancel edition selection
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-cancel-init", function(event) {
		$tw.wiki.setText("$:/temp/tiddlydesktop/show-edition-selector", "text", null, "no");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop/init-folder-path");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop/init-folder-name");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop/init-file-path");
		$tw.wiki.deleteTiddler("$:/temp/tiddlydesktop/create-mode");
	});

	// Message handler: open a specific wiki path (auto-detect file vs folder)
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-open-path", function(event) {
		var path = event.param || event.paramObject.path;
		var isFolder = event.paramObject && event.paramObject.isFolder === "true";
		if (path) {
			var command = isFolder ? "open_wiki_folder" : "open_wiki_window";
			invoke(command, { path: path }).then(function() {
				refreshWikiList();
			}).catch(function(err) {
				console.error("Failed to open wiki:", err);
				alert("Failed to open: " + err);
			});
		}
	});

	// Message handler: reveal wiki in file manager
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-reveal", function(event) {
		var path = event.param || event.paramObject.path;
		if (path) {
			invoke("reveal_in_folder", { path: path }).catch(function(err) {
				console.error("Failed to reveal:", err);
			});
		}
	});

	// Message handler: remove wiki from recent list
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-remove", function(event) {
		var path = event.param || event.paramObject.path;
		if (path) {
			invoke("remove_recent_file", { path: path }).then(function() {
				refreshWikiList();
			});
		}
	});

	// Message handler: toggle backups for a wiki
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-set-backups", function(event) {
		var path = event.paramObject && event.paramObject.path;
		var enabled = event.paramObject && event.paramObject.enabled === "true";
		if (path) {
			if (enabled && $tw.tiddlydesktop.isMobile) {
				// On mobile, check if we have folder access before enabling backups
				invoke("has_backup_folder_access", { wikiPath: path }).then(function(hasAccess) {
					if (hasAccess) {
						// Already have access, just enable backups
						invoke("set_wiki_backups", { path: path, enabled: true }).then(function() {
							refreshWikiList();
						});
					} else {
						// Need to request folder access first - use folder picker dialog
						openDialog({
							directory: true,
							multiple: false,
							title: "Select folder containing your wiki for backups"
						}).then(function(folderUri) {
							if (folderUri) {
								// Store the folder access
								invoke("set_backup_folder_access", { wikiPath: path, folderUri: folderUri }).then(function() {
									// Now enable backups
									invoke("set_wiki_backups", { path: path, enabled: true }).then(function() {
										refreshWikiList();
									});
								}).catch(function(err) {
									console.error("Failed to set folder access:", err);
									refreshWikiList();
								});
							} else {
								console.log("Folder selection cancelled");
								refreshWikiList();
							}
						}).catch(function(err) {
							console.log("Folder picker failed:", err);
							refreshWikiList();
						});
					}
				});
			} else {
				// Desktop or disabling backups - just update directly
				invoke("set_wiki_backups", { path: path, enabled: enabled }).then(function() {
					refreshWikiList();
				});
			}
		}
	});

	// Message handler: request backup folder access (mobile only) - uses folder picker
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-request-backup-access", function(event) {
		var path = event.paramObject && event.paramObject.path;
		if (path) {
			openDialog({
				directory: true,
				multiple: false,
				title: "Select folder containing your wiki for backups"
			}).then(function(folderUri) {
				if (folderUri) {
					invoke("set_backup_folder_access", { wikiPath: path, folderUri: folderUri }).then(function() {
						refreshWikiList();
					}).catch(function(err) {
						console.error("Failed to set folder access:", err);
					});
				}
			}).catch(function(err) {
				console.log("Folder picker failed:", err);
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
					invoke("open_wiki_window", { path: path }).then(function() {
						refreshWikiList();
					});
				} else {
					// Try to open as a folder - backend will verify if it's a valid wiki folder
					invoke("check_is_wiki_folder", { path: path }).then(function(isFolder) {
						if (isFolder) {
							invoke("open_wiki_folder", { path: path }).then(function() {
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
		$tw.wiki.setText("$:/temp/tiddlydesktop/drag-over", "text", null, "yes");
	});

	listen("tauri://drag-leave", function() {
		$tw.wiki.setText("$:/temp/tiddlydesktop/drag-over", "text", null, "no");
	});

	// Function to refresh wiki list from backend
	function refreshWikiList() {
		invoke("get_recent_files").then(function(entries) {
			console.log("get_recent_files returned:", entries);
			// Clear existing wiki entries
			$tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop/wikis/]]").forEach(function(title) {
				$tw.wiki.deleteTiddler(title);
			});
			// Add new entries
			if (entries && entries.length > 0) {
				// On mobile, also check backup folder access for each file wiki
				var accessChecks = [];
				if ($tw.tiddlydesktop.isMobile) {
					entries.forEach(function(entry) {
						if (!entry.is_folder) {
							accessChecks.push(
								invoke("has_backup_folder_access", { wikiPath: entry.path })
									.then(function(hasAccess) {
										return { path: entry.path, hasAccess: hasAccess };
									})
									.catch(function() {
										return { path: entry.path, hasAccess: false };
									})
							);
						}
					});
				}

				Promise.all(accessChecks).then(function(accessResults) {
					// Build a map of path -> hasAccess
					var accessMap = {};
					accessResults.forEach(function(result) {
						accessMap[result.path] = result.hasAccess;
					});

					entries.forEach(function(entry, index) {
						console.log("Adding wiki entry:", index, entry);
						var hasFolderAccess = entry.is_folder ? true : (accessMap[entry.path] || false);
						$tw.wiki.addTiddler({
							title: "$:/temp/tiddlydesktop/wikis/" + index,
							path: entry.path,
							filename: entry.filename,
							favicon: entry.favicon || "",
							is_folder: entry.is_folder ? "true" : "false",
							backups_enabled: entry.backups_enabled ? "true" : "false",
							has_folder_access: hasFolderAccess ? "true" : "false",
							text: ""
						});
					});
					$tw.wiki.setText("$:/temp/tiddlydesktop/wiki-count", "text", null, String(entries.length));
				});
			} else {
				$tw.wiki.setText("$:/temp/tiddlydesktop/wiki-count", "text", null, "0");
			}
		}).catch(function(err) {
			console.error("get_recent_files error:", err);
		});
	}

	// Initial load of wiki list
	refreshWikiList();

	callback();
};

})();
