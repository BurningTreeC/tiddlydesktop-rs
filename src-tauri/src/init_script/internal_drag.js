// TiddlyDesktop Initialization Script - Internal Drag Polyfill Module
// Handles: internal TiddlyWiki drag-and-drop (tiddlers, tags, links)
//
// This polyfill intercepts drags of draggable elements within the wiki and
// handles them using pointer events + synthetic DOM events. This is necessary
// because GTK's drag_dest_set may interfere with WebKitGTK's native drag handling.
//
// External drops (from other apps) are handled by drag_drop.js via td-* events.

(function(TD) {
    'use strict';

    // Skip entirely for the main launcher window - it has no drag-drop functionality
    if (window.__WINDOW_LABEL__ === 'main') {
        return;
    }

    // Helper function to log to terminal via Tauri
    function log(message) {
        if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
            window.__TAURI__.core.invoke("js_log", { message: "[internal_drag] " + message });
        }
    }

    // Global error handler to catch and log JavaScript errors to terminal
    window.addEventListener("error", function(event) {
        log('[JS ERROR] ' + event.message + ' at ' + event.filename + ':' + event.lineno + ':' + event.colno);
        if (event.error && event.error.stack) {
            log('[JS ERROR STACK] ' + event.error.stack);
        }
    });

    // Also catch unhandled promise rejections
    window.addEventListener("unhandledrejection", function(event) {
        log('[JS UNHANDLED REJECTION] ' + (event.reason ? event.reason.toString() : 'unknown'));
        if (event.reason && event.reason.stack) {
            log('[JS REJECTION STACK] ' + event.reason.stack);
        }
    });

    // DEBUG: Very early window-level pointerdown handler to detect if events are firing
    window.addEventListener("pointerdown", function(event) {
        if (event.button === 0 && !event.__tiddlyDesktopSynthetic) {
            var targetTag = event.target && event.target.tagName ? event.target.tagName : 'unknown';
            log('[WINDOW pointerdown] fired on ' + targetTag +
                ', pointerType=' + event.pointerType + ', pointerId=' + event.pointerId);
        }
    }, true);  // Capture phase - runs before document handlers

    // Set up window-specific Tauri event listener
    // This ensures we only receive events for THIS window, not all windows
    var tauriListen = null;
    var currentWindowLabel = window.__WINDOW_LABEL__ || 'unknown';
    function setupTauriListen() {
        if (tauriListen) return; // Already set up
        if (window.__TAURI__ && window.__TAURI__.window) {
            var currentWindow = window.__TAURI__.window.getCurrentWindow();
            tauriListen = currentWindow.listen.bind(currentWindow);
            currentWindowLabel = window.__WINDOW_LABEL__ || 'unknown';
            log('Tauri window-specific listen set up for: ' + currentWindowLabel);
        }
    }

    // Store drag data globally since dataTransfer may not work reliably
    window.__tiddlyDesktopDragData = null;
    var internalDragSource = null;
    var internalDragImage = null;
    var internalDragActive = false;
    var dragImageOffsetX = 0;
    var dragImageOffsetY = 0;
    var dragImageIsBlank = false;
    var pointerDownTarget = null;
    var pointerDownPos = null;
    var pointerDragStarted = false;
    var pointerDragDataTransfer = null;
    var lastDragOverTarget = null;
    var DRAG_THRESHOLD = 3;
    var capturedSelection = null;  // Selection captured on pointerdown
    var capturedSelectionHtml = null;
    var capturedPointerId = null;  // Track which pointer we're handling
    var nativeDragStarting = false;  // Flag to prevent multiple native drag starts
    var nativeDragFromSelf = false;  // True when we started a native drag that might re-enter
    var nativeDragData = null;  // Saved drag data for re-entry detection
    var savedDragImage = null;  // Saved drag image element for re-entry
    var savedDragInProgress = null;  // Saved $tw.dragInProgress for re-entry
    var savedDragSource = null;  // Saved internalDragSource element for re-entry
    var pollingActive = false;  // True when Rust GDK polling is handling drag tracking
    var dragCancelledByEscape = false;  // True when Escape was pressed to cancel drag

    // Convert payload coordinates from native events
    // Windows sends physical pixels (needs dpr scaling), Linux/macOS send CSS pixels
    function getPayloadCoordinates(payload) {
        var x = payload.x !== undefined ? payload.x : 0;
        var y = payload.y !== undefined ? payload.y : 0;
        if (payload.physicalPixels) {
            var dpr = window.devicePixelRatio || 1;
            x = x / dpr;
            y = y / dpr;
        }
        return { x: x, y: y };
    }

    // Extract background color from element or its ancestors
    function getBackgroundColor(element) {
        var el = element;
        while (el) {
            var style = window.getComputedStyle(el);
            var bg = style.backgroundColor;
            if (bg && bg !== "transparent" && bg !== "rgba(0, 0, 0, 0)") {
                return bg;
            }
            el = el.parentElement;
        }
        return "var(--tiddler-background, white)";
    }

    // Extract foreground/text color from element or its ancestors
    function getForegroundColor(element) {
        var el = element;
        while (el) {
            var style = window.getComputedStyle(el);
            var color = style.color;
            if (color && color !== "transparent" && color !== "rgba(0, 0, 0, 0)") {
                return color;
            }
            el = el.parentElement;
        }
        return "#333";
    }

    // Copy all computed styles from source to target element
    // This ensures the cloned element looks identical to the original
    function copyComputedStyles(source, target) {
        var computedStyle = window.getComputedStyle(source);
        for (var i = 0; i < computedStyle.length; i++) {
            var prop = computedStyle[i];
            try {
                target.style.setProperty(prop, computedStyle.getPropertyValue(prop));
            } catch (e) {
                // Some properties may not be settable
            }
        }
    }

    // Recursively copy computed styles for element and all descendants
    function deepCopyComputedStyles(source, target) {
        copyComputedStyles(source, target);
        var sourceChildren = source.children;
        var targetChildren = target.children;
        for (var i = 0; i < sourceChildren.length && i < targetChildren.length; i++) {
            deepCopyComputedStyles(sourceChildren[i], targetChildren[i]);
        }
    }

    function createDragImage(sourceElement, clientX, clientY, selectedText) {
        if (internalDragImage && internalDragImage.parentNode) {
            internalDragImage.parentNode.removeChild(internalDragImage);
        }

        if (dragImageIsBlank) {
            return null;
        }

        var dragEl;

        // For text selection drags, create a simple text element
        if (selectedText) {
            dragEl = document.createElement("div");
            // Truncate long selections for the drag image
            var displayText = selectedText.length > 50 ? selectedText.substring(0, 47) + "..." : selectedText;
            dragEl.textContent = displayText;
            dragEl.style.position = "fixed";
            dragEl.style.pointerEvents = "none";
            dragEl.style.zIndex = "999999";
            dragEl.style.opacity = "0.8";
            dragEl.style.maxWidth = "300px";
            dragEl.style.overflow = "hidden";
            dragEl.style.whiteSpace = "nowrap";
            dragEl.style.textOverflow = "ellipsis";
            dragEl.style.background = getBackgroundColor(sourceElement);
            dragEl.style.color = getForegroundColor(sourceElement);
            dragEl.style.padding = "6px 10px";
            dragEl.style.borderRadius = "4px";
            dragEl.style.boxShadow = "0 2px 8px rgba(0,0,0,0.3)";
            dragEl.style.border = "1px solid rgba(0,0,0,0.1)";
            dragEl.style.fontSize = "13px";

            dragImageOffsetX = 10;
            dragImageOffsetY = 10;
        } else {
            // For element drags, clone the element with all computed styles
            dragEl = sourceElement.cloneNode(true);
            // Copy all computed styles from source to ensure identical appearance
            deepCopyComputedStyles(sourceElement, dragEl);
            // Override with drag-specific positioning and effects
            dragEl.style.position = "fixed";
            dragEl.style.pointerEvents = "none";
            dragEl.style.zIndex = "999999";
            dragEl.style.opacity = "0.7";

            var rect = sourceElement.getBoundingClientRect();
            dragImageOffsetX = clientX - rect.left;
            dragImageOffsetY = clientY - rect.top;
        }

        dragEl.style.left = (clientX - dragImageOffsetX) + "px";
        dragEl.style.top = (clientY - dragImageOffsetY) + "px";

        document.body.appendChild(dragEl);
        internalDragImage = dragEl;
        return dragEl;
    }

    function updateDragImagePosition(clientX, clientY) {
        if (internalDragImage) {
            internalDragImage.style.left = (clientX - dragImageOffsetX) + "px";
            internalDragImage.style.top = (clientY - dragImageOffsetY) + "px";
        }
    }

    function removeDragImage() {
        if (internalDragImage && internalDragImage.parentNode) {
            internalDragImage.parentNode.removeChild(internalDragImage);
            internalDragImage = null;
        }
    }

    // Get the element at a point, handling iframes
    // Returns { target, iframe, adjustedX, adjustedY }
    function getElementAtPoint(clientX, clientY) {
        var target = document.elementFromPoint(clientX, clientY);

        if (target && target.tagName === 'IFRAME') {
            try {
                var iframeDoc = target.contentDocument || target.contentWindow.document;
                if (iframeDoc) {
                    var iframeRect = target.getBoundingClientRect();
                    var adjustedX = clientX - iframeRect.left;
                    var adjustedY = clientY - iframeRect.top;
                    var innerTarget = iframeDoc.elementFromPoint(adjustedX, adjustedY);
                    if (innerTarget) {
                        return {
                            target: innerTarget,
                            iframe: target,
                            adjustedX: adjustedX,
                            adjustedY: adjustedY
                        };
                    }
                }
            } catch (e) {
                // Cross-origin iframe - can't access contentDocument
            }
        }

        return { target: target, iframe: null, adjustedX: clientX, adjustedY: clientY };
    }

    // Check if element is a text-accepting input
    function isTextInput(el) {
        if (!el || !el.tagName) return false;
        if (el.tagName === 'TEXTAREA') return true;
        if (el.tagName === 'INPUT') {
            var type = (el.type || 'text').toLowerCase();
            // These input types accept text
            return ['text', 'search', 'url', 'tel', 'email', 'password', 'number'].indexOf(type) !== -1;
        }
        return false;
    }

    // Check if element is contenteditable
    function isContentEditable(el) {
        if (!el) return false;
        return el.isContentEditable || el.contentEditable === 'true';
    }

    // Get character position in input/textarea from coordinates
    function getInputCaretPositionFromPoint(el, clientX, clientY) {
        var rect = el.getBoundingClientRect();
        var style = window.getComputedStyle(el);

        // Get padding
        var paddingLeft = parseFloat(style.paddingLeft) || 0;
        var paddingTop = parseFloat(style.paddingTop) || 0;

        // Calculate relative position within the text area
        var relX = clientX - rect.left - paddingLeft + el.scrollLeft;
        var relY = clientY - rect.top - paddingTop + el.scrollTop;

        // For single-line inputs
        if (el.tagName === 'INPUT') {
            // Create a temporary span to measure text width
            var span = document.createElement('span');
            span.style.font = style.font;
            span.style.fontSize = style.fontSize;
            span.style.fontFamily = style.fontFamily;
            span.style.fontWeight = style.fontWeight;
            span.style.letterSpacing = style.letterSpacing;
            span.style.position = 'absolute';
            span.style.visibility = 'hidden';
            span.style.whiteSpace = 'pre';
            document.body.appendChild(span);

            var text = el.value;
            var pos = text.length; // Default to end

            // Binary search for the position
            for (var i = 0; i <= text.length; i++) {
                span.textContent = text.substring(0, i);
                if (span.offsetWidth >= relX) {
                    // Check if we're closer to this position or the previous one
                    if (i > 0) {
                        span.textContent = text.substring(0, i - 1);
                        var prevWidth = span.offsetWidth;
                        span.textContent = text.substring(0, i);
                        var currWidth = span.offsetWidth;
                        pos = (relX - prevWidth) < (currWidth - relX) ? i - 1 : i;
                    } else {
                        pos = 0;
                    }
                    break;
                }
            }

            document.body.removeChild(span);
            return pos;
        }

        // For textareas (multi-line)
        if (el.tagName === 'TEXTAREA') {
            // Create a mirror div
            var mirror = document.createElement('div');
            mirror.style.position = 'absolute';
            mirror.style.visibility = 'hidden';
            mirror.style.font = style.font;
            mirror.style.fontSize = style.fontSize;
            mirror.style.fontFamily = style.fontFamily;
            mirror.style.fontWeight = style.fontWeight;
            mirror.style.letterSpacing = style.letterSpacing;
            mirror.style.lineHeight = style.lineHeight;
            mirror.style.whiteSpace = 'pre-wrap';
            mirror.style.wordWrap = 'break-word';
            mirror.style.width = (el.clientWidth - paddingLeft - parseFloat(style.paddingRight)) + 'px';
            document.body.appendChild(mirror);

            var text = el.value;
            var lineHeight = parseFloat(style.lineHeight) || parseFloat(style.fontSize) * 1.2;
            var lineIndex = Math.floor(relY / lineHeight);

            // Split into lines (respecting word wrap is complex, so approximate)
            var lines = text.split('\n');
            var charPos = 0;
            var currentLine = 0;

            // Find which line we're on
            for (var i = 0; i < lines.length && currentLine < lineIndex; i++) {
                charPos += lines[i].length + 1; // +1 for newline
                currentLine++;
            }

            // Now find position within the line
            if (lineIndex < lines.length) {
                var lineText = lines[lineIndex] || '';
                var span = document.createElement('span');
                span.style.font = style.font;
                span.style.fontSize = style.fontSize;
                span.style.fontFamily = style.fontFamily;
                span.style.visibility = 'hidden';
                span.style.position = 'absolute';
                span.style.whiteSpace = 'pre';
                document.body.appendChild(span);

                for (var j = 0; j <= lineText.length; j++) {
                    span.textContent = lineText.substring(0, j);
                    if (span.offsetWidth >= relX) {
                        charPos += Math.max(0, j - 1);
                        break;
                    }
                    if (j === lineText.length) {
                        charPos += lineText.length;
                    }
                }

                document.body.removeChild(span);
            }

            document.body.removeChild(mirror);
            return Math.min(charPos, text.length);
        }

        return 0;
    }

    // Set caret position in input/textarea from coordinates (for visual feedback during drag)
    function setInputCaretFromPoint(el, clientX, clientY) {
        var pos = getInputCaretPositionFromPoint(el, clientX, clientY);
        // Ensure the browser window is focused before focusing the element
        // This is needed during re-entry when the OS drag system had focus
        window.focus();
        el.focus();
        el.setSelectionRange(pos, pos);
        return pos;
    }

    // Set caret position in contenteditable from coordinates
    function setContentEditableCaretFromPoint(el, clientX, clientY) {
        // Use the document that owns the element (important for iframes)
        var doc = el.ownerDocument || document;
        var win = doc.defaultView || window;

        // Ensure the browser window is focused before focusing the element
        // This is needed during re-entry when the OS drag system had focus
        window.focus();
        // For iframes, also focus the iframe's window
        if (win !== window) {
            win.focus();
        }

        if (doc.caretRangeFromPoint) {
            var range = doc.caretRangeFromPoint(clientX, clientY);
            if (range) {
                var sel = win.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
                el.focus();
                return range;
            }
        } else if (doc.caretPositionFromPoint) {
            var pos = doc.caretPositionFromPoint(clientX, clientY);
            if (pos) {
                var range = doc.createRange();
                range.setStart(pos.offsetNode, pos.offset);
                range.collapse(true);
                var sel = win.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
                el.focus();
                return range;
            }
        }
        return null;
    }

    // Insert text into input/textarea at the position determined by coordinates
    function insertTextAtPoint(el, clientX, clientY, text) {
        if (!text) return false;

        var pos = getInputCaretPositionFromPoint(el, clientX, clientY);
        var value = el.value || '';

        // Insert text at calculated position
        el.value = value.substring(0, pos) + text + value.substring(pos);

        // Set cursor position after inserted text
        var newPos = pos + text.length;
        el.focus();
        el.setSelectionRange(newPos, newPos);

        // Trigger input event for any listeners (e.g., TiddlyWiki's change detection)
        el.dispatchEvent(new Event('input', { bubbles: true }));
        el.dispatchEvent(new Event('change', { bubbles: true }));

        return true;
    }

    // Insert text/HTML into contenteditable at the position determined by coordinates
    function insertIntoContentEditableAtPoint(el, clientX, clientY, text, html) {
        // Use the document that owns the element (important for iframes)
        var doc = el.ownerDocument || document;
        var win = doc.defaultView || window;

        el.focus();

        // Get caret position from coordinates
        var range = null;
        if (doc.caretRangeFromPoint) {
            range = doc.caretRangeFromPoint(clientX, clientY);
        } else if (doc.caretPositionFromPoint) {
            var pos = doc.caretPositionFromPoint(clientX, clientY);
            if (pos) {
                range = doc.createRange();
                range.setStart(pos.offsetNode, pos.offset);
                range.collapse(true);
            }
        }

        if (range) {
            var sel = win.getSelection();
            sel.removeAllRanges();
            sel.addRange(range);

            // Insert content
            if (html) {
                var frag = range.createContextualFragment(html);
                range.insertNode(frag);
            } else if (text) {
                var textNode = doc.createTextNode(text);
                range.insertNode(textNode);
                // Move cursor after inserted text
                range.setStartAfter(textNode);
                range.setEndAfter(textNode);
                sel.removeAllRanges();
                sel.addRange(range);
            }
        } else {
            // Fallback: append to end
            if (html) {
                el.innerHTML += html;
            } else if (text) {
                el.appendChild(doc.createTextNode(text));
            }
        }

        // Trigger input event
        el.dispatchEvent(new Event('input', { bubbles: true }));
        return true;
    }

    // Patch DataTransfer.prototype.setData to capture data as it's set
    var originalSetData = DataTransfer.prototype.setData;
    DataTransfer.prototype.setData = function(type, data) {
        if (!window.__tiddlyDesktopDragData) {
            window.__tiddlyDesktopDragData = {};
        }
        window.__tiddlyDesktopDragData[type] = data;
        return originalSetData.call(this, type, data);
    };

    // Also patch getData to use our cache as fallback
    var originalGetData = DataTransfer.prototype.getData;
    DataTransfer.prototype.getData = function(type) {
        var result = originalGetData.call(this, type);
        if (!result && window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData[type]) {
            return window.__tiddlyDesktopDragData[type];
        }
        return result;
    };

    // Helper to create synthetic drag events
    function createSyntheticDragEvent(type, options, dataTransfer) {
        var event = new DragEvent(type, Object.assign({
            bubbles: true,
            cancelable: true
        }, options));

        if (dataTransfer) {
            Object.defineProperty(event, 'dataTransfer', {
                value: dataTransfer,
                writable: false,
                configurable: true
            });
        }

        event.__tiddlyDesktopSynthetic = true;
        return event;
    }

    // Helper to check if element is a draggable TiddlyWiki element
    function isDraggableElement(el) {
        if (!el) return false;
        return el.getAttribute("draggable") === "true" ||
               el.classList.contains("tc-draggable") ||
               el.classList.contains("tc-tiddlylink");
    }

    function findDraggableAncestor(el) {
        while (el) {
            if (isDraggableElement(el)) return el;
            el = el.parentElement;
        }
        return null;
    }

    // Helper to cancel drag
    function cancelDrag(reason) {
        if (!internalDragActive && !pointerDragStarted && !nativeDragFromSelf) return;

        log('[TiddlyDesktop] cancelDrag called: ' + reason);

        // Create a DataTransfer with dropEffect="none" to signal cancellation
        // We use a wrapper object since DataTransfer.dropEffect may be read-only
        var dt = pointerDragDataTransfer || new DataTransfer();
        var cancelDt = {
            dropEffect: "none",
            effectAllowed: dt.effectAllowed || "none",
            types: dt.types || [],
            getData: function(type) { return dt.getData ? dt.getData(type) : ""; },
            setData: function(type, data) { if (dt.setData) dt.setData(type, data); },
            items: dt.items,
            files: dt.files
        };

        if (lastDragOverTarget) {
            var leaveEvent = createSyntheticDragEvent("dragleave", { relatedTarget: null }, cancelDt);
            lastDragOverTarget.dispatchEvent(leaveEvent);
        }

        // Remove visual feedback classes first
        document.querySelectorAll(".tc-dragover").forEach(function(el) {
            el.classList.remove("tc-dragover");
        });

        document.querySelectorAll(".tc-dragging").forEach(function(el) {
            el.classList.remove("tc-dragging");
        });

        // Dispatch dragend to the source element (TiddlyWiki listens for this)
        // IMPORTANT: Do this while $tw.dragInProgress is still set
        if (internalDragSource) {
            var endEvent = createSyntheticDragEvent("dragend", {}, cancelDt);
            internalDragSource.dispatchEvent(endEvent);

            // Release pointer capture
            if (capturedPointerId !== null) {
                try {
                    internalDragSource.releasePointerCapture(capturedPointerId);
                } catch (e) {}
            }
        }

        // Clear TiddlyWiki state AFTER dispatching events
        if (typeof $tw !== "undefined") {
            $tw.dragInProgress = null;
        }

        // Remove visual feedback classes
        document.querySelectorAll(".tc-dragover").forEach(function(el) {
            el.classList.remove("tc-dragover");
        });
        document.querySelectorAll(".tc-dragging").forEach(function(el) {
            el.classList.remove("tc-dragging");
        });

        // Remove any leftover TiddlyWiki drag image elements
        document.querySelectorAll(".tc-tiddler-dragger").forEach(function(el) {
            el.parentNode.removeChild(el);
        });

        // Clean up native drag state on Rust side
        if (nativeDragFromSelf || nativeDragStarting) {
            cleanupNativeDrag();
        }

        window.__tiddlyDesktopDragData = null;
        window.__tiddlyDesktopEffectAllowed = null;
        internalDragSource = null;
        internalDragActive = false;
        pointerDownTarget = null;
        pointerDownPos = null;
        pointerDragStarted = false;
        pointerDragDataTransfer = null;
        lastDragOverTarget = null;
        capturedSelection = null;
        capturedSelectionHtml = null;
        capturedPointerId = null;
        nativeDragStarting = false;
        nativeDragFromSelf = false;
        nativeDragData = null;
        savedDragInProgress = null;
        savedDragSource = null;
        pollingActive = false;
        // Destroy saved drag image
        if (savedDragImage && savedDragImage.parentNode) {
            savedDragImage.parentNode.removeChild(savedDragImage);
        }
        savedDragImage = null;
        removeDragImage();
        document.body.style.userSelect = "";
        document.body.style.webkitUserSelect = "";
    }

    // Check if there's a text selection
    function getSelectedText() {
        var selection = window.getSelection();
        if (selection && selection.toString().trim().length > 0) {
            return selection.toString();
        }
        return null;
    }

    // Get HTML of selection
    function getSelectedHtml() {
        var selection = window.getSelection();
        if (selection && selection.rangeCount > 0) {
            var range = selection.getRangeAt(0);
            var container = document.createElement("div");
            container.appendChild(range.cloneContents());
            return container.innerHTML;
        }
        return null;
    }

    // Capture dragstart to intercept native drags
    document.addEventListener("dragstart", function(event) {
        if (event.__tiddlyDesktopSynthetic) return;

        var target = event.target;
        if (target && target.nodeType !== 1) {
            target = target.parentElement;
        }
        if (!target) return;

        // Check if this is a draggable element (tiddlers, tags, links)
        var draggable = findDraggableAncestor(target);

        // Check if this is a text selection drag (use captured selection since browser clears it on drag)
        var selectedText = capturedSelection || getSelectedText();
        var isTextSelectionDrag = !draggable && selectedText;

        if (!draggable && !isTextSelectionDrag) {
            // Neither draggable element nor text selection - don't intercept
            return;
        }

        // Cancel native drag - we'll handle it with pointer events
        event.preventDefault();
    }, true);

    // Reset cache at start of each drag (but not if we're already handling an internal drag)
    document.addEventListener("dragstart", function(event) {
        if (!event.__tiddlyDesktopSynthetic && !internalDragActive) {
            window.__tiddlyDesktopDragData = {};
        }
    }, true);

    // Enhance dragover for internal drags
    document.addEventListener("dragover", function(event) {
        if (event.__tiddlyDesktopSynthetic) return;
        if (internalDragActive) {
            event.preventDefault();
            if (event.dataTransfer) {
                var effect = window.__tiddlyDesktopEffectAllowed || "all";
                if (effect === "copyMove" || effect === "all") {
                    event.dataTransfer.dropEffect = "move";
                } else if (effect === "copy" || effect === "copyLink") {
                    event.dataTransfer.dropEffect = "copy";
                } else if (effect === "link" || effect === "linkMove") {
                    event.dataTransfer.dropEffect = "link";
                } else {
                    event.dataTransfer.dropEffect = "move";
                }
            }
        }
    }, true);

    // Clean up when drag ends
    document.addEventListener("dragend", function(event) {
        // If we're handling an internal drag via pointer events, ignore native dragend
        if (pointerDragStarted && !event.__tiddlyDesktopSynthetic) {
            return;
        }

        // If transitioning to native drag, don't clean up - we need the drag image
        if (nativeDragStarting || nativeDragFromSelf) {
            return;
        }

        window.__tiddlyDesktopDragData = null;
        window.__tiddlyDesktopEffectAllowed = null;
        internalDragSource = null;
        internalDragActive = false;
        capturedSelection = null;
        capturedSelectionHtml = null;
        removeDragImage();
        document.body.style.userSelect = "";
        document.body.style.webkitUserSelect = "";
    }, true);

    // Flag set when native input injection fails on Wayland + wlroots
    // When true, mousedown fallback is enabled for the next interaction
    var webkitPointerBroken = false;

    // Mousedown fallback handler - only active when webkitPointerBroken is true
    // This is needed on Wayland + wlroots where libei is not available
    // On X11 and Wayland + GNOME/KDE, native input injection should work
    document.addEventListener("mousedown", function(event) {
        try {
            if (event.button !== 0) return;

            // Only use fallback if webkitPointerBroken flag is set
            if (!webkitPointerBroken) {
                return;
            }

            var targetTag = event.target && event.target.tagName ? event.target.tagName : 'unknown';
            log('[mousedown] FALLBACK active: button=' + event.button + ', capturedPointerId=' + capturedPointerId +
                ', target=' + targetTag);

            // Use mousedown as fallback to start drag tracking
            if (capturedPointerId === null && !pointerDragStarted && !internalDragActive && !nativeDragFromSelf) {
                log('[mousedown] FALLBACK: using mousedown because webkitPointerBroken=true');

                // Capture any existing text selection
                capturedSelection = getSelectedText();
                capturedSelectionHtml = getSelectedHtml();

                var target = findDraggableAncestor(event.target);
                if (target) {
                    pointerDownTarget = target;
                    pointerDownPos = { x: event.clientX, y: event.clientY };
                    pointerDragStarted = false;
                    // Use pointerId 1 as fallback (standard mouse pointer)
                    capturedPointerId = 1;
                    log('[mousedown] FALLBACK: Started tracking drag on ' + target.tagName);
                } else if (capturedSelection) {
                    pointerDownTarget = null;
                    pointerDownPos = { x: event.clientX, y: event.clientY };
                    pointerDragStarted = false;
                    capturedPointerId = 1;
                    log('[mousedown] FALLBACK: Started tracking text selection drag');
                }

                // Clear the broken flag after using the fallback once
                // The next pointerdown (if it fires) will work normally
                webkitPointerBroken = false;
            }
        } catch (e) {
            log('[mousedown] ERROR: ' + e.message + ' | ' + e.stack);
        }
    }, true);

    // Pointerdown to track potential drag
    document.addEventListener("pointerdown", function(event) {
        // Skip synthetic events we dispatched ourselves (for pointer state reset)
        if (event.__tiddlyDesktopSynthetic) {
            log('[pointerdown] Skipping synthetic event');
            return;
        }

        // If pointerdown fires naturally, WebKitGTK is working again
        // Reset the fallback flag
        if (webkitPointerBroken) {
            log('[pointerdown] WebKitGTK pointer events recovered - disabling fallback');
            webkitPointerBroken = false;
        }

        // Debug: log state at pointerdown
        var targetTag = event.target && event.target.tagName ? event.target.tagName : 'unknown';
        log('[pointerdown] button=' + event.button + ', capturedPointerId=' + capturedPointerId +
            ', nativeDragFromSelf=' + nativeDragFromSelf + ', internalDragActive=' + internalDragActive +
            ', pointerDragStarted=' + pointerDragStarted + ', target=' + targetTag);

        // Only handle primary button (left click / touch / pen tip)
        if (event.button !== 0) return;

        // Safety: if capturedPointerId is set but no drag is active, we have stale state
        // This can happen if lostpointercapture didn't fire or we missed it
        if (capturedPointerId !== null) {
            if (!internalDragActive && !nativeDragFromSelf && !pointerDragStarted) {
                log('[pointerdown] Detected stale capturedPointerId=' + capturedPointerId + ' - resetting');
                var stalePointerId = capturedPointerId;
                // Try to release capture from any element that might have it
                [savedDragSource, internalDragSource, pointerDownTarget, document.body].forEach(function(el) {
                    if (el) {
                        try {
                            el.releasePointerCapture(stalePointerId);
                        } catch (e) {}
                    }
                });
                // Reset all stale state
                capturedPointerId = null;
                savedDragSource = null;
                nativeDragData = null;
                savedDragInProgress = null;
                internalDragSource = null;
                pointerDownTarget = null;
                pointerDownPos = null;
            } else {
                // Legitimate block - there's an active drag
                log('[pointerdown] BLOCKED: capturedPointerId is not null: ' + capturedPointerId);
                return;
            }
        }

        // Capture any existing text selection (for potential text selection drag)
        capturedSelection = getSelectedText();
        capturedSelectionHtml = getSelectedHtml();

        var target = findDraggableAncestor(event.target);
        if (target) {
            pointerDownTarget = target;
            pointerDownPos = { x: event.clientX, y: event.clientY };
            pointerDragStarted = false;
            capturedPointerId = event.pointerId;
        } else if (capturedSelection) {
            // For potential text selection drags, track pointerdown position
            pointerDownTarget = null;
            pointerDownPos = { x: event.clientX, y: event.clientY };
            pointerDragStarted = false;
            capturedPointerId = event.pointerId;
        }
    }, true);

    // Pointermove to start drag and track position
    document.addEventListener("pointermove", function(event) {
        // Debug: log all pointermove events when in native drag mode
        if (nativeDragFromSelf) {
            log('[pointermove] nativeDragFromSelf=true, x=' + event.clientX + ', y=' + event.clientY +
                ', capturedPointerId=' + capturedPointerId + ', event.pointerId=' + event.pointerId +
                ', pointerDownPos=' + !!pointerDownPos + ', internalDragActive=' + internalDragActive +
                ', pollingActive=' + pollingActive);
        }

        // If GDK polling is handling the drag, don't interfere
        // Polling provides accurate tracking when pointer is outside window with button held
        if (pollingActive && nativeDragFromSelf) {
            return;
        }

        // Only handle the pointer we're tracking
        if (capturedPointerId !== null && event.pointerId !== capturedPointerId) return;

        if (!pointerDownPos) return;

        // Check for re-entry from native drag via pointer capture
        // This handles the case where GTK drag-end fired immediately (no valid GDK event)
        // and we're tracking pointer position via capture instead
        // NOTE: This is a fallback - GDK polling is the primary mechanism
        if (nativeDragFromSelf && !internalDragActive && !pollingActive) {
            var insideWindow = event.clientX >= 0 && event.clientY >= 0 &&
                              event.clientX <= window.innerWidth &&
                              event.clientY <= window.innerHeight;
            if (insideWindow) {
                log('[TiddlyDesktop] Re-entry detected via pointer capture at ' + event.clientX + ', ' + event.clientY);

                // Release pointer capture - it can prevent focus on other elements
                // We've detected re-entry, so we don't need capture anymore
                if (capturedPointerId !== null) {
                    var captureElement = savedDragSource || internalDragSource;
                    if (captureElement) {
                        try {
                            captureElement.releasePointerCapture(capturedPointerId);
                            log('[TiddlyDesktop] Re-entry via capture: released pointer capture');
                        } catch (e) {}
                    }
                    capturedPointerId = null;
                }

                // Focus the window so it can receive events properly
                if (window.__TAURI__ && window.__TAURI__.window) {
                    window.__TAURI__.window.getCurrentWindow().setFocus().catch(function(err) {
                        log('[TiddlyDesktop] Failed to focus window on re-entry: ' + err);
                    });
                }
                // Also call browser's window.focus() for immediate effect
                window.focus();

                // Restore the drag data if we still have it
                if (nativeDragData) {
                    window.__tiddlyDesktopDragData = nativeDragData;
                }

                // Restore drag source element
                if (savedDragSource) {
                    internalDragSource = savedDragSource;
                }

                // Restore the saved drag image if it's not currently in the DOM
                // First, clean up any existing drag image to prevent duplicates
                log('[TiddlyDesktop] Re-entry via capture: savedDragImage=' + !!savedDragImage +
                    ', inDOM=' + !!(savedDragImage && savedDragImage.parentNode) +
                    ', internalDragImage=' + !!internalDragImage +
                    ', internalInDOM=' + !!(internalDragImage && internalDragImage.parentNode));

                // Remove any existing internalDragImage that's different from savedDragImage
                if (internalDragImage && internalDragImage !== savedDragImage && internalDragImage.parentNode) {
                    log('[TiddlyDesktop] Re-entry via capture: removing old internalDragImage from DOM');
                    internalDragImage.parentNode.removeChild(internalDragImage);
                    internalDragImage = null;
                }

                if (savedDragImage && !savedDragImage.parentNode) {
                    document.body.appendChild(savedDragImage);
                    internalDragImage = savedDragImage;
                    savedDragImage = null;  // Clear to prevent duplicates
                    log('[TiddlyDesktop] Re-entry via capture: drag image restored to DOM');
                } else if (savedDragImage && savedDragImage.parentNode) {
                    // Element is already in DOM (e.g., during async capture) - just use it
                    internalDragImage = savedDragImage;
                    savedDragImage = null;
                    log('[TiddlyDesktop] Re-entry via capture: drag image already in DOM, using it');
                } else if (!savedDragImage) {
                    log('[TiddlyDesktop] Re-entry via capture: WARNING - no savedDragImage to restore!');
                }

                // Create a DataTransfer with our data for synthetic events
                pointerDragDataTransfer = new DataTransfer();
                var dataToUse = nativeDragData || window.__tiddlyDesktopDragData;
                if (dataToUse) {
                    for (var type in dataToUse) {
                        if (dataToUse[type]) {
                            try {
                                pointerDragDataTransfer.setData(type, dataToUse[type]);
                            } catch (e) {}
                        }
                    }
                }

                // Update drag image position
                if (internalDragImage) {
                    internalDragImage.style.left = (event.clientX - dragImageOffsetX) + "px";
                    internalDragImage.style.top = (event.clientY - dragImageOffsetY) + "px";
                }

                // Restore TiddlyWiki's drag state
                var dragElement = null;
                if (typeof $tw !== "undefined") {
                    dragElement = savedDragInProgress || savedDragSource || internalDragSource;
                    if (dragElement) {
                        $tw.dragInProgress = dragElement;
                        $tw.utils.addClass(dragElement, "tc-dragging");
                        log('[TiddlyDesktop] Re-entry via capture: restored $tw.dragInProgress');
                    }
                }

                // Note: We don't dispatch dragstart here - the drag was already started before leaving.
                // We just need to continue with dragenter/dragover to activate drop zones.

                // Dispatch dragenter and dragover to the element under cursor
                var elementInfo = getElementAtPoint(event.clientX, event.clientY);
                if (elementInfo.target) {
                    var enterEvent = createSyntheticDragEvent("dragenter", {
                        clientX: event.clientX,
                        clientY: event.clientY,
                        relatedTarget: null
                    }, pointerDragDataTransfer);
                    elementInfo.target.dispatchEvent(enterEvent);
                    lastDragOverTarget = elementInfo.target;

                    var overEvent = createSyntheticDragEvent("dragover", {
                        clientX: event.clientX,
                        clientY: event.clientY
                    }, pointerDragDataTransfer);
                    elementInfo.target.dispatchEvent(overEvent);
                }

                // Mark as active again
                internalDragActive = true;
                pointerDragStarted = true;
                nativeDragStarting = false;

                // Don't return - continue with normal drag tracking
            } else {
                // Still outside window, just update tracking
                return;
            }
        }

        var dx = event.clientX - pointerDownPos.x;
        var dy = event.clientY - pointerDownPos.y;

        // If drag hasn't started yet, check threshold
        if (!pointerDragStarted) {
            if (Math.abs(dx) < DRAG_THRESHOLD && Math.abs(dy) < DRAG_THRESHOLD) return;

            // Check if this is a draggable element or text selection
            var selectedText = capturedSelection || getSelectedText();
            if (!pointerDownTarget && !selectedText) {
                // No draggable target and no text selection
                pointerDownPos = null;
                capturedPointerId = null;
                return;
            }

            // Start the drag
            pointerDragStarted = true;
            internalDragActive = true;
            internalDragSource = pointerDownTarget || event.target;

            // Set pointer capture to receive all pointer events
            try {
                internalDragSource.setPointerCapture(event.pointerId);
                log('[TiddlyDesktop] Pointer capture set on: ' + internalDragSource.tagName);

                // Add lostpointercapture handler to this element
                // This is CRITICAL - lostpointercapture fires on the element, not document
                // Capture the element in a local variable since internalDragSource may be nulled
                // before lostpointercapture fires (it can fire async after releasePointerCapture)
                var captureElement = internalDragSource;
                var lostCaptureHandler = function(lostEvent) {
                    log('[TiddlyDesktop] lostpointercapture on element: pointerId=' + lostEvent.pointerId +
                        ', capturedPointerId=' + capturedPointerId);
                    if (capturedPointerId === lostEvent.pointerId) {
                        capturedPointerId = null;
                    }
                    // Remove this handler after it fires
                    captureElement.removeEventListener('lostpointercapture', lostCaptureHandler);
                };
                captureElement.addEventListener('lostpointercapture', lostCaptureHandler);
            } catch (e) {
                log('[TiddlyDesktop] Failed to set pointer capture: ' + e);
            }

            document.body.style.userSelect = "none";
            document.body.style.webkitUserSelect = "none";

            window.__tiddlyDesktopDragData = {};
            dragImageIsBlank = false;
            pointerDragDataTransfer = new DataTransfer();

            var originalSetDragImage = pointerDragDataTransfer.setDragImage;
            pointerDragDataTransfer.setDragImage = function(element, x, y) {
                if (element && (!element.firstChild || element.offsetWidth === 0 || element.offsetHeight === 0)) {
                    dragImageIsBlank = true;
                }
                if (originalSetDragImage) {
                    originalSetDragImage.call(this, element, x, y);
                }
            };

            // For text selection drags, pre-populate the dataTransfer
            if (!pointerDownTarget && selectedText) {
                var htmlContent = capturedSelectionHtml || getSelectedHtml();
                pointerDragDataTransfer.setData("text/plain", selectedText);
                window.__tiddlyDesktopDragData["text/plain"] = selectedText;
                if (htmlContent) {
                    pointerDragDataTransfer.setData("text/html", htmlContent);
                    window.__tiddlyDesktopDragData["text/html"] = htmlContent;
                }
            }

            var dragStartEvent = createSyntheticDragEvent("dragstart", {
                clientX: pointerDownPos.x,
                clientY: pointerDownPos.y
            }, pointerDragDataTransfer);

            internalDragSource.dispatchEvent(dragStartEvent);

            var isTextDrag = !pointerDownTarget && selectedText;
            window.__tiddlyDesktopEffectAllowed = pointerDragDataTransfer.effectAllowed || "all";

            log('[TiddlyDesktop] Drag threshold exceeded - creating drag image for: ' +
                (internalDragSource ? internalDragSource.tagName : 'null'));
            createDragImage(internalDragSource, event.clientX, event.clientY, isTextDrag ? selectedText : null);
            log('[TiddlyDesktop] Drag image created: ' + !!internalDragImage);

            // Prepare native drag in case pointer leaves window
            // Do this AFTER dragstart event so TiddlyWiki has set the data
            prepareNativeDrag();

            var elementInfo = getElementAtPoint(event.clientX, event.clientY);
            if (elementInfo.target) {
                var enterEvent = createSyntheticDragEvent("dragenter", {
                    clientX: event.clientX,
                    clientY: event.clientY,
                    relatedTarget: null
                }, pointerDragDataTransfer);
                elementInfo.target.dispatchEvent(enterEvent);
                lastDragOverTarget = elementInfo.target;
            }
            return;
        }

        // Drag is in progress - update position and target
        if (!internalDragSource) return;

        // Check if pointer has left the window bounds - start native drag
        var outsideWindow = event.clientX < 0 || event.clientY < 0 ||
                           event.clientX > window.innerWidth ||
                           event.clientY > window.innerHeight;
        if (outsideWindow) {
            log('[TiddlyDesktop] Pointer left window bounds at ' + event.clientX + ', ' + event.clientY +
                ', nativeDragFromSelf=' + nativeDragFromSelf +
                ', nativeDragStarting=' + nativeDragStarting +
                ', internalDragActive=' + internalDragActive);
            startNativeDrag(event.clientX, event.clientY);
            return;
        }

        updateDragImagePosition(event.clientX, event.clientY);

        var elementInfo = getElementAtPoint(event.clientX, event.clientY);
        var target = elementInfo.target;
        if (!target) return;

        if (lastDragOverTarget && lastDragOverTarget !== target) {
            var leaveEvent = createSyntheticDragEvent("dragleave", {
                clientX: event.clientX,
                clientY: event.clientY,
                relatedTarget: target
            }, pointerDragDataTransfer);
            lastDragOverTarget.dispatchEvent(leaveEvent);

            var enterEvent = createSyntheticDragEvent("dragenter", {
                clientX: event.clientX,
                clientY: event.clientY,
                relatedTarget: lastDragOverTarget
            }, pointerDragDataTransfer);
            target.dispatchEvent(enterEvent);
        }
        lastDragOverTarget = target;

        // Update caret position for text inputs and contenteditable during drag
        if (isTextInput(target)) {
            setInputCaretFromPoint(target, elementInfo.adjustedX, elementInfo.adjustedY);
        } else if (isContentEditable(target)) {
            setContentEditableCaretFromPoint(target, elementInfo.adjustedX, elementInfo.adjustedY);
        }

        var overEvent = createSyntheticDragEvent("dragover", {
            clientX: event.clientX,
            clientY: event.clientY
        }, pointerDragDataTransfer);
        target.dispatchEvent(overEvent);
    }, true);

    // Pointerup to complete drop
    document.addEventListener("pointerup", function(event) {
        // Ignore synthetic events we dispatched ourselves
        if (event.__tiddlyDesktopSynthetic) return;

        // Debug: log all pointerup events when relevant
        if (nativeDragFromSelf || pointerDragStarted) {
            log('[pointerup] x=' + event.clientX + ', y=' + event.clientY +
                ', nativeDragFromSelf=' + nativeDragFromSelf +
                ', pointerDragStarted=' + pointerDragStarted +
                ', capturedPointerId=' + capturedPointerId + ', event.pointerId=' + event.pointerId);
        }

        // Only handle the pointer we're tracking
        if (capturedPointerId !== null && event.pointerId !== capturedPointerId) return;

        if (pointerDragStarted && internalDragSource) {
            // Release pointer capture
            try {
                internalDragSource.releasePointerCapture(event.pointerId);
            } catch (e) {}

            // Get the actual drop target (may be inside an iframe)
            var elementInfo = getElementAtPoint(event.clientX, event.clientY);
            var target = elementInfo.target;

            if (lastDragOverTarget) {
                var leaveEvent = createSyntheticDragEvent("dragleave", {
                    clientX: event.clientX,
                    clientY: event.clientY,
                    relatedTarget: null
                }, pointerDragDataTransfer);
                lastDragOverTarget.dispatchEvent(leaveEvent);
            }

            if (target) {
                var dropDt = new DataTransfer();
                // Copy captured data to drop dataTransfer
                if (window.__tiddlyDesktopDragData) {
                    for (var type in window.__tiddlyDesktopDragData) {
                        try {
                            dropDt.setData(type, window.__tiddlyDesktopDragData[type]);
                        } catch(e) {}
                    }
                }

                // Special handling for text inputs and textareas
                var textData = window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData['text/plain'];
                var htmlData = window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData['text/html'];

                if (isTextInput(target)) {
                    // Insert text at the position determined by pointer coordinates
                    insertTextAtPoint(target, elementInfo.adjustedX, elementInfo.adjustedY, textData);
                } else if (isContentEditable(target)) {
                    // Insert into contenteditable at the position determined by pointer coordinates
                    insertIntoContentEditableAtPoint(target, elementInfo.adjustedX, elementInfo.adjustedY, textData, htmlData);
                } else {
                    // Standard drop event for other elements
                    var dropEvent = createSyntheticDragEvent("drop", {
                        clientX: event.clientX,
                        clientY: event.clientY
                    }, dropDt);
                    target.dispatchEvent(dropEvent);
                }
            }

            var endEvent = createSyntheticDragEvent("dragend", {
                clientX: event.clientX,
                clientY: event.clientY
            }, pointerDragDataTransfer);
            internalDragSource.dispatchEvent(endEvent);

            // Clean up native drag preparation - drag is complete
            cleanupNativeDrag();

            // Reset all drag state (including native drag from self state)
            window.__tiddlyDesktopDragData = null;
            window.__tiddlyDesktopEffectAllowed = null;
            internalDragSource = null;
            internalDragActive = false;
            lastDragOverTarget = null;
            pointerDragDataTransfer = null;
            nativeDragFromSelf = false;
            nativeDragData = null;
            savedDragInProgress = null;
            savedDragSource = null;
            pollingActive = false;
            // Destroy saved drag image
            if (savedDragImage && savedDragImage.parentNode) {
                savedDragImage.parentNode.removeChild(savedDragImage);
            }
            savedDragImage = null;
            removeDragImage();
            document.body.style.userSelect = "";
            document.body.style.webkitUserSelect = "";
        } else if (nativeDragFromSelf && !internalDragActive) {
            // User released mouse button while in native drag mode AND we haven't re-entered
            // This means the drop happened outside the window
            // If internalDragActive is true, we've re-entered and td-pointer-up will handle the drop
            log('[TiddlyDesktop] pointerup during nativeDragFromSelf (outside window) - cleaning up');

            // Release pointer capture if we still have it
            if (savedDragSource) {
                try {
                    savedDragSource.releasePointerCapture(event.pointerId);
                } catch (e) {}
            } else if (internalDragSource) {
                try {
                    internalDragSource.releasePointerCapture(event.pointerId);
                } catch (e) {}
            }

            // Clean up native drag on Rust side
            cleanupNativeDrag();

            // Reset all state
            window.__tiddlyDesktopDragData = null;
            window.__tiddlyDesktopEffectAllowed = null;
            internalDragSource = null;
            internalDragActive = false;
            lastDragOverTarget = null;
            pointerDragDataTransfer = null;
            nativeDragFromSelf = false;
            nativeDragData = null;
            savedDragInProgress = null;
            savedDragSource = null;
            pollingActive = false;
            // Destroy saved drag image
            if (savedDragImage && savedDragImage.parentNode) {
                savedDragImage.parentNode.removeChild(savedDragImage);
            }
            savedDragImage = null;
            removeDragImage();
            document.body.style.userSelect = "";
            document.body.style.webkitUserSelect = "";
        }

        pointerDownTarget = null;
        pointerDownPos = null;
        pointerDragStarted = false;
        capturedSelection = null;
        capturedSelectionHtml = null;
        capturedPointerId = null;
        nativeDragStarting = false;
    }, true);

    // Handle pointer cancel (e.g., system gesture, palm rejection)
    document.addEventListener("pointercancel", function(event) {
        if (capturedPointerId !== null && event.pointerId === capturedPointerId) {
            // Don't cancel if we're transitioning to native drag
            // (releasing pointer capture triggers pointercancel)
            if (nativeDragStarting || nativeDragFromSelf) {
                log('[TiddlyDesktop] Ignoring pointercancel during native drag transition');
                return;
            }
            cancelDrag("pointer cancelled");
        }
    }, true);

    // Note: pointerleave doesn't fire when pointer capture is active
    // We detect window boundary crossing in pointermove instead
    // Note: lostpointercapture doesn't bubble, so we add element-level handlers
    // when setting pointer capture (see pointermove handler)

    // Prepare for potential native drag (called when internal drag starts)
    function prepareNativeDrag() {
        if (!window.__tiddlyDesktopDragData) {
            return;
        }

        // Get the tiddler JSON data
        var tiddlerJson = window.__tiddlyDesktopDragData['text/vnd.tiddler'] || null;

        // TiddlyWiki uses data URIs for text/x-moz-url and URL types
        // Format: data:text/vnd.tiddler,<url-encoded-json>
        var tiddlerDataUri = null;
        if (tiddlerJson) {
            tiddlerDataUri = 'data:text/vnd.tiddler,' + encodeURIComponent(tiddlerJson);
        }

        // Get text/x-moz-url if set, otherwise generate from tiddler data
        var mozUrl = window.__tiddlyDesktopDragData['text/x-moz-url'] || tiddlerDataUri;
        // Get URL if set, otherwise generate from tiddler data
        var url = window.__tiddlyDesktopDragData['URL'] || tiddlerDataUri;

        var data = {
            text_plain: window.__tiddlyDesktopDragData['text/plain'] || null,
            text_html: window.__tiddlyDesktopDragData['text/html'] || null,
            text_vnd_tiddler: tiddlerJson,
            text_uri_list: window.__tiddlyDesktopDragData['text/uri-list'] || null,
            text_x_moz_url: mozUrl,
            url: url
        };

        log('[TiddlyDesktop] Preparing native drag with data types: ' + Object.keys(data).filter(function(k) { return data[k] !== null; }).join(', '));

        if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
            window.__TAURI__.core.invoke('prepare_native_drag', { data: data })
                .then(function() {
                    log('[TiddlyDesktop] Native drag prepared');
                })
                .catch(function(err) {
                    console.error('[TiddlyDesktop] Failed to prepare native drag:', err);
                });
        }
    }

    // Clean up native drag preparation (called when internal drag ends normally)
    function cleanupNativeDrag() {
        if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
            window.__TAURI__.core.invoke('cleanup_native_drag')
                .catch(function(err) {
                    console.error('[TiddlyDesktop] Failed to cleanup native drag:', err);
                });
        }
    }

    // Capture drag image element as PNG bytes
    function captureDragImageAsPng(callback) {
        if (!internalDragImage) {
            callback(null);
            return;
        }

        try {
            // Get the element's dimensions and styles
            var rect = internalDragImage.getBoundingClientRect();
            var style = window.getComputedStyle(internalDragImage);
            var width = Math.ceil(rect.width);
            var height = Math.ceil(rect.height);

            if (width <= 0 || height <= 0) {
                callback(null);
                return;
            }

            // Create a canvas - use 1:1 for Linux, DPR scaling for macOS/Windows
            // Platform-specific handling is done in Rust
            var canvas = document.createElement('canvas');
            var dpr = window.devicePixelRatio || 1;
            // For now, use 1:1 on all platforms (Linux is the only one implemented)
            // TODO: When macOS/Windows are implemented, they may need DPR-scaled images
            var scale = 1; // Could be: navigator.platform.includes('Mac') ? dpr : 1
            canvas.width = width * scale;
            canvas.height = height * scale;
            var ctx = canvas.getContext('2d');
            if (scale !== 1) {
                ctx.scale(scale, scale);
            }

            // Draw background
            var bgColor = style.backgroundColor || 'white';
            if (bgColor === 'transparent' || bgColor === 'rgba(0, 0, 0, 0)') {
                bgColor = 'white';
            }
            ctx.fillStyle = bgColor;

            // Draw rounded rectangle if border-radius is set
            var borderRadius = parseFloat(style.borderRadius) || 0;
            if (borderRadius > 0) {
                ctx.beginPath();
                ctx.roundRect(0, 0, width, height, borderRadius);
                ctx.fill();
            } else {
                ctx.fillRect(0, 0, width, height);
            }

            // Draw text
            var text = internalDragImage.textContent || '';
            if (text) {
                ctx.fillStyle = style.color || '#333';
                ctx.font = style.font || '13px sans-serif';
                ctx.textBaseline = 'middle';
                var paddingLeft = parseFloat(style.paddingLeft) || 6;
                var paddingTop = parseFloat(style.paddingTop) || 6;
                ctx.fillText(text, paddingLeft, height / 2);
            }

            // Convert to PNG blob
            canvas.toBlob(function(blob) {
                if (blob) {
                    var reader = new FileReader();
                    reader.onload = function() {
                        // Convert ArrayBuffer to Uint8Array
                        var arrayBuffer = reader.result;
                        var bytes = new Uint8Array(arrayBuffer);
                        callback(Array.from(bytes)); // Convert to regular array for JSON serialization
                    };
                    reader.onerror = function() {
                        console.error('[TiddlyDesktop] Failed to read drag image blob');
                        callback(null);
                    };
                    reader.readAsArrayBuffer(blob);
                } else {
                    callback(null);
                }
            }, 'image/png');
        } catch (e) {
            console.error('[TiddlyDesktop] Failed to capture drag image:', e);
            callback(null);
        }
    }

    // Start a native OS drag via Tauri command
    function startNativeDrag(x, y) {
        // Prevent multiple calls while async operation is in progress
        if (nativeDragStarting) {
            return;
        }

        if (!window.__tiddlyDesktopDragData) {
            cancelDrag("no drag data for native drag");
            return;
        }

        // Set flag immediately to prevent re-entry
        nativeDragStarting = true;

        // Get the tiddler JSON data
        var tiddlerJson = window.__tiddlyDesktopDragData['text/vnd.tiddler'] || null;

        // TiddlyWiki uses data URIs for text/x-moz-url and URL types
        // Format: data:text/vnd.tiddler,<url-encoded-json>
        var tiddlerDataUri = null;
        if (tiddlerJson) {
            tiddlerDataUri = 'data:text/vnd.tiddler,' + encodeURIComponent(tiddlerJson);
        }

        // Get text/x-moz-url if set, otherwise generate from tiddler data
        var mozUrl = window.__tiddlyDesktopDragData['text/x-moz-url'] || tiddlerDataUri;
        // Get URL if set, otherwise generate from tiddler data
        var url = window.__tiddlyDesktopDragData['URL'] || tiddlerDataUri;

        var data = {
            text_plain: window.__tiddlyDesktopDragData['text/plain'] || null,
            text_html: window.__tiddlyDesktopDragData['text/html'] || null,
            text_vnd_tiddler: tiddlerJson,
            text_uri_list: window.__tiddlyDesktopDragData['text/uri-list'] || null,
            text_x_moz_url: mozUrl,
            url: url
        };

        // DON'T release pointer capture - we want to keep receiving pointermove events
        // so we can detect when the pointer re-enters the window
        // The pointer capture allows us to track the pointer even outside the window

        // Save state for potential re-entry
        nativeDragFromSelf = true;
        nativeDragData = window.__tiddlyDesktopDragData;
        savedDragInProgress = (typeof $tw !== "undefined") ? $tw.dragInProgress : null;
        savedDragSource = internalDragSource;

        log('startNativeDrag: Saved state for re-entry:' +
            ' nativeDragFromSelf=' + nativeDragFromSelf +
            ', hasDragData=' + !!nativeDragData +
            ', hasSavedDragInProgress=' + !!savedDragInProgress +
            ', hasSavedDragSource=' + !!savedDragSource +
            ', hasInternalDragImage=' + !!internalDragImage);

        // Mark that we're transitioning to native drag BEFORE async operations
        // This prevents pointerup handler from calling cleanup
        internalDragActive = false;
        pointerDragStarted = false;

        // Dispatch dragleave to current target before leaving
        if (lastDragOverTarget && pointerDragDataTransfer) {
            var leaveEvent = createSyntheticDragEvent("dragleave", {
                relatedTarget: null
            }, pointerDragDataTransfer);
            lastDragOverTarget.dispatchEvent(leaveEvent);
            lastDragOverTarget = null;
        }

        // NOTE: Do NOT dispatch dragend here - TiddlyWiki should still think the drag is active
        // This preserves the original position so Escape can cancel properly after re-entry

        // Capture the drag image FIRST while it's still in the DOM
        log('[TiddlyDesktop] About to capture drag image, internalDragImage=' + !!internalDragImage);

        captureDragImageAsPng(function(imageData) {
            log('[TiddlyDesktop] captureDragImageAsPng callback called, imageData=' + (imageData ? imageData.length + ' bytes' : 'null'));

            // Now save and detach the drag image for re-entry
            // Do this INSIDE the callback so it happens after capture completes
            if (internalDragImage && internalDragImage.parentNode) {
                // Clean up any existing savedDragImage first to prevent orphans
                if (savedDragImage && savedDragImage !== internalDragImage && savedDragImage.parentNode) {
                    savedDragImage.parentNode.removeChild(savedDragImage);
                }
                savedDragImage = internalDragImage;
                savedDragImage.parentNode.removeChild(savedDragImage);
                log('[TiddlyDesktop] Drag image detached from DOM (after capture)');
            }
            internalDragImage = null;

            // Call Tauri command to start native drag
            if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
                var params = {
                    data: data,
                    x: Math.round(x),
                    y: Math.round(y),
                    imageData: imageData, // May be null if capture failed
                    // Pass drag image offset so GTK can position its icon identically to JS
                    imageOffsetX: Math.round(dragImageOffsetX),
                    imageOffsetY: Math.round(dragImageOffsetY)
                };
                log('[TiddlyDesktop] Starting native drag with image=' + (imageData ? imageData.length + ' bytes' : 'none') +
                    ', offset=(' + dragImageOffsetX + ', ' + dragImageOffsetY + ')');

                window.__TAURI__.core.invoke('start_native_drag', params)
                    .then(function() {
                        log('[TiddlyDesktop] Native drag started, tracking for re-entry');
                    })
                    .catch(function(err) {
                        console.error('[TiddlyDesktop] Failed to start native drag:', err);
                        // Native drag failed - clean up completely
                        nativeDragFromSelf = false;
                        nativeDragData = null;
                        nativeDragText = null;
                        cleanupDragState();
                    });
            } else {
                console.warn('[TiddlyDesktop] Tauri invoke not available for native drag');
                nativeDragFromSelf = false;
                nativeDragData = null;
                nativeDragText = null;
                cleanupDragState();
            }
        });
    }

    // Clean up drag state without dispatching events
    function cleanupDragState() {
        window.__tiddlyDesktopDragData = null;
        window.__tiddlyDesktopEffectAllowed = null;
        internalDragSource = null;
        internalDragActive = false;
        pointerDownTarget = null;
        pointerDownPos = null;
        pointerDragStarted = false;
        nativeDragFromSelf = false;
        nativeDragData = null;
        savedDragInProgress = null;
        savedDragSource = null;
        pollingActive = false;
        // Destroy saved drag image
        if (savedDragImage && savedDragImage.parentNode) {
            savedDragImage.parentNode.removeChild(savedDragImage);
        }
        savedDragImage = null;
        pointerDragDataTransfer = null;
        lastDragOverTarget = null;
        capturedSelection = null;
        capturedSelectionHtml = null;
        capturedPointerId = null;
        nativeDragStarting = false;
        removeDragImage();
        document.body.style.userSelect = "";
        document.body.style.webkitUserSelect = "";
    }

    // Handle escape to cancel drag
    // Use bubbling mode (no capture) so TiddlyWiki's handlers can run first
    document.addEventListener("keydown", function(event) {
        if (event.key === "Escape") {
            // Set flag to prevent td-pointer-up from triggering a drop
            dragCancelledByEscape = true;

            // If we're in a re-entry scenario, don't dispatch synthetic events
            // Just clean up our state and let TiddlyWiki handle it
            if (nativeDragFromSelf && internalDragActive) {
                log('[TiddlyDesktop] Escape during re-entry - cleaning up without dispatching events');
                // Just clean up our internal state without triggering TiddlyWiki's droppables
                if (lastDragOverTarget) {
                    lastDragOverTarget = null;
                }
                // Remove visual feedback classes
                document.querySelectorAll(".tc-dragover").forEach(function(el) {
                    el.classList.remove("tc-dragover");
                });
                document.querySelectorAll(".tc-dragging").forEach(function(el) {
                    el.classList.remove("tc-dragging");
                });
                // Clear TiddlyWiki state
                if (typeof $tw !== "undefined") {
                    $tw.dragInProgress = null;
                }
                // Clean up native drag state on Rust side
                cleanupNativeDrag();
                // Reset all our state
                window.__tiddlyDesktopDragData = null;
                window.__tiddlyDesktopEffectAllowed = null;
                internalDragSource = null;
                internalDragActive = false;
                pointerDownTarget = null;
                pointerDownPos = null;
                pointerDragStarted = false;
                pointerDragDataTransfer = null;
                lastDragOverTarget = null;
                capturedSelection = null;
                capturedSelectionHtml = null;
                capturedPointerId = null;
                nativeDragStarting = false;
                nativeDragFromSelf = false;
                nativeDragData = null;
                savedDragInProgress = null;
                savedDragSource = null;
                pollingActive = false;
                if (savedDragImage && savedDragImage.parentNode) {
                    savedDragImage.parentNode.removeChild(savedDragImage);
                }
                savedDragImage = null;
                removeDragImage();
                document.body.style.userSelect = "";
                document.body.style.webkitUserSelect = "";
            } else {
                cancelDrag("escape pressed");
            }
        }
    }, true);

    // Handle window blur
    window.addEventListener("blur", function(event) {
        // Don't cancel if transitioning to or in native drag mode
        if (nativeDragStarting || nativeDragFromSelf) {
            return;
        }
        if (internalDragActive || pointerDragStarted) {
            setTimeout(function() {
                // Re-check native drag state in case it changed
                if (nativeDragStarting || nativeDragFromSelf) {
                    return;
                }
                if ((internalDragActive || pointerDragStarted) && !document.hasFocus()) {
                    cancelDrag("window lost focus");
                }
            }, 100);
        }
    }, true);

    // Set up Tauri event listeners for native drag events from Rust
    // These events are emitted by the Rust drag_drop module
    // Use window-level guard to prevent double registration even if script is loaded twice
    function setupTauriEventListeners() {
        if (window.__tiddlyDesktopInternalDragListenersSetUp) {
            log('Tauri event listeners already set up (window guard), skipping');
            return;
        }

        setupTauriListen();
        if (!tauriListen) {
            // Tauri not available yet, retry later
            setTimeout(setupTauriEventListeners, 100);
            return;
        }

        window.__tiddlyDesktopInternalDragListenersSetUp = true;
        log('Setting up Tauri event listeners for native drag events');

        // Handle native drag re-entering window (td-drag-motion from Rust)
        // This fires when a native drag (including one we started) enters our window
        tauriListen("td-drag-motion", function(event) {
            try {
            var payload = event.payload || {};
            // Check if Rust says this is our drag (has outgoing data stored)
            var isOurDragFromRust = payload.isOurDrag;
            // Check if this event came from GDK pointer polling (not GTK drag signals)
            var fromPolling = payload.fromPolling || false;

            var eventWindowLabel = payload.windowLabel || '';

            log('td-drag-motion received: x=' + payload.x + ', y=' + payload.y +
                ', isOurDragFromRust=' + isOurDragFromRust +
                ', nativeDragFromSelf=' + nativeDragFromSelf +
                ', internalDragActive=' + internalDragActive +
                ', fromPolling=' + fromPolling +
                ', eventWindowLabel=' + eventWindowLabel +
                ', currentWindowLabel=' + currentWindowLabel);

            // Only process polling events if they're for THIS window
            // This ensures re-entry only works for the window where the drag originated
            if (fromPolling && eventWindowLabel && eventWindowLabel !== currentWindowLabel) {
                log('Ignoring polling event for different window: ' + eventWindowLabel);
                return;
            }

            // If this came from polling, mark that polling is active
            // This tells pointermove to not interfere
            if (fromPolling && isOurDragFromRust) {
                pollingActive = true;
            }

            // Check if this is our drag coming back (either JS knows, or Rust tells us)
            // GTK may fire drag-end immediately, clearing nativeDragFromSelf, but Rust still knows
            var isOurDragReentry = (nativeDragFromSelf || isOurDragFromRust) && !internalDragActive;

            if (isOurDragReentry) {
                log('[TiddlyDesktop] Native drag re-entered window (nativeDragFromSelf=' + nativeDragFromSelf + ', isOurDragFromRust=' + isOurDragFromRust + ')');

                // Restore nativeDragFromSelf if Rust says it's our drag
                if (isOurDragFromRust && !nativeDragFromSelf) {
                    log('[TiddlyDesktop] Re-enabling nativeDragFromSelf based on Rust flag');
                    nativeDragFromSelf = true;
                }

                // Release pointer capture - it can prevent focus on other elements
                // During re-entry, Rust is polling the pointer so we don't need browser capture
                if (capturedPointerId !== null) {
                    var captureElement = savedDragSource || internalDragSource;
                    if (captureElement) {
                        try {
                            captureElement.releasePointerCapture(capturedPointerId);
                            log('[TiddlyDesktop] Re-entry: released pointer capture');
                        } catch (e) {
                            log('[TiddlyDesktop] Re-entry: failed to release pointer capture: ' + e);
                        }
                    }
                    capturedPointerId = null;
                }

                // Focus the window so it can receive events properly
                if (window.__TAURI__ && window.__TAURI__.window) {
                    window.__TAURI__.window.getCurrentWindow().setFocus().catch(function(err) {
                        log('[TiddlyDesktop] Failed to focus window on re-entry: ' + err);
                    });
                }
                // Also call browser's window.focus() for immediate effect
                window.focus();

                // Restore the drag data if we still have it
                if (nativeDragData) {
                    window.__tiddlyDesktopDragData = nativeDragData;
                }

                // Restore drag source element
                if (savedDragSource) {
                    internalDragSource = savedDragSource;
                }

                // Restore the saved drag image if it's not currently in the DOM
                // First, clean up any existing drag image to prevent duplicates
                log('[TiddlyDesktop] Re-entry drag image state: savedDragImage=' + !!savedDragImage +
                    ', inDOM=' + !!(savedDragImage && savedDragImage.parentNode) +
                    ', internalDragImage=' + !!internalDragImage +
                    ', internalInDOM=' + !!(internalDragImage && internalDragImage.parentNode));

                // Remove any existing internalDragImage that's different from savedDragImage
                if (internalDragImage && internalDragImage !== savedDragImage && internalDragImage.parentNode) {
                    log('[TiddlyDesktop] Re-entry: removing old internalDragImage from DOM');
                    internalDragImage.parentNode.removeChild(internalDragImage);
                    internalDragImage = null;
                }

                if (savedDragImage && !savedDragImage.parentNode) {
                    document.body.appendChild(savedDragImage);
                    internalDragImage = savedDragImage;
                    savedDragImage = null;  // Clear to prevent duplicates
                    log('[TiddlyDesktop] Re-entry: restored drag image to DOM');
                } else if (savedDragImage && savedDragImage.parentNode) {
                    // Element is already in DOM (e.g., during async capture) - just use it
                    internalDragImage = savedDragImage;
                    savedDragImage = null;
                    log('[TiddlyDesktop] Re-entry: drag image already in DOM, using it');
                }

                // Create a DataTransfer with our data for synthetic events
                pointerDragDataTransfer = new DataTransfer();
                var dataToUse = nativeDragData || window.__tiddlyDesktopDragData;
                if (dataToUse) {
                    for (var type in dataToUse) {
                        if (dataToUse[type]) {
                            try {
                                pointerDragDataTransfer.setData(type, dataToUse[type]);
                            } catch (e) {}
                        }
                    }
                }

                // Update position from event (Windows sends physical pixels, Linux/macOS send CSS pixels)
                var coords = getPayloadCoordinates(payload);
                var x = coords.x;
                var y = coords.y;
                if (internalDragImage) {
                    internalDragImage.style.left = (x - dragImageOffsetX) + "px";
                    internalDragImage.style.top = (y - dragImageOffsetY) + "px";
                }

                // Restore TiddlyWiki's drag state
                var dragElement = null;
                if (typeof $tw !== "undefined") {
                    dragElement = savedDragInProgress || savedDragSource || internalDragSource;
                    if (dragElement) {
                        $tw.dragInProgress = dragElement;
                        $tw.utils.addClass(dragElement, "tc-dragging");
                        log('[TiddlyDesktop] Re-entry: restored $tw.dragInProgress to ' + (dragElement.tagName || 'element'));
                    }
                }

                // Note: We don't dispatch dragstart here - the drag was already started before leaving.
                // We just need to continue with dragenter/dragover to activate drop zones.

                // Dispatch dragenter and dragover to the element under cursor
                var elementInfo = getElementAtPoint(x, y);
                log('[TiddlyDesktop] Re-entry: dispatching dragenter to ' + (elementInfo.target ? elementInfo.target.tagName + '.' + elementInfo.target.className : 'null'));
                if (elementInfo.target) {
                    var enterEvent = createSyntheticDragEvent("dragenter", {
                        clientX: x,
                        clientY: y,
                        relatedTarget: null
                    }, pointerDragDataTransfer);
                    elementInfo.target.dispatchEvent(enterEvent);
                    log('[TiddlyDesktop] Re-entry: dragenter dispatched');
                    lastDragOverTarget = elementInfo.target;

                    // Also dispatch dragover - this is what TiddlyWiki listens to for drop zones
                    var overEvent = createSyntheticDragEvent("dragover", {
                        clientX: x,
                        clientY: y
                    }, pointerDragDataTransfer);
                    elementInfo.target.dispatchEvent(overEvent);

                    // Update caret position for text inputs and contenteditable
                    if (isTextInput(elementInfo.target)) {
                        setInputCaretFromPoint(elementInfo.target, elementInfo.adjustedX, elementInfo.adjustedY);
                    } else if (isContentEditable(elementInfo.target)) {
                        setContentEditableCaretFromPoint(elementInfo.target, elementInfo.adjustedX, elementInfo.adjustedY);
                    }
                }

                // Mark as active again so drag_drop.js knows this is internal
                internalDragActive = true;

                // Note: WebKitGTK pointer state reset is now handled by native input injection
                // from Rust (via XTest on X11 or libei on Wayland). The td-reset-pointer-state
                // event will set webkitPointerBroken=true only if injection fails (wlroots).

            } else if ((nativeDragFromSelf || isOurDragFromRust) && internalDragActive) {
                // Update drag image position during re-entry drag
                // (Windows sends physical pixels, Linux/macOS send CSS pixels)
                var coords = getPayloadCoordinates(payload);
                var x = coords.x;
                var y = coords.y;
                if (internalDragImage) {
                    internalDragImage.style.left = (x - dragImageOffsetX) + "px";
                    internalDragImage.style.top = (y - dragImageOffsetY) + "px";
                }

                // Dispatch dragover/dragleave/dragenter as needed
                var elementInfo = getElementAtPoint(x, y);
                var target = elementInfo.target;

                if (target && lastDragOverTarget && lastDragOverTarget !== target) {
                    // Left one element, entered another
                    var leaveEvent = createSyntheticDragEvent("dragleave", {
                        clientX: x,
                        clientY: y,
                        relatedTarget: target
                    }, pointerDragDataTransfer);
                    lastDragOverTarget.dispatchEvent(leaveEvent);

                    var enterEvent = createSyntheticDragEvent("dragenter", {
                        clientX: x,
                        clientY: y,
                        relatedTarget: lastDragOverTarget
                    }, pointerDragDataTransfer);
                    target.dispatchEvent(enterEvent);
                }
                lastDragOverTarget = target;

                // Always dispatch dragover
                if (target) {
                    var overEvent = createSyntheticDragEvent("dragover", {
                        clientX: x,
                        clientY: y
                    }, pointerDragDataTransfer);
                    target.dispatchEvent(overEvent);

                    // Update caret position for text inputs and contenteditable
                    if (isTextInput(target)) {
                        setInputCaretFromPoint(target, elementInfo.adjustedX, elementInfo.adjustedY);
                    } else if (isContentEditable(target)) {
                        setContentEditableCaretFromPoint(target, elementInfo.adjustedX, elementInfo.adjustedY);
                    }
                }
            }
            } catch (e) {
                log('[td-drag-motion] ERROR: ' + e.message + ' | ' + (e.stack || ''));
            }
        });

        // Handle native drag leaving window again
        tauriListen("td-drag-leave", function(event) {
            var payload = event.payload || {};
            // Check if Rust says this is our drag
            var isOurDragFromRust = payload.isOurDrag;

            if ((nativeDragFromSelf || isOurDragFromRust) && internalDragActive) {
                log('[TiddlyDesktop] Native drag left window again (nativeDragFromSelf=' + nativeDragFromSelf + ', isOurDragFromRust=' + isOurDragFromRust + ')');

                // Make sure nativeDragFromSelf is set if Rust says it's our drag
                if (isOurDragFromRust && !nativeDragFromSelf) {
                    nativeDragFromSelf = true;
                }

                // Dispatch dragleave to the last target
                if (lastDragOverTarget && pointerDragDataTransfer) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", {
                        relatedTarget: null
                    }, pointerDragDataTransfer);
                    lastDragOverTarget.dispatchEvent(leaveEvent);
                    lastDragOverTarget = null;
                }

                // Dispatch dragend to the source element so TiddlyWiki cleans up its state
                if (internalDragSource && pointerDragDataTransfer) {
                    var endEvent = createSyntheticDragEvent("dragend", {}, pointerDragDataTransfer);
                    internalDragSource.dispatchEvent(endEvent);
                }

                // Detach the drag image (keep it saved for potential re-entry)
                if (internalDragImage && internalDragImage.parentNode) {
                    internalDragImage.parentNode.removeChild(internalDragImage);
                    // Clean up any existing savedDragImage first to prevent orphans
                    if (savedDragImage && savedDragImage !== internalDragImage && savedDragImage.parentNode) {
                        savedDragImage.parentNode.removeChild(savedDragImage);
                    }
                    // Keep savedDragImage pointing to it
                    savedDragImage = internalDragImage;
                    internalDragImage = null;
                }
                internalDragActive = false;
                // Keep nativeDragFromSelf true - drag might re-enter again
            }
        });

        // Handle drop position for our own drag re-entering (Windows native DoDragDrop)
        // This fires when IDropTarget::Drop is called but it's our own drag
        tauriListen("td-drag-drop-position", function(event) {
            var payload = event.payload || {};
            var isOurDrag = payload.isOurDrag;
            var eventWindowLabel = payload.windowLabel || '';
            // Convert coordinates (Windows sends physical pixels, Linux/macOS send CSS pixels)
            var coords = getPayloadCoordinates(payload);
            var x = coords.x;
            var y = coords.y;

            // Only handle if this is our own drag
            if (!isOurDrag) return;

            // Only process if this event is for THIS window
            if (eventWindowLabel && eventWindowLabel !== currentWindowLabel) {
                log('Ignoring drop-position event for different window: ' + eventWindowLabel);
                return;
            }

            log('[TiddlyDesktop] td-drag-drop-position (our drag) at x=' + x + ', y=' + y +
                ', nativeDragFromSelf=' + nativeDragFromSelf + ', internalDragActive=' + internalDragActive);

            // If this is our drag re-entering and being dropped, handle the drop
            if (nativeDragFromSelf || internalDragActive) {
                // Get the drop target
                var elementInfo = getElementAtPoint(x, y);
                var target = elementInfo.target;

                if (lastDragOverTarget) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", {
                        clientX: x,
                        clientY: y,
                        relatedTarget: null
                    }, pointerDragDataTransfer);
                    lastDragOverTarget.dispatchEvent(leaveEvent);
                }

                if (target) {
                    var dropDt = new DataTransfer();
                    // Copy captured data to drop dataTransfer
                    if (window.__tiddlyDesktopDragData) {
                        for (var type in window.__tiddlyDesktopDragData) {
                            try {
                                dropDt.setData(type, window.__tiddlyDesktopDragData[type]);
                            } catch(e) {}
                        }
                    }

                    // Special handling for text inputs and textareas
                    var textData = window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData['text/plain'];
                    var htmlData = window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData['text/html'];

                    if (isTextInput(target)) {
                        insertTextAtPoint(target, elementInfo.adjustedX, elementInfo.adjustedY, textData);
                    } else if (isContentEditable(target)) {
                        insertIntoContentEditableAtPoint(target, elementInfo.adjustedX, elementInfo.adjustedY, textData, htmlData);
                    } else {
                        // Standard drop event for other elements
                        var dropEvent = createSyntheticDragEvent("drop", {
                            clientX: x,
                            clientY: y
                        }, dropDt);
                        target.dispatchEvent(dropEvent);
                    }
                }

                // Dispatch dragend to source
                if (savedDragSource || internalDragSource) {
                    var endEvent = createSyntheticDragEvent("dragend", {
                        clientX: x,
                        clientY: y
                    }, pointerDragDataTransfer);
                    (savedDragSource || internalDragSource).dispatchEvent(endEvent);
                }

                // Remove visual feedback classes
                document.querySelectorAll(".tc-dragover").forEach(function(el) {
                    el.classList.remove("tc-dragover");
                });
                document.querySelectorAll(".tc-dragging").forEach(function(el) {
                    el.classList.remove("tc-dragging");
                });

                // Clean up all state
                cleanupNativeDrag();
                window.__tiddlyDesktopDragData = null;
                window.__tiddlyDesktopEffectAllowed = null;
                internalDragSource = null;
                internalDragActive = false;
                lastDragOverTarget = null;
                pointerDragDataTransfer = null;
                nativeDragFromSelf = false;
                nativeDragData = null;
                savedDragInProgress = null;
                savedDragSource = null;
                pollingActive = false;
                if (savedDragImage && savedDragImage.parentNode) {
                    savedDragImage.parentNode.removeChild(savedDragImage);
                }
                savedDragImage = null;
                removeDragImage();
                document.body.style.userSelect = "";
                document.body.style.webkitUserSelect = "";
            }
        });

        // Handle drop completion - clean up native drag state
        tauriListen("td-drag-drop-start", function(event) {
            if (nativeDragFromSelf) {
                log('[TiddlyDesktop] Drop completed, cleaning up native drag state');
                // The drop will be handled by drag_drop.js
                // Clean up our tracking state (TiddlyWiki state was already cleaned up by dragend event)
                nativeDragFromSelf = false;
                nativeDragData = null;
                savedDragInProgress = null;
                savedDragSource = null;
                pollingActive = false;
                removeDragImage();
                // Also destroy the saved drag image
                if (savedDragImage && savedDragImage.parentNode) {
                    savedDragImage.parentNode.removeChild(savedDragImage);
                }
                savedDragImage = null;
                internalDragActive = false;
                nativeDragStarting = false;
                cleanupNativeDrag();
            }
        });

        // Handle pointer up detected via Rust polling (for re-entry while dragging)
        // This fires when Rust's GDK pointer polling detects the button was released
        tauriListen("td-pointer-up", function(event) {
            try {
            var payload = event.payload || {};
            // Convert coordinates (Windows sends physical pixels, Linux/macOS send CSS pixels)
            var coords = getPayloadCoordinates(payload);
            var x = coords.x;
            var y = coords.y;
            var inside = payload.inside || false;
            var eventWindowLabel = payload.windowLabel || '';

            log('[TiddlyDesktop] td-pointer-up received: x=' + x + ', y=' + y + ', inside=' + inside +
                ', nativeDragFromSelf=' + nativeDragFromSelf + ', internalDragActive=' + internalDragActive +
                ', eventWindowLabel=' + eventWindowLabel + ', currentWindowLabel=' + currentWindowLabel);

            // Only process if this event is for THIS window
            if (eventWindowLabel && eventWindowLabel !== currentWindowLabel) {
                log('Ignoring pointer-up event for different window: ' + eventWindowLabel);
                return;
            }

            // If we're tracking a native drag from self and button was released
            if (nativeDragFromSelf) {
                // Check if drag was cancelled by Escape - don't trigger drop
                if (dragCancelledByEscape) {
                    log('[TiddlyDesktop] Button released but drag was cancelled by Escape - skipping drop');
                    dragCancelledByEscape = false;  // Reset the flag
                    // Remove visual feedback classes
                    document.querySelectorAll(".tc-dragover").forEach(function(el) {
                        el.classList.remove("tc-dragover");
                    });
                    document.querySelectorAll(".tc-dragging").forEach(function(el) {
                        el.classList.remove("tc-dragging");
                    });
                    // Fall through to cleanup below
                } else if (inside && internalDragActive) {
                    // Button released inside window while drag was active - this is a drop
                    log('[TiddlyDesktop] Button released inside window - triggering drop');

                    // Get the drop target
                    var elementInfo = getElementAtPoint(x, y);
                    var target = elementInfo.target;

                    if (lastDragOverTarget) {
                        var leaveEvent = createSyntheticDragEvent("dragleave", {
                            clientX: x,
                            clientY: y,
                            relatedTarget: null
                        }, pointerDragDataTransfer);
                        lastDragOverTarget.dispatchEvent(leaveEvent);
                    }

                    if (target) {
                        var dropDt = new DataTransfer();
                        // Copy captured data to drop dataTransfer
                        if (window.__tiddlyDesktopDragData) {
                            for (var type in window.__tiddlyDesktopDragData) {
                                try {
                                    dropDt.setData(type, window.__tiddlyDesktopDragData[type]);
                                } catch(e) {}
                            }
                        }

                        // Special handling for text inputs and textareas
                        var textData = window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData['text/plain'];
                        var htmlData = window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData['text/html'];

                        if (isTextInput(target)) {
                            insertTextAtPoint(target, elementInfo.adjustedX, elementInfo.adjustedY, textData);
                        } else if (isContentEditable(target)) {
                            insertIntoContentEditableAtPoint(target, elementInfo.adjustedX, elementInfo.adjustedY, textData, htmlData);
                        } else {
                            // Standard drop event for other elements
                            var dropEvent = createSyntheticDragEvent("drop", {
                                clientX: x,
                                clientY: y
                            }, dropDt);
                            target.dispatchEvent(dropEvent);
                        }
                    }

                    // Dispatch dragend
                    if (internalDragSource && pointerDragDataTransfer) {
                        var endEvent = createSyntheticDragEvent("dragend", {
                            clientX: x,
                            clientY: y
                        }, pointerDragDataTransfer);
                        internalDragSource.dispatchEvent(endEvent);
                    }
                }

                // Dispatch synthetic pointerup on the original drag source element
                // This completes the pointer interaction cycle so the browser allows new pointerdown events
                var originalSource = savedDragSource || internalDragSource || pointerDownTarget;
                if (originalSource) {
                    try {
                        var syntheticPointerUp = new PointerEvent('pointerup', {
                            pointerId: capturedPointerId || 1,
                            bubbles: true,
                            cancelable: true,
                            pointerType: 'mouse',
                            button: 0,
                            buttons: 0,
                            clientX: x,
                            clientY: y
                        });
                        syntheticPointerUp.__tiddlyDesktopSynthetic = true;
                        originalSource.dispatchEvent(syntheticPointerUp);
                        log('[td-pointer-up] Dispatched synthetic pointerup on ' + originalSource.tagName);
                    } catch (e) {
                        log('[td-pointer-up] Failed to dispatch synthetic pointerup: ' + e);
                    }
                }

                // Release pointer capture before cleaning up state
                // This is critical - we must release capture while we still have the element references
                if (capturedPointerId !== null) {
                    var releasePointerId = capturedPointerId;
                    [savedDragSource, internalDragSource, pointerDownTarget].forEach(function(el) {
                        if (el) {
                            try {
                                el.releasePointerCapture(releasePointerId);
                                log('[td-pointer-up] Released pointer capture from ' + el.tagName);
                            } catch (e) {
                                // Element may not have capture, that's OK
                            }
                        }
                    });
                }

                // Clean up all state
                cleanupNativeDrag();

                // Clear TiddlyWiki's drag state
                if (typeof $tw !== "undefined") {
                    $tw.dragInProgress = null;
                }

                // Remove visual feedback classes
                document.querySelectorAll(".tc-dragover").forEach(function(el) {
                    el.classList.remove("tc-dragover");
                });
                document.querySelectorAll(".tc-dragging").forEach(function(el) {
                    el.classList.remove("tc-dragging");
                });

                // Remove any leftover TiddlyWiki drag image elements
                document.querySelectorAll(".tc-tiddler-dragger").forEach(function(el) {
                    el.parentNode.removeChild(el);
                });

                // Blur any focused element to ensure clean state for next interaction
                if (document.activeElement && document.activeElement !== document.body) {
                    try {
                        document.activeElement.blur();
                    } catch (e) {}
                }

                // Cancel any lingering browser drag state by dispatching dragend on document
                // This helps reset the browser's internal state that might be blocking pointer events
                try {
                    var cancelDragEvent = new DragEvent('dragend', {
                        bubbles: true,
                        cancelable: true
                    });
                    document.dispatchEvent(cancelDragEvent);
                    log('[TiddlyDesktop] Dispatched document-level dragend to reset browser drag state');
                } catch (e) {
                    log('[TiddlyDesktop] Failed to dispatch dragend: ' + e);
                }

                // Reset pointer-events CSS in case something set it to none
                document.body.style.pointerEvents = '';
                document.documentElement.style.pointerEvents = '';

                // WebKitGTK workaround: After a GTK drag, pointer events may be stuck.
                // Dispatch a full click cycle (pointerdown + pointerup) at off-screen coordinates
                // to reset WebKitGTK's internal pointer tracking state.
                // Using coordinates far outside viewport (-1000, -1000) to avoid triggering any UI.
                setTimeout(function() {
                    try {
                        // First dispatch pointerdown at off-screen position
                        var resetDown = new PointerEvent('pointerdown', {
                            pointerId: 1,
                            bubbles: true,
                            cancelable: true,
                            pointerType: 'mouse',
                            button: 0,
                            buttons: 1,
                            clientX: -1000,
                            clientY: -1000,
                            screenX: -1000,
                            screenY: -1000,
                            isPrimary: true
                        });
                        resetDown.__tiddlyDesktopSynthetic = true;
                        document.dispatchEvent(resetDown);
                        log('[TiddlyDesktop] Dispatched synthetic pointerdown at (-1000, -1000)');

                        // Then dispatch pointerup at same position
                        var resetUp = new PointerEvent('pointerup', {
                            pointerId: 1,
                            bubbles: true,
                            cancelable: true,
                            pointerType: 'mouse',
                            button: 0,
                            buttons: 0,
                            clientX: -1000,
                            clientY: -1000,
                            screenX: -1000,
                            screenY: -1000,
                            isPrimary: true
                        });
                        resetUp.__tiddlyDesktopSynthetic = true;
                        document.dispatchEvent(resetUp);
                        log('[TiddlyDesktop] Dispatched synthetic pointerup at (-1000, -1000)');

                        // Also try click event for good measure
                        var resetClick = new MouseEvent('click', {
                            bubbles: true,
                            cancelable: true,
                            button: 0,
                            clientX: -1000,
                            clientY: -1000,
                            screenX: -1000,
                            screenY: -1000
                        });
                        resetClick.__tiddlyDesktopSynthetic = true;
                        document.dispatchEvent(resetClick);
                        log('[TiddlyDesktop] Dispatched synthetic click at (-1000, -1000) to reset pointer state');
                    } catch (e) {
                        log('[TiddlyDesktop] Pointer reset error: ' + e);
                    }
                }, 10);

                window.__tiddlyDesktopDragData = null;
                window.__tiddlyDesktopEffectAllowed = null;
                internalDragSource = null;
                internalDragActive = false;
                lastDragOverTarget = null;
                pointerDragDataTransfer = null;
                nativeDragFromSelf = false;
                nativeDragData = null;
                savedDragInProgress = null;
                savedDragSource = null;
                pointerDownTarget = null;
                pointerDownPos = null;
                pointerDragStarted = false;
                capturedPointerId = null;
                nativeDragStarting = false;
                pollingActive = false;
                if (savedDragImage && savedDragImage.parentNode) {
                    savedDragImage.parentNode.removeChild(savedDragImage);
                }
                savedDragImage = null;
                removeDragImage();
                document.body.style.userSelect = "";
                document.body.style.webkitUserSelect = "";

                log('[TiddlyDesktop] Cleaned up after td-pointer-up, capturedPointerId=' + capturedPointerId +
                    ', nativeDragFromSelf=' + nativeDragFromSelf + ', internalDragActive=' + internalDragActive);
            }
            } catch (e) {
                log('[td-pointer-up] ERROR: ' + e.message + ' | ' + (e.stack || ''));
            }
        });

        // Handle pointer state reset request from Rust
        // This fires after a re-entry + drop to reset WebKitGTK's pointer event state
        // Rust will attempt native input injection (XTest on X11, libei on Wayland)
        // If injection fails (e.g., wlroots without libei), needsFallback will be true
        tauriListen("td-reset-pointer-state", function(event) {
            var payload = event.payload || {};
            var x = payload.x || 0;
            var y = payload.y || 0;
            var needsFallback = payload.needsFallback || false;
            log('[TiddlyDesktop] td-reset-pointer-state received at (' + x + ', ' + y + '), needsFallback=' + needsFallback);

            if (needsFallback) {
                // Native input injection failed (e.g., Wayland + wlroots)
                // Enable the mousedown fallback for subsequent interactions
                webkitPointerBroken = true;
                log('[TiddlyDesktop] Enabled mousedown fallback (native injection unavailable)');
            } else {
                // Native input injection succeeded
                // The injected click should reset WebKitGTK's pointer tracking
                // No fallback needed
                webkitPointerBroken = false;
                log('[TiddlyDesktop] Native injection succeeded, no fallback needed');
            }
        });

        // Handle native drag end signal from GTK
        // GTK may fire drag-end in two cases:
        // 1. Prematurely - drag left the window but user might re-enter (data_was_requested=false)
        // 2. Real drop - data was transferred to external app (data_was_requested=true)
        // We only clean up if data was actually requested (real external drop).
        // For premature end, we keep state so re-entry works.
        tauriListen("td-drag-end", function(event) {
            var payload = event.payload || {};
            var dataWasRequested = payload.data_was_requested || false;
            log('[TiddlyDesktop] td-drag-end received, nativeDragFromSelf=' + nativeDragFromSelf +
                ', internalDragActive=' + internalDragActive + ', dataWasRequested=' + dataWasRequested);

            // If we're not tracking an outgoing native drag, ignore this event
            if (!nativeDragFromSelf) {
                return;
            }

            // Only clean up if data was actually requested (real drop to external app)
            // If data was NOT requested, GTK is just signaling drag-end because the pointer
            // left the window - but the user might re-enter, so we preserve state
            if (!dataWasRequested) {
                log('[TiddlyDesktop] GTK drag-end without data request - preserving state for potential re-entry');
                // Don't clean up - user might re-enter
                // The state will be cleaned up by td-pointer-up when button is released
                return;
            }

            log('[TiddlyDesktop] Native drag completed with external drop - cleaning up');

            // Remove visual feedback classes
            document.querySelectorAll(".tc-dragover").forEach(function(el) {
                el.classList.remove("tc-dragover");
            });
            document.querySelectorAll(".tc-dragging").forEach(function(el) {
                el.classList.remove("tc-dragging");
            });

            // Release pointer capture before cleaning up state
            if (capturedPointerId !== null) {
                var releasePointerId = capturedPointerId;
                [savedDragSource, internalDragSource, pointerDownTarget].forEach(function(el) {
                    if (el) {
                        try {
                            el.releasePointerCapture(releasePointerId);
                            log('[td-drag-end] Released pointer capture from ' + el.tagName);
                        } catch (e) {
                            // Element may not have capture, that's OK
                        }
                    }
                });
            }

            // Clean up native drag on Rust side
            cleanupNativeDrag();

            // Reset all state
            window.__tiddlyDesktopDragData = null;
            window.__tiddlyDesktopEffectAllowed = null;
            internalDragSource = null;
            internalDragActive = false;
            lastDragOverTarget = null;
            pointerDragDataTransfer = null;
            nativeDragFromSelf = false;
            nativeDragData = null;
            savedDragInProgress = null;
            savedDragSource = null;
            pointerDownTarget = null;
            pointerDownPos = null;
            pointerDragStarted = false;
            capturedPointerId = null;
            nativeDragStarting = false;
            pollingActive = false;
            // Destroy saved drag image
            if (savedDragImage && savedDragImage.parentNode) {
                savedDragImage.parentNode.removeChild(savedDragImage);
            }
            savedDragImage = null;
            removeDragImage();
            document.body.style.userSelect = "";
            document.body.style.webkitUserSelect = "";
        });

        // Safety: If we receive td-drag-content while nativeDragFromSelf but we're not active,
        // this is external content and our state is stale - clean up
        tauriListen("td-drag-content", function(event) {
            // If we're tracking a native drag from self but receive external content,
            // it means our drag ended without us knowing - clean up
            if (nativeDragFromSelf && !internalDragActive) {
                log('[TiddlyDesktop] Received external drag content while nativeDragFromSelf - resetting stale state');
                nativeDragFromSelf = false;
                nativeDragData = null;
                savedDragInProgress = null;
                savedDragSource = null;
                pollingActive = false;
                if (savedDragImage && savedDragImage.parentNode) {
                    savedDragImage.parentNode.removeChild(savedDragImage);
                }
                savedDragImage = null;
                nativeDragStarting = false;
                cleanupNativeDrag();
            }
        });

        log('Tauri event listeners for native drag events set up successfully');
    }

    // Initialize Tauri event listeners when ready
    if (window.__TAURI__) {
        setupTauriEventListeners();
    } else {
        // Wait for Tauri to be available
        var tauriWaitCount = 0;
        var tauriWaitInterval = setInterval(function() {
            tauriWaitCount++;
            if (window.__TAURI__) {
                clearInterval(tauriWaitInterval);
                setupTauriEventListeners();
            } else if (tauriWaitCount > 50) {
                clearInterval(tauriWaitInterval);
                log('Tauri not available after 5 seconds, native drag events will not work');
            }
        }, 100);
    }

    // Export for use by drag_drop.js
    TD.isInternalDragActive = function() {
        return internalDragActive || nativeDragFromSelf;
    };

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
