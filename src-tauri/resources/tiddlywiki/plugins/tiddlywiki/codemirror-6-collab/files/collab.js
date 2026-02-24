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

// Fields excluded from Y.Map sync (handled separately or immutable)
var _YMAP_EXCLUDED_FIELDS = {
	"title": true, "created": true, "modified": true, "modifier": true,
	"creator": true, "draft.of": true, "revision": true, "bag": true
};

// Check if a field is hard-excluded from Y.Map sync (immutable/internal fields).
function _isFieldHardExcluded(fieldName) {
	return !!_YMAP_EXCLUDED_FIELDS[fieldName];
}

// Get the Y.Text key for a given edit field.
// "text" → "content" (backward compat), others → "field:" + name.
function _ytextKeyForField(editField) {
	return editField === "text" ? "content" : "field:" + editField;
}

// Module-level registry of active collab engines by tiddler title
// Keyed by tiddlerTitle (the draft title, e.g. "Draft of 'Foo'")
var _activeEngines = {};
var _lifecycleListenersRegistered = false;

// Module-level registry of collab state by collabTitle (the original tiddler name).
// When TiddlyWiki recreates an editor widget (e.g., during refresh), we reuse the
// existing Y.Doc rather than creating a fresh one. This avoids duplicate text,
// orphaned listeners, and dedup cycles.
var _collabStateByTitle = {};

// Show a transient notification banner for collab events
function _showCollabBanner(message, duration) {
	var banner = document.createElement("div");
	banner.className = "td-collab-banner";
	banner.textContent = message;
	banner.style.cssText = "position:fixed;top:0;left:0;right:0;padding:8px 16px;background:#2196F3;color:white;text-align:center;z-index:10000;font-size:14px;opacity:1;transition:opacity 0.5s;";
	document.body.appendChild(banner);
	setTimeout(function() {
		banner.style.opacity = "0";
		setTimeout(function() { banner.remove(); }, 500);
	}, duration || 4000);
}

// Destroy ALL collab field editors for a given tiddler title.
// Keys in _activeEngines are "tiddlerTitle\0editField".
// Also matches by collabTitle (original tiddler, not the draft).
function _destroyCollabForTitle(title) {
	var enginesToDestroy = [];
	for(var key in _activeEngines) {
		if(!_activeEngines.hasOwnProperty(key)) continue;
		var eng = _activeEngines[key];
		var tTitle = key.split("\0")[0];
		if(tTitle === title) {
			enginesToDestroy.push(eng);
			continue;
		}
		var st = eng._collabState;
		if(st && st.collabTitle === title) {
			enginesToDestroy.push(eng);
		}
	}
	for(var i = 0; i < enginesToDestroy.length; i++) {
		var eng = enginesToDestroy[i];
		if(eng._collabState && !eng._collabState.destroyed) {
			_clog("[Collab] Destroying session for: " + title);
			exports.plugin.destroy(eng);
		}
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
			if(!changes[title].deleted) continue;
			// Check if any _activeEngines key starts with "title\0"
			var hasEngine = false;
			var prefix = title + "\0";
			for(var key in _activeEngines) {
				if(key.substring(0, prefix.length) === prefix) {
					hasEngine = true;
					break;
				}
			}
			if(hasEngine) {
				_clog("[Collab] Draft deleted, destroying session: " + title);
				_destroyCollabForTitle(title);
			}
		}
	});

	// Secondary: TW message listeners as backup (e.g. tm-close-tiddler
	// removes from story river without deleting the draft in some configs)
	if($tw.rootWidget) {
		// Intercept tm-save-tiddler to broadcast peer-saved BEFORE destroying
		$tw.rootWidget.addEventListener("tm-save-tiddler", function(event) {
			if(!event.param) return;
			var draftTitle = event.param;
			_clog("[Collab] tm-save-tiddler: " + draftTitle);
			// Find any engine for this draft (any field shares the Y.Doc)
			var foundEngine = null;
			var prefix = draftTitle + "\0";
			for(var key in _activeEngines) {
				if(!_activeEngines.hasOwnProperty(key)) continue;
				if(key.substring(0, prefix.length) === prefix) {
					foundEngine = _activeEngines[key];
					break;
				}
				var st = _activeEngines[key]._collabState;
				if(st && st.collabTitle === draftTitle) {
					foundEngine = _activeEngines[key];
					break;
				}
			}
			if(foundEngine && foundEngine._collabState && !foundEngine._collabState.destroyed) {
				var state = foundEngine._collabState;
				var collab = window.TiddlyDesktop && window.TiddlyDesktop.collab;
				if(collab && state._transportConnected) {
					// Get the title the tiddler is being saved as
					var tid = $tw.wiki.getTiddler(state.tiddlerTitle);
					var savedTitle = (tid && tid.fields["draft.title"]) || state.collabTitle;
					_clog("[Collab] Broadcasting peer-saved: collabTitle=" + state.collabTitle + " savedTitle=" + savedTitle);
					// Send final Y.Doc state so peers get any last changes
					try {
						var finalState = Y.encodeStateAsUpdate(state.doc);
						collab.sendUpdate(state.collabTitle, uint8ToBase64(finalState));
					} catch(_e) {
						_clog("[Collab] peer-saved final state send error: " + (_e && _e.message ? _e.message : String(_e)));
					}
					// Broadcast peer-saved message
					collab.peerSaved(state.collabTitle, savedTitle);
				}
			}
			_destroyCollabForTitle(draftTitle);
		});

		var msgs = ["tm-cancel-tiddler", "tm-delete-tiddler", "tm-close-tiddler"];
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

	// After mount: detect if dot/info would be clipped (single-line field)
	// and flip them below the caret instead of above.
	requestAnimationFrame(function() {
		if(!span.parentNode) return;
		var editor = span.closest(".cm-editor");
		if(!editor) return;
		var editorRect = editor.getBoundingClientRect();
		var spanRect = span.getBoundingClientRect();
		// If the caret is close to the top of the editor (within 1.5em ≈ 24px),
		// the above-positioned elements would be clipped
		if(spanRect.top - editorRect.top < 24) {
			span.classList.add("cm-ySelectionCaret-below");
		}
	});

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
		},
		// Flipped positioning for single-line / overflow-hidden editors
		".cm-ySelectionCaret-below > .cm-ySelectionCaretDot": {
			top: "auto",
			bottom: "-.2em"
		},
		".cm-ySelectionCaret-below > .cm-ySelectionInfo": {
			top: "auto",
			bottom: "-1.05em"
		}
	});
}

// Build the ViewPlugin for remote selections
// This replicates y-codemirror.next's YRemoteSelectionsPluginValue exactly,
// but reads from engine._collabState instead of ySyncFacet.
function _buildRemoteSelectionsPlugin(core, collabState, fieldState) {
	var ViewPlugin = core.view.ViewPlugin;
	var Decoration = core.view.Decoration;
	var WidgetType = core.view.WidgetType;
	var Annotation = core.state.Annotation;
	var yRemoteSelectionsAnnotation = Annotation.define();
	var awareness = collabState.awareness;
	var ytext = fieldState.ytext;
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
function _buildSyncPlugin(core, collabState, fieldState) {
	var ViewPlugin = core.view.ViewPlugin;
	var Annotation = core.state.Annotation;

	var syncAnnotation = Annotation.define();
	var ytext = fieldState.ytext;
	var syncOrigin = fieldState.syncOrigin;
	var editField = fieldState.editField;

	// We must capture 'collabState' and 'fieldState' in this closure so the
	// ViewPlugin instance has access to the correct ytext/doc.
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
						var tid = $tw.wiki.getTiddler(collabState.tiddlerTitle);
						if(tid && tid.fields[editField] !== newText) {
							var fields = {};
							fields[editField] = newText;
							// Update tiddler store SILENTLY (no change/refresh event).
					// Using addTiddler would enqueue a deferred refresh; if the
					// user types before that refresh fires, the editTextWidget
					// sees a mismatch and resets the editor — moving the cursor.
					// By storing directly, we keep the tiddler in sync without
					// triggering a refresh. The CM6 engine's own saveChanges
					// (via updateListener) will later do a proper addTiddler.
					var newTid = new $tw.Tiddler(tid, fields, {modified: tid.fields.modified});
					$tw.wiki.tiddlers[collabState.tiddlerTitle] = newTid;
					$tw.wiki.clearCache(collabState.tiddlerTitle);
					$tw.wiki.clearGlobalCache();
						}
					}
				} catch(e) {
					_clog("[Collab] YSync dispatch error: " + (e && e.message ? e.message : String(e)));
				}
			}
		};

		this._ytext.observe(this._observer);
		_clog("[Collab] YSyncPlugin constructed for " + collabState.collabTitle + " field=" + editField);
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
	var editField = (context.options && context.options.widget && context.options.widget.editField) || "text";
	var ytextKey = _ytextKeyForField(editField);
	var engineKey = tiddlerTitle + "\0" + editField;

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
	// during TW5 refresh, or a new field joining an existing session).
	var existingState = _collabStateByTitle[collabTitle];

	if(existingState && !existingState.destroyed) {
		// --- Case A: Widget recreate for same field ---
		if(existingState._fieldEditors && existingState._fieldEditors[editField]) {
			_clog("[Collab] Case A: Reusing Y.Text for " + collabTitle + " field=" + editField + " (widget recreated)");

			// Remove old engine reference for this field
			var oldKey = existingState.tiddlerTitle + "\0" + editField;
			if(_activeEngines[oldKey]) {
				delete _activeEngines[oldKey];
			}

			// Update state to point to new engine/title
			existingState.tiddlerTitle = tiddlerTitle;
			var fieldState = existingState._fieldEditors[editField];
			fieldState.engine = engine;
			engine._collabState = existingState;
			engine._collabFieldState = fieldState;
			_activeEngines[engineKey] = engine;

			// Create fresh sync + remote selection extensions bound to the SAME Y.Text
			var syncPlugin = _buildSyncPlugin(core, existingState, fieldState);
			var theme = _buildRemoteSelectionsTheme(EditorView);
			var plugin = _buildRemoteSelectionsPlugin(core, existingState, fieldState);
			return [syncPlugin, theme, plugin];
		}

		// --- Case B: New field joining existing Y.Doc ---
		_clog("[Collab] Case B: New field " + editField + " joining Y.Doc for " + collabTitle);

		existingState.tiddlerTitle = tiddlerTitle;
		var doc = existingState.doc;
		var ytext = doc.getText(ytextKey);

		// Populate Y.Text with current field content
		var currentText = "";
		var tid = wiki ? wiki.getTiddler(tiddlerTitle) : null;
		if(tid && tid.fields[editField] !== undefined) {
			currentText = "" + tid.fields[editField];
		}
		if(currentText && ytext.length === 0) {
			doc.transact(function() { ytext.insert(0, currentText); });
		}

		var fieldSyncOrigin = { _field: editField };
		var fieldState = {
			ytext: ytext,
			editField: editField,
			syncOrigin: fieldSyncOrigin,
			engine: engine
		};
		existingState._fieldEditors[editField] = fieldState;
		existingState._ytextFields.add(editField);

		engine._collabState = existingState;
		engine._collabFieldState = fieldState;
		_activeEngines[engineKey] = engine;

		var syncPlugin = _buildSyncPlugin(core, existingState, fieldState);
		var theme = _buildRemoteSelectionsTheme(EditorView);
		var plugin = _buildRemoteSelectionsPlugin(core, existingState, fieldState);

		_clog("[Collab] Case B: Y.Text(" + ytextKey + ") created with " + (currentText ? currentText.length : 0) + " chars for " + collabTitle);
		return [syncPlugin, theme, plugin];
	}

	// --- Case C: Brand new session ---
	_clog("[Collab] Case C: New session for " + collabTitle + " field=" + editField);

	var doc = new Y.Doc();
	var ytext = doc.getText(ytextKey);
	var ymap = doc.getMap("fields");

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

	// Per-field state
	var fieldSyncOrigin = { _field: editField };
	var fieldState = {
		ytext: ytext,
		editField: editField,
		syncOrigin: fieldSyncOrigin,
		engine: engine
	};

	// Store collab state on engine
	var state = {
		id: _collabId,
		doc: doc,
		ymap: ymap,
		awareness: awareness,
		tiddlerTitle: tiddlerTitle,
		collabTitle: collabTitle,
		listeners: {},
		destroyed: false,
		_transportConnected: false,
		_awaitingRemoteState: false,
		_receivedRemoteState: false,
		_fieldEditors: {},
		_ytextFields: new Set()
	};
	state._fieldEditors[editField] = fieldState;
	state._ytextFields.add(editField);

	engine._collabState = state;
	engine._collabFieldState = fieldState;
	_activeEngines[engineKey] = engine;
	_collabStateByTitle[collabTitle] = state;

	// Insert tiddler text into Y.Text. Every editor starts as "first editor".
	// When transport connects (Phase 2), joining mode may clear this text
	// and replace it with the remote peer's state.
	var currentText = "";
	var tid = wiki ? wiki.getTiddler(tiddlerTitle) : null;
	if(tid && tid.fields[editField] !== undefined) {
		currentText = "" + tid.fields[editField];
	} else if(wiki) {
		currentText = wiki.getTiddlerText(tiddlerTitle) || "";
	}
	if(currentText) {
		doc.transact(function() { ytext.insert(0, currentText); });
	}
	_clog("[Collab] Case C: Y.Text(" + ytextKey + ") inserted " + (currentText ? currentText.length : 0) + " chars for " + collabTitle);

	// Create sync + remote selections — ALWAYS in initial extensions.
	var syncPlugin = _buildSyncPlugin(core, state, fieldState);
	var theme = _buildRemoteSelectionsTheme(EditorView);
	var plugin = _buildRemoteSelectionsPlugin(core, state, fieldState);

	_clog("[Collab] Phase 1: sync plugin created (initial extensions) for " + collabTitle + " field=" + editField);
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

	// --- Y.Map field sync: remote ↔ local draft fields ---
	// Y.Map handles STRUCTURAL changes (add/delete fields) for ALL fields,
	// and VALUE changes only for fields WITHOUT Y.Text editors.
	// Fields with Y.Text editors get character-level CRDT merge instead.

	var ymapOrigin = "ymap-local";
	var ymap = state.ymap;

	// Y.Map observer: remote field changes → update local draft tiddler
	var onYmapChange = function(event, transaction) {
		if(state.destroyed) return;
		if(transaction.origin === ymapOrigin) return; // skip our own local writes

		if(!$tw || !$tw.wiki) return;
		var tid = $tw.wiki.getTiddler(state.tiddlerTitle);
		if(!tid) return;

		var changedFields = {};
		var hasChanges = false;
		event.changes.keys.forEach(function(change, key) {
			if(change.action === "add") {
				// Structural: new field from remote — always apply
				changedFields[key] = ymap.get(key);
				hasChanges = true;
			} else if(change.action === "update") {
				// Skip VALUE updates for Y.Text fields (Y.Text handles those)
				if(state._ytextFields && state._ytextFields.has(key)) return;
				changedFields[key] = ymap.get(key);
				hasChanges = true;
			} else if(change.action === "delete") {
				// Structural: field removed — always apply
				changedFields[key] = undefined;
				hasChanges = true;
			}
		});

		if(!hasChanges) return;

		_clog("[Collab] Y.Map remote change: " + JSON.stringify(Object.keys(changedFields)) + " for " + state.collabTitle);

		// Suppress the echo in our draft change listener BEFORE addTiddler,
		// since addTiddler enqueues a deferred change event.
		state._ymapSuppressDraftListener = true;

		// Use addTiddler (NOT silent store) so TW5 triggers a refresh cycle.
		var newTid = new $tw.Tiddler(tid, changedFields, {modified: tid.fields.modified});
		$tw.wiki.addTiddler(newTid);
	};
	ymap.observe(onYmapChange);
	state._onYmapChange = onYmapChange;

	// Draft change listener: local field edits → Y.Map
	var onDraftFieldChange = function(changes) {
		if(state.destroyed) return;
		if(!changes[state.tiddlerTitle]) return;
		if(changes[state.tiddlerTitle].deleted) return;

		// Skip echo from Y.Map observer
		if(state._ymapSuppressDraftListener) {
			state._ymapSuppressDraftListener = false;
			return;
		}

		var tid = $tw.wiki.getTiddler(state.tiddlerTitle);
		if(!tid) return;

		var fields = tid.fields;
		var hasUpdates = false;

		doc.transact(function() {
			// Sync new/changed fields to Y.Map
			for(var key in fields) {
				if(!Object.prototype.hasOwnProperty.call(fields, key)) continue;
				if(_isFieldHardExcluded(key)) continue;
				var val = typeof fields[key] === "string" ? fields[key] : "" + fields[key];
				if(state._ytextFields && state._ytextFields.has(key)) {
					// Y.Text field: only sync structural presence (new field)
					if(!ymap.has(key)) {
						ymap.set(key, val);
						hasUpdates = true;
					}
					// Value changes handled by Y.Text CRDT
				} else {
					// Non-Y.Text field: sync value changes normally
					if(ymap.get(key) !== val) {
						ymap.set(key, val);
						hasUpdates = true;
					}
				}
			}
			// Remove deleted fields from Y.Map (structural — always sync)
			ymap.forEach(function(_val, key) {
				if(fields[key] === undefined && !_isFieldHardExcluded(key)) {
					ymap.delete(key);
					hasUpdates = true;
				}
			});
		}, ymapOrigin);

		if(hasUpdates) {
			_clog("[Collab] Y.Map local update for " + state.collabTitle);
		}
	};
	$tw.wiki.addEventListener("change", onDraftFieldChange);
	state._onDraftFieldChange = onDraftFieldChange;

	// Helper: populate Y.Map from current draft fields (all non-hard-excluded)
	state._populateYmapFromDraft = function() {
		var tid = $tw.wiki.getTiddler(state.tiddlerTitle);
		if(!tid) return;
		doc.transact(function() {
			var fields = tid.fields;
			for(var key in fields) {
				if(!Object.prototype.hasOwnProperty.call(fields, key)) continue;
				if(_isFieldHardExcluded(key)) continue;
				var val = typeof fields[key] === "string" ? fields[key] : "" + fields[key];
				ymap.set(key, val);
			}
		}, ymapOrigin);
		_clog("[Collab] Y.Map populated from draft for " + state.collabTitle);
	};

	// --- Inbound: transport → local Yjs doc ---

	state.listeners["collab-update"] = function(data) {
		if(state.destroyed) return;
		if(data.tiddler_title !== collabTitle) return;
		try {
			var update = base64ToUint8(data.update_base64);

			// Capture lengths of ALL Y.Texts before applying update
			var ytextLengthsBefore = {};
			var fieldEditors = state._fieldEditors || {};
			for(var fname in fieldEditors) {
				if(fieldEditors.hasOwnProperty(fname)) {
					ytextLengthsBefore[fname] = fieldEditors[fname].ytext.toString().length;
				}
			}

			_clog("[Collab] INBOUND update: " + update.length + " bytes for " + collabTitle + ", awaitingRemote=" + state._awaitingRemoteState + ", fields=" + Object.keys(ytextLengthsBefore).join(","));

			// JOINING: on first remote update, clear ALL Y.Texts
			// BEFORE applying the remote state.
			if(state._awaitingRemoteState) {
				state._awaitingRemoteState = false;
				if(state._joinTimer) { clearTimeout(state._joinTimer); state._joinTimer = null; }
				_clog("[Collab] Joining: clearing all Y.Texts before applying remote state for " + collabTitle);
				doc.transact(function() {
					for(var fname in fieldEditors) {
						if(fieldEditors.hasOwnProperty(fname)) {
							var yt = fieldEditors[fname].ytext;
							if(yt.length > 0) {
								yt.delete(0, yt.length);
							}
						}
					}
				});
				// Reset lengths to 0 after clearing
				for(var fname in ytextLengthsBefore) {
					if(ytextLengthsBefore.hasOwnProperty(fname)) {
						ytextLengthsBefore[fname] = 0;
					}
				}
			}

			Y.applyUpdate(doc, update, "remote");

			// Per-field dedup safety net
			for(var fname in fieldEditors) {
				if(!fieldEditors.hasOwnProperty(fname)) continue;
				var yt = fieldEditors[fname].ytext;
				var before = ytextLengthsBefore[fname] || 0;
				var after = yt.toString().length;
				if(before > 0 && after === before * 2) {
					var afterStr = yt.toString();
					if(afterStr.substring(0, before) === afterStr.substring(before)) {
						_clog("[Collab] DEDUP field=" + fname + ": removing duplicate " + before + " chars for " + collabTitle);
						doc.transact(function() {
							yt.delete(before, before);
						});
					}
				}
			}

			state._receivedRemoteState = true;
			_clog("[Collab] After update for " + collabTitle);
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

	// When a peer saves the tiddler: update draft.of/draft.title, show banner, continue editing
	state.listeners["peer-saved"] = function(data) {
		if(state.destroyed) return;
		if(data.tiddler_title !== collabTitle) return;
		var savedTitle = data.saved_title || collabTitle;
		var deviceName = data.device_name || data.device_id || "A peer";
		_clog("[Collab] peer-saved for " + collabTitle + " savedAs=" + savedTitle + " from=" + deviceName);

		// Show notification banner
		_showCollabBanner(deviceName + " saved this tiddler" + (savedTitle !== collabTitle ? " as '" + savedTitle + "'" : ""));

		// Update the draft tiddler's draft.of and draft.title to point to the saved title
		try {
			var tid = $tw.wiki.getTiddler(state.tiddlerTitle);
			if(tid && savedTitle !== (tid.fields["draft.of"] || "")) {
				var newFields = {"draft.of": savedTitle, "draft.title": savedTitle};
				$tw.wiki.addTiddler(new $tw.Tiddler(tid, newFields));
				_clog("[Collab] Updated draft.of/draft.title to: " + savedTitle);
			}
		} catch(_e) {
			_clog("[Collab] peer-saved draft update error: " + (_e && _e.message ? _e.message : String(_e)));
		}

		// Update collab session title if it changed
		var oldCollabTitle = collabTitle;
		if(savedTitle !== collabTitle) {
			// Re-register in _collabStateByTitle under the new key
			if(_collabStateByTitle[collabTitle] === state) {
				delete _collabStateByTitle[collabTitle];
			}
			state.collabTitle = savedTitle;
			collabTitle = savedTitle;
			_collabStateByTitle[savedTitle] = state;
			_clog("[Collab] Session retargeted: " + oldCollabTitle + " -> " + savedTitle);

			// Re-register editing status under new title
			try {
				collab.stopEditing(oldCollabTitle);
				collab.startEditing(savedTitle);
			} catch(_e) {}
		}
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
			// Do NOT populate Y.Map here — defer to remote state arrival.
			// Fallback: if no remote state arrives in 5s, stop waiting
			// and send our full state so peers can dedup-merge.
			state._joinTimer = setTimeout(function() {
				if(state._awaitingRemoteState && !state.destroyed) {
					state._awaitingRemoteState = false;
					_clog("[Collab] Join timeout (5s): keeping local text for " + collabTitle);
					// Populate Y.Map from draft as fallback (same as first-editor)
					if(state._populateYmapFromDraft) {
						state._populateYmapFromDraft();
					}
					// Send full state as fallback (peers' dedup safety net handles doubling)
					try {
						var fullState = Y.encodeStateAsUpdate(doc);
						_clog("[Collab] Join timeout: sending full state (" + fullState.length + " bytes) for " + collabTitle);
						collab.sendUpdate(collabTitle, uint8ToBase64(fullState));
					} catch(_e2) {}
				}
			}, 5000);
		}

		// Notify peers that we're editing (triggers them to send full state)
		try {
			collab.startEditing(collabTitle);
		} catch(_e) {}

		// Send our full Y.Doc state — but only when NOT joining.
		// When joining, we skip sending to avoid transient text doubling on
		// existing peers. Our startEditing() triggers them to send their state;
		// once we receive it, we clear our local text and apply theirs.
		// If the remote state never arrives, the join timeout above sends
		// our state as a fallback.
		if(!hasRemote) {
			// First editor: populate Y.Map from draft before sending full state
			if(state._populateYmapFromDraft) {
				state._populateYmapFromDraft();
			}
			try {
				var fullState = Y.encodeStateAsUpdate(doc);
				_clog("[Collab] Phase 2: sending full state (" + fullState.length + " bytes) for " + collabTitle);
				collab.sendUpdate(collabTitle, uint8ToBase64(fullState));
				var awarenessUpdate = encodeAwarenessUpdate(awareness, [doc.clientID]);
				collab.sendAwareness(collabTitle, uint8ToBase64(awarenessUpdate));
			} catch(_e) {}
		} else {
			// Still send awareness so remote cursors show up immediately
			try {
				var awarenessUpdate = encodeAwarenessUpdate(awareness, [doc.clientID]);
				collab.sendAwareness(collabTitle, uint8ToBase64(awarenessUpdate));
			} catch(_e) {}
		}
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


// Update the user name (and derived color) on all active Yjs awareness instances.
// Called when $:/status/UserName changes mid-session so remote cursor labels update.
function _updateAllUserNames(newName) {
	var color = _getUserColor(newName);
	for(var title in _collabStateByTitle) {
		if(_collabStateByTitle.hasOwnProperty(title)) {
			var state = _collabStateByTitle[title];
			if(!state.destroyed && state.awareness) {
				try {
					state.awareness.setLocalStateField("user", {
						name: newName,
						color: color.color,
						colorLight: color.light
					});
				} catch(_e) {}
			}
		}
	}
}

exports.updateUserName = _updateAllUserNames;

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
		if(!enabled) return false;
		if(!context.tiddlerTitle) return false;
		// Enable collab for all text-editable fields. Each field gets its own
		// Y.Text instance within the shared Y.Doc (per-field CRDT).
		var editField = (context.options && context.options.widget && context.options.widget.editField) || "text";
		_clog("[Collab] condition: tiddlerTitle=" + (context.tiddlerTitle || "none") + ", editField=" + editField + ", enabled=" + enabled);
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
				// Collab API not available yet — editor created before lan_sync.js ran
				// (Android: evaluateJavascript runs after onPageFinished).
				// Listen for the collab-sync-activated event to connect transport late.
				_clog("[Collab] collab API not found for " + tiddlerTitle + " — waiting for collab-sync-activated event");
				var _lateEngine = engine;
				var _onSyncActivated = function() {
					window.removeEventListener("collab-sync-activated", _onSyncActivated);
					if(state) state._syncActivatedListener = null;
					var collabApi = window.TiddlyDesktop && window.TiddlyDesktop.collab;
					if(collabApi && _lateEngine._collabState && !_lateEngine._collabState.destroyed && !_lateEngine._collabState._transportConnected) {
						_clog("[Collab] Late Phase 2: connecting transport for " + tiddlerTitle);
						try {
							_connectTransport(_lateEngine, collabApi);
						} catch(ce) {
							_clog("[Collab] Late Phase 2 error: " + (ce && ce.message ? ce.message : String(ce)));
						}
					}
				};
				if(state) state._syncActivatedListener = _onSyncActivated;
				window.addEventListener("collab-sync-activated", _onSyncActivated);
			}

			return [compartment.of(exts)];
		} catch(e) {
			_clog("[Collab] getExtensions ERROR: " + (e && e.message ? e.message : String(e)) + "\n" + (e && e.stack ? e.stack : ""));
			return [compartment.of([])];
		}
	},

	destroy: function(engine) {
		var state = engine._collabState;
		if(!state) return;

		var fieldState = engine._collabFieldState;
		var editField = fieldState ? fieldState.editField : null;

		// Remove this field from per-field tracking
		if(editField && state._fieldEditors && state._fieldEditors[editField]) {
			// Only remove if this engine owns this field slot
			if(state._fieldEditors[editField].engine === engine) {
				delete state._fieldEditors[editField];
				if(state._ytextFields) state._ytextFields.delete(editField);
			}
		}

		// Remove from _activeEngines (composite key)
		var engineKey = state.tiddlerTitle + "\0" + (editField || "text");
		if(_activeEngines[engineKey] === engine) {
			delete _activeEngines[engineKey];
		}

		engine._collabState = null;
		engine._collabFieldState = null;

		// Check if other field editors remain for this Y.Doc
		var remainingFields = state._fieldEditors ? Object.keys(state._fieldEditors) : [];
		if(remainingFields.length > 0) {
			_clog("[Collab] Field " + editField + " removed, " + remainingFields.length + " field(s) remain for " + state.collabTitle);
			return; // Don't tear down shared state yet
		}

		// Last field editor — full teardown
		if(state.destroyed) return;
		state.destroyed = true;

		// Clean up pending collab-sync-activated listener (editor created before sync)
		if(state._syncActivatedListener) {
			window.removeEventListener("collab-sync-activated", state._syncActivatedListener);
			state._syncActivatedListener = null;
		}

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

		// Clean up Y.Map observer and draft change listener
		if(state._onYmapChange && state.ymap) {
			state.ymap.unobserve(state._onYmapChange);
		}
		if(state._onDraftFieldChange && $tw && $tw.wiki) {
			$tw.wiki.removeEventListener("change", state._onDraftFieldChange);
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
		if(_collabStateByTitle[state.collabTitle] === state) {
			delete _collabStateByTitle[state.collabTitle];
		}
		_clog("[Collab] Session fully destroyed for: " + state.collabTitle);
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
