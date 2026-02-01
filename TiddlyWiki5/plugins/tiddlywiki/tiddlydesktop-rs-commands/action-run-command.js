/*\
title: $:/plugins/tiddlywiki/tiddlydesktop-rs-commands/action-run-command.js
type: application/javascript
module-type: widget

Action widget to run shell commands via TiddlyDesktop-RS

\*/

"use strict";

var Widget = require("$:/core/modules/widgets/widget.js").widget;

var RunCommandWidget = function(parseTreeNode, options) {
	this.initialise(parseTreeNode, options);
};

/*
Inherit from the base widget class
*/
RunCommandWidget.prototype = new Widget();

/*
Render this widget into the DOM
*/
RunCommandWidget.prototype.render = function(parent, nextSibling) {
	this.computeAttributes();
	this.execute();
};

/*
Compute the internal state of the widget
*/
RunCommandWidget.prototype.execute = function() {
	this.actionCommand = this.getAttribute("$command");
	this.actionArgs = this.getAttribute("$args");
	this.actionWorkingDir = this.getAttribute("$workingDir");
	this.actionWait = this.getAttribute("$wait", "no") === "yes";
	this.actionConfirm = this.getAttribute("$confirm", "yes") === "yes";
	this.actionOutputTiddler = this.getAttribute("$outputTiddler");
	this.actionOutputField = this.getAttribute("$outputField", "text");
	this.actionExitCodeField = this.getAttribute("$exitCodeField");
	this.actionStderrField = this.getAttribute("$stderrField");
};

/*
Refresh the widget by ensuring our attributes are up to date
*/
RunCommandWidget.prototype.refresh = function(changedTiddlers) {
	return this.refreshChildren(changedTiddlers);
};

/*
Invoke the action associated with this widget
*/
RunCommandWidget.prototype.invokeAction = function(triggeringWidget, event) {
	var self = this;

	// Check if we're in a TiddlyDesktop-RS environment
	if(typeof window === "undefined" || !window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
		console.warn("action-run-command: Not running in TiddlyDesktop-RS environment");
		return true;
	}

	// Check if command execution permission has been granted
	var permissionTiddler = this.wiki.getTiddler("$:/temp/TiddlyDesktop/CommandPermission");
	var permissionStatus = permissionTiddler ? permissionTiddler.fields.status : null;
	if(permissionStatus !== "granted") {
		console.warn("action-run-command: Command execution not permitted. Status: " + permissionStatus);
		// Store error in output tiddler if specified
		if(this.actionOutputTiddler) {
			var fields = {};
			fields[this.actionOutputField] = "";
			fields["success"] = "no";
			fields["error"] = "Command execution not permitted for this wiki. Install the tiddlydesktop-rs-commands plugin and approve the permission request.";
			this.wiki.addTiddler(new $tw.Tiddler(
				this.wiki.getTiddler(this.actionOutputTiddler),
				{title: this.actionOutputTiddler},
				fields,
				this.wiki.getModificationFields()
			));
		}
		return true;
	}

	if(!this.actionCommand) {
		console.warn("action-run-command: No command specified");
		return true;
	}

	// Parse args - split by spaces, respecting quoted strings
	var args = null;
	if(this.actionArgs) {
		args = this.parseArgs(this.actionArgs);
	}

	// Build the invoke parameters
	var params = {
		command: this.actionCommand,
		args: args,
		workingDir: this.actionWorkingDir || null,
		wait: this.actionWait,
		confirm: this.actionConfirm
	};

	// Call the Tauri command
	window.__TAURI__.core.invoke("run_command", params)
		.then(function(result) {
			// If we have a result (waited) and output tiddler is specified
			if(result && self.actionOutputTiddler) {
				var fields = {};
				fields[self.actionOutputField] = result.stdout || "";

				if(self.actionExitCodeField) {
					fields[self.actionExitCodeField] = String(result.exit_code !== null ? result.exit_code : "");
				}

				if(self.actionStderrField) {
					fields[self.actionStderrField] = result.stderr || "";
				}

				// Add success field
				fields["success"] = result.success ? "yes" : "no";

				self.wiki.addTiddler(new $tw.Tiddler(
					self.wiki.getTiddler(self.actionOutputTiddler),
					{title: self.actionOutputTiddler},
					fields,
					self.wiki.getModificationFields()
				));
			}
		})
		.catch(function(err) {
			console.error("action-run-command: Error executing command:", err);

			// Store error in output tiddler if specified
			if(self.actionOutputTiddler) {
				var fields = {};
				fields[self.actionOutputField] = "";
				fields["success"] = "no";
				fields["error"] = String(err);

				self.wiki.addTiddler(new $tw.Tiddler(
					self.wiki.getTiddler(self.actionOutputTiddler),
					{title: self.actionOutputTiddler},
					fields,
					self.wiki.getModificationFields()
				));
			}
		});

	return true; // Action was invoked
};

/*
Parse arguments string, respecting quoted strings
*/
RunCommandWidget.prototype.parseArgs = function(argsString) {
	var args = [];
	var current = "";
	var inQuote = false;
	var quoteChar = "";

	for(var i = 0; i < argsString.length; i++) {
		var char = argsString[i];

		if(!inQuote && (char === '"' || char === "'")) {
			inQuote = true;
			quoteChar = char;
		} else if(inQuote && char === quoteChar) {
			inQuote = false;
			quoteChar = "";
		} else if(!inQuote && char === " ") {
			if(current.length > 0) {
				args.push(current);
				current = "";
			}
		} else {
			current += char;
		}
	}

	if(current.length > 0) {
		args.push(current);
	}

	return args.length > 0 ? args : null;
};

exports["action-run-command"] = RunCommandWidget;
