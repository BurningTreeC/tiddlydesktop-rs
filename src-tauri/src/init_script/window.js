// TiddlyDesktop Initialization Script - Window Module
// Handles: window close with unsaved changes check

(function(TD) {
    'use strict';

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

            if (isDirty) {
                TD.showConfirmModal('You have unsaved changes. Are you sure you want to close?', function(confirmed) {
                    if (confirmed) {
                        invoke('close_window');
                    }
                });
            } else {
                invoke('close_window');
            }
        });
    }

    setupCloseHandler();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
