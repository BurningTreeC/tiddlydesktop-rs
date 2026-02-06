/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs/platform-macros.js
type: application/javascript
module-type: macro

Platform detection macros

\*/
(function(){

"use strict";

exports.name = "td-is-android";

exports.params = [];

exports.run = function() {
	if(typeof navigator !== "undefined" && navigator.userAgent) {
		return /android/i.test(navigator.userAgent) ? "yes" : "no";
	}
	return "no";
};

})();
