// Title sync - mirror document.title to native window titlebar
// Primary: $tw.wiki change listener watches $:/SiteTitle and $:/SiteSubtitle
// Fallback: MutationObserver on <title> element for non-TW title changes
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

    // Primary: TiddlyWiki change listener for $:/SiteTitle and $:/SiteSubtitle
    // These tiddlers feed into $:/core/wiki/title which TW renders to document.title
    function setupTiddlerChangeListener() {
        if (typeof $tw === 'undefined' || !$tw.wiki || !$tw.wiki.addEventListener) {
            setTimeout(setupTiddlerChangeListener, 100);
            return;
        }

        // Initial sync once TW is ready
        syncTitle();

        $tw.wiki.addEventListener('change', function(changes) {
            if (changes['$:/SiteTitle'] || changes['$:/SiteSubtitle']) {
                // Small delay to let TiddlyWiki re-render the title template
                setTimeout(syncTitle, 50);
            }
        });
    }

    // Fallback: MutationObserver on <title> element
    // Catches title changes from non-TW sources or during initial page load
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

    setupTiddlerChangeListener();
    setupTitleObserver();
})();
