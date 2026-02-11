package com.burningtreec.tiddlydesktop_rs

import android.annotation.SuppressLint
import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Color
import android.media.MediaMetadataRetriever
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.util.Base64
import android.util.Log
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.webkit.JavascriptInterface
import android.webkit.MimeTypeMap
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.documentfile.provider.DocumentFile
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.IOException
import java.net.URLDecoder
import java.net.URLEncoder
import java.util.concurrent.ConcurrentHashMap

/**
 * Activity for opening individual wiki files in separate app instances.
 * Each wiki opens in its own task (visible as separate entry in recent apps).
 *
 * This is a standalone WebView activity that doesn't use Tauri infrastructure.
 * It simply loads the wiki URL (http://127.0.0.1:port) in a WebView.
 *
 * The wiki URL must be passed via Intent extras:
 * - EXTRA_WIKI_PATH: The wiki path/URI (used as unique identifier)
 * - EXTRA_WIKI_URL: The URL to load (required)
 * - EXTRA_WIKI_TITLE: Display name for the wiki (optional)
 */
class WikiActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_WIKI_PATH = "wiki_path"
        const val EXTRA_WIKI_URL = "wiki_url"  // Server URL for folder wikis (Node.js --listen)
        const val EXTRA_WIKI_TITLE = "wiki_title"
        const val EXTRA_IS_FOLDER = "is_folder"
        const val EXTRA_BACKUPS_ENABLED = "backups_enabled"
        const val EXTRA_BACKUP_COUNT = "backup_count"
        const val EXTRA_TIDDLER_TITLE = "tiddler_title"  // For tm-open-window: navigate to specific tiddler
        private const val TAG = "WikiActivity"

        init {
            // Load the native library for JNI calls
            try {
                System.loadLibrary("tiddlydesktop_rs_lib")
                Log.d(TAG, "Native library loaded successfully")
            } catch (e: UnsatisfiedLinkError) {
                Log.e(TAG, "Failed to load native library: ${e.message}")
            }
        }

        /**
         * Native method to clean up local wiki copies for folder wikis.
         * Called from onDestroy() for folder wikis that use Node.js server.
         */
        @JvmStatic
        external fun cleanupWikiLocalCopy(wikiPath: String, isFolder: Boolean)

        /**
         * Check if a wiki is already open by scanning running tasks.
         * Returns the task ID if open, or -1 if not.
         * Works across processes via ActivityManager.
         */
        @JvmStatic
        fun getOpenWikiTaskId(context: Context, wikiPath: String): Int {
            Log.d(TAG, "getOpenWikiTaskId: checking for $wikiPath")

            val appTask = findWikiTask(context, wikiPath)
            if (appTask != null) {
                val taskId = appTask.taskInfo.taskId
                Log.d(TAG, "getOpenWikiTaskId: found task with taskId $taskId")
                return taskId
            }

            Log.d(TAG, "getOpenWikiTaskId: not found")
            return -1
        }

        /**
         * Find an existing wiki task by scanning app tasks.
         * Works across processes via ActivityManager.
         * Returns the AppTask if found, null otherwise.
         */
        @JvmStatic
        fun findWikiTask(context: Context, wikiPath: String): ActivityManager.AppTask? {
            Log.d(TAG, "findWikiTask: scanning for $wikiPath")

            try {
                val activityManager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
                val appTasks = activityManager.appTasks

                Log.d(TAG, "findWikiTask: found ${appTasks.size} app tasks")

                for (task in appTasks) {
                    try {
                        val taskInfo = task.taskInfo
                        val baseIntent = taskInfo.baseIntent
                        val component = baseIntent.component

                        // Check if this is a WikiActivity task
                        if (component?.className == WikiActivity::class.java.name) {
                            val taskWikiPath = baseIntent.getStringExtra(EXTRA_WIKI_PATH)
                            Log.d(TAG, "findWikiTask: found WikiActivity task with path: $taskWikiPath")

                            if (taskWikiPath == wikiPath) {
                                Log.d(TAG, "findWikiTask: MATCH! taskId=${taskInfo.taskId}")
                                return task
                            }
                        }
                    } catch (e: Exception) {
                        Log.w(TAG, "findWikiTask: error checking task: ${e.message}")
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "findWikiTask: error scanning tasks: ${e.message}")
            }

            Log.d(TAG, "findWikiTask: no matching task found")
            return null
        }

        /**
         * Bring an existing wiki task to the foreground.
         * Returns true if successful.
         */
        @JvmStatic
        fun bringWikiToFront(context: Context, wikiPath: String): Boolean {
            Log.d(TAG, "bringWikiToFront: attempting for $wikiPath")

            // Use AppTask API to find and bring task to front
            val appTask = findWikiTask(context, wikiPath)
            if (appTask != null) {
                try {
                    appTask.moveToFront()
                    Log.d(TAG, "bringWikiToFront: success via AppTask API")
                    return true
                } catch (e: Exception) {
                    Log.e(TAG, "bringWikiToFront: AppTask.moveToFront failed: ${e.message}")
                }
            }

            Log.d(TAG, "bringWikiToFront: failed - no task found")
            return false
        }

        /**
         * Check if any wiki is open by scanning running tasks.
         * Works across processes via ActivityManager.
         */
        @JvmStatic
        fun hasOpenWikis(context: Context): Boolean {
            try {
                val activityManager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
                val appTasks = activityManager.appTasks

                for (task in appTasks) {
                    try {
                        val taskInfo = task.taskInfo
                        val component = taskInfo.baseIntent.component
                        if (component?.className == WikiActivity::class.java.name) {
                            Log.d(TAG, "hasOpenWikis: found open wiki task")
                            return true
                        }
                    } catch (e: Exception) {
                        // Ignore individual task errors
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "hasOpenWikis: error scanning tasks: ${e.message}")
            }
            return false
        }

        /**
         * Update recent_wikis.json with a newly opened wiki.
         * This file is read by the home screen widget to display recent wikis.
         * Maximum 10 recent wikis are stored, sorted by last opened time.
         */
        @JvmStatic
        fun updateRecentWikis(context: Context, wikiPath: String, wikiTitle: String, isFolder: Boolean) {
            try {
                val recentWikisFile = File(context.filesDir, "recent_wikis.json")
                val maxWikis = 10

                // Read existing data
                val existingWikis = if (recentWikisFile.exists()) {
                    try {
                        JSONArray(recentWikisFile.readText())
                    } catch (e: Exception) {
                        Log.w(TAG, "Could not parse recent_wikis.json, starting fresh: ${e.message}")
                        JSONArray()
                    }
                } else {
                    JSONArray()
                }

                // Remove this wiki if it already exists (we'll re-add it at the top)
                val filteredWikis = JSONArray()
                for (i in 0 until existingWikis.length()) {
                    val wiki = existingWikis.optJSONObject(i)
                    if (wiki != null && wiki.optString("path") != wikiPath) {
                        filteredWikis.put(wiki)
                    }
                }

                // Create new entry for this wiki
                val newEntry = JSONObject().apply {
                    put("path", wikiPath)
                    put("title", wikiTitle)
                    put("is_folder", isFolder)
                    put("last_opened", System.currentTimeMillis())
                }

                // Build new list with this wiki at the top
                val newWikis = JSONArray()
                newWikis.put(newEntry)
                for (i in 0 until minOf(filteredWikis.length(), maxWikis - 1)) {
                    newWikis.put(filteredWikis.getJSONObject(i))
                }

                // Write to file
                recentWikisFile.writeText(newWikis.toString(2))
                Log.d(TAG, "Updated recent_wikis.json with: $wikiTitle")
            } catch (e: Exception) {
                Log.e(TAG, "Failed to update recent_wikis.json: ${e.message}")
            }
        }

        /**
         * Update recent_wikis.json with favicon path for an existing wiki entry.
         */
        @JvmStatic
        fun updateRecentWikisWithFavicon(context: Context, wikiPath: String, faviconPath: String) {
            try {
                val recentWikisFile = File(context.filesDir, "recent_wikis.json")

                if (!recentWikisFile.exists()) {
                    Log.w(TAG, "recent_wikis.json does not exist, cannot update favicon")
                    return
                }

                val existingWikis = JSONArray(recentWikisFile.readText())
                var updated = false

                // Find and update the wiki entry with the favicon path
                for (i in 0 until existingWikis.length()) {
                    val wiki = existingWikis.optJSONObject(i)
                    if (wiki != null && wiki.optString("path") == wikiPath) {
                        wiki.put("favicon_path", faviconPath)
                        updated = true
                        break
                    }
                }

                if (updated) {
                    recentWikisFile.writeText(existingWikis.toString(2))
                    Log.d(TAG, "Updated favicon path for wiki: $wikiPath")
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to update favicon in recent_wikis.json: ${e.message}")
            }
        }
    }

    private lateinit var webView: WebView
    private lateinit var rootLayout: FrameLayout
    // Android 15+: Colored views behind transparent system bars
    private var statusBarBgView: View? = null
    private var navBarBgView: View? = null
    private var wikiPath: String? = null
    private var wikiTitle: String = "TiddlyWiki"
    private var isFolder: Boolean = false
    private var httpServer: WikiHttpServer? = null

    // Fullscreen video support
    private var fullscreenView: View? = null
    private var fullscreenCallback: WebChromeClient.CustomViewCallback? = null

    // App immersive fullscreen mode (toggled via tm-full-screen)
    private var isImmersiveFullscreen: Boolean = false

    // Flag to track if this window was opened via tm-open-window
    // If true, back button returns to parent window instead of closing
    private var isChildWindow: Boolean = false

    // WakeLock to keep the HTTP server alive when app is in background
    private var wakeLock: PowerManager.WakeLock? = null

    // File chooser support for import functionality
    private var filePathCallback: ValueCallback<Array<Uri>>? = null
    private lateinit var fileChooserLauncher: ActivityResultLauncher<Intent>

    // Export/save file support
    private lateinit var createDocumentLauncher: ActivityResultLauncher<Intent>
    private var pendingExportContent: ByteArray? = null
    private var pendingExportCallback: String? = null

    // Auth overlay WebView for session auth login
    private var authOverlayContainer: FrameLayout? = null
    private var authWebView: WebView? = null

    /**
     * JavaScript interface for receiving palette color updates from TiddlyWiki.
     */
    inner class PaletteInterface {
        @JavascriptInterface
        fun setSystemBarColors(statusBarColor: String, navBarColor: String, foregroundColor: String?) {
            Log.d(TAG, "setSystemBarColors called: status=$statusBarColor, nav=$navBarColor, fg=$foregroundColor")
            runOnUiThread {
                updateSystemBarColors(statusBarColor, navBarColor)
            }
        }
    }

    /**
     * JavaScript interface for server control (restart on disconnect).
     */
    inner class ServerInterface {
        /**
         * Restart the HTTP server and navigate to the new URL.
         * This clears the WebView history to prevent back-navigation to dead server URLs.
         * Returns JSON with success status.
         */
        @JavascriptInterface
        fun restartServerAndNavigate(): String {
            Log.d(TAG, "restartServerAndNavigate called from JavaScript")
            return try {
                if (httpServer != null) {
                    val newUrl = httpServer!!.restart()
                    Log.d(TAG, "Server restarted at: $newUrl")

                    // Load the new URL and clear history on the UI thread
                    runOnUiThread {
                        webView.clearHistory()
                        webView.loadUrl(newUrl)
                    }

                    "{\"success\":true}"
                } else {
                    Log.e(TAG, "No HTTP server to restart")
                    "{\"success\":false,\"error\":\"No server available\"}"
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to restart server: ${e.message}")
                val escapedError = e.message?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: "Unknown error"
                "{\"success\":false,\"error\":\"$escapedError\"}"
            }
        }

        /**
         * Legacy method for backwards compatibility.
         * Prefer restartServerAndNavigate() to avoid back-navigation issues.
         */
        @JavascriptInterface
        fun restartServer(): String {
            Log.d(TAG, "restartServer called from JavaScript")
            return try {
                if (httpServer != null) {
                    val newUrl = httpServer!!.restart()
                    Log.d(TAG, "Server restarted at: $newUrl")
                    "{\"success\":true,\"url\":\"$newUrl\"}"
                } else {
                    Log.e(TAG, "No HTTP server to restart")
                    "{\"success\":false,\"error\":\"No server available\"}"
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to restart server: ${e.message}")
                val escapedError = e.message?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: "Unknown error"
                "{\"success\":false,\"error\":\"$escapedError\"}"
            }
        }

        /**
         * Check if the server is running.
         */
        @JavascriptInterface
        fun isServerRunning(): Boolean {
            return httpServer?.isRunning() ?: false
        }

        /**
         * Check if this is a single-file wiki (has local HTTP server).
         */
        @JavascriptInterface
        fun isSingleFileWiki(): Boolean {
            return !isFolder && httpServer != null
        }

        /**
         * Toggle immersive fullscreen mode.
         * Returns JSON with the new fullscreen state.
         */
        @JavascriptInterface
        fun toggleFullscreen(): String {
            Log.d(TAG, "toggleFullscreen called from JavaScript")
            return try {
                runOnUiThread {
                    isImmersiveFullscreen = !isImmersiveFullscreen
                    if (isImmersiveFullscreen) {
                        enterImmersiveMode()
                    } else {
                        exitImmersiveMode()
                    }
                }
                "{\"success\":true,\"fullscreen\":$isImmersiveFullscreen}"
            } catch (e: Exception) {
                Log.e(TAG, "Failed to toggle fullscreen: ${e.message}")
                val escapedError = e.message?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: "Unknown error"
                "{\"success\":false,\"error\":\"$escapedError\"}"
            }
        }

        /**
         * Check if currently in immersive fullscreen mode.
         */
        @JavascriptInterface
        fun isFullscreen(): Boolean {
            return isImmersiveFullscreen
        }

        /**
         * Exit video fullscreen (onShowCustomView/onHideCustomView).
         * Called from the exitFullscreen stub when Plyr or other players
         * request exiting fullscreen via the Fullscreen API.
         */
        @JavascriptInterface
        fun exitVideoFullscreen() {
            Log.d(TAG, "exitVideoFullscreen called from JavaScript")
            runOnUiThread {
                if (fullscreenView != null) {
                    fullscreenCallback?.onCustomViewHidden()
                }
            }
        }

        /**
         * Get the current attachment server URL.
         * Used by JavaScript to dynamically resolve attachment URLs after server restart.
         */
        @JavascriptInterface
        fun getAttachmentServerUrl(): String {
            return if (httpServer != null) "http://127.0.0.1:${httpServer!!.port}" else ""
        }
    }

    /**
     * JavaScript interface for clipboard operations.
     * Handles copy-to-clipboard since document.execCommand doesn't work reliably in WebView.
     */
    inner class ClipboardInterface {
        /**
         * Copy text to the system clipboard.
         * Must run on the UI thread — setPrimaryClip silently fails on the JavaBridge thread
         * on some Android versions.
         * Returns JSON with success status.
         */
        @JavascriptInterface
        fun copyText(text: String): String {
            Log.d(TAG, "copyText called: ${text.take(50)}...")
            return try {
                // Post to UI thread — ClipboardManager requires it on some devices
                runOnUiThread {
                    try {
                        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                        val clip = ClipData.newPlainText("TiddlyWiki", text)
                        clipboard.setPrimaryClip(clip)
                        Log.d(TAG, "Text copied to clipboard on UI thread: ${text.length} chars")
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to copy to clipboard on UI thread: ${e.message}")
                    }
                }
                "{\"success\":true}"
            } catch (e: Exception) {
                Log.e(TAG, "Failed to copy to clipboard: ${e.message}")
                val escapedError = e.message?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: "Unknown error"
                "{\"success\":false,\"error\":\"$escapedError\"}"
            }
        }

        /**
         * Get text from the system clipboard.
         * Returns JSON with the text or error.
         */
        @JavascriptInterface
        fun getText(): String {
            Log.d(TAG, "getText called")
            return try {
                val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                val clip = clipboard.primaryClip
                if (clip != null && clip.itemCount > 0) {
                    val text = clip.getItemAt(0).coerceToText(this@WikiActivity).toString()
                    val escapedText = text.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n").replace("\r", "\\r")
                    Log.d(TAG, "Got clipboard text: ${text.length} chars")
                    "{\"success\":true,\"text\":\"$escapedText\"}"
                } else {
                    "{\"success\":true,\"text\":\"\"}"
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to get clipboard: ${e.message}")
                val escapedError = e.message?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: "Unknown error"
                "{\"success\":false,\"error\":\"$escapedError\"}"
            }
        }
    }

    /**
     * JavaScript interface for printing.
     * Handles tm-print message from TiddlyWiki.
     */
    inner class PrintInterface {
        @JavascriptInterface
        fun print() {
            Log.d(TAG, "print() called")
            runOnUiThread {
                exitFullscreenIfNeeded()
                try {
                    val printManager = getSystemService(Context.PRINT_SERVICE) as android.print.PrintManager
                    val jobName = "$wikiTitle - ${System.currentTimeMillis()}"
                    val printAdapter = webView.createPrintDocumentAdapter(jobName)
                    printManager.print(jobName, printAdapter, null)
                    Log.d(TAG, "Print job started: $jobName")
                } catch (e: Exception) {
                    Log.e(TAG, "Print failed: ${e.message}")
                }
            }
        }
    }

    /**
     * JavaScript interface for opening URLs in external browser/apps.
     * Handles tm-open-external-window message from TiddlyWiki.
     */
    inner class ExternalWindowInterface {
        @JavascriptInterface
        fun openExternal(url: String): String {
            Log.d(TAG, "openExternal called: $url")
            return try {
                runOnUiThread {
                    val intent = Intent(Intent.ACTION_VIEW, Uri.parse(url))
                    startActivity(intent)
                }
                "{\"success\":true}"
            } catch (e: Exception) {
                Log.e(TAG, "Failed to open external URL: ${e.message}")
                "{\"success\":false,\"error\":\"${e.message?.replace("\"", "\\\"") ?: "Unknown"}\"}"
            }
        }

        @JavascriptInterface
        fun openAuthUrl(url: String): String {
            Log.d(TAG, "openAuthUrl called: $url")
            return try {
                runOnUiThread {
                    showAuthOverlay(url)
                }
                "{\"success\":true}"
            } catch (e: Exception) {
                Log.e(TAG, "Failed to open auth URL: ${e.message}")
                "{\"success\":false,\"error\":\"${e.message?.replace("\"", "\\\"") ?: "Unknown"}\"}"
            }
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun showAuthOverlay(url: String) {
        // Remove existing overlay if any
        dismissAuthOverlay()

        val container = FrameLayout(this)
        container.setBackgroundColor(Color.WHITE)

        // Close button bar at top
        val closeBar = FrameLayout(this).apply {
            setBackgroundColor(0xFF424242.toInt())
            val density = resources.displayMetrics.density
            val barHeight = (48 * density).toInt()
            layoutParams = FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, barHeight
            )
        }

        val closeButton = android.widget.TextView(this).apply {
            text = "\u2715  Close"
            setTextColor(Color.WHITE)
            textSize = 16f
            val density = resources.displayMetrics.density
            setPadding((16 * density).toInt(), (12 * density).toInt(), (16 * density).toInt(), (12 * density).toInt())
            setOnClickListener { dismissAuthOverlay() }
        }
        closeBar.addView(closeButton)
        container.addView(closeBar)

        // WebView for auth
        val density = resources.displayMetrics.density
        val barHeight = (48 * density).toInt()
        val wv = WebView(this).apply {
            layoutParams = FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT
            ).apply {
                topMargin = barHeight
            }
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            @Suppress("DEPRECATION")
            settings.databaseEnabled = true
            settings.userAgentString = settings.userAgentString?.replace("; wv", "")
            webViewClient = WebViewClient()
            webChromeClient = WebChromeClient()
        }
        container.addView(wv)

        rootLayout.addView(container, FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.MATCH_PARENT,
            FrameLayout.LayoutParams.MATCH_PARENT
        ))

        authOverlayContainer = container
        authWebView = wv
        wv.loadUrl(url)
        Log.d(TAG, "Auth overlay shown for: $url")
    }

    private fun dismissAuthOverlay() {
        authWebView?.let { wv ->
            wv.stopLoading()
            wv.destroy()
        }
        authWebView = null
        authOverlayContainer?.let { container ->
            rootLayout.removeView(container)
        }
        authOverlayContainer = null
        Log.d(TAG, "Auth overlay dismissed")
    }

    /**
     * JavaScript interface for favicon extraction.
     * Receives favicon data from TiddlyWiki and saves it to disk.
     */
    inner class FaviconInterface {
        @JavascriptInterface
        fun saveFavicon(base64Data: String, mimeType: String): String {
            Log.d(TAG, "saveFavicon called: mimeType=$mimeType, dataLength=${base64Data.length}")
            return try {
                if (base64Data.isEmpty()) {
                    Log.d(TAG, "No favicon data provided")
                    return "{\"success\":false,\"error\":\"No favicon data\"}"
                }

                // Create favicons directory
                val faviconsDir = File(applicationContext.filesDir, "favicons")
                if (!faviconsDir.exists()) {
                    faviconsDir.mkdirs()
                }

                // Generate filename based on MD5 hash of wiki path (collision-proof)
                val pathHash = md5Hash(wikiPath ?: "unknown")
                val imageData = android.util.Base64.decode(base64Data, android.util.Base64.DEFAULT)

                // Try to decode with BitmapFactory and re-save as PNG.
                // This normalizes all formats (ICO, BMP, etc.) to PNG for
                // consistent display on the landing page.
                val bitmap = android.graphics.BitmapFactory.decodeByteArray(imageData, 0, imageData.size)
                val extension = if (bitmap != null) {
                    "png"  // Always save as PNG when decodable
                } else if (mimeType.contains("svg")) {
                    "svg"  // SVGs can't be decoded by BitmapFactory
                } else {
                    "ico"  // Fallback for truly undecodable formats
                }

                // Clean up old favicon files for this wiki (different extensions)
                val allExtensions = listOf("png", "jpg", "gif", "svg", "ico")
                for (ext in allExtensions) {
                    if (ext != extension) {
                        val oldFile = File(faviconsDir, "$pathHash.$ext")
                        if (oldFile.exists()) {
                            oldFile.delete()
                            Log.d(TAG, "Deleted old favicon: ${oldFile.name}")
                        }
                    }
                }

                val faviconFile = File(faviconsDir, "$pathHash.$extension")

                // Save: re-encode as PNG if we decoded successfully, raw bytes otherwise
                if (bitmap != null) {
                    java.io.FileOutputStream(faviconFile).use { out ->
                        bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, out)
                    }
                } else {
                    faviconFile.writeBytes(imageData)
                }

                Log.d(TAG, "Saved favicon to: ${faviconFile.absolutePath} for wiki: $wikiPath")

                // Update recent_wikis.json with favicon path
                updateRecentWikisWithFavicon(applicationContext, wikiPath ?: "", faviconFile.absolutePath)

                "{\"success\":true,\"path\":\"${faviconFile.absolutePath}\"}"
            } catch (e: Exception) {
                Log.e(TAG, "Failed to save favicon: ${e.message}")
                "{\"success\":false,\"error\":\"${e.message?.replace("\"", "\\\"") ?: "Unknown"}\"}"
            }
        }

        /**
         * Look for favicon in the local wiki-mirrors copy (folder wikis).
         * TiddlyWiki stores $:/favicon.ico as tiddlers/$__favicon.ico on disk.
         */
        @JavascriptInterface
        fun extractFaviconFromLocalCopy(): String {
            if (!isFolder || wikiPath.isNullOrEmpty()) {
                return "{\"success\":false,\"error\":\"Not a folder wiki\"}"
            }
            return try {
                // Use filesDir.parentFile to match Tauri's app_data_dir() on Android
                val dataDir = applicationContext.filesDir.parentFile
                    ?: return "{\"success\":false,\"error\":\"Cannot get data dir\"}"
                val uriHash = md5Hash(wikiPath!!)
                val mirrorsDir = File(dataDir, "wiki-mirrors/$uriHash/tiddlers")
                Log.d(TAG, "Looking for favicon in: ${mirrorsDir.absolutePath}")

                if (!mirrorsDir.exists()) {
                    return "{\"success\":false,\"error\":\"No local mirror found\"}"
                }

                // Look for $__favicon.ico (TW5 stores $:/favicon.ico as $__favicon.ico)
                val candidates = listOf("\$__favicon.ico")
                var faviconFile: File? = null
                for (name in candidates) {
                    val f = File(mirrorsDir, name)
                    if (f.exists() && f.length() > 0) {
                        faviconFile = f
                        break
                    }
                }

                // Also check for files with .meta companion (binary tiddler pattern)
                if (faviconFile == null) {
                    val files = mirrorsDir.listFiles() ?: emptyArray()
                    for (f in files) {
                        if (f.name.contains("favicon", ignoreCase = true) && !f.name.endsWith(".meta")) {
                            faviconFile = f
                            break
                        }
                    }
                }

                if (faviconFile == null) {
                    return "{\"success\":false,\"error\":\"No favicon file found in tiddlers/\"}"
                }

                Log.d(TAG, "Found local favicon: ${faviconFile.absolutePath} (${faviconFile.length()} bytes)")

                // Determine MIME type from .meta file or extension
                val metaFile = File(faviconFile.absolutePath + ".meta")
                var mimeType = "image/x-icon"
                if (metaFile.exists()) {
                    val metaText = metaFile.readText()
                    val typeMatch = Regex("type:\\s*(.+)").find(metaText)
                    if (typeMatch != null) {
                        mimeType = typeMatch.groupValues[1].trim()
                    }
                }

                // Read and base64-encode the file, then save via saveFavicon
                val imageData = faviconFile.readBytes()
                val base64Data = android.util.Base64.encodeToString(imageData, android.util.Base64.NO_WRAP)
                saveFavicon(base64Data, mimeType)
            } catch (e: Exception) {
                Log.e(TAG, "Failed to extract favicon from local copy: ${e.message}")
                "{\"success\":false,\"error\":\"${e.message?.replace("\"", "\\\"") ?: "Unknown"}\"}"
            }
        }

        private fun md5Hash(input: String): String {
            val md = java.security.MessageDigest.getInstance("MD5")
            val digest = md.digest(input.toByteArray())
            return digest.joinToString("") { "%02x".format(it) }
        }
    }

    /**
     * JavaScript interface for video poster frame extraction.
     * Uses native MediaMetadataRetriever for fast, direct file access.
     * Caches results to disk so posters survive wiki reopens.
     */
    inner class PosterInterface {
        /**
         * Extract a poster frame from a video given its relative path (e.g. "./attachments/video.mp4").
         * Returns a data:image/jpeg;base64,... URL, or empty string on failure.
         * Results are cached to disk.
         */
        @JavascriptInterface
        fun getPoster(relativePath: String): String {
            return try {
                val postersDir = File(applicationContext.filesDir, "posters")
                if (!postersDir.exists()) postersDir.mkdirs()

                val pathHash = md5Hash(relativePath)
                val cacheFile = File(postersDir, "$pathHash.jpg")

                // Check disk cache
                if (cacheFile.exists() && cacheFile.length() > 0) {
                    val bytes = cacheFile.readBytes()
                    val b64 = Base64.encodeToString(bytes, Base64.NO_WRAP)
                    Log.d(TAG, "Poster cache hit: $relativePath")
                    return "data:image/jpeg;base64,$b64"
                }

                // Resolve the relative path to a content URI
                val uri = resolveRelativePath(relativePath) ?: run {
                    Log.w(TAG, "Poster: could not resolve path: $relativePath")
                    return ""
                }

                // Extract frame using MediaMetadataRetriever
                val retriever = MediaMetadataRetriever()
                try {
                    contentResolver.openFileDescriptor(uri, "r")?.use { pfd ->
                        retriever.setDataSource(pfd.fileDescriptor)
                    } ?: run {
                        Log.w(TAG, "Poster: could not open file: $relativePath")
                        return ""
                    }

                    // Get frame at 500ms (or first available frame)
                    val bitmap = retriever.getFrameAtTime(
                        500_000L, // 500ms in microseconds
                        MediaMetadataRetriever.OPTION_CLOSEST_SYNC
                    ) ?: retriever.getFrameAtTime(0L) ?: run {
                        Log.w(TAG, "Poster: no frame extracted: $relativePath")
                        return ""
                    }

                    // Compress to JPEG
                    val baos = java.io.ByteArrayOutputStream()
                    bitmap.compress(Bitmap.CompressFormat.JPEG, 80, baos)
                    bitmap.recycle()
                    val jpegBytes = baos.toByteArray()

                    // Cache to disk
                    cacheFile.writeBytes(jpegBytes)

                    val b64 = Base64.encodeToString(jpegBytes, Base64.NO_WRAP)
                    Log.d(TAG, "Poster extracted: $relativePath (${jpegBytes.size} bytes)")
                    "data:image/jpeg;base64,$b64"
                } finally {
                    retriever.release()
                }
            } catch (e: Exception) {
                Log.e(TAG, "Poster extraction failed: $relativePath: ${e.message}")
                ""
            }
        }

        /**
         * Generate thumbnail sprite + VTT for a video.
         * Returns VTT text with embedded sprite data URL, or empty string on failure.
         * Results are cached to disk.
         */
        @JavascriptInterface
        fun getThumbnails(relativePath: String): String {
            return try {
                val thumbsDir = File(applicationContext.filesDir, "posters")
                if (!thumbsDir.exists()) thumbsDir.mkdirs()

                val pathHash = md5Hash(relativePath)
                val cacheFile = File(thumbsDir, "${pathHash}_vtt.txt")

                // Check disk cache
                if (cacheFile.exists() && cacheFile.length() > 0) {
                    Log.d(TAG, "Thumbnail VTT cache hit: $relativePath")
                    return cacheFile.readText()
                }

                val uri = resolveRelativePath(relativePath) ?: run {
                    Log.w(TAG, "Thumbnails: could not resolve path: $relativePath")
                    return ""
                }

                val retriever = MediaMetadataRetriever()
                try {
                    contentResolver.openFileDescriptor(uri, "r")?.use { pfd ->
                        retriever.setDataSource(pfd.fileDescriptor)
                    } ?: return ""

                    val durationStr = retriever.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION) ?: return ""
                    val durationMs = durationStr.toLongOrNull() ?: return ""
                    val durationSec = durationMs / 1000.0
                    if (durationSec < 2.0) return ""

                    val thumbWidth = 160
                    val thumbHeight = 90
                    val maxThumbs = 60
                    val interval = maxOf(5.0, durationSec / maxThumbs)

                    // Calculate timestamps
                    val timestamps = mutableListOf<Double>()
                    var t = 0.0
                    while (t < durationSec) {
                        timestamps.add(t)
                        t += interval
                    }
                    if (timestamps.isEmpty()) return ""

                    Log.d(TAG, "Generating ${timestamps.size} thumbnails for ${durationSec.toInt()}s video: $relativePath")

                    // Extract frames and create sprite sheet
                    val spriteWidth = thumbWidth * timestamps.size
                    val sprite = Bitmap.createBitmap(spriteWidth, thumbHeight, Bitmap.Config.RGB_565)
                    val canvas = android.graphics.Canvas(sprite)

                    for ((i, ts) in timestamps.withIndex()) {
                        val frame = retriever.getFrameAtTime(
                            (ts * 1_000_000).toLong(),
                            MediaMetadataRetriever.OPTION_CLOSEST_SYNC
                        )
                        if (frame != null) {
                            val scaled = Bitmap.createScaledBitmap(frame, thumbWidth, thumbHeight, true)
                            canvas.drawBitmap(scaled, (i * thumbWidth).toFloat(), 0f, null)
                            if (scaled !== frame) scaled.recycle()
                            frame.recycle()
                        }
                    }

                    // Encode sprite as JPEG
                    val baos = java.io.ByteArrayOutputStream()
                    sprite.compress(Bitmap.CompressFormat.JPEG, 70, baos)
                    sprite.recycle()
                    val spriteB64 = Base64.encodeToString(baos.toByteArray(), Base64.NO_WRAP)
                    val spriteUrl = "data:image/jpeg;base64,$spriteB64"

                    // Generate VTT
                    val vtt = StringBuilder("WEBVTT\n\n")
                    for (i in timestamps.indices) {
                        val startTime = timestamps[i]
                        val endTime = if (i + 1 < timestamps.size) timestamps[i + 1] else durationSec
                        vtt.append(formatVttTime(startTime))
                        vtt.append(" --> ")
                        vtt.append(formatVttTime(endTime))
                        vtt.append("\n")
                        vtt.append(spriteUrl)
                        vtt.append("#xywh=${i * thumbWidth},0,$thumbWidth,$thumbHeight\n\n")
                    }

                    val vttText = vtt.toString()
                    cacheFile.writeText(vttText)
                    Log.d(TAG, "Thumbnails generated: $relativePath (${timestamps.size} frames)")
                    vttText
                } finally {
                    retriever.release()
                }
            } catch (e: Exception) {
                Log.e(TAG, "Thumbnail generation failed: $relativePath: ${e.message}")
                ""
            }
        }

        private fun formatVttTime(seconds: Double): String {
            val h = (seconds / 3600).toInt()
            val m = ((seconds % 3600) / 60).toInt()
            val s = (seconds % 60).toInt()
            val ms = ((seconds % 1) * 1000).toInt()
            return "%02d:%02d:%02d.%03d".format(h, m, s, ms)
        }

        private fun resolveRelativePath(relativePath: String): Uri? {
            val parentDoc = if (treeUri != null) {
                DocumentFile.fromTreeUri(this@WikiActivity, treeUri!!)
            } else if (wikiUri != null) {
                DocumentFile.fromSingleUri(this@WikiActivity, wikiUri!!)?.parentFile
            } else {
                null
            } ?: return null

            val pathParts = relativePath.split("/")
            var currentDoc: DocumentFile? = parentDoc
            for (part in pathParts) {
                if (part.isEmpty() || part == ".") continue
                if (part == "..") {
                    currentDoc = currentDoc?.parentFile
                } else {
                    currentDoc = currentDoc?.findFile(part)
                }
                if (currentDoc == null) return null
            }
            return if (currentDoc != null && currentDoc.exists()) currentDoc.uri else null
        }

        private fun md5Hash(input: String): String {
            val md = java.security.MessageDigest.getInstance("MD5")
            val digest = md.digest(input.toByteArray())
            return digest.joinToString("") { "%02x".format(it) }
        }
    }

    /**
     * JavaScript interface for external attachments.
     * Files stay where they are - we just reference them.
     */
    inner class AttachmentInterface {
        /**
         * Get the content:// URI for a file that was just picked.
         * Called from JavaScript import hook to get the URI to store in _canonical_uri.
         */
        @JavascriptInterface
        fun getFileUri(filename: String): String {
            Log.d(TAG, "getFileUri called: $filename")

            val uri = pendingFileUris[filename]
            return if (uri != null) {
                // Remove from pending after retrieval
                pendingFileUris.remove(filename)
                Log.d(TAG, "Found URI for $filename: $uri")
                "{\"success\":true,\"uri\":\"$uri\"}"
            } else {
                Log.w(TAG, "No URI found for: $filename")
                "{\"success\":false,\"error\":\"No URI found for file\"}"
            }
        }

        /**
         * Check if a content:// URI is accessible (has permission).
         * Returns JSON with success status.
         */
        @JavascriptInterface
        fun checkUriPermission(uriString: String): String {
            Log.d(TAG, "checkUriPermission called: $uriString")
            return try {
                val uri = Uri.parse(uriString)
                // Try to get the file name - this will fail if we don't have permission
                contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                    if (cursor.moveToFirst()) {
                        "{\"accessible\":true}"
                    } else {
                        "{\"accessible\":false,\"error\":\"Empty cursor\"}"
                    }
                } ?: "{\"accessible\":false,\"error\":\"Query returned null\"}"
            } catch (e: SecurityException) {
                Log.d(TAG, "No permission for URI: $uriString")
                "{\"accessible\":false,\"error\":\"No permission\"}"
            } catch (e: Exception) {
                Log.e(TAG, "Error checking URI: ${e.message}")
                "{\"accessible\":false,\"error\":\"${e.message?.replace("\"", "\\\"") ?: "Unknown error"}\"}"
            }
        }

        /**
         * Get metadata about a file (filename, size, type).
         * Used when importing to store alongside _canonical_uri.
         */
        @JavascriptInterface
        fun getFileMetadata(uriString: String): String {
            Log.d(TAG, "getFileMetadata called: $uriString")
            return try {
                val uri = Uri.parse(uriString)
                var filename: String? = null
                var size: Long = -1

                contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                    if (cursor.moveToFirst()) {
                        val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                        val sizeIndex = cursor.getColumnIndex(android.provider.OpenableColumns.SIZE)
                        if (nameIndex >= 0) filename = cursor.getString(nameIndex)
                        if (sizeIndex >= 0) size = cursor.getLong(sizeIndex)
                    }
                }

                val mimeType = contentResolver.getType(uri)

                val escapedFilename = filename?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: ""
                val escapedMimeType = mimeType?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: ""

                "{\"success\":true,\"filename\":\"$escapedFilename\",\"size\":$size,\"mimeType\":\"$escapedMimeType\"}"
            } catch (e: Exception) {
                Log.e(TAG, "Error getting file metadata: ${e.message}")
                "{\"success\":false,\"error\":\"${e.message?.replace("\"", "\\\"") ?: "Unknown error"}\"}"
            }
        }

        /**
         * Copy a file from its content:// URI to the wiki's attachments folder.
         * Returns JSON with the relative path for _canonical_uri.
         * This avoids SAF permission issues by keeping attachments local.
         */
        @JavascriptInterface
        fun copyToAttachments(sourceUriString: String, suggestedFilename: String, mimeType: String): String {
            Log.d(TAG, "copyToAttachments called: uri=$sourceUriString, filename=$suggestedFilename, type=$mimeType")

            return try {
                val sourceUri = Uri.parse(sourceUriString)

                // Check if we have tree access for creating attachments directory
                if (treeUri == null) {
                    Log.e(TAG, "No tree URI available - cannot create attachments directory")
                    return "{\"success\":false,\"error\":\"No folder access. Please re-open this wiki via the folder picker to enable attachments.\"}"
                }

                val parentDoc = DocumentFile.fromTreeUri(this@WikiActivity, treeUri!!)
                if (parentDoc == null) {
                    Log.e(TAG, "Cannot access tree URI")
                    return "{\"success\":false,\"error\":\"Cannot access wiki folder\"}"
                }

                // Find or create attachments directory
                var attachmentsDir = parentDoc.findFile("attachments")
                if (attachmentsDir == null) {
                    attachmentsDir = parentDoc.createDirectory("attachments")
                    if (attachmentsDir == null) {
                        Log.e(TAG, "Failed to create attachments directory")
                        return "{\"success\":false,\"error\":\"Failed to create attachments folder\"}"
                    }
                    Log.d(TAG, "Created attachments directory")
                }

                // Sanitize filename
                val safeName = suggestedFilename.replace("/", "_").replace("\\", "_")

                // Helper to get file size from URI
                fun getFileSize(uri: Uri): Long {
                    return try {
                        contentResolver.openAssetFileDescriptor(uri, "r")?.use { it.length } ?: -1L
                    } catch (e: Exception) { -1L }
                }

                // Helper to check if source and existing file are likely the same
                // by comparing file sizes (fast, avoids reading entire file contents).
                // If sizes can't be determined (both -1), assume match when the file
                // already exists with the same name — avoids copying a file onto itself.
                fun filesMatchBySize(uri1: Uri, uri2: Uri): Boolean {
                    val size1 = getFileSize(uri1)
                    val size2 = getFileSize(uri2)
                    if (size1 > 0 && size1 == size2) return true
                    if (size1 < 0 && size2 < 0) return true  // Both unknown, assume same
                    return false
                }

                // Check for existing file and generate unique name (or reuse if same size)
                var finalName = safeName
                var existingFile = attachmentsDir.findFile(safeName)
                if (existingFile != null) {
                    // If file exists with same name and same size, reuse it
                    if (filesMatchBySize(sourceUri, existingFile.uri)) {
                        val relativePath = "./attachments/$safeName"
                        Log.d(TAG, "Attachment already exists with matching size: $relativePath")
                        val escapedPath = relativePath.replace("\\", "\\\\").replace("\"", "\\\"")
                        return "{\"success\":true,\"path\":\"$escapedPath\",\"reused\":true}"
                    }

                    // Different size, find unique name
                    val baseName = safeName.substringBeforeLast(".")
                    val ext = safeName.substringAfterLast(".", "")
                    var counter = 1
                    do {
                        finalName = if (ext.isNotEmpty()) "${baseName}-$counter.$ext" else "${baseName}-$counter"
                        existingFile = attachmentsDir.findFile(finalName)
                        if (existingFile != null && filesMatchBySize(sourceUri, existingFile.uri)) {
                            val relativePath = "./attachments/$finalName"
                            Log.d(TAG, "Attachment already exists with matching size: $relativePath")
                            val escapedPath = relativePath.replace("\\", "\\\\").replace("\"", "\\\"")
                            return "{\"success\":true,\"path\":\"$escapedPath\",\"reused\":true}"
                        }
                        counter++
                    } while (existingFile != null && counter < 1000)
                }

                // Create the target file
                val targetFile = attachmentsDir.createFile(mimeType.ifEmpty { "application/octet-stream" }, finalName)
                if (targetFile == null) {
                    Log.e(TAG, "Failed to create attachment file")
                    return "{\"success\":false,\"error\":\"Failed to create attachment file\"}"
                }

                val relativePath = "./attachments/$finalName"

                // Register pending copy so shouldInterceptRequest serves from source URI
                // while the background copy is in progress
                pendingFileCopies[relativePath] = sourceUri
                addPendingCopyRecord(relativePath)

                // Copy content in background thread — returns immediately so wiki can save
                val targetUri = targetFile.uri
                Thread {
                    try {
                        contentResolver.openInputStream(sourceUri)?.use { input ->
                            contentResolver.openOutputStream(targetUri)?.use { output ->
                                input.copyTo(output)
                            }
                        } ?: throw IOException("Failed to open streams for copy")
                        Log.d(TAG, "Background attachment copy complete: $relativePath")
                    } catch (e: Exception) {
                        Log.e(TAG, "Background attachment copy failed: ${e.message}", e)
                        // Delete the partial/empty target file
                        try { targetFile.delete() } catch (_: Exception) {}
                    } finally {
                        pendingFileCopies.remove(relativePath)
                        removePendingCopyRecord(relativePath)
                    }
                }.start()

                Log.d(TAG, "Attachment copy started in background: $relativePath")
                val escapedPath = relativePath.replace("\\", "\\\\").replace("\"", "\\\"")
                "{\"success\":true,\"path\":\"$escapedPath\"}"
            } catch (e: Exception) {
                Log.e(TAG, "Error copying attachment: ${e.message}", e)
                val escapedError = e.message?.replace("\\", "\\\\")?.replace("\"", "\\\"") ?: "Unknown error"
                "{\"success\":false,\"error\":\"$escapedError\"}"
            }
        }

        /**
         * Check if we have folder access for creating attachments.
         */
        @JavascriptInterface
        fun hasFolderAccess(): Boolean {
            return treeUri != null
        }

        /**
         * Request folder access for saving attachments.
         * Stores the pending attachment info and launches folder picker.
         * The result will be sent via __pendingAttachmentCallback.
         */
        @JavascriptInterface
        fun requestFolderAccessForAttachment(sourceUri: String, filename: String, mimeType: String) {
            Log.d(TAG, "requestFolderAccessForAttachment: uri=$sourceUri, filename=$filename")
            pendingAttachmentCopy = Triple(sourceUri, filename, mimeType)

            runOnUiThread {
                try {
                    val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
                        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                        addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
                        addFlags(Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION)
                    }
                    attachmentFolderLauncher.launch(intent)
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to launch folder picker: ${e.message}")
                    pendingAttachmentCopy = null
                    runOnUiThread {
                        webView.evaluateJavascript("""
                            if (window.__pendingAttachmentCallback) {
                                window.__pendingAttachmentCallback({"success":false,"error":"Failed to open folder picker: ${e.message?.replace("\"", "\\\"")}"});
                                delete window.__pendingAttachmentCallback;
                            }
                        """.trimIndent(), null)
                    }
                }
            }
        }
    }

    /**
     * JavaScript interface for exporting/saving files.
     * Opens a SAF "create document" dialog to let user choose save location.
     */
    inner class ExportInterface {
        /**
         * Save a file with user-chosen location.
         * @param filename Suggested filename for the save dialog
         * @param mimeType MIME type of the content
         * @param base64Content Base64-encoded file content
         * @param callbackId Optional callback ID for async result
         */
        @JavascriptInterface
        fun saveFile(filename: String, mimeType: String, base64Content: String, callbackId: String?) {
            Log.d(TAG, "saveFile called: filename=$filename, mimeType=$mimeType, contentLength=${base64Content.length}, callback=$callbackId")

            try {
                // Decode the base64 content
                val content = Base64.decode(base64Content, Base64.DEFAULT)
                pendingExportContent = content
                pendingExportCallback = callbackId

                // Launch the create document intent on the UI thread
                runOnUiThread {
                    exitFullscreenIfNeeded()
                    try {
                        val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
                            addCategory(Intent.CATEGORY_OPENABLE)
                            type = mimeType
                            putExtra(Intent.EXTRA_TITLE, filename)
                        }
                        createDocumentLauncher.launch(intent)
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to launch save dialog: ${e.message}")
                        notifyExportResult(false, "Failed to open save dialog: ${e.message}")
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to decode export content: ${e.message}")
                notifyExportResult(false, "Failed to decode content: ${e.message}")
            }
        }

        /**
         * Save text content (convenience method - no base64 encoding needed).
         */
        @JavascriptInterface
        fun saveTextFile(filename: String, mimeType: String, textContent: String, callbackId: String?) {
            Log.d(TAG, "saveTextFile called: filename=$filename, mimeType=$mimeType, contentLength=${textContent.length}")

            pendingExportContent = textContent.toByteArray(Charsets.UTF_8)
            pendingExportCallback = callbackId

            runOnUiThread {
                exitFullscreenIfNeeded()
                try {
                    val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
                        addCategory(Intent.CATEGORY_OPENABLE)
                        type = mimeType
                        putExtra(Intent.EXTRA_TITLE, filename)
                    }
                    createDocumentLauncher.launch(intent)
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to launch save dialog: ${e.message}")
                    notifyExportResult(false, "Failed to open save dialog: ${e.message}")
                }
            }
        }
    }

    /**
     * Notify JavaScript of export result.
     */
    private fun notifyExportResult(success: Boolean, message: String?) {
        val callbackId = pendingExportCallback
        pendingExportCallback = null
        pendingExportContent = null

        if (callbackId != null) {
            val escapedMessage = message?.replace("\\", "\\\\")?.replace("'", "\\'")?.replace("\n", "\\n") ?: ""
            val script = """
                (function() {
                    if (window.__exportCallbacks && window.__exportCallbacks['$callbackId']) {
                        window.__exportCallbacks['$callbackId']($success, '$escapedMessage');
                        delete window.__exportCallbacks['$callbackId'];
                    }
                })();
            """.trimIndent()
            runOnUiThread {
                webView.evaluateJavascript(script, null)
            }
        }
    }

    // Map of filename -> content:// URI for files picked via file chooser
    private val pendingFileUris = mutableMapOf<String, String>()

    // Wiki document URI (for single-file wikis: the wiki file itself)
    private var wikiUri: Uri? = null

    // Tree URI for folder access
    private var treeUri: Uri? = null

    // Pending attachment copy operation (waiting for folder access)
    private var pendingAttachmentCopy: Triple<String, String, String>? = null  // sourceUri, filename, mimeType

    // Map of relativePath -> sourceUri for files being copied in background.
    // shouldInterceptRequest serves from the source URI while the copy is in progress.
    private val pendingFileCopies = ConcurrentHashMap<String, Uri>()

    // Launcher for requesting folder access for attachments
    private lateinit var attachmentFolderLauncher: ActivityResultLauncher<Intent>

    /**
     * Get the display name (filename) from a content:// URI.
     */
    private fun getFileName(uri: Uri): String? {
        var name: String? = null
        if (uri.scheme == "content") {
            contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                    if (nameIndex >= 0) {
                        name = cursor.getString(nameIndex)
                    }
                }
            }
        }
        if (name == null) {
            name = uri.lastPathSegment
        }
        return name
    }

    /**
     * Update the status bar and navigation bar colors.
     * Icon colors are determined by the background luminance to ensure contrast.
     */
    @Suppress("DEPRECATION")
    private fun updateSystemBarColors(statusBarColorHex: String, navBarColorHex: String) {
        try {
            val statusColor = parseCssColor(statusBarColorHex)
            val navColor = parseCssColor(navBarColorHex)

            // Android 15+: system bars are transparent, color them via background views
            if (Build.VERSION.SDK_INT >= 35) {
                statusBarBgView?.setBackgroundColor(statusColor)
                navBarBgView?.setBackgroundColor(navColor)
            } else {
                window.statusBarColor = statusColor
                window.navigationBarColor = navColor
            }

            // Determine icon colors based on BACKGROUND luminance to ensure contrast
            // Light background (luminance > 0.5) = dark icons for visibility
            // Dark background (luminance <= 0.5) = light icons for visibility
            val statusBarLuminance = calculateLuminance(statusColor)
            val navBarLuminance = calculateLuminance(navColor)

            // Use dark icons on status bar if background is light
            val useDarkStatusIcons = statusBarLuminance > 0.5
            // Use dark icons on nav bar if background is light
            val useDarkNavIcons = navBarLuminance > 0.5

            Log.d(TAG, "Status bar luminance: $statusBarLuminance, dark icons: $useDarkStatusIcons")
            Log.d(TAG, "Nav bar luminance: $navBarLuminance, dark icons: $useDarkNavIcons")

            // Update icon colors - dark icons on light background, light icons on dark background
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                val insetsController = window.insetsController
                if (insetsController != null) {
                    // APPEARANCE_LIGHT_STATUS_BARS means dark icons (for light backgrounds)
                    var appearance = 0
                    if (useDarkStatusIcons) {
                        appearance = appearance or WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                    }
                    if (useDarkNavIcons) {
                        appearance = appearance or WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
                    }
                    insetsController.setSystemBarsAppearance(
                        appearance,
                        WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS or WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
                    )
                }
            } else {
                @Suppress("DEPRECATION")
                var newFlags = window.decorView.systemUiVisibility

                newFlags = if (useDarkStatusIcons) {
                    newFlags or View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                } else {
                    newFlags and View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR.inv()
                }

                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    newFlags = if (useDarkNavIcons) {
                        newFlags or View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR
                    } else {
                        newFlags and View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR.inv()
                    }
                }

                @Suppress("DEPRECATION")
                window.decorView.systemUiVisibility = newFlags
            }

            Log.d(TAG, "System bar colors updated successfully")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to update system bar colors: ${e.message}")
        }
    }

    /**
     * Parse a CSS color string (hex, rgb(), rgba()) into an Android Color int.
     * Falls back to Color.parseColor() for hex and named colors.
     */
    private fun parseCssColor(css: String): Int {
        val trimmed = css.trim()
        val rgbaMatch = Regex("""^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*(?:,\s*([\d.]+)\s*)?\)$""").matchEntire(trimmed)
        if (rgbaMatch != null) {
            val r = rgbaMatch.groupValues[1].toInt().coerceIn(0, 255)
            val g = rgbaMatch.groupValues[2].toInt().coerceIn(0, 255)
            val b = rgbaMatch.groupValues[3].toInt().coerceIn(0, 255)
            val a = if (rgbaMatch.groupValues[4].isNotEmpty()) {
                (rgbaMatch.groupValues[4].toFloat().coerceIn(0f, 1f) * 255).toInt()
            } else {
                255
            }
            return Color.argb(a, r, g, b)
        }
        return Color.parseColor(trimmed)
    }

    /**
     * Calculate relative luminance of a color (0.0 = dark, 1.0 = light).
     */
    private fun calculateLuminance(color: Int): Double {
        val r = Color.red(color) / 255.0
        val g = Color.green(color) / 255.0
        val b = Color.blue(color) / 255.0

        val rLinear = if (r <= 0.03928) r / 12.92 else Math.pow((r + 0.055) / 1.055, 2.4)
        val gLinear = if (g <= 0.03928) g / 12.92 else Math.pow((g + 0.055) / 1.055, 2.4)
        val bLinear = if (b <= 0.03928) b / 12.92 else Math.pow((b + 0.055) / 1.055, 2.4)

        return 0.2126 * rLinear + 0.7152 * gLinear + 0.0722 * bLinear
    }

    // ========== Pending file copy persistence ==========
    // Tracks background attachment copies so partial files can be cleaned up if the app is killed.

    private fun getPendingCopiesFile(): File {
        val hash = wikiPath?.hashCode()?.toLong()?.and(0xFFFFFFFFL)?.let { String.format("%08x", it) } ?: "default"
        return File(filesDir, "pending_copies_$hash.json")
    }

    private fun addPendingCopyRecord(relativePath: String) {
        try {
            val file = getPendingCopiesFile()
            val paths = loadPendingCopyRecords().toMutableSet()
            paths.add(relativePath)
            file.writeText(JSONArray(paths.toList()).toString())
        } catch (e: Exception) {
            Log.e(TAG, "Failed to save pending copy record: ${e.message}")
        }
    }

    private fun removePendingCopyRecord(relativePath: String) {
        try {
            val file = getPendingCopiesFile()
            val paths = loadPendingCopyRecords().toMutableSet()
            paths.remove(relativePath)
            if (paths.isEmpty()) {
                file.delete()
            } else {
                file.writeText(JSONArray(paths.toList()).toString())
            }
        } catch (e: Exception) {
            Log.e(TAG, "Failed to remove pending copy record: ${e.message}")
        }
    }

    private fun loadPendingCopyRecords(): Set<String> {
        return try {
            val file = getPendingCopiesFile()
            if (file.exists()) {
                val arr = JSONArray(file.readText())
                (0 until arr.length()).map { arr.getString(it) }.toSet()
            } else {
                emptySet()
            }
        } catch (e: Exception) {
            Log.e(TAG, "Failed to load pending copy records: ${e.message}")
            emptySet()
        }
    }

    /**
     * Clean up partial attachment files left by background copies that were interrupted
     * (e.g., app killed by OOM, crash, or force-stop during copy).
     * Runs on a background thread since DocumentFile operations involve SAF I/O.
     */
    private fun cleanupPartialCopies() {
        val staleEntries = loadPendingCopyRecords()
        if (staleEntries.isEmpty()) return

        Log.d(TAG, "Found ${staleEntries.size} stale pending copies, cleaning up on background thread")

        Thread {
            val parentDoc = if (treeUri != null) DocumentFile.fromTreeUri(this, treeUri!!) else null
            if (parentDoc == null) {
                Log.w(TAG, "No tree URI for cleanup - clearing stale records")
                getPendingCopiesFile().delete()
                return@Thread
            }

            for (relativePath in staleEntries) {
                try {
                    val pathParts = relativePath.removePrefix("./").split("/")
                    var doc: DocumentFile? = parentDoc
                    for (part in pathParts) {
                        if (part.isEmpty() || part == ".") continue
                        doc = doc?.findFile(part)
                        if (doc == null) break
                    }
                    if (doc != null && doc.exists()) {
                        doc.delete()
                        Log.d(TAG, "Deleted partial copy: $relativePath")
                    }
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to delete partial copy $relativePath: ${e.message}")
                }
            }

            getPendingCopiesFile().delete()
            Log.d(TAG, "Partial copy cleanup complete")
        }.start()
    }

    /**
     * Exit immersive fullscreen if currently active.
     * Call before launching external UI (file chooser, print, export).
     */
    private fun exitFullscreenIfNeeded() {
        if (isImmersiveFullscreen) {
            isImmersiveFullscreen = false
            exitImmersiveMode()
        }
    }

    /**
     * "Fullscreen" — hide the status bar and extend content into its space.
     * The navigation bar is kept visible to avoid consuming drag gestures.
     *
     * Previously, Android 14's InsetsController was hiding both bars even when
     * only statusBars() was requested. The root cause was TiddlyWiki's built-in
     * tm-full-screen handler (rootwidget.js) calling requestFullscreen() — the
     * Browser Fullscreen API — which triggers WebView's native immersive mode.
     * With requestFullscreen() now blocked in the injected JS, hiding only the
     * status bar via WindowInsetsControllerCompat works correctly.
     */
    private var savedStatusBarColor: Int = 0
    private var savedCutoutMode: Int = 0

    // =========================================================================
    // Pre-Android 15 (API < 35): Original fullscreen implementation.
    // System manages insets via setDecorFitsSystemWindows. We only add an
    // insets listener during fullscreen to pad for the nav bar.
    // =========================================================================

    @Suppress("DEPRECATION")
    private fun enterImmersiveModeLegacy() {
        Log.d(TAG, "enterImmersiveModeLegacy — hiding status bar, keeping nav bar")
        WindowCompat.setDecorFitsSystemWindows(window, false)
        val insetsController = WindowInsetsControllerCompat(window, window.decorView)
        insetsController.hide(WindowInsetsCompat.Type.statusBars())
        insetsController.systemBarsBehavior =
            WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            val attrs = window.attributes
            savedCutoutMode = attrs.layoutInDisplayCutoutMode
            attrs.layoutInDisplayCutoutMode =
                android.view.WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
            window.attributes = attrs
        }
        ViewCompat.setOnApplyWindowInsetsListener(rootLayout) { view, insets ->
            val navBars = insets.getInsets(WindowInsetsCompat.Type.navigationBars())
            view.setPadding(0, 0, 0, navBars.bottom)
            WindowInsetsCompat.CONSUMED
        }
        rootLayout.requestApplyInsets()
    }

    @Suppress("DEPRECATION")
    private fun exitImmersiveModeLegacy() {
        Log.d(TAG, "exitImmersiveModeLegacy — restoring status bar")
        val insetsController = WindowInsetsControllerCompat(window, window.decorView)
        insetsController.show(WindowInsetsCompat.Type.statusBars())
        WindowCompat.setDecorFitsSystemWindows(window, true)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            val attrs = window.attributes
            attrs.layoutInDisplayCutoutMode = savedCutoutMode
            window.attributes = attrs
        }
        ViewCompat.setOnApplyWindowInsetsListener(rootLayout, null)
        rootLayout.setPadding(0, 0, 0, 0)
    }

    // =========================================================================
    // Android 15+ (API 35+): Edge-to-edge is enforced. We use a persistent
    // insets listener and the native insetsController for status bar toggle.
    // Cutout mode ALWAYS is set only during fullscreen to fill the notch area.
    // =========================================================================

    private fun enterImmersiveModeModern() {
        Log.d(TAG, "enterImmersiveModeModern — hiding status bar, keeping nav bar")
        val attrs = window.attributes
        savedCutoutMode = attrs.layoutInDisplayCutoutMode
        attrs.layoutInDisplayCutoutMode =
            android.view.WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        window.attributes = attrs
        window.insetsController?.hide(WindowInsets.Type.statusBars())
        rootLayout.requestApplyInsets()
    }

    private fun exitImmersiveModeModern() {
        Log.d(TAG, "exitImmersiveModeModern — restoring status bar")
        window.insetsController?.show(WindowInsets.Type.statusBars())
        val attrs = window.attributes
        attrs.layoutInDisplayCutoutMode = savedCutoutMode
        window.attributes = attrs
        rootLayout.requestApplyInsets()
    }

    private fun enterImmersiveMode() {
        if (Build.VERSION.SDK_INT >= 35) enterImmersiveModeModern()
        else enterImmersiveModeLegacy()
    }

    private fun exitImmersiveMode() {
        if (Build.VERSION.SDK_INT >= 35) exitImmersiveModeModern()
        else exitImmersiveModeLegacy()
    }

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Register the file chooser launcher for import functionality
        fileChooserLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            val uris = if (result.resultCode == RESULT_OK && result.data != null) {
                val data = result.data!!
                // Handle multiple file selection
                val clipData = data.clipData
                if (clipData != null) {
                    Array(clipData.itemCount) { i -> clipData.getItemAt(i).uri }
                } else {
                    // Single file
                    data.data?.let { arrayOf(it) } ?: emptyArray()
                }
            } else {
                emptyArray()
            }
            Log.d(TAG, "File chooser result: ${uris.size} files selected")

            // Store URI -> filename mapping and take persistent permissions
            for (uri in uris) {
                try {
                    // Take persistent permission so we can access the file later
                    contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    Log.d(TAG, "Took persistent permission for: $uri")
                } catch (e: Exception) {
                    Log.w(TAG, "Could not take persistent permission: ${e.message}")
                }

                // Get the display name and store the mapping
                val filename = getFileName(uri)
                if (filename != null) {
                    pendingFileUris[filename] = uri.toString()
                    Log.d(TAG, "Stored URI mapping: $filename -> $uri")
                }
            }

            filePathCallback?.onReceiveValue(uris)
            filePathCallback = null
        }

        // Register the create document launcher for export/save functionality
        createDocumentLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            if (result.resultCode == RESULT_OK && result.data?.data != null) {
                val uri = result.data!!.data!!
                Log.d(TAG, "Save location selected: $uri")

                val content = pendingExportContent
                if (content != null) {
                    try {
                        contentResolver.openOutputStream(uri)?.use { outputStream ->
                            outputStream.write(content)
                        }
                        Log.d(TAG, "File saved successfully: ${content.size} bytes")
                        notifyExportResult(true, "File saved successfully")
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to save file: ${e.message}")
                        notifyExportResult(false, "Failed to save file: ${e.message}")
                    }
                } else {
                    Log.e(TAG, "No pending export content")
                    notifyExportResult(false, "No content to save")
                }
            } else {
                Log.d(TAG, "Save cancelled by user")
                notifyExportResult(false, "Save cancelled")
            }
            pendingExportContent = null
        }

        // Register the folder picker for granting attachment folder access
        attachmentFolderLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            if (result.resultCode == RESULT_OK && result.data?.data != null) {
                val newTreeUri = result.data!!.data!!
                Log.d(TAG, "Attachment folder selected: $newTreeUri")

                try {
                    // Take persistent permission for the tree (read + write)
                    contentResolver.takePersistableUriPermission(
                        newTreeUri,
                        Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    )
                    Log.d(TAG, "Took persistent permission for attachment folder: $newTreeUri")

                    // Update the tree URI
                    treeUri = newTreeUri

                    // Retry pending attachment copy if any
                    pendingAttachmentCopy?.let { (sourceUri, filename, mimeType) ->
                        pendingAttachmentCopy = null
                        val copyResult = AttachmentInterface().copyToAttachments(sourceUri, filename, mimeType)
                        Log.d(TAG, "Retry attachment copy result: $copyResult")
                        // Notify JavaScript of the result
                        runOnUiThread {
                            webView.evaluateJavascript("""
                                if (window.__pendingAttachmentCallback) {
                                    window.__pendingAttachmentCallback($copyResult);
                                    delete window.__pendingAttachmentCallback;
                                }
                            """.trimIndent(), null)
                        }
                    }
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to handle attachment folder selection: ${e.message}")
                    runOnUiThread {
                        webView.evaluateJavascript("""
                            if (window.__pendingAttachmentCallback) {
                                window.__pendingAttachmentCallback({"success":false,"error":"Failed to access folder: ${e.message?.replace("\"", "\\\"")}"});
                                delete window.__pendingAttachmentCallback;
                            }
                        """.trimIndent(), null)
                    }
                }
            } else {
                Log.d(TAG, "Attachment folder selection cancelled")
                pendingAttachmentCopy = null
                runOnUiThread {
                    webView.evaluateJavascript("""
                        if (window.__pendingAttachmentCallback) {
                            window.__pendingAttachmentCallback({"success":false,"error":"Folder selection cancelled"});
                            delete window.__pendingAttachmentCallback;
                        }
                    """.trimIndent(), null)
                }
            }
        }

        // Set unique WebView data directory suffix per wiki for session isolation.
        // Each wiki gets its own cookies, localStorage, etc.
        // This also prevents "Using WebView from more than one process at once" error.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            try {
                val processName = android.app.Application.getProcessName()
                if (processName != null && processName.contains(":wiki")) {
                    // Use hash of wiki path for unique per-wiki session directory
                    val wikiPathForHash = intent.getStringExtra(EXTRA_WIKI_PATH) ?: "default"
                    val hash = wikiPathForHash.hashCode().toLong() and 0xFFFFFFFFL // Make positive
                    val suffix = "wiki_${String.format("%08x", hash)}"
                    WebView.setDataDirectorySuffix(suffix)
                    Log.d(TAG, "Set WebView data directory suffix to '$suffix' for wiki: $wikiPathForHash")
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to set WebView data directory suffix: ${e.message}")
            }
        }

        wikiPath = intent.getStringExtra(EXTRA_WIKI_PATH)
        wikiTitle = intent.getStringExtra(EXTRA_WIKI_TITLE) ?: "TiddlyWiki"
        isFolder = intent.getBooleanExtra(EXTRA_IS_FOLDER, false)
        val folderServerUrl = intent.getStringExtra(EXTRA_WIKI_URL)  // For folder wikis: Node.js server URL
        val backupsEnabled = intent.getBooleanExtra(EXTRA_BACKUPS_ENABLED, true)  // Default: enabled
        val backupCount = intent.getIntExtra(EXTRA_BACKUP_COUNT, 20)  // Default: 20 backups

        Log.d(TAG, "WikiActivity onCreate - path: $wikiPath, title: $wikiTitle, isFolder: $isFolder, folderUrl: $folderServerUrl, backupsEnabled: $backupsEnabled, backupCount: $backupCount")

        // Start foreground service from WikiActivity (runs in :wiki process, same as the service).
        // This is more reliable than the cross-process JNI call from the main process,
        // because SharedPreferences used for the active wiki count are not multi-process safe.
        try {
            WikiServerService.startService(applicationContext)
            Log.d(TAG, "Started foreground service from WikiActivity")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start foreground service: ${e.message}")
        }

        if (wikiPath.isNullOrEmpty()) {
            Log.e(TAG, "No wiki path provided!")
            finish()
            return
        }

        // Parse the wiki path to get URIs (needed for attachment saving)
        val (parsedWikiUri, parsedTreeUri) = try {
            parseWikiPath(wikiPath!!)
        } catch (e: Exception) {
            Log.e(TAG, "Failed to parse wiki path: ${e.message}")
            finish()
            return
        }
        wikiUri = parsedWikiUri
        treeUri = parsedTreeUri
        Log.d(TAG, "Parsed wiki path: wikiUri=$wikiUri, treeUri=$treeUri")

        // Clean up any partial attachment files from interrupted background copies
        cleanupPartialCopies()

        // Determine the wiki URL based on wiki type
        var wikiUrl: String
        val attachmentServerUrl: String

        if (isFolder) {
            // Folder wiki: use Node.js server URL provided by Rust
            if (folderServerUrl.isNullOrEmpty()) {
                Log.e(TAG, "No server URL provided for folder wiki!")
                finish()
                return
            }
            wikiUrl = folderServerUrl
            Log.d(TAG, "Folder wiki using Node.js server at: $wikiUrl")

            // Also start an HTTP server for serving attachments (Node.js server doesn't have /_relative/ endpoint)
            if (wikiUri != null && parsedTreeUri != null) {
                httpServer = WikiHttpServer(this, wikiUri!!, parsedTreeUri, true, null, false, 0)  // No backups for folder wikis
                attachmentServerUrl = try {
                    httpServer!!.start()                } catch (e: Exception) {
                    Log.e(TAG, "Failed to start attachment server: ${e.message}")
                    // Fall back to wiki URL (attachments won't display but wiki will work)
                    wikiUrl
                }
                Log.d(TAG, "Folder wiki attachment server at: $attachmentServerUrl")
            } else {
                attachmentServerUrl = wikiUrl
                Log.w(TAG, "No tree URI for folder wiki - attachments won't be served locally")
            }
        } else {
            // Single-file wiki: start local HTTP server in this process
            if (wikiUri == null) {
                Log.e(TAG, "No wiki URI in path!")
                finish()
                return
            }

            // Start local HTTP server in this process (independent of Tauri/landing page)
            httpServer = WikiHttpServer(this, wikiUri!!, parsedTreeUri, false, null, backupsEnabled, backupCount)
            wikiUrl = try {
                httpServer!!.start()            } catch (e: Exception) {
                Log.e(TAG, "Failed to start HTTP server: ${e.message}")
                finish()
                return
            }
            // For single-file wikis, the wiki server also serves attachments
            attachmentServerUrl = wikiUrl
            Log.d(TAG, "Single-file wiki using local server at: $wikiUrl")

            // Acquire WakeLock to keep server alive when app is in background
            acquireWakeLock()
        }

        // Handle tm-open-window: append tiddler title as URL fragment for navigation
        val tiddlerTitle = intent.getStringExtra(EXTRA_TIDDLER_TITLE)
        if (!tiddlerTitle.isNullOrEmpty()) {
            wikiUrl = "$wikiUrl#${URLEncoder.encode(tiddlerTitle, "UTF-8")}"
            isChildWindow = true  // Mark as child window so back button returns to parent
            Log.d(TAG, "Navigating to tiddler: $tiddlerTitle (child window)")
        }

        Log.d(TAG, "Wiki opened: path=$wikiPath, url=$wikiUrl, taskId=$taskId")

        // Update recent wikis list for home screen widget
        updateRecentWikis(applicationContext, wikiPath!!, wikiTitle, isFolder)

        // Set Recents task label (icon bitmap is ignored on API 28+)
        @Suppress("DEPRECATION")
        setTaskDescription(ActivityManager.TaskDescription(wikiTitle))

        // Create and configure WebView
        webView = WebView(this).apply {
            settings.apply {
                javaScriptEnabled = true
                domStorageEnabled = true
                @Suppress("DEPRECATION")
                databaseEnabled = true
                allowFileAccess = true
                allowContentAccess = true
                mixedContentMode = WebSettings.MIXED_CONTENT_ALWAYS_ALLOW
                // Enable modern web features
                setSupportZoom(true)
                builtInZoomControls = true
                displayZoomControls = false
                loadWithOverviewMode = true
                useWideViewPort = true
                @Suppress("DEPRECATION")
                mediaPlaybackRequiresUserGesture = false
            }

            // Custom WebChromeClient to handle file chooser and fullscreen video
            webChromeClient = object : WebChromeClient() {
                override fun onConsoleMessage(consoleMessage: android.webkit.ConsoleMessage?): Boolean {
                    consoleMessage?.let {
                        val level = when (it.messageLevel()) {
                            android.webkit.ConsoleMessage.MessageLevel.ERROR -> "E"
                            android.webkit.ConsoleMessage.MessageLevel.WARNING -> "W"
                            else -> "I"
                        }
                        Log.println(when(level) { "E" -> Log.ERROR; "W" -> Log.WARN; else -> Log.INFO },
                            "WebConsole", "${it.message()} [${it.sourceId()}:${it.lineNumber()}]")
                    }
                    return true
                }

                override fun onShowFileChooser(
                    webView: WebView?,
                    filePathCallback: ValueCallback<Array<Uri>>?,
                    fileChooserParams: FileChooserParams?
                ): Boolean {
                    // Exit fullscreen before showing file chooser
                    exitFullscreenIfNeeded()

                    // Cancel any pending callback
                    this@WikiActivity.filePathCallback?.onReceiveValue(null)
                    this@WikiActivity.filePathCallback = filePathCallback

                    val acceptTypes = fileChooserParams?.acceptTypes ?: arrayOf("*/*")
                    val mimeTypes = acceptTypes.filter { it.isNotEmpty() }.toTypedArray()
                    val allowMultiple = fileChooserParams?.mode == FileChooserParams.MODE_OPEN_MULTIPLE

                    Log.d(TAG, "onShowFileChooser: mimeTypes=${mimeTypes.joinToString()}, allowMultiple=$allowMultiple")

                    try {
                        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                            addCategory(Intent.CATEGORY_OPENABLE)
                            type = if (mimeTypes.isNotEmpty() && mimeTypes[0].isNotEmpty()) mimeTypes[0] else "*/*"
                            if (mimeTypes.size > 1) {
                                putExtra(Intent.EXTRA_MIME_TYPES, mimeTypes)
                            }
                            if (allowMultiple) {
                                putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
                            }
                        }
                        fileChooserLauncher.launch(intent)
                        return true
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to launch file chooser: ${e.message}")
                        this@WikiActivity.filePathCallback?.onReceiveValue(null)
                        this@WikiActivity.filePathCallback = null
                        return false
                    }
                }

                override fun onShowCustomView(view: View?, callback: CustomViewCallback?) {
                    if (fullscreenView != null) {
                        callback?.onCustomViewHidden()
                        return
                    }
                    fullscreenView = view
                    fullscreenCallback = callback
                    rootLayout.addView(view, FrameLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT
                    ))
                    webView.visibility = View.GONE
                    // Immersive fullscreen — hide both status bar and nav bar for video
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                        window.insetsController?.hide(android.view.WindowInsets.Type.systemBars())
                        window.insetsController?.systemBarsBehavior =
                            WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                    } else {
                        @Suppress("DEPRECATION")
                        window.decorView.systemUiVisibility = (
                            View.SYSTEM_UI_FLAG_FULLSCREEN
                            or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        )
                    }
                }

                override fun onHideCustomView() {
                    fullscreenView?.let { rootLayout.removeView(it) }
                    fullscreenView = null
                    fullscreenCallback?.onCustomViewHidden()
                    fullscreenCallback = null
                    webView.visibility = View.VISIBLE
                    // Restore system bars
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                        window.insetsController?.show(android.view.WindowInsets.Type.systemBars())
                    } else {
                        @Suppress("DEPRECATION")
                        window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_VISIBLE
                    }
                }
            }

            // Add JavaScript interface for palette color updates
            addJavascriptInterface(PaletteInterface(), "TiddlyDesktopAndroid")

            // Add JavaScript interface for saving attachments
            addJavascriptInterface(AttachmentInterface(), "TiddlyDesktopAttachments")

            // Add JavaScript interface for exporting/saving files
            addJavascriptInterface(ExportInterface(), "TiddlyDesktopExport")

            // Add JavaScript interface for server control (restart on disconnect)
            addJavascriptInterface(ServerInterface(), "TiddlyDesktopServer")

            // Add JavaScript interface for clipboard operations
            addJavascriptInterface(ClipboardInterface(), "TiddlyDesktopClipboard")

            // Add JavaScript interface for printing
            addJavascriptInterface(PrintInterface(), "TiddlyDesktopPrint")

            // Add JavaScript interface for opening URLs in external browser/apps
            addJavascriptInterface(ExternalWindowInterface(), "TiddlyDesktopExternal")

            // Add JavaScript interface for favicon extraction
            addJavascriptInterface(FaviconInterface(), "TiddlyDesktopFavicon")

            // Add JavaScript interface for video poster extraction
            addJavascriptInterface(PosterInterface(), "TiddlyDesktopPoster")
        }

        // Use FrameLayout wrapper for fullscreen video support
        rootLayout = FrameLayout(this)
        rootLayout.addView(webView)
        setContentView(rootLayout)

        // Android 15+ (API 35+): Edge-to-edge is enforced and setDecorFitsSystemWindows
        // is ignored. We must handle insets ourselves with a persistent listener.
        // Also add colored background views behind the transparent system bars for palette colors.
        // Pre-Android 15: System manages insets via setDecorFitsSystemWindows — no setup needed.
        if (Build.VERSION.SDK_INT >= 35) {
            // Disable system scrim so our background colors show through unmodified
            window.isStatusBarContrastEnforced = false
            window.isNavigationBarContrastEnforced = false

            // Create background views for system bar coloring (behind transparent bars)
            statusBarBgView = View(this).apply {
                setBackgroundColor(android.graphics.Color.TRANSPARENT)
            }
            navBarBgView = View(this).apply {
                setBackgroundColor(android.graphics.Color.TRANSPARENT)
            }
            // Add at index 0 so they're behind the webView
            rootLayout.addView(statusBarBgView, 0, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, 0, android.view.Gravity.TOP))
            rootLayout.addView(navBarBgView, 0, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, 0, android.view.Gravity.BOTTOM))

            window.decorView.setOnApplyWindowInsetsListener { _, insets ->
                // Use webView margins (not rootLayout padding) so bg views stay at screen edges.
                // rootLayout has no padding, so Gravity.TOP/BOTTOM bg views are at the true edges.
                if (isImmersiveFullscreen) {
                    val navBars = insets.getInsets(WindowInsets.Type.navigationBars())
                    val params = webView.layoutParams as FrameLayout.LayoutParams
                    params.setMargins(0, 0, 0, navBars.bottom)
                    webView.layoutParams = params
                    statusBarBgView?.layoutParams?.height = 0
                    navBarBgView?.layoutParams?.height = navBars.bottom
                } else {
                    val systemBars = insets.getInsets(WindowInsets.Type.systemBars())
                    val params = webView.layoutParams as FrameLayout.LayoutParams
                    params.setMargins(systemBars.left, systemBars.top, systemBars.right, systemBars.bottom)
                    webView.layoutParams = params
                    statusBarBgView?.layoutParams?.height = systemBars.top
                    navBarBgView?.layoutParams?.height = systemBars.bottom
                }
                statusBarBgView?.requestLayout()
                navBarBgView?.requestLayout()
                insets
            }
        }

        // Register back navigation handler for modern Android (gesture nav / API 33+)
        // onKeyDown(KEYCODE_BACK) is NOT called with gesture navigation on targetSdk >= 33,
        // so we use OnBackPressedCallback. Keep onKeyDown too for physical back buttons.
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                handleBackNavigation()
            }
        })

        // Inject JavaScript to handle external attachments and saving
        // This transforms _canonical_uri paths to use the server's /_file/ endpoint
        // Just use the URLs directly, trimming any trailing slash
        val serverBaseUrl = wikiUrl.trimEnd('/')
        val attachmentServerBaseUrl = attachmentServerUrl.trimEnd('/')

        // Escape wikiPath for JavaScript (handle JSON and special characters)
        val escapedWikiPath = wikiPath?.replace("\\", "\\\\")?.replace("'", "\\'")?.replace("\n", "\\n") ?: ""

        // Script to handle external attachments - comprehensive implementation matching Desktop
        val externalAttachmentScript = """
            (function() {
                // Wait for TiddlyWiki to fully load (including ${'$'}tw.wiki)
                var twReady = (typeof ${'$'}tw !== 'undefined') && ${'$'}tw && ${'$'}tw.wiki;
                if (!twReady) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // Store the server base URL and wiki path
                window.__TD_SERVER_URL__ = '$serverBaseUrl';
                window.__TD_ATTACHMENT_SERVER_URL__ = '$attachmentServerBaseUrl';
                window.__WIKI_PATH__ = '$escapedWikiPath';
                window.__IS_FOLDER_WIKI__ = $isFolder;

                // ========== External Attachments Configuration ==========
                var PLUGIN_TITLE = "${'$'}:/plugins/tiddlydesktop-rs/injected";
                var CONFIG_PREFIX = "${'$'}:/plugins/tiddlydesktop-rs/external-attachments/";
                var CONFIG_ENABLE = CONFIG_PREFIX + "enable";
                var CONFIG_SETTINGS_TAB = CONFIG_PREFIX + "settings";
                var SESSION_AUTH_PREFIX = "${'$'}:/plugins/tiddlydesktop-rs/session-auth/";
                var SESSION_AUTH_SETTINGS_TAB = SESSION_AUTH_PREFIX + "settings";

                // Install save hook to filter out our injected plugin
                if (!window.__tdSaveHookInstalled && ${'$'}tw.hooks) {
                    ${'$'}tw.hooks.addHook("th-saving-tiddler", function(tiddler) {
                        if (tiddler && tiddler.fields && tiddler.fields.title) {
                            var title = tiddler.fields.title;
                            if (title === PLUGIN_TITLE ||
                                title.indexOf("${'$'}:/plugins/tiddlydesktop-rs/") === 0 ||
                                title.indexOf("${'$'}:/temp/tiddlydesktop") === 0) {
                                return null;
                            }
                        }
                        return tiddler;
                    });
                    window.__tdSaveHookInstalled = true;
                    console.log("[TiddlyDesktop] Save hook installed");
                }

                // Plugin tiddlers collection
                var pluginTiddlers = {};

                function addPluginTiddler(fields) {
                    pluginTiddlers[fields.title] = fields;
                }

                function removePluginTiddler(title) {
                    delete pluginTiddlers[title];
                }

                function registerPlugin() {
                    // Build plugin content
                    var pluginContent = { tiddlers: {} };
                    Object.keys(pluginTiddlers).forEach(function(title) {
                        pluginContent.tiddlers[title] = pluginTiddlers[title];
                    });

                    // Create/update the plugin tiddler
                    ${'$'}tw.wiki.addTiddler(new ${'$'}tw.Tiddler({
                        title: PLUGIN_TITLE,
                        type: "application/json",
                        "plugin-type": "plugin",
                        name: "TiddlyDesktop Injected",
                        description: "Runtime-injected TiddlyDesktop settings UI",
                        version: "1.0.0",
                        text: JSON.stringify(pluginContent)
                    }));

                    // Re-process plugins to unpack shadow tiddlers
                    ${'$'}tw.wiki.readPluginInfo();
                    ${'$'}tw.wiki.registerPluginTiddlers("plugin");
                    ${'$'}tw.wiki.unpackPluginTiddlers();

                    // Trigger UI refresh
                    ${'$'}tw.rootWidget.refresh({});

                    console.log("[TiddlyDesktop] Plugin registered with " + Object.keys(pluginTiddlers).length + " shadow tiddlers");
                }

                function updatePlugin() {
                    registerPlugin();
                }

                // Load settings from localStorage
                function loadSettings() {
                    try {
                        var key = 'tiddlydesktop_external_attachments_' + window.__WIKI_PATH__;
                        var stored = localStorage.getItem(key);
                        if (stored) {
                            return JSON.parse(stored);
                        }
                    } catch (e) {
                        console.error('[TiddlyDesktop] Failed to load settings:', e);
                    }
                    // Default: enabled for all wikis
                    return { enabled: true };
                }

                // Save settings to localStorage
                function saveSettings(settings) {
                    try {
                        var key = 'tiddlydesktop_external_attachments_' + window.__WIKI_PATH__;
                        localStorage.setItem(key, JSON.stringify(settings));
                    } catch (e) {
                        console.error('[TiddlyDesktop] Failed to save settings:', e);
                    }
                }

                // Initialize from saved settings
                var settings = loadSettings();

                function injectSettingsUI() {
                    // Set initial enable state using shadow tiddler
                    addPluginTiddler({
                        title: CONFIG_ENABLE,
                        text: settings.enabled ? "yes" : "no"
                    });

                    // Build settings tab content - matches Desktop style
                    var tabText = "When importing binary files (images, PDFs, etc.) into this wiki, you can optionally store them as external references instead of embedding them.\n\n" +
                        "This keeps your wiki file smaller and allows the files to be edited externally.\n\n" +
                        "<${'$'}checkbox tiddler=\"" + CONFIG_ENABLE + "\" field=\"text\" checked=\"yes\" unchecked=\"no\" default=\"yes\"> Enable external attachments</${'$'}checkbox>\n\n" +
                        "//Attachments will be saved in the ''attachments'' folder next to your wiki.//\n\n";

                    addPluginTiddler({
                        title: CONFIG_SETTINGS_TAB,
                        caption: "External Attachments",
                        tags: "${'$'}:/tags/ControlPanel/SettingsTab",
                        text: tabText
                    });

                    // Listen for changes to the enable setting
                    ${'$'}tw.wiki.addEventListener("change", function(changes) {
                        if (changes[CONFIG_ENABLE]) {
                            var enabled = ${'$'}tw.wiki.getTiddlerText(CONFIG_ENABLE) === "yes";
                            settings.enabled = enabled;
                            saveSettings(settings);
                            console.log('[TiddlyDesktop] External attachments ' + (enabled ? 'enabled' : 'disabled'));
                        }
                    });

                    console.log('[TiddlyDesktop] External attachments UI injected');
                }

                // ========== Transform image/media URLs for display ==========
                // Use MutationObserver to transform src attributes at render time
                // This preserves the original _canonical_uri (relative path) in the tiddler
                // while displaying images via the local attachment server

                // Get the attachment base URL dynamically.
                // For single-file wikis: use location.origin (always the current server).
                // For folder wikis: use the Kotlin attachment server URL via JS interface.
                // Using dynamic resolution means URLs survive server restarts.
                function getAttachmentBaseUrl() {
                    if (window.__IS_FOLDER_WIKI__) {
                        return window.TiddlyDesktopServer.getAttachmentServerUrl();
                    }
                    return location.origin;
                }

                function transformUrl(url) {
                    if (!url) return url;
                    // data: and blob: URLs should never be transformed
                    if (url.startsWith('data:') || url.startsWith('blob:')) {
                        return url;
                    }
                    var baseUrl = getAttachmentBaseUrl();
                    // For folder wikis: transform Node.js server URLs to use Kotlin attachment server
                    // This is needed because Node.js TiddlyWiki server may not support range requests
                    // for video seeking/thumbnail generation
                    if (window.__IS_FOLDER_WIKI__ && url.startsWith('http://') && window.__TD_SERVER_URL__) {
                        var serverBase = window.__TD_SERVER_URL__.replace(/\/$/, '');
                        if (url.startsWith(serverBase + '/')) {
                            // Extract the path after the server URL (e.g., /files/video.mp4 -> files/video.mp4)
                            var path = url.substring(serverBase.length + 1);
                            // Transform to attachment server's /_relative/ endpoint
                            return baseUrl + '/_relative/' + encodeURIComponent(path);
                        }
                    }
                    // Already transformed or external URL (https:// or http:// from different origin)
                    if (url.startsWith('http://') || url.startsWith('https://')) {
                        return url;
                    }
                    // Absolute paths or content:// URIs -> /_file/ endpoint
                    if (url.startsWith('/') || url.startsWith('content://') || url.startsWith('file://')) {
                        var encoded = btoa(url).replace(/\+/g, '-').replace(/\//g, '_').replace(/=/g, '');
                        return baseUrl + '/_file/' + encoded;
                    }
                    // Relative paths -> /_relative/ endpoint
                    return baseUrl + '/_relative/' + encodeURIComponent(url);
                }

                // Helper: check if a URL should be transformed
                // For folder wikis, we also need to transform http:// URLs that point to the Node.js server
                function shouldTransform(url) {
                    if (!url) return false;
                    if (url.startsWith('data:') || url.startsWith('blob:')) return false;
                    // For folder wikis: transform Node.js server URLs
                    if (window.__IS_FOLDER_WIKI__ && url.startsWith('http://') && window.__TD_SERVER_URL__) {
                        var serverBase = window.__TD_SERVER_URL__.replace(/\/$/, '');
                        console.log('[TiddlyDesktop] shouldTransform check:', url, 'serverBase:', serverBase, 'match:', url.startsWith(serverBase + '/'));
                        if (url.startsWith(serverBase + '/')) return true;
                    }
                    // Transform relative paths and local file paths
                    if (!url.startsWith('http://') && !url.startsWith('https://')) return true;
                    return false;
                }

                function transformElement(el) {
                    // Transform img src
                    if (el.tagName === 'IMG' && el.src) {
                        var originalSrc = el.getAttribute('src');
                        if (shouldTransform(originalSrc)) {
                            el.src = transformUrl(originalSrc);
                        }
                    }
                    // Transform video/audio src
                    if ((el.tagName === 'VIDEO' || el.tagName === 'AUDIO') && el.src) {
                        var originalSrc = el.getAttribute('src');
                        console.log('[TiddlyDesktop] transformElement VIDEO/AUDIO:', originalSrc, 'isFolder:', window.__IS_FOLDER_WIKI__, 'serverUrl:', window.__TD_SERVER_URL__);
                        if (shouldTransform(originalSrc)) {
                            var newSrc = transformUrl(originalSrc);
                            console.log('[TiddlyDesktop] Transformed video src:', originalSrc, '->', newSrc);
                            el.src = newSrc;
                        }
                    }
                    // Transform source elements inside video/audio
                    if (el.tagName === 'SOURCE' && el.src) {
                        var originalSrc = el.getAttribute('src');
                        if (shouldTransform(originalSrc)) {
                            el.src = transformUrl(originalSrc);
                        }
                    }
                    // Transform object/embed data
                    if ((el.tagName === 'OBJECT' || el.tagName === 'EMBED') && el.data) {
                        var originalData = el.getAttribute('data');
                        if (shouldTransform(originalData)) {
                            el.data = transformUrl(originalData);
                        }
                    }
                    // Transform iframe src for PDFs etc
                    if (el.tagName === 'IFRAME' && el.src) {
                        var originalSrc = el.getAttribute('src');
                        if (shouldTransform(originalSrc) && !originalSrc.startsWith('about:')) {
                            el.src = transformUrl(originalSrc);
                        }
                    }
                }

                // Transform existing elements
                document.querySelectorAll('img, video, audio, source, object, embed, iframe').forEach(transformElement);

                // Watch for new elements
                var observer = new MutationObserver(function(mutations) {
                    mutations.forEach(function(mutation) {
                        mutation.addedNodes.forEach(function(node) {
                            if (node.nodeType === 1) { // Element node
                                transformElement(node);
                                // Also check children
                                node.querySelectorAll && node.querySelectorAll('img, video, audio, source, object, embed, iframe').forEach(transformElement);
                            }
                        });
                        // Also handle attribute changes on existing elements
                        if (mutation.type === 'attributes' && mutation.attributeName === 'src') {
                            transformElement(mutation.target);
                        }
                    });
                });
                observer.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['src', 'data'] });
                // Expose for tm-open-window overlays (iframe has its own document)
                window.__tdTransformElement = transformElement;
                console.log('[TiddlyDesktop] URL transform observer installed');

                // ========== Import Hook (th-importing-file) - matches Desktop ==========
                function installImportHook() {
                    if (!${'$'}tw.hooks) {
                        setTimeout(installImportHook, 100);
                        return;
                    }

                    ${'$'}tw.hooks.addHook("th-importing-file", function(info) {
                        // Guard against invokeHook chaining: if a previous hook returned
                        // a non-object (e.g., false), info won't have the expected properties.
                        // Pass through the value unchanged so chaining continues correctly.
                        if (!info || typeof info !== 'object' || !info.file) {
                            return info;
                        }

                        var file = info.file;
                        var filename = file.name;
                        var type = info.type;

                        console.log('[TiddlyDesktop] th-importing-file hook: filename=' + filename + ', type=' + type + ', isBinary=' + info.isBinary);

                        // Check if there's a deserializer for this file type
                        // Also resolve the type from filename extension (Android ContentResolver
                        // often returns application/octet-stream for .tid, .json, .csv, etc.)
                        var hasDeserializer = false;
                        var extType = null;
                        if (filename) {
                            var dotPos = filename.lastIndexOf('.');
                            if (dotPos !== -1) {
                                var extInfo = ${'$'}tw.utils.getFileExtensionInfo ? ${'$'}tw.utils.getFileExtensionInfo(filename.substr(dotPos)) : null;
                                if (extInfo) {
                                    extType = extInfo.type;
                                }
                            }
                        }
                        if (${'$'}tw.Wiki.tiddlerDeserializerModules) {
                            // Check MIME type directly
                            if (${'$'}tw.Wiki.tiddlerDeserializerModules[type]) {
                                hasDeserializer = true;
                            }
                            // Check extension-resolved type
                            if (!hasDeserializer && extType && ${'$'}tw.Wiki.tiddlerDeserializerModules[extType]) {
                                hasDeserializer = true;
                            }
                            // Check deserializerType from contentTypeInfo
                            if (!hasDeserializer && ${'$'}tw.config.contentTypeInfo) {
                                var cti = ${'$'}tw.config.contentTypeInfo[type] || (extType && ${'$'}tw.config.contentTypeInfo[extType]);
                                if (cti && cti.deserializerType && ${'$'}tw.Wiki.tiddlerDeserializerModules[cti.deserializerType]) {
                                    hasDeserializer = true;
                                }
                            }
                        }

                        // If there's a deserializer, let TiddlyWiki handle it (import as tiddlers)
                        if (hasDeserializer) {
                            // Update info.type so TiddlyWiki uses the correct deserializer
                            // (Android ContentResolver often returns application/octet-stream)
                            if (extType && extType !== type) {
                                info.type = extType;
                            }
                            console.log('[TiddlyDesktop] Deserializer found for type ' + info.type + (extType ? ' (ext: ' + extType + ')' : '') + ', letting TiddlyWiki handle import');
                            return info;
                        }

                        // Check if external attachments are enabled
                        var externalEnabled = ${'$'}tw.wiki.getTiddlerText(CONFIG_ENABLE, "yes") === "yes";

                        // Determine if this is a binary file
                        // Use extension-resolved type if available (don't trust application/octet-stream)
                        var effectiveType = extType || type;
                        var effectiveContentTypeInfo = ${'$'}tw.config.contentTypeInfo[effectiveType];
                        var isBinaryType = (effectiveContentTypeInfo ? effectiveContentTypeInfo.encoding === 'base64' : info.isBinary) ||
                            effectiveType.indexOf('audio/') === 0 ||
                            effectiveType.indexOf('video/') === 0 ||
                            effectiveType.indexOf('image/') === 0 ||
                            effectiveType === 'application/pdf';

                        // Only handle binary files when external attachments enabled
                        if (!externalEnabled || !isBinaryType) {
                            console.log('[TiddlyDesktop] Letting TiddlyWiki handle import (external=' + externalEnabled + ', binary=' + info.isBinary + ', isBinaryType=' + isBinaryType + ')');
                            return info; // Let TiddlyWiki handle it normally
                        }

                        console.log('[TiddlyDesktop] Intercepting binary import for external attachment: ' + filename);

                        // Get the content:// URI for this file (stored when file was picked)
                        if (typeof window.TiddlyDesktopAttachments === 'undefined' ||
                            typeof window.TiddlyDesktopAttachments.getFileUri !== 'function') {
                            console.log('[TiddlyDesktop] Attachment interface not available, letting TiddlyWiki handle');
                            return info;
                        }

                        try {
                            var resultJson = window.TiddlyDesktopAttachments.getFileUri(filename);
                            console.log('[TiddlyDesktop] getFileUri result:', resultJson);
                            var response = JSON.parse(resultJson);

                            if (response.success && response.uri) {
                                var sourceUri = response.uri;
                                console.log('[TiddlyDesktop] Copying attachment to local folder: ' + filename);

                                // Get file metadata first
                                var metadata = { filename: filename, size: -1, mimeType: type };
                                try {
                                    var metaJson = window.TiddlyDesktopAttachments.getFileMetadata(sourceUri);
                                    var metaResponse = JSON.parse(metaJson);
                                    if (metaResponse.success) {
                                        metadata.filename = metaResponse.filename || filename;
                                        metadata.size = metaResponse.size || -1;
                                        metadata.mimeType = metaResponse.mimeType || type;
                                    }
                                } catch (metaErr) {
                                    console.warn('[TiddlyDesktop] Could not get file metadata:', metaErr);
                                }

                                // Check if we have folder access first
                                if (!window.TiddlyDesktopAttachments.hasFolderAccess()) {
                                    console.log('[TiddlyDesktop] No folder access - requesting permission');
                                    // Store callback for when folder picker returns
                                    window.__pendingAttachmentCallback = function(result) {
                                        if (result && result.success && result.path) {
                                            console.log('[TiddlyDesktop] Folder access granted, attachment copied: ' + result.path);
                                            info.callback([{
                                                title: metadata.filename,
                                                type: metadata.mimeType,
                                                _canonical_uri: result.path
                                            }]);
                                        } else {
                                            console.log('[TiddlyDesktop] Folder access denied or failed, embedding file');
                                            // Fall back to embedding
                                            info.callback(null);
                                        }
                                    };
                                    // Request folder access (will call callback when done)
                                    window.TiddlyDesktopAttachments.requestFolderAccessForAttachment(sourceUri, metadata.filename, metadata.mimeType);
                                    return true; // Signal we're handling it asynchronously
                                }

                                // Copy file to attachments folder (avoids SAF permission expiry)
                                var copyResultJson = window.TiddlyDesktopAttachments.copyToAttachments(sourceUri, metadata.filename, metadata.mimeType);
                                console.log('[TiddlyDesktop] copyToAttachments result:', copyResultJson);
                                var copyResult = JSON.parse(copyResultJson);

                                if (copyResult.success && copyResult.path) {
                                    // Use relative path for _canonical_uri (works after app restart)
                                    console.log('[TiddlyDesktop] Attachment copied: ' + copyResult.path);
                                    info.callback([{
                                        title: metadata.filename,
                                        type: metadata.mimeType,
                                        _canonical_uri: copyResult.path
                                    }]);
                                    return true;
                                } else {
                                    console.error('[TiddlyDesktop] Failed to copy attachment: ' + (copyResult.error || 'unknown error'));
                                    // Fall through to let TiddlyWiki embed it
                                }
                            }

                            console.log('[TiddlyDesktop] Letting TiddlyWiki embed the file');
                            return info; // Let TiddlyWiki handle it (embed)
                        } catch (e) {
                            console.error('[TiddlyDesktop] Error handling import:', e && e.message ? e.message : String(e));
                            return info; // Let TiddlyWiki handle it
                        }
                    });

                    console.log('[TiddlyDesktop] Import hook (th-importing-file) installed');
                }

                // ========== Session Auth Configuration - matches Desktop ==========
                // Load auth URLs from localStorage
                function loadAuthUrls() {
                    try {
                        var key = 'tiddlydesktop_auth_urls_' + window.__WIKI_PATH__;
                        var stored = localStorage.getItem(key);
                        if (stored) {
                            return JSON.parse(stored);
                        }
                    } catch (e) {
                        console.error('[TiddlyDesktop] Failed to load auth URLs:', e);
                    }
                    return [];
                }

                // Save auth URLs to localStorage
                function saveAuthUrls(urls) {
                    try {
                        var key = 'tiddlydesktop_auth_urls_' + window.__WIKI_PATH__;
                        localStorage.setItem(key, JSON.stringify(urls));
                    } catch (e) {
                        console.error('[TiddlyDesktop] Failed to save auth URLs:', e);
                    }
                }

                function rebuildUrlTiddlers() {
                    var titles = [];
                    ${'$'}tw.wiki.each(function(tiddler, title) {
                        if (title.indexOf(SESSION_AUTH_PREFIX + "url/") === 0) {
                            titles.push(title);
                        }
                    });
                    titles.forEach(function(title) {
                        ${'$'}tw.wiki.deleteTiddler(title);
                    });
                    var authUrls = loadAuthUrls();
                    authUrls.forEach(function(entry, index) {
                        ${'$'}tw.wiki.addTiddler(new ${'$'}tw.Tiddler({
                            title: SESSION_AUTH_PREFIX + "url/" + index,
                            name: entry.name,
                            url: entry.url,
                            text: ""
                        }));
                    });
                }

                function injectSessionAuthUI() {
                    // Load and inject auth URL tiddlers as real tiddlers (not shadow)
                    // Real tiddlers are visible to filter operators and trigger proper refresh
                    var authUrls = loadAuthUrls();
                    authUrls.forEach(function(entry, index) {
                        ${'$'}tw.wiki.addTiddler(new ${'$'}tw.Tiddler({
                            title: SESSION_AUTH_PREFIX + "url/" + index,
                            name: entry.name,
                            url: entry.url,
                            text: ""
                        }));
                    });

                    var tabText = "Authenticate with external services to access protected resources (like SharePoint profile images).\n\n" +
                        "Session cookies will be stored in this wiki's isolated session data.\n\n" +
                        "!! Authentication URLs\n\n" +
                        "<${'$'}list filter=\"[prefix[" + SESSION_AUTH_PREFIX + "url/]]\" variable=\"urlTiddler\">\n" +
                        "<div style=\"display:flex;align-items:center;gap:8px;margin-bottom:8px;padding:8px;background:rgba(128,128,128,0.1);border-radius:4px;\">\n" +
                        "<div style=\"flex:1;\">\n" +
                        "<strong><${'$'}text text={{{ [<urlTiddler>get[name]] }}}/></strong><br/>\n" +
                        "<small><${'$'}text text={{{ [<urlTiddler>get[url]] }}}/></small>\n" +
                        "</div>\n" +
                        "<${'$'}button class=\"tc-btn-invisible tc-tiddlylink\" message=\"tm-tiddlydesktop-open-auth-url\" param=<<urlTiddler>> tooltip=\"Open login page\">\n" +
                        "{{${'$'}:/core/images/external-link}} Login\n" +
                        "</${'$'}button>\n" +
                        "<${'$'}button class=\"tc-btn-invisible tc-tiddlylink\" message=\"tm-tiddlydesktop-remove-auth-url\" param=<<urlTiddler>> tooltip=\"Remove this URL\">\n" +
                        "{{${'$'}:/core/images/delete-button}}\n" +
                        "</${'$'}button>\n" +
                        "</div>\n" +
                        "</${'$'}list>\n\n" +
                        "<${'$'}list filter=\"[prefix[" + SESSION_AUTH_PREFIX + "url/]count[]match[0]]\" variable=\"ignore\">\n" +
                        "//No authentication URLs configured.//\n" +
                        "</${'$'}list>\n\n" +
                        "!! Add New URL\n\n" +
                        "<${'$'}edit-text tiddler=\"" + SESSION_AUTH_PREFIX + "new-name\" tag=\"input\" placeholder=\"Name (e.g. SharePoint)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:4px;\"/>\n\n" +
                        "<${'$'}edit-text tiddler=\"" + SESSION_AUTH_PREFIX + "new-url\" tag=\"input\" placeholder=\"URL (e.g. https://company.sharepoint.com)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:8px;\"/>\n\n" +
                        "<${'$'}button message=\"tm-tiddlydesktop-add-auth-url\" class=\"tc-btn-big-green\">Add URL</${'$'}button>\n\n" +
                        "!! Session Data\n\n" +
                        "This wiki has its own isolated session storage (cookies, localStorage). You can clear it if you want to log out of all services.\n\n" +
                        "<${'$'}button message=\"tm-tiddlydesktop-clear-session\" class=\"tc-btn-big-green\" style=\"background:#c42b2b;\">Clear Session Data</${'$'}button>\n\n" +
                        "//This will clear all cookies and localStorage for this wiki. You will need to log in again to any authenticated services.//\n";

                    addPluginTiddler({
                        title: SESSION_AUTH_SETTINGS_TAB,
                        caption: "Session Auth",
                        tags: "${'$'}:/tags/ControlPanel/SettingsTab",
                        text: tabText
                    });

                    // Message handler: add new auth URL
                    ${'$'}tw.rootWidget.addEventListener("tm-tiddlydesktop-add-auth-url", function(event) {
                        var name = ${'$'}tw.wiki.getTiddlerText(SESSION_AUTH_PREFIX + "new-name", "").trim();
                        var url = ${'$'}tw.wiki.getTiddlerText(SESSION_AUTH_PREFIX + "new-url", "").trim();

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
                        var isLocalhost = parsedUrl.hostname === "localhost" || parsedUrl.hostname === "127.0.0.1";
                        var isLocalhostHttp = parsedUrl.protocol === "http:" && isLocalhost;

                        if (!isHttps && !isLocalhostHttp) {
                            alert("Security: Only HTTPS URLs are allowed for authentication (except localhost)");
                            return;
                        }

                        // Add to list
                        var authUrls = loadAuthUrls();
                        authUrls.push({ name: name, url: url });
                        saveAuthUrls(authUrls);

                        // Add real tiddler for UI (triggers proper widget refresh)
                        var index = authUrls.length - 1;
                        ${'$'}tw.wiki.addTiddler(new ${'$'}tw.Tiddler({
                            title: SESSION_AUTH_PREFIX + "url/" + index,
                            name: name,
                            url: url,
                            text: ""
                        }));

                        // Clear input fields (these are real tiddlers created by edit-text widget)
                        ${'$'}tw.wiki.deleteTiddler(SESSION_AUTH_PREFIX + "new-name");
                        ${'$'}tw.wiki.deleteTiddler(SESSION_AUTH_PREFIX + "new-url");
                    });

                    // Message handler: remove auth URL
                    ${'$'}tw.rootWidget.addEventListener("tm-tiddlydesktop-remove-auth-url", function(event) {
                        var tiddlerTitle = event.param;
                        if (tiddlerTitle) {
                            var tiddler = ${'$'}tw.wiki.getTiddler(tiddlerTitle);
                            if (tiddler) {
                                var urlToRemove = tiddler.fields.url;
                                // Remove from storage
                                var authUrls = loadAuthUrls();
                                authUrls = authUrls.filter(function(entry) { return entry.url !== urlToRemove; });
                                saveAuthUrls(authUrls);

                                // Rebuild URL tiddlers from localStorage
                                rebuildUrlTiddlers();
                            }
                        }
                    });

                    // Message handler: open auth URL in browser
                    ${'$'}tw.rootWidget.addEventListener("tm-tiddlydesktop-open-auth-url", function(event) {
                        var tiddlerTitle = event.param;
                        if (tiddlerTitle) {
                            var tiddler = ${'$'}tw.wiki.getTiddler(tiddlerTitle);
                            if (tiddler && tiddler.fields.url) {
                                // Open in overlay WebView (same cookie store for auth)
                                TiddlyDesktopExternal.openAuthUrl(tiddler.fields.url);
                            }
                        }
                    });

                    // Message handler: clear session data
                    ${'$'}tw.rootWidget.addEventListener("tm-tiddlydesktop-clear-session", function(event) {
                        if (confirm("Are you sure you want to clear all session data for this wiki?\n\nThis will log you out of all authenticated services.")) {
                            try {
                                // Note: We preserve auth URLs list but clear everything else
                                var authUrlsKey = 'tiddlydesktop_auth_urls_' + window.__WIKI_PATH__;
                                var extAttachKey = 'tiddlydesktop_external_attachments_' + window.__WIKI_PATH__;
                                var authUrls = localStorage.getItem(authUrlsKey);
                                var extAttach = localStorage.getItem(extAttachKey);

                                localStorage.clear();
                                sessionStorage.clear();

                                // Restore settings
                                if (authUrls) localStorage.setItem(authUrlsKey, authUrls);
                                if (extAttach) localStorage.setItem(extAttachKey, extAttach);

                                alert("Session data cleared. Please reload the wiki for changes to take effect.");
                            } catch (e) {
                                alert("Failed to clear session data: " + e.message);
                            }
                        }
                    });

                    console.log('[TiddlyDesktop] Session auth UI injected');
                }

                // NOTE: Broken Attachments UI removed - now using local "attachments" subfolder approach
                // which doesn't require re-authorization after app reinstall

                // ========== Android Drag/Drop Bracket Stripping ==========
                // When dragging tiddler links with spaces on Android, WebView wraps them in [[...]]
                // This handler strips those brackets on drop for non-editable targets (like tc-droppable)
                // For editable elements (inputs), we let native handling work - brackets are fine there
                function installBracketStripping() {
                    if (window.__tdBracketStripInstalled) return;
                    window.__tdBracketStripInstalled = true;

                    function stripBrackets(text) {
                        if (!text || typeof text !== 'string') return text;
                        var match = text.match(/^\[\[(.+)\]\]$/);
                        if (match) {
                            console.log('[TiddlyDesktop] Stripped brackets from: ' + text);
                            return match[1];
                        }
                        return text;
                    }

                    function isEditable(el) {
                        if (!el) return false;
                        var tagName = el.tagName;
                        if (tagName === 'INPUT') {
                            var type = (el.type || 'text').toLowerCase();
                            return ['text', 'search', 'url', 'tel', 'email', 'password'].indexOf(type) !== -1;
                        }
                        if (tagName === 'TEXTAREA') return true;
                        if (el.isContentEditable) return true;
                        return false;
                    }

                    document.addEventListener('drop', function(event) {
                        var dt = event.dataTransfer;
                        if (!dt) return;

                        // Skip editable elements - let native handling work
                        if (isEditable(event.target)) return;

                        var textData = null;
                        try {
                            textData = dt.getData('text/plain');
                        } catch(e) {}

                        if (textData && /^\[\[.+\]\]$/.test(textData)) {
                            var strippedText = stripBrackets(textData);
                            event.preventDefault();
                            event.stopImmediatePropagation();

                            var newDt = new DataTransfer();
                            newDt.setData('text/plain', strippedText);

                            if (dt.files) {
                                for (var i = 0; i < dt.files.length; i++) {
                                    newDt.items.add(dt.files[i]);
                                }
                            }

                            var newEvent = new DragEvent('drop', {
                                bubbles: true,
                                cancelable: true,
                                dataTransfer: newDt,
                                clientX: event.clientX,
                                clientY: event.clientY,
                                screenX: event.screenX,
                                screenY: event.screenY
                            });
                            newEvent.__tdBracketStripped = true;
                            event.target.dispatchEvent(newEvent);
                        }
                    }, true);

                    console.log('[TiddlyDesktop] Android bracket stripping installed');
                }
                installBracketStripping();

                // ========== Extra Media Type Support ==========
                // Register file types and parsers for formats not in TiddlyWiki core.
                // File type registration enables import recognition; parser registration
                // enables rendering as <audio>/<video> elements.
                function registerExtraMediaTypes() {
                    // Audio types missing from core
                    ${'$'}tw.utils.registerFileType("audio/wav","base64",[".wav",".wave"]);
                    ${'$'}tw.utils.registerFileType("audio/flac","base64",".flac");
                    ${'$'}tw.utils.registerFileType("audio/aac","base64",".aac");
                    ${'$'}tw.utils.registerFileType("audio/webm","base64",".weba");
                    ${'$'}tw.utils.registerFileType("audio/opus","base64",".opus");
                    ${'$'}tw.utils.registerFileType("audio/aiff","base64",[".aiff",".aif"]);
                    // Video types missing from core
                    ${'$'}tw.utils.registerFileType("video/quicktime","base64",".mov");
                    ${'$'}tw.utils.registerFileType("video/x-matroska","base64",".mkv");
                    ${'$'}tw.utils.registerFileType("video/3gpp","base64",".3gp");

                    // Audio parser for types not covered by core's audioparser.js
                    var ExtraAudioParser = function(type,text,options) {
                        var element = {
                            type: "element",
                            tag: "audio",
                            attributes: {
                                controls: {type: "string", value: "controls"},
                                preload: {type: "string", value: "auto"},
                                style: {type: "string", value: "width: 100%; object-fit: contain"}
                            }
                        };
                        if(options._canonical_uri) {
                            element.attributes.src = {type: "string", value: options._canonical_uri};
                        } else if(text) {
                            element.attributes.src = {type: "string", value: "data:" + type + ";base64," + text};
                        }
                        this.tree = [element];
                        this.source = text;
                        this.type = type;
                    };

                    // Video parser for types not covered by core's videoparser.js
                    var ExtraVideoParser = function(type,text,options) {
                        var element = {
                            type: "element",
                            tag: "video",
                            attributes: {
                                controls: {type: "string", value: "controls"},
                                style: {type: "string", value: "width: 100%; object-fit: contain"}
                            }
                        };
                        if(options._canonical_uri) {
                            element.attributes.src = {type: "string", value: options._canonical_uri};
                        } else if(text) {
                            element.attributes.src = {type: "string", value: "data:" + type + ";base64," + text};
                        }
                        this.tree = [element];
                        this.source = text;
                        this.type = type;
                    };

                    // Register parsers directly (initParsers() already ran at startup)
                    var audioTypes = ["audio/wav","audio/wave","audio/x-wav","audio/flac",
                                      "audio/aac","audio/webm","audio/opus","audio/aiff","audio/x-aiff"];
                    var videoTypes = ["video/quicktime","video/x-matroska","video/3gpp"];

                    audioTypes.forEach(function(t) {
                        if(!${'$'}tw.Wiki.parsers[t]) {
                            ${'$'}tw.Wiki.parsers[t] = ExtraAudioParser;
                        }
                    });
                    videoTypes.forEach(function(t) {
                        if(!${'$'}tw.Wiki.parsers[t]) {
                            ${'$'}tw.Wiki.parsers[t] = ExtraVideoParser;
                        }
                    });

                    console.log('[TiddlyDesktop] Extra media types registered');
                }

                // Initialize - add UI tiddlers then register as plugin
                // Capture dirty state before any modifications
                var originalNumChanges = ${'$'}tw.saverHandler ? ${'$'}tw.saverHandler.numChanges : 0;

                registerExtraMediaTypes();
                // Refresh any tiddlers with newly-registered media types.
                // Parsers were registered after TW's initial render (onPageFinished),
                // so tiddlers with these types may have failed to render.
                (function() {
                    var changedTiddlers = {};
                    ${'$'}tw.wiki.each(function(tiddler, title) {
                        var type = tiddler.fields.type;
                        if (type && ${'$'}tw.Wiki.parsers[type] &&
                            (type.startsWith("audio/") || type.startsWith("video/"))) {
                            ${'$'}tw.wiki.clearCache(title);
                            changedTiddlers[title] = {modified: true};
                        }
                    });
                    var keys = Object.keys(changedTiddlers);
                    if (keys.length > 0) {
                        console.log('[TiddlyDesktop] Refreshing ' + keys.length + ' media tiddlers');
                        ${'$'}tw.rootWidget.refresh(changedTiddlers);
                    }
                })();
                injectSettingsUI();
                injectSessionAuthUI();
                registerPlugin();  // Register all shadow tiddlers as a plugin
                installImportHook();

                // Override clipboard for Android — document.execCommand("copy") doesn't work in WebView.
                // Replace the tm-copy-to-clipboard handler with one that:
                //   1. Uses native Android clipboard (via JavascriptInterface)
                //   2. When event.param is empty (src variable was empty at render time),
                //      walks up the widget tree to find the TranscludeWidget that provides
                //      the 'src' parameter and re-evaluates its filter/variable at click time.
                if (${'$'}tw.rootWidget.eventListeners && ${'$'}tw.rootWidget.eventListeners['tm-copy-to-clipboard']) {
                    ${'$'}tw.rootWidget.eventListeners['tm-copy-to-clipboard'] = [];
                }
                ${'$'}tw.rootWidget.addEventListener('tm-copy-to-clipboard', function(event) {
                    var text = event.param || '';
                    // If param is empty, re-evaluate the src from ancestor TranscludeWidgets' parse trees.
                    // The src can be a variable (<<var>>) that resolves to empty in the widget tree
                    // even though the underlying filter/reference has data at click time.
                    if (!text && event.widget) {
                        var w = event.widget;
                        while (w) {
                            if (w.parseTreeNode && w.parseTreeNode.attributes && w.parseTreeNode.attributes.src) {
                                var attr = w.parseTreeNode.attributes.src;
                                try {
                                    if (attr.type === 'filtered') {
                                        var result = ${'$'}tw.wiki.filterTiddlers(attr.filter, w);
                                        if (result && result[0]) { text = result[0]; break; }
                                    } else if (attr.type === 'macro') {
                                        var info = w.getVariableInfo(attr.value.name, {params: attr.value.params});
                                        if (info && info.text) { text = info.text; break; }
                                    } else if (attr.type === 'indirect') {
                                        var val = ${'$'}tw.wiki.getTextReference(attr.textReference, '', w.getVariable('currentTiddler'));
                                        if (val) { text = val; break; }
                                    } else if (attr.type === 'string' && attr.value) {
                                        text = attr.value; break;
                                    } else if (attr.type === 'substituted') {
                                        var val = ${'$'}tw.wiki.getSubstitutedText(attr.rawValue, w);
                                        if (val) { text = val; break; }
                                    }
                                } catch(e) {}
                            }
                            w = w.parentWidget;
                        }
                    }
                    var opts = {
                        successNotification: (event.paramObject && event.paramObject.successNotification) || '${'$'}:/language/Notifications/CopiedToClipboard/Succeeded',
                        failureNotification: (event.paramObject && event.paramObject.failureNotification) || '${'$'}:/language/Notifications/CopiedToClipboard/Failed'
                    };
                    if (text && window.TiddlyDesktopClipboard) {
                        TiddlyDesktopClipboard.copyText(text);
                        ${'$'}tw.notifier.display(opts.successNotification);
                    } else {
                        ${'$'}tw.notifier.display(opts.failureNotification);
                    }
                });
                // Also override ${'$'}tw.utils.copyToClipboard for direct programmatic calls
                ${'$'}tw.utils.copyToClipboard = function(text, options) {
                    options = options || {};
                    text = text || '';
                    if (text && window.TiddlyDesktopClipboard) {
                        TiddlyDesktopClipboard.copyText(text);
                        if (!options.doNotNotify && ${'$'}tw.notifier) {
                            ${'$'}tw.notifier.display(options.successNotification || '${'$'}:/language/Notifications/CopiedToClipboard/Succeeded');
                        }
                    } else {
                        if (!options.doNotNotify && ${'$'}tw.notifier) {
                            ${'$'}tw.notifier.display(options.failureNotification || '${'$'}:/language/Notifications/CopiedToClipboard/Failed');
                        }
                    }
                };

                // Restore dirty state after event loop completes - plugin injection should not mark wiki as modified
                // Using setTimeout(0) ensures all synchronous change events have propagated
                setTimeout(function() {
                    if (${'$'}tw.saverHandler) {
                        ${'$'}tw.saverHandler.numChanges = originalNumChanges;
                        ${'$'}tw.saverHandler.updateDirtyStatus();
                        console.log('[TiddlyDesktop] Dirty state restored after plugin injection');
                    }
                }, 0);

                console.log('[TiddlyDesktop] External attachments handler installed');
            })();
        """.trimIndent()

        // Script to install the saver for single-file wikis
        val saverScript = if (!isFolder) """
            (function() {
                // Wait for TiddlyWiki to load
                if (typeof ${'$'}tw === 'undefined') {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // Register a custom saver
                function TiddlyDesktopSaver(wiki) {
                    this.wiki = wiki;
                }

                TiddlyDesktopSaver.prototype.save = function(text, method, callback) {
                    var self = this;
                    var contentLength = new Blob([text]).size;
                    console.log('[TiddlyDesktop Saver] Saving ' + text.length + ' chars (' + contentLength + ' bytes) via ' + method + '...');

                    var cloudSaverNames = ['github', 'gitlab', 'Gitea', 'upload'];
                    function chainCloudSavers() {
                        if (!${'$'}tw || !${'$'}tw.saverHandler) return;
                        var savers = ${'$'}tw.saverHandler.savers;
                        for (var i = savers.length - 1; i >= 0; i--) {
                            var saver = savers[i];
                            if (cloudSaverNames.indexOf(saver.info.name) === -1) continue;
                            if (saver.info.capabilities.indexOf(method) === -1) continue;
                            (function(s) {
                                try {
                                    if (s.save(text, method, function(err) {
                                        if (err) {
                                            console.warn('[TiddlyDesktop] Cloud saver \'' + s.info.name + '\' failed: ' + err);
                                        } else {
                                            console.log('[TiddlyDesktop] Cloud saver \'' + s.info.name + '\' succeeded');
                                        }
                                    })) {
                                        console.log('[TiddlyDesktop] Triggered cloud saver: ' + s.info.name);
                                    }
                                } catch(e) {
                                    console.warn('[TiddlyDesktop] Cloud saver \'' + s.info.name + '\' threw: ' + e);
                                }
                            })(saver);
                        }
                    }

                    var xhr = new XMLHttpRequest();
                    xhr.timeout = 60000;
                    xhr.open('PUT', '$wikiUrl', true);
                    xhr.setRequestHeader('Content-Type', 'text/html;charset=UTF-8');

                    xhr.onload = function() {
                        if (xhr.status === 200) {
                            console.log('[TiddlyDesktop Saver] Save successful (' + contentLength + ' bytes)');
                            callback(null);
                            chainCloudSavers();
                        } else {
                            var msg = 'Save failed: HTTP ' + xhr.status + ' ' + xhr.statusText + ' — ' + xhr.responseText;
                            console.error('[TiddlyDesktop Saver] ' + msg);
                            callback(msg);
                        }
                    };

                    xhr.ontimeout = function() {
                        console.error('[TiddlyDesktop Saver] Save timed out after 60s (' + contentLength + ' bytes)');
                        callback('Save timed out after 60 seconds');
                    };

                    xhr.onerror = function() {
                        console.error('[TiddlyDesktop Saver] Network error saving ' + contentLength + ' bytes');
                        callback('Network error during save');
                    };

                    xhr.send(text);
                    return true;
                };

                TiddlyDesktopSaver.prototype.info = {
                    name: 'tiddlydesktop',
                    priority: 5000,
                    capabilities: ['save', 'autosave']
                };

                // Wait for saverHandler to be ready, then register
                function addToSaverHandler() {
                    if (!${'$'}tw.saverHandler) {
                        setTimeout(addToSaverHandler, 50);
                        return;
                    }

                    // Check if already added
                    var alreadyAdded = ${'$'}tw.saverHandler.savers.some(function(s) {
                        return s.info && s.info.name === 'tiddlydesktop';
                    });

                    if (!alreadyAdded) {
                        var saver = new TiddlyDesktopSaver(${'$'}tw.wiki);
                        // Add to array and re-sort by priority (TiddlyWiki iterates backwards)
                        ${'$'}tw.saverHandler.savers.push(saver);
                        ${'$'}tw.saverHandler.savers.sort(function(a, b) {
                            if (a.info.priority < b.info.priority) return -1;
                            if (a.info.priority > b.info.priority) return 1;
                            return 0;
                        });
                        console.log('[TiddlyDesktop] Saver registered via saverHandler');
                    }
                }

                addToSaverHandler();
                console.log('[TiddlyDesktop] Saver script installed');
            })();
        """.trimIndent() else ""

        // Script to monitor palette changes and update system bar colors
        val paletteScript = """
            (function() {
                // Wait for TiddlyWiki to fully load (including after decryption for encrypted wikis)
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.wiki) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // Function to get a color from TiddlyWiki's current palette (with recursive resolution)
                function getColour(name, fallback, depth) {
                    depth = depth || 0;
                    if (depth > 10) return fallback; // Prevent infinite recursion

                    try {
                        // Get the current palette title
                        var paletteName = ${'$'}tw.wiki.getTiddlerText('${'$'}:/palette');
                        if (paletteName) {
                            paletteName = paletteName.trim();
                            var paletteTiddler = ${'$'}tw.wiki.getTiddler(paletteName);
                            if (paletteTiddler) {
                                // Colors are in the tiddler text (one per line: name: value)
                                var text = paletteTiddler.fields.text || '';
                                var lines = text.split('\n');
                                for (var i = 0; i < lines.length; i++) {
                                    var line = lines[i].trim();
                                    var colonIndex = line.indexOf(':');
                                    if (colonIndex > 0) {
                                        var colorName = line.substring(0, colonIndex).trim();
                                        var colorValue = line.substring(colonIndex + 1).trim();
                                        if (colorName === name && colorValue) {
                                            // Handle references to other colors like <<colour background>>
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
                    } catch (e) {
                        console.error('[TiddlyDesktop] getColour error:', e);
                    }
                    return fallback;
                }

                // Resolve any CSS color to #rrggbb or rgba(r,g,b,a) using a canvas context
                var _colorCtx = null;
                function resolveCssColor(color, fallback) {
                    try {
                        if (!_colorCtx) _colorCtx = document.createElement('canvas').getContext('2d');
                        _colorCtx.fillStyle = '#000';
                        _colorCtx.fillStyle = color;
                        var resolved = _colorCtx.fillStyle;
                        // fillStyle stays '#000000' if the color was invalid
                        if (resolved === '#000000' && color.trim().toLowerCase() !== '#000000' && color.trim().toLowerCase() !== '#000' && color.trim().toLowerCase() !== 'black') {
                            return fallback || color;
                        }
                        return resolved;
                    } catch (e) {
                        return fallback || color;
                    }
                }

                // Dark mode fallback colors (used when palette has no color defined)
                var _isDarkMode = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
                var _defaultBg = _isDarkMode ? '#333333' : '#ffffff';
                var _defaultFg = _isDarkMode ? '#cccccc' : '#333333';

                // Function to update system bar colors via native bridge
                function updateSystemBarColors() {
                    var statusBarColor = resolveCssColor(getColour('page-background', _defaultBg), _defaultBg);
                    var navBarColor = resolveCssColor(getColour('tiddler-background', statusBarColor), statusBarColor);
                    var foregroundColor = resolveCssColor(getColour('foreground', _defaultFg), _defaultFg);

                    if (window.TiddlyDesktopAndroid && window.TiddlyDesktopAndroid.setSystemBarColors) {
                        window.TiddlyDesktopAndroid.setSystemBarColors(statusBarColor, navBarColor, foregroundColor);
                    }
                }

                // Wait for palette to be ready before updating colors
                function waitForPaletteAndUpdate(retries) {
                    retries = retries || 0;
                    if (retries > 100) {
                        // Give up after 5 seconds, use defaults
                        updateSystemBarColors();
                        return;
                    }
                    var paletteName = (${'$'}tw.wiki.getTiddlerText('${'$'}:/palette') || '').trim();
                    if (paletteName) {
                        var paletteTiddler = ${'$'}tw.wiki.getTiddler(paletteName);
                        // Check that palette exists AND has color content
                        if (paletteTiddler && paletteTiddler.fields.text && paletteTiddler.fields.text.indexOf(':') > 0) {
                            // Palette is ready with color definitions, update colors
                            updateSystemBarColors();
                            return;
                        }
                    }
                    // Palette not ready yet, try again
                    setTimeout(function() { waitForPaletteAndUpdate(retries + 1); }, 50);
                }

                // Set default colors immediately, then update when palette is ready
                updateSystemBarColors();
                // Then start checking for palette to get actual colors
                waitForPaletteAndUpdate(0);

                // Listen for palette changes
                ${'$'}tw.wiki.addEventListener('change', function(changes) {
                    // Check if the palette reference changed
                    if (changes['${'$'}:/palette']) {
                        updateSystemBarColors();
                        return;
                    }
                    // Check if the referenced palette tiddler itself changed
                    var paletteName = (${'$'}tw.wiki.getTiddlerText('${'$'}:/palette') || '').trim();
                    if (paletteName && changes[paletteName]) {
                        updateSystemBarColors();
                    }
                });
            })();
        """.trimIndent()

        // Script to handle tm-full-screen message for toggling fullscreen (status bar hide)
        val fullscreenScript = """
            (function() {
                // Stub the Fullscreen API on document.documentElement to prevent
                // TiddlyWiki's built-in tm-full-screen handler (rootwidget.js) from
                // calling requestFullscreen(), which triggers WebView's native
                // immersive mode — hiding both bars and breaking drag & drop.
                // Video fullscreen (onShowCustomView) uses a separate mechanism.
                var docEl = document.documentElement;
                if (docEl.requestFullscreen) {
                    docEl.requestFullscreen = function() {
                        console.log('[TiddlyDesktop] requestFullscreen blocked (handled natively)');
                        return Promise.resolve();
                    };
                }
                if (docEl.webkitRequestFullscreen) {
                    docEl.webkitRequestFullscreen = function() {
                        console.log('[TiddlyDesktop] webkitRequestFullscreen blocked (handled natively)');
                    };
                }
                // Stub exitFullscreen to exit video fullscreen (Plyr etc.) via Kotlin,
                // while also preventing TiddlyWiki's toggle logic from erroring
                if (document.exitFullscreen) {
                    document.exitFullscreen = function() {
                        console.log('[TiddlyDesktop] exitFullscreen — delegating to native');
                        if (window.TiddlyDesktopServer && window.TiddlyDesktopServer.exitVideoFullscreen) {
                            window.TiddlyDesktopServer.exitVideoFullscreen();
                        }
                        return Promise.resolve();
                    };
                }
                if (document.webkitExitFullscreen) {
                    document.webkitExitFullscreen = function() {
                        console.log('[TiddlyDesktop] webkitExitFullscreen — delegating to native');
                        if (window.TiddlyDesktopServer && window.TiddlyDesktopServer.exitVideoFullscreen) {
                            window.TiddlyDesktopServer.exitVideoFullscreen();
                        }
                    };
                }

                // Wait for TiddlyWiki to fully load (including after decryption for encrypted wikis)
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.rootWidget) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // Remove TiddlyWiki's built-in tm-full-screen handlers (from rootwidget.js)
                // which call requestFullscreen(). We handle fullscreen natively.
                if (${'$'}tw.rootWidget.eventListeners && ${'$'}tw.rootWidget.eventListeners['tm-full-screen']) {
                    ${'$'}tw.rootWidget.eventListeners['tm-full-screen'] = [];
                }

                // tm-full-screen handler - toggle status bar visibility
                ${'$'}tw.rootWidget.addEventListener('tm-full-screen', function(event) {
                    console.log('[TiddlyDesktop] tm-full-screen received');
                    try {
                        if (window.TiddlyDesktopServer && window.TiddlyDesktopServer.toggleFullscreen) {
                            var resultJson = window.TiddlyDesktopServer.toggleFullscreen();
                            var result = JSON.parse(resultJson);
                            if (result.success) {
                                console.log('[TiddlyDesktop] Fullscreen:', result.fullscreen);
                            } else {
                                console.error('[TiddlyDesktop] Failed to toggle fullscreen:', result.error);
                            }
                        } else {
                            console.warn('[TiddlyDesktop] toggleFullscreen not available');
                        }
                    } catch (e) {
                        console.error('[TiddlyDesktop] Error toggling fullscreen:', e);
                    }
                    return false;
                });

                console.log('[TiddlyDesktop] tm-full-screen handler installed (requestFullscreen blocked)');
            })();
        """.trimIndent()

        // Script to handle tm-print message
        val printScript = """
            (function() {
                // Wait for TiddlyWiki to fully load (including after decryption for encrypted wikis)
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.rootWidget) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // tm-print handler - use native print API
                ${'$'}tw.rootWidget.addEventListener('tm-print', function(event) {
                    console.log('[TiddlyDesktop] tm-print received');
                    try {
                        if (window.TiddlyDesktopPrint && window.TiddlyDesktopPrint.print) {
                            window.TiddlyDesktopPrint.print();
                        } else {
                            // Fallback to browser print if native interface not available
                            window.print();
                        }
                    } catch (e) {
                        console.error('[TiddlyDesktop] Error printing:', e);
                        // Fallback to browser print
                        window.print();
                    }
                    return false;
                });

                console.log('[TiddlyDesktop] tm-print handler installed');
            })();
        """.trimIndent()

        // Script to handle tm-open-external-window message (open URLs in external browser)
        val externalWindowScript = """
            (function() {
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.rootWidget) {
                    setTimeout(arguments.callee, 100);
                    return;
                }
                ${'$'}tw.rootWidget.addEventListener('tm-open-external-window', function(event) {
                    var url = event.paramObject ? event.paramObject.url : event.param;
                    console.log('[TiddlyDesktop] tm-open-external-window received:', url);
                    if (url && window.TiddlyDesktopExternal) {
                        window.TiddlyDesktopExternal.openExternal(url);
                    }
                    return false;
                });
                console.log('[TiddlyDesktop] tm-open-external-window handler installed');
            })();
        """.trimIndent()

        // Script to handle tm-open-window message (open tiddler in new window)
        // On Android WebView, window.open() causes a full page reload, so we:
        // 1. Monkey-patch dispatchEvent to intercept tm-open-window
        // 2. Render the tiddler as a full-screen overlay in the same $tw context
        //    (replicating what TW5's windows.js does but without window.open)
        // 3. Track overlays in a stack so the back button can close them
        // 4. Override window.open("","_blank") as safety net against page reload
        val openWindowScript = """
            (function() {
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.rootWidget) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // Find dispatchEvent in the prototype chain of rootWidget
                var proto = ${'$'}tw.rootWidget;
                while (proto && !proto.hasOwnProperty('dispatchEvent')) {
                    proto = Object.getPrototypeOf(proto);
                }
                if (!proto) {
                    console.error('[TiddlyDesktop] Could not find dispatchEvent in prototype chain');
                    return;
                }

                // Stack of open overlays (for back button)
                window.__tdOpenWindowStack = [];

                // Close the topmost overlay — called by Kotlin back handler
                window.__tdCloseLastOpenWindow = function() {
                    var entry = window.__tdOpenWindowStack[window.__tdOpenWindowStack.length - 1];
                    if (entry && ${'$'}tw.windows && ${'$'}tw.windows[entry.windowID] && ${'$'}tw.windows[entry.windowID].__tdOverlay) {
                        console.log('[TiddlyDesktop] Closing overlay:', entry.tiddler);
                        ${'$'}tw.windows[entry.windowID].close();
                        return true;
                    }
                    return false;
                };

                var DEFAULT_TEMPLATE = '${'$'}:/core/templates/single.tiddler.window';

                // Intercept tm-open-window at the dispatchEvent level
                var originalDispatch = proto.dispatchEvent;
                proto.dispatchEvent = function(event) {
                    if (event && event.type === 'tm-open-window') {
                        var title = event.param || event.tiddlerTitle || '';
                        var paramObject = event.paramObject || {};
                        var windowTitle = paramObject.windowTitle || title;
                        var windowID = paramObject.windowID || title;
                        var template = paramObject.template || DEFAULT_TEMPLATE;
                        var variables = ${'$'}tw.utils.extend({}, paramObject, {
                            currentTiddler: title,
                            'tv-window-id': windowID
                        });

                        console.log('[TiddlyDesktop] tm-open-window intercepted:', title);
                        if (!title) return true;

                        // If this window is already open, just bring it to front
                        if (${'$'}tw.windows && ${'$'}tw.windows[windowID] && ${'$'}tw.windows[windowID].__tdOverlay) {
                            return true;
                        }

                        // Use an iframe to create a separate document — exactly like
                        // windows.js uses window.open() to get a real <body> element.
                        // This ensures CSS selectors like "html body.tc-body.tc-single-tiddler-window"
                        // match correctly and PageStylesheet applies all styles naturally.
                        var iframe = document.createElement('iframe');
                        iframe.style.cssText = 'position:fixed;top:0;left:0;width:100%;height:100%;z-index:10000;border:none;';
                        document.body.appendChild(iframe);

                        var srcWindow = iframe.contentWindow;
                        var srcDocument = iframe.contentDocument;

                        // Initialise the document (same as windows.js)
                        srcDocument.write("<!DOCTYPE html><head></head><body class='tc-body tc-single-tiddler-window'></body></html>");
                        srcDocument.close();
                        srcDocument.title = windowTitle;

                        // Set up the styles (same as windows.js)
                        var styleWidgetNode = ${'$'}tw.wiki.makeTranscludeWidget('${'$'}:/core/ui/PageStylesheet', {
                            document: ${'$'}tw.fakeDocument,
                            variables: variables,
                            importPageMacros: true
                        });
                        var styleContainer = ${'$'}tw.fakeDocument.createElement('style');
                        styleWidgetNode.render(styleContainer, null);
                        var styleElement = srcDocument.createElement('style');
                        styleElement.innerHTML = styleContainer.textContent;
                        srcDocument.head.insertBefore(styleElement, srcDocument.head.firstChild);

                        // Render the tiddler using the template (same as windows.js)
                        var parser = ${'$'}tw.wiki.parseTiddler(template);
                        var widgetNode = ${'$'}tw.wiki.makeWidget(parser, {
                            document: srcDocument,
                            parentWidget: ${'$'}tw.rootWidget,
                            variables: variables
                        });
                        widgetNode.render(srcDocument.body, srcDocument.body.firstChild);

                        // Set up refresh handler so the iframe stays in sync
                        var refreshHandler = function(changes) {
                            if (styleWidgetNode.refresh(changes, styleContainer, null)) {
                                styleElement.innerHTML = styleContainer.textContent;
                            }
                            widgetNode.refresh(changes);
                        };
                        ${'$'}tw.wiki.addEventListener('change', refreshHandler);

                        // Listen for keyboard shortcuts (same as windows.js)
                        ${'$'}tw.utils.addEventListeners(srcDocument, [{
                            name: 'keydown',
                            handlerObject: ${'$'}tw.keyboardManager,
                            handlerMethod: 'handleKeydownEvent'
                        }]);
                        srcDocument.documentElement.addEventListener('click', ${'$'}tw.popup, true);

                        // Track in ${'$'}tw.windows (same as windows.js)
                        ${'$'}tw.windows = ${'$'}tw.windows || {};

                        var stackEntry = {
                            tiddler: title,
                            windowID: windowID,
                            iframe: iframe,
                            refreshHandler: refreshHandler
                        };

                        // Create window-like object that windows.js consumers expect
                        var fakeWin = {
                            __tdOverlay: true,
                            document: srcDocument,
                            close: function() {
                                ${'$'}tw.wiki.removeEventListener('change', stackEntry.refreshHandler);
                                delete ${'$'}tw.windows[windowID];
                                if (stackEntry.iframe.parentNode) stackEntry.iframe.parentNode.removeChild(stackEntry.iframe);
                                var idx = window.__tdOpenWindowStack.indexOf(stackEntry);
                                if (idx >= 0) window.__tdOpenWindowStack.splice(idx, 1);
                            },
                            focus: function() {},
                            addEventListener: function() {},
                            haveInitialisedWindow: true
                        };
                        ${'$'}tw.windows[windowID] = fakeWin;

                        // Push to stack for back button
                        window.__tdOpenWindowStack.push(stackEntry);

                        // Transform _canonical_uri URLs in the overlay iframe
                        if (window.__tdTransformElement) {
                            srcDocument.querySelectorAll('img, video, audio, source, object, embed, iframe').forEach(window.__tdTransformElement);
                            new MutationObserver(function(muts) {
                                muts.forEach(function(m) {
                                    m.addedNodes.forEach(function(n) {
                                        if (n.nodeType === 1) {
                                            window.__tdTransformElement(n);
                                            if (n.querySelectorAll) n.querySelectorAll('img, video, audio, source, object, embed, iframe').forEach(window.__tdTransformElement);
                                        }
                                    });
                                    if (m.type === 'attributes' && (m.attributeName === 'src' || m.attributeName === 'data')) {
                                        window.__tdTransformElement(m.target);
                                    }
                                });
                            }).observe(srcDocument.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['src', 'data'] });
                        }

                        // Inject media enhancement (Plyr + PDF.js) into the overlay iframe
                        (function(iDoc, iWin) {
                            var TD = '/_td/';
                            // Plyr CSS
                            var plyrCss = iDoc.createElement('link');
                            plyrCss.rel = 'stylesheet'; plyrCss.href = TD + 'plyr/dist/plyr.css';
                            iDoc.head.appendChild(plyrCss);
                            // Hide raw videos until Plyr loads
                            var hideStyle = iDoc.createElement('style');
                            hideStyle.textContent = 'video:not(.plyr__video-wrapper video){opacity:0!important;max-height:0!important;overflow:hidden!important}audio{max-width:100%;box-sizing:border-box;}.plyr{width:100%;height:100%;}.plyr__video-wrapper{width:100%!important;height:100%!important;padding-bottom:0!important;background:#000;}.plyr video{opacity:1!important;width:100%!important;height:100%!important;object-fit:contain!important;}.plyr--compact .plyr__control--overlaid{padding:10px!important;}.plyr--compact .plyr__control--overlaid svg{width:18px!important;height:18px!important;}.plyr--compact .plyr__time--duration,.plyr--compact [data-plyr="settings"],.plyr--compact .plyr__volume{display:none!important;}.plyr--compact .plyr__controls{padding:2px 5px!important;}.plyr--compact .plyr__control{padding:3px!important;}.plyr--compact .plyr__control svg{width:14px!important;height:14px!important;}.plyr--compact .plyr__progress__container{margin-left:4px!important;}.plyr--tiny .plyr__time,.plyr--tiny [data-plyr="fullscreen"]{display:none!important;}.plyr--tiny .plyr__control--overlaid{padding:6px!important;}.plyr--tiny .plyr__control--overlaid svg{width:14px!important;height:14px!important;}.plyr--tiny .plyr__control svg{width:12px!important;height:12px!important;}';
                            iDoc.head.appendChild(hideStyle);
                            // PDF viewer styles
                            var pdfStyle = iDoc.createElement('style');
                            pdfStyle.textContent = '.td-pdf-btn{background:#555;color:#fff;border:none;border-radius:3px;padding:4px 10px;font-size:14px;cursor:pointer;min-width:32px}.td-pdf-btn:active{background:#777}.td-pdf-page-wrap{display:flex;justify-content:center;padding:8px 0}.td-pdf-page-wrap canvas{background:#fff;box-shadow:0 2px 8px rgba(0,0,0,.3);display:block;max-width:none}';
                            iDoc.head.appendChild(pdfStyle);

                            var plyrOpts = {
                                controls: ['play-large','play','progress','current-time','duration','mute','volume','settings','fullscreen'],
                                settings: ['speed'],
                                speed: { selected: 1, options: [0.5, 0.75, 1, 1.25, 1.5, 2] },
                                iconUrl: TD + 'plyr/dist/plyr.svg',
                                blankVideo: ''
                            };

                            function applyPoster(el, posterUrl) {
                                el.setAttribute('poster', posterUrl);
                                var plyrContainer = el.closest('.plyr');
                                if (plyrContainer) {
                                    var posterDiv = plyrContainer.querySelector('.plyr__poster');
                                    if (posterDiv) {
                                        posterDiv.style.backgroundImage = 'url(' + posterUrl + ')';
                                        posterDiv.removeAttribute('hidden');
                                    }
                                }
                                if (el.plyr) el.plyr.poster = posterUrl;
                            }

                            function fitPlyrToParent(video) {
                                var plyrEl = video.closest('.plyr');
                                if (!plyrEl) return;
                                plyrEl.classList.remove('plyr--compact', 'plyr--tiny');
                                var w = plyrEl.clientWidth, h = plyrEl.clientHeight;
                                if (w < 350 || h < 250) plyrEl.classList.add('plyr--compact');
                                if (w < 200 || h < 150) plyrEl.classList.add('plyr--tiny');
                            }

                            function enhanceVideo(el) {
                                if (el.__tdPlyrDone || typeof iWin.Plyr === 'undefined') return;
                                el.__tdPlyrDone = true;
                                setTimeout(function() {
                                    var src = el.src || (el.querySelector('source') ? el.querySelector('source').src : null);
                                    el.setAttribute('preload', 'none');
                                    try {
                                        new iWin.Plyr(el, plyrOpts);
                                        if (el.videoWidth && el.videoHeight) {
                                            requestAnimationFrame(function() { fitPlyrToParent(el); });
                                        } else {
                                            el.addEventListener('loadedmetadata', function() {
                                                requestAnimationFrame(function() { fitPlyrToParent(el); });
                                            }, { once: true });
                                        }
                                    } catch(e) { console.warn('[TD-Plyr] overlay error:', e); }
                                    if (src && typeof TiddlyDesktopPoster !== 'undefined') {
                                        try {
                                            var url = new URL(src);
                                            var relPath = decodeURIComponent(url.pathname).replace(/^\/_relative\//, '');
                                            var posterUrl = TiddlyDesktopPoster.getPoster(relPath);
                                            if (posterUrl) applyPoster(el, posterUrl);
                                            var vttText = TiddlyDesktopPoster.getThumbnails(relPath);
                                            if (vttText && el.plyr) {
                                                var vttBlob = new Blob([vttText], { type: 'text/vtt' });
                                                el.plyr.config.previewThumbnails = { enabled: true, src: URL.createObjectURL(vttBlob) };
                                            }
                                        } catch(e) { console.warn('[TD-Plyr] overlay poster/thumbnails failed:', e.message); }
                                    }
                                }, 50);
                            }

                            function getPdfSrc(el) {
                                var tag = el.tagName.toLowerCase();
                                var src = el.getAttribute('src') || el.getAttribute('data') || '';
                                if (tag === 'object') src = el.getAttribute('data') || src;
                                if (!src) return null;
                                if (src.toLowerCase().indexOf('.pdf') === -1 &&
                                    (el.getAttribute('type') || '').toLowerCase() !== 'application/pdf') return null;
                                return src;
                            }

                            function replacePdfElement(el) {
                                if (el.__tdPdfDone) return;
                                el.__tdPdfDone = true;
                                var src = getPdfSrc(el);
                                if (!src || typeof iWin.pdfjsLib === 'undefined') { el.__tdPdfDone = false; return; }
                                var container = iDoc.createElement('div');
                                container.style.cssText = 'width:100%;max-width:100%;overflow:auto;background:#525659;padding:8px 0;border-radius:4px;position:relative';
                                var toolbar = iDoc.createElement('div');
                                toolbar.style.cssText = 'display:flex;align-items:center;justify-content:center;gap:8px;padding:6px 8px;background:#333;color:#fff;font:13px sans-serif;border-radius:4px 4px 0 0;flex-wrap:wrap;position:sticky;top:0;z-index:10';
                                toolbar.innerHTML = '<button class="td-pdf-btn" data-action="prev">&#9664;</button><span class="td-pdf-pageinfo">- / -</span><button class="td-pdf-btn" data-action="next">&#9654;</button><span style="margin:0 4px">|</span><button class="td-pdf-btn" data-action="zoomout">&#8722;</button><button class="td-pdf-btn" data-action="fitwidth">Fit</button><button class="td-pdf-btn" data-action="zoomin">&#43;</button>';
                                container.appendChild(toolbar);
                                var pagesWrap = iDoc.createElement('div');
                                pagesWrap.style.cssText = 'max-height:80vh;overflow-y:auto;-webkit-overflow-scrolling:touch';
                                container.appendChild(pagesWrap);
                                el.parentNode.replaceChild(container, el);
                                var scale = 1.5, pdfDoc = null, pageCanvases = [];
                                var pageInfo = toolbar.querySelector('.td-pdf-pageinfo');
                                function renderPage(num, canvas) {
                                    pdfDoc.getPage(num).then(function(page) {
                                        var vp = page.getViewport({ scale: scale });
                                        canvas.width = vp.width; canvas.height = vp.height;
                                        page.render({ canvasContext: canvas.getContext('2d'), viewport: vp });
                                    });
                                }
                                function renderAll() { pageCanvases.forEach(function(c, i) { renderPage(i + 1, c); }); }
                                function fitWidth() {
                                    if (!pdfDoc) return;
                                    pdfDoc.getPage(1).then(function(page) {
                                        var vp = page.getViewport({ scale: 1 });
                                        scale = Math.max(0.5, Math.min(5, (pagesWrap.clientWidth - 16) / vp.width));
                                        renderAll();
                                    });
                                }
                                iWin.pdfjsLib.getDocument({ url: src, cMapUrl: TD + 'pdfjs/cmaps/', cMapPacked: true }).promise.then(function(pdf) {
                                    pdfDoc = pdf;
                                    pageInfo.textContent = pdf.numPages + ' page' + (pdf.numPages !== 1 ? 's' : '');
                                    for (var p = 1; p <= pdf.numPages; p++) {
                                        var wrap = iDoc.createElement('div');
                                        wrap.className = 'td-pdf-page-wrap';
                                        var canvas = iDoc.createElement('canvas');
                                        wrap.appendChild(canvas); pagesWrap.appendChild(wrap);
                                        pageCanvases.push(canvas);
                                    }
                                    fitWidth();
                                }).catch(function(err) {
                                    pagesWrap.innerHTML = '<p style="color:#f88;padding:20px;text-align:center">Failed to load PDF: ' + err.message + '</p>';
                                });
                                toolbar.addEventListener('click', function(e) {
                                    var btn = e.target.closest('[data-action]');
                                    if (!btn) return;
                                    var a = btn.getAttribute('data-action');
                                    if (a === 'zoomin') { scale = Math.min(scale * 1.25, 5); renderAll(); }
                                    else if (a === 'zoomout') { scale = Math.max(scale / 1.25, 0.5); renderAll(); }
                                    else if (a === 'fitwidth') fitWidth();
                                    else if (a === 'prev') pagesWrap.scrollBy(0, -pagesWrap.clientHeight * 0.8);
                                    else if (a === 'next') pagesWrap.scrollBy(0, pagesWrap.clientHeight * 0.8);
                                });
                            }

                            function scanAll() {
                                if (typeof iWin.Plyr !== 'undefined') iDoc.querySelectorAll('video').forEach(enhanceVideo);
                                if (typeof iWin.pdfjsLib !== 'undefined') iDoc.querySelectorAll('embed, object, iframe').forEach(function(el) { if (getPdfSrc(el)) replacePdfElement(el); });
                            }

                            // Load Plyr JS
                            var plyrJs = iDoc.createElement('script');
                            plyrJs.src = TD + 'plyr/dist/plyr.min.js';
                            plyrJs.onload = function() { scanAll(); };
                            iDoc.head.appendChild(plyrJs);

                            // Load PDF.js
                            var pdfJs = iDoc.createElement('script');
                            pdfJs.src = TD + 'pdfjs/build/pdf.min.js';
                            pdfJs.onload = function() {
                                if (iWin.pdfjsLib) {
                                    iWin.pdfjsLib.GlobalWorkerOptions.workerSrc = TD + 'pdfjs/build/pdf.worker.min.js';
                                    scanAll();
                                }
                            };
                            iDoc.head.appendChild(pdfJs);

                            // Watch for dynamically added elements
                            new MutationObserver(function(muts) {
                                var needScan = false;
                                muts.forEach(function(m) { m.addedNodes.forEach(function(n) { if (n.nodeType === 1) needScan = true; }); });
                                if (needScan) scanAll();
                            }).observe(iDoc.body, { childList: true, subtree: true });
                        })(srcDocument, srcWindow);

                        return true;
                    }

                    // Also intercept tm-close-window to close overlays
                    if (event && event.type === 'tm-close-window') {
                        var closeID = (event.paramObject && event.paramObject.windowID) || event.param || '';
                        if (closeID && ${'$'}tw.windows && ${'$'}tw.windows[closeID] && ${'$'}tw.windows[closeID].__tdOverlay) {
                            ${'$'}tw.windows[closeID].close();
                            return true;
                        }
                    }

                    return originalDispatch.apply(this, arguments);
                };

                // Safety net: block window.open("","_blank") to prevent page reload
                var originalOpen = window.open;
                window.open = function(url, target, features) {
                    if (url === '' && target === '_blank') {
                        console.log('[TiddlyDesktop] window.open("","_blank") blocked');
                        return null;
                    }
                    return originalOpen.apply(window, arguments);
                };

                console.log('[TiddlyDesktop] tm-open-window handler installed (overlay mode)');
            })();
        """.trimIndent()

        // Script to extract and save favicon from TiddlyWiki
        val faviconScript = """
            (function() {
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.wiki) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // Check if we already extracted favicon for this session
                if (window.__faviconExtracted) {
                    return;
                }
                window.__faviconExtracted = true;

                function saveFaviconData(base64Data, mimeType) {
                    if (!base64Data || !window.TiddlyDesktopFavicon) return;
                    // Remove any data URL prefix if present
                    if (base64Data.indexOf('base64,') !== -1) {
                        base64Data = base64Data.split('base64,')[1];
                    }
                    // Clean up whitespace
                    base64Data = base64Data.replace(/\s/g, '');
                    if (!base64Data) return;
                    window.TiddlyDesktopFavicon.saveFavicon(base64Data, mimeType || 'image/x-icon');
                }

                // Detect client-server mode (folder wikis use TW syncer)
                var isClientServer = !!${'$'}tw.syncer;

                // In client-server mode, always use HTTP fetch because:
                // 1. Binary tiddlers are "skinny" (no text loaded in browser)
                // 2. The server merges tiddlers from includeWikis, so only it has the real favicon
                // 3. getTiddler would return the shadow (default) favicon, not the actual one
                if (!isClientServer) {
                    // Single-file wikis: get favicon from tiddler store (all data is in-memory)
                    var faviconTiddler = ${'$'}tw.wiki.getTiddler('${'$'}:/favicon.ico');
                    if (faviconTiddler) {
                        var faviconText = faviconTiddler.fields.text || '';
                        var faviconType = faviconTiddler.fields.type || 'image/x-icon';

                        if (faviconText) {
                            saveFaviconData(faviconText, faviconType);
                            return;
                        }
                    }
                }

                // Fetch /favicon.ico from server (folder wikis in client-server mode,
                // or single-file wikis where tiddler text was empty)
                console.log('[TiddlyDesktop] Fetching favicon via HTTP' + (isClientServer ? ' (client-server mode)' : ''));
                function tryLocalFaviconFallback() {
                    if (window.TiddlyDesktopFavicon && window.TiddlyDesktopFavicon.extractFaviconFromLocalCopy) {
                        console.log('[TiddlyDesktop] Trying local wiki-mirrors copy');
                        var result = window.TiddlyDesktopFavicon.extractFaviconFromLocalCopy();
                        console.log('[TiddlyDesktop] Local favicon result:', result);
                    }
                }

                fetch(window.location.origin + '/favicon.ico')
                    .then(function(response) {
                        if (!response.ok) throw new Error('HTTP ' + response.status);
                        var contentType = response.headers.get('content-type') || 'image/x-icon';
                        return response.blob().then(function(blob) {
                            // TW server returns 200 with empty body when no favicon tiddler exists
                            if (blob.size === 0) throw new Error('Empty response');
                            return { blob: blob, type: contentType };
                        });
                    })
                    .then(function(result) {
                        var reader = new FileReader();
                        reader.onload = function() {
                            // reader.result is a data URL like "data:image/x-icon;base64,..."
                            var data = reader.result;
                            if (data && data.indexOf('base64,') !== -1 && data.split('base64,')[1]) {
                                saveFaviconData(data, result.type);
                            } else {
                                console.log('[TiddlyDesktop] HTTP favicon data empty after decode');
                                tryLocalFaviconFallback();
                            }
                        };
                        reader.readAsDataURL(result.blob);
                    })
                    .catch(function(err) {
                        console.log('[TiddlyDesktop] HTTP favicon fetch failed:', err.message);
                        tryLocalFaviconFallback();
                    });

                // Watch for changes to $/favicon.ico and re-extract
                (function watchFaviconChanges() {
                    if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.wiki || !${'$'}tw.wiki.addEventListener) {
                        setTimeout(watchFaviconChanges, 200);
                        return;
                    }
                    ${'$'}tw.wiki.addEventListener('change', function(changes) {
                        if (!changes['${'$'}:/favicon.ico']) return;
                        console.log('[TiddlyDesktop] Favicon tiddler changed, re-extracting');
                        var isClientServer = !!${'$'}tw.syncer;
                        if (!isClientServer) {
                            var t = ${'$'}tw.wiki.getTiddler('${'$'}:/favicon.ico');
                            if (t && t.fields.text) {
                                saveFaviconData(t.fields.text, t.fields.type || 'image/x-icon');
                            }
                        } else {
                            fetch(window.location.origin + '/favicon.ico')
                                .then(function(response) {
                                    if (!response.ok) throw new Error('HTTP ' + response.status);
                                    var contentType = response.headers.get('content-type') || 'image/x-icon';
                                    return response.blob().then(function(blob) {
                                        if (blob.size === 0) throw new Error('Empty response');
                                        return { blob: blob, type: contentType };
                                    });
                                })
                                .then(function(result) {
                                    var reader = new FileReader();
                                    reader.onload = function() {
                                        var data = reader.result;
                                        if (data && data.indexOf('base64,') !== -1 && data.split('base64,')[1]) {
                                            saveFaviconData(data, result.type);
                                        }
                                    };
                                    reader.readAsDataURL(result.blob);
                                })
                                .catch(function(err) {
                                    console.log('[TiddlyDesktop] Favicon re-fetch failed:', err.message);
                                });
                        }
                    });
                })();
            })();
        """.trimIndent()

        // Script to handle file exports/downloads
        val exportScript = """
            (function() {
                // Callback storage for async export results
                window.__exportCallbacks = window.__exportCallbacks || {};
                var callbackCounter = 0;

                // Helper to generate unique callback ID
                function generateCallbackId() {
                    return 'export_' + (++callbackCounter) + '_' + Date.now();
                }

                // Helper to convert blob to base64
                function blobToBase64(blob) {
                    return new Promise(function(resolve, reject) {
                        var reader = new FileReader();
                        reader.onloadend = function() {
                            // Remove data URL prefix to get just base64
                            var base64 = reader.result.split(',')[1] || '';
                            resolve(base64);
                        };
                        reader.onerror = reject;
                        reader.readAsDataURL(blob);
                    });
                }

                // Helper to extract filename from Content-Disposition or URL
                function extractFilename(url, contentDisposition) {
                    if (contentDisposition) {
                        var match = contentDisposition.match(/filename[^;=\n]*=((['"]).*?\2|[^;\n]*)/);
                        if (match && match[1]) {
                            return match[1].replace(/['"]/g, '');
                        }
                    }
                    // Try to get from URL
                    try {
                        var urlObj = new URL(url, window.location.href);
                        var pathname = urlObj.pathname;
                        var filename = pathname.substring(pathname.lastIndexOf('/') + 1);
                        if (filename) return decodeURIComponent(filename);
                    } catch (e) {}
                    return 'download';
                }

                // Helper to get MIME type
                function getMimeType(filename, blob) {
                    if (blob && blob.type) return blob.type;
                    var ext = filename.split('.').pop().toLowerCase();
                    var mimeTypes = {
                        // Text/markup
                        'json': 'application/json',
                        'html': 'text/html',
                        'htm': 'text/html',
                        'txt': 'text/plain',
                        'tid': 'text/vnd.tiddlywiki',
                        'css': 'text/css',
                        'js': 'application/javascript',
                        'xml': 'application/xml',
                        'csv': 'text/csv',
                        'md': 'text/markdown',
                        // Images
                        'png': 'image/png',
                        'jpg': 'image/jpeg',
                        'jpeg': 'image/jpeg',
                        'gif': 'image/gif',
                        'svg': 'image/svg+xml',
                        'webp': 'image/webp',
                        'bmp': 'image/bmp',
                        'ico': 'image/x-icon',
                        'heic': 'image/heic',
                        'heif': 'image/heif',
                        // Audio
                        'mp3': 'audio/mpeg',
                        'm4a': 'audio/mp4',
                        'aac': 'audio/aac',
                        'ogg': 'audio/ogg',
                        'oga': 'audio/ogg',
                        'opus': 'audio/opus',
                        'wav': 'audio/wav',
                        'flac': 'audio/flac',
                        'aiff': 'audio/aiff',
                        'aif': 'audio/aiff',
                        // Video
                        'mp4': 'video/mp4',
                        'm4v': 'video/mp4',
                        'webm': 'video/webm',
                        'ogv': 'video/ogg',
                        'mov': 'video/quicktime',
                        'avi': 'video/x-msvideo',
                        'mkv': 'video/x-matroska',
                        '3gp': 'video/3gpp',
                        // Documents
                        'pdf': 'application/pdf',
                        // Archives
                        'zip': 'application/zip'
                    };
                    return mimeTypes[ext] || 'application/octet-stream';
                }

                // Intercept anchor click downloads
                document.addEventListener('click', function(event) {
                    var anchor = event.target.closest('a[download]');
                    if (!anchor) return;

                    var href = anchor.getAttribute('href');
                    var downloadAttr = anchor.getAttribute('download');

                    if (!href) return;

                    // Handle blob: URLs
                    if (href.startsWith('blob:')) {
                        event.preventDefault();
                        event.stopPropagation();

                        console.log('[TiddlyDesktop Export] Intercepted blob download:', downloadAttr);

                        fetch(href)
                            .then(function(response) { return response.blob(); })
                            .then(function(blob) {
                                var filename = downloadAttr || 'download';
                                var mimeType = getMimeType(filename, blob);
                                return blobToBase64(blob).then(function(base64) {
                                    var callbackId = generateCallbackId();
                                    window.__exportCallbacks[callbackId] = function(success, message) {
                                        console.log('[TiddlyDesktop Export] Save result:', success, message);
                                    };
                                    window.TiddlyDesktopExport.saveFile(filename, mimeType, base64, callbackId);
                                });
                            })
                            .catch(function(error) {
                                console.error('[TiddlyDesktop Export] Error:', error);
                            });
                        return;
                    }

                    // Handle data: URLs
                    if (href.startsWith('data:')) {
                        event.preventDefault();
                        event.stopPropagation();

                        console.log('[TiddlyDesktop Export] Intercepted data URL download:', downloadAttr);

                        var filename = downloadAttr || 'download';
                        // Parse data URL: data:[<mediatype>][;base64],<data>
                        var match = href.match(/^data:([^;,]+)?(?:;base64)?,(.*)$/);
                        if (match) {
                            var mimeType = match[1] || getMimeType(filename, null);
                            var data = match[2];
                            var isBase64 = href.indexOf(';base64,') !== -1;

                            if (isBase64) {
                                var callbackId = generateCallbackId();
                                window.__exportCallbacks[callbackId] = function(success, message) {
                                    console.log('[TiddlyDesktop Export] Save result:', success, message);
                                };
                                window.TiddlyDesktopExport.saveFile(filename, mimeType, data, callbackId);
                            } else {
                                // URL-encoded data - decode and convert to base64
                                try {
                                    var decoded = decodeURIComponent(data);
                                    var callbackId = generateCallbackId();
                                    window.__exportCallbacks[callbackId] = function(success, message) {
                                        console.log('[TiddlyDesktop Export] Save result:', success, message);
                                    };
                                    window.TiddlyDesktopExport.saveTextFile(filename, mimeType, decoded, callbackId);
                                } catch (e) {
                                    console.error('[TiddlyDesktop Export] Failed to decode data URL:', e);
                                }
                            }
                        }
                        return;
                    }
                }, true);

                // Also intercept programmatic downloads via createElement('a').click()
                var originalCreateElement = document.createElement.bind(document);
                document.createElement = function(tagName) {
                    var element = originalCreateElement(tagName);

                    if (tagName.toLowerCase() === 'a') {
                        var originalClick = element.click.bind(element);
                        element.click = function() {
                            var href = element.getAttribute('href');
                            var download = element.getAttribute('download');

                            if (download && href && (href.startsWith('blob:') || href.startsWith('data:'))) {
                                console.log('[TiddlyDesktop Export] Intercepted programmatic download:', download);

                                // Create and dispatch a fake click event that our listener will catch
                                var event = new MouseEvent('click', {
                                    bubbles: true,
                                    cancelable: true,
                                    view: window
                                });
                                element.dispatchEvent(event);
                                return;
                            }

                            return originalClick();
                        };
                    }

                    return element;
                };

                console.log('[TiddlyDesktop] Export handler installed');
            })();
        """.trimIndent()

        // Inline PDF.js renderer and Plyr video enhancement script
        val inlineMediaScript = """
            (function() {
                var TD_BASE = '/_td/';
                var pdfjsLoaded = false;
                var plyrLoaded = false;

                // ---- Helper: dynamically load a script ----
                function loadScript(src, cb) {
                    var s = document.createElement('script');
                    s.src = src;
                    s.onload = cb || function(){};
                    s.onerror = function(){ console.error('[TD] Failed to load ' + src); };
                    document.head.appendChild(s);
                }

                // ---- Helper: dynamically load CSS ----
                function loadCSS(href) {
                    var l = document.createElement('link');
                    l.rel = 'stylesheet';
                    l.href = href;
                    document.head.appendChild(l);
                }

                // ---- PDF.js inline renderer ----
                function getPdfSrc(el) {
                    var tag = el.tagName.toLowerCase();
                    var src = el.getAttribute('src') || el.getAttribute('data') || '';
                    if (tag === 'object') src = el.getAttribute('data') || src;
                    if (!src) return null;
                    // Only handle PDF sources
                    if (src.toLowerCase().indexOf('.pdf') === -1 &&
                        (el.getAttribute('type') || '').toLowerCase() !== 'application/pdf') return null;
                    return src;
                }

                function replacePdfElement(el) {
                    if (el.__tdPdfDone) return;
                    el.__tdPdfDone = true;
                    var src = getPdfSrc(el);
                    if (!src) return;

                    // Verify pdfjsLib is available
                    if (typeof pdfjsLib === 'undefined' || !pdfjsLib.getDocument) {
                        console.warn('[TD-PDF] PDF.js not ready, skipping:', src);
                        el.__tdPdfDone = false; // Allow retry
                        return;
                    }

                    console.log('[TD-PDF] Replacing PDF element:', src);

                    var container = document.createElement('div');
                    container.className = 'td-pdf-container';
                    container.style.cssText = 'width:100%;max-width:100%;overflow:auto;background:#525659;padding:8px 0;border-radius:4px;position:relative;';

                    // Toolbar
                    var toolbar = document.createElement('div');
                    toolbar.style.cssText = 'display:flex;align-items:center;justify-content:center;gap:8px;padding:6px 8px;background:#333;color:#fff;font:13px sans-serif;border-radius:4px 4px 0 0;flex-wrap:wrap;position:sticky;top:0;z-index:10;';
                    toolbar.innerHTML =
                        '<button class="td-pdf-btn" data-action="prev" title="Previous page">&#9664;</button>' +
                        '<span class="td-pdf-pageinfo">- / -</span>' +
                        '<button class="td-pdf-btn" data-action="next" title="Next page">&#9654;</button>' +
                        '<span style="margin:0 4px">|</span>' +
                        '<button class="td-pdf-btn" data-action="zoomout" title="Zoom out">&#8722;</button>' +
                        '<button class="td-pdf-btn" data-action="fitwidth" title="Fit width">Fit</button>' +
                        '<button class="td-pdf-btn" data-action="zoomin" title="Zoom in">&#43;</button>';
                    container.appendChild(toolbar);

                    // Style toolbar buttons
                    var style = document.createElement('style');
                    if (!document.querySelector('#td-pdf-styles')) {
                        style.id = 'td-pdf-styles';
                        style.textContent = '.td-pdf-btn{background:#555;color:#fff;border:none;border-radius:3px;padding:4px 10px;font-size:14px;cursor:pointer;min-width:32px;}.td-pdf-btn:active{background:#777;}.td-pdf-pages-wrap{overflow-y:auto;-webkit-overflow-scrolling:touch;}.td-pdf-page-wrap{display:flex;justify-content:center;padding:8px 0;}.td-pdf-page-wrap canvas{background:#fff;box-shadow:0 2px 8px rgba(0,0,0,.3);display:block;max-width:none;}';
                        document.head.appendChild(style);
                    }

                    // Scrollable page area
                    var pagesWrap = document.createElement('div');
                    pagesWrap.className = 'td-pdf-pages-wrap';
                    pagesWrap.style.cssText = 'max-height:80vh;overflow-y:auto;-webkit-overflow-scrolling:touch;';
                    container.appendChild(pagesWrap);

                    el.parentNode.replaceChild(container, el);

                    // Load PDF
                    var scale = 1.5;
                    var pdfDoc = null;
                    var pageCanvases = [];
                    var pageInfo = toolbar.querySelector('.td-pdf-pageinfo');

                    function renderPage(num, canvas) {
                        pdfDoc.getPage(num).then(function(page) {
                            var viewport = page.getViewport({ scale: scale });
                            canvas.width = viewport.width;
                            canvas.height = viewport.height;
                            var ctx = canvas.getContext('2d');
                            page.render({ canvasContext: ctx, viewport: viewport });
                        });
                    }

                    function renderAll() {
                        pageCanvases.forEach(function(c, i) { renderPage(i + 1, c); });
                    }

                    function fitWidth() {
                        if (!pdfDoc) return;
                        pdfDoc.getPage(1).then(function(page) {
                            var vp = page.getViewport({ scale: 1 });
                            var containerWidth = pagesWrap.clientWidth - 16;
                            scale = containerWidth / vp.width;
                            if (scale < 0.5) scale = 0.5;
                            if (scale > 5) scale = 5;
                            renderAll();
                        });
                    }

                    pdfjsLib.getDocument({ url: src, cMapUrl: TD_BASE + 'pdfjs/cmaps/', cMapPacked: true }).promise.then(function(pdf) {
                        pdfDoc = pdf;
                        var total = pdf.numPages;
                        pageInfo.textContent = total + ' page' + (total !== 1 ? 's' : '');

                        for (var p = 1; p <= total; p++) {
                            var wrap = document.createElement('div');
                            wrap.className = 'td-pdf-page-wrap';
                            var canvas = document.createElement('canvas');
                            wrap.appendChild(canvas);
                            pagesWrap.appendChild(wrap);
                            pageCanvases.push(canvas);
                        }

                        // Initial fit-width render
                        fitWidth();

                        // Use IntersectionObserver for lazy rendering
                        if (typeof IntersectionObserver !== 'undefined') {
                            var observer = new IntersectionObserver(function(entries) {
                                entries.forEach(function(entry) {
                                    if (entry.isIntersecting) {
                                        var idx = pageCanvases.indexOf(entry.target);
                                        if (idx >= 0) renderPage(idx + 1, entry.target);
                                    }
                                });
                            }, { root: pagesWrap, rootMargin: '200px' });
                            pageCanvases.forEach(function(c) { observer.observe(c); });
                        }
                    }).catch(function(err) {
                        console.error('[TD-PDF] Error loading PDF:', err);
                        pagesWrap.innerHTML = '<p style="color:#f88;padding:20px;text-align:center;">Failed to load PDF: ' + err.message + '</p>';
                    });

                    // Toolbar actions
                    toolbar.addEventListener('click', function(e) {
                        var btn = e.target.closest('[data-action]');
                        if (!btn) return;
                        var action = btn.getAttribute('data-action');
                        if (action === 'zoomin') { scale = Math.min(scale * 1.25, 5); renderAll(); }
                        else if (action === 'zoomout') { scale = Math.max(scale / 1.25, 0.5); renderAll(); }
                        else if (action === 'fitwidth') { fitWidth(); }
                        else if (action === 'prev') { pagesWrap.scrollBy(0, -pagesWrap.clientHeight * 0.8); }
                        else if (action === 'next') { pagesWrap.scrollBy(0, pagesWrap.clientHeight * 0.8); }
                    });
                }

                // ---- Plyr video enhancement ----
                function applyPoster(el, posterUrl) {
                    // Set on the video element
                    el.setAttribute('poster', posterUrl);
                    // Update Plyr's poster overlay directly
                    var plyrContainer = el.closest('.plyr');
                    if (plyrContainer) {
                        var posterDiv = plyrContainer.querySelector('.plyr__poster');
                        if (posterDiv) {
                            posterDiv.style.backgroundImage = 'url(' + posterUrl + ')';
                            posterDiv.removeAttribute('hidden');
                        }
                    }
                    if (el.plyr) el.plyr.poster = posterUrl;
                    console.log('[TD-Plyr] Applied poster');
                }

                // Queue for sequential video processing (poster + thumbnails)
                var videoQueue = [];
                var videoQueueRunning = false;
                function enqueueVideoWork(work) {
                    videoQueue.push(work);
                    if (!videoQueueRunning) {
                        // Delay first drain to let page finish initial resource loading
                        videoQueueRunning = true;
                        setTimeout(drainVideoQueue, 2000);
                    }
                }
                function drainVideoQueue() {
                    if (videoQueue.length === 0) { videoQueueRunning = false; return; }
                    videoQueueRunning = true;
                    var task = videoQueue.shift();
                    task(function() { drainVideoQueue(); });
                }

                function fitPlyrToParent(video) {
                    var plyrEl = video.closest('.plyr');
                    if (!plyrEl) return;
                    plyrEl.classList.remove('plyr--compact', 'plyr--tiny');
                    var w = plyrEl.clientWidth, h = plyrEl.clientHeight;
                    if (w < 350 || h < 250) plyrEl.classList.add('plyr--compact');
                    if (w < 200 || h < 150) plyrEl.classList.add('plyr--tiny');
                }

                function enhanceVideo(el) {
                    if (el.__tdPlyrDone) return;
                    el.__tdPlyrDone = true;

                    // Verify Plyr is available
                    if (typeof Plyr === 'undefined') {
                        console.warn('[TD-Plyr] Plyr not ready, skipping video');
                        el.__tdPlyrDone = false; // Allow retry
                        return;
                    }

                    // Small delay to let URL-transform complete
                    setTimeout(function() {
                        var videoSrc = el.src || (el.querySelector('source') ? el.querySelector('source').src : null);
                        console.log('[TD-Plyr] enhanceVideo videoSrc:', videoSrc);

                        function initPlyr(vttUrl) {
                            try {
                                var opts = {
                                    controls: ['play-large','play','progress','current-time','duration','mute','volume','settings','fullscreen'],
                                    settings: ['speed'],
                                    speed: { selected: 1, options: [0.5, 0.75, 1, 1.25, 1.5, 2] },
                                    iconUrl: TD_BASE + 'plyr/dist/plyr.svg',
                                    blankVideo: ''
                                };
                                if (vttUrl) {
                                    opts.previewThumbnails = { enabled: true, src: vttUrl };
                                }
                                var player = new Plyr(el, opts);

                                // Fit within parent constraints
                                if (el.videoWidth && el.videoHeight) {
                                    requestAnimationFrame(function() { fitPlyrToParent(el); });
                                } else {
                                    el.addEventListener('loadedmetadata', function() {
                                        requestAnimationFrame(function() { fitPlyrToParent(el); });
                                    }, { once: true });
                                }

                                console.log('[TD-Plyr] Enhanced video:', videoSrc || '(no src)', vttUrl ? '(with thumbnails)' : '');
                            } catch(err) {
                                console.error('[TD-Plyr] Error:', err && err.message ? err.message : JSON.stringify(err));
                            }
                        }

                        // Prevent Plyr from preloading video data
                        el.setAttribute('preload', 'none');

                        // Initialize Plyr immediately
                        initPlyr(null);

                        // Queue poster extraction (native) + thumbnail generation
                        if (videoSrc) {
                            enqueueVideoWork(function(done) {
                                try {
                                    var url = new URL(videoSrc);
                                    var pathPart = decodeURIComponent(url.pathname);
                                    var relPath = pathPart.replace(/^\/_relative\//, '');

                                    if (typeof TiddlyDesktopPoster !== 'undefined') {
                                        // Extract poster via native MediaMetadataRetriever
                                        var posterUrl = TiddlyDesktopPoster.getPoster(relPath);
                                        if (posterUrl) {
                                            applyPoster(el, posterUrl);
                                        }

                                        // Generate thumbnails natively
                                        var vttText = TiddlyDesktopPoster.getThumbnails(relPath);
                                        if (vttText && el.plyr) {
                                            var vttBlob = new Blob([vttText], { type: 'text/vtt' });
                                            var vttUrl = URL.createObjectURL(vttBlob);
                                            try {
                                                el.plyr.config.previewThumbnails = { enabled: true, src: vttUrl };
                                                console.log('[TD-Plyr] Added thumbnails to player');
                                            } catch(e) {
                                                console.warn('[TD-Plyr] Could not add thumbnails:', e);
                                            }
                                        }
                                    }
                                } catch(e) {
                                    console.warn('[TD-Plyr] Native poster/thumbnails failed:', e.message);
                                }
                                done();
                            });
                        }
                    }, 50);
                }

                // ---- Scan and enhance existing elements ----
                function scanAll() {
                    // Only process PDFs if PDF.js is loaded
                    if (pdfjsLoaded && typeof pdfjsLib !== 'undefined') {
                        document.querySelectorAll('embed, object, iframe').forEach(function(el) {
                            if (getPdfSrc(el)) replacePdfElement(el);
                        });
                    }
                    // Only process videos if Plyr is loaded
                    if (plyrLoaded && typeof Plyr !== 'undefined') {
                        document.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
                    }
                }

                // ---- MutationObserver to catch dynamically added elements ----
                if (!window.__tdObserverSet) {
                    window.__tdObserverSet = true;
                    var obs = new MutationObserver(function(mutations) {
                        mutations.forEach(function(m) {
                            m.addedNodes.forEach(function(node) {
                                if (node.nodeType !== 1) return;
                                // Check the node itself
                                var tag = node.tagName ? node.tagName.toLowerCase() : '';
                                if ((tag === 'embed' || tag === 'object' || tag === 'iframe') && getPdfSrc(node)) {
                                    if (pdfjsLoaded && typeof pdfjsLib !== 'undefined') {
                                        replacePdfElement(node);
                                    }
                                } else if (tag === 'video') {
                                    if (plyrLoaded && typeof Plyr !== 'undefined') {
                                        enhanceVideo(node);
                                    }
                                }
                                // Check children
                                if (node.querySelectorAll) {
                                    if (pdfjsLoaded && typeof pdfjsLib !== 'undefined') {
                                        node.querySelectorAll('embed, object, iframe').forEach(function(el) {
                                            if (getPdfSrc(el)) replacePdfElement(el);
                                        });
                                    }
                                    if (plyrLoaded && typeof Plyr !== 'undefined') {
                                        node.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
                                    }
                                }
                            });
                        });
                    });
                    obs.observe(document.body, { childList: true, subtree: true });

                    // Re-scan on pageshow (back/forward navigation)
                    window.addEventListener('pageshow', function(event) {
                        if (event.persisted) {
                            console.log('[TiddlyDesktop] Page restored from bfcache, re-scanning');
                            scanAll();
                        }
                    });
                }

                // ---- Load libraries then scan ----
                function loadPdfJs() {
                    if (pdfjsLoaded || typeof pdfjsLib !== 'undefined') {
                        pdfjsLoaded = true;
                        scanAll();
                        return;
                    }
                    loadScript(TD_BASE + 'pdfjs/build/pdf.min.js', function() {
                        if (typeof pdfjsLib !== 'undefined') {
                            pdfjsLib.GlobalWorkerOptions.workerSrc = TD_BASE + 'pdfjs/build/pdf.worker.min.js';
                            pdfjsLoaded = true;
                            console.log('[TD-PDF] PDF.js loaded');
                            scanAll();
                        }
                    });
                }

                function loadPlyr() {
                    if (plyrLoaded || typeof Plyr !== 'undefined') {
                        plyrLoaded = true;
                        scanAll();
                        return;
                    }
                    // CSS and JS may already be preloaded by onPageStarted
                    if (!document.querySelector('link[href*="plyr.css"]')) {
                        loadCSS(TD_BASE + 'plyr/dist/plyr.css');
                    }
                    if (document.querySelector('script[src*="plyr.min.js"]')) {
                        // Already preloaded — wait for it to finish loading
                        var check = setInterval(function() {
                            if (typeof Plyr !== 'undefined') {
                                clearInterval(check);
                                plyrLoaded = true;
                                console.log('[TD-Plyr] Plyr loaded (preloaded)');
                                scanAll();
                            }
                        }, 20);
                    } else {
                        loadScript(TD_BASE + 'plyr/dist/plyr.min.js', function() {
                            if (typeof Plyr !== 'undefined') {
                                plyrLoaded = true;
                                console.log('[TD-Plyr] Plyr loaded');
                                scanAll();
                            }
                        });
                    }
                }

                loadPdfJs();
                loadPlyr();

                console.log('[TiddlyDesktop] Inline media enhancement initialized');
            })();
        """.trimIndent()

        // Add a WebViewClient that injects the script after page load
        webView.webViewClient = object : WebViewClient() {
            private fun handleUrl(url: String): Boolean {
                val uri = Uri.parse(url)
                val scheme = uri.scheme?.lowercase()
                // No scheme (e.g. bare words like "nothing", relative paths) —
                // block navigation so WebView doesn't silently fail
                if (scheme == null) {
                    Log.d(TAG, "Blocking navigation to scheme-less URL: $url")
                    return true
                }
                // Allow wiki server URLs to load in the WebView
                if (scheme == "http" && uri.host == "127.0.0.1") return false
                // Internal schemes that should stay in the WebView
                if (scheme == "data" || scheme == "blob" || scheme == "javascript") return false
                // Handle intent:// URIs (e.g. calendar events, app deep links)
                if (scheme == "intent") {
                    Log.d(TAG, "Parsing intent:// URI: $url")
                    try {
                        val intent = Intent.parseUri(url, Intent.URI_INTENT_SCHEME)
                        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                        if (intent.resolveActivity(packageManager) != null) {
                            startActivity(intent)
                        } else {
                            // Try browser_fallback_url if no handler found
                            val fallback = intent.getStringExtra("browser_fallback_url")
                            if (fallback != null) {
                                val fallbackIntent = Intent(Intent.ACTION_VIEW, Uri.parse(fallback))
                                fallbackIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                                startActivity(fallbackIntent)
                            } else {
                                Log.w(TAG, "No handler found for intent:// URI and no fallback URL")
                                android.widget.Toast.makeText(this@WikiActivity, "No app found to handle this link", android.widget.Toast.LENGTH_SHORT).show()
                            }
                        }
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to parse/launch intent:// URI: ${e.message}")
                        android.widget.Toast.makeText(this@WikiActivity, "No app found to handle this link", android.widget.Toast.LENGTH_SHORT).show()
                    }
                    return true
                }
                // All other URLs (http, https, mailto, tel, sms, geo, etc.)
                // open via the OS-assigned handler
                Log.d(TAG, "Opening external URL: $url (scheme=$scheme)")
                try {
                    val intent = Intent(Intent.ACTION_VIEW, uri)
                    intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    if (intent.resolveActivity(packageManager) != null) {
                        startActivity(intent)
                    } else {
                        Log.w(TAG, "No handler found for URL: $url")
                        android.widget.Toast.makeText(this@WikiActivity, "No app found to handle this link", android.widget.Toast.LENGTH_SHORT).show()
                    }
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to open external URL: ${e.message}")
                    android.widget.Toast.makeText(this@WikiActivity, "No app found to handle this link", android.widget.Toast.LENGTH_SHORT).show()
                }
                return true
            }

            @Deprecated("Deprecated in Java")
            override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean {
                return handleUrl(url)
            }

            override fun shouldOverrideUrlLoading(view: WebView, request: android.webkit.WebResourceRequest): Boolean {
                return handleUrl(request.url.toString())
            }

            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
                val url = request.url
                val path = url.path ?: return super.shouldInterceptRequest(view, request)

                // Serve attachment files directly, bypassing HTTP server.
                // EXCEPT: Range requests must go through the HTTP server because
                // WebView's shouldInterceptRequest doesn't properly support 206
                // Partial Content responses for media seeking (Chrome retries endlessly).
                val hasRangeHeader = request.requestHeaders?.let {
                    it.containsKey("Range") || it.containsKey("range")
                } ?: false

                if (path.startsWith("/_file/")) {
                    if (hasRangeHeader) {
                        Log.d(TAG, "Letting Range request fall through to HTTP server: $url")
                        return super.shouldInterceptRequest(view, request)
                    }
                    Log.d(TAG, "Intercepting /_file/ request: $url")
                    return serveFileRequest(path, request.requestHeaders)
                }
                if (path.startsWith("/_relative/")) {
                    if (hasRangeHeader) {
                        Log.d(TAG, "Letting Range request fall through to HTTP server: $url")
                        return super.shouldInterceptRequest(view, request)
                    }
                    Log.d(TAG, "Intercepting /_relative/ request: $url")
                    return serveRelativeRequest(path, request.requestHeaders)
                }

                // Serve bundled library assets from /_td/ prefix
                if (path.startsWith("/_td/")) {
                    Log.d(TAG, "Intercepting /_td/ request: $url")
                    val response = serveTdAsset(path.removePrefix("/_td/"))
                    if (response != null) {
                        Log.d(TAG, "Served /_td/ asset: ${path.removePrefix("/_td/")}")
                    } else {
                        Log.e(TAG, "Failed to serve /_td/ asset: ${path.removePrefix("/_td/")}")
                    }
                    return response
                }

                // Fallback: serve unknown paths as relative files from the wiki folder.
                // This handles _canonical_uri relative paths (e.g. ./attachments/video.mp4)
                // that the browser resolves to /attachments/video.mp4 before the URL-transform
                // MutationObserver has had a chance to rewrite them to /_relative/... URLs.
                // Skip range requests — let them go to the HTTP server for proper 206 support.
                if (!hasRangeHeader && (treeUri != null || wikiUri != null) && !path.startsWith("/_") && path != "/" &&
                    !path.startsWith("/recipes/") && !path.startsWith("/bags/") &&
                    !path.startsWith("/status")) {
                    val relativePath = "./" + path.trimStart('/')
                    val encodedPath = "/_relative/" + java.net.URLEncoder.encode(relativePath, "UTF-8")
                    val response = serveRelativeRequest(encodedPath, request.requestHeaders)
                    if (response != null) {
                        Log.d(TAG, "Served relative fallback: $path -> $relativePath")
                        return response
                    }
                }

                return super.shouldInterceptRequest(view, request)
            }

            override fun onPageFinished(view: WebView, url: String) {
                super.onPageFinished(view, url)
                // Guard against duplicate onPageFinished calls (common for HTTP-served wikis).
                // Check+set a JS flag atomically; only inject scripts on first call.
                view.evaluateJavascript(
                    "(function(){if(window.__tdScriptsInjected){return 'yes'}window.__tdScriptsInjected=true;return 'no'})()"
                ) { result ->
                    if (result?.contains("yes") == true) {
                        Log.d(TAG, "Scripts already injected, skipping duplicate onPageFinished")
                        return@evaluateJavascript
                    }
                    Log.d(TAG, "Injecting scripts for: $url")
                    // Inject the palette monitoring script FIRST — must run before media
                    // scripts which can saturate WebView connections and cause timeouts
                    view.evaluateJavascript(paletteScript, null)
                    // Inject the external attachment handler
                    view.evaluateJavascript(externalAttachmentScript, null)
                    // Inject the saver for single-file wikis
                    if (saverScript.isNotEmpty()) {
                        view.evaluateJavascript(saverScript, null)
                    }
                    // Inject the fullscreen toggle handler
                    view.evaluateJavascript(fullscreenScript, null)
                    // Clipboard override is now in the main settings script (settingsScript)
                    // Inject the print handler
                    view.evaluateJavascript(printScript, null)
                    // Inject the external window handler (open URLs in external browser)
                    view.evaluateJavascript(externalWindowScript, null)
                    // Inject the open window handler (open tiddler in new window)
                    view.evaluateJavascript(openWindowScript, null)
                    // Inject the favicon extraction handler
                    view.evaluateJavascript(faviconScript, null)
                    // Inject the export/download handler
                    view.evaluateJavascript(exportScript, null)
                    // Inject server health check for single-file wikis
                    injectServerHealthCheck()
                    // Inject inline PDF.js and Plyr enhancement
                    view.evaluateJavascript(inlineMediaScript, null)
                    // Import pending Quick Captures
                    importPendingCaptures()
                }
            }
        }

        // Set initial system bar colors based on system dark mode setting.
        // These are fallback colors shown before JavaScript sets the palette colors.
        val isDarkMode = (resources.configuration.uiMode and
            android.content.res.Configuration.UI_MODE_NIGHT_MASK) ==
            android.content.res.Configuration.UI_MODE_NIGHT_YES
        if (isDarkMode) {
            updateSystemBarColors("#333333", "#333333")
        } else {
            updateSystemBarColors("#ffffff", "#ffffff")
        }

        // Load the wiki URL
        Log.d(TAG, "Loading wiki URL: $wikiUrl")
        webView.loadUrl(wikiUrl)
    }

    // Flag to track if we're waiting for unsaved changes check
    private var pendingBackAction = false

    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        // Delegate back button to shared handler (for physical back button on older devices)
        if (keyCode == KeyEvent.KEYCODE_BACK) {
            handleBackNavigation()
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    /**
     * Shared back navigation handler used by both OnBackPressedCallback (gesture nav)
     * and onKeyDown (physical back button).
     */
    private fun handleBackNavigation() {
        // Dismiss auth overlay if showing
        if (authOverlayContainer != null) {
            // If the auth WebView can go back, navigate back within it
            if (authWebView?.canGoBack() == true) {
                authWebView?.goBack()
            } else {
                dismissAuthOverlay()
            }
            return
        }

        // Exit immersive fullscreen first (app fullscreen via tm-full-screen)
        if (isImmersiveFullscreen) {
            isImmersiveFullscreen = false
            exitImmersiveMode()
            return
        }

        // Exit video fullscreen
        if (fullscreenView != null) {
            fullscreenCallback?.onCustomViewHidden()
            return
        }

        // Handle back button for child windows (opened via tm-open-window)
        if (isChildWindow) {
            Log.d(TAG, "Child window back pressed, returning to parent")
            finish()
            return
        }

        // Check if there's a tiddler opened via tm-open-window to close.
        // __tdCloseLastOpenWindow() pops the stack and closes the tiddler.
        // The OnBackPressedCallback already consumed the event, so the activity
        // won't close until we explicitly call finish().
        webView.evaluateJavascript(
            "window.__tdCloseLastOpenWindow ? window.__tdCloseLastOpenWindow() : false"
        ) { result ->
            runOnUiThread {
                if (result == "true") {
                    Log.d(TAG, "Back: closed tm-open-window tiddler")
                } else {
                    handleBackNavigationDefault()
                }
            }
        }
    }

    /**
     * Default back navigation when there's no hash fragment.
     * Handles WebView history and activity closing.
     */
    private fun handleBackNavigationDefault() {
        // Handle WebView navigation
        if (!isFolder && httpServer != null) {
            // For single-file wikis: Don't go back to old server URLs after reconnect
            val currentPort = httpServer!!.port

            if (webView.canGoBack()) {
                val history = webView.copyBackForwardList()
                val currentIndex = history.currentIndex

                if (currentIndex > 0) {
                    val backUrl = history.getItemAtIndex(currentIndex - 1).url
                    if (!backUrl.contains(":$currentPort/")) {
                        Log.d(TAG, "Back would go to old server URL, checking for unsaved changes")
                        checkUnsavedChangesAndClose()
                        return
                    }
                    webView.goBack()
                    return
                }
            }
            checkUnsavedChangesAndClose()
            return
        }

        // For folder wikis: normal back behavior
        if (webView.canGoBack()) {
            webView.goBack()
            return
        }

        // Nothing to go back to — close the activity
        finish()
    }

    /**
     * Check for unsaved changes before closing the activity.
     * If there are unsaved changes, prompt the user to save them first.
     */
    private fun checkUnsavedChangesAndClose() {
        if (pendingBackAction) return
        pendingBackAction = true

        // Check for unsaved changes via JavaScript
        webView.evaluateJavascript("""
            (function() {
                if (typeof ${'$'}tw !== 'undefined' && ${'$'}tw.wiki && ${'$'}tw.wiki.getChangeCount) {
                    return ${'$'}tw.wiki.getChangeCount();
                }
                return 0;
            })();
        """.trimIndent()) { result ->
            val changeCount = result?.toIntOrNull() ?: 0
            pendingBackAction = false

            if (changeCount > 0) {
                // Show confirmation dialog
                runOnUiThread {
                    android.app.AlertDialog.Builder(this)
                        .setTitle("Unsaved Changes")
                        .setMessage("You have $changeCount unsaved change(s). What would you like to do?")
                        .setPositiveButton("Save & Close") { _, _ ->
                            // Save changes to localStorage and close
                            webView.evaluateJavascript("""
                                (function() {
                                    try {
                                        var key = 'tiddlydesktop_unsaved_' + btoa(window.__WIKI_PATH__ || 'default').replace(/[^a-zA-Z0-9]/g, '');
                                        var modified = [];
                                        if (${'$'}tw.syncer && ${'$'}tw.syncer.tiddlerInfo) {
                                            for (var title in ${'$'}tw.syncer.tiddlerInfo) {
                                                var info = ${'$'}tw.syncer.tiddlerInfo[title];
                                                if (info && info.changeCount > 0) {
                                                    var tiddler = ${'$'}tw.wiki.getTiddler(title);
                                                    if (tiddler) modified.push(JSON.parse(JSON.stringify(tiddler.fields)));
                                                }
                                            }
                                        }
                                        if (modified.length > 0) {
                                            localStorage.setItem(key, JSON.stringify({
                                                timestamp: Date.now(),
                                                path: window.__WIKI_PATH__,
                                                tiddlers: modified
                                            }));
                                        }
                                        return true;
                                    } catch(e) { return false; }
                                })();
                            """.trimIndent()) { _ ->
                                finish()
                            }
                        }
                        .setNegativeButton("Discard & Close") { _, _ ->
                            finish()
                        }
                        .setNeutralButton("Cancel", null)
                        .show()
                }
            } else {
                finish()
            }
        }
    }

    override fun onPause() {
        super.onPause()
        webView.onPause()
    }

    override fun onResume() {
        super.onResume()
        webView.onResume()

        // Check if HTTP server needs restart (single-file wikis only)
        // This handles the case where the phone went to sleep and Android killed the socket
        if (!isFolder && httpServer != null) {
            if (!httpServer!!.isRunning()) {
                Log.d(TAG, "HTTP server died while paused, restarting...")
                try {
                    val newUrl = httpServer!!.restart()
                    Log.d(TAG, "Server restarted at: $newUrl")
                    // Reload the page with the new server URL
                    webView.loadUrl(newUrl)
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to restart HTTP server: ${e.message}")
                    // Show error to user
                    webView.loadData(
                        "<html><body><h1>Server Error</h1><p>Failed to restart wiki server: ${e.message}</p><p>Please close and reopen this wiki.</p></body></html>",
                        "text/html",
                        "UTF-8"
                    )
                }
            } else {
                Log.d(TAG, "HTTP server still running on port ${httpServer!!.port}")
            }
        }

        // For folder wikis using Node.js server, we can check if the server is still accessible
        // by attempting a quick HTTP request. If it fails, show a reload button.
        if (isFolder) {
            checkFolderServerHealth()
        }

        // Check for pending captures if wiki is loaded
        if (::webView.isInitialized) {
            webView.evaluateJavascript("typeof \$tw!=='undefined'&&\$tw.wiki?'ready':'no'") { r ->
                if (r?.contains("ready") == true) importPendingCaptures()
            }
        }
    }

    /**
     * Check if the folder wiki's Node.js server is still accessible.
     * If not, inject a message suggesting to reload.
     */
    private fun checkFolderServerHealth() {
        val serverUrl = intent.getStringExtra(EXTRA_WIKI_URL) ?: return

        Thread {
            try {
                val url = java.net.URL("$serverUrl/status")
                val connection = url.openConnection() as java.net.HttpURLConnection
                connection.connectTimeout = 2000
                connection.readTimeout = 2000
                connection.requestMethod = "GET"

                val responseCode = connection.responseCode
                connection.disconnect()

                if (responseCode != 200) {
                    Log.w(TAG, "Folder wiki server health check failed: $responseCode")
                    showServerUnavailableMessage(serverUrl)
                } else {
                    Log.d(TAG, "Folder wiki server is healthy")
                }
            } catch (e: Exception) {
                Log.w(TAG, "Folder wiki server health check failed: ${e.message}")
                showServerUnavailableMessage(serverUrl)
            }
        }.start()
    }

    /**
     * Show a message when the wiki server is unavailable.
     * For single-file wikis: Offers to restart the local HTTP server
     * For folder wikis: Offers to reload the page
     */
    @Suppress("UNUSED_PARAMETER")
    private fun showServerUnavailableMessage(serverUrl: String) {
        runOnUiThread {
            // Inject a message into the page with appropriate action
            val script = if (!isFolder && httpServer != null) {
                // Single-file wiki: Can restart the local HTTP server
                """
                (function() {
                    // Only show if we haven't already
                    if (document.getElementById('td-server-unavailable')) return;

                    var div = document.createElement('div');
                    div.id = 'td-server-unavailable';
                    div.style.cssText = 'position:fixed;top:0;left:0;right:0;background:#c42b2b;color:white;padding:12px;text-align:center;z-index:999999;font-family:sans-serif;';
                    div.innerHTML = 'Server connection lost. <button id="td-reconnect-btn" style="margin-left:8px;padding:4px 12px;background:white;color:#c42b2b;border:none;border-radius:4px;cursor:pointer;">Reconnect</button>';
                    document.body.insertBefore(div, document.body.firstChild);

                    document.getElementById('td-reconnect-btn').onclick = function() {
                        this.textContent = 'Reconnecting...';
                        this.disabled = true;
                        try {
                            var resultJson = window.TiddlyDesktopServer.restartServerAndNavigate();
                            var result = JSON.parse(resultJson);
                            if (!result.success) {
                                alert('Failed to restart server: ' + (result.error || 'Unknown error'));
                                this.textContent = 'Reconnect';
                                this.disabled = false;
                            }
                        } catch (e) {
                            alert('Error: ' + e.message);
                            this.textContent = 'Reconnect';
                            this.disabled = false;
                        }
                    };
                })();
                """.trimIndent()
            } else {
                // Folder wiki: Can only reload (Node.js server is in main process)
                """
                (function() {
                    // Only show if we haven't already
                    if (document.getElementById('td-server-unavailable')) return;

                    var div = document.createElement('div');
                    div.id = 'td-server-unavailable';
                    div.style.cssText = 'position:fixed;top:0;left:0;right:0;background:#c42b2b;color:white;padding:12px;text-align:center;z-index:999999;font-family:sans-serif;';
                    div.innerHTML = 'Server connection lost. <button onclick="location.reload()" style="margin-left:8px;padding:4px 12px;background:white;color:#c42b2b;border:none;border-radius:4px;cursor:pointer;">Reload</button>';
                    document.body.insertBefore(div, document.body.firstChild);
                })();
                """.trimIndent()
            }
            webView.evaluateJavascript(script, null)
        }
    }

    /**
     * Inject a periodic server health check that shows reconnect banner if server dies.
     * This is called once during wiki load.
     */
    private fun injectServerHealthCheck() {
        // Only for single-file wikis with local HTTP server
        if (isFolder || httpServer == null) return

        val serverPort = httpServer!!.port
        val script = """
            (function() {
                var checkInterval = 5000; // Check every 5 seconds
                var serverUrl = 'http://127.0.0.1:$serverPort';
                var consecutiveFailures = 0;

                function checkServerHealth() {
                    fetch(serverUrl + '/', { method: 'HEAD' })
                        .then(function(response) {
                            if (!response.ok) {
                                consecutiveFailures++;
                                if (consecutiveFailures >= 2) {
                                    showDisconnectBanner();
                                }
                            } else {
                                consecutiveFailures = 0;
                                hideDisconnectBanner();
                            }
                        })
                        .catch(function() {
                            consecutiveFailures++;
                            if (consecutiveFailures >= 2) {
                                showDisconnectBanner();
                            }
                        });
                }

                // Get modified tiddlers that haven't been saved
                function getModifiedTiddlers() {
                    if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.wiki) return [];
                    if (!${'$'}tw.wiki.getChangeCount || ${'$'}tw.wiki.getChangeCount() === 0) return [];

                    var modified = [];

                    // Method 1: Check syncer's changed tiddlers
                    if (${'$'}tw.syncer && ${'$'}tw.syncer.tiddlerInfo) {
                        for (var title in ${'$'}tw.syncer.tiddlerInfo) {
                            var info = ${'$'}tw.syncer.tiddlerInfo[title];
                            // Check if tiddler has local changes not synced
                            if (info && info.changeCount > 0) {
                                var tiddler = ${'$'}tw.wiki.getTiddler(title);
                                if (tiddler) {
                                    modified.push(JSON.parse(JSON.stringify(tiddler.fields)));
                                }
                            }
                        }
                    }

                    // Method 2: If syncer method found nothing, check all recent tiddlers
                    if (modified.length === 0) {
                        ${'$'}tw.wiki.forEachTiddler(function(title, tiddler) {
                            // Include non-system tiddlers modified recently (within this session)
                            if (!title.startsWith('$:/') || title.startsWith('$:/temp/') || title.indexOf('/Draft/') !== -1) {
                                var fields = tiddler.fields;
                                // Include drafts and recently modified tiddlers
                                if (title.indexOf('/Draft/') !== -1 || (fields.modified && new Date(fields.modified) > window.__TD_SESSION_START__)) {
                                    modified.push(JSON.parse(JSON.stringify(fields)));
                                }
                            }
                        });
                    }

                    return modified;
                }

                // Save modified tiddlers to localStorage
                function saveModifiedToStorage() {
                    var modified = getModifiedTiddlers();
                    if (modified.length === 0) return true;

                    try {
                        var key = 'tiddlydesktop_unsaved_' + btoa(window.__WIKI_PATH__ || 'default').replace(/[^a-zA-Z0-9]/g, '');
                        localStorage.setItem(key, JSON.stringify({
                            timestamp: Date.now(),
                            path: window.__WIKI_PATH__,
                            tiddlers: modified
                        }));
                        console.log('[TiddlyDesktop] Saved ' + modified.length + ' modified tiddlers to localStorage');
                        return true;
                    } catch (e) {
                        console.error('[TiddlyDesktop] Failed to save to localStorage:', e);
                        return false;
                    }
                }

                // Record session start time for change detection
                window.__TD_SESSION_START__ = window.__TD_SESSION_START__ || new Date();

                function showDisconnectBanner() {
                    if (document.getElementById('td-server-unavailable')) return;

                    var hasUnsaved = typeof ${'$'}tw !== 'undefined' && ${'$'}tw.wiki && ${'$'}tw.wiki.getChangeCount && ${'$'}tw.wiki.getChangeCount() > 0;
                    var changeCount = hasUnsaved ? ${'$'}tw.wiki.getChangeCount() : 0;

                    var div = document.createElement('div');
                    div.id = 'td-server-unavailable';
                    div.style.cssText = 'position:fixed;top:0;left:0;right:0;background:#c42b2b;color:white;padding:12px;text-align:center;z-index:999999;font-family:sans-serif;';

                    if (hasUnsaved) {
                        div.innerHTML = 'Server connection lost. <strong>' + changeCount + ' unsaved change(s)</strong>. ' +
                            '<button id="td-reconnect-btn" style="margin-left:8px;padding:4px 12px;background:white;color:#c42b2b;border:none;border-radius:4px;cursor:pointer;">Save & Reconnect</button>';
                    } else {
                        div.innerHTML = 'Server connection lost. ' +
                            '<button id="td-reconnect-btn" style="margin-left:8px;padding:4px 12px;background:white;color:#c42b2b;border:none;border-radius:4px;cursor:pointer;">Reconnect</button>';
                    }
                    document.body.insertBefore(div, document.body.firstChild);

                    document.getElementById('td-reconnect-btn').onclick = function() {
                        var btn = this;
                        btn.textContent = hasUnsaved ? 'Saving...' : 'Reconnecting...';
                        btn.disabled = true;

                        try {
                            // Save modified tiddlers to localStorage first
                            if (hasUnsaved) {
                                if (!saveModifiedToStorage()) {
                                    if (!confirm('Failed to backup changes to local storage. Reconnect anyway? (Changes may be lost)')) {
                                        btn.textContent = 'Save & Reconnect';
                                        btn.disabled = false;
                                        return;
                                    }
                                }
                                btn.textContent = 'Reconnecting...';
                            }

                            if (window.TiddlyDesktopServer && window.TiddlyDesktopServer.restartServerAndNavigate) {
                                var resultJson = window.TiddlyDesktopServer.restartServerAndNavigate();
                                var result = JSON.parse(resultJson);
                                if (!result.success) {
                                    alert('Failed to restart server: ' + (result.error || 'Unknown error'));
                                    btn.textContent = hasUnsaved ? 'Save & Reconnect' : 'Reconnect';
                                    btn.disabled = false;
                                }
                            } else {
                                location.reload();
                            }
                        } catch (e) {
                            alert('Error: ' + e.message);
                            btn.textContent = hasUnsaved ? 'Save & Reconnect' : 'Reconnect';
                            btn.disabled = false;
                        }
                    };
                }

                function hideDisconnectBanner() {
                    var banner = document.getElementById('td-server-unavailable');
                    if (banner) {
                        banner.remove();
                    }
                }

                // Check for and restore saved tiddlers from previous session
                function checkAndRestoreSavedTiddlers() {
                    if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.wiki) {
                        // TiddlyWiki not ready, try again later
                        setTimeout(checkAndRestoreSavedTiddlers, 500);
                        return;
                    }

                    try {
                        var key = 'tiddlydesktop_unsaved_' + btoa(window.__WIKI_PATH__ || 'default').replace(/[^a-zA-Z0-9]/g, '');
                        var saved = localStorage.getItem(key);
                        if (!saved) return;

                        var data = JSON.parse(saved);
                        var age = Date.now() - data.timestamp;
                        var maxAge = 60 * 60 * 1000; // 1 hour

                        if (age > maxAge) {
                            // Too old, discard
                            localStorage.removeItem(key);
                            console.log('[TiddlyDesktop] Discarded old saved tiddlers (age: ' + Math.round(age/1000/60) + ' minutes)');
                            return;
                        }

                        if (!data.tiddlers || data.tiddlers.length === 0) {
                            localStorage.removeItem(key);
                            return;
                        }

                        // Show restore prompt
                        var minutes = Math.round(age / 1000 / 60);
                        var timeAgo = minutes < 1 ? 'just now' : minutes + ' minute(s) ago';

                        if (confirm('Found ' + data.tiddlers.length + ' unsaved change(s) from ' + timeAgo + '. Restore them?')) {
                            var restored = 0;
                            data.tiddlers.forEach(function(fields) {
                                try {
                                    ${'$'}tw.wiki.addTiddler(new ${'$'}tw.Tiddler(fields));
                                    restored++;
                                } catch (e) {
                                    console.error('[TiddlyDesktop] Failed to restore tiddler:', fields.title, e);
                                }
                            });
                            console.log('[TiddlyDesktop] Restored ' + restored + ' tiddlers');

                            // Show notification
                            if (restored > 0) {
                                ${'$'}tw.notifier.display('${'$'}:/core/images/done-button', {
                                    title: 'Changes Restored',
                                    text: 'Restored ' + restored + ' unsaved change(s). Remember to save!'
                                });
                            }
                        }

                        // Clear the backup after restore (or rejection)
                        localStorage.removeItem(key);

                    } catch (e) {
                        console.error('[TiddlyDesktop] Error checking for saved tiddlers:', e);
                    }
                }

                // Start periodic health check after page is fully loaded
                setTimeout(function() {
                    setInterval(checkServerHealth, checkInterval);
                    // Also check for saved tiddlers to restore
                    checkAndRestoreSavedTiddlers();
                }, 5000);

                console.log('[TiddlyDesktop] Server health check installed');
            })();
        """.trimIndent()

        webView.evaluateJavascript(script, null)
    }

    override fun onDestroy() {
        Log.d(TAG, "Wiki closed: path=$wikiPath, isFolder=$isFolder")

        // Clean up auth overlay if present
        dismissAuthOverlay()

        // Notify foreground service that wiki is closed
        WikiServerService.wikiClosed(applicationContext)

        // Release WakeLock
        releaseWakeLock()

        // Stop the HTTP server (for single-file wikis)
        httpServer?.stop()
        httpServer = null

        // For folder wikis, clean up the local copy used by Node.js server
        // This triggers cleanup in the main process via JNI
        if (isFolder && !wikiPath.isNullOrEmpty()) {
            try {
                cleanupWikiLocalCopy(wikiPath!!, true)
                Log.d(TAG, "Triggered cleanup for folder wiki local copy")
            } catch (e: Exception) {
                Log.e(TAG, "Failed to cleanup wiki local copy: ${e.message}")
            }
        }

        if (::webView.isInitialized) {
            webView.destroy()
        }
        super.onDestroy()
    }

    /**
     * Acquire a partial WakeLock to keep the HTTP server alive when the app is in background.
     * This prevents Android from killing the server thread when the screen is off.
     */
    @SuppressLint("WakelockTimeout")
    private fun acquireWakeLock() {
        if (wakeLock != null) return  // Already acquired

        try {
            val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = powerManager.newWakeLock(
                PowerManager.PARTIAL_WAKE_LOCK,
                "TiddlyDesktop:WikiServer"
            )
            wakeLock?.acquire()
            Log.d(TAG, "WakeLock acquired to keep server alive")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to acquire WakeLock: ${e.message}")
        }
    }

    /**
     * Release the WakeLock when the activity is destroyed.
     */
    private fun releaseWakeLock() {
        try {
            if (wakeLock?.isHeld == true) {
                wakeLock?.release()
                Log.d(TAG, "WakeLock released")
            }
            wakeLock = null
        } catch (e: Exception) {
            Log.e(TAG, "Failed to release WakeLock: ${e.message}")
        }
    }

    /**
     * Serve a bundled asset from the td/ directory in Android assets.
     * Used for PDF.js and Plyr library files accessed via /_td/ URL prefix.
     */
    /**
     * Serve /_file/{base64path} requests directly via WebView interception.
     * Bypasses the HTTP server for better reliability and performance.
     */
    private fun serveFileRequest(path: String, requestHeaders: Map<String, String>): WebResourceResponse? {
        return try {
            val encoded = path.removePrefix("/_file/")
            // Decode base64url (- -> +, _ -> /)
            val base64 = encoded.replace('-', '+').replace('_', '/')
            val padded = when (base64.length % 4) {
                2 -> "$base64=="
                3 -> "$base64="
                else -> base64
            }
            val decodedPath = String(Base64.decode(padded, Base64.DEFAULT))
            Log.d(TAG, "Intercepting file request: $decodedPath")

            val uri = when {
                decodedPath.startsWith("content://") -> Uri.parse(decodedPath)
                decodedPath.startsWith("file://") -> Uri.parse(decodedPath)
                else -> Uri.fromFile(File(decodedPath))
            }

            val mimeType = guessMimeTypeForIntercept(decodedPath)

            // Convert unsupported image formats to JPEG
            if (mimeType == "image/heic" || mimeType == "image/heif" ||
                mimeType == "image/tiff" || mimeType == "image/avif") {
                return serveConvertedImage(uri)
            }

            serveUriWithRangeSupport(uri, mimeType, requestHeaders)
        } catch (e: Exception) {
            Log.e(TAG, "Error serving file request: ${e.message}", e)
            null
        }
    }

    /**
     * Serve /_relative/{relativePath} requests directly via WebView interception.
     * Bypasses the HTTP server for better reliability and performance.
     */
    private fun serveRelativeRequest(path: String, requestHeaders: Map<String, String>): WebResourceResponse? {
        return try {
            val relativePath = URLDecoder.decode(path.removePrefix("/_relative/"), "UTF-8")
            Log.d(TAG, "Intercepting relative request: $relativePath")

            // Check if this file is being copied in the background.
            // Serve from the original source URI while the copy is in progress.
            val normalizedPath = if (relativePath.startsWith("./")) relativePath else "./$relativePath"
            val sourceUri = pendingFileCopies[normalizedPath]
            if (sourceUri != null) {
                Log.d(TAG, "Serving from source URI (copy in progress): $relativePath")
                val mimeType = contentResolver.getType(sourceUri) ?: guessMimeTypeForIntercept(relativePath)
                return serveUriWithRangeSupport(sourceUri, mimeType, requestHeaders)
            }

            val parentDoc = if (treeUri != null) {
                DocumentFile.fromTreeUri(this, treeUri!!)
            } else if (wikiUri != null) {
                // Fall back to parent of single document URI (single-file wikis without tree access)
                DocumentFile.fromSingleUri(this, wikiUri!!)?.parentFile
            } else {
                null
            }

            if (parentDoc == null) {
                Log.e(TAG, "No tree URI or wiki URI for relative file: $relativePath")
                return null
            }

            // Navigate the path
            val pathParts = relativePath.split("/")
            var currentDoc: DocumentFile? = parentDoc
            for (part in pathParts) {
                if (part.isEmpty() || part == ".") continue
                if (part == "..") {
                    currentDoc = currentDoc?.parentFile
                } else {
                    currentDoc = currentDoc?.findFile(part)
                }
                if (currentDoc == null) break
            }

            if (currentDoc == null || !currentDoc.exists()) {
                Log.e(TAG, "Relative file not found: $relativePath")
                return null
            }

            val mimeType = currentDoc.type ?: guessMimeTypeForIntercept(relativePath)
            val uri = currentDoc.uri

            // Convert unsupported image formats to JPEG
            if (mimeType == "image/heic" || mimeType == "image/heif" ||
                mimeType == "image/tiff" || mimeType == "image/avif") {
                return serveConvertedImage(uri)
            }

            serveUriWithRangeSupport(uri, mimeType, requestHeaders)
        } catch (e: Exception) {
            Log.e(TAG, "Error serving relative request: ${e.message}", e)
            null
        }
    }

    /**
     * Serve a file URI with HTTP Range request support.
     * Uses ParcelFileDescriptor for O(1) seeking instead of InputStream.skip() which is O(n).
     * Returns a 206 Partial Content response for Range requests (needed for video seeking),
     * or a 200 OK response with Content-Length and Accept-Ranges headers for full requests.
     */
    private fun serveUriWithRangeSupport(uri: Uri, mimeType: String, requestHeaders: Map<String, String>): WebResourceResponse? {
        // Get file size for Range support
        val fileSize = try {
            contentResolver.openAssetFileDescriptor(uri, "r")?.use { it.length } ?: -1L
        } catch (e: Exception) { -1L }

        val rangeHeader = requestHeaders["Range"] ?: requestHeaders["range"]

        if (rangeHeader != null && rangeHeader.startsWith("bytes=") && fileSize > 0) {
            // Handle Range request for video/audio seeking
            val rangeSpec = rangeHeader.removePrefix("bytes=")
            val rangeParts = rangeSpec.split("-")
            val start = rangeParts[0].toLongOrNull() ?: 0L
            val end = if (rangeParts.size > 1 && rangeParts[1].isNotEmpty()) {
                rangeParts[1].toLongOrNull() ?: (fileSize - 1)
            } else {
                fileSize - 1
            }

            if (start >= fileSize || start > end) {
                return WebResourceResponse(mimeType, null, 416, "Range Not Satisfiable",
                    mapOf("Content-Range" to "bytes */$fileSize"), null)
            }

            val contentLength = end - start + 1

            // Use ParcelFileDescriptor for O(1) seeking via file channel
            val pfd = contentResolver.openFileDescriptor(uri, "r") ?: return null
            val fis = java.io.FileInputStream(pfd.fileDescriptor)
            fis.channel.position(start)

            // Wrap in a limiting stream that also closes the ParcelFileDescriptor
            val limitedStream = LimitedInputStream(fis, contentLength, pfd)

            val headers = mapOf(
                "Content-Type" to mimeType,
                "Content-Length" to contentLength.toString(),
                "Content-Range" to "bytes $start-$end/$fileSize",
                "Accept-Ranges" to "bytes",
                "Access-Control-Allow-Origin" to "*"
            )

            Log.d(TAG, "Range response: bytes $start-$end/$fileSize ($contentLength bytes)")
            return WebResourceResponse(mimeType, null, 206, "Partial Content", headers, limitedStream)
        }

        // Full response with Content-Length and Accept-Ranges
        val inputStream = contentResolver.openInputStream(uri) ?: return null
        val headers = mutableMapOf(
            "Accept-Ranges" to "bytes",
            "Access-Control-Allow-Origin" to "*"
        )
        if (fileSize > 0) {
            headers["Content-Length"] = fileSize.toString()
        }

        return WebResourceResponse(mimeType, null, 200, "OK", headers, inputStream)
    }

    /**
     * InputStream wrapper that limits reads to a specified number of bytes.
     * Used for serving HTTP Range responses. Optionally closes a ParcelFileDescriptor on close.
     */
    private class LimitedInputStream(
        private val wrapped: java.io.InputStream,
        private var remaining: Long,
        private val pfd: android.os.ParcelFileDescriptor? = null
    ) : java.io.InputStream() {
        override fun read(): Int {
            if (remaining <= 0) return -1
            val b = wrapped.read()
            if (b >= 0) remaining--
            return b
        }
        override fun read(b: ByteArray, off: Int, len: Int): Int {
            if (remaining <= 0) return -1
            val toRead = minOf(len.toLong(), remaining).toInt()
            val n = wrapped.read(b, off, toRead)
            if (n > 0) remaining -= n
            return n
        }
        override fun close() {
            wrapped.close()
            pfd?.close()
        }
    }

    /**
     * Convert an unsupported image format to JPEG and return as WebResourceResponse.
     */
    private fun serveConvertedImage(uri: Uri): WebResourceResponse? {
        return try {
            val bitmap = contentResolver.openInputStream(uri)?.use { input ->
                android.graphics.BitmapFactory.decodeStream(input)
            } ?: return null

            val baos = java.io.ByteArrayOutputStream()
            bitmap.compress(android.graphics.Bitmap.CompressFormat.JPEG, 90, baos)
            bitmap.recycle()

            WebResourceResponse("image/jpeg", null, java.io.ByteArrayInputStream(baos.toByteArray()))
        } catch (e: Exception) {
            Log.e(TAG, "Error converting image: ${e.message}", e)
            null
        }
    }

    /**
     * Guess MIME type from file path extension.
     */
    private fun guessMimeTypeForIntercept(path: String): String {
        val ext = path.substringAfterLast('.', "").lowercase()
        return when (ext) {
            "html", "htm" -> "text/html"
            "css" -> "text/css"
            "js" -> "application/javascript"
            "json" -> "application/json"
            "txt" -> "text/plain"
            "png" -> "image/png"
            "jpg", "jpeg" -> "image/jpeg"
            "gif" -> "image/gif"
            "svg" -> "image/svg+xml"
            "webp" -> "image/webp"
            "ico" -> "image/x-icon"
            "bmp" -> "image/bmp"
            "tiff", "tif" -> "image/tiff"
            "heic", "heif" -> "image/heic"
            "avif" -> "image/avif"
            "mp3" -> "audio/mpeg"
            "m4a" -> "audio/mp4"
            "ogg", "oga" -> "audio/ogg"
            "wav" -> "audio/wav"
            "flac" -> "audio/flac"
            "mp4", "m4v" -> "video/mp4"
            "webm" -> "video/webm"
            "ogv" -> "video/ogg"
            "mov" -> "video/quicktime"
            "pdf" -> "application/pdf"
            else -> MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
        }
    }

    private fun serveTdAsset(assetPath: String): WebResourceResponse? {
        return try {
            val inputStream = assets.open("td/$assetPath")
            val mimeType = when {
                assetPath.endsWith(".js") -> "application/javascript"
                assetPath.endsWith(".css") -> "text/css"
                assetPath.endsWith(".svg") -> "image/svg+xml"
                assetPath.endsWith(".bcmap") -> "application/octet-stream"
                else -> "application/octet-stream"
            }
            WebResourceResponse(mimeType, "UTF-8", inputStream)
        } catch (e: Exception) {
            Log.e(TAG, "Failed to serve td asset: $assetPath - ${e.message}")
            null
        }
    }

    /**
     * Parse the wiki path JSON to extract URIs.
     * Format: {"uri":"content://...","documentTopTreeUri":"content://..." or null}
     */
    private fun parseWikiPath(pathJson: String): Pair<Uri?, Uri?> {
        return try {
            val json = JSONObject(pathJson)
            val uriStr = json.optString("uri", "")
            val treeUriStr = json.optString("documentTopTreeUri", "")

            val uri = if (uriStr.isNotEmpty()) Uri.parse(uriStr) else null
            val treeUri = if (treeUriStr.isNotEmpty() && treeUriStr != "null") Uri.parse(treeUriStr) else null

            Pair(uri, treeUri)
        } catch (e: Exception) {
            Log.e(TAG, "Failed to parse wiki path JSON: ${e.message}")
            // Try as plain URI string
            Pair(Uri.parse(pathJson), null)
        }
    }

    /**
     * Escape a string for safe inclusion in a JavaScript single-quoted string literal.
     */
    private fun escapeForJs(s: String): String =
        s.replace("\\", "\\\\")
         .replace("'", "\\'")
         .replace("\n", "\\n")
         .replace("\r", "\\r")
         .replace("\t", "\\t")

    /**
     * Import pending Quick Captures that target this wiki.
     * Captures are JSON files in {filesDir}/captures/ written by CaptureActivity.
     * Auto-deletes captures older than 7 days.
     */
    private fun importPendingCaptures() {
        val capturesDir = File(filesDir, "captures")
        if (!capturesDir.exists() || !capturesDir.isDirectory) return
        val myPath = wikiPath ?: return
        val captureFiles = capturesDir.listFiles { f ->
            f.name.startsWith("capture_") && f.name.endsWith(".json")
        } ?: return

        // Read and delete files immediately to prevent duplicate imports
        // (both onPageFinished and onResume can call this concurrently)
        val now = System.currentTimeMillis()
        val captureJsons = mutableListOf<JSONObject>()
        for (f in captureFiles) {
            try {
                val json = JSONObject(f.readText())
                if (now - json.optLong("created", now) > 7 * 86400000L) {
                    f.delete(); continue  // expired
                }
                if (json.optString("target_wiki_path") == myPath) {
                    captureJsons.add(json)
                    f.delete()  // Delete immediately after reading
                }
            } catch (e: Exception) {
                Log.w(TAG, "Skipping malformed capture file ${f.name}: ${e.message}")
            }
        }
        if (captureJsons.isEmpty()) return

        // Separate file imports (native TW import) from direct tiddler captures
        val directCaptures = mutableListOf<JSONObject>()
        val fileImports = mutableListOf<JSONObject>()
        for (json in captureJsons) {
            if (json.has("import_file")) {
                fileImports.add(json)
            } else {
                directCaptures.add(json)
            }
        }

        // Handle direct tiddler captures (text, images, etc.)
        if (directCaptures.isNotEmpty()) {
            val js = StringBuilder("(function(){if(typeof \$tw==='undefined'||!\$tw.wiki)return 0;var c=0;")
            for (json in directCaptures) {
                try {
                    val title = escapeForJs(json.optString("title", "Untitled Capture"))
                    val text = escapeForJs(json.optString("text", ""))
                    val tags = escapeForJs(json.optString("tags", ""))
                    val type = escapeForJs(json.optString("type", "text/vnd.tiddlywiki"))
                    val sourceUrl = escapeForJs(json.optString("source_url", ""))
                    val canonicalUri = escapeForJs(json.optString("_canonical_uri", ""))
                    val created = json.optLong("created", now)
                    js.append("try{var f={title:'$title',text:'$text',tags:'$tags',type:'$type'")
                    if (sourceUrl.isNotEmpty()) js.append(",'source-url':'$sourceUrl'")
                    if (canonicalUri.isNotEmpty()) js.append(",'_canonical_uri':'$canonicalUri'")
                    js.append(",created:new Date($created),modified:new Date()};")
                    js.append("\$tw.wiki.addTiddler(new \$tw.Tiddler(f));c++;}catch(e){}")
                } catch (e: Exception) {
                    Log.w(TAG, "Skipping capture: ${e.message}")
                }
            }
            js.append("return c;})()")

            webView.evaluateJavascript(js.toString()) { result ->
                val count = result?.trim('"')?.toIntOrNull() ?: 0
                if (count > 0) {
                    runOnUiThread {
                        android.widget.Toast.makeText(this, getString(R.string.capture_imported, count), android.widget.Toast.LENGTH_SHORT).show()
                    }
                }
            }
        }

        // Handle file imports — use TiddlyWiki's native import via tm-import-tiddlers
        // The event must be dispatched from a leaf widget so it bubbles UP to NavigatorWidget,
        // which handles creating $:/Import, navigating to it, and merging with existing imports.
        for (json in fileImports) {
            val importFilename = json.optString("import_file", "")
            val fileType = json.optString("file_type", "text/html")
            if (importFilename.isEmpty()) continue

            val importFile = File(capturesDir, importFilename)
            if (!importFile.exists()) continue

            try {
                val contentBytes = importFile.readBytes()
                importFile.delete()

                // Base64 encode to safely pass content to JS (avoids all escaping issues)
                val base64Content = android.util.Base64.encodeToString(contentBytes, android.util.Base64.NO_WRAP)
                val safeFileType = escapeForJs(fileType)

                // Poll until TW5 widget tree is ready, then dispatch tm-import-tiddlers
                // from a leaf widget so it bubbles up through NavigatorWidget.
                // NavigatorWidget's handler creates $:/Import, navigates to it, and merges.
                val importJs = "(function check(){" +
                    "if(typeof \$tw==='undefined'||!\$tw.wiki||!\$tw.rootWidget" +
                    "||!\$tw.rootWidget.children||!\$tw.rootWidget.children.length){" +
                    "setTimeout(check,200);return;}" +
                    "try{" +
                    "var b=atob('$base64Content');" +
                    "var bytes=new Uint8Array(b.length);" +
                    "for(var i=0;i<b.length;i++)bytes[i]=b.charCodeAt(i);" +
                    "var content=new TextDecoder().decode(bytes);" +
                    "var tiddlers=\$tw.wiki.deserializeTiddlers('$safeFileType',content);" +
                    "if(!tiddlers||tiddlers.length===0)return;" +
                    // Find a leaf widget and dispatch — event bubbles up to NavigatorWidget
                    "var w=\$tw.rootWidget;" +
                    "while(w.children&&w.children.length>0)w=w.children[0];" +
                    "w.dispatchEvent({type:'tm-import-tiddlers',param:JSON.stringify(tiddlers)});" +
                    "}catch(e){console.error('Import error:',e);}" +
                    "})()"

                webView.evaluateJavascript(importJs, null)
                runOnUiThread {
                    android.widget.Toast.makeText(this, "Importing file...", android.widget.Toast.LENGTH_SHORT).show()
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to process import file $importFilename: ${e.message}")
                importFile.delete()
            }
        }
    }
}
