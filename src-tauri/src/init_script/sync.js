// TiddlyDesktop Initialization Script - Sync Module
// Handles: tm-open-window, tm-full-screen, tm-print, tm-download-file handlers,
//          cross-window tiddler synchronization (via IPC for multi-process)

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
            var listen = window.__TAURI__.event.listen;
            var windowLabel = window.__WINDOW_LABEL__ || 'unknown';
            var wikiPath = window.__WIKI_PATH__ || '';

            // Check if we're in a wiki mode process (has IPC commands available)
            var isWikiProcess = true; // Assume wiki process, will fallback gracefully

            // Store references to opened tiddler windows (by windowID)
            window.__tiddlyDesktopWindows = window.__tiddlyDesktopWindows || {};

            // Remove TiddlyWiki's built-in tm-open-window/close/close-all handlers
            // (from core/modules/startup/windows.js) which use window.open() — that
            // doesn't work in Tauri webviews and causes "Cannot read properties of
            // undefined (reading 'document')" errors.
            $tw.rootWidget.eventListeners['tm-open-window'] = [];
            $tw.rootWidget.eventListeners['tm-close-window'] = [];
            $tw.rootWidget.eventListeners['tm-close-all-windows'] = [];

            // tm-open-window handler - opens tiddler in new window (same process, shares $tw.wiki)
            $tw.rootWidget.addEventListener('tm-open-window', function(event) {
                var title = event.param || event.tiddlerTitle;
                var paramObject = event.paramObject || {};
                var template = paramObject.template || '$:/core/templates/single.tiddler.window';
                var windowTitle = paramObject.windowTitle || title;
                var windowID = paramObject.windowID || title;
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

                // Open tiddler window in same process (shares $tw.wiki)
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
                    console.log('[TiddlyDesktop] Tiddler window opened:', newLabel);
                }).catch(function(err) {
                    console.error('[TiddlyDesktop] Failed to open tiddler window:', err);
                });

                return false;
            });

            // tm-close-window handler - close tiddler windows
            // Note: In multi-process mode, tiddler windows manage their own lifecycle
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

            // tm-full-screen handler - toggle fullscreen using Tauri window API
            // TiddlyWiki's native Fullscreen API doesn't work reliably in webviews
            $tw.rootWidget.addEventListener('tm-full-screen', function(event) {
                invoke('toggle_fullscreen').then(function(isFullscreen) {
                    console.log('[TiddlyDesktop] Fullscreen:', isFullscreen);
                }).catch(function(err) {
                    console.error('[TiddlyDesktop] Failed to toggle fullscreen:', err);
                });
                return false;
            });

            // tm-print handler - print using Tauri's webview print API
            // More reliable than window.print() in webviews
            $tw.rootWidget.addEventListener('tm-print', function(event) {
                invoke('print_page').then(function() {
                    console.log('[TiddlyDesktop] Print dialog opened');
                }).catch(function(err) {
                    console.error('[TiddlyDesktop] Failed to print:', err);
                });
                return false;
            });

            // Intercept Blob downloads - TW's download saver creates a Blob,
            // builds an <a download="filename"> element, and clicks it.
            // We intercept that click, read the Blob, and show Tauri's save dialog instead.
            // This works for all TW versions and all export formats.
            var _origCreateObjectURL = URL.createObjectURL;
            var _pendingBlobs = {};

            URL.createObjectURL = function(obj) {
                var url = _origCreateObjectURL.call(URL, obj);
                if (obj instanceof Blob) {
                    _pendingBlobs[url] = obj;
                }
                return url;
            };

            function handleDownloadAnchor(anchor) {
                if (!anchor || anchor.tagName !== 'A' || !anchor.hasAttribute('download')) return false;

                var href = anchor.href || anchor.getAttribute('href') || '';
                var filename = anchor.download || anchor.getAttribute('download') || 'download';
                var blob = _pendingBlobs[href];

                // Handle blob: URLs
                if (blob) {
                    var reader = new FileReader();
                    reader.onload = function() {
                        invoke('download_file', {
                            filename: filename,
                            content: reader.result,
                            contentType: blob.type || 'text/plain'
                        }).then(function(savedPath) {
                            console.log('[TiddlyDesktop] File saved to:', savedPath);
                            if (typeof $tw !== 'undefined' && $tw.notifier) {
                                $tw.notifier.display('$:/language/Notifications/Save/Done');
                            }
                        }).catch(function(err) {
                            if (err !== 'Save cancelled') {
                                console.error('[TiddlyDesktop] Failed to save file:', err);
                            }
                        });
                    };
                    reader.readAsText(blob);
                    delete _pendingBlobs[href];
                    URL.revokeObjectURL(href);
                    return true;
                }

                // Handle data: URIs (fallback when Blob is unavailable)
                if (href.indexOf('data:') === 0) {
                    var commaIdx = href.indexOf(',');
                    if (commaIdx !== -1) {
                        var meta = href.substring(5, commaIdx);
                        var encoded = href.substring(commaIdx + 1);
                        var content = meta.indexOf('base64') !== -1
                            ? atob(encoded)
                            : decodeURIComponent(encoded);
                        var contentType = meta.split(';')[0] || 'text/plain';
                        invoke('download_file', {
                            filename: filename,
                            content: content,
                            contentType: contentType
                        }).catch(function(err) {
                            if (err !== 'Save cancelled') {
                                console.error('[TiddlyDesktop] Failed to save file:', err);
                            }
                        });
                        return true;
                    }
                }

                return false;
            }

            // Method 1: Capture-phase click listener for DOM-attached anchors
            document.addEventListener('click', function(e) {
                var anchor = e.target;
                while (anchor && anchor.tagName !== 'A') {
                    anchor = anchor.parentElement;
                }
                if (handleDownloadAnchor(anchor)) {
                    e.preventDefault();
                    e.stopPropagation();
                }
            }, true);

            // Method 2: Override HTMLAnchorElement.prototype.click to catch
            // detached anchors (not in DOM) that some TW versions may use
            var _origAnchorClick = HTMLAnchorElement.prototype.click;
            HTMLAnchorElement.prototype.click = function() {
                if (handleDownloadAnchor(this)) {
                    return; // Intercepted - don't trigger browser download
                }
                return _origAnchorClick.call(this);
            };

            // ========================================
            // Cross-window tiddler synchronization (via Tauri events)
            // Works between WebviewWindows in the same process
            // ========================================
            var isReceivingSync = false;
            var emit = window.__TAURI__.event.emit;
            var isTiddlerWindow = !!window.__SINGLE_TIDDLER_TITLE__;

            // Track tiddlers modified since last save (only for source wikis)
            var unsavedChanges = {};
            // Tiddler windows wait for initial sync before broadcasting
            var initialSyncReceived = !isTiddlerWindow; // Source wikis are ready immediately

            // Listen for tiddler changes from other windows (same process)
            listen('wiki-tiddler-change', function(event) {
                var payload = event.payload;
                // Only apply if same wiki and different window
                if (payload.wikiPath === wikiPath && payload.sourceWindow !== windowLabel) {
                    isReceivingSync = true;
                    // Mark initial sync as received for tiddler windows
                    if (isTiddlerWindow && !initialSyncReceived) {
                        initialSyncReceived = true;
                    }
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

            // Listen for sync requests from newly opened tiddler windows
            listen('wiki-sync-request', function(event) {
                var payload = event.payload;
                // Only respond if same wiki, different window, and we're not a tiddler window
                if (payload.wikiPath === wikiPath && payload.sourceWindow !== windowLabel && !isTiddlerWindow) {
                    console.log('[TiddlyDesktop] Sync request from:', payload.sourceWindow);

                    // Helper to send a tiddler
                    function sendTiddler(title) {
                        var tiddler = $tw.wiki.getTiddler(title);
                        if (tiddler) {
                            emit('wiki-tiddler-change', {
                                wikiPath: wikiPath,
                                sourceWindow: windowLabel,
                                title: title,
                                deleted: false,
                                tiddler: tiddler.fields
                            });
                            return true;
                        }
                        return false;
                    }

                    var count = 0;

                    // Always send StoryList and HistoryList (current navigation state)
                    if (sendTiddler('$:/StoryList')) count++;
                    if (sendTiddler('$:/HistoryList')) count++;

                    // Send unsaved changes (tiddlers modified since last save)
                    Object.keys(unsavedChanges).forEach(function(title) {
                        // Skip StoryList/HistoryList as we already sent them
                        if (title === '$:/StoryList' || title === '$:/HistoryList') return;

                        var change = unsavedChanges[title];
                        if (change.deleted) {
                            emit('wiki-tiddler-change', {
                                wikiPath: wikiPath,
                                sourceWindow: windowLabel,
                                title: title,
                                deleted: true,
                                tiddler: null
                            });
                            count++;
                        } else {
                            if (sendTiddler(title)) count++;
                        }
                    });
                    console.log('[TiddlyDesktop] Sent', count, 'tiddlers (including StoryList/HistoryList)');
                }
            });

            // Watch for local tiddler changes and broadcast to other windows
            $tw.wiki.addEventListener('change', function(changes) {
                if (isReceivingSync) return;
                // Tiddler windows don't broadcast until initial sync is received
                if (!initialSyncReceived) return;

                Object.keys(changes).forEach(function(title) {
                    var tiddler = $tw.wiki.getTiddler(title);
                    var deleted = changes[title].deleted;

                    // Track unsaved changes (for sync requests) - only for source wikis
                    if (!isTiddlerWindow) {
                        unsavedChanges[title] = { deleted: deleted };
                    }

                    // Broadcast to other windows
                    emit('wiki-tiddler-change', {
                        wikiPath: wikiPath,
                        sourceWindow: windowLabel,
                        title: title,
                        deleted: deleted,
                        tiddler: tiddler ? tiddler.fields : null
                    });
                });
            });

            // Clear unsaved changes tracking when wiki is saved
            // But keep $:/StoryList and $:/HistoryList (session state, not saved to file)
            function clearSavedChanges() {
                var storyList = unsavedChanges['$:/StoryList'];
                var historyList = unsavedChanges['$:/HistoryList'];
                unsavedChanges = {};
                if (storyList) unsavedChanges['$:/StoryList'] = storyList;
                if (historyList) unsavedChanges['$:/HistoryList'] = historyList;
            }
            // Hook into save events WITHOUT replacing existing handlers
            // TiddlyWiki < 5.3.7 uses single-function eventListeners (not arrays),
            // so addEventListener would REPLACE SaverHandler's tm-save-wiki handler
            function wrapEventListener(eventType, extraFn) {
                var existing = $tw.rootWidget.eventListeners && $tw.rootWidget.eventListeners[eventType];
                $tw.rootWidget.addEventListener(eventType, function(event) {
                    extraFn();
                    // Call the original handler and preserve its return value
                    // In TW < 5.3.7 existing is a function; in 5.3.7+ it's an array
                    // (addEventListener appends, so originals are still called by TW)
                    if (typeof existing === 'function') {
                        return existing(event);
                    }
                    return true; // Allow propagation if no existing handler
                });
            }
            wrapEventListener('tm-auto-save-wiki', clearSavedChanges);
            wrapEventListener('tm-save-wiki', clearSavedChanges);

            // If this is a tiddler window, request unsaved changes from source wiki
            if (isTiddlerWindow) {
                console.log('[TiddlyDesktop] Tiddler window requesting unsaved changes');
                emit('wiki-sync-request', {
                    wikiPath: wikiPath,
                    sourceWindow: windowLabel
                });
            }

            console.log('[TiddlyDesktop] Sync handlers ready for:', wikiPath);
        }

        waitForTiddlyWikiReady();
    }

    setupWindowHandlers();

    // Listen for wiki favicon updates (main wiki only)
    // This allows the landing page to update favicons in real-time
    function setupFaviconUpdateListener() {
        if (!window.__IS_MAIN_WIKI__) {
            return; // Only relevant for main wiki
        }

        function waitForTauri() {
            if (!window.__TAURI__ || !window.__TAURI__.event) {
                setTimeout(waitForTauri, 100);
                return;
            }

            var listen = window.__TAURI__.event.listen;

            listen('wiki-favicon-updated', function(event) {
                var payload = event.payload;
                if (!payload || !payload.path) return;

                // Dispatch a custom event for TiddlyWiki plugins to handle
                var customEvent = new CustomEvent('td-favicon-updated', {
                    detail: {
                        path: payload.path,
                        favicon: payload.favicon
                    }
                });
                window.dispatchEvent(customEvent);

                console.log('[TiddlyDesktop] Favicon updated for:', payload.path);
            });
        }

        waitForTauri();
    }

    setupFaviconUpdateListener();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
