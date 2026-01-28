// TiddlyDesktop Initialization Script - Core Module
// Provides: initialization guard, modal UI, confirm override

(function(TD) {
    'use strict';

    // Prevent double execution
    if (window.__TD_INIT_SCRIPT_LOADED__) {
        console.log('[TiddlyDesktop] Initialization script already loaded - skipping duplicate');
        return false;
    }
    window.__TD_INIT_SCRIPT_LOADED__ = true;
    console.log('[TiddlyDesktop] Initialization script loaded');

    // Shared state
    var promptWrapper = null;
    var confirmationBypassed = false;

    // Get a color from TiddlyWiki's palette using the <<colour>> macro
    function getColour(name, fallback) {
        if (typeof $tw !== 'undefined' && $tw.wiki && $tw.wiki.renderText) {
            try {
                var result = $tw.wiki.renderText("text/plain", "text/vnd.tiddlywiki", "<<colour " + name + ">>").trim();
                if (result && result !== "<<colour " + name + ">>") {
                    return result;
                }
            } catch (e) {
                // Fall through to fallback
            }
        }
        return fallback;
    }

    function ensureWrapper() {
        if (!promptWrapper && document.body) {
            promptWrapper = document.createElement('div');
            promptWrapper.className = 'td-confirm-wrapper';
            promptWrapper.style.cssText = 'display:none;position:fixed;top:0;left:0;right:0;bottom:0;background:rgba(0,0,0,0.5);z-index:10000;align-items:center;justify-content:center;';
            document.body.appendChild(promptWrapper);
        }
        return promptWrapper;
    }

    function showConfirmModal(message, callback) {
        var wrapper = ensureWrapper();
        if (!wrapper) {
            if (callback) callback(true);
            return;
        }

        // Get colors from palette using <<colour>> macro
        var modalBackground = getColour('modal-background', getColour('tiddler-background', '#ffffff'));
        var modalBorder = getColour('modal-border', getColour('tiddler-border', '#cccccc'));
        var foreground = getColour('foreground', '#333333');
        var primary = getColour('primary', '#5778d8');
        var mutedForeground = getColour('muted-foreground', '#999999');
        var buttonBackground = getColour('button-background', '#f0f0f0');
        var buttonForeground = getColour('button-foreground', foreground);
        var buttonBorder = getColour('button-border', '#cccccc');

        var modal = document.createElement('div');
        modal.style.cssText = 'background:' + modalBackground + ';color:' + foreground + ';padding:20px;border-radius:8px;border:1px solid ' + modalBorder + ';box-shadow:0 4px 20px rgba(0,0,0,0.3);max-width:400px;text-align:center;';

        var msgP = document.createElement('p');
        msgP.textContent = message;
        msgP.style.cssText = 'margin:0 0 20px 0;font-size:16px;color:' + foreground + ';';

        var btnContainer = document.createElement('div');
        btnContainer.style.cssText = 'display:flex;gap:10px;justify-content:center;';

        var cancelBtn = document.createElement('button');
        cancelBtn.textContent = 'Cancel';
        cancelBtn.style.cssText = 'padding:8px 20px;background:' + buttonBackground + ';color:' + buttonForeground + ';border:1px solid ' + buttonBorder + ';border-radius:4px;cursor:pointer;font-size:14px;';
        cancelBtn.onclick = function() {
            wrapper.style.display = 'none';
            wrapper.innerHTML = '';
            if (callback) callback(false);
        };

        var okBtn = document.createElement('button');
        okBtn.textContent = 'OK';
        okBtn.style.cssText = 'padding:8px 20px;background:' + primary + ';color:' + modalBackground + ';border:1px solid ' + primary + ';border-radius:4px;cursor:pointer;font-size:14px;';
        okBtn.onclick = function() {
            wrapper.style.display = 'none';
            wrapper.innerHTML = '';
            if (callback) callback(true);
        };

        btnContainer.appendChild(cancelBtn);
        btnContainer.appendChild(okBtn);
        modal.appendChild(msgP);
        modal.appendChild(btnContainer);
        wrapper.innerHTML = '';
        wrapper.appendChild(modal);
        wrapper.style.display = 'flex';
        okBtn.focus();
    }

    // Custom confirm function
    var customConfirm = function(message) {
        if (confirmationBypassed) {
            return true;
        }

        var currentEvent = window.event;

        showConfirmModal(message, function(confirmed) {
            if (confirmed && currentEvent && currentEvent.target) {
                confirmationBypassed = true;
                try {
                    var target = currentEvent.target;
                    if (typeof target.click === 'function') {
                        target.click();
                    } else {
                        var newEvent = new MouseEvent('click', {
                            bubbles: true,
                            cancelable: true,
                            view: window
                        });
                        target.dispatchEvent(newEvent);
                    }
                } finally {
                    confirmationBypassed = false;
                }
            }
        });

        return false;
    };

    // Install the override using Object.defineProperty
    function installConfirmOverride() {
        try {
            Object.defineProperty(window, 'confirm', {
                value: customConfirm,
                writable: false,
                configurable: true
            });
        } catch (e) {
            window.confirm = customConfirm;
        }
    }

    // Install immediately and reinstall after DOM events
    installConfirmOverride();
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', installConfirmOverride);
    }
    window.addEventListener('load', installConfirmOverride);

    // Export to TD namespace
    TD.showConfirmModal = showConfirmModal;

    return true; // Signal successful initialization
})(window.TiddlyDesktop = window.TiddlyDesktop || {});
