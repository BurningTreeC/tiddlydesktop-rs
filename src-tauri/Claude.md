# TiddlyDesktop Development Notes

## Windows Drag & Drop Implementation

### Current State (January 2026)

The Windows drag-drop implementation uses a custom OLE `IDropTarget` to intercept drag events before WebView2's default handling. This allows capturing content from external applications (text, HTML, URLs) that WebView2 would otherwise block.

### Architecture

**Location**: `src-tauri/src/lib.rs` (windows_drag module)

**Components**:
- `DropTargetImpl`: Custom COM IDropTarget implementation
- Registered on the parent HWND (Tauri window), not the WebView2 content window
- Emits Tauri events (`td-drag-motion`, `td-drag-leave`, `td-drag-content`, etc.) for JavaScript handling
- JavaScript side creates synthetic DragEvents to trigger TiddlyWiki's dropzone widget

### Internal vs External Drag Detection

Drags are classified by checking for the `chromium/x-renderer-taint` clipboard format:
- **Present**: Internal drag (originated from within WebView2)
- **Absent**: External drag (from file manager, other apps, etc.)

### Known Limitation: ICoreWebView2CompositionController3

**Problem**: The code attempts to use `ICoreWebView2CompositionController3` for forwarding drag events to WebView2, but this interface is **not available** in Tauri/wry.

**Root Cause**:
- wry creates WebView2 using `CreateCoreWebView2Controller` (windowed hosting mode)
- `ICoreWebView2CompositionController3` is only available when using `CreateCoreWebView2CompositionController` (visual/composition hosting mode)
- These are separate interface hierarchies - you cannot cast from Controller to CompositionController

**Current Workaround** (implemented but untested on Windows):
1. Before revoking the existing drop target, query it using `GetPropW(hwnd, "OleDropTargetInterface")`
2. Store the original drop target reference
3. For internal drags, forward to the original drop target instead of trying to use CompositionController3
4. The original drop target should be wry's internal handler which can process internal WebView2 drags

### Event Flow

**External Drags**:
1. OLE `DragEnter` → emit `td-drag-motion` with screen coordinates
2. JavaScript converts to client coordinates, creates synthetic `dragenter`/`dragover` events
3. TiddlyWiki dropzone widget receives events, shows visual feedback
4. OLE `Drop` → extract content, emit `td-drag-content` and `td-drag-drop-position`
5. JavaScript dispatches synthetic `drop` event with the content

**Internal Drags**:
1. OLE `DragEnter` → detect via `chromium/x-renderer-taint` format
2. Forward to original drop target (wry's handler)
3. Skip emitting td-* events to avoid interference

### Outstanding Issues

1. **Dropzone not lighting up**: The dropzone visual feedback (`tc-dragover` class) may not be triggering correctly. Possible causes:
   - DPI scaling causing coordinate misalignment (negative screen coords on secondary monitors)
   - Synthetic `dragenter` not reaching the dropzone element
   - `$tw.dragInProgress` flag stuck from previous internal drag

2. **Content drags from browser failing**: External content drags (not files) were reported as not working, while file drags succeed. This may be related to the internal/external detection or event handling.

3. **CompositionController unavailable**: Cannot be fixed without changes to upstream wry to support visual hosting mode. The original drop target forwarding is a workaround.

### Testing Checklist

- [ ] External file drop (from file manager)
- [ ] External text drop (from another app)
- [ ] External URL drop (from browser address bar)
- [ ] Internal tiddler drag (within TiddlyWiki)
- [ ] Dropzone visual feedback (tc-dragover class appears)
- [ ] Multi-monitor with different DPI scaling
- [ ] Secondary monitor with negative screen coordinates

### Relevant Files

- `src-tauri/src/lib.rs`: Windows OLE IDropTarget implementation (lines ~500-1130)
- `src-tauri/src/lib.rs`: JavaScript event handlers injected into wiki windows (lines ~4192+)
- TiddlyWiki5 dropzone widget: handles the synthetic drag events

### References

- [WebView2 Windowed vs Visual hosting](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/windowed-vs-visual-hosting)
- [ICoreWebView2CompositionController](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2compositioncontroller)
- [wry GitHub issue #418](https://github.com/tauri-apps/wry/issues/418) - Discussion about using CompositionController for drag-drop
