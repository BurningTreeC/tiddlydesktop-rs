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
    var isTextSelectionDrag = false; // True when dragging text selection (not a draggable element)
    var dragData = null;             // Current drag data (for internal drags)
    var dragSource = null;           // Element that started the drag
    var lastTarget = null;           // Last element we dispatched dragover to
    var iframesDisabled = false;     // True when iframes have pointer-events: none
    var lastDragPosition = null;     // Last known drag position (for drop targeting)
    var pendingDragElement = null;   // Element that might be dragged (set on pointerdown)
    var lastPointerType = 'mouse';   // Pointer type from last pointerdown (for synthetic pointerup)

    // === Patch DataTransfer.prototype for Windows/macOS ===
    // On these platforms, the webview strips custom MIME types like text/vnd.tiddler
    // during same-window drags. We patch getData and types to return our captured data.
    // This allows native drops to happen while ensuring correct data is available.
    (function() {
        var originalGetData = DataTransfer.prototype.getData;
        DataTransfer.prototype.getData = function(type) {
            // If internal drag is active and we have captured data, use it
            if (internalDragActive && dragData && type in dragData) {
                return dragData[type];
            }
            return originalGetData.call(this, type);
        };

        // Also patch types getter so iteration includes our captured types
        var typesDescriptor = Object.getOwnPropertyDescriptor(DataTransfer.prototype, 'types');
        if (typesDescriptor && typesDescriptor.get) {
            var originalTypesGetter = typesDescriptor.get;
            Object.defineProperty(DataTransfer.prototype, 'types', {
                get: function() {
                    var originalTypes = originalTypesGetter.call(this);
                    if (internalDragActive && dragData) {
                        var capturedTypes = Object.keys(dragData);
                        var original = Array.from(originalTypes || []);
                        // Merge captured types with original, captured first for priority
                        var merged = capturedTypes.slice();
                        for (var i = 0; i < original.length; i++) {
                            if (merged.indexOf(original[i]) === -1) {
                                merged.push(original[i]);
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

        log('DataTransfer.prototype patched for same-window drag support');
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
        var event = new DragEvent(type, {
            bubbles: true,
            cancelable: true,
            clientX: x,
            clientY: y,
            dataTransfer: dataTransfer,
            relatedTarget: relatedTarget || null
        });
        event.__tiddlyDesktopSynthetic = true;
        return event;
    }

    // === Get element at point ===
    function getElementAt(x, y) {
        var el = document.elementFromPoint(x, y);
        return el || document.body;
    }

    // === Find editable element at position ===
    function findEditableAt(x, y) {
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
        isTextSelectionDrag = false;
        dragData = null;
        dragSource = null;
        lastTarget = null;
        lastDragPosition = null;
        pendingDragElement = null;
        enableIframePointerEvents();

        if (typeof $tw !== 'undefined') {
            $tw.dragInProgress = null;
        }

        document.querySelectorAll('.tc-dragover').forEach(function(el) {
            el.classList.remove('tc-dragover');
        });

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
        dragData = {};

        log('isTextSelectionDrag=' + isTextSelectionDrag);

        // Read data from the real DataTransfer (TiddlyWiki has already populated it)
        var dt = event.dataTransfer;
        if (dt) {
            try {
                var types = dt.types || [];
                for (var i = 0; i < types.length; i++) {
                    var type = types[i];
                    try {
                        dragData[type] = dt.getData(type);
                    } catch (e) {}
                }
                log('Captured drag data types: ' + Object.keys(dragData).join(', '));
            } catch (e) {
                log('Failed to read dataTransfer: ' + e);
            }
        }

        // For text-selection drags, capture the selection if not in DataTransfer
        if (isTextSelectionDrag && !dragData['text/plain']) {
            var selection = window.getSelection();
            if (selection && selection.toString()) {
                dragData['text/plain'] = selection.toString();
                log('Captured selection text: ' + dragData['text/plain'].substring(0, 50));
            }
        }

        // Send data to Rust for inter-wiki drops
        if (window.__TAURI__?.core?.invoke) {
            var tiddlerJson = dragData['text/vnd.tiddler'] || null;
            var tiddlerUri = tiddlerJson ? 'data:text/vnd.tiddler,' + encodeURIComponent(tiddlerJson) : null;

            var data = {
                text_plain: dragData['text/plain'] || null,
                text_html: dragData['text/html'] || null,
                text_vnd_tiddler: tiddlerJson,
                text_uri_list: dragData['text/uri-list'] || null,
                text_x_moz_url: dragData['text/x-moz-url'] || tiddlerUri,
                url: dragData['URL'] || tiddlerUri,
                is_text_selection_drag: isTextSelectionDrag
            };

            window.__TAURI__.core.invoke('prepare_native_drag', { data: data })
                .then(function() { log('Native drag prepared'); })
                .catch(function(err) { log('prepare_native_drag failed: ' + err); });

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

        // Set TiddlyWiki drag state
        if (typeof $tw !== 'undefined') {
            $tw.dragInProgress = dragSource;
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

    // === Block native events ONLY for external drags ===
    // For internal drags, let WebKit handle everything (including caret updates)
    function blockExternalDragEvent(event) {
        if (event.__tiddlyDesktopSynthetic) return;

        // If this is our internal drag, let WebKit handle it naturally
        if (internalDragActive) return;

        // For external drags, we handle via td-drag-* events
        if (externalDragActive) {
            event.preventDefault();
            event.stopPropagation();
            event.stopImmediatePropagation();
        }
    }
    document.addEventListener("dragenter", blockExternalDragEvent, true);
    document.addEventListener("dragover", blockExternalDragEvent, true);
    document.addEventListener("dragleave", blockExternalDragEvent, true);
    document.addEventListener("drop", blockExternalDragEvent, true);

    // Track position during internal drags for tauri://drag-drop handler
    document.addEventListener("dragover", function(event) {
        if (internalDragActive) {
            lastDragPosition = { x: event.clientX, y: event.clientY };
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

        // td-drag-motion: External or cross-wiki drag is over this window
        tauriListen("td-drag-motion", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            // Skip same-window drags - WebKit/internal JS handles them natively
            // But process cross-wiki drags (isOurDrag=true, isSameWindow=false)
            if (p.isSameWindow) return;

            var x = p.x || 0;
            var y = p.y || 0;

            if (!externalDragActive) {
                externalDragActive = true;
                disableIframePointerEvents();
                log('External drag entered window');
            }

            // Create DataTransfer for synthetic events
            var dataTransfer = new DataTransfer();

            var target = getElementAt(x, y);

            // Dispatch dragenter if target changed
            if (target !== lastTarget) {
                if (lastTarget) {
                    var leaveEvent = createDragEvent("dragleave", x, y, dataTransfer, target);
                    lastTarget.dispatchEvent(leaveEvent);
                }
                var enterEvent = createDragEvent("dragenter", x, y, dataTransfer, lastTarget);
                target.dispatchEvent(enterEvent);
                lastTarget = target;
            }

            // Dispatch dragover
            var overEvent = createDragEvent("dragover", x, y, dataTransfer, null);
            target.dispatchEvent(overEvent);
        });

        // td-drag-leave: External or cross-wiki drag left this window
        tauriListen("td-drag-leave", function(event) {
            var p = event.payload || {};
            if (p.targetWindow && p.targetWindow !== window.__WINDOW_LABEL__) return;

            // Skip same-window drags - WebKit/internal JS handles them natively
            if (p.isSameWindow) return;

            log('td-drag-leave');

            if (lastTarget) {
                var dataTransfer = new DataTransfer();
                var leaveEvent = createDragEvent("dragleave", 0, 0, dataTransfer, null);
                lastTarget.dispatchEvent(leaveEvent);
                lastTarget = null;
            }

            document.querySelectorAll('.tc-dragover').forEach(function(el) {
                el.classList.remove('tc-dragover');
            });

            externalDragActive = false;
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
            var dataTransfer = new DataTransfer();

            if (tiddler) {
                try { dataTransfer.setData('text/vnd.tiddler', tiddler); } catch (e) {}
            }
            if (text) {
                try { dataTransfer.setData('text/plain', text); } catch (e) {}
            }
            if (html) {
                try { dataTransfer.setData('text/html', html); } catch (e) {}
            }

            // For INPUT/TEXTAREA, manually insert text
            if (editable && (editable.tagName === 'INPUT' || editable.tagName === 'TEXTAREA')) {
                var textToInsert = text || '';
                if (textToInsert) {
                    editable.focus();
                    var start = editable.selectionStart || 0;
                    var end = editable.selectionEnd || 0;
                    var value = editable.value || '';
                    editable.value = value.substring(0, start) + textToInsert + value.substring(end);
                    var newPos = start + textToInsert.length;
                    editable.setSelectionRange(newPos, newPos);
                    log('Inserted text into ' + editable.tagName);
                    editable.dispatchEvent(new Event('input', { bubbles: true }));
                    cleanup();
                    return;
                }
            }

            // Dispatch drop event for other elements
            var dropEvent = createDragEvent("drop", pos.x, pos.y, dataTransfer, null);
            target.dispatchEvent(dropEvent);
            log('Dispatched drop to ' + target.tagName);

            cleanup();
        });

        // tauri://drag-drop: Tauri's native drop handler intercepted a drop
        // Convert it to a DOM drop event for TiddlyWiki
        tauriListen("tauri://drag-drop", function(event) {
            var p = event.payload || {};
            var paths = p.paths || [];

            log('tauri://drag-drop received, paths=' + paths.length);

            if (paths.length === 0) {
                cleanup();
                return;
            }

            // Check if this is tiddler data (data: URL)
            var tiddlerData = null;
            var textData = null;

            for (var i = 0; i < paths.length; i++) {
                var path = paths[i];
                if (path.startsWith('data:text/vnd.tiddler,')) {
                    try {
                        tiddlerData = decodeURIComponent(path.substring('data:text/vnd.tiddler,'.length));
                        log('Extracted tiddler data from tauri://drag-drop');
                    } catch (e) {
                        log('Failed to decode tiddler data: ' + e);
                    }
                } else if (!path.startsWith('data:') && !path.startsWith('/')) {
                    // Plain text
                    textData = path;
                }
            }

            if (!tiddlerData && !textData) {
                log('No usable data in tauri://drag-drop');
                cleanup();
                return;
            }

            // Get drop position - use tracked position, pending position, or center of viewport
            var pos = lastDragPosition || window.__pendingDropPosition || { x: window.innerWidth / 2, y: window.innerHeight / 2 };
            delete window.__pendingDropPosition;
            log('Drop position: (' + pos.x + ', ' + pos.y + ')');

            // Find drop target
            var target = getElementAt(pos.x, pos.y);

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
