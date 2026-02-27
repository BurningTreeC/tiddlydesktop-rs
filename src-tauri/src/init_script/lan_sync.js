// LAN Sync — change hooks and inbound change application
// This script hooks into TiddlyWiki's change event system to detect local edits
// and sends them to the Rust LAN sync module. It also listens for inbound changes
// from remote peers and applies them to the local wiki.
//
// Transport:
//   Desktop: Tauri IPC (window.__TAURI__)
//   Android: @JavascriptInterface (TiddlyDesktopSync) — bypasses WebView connection pool

(function() {
  'use strict';

  // Pre-guard diagnostic — uses same pattern as internal_drag.js which is proven to work
  if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
    window.__TAURI__.core.invoke('js_log', {
      message: '[LAN Sync] IIFE entered: __IS_MAIN_WIKI__=' + window.__IS_MAIN_WIKI__ +
        ', __WINDOW_LABEL__=' + (window.__WINDOW_LABEL__ || '(none)') +
        ', __WIKI_PATH__=' + (window.__WIKI_PATH__ || '(none)')
    }).catch(function() {});
  }

  // Only run in wiki windows (not landing page)
  // Use __WINDOW_LABEL__ check — same pattern as internal_drag.js which works on all platforms.
  // Previously used __IS_MAIN_WIKI__ but that guard failed silently on Windows (unknown cause).
  if (window.__WINDOW_LABEL__ === 'main') return;

  // Early diagnostic logging — uses js_log if available, so it appears in stderr
  function _earlyLog(msg) {
    if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
      window.__TAURI__.core.invoke('js_log', { message: msg }).catch(function() {});
    }
    console.warn(msg);
  }
  _earlyLog('[LAN Sync] IIFE started, __IS_MAIN_WIKI__=' + !!window.__IS_MAIN_WIKI__ + ', __WIKI_PATH__=' + (window.__WIKI_PATH__ || '(none)'));

  // Transport variables — determined lazily once $tw is ready
  // (window.__TAURI__ may not be available yet during initial script parse)
  var isAndroid = false;
  var hasTauri = false;

  // ── Collab API (always available) ──────────────────────────────────
  // Created immediately so the CM6 collab plugin can find it at any time,
  // even before LAN sync activates.  Outbound methods queue until sync
  // is active, then the queue is flushed.

  var collabWs = null;
  var collabListeners = {};
  var remoteEditorsCache = {};
  var _collabSyncActive = false;
  var _collabWikiId = null;
  var _collabLocalDeviceId = null;
  var _collabOutboundQueue = [];

  if (!window.TiddlyDesktop) window.TiddlyDesktop = {};

  window.TiddlyDesktop.collab = {
    startEditing: function(tiddlerTitle) {
      if (isSyncExcluded(tiddlerTitle)) return;
      if (_collabSyncActive) {
        sendCollabOutbound('startEditing', _collabWikiId, tiddlerTitle);
      } else {
        _collabOutboundQueue.push(['startEditing', tiddlerTitle]);
      }
    },
    stopEditing: function(tiddlerTitle) {
      if (isSyncExcluded(tiddlerTitle)) return;
      if (_collabSyncActive) {
        sendCollabOutbound('stopEditing', _collabWikiId, tiddlerTitle);
      } else {
        _collabOutboundQueue.push(['stopEditing', tiddlerTitle]);
      }
    },
    sendUpdate: function(tiddlerTitle, base64) {
      if (isSyncExcluded(tiddlerTitle)) return;
      if (_collabSyncActive) {
        sendCollabOutbound('sendUpdate', _collabWikiId, tiddlerTitle, base64);
      } else {
        _collabOutboundQueue.push(['sendUpdate', tiddlerTitle, base64]);
      }
    },
    sendAwareness: function(tiddlerTitle, base64) {
      if (isSyncExcluded(tiddlerTitle)) return;
      if (_collabSyncActive) {
        sendCollabOutbound('sendAwareness', _collabWikiId, tiddlerTitle, base64);
      } else {
        _collabOutboundQueue.push(['sendAwareness', tiddlerTitle, base64]);
      }
    },
    peerSaved: function(tiddlerTitle, savedTitle) {
      if (_collabSyncActive) {
        sendCollabOutbound('peerSaved', _collabWikiId, tiddlerTitle, null, savedTitle);
      } else {
        _collabOutboundQueue.push(['peerSaved', tiddlerTitle, null, savedTitle]);
      }
    },
    getRemoteEditors: function(tiddlerTitle) {
      if (!_collabSyncActive) return [];
      if (isAndroid) {
        try {
          var json = window.TiddlyDesktopSync.getRemoteEditors(_collabWikiId, tiddlerTitle);
          return JSON.parse(json || '[]');
        } catch (e) { return []; }
      }
      return remoteEditorsCache[tiddlerTitle] || [];
    },
    getRemoteEditorsAsync: function(tiddlerTitle) {
      if (!_collabSyncActive) return Promise.resolve([]);
      if (hasTauri) {
        return window.__TAURI__.core.invoke('lan_sync_get_remote_editors', {
          wikiId: _collabWikiId, tiddlerTitle: tiddlerTitle
        }).catch(function() { return []; });
      }
      return Promise.resolve(this.getRemoteEditors(tiddlerTitle));
    },
    on: function(eventType, callback) {
      if (!collabListeners[eventType]) collabListeners[eventType] = [];
      collabListeners[eventType].push(callback);
    },
    off: function(eventType, callback) {
      if (!collabListeners[eventType]) return;
      collabListeners[eventType] = collabListeners[eventType].filter(function(cb) {
        return cb !== callback;
      });
    }
  };

  // Dispatch collab-api-ready immediately
  try { window.dispatchEvent(new Event("collab-api-ready")); } catch(_e) {}

  // Wait for TiddlyWiki AND transport to be ready
  var _initCheckCount = 0;
  var _tauriLoggedOnce = false;
  var _rootWidgetLoggedOnce = false;
  function _checkReady() {
    _initCheckCount++;
    // Re-check transport each tick (Tauri IPC bridge loads asynchronously)
    isAndroid = typeof window.TiddlyDesktopSync !== 'undefined';
    hasTauri = !!(window.__TAURI__ && window.__TAURI__.core);

    if (!isAndroid && !hasTauri) {
      // Log once at 2s to help diagnose Windows transport issues
      if (_initCheckCount === 20) {
        _earlyLog('[LAN Sync] Transport not ready after 2s (__TAURI__=' + !!window.__TAURI__ + ')');
      }
      return; // transport not ready yet
    }

    if (!_tauriLoggedOnce) {
      _tauriLoggedOnce = true;
      _earlyLog('[LAN Sync] hasTauri became true (tick ' + _initCheckCount + ')');
    }

    // On Android, check that bridge is running
    if (isAndroid && window.TiddlyDesktopSync.getBridgePort() <= 0) return;

    // Wait for $tw.rootWidget — this ensures TW's eventsTriggered is true
    // and the rendering system is ready.  Without this, addTiddler() during
    // catch-up sync silently drops change events and the UI never updates.
    if (typeof $tw !== 'undefined' && $tw.wiki && $tw.wiki.addEventListener && $tw.rootWidget) {
      if (!_rootWidgetLoggedOnce) {
        _rootWidgetLoggedOnce = true;
        _earlyLog('[LAN Sync] $tw.rootWidget ready (tick ' + _initCheckCount + ')');
      }
      clearInterval(checkInterval);
      initLanSync();
      return;
    }

    // Log progress at 5s and 15s if $tw isn't ready yet (transport IS ready)
    if ((_initCheckCount === 50 || _initCheckCount === 150) && !_rootWidgetLoggedOnce) {
      _earlyLog('[LAN Sync] $tw not ready after ' + (_initCheckCount / 10) + 's: $tw=' + (typeof $tw !== 'undefined') + ', wiki=' + !!(typeof $tw !== 'undefined' && $tw.wiki) + ', addEventListener=' + !!(typeof $tw !== 'undefined' && $tw.wiki && $tw.wiki.addEventListener) + ', rootWidget=' + !!(typeof $tw !== 'undefined' && $tw.rootWidget));
    }

    // After 60s (600 ticks), back off to 1s polling instead of giving up
    if (_initCheckCount === 600) {
      clearInterval(checkInterval);
      _earlyLog('[LAN Sync] Switching to 1s polling after 60s');
      checkInterval = setInterval(_checkReady, 1000);
    }
  }
  var checkInterval = setInterval(_checkReady, 100);

  // ── Initialization ───────────────────────────────────────────────────

  // Track active sync state so we can deactivate cleanly
  var activeSyncState = null;

  // Buffer for non-activation messages drained from the IPC queue by
  // pollIpcForActivation before the main pollIpc handler starts.
  // Module-scoped so both initLanSync and setupSyncHandlers can access it.
  var _preActivationOverflow = [];

  // Logging helper at module scope — rsLog inside initLanSync is only available
  // there; setupSyncHandlers needs its own access.
  function _log(msg) {
    if (window.__TAURI__ && window.__TAURI__.core) {
      window.__TAURI__.core.invoke('js_log', { message: msg }).catch(function() {});
    }
    console.log(msg);
  }

  // Add a $:/temp/* tiddler without triggering change events.
  // TW5's saveTiddlerFilter already excludes $:/temp/* from saves,
  // and SaverFilter excludes them from dirty tracking.
  function addTempTiddler(fields) {
    var origEnqueue = $tw.wiki.enqueueTiddlerEvent;
    $tw.wiki.enqueueTiddlerEvent = function() {};
    $tw.wiki.addTiddler(fields);
    $tw.wiki.enqueueTiddlerEvent = origEnqueue;
  }

  // Delete a tiddler without dirtying the wiki or triggering autosave.
  function deleteTempTiddler(title) {
    var origEnqueue = $tw.wiki.enqueueTiddlerEvent;
    $tw.wiki.enqueueTiddlerEvent = function() {};
    $tw.wiki.deleteTiddler(title);
    $tw.wiki.enqueueTiddlerEvent = origEnqueue;
  }

  // Add a temp tiddler and trigger a targeted UI refresh (no dirty/autosave).
  function addTempTiddlerWithRefresh(fields) {
    addTempTiddler(fields);
    var changes = {};
    changes[fields.title] = { modified: true };
    $tw.wiki.eventsTriggered = false;
    $tw.wiki.dispatchEvent('change', changes);
  }

  // Delete a temp tiddler and trigger a targeted UI refresh (no dirty/autosave).
  function deleteTempTiddlerWithRefresh(title) {
    deleteTempTiddler(title);
    var changes = {};
    changes[title] = { deleted: true };
    $tw.wiki.eventsTriggered = false;
    $tw.wiki.dispatchEvent('change', changes);
  }

  function initLanSync() {
    var wikiPath = window.__WIKI_PATH__ || '';
    if (!wikiPath) return;

    // Helper to log via Rust stderr (console.log doesn't appear in desktop logs)
    function rsLog(msg) {
      if (hasTauri) {
        window.__TAURI__.core.invoke('js_log', { message: msg }).catch(function() {});
      }
      console.log(msg);
    }

    // Ask backend for sync_id (from recent_wikis.json)
    rsLog('[LAN Sync] Checking sync_id for: ' + wikiPath);
    getSyncId(wikiPath, function(syncId) {
      rsLog('[LAN Sync] Got sync_id: ' + (syncId || '(none)'));
      if (syncId) {
        activateSync(syncId);
      }
    });

    if (isAndroid) {
      // On Android, Tauri events don't cross process boundaries.
      // Periodically re-check sync_id to detect enable/disable changes.
      setInterval(function() {
        getSyncId(wikiPath, function(id) {
          var currentId = activeSyncState ? activeSyncState.syncId : '';
          if (id && id !== currentId) {
            rsLog('[LAN Sync] Sync enabled via periodic check: ' + id);
            activateSync(id);
          } else if (!id && currentId) {
            rsLog('[LAN Sync] Sync disabled via periodic check');
            deactivateSync();
          }
        });
      }, 500);
    }

    // Desktop: poll IPC queue for sync activation messages.
    // Tauri events (app.emit) do NOT cross process boundaries — each wiki
    // window is a separate Tauri process. The main process sends activation
    // messages via IPC (TCP), which the wiki process's listener thread pushes
    // to IPC_SYNC_QUEUE. We poll that queue here.
    if (hasTauri && !isAndroid) {
      var preActivationPollActive = true;
      function pollIpcForActivation() {
        if (!preActivationPollActive) return;
        window.__TAURI__.core.invoke('lan_sync_poll_ipc', { wikiId: '' }).then(function(messages) {
          if (messages && messages.length) {
            rsLog('[LAN Sync] pollIpcForActivation: ' + messages.length + ' messages');
            var activated = false;
            for (var i = 0; i < messages.length; i++) {
              try {
                var data = JSON.parse(messages[i]);
                if (!data || !data.type) continue;
                if (!activated && data.type === 'sync-activate' && data.wiki_path === wikiPath && data.sync_id) {
                  rsLog('[LAN Sync] Activated via IPC: ' + data.sync_id);
                  activated = true;
                  preActivationPollActive = false;
                  // Don't call activateSync yet — collect remaining messages first
                  continue;
                }
                if (data.type === 'sync-deactivate' && data.wiki_path === wikiPath) {
                  rsLog('[LAN Sync] Deactivated via IPC');
                  deactivateSync();
                  continue;
                }
                // Buffer ALL non-activation messages so they aren't lost.
                // Messages can arrive before the sync-activate in the same
                // batch (or in earlier polls if sync was already pending).
                _preActivationOverflow.push(messages[i]);
              } catch (e) {}
            }
            if (activated) {
              // Now activate — setupSyncHandlers will start pollIpc which
              // checks _preActivationOverflow on its first iteration
              var syncData = JSON.parse(messages.find(function(m) {
                try { var d = JSON.parse(m); return d.type === 'sync-activate' && d.wiki_path === wikiPath; } catch(e) { return false; }
              }));
              activateSync(syncData.sync_id);
              return;
            }
          }
          if (preActivationPollActive) setTimeout(pollIpcForActivation, 500);
        }).catch(function() {
          if (preActivationPollActive) setTimeout(pollIpcForActivation, 2000);
        });
      }
      // Only start pre-activation polling if sync isn't already active
      if (!activeSyncState) {
        setTimeout(pollIpcForActivation, 500);
      }
    }

    // Wiki config change notifications (tiddlywiki.info updated via LAN sync)
    // are delivered through IPC poll (desktop) or bridge poll (Android),
    // handled as type 'wiki-info-changed' in queueInboundChange / IPC switch.

    function activateSync(syncId) {
      // If already active with the same syncId, don't duplicate
      if (activeSyncState && activeSyncState.syncId === syncId) {
        rsLog('[LAN Sync] Already active with syncId: ' + syncId);
        return;
      }

      // If active with a different syncId, deactivate first
      if (activeSyncState) {
        deactivateSync();
      }

      // Stop the pre-activation poll — sync is now active, setupSyncHandlers
      // starts its own poll that handles all message types including activation.
      // Without this, the pre-activation poll races with the main poll and
      // discards non-activate IPC messages (apply-change, compare-fingerprints, etc.)
      preActivationPollActive = false;

      try {
        rsLog('[LAN Sync] activateSync: ' + syncId);

        activeSyncState = setupSyncHandlers(syncId);

        // Activate collab API (set active flag, flush queued outbound messages)
        _collabSyncActive = true;
        _collabWikiId = syncId;
        // NOTE: Do NOT reset collabListeners here — CM6 editors created before
        // sync activation have already registered their inbound listeners via
        // collab.on(). Wiping them would break inbound message delivery.
        clearAllEditingTiddlers();
        remoteEditorsCache = {};

        // Fetch our own device_id so we can filter out self-echoed collab messages
        if (hasTauri && !isAndroid) {
          window.__TAURI__.core.invoke('lan_sync_get_status').then(function(status) {
            if (status && status.device_id) {
              _collabLocalDeviceId = status.device_id;
              rsLog('[LAN Sync] Local device_id: ' + _collabLocalDeviceId);
            }
          }).catch(function() {});
        }

        // Flush queued outbound messages
        var queued = _collabOutboundQueue;
        _collabOutboundQueue = [];
        for (var qi = 0; qi < queued.length; qi++) {
          var q = queued[qi];
          sendCollabOutbound(q[0], syncId, q[1], q[2], q[3]);
        }

        // Connect collab WebSocket (desktop only — gives sub-ms push delivery)
        if (hasTauri && !isAndroid) {
          connectCollabWs(syncId);
        }

        rsLog('[LAN Sync] Collab API activated for wiki: ' + syncId);

        // Notify CM6 collab plugins that sync transport is now active.
        // Editors created before the collab API existed (Android: evaluateJavascript
        // runs after page load) listen for this to trigger late Phase 2 connection.
        try { window.dispatchEvent(new Event('collab-sync-activated')); } catch(_e) {}

        // Notify Rust that this wiki window is now open and ready for sync.
        notifyWikiOpened(syncId);

        rsLog('[LAN Sync] Initialized for wiki: ' + syncId);
      } catch (e) {
        rsLog('[LAN Sync] activateSync error: ' + e.message);
      }
    }

    function deactivateSync() {
      if (!activeSyncState) return;

      rsLog('[LAN Sync] Deactivating sync for: ' + activeSyncState.syncId);

      // Remove the TiddlyWiki change listener
      if (activeSyncState.changeListener) {
        $tw.wiki.removeEventListener('change', activeSyncState.changeListener);
      }

      // Clear pending save timer
      if (activeSyncState.saveTimer) {
        clearTimeout(activeSyncState.saveTimer);
      }

      // Clear inbound batch timer and queue
      if (activeSyncState.batchTimer) {
        clearTimeout(activeSyncState.batchTimer);
      }
      activeSyncState.inboundQueue = null;

      // Clear outbound batch timer
      if (activeSyncState.outboundTimer) {
        clearTimeout(activeSyncState.outboundTimer);
      }

      // Stop Android polling
      if (activeSyncState.pollTimerId) {
        clearTimeout(activeSyncState.pollTimerId);
      }

      // Unlisten Tauri event listeners
      if (activeSyncState.unlistenFns) {
        activeSyncState.unlistenFns.forEach(function(fn) { fn(); });
      }

      // Deactivate collab (keep API object alive so CM6 plugin references remain valid)
      _collabSyncActive = false;
      _collabWikiId = null;
      _collabLocalDeviceId = null;
      _collabOutboundQueue = [];
      collabListeners = {};
      clearAllEditingTiddlers();
      remoteEditorsCache = {};
      if (collabWs) {
        try { collabWs.close(); } catch (e) {}
        collabWs = null;
      }

      activeSyncState = null;
      rsLog('[LAN Sync] Sync deactivated');
    }
  }

  // ── Transport helpers ────────────────────────────────────────────────

  function getSyncId(path, callback) {
    if (isAndroid) {
      var id = window.TiddlyDesktopSync.getSyncId(path);
      callback(id || '');
    } else {
      window.__TAURI__.core.invoke('get_wiki_sync_id', { path: path })
        .then(function(id) { callback(id || ''); })
        .catch(function() { callback(''); });
    }
  }

  function notifyWikiOpened(wikiId) {
    if (isAndroid) {
      window.TiddlyDesktopSync.wikiOpened(wikiId);
    } else {
      window.__TAURI__.core.invoke('lan_sync_wiki_opened', { wikiId: wikiId }).catch(function(e) { console.error('[LAN Sync] wiki_opened error:', e); });
    }
  }

  function sendTiddlerChanged(wikiId, title, tiddlerJson) {
    if (isAndroid) {
      window.TiddlyDesktopSync.tiddlerChanged(wikiId, title, tiddlerJson);
    } else {
      window.__TAURI__.core.invoke('lan_sync_tiddler_changed', {
        wikiId: wikiId, title: title, tiddlerJson: tiddlerJson
      }).catch(function(e) { console.error('[LAN Sync] tiddler_changed error:', e); });
    }
  }

  function sendTiddlerDeleted(wikiId, title) {
    if (isAndroid) {
      window.TiddlyDesktopSync.tiddlerDeleted(wikiId, title);
    } else {
      window.__TAURI__.core.invoke('lan_sync_tiddler_deleted', {
        wikiId: wikiId, title: title
      }).catch(function(e) { console.error('[LAN Sync] tiddler_deleted error:', e); });
    }
  }

  function sendFullSyncBatch(wikiId, toDeviceId, tiddlers, isLastBatch) {
    if (isAndroid) {
      window.TiddlyDesktopSync.sendFullSyncBatch(
        wikiId, toDeviceId, JSON.stringify(tiddlers), isLastBatch
      );
      return Promise.resolve();
    } else {
      return window.__TAURI__.core.invoke('lan_sync_send_full_sync', {
        wikiId: wikiId, toDeviceId: toDeviceId, tiddlers: tiddlers, isLastBatch: isLastBatch
      });
    }
  }

  function sendFingerprints(wikiId, toDeviceId, fingerprints) {
    if (isAndroid) {
      window.TiddlyDesktopSync.sendFingerprints(
        wikiId, toDeviceId, JSON.stringify(fingerprints)
      );
      return Promise.resolve();
    } else {
      return window.__TAURI__.core.invoke('lan_sync_send_fingerprints', {
        wikiId: wikiId, toDeviceId: toDeviceId, fingerprints: fingerprints
      });
    }
  }

  function broadcastFingerprints(wikiId, fingerprints) {
    if (isAndroid) {
      window.TiddlyDesktopSync.broadcastFingerprints(
        wikiId, JSON.stringify(fingerprints)
      );
      return Promise.resolve();
    } else {
      return window.__TAURI__.core.invoke('lan_sync_broadcast_fingerprints', {
        wikiId: wikiId, fingerprints: fingerprints
      });
    }
  }

  function loadTombstones(wikiId, callback) {
    if (isAndroid) {
      var json = window.TiddlyDesktopSync.loadTombstones(wikiId);
      callback(json || '{}');
    } else {
      window.__TAURI__.core.invoke('lan_sync_load_tombstones', { wikiId: wikiId })
        .then(function(json) { callback(json || '{}'); })
        .catch(function() { callback('{}'); });
    }
  }

  function saveTombstones(wikiId, tombstonesJson) {
    if (isAndroid) {
      window.TiddlyDesktopSync.saveTombstones(wikiId, tombstonesJson);
    } else {
      window.__TAURI__.core.invoke('lan_sync_save_tombstones', {
        wikiId: wikiId, tombstonesJson: tombstonesJson
      }).catch(function(e) { console.error('[LAN Sync] Failed to save tombstones:', e); });
    }
  }

  // ── Helpers ─────────────────────────────────────────────────────────

  // Convert a field value to a comparable string
  function fieldToString(v) {
    if (v instanceof Date) {
      return $tw.utils.stringifyDate(v);
    }
    if (Array.isArray(v)) {
      return $tw.utils.stringifyList(v);
    }
    return String(v);
  }

  // Serialize tiddler fields to JSON, converting Date objects to TW date strings
  function serializeTiddlerFields(fields) {
    var out = {};
    var keys = Object.keys(fields);
    for (var i = 0; i < keys.length; i++) {
      var k = keys[i];
      out[k] = fieldToString(fields[k]);
    }
    return JSON.stringify(out);
  }

  // Check if a title is a draft tiddler (including numbered drafts like "Draft 2 of 'Title'")
  function isDraft(title) {
    if (title.indexOf("Draft of '") === 0) return true;
    if (title.indexOf("Draft ") === 0) {
      var rest = title.substring(6);
      var p = rest.indexOf(" of '");
      if (p > 0 && /^\d+$/.test(rest.substring(0, p))) return true;
    }
    return false;
  }

  // Compare version strings (semver-like: "0.0.4" vs "0.0.5").
  // Returns  1 if a > b,  -1 if a < b,  0 if equal.
  function compareVersions(a, b) {
    if (!a && !b) return 0;
    if (!a) return -1;
    if (!b) return 1;
    var pa = a.split('.'), pb = b.split('.');
    var len = Math.max(pa.length, pb.length);
    for (var i = 0; i < len; i++) {
      var na = parseInt(pa[i] || '0', 10) || 0;
      var nb = parseInt(pb[i] || '0', 10) || 0;
      if (na > nb) return 1;
      if (na < nb) return -1;
    }
    return 0;
  }

  // Check if a title is a syncable plugin tiddler (has plugin-type, not our injected ones)
  function isSyncablePlugin(title) {
    if (title.indexOf('$:/plugins/') !== 0) return false;
    var t = $tw.wiki.getTiddler(title);
    return t && t.fields['plugin-type'];
  }

  // Check if a title should be excluded from sync
  function isSyncExcluded(title) {
    if (title === '$:/StoryList' || title === '$:/HistoryList' || title === '$:/library/sjcl.js' ||
        title === '$:/Import' || title === '$:/language' || title === '$:/theme' || title === '$:/palette' ||
        title === '$:/isEncrypted' || title === '$:/view' || title === '$:/layout' ||
        title === '$:/DefaultTiddlers' || title === '$:/core') return true;
    if (isDraft(title)) return true;
    if (title.indexOf('$:/TiddlyDesktopRS/Conflicts/') === 0) return true;
    if (title.indexOf('$:/state/') === 0) return true;
    if (title.indexOf('$:/status/') === 0) return true;
    if (title.indexOf('$:/temp/') === 0) return true;
    if (title.indexOf('$:/language/') === 0) return true;
    if (title.indexOf('$:/plugins/tiddlydesktop-rs/') === 0) return true;
    if (title.indexOf('$:/plugins/tiddlydesktop/') === 0) return true;
    if (title.indexOf('$:/config/') === 0) return true;
    if (title.indexOf('$:/themes/tiddlywiki/vanilla/options/') === 0) return true;
    if (title.indexOf('$:/themes/tiddlywiki/vanilla/metrics/') === 0) return true;
    return false;
  }

  // Check if a title is a pure shadow tiddler (part of a plugin, not overridden)
  function isPureShadow(title) {
    return $tw.wiki.isShadowTiddler(title) && !$tw.wiki.tiddlerExists(title);
  }

  // Get all syncable (non-shadow, non-excluded) tiddler titles
  function getSyncableTitles() {
    var allTitles = $tw.wiki.allTitles();
    var result = [];
    for (var i = 0; i < allTitles.length; i++) {
      var title = allTitles[i];
      if (isSyncExcluded(title)) continue;
      if (isPureShadow(title)) continue;
      result.push(title);
    }
    return result;
  }

  // ── Sync handlers ────────────────────────────────────────────────────

  // Returns a state object with cleanup references
  function setupSyncHandlers(wikiId) {
    // Flag to prevent echo: when applying remote changes, don't re-send them
    // Titles currently being applied from remote — checked by the change
    // listener to suppress re-broadcasting received changes.  Using a Set
    // instead of a boolean flag because TiddlyWiki dispatches change events
    // asynchronously via $tw.utils.nextTick(), so a boolean set/cleared
    // synchronously around addTiddler() would already be false when the
    // deferred change listener fires.
    var suppressOutbound = new Set();

    // Track titles being saved as conflicts (don't re-sync these)
    var conflictTitles = new Set();

    // Titles received from peers but skipped (identical to local shadow/real).
    // These are included in fingerprint generation so the peer stops re-sending.
    // Maps title → modified string.
    var knownSyncTitles = {};

    // Deletion tombstones: title → {modified: TW date string, time: epoch ms}
    // When a tiddler is deleted locally, a tombstone is recorded so fingerprint
    // broadcasts inform peers to delete it too — even if they were offline.
    var deletionTombstones = {};
    var tombstonesLoaded = false; // Set true after loadTombstones callback
    var TOMBSTONE_MAX_AGE_MS = 30 * 24 * 60 * 60 * 1000; // 30 days


    // State object to return for cleanup
    var state = {
      syncId: wikiId,
      changeListener: null,
      pollTimerId: null,
      unlistenFns: [],
      saveTimer: null,
      batchTimer: null,
      inboundQueue: [],
      outboundTimer: null
    };

    // Compare incoming tiddler fields with local — returns true if content differs.
    // Ignores 'modified', 'modifier', and 'created' (metadata-only differences
    // should NOT trigger a re-apply that dirties the wiki). Fingerprint convergence
    // is handled by knownSyncTitles: when content matches, the remote's modified
    // timestamp is tracked there so the peer stops re-sending.
    function tiddlerDiffers(fields) {
      var existing = $tw.wiki.getTiddler(fields.title);
      if (!existing) return true;
      var ef = existing.fields;
      // Check all incoming fields exist and match in local (skip metadata)
      var keys = Object.keys(fields);
      for (var i = 0; i < keys.length; i++) {
        var k = keys[i];
        if (k === 'created' || k === 'modified' || k === 'modifier') continue;
        var v1 = String(fields[k]);
        var v2 = ef[k];
        if (v2 === undefined) return true;
        if (v1 !== fieldToString(v2)) return true;
      }
      // Check local doesn't have extra fields not in incoming (skip metadata)
      var eKeys = Object.keys(ef);
      for (var j = 0; j < eKeys.length; j++) {
        var ek = eKeys[j];
        if (ek === 'created' || ek === 'modified' || ek === 'modifier') continue;
        if (fields[ek] === undefined) return true;
      }
      return false;
    }

    // Collect fingerprints for all syncable tiddlers, including titles we
    // received from peers but skipped as identical (shadow-only on this side),
    // and deletion tombstones so peers learn about offline deletions.
    function collectFingerprints() {
      var titles = getSyncableTitles();
      var seen = {};
      var fps = [];
      for (var i = 0; i < titles.length; i++) {
        var t = $tw.wiki.getTiddler(titles[i]);
        if (t) {
          var mod = t.fields.modified;
          var fp = { title: titles[i], modified: mod ? fieldToString(mod) : '' };
          // Include version for plugin tiddlers so peers can do version-aware comparison
          if (t.fields['plugin-type'] && t.fields.version) {
            fp.version = t.fields.version;
          }
          fps.push(fp);
          seen[titles[i]] = true;
        }
      }
      var knownKeys = Object.keys(knownSyncTitles);
      for (var j = 0; j < knownKeys.length; j++) {
        if (!seen[knownKeys[j]]) {
          fps.push({ title: knownKeys[j], modified: knownSyncTitles[knownKeys[j]] });
        }
      }
      // Include deletion tombstones (skip if tiddler was re-created locally or cleared)
      var tombKeys = Object.keys(deletionTombstones);
      var tombstonesChanged = false;
      for (var k = 0; k < tombKeys.length; k++) {
        if (deletionTombstones[tombKeys[k]].cleared) {
          // Tombstone was cleared (tiddler re-created) — don't include in fingerprints
          continue;
        }
        if (!seen[tombKeys[k]]) {
          fps.push({
            title: tombKeys[k],
            modified: deletionTombstones[tombKeys[k]].modified,
            deleted: true
          });
        } else {
          // Tiddler was re-created — mark tombstone as cleared
          deletionTombstones[tombKeys[k]].cleared = true;
          tombstonesChanged = true;
        }
      }
      if (tombstonesChanged) {
        saveTombstones(wikiId, JSON.stringify(deletionTombstones));
      }
      return fps;
    }

    // ── Outbound: detect local changes (batched with 50ms window) ──────

    // Pending outbound changes: title → {deleted: bool, tiddlerJson: string|null}
    var pendingOutbound = {};

    function flushOutbound() {
      state.outboundTimer = null;
      var batch = pendingOutbound;
      pendingOutbound = {};
      var titles = Object.keys(batch);
      for (var i = 0; i < titles.length; i++) {
        var title = titles[i];
        var entry = batch[title];
        if (entry.deleted) {
          sendTiddlerDeleted(wikiId, title);
        } else if (entry.tiddlerJson) {
          sendTiddlerChanged(wikiId, title, entry.tiddlerJson);
        }
      }
    }

    var changeListener = function(changes) {
      Object.keys(changes).forEach(function(title) {
        // Skip titles just applied from remote sync (prevents echo loop)
        if (suppressOutbound.delete(title)) return;
        if (isSyncExcluded(title)) return;
        if (conflictTitles.has(title)) return;

        _log('[LAN Sync] Outbound change: ' + title);
        if (changes[title].deleted) {
          // Record tombstone so peers learn about this deletion even if offline
          var delMod = $tw.utils.stringifyDate(new Date());
          deletionTombstones[title] = { modified: delMod, time: Date.now() };
          saveTombstones(wikiId, JSON.stringify(deletionTombstones));
          pendingOutbound[title] = { deleted: true, tiddlerJson: null };
        } else {
          // If this tiddler had a deletion tombstone, mark it cleared
          // so the tombstone won't re-delete it on the next fingerprint sync
          if (deletionTombstones[title] && !deletionTombstones[title].cleared) {
            deletionTombstones[title].cleared = true;
            saveTombstones(wikiId, JSON.stringify(deletionTombstones));
            _log('[LAN Sync] Cleared tombstone for re-created tiddler: ' + title);
          }
          var tiddler = $tw.wiki.getTiddler(title);
          if (tiddler) {
            pendingOutbound[title] = { deleted: false, tiddlerJson: serializeTiddlerFields(tiddler.fields) };
          }
        }
      });

      if (!state.outboundTimer && Object.keys(pendingOutbound).length > 0) {
        state.outboundTimer = setTimeout(flushOutbound, 50);
      }
    };

    $tw.wiki.addEventListener('change', changeListener);
    state.changeListener = changeListener;

    // ── Debounced auto-save for single-file wikis ──────────────────────
    var isSingleFileWiki = !$tw.syncer;

    function scheduleSave() {
      if (!isSingleFileWiki) return;
      if (state.saveTimer) clearTimeout(state.saveTimer);
      state.saveTimer = setTimeout(function() {
        state.saveTimer = null;
        $tw.rootWidget.dispatchEvent({type: 'tm-save-wiki'});
      }, 500);
    }

    // ── Inbound: batched change application ────────────────────────────
    // Changes are collected and applied in bulk so TiddlyWiki re-renders
    // only once per batch (instead of once per tiddler during full sync).

    function queueInboundChange(data) {
      if (data.wiki_id !== wikiId) return;

      // dump-tiddlers requests must be handled immediately (not deferred)
      if (data.type === 'dump-tiddlers') {
        handleDumpTiddlers(data.to_device_id);
        return;
      }

      // Attachment received — reload elements immediately (not batched)
      if (data.type === 'attachment-received') {
        reloadAttachment(data.filename);
        return;
      }

      // Fingerprint-based sync: send our fingerprints to the peer
      if (data.type === 'send-fingerprints') {
        handleSendFingerprints(data.to_device_id);
        return;
      }

      // Fingerprint-based sync: compare peer's fingerprints and send diffs
      if (data.type === 'compare-fingerprints') {
        handleCompareFingerprints(data.from_device_id, data.fingerprints);
        return;
      }

      // Wiki config changed (tiddlywiki.info updated via LAN sync)
      if (data.type === 'wiki-info-changed') {
        _log('[LAN Sync] Wiki config changed from another device');
        addTempTiddlerWithRefresh({
          title: "$:/temp/tiddlydesktop/config-reload-required",
          text: "yes"
        });
        return;
      }

      // Collaborative editing messages — route to collab API
      if (data.type === 'editing-started' || data.type === 'editing-stopped' ||
          data.type === 'collab-update' || data.type === 'collab-awareness') {
        handleCollabMessage(data);
        return;
      }

      // Peer status updates — update shadow tiddlers for peer badge UI
      if (data.type === 'peer-update') {
        if (data.peers) {
          // Convert array to object keyed by index — strip device_id (sensitive)
          var peersObj = {};
          for (var pi = 0; pi < data.peers.length; pi++) {
            peersObj[pi] = {user_name: data.peers[pi].user_name || '', device_name: data.peers[pi].device_name || ''};
          }
          var peersJson = JSON.stringify(peersObj);
          var countStr = String(data.peers.length);
          var PEERS_TITLE = '$:/temp/tiddlydesktop/connected-peers';
          var COUNT_TITLE = '$:/temp/tiddlydesktop/peer-count';
          var changed = {};
          if ($tw.wiki.getTiddlerText(PEERS_TITLE) !== peersJson) {
            addTempTiddler({ title: PEERS_TITLE, type: 'application/json', text: peersJson });
            changed[PEERS_TITLE] = { modified: true };
          }
          if ($tw.wiki.getTiddlerText(COUNT_TITLE) !== countStr) {
            addTempTiddler({ title: COUNT_TITLE, text: countStr });
            changed[COUNT_TITLE] = { modified: true };
          }
          if (Object.keys(changed).length > 0) {
            $tw.wiki.eventsTriggered = false;
            $tw.wiki.dispatchEvent('change', changed);
          }
        }
        return;
      }

      state.inboundQueue.push(data);
      if (!state.batchTimer) {
        state.batchTimer = setTimeout(applyInboundBatch, 50);
      }
    }

    function applyInboundBatch() {
      state.batchTimer = null;
      var batch = state.inboundQueue;
      state.inboundQueue = [];
      if (batch.length === 0) return;

      _log('[LAN Sync] Applying batch of ' + batch.length + ' inbound changes');

      var needSave = false;
      var pluginsChanged = false;
      // Pending conflicts: title → local tiddler snapshot.
      // Created when a 'conflict' event arrives; consumed when the subsequent
      // 'apply-change' arrives. If the change is content-identical or a plugin,
      // the conflict is discarded (no conflict tiddler created).
      var pendingConflicts = {};
      for (var i = 0; i < batch.length; i++) {
        var data = batch[i];

        if (data.type === 'apply-change') {
          try {
            var fields = JSON.parse(data.tiddler_json);
            // Plugin tiddlers: only accept if incoming version is newer
            if (fields['plugin-type'] && fields.version) {
              // Never create conflicts for plugin tiddlers — version comparison only
              delete pendingConflicts[fields.title];
              var localPlugin = $tw.wiki.getTiddler(fields.title);
              if (localPlugin && localPlugin.fields.version &&
                  compareVersions(fields.version, localPlugin.fields.version) <= 0) {
                _log('[LAN Sync] Skipped older/equal plugin: ' + fields.title +
                     ' (local=' + localPlugin.fields.version + ', remote=' + fields.version + ')');
                knownSyncTitles[fields.title] = fields.modified ? String(fields.modified) : '';
                continue;
              }
            }
            // If we had a tombstone for this title, the peer re-created it
            if (deletionTombstones[fields.title]) {
              delete deletionTombstones[fields.title];
            }
            if (tiddlerDiffers(fields)) {
              // Content actually differs — create the pending conflict if one exists
              if (pendingConflicts[fields.title]) {
                var localSnap = pendingConflicts[fields.title];
                var conflictTitle = '$:/TiddlyDesktopRS/Conflicts/' + fields.title;
                conflictTitles.add(conflictTitle);
                var conflictFields = Object.assign({}, localSnap.fields, {
                  title: conflictTitle,
                  'conflict-original-title': fields.title,
                  'conflict-timestamp': new Date().toISOString(),
                  'conflict-source': 'local'
                });
                $tw.wiki.addTiddler(new $tw.Tiddler(conflictFields));
                setTimeout(function() { conflictTitles.delete(conflictTitle); }, 500);
                delete pendingConflicts[fields.title];
              }
              // Parse date strings to Date objects so TiddlyWiki stores them correctly
              if (fields.created) fields.created = $tw.utils.parseDate(fields.created);
              if (fields.modified) fields.modified = $tw.utils.parseDate(fields.modified);
              suppressOutbound.add(fields.title);
              $tw.wiki.addTiddler(new $tw.Tiddler(fields));
              needSave = true;
              // Track plugin tiddler updates for shadow re-extraction
              if (fields.title.indexOf('$:/plugins/') === 0 && fields['plugin-type']) {
                pluginsChanged = true;
              }
            } else {
              // Content identical (only metadata differs) — discard any pending conflict
              if (pendingConflicts[fields.title]) {
                _log('[LAN Sync] Skipped conflict for metadata-only diff: ' + fields.title);
                delete pendingConflicts[fields.title];
              }
              _log('[LAN Sync] Skipped identical tiddler: ' + fields.title);
              // Track so we include it in fingerprints (peer will stop re-sending)
              var skipMod = fields.modified ? String(fields.modified) : '';
              knownSyncTitles[fields.title] = skipMod;
            }
          } catch (e) {
            console.error('[LAN Sync] Failed to apply remote change:', e);
          }

        } else if (data.type === 'apply-deletion') {
          try {
            if ($tw.wiki.tiddlerExists(data.title)) {
              suppressOutbound.add(data.title);
              $tw.wiki.deleteTiddler(data.title);
              needSave = true;
            }
            // Record tombstone so this deletion propagates to other peers
            var delMod = $tw.utils.stringifyDate(new Date());
            if (!deletionTombstones[data.title] ||
                deletionTombstones[data.title].modified < delMod) {
              deletionTombstones[data.title] = { modified: delMod, time: Date.now() };
              saveTombstones(wikiId, JSON.stringify(deletionTombstones));
            }
          } catch (e) {
            console.error('[LAN Sync] Failed to apply remote deletion:', e);
          }

        } else if (data.type === 'conflict') {
          var title = data.title;
          // Skip conflicts for plugin tiddlers entirely
          if (title.indexOf('$:/plugins/') === 0) {
            _log('[LAN Sync] Skipped conflict for plugin: ' + title);
            continue;
          }
          // Defer conflict creation — will be resolved when the subsequent
          // apply-change arrives (skip if content is metadata-only different)
          var localTiddler = $tw.wiki.getTiddler(title);
          if (localTiddler) {
            pendingConflicts[title] = localTiddler;
          }
        }
      }
      // Any remaining pending conflicts without a subsequent apply-change
      // (shouldn't normally happen, but handle gracefully)
      var pcKeys = Object.keys(pendingConflicts);
      for (var pc = 0; pc < pcKeys.length; pc++) {
        var pcTitle = pcKeys[pc];
        var pcLocal = pendingConflicts[pcTitle];
        var pcConflictTitle = '$:/TiddlyDesktopRS/Conflicts/' + pcTitle;
        conflictTitles.add(pcConflictTitle);
        var pcConflictFields = Object.assign({}, pcLocal.fields, {
          title: pcConflictTitle,
          'conflict-original-title': pcTitle,
          'conflict-timestamp': new Date().toISOString(),
          'conflict-source': 'local'
        });
        $tw.wiki.addTiddler(new $tw.Tiddler(pcConflictFields));
        setTimeout(function() { conflictTitles.delete(pcConflictTitle); }, 500);
      }
      // If plugin tiddlers were updated, re-extract shadow tiddlers
      if (pluginsChanged) {
        _log('[LAN Sync] Plugin tiddler(s) updated — re-registering plugins');
        try {
          $tw.wiki.readPluginInfo();
          $tw.wiki.registerPluginTiddlers('plugin');
          $tw.wiki.unpackPluginTiddlers();
        } catch (e) {
          console.error('[LAN Sync] Failed to re-register plugins:', e);
        }
      }
      if (needSave) scheduleSave();
    }

    // ── Fingerprint-based diff sync ─────────────────────────────────
    // Phase 1: Collect fingerprints (title + modified) for all syncable
    //          tiddlers and send them to the peer.
    // Phase 2: When we receive the peer's fingerprints, compare with local
    //          state and send only tiddlers that are missing or newer here.

    function handleSendFingerprints(toDeviceId) {
      // Defer until tombstones are loaded so our fingerprints include them
      if (!tombstonesLoaded) {
        _log('[LAN Sync] Deferring send-fingerprints until tombstones loaded');
        setTimeout(function() { handleSendFingerprints(toDeviceId); }, 100);
        return;
      }
      var fingerprints = collectFingerprints();

      console.log('[LAN Sync] Sending ' + fingerprints.length + ' fingerprints to ' + toDeviceId);

      sendFingerprints(wikiId, toDeviceId, fingerprints);
    }

    function handleCompareFingerprints(fromDeviceId, peerFingerprints) {
      // Defer until tombstones are loaded so comparisons are accurate
      if (!tombstonesLoaded) {
        _log('[LAN Sync] Deferring compare-fingerprints until tombstones loaded');
        setTimeout(function() { handleCompareFingerprints(fromDeviceId, peerFingerprints); }, 100);
        return;
      }
      // Guard against undefined/null fingerprints (e.g. truncated IPC message)
      if (!peerFingerprints || !peerFingerprints.length) {
        _log('[LAN Sync] compare-fingerprints: no fingerprints received');
        return;
      }
      // Separate peer's fingerprints into normal tiddlers and tombstones
      var peerMap = {};       // title → modified
      var peerVersions = {};  // title → version (only for plugin tiddlers)
      var peerTombstones = {};
      for (var i = 0; i < peerFingerprints.length; i++) {
        var fp = peerFingerprints[i];
        if (fp.deleted) {
          peerTombstones[fp.title] = fp.modified;
        } else {
          peerMap[fp.title] = fp.modified;
          if (fp.version) peerVersions[fp.title] = fp.version;
        }
      }

      // Phase 1: Apply peer's tombstones — delete local tiddlers that the peer
      // intentionally deleted (if our version is older than the deletion).
      var tombKeys = Object.keys(peerTombstones);
      var needSave = false;
      for (var t = 0; t < tombKeys.length; t++) {
        var tombTitle = tombKeys[t];
        var tombModified = peerTombstones[tombTitle];

        // If we previously cleared a tombstone for this title (tiddler was
        // re-created locally), skip if the peer's tombstone is not newer
        var localTomb = deletionTombstones[tombTitle];
        if (localTomb && localTomb.cleared && localTomb.modified >= tombModified) {
          // Peer's tombstone is same or older than the one we cleared — skip
          continue;
        }

        var localTiddler = $tw.wiki.getTiddler(tombTitle);
        if (localTiddler) {
          var localMod = localTiddler.fields.modified ? fieldToString(localTiddler.fields.modified) : '';
          if (!localMod || localMod <= tombModified) {
            // Peer deleted it after our version — apply deletion
            suppressOutbound.add(tombTitle);
            $tw.wiki.deleteTiddler(tombTitle);
            needSave = true;
            _log('[LAN Sync] Applied tombstone deletion: ' + tombTitle);
          }
          // else: our version is newer than the deletion — keep it, will send below
        }

        // Record peer's tombstone locally (propagate to other peers)
        if (!localTomb || localTomb.modified < tombModified) {
          deletionTombstones[tombTitle] = { modified: tombModified, time: Date.now() };
        }
      }
      if (needSave) scheduleSave();
      if (tombKeys.length > 0) {
        saveTombstones(wikiId, JSON.stringify(deletionTombstones));
      }

      // Phase 2: Find tiddlers we have that the peer needs (missing or newer)
      var titles = getSyncableTitles();
      var toSend = [];

      for (var j = 0; j < titles.length; j++) {
        var title = titles[j];
        var tiddler = $tw.wiki.getTiddler(title);
        if (!tiddler) continue;

        if (!(title in peerMap)) {
          // Peer doesn't have this tiddler — send it
          toSend.push(title);
        } else if (tiddler.fields['plugin-type']) {
          // Plugin tiddler: compare by version exclusively (not modified timestamp)
          var localVer = tiddler.fields.version || '';
          var peerVer = peerVersions[title] || '';
          if (compareVersions(localVer, peerVer) > 0) {
            toSend.push(title);
          }
        } else {
          // Regular tiddler: compare by modified timestamp
          var localMod2 = tiddler.fields.modified ? fieldToString(tiddler.fields.modified) : '';
          if (localMod2 > (peerMap[title] || '')) {
            toSend.push(title);
          }
        }
      }

      _log('[LAN Sync] Fingerprint diff: ' + toSend.length + ' tiddlers to send to ' + fromDeviceId +
                  ' (of ' + titles.length + ' local, peer has ' + peerFingerprints.length +
                  ', ' + tombKeys.length + ' tombstones processed)');

      if (toSend.length === 0) {
        // Still send an empty last batch to signal completion
        sendFullSyncBatch(wikiId, fromDeviceId, [], true);
        return;
      }

      // Send only the different tiddlers in batches (max ~500KB per batch)
      var MAX_BATCH_BYTES = 500000;
      function sendBatch(startIndex) {
        var batch = [];
        var bytes = 0;
        var k = startIndex;
        while (k < toSend.length && (batch.length === 0 || bytes < MAX_BATCH_BYTES)) {
          var t = $tw.wiki.getTiddler(toSend[k]);
          if (t) {
            var json = serializeTiddlerFields(t.fields);
            bytes += json.length;
            batch.push({ title: toSend[k], tiddler_json: json });
          }
          k++;
        }
        var isLast = k >= toSend.length;
        sendFullSyncBatch(wikiId, fromDeviceId, batch, isLast)
          .then(function() {
            if (!isLast) {
              setTimeout(function() { sendBatch(k); }, 100);
            } else {
              _log('[LAN Sync] Diff sync complete — sent ' + toSend.length + ' tiddlers');
            }
          })
          .catch(function(e) {
            _log('[LAN Sync] Failed to send diff sync batch: ' + e);
          });
      }
      sendBatch(0);
    }

    // Legacy full dump (kept as fallback for dump-tiddlers events)
    function handleDumpTiddlers(toDeviceId) {
      var titles = getSyncableTitles();
      var MAX_BATCH_BYTES = 500000;

      console.log('[LAN Sync] Full dump of ' + titles.length + ' tiddlers to ' + toDeviceId);

      function sendBatch(startIndex) {
        var batch = [];
        var bytes = 0;
        var i = startIndex;
        while (i < titles.length && (batch.length === 0 || bytes < MAX_BATCH_BYTES)) {
          var tiddler = $tw.wiki.getTiddler(titles[i]);
          if (tiddler) {
            var json = serializeTiddlerFields(tiddler.fields);
            bytes += json.length;
            batch.push({ title: titles[i], tiddler_json: json });
          }
          i++;
        }
        var isLast = i >= titles.length;
        sendFullSyncBatch(wikiId, toDeviceId, batch, isLast)
          .then(function() {
            if (!isLast) {
              setTimeout(function() { sendBatch(i); }, 100);
            } else {
              console.log('[LAN Sync] Full dump complete');
            }
          })
          .catch(function(e) {
            console.error('[LAN Sync] Failed to send full sync batch:', e);
          });
      }

      if (titles.length > 0) {
        sendBatch(0);
      }
    }

    // ── Set up inbound transport ───────────────────────────────────────
    _log('[LAN Sync] Transport: isAndroid=' + isAndroid + ' hasTauri=' + hasTauri);

    if (isAndroid) {
      // Android: poll bridge via @JavascriptInterface (no WebView connections used)
      // Changes from the poll are already batched (all pending at once)
      var pollActive = true;
      var pollFailures = 0;
      var pollInterval = 100;
      function pollBridge() {
        if (!pollActive) return;
        try {
          var changesJson = window.TiddlyDesktopSync.pollChanges(wikiId);
          var changes = JSON.parse(changesJson);
          if (changes && changes.length) {
            changes.forEach(queueInboundChange);
          }
          pollFailures = 0; // reset on success
          pollInterval = 100;
        } catch (e) {
          pollFailures++;
          if (pollFailures === 1 || pollFailures % 10 === 0) {
            console.error('[LAN Sync] Bridge poll error (' + pollFailures + ' failures):', e);
          }
          if (pollFailures >= 30) {
            console.error('[LAN Sync] Bridge appears dead after ' + pollFailures + ' consecutive failures — stopping poll');
            pollActive = false;
            return;
          }
          if (pollFailures >= 10) {
            pollInterval = 1000; // back off to 1s after 10 failures
          }
        }
        state.pollTimerId = setTimeout(pollBridge, pollInterval);
      }
      state.pollTimerId = setTimeout(pollBridge, pollInterval);
      // Store a way to stop the poll
      state._stopPoll = function() { pollActive = false; };
    } else if (hasTauri) {
      // Desktop: poll IPC queue for LAN sync messages.
      // Neither Tauri event.listen() nor WebView eval() reliably deliver events
      // from IPC listener threads to JS on Linux/WebKitGTK, so we poll instead.
      _log('[LAN Sync] Starting IPC poll (desktop)');
      var ipcPollActive = true;
      var _overflowDrained = false;
      function pollIpc() {
        if (!ipcPollActive) return;
        // On first call, process any messages buffered by pollIpcForActivation
        if (!_overflowDrained && _preActivationOverflow.length > 0) {
          _overflowDrained = true;
          var overflow = _preActivationOverflow;
          _preActivationOverflow = [];
          _log('[LAN Sync] Processing ' + overflow.length + ' pre-activation overflow messages');
          for (var oi = 0; oi < overflow.length; oi++) {
            try {
              var oData = JSON.parse(overflow[oi]);
              if (!oData || !oData.type) continue;
              if (oData.wiki_id && oData.wiki_id !== wikiId) continue;
              queueInboundChange(oData);
            } catch (e) {
              _log('[LAN Sync] Overflow parse error: ' + e);
            }
          }
        } else {
          _overflowDrained = true;
        }
        window.__TAURI__.core.invoke('lan_sync_poll_ipc', { wikiId: wikiId }).then(function(messages) {
          if (messages && messages.length) {
            _log('[LAN Sync] IPC poll: ' + messages.length + ' messages');
            for (var i = 0; i < messages.length; i++) {
              try {
                var data = JSON.parse(messages[i]);
                if (!data || !data.type) continue;

                // Handle sync deactivation via IPC (cross-process)
                if (data.type === 'sync-deactivate' && data.wiki_path === wikiPath) {
                  _log('[LAN Sync] Deactivated via IPC (in sync handler)');
                  deactivateSync();
                  return; // stop this poll iteration
                }

                switch (data.type) {
                  case 'apply-change':
                  case 'apply-deletion':
                  case 'conflict':
                    queueInboundChange(data);
                    break;
                  case 'dump-tiddlers':
                    handleDumpTiddlers(data.to_device_id);
                    break;
                  case 'send-fingerprints':
                    handleSendFingerprints(data.to_device_id);
                    break;
                  case 'compare-fingerprints':
                    _log('[LAN Sync] compare-fingerprints: ' + (data.fingerprints ? data.fingerprints.length : 0) + ' fingerprints from peer');
                    handleCompareFingerprints(data.from_device_id, data.fingerprints);
                    break;
                  case 'attachment-received':
                    reloadAttachment(data.filename);
                    break;
                  case 'wiki-info-changed':
                    _log('[LAN Sync] Wiki config changed from another device');
                    addTempTiddlerWithRefresh({
                      title: "$:/temp/tiddlydesktop/config-reload-required",
                      text: "yes"
                    });
                    break;
                  case 'editing-started':
                  case 'editing-stopped':
                  case 'collab-update':
                  case 'collab-awareness':
                    handleCollabMessage(data);
                    break;
                  case 'peer-update':
                    // Update shadow tiddlers for peer badge (pushed from main process)
                    if (data.peers) {
                      var PEERS_TIDDLER = '$:/temp/tiddlydesktop/connected-peers';
                      var COUNT_TIDDLER = '$:/temp/tiddlydesktop/peer-count';
                      var peersObj2 = {};
                      for (var pi2 = 0; pi2 < data.peers.length; pi2++) {
                        peersObj2[pi2] = {user_name: data.peers[pi2].user_name || '', device_name: data.peers[pi2].device_name || ''};
                      }
                      var peersJson = JSON.stringify(peersObj2);
                      var countStr = String(data.peers.length);
                      var changed2 = {};
                      if ($tw.wiki.getTiddlerText(PEERS_TIDDLER) !== peersJson) {
                        addTempTiddler({
                          title: PEERS_TIDDLER,
                          type: 'application/json',
                          text: peersJson
                        });
                        changed2[PEERS_TIDDLER] = { modified: true };
                      }
                      if ($tw.wiki.getTiddlerText(COUNT_TIDDLER) !== countStr) {
                        addTempTiddler({
                          title: COUNT_TIDDLER,
                          text: countStr
                        });
                        changed2[COUNT_TIDDLER] = { modified: true };
                      }
                      if (Object.keys(changed2).length > 0) {
                        $tw.wiki.eventsTriggered = false;
                        $tw.wiki.dispatchEvent('change', changed2);
                      }
                    }
                    break;
                }
              } catch (e) {
                _log('[LAN Sync] IPC poll parse error: ' + e);
              }
            }
          }
          if (ipcPollActive) setTimeout(pollIpc, ipcPollInterval());
        }).catch(function(e) {
          // Command not available (e.g. main process mode) — stop polling
          _log('[LAN Sync] IPC poll not available: ' + e);
          ipcPollActive = false;
        });
      }
      // Fast polling for the first 5 seconds (20ms) to speed up initial sync,
      // then back to 100ms for steady-state
      var ipcPollStart = Date.now();
      function ipcPollInterval() {
        return (Date.now() - ipcPollStart < 5000) ? 20 : 100;
      }
      setTimeout(pollIpc, 0); // First poll immediately
      state.unlistenFns.push(function() { ipcPollActive = false; });
    }

    // ── Load persisted tombstones + initial fingerprint broadcast ──────
    loadTombstones(wikiId, function(stored) {
      try {
        var parsed = JSON.parse(stored);
        var now = Date.now();
        var keys = Object.keys(parsed);
        for (var i = 0; i < keys.length; i++) {
          if (parsed[keys[i]].time && now - parsed[keys[i]].time > TOMBSTONE_MAX_AGE_MS) {
            continue; // expired
          }
          deletionTombstones[keys[i]] = parsed[keys[i]];
        }
        _log('[LAN Sync] Loaded ' + Object.keys(deletionTombstones).length + ' tombstones');
      } catch (e) {}
      tombstonesLoaded = true;

      // Broadcast fingerprints (includes tombstones) for catch-up
      var fps = collectFingerprints();
      _log('[LAN Sync] Broadcasting ' + fps.length + ' fingerprints for catch-up');
      broadcastFingerprints(wikiId, fps).catch(function(e) {
        _log('[LAN Sync] Broadcast fingerprints error: ' + e);
      });
    });

    // ── Periodic re-sync (5s safety net) ──────────────────────────────
    // Periodically re-broadcast fingerprints so peers detect diffs and
    // re-send missed changes.  Converges quickly then becomes a no-op
    // once both sides have the same tiddler set.
    var resyncIntervalId = setInterval(function() {
      try {
        var fps = collectFingerprints();
        broadcastFingerprints(wikiId, fps).catch(function(e) {
          _log('[LAN Sync] Periodic resync error: ' + e);
        });
      } catch (e) {
        _log('[LAN Sync] Periodic resync error: ' + e);
      }
    }, 5000);
    state.unlistenFns.push(function() { clearInterval(resyncIntervalId); });

    // ── Periodic tombstone cleanup (every 10 minutes) ────────────────
    var tombstoneCleanupId = setInterval(function() {
      var now = Date.now();
      var changed = false;
      var keys = Object.keys(deletionTombstones);
      for (var i = 0; i < keys.length; i++) {
        if (deletionTombstones[keys[i]].time && now - deletionTombstones[keys[i]].time > TOMBSTONE_MAX_AGE_MS) {
          delete deletionTombstones[keys[i]];
          changed = true;
        }
      }
      if (changed) {
        saveTombstones(wikiId, JSON.stringify(deletionTombstones));
      }
    }, 600000);
    state.unlistenFns.push(function() { clearInterval(tombstoneCleanupId); });

    return state;
  }

  // ── Attachment reload ───────────────────────────────────────────────
  // When an attachment file arrives via sync, find any elements referencing
  // it and force a reload so the content becomes visible.

  function reloadAttachment(filename) {
    if (!filename) return;

    // Normalize: strip leading './' if present
    var cleanName = filename.replace(/^\.\//, '');

    console.log('[LAN Sync] Reloading attachment:', cleanName);

    // Find all media elements (img, video, audio, source, embed, object)
    var selectors = 'img, video, audio, source, embed, object, iframe';
    var elements = document.querySelectorAll(selectors);

    for (var i = 0; i < elements.length; i++) {
      var el = elements[i];
      var src = el.getAttribute('src') || '';

      // Check if src references this attachment (via tdasset://, HTTP server, or relative path)
      if (srcMatchesAttachment(src, cleanName)) {
        forceReload(el, src);
      }

      // Also check poster attribute on video elements
      if (el.tagName === 'VIDEO') {
        var poster = el.getAttribute('poster') || '';
        if (srcMatchesAttachment(poster, cleanName)) {
          el.setAttribute('poster', appendCacheBuster(poster));
        }
      }
    }
  }

  function srcMatchesAttachment(src, cleanName) {
    if (!src) return false;
    // tdasset://localhost/attachments%2Fimage.png → decode and compare
    if (src.indexOf('tdasset://') === 0) {
      var decoded = decodeURIComponent(src.replace('tdasset://localhost/', ''));
      decoded = decoded.replace(/^\.\//, '');
      return decoded === cleanName;
    }
    // HTTP server URL: http://127.0.0.1:{port}/_file/... or /_relative/...
    if (src.indexOf('http://127.0.0.1') === 0) {
      var fileIdx = src.indexOf('/_file/');
      var relIdx = src.indexOf('/_relative/');
      if (fileIdx >= 0) {
        var filePath = decodeURIComponent(src.substring(fileIdx + 7).split('?')[0]);
        return filePath.indexOf(cleanName) >= 0;
      }
      if (relIdx >= 0) {
        var relPath = decodeURIComponent(src.substring(relIdx + 11).split('?')[0]);
        relPath = relPath.replace(/^\.\//, '');
        return relPath === cleanName;
      }
    }
    // Relative path match
    var cleanSrc = src.replace(/^\.\//, '').split('?')[0];
    return cleanSrc === cleanName;
  }

  function appendCacheBuster(url) {
    var sep = url.indexOf('?') >= 0 ? '&' : '?';
    return url + sep + '_sync=' + Date.now();
  }

  function forceReload(element, originalSrc) {
    // Append cache-buster to force re-fetch
    var newSrc = appendCacheBuster(originalSrc);
    element.setAttribute('src', newSrc);
    console.log('[LAN Sync] Reloaded element:', element.tagName, originalSrc);
  }

  // ── Collaborative editing helpers ────────────────────────────────────

  function emitCollabEvent(eventType, data) {
    var listeners = collabListeners[eventType];
    _log('[Collab] emitCollabEvent: type=' + eventType + ', listeners=' + (listeners ? listeners.length : 0) + ', tiddler=' + (data && data.tiddler_title || 'none'));
    if (listeners) {
      for (var i = 0; i < listeners.length; i++) {
        _log('[Collab] Calling handler ' + i + ' for ' + eventType);
        try {
          listeners[i](data);
          _log('[Collab] Handler ' + i + ' for ' + eventType + ' completed OK');
        } catch (e) {
          _log('[Collab] Handler ' + i + ' for ' + eventType + ' THREW: ' + (e && e.message ? e.message : String(e)) + '\n' + (e && e.stack ? e.stack : ''));
        }
      }
    }
  }

  // Write/delete a $:/temp/tiddlydesktop/editing/<title> tiddler from remoteEditorsCache
  function updateEditingTiddler(tiddlerTitle) {
    if (typeof $tw === 'undefined' || !$tw.wiki) return;
    var editors = remoteEditorsCache[tiddlerTitle] || [];
    var tid = '$:/temp/tiddlydesktop/editing/' + tiddlerTitle;
    if (editors.length > 0) {
      addTempTiddlerWithRefresh({
        title: tid,
        type: 'application/json',
        text: JSON.stringify(editors)
      });
    } else {
      deleteTempTiddlerWithRefresh(tid);
    }
  }

  // Delete all $:/temp/tiddlydesktop/editing/* tiddlers (on sync reset)
  function clearAllEditingTiddlers() {
    if (typeof $tw === 'undefined' || !$tw.wiki) return;
    var prefix = '$:/temp/tiddlydesktop/editing/';
    var toDelete = [];
    $tw.wiki.each(function(tiddler, title) {
      if (title.indexOf(prefix) === 0) {
        toDelete.push(title);
      }
    });
    if (toDelete.length === 0) return;
    var changes = {};
    for (var i = 0; i < toDelete.length; i++) {
      deleteTempTiddler(toDelete[i]);
      changes[toDelete[i]] = { deleted: true };
    }
    $tw.wiki.eventsTriggered = false;
    $tw.wiki.dispatchEvent('change', changes);
  }

  function handleCollabMessage(data) {
    if (!data || !data.type) return;
    // Skip messages from our own device (self-echo guard)
    if (_collabLocalDeviceId && data.device_id && data.device_id === _collabLocalDeviceId) {
      _log('[Collab] Skipping self-echoed ' + data.type + ' for ' + (data.tiddler_title || ''));
      return;
    }
    switch (data.type) {
      case 'editing-started':
        // Update remote editors cache
        if (data.tiddler_title && data.device_id) {
          if (!remoteEditorsCache[data.tiddler_title]) remoteEditorsCache[data.tiddler_title] = [];
          var found = false;
          for (var i = 0; i < remoteEditorsCache[data.tiddler_title].length; i++) {
            if (remoteEditorsCache[data.tiddler_title][i].device_id === data.device_id) {
              found = true;
              remoteEditorsCache[data.tiddler_title][i].user_name = data.user_name || '';
              break;
            }
          }
          if (!found) {
            remoteEditorsCache[data.tiddler_title].push({
              device_id: data.device_id,
              device_name: data.device_name || '',
              user_name: data.user_name || ''
            });
          }
          _log('[Collab] Cache: added editor for ' + data.tiddler_title + ', now ' + remoteEditorsCache[data.tiddler_title].length + ' remote editors');
          updateEditingTiddler(data.tiddler_title);
        }
        emitCollabEvent('editing-started', data);
        break;
      case 'editing-stopped':
        // Update remote editors cache
        if (data.tiddler_title && data.device_id && remoteEditorsCache[data.tiddler_title]) {
          remoteEditorsCache[data.tiddler_title] = remoteEditorsCache[data.tiddler_title].filter(function(e) {
            return e.device_id !== data.device_id;
          });
          if (remoteEditorsCache[data.tiddler_title].length === 0) {
            delete remoteEditorsCache[data.tiddler_title];
          }
          _log('[Collab] Cache: removed editor for ' + data.tiddler_title + ', now ' + (remoteEditorsCache[data.tiddler_title] ? remoteEditorsCache[data.tiddler_title].length : 0) + ' remote editors');
          updateEditingTiddler(data.tiddler_title);
        }
        emitCollabEvent('editing-stopped', data);
        break;
      case 'collab-update':
        emitCollabEvent('collab-update', data);
        break;
      case 'collab-awareness':
        emitCollabEvent('collab-awareness', data);
        break;
      case 'peer-saved':
        emitCollabEvent('peer-saved', data);
        break;
    }
  }

  function sendCollabOutbound(type, wikiId, tiddlerTitle, base64, savedTitle) {
    // Try WebSocket first (instant)
    if (collabWs && collabWs.readyState === WebSocket.OPEN) {
      var msg = { type: type, wiki_id: wikiId, tiddler_title: tiddlerTitle };
      if (base64) msg.update_base64 = base64;
      if (savedTitle) msg.saved_title = savedTitle;
      collabWs.send(JSON.stringify(msg));
      return;
    }

    // Fall back to Tauri invoke / Android bridge
    if (isAndroid) {
      switch (type) {
        case 'startEditing':
          window.TiddlyDesktopSync.collabEditingStarted(wikiId, tiddlerTitle);
          break;
        case 'stopEditing':
          window.TiddlyDesktopSync.collabEditingStopped(wikiId, tiddlerTitle);
          break;
        case 'sendUpdate':
          window.TiddlyDesktopSync.collabUpdate(wikiId, tiddlerTitle, base64);
          break;
        case 'sendAwareness':
          window.TiddlyDesktopSync.collabAwareness(wikiId, tiddlerTitle, base64);
          break;
        case 'peerSaved':
          window.TiddlyDesktopSync.collabPeerSaved(wikiId, tiddlerTitle, savedTitle);
          break;
      }
    } else if (hasTauri) {
      switch (type) {
        case 'startEditing':
          window.__TAURI__.core.invoke('lan_sync_collab_editing_started', {
            wikiId: wikiId, tiddlerTitle: tiddlerTitle
          }).catch(function() {});
          break;
        case 'stopEditing':
          window.__TAURI__.core.invoke('lan_sync_collab_editing_stopped', {
            wikiId: wikiId, tiddlerTitle: tiddlerTitle
          }).catch(function() {});
          break;
        case 'sendUpdate':
          window.__TAURI__.core.invoke('lan_sync_collab_update', {
            wikiId: wikiId, tiddlerTitle: tiddlerTitle, updateBase64: base64
          }).catch(function() {});
          break;
        case 'sendAwareness':
          window.__TAURI__.core.invoke('lan_sync_collab_awareness', {
            wikiId: wikiId, tiddlerTitle: tiddlerTitle, updateBase64: base64
          }).catch(function() {});
          break;
        case 'peerSaved':
          window.__TAURI__.core.invoke('lan_sync_collab_peer_saved', {
            wikiId: wikiId, tiddlerTitle: tiddlerTitle, savedTitle: savedTitle
          }).catch(function() {});
          break;
      }
    }
  }

  var collabWsReconnects = 0;
  var collabWsPort = 0;

  function connectCollabWs(wikiId, attempt) {
    if (!hasTauri || isAndroid) return;
    attempt = attempt || 1;

    function doConnect(port) {
      _log('[Collab WS] Connecting to ws://127.0.0.1:' + port);
      var ws = new WebSocket('ws://127.0.0.1:' + port);

      ws.onopen = function() {
        _log('[Collab WS] Connected');
        collabWs = ws;
        collabWsReconnects = 0;
        ws.send(JSON.stringify({ type: 'identify', wiki_id: wikiId }));
      };

      ws.onmessage = function(event) {
        try {
          var data = JSON.parse(event.data);
          handleCollabMessage(data);
        } catch (e) {
          console.error('[Collab WS] Parse error:', e);
        }
      };

      ws.onerror = function(e) {
        _log('[Collab WS] Error: ' + (e.message || 'unknown'));
      };

      ws.onclose = function() {
        if (collabWs === ws) collabWs = null;
        collabWsReconnects++;
        if (collabWsReconnects <= 10) {
          var delay = Math.min(1000 * Math.pow(2, collabWsReconnects - 1), 30000);
          _log('[Collab WS] Disconnected, reconnecting in ' + delay + 'ms (attempt ' + collabWsReconnects + ')');
          setTimeout(function() { doConnect(port); }, delay);
        } else {
          _log('[Collab WS] Disconnected, giving up after ' + collabWsReconnects + ' attempts');
        }
      };
    }

    // Use cached port if available
    if (collabWsPort > 0) {
      doConnect(collabWsPort);
      return;
    }

    window.__TAURI__.core.invoke('lan_sync_get_collab_port').then(function(port) {
      if (!port || port === 0) {
        if (attempt < 30) {
          setTimeout(function() { connectCollabWs(wikiId, attempt + 1); }, 500);
        } else {
          _log('[Collab WS] No collab port available after ' + attempt + ' attempts');
        }
        return;
      }
      collabWsPort = port;
      doConnect(port);
    }).catch(function(e) {
      _log('[Collab WS] Failed to get port: ' + e);
    });
  }
})();

