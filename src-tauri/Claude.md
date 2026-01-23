# TiddlyDesktop Development Notes

## Windows Drag & Drop Implementation

### Current State (January 2026)

The Windows drag-drop implementation uses a custom OLE `IDropTarget` to intercept drag events before WebView2's default handling. This allows capturing content from external applications (text, HTML, URLs) that WebView2 would otherwise block.

**Status**: CI builds passing. Original drop target forwarding implemented but needs Windows testing.

### Architecture

**Location**: `src-tauri/src/lib.rs` (windows_drag module, lines ~500-1150)

**Components**:
- `DropTargetImpl`: Custom COM IDropTarget implementation
- Registered on the parent HWND (Tauri window), not the WebView2 content window
- Emits Tauri events (`td-drag-motion`, `td-drag-leave`, `td-drag-content`, etc.) for JavaScript handling
- JavaScript side creates synthetic DragEvents to trigger TiddlyWiki's dropzone widget

**DropTargetImpl struct fields**:
```rust
struct DropTargetImpl {
    vtbl: *const IDropTargetVtbl,
    ref_count: AtomicU32,
    window: WebviewWindow,
    composition_controller: Option<ICoreWebView2CompositionController3>,  // Always None in windowed mode
    original_drop_target: Option<IDropTarget>,  // wry's original handler, queried before revoke
    parent_hwnd: HWND,
    drag_active: Mutex<bool>,
    is_internal_drag: Mutex<bool>,
}
```

### Internal vs External Drag Detection

Drags are classified by checking for the `chromium/x-renderer-taint` clipboard format:
- **Present**: Internal drag (originated from within WebView2)
- **Absent**: External drag (from file manager, other apps, etc.)

Detection happens in `is_chromium_renderer_drag()` function which enumerates IDataObject formats.

### Known Limitation: ICoreWebView2CompositionController3

**Problem**: The code attempts to use `ICoreWebView2CompositionController3` for forwarding drag events to WebView2, but this interface is **not available** in Tauri/wry.

**Root Cause**:
- wry creates WebView2 using `CreateCoreWebView2Controller` (windowed hosting mode)
- `ICoreWebView2CompositionController3` is only available when using `CreateCoreWebView2CompositionController` (visual/composition hosting mode)
- These are separate interface hierarchies - you cannot cast from Controller to CompositionController
- See wry source: `~/.cargo/registry/src/index.crates.io-.../wry-0.53.5/src/webview2/mod.rs` line ~404

**Current Workaround** (implemented January 2026):
1. Before revoking the existing drop target, query it using `GetPropW(hwnd, w!("OleDropTargetInterface"))`
2. Cast the returned pointer to `IDropTarget`
3. Store the original drop target reference in `DropTargetImpl`
4. For internal drags AND external drags, forward to the original drop target
5. The original drop target should be wry's internal handler which can process drags

### Windows API Types

When calling `IDropTarget` methods on the original drop target, use the correct Windows types:

```rust
// Imports needed
use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;

// Correct call pattern
let mut effect = DROPEFFECT_COPY;
odt.DragEnter(&data_object, MODIFIERKEYS_FLAGS(key), pt, &mut effect as *mut DROPEFFECT);
if !pdw_effect.is_null() { *pdw_effect = effect.0 as u32; }

// DragOver
odt.DragOver(MODIFIERKEYS_FLAGS(key), pt, &mut effect as *mut DROPEFFECT);

// Drop
odt.Drop(&data_object, MODIFIERKEYS_FLAGS(key), pt, &mut effect as *mut DROPEFFECT);
```

Note: The CompositionController uses different types (`u32` for key, `&mut u32` for effect).

### Event Flow

**External Drags**:
1. OLE `DragEnter` → detect external via absence of `chromium/x-renderer-taint`
2. Forward to original drop target (if available)
3. Emit `td-drag-motion` with screen coordinates
4. JavaScript converts to client coordinates, creates synthetic `dragenter`/`dragover` events
5. TiddlyWiki dropzone widget receives events, shows visual feedback
6. OLE `Drop` → extract content, emit `td-drag-content` and `td-drag-drop-position`
7. JavaScript dispatches synthetic `drop` event with the content

**Internal Drags**:
1. OLE `DragEnter` → detect via `chromium/x-renderer-taint` format
2. Forward to original drop target (wry's handler)
3. Skip emitting td-* events to avoid interference
4. Return S_OK

### JavaScript Event Handling

JavaScript handlers are injected into wiki windows (search for `td-drag-motion` in lib.rs, lines ~4192+):

- `td-drag-motion`: Received during drag, creates synthetic dragenter/dragover
- `td-drag-leave`: Received when drag leaves window
- `td-drag-content`: Received at drop time with extracted content data
- `td-drag-drop-position`: Received at drop time with coordinates
- `td-file-drop`: Received for file drops with paths

Key JavaScript state variables:
- `nativeDragActive`: Set when IDropTarget fires td-drag-motion
- `webviewInternalDragActive`: Set by document dragstart/dragend (internal drags)
- `nativeDropInProgress`: Set when td-drag-drop-start fires

### Coordinate Conversion

Screen coordinates from Windows need conversion to client coordinates:
```javascript
function screenToClient(screenX, screenY) {
    var dpr = window.devicePixelRatio || 1;
    var cssScreenX = screenX / dpr;
    var cssScreenY = screenY / dpr;
    var clientX = cssScreenX - window.screenX;
    var clientY = cssScreenY - window.screenY;
    // Clamp to viewport bounds
    clientX = Math.max(0, Math.min(clientX, window.innerWidth - 1));
    clientY = Math.max(0, Math.min(clientY, window.innerHeight - 1));
    return { x: Math.round(clientX), y: Math.round(clientY) };
}
```

### Outstanding Issues

1. **Dropzone not lighting up**: The dropzone visual feedback (`tc-dragover` class) may not be triggering correctly. Possible causes:
   - DPI scaling causing coordinate misalignment (negative screen coords on secondary monitors)
   - Synthetic `dragenter` not reaching the dropzone element
   - `$tw.dragInProgress` flag stuck from previous internal drag

2. **Content drags from browser failing**: External content drags (not files) were reported as not working, while file drags succeed. This may be related to the internal/external detection - browser-originated drags have `chromium/x-renderer-taint` format even when coming from a different browser window.

3. **CompositionController unavailable**: Cannot be fixed without changes to upstream wry to support visual hosting mode. The original drop target forwarding is the workaround.

### Testing Checklist

- [ ] External file drop (from file manager)
- [ ] External text drop (from another app like Notepad)
- [ ] External URL drop (from browser address bar)
- [ ] Internal tiddler drag (within TiddlyWiki)
- [ ] Dropzone visual feedback (tc-dragover class appears)
- [ ] Multi-monitor with different DPI scaling
- [ ] Secondary monitor with negative screen coordinates

### Relevant Files

- `src-tauri/src/lib.rs`: Windows OLE IDropTarget implementation (lines ~500-1150)
- `src-tauri/src/lib.rs`: JavaScript event handlers injected into wiki windows (lines ~4192+)
- TiddlyWiki5 dropzone widget: `TiddlyWiki5/core/modules/widgets/dropzone.js`

### Key Imports for Windows Drag-Drop

```rust
use windows::Win32::System::Ole::{
    IDropTarget, OleInitialize, RegisterDragDrop, RevokeDragDrop,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::GetPropW;
use windows::core::w;
```

### References

- [WebView2 Windowed vs Visual hosting](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/windowed-vs-visual-hosting)
- [ICoreWebView2CompositionController](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2compositioncontroller)
- [wry GitHub issue #418](https://github.com/tauri-apps/wry/issues/418) - Discussion about using CompositionController for drag-drop
- [OleDropTargetInterface property](https://www.autohotkey.com/boards/viewtopic.php?style=7&t=56720) - Getting existing drop target

### Recent Changes (January 2026)

1. Added `original_drop_target: Option<IDropTarget>` field to `DropTargetImpl`
2. Query original drop target using `GetPropW(hwnd, "OleDropTargetInterface")` before revoking
3. Forward all drags (internal and external) to original drop target when available
4. Fixed Windows API types: use `MODIFIERKEYS_FLAGS(key)` and `&mut effect as *mut DROPEFFECT`
5. CI builds passing on Windows
