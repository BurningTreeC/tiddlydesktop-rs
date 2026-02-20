// TiddlyDesktop - Sync Conflict Resolution UI
// Shows a notification banner when LAN sync conflicts exist and provides
// a modal UI for reviewing and resolving conflicts field-by-field.
(function() {
    'use strict';

    // Only run for wiki windows, not the landing page
    if (!window.__WIKI_PATH__) return;

    var CONFLICT_PREFIX = '$:/TiddlyDesktopRS/Conflicts/';
    var banner = null;
    var bannerDismissed = false;
    var modalOverlay = null;

    // --- Helpers ---

    // Get a color from TiddlyWiki's current palette (self-contained, no TD dependency)
    function getColour(name, fallback, depth) {
        depth = depth || 0;
        if (depth > 10) return fallback;
        if (typeof $tw !== 'undefined' && $tw.wiki) {
            try {
                var paletteName = $tw.wiki.getTiddlerText('$:/palette');
                if (paletteName) {
                    paletteName = paletteName.trim();
                    var paletteTiddler = $tw.wiki.getTiddler(paletteName);
                    if (paletteTiddler) {
                        var text = paletteTiddler.fields.text || '';
                        var lines = text.split('\n');
                        for (var i = 0; i < lines.length; i++) {
                            var line = lines[i].trim();
                            var colonIndex = line.indexOf(':');
                            if (colonIndex > 0) {
                                var colorName = line.substring(0, colonIndex).trim();
                                var colorValue = line.substring(colonIndex + 1).trim();
                                if (colorName === name && colorValue) {
                                    var match = colorValue.match(/<<colour\s+([^>]+)>>/);
                                    if (match) {
                                        return getColour(match[1].trim(), fallback, depth + 1);
                                    }
                                    return colorValue;
                                }
                            }
                        }
                    }
                }
            } catch (e) {}
        }
        return fallback;
    }

    function getConflictTitles() {
        var titles = [];
        $tw.wiki.each(function(tiddler, title) {
            if (title.indexOf(CONFLICT_PREFIX) === 0) {
                titles.push(title);
            }
        });
        return titles;
    }

    function formatTimestamp(iso) {
        if (!iso) return '';
        try {
            var d = new Date(iso);
            return d.toLocaleString();
        } catch (e) {
            return iso;
        }
    }

    // --- Banner ---

    function createBanner() {
        if (banner) return banner;

        banner = document.createElement('div');
        banner.id = 'td-conflict-banner';

        var warningBg = '#fff3cd';
        var warningFg = '#856404';
        var warningBorder = '#ffc107';

        banner.style.cssText = 'display:none;position:fixed;top:0;left:0;right:0;z-index:9999;' +
            'background:' + warningBg + ';color:' + warningFg + ';' +
            'border-bottom:2px solid ' + warningBorder + ';' +
            'padding:8px 16px;font-size:14px;font-family:system-ui,sans-serif;' +
            'display:none;align-items:center;gap:8px;box-shadow:0 2px 4px rgba(0,0,0,0.1);';

        var textSpan = document.createElement('span');
        textSpan.style.cssText = 'flex:1;';
        banner.__textSpan = textSpan;

        var resolveBtn = document.createElement('button');
        resolveBtn.textContent = 'Resolve';
        resolveBtn.style.cssText = 'padding:4px 12px;border:1px solid ' + warningBorder + ';' +
            'border-radius:4px;background:#ffc107;color:#333;cursor:pointer;font-size:13px;font-weight:500;';
        resolveBtn.onclick = function() { showConflictModal(); };

        var dismissBtn = document.createElement('button');
        dismissBtn.textContent = '\u00d7';
        dismissBtn.style.cssText = 'padding:2px 8px;border:none;background:transparent;' +
            'color:' + warningFg + ';cursor:pointer;font-size:18px;line-height:1;';
        dismissBtn.onclick = function() {
            bannerDismissed = true;
            banner.style.display = 'none';
        };

        banner.appendChild(textSpan);
        banner.appendChild(resolveBtn);
        banner.appendChild(dismissBtn);
        document.body.appendChild(banner);
        return banner;
    }

    function updateBanner() {
        var conflicts = getConflictTitles();
        if (conflicts.length === 0) {
            if (banner) banner.style.display = 'none';
            bannerDismissed = false;
            return;
        }

        if (bannerDismissed) return;

        if (!banner) createBanner();
        banner.__textSpan.textContent = '\u26a0 ' + conflicts.length + ' sync conflict' +
            (conflicts.length !== 1 ? 's' : '') + ' detected';
        banner.style.display = 'flex';
    }

    // --- Diff rendering ---

    function renderDiff(localText, remoteText) {
        var container = document.createElement('div');
        container.style.cssText = 'font-family:monospace;font-size:13px;white-space:pre-wrap;' +
            'word-break:break-word;padding:8px;border-radius:4px;max-height:300px;overflow:auto;' +
            'background:' + getColour('tiddler-background', '#ffffff') + ';' +
            'border:1px solid ' + getColour('tiddler-border', '#cccccc') + ';';

        try {
            var dmpMod = $tw.modules.execute('$:/core/modules/utils/diff-match-patch/diff_match_patch.js');
            var dmp = new dmpMod.diff_match_patch();
            var diffs = dmp.diff_main(localText || '', remoteText || '');
            dmp.diff_cleanupSemantic(diffs);

            for (var i = 0; i < diffs.length; i++) {
                var op = diffs[i][0];
                var text = diffs[i][1];
                var span = document.createElement('span');
                span.textContent = text;
                if (op === -1) {
                    // Deleted (local text not in remote)
                    span.style.cssText = 'background:#fdd;color:#900;text-decoration:line-through;';
                } else if (op === 1) {
                    // Inserted (remote text not in local)
                    span.style.cssText = 'background:#dfd;color:#060;';
                }
                container.appendChild(span);
            }
        } catch (e) {
            console.error('[TiddlyDesktop] diff-match-patch error:', e);
            container.textContent = 'Unable to compute diff';
        }
        return container;
    }

    // --- Modal ---

    function showConflictModal() {
        if (modalOverlay) closeModal();

        var conflicts = getConflictTitles();
        if (conflicts.length === 0) return;

        var modalBg = getColour('modal-background', getColour('tiddler-background', '#ffffff'));
        var modalBorder = getColour('modal-border', getColour('tiddler-border', '#cccccc'));
        var fg = getColour('foreground', '#333333');
        var mutedFg = getColour('muted-foreground', '#999999');
        var pageBg = getColour('page-background', '#f4f4f4');
        var primary = getColour('primary', '#5778d8');
        var primaryText = getContrastingColor(primary);
        var btnBg = getColour('button-background', '#f0f0f0');
        var btnFg = getContrastingColor(btnBg);
        var btnBorder = getColour('button-border', '#cccccc');

        // Overlay
        modalOverlay = document.createElement('div');
        modalOverlay.id = 'td-conflict-modal-overlay';
        modalOverlay.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;' +
            'background:rgba(0,0,0,0.5);z-index:10001;display:flex;align-items:flex-start;' +
            'justify-content:center;padding:40px 20px;overflow:auto;';

        // Modal container
        var modal = document.createElement('div');
        modal.style.cssText = 'background:' + pageBg + ';color:' + fg + ';' +
            'border-radius:8px;border:1px solid ' + modalBorder + ';' +
            'box-shadow:0 8px 32px rgba(0,0,0,0.3);max-width:700px;width:100%;' +
            'max-height:calc(100vh - 80px);display:flex;flex-direction:column;';

        // Header
        var header = document.createElement('div');
        header.style.cssText = 'display:flex;align-items:center;padding:16px 20px;' +
            'border-bottom:1px solid ' + modalBorder + ';background:' + modalBg + ';' +
            'border-radius:8px 8px 0 0;flex-shrink:0;';

        var headerTitle = document.createElement('span');
        headerTitle.textContent = 'Sync Conflicts';
        headerTitle.style.cssText = 'flex:1;font-size:18px;font-weight:600;';

        var closeBtn = document.createElement('button');
        closeBtn.textContent = 'Close';
        closeBtn.style.cssText = 'padding:6px 14px;background:' + btnBg + ';color:' + btnFg + ';' +
            'border:1px solid ' + btnBorder + ';border-radius:4px;cursor:pointer;font-size:13px;';
        closeBtn.onclick = function() { closeModal(); };

        header.appendChild(headerTitle);
        header.appendChild(closeBtn);

        // Scrollable body
        var body = document.createElement('div');
        body.style.cssText = 'padding:16px 20px;overflow-y:auto;flex:1;';

        for (var i = 0; i < conflicts.length; i++) {
            var card = renderConflictCard(conflicts[i], modalBg, modalBorder, fg, mutedFg,
                primary, primaryText, btnBg, btnFg, btnBorder);
            body.appendChild(card);
        }

        // Footer with Resolve All buttons
        var footer = document.createElement('div');
        footer.style.cssText = 'padding:12px 20px;border-top:1px solid ' + modalBorder + ';' +
            'background:' + modalBg + ';border-radius:0 0 8px 8px;' +
            'display:flex;justify-content:flex-end;gap:8px;flex-shrink:0;';

        var resolveAllLocalBtn = document.createElement('button');
        resolveAllLocalBtn.textContent = 'Resolve All: Keep Local';
        resolveAllLocalBtn.style.cssText = 'padding:8px 16px;background:' + btnBg + ';color:' + btnFg + ';' +
            'border:1px solid ' + btnBorder + ';border-radius:4px;cursor:pointer;font-size:13px;';
        resolveAllLocalBtn.onclick = function() { resolveAll('local'); };

        var resolveAllRemoteBtn = document.createElement('button');
        resolveAllRemoteBtn.textContent = 'Resolve All: Keep Remote';
        resolveAllRemoteBtn.style.cssText = 'padding:8px 16px;background:' + primary + ';color:' + primaryText + ';' +
            'border:1px solid ' + primary + ';border-radius:4px;cursor:pointer;font-size:13px;';
        resolveAllRemoteBtn.onclick = function() { resolveAll('remote'); };

        footer.appendChild(resolveAllLocalBtn);
        footer.appendChild(resolveAllRemoteBtn);

        modal.appendChild(header);
        modal.appendChild(body);
        modal.appendChild(footer);
        modalOverlay.appendChild(modal);
        document.body.appendChild(modalOverlay);

        // Close on overlay click (not modal body)
        modalOverlay.addEventListener('click', function(e) {
            if (e.target === modalOverlay) closeModal();
        });

        // Close on Escape
        modalOverlay.__escHandler = function(e) {
            if (e.key === 'Escape') closeModal();
        };
        document.addEventListener('keydown', modalOverlay.__escHandler);
    }

    function renderConflictCard(conflictTitle, modalBg, modalBorder, fg, mutedFg,
            primary, primaryText, btnBg, btnFg, btnBorder) {
        var conflict = $tw.wiki.getTiddler(conflictTitle);
        if (!conflict) return document.createElement('div');

        var originalTitle = conflict.fields['conflict-original-title'] || '';
        var timestamp = conflict.fields['conflict-timestamp'] || '';
        var original = $tw.wiki.getTiddler(originalTitle);

        var card = document.createElement('div');
        card.style.cssText = 'background:' + modalBg + ';border:1px solid ' + modalBorder + ';' +
            'border-radius:6px;padding:16px;margin-bottom:12px;';
        card.setAttribute('data-conflict-title', conflictTitle);

        // Card title
        var title = document.createElement('div');
        title.style.cssText = 'font-size:15px;font-weight:600;margin-bottom:4px;';
        title.textContent = originalTitle;
        card.appendChild(title);

        // Timestamp
        var ts = document.createElement('div');
        ts.style.cssText = 'font-size:12px;color:' + mutedFg + ';margin-bottom:12px;';
        ts.textContent = 'Conflicted at: ' + formatTimestamp(timestamp);
        card.appendChild(ts);

        // Field comparisons
        var localFields = conflict.fields;
        var remoteFields = original ? original.fields : {};
        var skipFields = { 'title': 1, 'conflict-original-title': 1, 'conflict-timestamp': 1, 'conflict-source': 1 };

        // Collect all unique field names
        var allFields = {};
        var key;
        for (key in localFields) {
            if (!skipFields[key]) allFields[key] = true;
        }
        for (key in remoteFields) {
            if (!skipFields[key]) allFields[key] = true;
        }

        var fieldNames = Object.keys(allFields).sort();
        var hasDiffs = false;

        for (var i = 0; i < fieldNames.length; i++) {
            var field = fieldNames[i];
            var localVal = localFields[field];
            var remoteVal = remoteFields[field];

            // Normalize to strings for comparison
            var localStr = localVal != null ? String(localVal) : '';
            var remoteStr = remoteVal != null ? String(remoteVal) : '';

            if (localStr === remoteStr) continue;
            hasDiffs = true;

            var fieldSection = document.createElement('div');
            fieldSection.style.cssText = 'margin-bottom:10px;';

            var fieldLabel = document.createElement('div');
            fieldLabel.style.cssText = 'font-size:12px;font-weight:600;color:' + mutedFg + ';' +
                'text-transform:uppercase;letter-spacing:0.5px;margin-bottom:4px;';
            fieldLabel.textContent = field;
            fieldSection.appendChild(fieldLabel);

            if (field === 'text') {
                // Full diff for text field
                fieldSection.appendChild(renderDiff(localStr, remoteStr));
            } else {
                // Side-by-side for other fields
                var comparison = document.createElement('div');
                comparison.style.cssText = 'font-size:13px;padding:6px 8px;' +
                    'border:1px solid ' + modalBorder + ';border-radius:4px;' +
                    'background:' + getColour('tiddler-background', '#ffffff') + ';';

                var localLine = document.createElement('div');
                localLine.style.cssText = 'margin-bottom:2px;';
                var localLabel = document.createElement('span');
                localLabel.textContent = 'Local: ';
                localLabel.style.cssText = 'font-weight:600;color:#900;';
                var localValue = document.createElement('span');
                localValue.textContent = localStr || '(empty)';
                localLine.appendChild(localLabel);
                localLine.appendChild(localValue);

                var remoteLine = document.createElement('div');
                var remoteLabel = document.createElement('span');
                remoteLabel.textContent = 'Remote: ';
                remoteLabel.style.cssText = 'font-weight:600;color:#060;';
                var remoteValue = document.createElement('span');
                remoteValue.textContent = remoteStr || '(empty)';
                remoteLine.appendChild(remoteLabel);
                remoteLine.appendChild(remoteValue);

                comparison.appendChild(localLine);
                comparison.appendChild(remoteLine);
                fieldSection.appendChild(comparison);
            }

            card.appendChild(fieldSection);
        }

        if (!hasDiffs) {
            var noDiff = document.createElement('div');
            noDiff.style.cssText = 'font-size:13px;color:' + mutedFg + ';font-style:italic;margin-bottom:10px;';
            noDiff.textContent = 'All fields are identical (conflict may have been resolved externally).';
            card.appendChild(noDiff);
        }

        // Action buttons
        var actions = document.createElement('div');
        actions.style.cssText = 'display:flex;gap:8px;margin-top:8px;';

        var keepLocalBtn = document.createElement('button');
        keepLocalBtn.textContent = 'Keep Local';
        keepLocalBtn.style.cssText = 'padding:6px 14px;background:' + btnBg + ';color:' + btnFg + ';' +
            'border:1px solid ' + btnBorder + ';border-radius:4px;cursor:pointer;font-size:13px;';
        keepLocalBtn.onclick = function() {
            resolveConflict(conflictTitle, 'local');
            card.remove();
            afterResolve();
        };

        var keepRemoteBtn = document.createElement('button');
        keepRemoteBtn.textContent = 'Keep Remote';
        keepRemoteBtn.style.cssText = 'padding:6px 14px;background:' + primary + ';color:' + primaryText + ';' +
            'border:1px solid ' + primary + ';border-radius:4px;cursor:pointer;font-size:13px;';
        keepRemoteBtn.onclick = function() {
            resolveConflict(conflictTitle, 'remote');
            card.remove();
            afterResolve();
        };

        actions.appendChild(keepLocalBtn);
        actions.appendChild(keepRemoteBtn);
        card.appendChild(actions);

        return card;
    }

    function getContrastingColor(bgColor) {
        try {
            var hex = bgColor.replace('#', '');
            if (hex.length === 3) hex = hex[0] + hex[0] + hex[1] + hex[1] + hex[2] + hex[2];
            var r = parseInt(hex.substr(0, 2), 16) / 255;
            var g = parseInt(hex.substr(2, 2), 16) / 255;
            var b = parseInt(hex.substr(4, 2), 16) / 255;
            r = r <= 0.03928 ? r / 12.92 : Math.pow((r + 0.055) / 1.055, 2.4);
            g = g <= 0.03928 ? g / 12.92 : Math.pow((g + 0.055) / 1.055, 2.4);
            b = b <= 0.03928 ? b / 12.92 : Math.pow((b + 0.055) / 1.055, 2.4);
            var lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            return lum > 0.179 ? '#000000' : '#ffffff';
        } catch (e) {
            return '#333333';
        }
    }

    // --- Resolution ---

    function resolveConflict(conflictTitle, action) {
        try {
            var conflict = $tw.wiki.getTiddler(conflictTitle);
            if (!conflict) return;

            if (action === 'local') {
                // Copy local (conflict) fields back to original tiddler using
                // TW5's Tiddler constructor override: pass conflict tiddler as base,
                // then override title and null out conflict-* metadata fields
                var originalTitle = conflict.fields['conflict-original-title'];
                if (originalTitle) {
                    $tw.wiki.addTiddler(new $tw.Tiddler(conflict, {
                        title: originalTitle,
                        'conflict-original-title': undefined,
                        'conflict-timestamp': undefined,
                        'conflict-source': undefined
                    }));
                }
            }
            // For 'remote': the remote version is already in place, just delete the conflict
            $tw.wiki.deleteTiddler(conflictTitle);
        } catch (e) {
            console.error('[TiddlyDesktop] resolveConflict error:', e);
        }
    }

    function resolveAll(action) {
        var conflicts = getConflictTitles();
        for (var i = 0; i < conflicts.length; i++) {
            resolveConflict(conflicts[i], action);
        }
        closeModal();
        updateBanner();
    }

    function afterResolve() {
        // Check if any conflicts remain in the modal
        if (modalOverlay) {
            var remaining = modalOverlay.querySelectorAll('[data-conflict-title]');
            if (remaining.length === 0) {
                closeModal();
            }
        }
        updateBanner();
    }

    function closeModal() {
        if (modalOverlay) {
            if (modalOverlay.__escHandler) {
                document.removeEventListener('keydown', modalOverlay.__escHandler);
            }
            modalOverlay.remove();
            modalOverlay = null;
        }
    }

    // --- Initialization ---

    function init() {
        if (typeof $tw === 'undefined' || !$tw.wiki) {
            setTimeout(init, 200);
            return;
        }

        // Check for existing conflicts
        updateBanner();

        // Watch for changes to conflict tiddlers
        $tw.wiki.addEventListener('change', function(changes) {
            var relevant = false;
            for (var title in changes) {
                if (title.indexOf(CONFLICT_PREFIX) === 0) {
                    relevant = true;
                    break;
                }
            }
            if (relevant) {
                // New conflict arrived — reset dismissed state
                bannerDismissed = false;
                updateBanner();
            }
        });
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function() { init(); });
    } else {
        init();
    }

})();
