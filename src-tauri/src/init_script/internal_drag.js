// TiddlyDesktop - Native GTK Drag Handler
// Strategy: Let WebKit handle drags naturally, we just:
// 1. Capture data during real dragstart and send to Rust for inter-wiki drops
// 2. Receive GTK events (td-drag-*) for external drags and dispatch synthetic DOM events

(function(TD) {
    'use strict';

    if (window.__WINDOW_LABEL__ === 'main') return;

    // === Logging ===
    function log(message) {
        if (window.__TAURI__?.core?.invoke) {
            window.__TAURI__.core.invoke("js_log", { message: "[drag] " + message });
        }
    }

    // === Tauri event listener (window-specific) ===
    var tauriListen = null;
    function setupTauriListen() {
        if (tauriListen) return;
        if (window.__TAURI__?.window) {
            var win = window.__TAURI__.window.getCurrentWindow();
            tauriListen = win.listen.bind(win);
            log('Tauri listen ready for: ' + window.__WINDOW_LABEL__);
        }
    }

    // === State ===
    var internalDragActive = false;  // True when we started a drag (we are the source)
    var externalDragActive = false;  // True when receiving an external drag
    var crossWikiDragData = null;    // Tiddler JSON pre-emitted from Rust for cross-wiki drops
    var isTextSelectionDrag = false; // True when dragging text selection (not a draggable element)
    var dragData = null;             // Current drag data (for internal drags)
    var dragSource = null;           // Element that started the drag
    var lastTarget = null;           // Last element we dispatched dragover to
    var iframesDisabled = false;     // True when iframes have pointer-events: none
    var lastDragPosition = null;     // Last known drag position (for drop targeting)
    var pendingDragElement = null;   // Element that might be dragged (set on pointerdown)
    var lastPointerType = 'mouse';   // Pointer type from last pointerdown (for synthetic pointerup)
    var currentDragEventTarget = null; // Track current drag event target for Issue 4b filtering

    // === Patch DataTransfer.prototype for Windows/macOS ===
    // On these platforms, the webview strips custom MIME types like text/vnd.tiddler
    // during same-window drags. We patch setData, getData, and types to capture and
    // return our data. This allows native drops to happen while ensuring correct data.
    //
    // IMPORTANT: On Chromium (WebView2), getData() returns empty during dragstart
    // due to security restrictions - data is only readable during drop. So we MUST
    // patch setData() to capture data as TiddlyWiki sets it.
    (function() {
        // Temporary storage for setData calls during dragstart
        // This captures data BEFORE our dragstart handler runs
        var pendingDragData = {};
        var capturingSetData = false;

        // Capture original types getter early (needed by getData filter)
        var typesDescriptor = Object.getOwnPropertyDescriptor(DataTransfer.prototype, 'types');
        var originalTypesGetter = typesDescriptor && typesDescriptor.get ? typesDescriptor.get : null;

        // Patch setData to capture data as it's being set
        var originalSetData = DataTransfer.prototype.setData;
        DataTransfer.prototype.setData = function(type, data) {
            // Always capture setData calls - we'll use them if this becomes an internal drag
            pendingDragData[type] = data;
            return originalSetData.call(this, type, data);
        };

        // Patch getData to return captured data during internal drags
        var originalGetData = DataTransfer.prototype.getData;
        DataTransfer.prototype.getData = function(type) {
            // For cross-wiki drops, return pre-emitted tiddler data
            // This allows native WebKit drop events to work with cross-wiki tiddler data
            if (crossWikiDragData) {
                if (type === 'text/vnd.tiddler') {
                    return crossWikiDragData;
                }
                // Also provide title as text/plain for fallback
                if (type === 'text/plain') {
                    try {
                        var parsed = JSON.parse(crossWikiDragData);
                        return parsed.title || '';
                    } catch (e) {}
                }
            }

            // If internal drag is active, check our captured data
            if (internalDragActive) {
                // Issue 4b: For same-wiki tiddler drags (not text-selection),
                // filter text/vnd.tiddler when NOT over a $droppable element.
                // This prevents the main dropzone from importing tiddlers that already exist.
                if (type === 'text/vnd.tiddler' && !isTextSelectionDrag) {
                    var droppable = null;
                    try {
                        if (currentDragEventTarget && currentDragEventTarget.closest) {
                            droppable = currentDragEventTarget.closest('.tc-droppable');
                        }
                    } catch (e) {}
                    if (!droppable) {
                        return ''; // Don't return tiddler data when not over $droppable
                    }
                }

                // Check dragData first (data we've already processed)
                if (dragData && type in dragData) {
                    return dragData[type];
                }
                // Fall back to pendingDragData - this contains setData() calls that happened
                // AFTER our dragstart handler ran (since we use capture phase, TiddlyWiki's
                // handler runs after ours and its setData calls go here)
                if (type in pendingDragData) {
                    return pendingDragData[type];
                }
                // For internal drags, filter out browser-resolved wikifile:// URLs
                // When dragging links (<a href="#TiddlerTitle">), the browser resolves the
                // relative href to the full page URL: wikifile://localhost/...#TiddlerTitle
                // TiddlyWiki doesn't set text/uri-list, so we'd fall through to the browser's
                // value which causes the wikifile:// URL to be used instead of the tiddler data
                if (type === 'text/uri-list' || type === 'text/x-moz-url' || type === 'URL') {
                    var nativeResult = originalGetData.call(this, type);
                    if (nativeResult && nativeResult.indexOf('wikifile://') !== -1) {
                        return '';  // Filter out browser-resolved wikifile:// URLs
                    }
                    return nativeResult;
                }
            }

            var result = originalGetData.call(this, type);

            // For external drops, filter out text/html to avoid styled HTML from browsers
            // TiddlyWiki will fall back to text/plain which is cleaner
            if (!internalDragActive && type === 'text/html') {
                return '';
            }

            // Filter out file URLs when files are present (prevents double import on Linux/Windows)
            // This happens during native drops where the browser provides both the file AND its URL/path
            if (!internalDragActive && (type === 'text/plain' || type === 'text/uri-list' || type === 'text/x-moz-url')) {
                // Check if result looks like a file URL or absolute path
                if (result) {
                    var trimmed = result.trim();
                    var isFileUrl = trimmed.indexOf('file://') === 0;
                    var isWindowsPath = /^[A-Za-z]:[\\\/]/.test(trimmed);
                    var isAbsPath = (trimmed.indexOf('/') === 0 || isWindowsPath) && trimmed.indexOf('\n') === -1;
                    if (isFileUrl || isAbsPath) {
                        // Check if this DataTransfer has files using types array (more reliable than files property)
                        try {
                            var types = originalTypesGetter ? originalTypesGetter.call(this) : this.types;
                            var typesArr = types ? Array.from(types) : [];
                            var hasFiles = typesArr.indexOf('Files') !== -1;
                            if (hasFiles) {
                                // Files present - filter out the URL to prevent double import
                                return '';
                            }
                        } catch (e) {}
                    }
                }
            }

            return result;
        };

        // Also patch types getter so iteration includes our captured types
        if (originalTypesGetter) {
            Object.defineProperty(DataTransfer.prototype, 'types', {
                get: function() {
                    var originalTypes = originalTypesGetter.call(this);

                    // For cross-wiki drops, ensure text/vnd.tiddler is in types
                    if (crossWikiDragData) {
                        var original = Array.from(originalTypes || []);
                        if (original.indexOf('text/vnd.tiddler') === -1) {
                            original.unshift('text/vnd.tiddler');
                        }
                        if (original.indexOf('text/plain') === -1) {
                            original.push('text/plain');
                        }
                        return original;
                    }

                    if (internalDragActive) {
                        // Merge types from dragData, pendingDragData, and original
                        var capturedTypes = dragData ? Object.keys(dragData) : [];
                        var pendingTypes = Object.keys(pendingDragData);
                        var original = Array.from(originalTypes || []);
                        // Merge: dragData first, then pendingDragData, then original
                        var merged = capturedTypes.slice();
                        for (var i = 0; i < pendingTypes.length; i++) {
                            if (merged.indexOf(pendingTypes[i]) === -1) {
                                merged.push(pendingTypes[i]);
                            }
                        }
                        for (var i = 0; i < original.length; i++) {
                            if (merged.indexOf(original[i]) === -1) {
                                merged.push(original[i]);
                            }
                        }

                        // Issue 4b: For same-wiki tiddler drags (not text-selection),
                        // filter text/vnd.tiddler from types when NOT over a $droppable element.
                        // This prevents the main dropzone from showing the import indicator
                        // for tiddlers that already exist in this wiki.
                        // $droppable elements (for reordering) still see text/vnd.tiddler.
                        if (!isTextSelectionDrag && merged.indexOf('text/vnd.tiddler') !== -1) {
                            var droppable = null;
                            try {
                                if (currentDragEventTarget && currentDragEventTarget.closest) {
                                    droppable = currentDragEventTarget.closest('.tc-droppable');
                                }
                            } catch (e) {}
                            if (!droppable) {
                                merged = merged.filter(function(t) { return t !== 'text/vnd.tiddler'; });
                            }
                        }

                        return merged;
                    }
                    return originalTypes;
                },
                configurable: true,
                enumerable: true
            });
        }

        // Track current drag event target for Issue 4b filtering (capture phase)
        document.addEventListener('dragenter', function(e) {
            if (internalDragActive) {
                currentDragEventTarget = e.target;
            }
        }, true);

        document.addEventListener('dragover', function(e) {
            if (internalDragActive) {
                currentDragEventTarget = e.target;
            }
        }, true);

        // Export functions to access pending data from dragstart handler
        TD._getPendingDragData = function() {
            var data = pendingDragData;
            pendingDragData = {};  // Clear for next drag
            return data;
        };

        TD._clearPendingDragData = function() {
            pendingDragData = {};
        };

        log('DataTransfer.prototype patched (setData + getData) for same-window drag support');

        // Helper to check if an element or its ancestors are editable
        function isEditableElement(el) {
            while (el && el !== document.body) {
                var tagName = el.tagName;
                if (tagName === 'INPUT') {
                    var type = (el.type || 'text').toLowerCase();
                    if (['text', 'search', 'url', 'tel', 'email', 'password'].indexOf(type) !== -1) {
                        return true;
                    }
                }
                if (tagName === 'TEXTAREA') return true;
                if (el.isContentEditable) return true;
                // Check if it's an iframe with editable content
                if (tagName === 'IFRAME') {
                    try {
                        var iframeDoc = el.contentDocument || el.contentWindow.document;
                        if (iframeDoc && (iframeDoc.designMode === 'on' ||
                            (iframeDoc.body && iframeDoc.body.isContentEditable))) {
                            return true;
                        }
                    } catch (e) {}
                }
                el = el.parentElement;
            }
            return false;
        }

        // Intercept drop events to filter out file URL items that would cause double imports
        // This runs in capture phase BEFORE TiddlyWiki's handlers
        document.addEventListener('drop', function(event) {
            // Skip synthetic events we created
            if (event.__tdFiltered) return;
            if (event.__tiddlyDesktopSynthetic) return;

            var dt = event.dataTransfer;
            if (!dt || !dt.items) return;

            // Helper to check if text looks like a file path
            function looksLikeFilePath(text) {
                if (!text) return false;
                text = text.trim();
                // Unix absolute path (but not URLs like //server)
                if (text.indexOf('/') === 0 && text.indexOf('//') !== 0) return true;
                // Windows path (C:\... or C:/...)
                if (/^[A-Za-z]:[\\\/]/.test(text)) return true;
                // file:// URL
                if (text.indexOf('file://') === 0) return true;
                return false;
            }

            // Case 1: Files present but text contains file path - filter to prevent double import
            // On Windows, WebView2's native drop event includes both files AND file paths as text
            if (dt.files.length > 0) {
                var plainText = '';
                var uriList = '';
                try {
                    plainText = dt.getData('text/plain') || '';
                    uriList = dt.getData('text/uri-list') || '';
                } catch (e) {}

                // Check if the text types are just file paths
                var textIsFilePath = looksLikeFilePath(plainText) || looksLikeFilePath(uriList);

                if (textIsFilePath) {
                    // Create a clean DataTransfer with only the files, no text types
                    // This prevents TiddlyWiki from importing both file AND path-as-text
                    event.preventDefault();
                    event.stopImmediatePropagation();

                    var newDt = new DataTransfer();
                    for (var i = 0; i < dt.files.length; i++) {
                        newDt.items.add(dt.files[i]);
                    }

                    var cleanEvent = new DragEvent('drop', {
                        bubbles: true,
                        cancelable: true,
                        dataTransfer: newDt,
                        clientX: event.clientX,
                        clientY: event.clientY,
                        screenX: event.screenX,
                        screenY: event.screenY
                    });
                    cleanEvent.__tdFiltered = true;
                    event.target.dispatchEvent(cleanEvent);
                    return;
                }
            }

            // Case 2: No files but has file:// URLs - block entirely
            // This is likely an external file drop where WebKitGTK provides file URL but not data
            // The actual file will come via tauri://drag-drop
            if (dt.files.length === 0) {
                var hasFileUrl = false;
                try {
                    var uriList = dt.getData('text/uri-list') || '';
                    var plainText = dt.getData('text/plain') || '';
                    var urls = (uriList + '\n' + plainText).split('\n');
                    for (var i = 0; i < urls.length; i++) {
                        if (looksLikeFilePath(urls[i])) {
                            hasFileUrl = true;
                            break;
                        }
                    }
                } catch (e) {}
                if (hasFileUrl) {
                    event.preventDefault();
                    event.stopImmediatePropagation();
                    return;
                }
            }
        }, true); // capture phase - runs before TiddlyWiki
    })();

    // === Find the actual draggable element ===
    function findDraggable(element) {
        var el = element;
        // Handle text nodes (nodeType 3) - browsers can fire drag events on them
        if (el && el.nodeType === 3) {
            el = el.parentElement;
        }
        while (el && el !== document.body) {
            if (el.draggable || (el.getAttribute && el.getAttribute('draggable') === 'true')) {
                return el;
            }
            el = el.parentElement;
        }
        return null;
    }

    // === Recursively inline all computed styles on an element and its children ===
    function inlineAllStyles(source, target) {
        // Safeguard: ensure source is a valid element
        if (!source || !source.nodeType || source.nodeType !== 1) return;
        if (!target || !target.style) return;

        var computed;
        try {
            computed = window.getComputedStyle(source);
        } catch (e) {
            return;
        }
        if (!computed) return;

        var cssText = '';
        for (var i = 0; i < computed.length; i++) {
            var prop = computed[i];
            cssText += prop + ':' + computed.getPropertyValue(prop) + ';';
        }
        target.style.cssText = cssText;

        // Recurse into children
        var sourceChildren = source.children;
        var targetChildren = target.children;
        for (var j = 0; j < sourceChildren.length && j < targetChildren.length; j++) {
            inlineAllStyles(sourceChildren[j], targetChildren[j]);
        }
    }

    // === Generate drag image PNG by cloning element with inlined styles ===
    function generateDragImagePng(element, callback) {
        if (!element) return callback(null);

        // Safeguard: ensure element has getBoundingClientRect method
        if (typeof element.getBoundingClientRect !== 'function') {
            log('Cannot generate drag image: element has no getBoundingClientRect');
            return callback(null);
        }

        var rect;
        try {
            rect = element.getBoundingClientRect();
        } catch (e) {
            log('Cannot generate drag image: getBoundingClientRect failed');
            return callback(null);
        }

        var width = Math.ceil(rect.width);
        var height = Math.ceil(rect.height);

        if (width <= 0 || height <= 0) {
            log('Cannot generate drag image: zero dimensions');
            return callback(null);
        }

        // Deep clone the element
        var clone = element.cloneNode(true);

        // Inline all computed styles on clone and all descendants
        inlineAllStyles(element, clone);

        // Reset positioning
        clone.style.margin = '0';
        clone.style.position = 'static';
        clone.style.left = 'auto';
        clone.style.top = 'auto';

        // Build the SVG with foreignObject
        var svgNS = 'http://www.w3.org/2000/svg';
        var xhtmlNS = 'http://www.w3.org/1999/xhtml';

        // Serialize the clone to XHTML string
        var serializer = new XMLSerializer();
        var cloneHtml = serializer.serializeToString(clone);

        // Ensure proper XHTML namespace
        if (cloneHtml.indexOf('xmlns') === -1) {
            cloneHtml = cloneHtml.replace(/^<(\w+)/, '<$1 xmlns="' + xhtmlNS + '"');
        }

        // Build SVG string manually to avoid namespace issues
        var svgString = '<svg xmlns="' + svgNS + '" width="' + width + '" height="' + height + '">' +
            '<foreignObject width="100%" height="100%">' +
            '<div xmlns="' + xhtmlNS + '" style="width:' + width + 'px;height:' + height + 'px;overflow:hidden;">' +
            cloneHtml +
            '</div>' +
            '</foreignObject>' +
            '</svg>';

        // Use base64 data URL instead of blob URL (more permissive)
        var svgBase64 = btoa(unescape(encodeURIComponent(svgString)));
        var dataUrl = 'data:image/svg+xml;base64,' + svgBase64;

        // Draw to canvas
        var img = new Image();
        img.onload = function() {
            var canvas = document.createElement('canvas');
            canvas.width = width;
            canvas.height = height;
            var ctx = canvas.getContext('2d');
            ctx.drawImage(img, 0, 0);

            // Try to extract PNG
            try {
                var pngDataUrl = canvas.toDataURL('image/png');
                var base64 = pngDataUrl.split(',')[1];
                var binary = atob(base64);
                var pngData = new Array(binary.length);
                for (var i = 0; i < binary.length; i++) {
                    pngData[i] = binary.charCodeAt(i);
                }

                callback({
                    pngData: pngData,
                    width: width,
                    height: height,
                    offsetX: Math.floor(width / 2),
                    offsetY: Math.floor(height / 2)
                });
            } catch (e) {
                log('toDataURL failed (security): ' + e.message);
                // Fallback: generate simple canvas representation
                generateSimpleDragImage(element, callback);
            }
        };
        img.onerror = function() {
            log('Failed to load SVG image');
            generateSimpleDragImage(element, callback);
        };
        img.src = dataUrl;
    }

    // === Fallback: simple canvas-based drag image ===
    function generateSimpleDragImage(element, callback) {
        // Safeguard: ensure element has getBoundingClientRect method
        if (!element || typeof element.getBoundingClientRect !== 'function') {
            return callback(null);
        }

        var rect;
        try {
            rect = element.getBoundingClientRect();
        } catch (e) {
            return callback(null);
        }

        var width = Math.ceil(rect.width);
        var height = Math.ceil(rect.height);

        // Safeguard: ensure we can get computed style
        var computed;
        try {
            computed = window.getComputedStyle(element);
        } catch (e) {
            computed = {};
        }

        var bgColor = computed.backgroundColor;
        if (bgColor === 'rgba(0, 0, 0, 0)' || bgColor === 'transparent') {
            bgColor = '#f0f0f0';
        }
        var textColor = computed.color || '#333';
        var text = (element.textContent || '').trim();
        var fontSize = computed.fontSize || '14px';
        var fontFamily = computed.fontFamily || 'sans-serif';
        var fontWeight = computed.fontWeight || 'normal';

        var canvas = document.createElement('canvas');
        canvas.width = width;
        canvas.height = height;
        var ctx = canvas.getContext('2d');

        // Background
        ctx.fillStyle = bgColor;
        ctx.fillRect(0, 0, width, height);

        // Border
        ctx.strokeStyle = 'rgba(0,0,0,0.2)';
        ctx.lineWidth = 1;
        ctx.strokeRect(0, 0, width, height);

        // Text
        if (text) {
            ctx.fillStyle = textColor;
            ctx.font = fontWeight + ' ' + fontSize + ' ' + fontFamily;
            ctx.textBaseline = 'middle';
            ctx.textAlign = 'center';
            var maxWidth = width - 8;
            if (ctx.measureText(text).width > maxWidth) {
                while (text.length > 1 && ctx.measureText(text + '…').width > maxWidth) {
                    text = text.slice(0, -1);
                }
                text += '…';
            }
            ctx.fillText(text, width / 2, height / 2);
        }

        var pngDataUrl = canvas.toDataURL('image/png');
        var base64 = pngDataUrl.split(',')[1];
        var binary = atob(base64);
        var pngData = new Array(binary.length);
        for (var i = 0; i < binary.length; i++) {
            pngData[i] = binary.charCodeAt(i);
        }

        callback({
            pngData: pngData,
            width: width,
            height: height,
            offsetX: Math.floor(width / 2),
            offsetY: Math.floor(height / 2)
        });
    }

    // === Generate and send drag image early ===
    function prepareDragImageEarly(target, pointerType) {
        // Skip if already in a drag
        if (internalDragActive || externalDragActive) return;

        // Find draggable element at this position
        var draggable = findDraggable(target);
        if (!draggable) return;

        // Skip if we already prepared for this same element
        if (pendingDragElement === draggable) {
            return;
        }

        pendingDragElement = draggable;
        log('pointerdown(' + pointerType + ') on draggable: ' + draggable.tagName);

        // Generate PNG and send to Rust immediately so it's ready when drag-begin fires
        generateDragImagePng(draggable, function(result) {
            if (result && window.__TAURI__?.core?.invoke) {
                window.__TAURI__.core.invoke('set_pending_drag_icon', {
                    imageData: result.pngData,
                    offsetX: result.offsetX,
                    offsetY: result.offsetY
                }).then(function() {
                    log('Drag image PNG sent: ' + result.width + 'x' + result.height);
                }).catch(function(e) {
                    log('set_pending_drag_icon failed: ' + e);
                });
            }
        });
    }

    // === Pointer down handler - handles mouse, touch, and pen/digitizer ===
    document.addEventListener('pointerdown', function(event) {
        // Only handle primary button/touch point (button 0)
        // This works for: mouse left-click, pen tip, touch, and any digitizer
        if (event.button !== 0) return;
        lastPointerType = event.pointerType || 'mouse';
        prepareDragImageEarly(event.target, lastPointerType);
    }, true);

    // === Pointer up handler - clear pending state ===
    document.addEventListener('pointerup', function(event) {
        if (pendingDragElement && !internalDragActive) {
            pendingDragElement = null;
        }
    }, true);

    // === Dispatch synthetic pointerup to reset pointer state after drag operations ===
    function dispatchSyntheticPointerUp() {
        // Schedule after the current event processing completes
        // This ensures the browser has finished its native drag end handling
        setTimeout(function() {
            var syntheticEvent = new PointerEvent('pointerup', {
                bubbles: true,
                cancelable: true,
                pointerType: lastPointerType,
                button: 0,
                buttons: 0,
                isPrimary: true
            });
            document.dispatchEvent(syntheticEvent);
            log('Dispatched synthetic pointerup (' + lastPointerType + ') to reset pointer state');
        }, 0);
    }

    // === Iframe pointer-events handling ===
    function disableIframePointerEvents() {
        if (iframesDisabled) return;
        var iframes = document.querySelectorAll('iframe');
        iframes.forEach(function(iframe) {
            iframe.dataset.tdPrevPointerEvents = iframe.style.pointerEvents || '';
            iframe.style.pointerEvents = 'none';
        });
        iframesDisabled = true;
    }

    function enableIframePointerEvents() {
        if (!iframesDisabled) return;
        var iframes = document.querySelectorAll('iframe');
        iframes.forEach(function(iframe) {
            if (iframe.dataset.tdPrevPointerEvents !== undefined) {
                iframe.style.pointerEvents = iframe.dataset.tdPrevPointerEvents;
                delete iframe.dataset.tdPrevPointerEvents;
            }
        });
        iframesDisabled = false;
    }

    // === Create synthetic drag event ===
    function createDragEvent(type, x, y, dataTransfer, relatedTarget) {
        // Sanitize coordinates - TiddlyWiki's handlers use these with elementFromPoint
        // which throws on non-finite values
        var safeX = Number.isFinite(x) ? x : 0;
        var safeY = Number.isFinite(y) ? y : 0;
        var event = new DragEvent(type, {
            bubbles: true,
            cancelable: true,
            clientX: safeX,
            clientY: safeY,
            dataTransfer: dataTransfer,
            relatedTarget: relatedTarget || null
        });
        event.__tiddlyDesktopSynthetic = true;
        return event;
    }

    // === Get element at point ===
    function getElementAt(x, y) {
        // Validate coordinates are finite numbers (elementFromPoint throws on NaN/Infinity)
        if (!Number.isFinite(x) || !Number.isFinite(y)) {
            return document.body;
        }
        var el = document.elementFromPoint(x, y);
        return el || document.body;
    }

    // === Find editable element at position ===
    function findEditableAt(x, y) {
        // Validate coordinates are finite numbers (elementFromPoint throws on NaN/Infinity)
        if (!Number.isFinite(x) || !Number.isFinite(y)) {
            return null;
        }

        // First check iframes manually (they may have pointer-events: none during drag)
        var iframes = document.querySelectorAll('iframe');
        for (var i = 0; i < iframes.length; i++) {
            var iframe = iframes[i];
            // Safeguard: ensure iframe has getBoundingClientRect method
            if (!iframe || typeof iframe.getBoundingClientRect !== 'function') continue;

            var rect;
            try {
                rect = iframe.getBoundingClientRect();
            } catch (e) {
                continue;
            }
            if (x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom) {
                try {
                    var iframeDoc = iframe.contentDocument || iframe.contentWindow.document;
                    if (iframeDoc) {
                        if (iframeDoc.designMode === 'on' ||
                            (iframeDoc.body && iframeDoc.body.isContentEditable)) {
                            return iframe;
                        }
                        var iframeX = x - rect.left;
                        var iframeY = y - rect.top;
                        var innerEl = iframeDoc.elementFromPoint(iframeX, iframeY);
                        while (innerEl && innerEl !== iframeDoc.body) {
                            if (innerEl.tagName === 'TEXTAREA') return iframe;
                            if (innerEl.tagName === 'INPUT') {
                                var inputType = (innerEl.type || 'text').toLowerCase();
                                if (['text', 'search', 'url', 'tel', 'email', 'password'].indexOf(inputType) !== -1) {
                                    return iframe;
                                }
                            }
                            if (innerEl.isContentEditable) return iframe;
                            innerEl = innerEl.parentElement;
                        }
                    }
                } catch (e) {}
            }
        }

        var el = document.elementFromPoint(x, y);
        while (el && el !== document.body) {
            var tagName = el.tagName;
            if (tagName === 'INPUT') {
                var type = (el.type || 'text').toLowerCase();
                if (['text', 'search', 'url', 'tel', 'email', 'password'].indexOf(type) !== -1) {
                    return el;
                }
            }
            if (tagName === 'TEXTAREA') return el;
            if (el.isContentEditable) return el;
            el = el.parentElement;
        }
        return null;
    }

    // === Cleanup after drag ends ===
    function cleanup() {
        log('Cleanup');
        internalDragActive = false;
        externalDragActive = false;
        crossWikiDragData = null;
        isTextSelectionDrag = false;
        dragData = null;
        dragSource = null;
        lastTarget = null;
        lastDragPosition = null;
        pendingDragElement = null;
        currentDragEventTarget = null;
        enableIframePointerEvents();

        // Clear any pending setData captures
        if (TD._clearPendingDragData) {
            TD._clearPendingDragData();
        }

        if (typeof $tw !== 'undefined') {
            $tw.dragInProgress = null;
        }

        // Note: Don't manually remove tc-dragover - TiddlyWiki's dropzone widget
        // handles that through its own state management

        // Dispatch synthetic pointerup to reset pointer state
        // This ensures the next pointerdown fires correctly
        dispatchSyntheticPointerUp();
    }

    // === Handle REAL dragstart from WebKit ===
    // This fires when WebKit detects a drag has started (after motion threshold)
    document.addEventListener("dragstart", function(event) {
        if (event.__tiddlyDesktopSynthetic) return;

        log('Real dragstart from WebKit, target=' + event.target.tagName);

        // This is an internal drag - we are the source
        internalDragActive = true;

        // Detect if this is a text-selection drag vs a draggable element drag
        var draggableElement = findDraggable(event.target);
        isTextSelectionDrag = !draggableElement;
        dragSource = draggableElement || event.target;

        // Set TiddlyWiki drag state IMMEDIATELY to prevent dropzone from activating
        // This must happen before any async operations so the dropzone's dragenter
        // handler sees it when it runs
        // BUT: Only for draggable elements, not text selections - text selections
        // SHOULD activate the dropzone so users can drop text to create tiddlers
        if (typeof $tw !== 'undefined' && !isTextSelectionDrag) {
            $tw.dragInProgress = dragSource;
        }

        // Get data captured from setData() calls (works on all platforms)
        // This is more reliable than getData() which returns empty on Chromium during dragstart
        dragData = TD._getPendingDragData ? TD._getPendingDragData() : {};

        log('isTextSelectionDrag=' + isTextSelectionDrag);

        // Also try to read from DataTransfer as fallback (works on WebKitGTK/Linux)
        var dt = event.dataTransfer;
        if (dt) {
            try {
                var types = dt.types || [];
                for (var i = 0; i < types.length; i++) {
                    var type = types[i];
                    // Only read if we don't already have this type from setData capture
                    if (!(type in dragData)) {
                        try {
                            var value = dt.getData(type);
                            if (value) {
                                dragData[type] = value;
                            }
                        } catch (e) {}
                    }
                }
            } catch (e) {
                log('Failed to read dataTransfer: ' + e);
            }
        }

        log('Captured drag data types (immediate): ' + Object.keys(dragData).join(', '));

        // For text-selection drags, capture the selection if not in DataTransfer
        if (isTextSelectionDrag && !dragData['text/plain']) {
            var selection = window.getSelection();
            if (selection && selection.toString()) {
                dragData['text/plain'] = selection.toString();
                log('Captured selection text: ' + dragData['text/plain'].substring(0, 50));
            }
        }

        // Send data to Rust for inter-wiki drops
        // Use setTimeout(0) to defer until after TiddlyWiki's bubble-phase dragstart handler
        // has called setData() - our capture-phase handler runs first, before TiddlyWiki sets data
        if (window.__TAURI__?.core?.invoke) {
            setTimeout(function() {
                // Merge pendingDragData into dragData (TiddlyWiki's setData calls after our handler)
                var pending = TD._getPendingDragData ? TD._getPendingDragData() : {};
                for (var key in pending) {
                    if (pending.hasOwnProperty(key) && !dragData[key]) {
                        dragData[key] = pending[key];
                    }
                }
                log('Captured drag data types (after TW): ' + Object.keys(dragData).join(', '));

                var tiddlerJson = dragData['text/vnd.tiddler'] || null;

                // Don't include data:text/vnd.tiddler URLs - WebKitGTK prioritizes URLs
                // over text/plain and tries to navigate, breaking native input text insertion.
                // Cross-wiki detection uses text/vnd.tiddler type in target list instead.
                var data = {
                    text_plain: dragData['text/plain'] || null,
                    text_html: dragData['text/html'] || null,
                    text_vnd_tiddler: tiddlerJson,
                    text_uri_list: dragData['text/uri-list'] || null,
                    text_x_moz_url: dragData['text/x-moz-url'] || null,
                    url: dragData['URL'] || null,
                    is_text_selection_drag: isTextSelectionDrag
                };

                window.__TAURI__.core.invoke('prepare_native_drag', { data: data })
                    .then(function() { log('Native drag prepared'); })
                    .catch(function(err) { log('prepare_native_drag failed: ' + err); });
            }, 0);

            // Also generate PNG as backup (in case pointerdown didn't fire)
            // This won't arrive in time for the first idle callback, but Rust will
            // schedule additional callbacks to pick it up
            generateDragImagePng(dragSource, function(result) {
                if (result && window.__TAURI__?.core?.invoke) {
                    window.__TAURI__.core.invoke('set_pending_drag_icon', {
                        imageData: result.pngData,
                        offsetX: result.offsetX,
                        offsetY: result.offsetY
                    }).then(function() {
                        log('Backup drag image PNG sent from dragstart: ' + result.width + 'x' + result.height);
                    }).catch(function(e) {
                        log('Backup set_pending_drag_icon failed: ' + e);
                    });
                }
            });
        }

    }, true);

    // === Handle REAL dragend from WebKit ===
    document.addEventListener("dragend", function(event) {
        if (event.__tiddlyDesktopSynthetic) return;
        if (!internalDragActive) return;

        log('Real dragend from WebKit');

        // Clean up Rust state
        if (window.__TAURI__?.core?.invoke) {
            window.__TAURI__.core.invoke('cleanup_native_drag').catch(function() {});
        }

        cleanup();
    }, true);

    // === Handle native drag events ===
    // For internal drags: let browser handle everything naturally
    // For cross-wiki drags: query Rust for tiddler data, patched getData() returns it
    // For external drags (file manager): let native events flow
    var crossWikiQueryDone = false;

    // On dragenter, query Rust for cross-wiki data if this isn't our own drag
    document.addEventListener("dragenter", function(event) {
        if (event.__tiddlyDesktopSynthetic) return;
        if (internalDragActive) return;

        // Query Rust for cross-wiki data (only once per drag session)
        if (!crossWikiDragData && !crossWikiQueryDone && window.__TAURI__?.core?.invoke) {
            crossWikiQueryDone = true;
            window.__TAURI__.core.invoke('get_pending_drag_data', {
                targetWindow: window.__WINDOW_LABEL__
            }).then(function(data) {
                if (data && data.text_vnd_tiddler) {
                    log('Cross-wiki drag data received via native dragenter');
                    crossWikiDragData = data.text_vnd_tiddler;
                    externalDragActive = true;
                }
            }).catch(function(err) {
                log('get_pending_drag_data error: ' + err);
            });
        }
    }, true);

    // Reset cross-wiki query state when drag leaves
    document.addEventListener("dragleave", function(event) {
        if (event.__tiddlyDesktopSynthetic) return;
        if (internalDragActive) return;
        // Only reset if leaving the document entirely
        if (event.relatedTarget === null) {
            crossWikiQueryDone = false;
            crossWikiDragData = null;
            externalDragActive = false;
        }
    }, true);

    // Clean up after drop
    document.addEventListener("drop", function(event) {
        if (event.__tiddlyDesktopSynthetic) return;
        if (internalDragActive) return;
        // Reset state after drop (with small delay to let getData() work)
        setTimeout(function() {
            crossWikiQueryDone = false;
            // Don't clear crossWikiDragData here - cleanup() will do it
        }, 100);
    }, true);

    // Track position during internal drags for tauri://drag-drop handler
    document.addEventListener("dragover", function(event) {
        if (internalDragActive) {
            lastDragPosition = { x: event.clientX, y: event.clientY };
        }
    }, false);

    // Safety net: Prevent browser navigation on external drops
    // If something is dropped and nothing handles it, the browser might navigate away
    // This handler runs in bubble phase (after TiddlyWiki dropzone) and prevents that
    document.addEventListener("drop", function(event) {
        // Don't interfere with drops into editable elements - native handling needed
        if (isEditable(event.target)) {
            return;
        }
        // Prevent default for any unhandled drop with content
        if (event.dataTransfer && !event.defaultPrevented) {
            var hasContent = (event.dataTransfer.files && event.dataTransfer.files.length > 0) ||
                             (event.dataTransfer.types && event.dataTransfer.types.length > 0);
            if (hasContent) {
                log('Safety net: preventing browser navigation on unhandled drop');
                event.preventDefault();
            }
        }
    }, false);

    // Also prevent dragover default to allow drops (required for drop events to fire)
    // This handles both file drops and text selection drags from external apps
    // BUT: Don't prevent default for editable elements - let native text insertion work
    function isEditable(el) {
        if (!el) return false;
        var tagName = el.tagName;
        if (tagName === 'INPUT' || tagName === 'TEXTAREA' || el.isContentEditable) {
            return true;
        }
        // Check if it's an iframe - the actual target might be inside
        if (tagName === 'IFRAME') {
            try {
                var iframeDoc = el.contentDocument || el.contentWindow.document;
                if (iframeDoc) {
                    var activeEl = iframeDoc.activeElement;
                    if (activeEl) {
                        var activeTag = activeEl.tagName;
                        if (activeTag === 'INPUT' || activeTag === 'TEXTAREA' || activeEl.isContentEditable) {
                            return true;
                        }
                    }
                    // Also check if iframe body is editable
                    if (iframeDoc.designMode === 'on' || (iframeDoc.body && iframeDoc.body.isContentEditable)) {
                        return true;
                    }
                }
            } catch (e) {
                // Cross-origin iframe, can't check
            }
        }
        return false;
    }
    document.addEventListener("dragover", function(event) {
        if (event.dataTransfer && event.dataTransfer.types && event.dataTransfer.types.length > 0) {
            // For editable elements, let native handling work (enables text insertion)
            if (isEditable(event.target)) {
                return;
            }
            // For any external drag with content, prevent default to enable dropping
            if (!event.defaultPrevented) {
                event.preventDefault();
                // Set dropEffect to copy for visual feedback
                event.dataTransfer.dropEffect = 'copy';
            }
        }
    }, false);

    // Clear cross-wiki data after native drop completes
    document.addEventListener("drop", function(event) {
        if (crossWikiDragData) {
            log('Native drop with cross-wiki data');
            // Clear after a short delay to ensure getData() has been called
            setTimeout(function() {
                crossWikiDragData = null;
                externalDragActive = false;
                enableIframePointerEvents();
                log('Cross-wiki drag cleanup complete');
            }, 0);
        }
    }, false);

    // === GTK event handlers (for external drags coming INTO our window) ===
    function setupGtkEventHandlers() {
        setupTauriListen();
        if (!tauriListen) {
            setTimeout(setupGtkEventHandlers, 100);
            return;
        }

        log('Setting up GTK event handlers');

        // td-cross-wiki-data: Rust detected a cross-wiki drag and pre-emitted the tiddler data
        // Store it so the patched getData() can return it when native drop happens
        tauriListen("td-cross-wiki-data", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            var tiddlerJson = p.tiddlerJson || null;

            // Validate JSON before storing to catch corruption early
            if (tiddlerJson) {
                try {
                    var parsed = JSON.parse(tiddlerJson);
                    log('td-cross-wiki-data received, valid JSON, length=' + tiddlerJson.length + ', title=' + (parsed.title || '(no title)'));
                } catch (e) {
                    log('td-cross-wiki-data received INVALID JSON: ' + e + ', data preview=' + tiddlerJson.substring(0, 100));
                    return; // Don't store invalid JSON
                }
            } else {
                log('td-cross-wiki-data received, tiddlerJson is null/empty');
            }

            crossWikiDragData = tiddlerJson;

            // Reset lastTarget so the next td-drag-motion will dispatch a fresh dragenter
            // with the correct types (text/vnd.tiddler). This is needed because the initial
            // dragenter was dispatched before we had the cross-wiki data.
            lastTarget = null;
        });

        // td-drag-motion: Handle all external drags (cross-wiki and file manager)
        // WebKitGTK doesn't fire native DOM events for cross-process drags,
        // so we dispatch synthetic events here.
        tauriListen("td-drag-motion", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) {
                return;
            }

            // Skip same-window drags - WebKit handles them natively
            if (p.isSameWindow) {
                return;
            }

            var x = p.x || 0;
            var y = p.y || 0;

            if (!externalDragActive) {
                externalDragActive = true;
                disableIframePointerEvents();
                log('External drag entered window at (' + x + ', ' + y + ')');
            }

            // Create DataTransfer - populate with cross-wiki data if available
            var dataTransfer = new DataTransfer();
            if (crossWikiDragData) {
                try {
                    dataTransfer.setData('text/vnd.tiddler', crossWikiDragData);
                    var parsed = JSON.parse(crossWikiDragData);
                    if (parsed.title) {
                        dataTransfer.setData('text/plain', parsed.title);
                    }
                } catch (e) {}
            }

            var target = getElementAt(x, y);

            // Dispatch dragenter/dragleave if target changed
            if (target !== lastTarget) {
                var enterEvent = createDragEvent("dragenter", x, y, dataTransfer, lastTarget);
                target.dispatchEvent(enterEvent);
                if (lastTarget) {
                    var leaveEvent = createDragEvent("dragleave", x, y, dataTransfer, target);
                    lastTarget.dispatchEvent(leaveEvent);
                }
                lastTarget = target;
            }

            // Dispatch dragover
            var overEvent = createDragEvent("dragover", x, y, dataTransfer, null);
            target.dispatchEvent(overEvent);
        });

        // td-drag-leave: Dispatch synthetic dragleave for external drags
        tauriListen("td-drag-leave", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            // Skip same-window drags - WebKit handles them natively
            if (p.isSameWindow) return;

            if (!externalDragActive) return;

            log('td-drag-leave');

            // Dispatch synthetic dragleave
            if (lastTarget) {
                var dataTransfer = new DataTransfer();
                var leaveEvent = createDragEvent("dragleave", 0, 0, dataTransfer, null);
                lastTarget.dispatchEvent(leaveEvent);
                lastTarget = null;
            }

            externalDragActive = false;
            crossWikiDragData = null;
            enableIframePointerEvents();
        });

        // td-drag-end: Our internal drag ended (from Rust)
        tauriListen("td-drag-end", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            log('td-drag-end from Rust');
            // Note: cleanup happens in the real dragend handler
        });

        // td-drag-cancel: Drag was cancelled
        tauriListen("td-drag-cancel", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            log('td-drag-cancel, reason=' + p.reason);
            cleanup();
        });

        // td-drag-drop-position: Drop occurred at position
        tauriListen("td-drag-drop-position", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            var x = p.x || 0;
            var y = p.y || 0;

            // Handle coordinate scaling based on format
            if (p.physicalPixels) {
                // Windows sends physical pixels that need DPR scaling
                var dpr = window.devicePixelRatio || 1;
                x = x / dpr;
                y = y / dpr;
            }

            log('td-drag-drop-position at (' + x + ', ' + y + ')');
            window.__pendingDropPosition = { x: x, y: y };
        });

        // td-drag-content: Drop data received (for external/inter-wiki drops)
        tauriListen("td-drag-content", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            // Skip if we already handled this via native drop with crossWikiDragData
            if (crossWikiDragData) {
                log('td-drag-content: skipping, native drop already handled with cross-wiki data');
                return;
            }

            var pos = window.__pendingDropPosition || { x: 0, y: 0 };
            delete window.__pendingDropPosition;

            // Extract data from the correct payload structure
            // Rust sends: { types: [...], data: { "text/plain": "...", ... }, targetWindow: "..." }
            var dataMap = p.data || {};
            var text = dataMap['text/plain'] || '';
            var html = dataMap['text/html'] || '';
            var tiddler = dataMap['text/vnd.tiddler'] || '';

            log('td-drag-content, hasText=' + !!text + ', hasHtml=' + !!html + ', hasTiddler=' + !!tiddler);

            var editable = findEditableAt(pos.x, pos.y);
            var target = editable || getElementAt(pos.x, pos.y);

            // Create DataTransfer with drop data
            // When we have text/vnd.tiddler, use the tiddler title for text/plain (not the URL)
            // to avoid TiddlyWiki importing both the tiddler AND the plain text URL
            var dataTransfer = new DataTransfer();

            if (tiddler) {
                try { dataTransfer.setData('text/vnd.tiddler', tiddler); } catch (e) {}
                // Extract title from tiddler JSON for text/plain (useful for input drops)
                try {
                    var parsed = JSON.parse(tiddler);
                    var title = parsed.title || '';
                    if (title) {
                        dataTransfer.setData('text/plain', title);
                    }
                } catch (e) {}
            } else {
                if (text) {
                    try { dataTransfer.setData('text/plain', text); } catch (e) {}
                }
                if (html) {
                    try { dataTransfer.setData('text/html', html); } catch (e) {}
                }
            }

            // Dispatch drop event
            var dropEvent = createDragEvent("drop", pos.x, pos.y, dataTransfer, null);
            target.dispatchEvent(dropEvent);
            log('Dispatched drop to ' + target.tagName);

            cleanup();
        });

        // === Tauri native drag events for CROSS-WIKI drags only ===
        // These dispatch synthetic DOM events with the pre-loaded tiddler data.
        // For file manager drags, native WebKitGTK events handle everything.
        //
        // IMPORTANT: On Windows/macOS, without custom IDropTarget/NSDraggingDestination,
        // Rust can't emit td-cross-wiki-data directly. Instead, we query Rust for
        // pending drag data when tauri://drag-enter fires (IPC approach).

        tauriListen("tauri://drag-enter", function(event) {
            var p = event.payload || {};

            // Skip if we're the drag source (internal drag)
            if (internalDragActive) return;

            var pos = p.position || { x: 0, y: 0 };
            var paths = p.paths || [];

            // If crossWikiDragData is already set (from td-cross-wiki-data on Linux),
            // proceed with synthetic events
            if (crossWikiDragData) {
                log('tauri://drag-enter (cross-wiki) at (' + pos.x + ', ' + pos.y + ')');
                dispatchCrossWikiDragEnter(pos);
                return;
            }

            // For file drags (paths.length > 0), let Tauri handle natively
            if (paths.length > 0) return;

            // For content drags without crossWikiDragData, query Rust via IPC
            // This is the Windows/macOS path where native drag handlers don't exist
            if (window.__TAURI__?.core?.invoke) {
                window.__TAURI__.core.invoke('get_pending_drag_data', {
                    targetWindow: window.__WINDOW_LABEL__
                }).then(function(data) {
                    if (data && data.text_vnd_tiddler) {
                        log('Cross-wiki drag detected via IPC: tiddler data received');
                        crossWikiDragData = data.text_vnd_tiddler;
                        dispatchCrossWikiDragEnter(pos);
                    }
                }).catch(function(err) {
                    // Command might not exist yet, that's OK - fallback to existing flow
                    log('get_pending_drag_data not available: ' + err);
                });
            }
        });

        // Helper to dispatch cross-wiki dragenter events
        function dispatchCrossWikiDragEnter(pos) {
            externalDragActive = true;
            disableIframePointerEvents();

            var dataTransfer = new DataTransfer();
            try {
                dataTransfer.setData('text/vnd.tiddler', crossWikiDragData);
                var parsed = JSON.parse(crossWikiDragData);
                if (parsed.title) {
                    dataTransfer.setData('text/plain', parsed.title);
                }
            } catch (e) {}

            var target = getElementAt(pos.x, pos.y);
            var enterEvent = createDragEvent("dragenter", pos.x, pos.y, dataTransfer, null);
            target.dispatchEvent(enterEvent);
            lastTarget = target;

            // Also dispatch initial dragover
            var overEvent = createDragEvent("dragover", pos.x, pos.y, dataTransfer, null);
            target.dispatchEvent(overEvent);
        }

        tauriListen("tauri://drag-over", function(event) {
            var p = event.payload || {};

            // Skip if we're the drag source (internal drag)
            if (internalDragActive) return;

            var pos = p.position || { x: 0, y: 0 };
            var paths = p.paths || [];

            // If crossWikiDragData is set, dispatch synthetic events
            if (crossWikiDragData) {
                dispatchCrossWikiDragOver(pos);
                return;
            }

            // For file drags (paths.length > 0), let Tauri handle natively
            if (paths.length > 0) return;

            // For content drags without crossWikiDragData, query Rust via IPC
            // This handles the case where drag-enter IPC was still pending
            if (window.__TAURI__?.core?.invoke && !externalDragActive) {
                window.__TAURI__.core.invoke('get_pending_drag_data', {
                    targetWindow: window.__WINDOW_LABEL__
                }).then(function(data) {
                    if (data && data.text_vnd_tiddler) {
                        log('Cross-wiki drag detected via IPC (dragover): tiddler data received');
                        crossWikiDragData = data.text_vnd_tiddler;
                        dispatchCrossWikiDragEnter(pos);
                    }
                }).catch(function(err) {
                    // Ignore - command might not exist
                });
            }
        });

        // Helper to dispatch cross-wiki dragover events
        function dispatchCrossWikiDragOver(pos) {
            // Mark as external drag if not already (in case we missed drag-enter)
            if (!externalDragActive) {
                externalDragActive = true;
                disableIframePointerEvents();
            }

            var dataTransfer = new DataTransfer();
            try {
                dataTransfer.setData('text/vnd.tiddler', crossWikiDragData);
                var parsed = JSON.parse(crossWikiDragData);
                if (parsed.title) {
                    dataTransfer.setData('text/plain', parsed.title);
                }
            } catch (e) {}

            var target = getElementAt(pos.x, pos.y);

            // Dispatch dragenter/dragleave if target changed
            if (target !== lastTarget) {
                var enterEvent = createDragEvent("dragenter", pos.x, pos.y, dataTransfer, lastTarget);
                target.dispatchEvent(enterEvent);
                if (lastTarget) {
                    var leaveEvent = createDragEvent("dragleave", pos.x, pos.y, dataTransfer, target);
                    lastTarget.dispatchEvent(leaveEvent);
                }
                lastTarget = target;
            }

            // Dispatch dragover
            var overEvent = createDragEvent("dragover", pos.x, pos.y, dataTransfer, null);
            target.dispatchEvent(overEvent);
        }

        tauriListen("tauri://drag-leave", function(event) {
            // Skip if we're the drag source (internal drag)
            if (internalDragActive) return;

            // Only dispatch synthetic events for cross-wiki drags
            if (!crossWikiDragData) return;

            log('tauri://drag-leave (cross-wiki)');

            if (lastTarget) {
                var dataTransfer = new DataTransfer();
                var leaveEvent = createDragEvent("dragleave", 0, 0, dataTransfer, null);
                lastTarget.dispatchEvent(leaveEvent);
                lastTarget = null;
            }

            // Note: Don't manually remove tc-dragover - TiddlyWiki's dropzone widget
            // handles that through its resetState() when it receives dragleave

            externalDragActive = false;
            enableIframePointerEvents();
        });

        // tauri://drag-drop: Tauri's native drop handler intercepted a drop
        // Convert it to a DOM drop event for TiddlyWiki
        tauriListen("tauri://drag-drop", function(event) {
            var p = event.payload || {};
            var paths = p.paths || [];

            // Skip if we're the drag source - let native DOM drop handle it
            if (internalDragActive) {
                log('tauri://drag-drop skipped - internal drag, letting native DOM handle');
                return;
            }

            // IMPORTANT: Save cross-wiki data immediately before any cleanup can clear it
            // The cleanup() function may be called by other event handlers (dragend, td-drag-end)
            // before this handler completes
            var savedCrossWikiData = crossWikiDragData;

            log('tauri://drag-drop received, paths=' + paths.length + ', hasCrossWikiData=' + !!savedCrossWikiData);

            // WINDOWS: Skip synthetic events for FILE drops - native HTML5 handles them.
            // SetAllowExternalDrop(true) makes WebView2 fire native drop events.
            // File paths are already stored in __pendingExternalFiles from drag_drop.js.
            // But still handle cross-wiki drops (savedCrossWikiData) since those need synthetic events.
            var isWindows = navigator.platform.indexOf('Win') !== -1 || navigator.userAgent.indexOf('Windows') !== -1;
            if (isWindows && !savedCrossWikiData && paths.length > 0) {
                // Check if these are actual file paths (not data: URLs or tiddler data)
                var hasRealFilePaths = paths.some(function(path) {
                    return path && !path.startsWith('data:') && (path.match(/^[A-Za-z]:[\\/]/) || path.startsWith('/'));
                });
                if (hasRealFilePaths) {
                    log('tauri://drag-drop: Windows file drop - skipping synthetic events, native HTML5 handles it');
                    cleanup();
                    return;
                }
            }

            // For cross-wiki drops, we have the tiddler data pre-loaded
            // Dispatch a synthetic drop event with that data
            if (savedCrossWikiData) {
                var pos = p.position || { x: 0, y: 0 };

                // If dropping into an editable element, skip synthetic drop
                // Native WebKit drop already inserted the text, no need for TiddlyWiki import
                if (findEditableAt(pos.x, pos.y)) {
                    log('Cross-wiki drop target is editable, skipping synthetic drop (native already handled)');
                    cleanup();
                    return;
                }

                log('tauri://drag-drop: dispatching synthetic drop with cross-wiki data, length=' + savedCrossWikiData.length);

                var target = getElementAt(pos.x, pos.y) || lastTarget || document.body;

                var dataTransfer = new DataTransfer();
                var tiddlerTitle = null;

                // Parse the tiddler data to extract title
                try {
                    var parsed = JSON.parse(savedCrossWikiData);
                    tiddlerTitle = parsed.title || null;
                    log('Parsed cross-wiki tiddler: title=' + tiddlerTitle);
                } catch (e) {
                    log('Error parsing cross-wiki JSON: ' + e + ', data preview=' + savedCrossWikiData.substring(0, 100));
                }

                // Set the data on DataTransfer
                try {
                    dataTransfer.setData('text/vnd.tiddler', savedCrossWikiData);
                    if (tiddlerTitle) {
                        dataTransfer.setData('text/plain', tiddlerTitle);
                    }
                } catch (e) {
                    log('Error setting DataTransfer: ' + e);
                }

                var dropEvent = createDragEvent("drop", pos.x, pos.y, dataTransfer, null);
                target.dispatchEvent(dropEvent);
                log('Dispatched cross-wiki drop to ' + target.tagName);

                cleanup();
                return;
            }

            if (paths.length === 0) {
                // If external drag is active, td-drag-content will handle the drop
                if (externalDragActive) {
                    log('tauri://drag-drop: no paths, but external drag active - td-drag-content will handle');
                    return;
                }
                cleanup();
                return;
            }

            // Check if this is tiddler data (data: URL)
            var tiddlerData = null;
            var textData = null;

            for (var i = 0; i < paths.length; i++) {
                var path = paths[i];
                if (path.startsWith('data:text/vnd.tiddler,')) {
                    var rawData = path.substring('data:text/vnd.tiddler,'.length);
                    try {
                        // First try to decode as URL-encoded data
                        tiddlerData = decodeURIComponent(rawData);
                        log('Extracted tiddler data from tauri://drag-drop (decoded)');
                    } catch (e) {
                        // If decoding fails, the data might already be raw JSON (e.g., from Firefox)
                        // Try to use it directly if it looks like valid JSON
                        if (rawData.trim().startsWith('[') || rawData.trim().startsWith('{')) {
                            tiddlerData = rawData;
                            log('Using raw tiddler data from tauri://drag-drop (not URL-encoded)');
                        } else {
                            log('Failed to decode tiddler data: ' + e);
                        }
                    }
                } else if (!path.startsWith('data:') && !path.startsWith('/')) {
                    // Plain text
                    textData = path;
                }
            }

            if (!tiddlerData && !textData) {
                // If this is an external drag (from file manager etc.), td-drag-content will handle it
                // Don't cleanup here or we'll clear the state before td-drag-content fires
                if (externalDragActive) {
                    log('tauri://drag-drop: no tiddler/text data, but external drag active - td-drag-content will handle');
                    return; // Let td-drag-content handle the drop
                }
                log('No usable data in tauri://drag-drop');
                cleanup();
                return;
            }

            // Get drop position - prefer event payload position, then tracked position
            var pos = p.position || lastDragPosition || window.__pendingDropPosition || { x: window.innerWidth / 2, y: window.innerHeight / 2 };
            delete window.__pendingDropPosition;
            log('Drop position: (' + pos.x + ', ' + pos.y + ')');

            // Find drop target
            var target = getElementAt(pos.x, pos.y);

            // If dropping into an editable element, skip synthetic drop
            // Native WebKit drop already inserted the text, no need for TiddlyWiki import
            if (findEditableAt(pos.x, pos.y)) {
                log('Drop target is editable, skipping synthetic drop (native already handled)');
                cleanup();
                return;
            }

            // Create DataTransfer with the data
            var dataTransfer = new DataTransfer();
            if (tiddlerData) {
                try { dataTransfer.setData('text/vnd.tiddler', tiddlerData); } catch (e) {}
                // Also set as plain text for fallback
                try {
                    var parsed = JSON.parse(tiddlerData);
                    dataTransfer.setData('text/plain', parsed.title || parsed.text || '');
                } catch (e) {}
            }
            if (textData) {
                try { dataTransfer.setData('text/plain', textData); } catch (e) {}
            }

            // Dispatch dragenter and dragover first to activate drop zone
            var enterEvent = createDragEvent("dragenter", pos.x, pos.y, dataTransfer, null);
            target.dispatchEvent(enterEvent);
            var overEvent = createDragEvent("dragover", pos.x, pos.y, dataTransfer, null);
            target.dispatchEvent(overEvent);

            // Dispatch drop event
            var dropEvent = createDragEvent("drop", pos.x, pos.y, dataTransfer, null);
            var dispatched = target.dispatchEvent(dropEvent);
            log('Dispatched dragenter+dragover+drop from tauri://drag-drop to ' + target.tagName +
                ' (class=' + (target.className || 'none') + ', dispatched=' + dispatched +
                ', defaultPrevented=' + dropEvent.defaultPrevented + ')');

            // Log DataTransfer state for debugging
            log('DataTransfer types: ' + (dataTransfer.types ? dataTransfer.types.join(', ') : 'none'));
            try {
                log('DataTransfer text/vnd.tiddler: ' + (dataTransfer.getData('text/vnd.tiddler') ? 'present' : 'empty'));
                log('DataTransfer text/plain: ' + (dataTransfer.getData('text/plain') || 'empty'));
            } catch (e) {
                log('DataTransfer getData error: ' + e);
            }

            cleanup();
        });

        log('GTK event handlers ready');
    }

    // Initialize
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', setupGtkEventHandlers);
    } else {
        setupGtkEventHandlers();
    }

    // Export for debugging
    TD.isInternalDragActive = function() { return internalDragActive; };
    TD.isExternalDragActive = function() { return externalDragActive; };
    TD.getDragData = function() { return dragData; };

})(window.TD = window.TD || {});
