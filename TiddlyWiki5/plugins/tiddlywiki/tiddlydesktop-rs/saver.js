/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/saver.js
type: application/javascript
module-type: saver

Saver for TiddlyDesktop main wiki - enables saving the main wiki file
which stores the wiki list as a tiddler.

\*/
(function() {

"use strict";

var TiddlyDesktopSaver = function(wiki) {
    this.wiki = wiki;
};

TiddlyDesktopSaver.prototype.save = function(text, method, callback) {
    // Only save if we have a wiki path
    if (!window.__WIKI_PATH__) {
        return false;
    }

    // Try Tauri IPC first (works reliably on all platforms)
    if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
        window.__TAURI__.core.invoke("save_wiki", {
            path: window.__WIKI_PATH__,
            content: text
        }).then(function() {
            callback(null);
        }).catch(function(err) {
            callback(err.toString());
        });
    } else if (window.__SAVE_URL__) {
        // Fallback to fetch via protocol
        fetch(window.__SAVE_URL__, {
            method: "PUT",
            body: text
        }).then(function(response) {
            if (response.ok) {
                callback(null);
            } else {
                callback("Save failed: HTTP " + response.status);
            }
        }).catch(function(err) {
            callback("Save failed: " + err.toString());
        });
    } else {
        callback("No save mechanism available");
    }

    return true;
};

TiddlyDesktopSaver.prototype.info = {
    name: "tiddlydesktop-rs",
    priority: 5000,
    capabilities: ["save", "autosave"]
};

exports.canSave = function(wiki) {
    return typeof window !== "undefined" &&
           window.__TAURI__ &&
           window.__WIKI_PATH__;
};

exports.create = function(wiki) {
    return new TiddlyDesktopSaver(wiki);
};

})();
