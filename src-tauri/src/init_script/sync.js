// TiddlyDesktop Initialization Script - Sync Module
// Handles: tm-open-window handlers, cross-window tiddler synchronization

(function(TD) {
    'use strict';

    function setupWindowHandlers() {
        function waitForTiddlyWikiReady() {
            if (typeof $tw === 'undefined' || !$tw.rootWidget) {
                setTimeout(waitForTiddlyWikiReady, 100);
                return;
            }

            // Skip main wiki - it uses its own startup.js handlers
            if (window.__IS_MAIN_WIKI__) {
                console.log('[TiddlyDesktop] Main wiki - window handlers not needed');
                return;
            }

            if (!window.__TAURI__ || !window.__TAURI__.core) {
                setTimeout(waitForTiddlyWikiReady, 100);
                return;
            }

            var invoke = window.__TAURI__.core.invoke;
            var windowLabel = window.__WINDOW_LABEL__ || 'unknown';

            // Store references to opened Tauri windows
            window.__tiddlyDesktopWindows = window.__tiddlyDesktopWindows || {};

            // tm-open-window handler - opens tiddler in new window
            $tw.rootWidget.addEventListener('tm-open-window', function(event) {
                var title = event.param || event.tiddlerTitle;
                var paramObject = event.paramObject || {};
                var windowTitle = paramObject.windowTitle || title;
                var windowID = paramObject.windowID || title;
                var template = paramObject.template || '$:/core/templates/single.tiddler.window';
                var width = paramObject.width ? parseFloat(paramObject.width) : null;
                var height = paramObject.height ? parseFloat(paramObject.height) : null;
                var left = paramObject.left ? parseFloat(paramObject.left) : null;
                var top = paramObject.top ? parseFloat(paramObject.top) : null;

                // Collect any additional variables
                var knownParams = ['windowTitle', 'windowID', 'template', 'width', 'height', 'left', 'top'];
                var extraVariables = {};
                for (var key in paramObject) {
                    if (paramObject.hasOwnProperty(key) && knownParams.indexOf(key) === -1) {
                        extraVariables[key] = paramObject[key];
                    }
                }
                extraVariables.currentTiddler = title;
                extraVariables['tv-window-id'] = windowID;

                invoke('open_tiddler_window', {
                    parentLabel: windowLabel,
                    tiddlerTitle: title,
                    template: template,
                    windowTitle: windowTitle,
                    width: width,
                    height: height,
                    left: left,
                    top: top,
                    variables: JSON.stringify(extraVariables)
                }).then(function(newLabel) {
                    window.__tiddlyDesktopWindows[windowID] = { label: newLabel, title: title };
                }).catch(function(err) {
                    console.error('[TiddlyDesktop] Failed to open tiddler window:', err);
                });

                return false;
            });

            // tm-close-window handler
            $tw.rootWidget.addEventListener('tm-close-window', function(event) {
                var windowID = event.param;
                var windows = window.__tiddlyDesktopWindows || {};
                if (windows[windowID]) {
                    var windowInfo = windows[windowID];
                    invoke('close_window_by_label', { label: windowInfo.label }).catch(function(err) {
                        console.error('[TiddlyDesktop] Failed to close window:', err);
                    });
                    delete windows[windowID];
                }
                return false;
            });

            // tm-close-all-windows handler
            $tw.rootWidget.addEventListener('tm-close-all-windows', function(event) {
                var windows = window.__tiddlyDesktopWindows || {};
                Object.keys(windows).forEach(function(windowID) {
                    var windowInfo = windows[windowID];
                    invoke('close_window_by_label', { label: windowInfo.label }).catch(function() {});
                });
                window.__tiddlyDesktopWindows = {};
                return false;
            });

            // tm-open-external-window handler - opens URL in default browser
            $tw.rootWidget.addEventListener('tm-open-external-window', function(event) {
                var url = event.param || 'https://tiddlywiki.com/';
                if (window.__TAURI__ && window.__TAURI__.opener) {
                    window.__TAURI__.opener.openUrl(url).catch(function(err) {
                        console.error('[TiddlyDesktop] Failed to open external URL:', err);
                    });
                }
                return false;
            });

            // ========================================
            // Cross-window tiddler synchronization
            // ========================================
            var wikiPath = window.__WIKI_PATH__ || '';
            var currentWindowLabel = window.__WINDOW_LABEL__ || 'unknown';
            var isReceivingSync = false;
            var emit = window.__TAURI__.event.emit;
            var listen = window.__TAURI__.event.listen;

            // Listen for tiddler changes from other windows
            listen('wiki-tiddler-change', function(event) {
                var payload = event.payload;
                if (payload.wikiPath === wikiPath && payload.sourceWindow !== currentWindowLabel) {
                    isReceivingSync = true;
                    try {
                        if (payload.deleted) {
                            $tw.wiki.deleteTiddler(payload.title);
                        } else if (payload.tiddler) {
                            $tw.wiki.addTiddler(new $tw.Tiddler(payload.tiddler));
                        }
                    } finally {
                        setTimeout(function() { isReceivingSync = false; }, 0);
                    }
                }
            });

            // Watch for local tiddler changes and broadcast to other windows
            $tw.wiki.addEventListener('change', function(changes) {
                if (isReceivingSync) return;

                Object.keys(changes).forEach(function(title) {
                    var tiddler = $tw.wiki.getTiddler(title);
                    var payload = {
                        wikiPath: wikiPath,
                        sourceWindow: currentWindowLabel,
                        title: title,
                        deleted: changes[title].deleted,
                        tiddler: tiddler ? tiddler.fields : null
                    };

                    emit('wiki-tiddler-change', payload);
                });
            });

            console.log('[TiddlyDesktop] Window message handlers ready, sync enabled for:', wikiPath);
        }

        waitForTiddlyWikiReady();
    }

    setupWindowHandlers();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
