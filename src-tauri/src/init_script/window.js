// TiddlyDesktop Initialization Script - Window Module
// Handles: window close with unsaved changes check, window state persistence

(function(TD) {
    'use strict';

    // Save window state (size, position, monitor) for this wiki
    function saveWindowState(callback) {
        var invoke = window.__TAURI__.core.invoke;
        var wikiPath = window.__WIKI_PATH__;

        // Don't save state for tiddler windows
        if (window.__SINGLE_TIDDLER_TITLE__) {
            if (callback) callback();
            return;
        }

        // Use special key for landing page, otherwise use wiki path
        var stateKey = window.__IS_MAIN_WIKI__ ? '__LANDING_PAGE__' : wikiPath;
        if (!stateKey) {
            if (callback) callback();
            return;
        }

        invoke('get_window_state_info').then(function(state) {
            return invoke('save_window_state', {
                path: stateKey,
                width: state.width,
                height: state.height,
                x: state.x,
                y: state.y,
                monitorName: state.monitor_name,
                monitorX: state.monitor_x,
                monitorY: state.monitor_y,
                maximized: state.maximized
            });
        }).then(function() {
            if (callback) callback();
        }).catch(function(err) {
            console.error('[TiddlyDesktop] Failed to save window state:', err);
            if (callback) callback();
        });
    }

    function setupCloseHandler() {
        if (typeof window.__TAURI__ === 'undefined' || !window.__TAURI__.event) {
            setTimeout(setupCloseHandler, 100);
            return;
        }

        var getCurrentWindow = window.__TAURI__.window.getCurrentWindow;
        var invoke = window.__TAURI__.core.invoke;
        var appWindow = getCurrentWindow();

        appWindow.onCloseRequested(function(event) {
            // Always prevent close first, then decide what to do
            event.preventDefault();

            // Tiddler windows (single-tiddler view) should close without prompting
            // The source wiki handles saving, not the tiddler window
            if (window.__SINGLE_TIDDLER_TITLE__) {
                invoke('close_window');
                return;
            }

            // Landing page (main wiki) should close without prompting
            // It doesn't have user data that needs saving
            if (window.__IS_MAIN_WIKI__) {
                saveWindowState(function() {
                    invoke('close_window');
                });
                return;
            }

            // Check if TiddlyWiki has unsaved changes
            var isDirty = false;
            if (typeof $tw !== 'undefined' && $tw.wiki) {
                if (typeof $tw.wiki.isDirty === 'function') {
                    isDirty = $tw.wiki.isDirty();
                } else if ($tw.saverHandler && typeof $tw.saverHandler.isDirty === 'function') {
                    isDirty = $tw.saverHandler.isDirty();
                } else if ($tw.saverHandler && typeof $tw.saverHandler.numChanges === 'function') {
                    isDirty = $tw.saverHandler.numChanges() > 0;
                } else if (document.title && document.title.startsWith('*')) {
                    isDirty = true;
                } else if ($tw.syncer && typeof $tw.syncer.isDirty === 'function') {
                    isDirty = $tw.syncer.isDirty();
                }
            }

            // Save window state before closing
            var closeWindow = function() {
                saveWindowState(function() {
                    invoke('close_window');
                });
            };

            if (isDirty) {
                TD.showConfirmModal('You have unsaved changes. Are you sure you want to close?', function(confirmed) {
                    if (confirmed) {
                        closeWindow();
                    }
                });
            } else {
                closeWindow();
            }
        });
    }

    setupCloseHandler();

    // Intercept clicks on links with external protocols (mailto:, tel:, etc.)
    // and external http/https URLs — open via the OS-assigned handler.
    function setupExternalProtocolHandler() {
        if (typeof window.__TAURI__ === 'undefined' || !window.__TAURI__.opener) {
            setTimeout(setupExternalProtocolHandler, 100);
            return;
        }
        // Protocols that should always stay in the WebView
        var internalProtocols = {
            'wikifile:': true, 'tdasset:': true, 'tdlib:': true,
            'data:': true, 'blob:': true, 'javascript:': true, '': true
        };
        // Our wiki origins that should stay in the WebView
        var ownOrigin = window.location.origin; // e.g. wikifile://localhost or http://127.0.0.1:port
        document.addEventListener('click', function(e) {
            var link = e.target.closest('a[href]');
            if (!link) return;
            var href = link.getAttribute('href');
            if (!href) return;
            // Parse protocol
            var colonIdx = href.indexOf(':');
            if (colonIdx < 1) return; // relative URL or no protocol
            var protocol = href.substring(0, colonIdx + 1).toLowerCase();
            if (internalProtocols[protocol]) return;
            // http/https: open externally unless it's our own wiki server
            if (protocol === 'http:' || protocol === 'https:') {
                try {
                    var url = new URL(href);
                    if (url.origin === ownOrigin) return; // our wiki, stay in WebView
                } catch(ex) { return; }
            }
            // External protocol or external website — open via system handler
            e.preventDefault();
            e.stopPropagation();
            window.__TAURI__.opener.openUrl(href).catch(function(err) {
                console.error('[TiddlyDesktop] Failed to open external URL:', err, href);
            });
        }, true); // capture phase to intercept before TiddlyWiki handlers
        console.log('[TiddlyDesktop] External protocol handler installed');
    }
    setupExternalProtocolHandler();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
