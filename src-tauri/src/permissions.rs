//! Auto-grant WebView permissions (camera, microphone, geolocation) on desktop platforms.
//!
//! - **Windows**: WebView2 `PermissionRequested` event handler that grants all permission kinds
//! - **Linux**: WebKitGTK `permission-request` signal handler that allows all requests
//! - **macOS**: No-op — WRY fork handles media capture; geolocation via Info.plist entries

use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{Manager, Wry};

pub fn init_plugin() -> TauriPlugin<Wry> {
    PluginBuilder::<Wry, ()>::new("permissions")
        .on_webview_ready(|webview| {
            let app_handle = webview.app_handle();
            let label = webview.label();

            if let Some(window) = app_handle.get_webview_window(label) {
                setup_permission_handlers(&window);
            }
        })
        .build()
}

fn setup_permission_handlers(window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    setup_windows(window);

    #[cfg(target_os = "linux")]
    setup_linux(window);

    // macOS: no-op (WRY fork auto-grants media; geolocation via Info.plist)
    #[cfg(target_os = "macos")]
    let _ = window;
}

#[cfg(target_os = "windows")]
fn setup_windows(window: &tauri::WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2PermissionRequestedEventArgs,
        ICoreWebView2PermissionRequestedEventHandler,
        ICoreWebView2PermissionRequestedEventHandler_Impl,
        COREWEBVIEW2_PERMISSION_STATE_ALLOW,
    };
    use windows::core::implement;
    use windows::core::Ref;

    #[implement(ICoreWebView2PermissionRequestedEventHandler)]
    struct PermissionHandler;

    impl ICoreWebView2PermissionRequestedEventHandler_Impl for PermissionHandler_Impl {
        fn Invoke(
            &self,
            _sender: Ref<'_, ICoreWebView2>,
            args: Ref<'_, ICoreWebView2PermissionRequestedEventArgs>,
        ) -> windows::core::Result<()> {
            unsafe {
                if let Some(args) = &*args {
                    args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                }
            }
            Ok(())
        }
    }

    let label = window.label().to_string();
    let _ = window.with_webview(move |webview| {
        #[cfg(windows)]
        unsafe {
            let controller = webview.controller();
            match controller.CoreWebView2() {
                Ok(core) => {
                    let handler: ICoreWebView2PermissionRequestedEventHandler =
                        PermissionHandler.into();
                    let mut token: i64 = 0;
                    match core.add_PermissionRequested(&handler, &mut token) {
                        Ok(()) => eprintln!(
                            "[TiddlyDesktop] Windows: Permission handler registered for '{}'",
                            label
                        ),
                        Err(e) => eprintln!(
                            "[TiddlyDesktop] Windows: Failed to register permission handler: {:?}",
                            e
                        ),
                    }
                }
                Err(e) => eprintln!(
                    "[TiddlyDesktop] Windows: Failed to get CoreWebView2: {:?}",
                    e
                ),
            }
        }
    });
}

#[cfg(target_os = "linux")]
fn setup_linux(window: &tauri::WebviewWindow) {
    use webkit2gtk::{PermissionRequestExt, WebViewExt};

    let label = window.label().to_string();
    let _ = window.with_webview(move |webview| {
        let wk_webview = webview.inner();
        wk_webview.connect_permission_request(move |_view, request| {
            eprintln!(
                "[TiddlyDesktop] Linux: Auto-granting permission request for '{}'",
                label
            );
            request.allow();
            true // handled
        });
    });
}
