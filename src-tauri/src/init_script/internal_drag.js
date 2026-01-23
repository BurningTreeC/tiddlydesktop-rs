// TiddlyDesktop Initialization Script - Internal Drag Polyfill Module
// Handles: internal TiddlyWiki drag-and-drop (tiddlers, tags, links)
//
// This polyfill intercepts drags of draggable elements within the wiki and
// handles them using mouse events + synthetic DOM events. This is necessary
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
    var mouseDownTarget = null;
    var mouseDownPos = null;
    var mouseDragStarted = false;
    var mouseDragDataTransfer = null;
    var lastDragOverTarget = null;
    var DRAG_THRESHOLD = 3;
    var capturedSelection = null;  // Selection captured on mousedown
    var capturedSelectionHtml = null;
    var isTextSelectionDrag = false;  // Track if current drag is a text selection

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
        if (!internalDragActive && !mouseDragStarted) return;

        var dt = mouseDragDataTransfer || new DataTransfer();

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
        }

        if (typeof $tw !== "undefined") {
            $tw.dragInProgress = null;
        }

        window.__tiddlyDesktopDragData = null;
        window.__tiddlyDesktopEffectAllowed = null;
        internalDragSource = null;
        internalDragActive = false;
        mouseDownTarget = null;
        mouseDownPos = null;
        mouseDragStarted = false;
        mouseDragDataTransfer = null;
        lastDragOverTarget = null;
        capturedSelection = null;
        capturedSelectionHtml = null;
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

        // Cancel native drag and use synthetic drag
        event.preventDefault();

        mouseDragStarted = true;
        internalDragActive = true;
        internalDragSource = draggable || target;
        mouseDownTarget = draggable || target;

        document.body.style.userSelect = "none";
        document.body.style.webkitUserSelect = "none";

        window.__tiddlyDesktopDragData = {};
        dragImageIsBlank = false;
        mouseDragDataTransfer = new DataTransfer();

        var originalSetDragImage = mouseDragDataTransfer.setDragImage;
        mouseDragDataTransfer.setDragImage = function(element, x, y) {
            if (element && (!element.firstChild || element.offsetWidth === 0 || element.offsetHeight === 0)) {
                dragImageIsBlank = true;
            }
            if (originalSetDragImage) {
                originalSetDragImage.call(this, element, x, y);
            }
        };

        var syntheticDragStart = createSyntheticDragEvent("dragstart", {
            clientX: event.clientX,
            clientY: event.clientY
        }, mouseDragDataTransfer);

        // For text selection drags, pre-populate the dataTransfer with selected content
        if (isTextSelectionDrag) {
            var htmlContent = capturedSelectionHtml || getSelectedHtml();
            if (selectedText) {
                mouseDragDataTransfer.setData("text/plain", selectedText);
                window.__tiddlyDesktopDragData["text/plain"] = selectedText;
            }
            if (htmlContent) {
                mouseDragDataTransfer.setData("text/html", htmlContent);
                window.__tiddlyDesktopDragData["text/html"] = htmlContent;
            }
        }

        internalDragSource.dispatchEvent(syntheticDragStart);

        window.__tiddlyDesktopEffectAllowed = mouseDragDataTransfer.effectAllowed || "all";
        createDragImage(internalDragSource, event.clientX, event.clientY, isTextSelectionDrag ? selectedText : null);

        var enterTarget = document.elementFromPoint(event.clientX, event.clientY);
        if (enterTarget) {
            var enterEvent = createSyntheticDragEvent("dragenter", {
                clientX: event.clientX,
                clientY: event.clientY,
                relatedTarget: null
            }, mouseDragDataTransfer);
            enterTarget.dispatchEvent(enterEvent);
            lastDragOverTarget = enterTarget;
        }
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
        // If we're handling an internal drag via mouse events, ignore native dragend
        // (native dragend might fire when we preventDefault on dragstart)
        if (mouseDragStarted && !event.__tiddlyDesktopSynthetic) {
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

    // Mousedown to track potential drag
    document.addEventListener("mousedown", function(event) {
        if (event.button !== 0) return;

        // Capture any existing text selection (for potential text selection drag)
        capturedSelection = getSelectedText();
        capturedSelectionHtml = getSelectedHtml();

        var target = findDraggableAncestor(event.target);
        if (target) {
            mouseDownTarget = target;
            mouseDownPos = { x: event.clientX, y: event.clientY };
            mouseDragStarted = false;
        } else {
            // For potential text selection drags, track mousedown position
            mouseDownTarget = null;
            mouseDownPos = { x: event.clientX, y: event.clientY };
            mouseDragStarted = false;
        }
    }, true);

    // Mousemove fallback if native dragstart didn't fire
    document.addEventListener("mousemove", function(event) {
        if (mouseDragStarted) return;
        if (internalDragActive) {
            mouseDownTarget = null;
            return;
        }
        if (!mouseDownPos) return;

        var dx = event.clientX - mouseDownPos.x;
        var dy = event.clientY - mouseDownPos.y;
        if (Math.abs(dx) < DRAG_THRESHOLD && Math.abs(dy) < DRAG_THRESHOLD) return;

        // Check if this is a draggable element or text selection (use captured selection)
        var selectedText = capturedSelection || getSelectedText();
        if (!mouseDownTarget && !selectedText) {
            // No draggable target and no text selection
            mouseDownPos = null;
            return;
        }

        mouseDragStarted = true;
        internalDragActive = true;
        internalDragSource = mouseDownTarget || document.elementFromPoint(event.clientX, event.clientY);

        document.body.style.userSelect = "none";
        document.body.style.webkitUserSelect = "none";

        window.__tiddlyDesktopDragData = {};
        dragImageIsBlank = false;
        mouseDragDataTransfer = new DataTransfer();

        var originalSetDragImage = mouseDragDataTransfer.setDragImage;
        mouseDragDataTransfer.setDragImage = function(element, x, y) {
            if (element && (!element.firstChild || element.offsetWidth === 0 || element.offsetHeight === 0)) {
                dragImageIsBlank = true;
            }
            if (originalSetDragImage) {
                originalSetDragImage.call(this, element, x, y);
            }
        };

        // For text selection drags, pre-populate the dataTransfer
        if (!mouseDownTarget && selectedText) {
            var htmlContent = capturedSelectionHtml || getSelectedHtml();
            mouseDragDataTransfer.setData("text/plain", selectedText);
            window.__tiddlyDesktopDragData["text/plain"] = selectedText;
            if (htmlContent) {
                mouseDragDataTransfer.setData("text/html", htmlContent);
                window.__tiddlyDesktopDragData["text/html"] = htmlContent;
            }
        }

        var dragStartEvent = createSyntheticDragEvent("dragstart", {
            clientX: mouseDownPos.x,
            clientY: mouseDownPos.y
        }, mouseDragDataTransfer);

        internalDragSource.dispatchEvent(dragStartEvent);

        // Determine if this is a text selection drag
        var isTextDrag = !mouseDownTarget && selectedText;
        window.__tiddlyDesktopEffectAllowed = mouseDragDataTransfer.effectAllowed || "all";
        createDragImage(internalDragSource, event.clientX, event.clientY, isTextDrag ? selectedText : null);

        var enterTarget = document.elementFromPoint(event.clientX, event.clientY);
        if (enterTarget) {
            var enterEvent = createSyntheticDragEvent("dragenter", {
                clientX: event.clientX,
                clientY: event.clientY,
                relatedTarget: null
            }, mouseDragDataTransfer);
            enterTarget.dispatchEvent(enterEvent);
            lastDragOverTarget = enterTarget;
        }
    }, true);

    // Mousemove for updating drag image and firing dragover
    document.addEventListener("mousemove", function(event) {
        if (!mouseDragStarted || !internalDragSource) return;

        updateDragImagePosition(event.clientX, event.clientY);

        var target = document.elementFromPoint(event.clientX, event.clientY);
        if (!target) return;

        if (lastDragOverTarget && lastDragOverTarget !== target) {
            var leaveEvent = createSyntheticDragEvent("dragleave", {
                clientX: event.clientX,
                clientY: event.clientY,
                relatedTarget: target
            }, mouseDragDataTransfer);
            lastDragOverTarget.dispatchEvent(leaveEvent);

            var enterEvent = createSyntheticDragEvent("dragenter", {
                clientX: event.clientX,
                clientY: event.clientY,
                relatedTarget: lastDragOverTarget
            }, mouseDragDataTransfer);
            target.dispatchEvent(enterEvent);
        }
        lastDragOverTarget = target;

        var overEvent = createSyntheticDragEvent("dragover", {
            clientX: event.clientX,
            clientY: event.clientY
        }, mouseDragDataTransfer);
        target.dispatchEvent(overEvent);
    }, true);

    // Mouseup to complete drop
    document.addEventListener("mouseup", function(event) {
        if (mouseDragStarted && internalDragSource) {
            var target = document.elementFromPoint(event.clientX, event.clientY);

            if (lastDragOverTarget) {
                var leaveEvent = createSyntheticDragEvent("dragleave", {
                    clientX: event.clientX,
                    clientY: event.clientY,
                    relatedTarget: null
                }, mouseDragDataTransfer);
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
                var dropEvent = createSyntheticDragEvent("drop", {
                    clientX: event.clientX,
                    clientY: event.clientY
                }, dropDt);
                target.dispatchEvent(dropEvent);
            }

            var endEvent = createSyntheticDragEvent("dragend", {
                clientX: event.clientX,
                clientY: event.clientY
            }, mouseDragDataTransfer);
            internalDragSource.dispatchEvent(endEvent);

            // Reset all drag state
            window.__tiddlyDesktopDragData = null;
            window.__tiddlyDesktopEffectAllowed = null;
            internalDragSource = null;
            internalDragActive = false;
            lastDragOverTarget = null;
            mouseDragDataTransfer = null;
            removeDragImage();
            document.body.style.userSelect = "";
            document.body.style.webkitUserSelect = "";
        }

        mouseDownTarget = null;
        mouseDownPos = null;
        mouseDragStarted = false;
        capturedSelection = null;
        capturedSelectionHtml = null;
    }, true);

    // Handle mouse leaving window
    document.addEventListener("mouseleave", function(event) {
        if (internalDragActive || mouseDragStarted) {
            if (!event.relatedTarget || !document.contains(event.relatedTarget)) {
                cancelDrag("mouse left window");
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
        if (internalDragActive || mouseDragStarted) {
            setTimeout(function() {
                if ((internalDragActive || mouseDragStarted) && !document.hasFocus()) {
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
