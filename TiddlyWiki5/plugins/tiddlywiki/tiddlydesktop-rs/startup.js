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
		openDialog: openDialog
	};

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

	// Message handler: open a specific wiki path
	$tw.rootWidget.addEventListener("tm-tiddlydesktop-open-path", function(event) {
		var path = event.param || event.paramObject.path;
		if (path) {
			invoke("open_wiki_window", { path: path }).then(function() {
				refreshWikiList();
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

	// Set up drag-drop listeners
	listen("tauri://drag-drop", function(event) {
		var paths = event.payload.paths;
		if (paths && paths.length > 0) {
			paths.forEach(function(path) {
				if (path.endsWith(".html") || path.endsWith(".htm")) {
					invoke("open_wiki_window", { path: path }).then(function() {
						refreshWikiList();
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
				entries.forEach(function(entry, index) {
					console.log("Adding wiki entry:", index, entry);
					$tw.wiki.addTiddler({
						title: "$:/temp/tiddlydesktop/wikis/" + index,
						path: entry.path,
						filename: entry.filename,
						favicon: entry.favicon || "",
						text: ""
					});
				});
			}
			$tw.wiki.setText("$:/temp/tiddlydesktop/wiki-count", "text", null, String(entries ? entries.length : 0));
		}).catch(function(err) {
			console.error("get_recent_files error:", err);
		});
	}

	// Initial load of wiki list
	refreshWikiList();

	callback();
};

})();
