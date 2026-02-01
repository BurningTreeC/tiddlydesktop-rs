/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/message-handlers/set-language.js
type: application/javascript
module-type: startup

Message handler for setting UI language

\*/
(function(){

/*jslint node: true, browser: true */
/*global $tw: false */
"use strict";

exports.name = "tiddlydesktop-set-language-handler";
exports.after = ["startup"];
exports.synchronous = true;

exports.startup = function() {
    // Only run in browser with Tauri
    if (typeof window === "undefined" || !window.__TAURI__) {
        return;
    }

    $tw.rootWidget.addEventListener("tm-tiddlydesktop-rs-set-language", function(event) {
        // TiddlyWiki passes action-sendmessage params in event.paramObject
        var language = (event.paramObject && event.paramObject.language) || "";

        console.log("[TiddlyDesktop] Setting language to:", language, "paramObject:", event.paramObject);

        // Check if wiki has unsaved changes
        var isDirty = false;
        try {
            if ($tw.wiki && typeof $tw.wiki.isDirty === 'function') {
                isDirty = $tw.wiki.isDirty();
            } else if ($tw.saverHandler && typeof $tw.saverHandler.isDirty === 'function') {
                isDirty = $tw.saverHandler.isDirty();
            } else if ($tw.saverHandler && typeof $tw.saverHandler.numChanges === 'function') {
                isDirty = $tw.saverHandler.numChanges() > 0;
            } else if (document.title && document.title.startsWith('*')) {
                isDirty = true;
            }
        } catch (e) {
            console.warn("[TiddlyDesktop] Could not check dirty state:", e);
        }

        // Function to save language and reload
        var saveLanguageAndReload = function() {
            window.__TAURI__.core.invoke("set_language", { language: language })
                .then(function() {
                    window.location.reload();
                })
                .catch(function(err) {
                    console.error("Failed to set language:", err);
                });
        };

        // If dirty or setting to auto-detect (empty), save wiki first, then change language
        if (isDirty || language === "") {
            console.log("[TiddlyDesktop] Saving wiki before language change...");
            // Trigger wiki save
            $tw.rootWidget.dispatchEvent({type: "tm-save-wiki"});
            // Wait a moment for save to process, then save language and reload
            setTimeout(saveLanguageAndReload, 500);
        } else {
            saveLanguageAndReload();
        }
    });
};

})();
