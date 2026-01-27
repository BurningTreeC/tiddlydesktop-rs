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

        // Call Tauri to save the language preference
        window.__TAURI__.core.invoke("set_language", { language: language })
            .then(function() {
                // Reload the page to apply the new language
                window.location.reload();
            })
            .catch(function(err) {
                console.error("Failed to set language:", err);
            });
    });
};

})();
