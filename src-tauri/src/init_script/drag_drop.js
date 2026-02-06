// TiddlyDesktop Initialization Script - Drag & Drop Module
// Handles: external attachments, file drops, content drags, paste, import hooks

(function(TD) {
    'use strict';

    // ========================================
    // Text Sanitization (encoding fixes)
    // ========================================

    // Sanitize text that may have encoding issues from clipboard/drag-drop
    // Removes null bytes, replacement characters, and fixes common encoding problems
    function sanitizeDroppedText(text) {
        if (!text || typeof text !== 'string') return text;

        // Remove null bytes (sometimes appear between characters in bad UTF-16 conversions)
        var sanitized = text.replace(/\u0000/g, '');

        // Remove Unicode replacement characters
        sanitized = sanitized.replace(/\uFFFD/g, '');

        // If text has replacement characters mixed with HTML entities, try to clean up
        // Pattern like "&lt;�span>" becomes "&lt;span>"
        sanitized = sanitized.replace(/&([a-z]+);[\uFFFD\u0000]+/gi, '&$1;');
        sanitized = sanitized.replace(/[\uFFFD\u0000]+&([a-z]+);/gi, '&$1;');

        return sanitized;
    }

    // ========================================
    // Path Utilities
    // ========================================

    function isWindowsPath(path) {
        return /^[A-Za-z]:[\\\/]/.test(path) || path.startsWith("\\\\");
    }

    function getNativeSeparator(originalPath) {
        if (originalPath.indexOf("\\") >= 0) return "\\";
        if (isWindowsPath(originalPath)) return "\\";
        return "/";
    }

    function normalizeForComparison(filepath) {
        var path = filepath.replace(/\\/g, "/");
        if (path.charAt(0) !== "/" && !isWindowsPath(filepath)) {
            path = "/" + path;
        }
        if (path.substring(0, 2) === "//") {
            path = path.substring(1);
        }
        return path;
    }

    function toNativePath(normalizedPath, useBackslashes) {
        if (useBackslashes) {
            return normalizedPath.replace(/\//g, "\\");
        }
        return normalizedPath;
    }

    function makePathRelative(sourcepath, rootpath, options) {
        options = options || {};

        var isWindows = isWindowsPath(sourcepath) || isWindowsPath(rootpath);
        var nativeSep = isWindows ? "\\" : "/";

        var normalizedSource = normalizeForComparison(sourcepath);
        var normalizedRoot = normalizeForComparison(rootpath);

        var sourceParts = normalizedSource.split("/");
        var rootParts = normalizedRoot.split("/");

        var c = 0;
        while (c < sourceParts.length && c < rootParts.length && sourceParts[c] === rootParts[c]) {
            c += 1;
        }

        if (c === 1 ||
            (options.useAbsoluteForNonDescendents && c < rootParts.length) ||
            (options.useAbsoluteForDescendents && c === rootParts.length)) {
            return toNativePath(normalizedSource, isWindows);
        }

        var outputParts = [];
        for (var p = c; p < rootParts.length - 1; p++) {
            outputParts.push("..");
        }
        for (p = c; p < sourceParts.length; p++) {
            outputParts.push(sourceParts[p]);
        }
        return outputParts.join(nativeSep);
    }

    function getMimeType(filename) {
        var ext = filename.split(".").pop().toLowerCase();
        var mimeTypes = {
            "png": "image/png", "jpg": "image/jpeg", "jpeg": "image/jpeg",
            "gif": "image/gif", "webp": "image/webp", "svg": "image/svg+xml",
            "ico": "image/x-icon", "bmp": "image/bmp", "tiff": "image/tiff",
            "pdf": "application/pdf",
            "mp3": "audio/mpeg", "ogg": "audio/ogg", "wav": "audio/wav",
            "flac": "audio/flac", "m4a": "audio/mp4",
            "mp4": "video/mp4", "webm": "video/webm", "ogv": "video/ogg",
            "mov": "video/quicktime", "avi": "video/x-msvideo",
            "zip": "application/zip",
            "json": "application/json",
            "tid": "application/x-tiddler",
            "tiddler": "application/x-tiddler-html-div",
            "multids": "application/x-tiddlers",
            "html": "text/html", "htm": "text/html",
            "csv": "text/csv",
            "txt": "text/plain",
            "css": "text/css",
            "js": "application/javascript",
            "xml": "application/xml",
            "md": "text/x-markdown"
        };
        return mimeTypes[ext] || "application/octet-stream";
    }

    // Export utilities
    TD.makePathRelative = makePathRelative;
    TD.getMimeType = getMimeType;

    // ========================================
    // Synthetic Event Creation
    // ========================================

    function createSyntheticDragEvent(type, position, dataTransfer, relatedTarget) {
        // Sanitize coordinates - TiddlyWiki's handlers use these with elementFromPoint
        // which throws on non-finite values (NaN, Infinity, undefined)
        var safeX = (position && Number.isFinite(position.x)) ? position.x : 0;
        var safeY = (position && Number.isFinite(position.y)) ? position.y : 0;
        var event = new DragEvent(type, {
            bubbles: true,
            cancelable: true,
            clientX: safeX,
            clientY: safeY,
            relatedTarget: relatedTarget !== undefined ? relatedTarget : null
        });

        if (dataTransfer) {
            try {
                Object.defineProperty(event, 'dataTransfer', {
                    value: dataTransfer,
                    writable: false,
                    configurable: true
                });
            } catch (e) {
                console.error("[TiddlyDesktop] Could not set dataTransfer:", e);
            }
        }

        event.__tiddlyDesktopSynthetic = true;
        return event;
    }

    // ========================================
    // Text Input/ContentEditable Helpers
    // ========================================

    // Check if element is a text input or textarea
    function isTextInput(el) {
        // Safeguard: ensure el is a valid element with tagName
        if (!el || typeof el !== 'object') return false;
        if (!el.tagName || typeof el.tagName !== 'string') return false;
        var tag = el.tagName.toLowerCase();
        if (tag === 'textarea') return true;
        if (tag === 'input') {
            var type = (el.type || 'text').toLowerCase();
            return type === 'text' || type === 'search' || type === 'url' || type === 'email' || type === 'password';
        }
        return false;
    }

    // Check if element is contenteditable
    function isContentEditable(el) {
        // Safeguard: ensure el is a valid element
        if (!el || typeof el !== 'object') return false;
        // Check if the element itself is contenteditable
        try {
            if (el.isContentEditable) return true;
            // Check for contenteditable attribute
            if (el.getAttribute && el.getAttribute('contenteditable') === 'true') return true;
        } catch (e) {
            return false;
        }
        return false;
    }

    // Get element at point, handling iframes
    function getElementAtPointForCaret(x, y) {
        // Validate coordinates are finite numbers (elementFromPoint throws on NaN/Infinity)
        if (!Number.isFinite(x) || !Number.isFinite(y)) {
            return { element: null, adjustedX: x, adjustedY: y };
        }
        var el = document.elementFromPoint(x, y);
        if (!el) return { element: null, adjustedX: x, adjustedY: y };

        // Check if it's an iframe
        if (el.tagName && el.tagName.toLowerCase() === 'iframe') {
            try {
                // Safeguard: ensure element has getBoundingClientRect method
                if (typeof el.getBoundingClientRect !== 'function') {
                    return { element: el, adjustedX: x, adjustedY: y };
                }
                var rect = el.getBoundingClientRect();
                var iframeX = x - rect.left;
                var iframeY = y - rect.top;
                var iframeDoc = el.contentDocument || el.contentWindow.document;
                var iframeEl = iframeDoc.elementFromPoint(iframeX, iframeY);
                if (iframeEl) {
                    return { element: iframeEl, adjustedX: iframeX, adjustedY: iframeY, iframe: el };
                }
            } catch (e) {
                // Cross-origin iframe or other error, can't access
            }
        }
        return { element: el, adjustedX: x, adjustedY: y };
    }

    // Set caret position in input/textarea from coordinates
    function setInputCaretFromPoint(el, clientX, clientY) {
        if (!el || !isTextInput(el)) return;

        // Safeguard: ensure element has getBoundingClientRect method
        if (typeof el.getBoundingClientRect !== 'function') return;

        // Ensure the browser window is focused before focusing the element
        // This is needed during native drags when the OS drag system had focus
        window.focus();
        el.focus();

        // Try to get caret position from click coordinates
        // This is an approximation - we measure character widths
        var text = el.value || '';
        if (!text.length) {
            el.setSelectionRange(0, 0);
            return;
        }

        // Create a temporary span to measure text
        var style;
        try {
            style = window.getComputedStyle(el);
        } catch (e) {
            return;
        }
        if (!style) return;

        var span = document.createElement('span');
        span.style.font = style.font;
        span.style.fontSize = style.fontSize;
        span.style.fontFamily = style.fontFamily;
        span.style.fontWeight = style.fontWeight;
        span.style.letterSpacing = style.letterSpacing;
        span.style.whiteSpace = 'pre';
        span.style.position = 'absolute';
        span.style.visibility = 'hidden';
        document.body.appendChild(span);

        var rect;
        try {
            rect = el.getBoundingClientRect();
        } catch (e) {
            document.body.removeChild(span);
            return;
        }
        var paddingLeft = parseFloat(style.paddingLeft) || 0;
        var relativeX = clientX - rect.left - paddingLeft + el.scrollLeft;

        // Binary search for the character position
        var pos = 0;
        for (var i = 0; i <= text.length; i++) {
            span.textContent = text.substring(0, i);
            if (span.offsetWidth >= relativeX) {
                // Check if we're closer to this position or the previous
                if (i > 0) {
                    span.textContent = text.substring(0, i - 1);
                    var prevWidth = span.offsetWidth;
                    span.textContent = text.substring(0, i);
                    var currWidth = span.offsetWidth;
                    pos = (relativeX - prevWidth) < (currWidth - relativeX) ? i - 1 : i;
                } else {
                    pos = 0;
                }
                break;
            }
            pos = i;
        }

        document.body.removeChild(span);
        el.setSelectionRange(pos, pos);
    }

    // Set caret position in contenteditable from coordinates
    function setContentEditableCaretFromPoint(el, clientX, clientY) {
        // Safeguard: ensure el is a valid element
        if (!el || typeof el !== 'object') return;

        // Use the document that owns the element (important for iframes)
        var doc;
        var win;
        try {
            doc = el.ownerDocument || document;
            win = doc.defaultView || window;
        } catch (e) {
            return;
        }

        // Ensure the browser window is focused before focusing the element
        // This is needed during native drags when the OS drag system had focus
        window.focus();
        // For iframes, also focus the iframe's window
        if (win !== window) {
            win.focus();
        }
        el.focus();

        if (doc.caretRangeFromPoint) {
            var range = doc.caretRangeFromPoint(clientX, clientY);
            if (range) {
                var sel = win.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
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
            }
        }
    }

    // Update caret position for text inputs during drag
    function updateCaretForDrag(x, y) {
        var info = getElementAtPointForCaret(x, y);
        var el = info.element;
        // Safeguard: ensure el is a valid element
        if (!el || typeof el !== 'object') return;

        try {
            if (isTextInput(el)) {
                setInputCaretFromPoint(el, info.adjustedX, info.adjustedY);
            } else if (isContentEditable(el)) {
                setContentEditableCaretFromPoint(el, info.adjustedX, info.adjustedY);
            }
        } catch (e) {
            // Silently ignore errors from invalid elements
        }
    }

    // ========================================
    // Landing Page Drag-Drop Setup
    // ========================================
    // For the landing page (main wiki), we handle file drops by opening them
    // as wikis. We do NOT trigger any import mechanism.

    function setupLandingPageDragDrop() {
        window.__TD_EXTERNAL_ATTACHMENTS_READY__ = true;

        var currentWindow = window.__TAURI__.window.getCurrentWindow();
        var listen = currentWindow.listen.bind(currentWindow);
        var invoke = window.__TAURI__.core.invoke;
        var windowLabel = window.__WINDOW_LABEL__ || 'main';

        invoke("js_log", { message: "Setting up landing page drag-drop for window: " + windowLabel });

        // Track drag state for visual feedback
        var nativeDragActive = false;

        // CRITICAL: Block ALL native drag events from reaching TiddlyWiki's dropzone
        // This prevents the import mechanism from triggering
        document.addEventListener("dragenter", function(event) {
            event.preventDefault();
            event.stopPropagation();
        }, true);

        document.addEventListener("dragover", function(event) {
            event.preventDefault();
            event.stopPropagation();
        }, true);

        document.addEventListener("dragleave", function(event) {
            event.preventDefault();
            event.stopPropagation();
        }, true);

        document.addEventListener("drop", function(event) {
            invoke("js_log", { message: "Landing page: native drop blocked (preventing import)" });
            event.preventDefault();
            event.stopPropagation();
        }, true);

        // Block paste events on landing page to prevent import mechanism
        document.addEventListener("paste", function(event) {
            invoke("js_log", { message: "Landing page: paste blocked (preventing import)" });
            event.preventDefault();
            event.stopPropagation();
        }, true);

        // Handle drag motion - show visual feedback
        listen("td-drag-motion", function(event) {
            if (!nativeDragActive) {
                nativeDragActive = true;
                document.body.classList.add("td-drag-over");
            }
        });

        // Handle drag leave - clear visual feedback
        listen("td-drag-leave", function(event) {
            nativeDragActive = false;
            document.body.classList.remove("td-drag-over");
        });

        // Handle td-drag-content - IGNORE for landing page (no imports)
        listen("td-drag-content", function(event) {
            invoke("js_log", { message: "Landing page: ignoring td-drag-content (no import on landing page)" });
            // Clear drag state
            nativeDragActive = false;
            document.body.classList.remove("td-drag-over");
        });

        // Handle td-file-drop for visual feedback only (Linux/macOS)
        // Note: Actual wiki opening is handled by startup.js in the TiddlyWiki plugin
        listen("td-file-drop", function(event) {
            invoke("js_log", { message: "Landing page: td-file-drop received (visual feedback only)" });
            nativeDragActive = false;
            document.body.classList.remove("td-drag-over");
        });

        // Handle tauri://drag-enter for visual feedback
        listen("tauri://drag-enter", function(event) {
            var paths = event.payload.paths || [];
            // Only show feedback for file drags
            if (paths.length > 0 && !paths.every(function(p) { return p.startsWith("data:"); })) {
                document.body.classList.add("td-drag-over");
            }
        });

        // Handle tauri://drag-leave for visual feedback
        listen("tauri://drag-leave", function(event) {
            document.body.classList.remove("td-drag-over");
        });

        // Handle tauri://drag-drop for visual feedback only
        // Note: Actual wiki opening is handled by startup.js in the TiddlyWiki plugin
        listen("tauri://drag-drop", function(event) {
            invoke("js_log", { message: "Landing page: tauri://drag-drop received (visual feedback only)" });
            nativeDragActive = false;
            document.body.classList.remove("td-drag-over");
        });

        invoke("js_log", { message: "Landing page drag-drop ready" });
    }

    // ========================================
    // Main Setup Function
    // ========================================

    window.__extAttachRetryCount = window.__extAttachRetryCount || 0;

    function setupExternalAttachments() {
        if (window.__TD_EXTERNAL_ATTACHMENTS_READY__) {
            return;
        }

        window.__extAttachRetryCount++;
        var extAttachRetryCount = window.__extAttachRetryCount;

        var shouldLog = extAttachRetryCount === 1 ||
            (extAttachRetryCount <= 100 && extAttachRetryCount % 10 === 0) ||
            (extAttachRetryCount > 100 && extAttachRetryCount % 60 === 0);

        if (!window.__TAURI__ || !window.__TAURI__.event) {
            setTimeout(setupExternalAttachments, 100);
            return;
        }

        if (window.__IS_MAIN_WIKI__) {
            setupLandingPageDragDrop();
            return;
        }

        if (!window.__WIKI_PATH__) {
            setTimeout(setupExternalAttachments, 100);
            return;
        }

        if (typeof $tw === 'undefined' || !$tw.wiki) {
            var interval = extAttachRetryCount < 100 ? 100 : 1000;
            setTimeout(setupExternalAttachments, interval);
            return;
        }

        // Use window-specific listen to avoid receiving events from other windows
        var currentWindow = window.__TAURI__.window.getCurrentWindow();
        var listen = currentWindow.listen.bind(currentWindow);
        var invoke = window.__TAURI__.core.invoke;
        var wikiPath = window.__WIKI_PATH__;
        var windowLabel = window.__WINDOW_LABEL__ || 'unknown';

        invoke("js_log", { message: "Setting up drag-drop listeners for: " + wikiPath + " window: " + windowLabel + " (window-specific)" });

        // ========================================
        // Tauri WebviewWindow onDragDropEvent (alternative API)
        // ========================================

        // Try the higher-level onDragDropEvent API which may work differently
        if (currentWindow.onDragDropEvent) {
            currentWindow.onDragDropEvent(function(event) {
                var payload = event.payload || event;
                var type = payload.type;
                var paths = payload.paths || [];

                invoke("js_log", { message: "onDragDropEvent: type=" + type + ", paths=" + JSON.stringify(paths) });

                if (type === 'hover' || type === 'enter' || type === 'over') {
                    // Store paths during hover/drag-over
                    if (paths.length > 0) {
                        paths.forEach(function(filepath) {
                            if (filepath && !filepath.startsWith("data:")) {
                                var filename = filepath.split(/[/\\]/).pop();
                                window.__pendingExternalFiles[filename] = filepath;
                                invoke("js_log", { message: "onDragDropEvent: stored path for '" + filename + "'" });
                            }
                        });
                    }
                } else if (type === 'drop') {
                    // Store paths on drop
                    if (paths.length > 0) {
                        invoke("js_log", { message: "onDragDropEvent DROP: " + paths.length + " files" });
                        paths.forEach(function(filepath) {
                            if (filepath && !filepath.startsWith("data:")) {
                                var filename = filepath.split(/[/\\]/).pop();
                                window.__pendingExternalFiles[filename] = filepath;
                                invoke("js_log", { message: "onDragDropEvent: stored drop path for '" + filename + "'" });
                            }
                        });
                    }
                } else if (type === 'cancel' || type === 'leave') {
                    invoke("js_log", { message: "onDragDropEvent: drag cancelled/left" });
                }
            }).then(function(unlisten) {
                invoke("js_log", { message: "onDragDropEvent listener registered successfully" });
            }).catch(function(err) {
                invoke("js_log", { message: "onDragDropEvent registration failed: " + err });
            });
        } else {
            invoke("js_log", { message: "onDragDropEvent API not available" });
        }

        // ========================================
        // Drag State Variables
        // ========================================

        var pendingFilePaths = [];
        var enteredTarget = null;
        var currentTarget = null;
        var isDragging = false;
        var nativeDragActive = false;
        var nativeDragTarget = null;
        var pendingGtkFileDrop = null;
        var nativeDropInProgress = false;
        var nativeLeaveTimeout = null;  // Timer for delayed leave cleanup
        var contentDragActive = false;
        var contentDragTarget = null;
        var contentDragTypes = [];
        var contentDragEnterCount = 0;
        var pendingContentDropData = null;
        var pendingContentDropPos = null;
        var contentDropTimeout = null;

        // Windows-specific: track pending native file drop waiting for tauri://drag-drop
        var pendingWindowsFileDrop = null;
        var isWindows = navigator.platform.indexOf('Win') !== -1 || navigator.userAgent.indexOf('Windows') !== -1;

        window.__pendingExternalFiles = window.__pendingExternalFiles || {};

        // ========================================
        // Helper Functions
        // ========================================

        function getTargetElement(position) {
            if (position && Number.isFinite(position.x) && Number.isFinite(position.y)) {
                var el = document.elementFromPoint(position.x, position.y);
                if (el) return el;
            }
            return document.body;
        }

        function getClassName(el) {
            if (!el) return "";
            var cn = el.className;
            if (typeof cn === "string") return cn;
            if (cn && typeof cn.baseVal === "string") return cn.baseVal;
            return "";
        }

        function createDataTransferWithFiles() {
            var dt = new DataTransfer();
            pendingFilePaths.forEach(function(path) {
                var filename = path.split(/[/\\]/).pop();
                dt.items.add(new File([""], filename, { type: getMimeType(filename) }));
            });
            return dt;
        }

        function createContentDataTransfer() {
            var dt = new DataTransfer();
            var types = contentDragTypes.length > 0 ? contentDragTypes : [
                "text/plain", "text/html", "text/uri-list", "text/vnd.tiddler"
            ];
            types.forEach(function(type) {
                if (type !== "Files") {
                    try { dt.setData(type, ""); } catch(e) {}
                }
            });
            return dt;
        }

        function screenToClient(screenX, screenY) {
            var dpr = window.devicePixelRatio || 1;
            var cssScreenX = screenX / dpr;
            var cssScreenY = screenY / dpr;
            var clientX = cssScreenX - window.screenX;
            var clientY = cssScreenY - window.screenY;
            clientX = Math.max(0, Math.min(clientX, window.innerWidth - 1));
            clientY = Math.max(0, Math.min(clientY, window.innerHeight - 1));
            return { x: Math.round(clientX), y: Math.round(clientY) };
        }

        // ========================================
        // Cancel Functions
        // ========================================

        function cancelExternalDrag(reason) {
            if (!isDragging) return;

            var dt = createDataTransferWithFiles();

            if (currentTarget) {
                var leaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                currentTarget.dispatchEvent(leaveEvent);
            }

            document.querySelectorAll(".tc-dragover").forEach(function(el) {
                var droppableLeaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                el.dispatchEvent(droppableLeaveEvent);
                var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                el.dispatchEvent(dropzoneEndEvent);
                el.classList.remove("tc-dragover");
            });

            document.querySelectorAll(".tc-dropzone").forEach(function(el) {
                var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                el.dispatchEvent(dropzoneEndEvent);
            });

            document.querySelectorAll(".tc-dragging").forEach(function(el) {
                el.classList.remove("tc-dragging");
            });

            var endEvent = createSyntheticDragEvent("dragend", null, dt, null);
            document.body.dispatchEvent(endEvent);

            if (typeof $tw !== "undefined") {
                $tw.dragInProgress = null;
            }

            pendingFilePaths = [];
            enteredTarget = null;
            currentTarget = null;
            isDragging = false;
            // Clear pending external files since drag was cancelled
            window.__pendingExternalFiles = {};
        }

        function cancelContentDrag(reason) {
            if (!contentDragActive) return;

            var dt = createContentDataTransfer();

            if (currentTarget) {
                var leaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                leaveEvent.__tiddlyDesktopSynthetic = true;
                currentTarget.dispatchEvent(leaveEvent);
            }

            document.querySelectorAll(".tc-dragover").forEach(function(el) {
                var droppableLeaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                droppableLeaveEvent.__tiddlyDesktopSynthetic = true;
                el.dispatchEvent(droppableLeaveEvent);
                var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                dropzoneEndEvent.__tiddlyDesktopSynthetic = true;
                el.dispatchEvent(dropzoneEndEvent);
                el.classList.remove("tc-dragover");
            });

            document.querySelectorAll(".tc-dropzone").forEach(function(el) {
                var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                dropzoneEndEvent.__tiddlyDesktopSynthetic = true;
                el.dispatchEvent(dropzoneEndEvent);
            });

            document.querySelectorAll(".tc-dragging").forEach(function(el) {
                el.classList.remove("tc-dragging");
            });

            var endEvent = createSyntheticDragEvent("dragend", null, dt, null);
            endEvent.__tiddlyDesktopSynthetic = true;
            document.body.dispatchEvent(endEvent);

            if (typeof $tw !== "undefined") {
                $tw.dragInProgress = null;
            }

            contentDragActive = false;
            contentDragTarget = null;
            contentDragTypes = [];
            contentDragEnterCount = 0;
            enteredTarget = null;
            currentTarget = null;
        }

        function resetGtkDragState() {
            nativeDragActive = false;
            nativeDragTarget = null;
            nativeDropInProgress = false;
            currentTarget = null;
            pendingContentDropPos = null;
        }

        // ========================================
        // Internal Drag Tracking (for logging only - all drags go through td-drag-*)
        // ========================================

        // ========================================
        // Tauri Drag Events
        // ========================================

        // Windows: Listen for file paths pushed from Rust vtable hook
        // This ensures paths are available immediately when native HTML5 drop fires
        listen("td-external-file-paths", function(event) {
            var paths = event.payload && event.payload.paths || [];
            if (paths.length > 0) {
                invoke("js_log", { message: "td-external-file-paths: received " + paths.length + " paths from Rust" });
                paths.forEach(function(filepath) {
                    if (filepath && !filepath.startsWith("data:")) {
                        var filename = filepath.split(/[/\\]/).pop();
                        window.__pendingExternalFiles[filename] = filepath;
                        invoke("js_log", { message: "td-external-file-paths: set __pendingExternalFiles['" + filename + "']" });
                    }
                });
            }
        });

        listen("tauri://drag-enter", function(event) {
            var paths = event.payload.paths || [];

            invoke("js_log", { message: "tauri://drag-enter on " + windowLabel + ": paths=" + JSON.stringify(paths) });

            // CRITICAL: Always populate __pendingExternalFiles for file drags, BEFORE any
            // other checks. On Windows, td-drag-motion fires first which sets nativeDragActive,
            // but we still need to capture file paths here so the external attachments hook
            // can find them when the native WebView2 drop event fires.
            if (paths.length > 0) {
                invoke("js_log", { message: "tauri://drag-enter: populating __pendingExternalFiles with " + paths.length + " paths" });
                paths.forEach(function(filepath) {
                    if (filepath && !filepath.startsWith("data:")) {
                        var filename = filepath.split(/[/\\]/).pop();
                        window.__pendingExternalFiles[filename] = filepath;
                        invoke("js_log", { message: "tauri://drag-enter: set __pendingExternalFiles['" + filename + "'] = '" + filepath + "'" });
                    }
                });
            } else {
                // Windows fallback: Try to get paths from WRY patch FFI storage
                // This handles cases where WRY extracted paths but they weren't in the event payload
                invoke("get_external_drop_paths").then(function(ffiPaths) {
                    if (ffiPaths && ffiPaths.length > 0) {
                        invoke("js_log", { message: "tauri://drag-enter: Got " + ffiPaths.length + " paths from FFI storage" });
                        ffiPaths.forEach(function(filepath) {
                            if (filepath && !filepath.startsWith("data:")) {
                                var filename = filepath.split(/[/\\]/).pop();
                                window.__pendingExternalFiles[filename] = filepath;
                                invoke("js_log", { message: "tauri://drag-enter: FFI set __pendingExternalFiles['" + filename + "']" });
                            }
                        });
                    }
                }).catch(function() {});
            }

            // WINDOWS: Just store paths - native HTML5 drag events fire automatically
            // (SetAllowExternalDrop is enabled in setup_drag_handlers)
            if (isWindows) {
                invoke("js_log", { message: "tauri://drag-enter: Windows - paths stored, native HTML5 handles events" });
                return;
            }

            // Now check if we should skip the rest of the handler
            if (nativeDragActive) return;

            var isInternalDrag = (typeof $tw !== "undefined" && $tw.dragInProgress) ||
                (paths.length > 0 && paths.every(function(p) { return p.startsWith("data:"); }));

            if (isInternalDrag) return;

            // Skip content drags (empty paths) - these are handled by td-drag-motion from our
            // custom GTK/Windows/macOS handlers which have proper window filtering.
            // tauri://drag-enter is sent to ALL windows, which causes multiple windows to show
            // dropzones during cross-wiki drags. Only process FILE drags here.
            if (paths.length === 0) return;

            var target = getTargetElement(event.payload.position);
            enteredTarget = target;
            currentTarget = target;

            // File drag only (paths.length > 0)
            pendingFilePaths = paths;
            isDragging = true;

            var dt = createDataTransferWithFiles();
            var enterEvent = createSyntheticDragEvent("dragenter", event.payload.position, dt);
            target.dispatchEvent(enterEvent);
        });

        listen("tauri://drag-over", function(event) {
            var paths = event.payload.paths || [];

            // CRITICAL: Always populate __pendingExternalFiles for file drags, BEFORE any
            // other checks. This ensures paths are available when native drop fires.
            if (paths.length > 0) {
                paths.forEach(function(filepath) {
                    if (filepath && !filepath.startsWith("data:")) {
                        var filename = filepath.split(/[/\\]/).pop();
                        if (!window.__pendingExternalFiles[filename]) {
                            window.__pendingExternalFiles[filename] = filepath;
                        }
                    }
                });
            } else if (Object.keys(window.__pendingExternalFiles).length === 0) {
                // Windows fallback: Try to get paths from WRY patch FFI storage
                invoke("get_external_drop_paths").then(function(ffiPaths) {
                    if (ffiPaths && ffiPaths.length > 0) {
                        ffiPaths.forEach(function(filepath) {
                            if (filepath && !filepath.startsWith("data:")) {
                                var filename = filepath.split(/[/\\]/).pop();
                                if (!window.__pendingExternalFiles[filename]) {
                                    window.__pendingExternalFiles[filename] = filepath;
                                }
                            }
                        });
                    }
                }).catch(function() {});
            }

            // WINDOWS: Native HTML5 dragover events fire automatically
            if (isWindows) {
                return;
            }

            if (nativeDragActive) return;
            if (!isDragging && !contentDragActive) return;
            if (typeof $tw !== "undefined" && $tw.dragInProgress) return;

            var target = getTargetElement(event.payload.position);
            var dt = isDragging ? createDataTransferWithFiles() : createContentDataTransfer();

            if (currentTarget && currentTarget !== target) {
                var leaveEvent = createSyntheticDragEvent("dragleave", event.payload.position, dt, target);
                currentTarget.dispatchEvent(leaveEvent);
                var enterEvent = createSyntheticDragEvent("dragenter", event.payload.position, dt, currentTarget);
                target.dispatchEvent(enterEvent);
            }

            currentTarget = target;
            if (contentDragActive) contentDragTarget = target;

            var overEvent = createSyntheticDragEvent("dragover", event.payload.position, dt);
            target.dispatchEvent(overEvent);
        });

        listen("tauri://drag-leave", function(event) {
            // WINDOWS: Native HTML5 handles drag-leave
            if (isWindows) return;

            if (nativeDragActive) return;
            if (isDragging) {
                cancelExternalDrag("drag left window");
            } else if (contentDragActive) {
                cancelContentDrag("drag left window");
            }
        });

        // ========================================
        // Native Drag Events (Linux GTK, Windows IDropTarget)
        // ========================================

        listen("td-drag-motion", function(event) {
            if (!event.payload) return;
            // Skip if internal drag is active (handled by internal_drag.js)
            if (TD.isInternalDragActive && TD.isInternalDragActive()) return;

            // Cancel any pending leave timeout - we're still dragging
            if (nativeLeaveTimeout) {
                clearTimeout(nativeLeaveTimeout);
                nativeLeaveTimeout = null;
            }

            var pos;
            if (event.payload.screenCoords) {
                pos = screenToClient(event.payload.x, event.payload.y);
            } else if (event.payload.physicalPixels) {
                // Windows sends physical pixels that need dpr scaling
                var dpr = window.devicePixelRatio || 1;
                pos = { x: event.payload.x / dpr, y: event.payload.y / dpr };
            } else {
                // Linux GTK and macOS send CSS pixels, no scaling needed
                pos = { x: event.payload.x, y: event.payload.y };
            }
            var target = getTargetElement(pos);

            // Issue 4b: For same-wiki tiddler drags (not text selection), only dispatch events
            // to $droppable elements - NOT to the main dropzone
            // This prevents dropzone from showing tc-dragover for same-wiki tiddler drags
            var isSameWikiTiddlerDrag = event.payload.isSameWindow &&
                                        !event.payload.isTextSelectionDrag &&
                                        event.payload.hasTiddlerData;

            if (isSameWikiTiddlerDrag) {
                var droppable = target.closest('.tc-droppable');
                if (!droppable) {
                    // Not over a $droppable - skip synthetic events
                    // This prevents dropzone from showing tc-dragover
                    // But still update caret position for potential input drops
                    updateCaretForDrag(pos.x, pos.y);
                    return;
                }
            }

            var dt = new DataTransfer();
            ["text/plain", "text/html", "text/uri-list", "text/vnd.tiddler"].forEach(function(type) {
                try { dt.setData(type, ""); } catch(e) {}
            });

            if (!nativeDragActive) {
                nativeDragActive = true;
                nativeDragTarget = target;
                currentTarget = target;

                var enterEvent = createSyntheticDragEvent("dragenter", pos, dt);
                enterEvent.__tiddlyDesktopSynthetic = true;
                target.dispatchEvent(enterEvent);
            } else {
                if (nativeDragTarget && nativeDragTarget !== target) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, target);
                    leaveEvent.__tiddlyDesktopSynthetic = true;
                    nativeDragTarget.dispatchEvent(leaveEvent);

                    var enterEvent = createSyntheticDragEvent("dragenter", pos, dt, nativeDragTarget);
                    enterEvent.__tiddlyDesktopSynthetic = true;
                    target.dispatchEvent(enterEvent);
                }
                nativeDragTarget = target;
                currentTarget = target;
            }

            var overEvent = createSyntheticDragEvent("dragover", pos, dt);
            overEvent.__tiddlyDesktopSynthetic = true;
            target.dispatchEvent(overEvent);

            // Update caret position for text inputs and contenteditable elements
            updateCaretForDrag(pos.x, pos.y);
        });

        listen("td-drag-drop-start", function(event) {
            // Skip if internal drag is active (handled by internal_drag.js)
            if (TD.isInternalDragActive && TD.isInternalDragActive()) return;
            nativeDropInProgress = true;
            if (event.payload) {
                if (event.payload.screenCoords) {
                    pendingContentDropPos = screenToClient(event.payload.x, event.payload.y);
                } else if (event.payload.physicalPixels) {
                    // Windows sends physical pixels that need dpr scaling
                    var dpr = window.devicePixelRatio || 1;
                    pendingContentDropPos = { x: event.payload.x / dpr, y: event.payload.y / dpr };
                } else {
                    // Linux GTK and macOS send CSS pixels, no scaling needed
                    pendingContentDropPos = { x: event.payload.x, y: event.payload.y };
                }
            }
        });

        listen("td-drag-leave", function(event) {
            // Skip if internal drag is active (handled by internal_drag.js)
            if (TD.isInternalDragActive && TD.isInternalDragActive()) return;
            if (nativeDropInProgress) return;

            if (nativeDragActive) {
                // For cross-wiki drags from our app (isOurDrag=true), clean up immediately
                // because Rust tracking has definitively determined the drag moved elsewhere.
                // For external drags, use a small delay to handle quick re-entry.
                var isOurDrag = event.payload && event.payload.isOurDrag;
                var delay = isOurDrag ? 0 : 100;

                // Cancel any previous pending leave timeout
                if (nativeLeaveTimeout) {
                    clearTimeout(nativeLeaveTimeout);
                }

                nativeLeaveTimeout = setTimeout(function() {
                    nativeLeaveTimeout = null;
                    if (nativeDropInProgress) return;
                    if (!nativeDragActive) return;

                    var dt = new DataTransfer();
                    if (nativeDragTarget) {
                        var leaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                        leaveEvent.__tiddlyDesktopSynthetic = true;
                        nativeDragTarget.dispatchEvent(leaveEvent);
                    }
                    document.querySelectorAll(".tc-dragover").forEach(function(el) {
                        el.classList.remove("tc-dragover");
                        var endEvent = createSyntheticDragEvent("dragend", null, dt, null);
                        endEvent.__tiddlyDesktopSynthetic = true;
                        el.dispatchEvent(endEvent);
                    });
                    nativeDragActive = false;
                    nativeDragTarget = null;
                    nativeDropInProgress = false;
                    currentTarget = null;
                }, delay);
            }
        });

        listen("td-drag-content", function(event) {
            // Skip if internal drag is active (handled by internal_drag.js)
            if (TD.isInternalDragActive && TD.isInternalDragActive()) return;
            if (event.payload) {
                // Sanitize all string data to fix encoding issues
                var sanitizedData = {};
                var rawData = event.payload.data || {};
                for (var key in rawData) {
                    if (rawData.hasOwnProperty(key)) {
                        sanitizedData[key] = sanitizeDroppedText(rawData[key]);
                    }
                }
                pendingContentDropData = {
                    types: event.payload.types || [],
                    data: sanitizedData,
                    files: [],
                    isTextSelectionDrag: event.payload.is_text_selection_drag || false,
                    isSameWindow: event.payload.isSameWindow || false
                };
                if (pendingContentDropPos) {
                    processContentDrop();
                }
            }
        });

        listen("td-drag-drop-position", function(event) {
            // Skip if internal drag is active (handled by internal_drag.js)
            // Note: On Windows/macOS, same-window drops are also handled here because
            // IDropTarget/NSDraggingDestination intercepts the drop before WebView can handle it.
            // The isInternalDragActive check covers the case where internal_drag.js is handling it.
            if (TD.isInternalDragActive && TD.isInternalDragActive()) return;
            if (event.payload) {
                var pos;
                if (event.payload.screenCoords) {
                    pos = screenToClient(event.payload.x, event.payload.y);
                } else if (event.payload.physicalPixels) {
                    // Windows sends physical pixels that need dpr scaling
                    var dpr = window.devicePixelRatio || 1;
                    pos = { x: event.payload.x / dpr, y: event.payload.y / dpr };
                } else {
                    // Linux GTK and macOS send CSS pixels, no scaling needed
                    pos = { x: event.payload.x, y: event.payload.y };
                }
                pendingContentDropPos = pos;
                if (pendingContentDropData) {
                    processContentDrop();
                }
            }
        });

        listen("td-file-drop", function(event) {
            invoke("js_log", { message: "td-file-drop received on window: " + windowLabel + ", paths: " + JSON.stringify(event.payload && event.payload.paths) });

            // Skip if internal drag is active (handled by internal_drag.js)
            if (TD.isInternalDragActive && TD.isInternalDragActive()) return;
            if (!event.payload || !event.payload.paths || event.payload.paths.length === 0) return;

            var paths = event.payload.paths;

            // CRITICAL: Immediately populate __pendingExternalFiles with file paths.
            // On Windows, the native WebView2 drop event may fire before processGtkFileDrop
            // completes its async file reading. By populating paths here, the external
            // attachments hook will have access to the original file paths.
            paths.forEach(function(filepath) {
                if (filepath && !filepath.startsWith("data:")) {
                    var filename = filepath.split(/[/\\]/).pop();
                    window.__pendingExternalFiles[filename] = filepath;
                }
            });

            pendingGtkFileDrop = paths;
            setTimeout(function() {
                if (!pendingGtkFileDrop) return;
                processGtkFileDrop();
            }, 10);
        });

        // ========================================
        // Drop Processing Functions
        // ========================================

        function processGtkFileDrop() {
            if (!pendingGtkFileDrop) return;

            var paths = pendingGtkFileDrop;
            var pos = pendingContentDropPos || { x: 100, y: 100 };
            var dropTarget = nativeDragTarget || getTargetElement(pos);

            pendingGtkFileDrop = null;
            pendingContentDropPos = null;

            var filePromises = paths.map(function(filepath) {
                if (filepath.startsWith("data:") || (!filepath.startsWith("/") && !filepath.match(/^[A-Za-z]:\\/))) {
                    return Promise.resolve(null);
                }

                var filename = filepath.split(/[/\\]/).pop();
                var mimeType = getMimeType(filename);

                return invoke("read_file_as_binary", { path: filepath }).then(function(bytes) {
                    window.__pendingExternalFiles[filename] = filepath;
                    return new File([new Uint8Array(bytes)], filename, { type: mimeType });
                }).catch(function(err) {
                    console.error("[TiddlyDesktop] Failed to read file:", filepath, err);
                    return null;
                });
            });

            Promise.all(filePromises).then(function(files) {
                var validFiles = files.filter(function(f) { return f !== null; });
                if (validFiles.length === 0) {
                    resetGtkDragState();
                    return;
                }

                var dt = new DataTransfer();
                validFiles.forEach(function(file) { dt.items.add(file); });

                if (nativeDragTarget) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, null);
                    leaveEvent.__tiddlyDesktopSynthetic = true;
                    nativeDragTarget.dispatchEvent(leaveEvent);
                }

                var dropEvent = createSyntheticDragEvent("drop", pos, dt);
                dropEvent.__tiddlyDesktopSynthetic = true;
                dropTarget.dispatchEvent(dropEvent);

                var endEvent = createSyntheticDragEvent("dragend", pos, dt);
                endEvent.__tiddlyDesktopSynthetic = true;
                document.body.dispatchEvent(endEvent);

                setTimeout(function() { window.__pendingExternalFiles = {}; }, 5000);
                resetGtkDragState();
            });
        }

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
                if (tagName === 'IFRAME') {
                    try {
                        var iframeDoc = el.contentDocument || el.contentWindow.document;
                        if (iframeDoc) {
                            // Check if iframe body is editable
                            if (iframeDoc.designMode === 'on' ||
                                (iframeDoc.body && iframeDoc.body.isContentEditable)) {
                                return true;
                            }
                            // Check if active element inside iframe is editable
                            var activeEl = iframeDoc.activeElement;
                            if (activeEl) {
                                var activeTag = activeEl.tagName;
                                if (activeTag === 'INPUT' || activeTag === 'TEXTAREA' || activeEl.isContentEditable) {
                                    return true;
                                }
                            }
                        }
                    } catch (e) {}
                }
                el = el.parentElement;
            }
            return false;
        }

        function processContentDrop() {
            if (!pendingContentDropData) return;

            var capturedData = pendingContentDropData;
            var pos = pendingContentDropPos;
            var dropTarget = getTargetElement(pos);

            // Issue 3: For text-selection drags, filter out text/html to prevent styled content import
            // This applies to cross-wiki text selection drags where we want plain text only
            if (capturedData.isTextSelectionDrag && capturedData.data['text/html']) {
                invoke("js_log", { message: "Filtering text/html from text-selection drag" });
                delete capturedData.data['text/html'];
                var htmlIdx = capturedData.types.indexOf('text/html');
                if (htmlIdx !== -1) {
                    capturedData.types.splice(htmlIdx, 1);
                }
            }

            // For same-window drops on editable elements, the native browser handler
            // should have already processed them (via original IDropTarget on Windows,
            // ORIGINAL_PERFORM_DRAG on macOS). If we reach here, just skip - don't
            // dispatch synthetic events to editable elements.
            if (capturedData.isSameWindow && isEditableElement(dropTarget)) {
                invoke("js_log", { message: "Same-window drop on editable - should have been handled natively" });
                pendingContentDropData = null;
                pendingContentDropPos = null;
                resetGtkDragState();
                return;
            }

            // Issue 4b: For same-wiki tiddler drags (not text-selection), only dispatch to $droppable elements
            // Don't dispatch to main dropzone (prevents import of already-existing tiddler)
            var isSameWikiTiddlerDrag = capturedData.isSameWindow &&
                                        !capturedData.isTextSelectionDrag &&
                                        capturedData.data['text/vnd.tiddler'];

            if (isSameWikiTiddlerDrag) {
                var droppable = dropTarget.closest('.tc-droppable');
                if (!droppable) {
                    // Dropped on main dropzone, not on $droppable
                    // Don't dispatch - no import for same-wiki tiddlers
                    invoke("js_log", { message: "Same-wiki tiddler drop on dropzone - skipping import" });
                    pendingContentDropData = null;
                    pendingContentDropPos = null;
                    resetGtkDragState();
                    return;
                }
                // Dropped on $droppable - continue to dispatch event for reordering etc.
            }

            // Merge in any captured internal drag data (from TiddlyWiki's dataTransfer.setData)
            if (window.__tiddlyDesktopDragData) {
                var internalData = window.__tiddlyDesktopDragData;
                for (var key in internalData) {
                    if (internalData.hasOwnProperty(key) && !capturedData.data[key]) {
                        capturedData.data[key] = internalData[key];
                        if (capturedData.types.indexOf(key) === -1) {
                            capturedData.types.push(key);
                        }
                    }
                }
            }

            pendingContentDropData = null;
            pendingContentDropPos = null;
            if (contentDropTimeout) {
                clearTimeout(contentDropTimeout);
                contentDropTimeout = null;
            }

            var dataMap = capturedData.data;
            var fileList = capturedData.files.slice();
            var typesList = Object.keys(dataMap);

            if (fileList.length > 0 && typesList.indexOf('Files') === -1) {
                typesList.push('Files');
            }

            var itemsArray = [];
            typesList.forEach(function(type) {
                if (type !== 'Files') {
                    itemsArray.push({
                        kind: "string",
                        type: type,
                        getAsString: function(callback) {
                            if (typeof callback === 'function') {
                                setTimeout(function() { callback(dataMap[type] || ""); }, 0);
                            }
                        },
                        getAsFile: function() { return null; }
                    });
                }
            });

            fileList.forEach(function(file) {
                itemsArray.push({
                    kind: "file",
                    type: file.type || "application/octet-stream",
                    getAsString: function(callback) {},
                    getAsFile: function() { return file; }
                });
            });

            itemsArray.add = function(data, type) {
                if (data instanceof File) {
                    fileList.push(data);
                    this.push({
                        kind: "file",
                        type: data.type || "application/octet-stream",
                        getAsString: function() {},
                        getAsFile: function() { return data; }
                    });
                } else if (typeof data === "string" && type) {
                    dataMap[type] = data;
                    if (typesList.indexOf(type) === -1) typesList.push(type);
                    this.push({
                        kind: "string",
                        type: type,
                        getAsString: function(cb) { if (cb) setTimeout(function() { cb(data); }, 0); },
                        getAsFile: function() { return null; }
                    });
                }
            };
            itemsArray.remove = function(index) { this.splice(index, 1); };
            itemsArray.clear = function() { this.length = 0; };

            var dt = {
                types: typesList,
                files: fileList,
                items: itemsArray,
                dropEffect: "copy",
                effectAllowed: "all",
                getData: function(type) { return (type in dataMap) ? dataMap[type] : ""; },
                setData: function(type, value) {
                    dataMap[type] = value;
                    if (typesList.indexOf(type) === -1) typesList.push(type);
                },
                clearData: function(type) {
                    if (type) {
                        delete dataMap[type];
                        var idx = typesList.indexOf(type);
                        if (idx !== -1) typesList.splice(idx, 1);
                    } else {
                        for (var k in dataMap) delete dataMap[k];
                        typesList.length = 0;
                    }
                },
                setDragImage: function() {}
            };

            if (currentTarget) {
                var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, null);
                leaveEvent.__tiddlyDesktopSynthetic = true;
                currentTarget.dispatchEvent(leaveEvent);
            }

            var dropEvent = createSyntheticDragEvent("drop", pos, dt);
            dropEvent.__tiddlyDesktopSynthetic = true;
            dropTarget.dispatchEvent(dropEvent);

            var endEvent = createSyntheticDragEvent("dragend", pos, dt);
            endEvent.__tiddlyDesktopSynthetic = true;
            document.body.dispatchEvent(endEvent);

            pendingFilePaths = [];
            enteredTarget = null;
            currentTarget = null;
            isDragging = false;
            contentDragActive = false;
            contentDragTarget = null;
            contentDragTypes = [];
            nativeDragActive = false;
            nativeDragTarget = null;
            nativeDropInProgress = false;
        }

        // ========================================
        // Native Browser Drag Events
        // ========================================

        document.addEventListener("dragenter", function(event) {
            if (event.__tiddlyDesktopSynthetic) return;
            if (nativeDragActive || isDragging) return;
            if (contentDragActive) {
                contentDragEnterCount++;
                return;
            }

            var dt = event.dataTransfer;
            if (!dt || !dt.types || dt.types.length === 0) return;
            if (typeof $tw !== "undefined" && $tw.dragInProgress) return;

            // Skip for editable elements - let native handling work
            var target = (Number.isFinite(event.clientX) && Number.isFinite(event.clientY))
                ? document.elementFromPoint(event.clientX, event.clientY)
                : null;
            if (target && isEditableElement(target)) return;

            var hasFiles = false;
            var hasContent = false;
            var types = [];

            for (var i = 0; i < dt.types.length; i++) {
                var type = dt.types[i];
                types.push(type);
                if (type === "Files") {
                    if (dt.items && dt.items.length > 0) {
                        for (var j = 0; j < dt.items.length; j++) {
                            if (dt.items[j].kind === "file") {
                                hasFiles = true;
                                break;
                            }
                        }
                    }
                } else if (type === "text/plain" || type === "text/html" || type === "text/uri-list" ||
                           type === "TEXT" || type === "STRING" || type === "UTF8_STRING") {
                    hasContent = true;
                }
            }

            if (hasFiles && !hasContent) return;

            contentDragActive = true;
            contentDragTarget = (Number.isFinite(event.clientX) && Number.isFinite(event.clientY))
                ? (document.elementFromPoint(event.clientX, event.clientY) || document.body)
                : document.body;
            contentDragTypes = types;
            contentDragEnterCount = 1;
            currentTarget = contentDragTarget;

            event.preventDefault();

            var enterDt = createContentDataTransfer();
            var enterEvent = createSyntheticDragEvent("dragenter", { x: event.clientX, y: event.clientY }, enterDt, null);
            enterEvent.__tiddlyDesktopSynthetic = true;
            contentDragTarget.dispatchEvent(enterEvent);
        }, true);

        document.addEventListener("dragover", function(event) {
            if (event.__tiddlyDesktopSynthetic) return;
            if (!contentDragActive || nativeDragActive || isDragging) return;
            if (typeof $tw !== "undefined" && $tw.dragInProgress) return;

            // Skip for editable elements - let native handling work
            var target = (Number.isFinite(event.clientX) && Number.isFinite(event.clientY))
                ? document.elementFromPoint(event.clientX, event.clientY)
                : null;
            if (target && isEditableElement(target)) return;

            event.preventDefault();

            target = target || document.body;
            var oldTarget = contentDragTarget;
            contentDragTarget = target;
            currentTarget = target;

            var dt = createContentDataTransfer();
            var overEvent = createSyntheticDragEvent("dragover", { x: event.clientX, y: event.clientY }, dt, null);
            overEvent.__tiddlyDesktopSynthetic = true;
            target.dispatchEvent(overEvent);

            if (oldTarget && oldTarget !== target) {
                var leaveEvent = createSyntheticDragEvent("dragleave", { x: event.clientX, y: event.clientY }, dt, target);
                leaveEvent.__tiddlyDesktopSynthetic = true;
                oldTarget.dispatchEvent(leaveEvent);

                var enterEvent = createSyntheticDragEvent("dragenter", { x: event.clientX, y: event.clientY }, dt, oldTarget);
                enterEvent.__tiddlyDesktopSynthetic = true;
                target.dispatchEvent(enterEvent);
            }
        }, true);

        document.addEventListener("dragleave", function(event) {
            if (!contentDragActive || event.__tiddlyDesktopSynthetic || isDragging) return;

            // Skip for editable elements - let native handling work
            var target = (Number.isFinite(event.clientX) && Number.isFinite(event.clientY))
                ? document.elementFromPoint(event.clientX, event.clientY)
                : null;
            if (target && isEditableElement(target)) return;

            contentDragEnterCount--;
            if (contentDragEnterCount <= 0) {
                contentDragEnterCount = 0;
                cancelContentDrag("drag left window");
            }
        }, true);

        document.addEventListener("drop", function(event) {
            if (window.__tiddlyDesktopDragData || (typeof $tw !== "undefined" && $tw.dragInProgress)) return;
            if (event.__tiddlyDesktopSynthetic) return;
            if (isDragging) return;

            // Skip for editable elements - let native handling work
            var target = (Number.isFinite(event.clientX) && Number.isFinite(event.clientY))
                ? document.elementFromPoint(event.clientX, event.clientY)
                : null;
            if ((target && isEditableElement(target)) || event.__tdEditableDrop) return;

            if (nativeDragActive) {
                event.preventDefault();
                event.stopPropagation();
                return;
            }

            var dt = event.dataTransfer;
            if (!dt) return;

            var capturedData = { types: [], data: {}, files: [] };

            for (var i = 0; i < dt.types.length; i++) {
                var type = dt.types[i];
                capturedData.types.push(type);
                if (type !== "Files") {
                    try {
                        var rawData = dt.getData(type);
                        // Sanitize text data to fix encoding issues
                        capturedData.data[type] = sanitizeDroppedText(rawData);
                    } catch(e) {}
                }
            }

            if (dt.files && dt.files.length > 0) {
                for (var j = 0; j < dt.files.length; j++) {
                    capturedData.files.push(dt.files[j]);
                }
            }

            if (!nativeDragActive && contentDragActive && capturedData.types.length > 0 &&
                (Object.keys(capturedData.data).length > 0 || capturedData.files.length > 0)) {
                var hasActualContent = capturedData.files.length > 0 || Object.keys(capturedData.data).some(function(key) {
                    return capturedData.data[key] && capturedData.data[key].length > 0;
                });
                if (!hasActualContent) return;

                pendingContentDropData = capturedData;
                pendingContentDropPos = { x: event.clientX, y: event.clientY };

                event.preventDefault();
                event.stopPropagation();

                if (!isDragging) {
                    if (contentDropTimeout) clearTimeout(contentDropTimeout);
                    contentDropTimeout = setTimeout(function() {
                        if (pendingContentDropData) processContentDrop();
                    }, 50);
                }
            }
        }, true);

        // ========================================
        // Tauri Drag-Drop Event
        // ========================================

        listen("tauri://drag-drop", function(event) {
            invoke("js_log", { message: "tauri://drag-drop received on window: " + windowLabel + ", paths: " + JSON.stringify(event.payload.paths || []) });

            if (nativeDragActive) return;

            var paths = event.payload.paths || [];

            if (contentDropTimeout) {
                clearTimeout(contentDropTimeout);
                contentDropTimeout = null;
            }

            var isInternalDrag = (typeof $tw !== "undefined" && $tw.dragInProgress) ||
                (paths.length > 0 && paths.every(function(p) { return p.startsWith("data:"); }));

            if (isInternalDrag) {
                isDragging = false;
                pendingContentDropData = null;
                pendingContentDropPos = null;
                return;
            }

            if (paths.length === 0 && pendingContentDropData) {
                if (!pendingContentDropPos && event.payload.position) {
                    pendingContentDropPos = event.payload.position;
                }
                processContentDrop();
                return;
            }

            if (paths.length === 0 && contentDragActive) {
                contentDragActive = false;
                contentDragTarget = null;
                contentDragTypes = [];
            }

            pendingContentDropData = null;
            pendingContentDropPos = null;

            if (paths.length === 0) {
                pendingFilePaths = [];
                enteredTarget = null;
                currentTarget = null;
                isDragging = false;
                return;
            }

            // CRITICAL: Populate __pendingExternalFiles IMMEDIATELY, before any async operations.
            // On Windows, the native DOM drop may have already fired and TiddlyWiki may be
            // processing the import RIGHT NOW. The th-importing-file hook needs these paths
            // to be available synchronously.
            paths.forEach(function(filepath) {
                if (filepath && !filepath.startsWith("data:") && (filepath.startsWith("/") || filepath.match(/^[A-Za-z]:\\/))) {
                    var filename = filepath.split(/[/\\]/).pop();
                    window.__pendingExternalFiles[filename] = filepath;
                    invoke("js_log", { message: "tauri://drag-drop: IMMEDIATE set __pendingExternalFiles['" + filename + "'] = '" + filepath + "'" });
                }
            });

            // WINDOWS: Native HTML5 drop events fire automatically
            // Paths are already stored in __pendingExternalFiles from tauri://drag-enter
            // The th-importing-file hook will look them up by filename
            if (isWindows) {
                invoke("js_log", { message: "tauri://drag-drop: Windows - paths stored, native HTML5 handles drop" });
                return;
            }

            // Linux/macOS: Use synthetic events (native handling not reliable)
            var dropTarget = getTargetElement(event.payload.position);
            var pos = event.payload.position;

            var filePromises = paths.map(function(filepath) {
                if (filepath.startsWith("data:") || (!filepath.startsWith("/") && !filepath.match(/^[A-Za-z]:\\/))) {
                    return Promise.resolve(null);
                }

                var filename = filepath.split(/[/\\]/).pop();
                var mimeType = getMimeType(filename);

                return invoke("read_file_as_binary", { path: filepath }).then(function(bytes) {
                    // Path already set above, but ensure it's still there
                    window.__pendingExternalFiles[filename] = filepath;
                    return new File([new Uint8Array(bytes)], filename, { type: mimeType });
                }).catch(function(err) {
                    console.error("[TiddlyDesktop] Failed to read file:", filepath, err);
                    return null;
                });
            });

            Promise.all(filePromises).then(function(files) {
                var validFiles = files.filter(function(f) { return f !== null; });
                if (validFiles.length === 0) return;

                invoke("js_log", { message: "Processing " + validFiles.length + " files on window: " + windowLabel });

                var dt = new DataTransfer();
                validFiles.forEach(function(file) { dt.items.add(file); });

                if (currentTarget) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, null);
                    currentTarget.dispatchEvent(leaveEvent);
                }

                var dropEvent = createSyntheticDragEvent("drop", pos, dt);
                invoke("js_log", { message: "Dispatching drop event on window: " + windowLabel + " to target: " + dropTarget.tagName });
                dropTarget.dispatchEvent(dropEvent);

                var endEvent = createSyntheticDragEvent("dragend", pos, dt);
                document.body.dispatchEvent(endEvent);

                setTimeout(function() { window.__pendingExternalFiles = {}; }, 5000);
            });

            pendingFilePaths = [];
            enteredTarget = null;
            currentTarget = null;
            isDragging = false;
        });

        // ========================================
        // Keyboard and Focus Handlers
        // ========================================

        document.addEventListener("keydown", function(event) {
            if (event.key === "Escape") {
                if (isDragging) cancelExternalDrag("escape pressed");
                else if (contentDragActive) cancelContentDrag("escape pressed");
            }

            if ((event.key === "f" || event.key === "F") && (event.ctrlKey || event.metaKey)) {
                if (window.__IS_MAIN_WIKI__) {
                    event.preventDefault();
                    event.stopPropagation();
                }
            }
        }, true);

        document.addEventListener("keydown", function(event) {
            if ((event.key === "f" || event.key === "F") && (event.ctrlKey || event.metaKey)) {
                if (window.__IS_MAIN_WIKI__) return;
                if (event.defaultPrevented) return;

                if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
                    event.preventDefault();
                    window.__TAURI__.core.invoke('show_find_in_page').catch(function(err) {
                        console.log('[TiddlyDesktop] Find in page error:', err);
                    });
                }
            }
        }, false);

        window.addEventListener("blur", function(event) {
            if (isDragging) cancelExternalDrag("window lost focus");
            else if (contentDragActive) cancelContentDrag("window lost focus");
        }, true);

        // ========================================
        // File Input Interception
        // ========================================

        document.addEventListener('click', function(e) {
            var input = e.target;
            if (input.tagName === 'INPUT' && input.type === 'file') {
                e.preventDefault();
                e.stopPropagation();

                var multiple = input.hasAttribute('multiple');
                invoke('pick_files_for_import', { multiple: multiple }).then(function(paths) {
                    if (paths.length === 0) return;

                    var filePromises = paths.map(function(filepath) {
                        var filename = filepath.split(/[/\\]/).pop();
                        if (filename.toLowerCase().endsWith('.html') || filename.toLowerCase().endsWith('.htm')) {
                            return Promise.resolve(null);
                        }

                        window.__pendingExternalFiles[filename] = filepath;

                        return invoke('read_file_as_binary', { path: filepath }).then(function(bytes) {
                            var mimeType = getMimeType(filename);
                            return new File([new Uint8Array(bytes)], filename, { type: mimeType });
                        }).catch(function(err) {
                            console.error('[TiddlyDesktop] Failed to read file:', filepath, err);
                            return null;
                        });
                    });

                    Promise.all(filePromises).then(function(files) {
                        var validFiles = files.filter(function(f) { return f !== null; });
                        if (validFiles.length === 0) return;

                        var dt = new DataTransfer();
                        validFiles.forEach(function(file) { dt.items.add(file); });

                        input.files = dt.files;
                        input.dispatchEvent(new Event('change', { bubbles: true }));

                        setTimeout(function() { window.__pendingExternalFiles = {}; }, 5000);
                    });
                }).catch(function(err) {
                    console.error('[TiddlyDesktop] Failed to pick files:', err);
                });
            }
        }, true);

        // ========================================
        // Paste Handling for File URIs
        // ========================================

        document.addEventListener("paste", function(event) {
            if (window.__IS_MAIN_WIKI__) return;

            var clipboardData = event.clipboardData;
            if (!clipboardData) return;

            var uriList = sanitizeDroppedText(clipboardData.getData("text/uri-list"));
            if (!uriList) {
                var plainText = sanitizeDroppedText(clipboardData.getData("text/plain"));
                if (plainText && plainText.trim().startsWith("file://")) {
                    uriList = plainText;
                }
            }

            if (!uriList) return;

            var filePaths = uriList.split(/[\r\n]+/)
                .filter(function(line) { return line.trim() && line.charAt(0) !== String.fromCharCode(35); })
                .map(function(line) {
                    var trimmed = line.trim();
                    if (trimmed.startsWith("file://")) {
                        var path = trimmed.substring(7);
                        if (path.startsWith("//")) {
                            path = path.substring(2);
                            var slashIdx = path.indexOf("/");
                            if (slashIdx !== -1) path = path.substring(slashIdx);
                        }
                        try { return decodeURIComponent(path); } catch (e) { return path; }
                    }
                    return null;
                })
                .filter(function(p) { return p !== null; });

            if (filePaths.length === 0) return;

            invoke("js_log", { message: "Paste: detected " + filePaths.length + " file URI(s)" });

            event.preventDefault();
            event.stopPropagation();

            var filePromises = filePaths.map(function(filepath) {
                var filename = filepath.split("/").pop();
                var mimeType = getMimeType(filename);

                return invoke("read_file_as_binary", { path: filepath }).then(function(bytes) {
                    window.__pendingExternalFiles = window.__pendingExternalFiles || {};
                    window.__pendingExternalFiles[filename] = filepath;
                    return new File([new Uint8Array(bytes)], filename, { type: mimeType });
                }).catch(function(err) {
                    console.error("[TiddlyDesktop] Failed to read pasted file:", filepath, err);
                    return null;
                });
            });

            Promise.all(filePromises).then(function(files) {
                var validFiles = files.filter(function(f) { return f !== null; });
                if (validFiles.length === 0) return;

                var dropzone = document.querySelector(".tc-dropzone");
                if (!dropzone) {
                    console.error("[TiddlyDesktop] No dropzone found for pasted files");
                    return;
                }

                var dt = new DataTransfer();
                validFiles.forEach(function(file) { dt.items.add(file); });

                var dropEvent = new DragEvent("drop", {
                    bubbles: true,
                    cancelable: true,
                    dataTransfer: dt
                });
                dropEvent.__tiddlyDesktopSynthetic = true;

                try {
                    Object.defineProperty(dropEvent, 'dataTransfer', {
                        value: dt,
                        writable: false,
                        configurable: true
                    });
                } catch (e) {}

                dropzone.dispatchEvent(dropEvent);
                setTimeout(function() { window.__pendingExternalFiles = {}; }, 5000);
            });
        }, true);

        // ========================================
        // External Attachments Configuration
        // ========================================

        // Use $:/plugins/tiddlydesktop-rs/ prefix for shadow tiddlers
        var PLUGIN_SOURCE = "$:/plugins/tiddlydesktop-rs/injected";
        var CONFIG_PREFIX = "$:/plugins/tiddlydesktop-rs/external-attachments/";
        var CONFIG_ENABLE = CONFIG_PREFIX + "Enable";
        var CONFIG_ABS_DESC = CONFIG_PREFIX + "UseAbsoluteForDescendents";
        var CONFIG_ABS_NONDESC = CONFIG_PREFIX + "UseAbsoluteForNonDescendents";
        var CONFIG_SETTINGS_TAB = CONFIG_PREFIX + "settings";
        var ALL_CONFIG_TIDDLERS = [CONFIG_ENABLE, CONFIG_ABS_DESC, CONFIG_ABS_NONDESC, CONFIG_SETTINGS_TAB];

        // Helper to add a shadow tiddler (never saved with wiki)
        function addShadowTiddler(fields) {
            var tiddler = new $tw.Tiddler(fields);
            $tw.wiki.shadowTiddlers[fields.title] = {
                tiddler: tiddler,
                source: PLUGIN_SOURCE
            };
            $tw.wiki.clearCache(fields.title);
            $tw.wiki.enqueueTiddlerEvent(fields.title);
        }

        // Helper to delete a shadow tiddler
        function deleteShadowTiddler(title) {
            if ($tw.wiki.shadowTiddlers[title]) {
                delete $tw.wiki.shadowTiddlers[title];
                $tw.wiki.clearCache(title);
                $tw.wiki.enqueueTiddlerEvent(title);
            }
        }

        // Note: Global save hook is installed by session_auth.js

        function installImportHook() {
            if (typeof $tw === 'undefined' || !$tw.hooks) {
                setTimeout(installImportHook, 100);
                return;
            }

            $tw.hooks.addHook("th-importing-file", function(info) {
                var file = info.file;
                var filename = file.name;
                var type = info.type;

                // Debug logging for Windows troubleshooting
                invoke("js_log", { message: "th-importing-file hook: filename='" + filename + "', type='" + type + "', isBinary=" + info.isBinary });
                invoke("js_log", { message: "th-importing-file: __pendingExternalFiles keys: " + Object.keys(window.__pendingExternalFiles || {}).join(", ") });

                // Check if File object has a path property (Tauri/Electron sometimes provide this)
                if (file.path) {
                    invoke("js_log", { message: "th-importing-file: file.path available: " + file.path });
                    // Use file.path directly if available and not already in pendingExternalFiles
                    if (!window.__pendingExternalFiles[filename]) {
                        window.__pendingExternalFiles[filename] = file.path;
                    }
                }

                // Windows: Try to get paths from WRY patch FFI storage (fallback if tauri://drag-drop didn't populate)
                if (!window.__pendingExternalFiles[filename]) {
                    // This is async but we try it anyway - if paths were stored by WRY, they'll be available
                    invoke("get_external_drop_paths").then(function(paths) {
                        if (paths && paths.length > 0) {
                            invoke("js_log", { message: "th-importing-file: Got " + paths.length + " paths from FFI storage" });
                            paths.forEach(function(filepath) {
                                var pathFilename = filepath.split(/[/\\]/).pop();
                                if (!window.__pendingExternalFiles[pathFilename]) {
                                    window.__pendingExternalFiles[pathFilename] = filepath;
                                    invoke("js_log", { message: "th-importing-file: FFI populated __pendingExternalFiles['" + pathFilename + "']" });
                                }
                            });
                        }
                    }).catch(function() {});
                }

                var hasDeserializer = false;
                if ($tw.Wiki.tiddlerDeserializerModules) {
                    if ($tw.Wiki.tiddlerDeserializerModules[type]) {
                        hasDeserializer = true;
                    }
                    if (!hasDeserializer && $tw.utils.getFileExtensionInfo) {
                        var extInfo = $tw.utils.getFileExtensionInfo(type);
                        if (extInfo && $tw.Wiki.tiddlerDeserializerModules[extInfo.type]) {
                            hasDeserializer = true;
                        }
                    }
                    if (!hasDeserializer && $tw.config.contentTypeInfo && $tw.config.contentTypeInfo[type]) {
                        var deserializerType = $tw.config.contentTypeInfo[type].deserializerType;
                        if (deserializerType && $tw.Wiki.tiddlerDeserializerModules[deserializerType]) {
                            hasDeserializer = true;
                        }
                    }
                }

                if (hasDeserializer) {
                    console.log("[TiddlyDesktop] Deserializer found for type '" + type + "', letting TiddlyWiki5 handle import");
                    return false;
                }

                var externalEnabled = $tw.wiki.getTiddlerText(CONFIG_ENABLE, "yes") === "yes";
                var useAbsDesc = $tw.wiki.getTiddlerText(CONFIG_ABS_DESC, "no") === "yes";
                var useAbsNonDesc = $tw.wiki.getTiddlerText(CONFIG_ABS_NONDESC, "no") === "yes";

                var originalPath = window.__pendingExternalFiles && window.__pendingExternalFiles[filename];
                invoke("js_log", { message: "th-importing-file: originalPath=" + (originalPath || "null") + ", externalEnabled=" + externalEnabled });

                // Check if we're on Android (wiki path starts with content:// or is a JSON with content://)
                var isAndroid = wikiPath && (wikiPath.indexOf("content://") !== -1 || (wikiPath.charAt(0) === '{' && wikiPath.indexOf("content://") !== -1));
                invoke("js_log", { message: "th-importing-file: isAndroid=" + isAndroid });

                if (originalPath && externalEnabled && info.isBinary) {
                    if (isAndroid) {
                        // Android: Copy attachment to ./attachments/ folder
                        // This is async, so we handle it differently
                        invoke("android_copy_attachment", {
                            wikiUri: wikiPath,
                            sourceUri: originalPath,
                            filename: filename
                        }).then(function(relativePath) {
                            delete window.__pendingExternalFiles[filename];
                            console.log("[TiddlyDesktop] Android: Created attachment '" + filename + "' -> " + relativePath);
                            info.callback([{
                                title: filename,
                                type: info.type,
                                "_canonical_uri": relativePath
                            }]);
                        }).catch(function(err) {
                            console.error("[TiddlyDesktop] Android: Failed to copy attachment:", err);
                            // Fall back to embedding
                            delete window.__pendingExternalFiles[filename];
                            info.callback(null);
                        });
                        return true; // Signal we're handling it asynchronously
                    } else {
                        // Desktop: Use relative path to original file location
                        var canonicalUri = makePathRelative(originalPath, wikiPath, {
                            useAbsoluteForDescendents: useAbsDesc,
                            useAbsoluteForNonDescendents: useAbsNonDesc
                        });

                        delete window.__pendingExternalFiles[filename];

                        console.log("[TiddlyDesktop] Creating external attachment for '" + filename + "' -> " + canonicalUri);

                        info.callback([{
                            title: filename,
                            type: info.type,
                            "_canonical_uri": canonicalUri
                        }]);

                        return true;
                    }
                }

                // Android fallback: If no originalPath but we're on Android with a binary file,
                // read the file content and save it to attachments folder
                if (isAndroid && externalEnabled && info.isBinary && file) {
                    invoke("js_log", { message: "th-importing-file: Android fallback - reading file content for " + filename });

                    var reader = new FileReader();
                    reader.onload = function(e) {
                        // Convert ArrayBuffer to base64
                        var bytes = new Uint8Array(e.target.result);
                        var binary = '';
                        for (var i = 0; i < bytes.byteLength; i++) {
                            binary += String.fromCharCode(bytes[i]);
                        }
                        var base64 = btoa(binary);

                        invoke("android_save_attachment", {
                            wikiUri: wikiPath,
                            contentBase64: base64,
                            filename: filename
                        }).then(function(relativePath) {
                            console.log("[TiddlyDesktop] Android: Saved attachment '" + filename + "' -> " + relativePath);
                            info.callback([{
                                title: filename,
                                type: info.type,
                                "_canonical_uri": relativePath
                            }]);
                        }).catch(function(err) {
                            console.error("[TiddlyDesktop] Android: Failed to save attachment:", err);
                            // Fall back to embedding - return false/null to let TW handle it
                            info.callback(null);
                        });
                    };
                    reader.onerror = function() {
                        console.error("[TiddlyDesktop] Failed to read file content");
                        info.callback(null);
                    };
                    reader.readAsArrayBuffer(file);
                    return true; // Signal we're handling it asynchronously
                }

                return false;
            });

            console.log("[TiddlyDesktop] Import hook installed");
        }

        function saveConfigToTauri() {
            if (typeof $tw === 'undefined' || !$tw.wiki) return;

            var config = {
                enabled: $tw.wiki.getTiddlerText(CONFIG_ENABLE, "yes") === "yes",
                use_absolute_for_descendents: $tw.wiki.getTiddlerText(CONFIG_ABS_DESC, "no") === "yes",
                use_absolute_for_non_descendents: $tw.wiki.getTiddlerText(CONFIG_ABS_NONDESC, "no") === "yes"
            };

            invoke("set_external_attachments_config", { wikiPath: wikiPath, config: config })
                .catch(function(err) {
                    console.error("[TiddlyDesktop] Failed to save config:", err);
                });
        }

        function deleteConfigTiddlers() {
            if (typeof $tw === 'undefined' || !$tw.wiki) return;

            var originalNumChanges = $tw.saverHandler ? $tw.saverHandler.numChanges : 0;

            ALL_CONFIG_TIDDLERS.forEach(function(title) {
                deleteShadowTiddler(title);
                // Also delete any regular tiddler that may have been created by widget interaction
                if ($tw.wiki.tiddlerExists(title)) {
                    $tw.wiki.deleteTiddler(title);
                }
            });

            if ($tw.saverHandler) {
                setTimeout(function() {
                    $tw.saverHandler.numChanges = originalNumChanges;
                    $tw.saverHandler.updateDirtyStatus();
                }, 0);
            }
        }

        function injectConfigTiddlers(config) {
            if (typeof $tw === 'undefined' || !$tw.wiki || !$tw.wiki.shadowTiddlers || !$tw.saverHandler) {
                setTimeout(function() { injectConfigTiddlers(config); }, 100);
                return;
            }

            var originalNumChanges = $tw.saverHandler.numChanges || 0;

            // Use shadow tiddlers so settings are never saved with the wiki
            addShadowTiddler({
                title: CONFIG_ENABLE,
                text: config.enabled ? "yes" : "no"
            });
            addShadowTiddler({
                title: CONFIG_ABS_DESC,
                text: config.use_absolute_for_descendents ? "yes" : "no"
            });
            addShadowTiddler({
                title: CONFIG_ABS_NONDESC,
                text: config.use_absolute_for_non_descendents ? "yes" : "no"
            });

            addShadowTiddler({
                title: CONFIG_SETTINGS_TAB,
                caption: "External Attachments",
                tags: "$:/tags/ControlPanel/SettingsTab",
                text: "When importing binary files (images, PDFs, etc.) into this wiki, you can optionally store them as external references instead of embedding them.\n\n" +
                      "This keeps your wiki file smaller and allows the files to be edited externally.\n\n" +
                      "<$checkbox tiddler=\"" + CONFIG_ENABLE + "\" field=\"text\" checked=\"yes\" unchecked=\"no\" default=\"yes\"> Enable external attachments</$checkbox>\n\n" +
                      "<$checkbox tiddler=\"" + CONFIG_ABS_DESC + "\" field=\"text\" checked=\"yes\" unchecked=\"no\" default=\"no\"> Use absolute paths for files inside wiki folder</$checkbox>\n\n" +
                      "<$checkbox tiddler=\"" + CONFIG_ABS_NONDESC + "\" field=\"text\" checked=\"yes\" unchecked=\"no\" default=\"no\"> Use absolute paths for files outside wiki folder</$checkbox>"
            });

            setTimeout(function() {
                $tw.saverHandler.numChanges = originalNumChanges;
                $tw.saverHandler.updateDirtyStatus();
            }, 0);

            $tw.wiki.addEventListener("change", function(changes) {
                if (changes[CONFIG_ENABLE] || changes[CONFIG_ABS_DESC] || changes[CONFIG_ABS_NONDESC]) {
                    saveConfigToTauri();
                }
            });

            console.log("[TiddlyDesktop] External Attachments settings UI ready (using shadow tiddlers)");
        }

        function setupCleanup() {
            window.addEventListener("beforeunload", function() {
                saveConfigToTauri();
                deleteConfigTiddlers();
            });

            if (window.__TAURI__ && window.__TAURI__.event) {
                window.__TAURI__.event.listen("tauri://close-requested", function() {
                    saveConfigToTauri();
                    deleteConfigTiddlers();
                });
            }
        }

        invoke("get_external_attachments_config", { wikiPath: wikiPath })
            .then(function(config) {
                injectConfigTiddlers(config);
            })
            .catch(function(err) {
                console.error("[TiddlyDesktop] Failed to load config, using defaults:", err);
                injectConfigTiddlers({ enabled: true, use_absolute_for_descendents: false, use_absolute_for_non_descendents: false });
            });

        installImportHook();
        setupCleanup();

        // ========================================
        // Native Clipboard Paste Handler
        // ========================================

        var lastClickedElement = null;
        document.addEventListener("click", function(event) {
            lastClickedElement = event.target;
        }, true);

        document.addEventListener("paste", function(event) {
            if (window.__IS_MAIN_WIKI__) return;

            var target = event.target;
            if (target.tagName === "TEXTAREA" || target.tagName === "INPUT" || target.isContentEditable) {
                return;
            }

            if (event.twEditor) return;
            if (event.__tiddlyDesktopSynthetic) return;

            event.preventDefault();
            event.stopPropagation();

            invoke("js_log", { message: "Paste event intercepted, reading native clipboard" });

            invoke("get_clipboard_content").then(function(clipboardData) {
                if (!clipboardData || !clipboardData.types || clipboardData.types.length === 0) {
                    invoke("js_log", { message: "Clipboard is empty or unreadable" });
                    return;
                }

                invoke("js_log", { message: "Clipboard content types: " + JSON.stringify(clipboardData.types) });

                var dataMap = clipboardData.data || {};
                var typesList = clipboardData.types || [];

                var itemsArray = [];
                typesList.forEach(function(type) {
                    itemsArray.push({
                        kind: "string",
                        type: type,
                        getAsString: function(callback) {
                            if (typeof callback === "function") {
                                setTimeout(function() { callback(dataMap[type] || ""); }, 0);
                            }
                        },
                        getAsFile: function() { return null; }
                    });
                });

                var mockClipboardData = {
                    types: typesList,
                    items: itemsArray,
                    getData: function(type) { return dataMap[type] || ""; },
                    setData: function() {},
                    clearData: function() {}
                };

                var syntheticPaste = new ClipboardEvent("paste", {
                    bubbles: true,
                    cancelable: true,
                    composed: true
                });

                Object.defineProperty(syntheticPaste, "clipboardData", {
                    value: mockClipboardData,
                    writable: false
                });

                syntheticPaste.__tiddlyDesktopSynthetic = true;

                var pasteTarget = lastClickedElement || target;
                var dropzone = pasteTarget.closest ? pasteTarget.closest(".tc-dropzone") : null;

                if (dropzone) {
                    invoke("js_log", { message: "Dispatching synthetic paste to: " + pasteTarget.tagName + " (inside dropzone)" });
                    pasteTarget.dispatchEvent(syntheticPaste);
                    invoke("js_log", { message: "Synthetic paste dispatched, defaultPrevented=" + syntheticPaste.defaultPrevented });
                } else {
                    invoke("js_log", { message: "Last clicked element is not inside a dropzone - no import" });
                }
            }).catch(function(err) {
                invoke("js_log", { message: "Failed to read clipboard: " + err });
            });
        }, true);

        window.__TD_EXTERNAL_ATTACHMENTS_READY__ = true;
        console.log("[TiddlyDesktop] External attachments ready for:", wikiPath);
    }

    setupExternalAttachments();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
