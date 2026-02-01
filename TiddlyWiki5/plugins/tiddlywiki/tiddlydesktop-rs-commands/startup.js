/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs-commands/startup.js
type: application/javascript
module-type: startup

Startup module to request command execution permission from TiddlyDesktop-RS

\*/

"use strict";

exports.name = "tiddlydesktop-rs-commands-startup";
exports.platforms = ["browser"];
exports.after = ["startup"];
exports.synchronous = false;

exports.startup = function(callback) {
	// Check if we're in a TiddlyDesktop-RS environment
	if(typeof window === "undefined" || !window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
		// Not in TiddlyDesktop-RS, skip
		callback();
		return;
	}

	// Check if the config tiddler exists (user has intentionally installed the plugin)
	if(!$tw.wiki.tiddlerExists("$:/config/TiddlyDesktop/AllowRunCommand")) {
		// Config tiddler doesn't exist, skip
		callback();
		return;
	}

	// Set pending status
	$tw.wiki.addTiddler({
		title: "$:/temp/TiddlyDesktop/CommandPermission",
		status: "pending"
	});

	// First check if we already have permission
	window.__TAURI__.core.invoke("check_run_command_permission")
		.then(function(hasPermission) {
			if(hasPermission) {
				// Already have permission
				$tw.wiki.addTiddler({
					title: "$:/temp/TiddlyDesktop/CommandPermission",
					status: "granted"
				});
				callback();
			} else {
				// Request permission
				window.__TAURI__.core.invoke("request_run_command_permission")
					.then(function(granted) {
						$tw.wiki.addTiddler({
							title: "$:/temp/TiddlyDesktop/CommandPermission",
							status: granted ? "granted" : "denied"
						});
						callback();
					})
					.catch(function(err) {
						console.error("TiddlyDesktop-RS: Failed to request run_command permission:", err);
						$tw.wiki.addTiddler({
							title: "$:/temp/TiddlyDesktop/CommandPermission",
							status: "denied",
							error: String(err)
						});
						callback();
					});
			}
		})
		.catch(function(err) {
			console.error("TiddlyDesktop-RS: Failed to check run_command permission:", err);
			$tw.wiki.addTiddler({
				title: "$:/temp/TiddlyDesktop/CommandPermission",
				status: "denied",
				error: String(err)
			});
			callback();
		});
};
