/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/message-handlers/convert-wiki.js
type: application/javascript
module-type: startup

Message handler for converting wikis between single-file and folder formats

\*/
(function(){

/*jslint node: true, browser: true */
/*global $tw: false */
"use strict";

exports.name = "tiddlydesktop-convert-wiki-handler";
exports.after = ["startup"];
exports.synchronous = true;

exports.startup = function() {
    // Only run in browser with Tauri
    if (typeof window === "undefined" || !window.__TAURI__) {
        return;
    }

    $tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-convert-wiki", function(event) {
        var sourcePath = event.paramObject && event.paramObject.path;
        var isFolder = event.paramObject && event.paramObject.isFolder === "true";

        if (!sourcePath) {
            console.error("[TiddlyDesktop] Convert wiki: no source path provided");
            return;
        }

        var toFolder = !isFolder; // If it's a folder, convert to file; if it's a file, convert to folder

        // Open save dialog to choose destination
        window.__TAURI__.dialog.save({
            title: toFolder ? "Save folder wiki as..." : "Save single-file wiki as...",
            filters: toFolder ? [] : [{ name: "TiddlyWiki", extensions: ["html"] }]
        }).then(function(destPath) {
            if (!destPath) {
                return; // User cancelled
            }

            console.log("[TiddlyDesktop] Converting wiki:", sourcePath, "to", destPath, "toFolder:", toFolder);

            // Show converting indicator
            $tw.wiki.addTiddler({
                title: "$:/temp/tiddlydesktop-rs/converting",
                text: "yes"
            });

            window.__TAURI__.core.invoke("convert_wiki", {
                sourcePath: sourcePath,
                destPath: destPath,
                toFolder: toFolder
            }).then(function() {
                console.log("[TiddlyDesktop] Conversion successful");
                // Remove converting indicator
                $tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/converting");
                // Refresh the wiki list
                $tw.rootWidget.dispatchEvent({type: "tm-tiddlydesktop-rs-refresh"});
            }).catch(function(err) {
                console.error("[TiddlyDesktop] Conversion failed:", err);
                // Remove converting indicator
                $tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/converting");
                window.__TAURI__.dialog.message("Conversion failed: " + err, { title: "Error", kind: "error" });
            });
        }).catch(function(err) {
            console.error("[TiddlyDesktop] Save dialog error:", err);
        });
    });
};

})();
