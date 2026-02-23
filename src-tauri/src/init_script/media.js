// TiddlyDesktop Initialization Script - Media Enhancement Module
// Handles: native video/audio styling, PDFium renderer, extra media type registration, video poster extraction

(function(TD) {
    'use strict';
    if (window.__IS_MAIN_WIKI__) return; // Skip landing page

    var TD_LIB_BASE = 'tdlib://localhost/';

    // Helper: check if a URL is an external (non-wiki-server) URL
    function isExternalUrl(url) {
        if (!url || typeof url !== 'string') return false;
        if (url.startsWith('http://127.0.0.1')) return false;
        if (url.startsWith('http://localhost')) return false;
        return url.startsWith('http://') || url.startsWith('https://') || url.startsWith('//');
    }

    // Custom URI schemes (wikifile://) are rejected as invalid origins by
    // external embed services (YouTube, SoundCloud, etc.). On Linux, the media
    // server provides an /embed proxy endpoint that wraps external URLs in a
    // valid HTTP page. External iframe src attributes are rewritten to point
    // to the proxy so the embed loads from a proper HTTP origin.

    function toProxyUrl(src) {
        if (!window.__TD_EMBED_PORT__) return null;
        var prefix = 'http://127.0.0.1:' + window.__TD_EMBED_PORT__ + '/embed';
        if (src.indexOf(prefix) === 0) return null; // Already proxied
        return prefix + '?url=' + encodeURIComponent(src);
    }

    // Early interception: patch setAttribute to proxy external src BEFORE load.
    // This catches iframes created via JavaScript (setAttribute('src', ...)).
    var _origIframeSetAttr = HTMLIFrameElement.prototype.setAttribute;
    HTMLIFrameElement.prototype.setAttribute = function(name, value) {
        if (name.toLowerCase() === 'src' && isExternalUrl(value)) {
            var proxied = toProxyUrl(value);
            if (proxied) {
                return _origIframeSetAttr.call(this, name, proxied);
            }
        }
        return _origIframeSetAttr.call(this, name, value);
    };

    function fixExternalIframe(iframe) {
        var src = iframe.getAttribute('src') || '';
        if (!isExternalUrl(src)) return;
        var proxied = toProxyUrl(src);
        if (proxied) {
            _origIframeSetAttr.call(iframe, 'src', proxied);
        }
    }

    // ---- Section 0: Early CSS Injection ----
    // Inject media controls CSS immediately (before body exists).
    // WebKitGTK doesn't load CSS from custom URI schemes via <link> tags,
    // so we inline it.
    (function() {
        var head = document.head || document.documentElement;
        if (window.__MEDIA_CONTROLS_CSS__) {
            var style = document.createElement('style');
            style.id = 'td-media-controls-css';
            style.textContent = window.__MEDIA_CONTROLS_CSS__;
            head.appendChild(style);
        }
    })();

    // ---- Section 1: Source Readiness Check ----
    // External attachments start with raw relative paths (e.g. "video.mp4") that
    // filesystem.js later converts to tdasset:// or HTTP URLs. Media enhancement must not
    // initialize until the src is valid, otherwise it captures the broken URL.
    function hasPendingSrcTransform(el) {
        // Check prototype intercept pending status first — element may have no
        // src attribute yet because the intercept prevented the original setAttribute
        if (el.__tdMediaPending) return true;
        var src = el.getAttribute('src') || '';
        if (!src) return false;
        // Already a valid protocol — no conversion needed
        if (/^(data:|tdasset:|https?:|blob:|tdlib:)/.test(src)) return false;

        // src needs conversion — try to do it ourselves using filesystem.js's path resolver
        var TD = window.TiddlyDesktop;
        if (TD && TD.resolveFilesystemPath) {
            var rawPath = src;
            if (src.startsWith('wikifile://')) {
                rawPath = src.replace(/^wikifile:\/\/localhost\//, '');
                if (!rawPath) return true;
            }
            var resolved = TD.resolveFilesystemPath(rawPath);
            if (resolved) {
                // In folder wiki mode, register with media server (tdasset:// blocked from HTTP origin)
                if (window.__TD_FOLDER_WIKI__ && window.__TD_MEDIA_SERVER__ &&
                    window.__TAURI__ && window.__TAURI__.core) {
                    el.__tdMediaPending = true;
                    window.__TAURI__.core.invoke('register_media_url', { path: resolved })
                        .then(function(httpUrl) {
                            TD.mediaTokenPaths = TD.mediaTokenPaths || {};
                            TD.mediaTokenPaths[httpUrl] = resolved;
                            el.setAttribute('src', httpUrl);
                            el.__tdMediaPending = false;
                            if (el.load) el.load();
                        }).catch(function() {
                            el.__tdMediaPending = false;
                        });
                    return true; // pending — async registration in progress
                }
                el.setAttribute('src', 'tdasset://localhost/' + encodeURIComponent(resolved));
                return false;
            }
        }

        return true; // Still pending — resolver not ready yet
    }

    // ---- Section 2: Video Enhancement ----

    function applyPoster(el, posterUrl) {
        el.setAttribute('poster', posterUrl);
    }

    function enhanceVideo(el) {
        if (el.__tdMediaDone) return;

        // Wait for filesystem.js to convert relative src to tdasset://
        if (hasPendingSrcTransform(el)) {
            if (!el.__tdMediaRetry) el.__tdMediaRetry = 0;
            el.__tdMediaRetry++;
            if (el.__tdMediaRetry < 100) { // Up to 10s
                setTimeout(function() { enhanceVideo(el); }, 100);
            }
            return;
        }

        el.__tdMediaDone = true;

        // Small delay to ensure DOM is settled
        setTimeout(function() {
            // Element may have been removed from DOM by TiddlyWiki re-render
            if (!el.parentNode) return;

            var videoSrc = el.src || (el.querySelector('source') ? el.querySelector('source').src : null);

            // Start with "metadata" — downloads just the moov atom (~50-100KB),
            // enabling duration display and fast play start without downloading
            // the full video. Critical for galleries with many videos.
            // Upgrade to "auto" once playback starts so GStreamer buffers
            // aggressively (prevents flicker after seeking on Linux).
            el.setAttribute('preload', 'metadata');
            el.addEventListener('playing', function onPlay() {
                el.removeEventListener('playing', onPlay);
                el.setAttribute('preload', 'auto');
            });

            // Fix GStreamer replay flicker on Linux: after playback ends,
            // re-load the video source so the pipeline is flushed cleanly.
            el.addEventListener('ended', function() {
                var src = el.currentSrc || el.src;
                if (src) {
                    el.src = src;
                    el.load();
                }
            });

            // Queue poster extraction via ffmpeg
            if (videoSrc) {
                enqueueVideoWork(function(done) {
                    var fsPath = resolveVideoPath(videoSrc);
                    if (!fsPath) { done(); return; }

                    extractPoster(el, fsPath, done);
                });
            }
        }, 50);
    }

    // ---- Section 3: PDFium Renderer ----

    // Detect PDF rendering backend: Android uses @JavascriptInterface,
    // desktop uses Tauri invoke commands
    function getPdfBackend() {
        if (window.TiddlyDesktopPdf) return 'android';
        if (window.__TAURI__ && window.__TAURI__.core) return 'desktop';
        return null;
    }

    function pdfApiOpen(dataBase64, src) {
        var backend = getPdfBackend();
        if (backend === 'android') {
            return new Promise(function(resolve, reject) {
                try {
                    var result = window.TiddlyDesktopPdf.open(dataBase64);
                    resolve(JSON.parse(result));
                } catch (e) { reject(e); }
            });
        }
        // Desktop: for tdasset:// on WebKitGTK, use pdf_open_file to avoid cross-scheme fetch
        if (src && src.startsWith('tdasset://localhost/') && location.protocol === 'wikifile:') {
            var encoded = src.substring('tdasset://localhost/'.length);
            var fsPath;
            try { fsPath = decodeURIComponent(encoded); } catch(e) { fsPath = encoded; }
            return window.__TAURI__.core.invoke('pdf_open_file', { path: fsPath });
        }
        return window.__TAURI__.core.invoke('pdf_open', { dataBase64: dataBase64 });
    }

    function pdfApiRenderPage(handle, pageNum, widthPx) {
        var backend = getPdfBackend();
        if (backend === 'android') {
            return new Promise(function(resolve, reject) {
                try {
                    var result = window.TiddlyDesktopPdf.renderPage(handle, pageNum, widthPx);
                    resolve(JSON.parse(result));
                } catch (e) { reject(e); }
            });
        }
        return window.__TAURI__.core.invoke('pdf_render_page', { handle: handle, pageNum: pageNum, widthPx: widthPx });
    }

    function pdfApiClose(handle) {
        var backend = getPdfBackend();
        if (backend === 'android') {
            try { window.TiddlyDesktopPdf.close(handle); } catch(e) {}
            return;
        }
        if (window.__TAURI__ && window.__TAURI__.core) {
            window.__TAURI__.core.invoke('pdf_close', { handle: handle }).catch(function(){});
        }
    }

    function pdfApiCharAtPos(handle, pageNum, pixelX, pixelY, renderWidth) {
        var backend = getPdfBackend();
        if (backend === 'android') {
            return new Promise(function(resolve) {
                try {
                    resolve(window.TiddlyDesktopPdf.charAtPos(handle, pageNum, pixelX, pixelY, renderWidth));
                } catch(e) { resolve(-1); }
            });
        }
        return window.__TAURI__.core.invoke('pdf_char_at_pos', {
            handle: handle, pageNum: pageNum, pixelX: pixelX, pixelY: pixelY, renderWidth: renderWidth
        });
    }

    function pdfApiSelectionRects(handle, pageNum, startIdx, endIdx, renderWidth) {
        var backend = getPdfBackend();
        if (backend === 'android') {
            return new Promise(function(resolve) {
                try {
                    var result = window.TiddlyDesktopPdf.selectionRects(handle, pageNum, startIdx, endIdx, renderWidth);
                    resolve(JSON.parse(result));
                } catch(e) { resolve([]); }
            });
        }
        return window.__TAURI__.core.invoke('pdf_selection_rects', {
            handle: handle, pageNum: pageNum, startIdx: startIdx, endIdx: endIdx, renderWidth: renderWidth
        });
    }

    function pdfApiGetText(handle, pageNum, startIdx, endIdx) {
        var backend = getPdfBackend();
        if (backend === 'android') {
            return new Promise(function(resolve) {
                try {
                    resolve(window.TiddlyDesktopPdf.getText(handle, pageNum, startIdx, endIdx));
                } catch(e) { resolve(''); }
            });
        }
        return window.__TAURI__.core.invoke('pdf_get_text', {
            handle: handle, pageNum: pageNum, startIdx: startIdx, endIdx: endIdx
        });
    }

    // Track all open PDF handles for cleanup
    var openPdfHandles = [];

    function getPdfSrc(el) {
        var tag = el.tagName.toLowerCase();
        var src = el.getAttribute('src') || el.getAttribute('data') || '';
        if (tag === 'object') src = el.getAttribute('data') || src;
        if (!src) return null;
        var srcLower = src.toLowerCase();
        if (srcLower.indexOf('.pdf') === -1 &&
            (el.getAttribute('type') || '').toLowerCase() !== 'application/pdf' &&
            srcLower.indexOf('data:application/pdf') !== 0) return null;
        return src;
    }

    function fetchPdfBytes(src) {
        // data: URI — decode directly
        if (src.startsWith('data:')) {
            var commaIdx = src.indexOf(',');
            if (commaIdx < 0) return Promise.reject(new Error('Invalid data URI'));
            var b64 = src.substring(commaIdx + 1);
            return Promise.resolve(b64);
        }
        // tdasset:// on WebKitGTK — can't cross-scheme fetch, return src for pdf_open_file path
        if (src.startsWith('tdasset://localhost/') && location.protocol === 'wikifile:') {
            return Promise.resolve(null); // Signal to use pdf_open_file via src
        }
        // HTTP or tdasset:// on non-WebKitGTK — fetch as ArrayBuffer
        return fetch(src).then(function(r) {
            if (!r.ok) throw new Error('HTTP ' + r.status);
            return r.arrayBuffer();
        }).then(function(ab) {
            // Convert to base64
            var bytes = new Uint8Array(ab);
            var binary = '';
            for (var i = 0; i < bytes.length; i++) {
                binary += String.fromCharCode(bytes[i]);
            }
            return btoa(binary);
        });
    }

    function replacePdfElement(el) {
        if (el.__tdPdfDone) return;
        if (!getPdfBackend()) return;

        // On first encounter, save original src and immediately blank the element
        // to prevent native PDF loading. On Windows, WebView2's built-in PDF viewer
        // would otherwise render the PDF inside the iframe before our PDFium renderer
        // can take over, causing visual conflicts and wasted resources.
        if (!el.__tdPdfOrigSrc) {
            var detectedSrc = getPdfSrc(el);
            if (!detectedSrc) return;
            el.__tdPdfOrigSrc = detectedSrc;
            // Blank element to prevent native loading.
            // Remove attributes rather than setting to about:blank, so filesystem.js's
            // observer skips the element (it checks !src and returns early).
            var tag = el.tagName.toLowerCase();
            if (tag === 'iframe' || tag === 'embed') { el.removeAttribute('src'); el.removeAttribute('type'); }
            else if (tag === 'object') el.removeAttribute('data');
            // Immediately hide to prevent any flash of native viewer
            el.style.display = 'none';
        }

        var src = el.__tdPdfOrigSrc;

        // Resolve raw filesystem paths to tdasset:// URLs (inline, without modifying element)
        if (!/^(data:|tdasset:|https?:|blob:|tdlib:)/.test(src)) {
            if (el.__tdMediaPending) {
                // Async media server registration in progress — retry
                if (!el.__tdPdfRetry) el.__tdPdfRetry = 0;
                el.__tdPdfRetry++;
                if (el.__tdPdfRetry < 100) {
                    setTimeout(function() { replacePdfElement(el); }, 100);
                }
                return;
            }
            var TD = window.TiddlyDesktop;
            if (TD && TD.resolveFilesystemPath) {
                var rawPath = src;
                if (src.startsWith('wikifile://')) {
                    rawPath = src.replace(/^wikifile:\/\/localhost\//, '');
                }
                var resolved = rawPath ? TD.resolveFilesystemPath(rawPath) : null;
                if (resolved) {
                    if (window.__TD_FOLDER_WIKI__ && window.__TD_MEDIA_SERVER__ &&
                        window.__TAURI__ && window.__TAURI__.core) {
                        // Folder wiki: register with media server for HTTP URL
                        el.__tdMediaPending = true;
                        window.__TAURI__.core.invoke('register_media_url', { path: resolved })
                            .then(function(httpUrl) {
                                TD.mediaTokenPaths = TD.mediaTokenPaths || {};
                                TD.mediaTokenPaths[httpUrl] = resolved;
                                el.__tdPdfOrigSrc = httpUrl;
                                el.__tdMediaPending = false;
                                replacePdfElement(el);
                            }).catch(function() {
                                el.__tdMediaPending = false;
                                // Fall through with tdasset:// URL
                                el.__tdPdfOrigSrc = 'tdasset://localhost/' + encodeURIComponent(resolved);
                                replacePdfElement(el);
                            });
                        return;
                    }
                    src = 'tdasset://localhost/' + encodeURIComponent(resolved);
                }
                // If resolution fails, proceed with raw src — fetchPdfBytes will handle it
            } else {
                // Resolver not ready — retry
                if (!el.__tdPdfRetry) el.__tdPdfRetry = 0;
                el.__tdPdfRetry++;
                if (el.__tdPdfRetry < 100) {
                    setTimeout(function() { replacePdfElement(el); }, 100);
                }
                return;
            }
        }

        el.__tdPdfDone = true;

        var container = document.createElement('div');
        container.className = 'td-pdf-container';
        container.style.cssText = 'width:100%;max-width:100%;overflow:auto;background:#525659;padding:8px 0;border-radius:4px;position:relative;';

        // Toolbar
        var toolbar = document.createElement('div');
        toolbar.style.cssText = 'display:flex;align-items:center;justify-content:center;gap:8px;padding:6px 8px;background:#333;color:#fff;font:13px sans-serif;border-radius:4px 4px 0 0;flex-wrap:wrap;position:sticky;top:0;z-index:10;';
        toolbar.innerHTML =
            '<button class="td-pdf-btn" data-action="prev" title="Previous page">&#9664;</button>' +
            '<span class="td-pdf-pageinfo">- / -</span>' +
            '<button class="td-pdf-btn" data-action="next" title="Next page">&#9654;</button>' +
            '<span style="margin:0 4px">|</span>' +
            '<button class="td-pdf-btn" data-action="zoomout" title="Zoom out">&#8722;</button>' +
            '<button class="td-pdf-btn" data-action="fitwidth" title="Fit width">Fit</button>' +
            '<button class="td-pdf-btn" data-action="zoomin" title="Zoom in">&#43;</button>';
        container.appendChild(toolbar);

        // PDF styles (once)
        if (!document.querySelector('#td-pdf-styles')) {
            var style = document.createElement('style');
            style.id = 'td-pdf-styles';
            style.textContent =
                '.td-pdf-btn{background:#555;color:#fff;border:none;border-radius:3px;padding:4px 10px;font-size:14px;cursor:pointer;min-width:32px;}' +
                '.td-pdf-btn:active{background:#777;}' +
                '.td-pdf-pages-wrap{overflow-y:auto;-webkit-overflow-scrolling:touch;}' +
                '.td-pdf-page-wrap{display:flex;justify-content:center;margin:8px 0;position:relative;}' +
                '.td-pdf-page-wrap img{background:#fff;box-shadow:0 2px 8px rgba(0,0,0,.3);display:block;max-width:none;cursor:text;}' +
                '.td-pdf-sel-layer{position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none;z-index:1;}' +
                '.td-pdf-sel-rect{position:absolute;background:rgba(0,100,255,0.3);pointer-events:none;}';
            document.head.appendChild(style);
        }

        // Scrollable page area
        var pagesWrap = document.createElement('div');
        pagesWrap.className = 'td-pdf-pages-wrap';
        pagesWrap.style.cssText = 'max-height:80vh;overflow-y:auto;-webkit-overflow-scrolling:touch;';
        container.appendChild(pagesWrap);

        // Keep original element in DOM (hidden) — TiddlyWiki's virtual DOM
        // expects it to stay. Replacing it causes TW to re-create it on the next
        // refresh cycle, triggering an infinite loop. display:none was set earlier
        // when we first detected the PDF to prevent native viewer loading.
        el.parentNode.insertBefore(container, el.nextSibling);

        var pdfHandle = null;
        var pageSizes = [];
        var pageCount = 0;
        var scale = 1.0;
        var pageWraps = [];
        var pageCharBounds = {}; // charBounds per page (flat Float32Array)
        var renderedPages = {};
        var userZoomed = false;
        var lastContainerWidth = 0;
        var pageInfo = toolbar.querySelector('.td-pdf-pageinfo');
        // Selection state
        var selAnchorPage = -1, selAnchorIdx = -1;
        var selActivePage = -1, selActiveIdx = -1;
        var selDragging = false;
        var selCopiedText = '';

        function getTargetWidthPx() {
            var containerWidth = pagesWrap.clientWidth - 16;
            if (containerWidth <= 0) containerWidth = 800;
            var dpr = window.devicePixelRatio || 1;
            return Math.floor(containerWidth * scale * dpr);
        }

        // Hit-test: find char index at bitmap pixel (x,y) using charBounds
        function hitTestChar(pageNum, bitmapX, bitmapY) {
            var bounds = pageCharBounds[pageNum];
            if (!bounds) return -1;
            var count = bounds.length / 4;
            for (var i = 0; i < count; i++) {
                var bx = bounds[i * 4], by = bounds[i * 4 + 1];
                var bw = bounds[i * 4 + 2], bh = bounds[i * 4 + 3];
                if (bw <= 0 || bh <= 0) continue;
                if (bitmapX >= bx && bitmapX <= bx + bw && bitmapY >= by && bitmapY <= by + bh) {
                    return i;
                }
            }
            // No exact hit — find nearest char by distance
            var bestIdx = -1, bestDist = Infinity;
            for (var i = 0; i < count; i++) {
                var bx = bounds[i * 4], by = bounds[i * 4 + 1];
                var bw = bounds[i * 4 + 2], bh = bounds[i * 4 + 3];
                if (bw <= 0 || bh <= 0) continue;
                var cx = bx + bw / 2, cy = by + bh / 2;
                var dx = Math.max(bx - bitmapX, 0, bitmapX - (bx + bw));
                var dy = Math.max(by - bitmapY, 0, bitmapY - (by + bh));
                var dist = dx * dx + dy * dy;
                if (dist < bestDist && dist < 400) { // Max ~20px tolerance
                    bestDist = dist;
                    bestIdx = i;
                }
            }
            return bestIdx;
        }

        // Compute merged selection rects from charBounds (line-merged)
        function computeSelectionRects(pageNum, startIdx, endIdx) {
            var bounds = pageCharBounds[pageNum];
            if (!bounds) return [];
            var lo = Math.min(startIdx, endIdx), hi = Math.max(startIdx, endIdx);
            // Collect char rects in the range
            var rects = [];
            for (var i = lo; i <= hi; i++) {
                var bx = bounds[i * 4], by = bounds[i * 4 + 1];
                var bw = bounds[i * 4 + 2], bh = bounds[i * 4 + 3];
                if (bw <= 0 || bh <= 0) continue;
                rects.push({ x: bx, y: by, w: bw, h: bh });
            }
            if (rects.length === 0) return [];
            // Merge into line segments: group by overlapping Y range
            var merged = [rects[0]];
            for (var i = 1; i < rects.length; i++) {
                var r = rects[i];
                var last = merged[merged.length - 1];
                // Same line: Y overlap > 50% of smaller height
                var overlapY = Math.min(last.y + last.h, r.y + r.h) - Math.max(last.y, r.y);
                var minH = Math.min(last.h, r.h);
                if (overlapY > minH * 0.5) {
                    // Merge horizontally
                    var nx = Math.min(last.x, r.x);
                    var ny = Math.min(last.y, r.y);
                    var nr = Math.max(last.x + last.w, r.x + r.w);
                    var nb = Math.max(last.y + last.h, r.y + r.h);
                    last.x = nx; last.y = ny; last.w = nr - nx; last.h = nb - ny;
                } else {
                    merged.push({ x: r.x, y: r.y, w: r.w, h: r.h });
                }
            }
            return merged;
        }

        function clearSelection() {
            selAnchorPage = -1; selAnchorIdx = -1;
            selActivePage = -1; selActiveIdx = -1;
            selCopiedText = '';
            pageWraps.forEach(function(wrap) {
                var layer = wrap.querySelector('.td-pdf-sel-layer');
                if (layer) layer.innerHTML = '';
            });
        }

        function drawSelectionHighlights(pageNum, startIdx, endIdx) {
            var wrap = pageWraps[pageNum];
            if (!wrap) return;
            var layer = wrap.querySelector('.td-pdf-sel-layer');
            if (!layer) return;
            layer.innerHTML = '';
            if (startIdx < 0 || endIdx < 0) return;
            var dpr = window.devicePixelRatio || 1;
            var cssScale = 1.0 / dpr;
            var rects = computeSelectionRects(pageNum, startIdx, endIdx);
            rects.forEach(function(r) {
                var div = document.createElement('div');
                div.className = 'td-pdf-sel-rect';
                div.style.left = (r.x * cssScale) + 'px';
                div.style.top = (r.y * cssScale) + 'px';
                div.style.width = (r.w * cssScale) + 'px';
                div.style.height = (r.h * cssScale) + 'px';
                layer.appendChild(div);
            });
        }

        function updateSelection() {
            if (selAnchorPage < 0 || selActivePage < 0) return;
            if (selAnchorPage !== selActivePage) return;
            drawSelectionHighlights(selAnchorPage, selAnchorIdx, selActiveIdx);
        }

        function renderPage(pageNum) {
            var widthPx = getTargetWidthPx();
            var key = pageNum + ':' + widthPx;
            if (renderedPages[key]) return Promise.resolve();
            renderedPages[key] = true;

            return pdfApiRenderPage(pdfHandle, pageNum, widthPx).then(function(result) {
                var wrap = pageWraps[pageNum];
                if (!wrap) return;
                wrap.innerHTML = '';

                // Store char bounds for this page (used by JS hit-testing)
                if (result.charBounds) {
                    pageCharBounds[pageNum] = result.charBounds;
                }

                var pageSize = pageSizes[pageNum];
                var containerWidth = pagesWrap.clientWidth - 16;
                var displayWidth = containerWidth * scale;

                var inner = document.createElement('div');
                inner.style.cssText = 'position:relative;display:inline-block;';
                wrap.appendChild(inner);

                var img = document.createElement('img');
                img.src = 'data:image/png;base64,' + result.imageBase64;
                if (pageSize) {
                    var aspect = pageSize.h / pageSize.w;
                    img.style.width = Math.floor(displayWidth) + 'px';
                    img.style.height = Math.floor(displayWidth * aspect) + 'px';
                }
                img.style.display = 'block';
                img.setAttribute('data-page', pageNum);
                inner.appendChild(img);

                var selLayer = document.createElement('div');
                selLayer.className = 'td-pdf-sel-layer';
                inner.appendChild(selLayer);
            }).catch(function(err) {
                console.error('[TD-PDF] Failed to render page ' + pageNum + ':', err);
            });
        }

        function renderVisiblePages() {
            var wrapRect = pagesWrap.getBoundingClientRect();
            var margin = wrapRect.height;
            pageWraps.forEach(function(wrap, i) {
                var rect = wrap.getBoundingClientRect();
                if (rect.bottom >= wrapRect.top - margin && rect.top <= wrapRect.bottom + margin) {
                    renderPage(i);
                }
            });
        }

        function clearRenderedCache() {
            renderedPages = {};
            pageCharBounds = {};
        }

        function renderAll() {
            clearRenderedCache();
            renderVisiblePages();
        }

        function fitWidth() {
            var w = pagesWrap.clientWidth;
            if (w <= 0) {
                requestAnimationFrame(function() { fitWidth(); });
                return;
            }
            userZoomed = false;
            lastContainerWidth = w;
            scale = 1.0;
            renderAll();
        }

        // Open the PDF
        fetchPdfBytes(src).then(function(b64OrNull) {
            if (b64OrNull === null) {
                return pdfApiOpen(null, src);
            }
            return pdfApiOpen(b64OrNull, src);
        }).then(function(result) {
            pdfHandle = result.handle;
            pageCount = result.pageCount;
            pageSizes = result.pageSizes;
            openPdfHandles.push(pdfHandle);

            pageInfo.textContent = pageCount + ' page' + (pageCount !== 1 ? 's' : '');

            for (var p = 0; p < pageCount; p++) {
                var wrap = document.createElement('div');
                wrap.className = 'td-pdf-page-wrap';
                var ps = pageSizes[p];
                if (ps) {
                    var containerWidth = pagesWrap.clientWidth - 16 || 800;
                    var aspect = ps.h / ps.w;
                    wrap.style.minHeight = Math.floor(containerWidth * aspect) + 'px';
                }
                pagesWrap.appendChild(wrap);
                pageWraps.push(wrap);
            }

            fitWidth();

            pagesWrap.addEventListener('scroll', function() {
                renderVisiblePages();
            });

            if (typeof ResizeObserver !== 'undefined') {
                var resizeTimer;
                new ResizeObserver(function() {
                    var w = pagesWrap.clientWidth;
                    if (w > 0 && w !== lastContainerWidth) {
                        lastContainerWidth = w;
                        if (!userZoomed) {
                            clearTimeout(resizeTimer);
                            resizeTimer = setTimeout(fitWidth, 100);
                        } else {
                            clearTimeout(resizeTimer);
                            resizeTimer = setTimeout(renderAll, 100);
                        }
                    }
                }).observe(container);
            }
        }).catch(function(err) {
            console.error('[TD-PDF] Error loading PDF:', err);
            pagesWrap.innerHTML = '<p style="color:#f88;padding:20px;text-align:center;">Failed to load PDF: ' + (err.message || err) + '</p>';
        });

        toolbar.addEventListener('click', function(e) {
            var btn = e.target.closest('[data-action]');
            if (!btn) return;
            var action = btn.getAttribute('data-action');
            if (action === 'zoomin') { userZoomed = true; scale = Math.min(scale * 1.25, 5); clearSelection(); renderAll(); }
            else if (action === 'zoomout') { userZoomed = true; scale = Math.max(scale / 1.25, 0.3); clearSelection(); renderAll(); }
            else if (action === 'fitwidth') { clearSelection(); fitWidth(); }
            else if (action === 'prev') { pagesWrap.scrollBy(0, -pagesWrap.clientHeight * 0.8); }
            else if (action === 'next') { pagesWrap.scrollBy(0, pagesWrap.clientHeight * 0.8); }
        });

        // --- Selection via mouse (all JS, no round-trips) ---
        function getPagePixelFromEvent(e, img) {
            var rect = img.getBoundingClientRect();
            var dpr = window.devicePixelRatio || 1;
            return { x: Math.round((e.clientX - rect.left) * dpr), y: Math.round((e.clientY - rect.top) * dpr) };
        }

        function findPageFromEvent(e) {
            var target = e.target;
            if (target.tagName !== 'IMG' || !target.hasAttribute('data-page')) return null;
            return { pageNum: parseInt(target.getAttribute('data-page'), 10), img: target };
        }

        pagesWrap.addEventListener('mousedown', function(e) {
            if (e.button !== 0) return;
            var hit = findPageFromEvent(e);
            if (!hit) return;

            clearSelection();
            selDragging = true;
            var pixel = getPagePixelFromEvent(e, hit.img);
            var idx = hitTestChar(hit.pageNum, pixel.x, pixel.y);
            if (idx >= 0) {
                selAnchorPage = hit.pageNum;
                selAnchorIdx = idx;
                selActivePage = hit.pageNum;
                selActiveIdx = idx;
            }
            e.preventDefault();
        });

        document.addEventListener('mousemove', function(e) {
            if (!selDragging || selAnchorPage < 0) return;
            var imgs = pagesWrap.querySelectorAll('img[data-page]');
            var bestImg = null, bestPage = -1;
            for (var i = 0; i < imgs.length; i++) {
                var rect = imgs[i].getBoundingClientRect();
                if (e.clientY >= rect.top && e.clientY <= rect.bottom) {
                    bestImg = imgs[i];
                    bestPage = parseInt(imgs[i].getAttribute('data-page'), 10);
                    break;
                }
            }
            if (!bestImg) {
                bestPage = selAnchorPage;
                var anchorWrap = pageWraps[selAnchorPage];
                if (anchorWrap) bestImg = anchorWrap.querySelector('img[data-page]');
            }
            if (!bestImg || bestPage < 0) return;

            var pixel = getPagePixelFromEvent(e, bestImg);
            var idx = hitTestChar(bestPage, pixel.x, pixel.y);
            if (idx >= 0) {
                selActivePage = bestPage;
                selActiveIdx = idx;
                updateSelection();
            }
        });

        document.addEventListener('mouseup', function() {
            if (!selDragging) return;
            selDragging = false;
            if (selAnchorPage >= 0 && selActivePage >= 0 && selAnchorPage === selActivePage && selAnchorIdx !== selActiveIdx) {
                pdfApiGetText(pdfHandle, selAnchorPage, selAnchorIdx, selActiveIdx).then(function(text) {
                    selCopiedText = text;
                });
            }
        });

        // Copy handler
        container.addEventListener('copy', function(e) {
            if (selCopiedText) {
                e.preventDefault();
                e.clipboardData.setData('text/plain', selCopiedText);
            }
        });

        container.setAttribute('tabindex', '-1');
        container.style.outline = 'none';
        pagesWrap.addEventListener('mousedown', function() {
            container.focus();
        }, true);

        // Cleanup on container removal
        container.__tdPdfHandle = function() { return pdfHandle; };
    }

    // ---- Section 4: Video Poster Extraction (via ffmpeg) ----

    function resolveVideoPath(src) {
        // HTTP media server URLs: look up filesystem path via token map
        if (src.startsWith('http://127.0.0.1')) {
            var TD = window.TiddlyDesktop;
            if (TD && TD.mediaTokenPaths && TD.mediaTokenPaths[src]) {
                return TD.mediaTokenPaths[src];
            }
            return null;
        }
        // Convert tdasset:// URLs back to filesystem paths for poster extraction
        if (src.startsWith('tdasset://localhost/')) {
            var encoded = src.substring('tdasset://localhost/'.length);
            try {
                return decodeURIComponent(encoded);
            } catch(e) {
                return null;
            }
        }
        return null;
    }

    function extractPoster(el, fsPath, done) {
        if (typeof window.__TAURI__ === 'undefined' || !window.__TAURI__.core) {
            done();
            return;
        }
        window.__TAURI__.core.invoke('extract_video_poster', { path: fsPath })
            .then(function(dataUri) {
                if (dataUri) {
                    applyPoster(el, dataUri);
                }
                done();
            })
            .catch(function(err) {
                console.warn('[TD-Media] Poster extraction failed:', err);
                done();
            });
    }

    // Sequential queue to avoid overwhelming ffmpeg
    var videoQueue = [];
    var videoQueueRunning = false;

    function enqueueVideoWork(work) {
        videoQueue.push(work);
        if (!videoQueueRunning) {
            videoQueueRunning = true;
            setTimeout(drainVideoQueue, 500);
        }
    }

    function drainVideoQueue() {
        if (videoQueue.length === 0) { videoQueueRunning = false; return; }
        videoQueueRunning = true;
        var task = videoQueue.shift();
        task(function() { drainVideoQueue(); });
    }

    // ---- Section 5: Extra Audio/Video Type Registration ----

    function registerExtraMediaTypes() {
        if (typeof $tw === 'undefined' || !$tw.Wiki || !$tw.Wiki.parsers || !$tw.utils || !$tw.utils.registerFileType) {
            setTimeout(registerExtraMediaTypes, 200);
            return;
        }

        // Audio types missing from core
        $tw.utils.registerFileType("audio/wav","base64",[".wav",".wave"]);
        $tw.utils.registerFileType("audio/flac","base64",".flac");
        $tw.utils.registerFileType("audio/aac","base64",".aac");
        $tw.utils.registerFileType("audio/webm","base64",".weba");
        $tw.utils.registerFileType("audio/opus","base64",".opus");
        $tw.utils.registerFileType("audio/aiff","base64",[".aiff",".aif"]);
        // Video types missing from core
        $tw.utils.registerFileType("video/quicktime","base64",".mov");
        $tw.utils.registerFileType("video/x-matroska","base64",".mkv");
        $tw.utils.registerFileType("video/3gpp","base64",".3gp");

        // Audio parser for types not covered by core's audioparser.js
        var ExtraAudioParser = function(type, text, options) {
            var element = {
                type: "element",
                tag: "audio",
                attributes: {
                    controls: {type: "string", value: "controls"},
                    preload: {type: "string", value: "metadata"},
                    style: {type: "string", value: "width: 100%; object-fit: contain"}
                }
            };
            if (options._canonical_uri) {
                element.attributes.src = {type: "string", value: options._canonical_uri};
            } else if (text) {
                element.attributes.src = {type: "string", value: "data:" + type + ";base64," + text};
            }
            this.tree = [element];
            this.source = text;
            this.type = type;
        };

        // Video parser for types not covered by core's videoparser.js
        var ExtraVideoParser = function(type, text, options) {
            var element = {
                type: "element",
                tag: "video",
                attributes: {
                    controls: {type: "string", value: "controls"},
                    preload: {type: "string", value: "metadata"},
                    style: {type: "string", value: "width: 100%; object-fit: contain"}
                }
            };
            if (options._canonical_uri) {
                element.attributes.src = {type: "string", value: options._canonical_uri};
            } else if (text) {
                element.attributes.src = {type: "string", value: "data:" + type + ";base64," + text};
            }
            this.tree = [element];
            this.source = text;
            this.type = type;
        };

        var audioTypes = ["audio/wav","audio/wave","audio/x-wav","audio/flac",
                          "audio/aac","audio/webm","audio/opus","audio/aiff","audio/x-aiff"];
        var videoTypes = ["video/quicktime","video/x-matroska","video/3gpp"];

        audioTypes.forEach(function(t) {
            if (!$tw.Wiki.parsers[t]) {
                $tw.Wiki.parsers[t] = ExtraAudioParser;
            }
        });
        videoTypes.forEach(function(t) {
            if (!$tw.Wiki.parsers[t]) {
                $tw.Wiki.parsers[t] = ExtraVideoParser;
            }
        });

        // Refresh tiddlers with newly-registered media types so they render correctly
        // (parsers were registered after TW's initial render, so these tiddlers may have failed)
        var changedTiddlers = {};
        $tw.wiki.each(function(tiddler, title) {
            var type = tiddler.fields.type;
            if (type && $tw.Wiki.parsers[type] &&
                (type.indexOf("audio/") === 0 || type.indexOf("video/") === 0)) {
                $tw.wiki.clearCache(title);
                changedTiddlers[title] = {modified: true};
            }
        });
        var keys = Object.keys(changedTiddlers);
        if (keys.length > 0) {
            console.log('[TD-Media] Refreshing ' + keys.length + ' media tiddlers');
            $tw.rootWidget.refresh(changedTiddlers);
        }

        console.log('[TD-Media] Extra media types registered');
    }

    // ---- Section 6: MutationObserver + Scan ----

    function scanAll() {
        document.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
        // PDF elements: always scan (PDFium is always available if backend exists)
        if (getPdfBackend()) {
            document.querySelectorAll('embed, object, iframe').forEach(function(el) {
                if (getPdfSrc(el)) replacePdfElement(el);
            });
        }
        // Rewrite external iframes that bypassed the prototype interception
        // (e.g. from HTML parser) to use the embed proxy
        document.querySelectorAll('iframe[src]').forEach(function(el) { fixExternalIframe(el); });
    }

    function setupObserver() {
        if (window.__tdMediaObserverSet) return;
        window.__tdMediaObserverSet = true;

        var hasPdf = !!getPdfBackend();

        var obs = new MutationObserver(function(mutations) {
            mutations.forEach(function(m) {
                m.addedNodes.forEach(function(node) {
                    if (node.nodeType !== 1) return;
                    var tag = node.tagName ? node.tagName.toLowerCase() : '';
                    if (tag === 'video') {
                        enhanceVideo(node);
                    } else if ((tag === 'embed' || tag === 'object' || tag === 'iframe') && hasPdf) {
                        if (getPdfSrc(node)) replacePdfElement(node);
                    }
                    if (tag === 'iframe') fixExternalIframe(node);
                    if (node.querySelectorAll) {
                        node.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
                        if (hasPdf) {
                            node.querySelectorAll('embed, object, iframe').forEach(function(el) {
                                if (getPdfSrc(el)) replacePdfElement(el);
                            });
                        }
                        node.querySelectorAll('iframe[src]').forEach(function(el) { fixExternalIframe(el); });
                    }
                });
            });
        });

        obs.observe(document.body, { childList: true, subtree: true });
    }

    // ---- Section 7: Init ----

    // Cleanup all open PDF handles on page unload
    window.addEventListener('beforeunload', function() {
        openPdfHandles.forEach(function(h) {
            pdfApiClose(h);
        });
        openPdfHandles = [];
    });

    function init() {
        if (!document.body) {
            setTimeout(init, 50);
            return;
        }

        setupObserver();

        // Scan for existing elements
        scanAll();

        registerExtraMediaTypes();

        console.log('[TD-Media] Media enhancement initialized');
    }

    init();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
