// Title sync - mirror document.title to native window titlebar
// Uses MutationObserver on <title> element
// TiddlyWiki5's render.js updates document.title from $:/core/wiki/title template
(function() {
    // Only run for wiki windows, not the landing page
    if (!window.__WINDOW_LABEL__) return;

    var windowLabel = window.__WINDOW_LABEL__;
    var lastTitle = '';

    function syncTitle() {
        var title = document.title || '';

        // Skip if title hasn't changed or is empty/generic
        if (!title || title === lastTitle || title === 'Loading...') {
            return;
        }

        lastTitle = title;

        // Update native window titlebar via Tauri
        if (window.__TAURI__ && window.__TAURI__.core) {
            window.__TAURI__.core.invoke('set_window_title', {
                label: windowLabel,
                title: title
            }).catch(function(e) {
                console.error('TiddlyDesktop: Failed to set window title:', e);
            });
        }
    }

    // Set up MutationObserver on <title> element
    function setupTitleObserver() {
        var titleElement = document.querySelector('title');
        if (!titleElement) {
            // Title element not in DOM yet, retry
            setTimeout(setupTitleObserver, 100);
            return;
        }

        // Initial sync
        syncTitle();

        // Observe changes to the title element
        var observer = new MutationObserver(function() {
            syncTitle();
        });

        observer.observe(titleElement, {
            childList: true,      // Text node added/removed
            characterData: true,  // Text content changes
            subtree: true         // Descendants (the text node)
        });
    }

    setupTitleObserver();
})();
