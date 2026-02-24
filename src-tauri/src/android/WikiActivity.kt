package com.burningtreec.tiddlydesktop_rs

import android.Manifest
import android.annotation.SuppressLint
import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.Color
import android.media.MediaMetadataRetriever
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.FileObserver
import android.os.Handler
import android.os.Looper
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
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
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
        const val EXTRA_BACKUP_DIR = "backup_dir"
        const val EXTRA_TIDDLER_TITLE = "tiddler_title"  // For tm-open-window: navigate to specific tiddler
        const val EXTRA_FOLDER_LOCAL_PATH = "folder_local_path"  // Local filesystem path for SAF folder wikis
        private const val TAG = "WikiActivity"

        // 1x1 transparent GIF (43 bytes) — served as placeholder during boot to prevent
        // heavy SAF I/O from blocking TiddlyWiki startup. MutationObserver replaces with
        // real images via attachment ports after onPageFinished.
        private val TRANSPARENT_GIF = byteArrayOf(
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61,  // GIF89a
            0x01, 0x00, 0x01, 0x00,                // 1x1
            0x80.toByte(), 0x00, 0x00,             // GCT flag
            0x00, 0x00, 0x00,                       // color 0: black (transparent)
            0x00, 0x00, 0x00,                       // color 1: black
            0x21, 0xF9.toByte(), 0x04, 0x01,       // GCE: dispose, transparent
            0x00, 0x00, 0x00, 0x00,                 // delay=0, transparent idx=0
            0x2C, 0x00, 0x00, 0x00, 0x00,          // image descriptor
            0x01, 0x00, 0x01, 0x00, 0x00,          // 1x1, no LCT
            0x02, 0x02, 0x44, 0x01, 0x00,          // LZW min=2, block
            0x3B                                    // trailer
        )

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
         * Native method to restart a folder wiki's Node.js server.
         * Returns the new server URL on success, or empty string on failure.
         */
        @JvmStatic
        external fun restartFolderWikiServer(wikiPath: String): String

        /**
         * Native method to start a Node.js server from a local filesystem path.
         * Does NOT do SAF operations — expects wiki files already at localPath.
         * Returns the server URL on success, or "ERROR:..." on failure.
         */
        @JvmStatic
        external fun startFolderWikiServerFromLocal(localPath: String, wikiPath: String): String

        /** Native method: Open a PDF from base64 data. Returns JSON with handle + page metadata. */
        @JvmStatic
        external fun pdfOpen(dataBase64: String): String

        /** Native method: Render a PDF page as PNG. Returns JSON. */
        @JvmStatic
        external fun pdfRenderPage(handle: Long, pageNum: Int, widthPx: Int): String

        /** Native method: Close a PDF document and release its handle. */
        @JvmStatic
        external fun pdfClose(handle: Long)

        /** Native method: Hit-test character at pixel position. Returns char index or -1. */
        @JvmStatic
        external fun pdfCharAtPos(handle: Long, pageNum: Int, pixelX: Int, pixelY: Int, renderWidth: Int): Int

        /** Native method: Get selection rectangles for character range. Returns JSON array. */
        @JvmStatic
        external fun pdfSelectionRects(handle: Long, pageNum: Int, startIdx: Int, endIdx: Int, renderWidth: Int): String

        /** Native method: Extract text for character range. */
        @JvmStatic
        external fun pdfGetText(handle: Long, pageNum: Int, startIdx: Int, endIdx: Int): String

        /** Native method: Get total character count for a page. */
        @JvmStatic
        external fun pdfCharCount(handle: Long, pageNum: Int): Int

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

        /**
         * Update recent_wikis.json with external attachments setting for a wiki.
         */
        @JvmStatic
        fun updateRecentWikisExternalAttachments(context: Context, wikiPath: String, enabled: Boolean) {
            try {
                val recentWikisFile = File(context.filesDir, "recent_wikis.json")

                if (!recentWikisFile.exists()) {
                    Log.w(TAG, "recent_wikis.json does not exist, cannot update external_attachments")
                    return
                }

                val existingWikis = JSONArray(recentWikisFile.readText())
                var updated = false

                for (i in 0 until existingWikis.length()) {
                    val wiki = existingWikis.optJSONObject(i)
                    if (wiki != null && wiki.optString("path") == wikiPath) {
                        wiki.put("external_attachments", enabled)
                        updated = true
                        break
                    }
                }

                if (updated) {
                    recentWikisFile.writeText(existingWikis.toString(2))
                    Log.d(TAG, "Updated external_attachments=$enabled for wiki: $wikiPath")
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to update external_attachments in recent_wikis.json: ${e.message}")
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
    private var currentWikiUrl: String? = null
    private var httpServer: WikiHttpServer? = null

    // Fullscreen video support
    private var fullscreenView: View? = null
    private var fullscreenCallback: WebChromeClient.CustomViewCallback? = null

    // App immersive fullscreen mode (toggled via tm-full-screen)
    private var isImmersiveFullscreen: Boolean = false

    // Flag to track if this window was opened via tm-open-window
    // If true, back button returns to parent window instead of closing
    private var isChildWindow: Boolean = false

    // Track whether the page has finished loading (set in onPageFinished).
    // Used in onResume to detect stalled loads — if the page was backgrounded
    // before finishing, it often won't resume loading and needs a reload.
    @Volatile private var pageLoaded = false
    // Track whether onPause was called — avoids false reload on initial onResume
    private var wasPaused = false

    // WakeLock to keep the HTTP server alive when app is in background
    private var wakeLock: PowerManager.WakeLock? = null

    // Watchdog thread for folder wiki server — detects server death and auto-restarts
    @Volatile private var folderWatchdogRunning = false
    private var folderWatchdog: Thread? = null
    // Guard to prevent concurrent folder server restart attempts
    @Volatile private var folderServerRestarting = false
    // Set to true once the folder wiki server is confirmed running and ready
    @Volatile private var folderServerReady = false
    // Local filesystem path for SAF folder wikis (Node.js reads from here)
    private var folderLocalPath: String? = null
    // Whether the sync foreground notification is currently started by this activity
    private var notificationStarted = false

    // SyncWatcher — syncs local folder wiki changes back to SAF via FileObserver (inotify)
    @Volatile private var syncWatcherRunning = false
    private val syncFileObservers = mutableListOf<FileObserver>()
    private val pendingSyncDeletes = java.util.Collections.synchronizedList(mutableListOf<String>())
    private var syncDeleteHandler: Handler? = null
    private val syncTrackedFileCount = java.util.concurrent.atomic.AtomicInteger(0)

    // File chooser support for import functionality
    private var filePathCallback: ValueCallback<Array<Uri>>? = null
    private lateinit var fileChooserLauncher: ActivityResultLauncher<Intent>

    // WebView permission request support (camera, microphone, geolocation)
    private var pendingPermissionRequest: android.webkit.PermissionRequest? = null
    private var pendingGeolocationOrigin: String? = null
    private var pendingGeolocationCallback: android.webkit.GeolocationPermissions.Callback? = null
    private lateinit var permissionRequestLauncher: ActivityResultLauncher<Array<String>>
    private lateinit var geolocationPermissionLauncher: ActivityResultLauncher<Array<String>>

    // Export/save file support
    private lateinit var createDocumentLauncher: ActivityResultLauncher<Intent>
    private var pendingExportContent: ByteArray? = null
    private var pendingExportCallback: String? = null

    // Auth overlay WebView for session auth login
    private var authOverlayContainer: FrameLayout? = null
    private var authWebView: WebView? = null

    // Watchdog: ensures main process (LAN sync) stays alive while wiki is open.
    // The main process can be killed by Android's memory management even when
    // LanSyncService is running as a foreground service (OEM battery optimizations).
    // This watchdog detects the death and restarts the main process.
    private val mainProcessWatchdog = Handler(Looper.getMainLooper())
    private val mainProcessCheckRunnable = object : Runnable {
        override fun run() {
            if (!isDestroyed && !isFinishing) {
                checkMainProcessAlive()
                mainProcessWatchdog.postDelayed(this, 250L)
            }
        }
    }


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
         * Restart the folder wiki's Node.js server via JNI.
         * Returns the new server URL on success, or empty string on failure.
         */
        @JavascriptInterface
        fun restartFolderServer(): String {
            if (!isFolder) return ""
            return attemptFolderServerRestart()
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
         * Called from the exitFullscreen stub when video players
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
         * Get the current attachment server URL (for folder wikis — single port).
         */
        @JavascriptInterface
        fun getAttachmentServerUrl(): String {
            if (httpServer == null) return ""
            return "http://127.0.0.1:${httpServer!!.port}"
        }

        /**
         * Get the attachment port list as comma-separated string.
         * JS hashes the URL to pick a port, distributing requests across ports.
         * With 5 attachment ports + 1 main port = 36 concurrent connections.
         */
        @JavascriptInterface
        fun getAttachmentPorts(): String {
            if (httpServer == null) return ""
            return httpServer!!.attachmentPorts.joinToString(",")
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
     * JavaScript interface for sharing tiddler content via Android share sheet.
     * Shows a format picker dialog, then generates a file and shares it.
     */
    inner class ShareInterface {
        @JavascriptInterface
        fun prepareShare(title: String, fieldsJson: String, renderedHtml: String, plainText: String) {
            runOnUiThread {
                showShareFormatDialog(title, fieldsJson, renderedHtml, plainText)
            }
        }
    }

    private fun showShareFormatDialog(title: String, fieldsJson: String, renderedHtml: String, plainText: String) {
        val formats = arrayOf(
            getString(R.string.share_image),
            getString(R.string.share_plain_text),
            getString(R.string.share_json),
            getString(R.string.share_tid),
            getString(R.string.share_csv)
        )
        AlertDialog.Builder(this)
            .setTitle(getString(R.string.share_title))
            .setItems(formats) { _, which ->
                when (which) {
                    0 -> shareAsImage(title)
                    1 -> shareAsPlainText(title, plainText)
                    2 -> shareAsFile(title, generateJson(fieldsJson), "json", "application/json")
                    3 -> shareAsFile(title, generateTid(fieldsJson), "tid", "text/plain")
                    4 -> shareAsFile(title, generateCsv(fieldsJson), "csv", "text/csv")
                }
            }
            .setNegativeButton(getString(R.string.btn_cancel), null)
            .show()
    }

    private fun shareAsPlainText(title: String, text: String) {
        try {
            val intent = Intent(Intent.ACTION_SEND).apply {
                type = "text/plain"
                putExtra(Intent.EXTRA_SUBJECT, title)
                putExtra(Intent.EXTRA_TEXT, text.ifBlank { title })
            }
            startActivity(Intent.createChooser(intent, null))
        } catch (e: Exception) {
            Log.e(TAG, "Failed to share as text: ${e.message}")
        }
    }

    private fun shareAsImage(title: String) {
        // Use html-to-image library to capture the tiddler body as a JPEG.
        // Captures at 2x pixelRatio for sharp output on high-DPI screens,
        // with automatic fallback to 1x if the canvas is too large / OOM.
        // Images are downscaled to max 800px and JPEG-compressed during
        // inlining to keep the SVG foreignObject string manageable.
        val escapedTitle = title.replace("\\", "\\\\").replace("'", "\\'").replace("\n", "\\n")
        webView?.evaluateJavascript("""
            (function() {
                var title = '$escapedTitle';
                function doCapture() {
                    var bodyEl = null;
                    document.querySelectorAll('[data-tiddler-title]').forEach(function(el) {
                        if (!bodyEl && el.getAttribute('data-tiddler-title') === title) {
                            bodyEl = el;
                        }
                    });
                    if (!bodyEl) {
                        console.error('[TiddlyDesktop] shareAsImage: tiddler element not found for: ' + title);
                        TiddlyDesktopShareCapture.onCaptureFailed('Tiddler element not found');
                        return;
                    }
                    // Early size check — abort before heavy processing if
                    // the tiddler is clearly too large to capture.
                    var MAX_CAPTURE_PIXELS = 25000000; // 25M pixels
                    var estW = bodyEl.offsetWidth || 800;
                    var estH = bodyEl.scrollHeight || 600;
                    if (estW * estH > MAX_CAPTURE_PIXELS) {
                        console.error('[TiddlyDesktop] shareAsImage: tiddler too large (' + estW + 'x' + estH + ' = ' + (estW * estH) + ' pixels)');
                        TiddlyDesktopShareCapture.onCaptureTooManyPixels(estW + '\u00D7' + estH);
                        return;
                    }
                    var wrapper = document.createElement('div');
                    wrapper.style.cssText = 'overflow:hidden;background:#ffffff;padding:8px;width:' + bodyEl.offsetWidth + 'px;';
                    // CSS is NOT embedded — html-to-image inlines computed styles
                    // per-element. Embedding all stylesheets added 500KB-2MB to the
                    // SVG foreignObject and URL-encoding tripled that.

                    var clone = bodyEl.cloneNode(true);
                    clone.style.margin = '0';
                    clone.querySelectorAll('embed, object').forEach(function(el) {
                        if (el.style.display === 'none') {
                            el.remove();
                        } else {
                            var tp = (el.getAttribute('type') || '').toLowerCase();
                            var src = el.getAttribute('data') || el.getAttribute('src') || '';
                            if (tp === 'application/pdf' || /\.pdf${'$'}/i.test(src)) {
                                var ph = document.createElement('div');
                                ph.style.cssText = 'background:#525659;color:#fff;padding:40px 20px;text-align:center;font:16px sans-serif;border-radius:4px;';
                                ph.textContent = 'PDF: ' + (src.split('/').pop() || 'document');
                                if (el.parentNode) el.parentNode.replaceChild(ph, el);
                            } else {
                                var eph = document.createElement('div');
                                var eName = src ? decodeURIComponent(src.split('/').pop().split('?')[0]) : (tp || 'Embedded content');
                                eph.style.cssText = 'background:#525659;color:#fff;padding:40px 20px;text-align:center;font:16px sans-serif;border-radius:4px;';
                                eph.textContent = eName;
                                if (el.parentNode) el.parentNode.replaceChild(eph, el);
                            }
                        }
                    });
                    clone.querySelectorAll('iframe').forEach(function(el) {
                        el.removeAttribute('src');
                        el.removeAttribute('data');
                    });
                    wrapper.appendChild(clone);
                    // Do NOT append wrapper to DOM yet — cloned images still have
                    // server URLs and appending would trigger 50+ full-resolution
                    // fetches. We append after inlineImages() replaces them with
                    // compressed data URLs.

                    function placeholder(w, h) {
                        var d = document.createElement('div');
                        d.style.cssText = 'width:' + w + 'px;height:' + h + 'px;background:#e8e8e8;display:block;';
                        return d;
                    }

                    function fixBlockSpacing() {
                        var origChildren = bodyEl.children;
                        var cloneChildren = clone.children;
                        for (var bi = 0; bi < origChildren.length && bi < cloneChildren.length; bi++) {
                            var ocs = window.getComputedStyle(origChildren[bi]);
                            var display = ocs.display;
                            if (display === 'block' || display === 'flex' || display === 'grid' ||
                                display === 'list-item' || display === 'flow-root') {
                                cloneChildren[bi].style.marginTop = ocs.marginTop;
                                cloneChildren[bi].style.marginBottom = ocs.marginBottom;
                                cloneChildren[bi].style.paddingTop = ocs.paddingTop;
                                cloneChildren[bi].style.paddingBottom = ocs.paddingBottom;
                            }
                        }
                    }

                    function fixLayout() {
                        var origAll = bodyEl.querySelectorAll('*');
                        var cloneAll = clone.querySelectorAll('*');
                        for (var i = 0; i < origAll.length && i < cloneAll.length; i++) {
                            var orig = origAll[i];
                            var cl = cloneAll[i];
                            var cs = window.getComputedStyle(orig);
                            var display = cs.display;
                            if (display === 'inline-block' || display === 'inline-flex' ||
                                display === 'inline-grid' || orig.tagName === 'BUTTON') {
                                cl.style.minWidth = orig.offsetWidth + 'px';
                                cl.style.minHeight = orig.offsetHeight + 'px';
                                if (cs.whiteSpace === 'normal') cl.style.whiteSpace = 'nowrap';
                            }
                        }
                    }

                    // Adaptive image quality based on count. More images →
                    // smaller dimensions + lower quality to avoid OOM.
                    // Adaptive image quality based on count. More images →
                    // smaller dimensions + lower quality to keep the SVG
                    // foreignObject data URL under the WebView renderer limit.
                    var imgCount = bodyEl.querySelectorAll('img').length;
                    var MAX_IMG_DIM, imgQuality, forceRatio1 = false;
                    if (imgCount > 50) {
                        MAX_IMG_DIM = 200; imgQuality = 0.3; forceRatio1 = true;
                    } else if (imgCount > 30) {
                        MAX_IMG_DIM = 400; imgQuality = 0.5; forceRatio1 = true;
                    } else if (imgCount > 15) {
                        MAX_IMG_DIM = 600; imgQuality = 0.6;
                    } else {
                        MAX_IMG_DIM = 800; imgQuality = 0.7;
                    }
                    console.log('[TiddlyDesktop] shareAsImage: imgCount=' + imgCount + ' MAX_IMG_DIM=' + MAX_IMG_DIM + ' imgQuality=' + imgQuality + ' forceRatio1=' + forceRatio1);
                    function inlineImages() {
                        var origImgs = bodyEl.querySelectorAll('img');
                        var cloneImgs = clone.querySelectorAll('img');
                        var promises = [];
                        for (var i = 0; i < origImgs.length && i < cloneImgs.length; i++) {
                            (function(origImg, cloneImg) {
                                var src = origImg.src || '';
                                if (!src || src.startsWith('data:')) return;
                                var p = new Promise(function(resolve) {
                                    function tryInline() {
                                        var nw = origImg.naturalWidth;
                                        var nh = origImg.naturalHeight;
                                        if (nw > 0 && nh > 0) {
                                            try {
                                                var scale = Math.min(1, MAX_IMG_DIM / Math.max(nw, nh));
                                                var c = document.createElement('canvas');
                                                c.width = Math.round(nw * scale);
                                                c.height = Math.round(nh * scale);
                                                c.getContext('2d').drawImage(origImg, 0, 0, c.width, c.height);
                                                var isSvg = (origImg.src || '').indexOf('.svg') !== -1 ||
                                                    (origImg.getAttribute('type') || '').indexOf('svg') !== -1;
                                                if (isSvg) {
                                                    cloneImg.setAttribute('src', c.toDataURL('image/png'));
                                                } else {
                                                    cloneImg.setAttribute('src', c.toDataURL('image/jpeg', imgQuality));
                                                }
                                                cloneImg.removeAttribute('srcset');
                                                // Release canvas backing store immediately
                                                c.width = 0; c.height = 0;
                                            } catch(e) { /* tainted canvas (cross-origin) */ }
                                        } else if (cloneImg.parentNode) {
                                            var w = origImg.offsetWidth || origImg.width || 100;
                                            var h = origImg.offsetHeight || origImg.height || 100;
                                            cloneImg.parentNode.replaceChild(placeholder(w, h), cloneImg);
                                        }
                                        resolve();
                                    }
                                    if (origImg.complete) {
                                        tryInline();
                                    } else {
                                        var done = false;
                                        origImg.addEventListener('load', function() {
                                            if (!done) { done = true; tryInline(); }
                                        });
                                        origImg.addEventListener('error', function() {
                                            if (!done) {
                                                done = true;
                                                if (cloneImg.parentNode) {
                                                    var w = origImg.offsetWidth || origImg.width || 100;
                                                    var h = origImg.offsetHeight || origImg.height || 100;
                                                    cloneImg.parentNode.replaceChild(placeholder(w, h), cloneImg);
                                                }
                                                resolve();
                                            }
                                        });
                                        setTimeout(function() {
                                            if (!done) { done = true; tryInline(); }
                                        }, 5000);
                                    }
                                });
                                promises.push(p);
                            })(origImgs[i], cloneImgs[i]);
                        }
                        return Promise.all(promises);
                    }

                    // Convert canvas elements to compressed JPEG images
                    function convertCanvases() {
                        var origCanvases = bodyEl.querySelectorAll('canvas');
                        var cloneCanvases = clone.querySelectorAll('canvas');
                        for (var i = 0; i < origCanvases.length && i < cloneCanvases.length; i++) {
                            var oc = origCanvases[i];
                            var cc = cloneCanvases[i];
                            if (!cc.parentNode) continue;
                            try {
                                if (oc.width > 0 && oc.height > 0) {
                                    // Downscale large canvases (e.g. PDF pages)
                                    var cScale = Math.min(1, 400 / Math.max(oc.width, oc.height));
                                    var tc = document.createElement('canvas');
                                    tc.width = Math.round(oc.width * cScale);
                                    tc.height = Math.round(oc.height * cScale);
                                    tc.getContext('2d').drawImage(oc, 0, 0, tc.width, tc.height);
                                    var img = document.createElement('img');
                                    img.src = tc.toDataURL('image/jpeg', 0.5);
                                    tc.width = 0; tc.height = 0;
                                    img.style.cssText = 'width:' + oc.offsetWidth + 'px;height:' + oc.offsetHeight + 'px;display:block;';
                                    cc.parentNode.replaceChild(img, cc);
                                } else {
                                    cc.parentNode.replaceChild(placeholder(oc.offsetWidth || 100, oc.offsetHeight || 100), cc);
                                }
                            } catch(e) {
                                cc.parentNode.replaceChild(placeholder(oc.offsetWidth || 100, oc.offsetHeight || 100), cc);
                            }
                        }
                    }

                    function cleanupMedia() {
                        var origVideos = bodyEl.querySelectorAll('video');
                        var cloneVideos = clone.querySelectorAll('video');
                        var posterPromises = [];
                        for (var i = 0; i < origVideos.length && i < cloneVideos.length; i++) {
                            var ov = origVideos[i];
                            var cv = cloneVideos[i];
                            if (!cv.parentNode) continue;
                            var posterUrl = ov.poster || '';
                            if (!posterUrl && ov.readyState >= 2 && ov.videoWidth > 0) {
                                try {
                                    var vc = document.createElement('canvas');
                                    vc.width = ov.videoWidth;
                                    vc.height = ov.videoHeight;
                                    vc.getContext('2d').drawImage(ov, 0, 0);
                                    posterUrl = vc.toDataURL();
                                } catch(e) {}
                            }
                            if (posterUrl) {
                                var vImg = document.createElement('img');
                                vImg.style.cssText = 'width:' + (ov.offsetWidth || 320) + 'px;height:' + (ov.offsetHeight || 180) + 'px;object-fit:contain;display:block;';
                                if (posterUrl.indexOf('data:') === 0) {
                                    vImg.src = posterUrl;
                                } else {
                                    (function(img, url) {
                                        posterPromises.push(new Promise(function(resolve) {
                                            var ti = new Image();
                                            ti.crossOrigin = 'anonymous';
                                            var done = false;
                                            ti.onload = function() {
                                                if (done) return; done = true;
                                                try {
                                                    var tc = document.createElement('canvas');
                                                    tc.width = ti.naturalWidth;
                                                    tc.height = ti.naturalHeight;
                                                    tc.getContext('2d').drawImage(ti, 0, 0);
                                                    img.src = tc.toDataURL('image/jpeg', 0.9);
                                                } catch(e) {
                                                    img.src = url;
                                                }
                                                resolve();
                                            };
                                            ti.onerror = function() {
                                                if (done) return; done = true;
                                                img.src = url;
                                                resolve();
                                            };
                                            setTimeout(function() {
                                                if (done) return; done = true;
                                                img.src = url;
                                                resolve();
                                            }, 5000);
                                            ti.src = url;
                                        }));
                                    })(vImg, posterUrl);
                                }
                                cv.parentNode.replaceChild(vImg, cv);
                            } else {
                                cv.parentNode.replaceChild(placeholder(ov.offsetWidth || 320, ov.offsetHeight || 180), cv);
                            }
                        }
                        // Audio elements: leave in clone as-is. html-to-image
                        // renders via SVG foreignObject, and the browser renders
                        // native <audio controls> in that context.
                        var iframePromises = [];
                        var origIframes = bodyEl.querySelectorAll('iframe');
                        var cloneIframes = clone.querySelectorAll('iframe');
                        for (var ii = 0; ii < origIframes.length && ii < cloneIframes.length; ii++) {
                            (function(origIf, cloneIf) {
                                var ifSrc = origIf.src || origIf.getAttribute('src') || '';
                                if (!ifSrc || ifSrc.indexOf('http://127.0.0.1') === 0) return;
                                var iw = origIf.offsetWidth || 480;
                                var ih = origIf.offsetHeight || 360;
                                var ytMatch = ifSrc.match(/youtube(?:-nocookie)?\.com\/embed\/([a-zA-Z0-9_-]+)/);
                                if (ytMatch && ytMatch[1]) {
                                    var thumbUrl = 'https://img.youtube.com/vi/' + ytMatch[1] + '/hqdefault.jpg';
                                    var rep = document.createElement('img');
                                    rep.style.cssText = 'width:' + iw + 'px;height:' + ih + 'px;object-fit:cover;display:block;background:#000;';
                                    if (cloneIf.parentNode) cloneIf.parentNode.replaceChild(rep, cloneIf);
                                    var p = new Promise(function(resolve) {
                                        var ti = new Image();
                                        ti.crossOrigin = 'anonymous';
                                        var done = false;
                                        ti.onload = function() {
                                            if (done) return; done = true;
                                            try {
                                                var tc = document.createElement('canvas');
                                                tc.width = ti.naturalWidth;
                                                tc.height = ti.naturalHeight;
                                                tc.getContext('2d').drawImage(ti, 0, 0);
                                                rep.src = tc.toDataURL();
                                            } catch(e) {
                                                if (rep.parentNode) rep.parentNode.replaceChild(placeholder(iw, ih), rep);
                                            }
                                            resolve();
                                        };
                                        ti.onerror = function() {
                                            if (done) return; done = true;
                                            if (rep.parentNode) rep.parentNode.replaceChild(placeholder(iw, ih), rep);
                                            resolve();
                                        };
                                        setTimeout(function() {
                                            if (done) return; done = true;
                                            if (rep.parentNode) rep.parentNode.replaceChild(placeholder(iw, ih), rep);
                                            resolve();
                                        }, 5000);
                                        ti.src = thumbUrl;
                                    });
                                    iframePromises.push(p);
                                } else {
                                    if (cloneIf.parentNode) cloneIf.parentNode.replaceChild(placeholder(iw, ih), cloneIf);
                                }
                            })(origIframes[ii], cloneIframes[ii]);
                        }
                        return Promise.all(iframePromises.concat(posterPromises));
                    }

                    var captureOpts = {
                        quality: 0.85,
                        backgroundColor: '#ffffff',
                        pixelRatio: 2,
                        imagePlaceholder: 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVQI12NgAAIABQABNjN9GQAAAAlwSFlzAAAWJQAAFiUBSVIk8AAAAA0lEQVQI12P4z8BQDwAEgAF/QualzQAAAABJRU5ErkJggg==',
                        filter: function(node) {
                            try {
                                if (!(node instanceof HTMLElement)) return true;
                                if (node.tagName === 'STYLE') return true;
                                if (node.hidden || node.getAttribute('hidden') === 'true' || node.getAttribute('hidden') === '') return false;
                                var style = window.getComputedStyle(node);
                                if (style.display === 'none' || style.visibility === 'hidden') return false;
                                if (node.tagName === 'IMG' && node.naturalWidth === 0 && node.complete) return false;
                                if (node.tagName === 'IFRAME') return false;
                                return true;
                            } catch(e) { return true; }
                        }
                    };

                    function fixPdfContainers() {
                        clone.querySelectorAll('.td-pdf-container embed, .td-pdf-container object').forEach(function(el) {
                            el.remove();
                        });
                        clone.querySelectorAll('.td-pdf-text-layer').forEach(function(tl) {
                            tl.remove();
                        });
                        clone.querySelectorAll('.td-pdf-container').forEach(function(pc) {
                            pc.style.overflow = 'visible';
                            var tb = pc.querySelector('div[style*="sticky"]');
                            if (tb) tb.style.position = 'static';
                            var pw = pc.querySelector('.td-pdf-pages-wrap');
                            if (pw) { pw.style.maxHeight = 'none'; pw.style.overflow = 'visible'; }
                            pc.querySelectorAll('.td-pdf-page-wrap').forEach(function(wrap) {
                                if (!wrap.querySelector('img')) {
                                    wrap.style.display = 'none';
                                    wrap.style.minHeight = '0';
                                }
                            });
                        });
                        var MAX_PAGE_W = 800;
                        var origPdfImgs = bodyEl.querySelectorAll('.td-pdf-page-wrap img');
                        var clonePdfImgs = clone.querySelectorAll('.td-pdf-page-wrap img');
                        for (var pi = 0; pi < origPdfImgs.length && pi < clonePdfImgs.length; pi++) {
                            try {
                                var opi = origPdfImgs[pi];
                                var cpi = clonePdfImgs[pi];
                                if (opi.naturalWidth > 0 && opi.naturalHeight > 0) {
                                    var scale = Math.min(1, MAX_PAGE_W / opi.naturalWidth);
                                    var w = Math.round(opi.naturalWidth * scale);
                                    var h = Math.round(opi.naturalHeight * scale);
                                    var pc2 = document.createElement('canvas');
                                    pc2.width = w;
                                    pc2.height = h;
                                    pc2.getContext('2d').drawImage(opi, 0, 0, w, h);
                                    cpi.src = pc2.toDataURL('image/jpeg', 0.6);
                                    cpi.style.width = '100%';
                                }
                            } catch(e) {}
                        }
                    }

                    // Early complexity check — html-to-image clones the DOM,
                    // computes styles for every element, decodes all images,
                    // and renders via SVG foreignObject. This all happens in
                    // the WebView renderer process which has limited memory.
                    // Too many elements + images causes renderer OOM kill.
                    var elementCount = clone.querySelectorAll('*').length;
                    console.log('[TiddlyDesktop] shareAsImage: pre-check elements=' + elementCount + ' imgCount=' + imgCount);
                    if (elementCount + imgCount * 3 > 500) {
                        console.error('[TiddlyDesktop] shareAsImage: too complex (elements=' + elementCount + ', images=' + imgCount + '), aborting');
                        TiddlyDesktopShareCapture.onCaptureTooLarge(elementCount + ' elements, ' + imgCount + ' images');
                        return;
                    }

                    fixBlockSpacing();
                    fixLayout();
                    fixPdfContainers();
                    inlineImages().then(function() {
                        convertCanvases();
                        return cleanupMedia();
                    }).then(function() {
                        // Now that all images are compressed data URLs, append to
                        // DOM. No server fetches will be triggered.
                        document.body.appendChild(wrapper);
                        // Wait for images to decode and layout to settle
                        return new Promise(function(resolve) {
                            requestAnimationFrame(function() {
                                requestAnimationFrame(function() { resolve(); });
                            });
                        });
                    }).then(function() {
                        // Pre-capture canvas size check.
                        var wrapW = wrapper.offsetWidth;
                        var wrapH = wrapper.offsetHeight;
                        var ratio = forceRatio1 ? 1 : 2;
                        if (wrapW * ratio * wrapH * ratio > MAX_CAPTURE_PIXELS) {
                            ratio = 1;
                        }
                        if (wrapW * ratio * wrapH * ratio > MAX_CAPTURE_PIXELS) {
                            wrapper.remove();
                            console.error('[TiddlyDesktop] shareAsImage: tiddler too large (' + wrapW + 'x' + wrapH + ')');
                            TiddlyDesktopShareCapture.onCaptureTooManyPixels(wrapW + '\u00D7' + wrapH);
                            return;
                        }
                        console.log('[TiddlyDesktop] shareAsImage: size=' + wrapW + 'x' + wrapH + ' pixelRatio=' + ratio);
                        captureOpts.pixelRatio = ratio;
                        return htmlToImage.toJpeg(wrapper, captureOpts).then(function(dataUrl) {
                            wrapper.remove();
                            console.log('[TiddlyDesktop] shareAsImage: captured at ' + ratio + 'x, length=' + dataUrl.length);
                            TiddlyDesktopShareCapture.onImageReady(title, dataUrl);
                        }).catch(function(err) {
                            if (ratio > 1) {
                                console.warn('[TiddlyDesktop] shareAsImage: ' + ratio + 'x failed, retrying at 1x:', err);
                                captureOpts.pixelRatio = 1;
                                return htmlToImage.toJpeg(wrapper, captureOpts).then(function(dataUrl) {
                                    wrapper.remove();
                                    console.log('[TiddlyDesktop] shareAsImage: captured at 1x fallback, length=' + dataUrl.length);
                                    TiddlyDesktopShareCapture.onImageReady(title, dataUrl);
                                }).catch(function(err2) {
                                    wrapper.remove();
                                    console.error('[TiddlyDesktop] shareAsImage: capture failed at both resolutions:', err2);
                                    if (TiddlyDesktopShareCapture.onCaptureFailed) {
                                        TiddlyDesktopShareCapture.onCaptureFailed(
                                            'Image capture failed: ' + (err2 && err2.message ? err2.message : String(err2))
                                        );
                                    }
                                });
                            } else {
                                wrapper.remove();
                                console.error('[TiddlyDesktop] shareAsImage: capture failed:', err);
                                if (TiddlyDesktopShareCapture.onCaptureFailed) {
                                    TiddlyDesktopShareCapture.onCaptureFailed(
                                        'Image capture failed: ' + (err && err.message ? err.message : String(err))
                                    );
                                }
                            }
                        });
                    });
                }
                if (typeof htmlToImage !== 'undefined') {
                    doCapture();
                } else {
                    var s = document.createElement('script');
                    s.src = '/_td/html-to-image.js';
                    s.onload = doCapture;
                    s.onerror = function() {
                        console.error('[TiddlyDesktop] Failed to load html-to-image.js');
                        if (TiddlyDesktopShareCapture.onCaptureFailed) {
                            TiddlyDesktopShareCapture.onCaptureFailed('Failed to load image capture library');
                        }
                    };
                    document.head.appendChild(s);
                }
            })();
        """.trimIndent(), null)
    }

    /**
     * JavaScript interface to receive captured image data from html-to-image.
     */
    inner class ShareCaptureInterface {
        @JavascriptInterface
        fun onImageReady(title: String, dataUrl: String) {
            try {
                val base64Data = dataUrl.substringAfter("base64,")
                val imageBytes = android.util.Base64.decode(base64Data, android.util.Base64.DEFAULT)
                Log.d(TAG, "shareAsImage: received ${imageBytes.size} bytes for '$title'")

                val shareDir = File(cacheDir, "shared")
                shareDir.mkdirs()
                shareDir.listFiles()?.forEach { it.delete() }

                val safeName = title.replace(Regex("[^\\w\\s.-]"), "_").take(80).trim()
                val file = File(shareDir, "${safeName}.jpg")
                file.writeBytes(imageBytes)
                Log.d(TAG, "shareAsImage: JPEG saved, size=${file.length()}")

                val uri = FileProvider.getUriForFile(
                    this@WikiActivity,
                    "${packageName}.fileprovider",
                    file
                )

                val intent = Intent(Intent.ACTION_SEND).apply {
                    type = "image/jpeg"
                    putExtra(Intent.EXTRA_STREAM, uri)
                    putExtra(Intent.EXTRA_SUBJECT, title)
                    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                }
                runOnUiThread {
                    startActivity(Intent.createChooser(intent, null))
                }
            } catch (t: Throwable) {
                Log.e(TAG, "Failed to share image: ${t.message}", t)
                runOnUiThread {
                    Toast.makeText(this@WikiActivity, getString(R.string.share_image_failed), Toast.LENGTH_LONG).show()
                }
            }
        }

        @JavascriptInterface
        fun onCaptureFailed(message: String) {
            Log.e(TAG, "shareAsImage capture failed: $message")
            runOnUiThread {
                Toast.makeText(this@WikiActivity, getString(R.string.share_image_failed), Toast.LENGTH_LONG).show()
            }
        }

        @JavascriptInterface
        fun onCaptureTooLarge(detail: String) {
            Log.e(TAG, "shareAsImage: content too large: $detail")
            runOnUiThread {
                Toast.makeText(this@WikiActivity, getString(R.string.share_image_too_large, detail), Toast.LENGTH_LONG).show()
            }
        }

        @JavascriptInterface
        fun onCaptureTooManyPixels(detail: String) {
            Log.e(TAG, "shareAsImage: too many pixels: $detail")
            runOnUiThread {
                Toast.makeText(this@WikiActivity, getString(R.string.share_image_too_many_pixels, detail), Toast.LENGTH_LONG).show()
            }
        }

    }

    private fun shareAsFile(title: String, content: String, extension: String, mimeType: String) {
        try {
            val shareDir = File(cacheDir, "shared")
            shareDir.mkdirs()
            // Clean old shared files
            shareDir.listFiles()?.forEach { it.delete() }

            val safeName = title.replace(Regex("[^\\w\\s.-]"), "_").take(80).trim()
            val file = File(shareDir, "${safeName}.${extension}")
            file.writeText(content, Charsets.UTF_8)

            val uri = FileProvider.getUriForFile(this,
                "${packageName}.fileprovider", file)

            val intent = Intent(Intent.ACTION_SEND).apply {
                type = mimeType
                putExtra(Intent.EXTRA_STREAM, uri)
                putExtra(Intent.EXTRA_SUBJECT, title)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            startActivity(Intent.createChooser(intent, null))
        } catch (e: Exception) {
            Log.e(TAG, "Failed to share as file: ${e.message}")
        }
    }

    private fun generateJson(fieldsJson: String): String {
        return try {
            // fieldsJson is already a JSON object of all fields — wrap in array
            val obj = JSONObject(fieldsJson)
            val arr = JSONArray().put(obj)
            arr.toString(2)
        } catch (e: Exception) {
            fieldsJson
        }
    }

    private fun generateTid(fieldsJson: String): String {
        return try {
            val obj = JSONObject(fieldsJson)
            buildString {
                val keys = obj.keys().asSequence().sorted().toList()
                // Write all fields except "text" as headers
                for (key in keys) {
                    if (key == "text") continue
                    appendLine("${key}: ${obj.optString(key, "")}")
                }
                appendLine()
                // Write text as body
                append(obj.optString("text", ""))
            }
        } catch (e: Exception) {
            fieldsJson
        }
    }

    private fun generateCsv(fieldsJson: String): String {
        return try {
            val obj = JSONObject(fieldsJson)
            val keys = obj.keys().asSequence().sorted().toList()
            buildString {
                // Header row
                appendLine(keys.joinToString(",") { csvEscape(it) })
                // Value row
                appendLine(keys.joinToString(",") { csvEscape(obj.optString(it, "")) })
            }
        } catch (e: Exception) {
            fieldsJson
        }
    }

    private fun csvEscape(value: String): String {
        return if (value.contains(',') || value.contains('"') || value.contains('\n')) {
            "\"${value.replace("\"", "\"\"")}\""
        } else {
            value
        }
    }

    /**
     * JavaScript interface for persisting wiki config changes to recent_wikis.json.
     */
    inner class ConfigInterface {
        @JavascriptInterface
        fun setExternalAttachments(enabled: Boolean) {
            val path = wikiPath ?: return
            Thread {
                updateRecentWikisExternalAttachments(applicationContext, path, enabled)
            }.start()
        }
    }

    inner class SyncInterface {
        private fun bridgePort(): Int {
            try {
                val dataDir = applicationContext.dataDir
                val portFile = java.io.File(dataDir, "sync_bridge_port")
                if (portFile.exists()) {
                    return portFile.readText().trim().toIntOrNull() ?: 0
                }
            } catch (_: Exception) {}
            return 0
        }

        @JavascriptInterface
        fun getBridgePort(): Int = bridgePort()

        @JavascriptInterface
        fun getSyncId(wikiPath: String): String {
            val port = bridgePort()
            if (port <= 0) {
                android.util.Log.i("TiddlyDesktopSync", "getSyncId: bridgePort=$port (not ready), returning empty")
                return ""
            }
            return try {
                val url = java.net.URL("http://127.0.0.1:$port/_bridge/sync-id?path=${java.net.URLEncoder.encode(wikiPath, "UTF-8")}")
                val conn = url.openConnection() as java.net.HttpURLConnection
                conn.connectTimeout = 2000
                conn.readTimeout = 2000
                val body = conn.inputStream.bufferedReader().readText()
                conn.disconnect()
                val syncId = org.json.JSONObject(body).optString("sync_id", "")
                if (syncId.isNotEmpty()) {
                    android.util.Log.i("TiddlyDesktopSync", "getSyncId: path=$wikiPath -> syncId=$syncId")
                }
                syncId
            } catch (e: Exception) {
                android.util.Log.e("TiddlyDesktopSync", "getSyncId error for path=$wikiPath port=$port: ${e.message}")
                ""
            }
        }

        @JavascriptInterface
        fun tiddlerChanged(wikiId: String, title: String, tiddlerJson: String) {
            Thread {
                bridgePost("/_bridge/tiddler-changed", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("title", title)
                    put("tiddler_json", tiddlerJson)
                })
            }.start()
        }

        @JavascriptInterface
        fun tiddlerDeleted(wikiId: String, title: String) {
            Thread {
                bridgePost("/_bridge/tiddler-deleted", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("title", title)
                })
            }.start()
        }

        @JavascriptInterface
        fun wikiOpened(wikiId: String) {
            Thread {
                bridgePost("/_bridge/wiki-opened", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                })
            }.start()
        }

        @JavascriptInterface
        fun sendFullSyncBatch(wikiId: String, toDeviceId: String, tiddlersJson: String, isLastBatch: Boolean) {
            android.util.Log.i("TiddlyDesktopSync", "sendFullSyncBatch called: tiddlersJson.length=${tiddlersJson.length}, isLastBatch=$isLastBatch, toDeviceId=$toDeviceId")
            Thread {
                try {
                    val payload = org.json.JSONObject().apply {
                        put("wiki_id", wikiId)
                        put("to_device_id", toDeviceId)
                        put("tiddlers", org.json.JSONArray(tiddlersJson))
                        put("is_last_batch", isLastBatch)
                    }
                    android.util.Log.i("TiddlyDesktopSync", "sendFullSyncBatch: payload.toString().length=${payload.toString().length}")
                    bridgePost("/_bridge/full-sync-batch", payload)
                    android.util.Log.i("TiddlyDesktopSync", "sendFullSyncBatch: bridgePost completed")
                } catch (e: Exception) {
                    android.util.Log.e("TiddlyDesktopSync", "sendFullSyncBatch thread error: ${e.message}", e)
                }
            }.start()
        }

        @JavascriptInterface
        fun sendFingerprints(wikiId: String, toDeviceId: String, fingerprintsJson: String) {
            Thread {
                bridgePost("/_bridge/send-fingerprints", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("to_device_id", toDeviceId)
                    put("fingerprints", org.json.JSONArray(fingerprintsJson))
                })
            }.start()
        }

        @JavascriptInterface
        fun broadcastFingerprints(wikiId: String, fingerprintsJson: String) {
            Thread {
                bridgePost("/_bridge/broadcast-fingerprints", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("fingerprints", org.json.JSONArray(fingerprintsJson))
                })
            }.start()
        }

        @JavascriptInterface
        fun pollChanges(wikiId: String): String {
            val port = bridgePort()
            if (port <= 0) return "[]"
            return try {
                val url = java.net.URL("http://127.0.0.1:$port/_bridge/poll?wiki_id=${java.net.URLEncoder.encode(wikiId, "UTF-8")}")
                val conn = url.openConnection() as java.net.HttpURLConnection
                conn.connectTimeout = 1000
                conn.readTimeout = 1000
                val body = conn.inputStream.bufferedReader().readText()
                conn.disconnect()
                body
            } catch (_: Exception) { "[]" }
        }

        private fun bridgePost(endpoint: String, payload: org.json.JSONObject) {
            val port = bridgePort()
            if (port <= 0) return
            try {
                val url = java.net.URL("http://127.0.0.1:$port$endpoint")
                val conn = url.openConnection() as java.net.HttpURLConnection
                conn.requestMethod = "POST"
                conn.setRequestProperty("Content-Type", "application/json")
                conn.doOutput = true
                conn.connectTimeout = 30000
                conn.readTimeout = 30000
                val body = payload.toString()
                conn.outputStream.bufferedWriter().use { it.write(body) }
                val code = conn.responseCode
                if (code != 200) {
                    android.util.Log.e("TiddlyDesktopSync", "bridgePost $endpoint: HTTP $code (body ${body.length} bytes)")
                }
                conn.disconnect()
            } catch (e: Exception) {
                android.util.Log.e("TiddlyDesktopSync", "bridgePost $endpoint failed: ${e.message} (payload ${payload.toString().length} bytes)")
            }
        }

        @JavascriptInterface
        fun loadTombstones(wikiId: String): String {
            return try {
                val dir = File(filesDir, "lan_sync_tombstones")
                val safeName = wikiId.replace(Regex("[^a-zA-Z0-9_-]"), "_")
                val file = File(dir, "$safeName.json")
                if (file.exists()) file.readText() else "{}"
            } catch (_: Exception) { "{}" }
        }

        @JavascriptInterface
        fun saveTombstones(wikiId: String, tombstonesJson: String) {
            try {
                val dir = File(filesDir, "lan_sync_tombstones")
                dir.mkdirs()
                val safeName = wikiId.replace(Regex("[^a-zA-Z0-9_-]"), "_")
                File(dir, "$safeName.json").writeText(tombstonesJson)
            } catch (e: Exception) {
                android.util.Log.e("TiddlyDesktopSync", "saveTombstones failed: ${e.message}")
            }
        }

        // ── Collaborative editing ─────────────────────────────────────

        @JavascriptInterface
        fun collabEditingStarted(wikiId: String, tiddlerTitle: String) {
            Thread {
                bridgePost("/_bridge/collab-editing-started", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("tiddler_title", tiddlerTitle)
                })
            }.start()
        }

        @JavascriptInterface
        fun collabEditingStopped(wikiId: String, tiddlerTitle: String) {
            Thread {
                bridgePost("/_bridge/collab-editing-stopped", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("tiddler_title", tiddlerTitle)
                })
            }.start()
        }

        @JavascriptInterface
        fun collabPeerSaved(wikiId: String, tiddlerTitle: String, savedTitle: String) {
            Thread {
                bridgePost("/_bridge/collab-peer-saved", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("tiddler_title", tiddlerTitle)
                    put("saved_title", savedTitle)
                })
            }.start()
        }

        @JavascriptInterface
        fun collabUpdate(wikiId: String, tiddlerTitle: String, updateBase64: String) {
            Thread {
                bridgePost("/_bridge/collab-update", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("tiddler_title", tiddlerTitle)
                    put("update_base64", updateBase64)
                })
            }.start()
        }

        @JavascriptInterface
        fun collabAwareness(wikiId: String, tiddlerTitle: String, updateBase64: String) {
            Thread {
                bridgePost("/_bridge/collab-awareness", org.json.JSONObject().apply {
                    put("wiki_id", wikiId)
                    put("tiddler_title", tiddlerTitle)
                    put("update_base64", updateBase64)
                })
            }.start()
        }

        @JavascriptInterface
        fun getRemoteEditors(wikiId: String, tiddlerTitle: String): String {
            val port = bridgePort()
            if (port <= 0) return "[]"
            return try {
                val url = java.net.URL("http://127.0.0.1:$port/_bridge/collab-editors?wiki_id=${java.net.URLEncoder.encode(wikiId, "UTF-8")}&tiddler_title=${java.net.URLEncoder.encode(tiddlerTitle, "UTF-8")}")
                val conn = url.openConnection() as java.net.HttpURLConnection
                conn.connectTimeout = 2000
                conn.readTimeout = 2000
                val body = conn.inputStream.bufferedReader().readText()
                conn.disconnect()
                body
            } catch (_: Exception) { "[]" }
        }

        // ── Peer status ─────────────────────────────────────────────────

        @JavascriptInterface
        fun announceUsername(userName: String) {
            Thread {
                bridgePost("/_bridge/announce-username", org.json.JSONObject().apply {
                    put("user_name", userName)
                })
            }.start()
        }

        @JavascriptInterface
        fun getSyncStatus(): String {
            val port = bridgePort()
            if (port <= 0) return "{}"
            return try {
                val url = java.net.URL("http://127.0.0.1:$port/_bridge/sync-status")
                val conn = url.openConnection() as java.net.HttpURLConnection
                conn.connectTimeout = 2000
                conn.readTimeout = 2000
                val body = conn.inputStream.bufferedReader().readText()
                conn.disconnect()
                body
            } catch (_: Exception) { "{}" }
        }

        @JavascriptInterface
        fun setRelayConnected(connected: Boolean) {
            this@WikiActivity.runOnUiThread {
                if (connected && !notificationStarted) {
                    try {
                        WikiServerService.startService(applicationContext)
                        notificationStarted = true
                        Log.d(TAG, "Started foreground service (relay connected)")
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to start foreground service: ${e.message}")
                    }
                } else if (!connected && notificationStarted) {
                    WikiServerService.wikiClosed(applicationContext)
                    notificationStarted = false
                    Log.d(TAG, "Stopped foreground service (relay disconnected)")
                }
            }
        }
    }

    /**
     * JavaScript interface for native PDF rendering via PDFium.
     * Accessible from all frames in the WebView as "TiddlyDesktopPdf".
     */
    inner class PdfInterface {
        @JavascriptInterface
        fun open(dataBase64: String): String {
            return pdfOpen(dataBase64)
        }

        @JavascriptInterface
        fun renderPage(handle: Long, pageNum: Int, widthPx: Int): String {
            return pdfRenderPage(handle, pageNum, widthPx)
        }

        @JavascriptInterface
        fun close(handle: Long) {
            pdfClose(handle)
        }

        @JavascriptInterface
        fun charAtPos(handle: Long, pageNum: Int, pixelX: Int, pixelY: Int, renderWidth: Int): Int {
            return pdfCharAtPos(handle, pageNum, pixelX, pixelY, renderWidth)
        }

        @JavascriptInterface
        fun selectionRects(handle: Long, pageNum: Int, startIdx: Int, endIdx: Int, renderWidth: Int): String {
            return pdfSelectionRects(handle, pageNum, startIdx, endIdx, renderWidth)
        }

        @JavascriptInterface
        fun getText(handle: Long, pageNum: Int, startIdx: Int, endIdx: Int): String {
            return pdfGetText(handle, pageNum, startIdx, endIdx)
        }

        @JavascriptInterface
        fun charCount(handle: Long, pageNum: Int): Int {
            return pdfCharCount(handle, pageNum)
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
     * Create the editor toolbar with Save (✓) and Cancel (✕) buttons.
     * The toolbar is initially GONE and shown above the keyboard when editing a tiddler.
     */
    /**
     * Update the status bar and navigation bar colors.
     * Icon colors are determined by the background luminance to ensure contrast.
     */
    @Suppress("DEPRECATION")
    private fun updateSystemBarColors(statusBarColorHex: String, navBarColorHex: String) {
        try {
            val statusColor = parseCssColor(statusBarColorHex)
            val navColor = parseCssColor(navBarColorHex)

            // Set background colors
            if (Build.VERSION.SDK_INT >= 35) {
                statusBarBgView?.setBackgroundColor(statusColor)
                navBarBgView?.setBackgroundColor(navColor)
            } else {
                window.statusBarColor = statusColor
                window.navigationBarColor = navColor
            }

            // Determine icon mode based on background luminance (separately for each bar)
            // Light background → dark icons, dark background → light icons
            val statusBarLuminance = calculateLuminance(statusColor)
            val navBarLuminance = calculateLuminance(navColor)
            val useDarkStatusIcons = statusBarLuminance > 0.5
            val useDarkNavIcons = navBarLuminance > 0.5

            Log.d(TAG, "Status bar luminance: $statusBarLuminance, dark icons: $useDarkStatusIcons")
            Log.d(TAG, "Nav bar luminance: $navBarLuminance, dark icons: $useDarkNavIcons")

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                // API 31+ (Android 12+): Use WindowInsetsController
                val insetsController = window.insetsController
                if (insetsController != null) {
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
                // API < 31: Use deprecated systemUiVisibility flags
                var newFlags = window.decorView.systemUiVisibility

                // SYSTEM_UI_FLAG_LIGHT_STATUS_BAR = dark icons on status bar (API 23+)
                newFlags = if (useDarkStatusIcons) {
                    newFlags or View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                } else {
                    newFlags and View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR.inv()
                }

                // SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR = dark icons on nav bar (API 26+)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    newFlags = if (useDarkNavIcons) {
                        newFlags or View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR
                    } else {
                        newFlags and View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR.inv()
                    }
                }

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

        // Register permission launcher for camera/microphone WebView requests
        permissionRequestLauncher = registerForActivityResult(
            ActivityResultContracts.RequestMultiplePermissions()
        ) { grants ->
            val req = pendingPermissionRequest
            pendingPermissionRequest = null
            if (req == null) return@registerForActivityResult

            val allGranted = grants.values.all { it }
            if (allGranted) {
                req.grant(req.resources)
                Log.d(TAG, "WebView permission granted: ${req.resources.joinToString()}")
            } else {
                req.deny()
                Log.d(TAG, "WebView permission denied by user")
            }
        }

        // Register permission launcher for geolocation WebView requests
        geolocationPermissionLauncher = registerForActivityResult(
            ActivityResultContracts.RequestMultiplePermissions()
        ) { grants ->
            val origin = pendingGeolocationOrigin
            val callback = pendingGeolocationCallback
            pendingGeolocationOrigin = null
            pendingGeolocationCallback = null
            if (origin == null || callback == null) return@registerForActivityResult

            val granted = grants.values.any { it }
            callback.invoke(origin, granted, false)
            Log.d(TAG, "Geolocation permission ${if (granted) "granted" else "denied"} for $origin")
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
        folderLocalPath = intent.getStringExtra(EXTRA_FOLDER_LOCAL_PATH)  // Local path for SAF folder wikis
        val backupsEnabled = intent.getBooleanExtra(EXTRA_BACKUPS_ENABLED, true)  // Default: enabled
        val backupCount = intent.getIntExtra(EXTRA_BACKUP_COUNT, 20)  // Default: 20 backups
        val customBackupDir = intent.getStringExtra(EXTRA_BACKUP_DIR)  // Custom backup directory URI

        Log.d(TAG, "WikiActivity onCreate - path: $wikiPath, title: $wikiTitle, isFolder: $isFolder, folderUrl: $folderServerUrl, localPath: $folderLocalPath, backupsEnabled: $backupsEnabled, backupCount: $backupCount")

        // Foreground notification is started on-demand when relay sync connects
        // (via SyncInterface.setRelayConnected called from JS polling)

        // Notify LAN sync service (main process) that a wiki is open — only if LAN sync is active
        if (LanSyncService.isLanSyncActive(applicationContext)) {
            LanSyncService.notifyWikiOpened(applicationContext)
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

        // Track whether we need to start the folder wiki server in a background thread
        var folderServerNeeded = false

        if (isFolder) {
            if (!folderServerUrl.isNullOrEmpty()) {
                // Server URL provided by caller
                wikiUrl = folderServerUrl
                Log.d(TAG, "Folder wiki using provided Node.js server at: $wikiUrl")
            } else if (!folderLocalPath.isNullOrEmpty()) {
                // Local path provided — start Node.js server in a background thread.
                folderServerNeeded = true
                wikiUrl = ""  // Will be set when server starts
                Log.d(TAG, "Folder wiki: will start Node.js server from local path: $folderLocalPath")
            } else {
                Log.e(TAG, "No server URL or local path provided for folder wiki!")
                finish()
                return
            }

            // Also start an HTTP server for serving attachments (Node.js server doesn't have /_relative/ endpoint)
            if (wikiUri != null && parsedTreeUri != null) {
                httpServer = WikiHttpServer(this, wikiUri!!, parsedTreeUri, true, null, false, 0)  // No backups for folder wikis
                attachmentServerUrl = try {
                    httpServer!!.start()
                } catch (e: Exception) {
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
            httpServer = WikiHttpServer(this, wikiUri!!, parsedTreeUri, false, null, backupsEnabled, backupCount, customBackupDir)
            wikiUrl = try {
                httpServer!!.start()            } catch (e: Exception) {
                Log.e(TAG, "Failed to start HTTP server: ${e.message}")
                finish()
                return
            }
            // For single-file wikis, use attachment ports for media/files.
            // JS distributes requests across ports via getAttachmentPorts() interface.
            // Fallback URL uses the first attachment port.
            attachmentServerUrl = "http://127.0.0.1:${httpServer!!.attachmentPorts[0]}"
            Log.d(TAG, "Single-file wiki using local server at: $wikiUrl")

            // Acquire WakeLock to keep server alive when app is in background
            acquireWakeLock()
        }

        // Handle widget navigation: append tiddler title as URL fragment for navigation
        val tiddlerTitle = intent.getStringExtra(EXTRA_TIDDLER_TITLE)
        if (!tiddlerTitle.isNullOrEmpty()) {
            wikiUrl = "$wikiUrl#${Uri.encode(tiddlerTitle)}"
            Log.d(TAG, "Navigating to tiddler: $tiddlerTitle")
        }

        Log.d(TAG, "Wiki opened: path=$wikiPath, url=$wikiUrl, taskId=$taskId")
        currentWikiUrl = wikiUrl

        // Clean up stale capture files (>24h old) on wiki open
        cleanupStaleCaptureFiles()

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
                @Suppress("DEPRECATION")
                setGeolocationEnabled(true)
            }

            // Enable third-party cookies for iframe embeds (YouTube, Vimeo, etc.)
            android.webkit.CookieManager.getInstance().setAcceptThirdPartyCookies(this, true)

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

                override fun onPermissionRequest(request: android.webkit.PermissionRequest?) {
                    if (request == null) return
                    Log.d(TAG, "onPermissionRequest: ${request.resources.joinToString()}")

                    val androidPermissions = mutableListOf<String>()
                    for (resource in request.resources) {
                        when (resource) {
                            android.webkit.PermissionRequest.RESOURCE_VIDEO_CAPTURE ->
                                androidPermissions.add(Manifest.permission.CAMERA)
                            android.webkit.PermissionRequest.RESOURCE_AUDIO_CAPTURE ->
                                androidPermissions.add(Manifest.permission.RECORD_AUDIO)
                        }
                    }

                    if (androidPermissions.isEmpty()) {
                        // No Android runtime permissions needed (e.g. RESOURCE_PROTECTED_MEDIA_ID)
                        request.grant(request.resources)
                        return
                    }

                    // Check if all permissions are already granted
                    val allGranted = androidPermissions.all {
                        ContextCompat.checkSelfPermission(this@WikiActivity, it) == PackageManager.PERMISSION_GRANTED
                    }

                    if (allGranted) {
                        request.grant(request.resources)
                        Log.d(TAG, "WebView permissions already granted")
                    } else {
                        pendingPermissionRequest = request
                        permissionRequestLauncher.launch(androidPermissions.toTypedArray())
                    }
                }

                override fun onPermissionRequestCanceled(request: android.webkit.PermissionRequest?) {
                    if (request == pendingPermissionRequest) {
                        pendingPermissionRequest = null
                    }
                }

                override fun onGeolocationPermissionsShowPrompt(
                    origin: String?,
                    callback: android.webkit.GeolocationPermissions.Callback?
                ) {
                    if (origin == null || callback == null) return
                    Log.d(TAG, "onGeolocationPermissionsShowPrompt: $origin")

                    val permissions = arrayOf(
                        Manifest.permission.ACCESS_FINE_LOCATION,
                        Manifest.permission.ACCESS_COARSE_LOCATION
                    )

                    val anyGranted = permissions.any {
                        ContextCompat.checkSelfPermission(this@WikiActivity, it) == PackageManager.PERMISSION_GRANTED
                    }

                    if (anyGranted) {
                        callback.invoke(origin, true, false)
                        Log.d(TAG, "Geolocation already granted for $origin")
                    } else {
                        pendingGeolocationOrigin = origin
                        pendingGeolocationCallback = callback
                        geolocationPermissionLauncher.launch(permissions)
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

            // Add JavaScript interface for sharing tiddler content
            addJavascriptInterface(ShareInterface(), "TiddlyDesktopShare")
            addJavascriptInterface(ShareCaptureInterface(), "TiddlyDesktopShareCapture")

            // Add JavaScript interface for persisting wiki config
            addJavascriptInterface(ConfigInterface(), "TiddlyDesktopConfig")

            // Add JavaScript interface for LAN sync bridge
            addJavascriptInterface(SyncInterface(), "TiddlyDesktopSync")

            // Add JavaScript interface for native PDF rendering
            addJavascriptInterface(PdfInterface(), "TiddlyDesktopPdf")

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

                // Check if a tiddler title is one of our injected ephemeral tiddlers.
                // CONFIG_ENABLE is NOT listed here — it persists in the wiki across reloads.
                function isInjectedTiddler(title) {
                    return title === PLUGIN_TITLE ||
                        title === CONFIG_SETTINGS_TAB ||
                        title === SESSION_AUTH_SETTINGS_TAB ||
                        title === "${'$'}:/plugins/tiddlydesktop-rs/android-share" ||
                        title === "${'$'}:/plugins/tiddlydesktop-rs/android-share/icon" ||
                        title === "${'$'}:/plugins/tiddlydesktop-rs/android-share/button" ||
                        title === "${'$'}:/config/ViewToolbarButtons/Visibility/${'$'}:/plugins/tiddlydesktop-rs/android-share/button" ||
                        title.indexOf(SESSION_AUTH_PREFIX + "url/") === 0 ||
                        title === SESSION_AUTH_PREFIX + "new-name" ||
                        title === SESSION_AUTH_PREFIX + "new-url";
                }

                // Expose isInjectedTiddler globally so other scripts can use it
                window.__tdIsInjected = isInjectedTiddler;

                // Install save hook to filter out our injected tiddlers (single-file wiki saver)
                if (!window.__tdSaveHookInstalled && ${'$'}tw.hooks) {
                    ${'$'}tw.hooks.addHook("th-saving-tiddler", function(tiddler) {
                        if (tiddler && tiddler.fields && tiddler.fields.title) {
                            if (isInjectedTiddler(tiddler.fields.title)) {
                                return null;
                            }
                        }
                        return tiddler;
                    });
                    window.__tdSaveHookInstalled = true;
                    console.log("[TiddlyDesktop] Save hook installed");

                    // For folder wikis (Node.js syncer): patch getSyncedTiddlers to exclude our tiddlers.
                    // The syncer uses getSyncedTiddlers() to determine which tiddlers to save/sync.
                    function patchSyncer() {
                        if (!${'$'}tw.syncer || ${'$'}tw.syncer.__tdPatched) return !!${'$'}tw.syncer;
                        ${'$'}tw.syncer.__tdPatched = true;
                        var origGS = ${'$'}tw.syncer.getSyncedTiddlers.bind(${'$'}tw.syncer);
                        ${'$'}tw.syncer.getSyncedTiddlers = function(source) {
                            return origGS(source).filter(function(t) { return !isInjectedTiddler(t); });
                        };
                        console.log("[TiddlyDesktop] Syncer patched: getSyncedTiddlers excludes injected tiddlers");
                        return true;
                    }
                    if (!patchSyncer()) {
                        var patchIv = setInterval(function() { if (patchSyncer()) clearInterval(patchIv); }, 100);
                    }
                }

                // Plugin tiddlers collection
                var pluginTiddlers = {};

                function addPluginTiddler(fields) {
                    pluginTiddlers[fields.title] = fields;
                }

                function removePluginTiddler(title) {
                    delete pluginTiddlers[title];
                }

                // Debounced registration: multiple callers add their tiddlers and call
                // registerPlugin(); only one actual registration fires after all callers
                // in the same event-loop vicinity have finished.
                var _registerPluginTimer = null;

                function registerPlugin() {
                    if (_registerPluginTimer !== null) {
                        clearTimeout(_registerPluginTimer);
                    }
                    _registerPluginTimer = setTimeout(function() {
                        _registerPluginTimer = null;
                        _doRegisterPlugin();
                    }, 10);
                }

                function _doRegisterPlugin() {
                    // Build plugin content
                    var pluginContent = { tiddlers: {} };
                    Object.keys(pluginTiddlers).forEach(function(title) {
                        pluginContent.tiddlers[title] = pluginTiddlers[title];
                    });

                    // Suppress change events during plugin injection — no dirty state
                    var origEnqueue = ${'$'}tw.wiki.enqueueTiddlerEvent;
                    ${'$'}tw.wiki.enqueueTiddlerEvent = function() {};

                    // Add plugin tiddler to the store (readPluginInfo reads from here)
                    ${'$'}tw.wiki.addTiddler(new ${'$'}tw.Tiddler({
                        title: PLUGIN_TITLE,
                        type: "application/json",
                        "plugin-type": "plugin",
                        name: "TiddlyDesktop Injected",
                        description: "Runtime-injected TiddlyDesktop settings UI",
                        version: "1.0.0",
                        text: JSON.stringify(pluginContent)
                    }));

                    // Process plugin
                    ${'$'}tw.wiki.readPluginInfo([PLUGIN_TITLE]);
                    ${'$'}tw.wiki.registerPluginTiddlers("plugin", [PLUGIN_TITLE]);
                    ${'$'}tw.wiki.unpackPluginTiddlers();

                    // Remove from real store — must NEVER be saved
                    ${'$'}tw.wiki.deleteTiddler(PLUGIN_TITLE);

                    // Restore change events
                    ${'$'}tw.wiki.enqueueTiddlerEvent = origEnqueue;

                    // Trigger UI refresh
                    ${'$'}tw.rootWidget.refresh({});

                    console.log("[TiddlyDesktop] Plugin registered with " + Object.keys(pluginTiddlers).length + " shadow tiddlers (no dirty state)");
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

                    // Sync localStorage and recent_wikis from wiki values
                    // (handles persisted real tiddlers that may differ from localStorage)
                    var wikiEnabled = ${'$'}tw.wiki.getTiddlerText(CONFIG_ENABLE, "yes") === "yes";
                    if (settings.enabled !== wikiEnabled) {
                        settings.enabled = wikiEnabled;
                        saveSettings(settings);
                    }
                    if (window.TiddlyDesktopConfig) {
                        window.TiddlyDesktopConfig.setExternalAttachments(wikiEnabled);
                    }

                    // Listen for changes to the enable setting
                    ${'$'}tw.wiki.addEventListener("change", function(changes) {
                        if (changes[CONFIG_ENABLE]) {
                            var enabled = ${'$'}tw.wiki.getTiddlerText(CONFIG_ENABLE) === "yes";
                            settings.enabled = enabled;
                            saveSettings(settings);
                            // Persist to recent_wikis.json so CaptureActivity can read it
                            if (window.TiddlyDesktopConfig) {
                                window.TiddlyDesktopConfig.setExternalAttachments(enabled);
                            }
                            console.log('[TiddlyDesktop] External attachments ' + (enabled ? 'enabled' : 'disabled'));
                        }
                    });

                    console.log('[TiddlyDesktop] External attachments UI injected');
                }

                // ========== Transform image/media URLs for display ==========
                // Use MutationObserver to transform src attributes at render time
                // This preserves the original _canonical_uri (relative path) in the tiddler
                // while displaying images via the local attachment server

                // Attachment port pool — distributes requests across multiple ports.
                // Chromium limits 6 connections per host:port; with N attachment ports
                // we get 6*N concurrent attachment connections that don't compete with
                // wiki HTML loading on the main port.
                var __attachmentPorts = null;
                function _getAttachmentPorts() {
                    if (__attachmentPorts) return __attachmentPorts;
                    try {
                        var csv = window.TiddlyDesktopServer.getAttachmentPorts();
                        if (csv) __attachmentPorts = csv.split(',');
                    } catch(e) {}
                    return __attachmentPorts;
                }
                // Simple string hash → port index (deterministic: same URL always
                // maps to same port for connection reuse / keep-alive).
                function _hashUrl(url) {
                    var h = 0;
                    for (var i = 0; i < url.length; i++) {
                        h = ((h << 5) - h + url.charCodeAt(i)) | 0;
                    }
                    return h < 0 ? -h : h;
                }
                function getAttachmentBaseUrl(url) {
                    // Folder wikis: single attachment server
                    if (window.__IS_FOLDER_WIKI__) {
                        try { return window.TiddlyDesktopServer.getAttachmentServerUrl(); } catch(e) {}
                        return window.__TD_ATTACHMENT_SERVER_URL__ || location.origin;
                    }
                    // Single-file wikis: distribute across attachment ports
                    var ports = _getAttachmentPorts();
                    if (ports && ports.length > 0) {
                        var idx = url ? _hashUrl(url) % ports.length : 0;
                        return 'http://127.0.0.1:' + ports[idx];
                    }
                    return window.__TD_ATTACHMENT_SERVER_URL__ || location.origin;
                }

                function transformUrl(url) {
                    if (!url) return url;
                    // data: and blob: URLs should never be transformed
                    if (url.startsWith('data:') || url.startsWith('blob:')) {
                        return url;
                    }
                    var baseUrl = getAttachmentBaseUrl(url);
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
                                preload: {type: "string", value: "metadata"},
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

                // Patch text-type parsers to support _canonical_uri lazy-loading.
                // Desktop uses invoke('read_file_as_binary') in filesystem.js but that
                // requires __TAURI__ which isn't available in WikiActivity's WebView.
                // Instead we use fetch() via the existing transformUrl() helper which
                // converts relative paths to /_relative/ URLs served by WikiHttpServer.
                (function patchTextParsersForCanonicalUri() {
                    if (!${'$'}tw.Wiki || !${'$'}tw.Wiki.parsers) return;
                    var textTypes = ['text/plain', 'text/x-tiddlywiki', 'application/javascript',
                                     'application/json', 'text/css', 'application/x-tiddler-dictionary',
                                     'text/markdown', 'text/x-markdown'];
                    textTypes.forEach(function(parserType) {
                        var OrigParser = ${'$'}tw.Wiki.parsers[parserType];
                        if (!OrigParser) return;
                        var PatchedParser = function(type, text, options) {
                            if ((text || '') === '' && options && options._canonical_uri) {
                                var uri = options._canonical_uri;
                                var wiki = options.wiki;
                                var fetchUrl = transformUrl(uri);
                                fetch(fetchUrl).then(function(r) { return r.text(); }).then(function(content) {
                                    if (wiki) {
                                        wiki.each(function(tiddler, title) {
                                            if (tiddler.fields._canonical_uri === uri &&
                                                (tiddler.fields.text || '') === '') {
                                                wiki.addTiddler(new ${'$'}tw.Tiddler(tiddler, {text: content}));
                                            }
                                        });
                                    }
                                }).catch(function(err) {
                                    console.error('[TiddlyDesktop] Failed to lazy-load _canonical_uri:', uri, err);
                                });
                                var placeholder = (${'$'}tw.language && ${'$'}tw.language.getRawString('LazyLoadingWarning')) || 'Loading...';
                                OrigParser.call(this, type, placeholder, options);
                            } else {
                                OrigParser.call(this, type, text, options);
                            }
                        };
                        PatchedParser.prototype = OrigParser.prototype;
                        ${'$'}tw.Wiki.parsers[parserType] = PatchedParser;
                    });
                    console.log('[TiddlyDesktop] Text parser _canonical_uri lazy-loading installed');
                })();

                injectSettingsUI();
                injectSessionAuthUI();

                // Reload warning for tiddlywiki.info changes from LAN sync
                addPluginTiddler({
                    title: "${'$'}:/plugins/tiddlydesktop-rs/injected/config-reload-warning",
                    tags: "${'$'}:/tags/PageTemplate",
                    text: '<${'$'}reveal state="${'$'}:/temp/tiddlydesktop/config-reload-required" type="match" text="yes" animate="yes">\n' +
                          '<div class="tc-plugin-reload-warning">\n' +
                          '{{${'$'}:/core/images/warning}} Wiki configuration was updated from another device. ' +
                          '<${'$'}button message="tm-browser-refresh" class="tc-btn-invisible tc-btn-mini">Click here to reload</${'$'}button> ' +
                          'to apply the changes.\n' +
                          '<${'$'}button set="${'$'}:/temp/tiddlydesktop/config-reload-required" setTo="" class="tc-btn-invisible tc-btn-mini" style="float:right;">{{${'$'}:/core/images/close-button}}</${'$'}button>\n' +
                          '</div>\n' +
                          '</${'$'}reveal>'
                });

                // Prevent address bar / location hash updates (not useful in app)
                addPluginTiddler({
                    title: "${'$'}:/config/Navigation/UpdateAddressBar",
                    text: "no"
                });

                registerPlugin();  // Register all shadow tiddlers as a plugin
                installImportHook();

                // Override permalink/permaview actions to do nothing — address bar URLs
                // are managed by the app container, not TW's hash-based navigation.
                ${'$'}tw.rootWidget.addEventListener("tm-permalink", function() {});
                ${'$'}tw.rootWidget.addEventListener("tm-permaview", function() {});

                // Override tm-home to preserve navigation but skip location.hash change
                ${'$'}tw.rootWidget.addEventListener("tm-home", function() {
                    var sf = ${'$'}tw.wiki.getTiddlerText("${'$'}:/DefaultTiddlers");
                    var sl = ${'$'}tw.wiki.filterTiddlers(sf);
                    sl = ${'$'}tw.hooks.invokeHook("th-opening-default-tiddlers-list", sl);
                    ${'$'}tw.wiki.addTiddler({
                        title: "${'$'}:/StoryList", text: "", list: sl
                    }, ${'$'}tw.wiki.getModificationFields());
                    if (sl[0]) { ${'$'}tw.wiki.addToHistory(sl[0]); }
                });

                // Block hashchange events from being processed by TW's story.js listener
                window.addEventListener("hashchange", function(e) {
                    e.stopImmediatePropagation();
                }, true);

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
                    xhr.open('PUT', window.location.origin, true);
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
                // Stub exitFullscreen to exit video fullscreen via Kotlin,
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

                        // Inject media enhancement (PDFium + poster extraction) into the overlay iframe
                        // TiddlyDesktopPdf is accessible from all frames in the WebView
                        (function(iDoc, iWin) {
                            var hasPdf = typeof TiddlyDesktopPdf !== 'undefined';
                            // Media controls CSS
                            var mediaStyle = iDoc.createElement('style');
                            mediaStyle.textContent = 'video{max-width:100%;height:auto;object-fit:contain;border-radius:4px;background:#000}audio{max-width:100%;width:100%;box-sizing:border-box}video::-webkit-media-controls-play-button,video::-webkit-media-controls-mute-button,video::-webkit-media-controls-fullscreen-button,video::-webkit-media-controls-overflow-button,video::-webkit-media-controls-timeline,video::-webkit-media-controls-volume-slider,video::-webkit-media-controls-overlay-play-button,audio::-webkit-media-controls-play-button,audio::-webkit-media-controls-mute-button,audio::-webkit-media-controls-timeline,audio::-webkit-media-controls-volume-slider,audio::-webkit-media-controls-overflow-button{cursor:pointer}video::-webkit-media-controls-overlay-play-button{display:flex;align-items:center;justify-content:center}';
                            iDoc.head.appendChild(mediaStyle);
                            // PDF viewer styles
                            var pdfStyle = iDoc.createElement('style');
                            pdfStyle.textContent = '.td-pdf-btn{background:#555;color:#fff;border:none;border-radius:3px;padding:4px 10px;font-size:14px;cursor:pointer;min-width:32px}.td-pdf-btn:active{background:#777}.td-pdf-page-wrap{display:flex;justify-content:center;margin:8px 0;position:relative}.td-pdf-page-wrap img{background:#fff;box-shadow:0 2px 8px rgba(0,0,0,.3);display:block;max-width:none;pointer-events:none}.td-pdf-text-layer{position:absolute;top:0;left:0;width:100%;height:100%;z-index:1;line-height:1}.td-pdf-text-layer span{position:absolute;color:transparent !important;white-space:pre;transform-origin:0% 0%;user-select:text;-webkit-user-select:text}.td-pdf-text-layer span::selection{background:rgba(0,100,255,0.3)}';
                            iDoc.head.appendChild(pdfStyle);

                            function enhanceVideo(el) {
                                if (el.__tdMediaDone) return;
                                el.__tdMediaDone = true;
                                setTimeout(function() {
                                    var src = el.src || (el.querySelector('source') ? el.querySelector('source').src : null);
                                    el.setAttribute('preload', 'metadata');
                                    if (src && typeof TiddlyDesktopPoster !== 'undefined') {
                                        try {
                                            var url = new URL(src);
                                            var relPath = decodeURIComponent(url.pathname).replace(/^\/_relative\//, '');
                                            var posterUrl = TiddlyDesktopPoster.getPoster(relPath);
                                            if (posterUrl) el.setAttribute('poster', posterUrl);
                                        } catch(e) { console.warn('[TD-Media] overlay poster failed:', e.message); }
                                    }
                                }, 50);
                            }

                            function getPdfSrc(el) {
                                var tag = el.tagName.toLowerCase();
                                var src = el.getAttribute('src') || el.getAttribute('data') || '';
                                if (tag === 'object') src = el.getAttribute('data') || src;
                                if (!src) return null;
                                var srcLower = src.toLowerCase();
                                if (srcLower.indexOf('.pdf') === -1 &&
                                    (el.getAttribute('type') || '').toLowerCase() !== 'application/pdf' &&
                                    srcLower.indexOf('data:application/pdf') !== 0) return null;
                                return src;
                            }

                            function replacePdfElement(el) {
                                if (el.__tdPdfDone || !hasPdf) return;
                                el.__tdPdfDone = true;
                                var src = getPdfSrc(el);
                                if (!src) return;
                                var container = iDoc.createElement('div');
                                container.style.cssText = 'width:100%;max-width:100%;overflow:auto;background:#525659;padding:8px 0;border-radius:4px;position:relative';
                                var toolbar = iDoc.createElement('div');
                                toolbar.style.cssText = 'display:flex;align-items:center;justify-content:center;gap:8px;padding:6px 8px;background:#333;color:#fff;font:13px sans-serif;border-radius:4px 4px 0 0;flex-wrap:wrap;position:sticky;top:0;z-index:10';
                                toolbar.innerHTML = '<button class="td-pdf-btn" data-action="prev">&#9664;</button><span class="td-pdf-pageinfo">- / -</span><button class="td-pdf-btn" data-action="next">&#9654;</button><span style="margin:0 4px">|</span><button class="td-pdf-btn" data-action="zoomout">&#8722;</button><button class="td-pdf-btn" data-action="fitwidth">Fit</button><button class="td-pdf-btn" data-action="zoomin">&#43;</button>';
                                container.appendChild(toolbar);
                                var pagesWrap = iDoc.createElement('div');
                                pagesWrap.style.cssText = 'max-height:80vh;overflow-y:auto;-webkit-overflow-scrolling:touch';
                                container.appendChild(pagesWrap);
                                el.style.display = 'none';
                                el.parentNode.insertBefore(container, el.nextSibling);
                                var pdfHandle = null, pageSizes = [], pageWraps = [], renderedPages = {}, pageCharBounds = {};
                                var scale = 1.0, userZoomed = false, lastContainerWidth = 0;
                                var pageInfo = toolbar.querySelector('.td-pdf-pageinfo');
                                function getTargetWidthPx() {
                                    var cw = pagesWrap.clientWidth - 16;
                                    if (cw <= 0) cw = 800;
                                    return Math.floor(cw * scale * (iWin.devicePixelRatio || 1));
                                }
                                function buildTextLayer(inner, pageNum, dw, dh) {
                                    var bounds = pageCharBounds[pageNum];
                                    if (!bounds || bounds.length === 0) return;
                                    var cc = bounds.length / 4;
                                    var ft = ''; try { ft = TiddlyDesktopPdf.getText(pdfHandle, pageNum, 0, cc - 1); } catch(ex) { return; }
                                    if (!ft) return;
                                    var tl = iDoc.createElement('div'); tl.className = 'td-pdf-text-layer';
                                    var dpr = iWin.devicePixelRatio || 1, wpx = dw * dpr, hpx = dh * dpr;
                                    var chs = [];
                                    for (var i = 0; i < cc; i++) chs.push({x:bounds[i*4],y:bounds[i*4+1],w:bounds[i*4+2],h:bounds[i*4+3]});
                                    var words = [], ws = 0;
                                    for (var i = 0; i < cc; i++) {
                                        var isL = (i === cc - 1), isG = false;
                                        if (!isL) {
                                            var c = chs[i], n = chs[i+1];
                                            if (c.w<=0||c.h<=0||n.w<=0||n.h<=0) isG = true;
                                            else {
                                                var ov = Math.min(c.y+c.h,n.y+n.h)-Math.max(c.y,n.y);
                                                if (ov<Math.min(c.h,n.h)*0.3) isG=true;
                                                else if (n.x-(c.x+c.w)>Math.min(c.h,n.h)*0.3) isG=true;
                                            }
                                        }
                                        if (isG || isL) {
                                            var end = i, wx=1e9,wy=1e9,wr=-1e9,wb=-1e9,hv=false;
                                            for(var j=ws;j<=end;j++){if(chs[j].w<=0||chs[j].h<=0)continue;hv=true;if(chs[j].x<wx)wx=chs[j].x;if(chs[j].y<wy)wy=chs[j].y;if(chs[j].x+chs[j].w>wr)wr=chs[j].x+chs[j].w;if(chs[j].y+chs[j].h>wb)wb=chs[j].y+chs[j].h;}
                                            if(hv) words.push({s:ws,e:end,x:wx,y:wy,w:wr-wx,h:wb-wy});
                                            ws=i+1;
                                        }
                                    }
                                    var tc = Array.from(ft), sds = [];
                                    for (var i = 0; i < words.length; i++) {
                                        var wd = words[i], txt = tc.slice(wd.s, wd.e+1).join('');
                                        if (!txt||!txt.trim()) continue;
                                        var sp = iDoc.createElement('span'); sp.textContent = txt;
                                        var cl=(wd.x/wpx)*dw, ct=(wd.y/hpx)*dh, cw=(wd.w/wpx)*dw, ch=(wd.h/hpx)*dh;
                                        sp.style.cssText='position:absolute;left:'+cl+'px;top:'+ct+'px;font-size:'+ch+'px;font-family:sans-serif;line-height:1;color:transparent !important;white-space:pre;transform-origin:0% 0%;user-select:text;-webkit-user-select:text;height:'+ch+'px;';
                                        sp.__tdTW=cw;
                                        tl.appendChild(sp); sds.push(sp);
                                    }
                                    inner.appendChild(tl);
                                    for(var i=0;i<sds.length;i++){var sp=sds[i],a=sp.scrollWidth;if(a>0&&sp.__tdTW>0)sp.style.transform='scaleX('+(sp.__tdTW/a)+')';delete sp.__tdTW;}
                                }
                                function renderPage(pageNum) {
                                    var widthPx = getTargetWidthPx();
                                    var key = pageNum + ':' + widthPx;
                                    if (renderedPages[key]) return;
                                    renderedPages[key] = true;
                                    try {
                                        var result = JSON.parse(TiddlyDesktopPdf.renderPage(pdfHandle, pageNum, widthPx));
                                        if (result.error) return;
                                        if (result.charBounds) { pageCharBounds[pageNum] = result.charBounds; }
                                        var wrap = pageWraps[pageNum];
                                        if (!wrap) return;
                                        wrap.innerHTML = '';
                                        var inner = iDoc.createElement('div');
                                        inner.style.cssText = 'position:relative;display:inline-block;';
                                        wrap.appendChild(inner);
                                        var img = iDoc.createElement('img');
                                        img.src = 'data:image/png;base64,' + result.imageBase64;
                                        var ps = pageSizes[pageNum];
                                        var displayWidth = (pagesWrap.clientWidth - 16) * scale;
                                        if (ps) { img.style.width = Math.floor(displayWidth) + 'px'; img.style.height = Math.floor(displayWidth * ps.h / ps.w) + 'px'; }
                                        img.style.display = 'block';
                                        img.setAttribute('data-page', pageNum);
                                        inner.appendChild(img);
                                        var dh = ps ? Math.floor(displayWidth * ps.h / ps.w) : Math.floor(displayWidth);
                                        buildTextLayer(inner, pageNum, Math.floor(displayWidth), dh);
                                    } catch(e) { console.error('[TD-PDF] overlay render page ' + pageNum + ':', e); }
                                }
                                function renderVisiblePages() {
                                    var wr = pagesWrap.getBoundingClientRect();
                                    pageWraps.forEach(function(w, i) { var r = w.getBoundingClientRect(); if (r.bottom >= wr.top - wr.height && r.top <= wr.bottom + wr.height) renderPage(i); });
                                }
                                function renderAll() { renderedPages = {}; pageCharBounds = {}; renderVisiblePages(); }
                                function fitWidth() {
                                    var w = pagesWrap.clientWidth;
                                    if (w <= 0) { iWin.requestAnimationFrame(fitWidth); return; }
                                    userZoomed = false; lastContainerWidth = w; scale = 1.0; renderAll();
                                }
                                (src.startsWith('data:') ? Promise.resolve(src.substring(src.indexOf(',') + 1)) :
                                    fetch(src).then(function(r) { return r.arrayBuffer(); }).then(function(ab) {
                                        var bytes = new Uint8Array(ab), binary = '';
                                        for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
                                        return btoa(binary);
                                    })
                                ).then(function(b64) {
                                    var result = JSON.parse(TiddlyDesktopPdf.open(b64));
                                    if (result.error) throw new Error(result.error);
                                    pdfHandle = result.handle;
                                    pageSizes = result.pageSizes;
                                    pageInfo.textContent = result.pageCount + ' page' + (result.pageCount !== 1 ? 's' : '');
                                    for (var p = 0; p < result.pageCount; p++) {
                                        var wrap = iDoc.createElement('div');
                                        wrap.className = 'td-pdf-page-wrap';
                                        var ps = pageSizes[p];
                                        if (ps) wrap.style.minHeight = Math.floor((pagesWrap.clientWidth - 16 || 800) * ps.h / ps.w) + 'px';
                                        pagesWrap.appendChild(wrap); pageWraps.push(wrap);
                                    }
                                    fitWidth();
                                    pagesWrap.addEventListener('scroll', function() {
                                        renderVisiblePages();
                                    });
                                    if (typeof iWin.ResizeObserver !== 'undefined') {
                                        var resizeTimer;
                                        new iWin.ResizeObserver(function() {
                                            var w = pagesWrap.clientWidth;
                                            if (w > 0 && w !== lastContainerWidth) {
                                                lastContainerWidth = w; clearTimeout(resizeTimer);
                                                resizeTimer = setTimeout(userZoomed ? renderAll : fitWidth, 100);
                                            }
                                        }).observe(container);
                                    }
                                }).catch(function(err) {
                                    pagesWrap.innerHTML = '<p style="color:#f88;padding:20px;text-align:center">Failed to load PDF: ' + (err.message || err) + '</p>';
                                });
                                toolbar.addEventListener('click', function(e) {
                                    var btn = e.target.closest('[data-action]');
                                    if (!btn) return;
                                    var a = btn.getAttribute('data-action');
                                    if (a === 'zoomin') { userZoomed = true; scale = Math.min(scale * 1.25, 5); renderAll(); }
                                    else if (a === 'zoomout') { userZoomed = true; scale = Math.max(scale / 1.25, 0.3); renderAll(); }
                                    else if (a === 'fitwidth') { fitWidth(); }
                                    else if (a === 'prev') pagesWrap.scrollBy(0, -pagesWrap.clientHeight * 0.8);
                                    else if (a === 'next') pagesWrap.scrollBy(0, pagesWrap.clientHeight * 0.8);
                                });
                            }

                            function enhanceAudioOverlay(el) {
                                if (el.__tdAudioDone) return;
                                el.__tdAudioDone = true;
                                function lockWidth() {
                                    var parent = el.parentElement;
                                    if (!parent) return;
                                    var pw = parent.clientWidth;
                                    if (pw > 0) {
                                        el.style.width = pw + 'px';
                                        el.style.maxWidth = '100%';
                                        el.style.boxSizing = 'border-box';
                                    }
                                }
                                if (el.offsetWidth > 0) { lockWidth(); } else { requestAnimationFrame(lockWidth); }
                                if (typeof ResizeObserver !== 'undefined' && el.parentElement) {
                                    new ResizeObserver(function() { lockWidth(); }).observe(el.parentElement);
                                }
                            }

                            function scanAll() {
                                iDoc.querySelectorAll('video').forEach(enhanceVideo);
                                iDoc.querySelectorAll('audio').forEach(enhanceAudioOverlay);
                                if (hasPdf) iDoc.querySelectorAll('embed, object, iframe').forEach(function(el) { if (getPdfSrc(el)) replacePdfElement(el); });
                            }

                            scanAll();

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

        // Inline PDFium renderer and video poster extraction script
        val inlineMediaScript = """
            (function() {
                var hasPdf = typeof TiddlyDesktopPdf !== 'undefined';
                var openPdfHandles = [];

                // ---- PDFium inline renderer ----
                function getPdfSrc(el) {
                    var tag = el.tagName.toLowerCase();
                    var src = el.getAttribute('src') || el.getAttribute('data') || '';
                    if (tag === 'object') src = el.getAttribute('data') || src;
                    if (!src) return null;
                    var srcLower = src.toLowerCase();
                    if (srcLower.indexOf('.pdf') === -1 &&
                        (el.getAttribute('type') || '').toLowerCase() !== 'application/pdf' &&
                        srcLower.indexOf('data:application/pdf') !== 0) return null;
                    return src;
                }

                function fetchPdfBytes(src) {
                    if (src.startsWith('data:')) {
                        var commaIdx = src.indexOf(',');
                        if (commaIdx < 0) return Promise.reject(new Error('Invalid data URI'));
                        return Promise.resolve(src.substring(commaIdx + 1));
                    }
                    return fetch(src).then(function(r) {
                        if (!r.ok) throw new Error('HTTP ' + r.status);
                        return r.arrayBuffer();
                    }).then(function(ab) {
                        var bytes = new Uint8Array(ab);
                        var binary = '';
                        for (var i = 0; i < bytes.length; i++) {
                            binary += String.fromCharCode(bytes[i]);
                        }
                        return btoa(binary);
                    });
                }

                function replacePdfElement(el) {
                    if (el.__tdPdfDone) return;
                    if (!hasPdf) return;
                    el.__tdPdfDone = true;
                    var src = getPdfSrc(el);
                    if (!src) return;

                    var container = document.createElement('div');
                    container.className = 'td-pdf-container';
                    container.style.cssText = 'width:100%;max-width:100%;overflow:auto;background:#525659;padding:8px 0;border-radius:4px;position:relative;';

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

                    if (!document.querySelector('#td-pdf-styles')) {
                        var style = document.createElement('style');
                        style.id = 'td-pdf-styles';
                        style.textContent =
                            '.td-pdf-btn{background:#555;color:#fff;border:none;border-radius:3px;padding:4px 10px;font-size:14px;cursor:pointer;min-width:32px;}' +
                            '.td-pdf-btn:active{background:#777;}' +
                            '.td-pdf-pages-wrap{overflow-y:auto;-webkit-overflow-scrolling:touch;}' +
                            '.td-pdf-page-wrap{display:flex;justify-content:center;margin:8px 0;position:relative;}' +
                            '.td-pdf-page-wrap img{background:#fff;box-shadow:0 2px 8px rgba(0,0,0,.3);display:block;max-width:none;pointer-events:none;}' +
                            '.td-pdf-text-layer{position:absolute;top:0;left:0;width:100%;height:100%;z-index:1;line-height:1;}' +
                            '.td-pdf-text-layer span{position:absolute;color:transparent !important;white-space:pre;transform-origin:0% 0%;user-select:text;-webkit-user-select:text;}' +
                            '.td-pdf-text-layer span::selection{background:rgba(0,100,255,0.3);}';
                        document.head.appendChild(style);
                    }

                    var pagesWrap = document.createElement('div');
                    pagesWrap.className = 'td-pdf-pages-wrap';
                    pagesWrap.style.cssText = 'max-height:80vh;overflow-y:auto;-webkit-overflow-scrolling:touch;';
                    container.appendChild(pagesWrap);

                    el.style.display = 'none';
                    el.parentNode.insertBefore(container, el.nextSibling);

                    var pdfHandle = null;
                    var pageSizes = [];
                    var pageCount = 0;
                    var scale = 1.0;
                    var pageWraps = [];
                    var renderedPages = {};
                    var pageCharBounds = {};
                    var userZoomed = false;
                    var lastContainerWidth = 0;
                    var pageInfo = toolbar.querySelector('.td-pdf-pageinfo');
                    function getTargetWidthPx() {
                        var containerWidth = pagesWrap.clientWidth - 16;
                        if (containerWidth <= 0) containerWidth = 800;
                        var dpr = window.devicePixelRatio || 1;
                        return Math.floor(containerWidth * scale * dpr);
                    }

                    function buildTextLayer(inner, pageNum, displayWidth, displayHeight) {
                        var bounds = pageCharBounds[pageNum];
                        if (!bounds || bounds.length === 0) return;
                        var charCount = bounds.length / 4;
                        var fullText = '';
                        try { fullText = TiddlyDesktopPdf.getText(pdfHandle, pageNum, 0, charCount - 1); } catch(ex) { return; }
                        if (!fullText) return;
                        var textLayer = document.createElement('div');
                        textLayer.className = 'td-pdf-text-layer';
                        var dpr = window.devicePixelRatio || 1;
                        var widthPx = displayWidth * dpr;
                        var heightPx = displayHeight * dpr;
                        var chars = [];
                        for (var i = 0; i < charCount; i++) {
                            chars.push({ x: bounds[i*4], y: bounds[i*4+1], w: bounds[i*4+2], h: bounds[i*4+3] });
                        }
                        // Group chars into words (same line, no large gap)
                        var words = [];
                        var wordStart = 0;
                        for (var i = 0; i < charCount; i++) {
                            var isLast = (i === charCount - 1);
                            var isGap = false;
                            if (!isLast) {
                                var c = chars[i], n = chars[i+1];
                                if (c.w <= 0 || c.h <= 0 || n.w <= 0 || n.h <= 0) { isGap = true; }
                                else {
                                    var overlapY = Math.min(c.y+c.h, n.y+n.h) - Math.max(c.y, n.y);
                                    if (overlapY < Math.min(c.h, n.h) * 0.3) isGap = true;
                                    else if (n.x - (c.x + c.w) > Math.min(c.h, n.h) * 0.3) isGap = true;
                                }
                            }
                            if (isGap || isLast) {
                                var end = isLast ? i : i;
                                var wx = Infinity, wy = Infinity, wr = -Infinity, wb = -Infinity;
                                var hasValid = false;
                                for (var j = wordStart; j <= end; j++) {
                                    if (chars[j].w <= 0 || chars[j].h <= 0) continue;
                                    hasValid = true;
                                    if (chars[j].x < wx) wx = chars[j].x;
                                    if (chars[j].y < wy) wy = chars[j].y;
                                    if (chars[j].x + chars[j].w > wr) wr = chars[j].x + chars[j].w;
                                    if (chars[j].y + chars[j].h > wb) wb = chars[j].y + chars[j].h;
                                }
                                if (hasValid) {
                                    words.push({ start: wordStart, end: end, x: wx, y: wy, w: wr - wx, h: wb - wy });
                                }
                                wordStart = i + 1;
                            }
                        }
                        var textChars = Array.from(fullText);
                        var spanData = [];
                        for (var i = 0; i < words.length; i++) {
                            var wd = words[i];
                            var text = textChars.slice(wd.start, wd.end + 1).join('');
                            if (!text || !text.trim()) continue;
                            var span = document.createElement('span');
                            span.textContent = text;
                            var cssLeft = (wd.x / widthPx) * displayWidth;
                            var cssTop = (wd.y / heightPx) * displayHeight;
                            var cssW = (wd.w / widthPx) * displayWidth;
                            var cssH = (wd.h / heightPx) * displayHeight;
                            span.style.cssText = 'position:absolute;left:' + cssLeft + 'px;top:' + cssTop + 'px;' +
                                'font-size:' + cssH + 'px;font-family:sans-serif;line-height:1;' +
                                'color:transparent !important;white-space:pre;transform-origin:0% 0%;' +
                                'user-select:text;-webkit-user-select:text;height:' + cssH + 'px;';
                            span.__tdTargetW = cssW;
                            textLayer.appendChild(span);
                            spanData.push(span);
                        }
                        inner.appendChild(textLayer);
                        // Measure actual rendered width and apply scaleX to fit
                        for (var i = 0; i < spanData.length; i++) {
                            var sp = spanData[i];
                            var actual = sp.scrollWidth;
                            if (actual > 0 && sp.__tdTargetW > 0) {
                                var sx = sp.__tdTargetW / actual;
                                sp.style.transform = 'scaleX(' + sx + ')';
                            }
                            delete sp.__tdTargetW;
                        }
                    }

                    function renderPage(pageNum) {
                        var widthPx = getTargetWidthPx();
                        var key = pageNum + ':' + widthPx;
                        if (renderedPages[key]) return;
                        renderedPages[key] = true;
                        try {
                            var resultJson = TiddlyDesktopPdf.renderPage(pdfHandle, pageNum, widthPx);
                            var result = JSON.parse(resultJson);
                            if (result.error) { console.error('[TD-PDF] Render error:', result.error); return; }
                            if (result.charBounds) { pageCharBounds[pageNum] = result.charBounds; }
                            var wrap = pageWraps[pageNum];
                            if (!wrap) return;
                            wrap.innerHTML = '';
                            var inner = document.createElement('div');
                            inner.style.cssText = 'position:relative;display:inline-block;';
                            wrap.appendChild(inner);
                            var img = document.createElement('img');
                            img.src = 'data:image/png;base64,' + result.imageBase64;
                            var ps = pageSizes[pageNum];
                            var containerWidth = pagesWrap.clientWidth - 16;
                            var displayWidth = containerWidth * scale;
                            if (ps) {
                                var aspect = ps.h / ps.w;
                                img.style.width = Math.floor(displayWidth) + 'px';
                                img.style.height = Math.floor(displayWidth * aspect) + 'px';
                            }
                            img.style.display = 'block';
                            img.setAttribute('data-page', pageNum);
                            inner.appendChild(img);
                            var dh = ps ? Math.floor(displayWidth * ps.h / ps.w) : Math.floor(displayWidth);
                            buildTextLayer(inner, pageNum, Math.floor(displayWidth), dh);
                        } catch(e) {
                            console.error('[TD-PDF] Failed to render page ' + pageNum + ':', e);
                        }
                    }

                    function renderVisiblePages() {
                        var wrapRect = pagesWrap.getBoundingClientRect();
                        var margin = wrapRect.height;
                        pageWraps.forEach(function(wrap, i) {
                            var rect = wrap.getBoundingClientRect();
                            if (rect.bottom >= wrapRect.top - margin && rect.top <= wrapRect.bottom + margin) {
                                renderPage(i);
                            }
                        });
                    }

                    function renderAll() { renderedPages = {}; pageCharBounds = {}; renderVisiblePages(); }

                    function fitWidth() {
                        var w = pagesWrap.clientWidth;
                        if (w <= 0) { requestAnimationFrame(function() { fitWidth(); }); return; }
                        userZoomed = false;
                        lastContainerWidth = w;
                        scale = 1.0;
                        renderAll();
                    }

                    fetchPdfBytes(src).then(function(b64) {
                        var resultJson = TiddlyDesktopPdf.open(b64);
                        var result = JSON.parse(resultJson);
                        if (result.error) throw new Error(result.error);

                        pdfHandle = result.handle;
                        pageCount = result.pageCount;
                        pageSizes = result.pageSizes;
                        openPdfHandles.push(pdfHandle);

                        pageInfo.textContent = pageCount + ' page' + (pageCount !== 1 ? 's' : '');

                        for (var p = 0; p < pageCount; p++) {
                            var wrap = document.createElement('div');
                            wrap.className = 'td-pdf-page-wrap';
                            var ps = pageSizes[p];
                            if (ps) {
                                var containerWidth = pagesWrap.clientWidth - 16 || 800;
                                wrap.style.minHeight = Math.floor(containerWidth * (ps.h / ps.w)) + 'px';
                            }
                            pagesWrap.appendChild(wrap);
                            pageWraps.push(wrap);
                        }

                        fitWidth();

                        pagesWrap.addEventListener('scroll', function() {
                            renderVisiblePages();
                        });

                        if (typeof ResizeObserver !== 'undefined') {
                            var resizeTimer;
                            new ResizeObserver(function() {
                                var w = pagesWrap.clientWidth;
                                if (w > 0 && w !== lastContainerWidth) {
                                    lastContainerWidth = w;
                                    if (!userZoomed) {
                                        clearTimeout(resizeTimer);
                                        resizeTimer = setTimeout(fitWidth, 100);
                                    } else {
                                        clearTimeout(resizeTimer);
                                        resizeTimer = setTimeout(renderAll, 100);
                                    }
                                }
                            }).observe(container);
                        }
                    }).catch(function(err) {
                        console.error('[TD-PDF] Error loading PDF:', err);
                        pagesWrap.innerHTML = '<p style="color:#f88;padding:20px;text-align:center;">Failed to load PDF: ' + (err.message || err) + '</p>';
                    });

                    toolbar.addEventListener('click', function(e) {
                        var btn = e.target.closest('[data-action]');
                        if (!btn) return;
                        var action = btn.getAttribute('data-action');
                        if (action === 'zoomin') { userZoomed = true; scale = Math.min(scale * 1.25, 5); renderAll(); }
                        else if (action === 'zoomout') { userZoomed = true; scale = Math.max(scale / 1.25, 0.3); renderAll(); }
                        else if (action === 'fitwidth') { fitWidth(); }
                        else if (action === 'prev') { pagesWrap.scrollBy(0, -pagesWrap.clientHeight * 0.8); }
                        else if (action === 'next') { pagesWrap.scrollBy(0, pagesWrap.clientHeight * 0.8); }
                    });

                }

                // ---- Video poster extraction ----
                function applyPoster(el, posterUrl) {
                    el.setAttribute('poster', posterUrl);
                }

                var videoQueue = [];
                var videoQueueRunning = false;
                function enqueueVideoWork(work) {
                    videoQueue.push(work);
                    if (!videoQueueRunning) {
                        videoQueueRunning = true;
                        setTimeout(drainVideoQueue, 500);
                    }
                }
                function drainVideoQueue() {
                    if (videoQueue.length === 0) { videoQueueRunning = false; return; }
                    videoQueueRunning = true;
                    var task = videoQueue.shift();
                    task(function() { drainVideoQueue(); });
                }

                function enhanceVideo(el) {
                    if (el.__tdMediaDone) return;
                    el.__tdMediaDone = true;

                    setTimeout(function() {
                        var videoSrc = el.src || (el.querySelector('source') ? el.querySelector('source').src : null);
                        el.setAttribute('preload', 'metadata');

                        if (videoSrc) {
                            enqueueVideoWork(function(done) {
                                try {
                                    var url = new URL(videoSrc);
                                    var pathPart = decodeURIComponent(url.pathname);
                                    var relPath = pathPart.replace(/^\/_relative\//, '');

                                    if (typeof TiddlyDesktopPoster !== 'undefined') {
                                        var posterUrl = TiddlyDesktopPoster.getPoster(relPath);
                                        if (posterUrl) {
                                            applyPoster(el, posterUrl);
                                        }
                                    }
                                } catch(e) {
                                    console.warn('[TD-Media] Native poster extraction failed:', e.message);
                                }
                                done();
                            });
                        }
                    }, 50);
                }

                // ---- Lock audio element width to container pixels ----
                // Android WebView re-renders native audio controls at the new
                // zoom level during playback, causing them to resize relative to
                // the viewport instead of the container. Locking width in CSS
                // pixels prevents this.
                function enhanceAudio(el) {
                    if (el.__tdAudioDone) return;
                    el.__tdAudioDone = true;
                    function lockWidth() {
                        var parent = el.parentElement;
                        if (!parent) return;
                        var pw = parent.clientWidth;
                        if (pw > 0) {
                            el.style.width = pw + 'px';
                            el.style.maxWidth = '100%';
                            el.style.boxSizing = 'border-box';
                        }
                    }
                    // Lock once the element is in layout
                    if (el.offsetWidth > 0) {
                        lockWidth();
                    } else {
                        requestAnimationFrame(lockWidth);
                    }
                    // Re-lock on container resize (sidebar toggle, orientation)
                    if (typeof ResizeObserver !== 'undefined' && el.parentElement) {
                        var ro = new ResizeObserver(function() { lockWidth(); });
                        ro.observe(el.parentElement);
                    }
                }

                // ---- Scan and enhance existing elements ----
                function scanAll() {
                    if (hasPdf) {
                        document.querySelectorAll('embed, object, iframe').forEach(function(el) {
                            if (getPdfSrc(el)) replacePdfElement(el);
                        });
                    }
                    document.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
                    document.querySelectorAll('audio').forEach(function(el) { enhanceAudio(el); });
                }

                // ---- MutationObserver to catch dynamically added elements ----
                if (!window.__tdObserverSet) {
                    window.__tdObserverSet = true;
                    var obs = new MutationObserver(function(mutations) {
                        mutations.forEach(function(m) {
                            m.addedNodes.forEach(function(node) {
                                if (node.nodeType !== 1) return;
                                var tag = node.tagName ? node.tagName.toLowerCase() : '';
                                if ((tag === 'embed' || tag === 'object' || tag === 'iframe') && hasPdf && getPdfSrc(node)) {
                                    replacePdfElement(node);
                                } else if (tag === 'video') {
                                    enhanceVideo(node);
                                } else if (tag === 'audio') {
                                    enhanceAudio(node);
                                }
                                if (node.querySelectorAll) {
                                    if (hasPdf) {
                                        node.querySelectorAll('embed, object, iframe').forEach(function(el) {
                                            if (getPdfSrc(el)) replacePdfElement(el);
                                        });
                                    }
                                    node.querySelectorAll('video').forEach(function(el) { enhanceVideo(el); });
                                    node.querySelectorAll('audio').forEach(function(el) { enhanceAudio(el); });
                                }
                            });
                        });
                    });
                    obs.observe(document.body, { childList: true, subtree: true });

                    window.addEventListener('pageshow', function(event) {
                        if (event.persisted) {
                            console.log('[TiddlyDesktop] Page restored from bfcache, re-scanning');
                            scanAll();
                        }
                    });
                }

                // ---- Blur active element on video/audio interaction ----
                if (!window.__tdMediaBlurSet) {
                    window.__tdMediaBlurSet = true;
                    document.addEventListener('mousedown', function(e) {
                        var t = e.target;
                        if (t && (t.tagName === 'VIDEO' || t.tagName === 'AUDIO')) {
                            var ae = document.activeElement;
                            if (ae && ae !== t) ae.blur();
                        }
                    }, true);
                }

                // Cleanup PDF handles on page unload
                window.addEventListener('beforeunload', function() {
                    openPdfHandles.forEach(function(h) {
                        try { TiddlyDesktopPdf.close(h); } catch(e) {}
                    });
                    openPdfHandles = [];
                });

                scanAll();

                console.log('[TiddlyDesktop] Inline media enhancement initialized');
            })();
        """.trimIndent()

        // Build the Android Share toolbar button as an ephemeral TiddlyWiki plugin.
        // Uses JSONObject to safely build the plugin JSON, then base64-encodes it
        // to avoid escaping issues when injecting via evaluateJavascript.
        val sharePluginJson = run {
            val iconTiddler = JSONObject().apply {
                put("title", "\$:/plugins/tiddlydesktop-rs/android-share/icon")
                put("tags", "\$:/tags/Image")
                put("type", "text/vnd.tiddlywiki")
                put("text", "\\parameters (size:\"22pt\")\n" +
                    "<svg width=<<size>> height=<<size>> class=\"tc-image-share tc-image-button\" viewBox=\"0 0 24 24\">" +
                    "<path d=\"M18 16.08c-.76 0-1.44.3-1.96.77L8.91 12.7c.05-.23.09-.46.09-.7s-.04-.47-.09-.7l7.05-4.11c.54.5 1.25.81 2.04.81 1.66 0 3-1.34 3-3s-1.34-3-3-3-3 1.34-3 3c0 .24.04.47.09.7L8.04 9.81C7.5 9.31 6.79 9 6 9c-1.66 0-3 1.34-3 3s1.34 3 3 3c.79 0 1.5-.31 2.04-.81l7.12 4.16c-.05.21-.08.43-.08.65 0 1.61 1.31 2.92 2.92 2.92 1.61 0 2.92-1.31 2.92-2.92s-1.31-2.92-2.92-2.92z\"/>" +
                    "</svg>")
            }
            val buttonTiddler = JSONObject().apply {
                put("title", "\$:/plugins/tiddlydesktop-rs/android-share/button")
                put("tags", "\$:/tags/ViewToolbar")
                put("caption", "{{\$:/plugins/tiddlydesktop-rs/android-share/icon}} share")
                put("description", "Share this tiddler")
                put("text", "\\whitespace trim\n" +
                    "<\$button message=\"tm-tiddlydesktop-rs-share\" param=<<currentTiddler>> " +
                    "tooltip=\"Share this tiddler\" aria-label=\"Share this tiddler\" " +
                    "class=<<tv-config-toolbar-class>>>\n" +
                    "<%if [<tv-config-toolbar-icons>match[yes]] %>\n" +
                    "{{\$:/plugins/tiddlydesktop-rs/android-share/icon}}\n" +
                    "<%endif%>\n" +
                    "<%if [<tv-config-toolbar-text>match[yes]] %>\n" +
                    "<span class=\"tc-btn-text\">share</span>\n" +
                    "<%endif%>\n" +
                    "</\$button>")
            }
            val visibilityTiddler = JSONObject().apply {
                put("title", "\$:/config/ViewToolbarButtons/Visibility/\$:/plugins/tiddlydesktop-rs/android-share/button")
                put("text", "hide")
            }
            val tiddlersMap = JSONObject().apply {
                put("\$:/plugins/tiddlydesktop-rs/android-share/icon", iconTiddler)
                put("\$:/plugins/tiddlydesktop-rs/android-share/button", buttonTiddler)
                put("\$:/config/ViewToolbarButtons/Visibility/\$:/plugins/tiddlydesktop-rs/android-share/button", visibilityTiddler)
            }
            JSONObject().apply {
                put("title", "\$:/plugins/tiddlydesktop-rs/android-share")
                put("type", "application/json")
                put("plugin-type", "plugin")
                put("description", "Share tiddler content via Android share sheet")
                put("name", "Android Share")
                put("version", "1.0.0")
                put("text", JSONObject().put("tiddlers", tiddlersMap).toString())
            }.toString()
        }
        val sharePluginBase64 = android.util.Base64.encodeToString(
            sharePluginJson.toByteArray(Charsets.UTF_8), android.util.Base64.NO_WRAP)
        val sharePluginScript = "(function w(){" +
            "if(typeof \$tw==='undefined'||!\$tw.wiki||!\$tw.rootWidget){setTimeout(w,100);return;}" +
            // Clean up stale .json/.meta files on disk from previous syncer leaks (folder wikis only).
            // We do NOT call deleteTiddler() — the tiddler must stay in the store for functionality.
            // REST DELETE removes the disk file; system tiddler protection prevents browser-side deletion.
            "if(!\$tw.rootWidget.__tdShareCleanup){" +
            "\$tw.rootWidget.__tdShareCleanup=true;" +
            "['\$:/plugins/tiddlydesktop-rs/android-share','\$:/plugins/tiddlydesktop-rs/injected'].forEach(function(t){" +
            "var ex=\$tw.wiki.getTiddler(t);" +
            "if(ex&&ex.fields.revision){" +
            "try{fetch('/bags/default/tiddlers/'+encodeURIComponent(t),{method:'DELETE',headers:{'X-Requested-With':'TiddlyWiki'}});}catch(e){}" +
            "console.log('[TiddlyDesktop] Deleted stale disk file for:',t);" +
            "}" +
            "});" +
            "}" +
            // Register share message handler FIRST (independent of plugin setup)
            "if(!\$tw.rootWidget.__tdShareHandler){" +
            "\$tw.rootWidget.__tdShareHandler=true;" +
            // Helper: resolve a URL to absolute using location.origin
            "function __tdAbsUrl(src){" +
            "if(!src||src.indexOf('data:')===0)return '';" +
            "if(src.indexOf('http://127.0.0.1')===0||src.indexOf('https://127.0.0.1')===0)return src;" +
            "if(src.indexOf('/')===0)return location.origin+src;" +
            "if(src.indexOf('http:')===0||src.indexOf('https:')===0)return '';" +  // external URL — skip
            "return location.origin+'/'+src;" +  // relative path
            "}" +
            // Async helper: convert a local URL to a data URI via fetch
            "function __tdToDataUri(src){" +
            "var abs=__tdAbsUrl(src);" +
            "if(!abs)return Promise.resolve('');" +
            "return fetch(abs).then(function(r){if(!r.ok)throw 0;return r.blob();}).then(function(b){" +
            "return new Promise(function(ok){var r=new FileReader();r.onload=function(){ok(r.result);};r.onerror=function(){ok('');};r.readAsDataURL(b);});" +
            "}).catch(function(){return '';});" +
            "}" +
            // Helper: strip local server URL prefix to get relative path
            "function __tdRelPath(src){" +
            "if(!src)return '';" +
            "return decodeURIComponent(src.replace(/^https?:\\/\\/127\\.0\\.0\\.1:\\d+\\//,'').replace(/^\\/_file\\//,'').replace(/^\\/_relative\\//,''));" +
            "}" +
            // Share handler: gathers fields as JSON + plain text, calls prepareShare.
            // No rendered HTML is generated here — shareAsImage captures directly from
            // the live DOM via html2canvas, and other formats use fieldsJson/plainText.
            "function __tdShareTiddler(title){" +
            "var tiddler=\$tw.wiki.getTiddler(title);" +
            "var fields={};" +
            "if(tiddler){for(var k in tiddler.fields){" +
            "var v=tiddler.fields[k];" +
            "if(v instanceof Date){fields[k]=\$tw.utils.stringifyDate(v);}else if(Array.isArray(v)){fields[k]=\$tw.utils.stringifyList(v);}else{fields[k]=String(v);}" +
            "}}" +
            "var fieldsJson=JSON.stringify(fields);" +
            "var plainText=fields.text||'';" +
            "if(window.TiddlyDesktopShare)window.TiddlyDesktopShare.prepareShare(title,fieldsJson,'',plainText);" +
            "}" +
            "\$tw.rootWidget.addEventListener('tm-tiddlydesktop-rs-share',function(e){" +
            "var t=e.param||e.tiddlerTitle;if(!t)return false;" +
            "__tdShareTiddler(t);" +
            "return true;" +
            "});}" +
            // Now set up the ephemeral plugin (button UI)
            "try{" +
            "var p=JSON.parse(atob('$sharePluginBase64'));" +
            // Suppress change events during plugin injection — no dirty state
            "var oE=\$tw.wiki.enqueueTiddlerEvent;\$tw.wiki.enqueueTiddlerEvent=function(){};" +
            "\$tw.wiki.addTiddler(new \$tw.Tiddler(p));" +
            "\$tw.wiki.readPluginInfo([p.title]);" +
            "\$tw.wiki.registerPluginTiddlers('plugin',[p.title]);" +
            "\$tw.wiki.unpackPluginTiddlers();" +
            // Remove from real store — must NEVER be saved
            "\$tw.wiki.deleteTiddler(p.title);" +
            // Restore change events
            "\$tw.wiki.enqueueTiddlerEvent=oE;" +
            // Trigger UI refresh so toolbar shows the share button
            "\$tw.rootWidget.refresh({});" +
            "console.log('[TiddlyDesktop] Share plugin injected (no dirty state)');" +
            "}catch(e){console.error('[TiddlyDesktop] Share plugin error:',e);}" +
            "})()"

        // LAN sync script: hooks into TiddlyWiki change events and polls bridge for inbound
        val lanSyncScript = "(function(){" +
            "var S=window.TiddlyDesktopSync;if(!S)return;" +
            // Don't exit when bridge not running (port<=0) — init()'s 500ms getSyncId
            // polling handles waiting for the bridge to come online. This allows wikis
            // opened before LAN sync is started to activate sync when it starts later.
            "var wp=window.__WIKI_PATH__||'';if(!wp)return;" +
            // Collab API: created immediately so CM6 plugin can find it before sync activates.
            // Outbound methods queue until activate() sets syncActive=true.
            "var collabListeners={};" +
            "var _syncActive=false;var _syncId=null;var _collabQueue=[];var rec={};" +
            "function emitCollab(type,data){var ls=collabListeners[type];if(ls)for(var i=0;i<ls.length;i++){try{ls[i](data);}catch(e){}}}" +
            "function updateET(tt){if(typeof \$tw==='undefined'||!\$tw.wiki)return;var eds=rec[tt]||[];var tid='\$:/temp/tiddlydesktop/editing/'+tt;if(eds.length>0){\$tw.wiki.addTiddler({title:tid,type:'application/json',text:JSON.stringify(eds)});}else{\$tw.wiki.deleteTiddler(tid);}}" +
            "function clearAllET(){if(typeof \$tw==='undefined'||!\$tw.wiki)return;\$tw.wiki.each(function(td,t){if(t.indexOf('\$:/temp/tiddlydesktop/editing/')===0)\$tw.wiki.deleteTiddler(t);});}" +
            "if(!window.TiddlyDesktop)window.TiddlyDesktop={};" +
            "window.TiddlyDesktop.collab={" +
            "startEditing:function(t){if(_syncActive){S.collabEditingStarted(_syncId,t);}else{_collabQueue.push(['startEditing',t]);}}," +
            "stopEditing:function(t){if(_syncActive){S.collabEditingStopped(_syncId,t);}else{_collabQueue.push(['stopEditing',t]);}}," +
            "sendUpdate:function(t,b){if(_syncActive){S.collabUpdate(_syncId,t,b);}else{_collabQueue.push(['sendUpdate',t,b]);}}," +
            "sendAwareness:function(t,b){if(_syncActive){S.collabAwareness(_syncId,t,b);}else{_collabQueue.push(['sendAwareness',t,b]);}}," +
            "peerSaved:function(t,s){if(_syncActive){S.collabPeerSaved(_syncId,t,s);}else{_collabQueue.push(['peerSaved',t,s]);}}," +
            "getRemoteEditors:function(t){if(!_syncActive)return[];try{return JSON.parse(S.getRemoteEditors(_syncId,t)||'[]');}catch(e){return [];}}," +
            "getRemoteEditorsAsync:function(t){return Promise.resolve(this.getRemoteEditors(t));}," +
            "on:function(ev,cb){if(!collabListeners[ev])collabListeners[ev]=[];collabListeners[ev].push(cb);}," +
            "off:function(ev,cb){if(!collabListeners[ev])return;collabListeners[ev]=collabListeners[ev].filter(function(c){return c!==cb;});}" +
            "};" +
            "try{window.dispatchEvent(new Event('collab-api-ready'));}catch(e){}" +
            "function init(){" +
            "if(typeof \$tw==='undefined'||!\$tw.wiki||!\$tw.wiki.addEventListener||!\$tw.rootWidget){setTimeout(init,100);return;}" +
            "var sid=S.getSyncId(wp);" +
            "if(sid){activate(sid);}else{" +
            // Re-check every 500ms in case user enables sync from landing page
            "console.log('[LAN Sync] No sync_id yet, polling for activation...');" +
            "var iv=setInterval(function(){var id=S.getSyncId(wp);if(id){clearInterval(iv);console.log('[LAN Sync] Sync activated via polling: '+id);activate(id);}},500);" +
            // Expose a check function for onResume — JS timers are throttled in background
            "window.__tdCheckSyncActivation=function(){var id=S.getSyncId(wp);if(id){clearInterval(iv);console.log('[LAN Sync] Sync activated via onResume: '+id);activate(id);window.__tdCheckSyncActivation=null;}};" +
            "}" +
            "}" +
            // Serialize tiddler fields, converting Date objects to TW date strings
            "function serFields(fields){var o={};var ks=Object.keys(fields);for(var i=0;i<ks.length;i++){var k=ks[i];var v=fields[k];o[k]=v instanceof Date?\$tw.utils.stringifyDate(v):Array.isArray(v)?\$tw.utils.stringifyList(v):String(v);}return JSON.stringify(o);}" +
            // Check if title is a draft tiddler (including numbered drafts)
            "function isDraft(t){if(t.indexOf(\"Draft of '\")==0)return true;" +
            "if(t.indexOf('Draft ')==0){var r=t.substring(6);var p=r.indexOf(\" of '\");if(p>0){var n=r.substring(0,p);if(/^\\d+\$/.test(n))return true;}}return false;}" +
            // Compare version strings (semver-like). Returns 1 if a>b, -1 if a<b, 0 if equal.
            "function cmpVer(a,b){if(!a&&!b)return 0;if(!a)return -1;if(!b)return 1;var pa=a.split('.'),pb=b.split('.');var len=Math.max(pa.length,pb.length);for(var i=0;i<len;i++){var na=parseInt(pa[i]||'0',10)||0;var nb=parseInt(pb[i]||'0',10)||0;if(na>nb)return 1;if(na<nb)return -1;}return 0;}" +
            "function activate(syncId){" +
            "var syncActive=true;" +
            "console.log('[LAN Sync] Activated for wiki: '+syncId);" +
            "S.wikiOpened(syncId);" +
            // Activate collab: set flag, flush queued outbound messages
            // NOTE: Do NOT reset collabListeners — CM6 editors created before sync
            // activation already registered their inbound listeners via collab.on().
            "clearAllET();rec={};_syncActive=true;_syncId=syncId;" +
            "var q=_collabQueue;_collabQueue=[];" +
            "for(var qi=0;qi<q.length;qi++){var qe=q[qi];" +
            "if(qe[0]==='startEditing')S.collabEditingStarted(syncId,qe[1]);" +
            "else if(qe[0]==='stopEditing')S.collabEditingStopped(syncId,qe[1]);" +
            "else if(qe[0]==='sendUpdate')S.collabUpdate(syncId,qe[1],qe[2]);" +
            "else if(qe[0]==='sendAwareness')S.collabAwareness(syncId,qe[1],qe[2]);" +
            "else if(qe[0]==='peerSaved')S.collabPeerSaved(syncId,qe[1],qe[2]);" +
            "}" +
            "console.log('[LAN Sync] Collab API activated for wiki: '+syncId);" +
            // Notify CM6 collab plugins that transport is active (for late Phase 2)
            "try{window.dispatchEvent(new Event('collab-sync-activated'));}catch(e){}" +
            // Use a Set to suppress re-broadcasting received changes.
            // TiddlyWiki dispatches change events asynchronously via $tw.utils.nextTick(),
            // so a boolean flag would already be cleared when the change listener fires.
            "var suppress=new Set();" +
            "var kst={};" +  // knownSyncTitles: title→modified for tiddlers received but skipped as identical
            "var tomb={};" +  // deletionTombstones: title→{modified,time}
            "var TOMB_MAX=30*24*60*60*1000;" +  // 30 days
            "var conflicts={};" +
            "var saveTimer=null;" +
            "var isSingle=!\$tw.syncer;" +
            "function scheduleSave(){if(!isSingle)return;if(saveTimer)clearTimeout(saveTimer);" +
            "saveTimer=setTimeout(function(){saveTimer=null;\$tw.rootWidget.dispatchEvent({type:'tm-save-wiki'});},500);}" +
            // Outbound: detect local changes
            "\$tw.wiki.addEventListener('change',function(ch){" +
            "if(!syncActive)return;" +
            "var keys=Object.keys(ch);" +
            "for(var i=0;i<keys.length;i++){" +
            "var t=keys[i];" +
            "if(suppress.delete(t))continue;" +
            "if(t==='\$:/StoryList'||t==='\$:/HistoryList'||t==='\$:/library/sjcl.js'||t==='\$:/Import'||t==='\$:/language'||t==='\$:/theme'||t==='\$:/palette'||t==='\$:/isEncrypted'||t==='\$:/view'||t==='\$:/layout'||t==='\$:/DefaultTiddlers')continue;" +
            "if(isDraft(t))continue;" +
            "if(t.indexOf('\$:/TiddlyDesktopRS/Conflicts/')==0)continue;" +
            "if(t.indexOf('\$:/state/')==0)continue;" +
            "if(t.indexOf('\$:/status/')==0)continue;" +
            "if(t.indexOf('\$:/temp/')==0)continue;" +
            "if(t.indexOf('\$:/plugins/tiddlydesktop-rs/')==0)continue;" +
            "if(t.indexOf('\$:/plugins/tiddlydesktop/')==0)continue;" +
            "if(t.indexOf('\$:/config/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/options/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/metrics/')==0)continue;" +
            "if(conflicts[t])continue;" +
            "var td=\$tw.wiki.getTiddler(t);" +
            "if(ch[t].deleted){var dm=\$tw.utils.stringifyDate(new Date());tomb[t]={modified:dm,time:Date.now()};S.saveTombstones(syncId,JSON.stringify(tomb));console.log('[LAN Sync] Outbound delete: '+t);S.tiddlerDeleted(syncId,t);}" +
            "else{if(tomb[t]&&!tomb[t].cleared){tomb[t].cleared=true;S.saveTombstones(syncId,JSON.stringify(tomb));console.log('[LAN Sync] Cleared tombstone for re-created: '+t);}if(td){console.log('[LAN Sync] Outbound change: '+t);S.tiddlerChanged(syncId,t,serFields(td.fields));}}" +
            "}" +
            "});" +
            // Inbound: poll bridge for changes (batched application)
            "var queue=[];var batchTimer=null;" +
            // fieldToString: normalize field values (Date→TW date string, Array→TW list) for comparison
            "function fts(v){if(v instanceof Date)return \$tw.utils.stringifyDate(v);if(Array.isArray(v))return \$tw.utils.stringifyList(v);return String(v);}" +
            // tiddlerDiffers: compare incoming fields with local (includes modified for convergence)
            "function tiddlerDiffers(f){var ex=\$tw.wiki.getTiddler(f.title);if(!ex)return true;var ef=ex.fields;" +
            "var rm=f.modified?String(f.modified):'';var lm=ef.modified?fts(ef.modified):'';if(rm!==lm)return true;" +
            "var ks=Object.keys(f);for(var i=0;i<ks.length;i++){var k=ks[i];if(k==='created'||k==='modified')continue;if(ef[k]===undefined||String(f[k])!==fts(ef[k]))return true;}" +
            "var eks=Object.keys(ef);for(var j=0;j<eks.length;j++){var ek=eks[j];if(ek==='created'||ek==='modified')continue;if(f[ek]===undefined)return true;}return false;}" +
            // collectFingerprints: includes knownSyncTitles for convergence
            "function cfps(){var all=\$tw.wiki.allTitles();var seen={};var fps=[];" +
            "for(var i=0;i<all.length;i++){var t=all[i];" +
            "if(t==='\$:/StoryList'||t==='\$:/HistoryList'||t==='\$:/library/sjcl.js'||t==='\$:/Import'||t==='\$:/language'||t==='\$:/theme'||t==='\$:/palette'||t==='\$:/isEncrypted'||t==='\$:/view'||t==='\$:/layout'||t==='\$:/DefaultTiddlers')continue;" +
            "if(isDraft(t))continue;" +
            "if(t.indexOf('\$:/TiddlyDesktopRS/Conflicts/')==0)continue;" +
            "if(t.indexOf('\$:/state/')==0||t.indexOf('\$:/status/')==0||t.indexOf('\$:/temp/')==0)continue;" +
            "if(t.indexOf('\$:/plugins/tiddlydesktop-rs/')==0||t.indexOf('\$:/plugins/tiddlydesktop/')==0)continue;" +
            "if(t.indexOf('\$:/config/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/options/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/metrics/')==0)continue;" +
            "if(\$tw.wiki.isShadowTiddler(t)&&!\$tw.wiki.tiddlerExists(t))continue;" +
            "var td=\$tw.wiki.getTiddler(t);if(td){var m=td.fields.modified;var fp={title:t,modified:m?fts(m):''};if(td.fields['plugin-type']&&td.fields.version)fp.version=td.fields.version;fps.push(fp);seen[t]=1;}}" +
            "var ks=Object.keys(kst);for(var j=0;j<ks.length;j++){if(!seen[ks[j]])fps.push({title:ks[j],modified:kst[ks[j]]});}" +
            "var tks=Object.keys(tomb);var tc=false;for(var k=0;k<tks.length;k++){if(tomb[tks[k]].cleared)continue;if(!seen[tks[k]])fps.push({title:tks[k],modified:tomb[tks[k]].modified,deleted:true});else{tomb[tks[k]].cleared=true;tc=true;}}if(tc)S.saveTombstones(syncId,JSON.stringify(tomb));" +
            "return fps;}" +
            "function queueChange(d){" +
            "if(d.wiki_id!==syncId)return;" +
            "if(d.type==='dump-tiddlers'){dumpTiddlers(d.to_device_id);return;}" +
            "if(d.type==='send-fingerprints'){sendFingerprints(d.to_device_id);return;}" +
            "if(d.type==='compare-fingerprints'){compareFingerprints(d.from_device_id,d.fingerprints);return;}" +
            "if(d.type==='attachment-received'){reloadAttachment(d.filename);return;}" +
            "if(d.type==='wiki-info-changed'){console.log('[LAN Sync] Wiki config changed from another device');\$tw.wiki.addTiddler(new \$tw.Tiddler({title:'\$:/temp/tiddlydesktop/config-reload-required',text:'yes'}));return;}" +
            "if(d.type==='editing-started'){if(d.tiddler_title&&d.device_id){if(!rec[d.tiddler_title])rec[d.tiddler_title]=[];var ef=false;for(var ei=0;ei<rec[d.tiddler_title].length;ei++){if(rec[d.tiddler_title][ei].device_id===d.device_id){ef=true;rec[d.tiddler_title][ei].user_name=d.user_name||'';break;}}if(!ef){rec[d.tiddler_title].push({device_id:d.device_id,device_name:d.device_name||'',user_name:d.user_name||''});}updateET(d.tiddler_title);}emitCollab(d.type,d);return;}" +
            "if(d.type==='editing-stopped'){if(d.tiddler_title&&d.device_id&&rec[d.tiddler_title]){rec[d.tiddler_title]=rec[d.tiddler_title].filter(function(e){return e.device_id!==d.device_id;});if(rec[d.tiddler_title].length===0)delete rec[d.tiddler_title];updateET(d.tiddler_title);}emitCollab(d.type,d);return;}" +
            "if(d.type==='collab-update'||d.type==='collab-awareness'){emitCollab(d.type,d);return;}" +
            "if(d.type==='peer-saved'){emitCollab(d.type,d);return;}" +
            "queue.push(d);if(!batchTimer)batchTimer=setTimeout(applyBatch,50);" +
            "}" +
            "function applyBatch(){batchTimer=null;var b=queue;queue=[];if(!b.length)return;" +
            "console.log('[LAN Sync] Applying batch of '+b.length+' changes');" +
            "var ns=false;var pc=false;" +
            "for(var i=0;i<b.length;i++){var d=b[i];" +
            "if(d.type==='apply-change'){try{var f=JSON.parse(d.tiddler_json);" +
            "if(f['plugin-type']&&f.version){var lp=\$tw.wiki.getTiddler(f.title);if(lp&&lp.fields.version&&cmpVer(f.version,lp.fields.version)<=0){kst[f.title]=f.modified?String(f.modified):'';continue;}}" +
            "if(tomb[f.title])delete tomb[f.title];if(tiddlerDiffers(f)){if(f.created)f.created=\$tw.utils.parseDate(f.created);if(f.modified)f.modified=\$tw.utils.parseDate(f.modified);suppress.add(f.title);\$tw.wiki.addTiddler(new \$tw.Tiddler(f));ns=true;if(f.title.indexOf('\$:/plugins/')===0&&f['plugin-type'])pc=true;}else{kst[f.title]=f.modified?String(f.modified):'';}}catch(e){}}" +
            "else if(d.type==='apply-deletion'){try{if(\$tw.wiki.tiddlerExists(d.title)){suppress.add(d.title);\$tw.wiki.deleteTiddler(d.title);ns=true;}var ddm=\$tw.utils.stringifyDate(new Date());if(!tomb[d.title]||tomb[d.title].modified<ddm){tomb[d.title]={modified:ddm,time:Date.now()};S.saveTombstones(syncId,JSON.stringify(tomb));}}catch(e){}}" +
            "else if(d.type==='conflict'){var lt=\$tw.wiki.getTiddler(d.title);if(lt){var ct='\$:/TiddlyDesktopRS/Conflicts/'+d.title;" +
            "conflicts[ct]=1;var cf=Object.assign({},lt.fields,{title:ct,'conflict-original-title':d.title,'conflict-timestamp':new Date().toISOString(),'conflict-source':'local'});" +
            "\$tw.wiki.addTiddler(new \$tw.Tiddler(cf));delete conflicts[ct];}}" +
            "}" +
            "if(pc){try{\$tw.wiki.readPluginInfo();\$tw.wiki.registerPluginTiddlers('plugin');\$tw.wiki.unpackPluginTiddlers();}catch(e){}}" +
            "if(ns)scheduleSave();}" +
            // Fingerprint-based diff sync
            "function sendFingerprints(toDevId){var fps=cfps();S.sendFingerprints(syncId,toDevId,JSON.stringify(fps));}" +
            "function compareFingerprints(fromDevId,fps){" +
            "console.log('[LAN Sync] compareFingerprints: received '+fps.length+' fingerprints from '+fromDevId);" +
            // Separate normal fingerprints from tombstones
            "var remote={};var peerVer={};var peerTombs={};for(var i=0;i<fps.length;i++){if(fps[i].deleted)peerTombs[fps[i].title]=fps[i].modified;else{remote[fps[i].title]=fps[i].modified;if(fps[i].version)peerVer[fps[i].title]=fps[i].version;}}" +
            // Apply peer tombstones: delete local tiddlers that peer intentionally deleted
            "var tombNs=false;var ptks=Object.keys(peerTombs);for(var ti=0;ti<ptks.length;ti++){" +
            "var tt=ptks[ti];var tm=peerTombs[tt];" +
            "var lt2=tomb[tt];if(lt2&&lt2.cleared&&lt2.modified>=tm)continue;" +
            "var lt=\$tw.wiki.getTiddler(tt);" +
            "if(lt){var lm2=lt.fields.modified?fts(lt.fields.modified):'';" +
            "if(!lm2||lm2<=tm){suppress.add(tt);\$tw.wiki.deleteTiddler(tt);tombNs=true;console.log('[LAN Sync] Applied tombstone deletion: '+tt);}}" +
            "if(!lt2||lt2.modified<tm)tomb[tt]={modified:tm,time:Date.now()};}" +
            "if(tombNs)scheduleSave();if(ptks.length>0)S.saveTombstones(syncId,JSON.stringify(tomb));" +
            "var all=\$tw.wiki.allTitles();var diffs=[];" +
            "for(var j=0;j<all.length;j++){var t=all[j];" +
            "if(t==='\$:/StoryList'||t==='\$:/HistoryList'||t==='\$:/library/sjcl.js'||t==='\$:/Import'||t==='\$:/language'||t==='\$:/theme'||t==='\$:/palette'||t==='\$:/isEncrypted'||t==='\$:/view'||t==='\$:/layout'||t==='\$:/DefaultTiddlers')continue;" +
            "if(isDraft(t))continue;" +
            "if(t.indexOf('\$:/TiddlyDesktopRS/Conflicts/')==0)continue;" +
            "if(t.indexOf('\$:/state/')==0||t.indexOf('\$:/status/')==0||t.indexOf('\$:/temp/')==0)continue;" +
            "if(t.indexOf('\$:/plugins/tiddlydesktop-rs/')==0||t.indexOf('\$:/plugins/tiddlydesktop/')==0)continue;" +
            "if(t.indexOf('\$:/config/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/options/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/metrics/')==0)continue;" +
            "if(\$tw.wiki.isShadowTiddler(t)&&!\$tw.wiki.tiddlerExists(t))continue;" +
            "var td=\$tw.wiki.getTiddler(t);if(!td)continue;" +
            "if(!(t in remote)){diffs.push({title:t,tiddler_json:serFields(td.fields)});}" +
            "else if(td.fields['plugin-type']){var lv=td.fields.version||'';var pv=peerVer[t]||'';if(cmpVer(lv,pv)>0)diffs.push({title:t,tiddler_json:serFields(td.fields)});}" +
            "else{var localMod=td.fields.modified?fts(td.fields.modified):'';if(localMod>(remote[t]||''))diffs.push({title:t,tiddler_json:serFields(td.fields)});}" +
            "delete remote[t];}" +
            "console.log('[LAN Sync] compareFingerprints: '+diffs.length+' diffs to send (of '+all.length+' local, peer has '+fps.length+')');" +
            "if(diffs.length>0){var MAX_BYTES=500000;" +
            "function sendBatch(si){var batch=[];var bytes=0;var i=si;" +
            "while(i<diffs.length&&(batch.length===0||bytes<MAX_BYTES)){bytes+=diffs[i].tiddler_json.length;batch.push(diffs[i]);i++;}" +
            "var last=i>=diffs.length;" +
            "console.log('[LAN Sync] sendBatch: sending '+batch.length+' tiddlers ('+Math.round(bytes/1024)+'KB, last='+last+') to '+fromDevId);" +
            "S.sendFullSyncBatch(syncId,fromDevId,JSON.stringify(batch),last);" +
            "if(!last)setTimeout(function(){sendBatch(i);},100);else console.log('[LAN Sync] compareFingerprints: diff sync complete');}sendBatch(0);" +
            "}else{console.log('[LAN Sync] compareFingerprints: no diffs, sending empty batch');S.sendFullSyncBatch(syncId,fromDevId,'[]',true);}}" +
            // Reload attachment elements when file received
            "function reloadAttachment(fn){" +
            "var els=document.querySelectorAll('img,video,audio,source');var ts='?t='+Date.now();" +
            "for(var i=0;i<els.length;i++){var el=els[i];var src=el.getAttribute('src')||'';" +
            "if(src.indexOf(fn)!==-1){el.src=src.split('?')[0]+ts;}}}" +
            // Full sync dump
            "function dumpTiddlers(toDevId){" +
            "var all=\$tw.wiki.allTitles();var MX=500000;" +
            "console.log('[LAN Sync] Dumping '+all.length+' tiddlers to '+toDevId);" +
            "function send(si){var batch=[];var bytes=0;var i=si;" +
            "while(i<all.length&&(batch.length===0||bytes<MX)){var t=all[i];i++;" +
            "if(t==='\$:/StoryList'||t==='\$:/HistoryList'||t==='\$:/library/sjcl.js'||t==='\$:/Import'||t==='\$:/language'||t==='\$:/theme'||t==='\$:/palette'||t==='\$:/isEncrypted'||t==='\$:/view'||t==='\$:/layout'||t==='\$:/DefaultTiddlers')continue;" +
            "if(isDraft(t))continue;" +
            "if(t.indexOf('\$:/TiddlyDesktopRS/Conflicts/')==0)continue;" +
            "if(t.indexOf('\$:/state/')==0||t.indexOf('\$:/status/')==0||t.indexOf('\$:/temp/')==0)continue;" +
            "if(t.indexOf('\$:/plugins/tiddlydesktop-rs/')==0||t.indexOf('\$:/plugins/tiddlydesktop/')==0)continue;" +
            "if(t.indexOf('\$:/config/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/options/')==0)continue;" +
            "if(t.indexOf('\$:/themes/tiddlywiki/vanilla/metrics/')==0)continue;" +
            "if(\$tw.wiki.isShadowTiddler(t)&&!\$tw.wiki.tiddlerExists(t))continue;" +
            "var td=\$tw.wiki.getTiddler(t);if(td){var j=serFields(td.fields);bytes+=j.length;batch.push({title:t,tiddler_json:j});}}" +
            "var last=i>=all.length;S.sendFullSyncBatch(syncId,toDevId,JSON.stringify(batch),last);" +
            "if(!last)setTimeout(function(){send(i);},100);" +
            "else console.log('[LAN Sync] Full sync dump complete');" +
            "}" +
            "if(all.length>0)send(0);}" +
            // Load tombstones and broadcast initial fingerprints
            "try{var stored=JSON.parse(S.loadTombstones(syncId));var now=Date.now();var sks=Object.keys(stored);" +
            "for(var si=0;si<sks.length;si++){if(stored[sks[si]].time&&now-stored[sks[si]].time>TOMB_MAX)continue;tomb[sks[si]]=stored[sks[si]];}" +
            "console.log('[LAN Sync] Loaded '+Object.keys(tomb).length+' tombstones');}catch(e){}" +
            "var fps=cfps();" +
            "console.log('[LAN Sync] Broadcasting '+fps.length+' fingerprints for catch-up');" +
            "S.broadcastFingerprints(syncId,JSON.stringify(fps));" +
            // Poll loop — fast polling (20ms) for first 5s to speed up initial sync
            "var pollStart=Date.now();" +
            "function pollIv(){return(Date.now()-pollStart<5000)?20:100;}" +
            "function poll(){if(!syncActive)return;try{var j=S.pollChanges(syncId);var c=JSON.parse(j);" +
            "if(c&&c.length){for(var i=0;i<c.length;i++){" +
            "if(c[i].type==='sync-deactivate'){console.log('[LAN Sync] Deactivated by landing page');syncActive=false;_syncActive=false;clearAllET();rec={};" +
            // After deactivation, poll for re-activation (user may re-enable sync)
            "var reiv=setInterval(function(){var rid=S.getSyncId(wp);if(rid){clearInterval(reiv);console.log('[LAN Sync] Re-activated: '+rid);syncActive=true;_syncActive=true;pollStart=Date.now();setTimeout(poll,0);var rfps=cfps();S.broadcastFingerprints(rid,JSON.stringify(rfps));}},500);" +
            "return;}" +
            "queueChange(c[i]);}}}" +
            "catch(e){}setTimeout(poll,pollIv());}" +
            "setTimeout(poll,0);" +
            // Periodic fingerprint re-broadcast (5s safety net for convergence)
            "setInterval(function(){if(!syncActive)return;try{var fps2=cfps();S.broadcastFingerprints(syncId,JSON.stringify(fps2));}catch(e){}},5000);" +
            // Periodic tombstone cleanup (every 10 minutes)
            "setInterval(function(){try{var now2=Date.now();var tks=Object.keys(tomb);var rm=0;for(var ti=0;ti<tks.length;ti++){if(tomb[tks[ti]].time&&now2-tomb[tks[ti]].time>TOMB_MAX){delete tomb[tks[ti]];rm++;}}if(rm>0){console.log('[LAN Sync] Cleaned up '+rm+' expired tombstones');S.saveTombstones(syncId,JSON.stringify(tomb));}}catch(e){}},600000);" +
            "}" +
            "setTimeout(init,100);" +
            "})()"

        // Conflict resolution UI: shows banner when LAN sync conflicts exist, modal to review/resolve
        val conflictUiScript = "(function(){" +
            "'use strict';" +
            "if(!window.__WIKI_PATH__)return;" +
            "var CONFLICT_PREFIX='" + "\$:/TiddlyDesktopRS/Conflicts/';" +
            "var banner=null,bannerDismissed=false,modalOverlay=null;" +
            // getColour - self-contained palette color lookup
            "function getColour(name,fallback,depth){depth=depth||0;if(depth>10)return fallback;" +
            "if(typeof " + "\$tw!=='undefined'&&" + "\$tw.wiki){try{var pn=" + "\$tw.wiki.getTiddlerText('" + "\$:/palette');" +
            "if(pn){pn=pn.trim();var pt=" + "\$tw.wiki.getTiddler(pn);" +
            "if(pt){var tx=pt.fields.text||'';var ls=tx.split('\\n');" +
            "for(var i=0;i<ls.length;i++){var ln=ls[i].trim();var ci=ln.indexOf(':');" +
            "if(ci>0){var cn=ln.substring(0,ci).trim();var cv=ln.substring(ci+1).trim();" +
            "if(cn===name&&cv){var mt=cv.match(/<<colour\\s+([^>]+)>>/);" +
            "if(mt)return getColour(mt[1].trim(),fallback,depth+1);return cv;}}}}}}catch(e){}}return fallback;}" +
            // getConflictTitles
            "function getConflictTitles(){var t=[];" + "\$tw.wiki.each(function(td,title){if(title.indexOf(CONFLICT_PREFIX)===0)t.push(title);});return t;}" +
            // formatTimestamp
            "function fmtTs(iso){if(!iso)return '';try{return new Date(iso).toLocaleString();}catch(e){return iso;}}" +
            // getContrastingColor
            "function gcc(bg){try{var h=bg.replace('#','');if(h.length===3)h=h[0]+h[0]+h[1]+h[1]+h[2]+h[2];" +
            "var r=parseInt(h.substr(0,2),16)/255,g=parseInt(h.substr(2,2),16)/255,b=parseInt(h.substr(4,2),16)/255;" +
            "r=r<=0.03928?r/12.92:Math.pow((r+0.055)/1.055,2.4);" +
            "g=g<=0.03928?g/12.92:Math.pow((g+0.055)/1.055,2.4);" +
            "b=b<=0.03928?b/12.92:Math.pow((b+0.055)/1.055,2.4);" +
            "return(0.2126*r+0.7152*g+0.0722*b)>0.179?'#000000':'#ffffff';}catch(e){return '#333333';}}" +
            // createBanner
            "function createBanner(){if(banner)return banner;" +
            "banner=document.createElement('div');banner.id='td-conflict-banner';" +
            "banner.style.cssText='display:none;position:fixed;top:0;left:0;right:0;z-index:9999;background:#fff3cd;color:#856404;border-bottom:2px solid #ffc107;padding:8px 16px;font-size:14px;font-family:system-ui,sans-serif;align-items:center;gap:8px;box-shadow:0 2px 4px rgba(0,0,0,0.1);';" +
            "var ts=document.createElement('span');ts.style.cssText='flex:1;';banner.__ts=ts;" +
            "var rb=document.createElement('button');rb.textContent='Resolve';" +
            "rb.style.cssText='padding:4px 12px;border:1px solid #ffc107;border-radius:4px;background:#ffc107;color:#333;cursor:pointer;font-size:13px;font-weight:500;';" +
            "rb.onclick=function(){showModal();};" +
            "var db=document.createElement('button');db.textContent='\\u00d7';" +
            "db.style.cssText='padding:2px 8px;border:none;background:transparent;color:#856404;cursor:pointer;font-size:18px;line-height:1;';" +
            "db.onclick=function(){bannerDismissed=true;banner.style.display='none';};" +
            "banner.appendChild(ts);banner.appendChild(rb);banner.appendChild(db);document.body.appendChild(banner);return banner;}" +
            // updateBanner
            "function updateBanner(){var c=getConflictTitles();if(c.length===0){if(banner)banner.style.display='none';bannerDismissed=false;return;}" +
            "if(bannerDismissed)return;if(!banner)createBanner();" +
            "banner.__ts.textContent='\\u26a0 '+c.length+' sync conflict'+(c.length!==1?'s':'')+' detected';" +
            "banner.style.display='flex';}" +
            // renderDiff
            "function renderDiff(lt,rt){var c=document.createElement('div');" +
            "c.style.cssText='font-family:monospace;font-size:13px;white-space:pre-wrap;word-break:break-word;padding:8px;border-radius:4px;max-height:300px;overflow:auto;background:'+getColour('tiddler-background','#ffffff')+';border:1px solid '+getColour('tiddler-border','#cccccc')+';';" +
            "try{var m=" + "\$tw.modules.execute('" + "\$:/core/modules/utils/diff-match-patch/diff_match_patch.js');" +
            "var d=new m.diff_match_patch();var df=d.diff_main(lt||'',rt||'');d.diff_cleanupSemantic(df);" +
            "for(var i=0;i<df.length;i++){var s=document.createElement('span');s.textContent=df[i][1];" +
            "if(df[i][0]===-1)s.style.cssText='background:#fdd;color:#900;text-decoration:line-through;';" +
            "else if(df[i][0]===1)s.style.cssText='background:#dfd;color:#060;';" +
            "c.appendChild(s);}}catch(e){c.textContent='Unable to compute diff';}return c;}" +
            // resolveConflict
            "function resolveConflict(ct,action){try{var cf=" + "\$tw.wiki.getTiddler(ct);if(!cf)return;" +
            "if(action==='local'){var ot=cf.fields['conflict-original-title'];" +
            "if(ot){\$tw.wiki.addTiddler(new \$tw.Tiddler(cf,{title:ot,'conflict-original-title':undefined,'conflict-timestamp':undefined,'conflict-source':undefined}));}}" +
            "\$tw.wiki.deleteTiddler(ct);}catch(e){console.error('[TiddlyDesktop] resolveConflict error:',e);}}" +
            // resolveAll
            "function resolveAll(action){var c=getConflictTitles();for(var i=0;i<c.length;i++)resolveConflict(c[i],action);closeModal();updateBanner();}" +
            // afterResolve
            "function afterResolve(){if(modalOverlay){var r=modalOverlay.querySelectorAll('[data-conflict-title]');if(r.length===0)closeModal();}updateBanner();}" +
            // closeModal
            "function closeModal(){if(modalOverlay){if(modalOverlay.__esc)document.removeEventListener('keydown',modalOverlay.__esc);modalOverlay.remove();modalOverlay=null;}}" +
            // showModal
            "function showModal(){if(modalOverlay)closeModal();var conflicts=getConflictTitles();if(conflicts.length===0)return;" +
            "var mbg=getColour('modal-background',getColour('tiddler-background','#ffffff'));" +
            "var mbd=getColour('modal-border',getColour('tiddler-border','#cccccc'));" +
            "var fg=getColour('foreground','#333333');" +
            "var mfg=getColour('muted-foreground','#999999');" +
            "var pbg=getColour('page-background','#f4f4f4');" +
            "var pri=getColour('primary','#5778d8');" +
            "var pt=gcc(pri);var bbg=getColour('button-background','#f0f0f0');" +
            "var bfg=gcc(bbg);var bbd=getColour('button-border','#cccccc');" +
            // overlay
            "modalOverlay=document.createElement('div');modalOverlay.id='td-conflict-modal-overlay';" +
            "modalOverlay.style.cssText='position:fixed;top:0;left:0;right:0;bottom:0;background:rgba(0,0,0,0.5);z-index:10001;display:flex;align-items:flex-start;justify-content:center;padding:40px 20px;overflow:auto;';" +
            // modal container
            "var modal=document.createElement('div');" +
            "modal.style.cssText='background:'+pbg+';color:'+fg+';border-radius:8px;border:1px solid '+mbd+';box-shadow:0 8px 32px rgba(0,0,0,0.3);max-width:700px;width:100%;max-height:calc(100vh - 80px);display:flex;flex-direction:column;';" +
            // header
            "var hdr=document.createElement('div');hdr.style.cssText='display:flex;align-items:center;padding:16px 20px;border-bottom:1px solid '+mbd+';background:'+mbg+';border-radius:8px 8px 0 0;flex-shrink:0;';" +
            "var ht=document.createElement('span');ht.textContent='Sync Conflicts';ht.style.cssText='flex:1;font-size:18px;font-weight:600;';" +
            "var cb=document.createElement('button');cb.textContent='Close';cb.style.cssText='padding:6px 14px;background:'+bbg+';color:'+bfg+';border:1px solid '+bbd+';border-radius:4px;cursor:pointer;font-size:13px;';" +
            "cb.onclick=function(){closeModal();};hdr.appendChild(ht);hdr.appendChild(cb);" +
            // body
            "var body=document.createElement('div');body.style.cssText='padding:16px 20px;overflow-y:auto;flex:1;';" +
            "for(var i=0;i<conflicts.length;i++){body.appendChild(renderCard(conflicts[i],mbg,mbd,fg,mfg,pri,pt,bbg,bfg,bbd));}" +
            // footer
            "var ftr=document.createElement('div');ftr.style.cssText='padding:12px 20px;border-top:1px solid '+mbd+';background:'+mbg+';border-radius:0 0 8px 8px;display:flex;justify-content:flex-end;gap:8px;flex-shrink:0;';" +
            "var alb=document.createElement('button');alb.textContent='Resolve All: Keep Local';alb.style.cssText='padding:8px 16px;background:'+bbg+';color:'+bfg+';border:1px solid '+bbd+';border-radius:4px;cursor:pointer;font-size:13px;';" +
            "alb.onclick=function(){resolveAll('local');};" +
            "var arb=document.createElement('button');arb.textContent='Resolve All: Keep Remote';arb.style.cssText='padding:8px 16px;background:'+pri+';color:'+pt+';border:1px solid '+pri+';border-radius:4px;cursor:pointer;font-size:13px;';" +
            "arb.onclick=function(){resolveAll('remote');};" +
            "ftr.appendChild(alb);ftr.appendChild(arb);" +
            "modal.appendChild(hdr);modal.appendChild(body);modal.appendChild(ftr);modalOverlay.appendChild(modal);document.body.appendChild(modalOverlay);" +
            "modalOverlay.addEventListener('click',function(e){if(e.target===modalOverlay)closeModal();});" +
            "modalOverlay.__esc=function(e){if(e.key==='Escape')closeModal();};document.addEventListener('keydown',modalOverlay.__esc);}" +
            // renderCard
            "function renderCard(ct,mbg,mbd,fg,mfg,pri,pt,bbg,bfg,bbd){" +
            "var cf=" + "\$tw.wiki.getTiddler(ct);if(!cf)return document.createElement('div');" +
            "var ot=cf.fields['conflict-original-title']||'';var ts=cf.fields['conflict-timestamp']||'';var orig=" + "\$tw.wiki.getTiddler(ot);" +
            "var card=document.createElement('div');card.style.cssText='background:'+mbg+';border:1px solid '+mbd+';border-radius:6px;padding:16px;margin-bottom:12px;';" +
            "card.setAttribute('data-conflict-title',ct);" +
            // title
            "var tl=document.createElement('div');tl.style.cssText='font-size:15px;font-weight:600;margin-bottom:4px;';tl.textContent=ot;card.appendChild(tl);" +
            // timestamp
            "var tsd=document.createElement('div');tsd.style.cssText='font-size:12px;color:'+mfg+';margin-bottom:12px;';tsd.textContent='Conflicted at: '+fmtTs(ts);card.appendChild(tsd);" +
            // fields
            "var lf=cf.fields;var rf=orig?orig.fields:{};" +
            "var skip={'title':1,'conflict-original-title':1,'conflict-timestamp':1,'conflict-source':1};" +
            "var af={};var k;for(k in lf){if(!skip[k])af[k]=1;}for(k in rf){if(!skip[k])af[k]=1;}" +
            "var fns=Object.keys(af).sort();var hasDiffs=false;" +
            "for(var i=0;i<fns.length;i++){var fn=fns[i];var lv=lf[fn];var rv=rf[fn];var ls=lv!=null?String(lv):'';var rs=rv!=null?String(rv):'';" +
            "if(ls===rs)continue;hasDiffs=true;" +
            "var fs=document.createElement('div');fs.style.cssText='margin-bottom:10px;';" +
            "var fl=document.createElement('div');fl.style.cssText='font-size:12px;font-weight:600;color:'+mfg+';text-transform:uppercase;letter-spacing:0.5px;margin-bottom:4px;';fl.textContent=fn;fs.appendChild(fl);" +
            "if(fn==='text'){fs.appendChild(renderDiff(ls,rs));}else{" +
            "var cmp=document.createElement('div');cmp.style.cssText='font-size:13px;padding:6px 8px;border:1px solid '+mbd+';border-radius:4px;background:'+getColour('tiddler-background','#ffffff')+';';" +
            "var ll=document.createElement('div');ll.style.cssText='margin-bottom:2px;';" +
            "var llb=document.createElement('span');llb.textContent='Local: ';llb.style.cssText='font-weight:600;color:#900;';" +
            "var llv=document.createElement('span');llv.textContent=ls||'(empty)';ll.appendChild(llb);ll.appendChild(llv);" +
            "var rl=document.createElement('div');var rlb=document.createElement('span');rlb.textContent='Remote: ';rlb.style.cssText='font-weight:600;color:#060;';" +
            "var rlv=document.createElement('span');rlv.textContent=rs||'(empty)';rl.appendChild(rlb);rl.appendChild(rlv);" +
            "cmp.appendChild(ll);cmp.appendChild(rl);fs.appendChild(cmp);}card.appendChild(fs);}" +
            "if(!hasDiffs){var nd=document.createElement('div');nd.style.cssText='font-size:13px;color:'+mfg+';font-style:italic;margin-bottom:10px;';nd.textContent='All fields are identical (conflict may have been resolved externally).';card.appendChild(nd);}" +
            // action buttons
            "var acts=document.createElement('div');acts.style.cssText='display:flex;gap:8px;margin-top:8px;';" +
            "var klb=document.createElement('button');klb.textContent='Keep Local';klb.style.cssText='padding:6px 14px;background:'+bbg+';color:'+bfg+';border:1px solid '+bbd+';border-radius:4px;cursor:pointer;font-size:13px;';" +
            "klb.onclick=(function(c,cd){return function(){resolveConflict(c,'local');cd.remove();afterResolve();};})(ct,card);" +
            "var krb=document.createElement('button');krb.textContent='Keep Remote';krb.style.cssText='padding:6px 14px;background:'+pri+';color:'+pt+';border:1px solid '+pri+';border-radius:4px;cursor:pointer;font-size:13px;';" +
            "krb.onclick=(function(c,cd){return function(){resolveConflict(c,'remote');cd.remove();afterResolve();};})(ct,card);" +
            "acts.appendChild(klb);acts.appendChild(krb);card.appendChild(acts);return card;}" +
            // init
            "function init(){if(typeof " + "\$tw==='undefined'||!\$tw.wiki){setTimeout(init,200);return;}" +
            "updateBanner();" +
            "\$tw.wiki.addEventListener('change',function(changes){var rel=false;for(var t in changes){if(t.indexOf(CONFLICT_PREFIX)===0){rel=true;break;}}if(rel){bannerDismissed=false;updateBanner();}});}" +
            "if(document.readyState==='loading'){document.addEventListener('DOMContentLoaded',function(){init();});}else{init();}" +
            "})()"

        // Peer status badge: shows connected LAN sync peers in TopRightBar
        val peerStatusScript = "(function(){" +
            "'use strict';" +
            "if(!window.__WIKI_PATH__)return;" +
            "var S=window.TiddlyDesktopSync;if(!S)return;" +
            "var POLL=5000;" +
            "var PT='\$:/temp/tiddlydesktop/connected-peers';" +
            "var CT='\$:/temp/tiddlydesktop/peer-count';" +
            "var BT='\$:/temp/tiddlydesktop/PeerBadge';" +
            "var EBT='\$:/temp/tiddlydesktop/EditingBadge';" +
            "var lastJ='';" +
            "var _lastRelay=false;" +
            "function waitTw(cb){if(typeof \$tw!=='undefined'&&\$tw.wiki&&\$tw.wiki.addTiddler){cb();}else{setTimeout(function(){waitTw(cb);},200);}}" +
            "function fetch(cb){try{var j=S.getSyncStatus();cb(JSON.parse(j||'{}'));}catch(_){cb({});}}" +
            "function announce(n){try{S.announceUsername(n);}catch(_){}}" +
            "function update(st){var p=st.connected_peers||[];var j=JSON.stringify(p);if(j!==lastJ){lastJ=j;" +
            "\$tw.wiki.addTiddler({title:PT,type:'application/json',text:j});" +
            "\$tw.wiki.addTiddler({title:CT,text:String(p.length)});}" +
            "var rc=st.relay_connected||false;if(rc!==_lastRelay){_lastRelay=rc;try{S.setRelayConnected(rc);}catch(_){}}}" +
            "function createBadge(){if(\$tw.wiki.tiddlerExists(BT))return;" +
            "var wt=" +
            "'\\\\define peer-badge-styles()\\n" +
            ".td-peer-badge{display:inline-block;cursor:pointer;padding:2px 6px;position:relative;}\\n" +
            ".td-peer-badge svg{width:18px;height:18px;vertical-align:middle;fill:<<colour foreground>>;}\\n" +
            ".td-peer-badge-count{font-size:0.75em;vertical-align:top;margin-left:1px;}\\n" +
            ".td-peer-dropdown{position:absolute;right:0;top:100%;background:<<colour dropdown-background>>;border:1px solid <<colour dropdown-border>>;border-radius:4px;padding:6px 0;min-width:200px;box-shadow:1px 1px 5px rgba(0,0,0,0.15);z-index:1000;white-space:nowrap;}\\n" +
            ".td-peer-dropdown-item{padding:4px 12px;font-size:0.85em;}\\n" +
            ".td-peer-dropdown-item-name{font-weight:bold;}\\n" +
            ".td-peer-dropdown-item-device{color:<<colour muted-foreground>>;font-size:0.85em;}\\n" +
            ".td-peer-dropdown-empty{padding:8px 12px;color:<<colour muted-foreground>>;font-size:0.85em;}\\n" +
            "\\\\end\\n" +
            "<\$reveal type=\"nomatch\" state=\"'+CT+'\" text=\"0\" default=\"0\">\\n" +
            "<\$reveal type=\"nomatch\" state=\"'+CT+'\" text=\"\" default=\"0\">\\n" +
            "<\$button popup=\"\\$:/state/tiddlydesktop/peer-dropdown\" class=\"tc-btn-invisible td-peer-badge\" tooltip=\"Connected peers\">\\n" +
            "<\$text text={{'+CT+'}}/> {{\\$:/core/images/globe}}\\n" +
            "</\$button>\\n" +
            "<\$reveal state=\"\\$:/state/tiddlydesktop/peer-dropdown\" type=\"popup\" position=\"belowleft\">\\n" +
            "<div class=\"td-peer-dropdown\">\\n" +
            "<\$list filter=\"[['+PT+']jsonindexes[]]\" variable=\"idx\" emptyMessage=\"\"\"<div class=\\\"td-peer-dropdown-empty\\\">No peers connected</div>\"\"\">\\n" +
            "<div class=\"td-peer-dropdown-item\">\\n" +
            "<\$let userName={{{ [['+PT+']jsonget<idx>,[user_name]] }}} deviceName={{{ [['+PT+']jsonget<idx>,[device_name]] }}}>\\n" +
            "<\$reveal type=\"nomatch\" default=<<userName>> text=\"\">\\n" +
            "<span class=\"td-peer-dropdown-item-name\"><\$text text=<<userName>>/></span> <span class=\"td-peer-dropdown-item-device\">(<\$text text=<<deviceName>>/>)</span>\\n" +
            "</\$reveal>\\n" +
            "<\$reveal type=\"match\" default=<<userName>> text=\"\">\\n" +
            "<span class=\"td-peer-dropdown-item-name\">Anonymous</span> <span class=\"td-peer-dropdown-item-device\">(<\$text text=<<deviceName>>/>)</span>\\n" +
            "</\$reveal>\\n" +
            "</\$let></div>\\n" +
            "</\$list></div>\\n" +
            "</\$reveal></\$reveal></\$reveal>\\n" +
            "<style><<peer-badge-styles>></style>';" +
            "\$tw.wiki.addTiddler({title:BT,tags:'\$:/tags/TopRightBar',text:wt});}" +
            "function createEditBadge(){if(\$tw.wiki.tiddlerExists(EBT))return;" +
            "var eb=" +
            "'\\\\define editing-badge-styles()\\n" +
            ".td-editing-badge{display:inline-block;font-size:0.8em;padding:2px 8px;margin:0 0 4px 0;border-radius:10px;background:<<colour notification-background>>;border:1px solid <<colour notification-border>>;color:<<colour foreground>>;}\\n" +
            ".td-editing-badge svg{width:14px;height:14px;vertical-align:middle;fill:<<colour foreground>>;margin-right:3px;}\\n" +
            "\\\\end\\n" +
            "<\$set name=\"editingTid\" value={{{ [[\\$:/temp/tiddlydesktop/editing/]addsuffix<currentTiddler>] }}}>\\n" +
            "<\$list filter=\"[<editingTid>is[tiddler]]\" variable=\"ignore\">\\n" +
            "<div class=\"td-editing-badge\">\\n" +
            "{{\\$:/core/images/edit-button}} \\n" +
            "<\$list filter=\"[<editingTid>jsonindexes[]]\" variable=\"idx\" counter=\"cnt\">\\n" +
            "<\$let un={{{ [<editingTid>jsonget<idx>,[user_name]] }}} dn={{{ [<editingTid>jsonget<idx>,[device_name]] }}}>\\n" +
            "<\$reveal type=\"nomatch\" default=<<un>> text=\"\"><\$text text=<<un>>/></\$reveal>\\n" +
            "<\$reveal type=\"match\" default=<<un>> text=\"\"><\$text text=<<dn>>/></\$reveal>\\n" +
            "</\$let>\\n" +
            "<\$list filter=\"[<editingTid>jsonindexes[]count[]compare:number:gt<cnt-first>]\" variable=\"ignore\">, </\$list>\\n" +
            "</\$list>\\n" +
            "</div>\\n" +
            "</\$list>\\n" +
            "</\$set>\\n" +
            "<style><<editing-badge-styles>></style>';" +
            "\$tw.wiki.addTiddler({title:EBT,tags:'\$:/tags/ViewTemplate','list-before':'\$:/core/ui/ViewTemplate/body',text:eb});}" +
            "waitTw(function(){" +
            "\$tw.wiki.addTiddler({title:PT,type:'application/json',text:'[]'});" +
            "\$tw.wiki.addTiddler({title:CT,text:'0'});" +
            "createBadge();createEditBadge();" +
            "var un=\$tw.wiki.getTiddlerText('\$:/status/UserName')||'';" +
            "if(un)announce(un);" +
            "\$tw.wiki.addEventListener('change',function(ch){if(ch['\$:/status/UserName']){var nn=\$tw.wiki.getTiddlerText('\$:/status/UserName')||'';announce(nn);try{var cp=require('\$:/plugins/tiddlywiki/codemirror-6-collab/collab.js');if(cp&&cp.updateUserName)cp.updateUserName(nn||'Anonymous');}catch(_){}}});" +
            "function poll(){fetch(function(st){update(st);});}" +
            "poll();setInterval(poll,POLL);" +
            "});" +
            "})()"

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
                        try {
                            startActivity(intent)
                        } catch (e: android.content.ActivityNotFoundException) {
                            // Try browser_fallback_url if no handler found
                            val fallback = intent.getStringExtra("browser_fallback_url")
                            if (fallback != null) {
                                val fallbackIntent = Intent(Intent.ACTION_VIEW, Uri.parse(fallback))
                                fallbackIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                                startActivity(fallbackIntent)
                            } else {
                                Log.w(TAG, "No handler found for intent:// URI and no fallback URL")
                                android.widget.Toast.makeText(this@WikiActivity, getString(R.string.no_app_for_link), android.widget.Toast.LENGTH_SHORT).show()
                            }
                        }
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to parse/launch intent:// URI: ${e.message}")
                        android.widget.Toast.makeText(this@WikiActivity, getString(R.string.no_app_for_link), android.widget.Toast.LENGTH_SHORT).show()
                    }
                    return true
                }
                // All other URLs (http, https, mailto, tel, sms, geo, etc.)
                // open via the OS-assigned handler.
                // Note: don't use resolveActivity() — it returns null on Android 11+
                // due to package visibility restrictions, even when a handler exists.
                Log.d(TAG, "Opening external URL: $url (scheme=$scheme)")
                try {
                    val intent = Intent(Intent.ACTION_VIEW, uri)
                    intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    startActivity(intent)
                } catch (e: android.content.ActivityNotFoundException) {
                    Log.w(TAG, "No handler found for URL: $url")
                    android.widget.Toast.makeText(this@WikiActivity, getString(R.string.no_app_for_link), android.widget.Toast.LENGTH_SHORT).show()
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to open external URL: ${e.message}")
                    android.widget.Toast.makeText(this@WikiActivity, getString(R.string.no_app_for_link), android.widget.Toast.LENGTH_SHORT).show()
                }
                return true
            }

            @Deprecated("Deprecated in Java")
            override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean {
                return handleUrl(url)
            }

            override fun shouldOverrideUrlLoading(view: WebView, request: android.webkit.WebResourceRequest): Boolean {
                // Allow subframe (iframe) navigations to http/https URLs.
                // This enables YouTube, Vimeo, and other iframe-based embeds
                // to load within the WebView instead of opening externally.
                if (!request.isForMainFrame) {
                    val scheme = request.url.scheme?.lowercase()
                    if (scheme == "http" || scheme == "https") {
                        return false
                    }
                }
                return handleUrl(request.url.toString())
            }

            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
                val url = request.url
                val path = url.path ?: return super.shouldInterceptRequest(view, request)

                // Only intercept requests to our local server — let external requests
                // (YouTube embeds, CDN assets, etc.) go through normally.
                if (url.host != "127.0.0.1") {
                    return super.shouldInterceptRequest(view, request)
                }

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
                    // During boot (before onPageFinished), serve a tiny transparent placeholder
                    // for media files instead of reading full files from SAF. This prevents
                    // 100+ image reads from contending with TiddlyWiki boot. After boot, the
                    // MutationObserver rewrites URLs to attachment ports and images load properly.
                    // Only apply to known media extensions — other paths (e.g. /embed/xxx for
                    // YouTube iframes) must not be replaced with a fake GIF.
                    if (!pageLoaded && isMediaPath(path)) {
                        Log.d(TAG, "Boot placeholder for: $path")
                        return WebResourceResponse(
                            "image/gif", null,
                            200, "OK",
                            mapOf("Cache-Control" to "no-store"),
                            java.io.ByteArrayInputStream(TRANSPARENT_GIF)
                        )
                    }
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
                pageLoaded = true
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
                    // Inject inline PDF.js and video poster extraction
                    view.evaluateJavascript(inlineMediaScript, null)
                    // Inject Android Share toolbar button (ephemeral plugin)
                    view.evaluateJavascript(sharePluginScript, null)
                    // Inject LAN sync script (change hooks + inbound polling)
                    view.evaluateJavascript(lanSyncScript, null)
                    // Inject conflict resolution UI (banner + modal for sync conflicts)
                    view.evaluateJavascript(conflictUiScript, null)
                    // Inject peer status badge (shows connected LAN sync peers)
                    view.evaluateJavascript(peerStatusScript, null)
                    // Import pending Quick Captures
                    importPendingCaptures()
                }
            }

            override fun onRenderProcessGone(view: WebView, detail: android.webkit.RenderProcessGoneDetail): Boolean {
                Log.e(TAG, "WebView render process gone! didCrash=${detail.didCrash()}, " +
                    "rendererPriorityAtExit=${detail.rendererPriorityAtExit()}")
                // Return true to prevent AwBrowserTerminator from killing the entire app.
                // The wiki WebView is dead — finish this activity gracefully.
                try {
                    (view.parent as? android.view.ViewGroup)?.removeView(view)
                    view.destroy()
                } catch (e: Exception) {
                    Log.e(TAG, "Error cleaning up dead WebView: ${e.message}")
                }
                finish()
                return true
            }

            override fun onReceivedError(view: WebView, request: WebResourceRequest, error: android.webkit.WebResourceError) {
                super.onReceivedError(view, request, error)
                // Only handle main frame errors for folder wikis
                if (!isFolder || !request.isForMainFrame) return

                val errorCode = error.errorCode
                Log.w(TAG, "Folder wiki main frame error: code=$errorCode desc=${error.description} url=${request.url}")

                // Connection errors (server not ready or died)
                if (errorCode == ERROR_CONNECT || errorCode == ERROR_TIMEOUT ||
                    errorCode == ERROR_HOST_LOOKUP || errorCode == ERROR_IO) {
                    // Show a reconnecting page instead of the ugly Chrome error
                    val reconnectHtml = """
                        <html><head><meta name="viewport" content="width=device-width,initial-scale=1">
                        <style>body{font-family:sans-serif;display:flex;align-items:center;justify-content:center;
                        min-height:100vh;margin:0;background:#f5f5f5;color:#333}
                        .c{text-align:center}.spinner{width:40px;height:40px;border:4px solid #ddd;
                        border-top:4px solid #5778d8;border-radius:50%;animation:s .8s linear infinite;margin:0 auto 16px}
                        @keyframes s{to{transform:rotate(360deg)}}</style></head>
                        <body><div class="c"><div class="spinner"></div>
                        <p>Connecting to wiki server...</p></div></body></html>
                    """.trimIndent()
                    view.loadData(reconnectHtml, "text/html", "UTF-8")

                    // Try to connect: first poll the existing server (it may just be slow to start),
                    // only restart if it's truly dead after several retries.
                    Thread {
                        val url = currentWikiUrl ?: return@Thread

                        // Poll for existing server (it may still be starting up)
                        for (attempt in 1..8) {
                            Thread.sleep(2000)
                            try {
                                val conn = java.net.URL("$url/status").openConnection() as java.net.HttpURLConnection
                                conn.connectTimeout = 3000
                                conn.readTimeout = 3000
                                val code = conn.responseCode
                                conn.disconnect()
                                if (code == 200) {
                                    Log.d(TAG, "Error handler: server came alive after ${attempt * 2}s")
                                    runOnUiThread {
                                        view.clearHistory()
                                        view.loadUrl(url)
                                    }
                                    return@Thread
                                }
                            } catch (_: Exception) {
                                Log.d(TAG, "Error handler: poll attempt $attempt failed, retrying...")
                            }
                        }

                        // Server is truly dead — restart via centralized method
                        Log.w(TAG, "Error handler: server dead after polling, restarting...")
                        val newUrl = attemptFolderServerRestart()
                        if (newUrl.isNotEmpty()) {
                            Log.d(TAG, "Error handler: server restarted at $newUrl")
                            runOnUiThread {
                                view.clearHistory()
                                view.loadUrl(newUrl)
                            }
                        } else {
                            Log.e(TAG, "Error handler: restart failed or already in progress")
                        }
                    }.start()
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

        // Load the wiki URL (skip for folder wikis that need to start a server first)
        if (!folderServerNeeded) {
            Log.d(TAG, "Loading wiki URL: $wikiUrl")
            webView.loadUrl(wikiUrl)
        } else {
            Log.d(TAG, "Folder wiki: waiting for Node.js server before loading")
        }

        if (folderServerNeeded && !folderLocalPath.isNullOrEmpty()) {
            // Start Node.js server in a background thread from the local filesystem path.
            // The SAF copy was done in the main process; this just starts Node.js.
            val localPath = folderLocalPath!!
            Thread {
                Log.d(TAG, "Background thread: starting Node.js server from local path: $localPath")
                val serverUrl = try {
                    startFolderWikiServerFromLocal(localPath, wikiPath!!)
                } catch (e: Throwable) {
                    Log.e(TAG, "Background thread: failed to start folder wiki server: ${e.message}", e)
                    "ERROR:${e.message}"
                }
                if (serverUrl.startsWith("ERROR:")) {
                    Log.e(TAG, "Background thread: folder wiki server failed: $serverUrl")
                    runOnUiThread {
                        webView.loadData(
                            "<html><body style='background:#1a1a2e;color:#e0e0e0;font-family:sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0'>" +
                            "<div style='text-align:center'><h2>Failed to start wiki server</h2><p>${serverUrl.removePrefix("ERROR:")}</p></div></body></html>",
                            "text/html", "UTF-8"
                        )
                    }
                    return@Thread
                }
                Log.d(TAG, "Background thread: Node.js server started at: $serverUrl")
                currentWikiUrl = serverUrl
                runOnUiThread {
                    if (!isFinishing) {
                        webView.clearHistory()
                        webView.loadUrl(serverUrl)
                    }
                }

                // Server is up — start watchdog and sync watcher immediately
                folderServerReady = true
                startFolderServerWatchdog()
                if (treeUri != null) {
                    startSyncWatcher(localPath)
                }
            }.start()
        }

        // For folder wikis where server URL was already provided (server already running):
        // start watchdog and sync watcher directly.
        if (isFolder && !folderServerNeeded) {
            folderServerReady = true
            startFolderServerWatchdog()
            if (!folderLocalPath.isNullOrEmpty() && treeUri != null) {
                startSyncWatcher(folderLocalPath!!)
            }
        }

        // Start watchdog to keep main process (LAN sync) alive — only if LAN sync is active.
        // If Android kills the main process, this detects it and restarts.
        if (LanSyncService.isLanSyncActive(applicationContext)) {
            mainProcessWatchdog.postDelayed(mainProcessCheckRunnable, 250L)
        }
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
                        .setTitle(getString(R.string.unsaved_title))
                        .setMessage(getString(R.string.unsaved_message, changeCount))
                        .setPositiveButton(getString(R.string.unsaved_save_close)) { _, _ ->
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
                        .setNegativeButton(getString(R.string.unsaved_discard_close)) { _, _ ->
                            finish()
                        }
                        .setNeutralButton(getString(R.string.btn_cancel), null)
                        .show()
                }
            } else {
                finish()
            }
        }
    }

    override fun onPause() {
        super.onPause()
        wasPaused = true
        webView.onPause()
    }

    override fun onResume() {
        super.onResume()
        webView.onResume()

        // Detect stalled or dead WebView after returning from background.
        // Two scenarios cause a black screen:
        // 1. Android killed the renderer process while backgrounded (common on low memory)
        // 2. WebView's HTTP connection pool (6 slots per origin) is exhausted by stalled
        //    media requests, preventing even the page HTML from loading
        // Check JS responsiveness with a timeout — if no response, restart server + reload.
        if (wasPaused && ::webView.isInitialized && currentWikiUrl != null) {
            var jsResponded = false
            webView.evaluateJavascript("'alive'") { jsResponded = true }
            webView.postDelayed({
                if (!jsResponded && !isFinishing) {
                    Log.w(TAG, "WebView not responding after resume, restarting server and reloading")
                    reloadWikiAfterStall()
                }
            }, 3000)
        }

        // Check if HTTP server needs restart (single-file wikis only)
        // This handles the case where the phone went to sleep and Android killed the socket
        if (!isFolder && httpServer != null) {
            if (!httpServer!!.isRunning()) {
                Log.d(TAG, "HTTP server died while paused, restarting...")
                reloadWikiAfterStall()
            } else {
                Log.d(TAG, "HTTP server still running on port ${httpServer!!.port}")
            }
        }

        // For folder wikis using Node.js server, we can check if the server is still accessible
        // by attempting a quick HTTP request. If it fails, show a reload button.
        // Import pending captures only AFTER health check passes (otherwise JS can't run).
        // Skip if server hasn't finished starting yet (avoids false restart during initial boot).
        if (isFolder && folderServerReady) {
            checkFolderServerHealth(onHealthy = {
                // Server is healthy — safe to import captures
                if (::webView.isInitialized) {
                    webView.evaluateJavascript("typeof \$tw!=='undefined'&&\$tw.wiki?'ready':'no'") { r ->
                        if (r?.contains("ready") == true) importPendingCaptures()
                    }
                }
            })
        } else {
            // Single-file wiki: check for pending captures directly
            if (::webView.isInitialized) {
                webView.evaluateJavascript("typeof \$tw!=='undefined'&&\$tw.wiki?'ready':'no'") { r ->
                    if (r?.contains("ready") == true) importPendingCaptures()
                }
            }
        }

        // Trigger sync activation check — JS setInterval is throttled while backgrounded,
        // so on resume we explicitly check if sync was enabled while we were away
        if (wasPaused && ::webView.isInitialized) {
            webView.evaluateJavascript(
                "(function(){if(window.__tdCheckSyncActivation)window.__tdCheckSyncActivation();})()"
            , null)
        }

    }

    /**
     * Reload the wiki after detecting a stalled or dead WebView.
     * For single-file wikis: restarts the HTTP server on a new port to clear stale
     * connections (the old port's connections are stuck in the WebView's pool).
     * For folder wikis: just reloads the page URL.
     */
    private fun reloadWikiAfterStall() {
        pageLoaded = false
        if (!isFolder && httpServer != null) {
            try {
                val newUrl = httpServer!!.restart()
                Log.d(TAG, "Server restarted at: $newUrl")
                currentWikiUrl = newUrl
                webView.loadUrl(newUrl)
            } catch (e: Exception) {
                Log.e(TAG, "Failed to restart HTTP server: ${e.message}")
                webView.loadData(
                    "<html><body><h1>Server Error</h1><p>Failed to restart wiki server: ${e.message}</p><p>Please close and reopen this wiki.</p></body></html>",
                    "text/html",
                    "UTF-8"
                )
            }
        } else {
            val url = currentWikiUrl
            if (url != null) {
                webView.loadUrl(url)
            }
        }
    }

    /**
     * Attempt to restart the folder wiki server. Thread-safe: only one restart runs at a time.
     * Returns the new URL on success, empty string on failure.
     */
    private fun attemptFolderServerRestart(): String {
        if (folderServerRestarting) {
            Log.d(TAG, "Folder server restart already in progress, skipping")
            return ""
        }
        folderServerRestarting = true
        try {
            val path = wikiPath ?: return ""
            Log.w(TAG, "Attempting folder server restart for: $path")
            // Use local path if available (SAF wikis), otherwise fall back to restartFolderWikiServer
            val newUrl = if (!folderLocalPath.isNullOrEmpty()) {
                startFolderWikiServerFromLocal(folderLocalPath!!, path)
            } else {
                restartFolderWikiServer(path)
            }
            if (newUrl.isNotEmpty() && !newUrl.startsWith("ERROR:")) {
                Log.d(TAG, "Folder server restarted at: $newUrl")
                currentWikiUrl = newUrl
                return newUrl
            }
            Log.e(TAG, "Folder server restart returned: $newUrl")
            return ""
        } catch (e: Exception) {
            Log.e(TAG, "Folder server restart failed: ${e.message}")
            return ""
        } finally {
            folderServerRestarting = false
        }
    }

    /**
     * Check if the folder wiki's Node.js server is still accessible.
     * Uses currentWikiUrl (updated after restarts) instead of the original intent URL.
     * Retries up to 3 times with increasing delay to handle the server
     * waking up after the app returns from background.
     * If not reachable after retries, attempts to restart the server via JNI.
     */
    private fun checkFolderServerHealth(onHealthy: (() -> Unit)? = null) {
        val serverUrl = currentWikiUrl ?: return

        Thread {
            val maxRetries = 3
            val delays = longArrayOf(500, 1500, 3000) // ms before each retry
            var healthy = false

            for (attempt in 0 until maxRetries) {
                if (attempt > 0) {
                    Thread.sleep(delays[attempt])
                }
                try {
                    val url = java.net.URL("$serverUrl/status")
                    val connection = url.openConnection() as java.net.HttpURLConnection
                    connection.connectTimeout = 3000
                    connection.readTimeout = 3000
                    connection.requestMethod = "GET"

                    val responseCode = connection.responseCode
                    connection.disconnect()

                    if (responseCode == 200) {
                        Log.d(TAG, "Folder wiki server is healthy (attempt ${attempt + 1})")
                        healthy = true
                        break
                    } else {
                        Log.w(TAG, "Folder wiki server health check attempt ${attempt + 1} failed: HTTP $responseCode")
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "Folder wiki server health check attempt ${attempt + 1} failed: ${e.message}")
                }
            }

            if (healthy) {
                if (onHealthy != null) {
                    runOnUiThread { onHealthy() }
                }
            } else {
                val newUrl = attemptFolderServerRestart()
                if (newUrl.isNotEmpty()) {
                    runOnUiThread {
                        webView.clearHistory()
                        webView.loadUrl(newUrl)
                    }
                } else {
                    showServerUnavailableMessage(serverUrl)
                }
            }
        }.start()
    }

    /**
     * Start a background watchdog thread for folder wikis.
     * Periodically checks if the Node.js server is reachable and auto-restarts it
     * if it dies. This handles:
     * - Main process killed (landing page closed, memory pressure)
     * - User switching away from the app
     * - Heavy wikis causing memory pressure
     *
     * The restarted server runs in the :wiki process (protected by foreground service),
     * so it won't die again unless the :wiki process itself is killed.
     */
    private fun startFolderServerWatchdog() {
        if (folderWatchdogRunning) return
        folderWatchdogRunning = true

        folderWatchdog = Thread {
            Log.d(TAG, "Folder server watchdog started")
            // Brief initial wait to let the server finish startup
            Thread.sleep(2000)

            while (folderWatchdogRunning && !isFinishing) {
                try {
                    Thread.sleep(5000) // Check every 5 seconds
                } catch (_: InterruptedException) {
                    break
                }
                if (!folderWatchdogRunning || isFinishing) break

                val url = currentWikiUrl ?: continue
                var healthy = false

                try {
                    val conn = java.net.URL("$url/status").openConnection() as java.net.HttpURLConnection
                    conn.connectTimeout = 3000
                    conn.readTimeout = 3000
                    conn.requestMethod = "GET"
                    val code = conn.responseCode
                    conn.disconnect()
                    healthy = (code == 200)
                } catch (_: Exception) {
                    // Server is down
                }

                if (healthy) continue

                // Server is dead — attempt restart via centralized method
                Log.w(TAG, "Watchdog: folder server unreachable at $url, restarting...")

                val newUrl = attemptFolderServerRestart()
                if (newUrl.isNotEmpty()) {
                    Log.d(TAG, "Watchdog: server restarted at $newUrl")
                    runOnUiThread {
                        // Remove any error banner
                        webView.evaluateJavascript(
                            "document.getElementById('td-server-unavailable')?.remove()", null
                        )
                        webView.clearHistory()
                        webView.loadUrl(newUrl)
                    }
                } else {
                    Log.e(TAG, "Watchdog: restart failed or already in progress")
                }

                // After a restart attempt, wait longer before next check to allow server startup
                try { Thread.sleep(10000) } catch (_: InterruptedException) { break }
            }
            Log.d(TAG, "Folder server watchdog stopped")
        }.apply {
            isDaemon = true
            name = "FolderServerWatchdog"
            start()
        }
    }

    /**
     * Stop the folder server watchdog thread.
     */
    private fun stopFolderServerWatchdog() {
        folderWatchdogRunning = false
        folderWatchdog?.interrupt()
        folderWatchdog = null
    }

    /**
     * Start the SyncWatcher using FileObserver (inotify) for instant change detection.
     * Syncs local folder wiki changes back to SAF. Runs in the :wiki process so it
     * survives main process death.
     */
    private fun startSyncWatcher(localPath: String) {
        if (syncWatcherRunning) return
        syncWatcherRunning = true

        val safTreeUri = treeUri ?: return
        val localBase = File(localPath)
        syncDeleteHandler = Handler(Looper.getMainLooper())

        Log.d(TAG, "[SyncWatcher] Starting FileObserver for: $localPath")

        // Recursively watch the directory tree
        val fileCount = java.util.concurrent.atomic.AtomicInteger(0)
        watchDirectoryRecursive(localBase, localBase.absolutePath, safTreeUri, fileCount)
        syncTrackedFileCount.set(fileCount.get())

        Log.d(TAG, "[SyncWatcher] Watching ${fileCount.get()} files across directory tree")
    }

    /**
     * Set up a FileObserver for a directory and all its subdirectories.
     */
    private fun watchDirectoryRecursive(
        dir: File, basePath: String, safTreeUri: Uri,
        fileCount: java.util.concurrent.atomic.AtomicInteger
    ) {
        val mask = FileObserver.CLOSE_WRITE or FileObserver.DELETE or
                   FileObserver.CREATE or FileObserver.MOVED_FROM or FileObserver.MOVED_TO

        val observer = object : FileObserver(dir, mask) {
            override fun onEvent(event: Int, path: String?) {
                if (path == null || !syncWatcherRunning) return
                val file = File(dir, path)
                val relPath = file.absolutePath.removePrefix(basePath).trimStart('/')

                when (event and FileObserver.ALL_EVENTS) {
                    FileObserver.CLOSE_WRITE -> {
                        // File finished writing — sync to SAF immediately
                        Log.d(TAG, "[SyncWatcher] File changed: $relPath")
                        try {
                            syncFileToSaf(File(basePath), relPath, safTreeUri)
                            Log.d(TAG, "[SyncWatcher] Synced: $relPath")
                        } catch (e: Exception) {
                            Log.e(TAG, "[SyncWatcher] Error syncing $relPath: ${e.message}")
                        }
                    }
                    FileObserver.CREATE -> {
                        if (file.isDirectory) {
                            // New subdirectory — add a watcher for it
                            Log.d(TAG, "[SyncWatcher] New directory: $relPath")
                            watchDirectoryRecursive(file, basePath, safTreeUri, fileCount)
                        } else {
                            syncTrackedFileCount.incrementAndGet()
                        }
                        // Note: CREATE for files is followed by CLOSE_WRITE, so no sync here
                    }
                    FileObserver.DELETE, FileObserver.MOVED_FROM -> {
                        syncTrackedFileCount.decrementAndGet()
                        scheduleSafDelete(relPath, safTreeUri)
                    }
                    FileObserver.MOVED_TO -> {
                        if (file.isDirectory) {
                            watchDirectoryRecursive(file, basePath, safTreeUri, fileCount)
                        } else {
                            syncTrackedFileCount.incrementAndGet()
                            Log.d(TAG, "[SyncWatcher] File moved in: $relPath")
                            try {
                                syncFileToSaf(File(basePath), relPath, safTreeUri)
                                Log.d(TAG, "[SyncWatcher] Synced: $relPath")
                            } catch (e: Exception) {
                                Log.e(TAG, "[SyncWatcher] Error syncing moved file $relPath: ${e.message}")
                            }
                        }
                    }
                }
            }
        }

        observer.startWatching()
        synchronized(syncFileObservers) {
            syncFileObservers.add(observer)
        }

        // Recurse into subdirectories and count existing files
        dir.listFiles()?.forEach { child ->
            if (child.isDirectory) {
                watchDirectoryRecursive(child, basePath, safTreeUri, fileCount)
            } else {
                fileCount.incrementAndGet()
            }
        }
    }

    /**
     * Schedule a SAF deletion with debouncing. DELETE events are batched for 500ms
     * so we can detect mass-deletion (directory clear) and skip it.
     */
    private fun scheduleSafDelete(relPath: String, safTreeUri: Uri) {
        pendingSyncDeletes.add(relPath)
        syncDeleteHandler?.removeCallbacksAndMessages(null)
        syncDeleteHandler?.postDelayed({
            flushPendingDeletes(safTreeUri)
        }, 500)
    }

    /**
     * Process batched DELETE events. If a large fraction of tracked files were deleted
     * at once, this is almost certainly a directory clear — skip to avoid wiping SAF.
     */
    private fun flushPendingDeletes(safTreeUri: Uri) {
        val deletes = mutableListOf<String>()
        synchronized(pendingSyncDeletes) {
            deletes.addAll(pendingSyncDeletes)
            pendingSyncDeletes.clear()
        }
        if (deletes.isEmpty()) return

        // Safety: if most tracked files disappeared at once, skip SAF deletion
        val total = deletes.size + syncTrackedFileCount.get()
        if (deletes.size > 5 && total > 2 && deletes.size >= total) {
            Log.w(TAG, "[SyncWatcher] Mass deletion detected (${deletes.size}/$total files) — skipping SAF deletion (likely directory clear)")
            return
        }

        Log.d(TAG, "[SyncWatcher] ${deletes.size} files deleted locally, removing from SAF...")
        Thread {
            for (relPath in deletes) {
                if (!syncWatcherRunning) break
                try {
                    deleteFileFromSaf(relPath, safTreeUri)
                    Log.d(TAG, "[SyncWatcher] Deleted from SAF: $relPath")
                } catch (e: Exception) {
                    Log.e(TAG, "[SyncWatcher] Error deleting $relPath: ${e.message}")
                }
            }
        }.start()
    }

    /**
     * Stop the SyncWatcher and all FileObservers.
     */
    private fun stopSyncWatcher() {
        syncWatcherRunning = false
        synchronized(syncFileObservers) {
            for (observer in syncFileObservers) {
                observer.stopWatching()
            }
            syncFileObservers.clear()
        }
        syncDeleteHandler?.removeCallbacksAndMessages(null)
        syncDeleteHandler = null
        pendingSyncDeletes.clear()
    }

    /**
     * Sync a single local file to SAF by navigating/creating the directory tree.
     */
    private fun syncFileToSaf(localBase: File, relPath: String, safTreeUri: Uri) {
        val localFile = File(localBase, relPath)
        if (!localFile.exists()) return

        val rootDoc = DocumentFile.fromTreeUri(this, safTreeUri) ?: return
        val parts = relPath.split("/")

        // Navigate/create subdirectories
        var currentDoc: DocumentFile = rootDoc
        for (dirName in parts.dropLast(1)) {
            currentDoc = currentDoc.findFile(dirName)
                ?: currentDoc.createDirectory(dirName)
                ?: return
        }

        // Find or create the file
        val fileName = parts.last()
        var file = currentDoc.findFile(fileName)
        if (file == null) {
            val mime = syncGuessMimeType(fileName)
            file = currentDoc.createFile(mime, fileName) ?: return
        }

        // Write content (truncate mode)
        contentResolver.openOutputStream(file.uri, "wt")?.use { os ->
            os.write(localFile.readBytes())
        }
    }

    /**
     * Delete a file from SAF that was deleted locally.
     */
    private fun deleteFileFromSaf(relPath: String, safTreeUri: Uri) {
        val rootDoc = DocumentFile.fromTreeUri(this, safTreeUri) ?: return
        val parts = relPath.split("/")

        // Navigate to parent directory
        var currentDoc: DocumentFile = rootDoc
        for (dirName in parts.dropLast(1)) {
            currentDoc = currentDoc.findFile(dirName) ?: return
        }

        currentDoc.findFile(parts.last())?.delete()
    }

    /**
     * Guess MIME type for SAF file creation based on file extension.
     */
    private fun syncGuessMimeType(fileName: String): String {
        // IMPORTANT: SAF's createFile() appends an extension based on MIME type.
        // Use "application/octet-stream" for types where SAF would add a wrong extension
        // (e.g. "text/plain" → ".txt" appended to "foo.tid" → "foo.tid.txt").
        return when (fileName.substringAfterLast('.', "").lowercase()) {
            "json" -> "application/json"
            "html", "htm" -> "text/html"
            "css" -> "text/css"
            "js" -> "application/javascript"
            "png" -> "image/png"
            "jpg", "jpeg" -> "image/jpeg"
            "gif" -> "image/gif"
            "svg" -> "image/svg+xml"
            "pdf" -> "application/pdf"
            "mp4" -> "video/mp4"
            "webm" -> "video/webm"
            "mp3" -> "audio/mpeg"
            "ogg" -> "audio/ogg"
            "woff" -> "font/woff"
            "woff2" -> "font/woff2"
            else -> "application/octet-stream" // .tid, .meta, .info, etc.
        }
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
                // Folder wiki: Try to restart Node.js server via JNI
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
                            var newUrl = window.TiddlyDesktopServer.restartFolderServer();
                            if (newUrl && newUrl.length > 0) {
                                window.location.href = newUrl;
                            } else {
                                alert('Failed to restart server. Please close and reopen this wiki.');
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

        // Stop main process watchdog
        mainProcessWatchdog.removeCallbacks(mainProcessCheckRunnable)

        // Stop folder server watchdog
        stopFolderServerWatchdog()

        // Stop sync watcher (local -> SAF)
        stopSyncWatcher()

        // Clean up auth overlay if present
        dismissAuthOverlay()

        // Notify LAN sync service (main process) that a wiki closed — only if LAN sync is active
        if (LanSyncService.isLanSyncActive(applicationContext)) {
            LanSyncService.notifyWikiClosed(applicationContext)
        }

        // Notify foreground service that wiki is closed (only if we started it)
        if (notificationStarted) {
            WikiServerService.wikiClosed(applicationContext)
            notificationStarted = false
        }

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
     * Check if the main process (which runs LAN sync) is alive.
     * If it has been killed by the system, restart it by starting
     * LanSyncService (creates the main process) and MainActivity
     * (initializes Tauri runtime → LAN sync re-enables on landing page load).
     */
    private fun checkMainProcessAlive() {
        // Only restart the main process if LAN sync was active.
        // Without this check, the sync notification would appear on every restart
        // even when LAN sync is disabled.
        if (!LanSyncService.isLanSyncActive(applicationContext)) {
            // LAN sync is not active — stop the watchdog, no need to keep checking
            mainProcessWatchdog.removeCallbacks(mainProcessCheckRunnable)
            return
        }
        try {
            val am = getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
            val processes = am.runningAppProcesses ?: return
            val mainProcessAlive = processes.any { it.processName == packageName }
            if (!mainProcessAlive) {
                Log.w(TAG, "Main process is dead — restarting for LAN sync")
                // Start LanSyncService to create the main process with foreground priority
                try {
                    val serviceIntent = Intent(applicationContext, LanSyncService::class.java)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        applicationContext.startForegroundService(serviceIntent)
                    } else {
                        applicationContext.startService(serviceIntent)
                    }
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to restart LanSyncService: ${e.message}")
                }
                // Start MainActivity to initialize the Tauri runtime — LAN sync
                // re-enables automatically when the landing page JS loads.
                try {
                    val activityIntent = Intent(applicationContext, MainActivity::class.java).apply {
                        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_NO_ANIMATION)
                        putExtra("RESTART_FOR_SYNC", true)
                    }
                    applicationContext.startActivity(activityIntent)
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to restart MainActivity: ${e.message}")
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "Error checking main process: ${e.message}")
        }
    }

    /**
     * Serve a bundled asset from the td/ directory in Android assets.
     * Used for PDF.js library files accessed via /_td/ URL prefix.
     */
    /**
     * Serve /_file/{base64path} requests directly via WebView interception.
     * Bypasses the HTTP server for better reliability and performance.
     */
    /** Check if a URL path looks like a file (has an extension like .jpg, .mp4, etc).
     *  Paths without extensions (e.g. /embed/9aBcBGqFBWY) are routes, not files. */
    private fun isMediaPath(path: String): Boolean {
        val lastSlash = path.lastIndexOf('/')
        val filename = if (lastSlash >= 0) path.substring(lastSlash + 1) else path
        val dot = filename.lastIndexOf('.')
        // Has a dot that's not at the start (hidden files) and has chars after it
        return dot > 0 && dot < filename.length - 1
    }

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
     * Generate a TiddlyWiki-format timestamp: YYYYMMDDHHmmssSSS (UTC).
     */
    private fun twTimestamp(): String {
        val now = java.util.Calendar.getInstance(java.util.TimeZone.getTimeZone("UTC"))
        return String.format(
            "%04d%02d%02d%02d%02d%02d%03d",
            now.get(java.util.Calendar.YEAR),
            now.get(java.util.Calendar.MONTH) + 1,
            now.get(java.util.Calendar.DAY_OF_MONTH),
            now.get(java.util.Calendar.HOUR_OF_DAY),
            now.get(java.util.Calendar.MINUTE),
            now.get(java.util.Calendar.SECOND),
            now.get(java.util.Calendar.MILLISECOND)
        )
    }

    /**
     * Import pending Quick Captures that target this wiki.
     * Captures are JSON files in {filesDir}/captures/ written by CaptureActivity.
     * Auto-deletes captures older than 7 days.
     */
    /**
     * Clean up stale capture files older than 24 hours.
     * Also removes orphaned import data files (.dat).
     */
    private fun cleanupStaleCaptureFiles() {
        try {
            val capturesDir = File(filesDir, "captures")
            if (!capturesDir.exists() || !capturesDir.isDirectory) return
            val files = capturesDir.listFiles() ?: return
            val now = System.currentTimeMillis()
            val maxAge = 24 * 60 * 60 * 1000L  // 24 hours

            val referencedImports = mutableSetOf<String>()
            var deletedCount = 0

            for (f in files) {
                if (f.name.startsWith("capture_") && f.name.endsWith(".json")) {
                    try {
                        val json = JSONObject(f.readText())
                        val created = json.optLong("created", 0)
                        if (created > 0 && now - created > maxAge) {
                            val importFile = json.optString("import_file", "")
                            if (importFile.isNotEmpty()) File(capturesDir, importFile).delete()
                            f.delete()
                            deletedCount++
                        } else {
                            val importFile = json.optString("import_file", "")
                            if (importFile.isNotEmpty()) referencedImports.add(importFile)
                        }
                    } catch (e: Exception) {
                        if (now - f.lastModified() > maxAge) { f.delete(); deletedCount++ }
                    }
                }
            }
            // Clean orphaned .dat import files
            for (f in files) {
                if (f.name.startsWith("import_") && f.name.endsWith(".dat") && f.name !in referencedImports) {
                    f.delete(); deletedCount++
                }
            }
            if (deletedCount > 0) Log.d(TAG, "Cleaned up $deletedCount stale capture file(s)")
        } catch (e: Exception) {
            Log.w(TAG, "Capture cleanup failed: ${e.message}")
        }
    }

    // Track which capture files are currently being imported to prevent duplicates
    private val importingCaptures = java.util.Collections.synchronizedSet(mutableSetOf<String>())

    private fun importPendingCaptures() {
        val capturesDir = File(filesDir, "captures")
        if (!capturesDir.exists() || !capturesDir.isDirectory) return
        val myPath = wikiPath ?: return
        val captureFiles = capturesDir.listFiles { f ->
            f.name.startsWith("capture_") && f.name.endsWith(".json")
        } ?: return

        val now = System.currentTimeMillis()
        data class CaptureEntry(val file: File, val json: JSONObject)
        val matching = mutableListOf<CaptureEntry>()
        for (f in captureFiles) {
            // Skip files already being imported
            if (!importingCaptures.add(f.name)) continue
            try {
                val json = JSONObject(f.readText())
                if (now - json.optLong("created", now) > 7 * 86400000L) {
                    f.delete(); importingCaptures.remove(f.name); continue  // expired
                }
                if (json.optString("target_wiki_path") == myPath) {
                    matching.add(CaptureEntry(f, json))
                } else {
                    importingCaptures.remove(f.name)  // not for this wiki
                }
            } catch (e: Exception) {
                Log.w(TAG, "Skipping malformed capture file ${f.name}: ${e.message}")
                importingCaptures.remove(f.name)
            }
        }
        if (matching.isEmpty()) return

        // Separate file imports (native TW import) from direct tiddler captures
        val directCaptures = mutableListOf<CaptureEntry>()
        val fileImports = mutableListOf<CaptureEntry>()
        for (entry in matching) {
            if (entry.json.has("import_file")) {
                fileImports.add(entry)
            } else {
                directCaptures.add(entry)
            }
        }

        // Handle direct tiddler captures (text, images, etc.)
        // Uses tm-import-tiddlers so user sees $:/Import dialog and can review before committing.
        if (directCaptures.isNotEmpty()) {
            val tiddlersJson = JSONArray()
            for (entry in directCaptures) {
                try {
                    val json = entry.json
                    val tiddler = JSONObject()
                    tiddler.put("title", json.optString("title", "Untitled Capture"))
                    tiddler.put("text", json.optString("text", ""))
                    val tags = json.optString("tags", "")
                    if (tags.isNotEmpty()) tiddler.put("tags", tags)
                    tiddler.put("type", json.optString("type", "text/vnd.tiddlywiki"))
                    val sourceUrl = json.optString("source_url", "")
                    if (sourceUrl.isNotEmpty()) tiddler.put("source-url", sourceUrl)
                    val canonicalUri = json.optString("_canonical_uri", "")
                    if (canonicalUri.isNotEmpty()) tiddler.put("_canonical_uri", canonicalUri)
                    val caption = json.optString("caption", "")
                    if (caption.isNotEmpty()) tiddler.put("caption", caption)
                    // Add TiddlyWiki timestamps (format: YYYYMMDDHHmmssSSS)
                    val ts = twTimestamp()
                    tiddler.put("created", ts)
                    tiddler.put("modified", ts)
                    tiddlersJson.put(tiddler)
                } catch (e: Exception) {
                    Log.w(TAG, "Skipping capture: ${e.message}")
                }
            }
            if (tiddlersJson.length() > 0) {
                // Collect filenames for deletion after successful import
                val fileNames = directCaptures.map { it.file.name }
                val base64Payload = android.util.Base64.encodeToString(
                    tiddlersJson.toString().toByteArray(Charsets.UTF_8),
                    android.util.Base64.NO_WRAP
                )
                val js = "(function check(){" +
                    "if(typeof \$tw==='undefined'||!\$tw.wiki||!\$tw.rootWidget" +
                    "||!\$tw.rootWidget.children||!\$tw.rootWidget.children.length){" +
                    "setTimeout(check,200);return;}" +
                    "try{" +
                    // atob() returns Latin-1 chars, but payload is UTF-8 bytes.
                    // Decode UTF-8 properly via TextDecoder (same pattern as file imports).
                    "var b=atob('$base64Payload');" +
                    "var bytes=new Uint8Array(b.length);" +
                    "for(var i=0;i<b.length;i++)bytes[i]=b.charCodeAt(i);" +
                    "var tiddlers=JSON.parse(new TextDecoder().decode(bytes));" +
                    "var w=\$tw.rootWidget;" +
                    "while(w.children&&w.children.length>0)w=w.children[0];" +
                    "w.dispatchEvent({type:'tm-import-tiddlers',param:JSON.stringify(tiddlers)});" +
                    "return 'ok';" +
                    "}catch(e){console.error('Capture import error:',e);return 'error';}" +
                    "})()"
                webView.evaluateJavascript(js) { result ->
                    // Delete capture files only after JS executed successfully
                    if (result != null && result.contains("ok")) {
                        for (entry in directCaptures) {
                            try { entry.file.delete() } catch (_: Exception) {}
                        }
                        Log.d(TAG, "Imported ${directCaptures.size} capture(s), files deleted")
                    } else {
                        Log.w(TAG, "Capture import JS failed, keeping files for retry")
                    }
                    // Release lock either way so retry can pick them up
                    for (name in fileNames) {
                        importingCaptures.remove(name)
                    }
                }
            }
        }

        // Handle file imports — use TiddlyWiki's native import via tm-import-tiddlers
        // The event must be dispatched from a leaf widget so it bubbles UP to NavigatorWidget,
        // which handles creating $:/Import, navigating to it, and merging with existing imports.
        for (entry in fileImports) {
            val importFilename = entry.json.optString("import_file", "")
            val fileType = entry.json.optString("file_type", "text/html")
            val importTitle = entry.json.optString("title", "import")
            if (importFilename.isEmpty()) {
                entry.file.delete(); importingCaptures.remove(entry.file.name); continue
            }

            val importFile = File(capturesDir, importFilename)
            if (!importFile.exists()) {
                entry.file.delete(); importingCaptures.remove(entry.file.name); continue
            }

            try {
                val contentBytes = importFile.readBytes()

                // Base64 encode to safely pass content to JS (avoids all escaping issues)
                val base64Content = android.util.Base64.encodeToString(contentBytes, android.util.Base64.NO_WRAP)
                val safeFileType = escapeForJs(fileType)
                val safeTitle = escapeForJs(importTitle)
                val captureFileName = entry.file.name

                // Poll until TW5 widget tree is ready, then dispatch tm-import-tiddlers
                // from a leaf widget so it bubbles up through NavigatorWidget.
                // NavigatorWidget's handler creates $:/Import, navigates to it, and merges.
                // Pass {title: filename} as srcFields so non-TiddlyWiki HTML gets a title
                // (matches TiddlyWiki's own browser import in $tw.utils.readFile).
                val importJs = "(function check(){" +
                    "if(typeof \$tw==='undefined'||!\$tw.wiki||!\$tw.rootWidget" +
                    "||!\$tw.rootWidget.children||!\$tw.rootWidget.children.length){" +
                    "setTimeout(check,200);return 'wait';}" +
                    "try{" +
                    "var b=atob('$base64Content');" +
                    "var bytes=new Uint8Array(b.length);" +
                    "for(var i=0;i<b.length;i++)bytes[i]=b.charCodeAt(i);" +
                    "var content=new TextDecoder().decode(bytes);" +
                    "var tiddlers=\$tw.wiki.deserializeTiddlers('$safeFileType',content,{title:'$safeTitle'});" +
                    "if(!tiddlers||tiddlers.length===0)return 'empty';" +
                    // Find a leaf widget and dispatch — event bubbles up to NavigatorWidget
                    "var w=\$tw.rootWidget;" +
                    "while(w.children&&w.children.length>0)w=w.children[0];" +
                    "w.dispatchEvent({type:'tm-import-tiddlers',param:JSON.stringify(tiddlers)});" +
                    "return 'ok';" +
                    "}catch(e){console.error('Import error:',e);return 'error';}" +
                    "})()"

                webView.evaluateJavascript(importJs) { result ->
                    if (result != null && (result.contains("ok") || result.contains("empty"))) {
                        try { entry.file.delete() } catch (_: Exception) {}
                        try { importFile.delete() } catch (_: Exception) {}
                        Log.d(TAG, "File import complete, files deleted")
                    } else {
                        Log.w(TAG, "File import JS failed, keeping files for retry")
                    }
                    importingCaptures.remove(captureFileName)
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to process import file $importFilename: ${e.message}")
                entry.file.delete()
                importFile.delete()
                importingCaptures.remove(entry.file.name)
            }
        }
    }
}
