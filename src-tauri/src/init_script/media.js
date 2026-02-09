// TiddlyDesktop Initialization Script - Media Enhancement Module
// Handles: Plyr video/audio player, PDF.js renderer, extra media type registration, video poster extraction

(function(TD) {
    'use strict';
    if (window.__IS_MAIN_WIKI__) return; // Skip landing page

    var TD_LIB_BASE = 'tdlib://localhost/';

    // ---- Section 0: Early CSS Injection ----
    // Inject Plyr CSS and video-hiding styles immediately (before body exists).
    // WebKitGTK doesn't load CSS from custom URI schemes via <link> tags,
    // so we inline it like we do for Plyr JS.
    (function() {
        var head = document.head || document.documentElement;
        // Plyr CSS (inlined from init_script.rs)
        if (window.__PLYR_CSS__) {
            var style = document.createElement('style');
            style.id = 'td-plyr-css';
            style.textContent = window.__PLYR_CSS__;
            head.appendChild(style);
        }
        // Video-hiding CSS: prevent flash of native controls before Plyr takes over
        // (same pattern as Android's WikiHttpServer injection)
        // Also ensure videos/Plyr containers fit within their parent container
        // (e.g. when transcluded into a smaller div)
        var hideStyle = document.createElement('style');
        hideStyle.id = 'td-plyr-hide-styles';
        hideStyle.textContent = 'video:not(.plyr__video-wrapper video){opacity:0!important;max-height:0!important;overflow:hidden!important;}audio{max-width:100%;box-sizing:border-box;}.plyr{width:100%;height:100%;}.plyr__video-wrapper{width:100%!important;height:100%!important;padding-bottom:0!important;will-change:transform;background:#000;}.plyr video{opacity:1!important;width:100%!important;height:100%!important;object-fit:contain!important;-webkit-transform:translateZ(0);transform:translateZ(0);-webkit-backface-visibility:hidden;backface-visibility:hidden;}.plyr--compact .plyr__control--overlaid{padding:10px!important;}.plyr--compact .plyr__control--overlaid svg{width:18px!important;height:18px!important;}.plyr--compact .plyr__time--duration,.plyr--compact [data-plyr="settings"],.plyr--compact .plyr__volume{display:none!important;}.plyr--compact .plyr__controls{padding:2px 5px!important;}.plyr--compact .plyr__control{padding:3px!important;}.plyr--compact .plyr__control svg{width:14px!important;height:14px!important;}.plyr--compact .plyr__progress__container{margin-left:4px!important;}.plyr--tiny .plyr__time,.plyr--tiny [data-plyr="fullscreen"]{display:none!important;}.plyr--tiny .plyr__control--overlaid{padding:6px!important;}.plyr--tiny .plyr__control--overlaid svg{width:14px!important;height:14px!important;}.plyr--tiny .plyr__control svg{width:12px!important;height:12px!important;}';
        head.appendChild(hideStyle);
    })();

    // ---- Section 1: Dynamic Library Loading ----

    function loadScript(src, cb) {
        var s = document.createElement('script');
        s.src = src;
        s.onload = cb || function(){};
        s.onerror = function(){ console.error('[TD-Media] Failed to load ' + src); };
        document.head.appendChild(s);
    }

    function loadCSS(href) {
        var l = document.createElement('link');
        l.rel = 'stylesheet';
        l.href = href;
        document.head.appendChild(l);
    }

    var plyrLoaded = false;
    var pdfjsLoaded = false;

    // Inject Plyr SVG sprite into DOM so inline icon references (#plyr-play etc.) work
    function injectPlyrSvg() {
        if (document.getElementById('td-plyr-sprite')) return;
        var svgContent = window.__PLYR_SVG_SPRITE__;
        if (!svgContent) return;
        // Strip XML declaration and DOCTYPE, keep only the <svg> element
        svgContent = svgContent.replace(/<\?xml[^?]*\?>/, '').replace(/<!DOCTYPE[^>]*>/, '');
        // Add hidden style and ID to the outer <svg>
        svgContent = svgContent.replace('<svg ', '<svg id="td-plyr-sprite" style="display:none;position:absolute;width:0;height:0;" ');
        if (document.body) {
            document.body.insertAdjacentHTML('afterbegin', svgContent);
        }
    }

    function ensurePlyr(cb) {
        // Plyr JS is included inline in the initialization script, so it should
        // be available immediately. Fall back to dynamic loading for folder wikis.
        if (plyrLoaded || typeof Plyr !== 'undefined') {
            plyrLoaded = true;
            console.log('[TD-Plyr] Plyr available');
            if (cb) cb();
            return;
        }
        // CSS is injected inline via Section 0 above. Load Plyr CSS dynamically
        // only if inline injection didn't happen (shouldn't occur in practice).
        if (!document.getElementById('td-plyr-css') && !document.querySelector('link[href*="plyr.css"]')) {
            loadCSS(TD_LIB_BASE + 'plyr/dist/plyr.css');
        }
        // Dynamic fallback (e.g. folder wikis where init script might differ)
        console.log('[TD-Plyr] Loading Plyr dynamically from:', TD_LIB_BASE + 'plyr/dist/plyr.min.js');
        loadScript(TD_LIB_BASE + 'plyr/dist/plyr.min.js', function() {
            if (typeof Plyr !== 'undefined') {
                plyrLoaded = true;
                console.log('[TD-Plyr] Plyr loaded (dynamic)');
                if (cb) cb();
            } else {
                console.error('[TD-Plyr] Failed to load Plyr');
            }
        });
    }

    function ensurePdfJs(cb) {
        if (pdfjsLoaded || typeof pdfjsLib !== 'undefined') {
            pdfjsLoaded = true;
            if (cb) cb();
            return;
        }
        // WebKitGTK doesn't execute <script src="tdlib://..."> tags, so we
        // fetch the JS as text and execute it manually via new Function().
        fetch(TD_LIB_BASE + 'pdfjs/build/pdf.min.js')
            .then(function(r) {
                if (!r.ok) throw new Error('HTTP ' + r.status);
                return r.text();
            })
            .then(function(code) {
                new Function(code)();
                if (typeof pdfjsLib !== 'undefined') {
                    // Worker also can't load from tdlib:// via new Worker(), so
                    // fetch + blob URL to create a same-origin worker.
                    fetch(TD_LIB_BASE + 'pdfjs/build/pdf.worker.min.js')
                        .then(function(r) { return r.blob(); })
                        .then(function(blob) {
                            pdfjsLib.GlobalWorkerOptions.workerSrc = URL.createObjectURL(blob);
                            pdfjsLoaded = true;
                            console.log('[TD-PDF] PDF.js loaded (fetch+eval, blob worker)');
                            if (cb) cb();
                        })
                        .catch(function() {
                            // Fallback: no worker (runs on main thread)
                            pdfjsLoaded = true;
                            console.log('[TD-PDF] PDF.js loaded (fetch+eval, no worker)');
                            if (cb) cb();
                        });
                }
            })
            .catch(function(err) {
                console.error('[TD-PDF] Failed to load PDF.js:', err);
            });
    }

    // ---- Section 1b: Source Readiness Check ----
    // External attachments start with raw relative paths (e.g. "video.mp4") that
    // filesystem.js later converts to tdasset:// or HTTP URLs. Plyr must not initialize
    // until the src is valid, otherwise it captures the broken URL.
    function hasPendingSrcTransform(el) {
        var src = el.getAttribute('src') || '';
        if (!src) return false;
        // Already a valid protocol — no conversion needed
        if (/^(data:|tdasset:|https?:|blob:|tdlib:)/.test(src)) return false;
        // Element is waiting for async media server registration
        if (el.__tdMediaPending) return true;

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
                el.setAttribute('src', 'tdasset://localhost/' + encodeURIComponent(resolved));
                return false;
            }
        }

        return true; // Still pending — resolver not ready yet
    }

    // ---- Section 2: Plyr Video Enhancement ----

    function applyPoster(el, posterUrl) {
        el.setAttribute('poster', posterUrl);
        var plyrContainer = el.closest('.plyr');
        if (plyrContainer) {
            var posterDiv = plyrContainer.querySelector('.plyr__poster');
            if (posterDiv) {
                posterDiv.style.backgroundImage = 'url(' + posterUrl + ')';
                posterDiv.removeAttribute('hidden');
            }
        }
        if (el.plyr) el.plyr.poster = posterUrl;
    }

    // Add compact/tiny classes based on Plyr container's rendered size.
    // Actual sizing is CSS-only: .plyr fills parent, object-fit:contain on video.
    function fitPlyrToParent(video) {
        var plyrEl = video.closest('.plyr');
        if (!plyrEl) return;
        plyrEl.classList.remove('plyr--compact', 'plyr--tiny');
        var w = plyrEl.clientWidth, h = plyrEl.clientHeight;
        if (w < 350 || h < 250) {
            plyrEl.classList.add('plyr--compact');
        }
        if (w < 200 || h < 150) {
            plyrEl.classList.add('plyr--tiny');
        }
    }

    var plyrOpts = {
        controls: ['play-large','play','progress','current-time','duration','mute','volume','settings','fullscreen'],
        settings: ['speed'],
        speed: { selected: 1, options: [0.5, 0.75, 1, 1.25, 1.5, 2] },
        iconUrl: '',
        blankVideo: ''
    };

    function enhanceVideo(el) {
        if (el.__tdPlyrDone) return;

        if (typeof Plyr === 'undefined') return;

        // Wait for filesystem.js to convert relative src to tdasset://
        if (hasPendingSrcTransform(el)) {
            if (!el.__tdPlyrRetry) el.__tdPlyrRetry = 0;
            el.__tdPlyrRetry++;
            if (el.__tdPlyrRetry < 100) { // Up to 10s
                setTimeout(function() { enhanceVideo(el); }, 100);
            }
            return;
        }

        el.__tdPlyrDone = true;

        // Small delay to ensure DOM is settled
        setTimeout(function() {
            // Element may have been removed from DOM by TiddlyWiki re-render
            if (!el.parentNode) return;

            var videoSrc = el.src || (el.querySelector('source') ? el.querySelector('source').src : null);

            // On desktop, allow full preloading so GStreamer buffers aggressively.
            // This prevents flicker after seeking — with "metadata" only the moov atom
            // is fetched, leaving GStreamer with an empty buffer after each seek.
            // "auto" tells the browser to download as much as possible ahead of playback.
            el.setAttribute('preload', 'auto');

            try {
                new Plyr(el, plyrOpts);
                // Plyr dispatches CustomEvent({type:'error', bubbles:true}) on its wrapper
                // when the video source fails. Stop these from bubbling to TW's error handler.
                var plyrWrap = el.closest('.plyr');
                if (plyrWrap) {
                    plyrWrap.addEventListener('error', function(e) {
                        if (e instanceof CustomEvent) e.stopPropagation();
                    });
                }

                // Fit Plyr within parent constraints (respects both width + height)
                if (el.videoWidth && el.videoHeight) {
                    requestAnimationFrame(function() { fitPlyrToParent(el); });
                } else {
                    el.addEventListener('loadedmetadata', function() {
                        requestAnimationFrame(function() { fitPlyrToParent(el); });
                    }, { once: true });
                }

                // Fix GStreamer replay flicker on Linux: after playback ends,
                // re-load the video source so the pipeline is flushed cleanly.
                el.addEventListener('ended', function() {
                    var src = el.currentSrc || el.src;
                    if (src) {
                        el.src = src;
                        el.load();
                    }
                });

} catch(err) {
                console.error('[TD-Plyr] Error:', err && err.message ? err.message : err);
            }

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

    // ---- Section 3: Plyr Audio Enhancement ----

    function enhanceAudio(el) {
        if (el.__tdPlyrDone) return;

        if (typeof Plyr === 'undefined') return;

        // Wait for filesystem.js to convert relative src to tdasset://
        if (hasPendingSrcTransform(el)) {
            if (!el.__tdPlyrRetry) el.__tdPlyrRetry = 0;
            el.__tdPlyrRetry++;
            if (el.__tdPlyrRetry < 100) { // Up to 10s
                setTimeout(function() { enhanceAudio(el); }, 100);
            }
            return;
        }

        el.__tdPlyrDone = true;

        try {
            new Plyr(el, {
                controls: ['play','progress','current-time','duration','mute','volume','settings'],
                settings: ['speed'],
                speed: { selected: 1, options: [0.5, 0.75, 1, 1.25, 1.5, 2] },
                iconUrl: ''
            });
            var plyrWrap = el.closest('.plyr');
            if (plyrWrap) {
                plyrWrap.addEventListener('error', function(e) {
                    if (e instanceof CustomEvent) e.stopPropagation();
                });
            }
        } catch(err) {
            console.error('[TD-Plyr] Audio error:', err && err.message ? err.message : err);
        }
    }

    // ---- Section 4: PDF.js Renderer ----

    function getPdfSrc(el) {
        var tag = el.tagName.toLowerCase();
        var src = el.getAttribute('src') || el.getAttribute('data') || '';
        if (tag === 'object') src = el.getAttribute('data') || src;
        if (!src) return null;
        if (src.toLowerCase().indexOf('.pdf') === -1 &&
            (el.getAttribute('type') || '').toLowerCase() !== 'application/pdf') return null;
        return src;
    }

    function replacePdfElement(el) {
        if (el.__tdPdfDone) return;
        el.__tdPdfDone = true;
        var src = getPdfSrc(el);
        if (!src) return;

        if (typeof pdfjsLib === 'undefined' || !pdfjsLib.getDocument) {
            el.__tdPdfDone = false;
            return;
        }

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
            style.textContent = '.td-pdf-btn{background:#555;color:#fff;border:none;border-radius:3px;padding:4px 10px;font-size:14px;cursor:pointer;min-width:32px;}.td-pdf-btn:active{background:#777;}.td-pdf-pages-wrap{overflow-y:auto;-webkit-overflow-scrolling:touch;}.td-pdf-page-wrap{display:flex;justify-content:center;padding:8px 0;}.td-pdf-page-wrap canvas{background:#fff;box-shadow:0 2px 8px rgba(0,0,0,.3);display:block;max-width:none;}';
            document.head.appendChild(style);
        }

        // Scrollable page area
        var pagesWrap = document.createElement('div');
        pagesWrap.className = 'td-pdf-pages-wrap';
        pagesWrap.style.cssText = 'max-height:80vh;overflow-y:auto;-webkit-overflow-scrolling:touch;';
        container.appendChild(pagesWrap);

        el.parentNode.replaceChild(container, el);

        var scale = 1.5;
        var pdfDoc = null;
        var pageCanvases = [];
        var pageInfo = toolbar.querySelector('.td-pdf-pageinfo');

        function renderPage(num, canvas) {
            pdfDoc.getPage(num).then(function(page) {
                var viewport = page.getViewport({ scale: scale });
                canvas.width = viewport.width;
                canvas.height = viewport.height;
                var ctx = canvas.getContext('2d');
                page.render({ canvasContext: ctx, viewport: viewport });
            });
        }

        function renderAll() {
            pageCanvases.forEach(function(c, i) { renderPage(i + 1, c); });
        }

        function fitWidth() {
            if (!pdfDoc) return;
            pdfDoc.getPage(1).then(function(page) {
                var vp = page.getViewport({ scale: 1 });
                var containerWidth = pagesWrap.clientWidth - 16;
                scale = containerWidth / vp.width;
                if (scale < 0.5) scale = 0.5;
                if (scale > 5) scale = 5;
                renderAll();
            });
        }

        pdfjsLib.getDocument({ url: src, cMapUrl: TD_LIB_BASE + 'pdfjs/cmaps/', cMapPacked: true }).promise.then(function(pdf) {
            pdfDoc = pdf;
            var total = pdf.numPages;
            pageInfo.textContent = total + ' page' + (total !== 1 ? 's' : '');

            for (var p = 1; p <= total; p++) {
                var wrap = document.createElement('div');
                wrap.className = 'td-pdf-page-wrap';
                var canvas = document.createElement('canvas');
                wrap.appendChild(canvas);
                pagesWrap.appendChild(wrap);
                pageCanvases.push(canvas);
            }

            fitWidth();

            // Lazy rendering with IntersectionObserver
            if (typeof IntersectionObserver !== 'undefined') {
                var observer = new IntersectionObserver(function(entries) {
                    entries.forEach(function(entry) {
                        if (entry.isIntersecting) {
                            var idx = pageCanvases.indexOf(entry.target);
                            if (idx >= 0) renderPage(idx + 1, entry.target);
                        }
                    });
                }, { root: pagesWrap, rootMargin: '200px' });
                pageCanvases.forEach(function(c) { observer.observe(c); });
            }
        }).catch(function(err) {
            console.error('[TD-PDF] Error loading PDF:', err);
            pagesWrap.innerHTML = '<p style="color:#f88;padding:20px;text-align:center;">Failed to load PDF: ' + err.message + '</p>';
        });

        toolbar.addEventListener('click', function(e) {
            var btn = e.target.closest('[data-action]');
            if (!btn) return;
            var action = btn.getAttribute('data-action');
            if (action === 'zoomin') { scale = Math.min(scale * 1.25, 5); renderAll(); }
            else if (action === 'zoomout') { scale = Math.max(scale / 1.25, 0.5); renderAll(); }
            else if (action === 'fitwidth') { fitWidth(); }
            else if (action === 'prev') { pagesWrap.scrollBy(0, -pagesWrap.clientHeight * 0.8); }
            else if (action === 'next') { pagesWrap.scrollBy(0, pagesWrap.clientHeight * 0.8); }
        });
    }

    // ---- Section 5: Video Poster Extraction (via ffmpeg) ----

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
                console.warn('[TD-Plyr] Poster extraction failed:', err);
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
            setTimeout(drainVideoQueue, 2000);
        }
    }

    function drainVideoQueue() {
        if (videoQueue.length === 0) { videoQueueRunning = false; return; }
        videoQueueRunning = true;
        var task = videoQueue.shift();
        task(function() { drainVideoQueue(); });
    }

    // ---- Section 6: Extra Audio/Video Type Registration ----

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
                    preload: {type: "string", value: "auto"},
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
                    preload: {type: "string", value: "auto"},
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

    // ---- Section 7: MutationObserver + Scan ----

    function scanAll() {
        if (plyrLoaded && typeof Plyr !== 'undefined') {
            document.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
            document.querySelectorAll('audio').forEach(function(el) { enhanceAudio(el); });
        }
        if (pdfjsLoaded && typeof pdfjsLib !== 'undefined') {
            document.querySelectorAll('embed, object, iframe').forEach(function(el) {
                if (getPdfSrc(el)) replacePdfElement(el);
            });
        }
    }

    function setupObserver() {
        if (window.__tdMediaObserverSet) return;
        window.__tdMediaObserverSet = true;

        var obs = new MutationObserver(function(mutations) {
            mutations.forEach(function(m) {
                m.addedNodes.forEach(function(node) {
                    if (node.nodeType !== 1) return;
                    var tag = node.tagName ? node.tagName.toLowerCase() : '';
                    if (tag === 'video' && plyrLoaded) {
                        enhanceVideo(node);
                    } else if (tag === 'audio' && plyrLoaded) {
                        enhanceAudio(node);
                    } else if ((tag === 'embed' || tag === 'object' || tag === 'iframe') && pdfjsLoaded) {
                        if (getPdfSrc(node)) replacePdfElement(node);
                    }
                    if (node.querySelectorAll) {
                        if (plyrLoaded) {
                            node.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
                            node.querySelectorAll('audio').forEach(function(el) { enhanceAudio(el); });
                        }
                        if (pdfjsLoaded) {
                            node.querySelectorAll('embed, object, iframe').forEach(function(el) {
                                if (getPdfSrc(el)) replacePdfElement(el);
                            });
                        }
                    }
                });
            });
        });

        obs.observe(document.body, { childList: true, subtree: true });
    }

    // ---- Section 8: Init ----

    function init() {
        if (!document.body) {
            setTimeout(init, 50);
            return;
        }

        injectPlyrSvg();
        setupObserver();

        ensurePlyr(function() {
            scanAll();
        });

        ensurePdfJs(function() {
            scanAll();
        });

        registerExtraMediaTypes();

        console.log('[TD-Media] Media enhancement initialized');
    }

    init();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
