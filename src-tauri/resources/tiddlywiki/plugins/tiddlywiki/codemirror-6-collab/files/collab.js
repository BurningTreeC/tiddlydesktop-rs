/*\
title: $:/plugins/tiddlywiki/codemirror-6-collab/collab.js
type: application/javascript
module-type: codemirror6-plugin

Real-time collaborative editing via Yjs over TiddlyDesktop LAN Sync.
Bridges the CM6 editor with the window.TiddlyDesktop.collab transport API.

\*/

/*jslint node: true, browser: true */
/*global $tw: false */
"use strict";

if(!$tw.browser) return;

// Use Tauri's js_log command for logging (appears in stderr, unlike console.log in child processes)
function _clog(msg) {
	if(window.__TAURI__ && window.__TAURI__.core) {
		window.__TAURI__.core.invoke('js_log', { message: msg }).catch(function() {});
	}
	console.log(msg);
}

_clog("[Collab] Module loading...");

// Load the bundled Yjs + y-codemirror library
var yjsLib;
try {
	yjsLib = require("$:/plugins/tiddlywiki/codemirror-6-collab/lib/yjs-collab.js");
	_clog("[Collab] yjs-collab.js loaded, exports:" + Object.keys(yjsLib || {}).join(", "));
} catch(e) {
	_clog("[Collab] Failed to load yjs-collab.js: " + e.message);
	return;
}

var Y = yjsLib.Y;
// NOTE: We do NOT use yjsLib.yCollab. Its sync ViewPlugin (Pi) is a module-level
// singleton created with the ViewPlugin class from yjs-collab.js's own import of
// codemirror-view.js. If there's any module identity mismatch with the CM6 core's
// ViewPlugin class, the sync plugin is silently ignored and update() never fires.
// Instead, we implement the CM6 ↔ Y.Text sync directly using core.view.ViewPlugin
// (guaranteed to be the same class the editor uses). See _buildSyncPlugin().
var Awareness = yjsLib.Awareness;
var encodeAwarenessUpdate = yjsLib.encodeAwarenessUpdate;
var applyAwarenessUpdate = yjsLib.applyAwarenessUpdate;
var removeAwarenessStates = yjsLib.removeAwarenessStates;

// Encode Uint8Array to base64
function uint8ToBase64(uint8) {
	var binary = "";
	for(var i = 0; i < uint8.length; i++) {
		binary += String.fromCharCode(uint8[i]);
	}
	return btoa(binary);
}

// Decode base64 to Uint8Array
function base64ToUint8(base64) {
	var binary = atob(base64);
	var bytes = new Uint8Array(binary.length);
	for(var i = 0; i < binary.length; i++) {
		bytes[i] = binary.charCodeAt(i);
	}
	return bytes;
}

// Simple string hash for deterministic color assignment
function _hashString(str) {
	var hash = 0;
	for(var i = 0; i < str.length; i++) {
		hash = ((hash << 5) - hash) + str.charCodeAt(i);
		hash = hash & hash; // Convert to 32-bit integer
	}
	return Math.abs(hash);
}

// User colors — visually distinct, readable against both light and dark backgrounds
var _userColors = [
	{ color: "#30bced", light: "#30bced33" },
	{ color: "#6eeb83", light: "#6eeb8333" },
	{ color: "#ffbc42", light: "#ffbc4233" },
	{ color: "#ee6352", light: "#ee635233" },
	{ color: "#9ac2c9", light: "#9ac2c933" },
	{ color: "#1b9aaa", light: "#1b9aaa33" },
	{ color: "#c17767", light: "#c1776733" },
	{ color: "#b08ea2", light: "#b08ea233" },
	{ color: "#9370db", light: "#9370db33" },
	{ color: "#e07b53", light: "#e07b5333" },
	{ color: "#56b870", light: "#56b87033" },
	{ color: "#5b8def", light: "#5b8def33" }
];

// Get a deterministic color pair based on username
function _getUserColor(userName) {
	var idx = _hashString(userName) % _userColors.length;
	return _userColors[idx];
}

// Get the username from the wiki, falling back to "Anonymous"
function _getUserName(context) {
	var wiki = context.options && context.options.widget && context.options.widget.wiki;
	if(wiki) {
		var userName = wiki.getTiddlerText("$:/status/UserName");
		if(userName && userName.trim()) {
			return userName.trim();
		}
	}
	return "Anonymous";
}

// Per-engine collab state
var _nextId = 0;

// Module-level: compartment from last registerCompartments() call
var _lastCollabCompartment = null;

// Module-level registry of active collab engines by tiddler title
// Keyed by tiddlerTitle (the draft title, e.g. "Draft of 'Foo'")
var _activeEngines = {};
var _lifecycleListenersRegistered = false;

// Module-level registry of collab state by collabTitle (the original tiddler name).
// When TiddlyWiki recreates an editor widget (e.g., during refresh), we reuse the
// existing Y.Doc rather than creating a fresh one. This avoids duplicate text,
// orphaned listeners, and dedup cycles.
var _collabStateByTitle = {};

// Destroy collab session for a given tiddler title.
// Tries both the draft title and the original title (draft.of).
function _destroyCollabForTitle(title) {
	var engine = _activeEngines[title];
	if(!engine) {
		// Try looking up by collabTitle (original tiddler, not the draft)
		for(var key in _activeEngines) {
			if(_activeEngines.hasOwnProperty(key)) {
				var st = _activeEngines[key]._collabState;
				if(st && st.collabTitle === title) {
					engine = _activeEngines[key];
					break;
				}
			}
		}
	}
	if(engine && engine._collabState && !engine._collabState.destroyed) {
		_clog("[Collab] Destroying session for: " + title);
		exports.plugin.destroy(engine);
	}
}

// Register wiki change listener to detect when draft tiddlers are deleted
// (happens on save, cancel, rename, delete). This is the most reliable
// approach since TW5 doesn't have widget destroy hooks yet and TW messages
// may carry changed titles that don't match the original draft title.
function _ensureLifecycleListeners() {
	if(_lifecycleListenersRegistered) return;
	if(!$tw || !$tw.wiki || !$tw.wiki.addEventListener) return;
	_lifecycleListenersRegistered = true;

	// Primary: wiki change listener — catches ALL draft deletions
	$tw.wiki.addEventListener("change", function(changes) {
		for(var title in changes) {
			if(changes[title].deleted && _activeEngines[title]) {
				_clog("[Collab] Draft deleted, destroying session: " + title);
				_destroyCollabForTitle(title);
			}
		}
	});

	// Secondary: TW message listeners as backup (e.g. tm-close-tiddler
	// removes from story river without deleting the draft in some configs)
	if($tw.rootWidget) {
		var msgs = ["tm-save-tiddler", "tm-cancel-tiddler", "tm-delete-tiddler", "tm-close-tiddler"];
		for(var i = 0; i < msgs.length; i++) {
			(function(msg) {
				$tw.rootWidget.addEventListener(msg, function(event) {
					if(event.param) {
						_clog("[Collab] " + msg + ": " + event.param);
						_destroyCollabForTitle(event.param);
					}
				});
			})(msgs[i]);
		}
	}

	_clog("[Collab] Lifecycle listeners registered");
}

// ============================================================================
// Custom remote selection rendering (replicates y-codemirror.next's appearance)
// ============================================================================

// Create the remote caret widget DOM element
function _createCaretDOM(color, name) {
	var span = document.createElement("span");
	span.className = "cm-ySelectionCaret";
	span.style.backgroundColor = color;
	span.style.borderColor = color;

	// Zero-width space for positioning
	span.appendChild(document.createTextNode("\u2060"));

	// Colored dot above caret
	var dot = document.createElement("div");
	dot.className = "cm-ySelectionCaretDot";
	span.appendChild(dot);

	span.appendChild(document.createTextNode("\u2060"));

	// Username label (shows on hover)
	var info = document.createElement("div");
	info.className = "cm-ySelectionInfo";
	info.appendChild(document.createTextNode(name));
	span.appendChild(info);

	span.appendChild(document.createTextNode("\u2060"));

	return span;
}

// Build the remote selections base theme (same CSS as y-codemirror.next)
function _buildRemoteSelectionsTheme(EditorView) {
	return EditorView.baseTheme({
		".cm-ySelection": {},
		".cm-yLineSelection": {
			padding: 0,
			margin: "0px 2px 0px 4px"
		},
		".cm-ySelectionCaret": {
			position: "relative",
			borderLeft: "1px solid black",
			borderRight: "1px solid black",
			marginLeft: "-1px",
			marginRight: "-1px",
			boxSizing: "border-box",
			display: "inline"
		},
		".cm-ySelectionCaretDot": {
			borderRadius: "50%",
			position: "absolute",
			width: ".4em",
			height: ".4em",
			top: "-.2em",
			left: "-.2em",
			backgroundColor: "inherit",
			transition: "transform .3s ease-in-out",
			boxSizing: "border-box"
		},
		".cm-ySelectionCaret:hover > .cm-ySelectionCaretDot": {
			transformOrigin: "bottom center",
			transform: "scale(0)"
		},
		".cm-ySelectionInfo": {
			position: "absolute",
			top: "-1.05em",
			left: "-1px",
			fontSize: ".75em",
			fontFamily: "serif",
			fontStyle: "normal",
			fontWeight: "normal",
			lineHeight: "normal",
			userSelect: "none",
			color: "white",
			paddingLeft: "2px",
			paddingRight: "2px",
			zIndex: 101,
			transition: "opacity .3s ease-in-out",
			backgroundColor: "inherit",
			opacity: 0,
			transitionDelay: "0s",
			whiteSpace: "nowrap"
		},
		".cm-ySelectionCaret:hover > .cm-ySelectionInfo": {
			opacity: 1,
			transitionDelay: "0s"
		}
	});
}

// Build the ViewPlugin for remote selections
// This replicates y-codemirror.next's YRemoteSelectionsPluginValue exactly,
// but reads from engine._collabState instead of ySyncFacet.
function _buildRemoteSelectionsPlugin(core, collabState) {
	var ViewPlugin = core.view.ViewPlugin;
	var Decoration = core.view.Decoration;
	var WidgetType = core.view.WidgetType;
	var Annotation = core.state.Annotation;
	var yRemoteSelectionsAnnotation = Annotation.define();
	var awareness = collabState.awareness;
	var ytext = collabState.ytext;
	var ydoc = collabState.doc;

	// Remote caret widget class (extends CM6 WidgetType)
	class YRemoteCaretWidget extends WidgetType {
		constructor(color, name) {
			super();
			this.color = color;
			this.name = name;
		}

		toDOM() {
			return _createCaretDOM(this.color, this.name);
		}

		eq(widget) {
			return widget.color === this.color;
		}

		compare(widget) {
			return widget.color === this.color;
		}

		updateDOM() {
			return false;
		}

		get estimatedHeight() { return -1; }

		ignoreEvent() {
			return true;
		}
	}

	// Remote selections ViewPlugin class
	class RemoteSelectionsPlugin {
		constructor(view) {
			this.decorations = Decoration.set([]);
			var self = this;
			this._listener = function(changes) {
				var clients = changes.added.concat(changes.updated).concat(changes.removed);
				var hasRemote = false;
				for(var i = 0; i < clients.length; i++) {
					if(clients[i] !== awareness.doc.clientID) {
						hasRemote = true;
						break;
					}
				}
				if(hasRemote) {
					view.dispatch({ annotations: [yRemoteSelectionsAnnotation.of([])] });
				}
			};
			awareness.on("change", this._listener);
		}

		destroy() {
			awareness.off("change", this._listener);
		}

		update(viewUpdate) {
			if(collabState.destroyed) return;

			var decorations = [];
			var localState = awareness.getLocalState();

			// Update local cursor position in awareness
			if(localState != null) {
				var hasFocus = viewUpdate.view.hasFocus && viewUpdate.view.dom.ownerDocument.hasFocus();
				var sel = hasFocus ? viewUpdate.state.selection.main : null;
				var currentAnchor = localState.cursor == null ? null : Y.createRelativePositionFromJSON(localState.cursor.anchor);
				var currentHead = localState.cursor == null ? null : Y.createRelativePositionFromJSON(localState.cursor.head);

				if(sel != null) {
					var anchor = Y.createRelativePositionFromTypeIndex(ytext, sel.anchor);
					var head = Y.createRelativePositionFromTypeIndex(ytext, sel.head);
					if(localState.cursor == null || !Y.compareRelativePositions(currentAnchor, anchor) || !Y.compareRelativePositions(currentHead, head)) {
						awareness.setLocalStateField("cursor", {
							anchor: anchor,
							head: head
						});
					}
				} else if(localState.cursor != null && hasFocus) {
					awareness.setLocalStateField("cursor", null);
				}
			}

			// Build decorations for remote selections
			awareness.getStates().forEach(function(state, clientid) {
				if(clientid === awareness.doc.clientID) return;

				var cursor = state.cursor;
				if(cursor == null || cursor.anchor == null || cursor.head == null) return;

				var absAnchor = Y.createAbsolutePositionFromRelativePosition(cursor.anchor, ydoc);
				var absHead = Y.createAbsolutePositionFromRelativePosition(cursor.head, ydoc);
				if(absAnchor == null || absHead == null || absAnchor.type !== ytext || absHead.type !== ytext) return;

				var userName = (state.user && state.user.name) || "Anonymous";
				var color = (state.user && state.user.color) || "#30bced";
				var colorLight = (state.user && state.user.colorLight) || color + "33";

				var start = Math.min(absAnchor.index, absHead.index);
				var end = Math.max(absAnchor.index, absHead.index);
				var startLine = viewUpdate.view.state.doc.lineAt(start);
				var endLine = viewUpdate.view.state.doc.lineAt(end);

				if(startLine.number === endLine.number) {
					// Single line selection
					decorations.push({
						from: start,
						to: end,
						value: Decoration.mark({
							attributes: { style: "background-color: " + colorLight },
							"class": "cm-ySelection"
						})
					});
				} else {
					// Multi-line selection
					// First line
					decorations.push({
						from: start,
						to: startLine.from + startLine.length,
						value: Decoration.mark({
							attributes: { style: "background-color: " + colorLight },
							"class": "cm-ySelection"
						})
					});
					// Last line
					decorations.push({
						from: endLine.from,
						to: end,
						value: Decoration.mark({
							attributes: { style: "background-color: " + colorLight },
							"class": "cm-ySelection"
						})
					});
					// Middle lines
					for(var i = startLine.number + 1; i < endLine.number; i++) {
						var linePos = viewUpdate.view.state.doc.line(i).from;
						decorations.push({
							from: linePos,
							to: linePos,
							value: Decoration.line({
								attributes: { style: "background-color: " + colorLight, "class": "cm-yLineSelection" }
							})
						});
					}
				}

				// Cursor caret widget
				decorations.push({
					from: absHead.index,
					to: absHead.index,
					value: Decoration.widget({
						side: absHead.index - absAnchor.index > 0 ? -1 : 1,
						block: false,
						widget: new YRemoteCaretWidget(color, userName)
					})
				});
			});

			this.decorations = Decoration.set(decorations, true);
		}
	}

	return ViewPlugin.fromClass(RemoteSelectionsPlugin, {
		decorations: function(v) { return v.decorations; }
	});
}

// ============================================================================
// Custom CM6 ↔ Y.Text sync ViewPlugin.
// This replaces yCollab's built-in sync plugin (Pi) to guarantee we use the
// SAME ViewPlugin class as the CM6 editor core. The bundled yjs-collab.js
// creates Pi=ViewPlugin.fromClass(Is) at module load time using its own import
// of codemirror-view.js. If TiddlyWiki's module system returns a different
// instance than what the CM6 core uses, Pi is silently ignored (update() never
// fires, so CM6 typing changes never reach Y.Text — exactly the bug we saw).
// ============================================================================
function _buildSyncPlugin(core, collabState) {
	var ViewPlugin = core.view.ViewPlugin;
	var Annotation = core.state.Annotation;

	var syncAnnotation = Annotation.define();
	var ytext = collabState.ytext;

	// Use the collabState object as the transaction origin.
	// The Y.Text observer checks origin === syncOrigin to skip feedback loops.
	var syncOrigin = collabState;
	collabState._syncOrigin = syncOrigin;

	// We must capture 'collabState' in this closure so the ViewPlugin instance
	// has access to the correct ytext/doc even if collabState is reused.
	var pluginClass = function(view) {
		this.view = view;
		this._ytext = ytext;
		this._syncOrigin = syncOrigin;
		this._destroyed = false;

		// Y.Text → CM6: observe Y.Text changes from remote peers
		var self = this;
		this._observer = function(event, transaction) {
			if(self._destroyed) return;
			if(transaction.origin === self._syncOrigin) return; // skip our own changes

			var delta = event.delta;
			var changes = [];
			var pos = 0;
			for(var i = 0; i < delta.length; i++) {
				var op = delta[i];
				if(op.insert != null) {
					changes.push({ from: pos, to: pos, insert: op.insert });
				} else if(op["delete"] != null) {
					changes.push({ from: pos, to: pos + op["delete"], insert: "" });
					pos += op["delete"];
				} else if(op.retain != null) {
					pos += op.retain;
				}
			}
			if(changes.length > 0) {
				try {
					// Dispatch remote change to CM6 editor (synchronous)
					view.dispatch({ changes: changes, annotations: [syncAnnotation.of(true)] });

					// CRITICAL: Also update the tiddler text to match.
					// Without this, TiddlyWiki's editTextWidget detects a mismatch
					// between CM6 doc and tiddler text, and resets CM6 to the old
					// tiddler text — causing an infinite insert/delete feedback loop.
					if($tw && $tw.wiki && collabState.tiddlerTitle) {
						var newText = ytext.toString();
						var editField = collabState._editField || "text";
						var tid = $tw.wiki.getTiddler(collabState.tiddlerTitle);
						if(tid && tid.fields[editField] !== newText) {
							var fields = {};
							fields[editField] = newText;
							$tw.wiki.addTiddler(new $tw.Tiddler(tid, fields, {modified: tid.fields.modified}));
						}
					}
				} catch(e) {
					_clog("[Collab] YSync dispatch error: " + (e && e.message ? e.message : String(e)));
				}
			}
		};

		this._ytext.observe(this._observer);
		_clog("[Collab] YSyncPlugin constructed for " + collabState.collabTitle);
	};

	pluginClass.prototype.update = function(viewUpdate) {
		// Skip if no doc changed
		if(!viewUpdate.docChanged) return;

		// Skip if this change came from Y.Text (our observer dispatched it)
		for(var i = 0; i < viewUpdate.transactions.length; i++) {
			if(viewUpdate.transactions[i].annotation(syncAnnotation) !== undefined) return;
		}

		// CM6 → Y.Text: apply editor changes to Y.Text
		var yt = this._ytext;
		var origin = this._syncOrigin;
		try {
			yt.doc.transact(function() {
				var adj = 0;
				viewUpdate.changes.iterChanges(function(fromA, toA, fromB, toB, inserted) {
					var text = inserted.sliceString(0, inserted.length, "\n");
					if(fromA !== toA) yt["delete"](fromA + adj, toA - fromA);
					if(text.length > 0) yt.insert(fromA + adj, text);
					adj += text.length - (toA - fromA);
				});
			}, origin);
		} catch(e) {
			_clog("[Collab] YSync update error: " + (e && e.message ? e.message : String(e)));
		}
	};

	pluginClass.prototype.destroy = function() {
		this._destroyed = true;
		this._ytext.unobserve(this._observer);
		_clog("[Collab] YSyncPlugin destroyed for " + collabState.collabTitle);
	};

	return ViewPlugin.fromClass(pluginClass);
}

// ============================================================================
// Phase 1: Create collab state and extensions (synchronous).
// Uses our custom sync plugin instead of yCollab's bundled one.
// ============================================================================
function _setupCollabExtensions(context, core) {
	_ensureLifecycleListeners();

	var tiddlerTitle = context.tiddlerTitle;
	var engine = context.engine;
	var EditorView = core.view.EditorView;

	// Use the underlying tiddler name (draft.of) as the collab channel so
	// drafts with different usernames still collaborate on the same document.
	var collabTitle = tiddlerTitle;
	var wiki = context.options && context.options.widget && context.options.widget.wiki;
	if(wiki) {
		var tiddler = wiki.getTiddler(tiddlerTitle);
		if(tiddler && tiddler.fields["draft.of"]) {
			collabTitle = tiddler.fields["draft.of"];
		}
	}

	// Check for existing Y.Doc for this collabTitle (editor widget recreated
	// during TW5 refresh). Reuse the Y.Doc to avoid duplicate text, orphaned
	// listeners, and dedup cycles.
	var existingState = _collabStateByTitle[collabTitle];
	if(existingState && !existingState.destroyed) {
		_clog("[Collab] Reusing Y.Doc for " + collabTitle + " (editor recreated)");

		// Remove old engine reference
		if(_activeEngines[existingState.tiddlerTitle]) {
			delete _activeEngines[existingState.tiddlerTitle];
		}

		// Update state to point to new engine/title
		existingState.tiddlerTitle = tiddlerTitle;
		engine._collabState = existingState;
		_activeEngines[tiddlerTitle] = engine;

		// Create fresh sync + remote selection extensions bound to the SAME Y.Text
		var syncPlugin = _buildSyncPlugin(core, existingState);
		var theme = _buildRemoteSelectionsTheme(EditorView);
		var plugin = _buildRemoteSelectionsPlugin(core, existingState);
		return [syncPlugin, theme, plugin];
	}

	// Create Yjs document and text type
	var doc = new Y.Doc();
	var ytext = doc.getText("content");

	// Debug: observe Y.Text changes
	ytext.observe(function(event) {
		try {
			_clog("[Collab] Y.Text CHANGED: delta=" + JSON.stringify(event.delta).substring(0, 200) + ", origin=" + event.transaction.origin + ", ytext.len=" + ytext.toString().length);
		} catch(_e) {}
	});

	// Create awareness for cursor/selection sharing
	var awareness = new Awareness(doc);
	var userName = _getUserName(context);
	var userColor = _getUserColor(userName);
	try {
		awareness.setLocalStateField("user", {
			name: userName,
			color: userColor.color,
			colorLight: userColor.light
		});
	} catch(_e) {}

	var _collabId = _nextId++;

	// Store collab state on engine
	var state = {
		id: _collabId,
		doc: doc,
		ytext: ytext,
		awareness: awareness,
		tiddlerTitle: tiddlerTitle,
		collabTitle: collabTitle,
		listeners: {},
		destroyed: false,
		_transportConnected: false,
		_awaitingRemoteState: false,
		_receivedRemoteState: false
	};
	engine._collabState = state;
	_activeEngines[tiddlerTitle] = engine;
	_collabStateByTitle[collabTitle] = state;

	// Insert tiddler text into Y.Text. Every editor starts as "first editor".
	// When transport connects (Phase 2), joining mode may clear this text
	// and replace it with the remote peer's state.
	var editField = (context.options && context.options.widget && context.options.widget.editField) || "text";
	state._editField = editField;
	var currentText = "";
	var tid = wiki ? wiki.getTiddler(tiddlerTitle) : null;
	if(tid && tid.fields[editField] !== undefined) {
		currentText = tid.fields[editField];
	} else if(wiki) {
		currentText = wiki.getTiddlerText(tiddlerTitle) || "";
	}
	if(currentText) {
		doc.transact(function() { ytext.insert(0, currentText); });
	}
	_clog("[Collab] Phase 1: inserted " + (currentText ? currentText.length : 0) + " chars for " + collabTitle);

	// Create sync + remote selections — ALWAYS in initial extensions.
	// Uses our custom sync plugin (not yCollab's bundled Pi) to guarantee
	// we use the same ViewPlugin class as the CM6 editor core.
	var syncPlugin = _buildSyncPlugin(core, state);
	var theme = _buildRemoteSelectionsTheme(EditorView);
	var plugin = _buildRemoteSelectionsPlugin(core, state);

	_clog("[Collab] Phase 1: sync plugin created (initial extensions) for " + collabTitle);
	return [syncPlugin, theme, plugin];
}

// ============================================================================
// Phase 2: Connect transport (requires collab API).
// Registers event listeners, determines first-editor vs joining mode,
// and handles state synchronization with remote peers.
// ============================================================================
function _connectTransport(engine, collab) {
	var state = engine._collabState;
	if(!state || state.destroyed || state._transportConnected) return;
	state._transportConnected = true;

	var collabTitle = state.collabTitle;
	var doc = state.doc;
	var ytext = state.ytext;
	var awareness = state.awareness;

	// --- Outbound: local Yjs changes → transport ---

	var onDocUpdate = function(update, origin) {
		if(state.destroyed) return;
		if(origin === "remote") return;
		try {
			collab.sendUpdate(collabTitle, uint8ToBase64(update));
		} catch(_e) {}
	};
	doc.on("update", onDocUpdate);
	state._onDocUpdate = onDocUpdate;

	var onAwarenessUpdate = function(changes) {
		if(state.destroyed) return;
		try {
			var update = encodeAwarenessUpdate(awareness, changes.added.concat(changes.updated).concat(changes.removed));
			collab.sendAwareness(collabTitle, uint8ToBase64(update));
		} catch(_e) {}
	};
	awareness.on("update", onAwarenessUpdate);
	state._onAwarenessUpdate = onAwarenessUpdate;

	// --- Inbound: transport → local Yjs doc ---

	state.listeners["collab-update"] = function(data) {
		if(state.destroyed) return;
		if(data.tiddler_title !== collabTitle) return;
		try {
			var update = base64ToUint8(data.update_base64);
			var ytextBefore = ytext.toString().length;
			_clog("[Collab] INBOUND update: " + update.length + " bytes for " + collabTitle + ", awaitingRemote=" + state._awaitingRemoteState + ", ytext.before=" + ytextBefore);

			// JOINING: on first remote update, clear our locally-inserted text
			// BEFORE applying the remote state. This avoids CRDT interleaving
			// (our items get tombstoned, remote items fill Y.Text cleanly).
			if(state._awaitingRemoteState) {
				state._awaitingRemoteState = false;
				if(state._joinTimer) { clearTimeout(state._joinTimer); state._joinTimer = null; }
				_clog("[Collab] Joining: clearing " + ytextBefore + " local chars before applying remote state for " + collabTitle);
				doc.transact(function() {
					if(ytext.length > 0) {
						ytext.delete(0, ytext.length);
					}
				});
				ytextBefore = 0;
			}

			Y.applyUpdate(doc, update, "remote");
			var ytextAfter = ytext.toString().length;

			// Dedup safety net: when both devices independently insert the
			// same text (both become first editors due to race condition),
			// Y.js merges them as two separate inserts → text doubles.
			// Delete the duplicate half. Y.js deletes reference items by ID,
			// so both devices can independently dedup and converge correctly.
			if(ytextBefore > 0 && ytextAfter === ytextBefore * 2) {
				var afterStr = ytext.toString();
				if(afterStr.substring(0, ytextBefore) === afterStr.substring(ytextBefore)) {
					_clog("[Collab] DEDUP: removing duplicate " + ytextBefore + " chars for " + collabTitle);
					doc.transact(function() {
						ytext.delete(ytextBefore, ytextBefore);
					});
					ytextAfter = ytext.toString().length;
				}
			}

			state._receivedRemoteState = true;
			_clog("[Collab] After update: ytext.after=" + ytextAfter + (ytextAfter !== ytextBefore ? " (CHANGED)" : " (unchanged)"));
		} catch(_e) {
			_clog("[Collab] INBOUND update error: " + (_e && _e.message ? _e.message : String(_e)));
		}
	};

	state.listeners["collab-awareness"] = function(data) {
		if(state.destroyed) return;
		if(data.tiddler_title !== collabTitle) return;
		try {
			var update = base64ToUint8(data.update_base64);
			applyAwarenessUpdate(awareness, update, "remote");
		} catch(_e) {}
	};

	// When a new editor joins, send our full state so they can sync
	state.listeners["editing-started"] = function(data) {
		if(state.destroyed) return;
		if(data.tiddler_title !== collabTitle) return;
		_clog("[Collab] editing-started for " + collabTitle + " from " + (data.device_id || "?") + ", awaitingRemote=" + state._awaitingRemoteState);

		// Don't respond if we're still joining (our state isn't authoritative)
		if(state._awaitingRemoteState) return;

		try {
			var fullState = Y.encodeStateAsUpdate(doc);
			_clog("[Collab] Sending full state (" + fullState.length + " bytes) to peer for " + collabTitle);
			collab.sendUpdate(collabTitle, uint8ToBase64(fullState));
			var awarenessUpdate = encodeAwarenessUpdate(awareness, [doc.clientID]);
			collab.sendAwareness(collabTitle, uint8ToBase64(awarenessUpdate));
		} catch(_e) {
			_clog("[Collab] editing-started handler error: " + (_e && _e.message ? _e.message : String(_e)));
		}
	};

	// When a peer stops editing, clean up their awareness (stale cursors)
	state.listeners["editing-stopped"] = function(data) {
		if(state.destroyed) return;
		if(data.tiddler_title !== collabTitle) return;
		_clog("[Collab] editing-stopped for " + collabTitle + " from " + (data.device_id || "?"));
		try {
			var states = awareness.getStates();
			var remoteClientIds = [];
			states.forEach(function(_s, clientId) {
				if(clientId !== doc.clientID) {
					remoteClientIds.push(clientId);
				}
			});
			if(remoteClientIds.length > 0) {
				_clog("[Collab] Removing " + remoteClientIds.length + " stale awareness states");
				removeAwarenessStates(awareness, remoteClientIds, "peer-disconnected");
			}
		} catch(_e) {}
	};

	// Register all event listeners
	for(var eventName in state.listeners) {
		if(state.listeners.hasOwnProperty(eventName)) {
			collab.on(eventName, state.listeners[eventName]);
		}
	}

	// Determine first-editor vs joining.
	// Desktop has getRemoteEditorsAsync (async Tauri command).
	// Android may only have getRemoteEditors (synchronous) or neither.
	function _onEditorsResolved(editors) {
		if(state.destroyed) return;

		var hasRemote = editors && editors.length > 0;
		_clog("[Collab] Phase 2: hasRemoteEditors=" + hasRemote + " for " + collabTitle + " (editors: " + JSON.stringify(editors) + ")");

		if(hasRemote) {
			// JOINING: mark that the next collab-update should clear our
			// locally-inserted text before applying the remote state.
			// The editor keeps showing our text until then (no blank flicker).
			state._awaitingRemoteState = true;
			// Fallback: if no remote state arrives in 5s, stop waiting
			// (the editor keeps our locally-inserted text which is fine)
			state._joinTimer = setTimeout(function() {
				if(state._awaitingRemoteState && !state.destroyed) {
					state._awaitingRemoteState = false;
					_clog("[Collab] Join timeout (5s): keeping local text for " + collabTitle);
				}
			}, 5000);
		}

		// Notify peers that we're editing (triggers them to send full state)
		try {
			collab.startEditing(collabTitle);
		} catch(_e) {}

		// Always send our full Y.Doc state after announcing ourselves.
		// This ensures the remote peer gets our items even if they started
		// editing before we were listening for their EditingStarted event.
		// Without this, the peer only gets our dedup deletes but never our items.
		try {
			var fullState = Y.encodeStateAsUpdate(doc);
			_clog("[Collab] Phase 2: sending full state (" + fullState.length + " bytes) for " + collabTitle);
			collab.sendUpdate(collabTitle, uint8ToBase64(fullState));
			var awarenessUpdate = encodeAwarenessUpdate(awareness, [doc.clientID]);
			collab.sendAwareness(collabTitle, uint8ToBase64(awarenessUpdate));
		} catch(_e) {}
	}

	if(typeof collab.getRemoteEditorsAsync === "function") {
		collab.getRemoteEditorsAsync(collabTitle).then(_onEditorsResolved);
	} else if(typeof collab.getRemoteEditors === "function") {
		_onEditorsResolved(collab.getRemoteEditors(collabTitle) || []);
	} else {
		// No editor query API — assume no remote editors (first editor mode)
		_clog("[Collab] Phase 2: no getRemoteEditors API, assuming first editor for " + collabTitle);
		_onEditorsResolved([]);
	}
}


exports.plugin = {
	name: "collab",
	description: "Real-time collaborative editing via Yjs",
	priority: 50,

	init: function(cm6Core) {
		this._core = cm6Core;
	},

	registerCompartments: function() {
		_lastCollabCompartment = new this._core.state.Compartment();
		return { collab: _lastCollabCompartment };
	},

	condition: function(context) {
		var wiki = context.options && context.options.widget && context.options.widget.wiki;
		var enabled = wiki && wiki.getTiddlerText("$:/config/codemirror-6/collab/enabled") !== "no";
		_clog("[Collab] condition: tiddlerTitle=" + (context.tiddlerTitle || "none") + ", enabled=" + enabled);
		if(!enabled) return false;
		if(!context.tiddlerTitle) return false;
		return true;
	},

	getExtensions: function(context) {
		var compartment = _lastCollabCompartment;
		var tiddlerTitle = context.tiddlerTitle;
		if(!tiddlerTitle) return [compartment.of([])];

		// Phase 1: Always create yCollab extensions (synchronous, no API needed)
		// If an existing Y.Doc is reused, _setupCollabExtensions returns extensions
		// bound to it and the transport is already connected (skip Phase 2).
		_clog("[Collab] getExtensions for " + tiddlerTitle + ", API=" + !!(window.TiddlyDesktop && window.TiddlyDesktop.collab));
		try {
			var exts = _setupCollabExtensions(context, this._core);
			var engine = context.engine;
			var state = engine._collabState;

			// Skip Phase 2 if transport is already connected (reused Y.Doc)
			if(state && state._transportConnected) {
				_clog("[Collab] Transport already connected, skipping Phase 2 for " + tiddlerTitle);
				return [compartment.of(exts)];
			}

			var collab = window.TiddlyDesktop && window.TiddlyDesktop.collab;

			if(collab) {
				// Phase 2: Transport available — connect immediately.
				// Wrapped in its own try/catch so errors don't discard extensions.
				try {
					_connectTransport(engine, collab);
				} catch(ce) {
					_clog("[Collab] _connectTransport error (non-fatal): " + (ce && ce.message ? ce.message : String(ce)));
				}
			} else {
				// Collab API not available (running outside TiddlyDesktop or unexpected state)
				_clog("[Collab] WARNING: window.TiddlyDesktop.collab not found for " + tiddlerTitle + " — collab transport not connected");
			}

			return [compartment.of(exts)];
		} catch(e) {
			_clog("[Collab] getExtensions ERROR: " + (e && e.message ? e.message : String(e)) + "\n" + (e && e.stack ? e.stack : ""));
			return [compartment.of([])];
		}
	},

	destroy: function(engine) {
		var state = engine._collabState;
		if(!state || state.destroyed) return;
		state.destroyed = true;

		// Clean up join timer
		if(state._joinTimer) {
			clearTimeout(state._joinTimer);
			state._joinTimer = null;
		}

		// Notify peers and unregister listeners (only if transport was connected)
		var collab = window.TiddlyDesktop && window.TiddlyDesktop.collab;
		if(collab && state._transportConnected) {
			try {
				collab.stopEditing(state.collabTitle);
			} catch(_e) {}

			for(var eventName in state.listeners) {
				if(state.listeners.hasOwnProperty(eventName)) {
					try {
						collab.off(eventName, state.listeners[eventName]);
					} catch(_e2) {}
				}
			}
		}

		// Clean up Yjs
		if(state._onDocUpdate) {
			state.doc.off("update", state._onDocUpdate);
		}
		if(state._onAwarenessUpdate) {
			state.awareness.off("update", state._onAwarenessUpdate);
		}

		try {
			removeAwarenessStates(state.awareness, [state.doc.clientID], "destroy");
		} catch(_e) {}

		try {
			state.awareness.destroy();
		} catch(_e) {}

		try {
			state.doc.destroy();
		} catch(_e) {}

		state.listeners = {};
		if(_activeEngines[state.tiddlerTitle] === engine) {
			delete _activeEngines[state.tiddlerTitle];
		}
		if(_collabStateByTitle[state.collabTitle] === state) {
			delete _collabStateByTitle[state.collabTitle];
		}
		engine._collabState = null;
		_clog("[Collab] Session destroyed for: " + state.collabTitle);
	},

	extendAPI: function(engine, context) {
		return {
			getCollabEditors: function() {
				var collab = window.TiddlyDesktop && window.TiddlyDesktop.collab;
				var state = this._collabState;
				if(!collab || !state) return [];
				try {
					return collab.getRemoteEditors(state.collabTitle) || [];
				} catch(_e) {
					return [];
				}
			},

			isCollabActive: function() {
				var state = this._collabState;
				return !!(state && !state.destroyed);
			}
		};
	}
};
