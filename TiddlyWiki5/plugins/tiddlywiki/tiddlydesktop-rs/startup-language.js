/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/startup-language.js
type: application/javascript
module-type: startup

Initialize UI language from TiddlyDesktop setting

\*/
(function(){

/*jslint node: true, browser: true */
/*global $tw: false */
"use strict";

exports.name = "tiddlydesktop-language";
exports.after = ["startup"];
exports.before = ["render"];
exports.synchronous = false;

exports.startup = function(callback) {
    // Only run on landing page (main wiki)
    if (typeof window === "undefined" || !window.__IS_MAIN_WIKI__) {
        callback();
        return;
    }

    // Check if Tauri is available
    if (!window.__TAURI__) {
        // Fallback to init script value or default
        var language = window.__TIDDLYDESKTOP_LANGUAGE__ || "en-GB";
        $tw.wiki.addTiddler({
            title: "$:/temp/tiddlydesktop-rs/language",
            text: language
        });
        callback();
        return;
    }

    // Fetch language from Tauri backend (handles user preference vs system detection)
    window.__TAURI__.core.invoke("get_language")
        .then(function(language) {
            console.log("[TiddlyDesktop] Fetched language from backend:", language);
            $tw.wiki.addTiddler({
                title: "$:/temp/tiddlydesktop-rs/language",
                text: language
            });
            callback();
        })
        .catch(function(err) {
            console.error("[TiddlyDesktop] Failed to get language:", err);
            // Fallback to init script value or default
            var language = window.__TIDDLYDESKTOP_LANGUAGE__ || "en-GB";
            $tw.wiki.addTiddler({
                title: "$:/temp/tiddlydesktop-rs/language",
                text: language
            });
            callback();
        });
};

})();
