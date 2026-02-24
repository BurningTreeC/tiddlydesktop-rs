// Peer status badge — shows connected LAN sync peers in the TopRightBar
// Creates wikitext badge UI tiddlers and data tiddlers that drive the badge.
// Desktop: peer data is pushed from main process via IPC and handled by lan_sync.js.
// Android: peer data is polled from the bridge here.
(function() {
  'use strict';

  // Only run in wiki windows, not the landing page
  if (!window.__WIKI_PATH__) return;
  if (window.__WINDOW_LABEL__ === 'main') return;

  // Transport detection (same pattern as lan_sync.js)
  var isAndroid = typeof window.TiddlyDesktopSync !== 'undefined';
  var hasTauri = !!(window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke);

  // Skip if no sync transport available
  if (!isAndroid && !hasTauri) return;

  var POLL_INTERVAL = 5000;
  var PEERS_TIDDLER = '$:/temp/tiddlydesktop/connected-peers';
  var COUNT_TIDDLER = '$:/temp/tiddlydesktop/peer-count';
  var BADGE_TIDDLER = '$:/temp/tiddlydesktop/PeerBadge';
  var EDITING_BADGE_TIDDLER = '$:/temp/tiddlydesktop/EditingBadge';

  var lastPeersJson = '';

  function waitForTw(cb) {
    if (typeof $tw !== 'undefined' && $tw.wiki && $tw.wiki.addTiddler) {
      cb();
    } else {
      setTimeout(function() { waitForTw(cb); }, 200);
    }
  }

  function announceUsername(name) {
    if (isAndroid) {
      try { window.TiddlyDesktopSync.announceUsername(name); } catch (_) {}
    } else if (hasTauri) {
      window.__TAURI__.core.invoke('lan_sync_announce_username', {
        userName: name
      }).catch(function() {});
    }
  }

  function updatePeerData(peers) {
    // Convert array to object keyed by device_id for jsonindexes compatibility
    var peersObj = {};
    for (var i = 0; i < peers.length; i++) {
      peersObj[peers[i].device_id] = peers[i];
    }
    var peersJson = JSON.stringify(peersObj);

    // Skip if unchanged
    if (peersJson === lastPeersJson) return;
    lastPeersJson = peersJson;

    var count = String(peers.length);

    $tw.wiki.addTiddler({
      title: PEERS_TIDDLER,
      type: 'application/json',
      text: peersJson
    });
    $tw.wiki.addTiddler({
      title: COUNT_TIDDLER,
      text: count
    });
  }

  function createBadgeTiddler() {
    // Only create if it doesn't already exist
    if ($tw.wiki.tiddlerExists(BADGE_TIDDLER)) return;

    var wikitext =
      '\\define peer-badge-styles()\n' +
      '.td-peer-badge { display: inline-block; cursor: pointer; padding: 2px 6px; position: relative; }\n' +
      '.td-peer-badge svg { width: 18px; height: 18px; vertical-align: middle; fill: <<colour foreground>>; }\n' +
      '.td-peer-badge-count { font-size: 0.75em; vertical-align: top; margin-left: 1px; }\n' +
      '.td-peer-dropdown { position: absolute; right: 0; top: 100%; background: <<colour dropdown-background>>; ' +
        'border: 1px solid <<colour dropdown-border>>; border-radius: 4px; padding: 6px 0; ' +
        'min-width: 200px; box-shadow: 1px 1px 5px rgba(0,0,0,0.15); z-index: 1000; white-space: nowrap; }\n' +
      '.td-peer-dropdown-item { padding: 4px 12px; font-size: 0.85em; }\n' +
      '.td-peer-dropdown-item-name { font-weight: bold; }\n' +
      '.td-peer-dropdown-item-device { color: <<colour muted-foreground>>; font-size: 0.85em; }\n' +
      '.td-peer-dropdown-empty { padding: 8px 12px; color: <<colour muted-foreground>>; font-size: 0.85em; }\n' +
      '\\end\n' +
      '<$reveal type="nomatch" state="' + COUNT_TIDDLER + '" text="0" default="0">\n' +
      '<$reveal type="nomatch" state="' + COUNT_TIDDLER + '" text="" default="0">\n' +
      '<$button popup="$:/state/tiddlydesktop/peer-dropdown" class="tc-btn-invisible td-peer-badge" tooltip="Connected peers">\n' +
      '<$text text={{' + COUNT_TIDDLER + '}}/>\n' +
      '{{$:/core/images/globe}}\n' +
      '</$button>\n' +
      '<$reveal state="$:/state/tiddlydesktop/peer-dropdown" type="popup" position="belowleft">\n' +
      '<div class="td-peer-dropdown">\n' +
      '<$list filter="[{' + PEERS_TIDDLER + '}jsonindexes[]]" variable="idx" emptyMessage="""<div class="td-peer-dropdown-empty">No peers connected</div>""">\n' +
      '<div class="td-peer-dropdown-item">\n' +
      '<$let userName={{{ [{' + PEERS_TIDDLER + '}jsonget<idx>,[user_name]] }}} deviceName={{{ [{' + PEERS_TIDDLER + '}jsonget<idx>,[device_name]] }}}>\n' +
      '<$reveal type="nomatch" default=<<userName>> text="">\n' +
      '<span class="td-peer-dropdown-item-name"><$text text=<<userName>>/></span>\n' +
      ' <span class="td-peer-dropdown-item-device">(<$text text=<<deviceName>>/>)</span>\n' +
      '</$reveal>\n' +
      '<$reveal type="match" default=<<userName>> text="">\n' +
      '<span class="td-peer-dropdown-item-name">Anonymous</span>\n' +
      ' <span class="td-peer-dropdown-item-device">(<$text text=<<deviceName>>/>)</span>\n' +
      '</$reveal>\n' +
      '</$let>\n' +
      '</div>\n' +
      '</$list>\n' +
      '</div>\n' +
      '</$reveal>\n' +
      '</$reveal>\n' +
      '</$reveal>\n' +
      '<style>\n' +
      '<<peer-badge-styles>>\n' +
      '</style>\n';

    $tw.wiki.addTiddler({
      title: BADGE_TIDDLER,
      tags: '$:/tags/TopRightBar',
      text: wikitext
    });
  }

  function createEditingBadgeTiddler() {
    if ($tw.wiki.tiddlerExists(EDITING_BADGE_TIDDLER)) return;

    var wikitext =
      '\\define editing-badge-styles()\n' +
      '.td-editing-badge { display: inline-block; font-size: 0.8em; padding: 2px 8px; margin: 0 0 4px 0; ' +
        'border-radius: 10px; background: <<colour notification-background>>; ' +
        'border: 1px solid <<colour notification-border>>; color: <<colour foreground>>; }\n' +
      '.td-editing-badge svg { width: 14px; height: 14px; vertical-align: middle; fill: <<colour foreground>>; margin-right: 3px; }\n' +
      '\\end\n' +
      '<$set name="editingTid" value={{{ [[$:/temp/tiddlydesktop/editing/]addsuffix<currentTiddler>] }}}>\n' +
      '<$list filter="[<editingTid>is[tiddler]]" variable="ignore">\n' +
      '<div class="td-editing-badge">\n' +
      '{{$:/core/images/edit-button}} \n' +
      '<$list filter="[<editingTid>get[text]jsonindexes[]]" variable="idx" counter="cnt">\n' +
      '<$let un={{{ [<editingTid>get[text]jsonget<idx>,[user_name]] }}} dn={{{ [<editingTid>get[text]jsonget<idx>,[device_name]] }}}>\n' +
      '<$reveal type="nomatch" default=<<un>> text="">\n' +
      '<$text text=<<un>>/>\n' +
      '</$reveal>\n' +
      '<$reveal type="match" default=<<un>> text="">\n' +
      '<$text text=<<dn>>/>\n' +
      '</$reveal>\n' +
      '</$let>\n' +
      '<$list filter="[<editingTid>get[text]jsonindexes[]count[]compare:number:gt<cnt-first>]" variable="ignore">, </$list>\n' +
      '</$list>\n' +
      '</div>\n' +
      '</$list>\n' +
      '</$set>\n' +
      '<style>\n' +
      '<<editing-badge-styles>>\n' +
      '</style>\n';

    $tw.wiki.addTiddler({
      title: EDITING_BADGE_TIDDLER,
      tags: '$:/tags/ViewTemplate',
      'list-before': '$:/core/ui/ViewTemplate/body',
      text: wikitext
    });
  }

  waitForTw(function() {
    // Initialize data tiddlers with empty state
    $tw.wiki.addTiddler({ title: PEERS_TIDDLER, type: 'application/json', text: '{}' });
    $tw.wiki.addTiddler({ title: COUNT_TIDDLER, text: '0' });

    // Create the badge UIs
    createBadgeTiddler();
    createEditingBadgeTiddler();

    // Announce our username
    var userName = $tw.wiki.getTiddlerText('$:/status/UserName') || '';
    if (userName) {
      announceUsername(userName);
    }

    // Watch for username changes
    $tw.wiki.addEventListener('change', function(changes) {
      if (changes['$:/status/UserName']) {
        var newName = $tw.wiki.getTiddlerText('$:/status/UserName') || '';
        announceUsername(newName);
        // Also update collab cursor username in CM6 editors
        try {
          var collabPlugin = require('$:/plugins/tiddlywiki/codemirror-6-collab/collab.js');
          if (collabPlugin && collabPlugin.updateUserName) {
            collabPlugin.updateUserName(newName || 'Anonymous');
          }
        } catch (_) {}
      }
    });

    // Android: poll for peers via bridge (sync manager is in-process)
    // Desktop: peer data is pushed from main process via IPC → lan_sync.js handles it
    if (isAndroid) {
      var currentSyncId = '';

      function getSyncId(cb) {
        try {
          var id = window.TiddlyDesktopSync.getSyncId(window.__WIKI_PATH__ || '');
          cb(id || '');
        } catch (_) { cb(''); }
      }

      function fetchWikiPeers(wikiId, cb) {
        try {
          var json = window.TiddlyDesktopSync.getWikiPeers(wikiId);
          cb(JSON.parse(json || '[]'));
        } catch (_) { cb([]); }
      }

      function poll() {
        if (!currentSyncId) {
          getSyncId(function(id) {
            if (id) {
              currentSyncId = id;
              fetchWikiPeers(currentSyncId, updatePeerData);
            }
          });
        } else {
          fetchWikiPeers(currentSyncId, updatePeerData);
        }
      }

      poll();
      setInterval(poll, POLL_INTERVAL);
    }
  });
})();
