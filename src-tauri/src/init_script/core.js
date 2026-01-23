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

        var modal = document.createElement('div');
        modal.style.cssText = 'background:white;padding:20px;border-radius:8px;box-shadow:0 4px 20px rgba(0,0,0,0.3);max-width:400px;text-align:center;';

        var msgP = document.createElement('p');
        msgP.textContent = message;
        msgP.style.cssText = 'margin:0 0 20px 0;font-size:16px;';

        var btnContainer = document.createElement('div');
        btnContainer.style.cssText = 'display:flex;gap:10px;justify-content:center;';

        var cancelBtn = document.createElement('button');
        cancelBtn.textContent = 'Cancel';
        cancelBtn.style.cssText = 'padding:8px 20px;background:#e0e0e0;border:none;border-radius:4px;cursor:pointer;font-size:14px;';
        cancelBtn.onclick = function() {
            wrapper.style.display = 'none';
            wrapper.innerHTML = '';
            if (callback) callback(false);
        };

        var okBtn = document.createElement('button');
        okBtn.textContent = 'OK';
        okBtn.style.cssText = 'padding:8px 20px;background:#4a90d9;color:white;border:none;border-radius:4px;cursor:pointer;font-size:14px;';
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
