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

  // Only run in wiki windows (not landing page)
  if (window.__IS_MAIN_WIKI__) return;

  // Transport variables — determined lazily once $tw is ready
  // (window.__TAURI__ may not be available yet during initial script parse)
  var isAndroid = false;
  var hasTauri = false;

  // Wait for TiddlyWiki AND transport to be ready
  var checkInterval = setInterval(function() {
    // Re-check transport each tick (Tauri IPC bridge loads asynchronously)
    isAndroid = typeof window.TiddlyDesktopSync !== 'undefined';
    hasTauri = !!(window.__TAURI__ && window.__TAURI__.core);

    if (!isAndroid && !hasTauri) return; // transport not ready yet

    // On Android, check that bridge is running
    if (isAndroid && window.TiddlyDesktopSync.getBridgePort() <= 0) return;

    // Wait for $tw.rootWidget — this ensures TW's eventsTriggered is true
    // and the rendering system is ready.  Without this, addTiddler() during
    // catch-up sync silently drops change events and the UI never updates.
    if (typeof $tw !== 'undefined' && $tw.wiki && $tw.wiki.addEventListener && $tw.rootWidget) {
      clearInterval(checkInterval);
      initLanSync();
    }
  }, 100);

  // Timeout after 30s
  setTimeout(function() { clearInterval(checkInterval); }, 30000);

  // ── Initialization ───────────────────────────────────────────────────

  // Track active sync state so we can deactivate cleanly
  var activeSyncState = null;

  // Logging helper at module scope — rsLog inside initLanSync is only available
  // there; setupSyncHandlers needs its own access.
  function _log(msg) {
    if (window.__TAURI__ && window.__TAURI__.core) {
      window.__TAURI__.core.invoke('js_log', { message: msg }).catch(function() {});
    }
    console.log(msg);
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

    // Desktop: listen for sync being enabled/disabled at runtime via Tauri events
    if (hasTauri && window.__TAURI__.event) {
      window.__TAURI__.event.listen('lan-sync-activate', function(event) {
        var data = event.payload;
        if (data.wiki_path === wikiPath && data.sync_id) {
          rsLog('[LAN Sync] Activated via event: ' + data.sync_id);
          activateSync(data.sync_id);
        }
      });

      window.__TAURI__.event.listen('lan-sync-deactivate', function(event) {
        var data = event.payload;
        if (data.wiki_path === wikiPath) {
          rsLog('[LAN Sync] Deactivated via event');
          deactivateSync();
        }
      });
    }

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

      try {
        rsLog('[LAN Sync] activateSync: ' + syncId);

        activeSyncState = setupSyncHandlers(syncId);

        // Notify Rust that this wiki window is now open and ready for sync.
        notifyWikiOpened(syncId);

        // Proactively collect and broadcast our fingerprints to all connected
        // peers. This is the most reliable catch-up mechanism — no event
        // round-trip needed. Each peer compares and sends back what we're missing.
        var titles = getSyncableTitles();
        var fingerprints = [];
        for (var i = 0; i < titles.length; i++) {
          var tiddler = $tw.wiki.getTiddler(titles[i]);
          if (tiddler) {
            var mod = tiddler.fields.modified;
            fingerprints.push({
              title: titles[i],
              modified: mod ? fieldToString(mod) : ''
            });
          }
        }
        rsLog('[LAN Sync] Broadcasting ' + fingerprints.length + ' fingerprints for catch-up');
        broadcastFingerprints(syncId, fingerprints);

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

  // Check if a title should be excluded from sync
  function isSyncExcluded(title) {
    if (title === '$:/StoryList' || title === '$:/HistoryList' || title === '$:/library/sjcl.js') return true;
    if (title.indexOf("Draft of '") === 0) return true;
    if (title.indexOf('$:/TiddlyDesktopRS/Conflicts/') === 0) return true;
    if (title.indexOf('$:/state/') === 0) return true;
    if (title.indexOf('$:/status/') === 0) return true;
    if (title.indexOf('$:/temp/') === 0) return true;
    if (title.indexOf('$:/plugins/tiddlydesktop-rs/') === 0) return true;
    if (title.indexOf('$:/plugins/tiddlydesktop/') === 0) return true;
    if (title.indexOf('$:/config/ViewToolbarButtons/Visibility/$:/plugins/tiddlydesktop-rs/') === 0) return true;
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

    // Compare incoming tiddler fields with local — returns true if they differ.
    // Includes 'modified' so fingerprint-based sync converges (fingerprints use
    // modified for diff detection; skipping it here would cause infinite loops).
    function tiddlerDiffers(fields) {
      var existing = $tw.wiki.getTiddler(fields.title);
      if (!existing) return true;
      var ef = existing.fields;
      // Check modified timestamp (fingerprint sync depends on this converging)
      var remoteMod = fields.modified ? String(fields.modified) : '';
      var localMod = ef.modified ? fieldToString(ef.modified) : '';
      if (remoteMod !== localMod) return true;
      // Check all incoming fields exist and match in local
      var keys = Object.keys(fields);
      for (var i = 0; i < keys.length; i++) {
        var k = keys[i];
        if (k === 'created' || k === 'modified') continue;
        var v1 = String(fields[k]);
        var v2 = ef[k];
        if (v2 === undefined) return true;
        if (v1 !== fieldToString(v2)) return true;
      }
      // Check local doesn't have extra fields not in incoming (skip created only)
      var eKeys = Object.keys(ef);
      for (var j = 0; j < eKeys.length; j++) {
        var ek = eKeys[j];
        if (ek === 'created' || ek === 'modified') continue;
        if (fields[ek] === undefined) return true;
      }
      return false;
    }

    // Collect fingerprints for all syncable tiddlers, including titles we
    // received from peers but skipped as identical (shadow-only on this side).
    function collectFingerprints() {
      var titles = getSyncableTitles();
      var seen = {};
      var fps = [];
      for (var i = 0; i < titles.length; i++) {
        var t = $tw.wiki.getTiddler(titles[i]);
        if (t) {
          var mod = t.fields.modified;
          fps.push({ title: titles[i], modified: mod ? fieldToString(mod) : '' });
          seen[titles[i]] = true;
        }
      }
      var knownKeys = Object.keys(knownSyncTitles);
      for (var j = 0; j < knownKeys.length; j++) {
        if (!seen[knownKeys[j]]) {
          fps.push({ title: knownKeys[j], modified: knownSyncTitles[knownKeys[j]] });
        }
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

        if (changes[title].deleted) {
          pendingOutbound[title] = { deleted: true, tiddlerJson: null };
        } else {
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
      for (var i = 0; i < batch.length; i++) {
        var data = batch[i];

        if (data.type === 'apply-change') {
          try {
            var fields = JSON.parse(data.tiddler_json);
            if (tiddlerDiffers(fields)) {
              // Parse date strings to Date objects so TiddlyWiki stores them correctly
              if (fields.created) fields.created = $tw.utils.parseDate(fields.created);
              if (fields.modified) fields.modified = $tw.utils.parseDate(fields.modified);
              suppressOutbound.add(fields.title);
              $tw.wiki.addTiddler(new $tw.Tiddler(fields));
              needSave = true;
            } else {
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
          } catch (e) {
            console.error('[LAN Sync] Failed to apply remote deletion:', e);
          }

        } else if (data.type === 'conflict') {
          var title = data.title;
          var localTiddler = $tw.wiki.getTiddler(title);
          if (localTiddler) {
            var conflictTitle = '$:/TiddlyDesktopRS/Conflicts/' + title;
            conflictTitles.add(conflictTitle);
            var conflictFields = Object.assign({}, localTiddler.fields, {
              title: conflictTitle,
              'conflict-original-title': title,
              'conflict-timestamp': new Date().toISOString(),
              'conflict-source': 'local'
            });
            $tw.wiki.addTiddler(new $tw.Tiddler(conflictFields));
            setTimeout(function() { conflictTitles.delete(conflictTitle); }, 500);
          }
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
      var fingerprints = collectFingerprints();

      console.log('[LAN Sync] Sending ' + fingerprints.length + ' fingerprints to ' + toDeviceId);

      sendFingerprints(wikiId, toDeviceId, fingerprints);
    }

    function handleCompareFingerprints(fromDeviceId, peerFingerprints) {
      // Build a map of peer's tiddlers: title → modified
      var peerMap = {};
      for (var i = 0; i < peerFingerprints.length; i++) {
        peerMap[peerFingerprints[i].title] = peerFingerprints[i].modified;
      }

      // Find tiddlers we have that the peer needs (missing or our version is newer)
      var titles = getSyncableTitles();
      var toSend = [];

      for (var j = 0; j < titles.length; j++) {
        var title = titles[j];
        var tiddler = $tw.wiki.getTiddler(title);
        if (!tiddler) continue;

        var localMod = tiddler.fields.modified ? fieldToString(tiddler.fields.modified) : '';

        if (!(title in peerMap)) {
          // Peer doesn't have this tiddler — send it
          toSend.push(title);
        } else if (localMod && peerMap[title] && localMod > peerMap[title]) {
          // Our version is newer — send it
          toSend.push(title);
        }
      }

      _log('[LAN Sync] Fingerprint diff: ' + toSend.length + ' tiddlers to send to ' + fromDeviceId +
                  ' (of ' + titles.length + ' local, peer has ' + peerFingerprints.length + ')');

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
      function pollIpc() {
        if (!ipcPollActive) return;
        window.__TAURI__.core.invoke('lan_sync_poll_ipc').then(function(messages) {
          if (messages && messages.length) {
            _log('[LAN Sync] IPC poll: ' + messages.length + ' messages');
            for (var i = 0; i < messages.length; i++) {
              try {
                var data = JSON.parse(messages[i]);
                if (!data || !data.type) continue;
                if (data.wiki_id && data.wiki_id !== wikiId) {
                  _log('[LAN Sync] IPC skip: wiki_id mismatch (' + data.wiki_id + ' vs ' + wikiId + ')');
                  continue;
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

    // ── Periodic re-sync (5s safety net) ──────────────────────────────
    // Periodically re-broadcast fingerprints so peers detect diffs and
    // re-send missed changes.  Converges quickly then becomes a no-op
    // once both sides have the same tiddler set.
    var resyncIntervalId = setInterval(function() {
      try {
        var fps = collectFingerprints();
        broadcastFingerprints(wikiId, fps);
      } catch (e) {
        _log('[LAN Sync] Periodic resync error: ' + e);
      }
    }, 5000);
    state.unlistenFns.push(function() { clearInterval(resyncIntervalId); });

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
})();

