/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/detect-android.js
type: application/javascript
module-type: startup

Early Android detection - runs before render to set platform flags

\*/
(function() {

"use strict";

exports.name = "tiddlydesktop-rs-detect-android";
exports.platforms = ["browser"];
exports.before = ["startup"];
exports.synchronous = true;

exports.startup = function() {
	// Detect Android early so UI can render correctly
	var isAndroid = /android/i.test(navigator.userAgent);
	$tw.wiki.addTiddler({
		title: "$:/temp/tiddlydesktop-rs/is-android",
		text: isAndroid ? "yes" : "no"
	});
};

})();
