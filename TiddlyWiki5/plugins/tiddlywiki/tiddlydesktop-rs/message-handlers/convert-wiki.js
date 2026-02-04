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
        var isAndroid = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/is-android") === "yes";

        // Function to perform the conversion
        function doConversion(destPath) {
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
        }

        if (isAndroid) {
            // Android: Use SAF-specific pickers
            if (toFolder) {
                // Converting file to folder - pick a destination folder
                window.__TAURI__.core.invoke("android_pick_folder_for_wiki_creation")
                    .then(function(result) {
                        if (!result) {
                            return; // User cancelled
                        }
                        var folderUri = result[0];
                        var isWiki = result[1];
                        var isEmpty = result[2];
                        var folderName = result[3];

                        if (isWiki) {
                            window.__TAURI__.dialog.message(
                                "The selected folder already contains a wiki. Please choose an empty folder.",
                                { title: "Error", kind: "error" }
                            );
                            return;
                        }

                        if (!isEmpty) {
                            // Folder has files but isn't a wiki - warn but allow
                            window.__TAURI__.dialog.confirm(
                                "The selected folder is not empty. Continue anyway?",
                                { title: "Warning", kind: "warning" }
                            ).then(function(confirmed) {
                                if (confirmed) {
                                    doConversion(folderUri);
                                }
                            });
                        } else {
                            doConversion(folderUri);
                        }
                    })
                    .catch(function(err) {
                        console.error("[TiddlyDesktop] Folder picker error:", err);
                        window.__TAURI__.dialog.message("Failed to pick folder: " + err, { title: "Error", kind: "error" });
                    });
            } else {
                // Converting folder to file - use save dialog to create new file
                // Extract a suggested name from the source folder
                var suggestedName = "wiki.html";
                try {
                    // Try to get folder name from path
                    var parts = sourcePath.split("/");
                    var lastPart = parts[parts.length - 1] || parts[parts.length - 2];
                    if (lastPart && !lastPart.includes(":")) {
                        suggestedName = lastPart + ".html";
                    }
                } catch(e) {}

                window.__TAURI__.core.invoke("android_create_wiki_file", { suggestedName: suggestedName })
                    .then(function(fileUri) {
                        doConversion(fileUri);
                    })
                    .catch(function(err) {
                        console.error("[TiddlyDesktop] Save dialog error:", err);
                        window.__TAURI__.dialog.message("Failed to create file: " + err, { title: "Error", kind: "error" });
                    });
            }
        } else {
            // Desktop: Use standard Tauri dialog
            window.__TAURI__.dialog.save({
                title: toFolder ? "Save folder wiki as..." : "Save single-file wiki as...",
                filters: toFolder ? [] : [{ name: "TiddlyWiki", extensions: ["html"] }]
            }).then(function(destPath) {
                doConversion(destPath);
            }).catch(function(err) {
                console.error("[TiddlyDesktop] Save dialog error:", err);
            });
        }
    });
};

})();
