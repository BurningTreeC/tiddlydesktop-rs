// TiddlyDesktop Initialization Script - Filesystem Module
// Handles: httpRequest override for filesystem paths, copyToClipboard override, media interceptor

(function(TD) {
    'use strict';

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

            // Override httpRequest to support filesystem paths
            var originalHttpRequest = $tw.utils.httpRequest;
            $tw.utils.httpRequest = function(options) {
                var url = options.url;

                if (isFilesystemPath(url)) {
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
            // Images use tdasset:// (validated custom protocol).
            // On Linux, video/audio use localhost HTTP server (GStreamer can't play custom URI schemes).
            // On Windows/macOS, video/audio also use tdasset:// (their media engines handle it fine).
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

                // Register a media file with the localhost HTTP server (Linux only).
                // Returns the HTTP URL via callback. On non-Linux, falls back to tdasset://.
                function registerMediaUrl(element, resolvedPath) {
                    // Mark element as pending to prevent re-processing
                    element.__tdMediaPending = true;
                    invoke('register_media_url', { path: resolvedPath })
                        .then(function(httpUrl) {
                            // Store token -> path mapping for poster extraction
                            TD.mediaTokenPaths[httpUrl] = resolvedPath;
                            element.setAttribute('src', httpUrl);
                            element.__tdMediaPending = false;
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
                        if (isMediaElement(element) && window.__TD_MEDIA_SERVER__) {
                            // Linux: register with localhost HTTP media server for GStreamer playback
                            registerMediaUrl(element, resolvedPath);
                        } else {
                            // Windows/macOS: tdasset:// works for all media types
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
            console.log('[TiddlyDesktop] Filesystem support installed');
        }

        waitForTiddlyWiki();
    }

    setupFilesystemSupport();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
