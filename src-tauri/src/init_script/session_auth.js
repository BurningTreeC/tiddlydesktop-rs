// TiddlyDesktop Initialization Script - Session Authentication Module
// Handles: authentication URL management for external services

(function(TD) {
    'use strict';

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

        var CONFIG_SETTINGS_TAB = "$:/plugins/tiddlydesktop/session-auth/settings";
        var CONFIG_AUTH_URLS = "$:/temp/tiddlydesktop-rs/session-auth/urls";
        var invoke = window.__TAURI__.core.invoke;

        function saveConfigToTauri() {
            var authUrls = [];
            $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]").forEach(function(title) {
                var tiddler = $tw.wiki.getTiddler(title);
                if (tiddler) {
                    authUrls.push({
                        name: tiddler.fields.name || "",
                        url: tiddler.fields.url || ""
                    });
                }
            });
            invoke("set_session_auth_config", {
                wikiPath: wikiPath,
                config: { auth_urls: authUrls }
            }).catch(function(err) {
                console.error("[TiddlyDesktop] Failed to save session auth config:", err);
            });
        }

        function deleteConfigTiddlers() {
            $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/]]").forEach(function(title) {
                $tw.wiki.deleteTiddler(title);
            });
            $tw.wiki.deleteTiddler(CONFIG_SETTINGS_TAB);
        }

        function refreshUrlList() {
            var count = $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]").length;
            $tw.wiki.setText(CONFIG_AUTH_URLS, "text", null, String(count));
        }

        function injectConfigTiddlers(config) {
            var originalNumChanges = $tw.saverHandler ? $tw.saverHandler.numChanges : 0;

            if (config.auth_urls) {
                config.auth_urls.forEach(function(entry, index) {
                    $tw.wiki.addTiddler(new $tw.Tiddler({
                        title: "$:/temp/tiddlydesktop-rs/session-auth/url/" + index,
                        name: entry.name,
                        url: entry.url,
                        text: ""
                    }));
                });
            }

            var tabText = "<p>Authenticate with external services to access protected resources (like SharePoint profile images).</p>\n" +
                "<p>Session cookies will be stored in this wiki's isolated session data.</p>\n\n" +
                "<h2>Authentication URLs</h2>\n\n" +
                "<$list filter=\"[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]\" variable=\"urlTiddler\">\n" +
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
                "<$list filter=\"[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]count[]match[0]]\" variable=\"ignore\">\n" +
                "<p><em>No authentication URLs configured.</em></p>\n" +
                "</$list>\n\n" +
                "<h2>Add New URL</h2>\n\n" +
                "<$keyboard key=\"enter\" actions=\"\"\"<$action-sendmessage $message=\"tm-tiddlydesktop-add-auth-url\"/>\"\"\">\n" +
                "<$edit-text tiddler=\"$:/temp/tiddlydesktop-rs/session-auth/new-name\" tag=\"input\" placeholder=\"Name (e.g. SharePoint)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:4px;\"/>\n\n" +
                "<$edit-text tiddler=\"$:/temp/tiddlydesktop-rs/session-auth/new-url\" tag=\"input\" placeholder=\"URL (e.g. https://company.sharepoint.com)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:8px;\"/>\n" +
                "</$keyboard>\n\n" +
                "<$button message=\"tm-tiddlydesktop-add-auth-url\" class=\"tc-btn-big-green\">Add URL</$button>\n";

            $tw.wiki.addTiddler(new $tw.Tiddler({
                title: CONFIG_SETTINGS_TAB,
                caption: "Session Auth",
                tags: "$:/tags/ControlPanel/SettingsTab",
                text: tabText
            }));

            setTimeout(function() {
                if ($tw.saverHandler) {
                    $tw.saverHandler.numChanges = originalNumChanges;
                    $tw.saverHandler.updateDirtyStatus();
                }
            }, 0);

            refreshUrlList();
            console.log("[TiddlyDesktop] Session Authentication settings UI ready");
        }

        // Message handler: add new auth URL
        $tw.rootWidget.addEventListener("tm-tiddlydesktop-add-auth-url", function(event) {
            var name = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/session-auth/new-name", "").trim();
            var url = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/session-auth/new-url", "").trim();

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

            var existingUrls = $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]");
            var nextIndex = existingUrls.length;

            $tw.wiki.addTiddler(new $tw.Tiddler({
                title: "$:/temp/tiddlydesktop-rs/session-auth/url/" + nextIndex,
                name: name,
                url: url,
                text: ""
            }));

            $tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/session-auth/new-name");
            $tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/session-auth/new-url");

            saveConfigToTauri();
            refreshUrlList();
        });

        // Message handler: remove auth URL
        $tw.rootWidget.addEventListener("tm-tiddlydesktop-remove-auth-url", function(event) {
            var tiddlerTitle = event.param;
            if (tiddlerTitle) {
                $tw.wiki.deleteTiddler(tiddlerTitle);
                saveConfigToTauri();
                refreshUrlList();
            }
        });

        // Message handler: open auth URL in new window
        $tw.rootWidget.addEventListener("tm-tiddlydesktop-open-auth-url", function(event) {
            var tiddlerTitle = event.param;
            if (tiddlerTitle) {
                var tiddler = $tw.wiki.getTiddler(tiddlerTitle);
                if (tiddler) {
                    var name = tiddler.fields.name || "Authentication";
                    var url = tiddler.fields.url;
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

        // Load config from Tauri
        invoke("get_session_auth_config", { wikiPath: wikiPath })
            .then(function(config) {
                injectConfigTiddlers(config);
            })
            .catch(function(err) {
                console.error("[TiddlyDesktop] Failed to load session auth config, using defaults:", err);
                injectConfigTiddlers({ auth_urls: [] });
            });

        // Cleanup on window close
        window.addEventListener("beforeunload", function() {
            saveConfigToTauri();
            deleteConfigTiddlers();
        });

        console.log("[TiddlyDesktop] Session authentication ready for:", wikiPath);
    }

    setupSessionAuthentication();

})(window.TiddlyDesktop = window.TiddlyDesktop || {});
