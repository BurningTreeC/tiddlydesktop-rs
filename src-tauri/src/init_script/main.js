// TiddlyDesktop Initialization Script - Main Entry Point
// This file bootstraps all initialization modules

// Create the TiddlyDesktop namespace
window.TiddlyDesktop = window.TiddlyDesktop || {};

// The individual module files will be concatenated after this file:
// 1. core.js      - Initialization guard, modal UI, confirm override
// 2. window.js    - Window close handler with unsaved changes check
// 3. filesystem.js - httpRequest override, path resolution, media interceptor
// 4. drag_drop.js - External attachments, file drops, content drags, paste, import hooks
// 5. session_auth.js - Session authentication URL management
// 6. internal_drag.js - Internal TiddlyWiki drag-and-drop polyfill
// 7. sync.js      - Window handlers, cross-window tiddler synchronization
