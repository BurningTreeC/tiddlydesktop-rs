// TiddlyDesktop Initialization Script - Filesystem Module
// Handles: httpRequest override for filesystem paths, copyToClipboard override, media interceptor

(function(TD) {
    'use strict';

    // ---- Early prototype intercept for media server ----
    // When the media server is active (Linux: GStreamer needs HTTP URLs;
    // folder wikis: tdasset:// blocked from HTTP origin), this intercept prevents
    // the browser from ever attempting to load media from non-HTTP sources by
    // catching src assignments at the prototype level, BEFORE the browser can start
    // its media pipeline. Must run before any video/audio elements are created.
    if (window.__TD_MEDIA_SERVER__) {
        (function() {
            var origVideoSetAttr = HTMLVideoElement.prototype.setAttribute;
            var origAudioSetAttr = HTMLAudioElement.prototype.setAttribute;
            var origSourceSetAttr = HTMLSourceElement.prototype.setAttribute;

            var pendingQueue = [];
            var resolverReady = false;
            var _invoke = null;
            var _resolvePath = null;

            // Check if a src value needs to be redirected through the media server.
            // Returns false for URLs that are already routed correctly.
            function needsRedirect(value) {
                if (!value || typeof value !== 'string') return false;
                if (value.startsWith('http://') || value.startsWith('https://')) return false;
                if (value.startsWith('data:') || value.startsWith('blob:')) return false;
                return true;
            }

            // Extract the raw filesystem path from various URL formats
            function extractPath(value) {
                if (value.startsWith('wikifile://')) {
                    return value.replace(/^wikifile:\/\/localhost\//, '') || null;
                }
                if (value.startsWith('tdasset://localhost/')) {
                    try { return decodeURIComponent(value.substring('tdasset://localhost/'.length)); }
                    catch(e) { return null; }
                }
                // Raw relative or absolute filesystem path
                return value;
            }

            function processElement(el, origSetAttr, rawSrc) {
                var rawPath = extractPath(rawSrc);
                if (!rawPath) {
                    el.__tdMediaPending = false;
                    origSetAttr.call(el, 'src', rawSrc);
                    return;
                }

                var resolvedPath = _resolvePath(rawPath);
                if (!resolvedPath) {
                    el.__tdMediaPending = false;
                    origSetAttr.call(el, 'src', rawSrc);
                    return;
                }

                _invoke('register_media_url', { path: resolvedPath })
                    .then(function(httpUrl) {
                        TD.mediaTokenPaths = TD.mediaTokenPaths || {};
                        TD.mediaTokenPaths[httpUrl] = resolvedPath;
                        origSetAttr.call(el, 'src', httpUrl);
                        el.__tdMediaPending = false;
                        // Trigger load for the media element
                        var mediaEl = (el.tagName === 'SOURCE') ? el.parentElement : el;
                        if (mediaEl && mediaEl.load) mediaEl.load();
                    })
                    .catch(function(err) {
                        console.warn('[TiddlyDesktop] Media intercept fallback:', resolvedPath, err);
                        origSetAttr.call(el, 'src', 'tdasset://localhost/' + encodeURIComponent(resolvedPath));
                        el.__tdMediaPending = false;
                    });
            }

            function interceptSetAttribute(origSetAttr) {
                return function(name, value) {
                    if (name.toLowerCase() !== 'src' || !needsRedirect(value)) {
                        return origSetAttr.call(this, name, value);
                    }

                    // For <source>, only intercept if parent is a media element
                    if (this.tagName === 'SOURCE' && (!this.parentElement ||
                        (this.parentElement.tagName !== 'VIDEO' && this.parentElement.tagName !== 'AUDIO'))) {
                        return origSetAttr.call(this, name, value);
                    }

                    // Mark as pending — prevents MutationObserver and media.js from interfering
                    this.__tdMediaPending = true;

                    if (resolverReady) {
                        processElement(this, origSetAttr, value);
                    } else {
                        pendingQueue.push({ el: this, origSetAttr: origSetAttr, rawSrc: value });
                    }
                    // Don't call original — prevents wikifile:// load
                };
            }

            HTMLVideoElement.prototype.setAttribute = interceptSetAttribute(origVideoSetAttr);
            HTMLAudioElement.prototype.setAttribute = interceptSetAttribute(origAudioSetAttr);
            HTMLSourceElement.prototype.setAttribute = interceptSetAttribute(origSourceSetAttr);

            // Also intercept .src property on HTMLMediaElement to catch direct assignments
            // (video.src = "path" bypasses setAttribute)
            var srcDesc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'src');
            if (srcDesc && srcDesc.set) {
                var origSrcSet = srcDesc.set;
                Object.defineProperty(HTMLMediaElement.prototype, 'src', {
                    get: srcDesc.get,
                    set: function(value) {
                        if (needsRedirect(value)) {
                            this.setAttribute('src', value);
                        } else {
                            origSrcSet.call(this, value);
                        }
                    },
                    configurable: true,
                    enumerable: true
                });
            }

            // Activation callback for when Tauri + path resolver become available
            TD._activateMediaIntercept = function(invoke, resolvePath) {
                _invoke = invoke;
                _resolvePath = resolvePath;
                resolverReady = true;
                var queue = pendingQueue;
                pendingQueue = [];
                queue.forEach(function(item) {
                    processElement(item.el, item.origSetAttr, item.rawSrc);
                });
            };

            console.log('[TiddlyDesktop] Media prototype intercept installed');
        })();
    }

    function setupFilesystemSupport() {
        if (typeof window.__TAURI__ === 'undefined' || !window.__TAURI__.core) {
            setTimeout(setupFilesystemSupport, 100);
            return;
        }

        function waitForTiddlyWiki() {
            if (typeof $tw === 'undefined' || !$tw.wiki || !$tw.utils || !$tw.utils.httpRequest) {
                setTimeout(waitForTiddlyWiki, 100);
                return;
            }

            var invoke = window.__TAURI__.core.invoke;
            var wikiPath = window.__WIKI_PATH__ || '';
            var isFolderWiki = !!window.__TD_FOLDER_WIKI__;

            function isUrl(path) {
                if (!path || typeof path !== 'string') return false;
                return path.startsWith('http:') || path.startsWith('https:') ||
                       path.startsWith('data:') || path.startsWith('blob:') ||
                       path.startsWith('file:');
            }

            function isAbsolutePath(path) {
                if (!path || typeof path !== 'string') return false;
                if (path.startsWith('/')) return true;
                if (path.length >= 3 && path[1] === ':' && (path[2] === '\\' || path[2] === '/')) return true;
                return false;
            }

            function isFilesystemPath(path) {
                if (!path || typeof path !== 'string') return false;
                if (isUrl(path)) return false;
                return true;
            }

            function normalizePath(path) {
                var separator = path.indexOf('\\') >= 0 ? '\\' : '/';
                var parts = path.split(/[/\\]/);
                var result = [];
                for (var i = 0; i < parts.length; i++) {
                    var part = parts[i];
                    if (part === '..') {
                        if (result.length > 0 && result[result.length - 1] !== '') {
                            result.pop();
                        }
                    } else if (part !== '.' && part !== '') {
                        result.push(part);
                    } else if (part === '' && i === 0) {
                        result.push('');
                    }
                }
                return result.join(separator);
            }

            function resolveFilesystemPath(path) {
                if (isAbsolutePath(path)) {
                    return normalizePath(path);
                }
                if (!wikiPath) {
                    console.warn('[TiddlyDesktop] Cannot resolve relative path without __WIKI_PATH__:', path);
                    return null;
                }
                var basePath = wikiPath;
                if (basePath.endsWith('.html') || basePath.endsWith('.htm')) {
                    var lastSlash = Math.max(basePath.lastIndexOf('/'), basePath.lastIndexOf('\\'));
                    if (lastSlash > 0) {
                        basePath = basePath.substring(0, lastSlash);
                    }
                }
                var separator = basePath.indexOf('\\') >= 0 ? '\\' : '/';
                var fullPath = basePath + separator + path.replace(/[/\\]/g, separator);
                return normalizePath(fullPath);
            }

            // Export path utilities for other modules
            TD.isAbsolutePath = isAbsolutePath;
            TD.resolveFilesystemPath = resolveFilesystemPath;

            // Activate media prototype intercept now that dependencies are available
            if (TD._activateMediaIntercept) {
                TD._activateMediaIntercept(invoke, resolveFilesystemPath);
            }

            // Override httpRequest to support filesystem paths
            // Single-file wikis: intercept all filesystem paths (relative + absolute)
            // Folder wikis: only intercept absolute paths (relative paths go to Node.js server)
            var originalHttpRequest = $tw.utils.httpRequest;

            var TEXT_EXTS = {
                'txt': true, 'css': true, 'js': true, 'json': true,
                'html': true, 'htm': true, 'xml': true, 'svg': true,
                'csv': true, 'md': true, 'tid': true, 'tiddler': true,
                'multids': true, 'yaml': true, 'yml': true, 'ini': true,
                'cfg': true, 'conf': true, 'log': true, 'sh': true,
                'bat': true, 'cmd': true, 'py': true, 'rb': true,
                'java': true, 'c': true, 'cpp': true, 'h': true,
                'rs': true, 'go': true, 'ts': true, 'tsx': true, 'jsx': true
            };
            function isTextFile(path) {
                var ext = path.split('.').pop().toLowerCase();
                return TEXT_EXTS[ext] === true;
            }

            // TiddlyWiki server API paths that must NOT be intercepted in folder wikis
            // (these go to the Node.js server for the syncer to work)
            function isTwServerApiPath(path) {
                return path === 'status' || path.indexOf('status/') === 0 ||
                       path.indexOf('recipes/') === 0 || path.indexOf('bags/') === 0 ||
                       path === 'login' || path.indexOf('login/') === 0;
            }

            $tw.utils.httpRequest = function(options) {
                var url = options.url;

                // Intercept filesystem paths:
                //   Single-file wikis: all filesystem paths
                //   Folder wikis: absolute paths + relative file paths (NOT TW API paths)
                if (isFilesystemPath(url) && (!isFolderWiki || isAbsolutePath(url) || !isTwServerApiPath(url))) {
                    var resolvedPath = resolveFilesystemPath(url);
                    if (!resolvedPath) {
                        if (options.callback) {
                            options.callback('Cannot resolve path: ' + url, null, {
                                status: 400, statusText: 'Bad Request',
                                responseText: '', response: '',
                                getAllResponseHeaders: function() { return ''; }
                            });
                        }
                        return { abort: function() {} };
                    }

                    if (isTextFile(resolvedPath)) {
                        // Text files: return actual text content
                        invoke('read_file_as_binary', { path: resolvedPath })
                            .then(function(bytes) {
                                var text = new TextDecoder('utf-8').decode(new Uint8Array(bytes));
                                var mockXhr = {
                                    status: 200,
                                    statusText: 'OK',
                                    responseText: text,
                                    response: text,
                                    getAllResponseHeaders: function() { return ''; }
                                };
                                if (options.callback) {
                                    options.callback(null, text, mockXhr);
                                }
                            })
                            .catch(function(err) {
                                var mockXhr = {
                                    status: 404,
                                    statusText: 'Not Found',
                                    responseText: '',
                                    response: '',
                                    getAllResponseHeaders: function() { return ''; }
                                };
                                if (options.callback) {
                                    options.callback(err, null, mockXhr);
                                }
                            });
                    } else {
                        // Binary files: return data URI
                        invoke('read_file_as_data_uri', { path: resolvedPath })
                            .then(function(dataUri) {
                                var mockXhr = {
                                    status: 200,
                                    statusText: 'OK',
                                    responseText: dataUri,
                                    response: dataUri,
                                    getAllResponseHeaders: function() { return ''; }
                                };
                                if (options.callback) {
                                    options.callback(null, dataUri, mockXhr);
                                }
                            })
                            .catch(function(err) {
                                var mockXhr = {
                                    status: 404,
                                    statusText: 'Not Found',
                                    responseText: '',
                                    response: '',
                                    getAllResponseHeaders: function() { return ''; }
                                };
                                if (options.callback) {
                                    options.callback(err, null, mockXhr);
                                }
                            });
                    }
                    return { abort: function() {} };
                }

                return originalHttpRequest.call($tw.utils, options);
            };
            console.log('[TiddlyDesktop] httpRequest override installed');

            // Override copyToClipboard to use native clipboard API
            // TiddlyWiki's document.execCommand("copy") doesn't work reliably in webviews
            $tw.utils.copyToClipboard = function(text, options) {
                options = options || {};
                invoke('set_clipboard_content', { text: text || '' })
                    .then(function(success) {
                        if (!options.doNotNotify) {
                            var notification = success
                                ? (options.successNotification || '$:/language/Notifications/CopiedToClipboard/Succeeded')
                                : (options.failureNotification || '$:/language/Notifications/CopiedToClipboard/Failed');
                            $tw.notifier.display(notification);
                        }
                    })
                    .catch(function(err) {
                        console.error('[TiddlyDesktop] Clipboard write failed:', err);
                        if (!options.doNotNotify) {
                            var notification = options.failureNotification || '$:/language/Notifications/CopiedToClipboard/Failed';
                            $tw.notifier.display(notification);
                        }
                    });
            };
            console.log('[TiddlyDesktop] copyToClipboard override installed');

            // Media interceptor for filesystem paths
            // Single-file wikis: images use tdasset://, Linux video/audio use media server.
            // Folder wikis: ALL elements use media server (tdasset:// blocked from HTTP origin).
            function setupMediaInterceptor() {
                function convertToTdassetUrl(path) {
                    return 'tdasset://localhost/' + encodeURIComponent(path);
                }

                // Media elements that need HTTP URLs on Linux (GStreamer)
                var mediaElements = { 'VIDEO': true, 'AUDIO': true, 'SOURCE': true };

                function isMediaElement(element) {
                    return mediaElements[element.tagName] || false;
                }

                // Per-file token map: HTTP URL -> filesystem path (for resolveVideoPath in media.js)
                TD.mediaTokenPaths = {};

                // Register a file with the localhost HTTP media server.
                // Returns an HTTP URL. Used for Linux media and all folder wiki elements.
                function registerMediaUrl(element, resolvedPath) {
                    // Mark element as pending to prevent re-processing
                    element.__tdMediaPending = true;
                    invoke('register_media_url', { path: resolvedPath })
                        .then(function(httpUrl) {
                            // Store token -> path mapping for poster extraction
                            TD.mediaTokenPaths[httpUrl] = resolvedPath;
                            element.setAttribute('src', httpUrl);
                            element.__tdMediaPending = false;
                            // Force reload: the browser may have already attempted loading
                            // from the original wikifile:// URL (synchronous handler blocks
                            // the main thread), leaving the element in an error state.
                            // Calling load() ensures it retries from the new HTTP URL.
                            if (element.load) element.load();
                        })
                        .catch(function(err) {
                            // Fallback to tdasset:// if media server unavailable
                            console.warn('[TiddlyDesktop] Media server fallback for:', resolvedPath, err);
                            element.setAttribute('src', convertToTdassetUrl(resolvedPath));
                            element.__tdMediaPending = false;
                        });
                }

                function convertElementSrc(element) {
                    var src = element.getAttribute('src');
                    if (!src) return;

                    // Skip if already processed or pending async registration
                    if (element.__tdMediaPending) return;

                    // Skip if already using a validated protocol or data URL
                    if (src.startsWith('tdasset://') ||
                        src.startsWith('asset://') || src.startsWith('data:') ||
                        src.startsWith('http://') || src.startsWith('https://') ||
                        src.startsWith('blob:') || src.startsWith('tdlib:')) {
                        return;
                    }

                    // wikifile:// URLs: extract the relative path and resolve
                    var rawPath = src;
                    if (src.startsWith('wikifile://')) {
                        rawPath = src.replace(/^wikifile:\/\/localhost\//, '');
                        if (!rawPath) return;
                    }

                    var resolvedPath = resolveFilesystemPath(rawPath);
                    if (resolvedPath) {
                        if (isFolderWiki && window.__TD_MEDIA_SERVER__) {
                            // Folder wiki: ALL elements use media server (HTTP URLs).
                            // tdasset:// doesn't work from HTTP-origin pages.
                            registerMediaUrl(element, resolvedPath);
                        } else if (isMediaElement(element) && window.__TD_MEDIA_SERVER__) {
                            // Linux single-file wiki: media server for GStreamer playback
                            registerMediaUrl(element, resolvedPath);
                        } else {
                            // Windows/macOS single-file wiki: tdasset:// works for all types
                            element.setAttribute('src', convertToTdassetUrl(resolvedPath));
                        }
                    }
                }

                var mediaSelectors = 'img, iframe, audio, video, embed, source';

                document.querySelectorAll(mediaSelectors).forEach(convertElementSrc);

                var observer = new MutationObserver(function(mutations) {
                    mutations.forEach(function(mutation) {
                        mutation.addedNodes.forEach(function(node) {
                            if (node.nodeType === 1) {
                                if (node.matches && node.matches(mediaSelectors)) {
                                    convertElementSrc(node);
                                }
                                if (node.querySelectorAll) {
                                    node.querySelectorAll(mediaSelectors).forEach(convertElementSrc);
                                }
                            }
                        });
                        if (mutation.type === 'attributes' && mutation.attributeName === 'src') {
                            convertElementSrc(mutation.target);
                        }
                    });
                });

                observer.observe(document.body, {
                    childList: true,
                    subtree: true,
                    attributes: true,
                    attributeFilter: ['src']
                });

                console.log('[TiddlyDesktop] Media interceptor installed');
            }

            setupMediaInterceptor();

            // Patch text-type parsers to support _canonical_uri lazy-loading.
            // TW's WikiParser handles this for text/vnd.tiddlywiki (shows "Loading..."
            // then fetches content via httpRequest). TextParser (used for text/plain,
            // application/json, text/css, etc.) doesn't — so _canonical_uri text tiddlers
            // render as empty. We wrap these parsers to add the same lazy-loading behavior.
            // Uses invoke('read_file_as_binary') directly instead of httpRequest because
            // our httpRequest override returns data URIs, not raw text content.
            function patchTextParsersForCanonicalUri() {
                if (!$tw.Wiki || !$tw.Wiki.parsers) return;
                var types = ['text/plain', 'text/x-tiddlywiki', 'application/javascript',
                             'application/json', 'text/css', 'application/x-tiddler-dictionary',
                             'text/markdown', 'text/x-markdown'];
                types.forEach(function(parserType) {
                    var OrigParser = $tw.Wiki.parsers[parserType];
                    if (!OrigParser) return;
                    var PatchedParser = function(type, text, options) {
                        if ((text || '') === '' && options && options._canonical_uri) {
                            var uri = options._canonical_uri;
                            var wiki = options.wiki;
                            // Resolve relative path to absolute filesystem path
                            var resolved = resolveFilesystemPath(uri);
                            if (resolved) {
                                invoke('read_file_as_binary', { path: resolved })
                                    .then(function(bytes) {
                                        var content = new TextDecoder('utf-8').decode(new Uint8Array(bytes));
                                        if (wiki) {
                                            wiki.each(function(tiddler, title) {
                                                if (tiddler.fields._canonical_uri === uri &&
                                                    (tiddler.fields.text || '') === '') {
                                                    wiki.addTiddler(new $tw.Tiddler(tiddler, {text: content}));
                                                }
                                            });
                                        }
                                    })
                                    .catch(function(err) {
                                        console.error('[TiddlyDesktop] Failed to lazy-load _canonical_uri:', uri, err);
                                    });
                            }
                            var placeholder = ($tw.language && $tw.language.getRawString('LazyLoadingWarning')) || 'Loading...';
                            OrigParser.call(this, type, placeholder, options);
                        } else {
                            OrigParser.call(this, type, text, options);
                        }
                    };
                    PatchedParser.prototype = OrigParser.prototype;
                    $tw.Wiki.parsers[parserType] = PatchedParser;
                });
                console.log('[TiddlyDesktop] Text parser _canonical_uri lazy-loading installed');
            }
            patchTextParsersForCanonicalUri();

            // Patch image parsers to resolve _canonical_uri filesystem paths at parse time.
            // TiddlyWiki's imageparser creates <img src="./attachments/image.png"> for
            // _canonical_uri tiddlers. On Windows/WebView2, the browser eagerly resolves
            // this relative URL against the wikifile:// page URL and fires a request
            // BEFORE the MutationObserver can convert it to tdasset://.
            // If the wikifile:// handler can't resolve the relative path (e.g. missing
            // or differently-formatted Referer header on Windows), the image fails.
            // By resolving the path in the parser, the <img> is created with a working
            // tdasset:// URL from the start, avoiding the race condition entirely.
            // Also ensures cross-platform _canonical_uri paths (e.g. ./attachments/image.png)
            // work when the same wiki is synced between Linux and Windows.
            function patchImageParsersForCanonicalUri() {
                if (!$tw.Wiki || !$tw.Wiki.parsers) return;
                var imageTypes = ['image/png', 'image/jpeg', 'image/jpg', 'image/gif',
                                  'image/webp', 'image/svg+xml', 'image/heic', 'image/heif',
                                  'image/avif', 'image/x-icon', 'image/vnd.microsoft.icon', 'image/bmp'];
                imageTypes.forEach(function(parserType) {
                    var OrigParser = $tw.Wiki.parsers[parserType];
                    if (!OrigParser) return;
                    var PatchedParser = function(type, text, options) {
                        if (options && options._canonical_uri && isFilesystemPath(options._canonical_uri)) {
                            var resolved = resolveFilesystemPath(options._canonical_uri);
                            if (resolved && !isFolderWiki) {
                                // Single-file wiki: resolve to tdasset:// URL directly.
                                // Folder wikis need async media server registration (tdasset://
                                // is blocked from HTTP origin), handled by the MutationObserver.
                                var newOptions = {};
                                for (var key in options) {
                                    if (options.hasOwnProperty(key)) {
                                        newOptions[key] = options[key];
                                    }
                                }
                                newOptions._canonical_uri = 'tdasset://localhost/' + encodeURIComponent(resolved);
                                OrigParser.call(this, type, text, newOptions);
                                return;
                            }
                        }
                        OrigParser.call(this, type, text, options);
                    };
                    PatchedParser.prototype = OrigParser.prototype;
                    $tw.Wiki.parsers[parserType] = PatchedParser;
                });
                console.log('[TiddlyDesktop] Image parser _canonical_uri path resolution installed');
            }
            patchImageParsersForCanonicalUri();

            console.log('[TiddlyDesktop] Filesystem support installed');
        }

        waitForTiddlyWiki();
    }

    setupFilesystemSupport();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
