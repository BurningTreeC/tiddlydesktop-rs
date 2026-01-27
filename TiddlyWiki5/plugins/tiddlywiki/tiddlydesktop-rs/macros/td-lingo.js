/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/macros/td-lingo.js
type: application/javascript
module-type: macro

TiddlyDesktop language macro - looks up translations based on current language.

Usage: <<td-lingo "Buttons/Open">>

Looks for: $:/plugins/tiddlywiki/tiddlydesktop-rs/languages/<lang>/<key>
Falls back to en-GB if not found.

\*/
(function(){

/*jslint node: true, browser: true */
/*global $tw: false */
"use strict";

exports.name = "td-lingo";
exports.params = [
    {name: "key"}
];

var BASE_PATH = "$:/plugins/tiddlywiki/tiddlydesktop-rs/languages/";
var FALLBACK_LANG = "en-GB";

exports.run = function(key) {
    // Get current language from temp tiddler (set by startup module)
    var currentLang = this.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/language", FALLBACK_LANG);

    // Try current language first
    var title = BASE_PATH + currentLang + "/" + key;
    var text = this.wiki.getTiddlerText(title);

    // Fall back to en-GB if not found
    if (!text && currentLang !== FALLBACK_LANG) {
        title = BASE_PATH + FALLBACK_LANG + "/" + key;
        text = this.wiki.getTiddlerText(title);
    }

    return text || key; // Return key itself if no translation found
};

})();
