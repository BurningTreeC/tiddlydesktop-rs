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

    // Calculate relative luminance of a color (for contrast calculation)
    function getLuminance(color) {
        // Parse hex color
        var hex = color.replace('#', '');
        if (hex.length === 3) {
            hex = hex[0] + hex[0] + hex[1] + hex[1] + hex[2] + hex[2];
        }
        var r = parseInt(hex.substr(0, 2), 16) / 255;
        var g = parseInt(hex.substr(2, 2), 16) / 255;
        var b = parseInt(hex.substr(4, 2), 16) / 255;
        // Apply gamma correction
        r = r <= 0.03928 ? r / 12.92 : Math.pow((r + 0.055) / 1.055, 2.4);
        g = g <= 0.03928 ? g / 12.92 : Math.pow((g + 0.055) / 1.055, 2.4);
        b = b <= 0.03928 ? b / 12.92 : Math.pow((b + 0.055) / 1.055, 2.4);
        return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    }

    // Get contrasting text color (black or white) for a given background
    function getContrastingColor(bgColor) {
        try {
            var luminance = getLuminance(bgColor);
            return luminance > 0.179 ? '#000000' : '#ffffff';
        } catch (e) {
            return '#333333'; // Safe fallback
        }
    }

    // Get a color from TiddlyWiki's current palette (with recursive resolution)
    function getColour(name, fallback, depth) {
        depth = depth || 0;
        if (depth > 10) return fallback; // Prevent infinite recursion

        if (typeof $tw !== 'undefined' && $tw.wiki) {
            try {
                // Get the current palette title
                var paletteName = $tw.wiki.getTiddlerText("$:/palette");
                if (paletteName) {
                    paletteName = paletteName.trim();
                    var paletteTiddler = $tw.wiki.getTiddler(paletteName);
                    if (paletteTiddler) {
                        // Colors are in the tiddler text (one per line: name: value)
                        var text = paletteTiddler.fields.text || "";
                        var lines = text.split("\n");
                        for (var i = 0; i < lines.length; i++) {
                            var line = lines[i].trim();
                            var colonIndex = line.indexOf(":");
                            if (colonIndex > 0) {
                                var colorName = line.substring(0, colonIndex).trim();
                                var colorValue = line.substring(colonIndex + 1).trim();
                                if (colorName === name && colorValue) {
                                    // Handle references to other colors like <<colour background>>
                                    var match = colorValue.match(/<<colour\s+([^>]+)>>/);
                                    if (match) {
                                        return getColour(match[1].trim(), fallback, depth + 1);
                                    }
                                    return colorValue;
                                }
                            }
                        }
                    }
                }
            } catch (e) {
                console.error('[TiddlyDesktop] getColour error:', e);
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
        // Always calculate contrasting color to ensure readability
        var buttonForeground = getContrastingColor(buttonBackground);
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
        var okBtnTextColor = getContrastingColor(primary);
        okBtn.style.cssText = 'padding:8px 20px;background:' + primary + ';color:' + okBtnTextColor + ';border:1px solid ' + primary + ';border-radius:4px;cursor:pointer;font-size:14px;';
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

    // Update Linux HeaderBar colors from TiddlyWiki's current palette
    function updateHeaderBarColors() {
        if (typeof $tw === 'undefined' || !window.__TAURI__ || !window.__WINDOW_LABEL__) {
            console.log('[TiddlyDesktop] updateHeaderBarColors: prerequisites not ready');
            return;
        }

        var bg = getColour('page-background', '#ffffff');
        var fg = getColour('foreground', '#333333');

        console.log('[TiddlyDesktop] updateHeaderBarColors: bg=' + bg + ', fg=' + fg);

        window.__TAURI__.core.invoke('set_headerbar_colors', {
            label: window.__WINDOW_LABEL__,
            background: bg,
            foreground: fg
        }).catch(function(err) {
            console.error('[TiddlyDesktop] Failed to set headerbar colors:', err);
        });
    }

    // Update find bar colors when palette changes
    function updateFindBarColors() {
        var bar = document.getElementById('td-find-bar');
        if (!bar) return;

        var pageBackground = getColour('page-background', '#f0f0f0');
        var background = getColour('background', '#ffffff');
        var foreground = getColour('foreground', '#333333');
        var tabBorder = getColour('tab-border', '#cccccc');
        var mutedForeground = getColour('muted-foreground', '#666666');

        bar.style.background = pageBackground;
        bar.style.borderBottomColor = tabBorder;

        var input = bar.querySelector('input');
        if (input) {
            input.style.background = background;
            input.style.color = foreground;
            input.style.borderColor = tabBorder;
        }

        var info = bar.querySelector('span');
        if (info) {
            info.style.color = mutedForeground;
        }

        var buttons = bar.querySelectorAll('button');
        buttons.forEach(function(btn) {
            if (btn.textContent === '✕') {
                btn.style.color = mutedForeground;
            } else {
                btn.style.background = background;
                btn.style.color = foreground;
                btn.style.borderColor = tabBorder;
            }
        });
    }

    // Initialize headerbar colors and palette change listener for ALL windows (including user wikis)
    function initPaletteSync() {
        if (typeof $tw !== 'undefined' && $tw.wiki) {
            // Update headerbar colors immediately
            updateHeaderBarColors();

            // Listen for palette changes
            $tw.wiki.addEventListener('change', function(changes) {
                if (changes['$:/palette']) {
                    // Small delay to let TiddlyWiki process the palette change
                    setTimeout(function() {
                        updateHeaderBarColors();
                        updateFindBarColors();
                    }, 50);
                }
            });
        } else {
            // TiddlyWiki not ready yet, retry
            setTimeout(initPaletteSync, 100);
        }
    }

    // Start palette sync after DOM is ready
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initPaletteSync);
    } else {
        initPaletteSync();
    }

    // Ctrl/Cmd+0 to reset zoom to 100%
    // (Ctrl/Cmd+Plus/Minus and Ctrl+mousewheel are handled by Tauri's built-in zoom_hotkeys_enabled)
    document.addEventListener('keydown', function(e) {
        if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey && (e.key === '0' || e.code === 'Digit0')) {
            e.preventDefault();
            e.stopPropagation();
            if (window.__TAURI__ && window.__TAURI__.core) {
                window.__TAURI__.core.invoke('set_zoom_level', { level: 1.0 }).catch(function() {});
            }
        }
    }, true);

    // Export to TD namespace
    TD.showConfirmModal = showConfirmModal;
    TD.getColour = getColour;
    TD.updateHeaderBarColors = updateHeaderBarColors;
    TD.updateFindBarColors = updateFindBarColors;

    return true; // Signal successful initialization
})(window.TiddlyDesktop = window.TiddlyDesktop || {});
