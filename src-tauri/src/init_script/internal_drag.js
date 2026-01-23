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
            // For element drags, clone the element
            dragEl = sourceElement.cloneNode(true);
            dragEl.style.position = "fixed";
            dragEl.style.pointerEvents = "none";
            dragEl.style.zIndex = "999999";
            dragEl.style.opacity = "0.7";
            dragEl.style.transform = "scale(0.9)";
            dragEl.style.maxWidth = "300px";
            dragEl.style.maxHeight = "100px";
            dragEl.style.overflow = "hidden";
            dragEl.style.whiteSpace = "nowrap";
            dragEl.style.textOverflow = "ellipsis";
            dragEl.style.background = getBackgroundColor(sourceElement);
            dragEl.style.padding = "4px 8px";
            dragEl.style.borderRadius = "4px";
            dragEl.style.boxShadow = "0 2px 8px rgba(0,0,0,0.3)";

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
        el.focus();
        el.setSelectionRange(pos, pos);
        return pos;
    }

    // Set caret position in contenteditable from coordinates
    function setContentEditableCaretFromPoint(el, clientX, clientY) {
        // Use the document that owns the element (important for iframes)
        var doc = el.ownerDocument || document;

        if (doc.caretRangeFromPoint) {
            var range = doc.caretRangeFromPoint(clientX, clientY);
            if (range) {
                var sel = doc.defaultView.getSelection();
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
                var sel = doc.defaultView.getSelection();
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
        if (!internalDragActive && !pointerDragStarted) return;

        var dt = pointerDragDataTransfer || new DataTransfer();

        if (lastDragOverTarget) {
            var leaveEvent = createSyntheticDragEvent("dragleave", { relatedTarget: null }, dt);
            lastDragOverTarget.dispatchEvent(leaveEvent);
        }

        document.querySelectorAll(".tc-dragover").forEach(function(el) {
            el.classList.remove("tc-dragover");
            var endEvent = createSyntheticDragEvent("dragend", {}, dt);
            el.dispatchEvent(endEvent);
        });

        document.querySelectorAll(".tc-dragging").forEach(function(el) {
            el.classList.remove("tc-dragging");
        });

        if (internalDragSource) {
            var endEvent = createSyntheticDragEvent("dragend", {}, dt);
            internalDragSource.dispatchEvent(endEvent);

            // Release pointer capture
            if (capturedPointerId !== null) {
                try {
                    internalDragSource.releasePointerCapture(capturedPointerId);
                } catch (e) {}
            }
        }

        if (typeof $tw !== "undefined") {
            $tw.dragInProgress = null;
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

    // Pointerdown to track potential drag
    document.addEventListener("pointerdown", function(event) {
        // Only handle primary button (left click / touch / pen tip)
        if (event.button !== 0) return;

        // Don't handle if already tracking a pointer
        if (capturedPointerId !== null) return;

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
        // Only handle the pointer we're tracking
        if (capturedPointerId !== null && event.pointerId !== capturedPointerId) return;

        if (!pointerDownPos) return;

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
            } catch (e) {
                // Element may not support pointer capture
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
            createDragImage(internalDragSource, event.clientX, event.clientY, isTextDrag ? selectedText : null);

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

            // Reset all drag state
            window.__tiddlyDesktopDragData = null;
            window.__tiddlyDesktopEffectAllowed = null;
            internalDragSource = null;
            internalDragActive = false;
            lastDragOverTarget = null;
            pointerDragDataTransfer = null;
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
    }, true);

    // Handle pointer cancel (e.g., system gesture, palm rejection)
    document.addEventListener("pointercancel", function(event) {
        if (capturedPointerId !== null && event.pointerId === capturedPointerId) {
            cancelDrag("pointer cancelled");
        }
    }, true);

    // Handle pointer leaving window
    document.addEventListener("pointerleave", function(event) {
        // Only cancel if pointer leaves the document element (the window)
        if (event.target === document.documentElement) {
            if (internalDragActive || pointerDragStarted) {
                cancelDrag("pointer left window");
            }
        }
    }, true);

    // Handle escape to cancel drag
    document.addEventListener("keydown", function(event) {
        if (event.key === "Escape") {
            cancelDrag("escape pressed");
        }
    }, true);

    // Handle window blur
    window.addEventListener("blur", function(event) {
        if (internalDragActive || pointerDragStarted) {
            setTimeout(function() {
                if ((internalDragActive || pointerDragStarted) && !document.hasFocus()) {
                    cancelDrag("window lost focus");
                }
            }, 100);
        }
    }, true);

    // Export for use by drag_drop.js
    TD.isInternalDragActive = function() {
        return internalDragActive;
    };

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
