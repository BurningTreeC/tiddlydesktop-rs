package com.burningtreec.tiddlydesktop_rs

import android.Manifest
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.Uri
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.webkit.RenderProcessGoneDetail
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {

    companion object {
        private const val TAG = "MainActivity"
        private const val NOTIFICATION_PERMISSION_REQUEST_CODE = 1001

        init {
            System.loadLibrary("tiddlydesktop_rs_lib")
        }
    }

    // JNI: notify Rust that an OAuth deep link arrived with this state token
    private external fun completeAuthDeepLink(state: String)

    // Android 15+: Colored views behind transparent system bars
    private var statusBarBgView: View? = null
    private var navBarBgView: View? = null

    // Receiver for WikiActivity close broadcasts (cross-process, from :wiki process)
    private val wikiClosedReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val wikiPath = intent.getStringExtra("wiki_path") ?: return
            Log.d(TAG, "Received wiki closed broadcast: $wikiPath")
            val webView = findWebView(window.decorView) ?: return
            val escaped = wikiPath.replace("\\", "\\\\").replace("'", "\\'")
            webView.evaluateJavascript(
                "if(window.__tdWikiClosed) window.__tdWikiClosed('$escaped');",
                null
            )
        }
    }

    /**
     * Called from Rust JNI to update the background colors of the system bar views.
     * On API 35+, window.setStatusBarColor/setNavigationBarColor are ignored,
     * so we use actual View elements positioned behind the transparent bars.
     */
    fun setBarBackgroundColors(statusColor: Int, navColor: Int) {
        runOnUiThread {
            statusBarBgView?.setBackgroundColor(statusColor)
            navBarBgView?.setBackgroundColor(navColor)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        // If we were started by WikiActivity to restart the main process for LAN sync,
        // move to background immediately to avoid disrupting the user's wiki window.
        val isRestartForSync = intent?.getBooleanExtra("RESTART_FOR_SYNC", false) == true

        super.onCreate(savedInstanceState)

        if (isRestartForSync) {
            Log.d(TAG, "Started for LAN sync process restart — moving to background")
            window.decorView.post {
                if (!isFinishing) {
                    moveTaskToBack(true)
                    @Suppress("DEPRECATION")
                    overridePendingTransition(0, 0)
                }
            }
        }

        // Android 15+ (API 35+): Edge-to-edge is enforced. Pad the content view
        // so it doesn't render behind the status bar and navigation bar.
        // Also add colored background views to the decorView for palette bar colors.
        if (Build.VERSION.SDK_INT >= 35) {
            val contentView = findViewById<FrameLayout>(android.R.id.content)
            val decorView = window.decorView as FrameLayout

            // Disable system scrim so our background colors show through unmodified
            window.isStatusBarContrastEnforced = false
            window.isNavigationBarContrastEnforced = false

            // Add colored bg views to decorView (not contentView) so they sit at
            // the true screen edges, above everything including Tauri's WebView.
            statusBarBgView = View(this).apply {
                setBackgroundColor(android.graphics.Color.TRANSPARENT)
            }
            navBarBgView = View(this).apply {
                setBackgroundColor(android.graphics.Color.TRANSPARENT)
            }
            decorView.addView(statusBarBgView, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, 0, Gravity.TOP))
            decorView.addView(navBarBgView, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, 0, Gravity.BOTTOM))

            decorView.setOnApplyWindowInsetsListener { _, insets ->
                val systemBars = insets.getInsets(WindowInsets.Type.systemBars())
                // Pad content to keep Tauri's WebView away from system bars
                contentView.setPadding(systemBars.left, systemBars.top, systemBars.right, systemBars.bottom)
                // Size bg views to match system bar heights
                statusBarBgView?.layoutParams?.height = systemBars.top
                navBarBgView?.layoutParams?.height = systemBars.bottom
                statusBarBgView?.requestLayout()
                navBarBgView?.requestLayout()
                insets
            }
        }

        requestNotificationPermission()

        // Register receiver for WikiActivity close broadcasts from :wiki process
        val filter = IntentFilter("com.burningtreec.tiddlydesktop_rs.ACTION_WIKI_CLOSED")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(wikiClosedReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(wikiClosedReceiver, filter)
        }

        // Track that MainActivity is alive for LAN sync service lifecycle
        LanSyncService.setMainActivityAlive(true)

        // Check for OAuth deep link on cold start
        handleAuthDeepLink(intent)

        // Protect Tauri's WebView from renderer crashes that would kill the whole app.
        // Schedule after layout so Tauri has time to create the WebView.
        window.decorView.post {
            installRenderProcessCrashProtection()
        }
    }

    /**
     * Find Tauri's WebView and install crash protection:
     * 1. Wrap the WebViewClient to handle onRenderProcessGone() (prevents app kill)
     * 2. Set renderer priority to IMPORTANT even when not visible (prevents OOM kill
     *    of the renderer when the landing page is in the background)
     */
    private fun installRenderProcessCrashProtection() {
        val webView = findWebView(window.decorView) ?: run {
            Log.w(TAG, "Could not find WebView for crash protection")
            return
        }

        // Keep the renderer process alive even when the activity is in the background.
        // This is critical for LAN sync — the main process must stay alive with its
        // WebView renderer to maintain sync connections.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            webView.setRendererPriorityPolicy(
                WebView.RENDERER_PRIORITY_IMPORTANT,
                false // waivedWhenNotVisible=false → keep high priority even in background
            )
            Log.d(TAG, "Set WebView renderer priority to IMPORTANT (not waived when not visible)")
        }

        // Wrap the existing WebViewClient to add onRenderProcessGone() handling.
        // Try to get the original client via reflection so we can delegate to it.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            var originalClient: WebViewClient? = null
            try {
                val method = WebView::class.java.getMethod("getWebViewClient")
                originalClient = method.invoke(webView) as? WebViewClient
                Log.d(TAG, "Got original WebViewClient: ${originalClient?.javaClass?.name}")
            } catch (e: Exception) {
                Log.w(TAG, "Could not get original WebViewClient via reflection: ${e.message}")
            }

            val activity = this
            val orig = originalClient

            webView.webViewClient = object : WebViewClient() {
                @Deprecated("Deprecated in Java")
                override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean {
                    return orig?.shouldOverrideUrlLoading(view, url)
                        ?: super.shouldOverrideUrlLoading(view, url)
                }

                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                    return orig?.shouldOverrideUrlLoading(view, request)
                        ?: super.shouldOverrideUrlLoading(view, request)
                }

                override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
                    return orig?.shouldInterceptRequest(view, request)
                        ?: super.shouldInterceptRequest(view, request)
                }

                override fun onPageFinished(view: WebView, url: String) {
                    if (orig != null) orig.onPageFinished(view, url)
                    else super.onPageFinished(view, url)
                }

                override fun onPageStarted(view: WebView, url: String?, favicon: android.graphics.Bitmap?) {
                    if (orig != null) orig.onPageStarted(view, url, favicon)
                    else super.onPageStarted(view, url, favicon)
                }

                override fun onReceivedError(view: WebView, request: WebResourceRequest, error: android.webkit.WebResourceError) {
                    if (orig != null) orig.onReceivedError(view, request, error)
                    else super.onReceivedError(view, request, error)
                }

                override fun onReceivedSslError(view: WebView, handler: android.webkit.SslErrorHandler, error: android.net.http.SslError) {
                    if (orig != null) orig.onReceivedSslError(view, handler, error)
                    else super.onReceivedSslError(view, handler, error)
                }

                override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, errorResponse: WebResourceResponse) {
                    if (orig != null) orig.onReceivedHttpError(view, request, errorResponse)
                    else super.onReceivedHttpError(view, request, errorResponse)
                }

                override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
                    Log.e(TAG, "WebView render process gone! didCrash=${detail.didCrash()}, " +
                        "rendererPriorityAtExit=${detail.rendererPriorityAtExit()}")
                    // Return true to prevent AwBrowserTerminator from killing the whole app.
                    // The landing page WebView is dead — recreate the activity to get a fresh one.
                    try {
                        // Remove the dead WebView from the hierarchy to avoid further crashes
                        (view.parent as? ViewGroup)?.removeView(view)
                        view.destroy()
                    } catch (e: Exception) {
                        Log.e(TAG, "Error cleaning up dead WebView: ${e.message}")
                    }
                    // Recreate the activity to get a fresh Tauri WebView
                    activity.recreate()
                    return true
                }
            }

            Log.d(TAG, "WebView crash protection installed (original client: ${orig != null})")
        }
    }

    /**
     * Recursively find the first WebView in the view hierarchy.
     */
    private fun findWebView(view: View): WebView? {
        if (view is WebView) return view
        if (view is ViewGroup) {
            for (i in 0 until view.childCount) {
                val result = findWebView(view.getChildAt(i))
                if (result != null) return result
            }
        }
        return null
    }

    /**
     * Request POST_NOTIFICATIONS permission on Android 13+
     */
    private fun requestNotificationPermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED) {
                Log.d(TAG, "Requesting POST_NOTIFICATIONS permission")
                ActivityCompat.requestPermissions(
                    this,
                    arrayOf(Manifest.permission.POST_NOTIFICATIONS),
                    NOTIFICATION_PERMISSION_REQUEST_CODE
                )
            } else {
                Log.d(TAG, "POST_NOTIFICATIONS permission already granted")
            }
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == NOTIFICATION_PERMISSION_REQUEST_CODE) {
            if (grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED) {
                Log.d(TAG, "POST_NOTIFICATIONS permission granted")
            } else {
                Log.d(TAG, "POST_NOTIFICATIONS permission denied")
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        if (intent.getBooleanExtra("RESTART_FOR_SYNC", false)) {
            Log.d(TAG, "New intent for LAN sync restart — moving to background")
            moveTaskToBack(true)
            @Suppress("DEPRECATION")
            overridePendingTransition(0, 0)
        }
        handleAuthDeepLink(intent)
    }

    /**
     * Handle OAuth deep link: tiddlydesktop://auth?state=...
     * Called from both onCreate() (cold start) and onNewIntent() (warm start).
     */
    private fun handleAuthDeepLink(intent: Intent?) {
        val uri = intent?.data ?: return
        if (uri.scheme != "tiddlydesktop" || uri.host != "auth") return
        val state = uri.getQueryParameter("state") ?: return
        Log.d(TAG, "OAuth deep link received, state=${state.take(8)}...")
        try {
            completeAuthDeepLink(state)
        } catch (e: Exception) {
            Log.e(TAG, "Failed to complete auth deep link: ${e.message}")
        }
    }

    override fun onDestroy() {
        try {
            unregisterReceiver(wikiClosedReceiver)
        } catch (_: Exception) {}
        super.onDestroy()
        // When the user presses Back (isFinishing=true), notify LAN sync service.
        // It will stop if no wiki activities are open.
        if (isFinishing) {
            Log.d(TAG, "MainActivity finishing — notifying LAN sync service")
            LanSyncService.setMainActivityAlive(false, applicationContext)
        }
    }
}
