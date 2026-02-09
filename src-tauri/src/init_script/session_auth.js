// TiddlyDesktop Initialization Script - Session Authentication Module
// Handles: authentication URL management for external services
// Uses runtime plugin injection to provide shadow tiddlers

(function(TD) {
    'use strict';

    var PLUGIN_TITLE = "$:/plugins/tiddlydesktop-rs/injected";

    function setupSessionAuthentication() {
        if (window.__IS_MAIN_WIKI__) {
            console.log('[TiddlyDesktop] Main wiki - session authentication disabled');
            return;
        }

        var wikiPath = window.__WIKI_PATH__;
        var twReady = (typeof $tw !== "undefined") && $tw && $tw.wiki;
        if (!wikiPath || !twReady) {
            if (!window.__sessionAuthRetryCount) window.__sessionAuthRetryCount = 0;
            window.__sessionAuthRetryCount++;
            if (window.__sessionAuthRetryCount < 600) {
                setTimeout(setupSessionAuthentication, 100);
            } else {
                console.log('[TiddlyDesktop] Wiki not ready after 60s - session authentication disabled');
            }
            return;
        }

        var CONFIG_PREFIX = "$:/plugins/tiddlydesktop-rs/session-auth/";
        var CONFIG_SETTINGS_TAB = CONFIG_PREFIX + "settings";
        var CONFIG_AUTH_URLS = CONFIG_PREFIX + "urls";
        var invoke = window.__TAURI__.core.invoke;

        // Install save hook to filter out the injected plugin tiddler
        if (!window.__tdSaveHookInstalled && $tw.hooks) {
            $tw.hooks.addHook("th-saving-tiddler", function(tiddler) {
                if (tiddler && tiddler.fields && tiddler.fields.title) {
                    var title = tiddler.fields.title;
                    if (title === PLUGIN_TITLE ||
                        title.indexOf("$:/plugins/tiddlydesktop-rs/") === 0 ||
                        title.indexOf("$:/temp/tiddlydesktop") === 0) {
                        return null;
                    }
                }
                return tiddler;
            });
            window.__tdSaveHookInstalled = true;
            console.log("[TiddlyDesktop] Save hook installed");
        }

        // Cleanup: Silently remove any accidentally-saved tiddlers from previous versions.
        // Temporarily suppresses enqueueTiddlerEvent so deleteTiddler() won't fire
        // change events or increment changeCount (which would mark the wiki dirty).
        if (!window.__tdCleanupDone) {
            window.__tdCleanupDone = true;
            var cleanupPrefixes = [
                "$:/temp/tiddlydesktop-rs/",
                "$:/temp/tiddlydesktop/",
                "$:/plugins/tiddlydesktop-rs/"
            ];
            var deletedCount = 0;
            var origEnqueue = $tw.wiki.enqueueTiddlerEvent;
            $tw.wiki.enqueueTiddlerEvent = function() {};
            cleanupPrefixes.forEach(function(prefix) {
                $tw.wiki.filterTiddlers("[prefix[" + prefix + "]]").forEach(function(title) {
                    if ($tw.wiki.tiddlerExists(title) && !$tw.wiki.isShadowTiddler(title)) {
                        $tw.wiki.deleteTiddler(title);
                        deletedCount++;
                    }
                });
            });
            $tw.wiki.enqueueTiddlerEvent = origEnqueue;
            if (deletedCount > 0) {
                console.log("[TiddlyDesktop] Cleaned up " + deletedCount + " accidentally-saved tiddlers");
            }
        }

        // Plugin tiddlers collection (shared with drag_drop.js via TD namespace)
        TD.pluginTiddlers = TD.pluginTiddlers || {};

        function addPluginTiddler(fields) {
            TD.pluginTiddlers[fields.title] = fields;
        }

        function removePluginTiddler(title) {
            delete TD.pluginTiddlers[title];
        }

        function registerPlugin() {
            // Capture dirty state - plugin registration should not mark wiki as modified
            var origNumChanges = $tw.saverHandler ? $tw.saverHandler.numChanges : 0;

            // Build plugin content
            var pluginContent = { tiddlers: {} };
            Object.keys(TD.pluginTiddlers).forEach(function(title) {
                pluginContent.tiddlers[title] = TD.pluginTiddlers[title];
            });

            // Create/update the plugin tiddler
            $tw.wiki.addTiddler(new $tw.Tiddler({
                title: PLUGIN_TITLE,
                type: "application/json",
                "plugin-type": "plugin",
                name: "TiddlyDesktop Injected",
                description: "Runtime-injected TiddlyDesktop settings UI",
                version: "1.0.0",
                text: JSON.stringify(pluginContent)
            }));

            // Re-process plugins to unpack shadow tiddlers
            $tw.wiki.readPluginInfo();
            $tw.wiki.registerPluginTiddlers("plugin");
            $tw.wiki.unpackPluginTiddlers();

            // Trigger UI refresh
            $tw.rootWidget.refresh({});

            // Restore dirty state after event loop completes - plugin injection should not mark wiki as modified
            setTimeout(function() {
                if ($tw.saverHandler) {
                    $tw.saverHandler.numChanges = origNumChanges;
                    $tw.saverHandler.updateDirtyStatus();
                }
            }, 0);

            console.log("[TiddlyDesktop] Plugin registered with " + Object.keys(TD.pluginTiddlers).length + " shadow tiddlers");
        }

        // Export for other modules
        TD.addPluginTiddler = addPluginTiddler;
        TD.removePluginTiddler = removePluginTiddler;
        TD.registerPlugin = registerPlugin;

        function saveConfigToTauri() {
            var authUrls = [];
            Object.keys(TD.pluginTiddlers).forEach(function(title) {
                if (title.indexOf(CONFIG_PREFIX + "url/") === 0) {
                    var tiddler = TD.pluginTiddlers[title];
                    if (tiddler) {
                        authUrls.push({
                            name: tiddler.name || "",
                            url: tiddler.url || ""
                        });
                    }
                }
            });
            invoke("set_session_auth_config", {
                wikiPath: wikiPath,
                config: { auth_urls: authUrls }
            }).catch(function(err) {
                console.error("[TiddlyDesktop] Failed to save session auth config:", err);
            });
        }

        function refreshUrlList() {
            var count = Object.keys(TD.pluginTiddlers).filter(function(title) {
                return title.indexOf(CONFIG_PREFIX + "url/") === 0;
            }).length;
            addPluginTiddler({
                title: CONFIG_AUTH_URLS,
                text: String(count)
            });
            registerPlugin();
        }

        function injectConfigTiddlers(config) {
            if (config.auth_urls) {
                config.auth_urls.forEach(function(entry, index) {
                    addPluginTiddler({
                        title: CONFIG_PREFIX + "url/" + index,
                        name: entry.name,
                        url: entry.url,
                        text: ""
                    });
                });
            }

            var tabText = "<p>Authenticate with external services to access protected resources (like SharePoint profile images).</p>\n" +
                "<p>Session cookies will be stored in this wiki's isolated session data.</p>\n\n" +
                "<h2>Authentication URLs</h2>\n\n" +
                "<$list filter=\"[prefix[" + CONFIG_PREFIX + "url/]]\" variable=\"urlTiddler\">\n" +
                "<div class=\"tc-tiddler-info\" style=\"display:flex;align-items:center;gap:8px;margin-bottom:8px;padding:8px;border-radius:4px;\">\n" +
                "<div style=\"flex:1;\">\n" +
                "<strong><$text text={{{ [<urlTiddler>get[name]] }}}/></strong><br/>\n" +
                "<small><$text text={{{ [<urlTiddler>get[url]] }}}/></small>\n" +
                "</div>\n" +
                "<$button class=\"tc-btn-invisible tc-tiddlylink\" message=\"tm-tiddlydesktop-open-auth-url\" param=<<urlTiddler>> tooltip=\"Open login window\">\n" +
                "{{$:/core/images/external-link}} Login\n" +
                "</$button>\n" +
                "<$button class=\"tc-btn-invisible tc-tiddlylink\" message=\"tm-tiddlydesktop-remove-auth-url\" param=<<urlTiddler>> tooltip=\"Remove this URL\">\n" +
                "{{$:/core/images/delete-button}}\n" +
                "</$button>\n" +
                "</div>\n" +
                "</$list>\n\n" +
                "<$list filter=\"[prefix[" + CONFIG_PREFIX + "url/]count[]match[0]]\" variable=\"ignore\">\n" +
                "<p><em>No authentication URLs configured.</em></p>\n" +
                "</$list>\n\n" +
                "<h2>Add New URL</h2>\n\n" +
                "<$keyboard key=\"enter\" actions=\"\"\"<$action-sendmessage $message=\"tm-tiddlydesktop-add-auth-url\"/>\"\"\">\n" +
                "<$edit-text tiddler=\"" + CONFIG_PREFIX + "new-name\" tag=\"input\" placeholder=\"Name (e.g. SharePoint)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:4px;\"/>\n\n" +
                "<$edit-text tiddler=\"" + CONFIG_PREFIX + "new-url\" tag=\"input\" placeholder=\"URL (e.g. https://company.sharepoint.com)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:8px;\"/>\n" +
                "</$keyboard>\n\n" +
                "<$button message=\"tm-tiddlydesktop-add-auth-url\" class=\"tc-btn-big-green\">Add URL</$button>\n\n" +
                "<h2>Session Data</h2>\n\n" +
                "<p>This wiki has its own isolated session storage (cookies, localStorage). You can clear it if you want to log out of all services.</p>\n\n" +
                "<$button message=\"tm-tiddlydesktop-clear-session\" class=\"tc-btn-big-green\" style=\"background:#c42b2b;\">Clear Session Data</$button>\n" +
                "<p><small>Note: This will clear all cookies and localStorage for this wiki. You will need to log in again to any authenticated services.</small></p>\n";

            addPluginTiddler({
                title: CONFIG_SETTINGS_TAB,
                caption: "Session Auth",
                tags: "$:/tags/ControlPanel/SettingsTab",
                text: tabText
            });

            // Register plugin with all tiddlers (dirty state guard is inside registerPlugin)
            registerPlugin();

            console.log("[TiddlyDesktop] Session Authentication settings UI ready");
        }

        // Message handler: add new auth URL
        $tw.rootWidget.addEventListener("tm-tiddlydesktop-add-auth-url", function(event) {
            var name = $tw.wiki.getTiddlerText(CONFIG_PREFIX + "new-name", "").trim();
            var url = $tw.wiki.getTiddlerText(CONFIG_PREFIX + "new-url", "").trim();

            if (!name || !url) {
                alert("Please enter both a name and URL");
                return;
            }

            var parsedUrl;
            try {
                parsedUrl = new URL(url);
            } catch (e) {
                alert("Please enter a valid URL");
                return;
            }

            var isHttps = parsedUrl.protocol === "https:";
            var isLocalhost = parsedUrl.hostname === "localhost" ||
                              parsedUrl.hostname === "127.0.0.1" ||
                              parsedUrl.hostname === "::1";
            var isLocalhostHttp = parsedUrl.protocol === "http:" && isLocalhost;

            if (!isHttps && !isLocalhostHttp) {
                alert("Security: Only HTTPS URLs are allowed for authentication (except localhost)");
                return;
            }

            var existingCount = Object.keys(TD.pluginTiddlers).filter(function(title) {
                return title.indexOf(CONFIG_PREFIX + "url/") === 0;
            }).length;

            addPluginTiddler({
                title: CONFIG_PREFIX + "url/" + existingCount,
                name: name,
                url: url,
                text: ""
            });

            // Clear the input fields (these are real tiddlers created by edit-text widget)
            $tw.wiki.deleteTiddler(CONFIG_PREFIX + "new-name");
            $tw.wiki.deleteTiddler(CONFIG_PREFIX + "new-url");

            saveConfigToTauri();
            refreshUrlList();
        });

        // Message handler: remove auth URL
        $tw.rootWidget.addEventListener("tm-tiddlydesktop-remove-auth-url", function(event) {
            var tiddlerTitle = event.param;
            if (tiddlerTitle) {
                removePluginTiddler(tiddlerTitle);
                saveConfigToTauri();
                refreshUrlList();
            }
        });

        // Message handler: open auth URL in new window
        $tw.rootWidget.addEventListener("tm-tiddlydesktop-open-auth-url", function(event) {
            var tiddlerTitle = event.param;
            if (tiddlerTitle) {
                var tiddler = TD.pluginTiddlers[tiddlerTitle];
                if (tiddler) {
                    var name = tiddler.name || "Authentication";
                    var url = tiddler.url;
                    if (url) {
                        invoke("open_auth_window", {
                            wikiPath: wikiPath,
                            url: url,
                            name: name
                        }).catch(function(err) {
                            console.error("[TiddlyDesktop] Failed to open auth window:", err);
                            alert("Failed to open authentication window: " + err);
                        });
                    }
                }
            }
        });

        // Message handler: clear session data
        $tw.rootWidget.addEventListener("tm-tiddlydesktop-clear-session", function(event) {
            if (confirm("Are you sure you want to clear all session data for this wiki?\n\nThis will log you out of all authenticated services.")) {
                invoke("clear_wiki_session", { wikiPath: wikiPath })
                    .then(function() {
                        alert("Session data cleared successfully.\n\nPlease reload the wiki for changes to take effect.");
                    })
                    .catch(function(err) {
                        console.error("[TiddlyDesktop] Failed to clear session:", err);
                        alert("Failed to clear session data: " + err);
                    });
            }
        });

        // Load config from Tauri
        invoke("get_session_auth_config", { wikiPath: wikiPath })
            .then(function(config) {
                injectConfigTiddlers(config);
            })
            .catch(function(err) {
                console.error("[TiddlyDesktop] Failed to load session auth config, using defaults:", err);
                injectConfigTiddlers({ auth_urls: [] });
            });

        console.log("[TiddlyDesktop] Session authentication ready for:", wikiPath);
    }

    setupSessionAuthentication();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
