/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/message-handlers/set-palette.js
type: application/javascript
module-type: startup

Message handler for setting color palette

\*/
(function(){

/*jslint node: true, browser: true */
/*global $tw: false */
"use strict";

exports.name = "tiddlydesktop-set-palette-handler";
exports.after = ["startup"];
exports.synchronous = true;

exports.startup = function() {
    // Only run in browser with Tauri
    if (typeof window === "undefined" || !window.__TAURI__) {
        return;
    }

    // Listen for palette changes and update headerbar colors
    $tw.wiki.addEventListener("change", function(changes) {
        if (changes["$:/palette"]) {
            if (window.TiddlyDesktop && window.TiddlyDesktop.updateHeaderBarColors) {
                window.TiddlyDesktop.updateHeaderBarColors();
            }
        }
    });

    $tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-palette", function(event) {
        var palette = (event.paramObject && event.paramObject.palette) || "";

        console.log("[TiddlyDesktop] Setting palette to:", palette);

        // Call Tauri to save the palette preference
        window.__TAURI__.core.invoke("set_palette", { palette: palette })
            .then(function() {
                // Set the $:/palette tiddler to apply the change immediately
                if (palette) {
                    $tw.wiki.setText("$:/palette", "text", null, palette);
                } else {
                    // Default palette
                    $tw.wiki.setText("$:/palette", "text", null, "$:/palettes/Vanilla");
                }
                // Trigger auto-save to persist the change (main wiki doesn't create backups)
                $tw.rootWidget.dispatchEvent({type: "tm-auto-save-wiki"});
            })
            .catch(function(err) {
                console.error("Failed to set palette:", err);
            });
    });

    // Load saved palette on startup
    window.__TAURI__.core.invoke("get_palette")
        .then(function(palette) {
            if (palette) {
                console.log("[TiddlyDesktop] Loading saved palette:", palette);
                $tw.wiki.setText("$:/palette", "text", null, palette);
            } else {
                // No saved palette - trigger initial headerbar update with current palette
                if (window.TiddlyDesktop && window.TiddlyDesktop.updateHeaderBarColors) {
                    window.TiddlyDesktop.updateHeaderBarColors();
                }
            }
        })
        .catch(function(err) {
            console.error("Failed to get palette:", err);
        });
};

})();
