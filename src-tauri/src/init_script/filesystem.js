// TiddlyDesktop Initialization Script - Filesystem Module
// Handles: httpRequest override for filesystem paths, media interceptor

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

            // Media interceptor for filesystem paths
            function setupMediaInterceptor() {
                if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.convertFileSrc) {
                    setTimeout(setupMediaInterceptor, 100);
                    return;
                }

                var convertFileSrc = window.__TAURI__.core.convertFileSrc;

                function convertElementSrc(element) {
                    var src = element.getAttribute('src');
                    if (!src) return;

                    if (src.startsWith('asset://') || src.startsWith('data:') ||
                        src.startsWith('http://') || src.startsWith('https://') ||
                        src.startsWith('blob:') || src.startsWith('wikifile://')) {
                        return;
                    }

                    var resolvedPath = resolveFilesystemPath(src);
                    if (resolvedPath) {
                        var assetUrl = convertFileSrc(resolvedPath);
                        element.setAttribute('src', assetUrl);
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
