// Favicon sync - extract from $:/favicon.ico and update landing page + window icon
// Also watches for changes so favicon updates are reflected instantly
(function() {
    // Only run for wiki windows, not the landing page
    if (!window.__WIKI_PATH__) return;

    var wikiPath = window.__WIKI_PATH__;
    var lastFavicon = '';

    function sendFaviconUpdate(dataUri) {
        // Skip if favicon hasn't changed
        if (dataUri === lastFavicon) {
            return;
        }
        lastFavicon = dataUri;

        // Send to Rust to update the wiki list entry and window icon
        if (window.__TAURI__ && window.__TAURI__.core) {
            // Update window icon (titlebar/taskbar)
            window.__TAURI__.core.invoke('set_window_icon', {
                label: window.__WINDOW_LABEL__,
                faviconDataUri: dataUri
            }).catch(function(err) {
                console.error('TiddlyDesktop: Failed to set window icon:', err);
            });

            // Update wiki list entry favicon
            // In main wiki mode (main process), use update_wiki_favicon directly
            // In wiki mode (child process), use IPC to send to main process
            if (window.__IS_MAIN_WIKI__) {
                // Main process - direct command
                window.__TAURI__.core.invoke('update_wiki_favicon', {
                    path: wikiPath,
                    favicon: dataUri
                }).catch(function(err) {
                    console.error('TiddlyDesktop: Failed to update favicon:', err);
                });
            } else {
                // Wiki child process - use IPC
                window.__TAURI__.core.invoke('ipc_update_favicon', {
                    favicon: dataUri
                }).catch(function(err) {
                    console.error('TiddlyDesktop: Failed to update favicon via IPC:', err);
                });
            }
        }
    }

    function extractAndUpdateFavicon() {
        if (typeof $tw === 'undefined' || !$tw.wiki) {
            return; // TiddlyWiki not ready
        }

        // Get the favicon tiddler
        var faviconTiddler = $tw.wiki.getTiddler('$:/favicon.ico');
        if (!faviconTiddler || !faviconTiddler.fields.text) {
            return; // No favicon tiddler
        }

        var text = faviconTiddler.fields.text;
        var type = faviconTiddler.fields.type || 'image/x-icon';

        // Build data URI
        var dataUri;
        if (text.startsWith('data:')) {
            dataUri = text; // Already a data URI
        } else {
            // Assume base64 encoded
            dataUri = 'data:' + type + ';base64,' + text;
        }

        sendFaviconUpdate(dataUri);
    }

    function setupFaviconSync() {
        if (typeof $tw === 'undefined' || !$tw.wiki || !$tw.wiki.addEventListener) {
            setTimeout(setupFaviconSync, 100);
            return;
        }

        // Initial extraction
        extractAndUpdateFavicon();

        // Watch for changes to $:/favicon.ico
        $tw.wiki.addEventListener('change', function(changes) {
            if (changes['$:/favicon.ico']) {
                extractAndUpdateFavicon();
            }
        });
    }

    setupFaviconSync();
})();
