package com.burningtreec.tiddlydesktop_rs

import android.annotation.SuppressLint
import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Color
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
import androidx.documentfile.provider.DocumentFile
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.net.URLDecoder
import java.net.URLEncoder

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

                // Request widget update
                try {
                    RecentWikisWidgetProvider.requestUpdate(context)
                } catch (e: Exception) {
                    Log.w(TAG, "Could not request widget update: ${e.message}")
                }
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

                    // Request widget update to show the new favicon
                    try {
                        RecentWikisWidgetProvider.requestUpdate(context)
                    } catch (e: Exception) {
                        Log.w(TAG, "Could not request widget update: ${e.message}")
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to update favicon in recent_wikis.json: ${e.message}")
            }
        }
    }

    private lateinit var webView: WebView
    private lateinit var rootLayout: FrameLayout
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

    /**
     * JavaScript interface for receiving palette color updates from TiddlyWiki.
     */
    inner class PaletteInterface {
        @JavascriptInterface
        fun setSystemBarColors(statusBarColor: String, navBarColor: String, foregroundColor: String?) {
            Log.d(TAG, "setSystemBarColors called: status=$statusBarColor, nav=$navBarColor, fg=$foregroundColor")
            runOnUiThread {
                updateSystemBarColors(statusBarColor, navBarColor, foregroundColor)
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
    }

    /**
     * JavaScript interface for clipboard operations.
     * Handles copy-to-clipboard since document.execCommand doesn't work reliably in WebView.
     */
    inner class ClipboardInterface {
        /**
         * Copy text to the system clipboard.
         * Returns JSON with success status.
         */
        @JavascriptInterface
        fun copyText(text: String): String {
            Log.d(TAG, "copyText called: ${text.take(50)}...")
            return try {
                val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                val clip = ClipData.newPlainText("TiddlyWiki", text)
                clipboard.setPrimaryClip(clip)
                Log.d(TAG, "Text copied to clipboard: ${text.length} chars")
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
                try {
                    val printManager = getSystemService(Context.PRINT_SERVICE) as android.print.PrintManager
                    val jobName = "${wikiTitle ?: "TiddlyWiki"} - ${System.currentTimeMillis()}"
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

                // Generate filename based on wiki path hash
                val pathHash = wikiPath?.hashCode()?.let { Math.abs(it).toString() } ?: "unknown"
                val extension = when {
                    mimeType.contains("png") -> "png"
                    mimeType.contains("jpeg") || mimeType.contains("jpg") -> "jpg"
                    mimeType.contains("gif") -> "gif"
                    mimeType.contains("svg") -> "svg"
                    else -> "ico"
                }
                val faviconFile = File(faviconsDir, "$pathHash.$extension")

                // Decode base64 and save
                val imageData = android.util.Base64.decode(base64Data, android.util.Base64.DEFAULT)
                faviconFile.writeBytes(imageData)

                Log.d(TAG, "Saved favicon to: ${faviconFile.absolutePath}")

                // Update recent_wikis.json with favicon path
                updateRecentWikisWithFavicon(applicationContext, wikiPath ?: "", faviconFile.absolutePath)

                "{\"success\":true,\"path\":\"${faviconFile.absolutePath}\"}"
            } catch (e: Exception) {
                Log.e(TAG, "Failed to save favicon: ${e.message}")
                "{\"success\":false,\"error\":\"${e.message?.replace("\"", "\\\"") ?: "Unknown"}\"}"
            }
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
                var mimeType: String? = null

                contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                    if (cursor.moveToFirst()) {
                        val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                        val sizeIndex = cursor.getColumnIndex(android.provider.OpenableColumns.SIZE)
                        if (nameIndex >= 0) filename = cursor.getString(nameIndex)
                        if (sizeIndex >= 0) size = cursor.getLong(sizeIndex)
                    }
                }

                mimeType = contentResolver.getType(uri)

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

                // Helper to compare file contents byte-by-byte
                fun filesAreIdentical(uri1: Uri, uri2: Uri): Boolean {
                    try {
                        contentResolver.openInputStream(uri1)?.use { stream1 ->
                            contentResolver.openInputStream(uri2)?.use { stream2 ->
                                val buffer1 = ByteArray(8192)
                                val buffer2 = ByteArray(8192)
                                while (true) {
                                    val read1 = stream1.read(buffer1)
                                    val read2 = stream2.read(buffer2)
                                    if (read1 != read2) return false
                                    if (read1 == -1) return true
                                    if (!buffer1.copyOf(read1).contentEquals(buffer2.copyOf(read2))) return false
                                }
                            }
                        }
                        return false
                    } catch (e: Exception) {
                        Log.w(TAG, "Error comparing files: ${e.message}")
                        return false
                    }
                }

                // Check for existing file and generate unique name (or reuse if identical)
                var finalName = safeName
                var existingFile = attachmentsDir.findFile(safeName)
                if (existingFile != null) {
                    // Check if content is identical - if so, reuse existing file
                    if (filesAreIdentical(sourceUri, existingFile.uri)) {
                        val relativePath = "./attachments/$safeName"
                        Log.d(TAG, "Attachment already exists with identical content: $relativePath")
                        val escapedPath = relativePath.replace("\\", "\\\\").replace("\"", "\\\"")
                        return "{\"success\":true,\"path\":\"$escapedPath\",\"reused\":true}"
                    }

                    // Different content, find unique name
                    val baseName = safeName.substringBeforeLast(".")
                    val ext = safeName.substringAfterLast(".", "")
                    var counter = 1
                    do {
                        finalName = if (ext.isNotEmpty()) "${baseName}-$counter.$ext" else "${baseName}-$counter"
                        existingFile = attachmentsDir.findFile(finalName)
                        // Also check if this numbered version has identical content
                        if (existingFile != null && filesAreIdentical(sourceUri, existingFile.uri)) {
                            val relativePath = "./attachments/$finalName"
                            Log.d(TAG, "Attachment already exists with identical content: $relativePath")
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

                // Copy content
                contentResolver.openInputStream(sourceUri)?.use { input ->
                    contentResolver.openOutputStream(targetFile.uri)?.use { output ->
                        input.copyTo(output)
                    }
                } ?: run {
                    targetFile.delete()
                    Log.e(TAG, "Failed to read source file")
                    return "{\"success\":false,\"error\":\"Failed to read source file\"}"
                }

                val relativePath = "./attachments/$finalName"
                Log.d(TAG, "Attachment copied successfully: $relativePath")

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

    // Tree URI for folder access
    private var treeUri: Uri? = null

    // Pending attachment copy operation (waiting for folder access)
    private var pendingAttachmentCopy: Triple<String, String, String>? = null  // sourceUri, filename, mimeType

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
    private fun updateSystemBarColors(statusBarColorHex: String, navBarColorHex: String, foregroundColorHex: String? = null) {
        try {
            val statusColor = Color.parseColor(statusBarColorHex)
            val navColor = Color.parseColor(navBarColorHex)

            window.statusBarColor = statusColor
            window.navigationBarColor = navColor

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

    /**
     * Enter immersive fullscreen mode (hide status bar and navigation bar).
     */
    private fun enterImmersiveMode() {
        Log.d(TAG, "Entering immersive fullscreen mode")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.insetsController?.let { controller ->
                controller.hide(WindowInsets.Type.statusBars() or WindowInsets.Type.navigationBars())
                controller.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            }
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility = (
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                or View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_FULLSCREEN
            )
        }
    }

    /**
     * Exit immersive fullscreen mode (show status bar and navigation bar).
     */
    private fun exitImmersiveMode() {
        Log.d(TAG, "Exiting immersive fullscreen mode")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.insetsController?.show(WindowInsets.Type.statusBars() or WindowInsets.Type.navigationBars())
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_VISIBLE
        }
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
                        val result = AttachmentInterface().copyToAttachments(sourceUri, filename, mimeType)
                        Log.d(TAG, "Retry attachment copy result: $result")
                        // Notify JavaScript of the result
                        runOnUiThread {
                            webView.evaluateJavascript("""
                                if (window.__pendingAttachmentCallback) {
                                    window.__pendingAttachmentCallback($result);
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

        if (wikiPath.isNullOrEmpty()) {
            Log.e(TAG, "No wiki path provided!")
            finish()
            return
        }

        // Parse the wiki path to get URIs (needed for attachment saving)
        val (wikiUri, parsedTreeUri) = try {
            parseWikiPath(wikiPath!!)
        } catch (e: Exception) {
            Log.e(TAG, "Failed to parse wiki path: ${e.message}")
            finish()
            return
        }
        treeUri = parsedTreeUri
        Log.d(TAG, "Parsed wiki path: wikiUri=$wikiUri, treeUri=$treeUri")

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
                httpServer = WikiHttpServer(this, wikiUri, parsedTreeUri, true, null, false, 0)  // No backups for folder wikis
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
            httpServer = WikiHttpServer(this, wikiUri, parsedTreeUri, false, null, backupsEnabled, backupCount)
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

        // Update the task title
        setTaskDescription(ActivityManager.TaskDescription(wikiTitle))

        // Create and configure WebView
        webView = WebView(this).apply {
            settings.apply {
                javaScriptEnabled = true
                domStorageEnabled = true
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
                    // Immersive fullscreen
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
        }

        // Use FrameLayout wrapper for fullscreen video support
        rootLayout = FrameLayout(this)
        rootLayout.addView(webView)
        setContentView(rootLayout)

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
                function transformUrl(url) {
                    if (!url) return url;
                    // data: and blob: URLs should never be transformed
                    if (url.startsWith('data:') || url.startsWith('blob:')) {
                        return url;
                    }
                    // For folder wikis: transform Node.js server URLs to use Kotlin attachment server
                    // This is needed because Node.js TiddlyWiki server may not support range requests
                    // for video seeking/thumbnail generation
                    if (window.__IS_FOLDER_WIKI__ && url.startsWith('http://') && window.__TD_SERVER_URL__) {
                        var serverBase = window.__TD_SERVER_URL__.replace(/\/$/, '');
                        if (url.startsWith(serverBase + '/')) {
                            // Extract the path after the server URL (e.g., /files/video.mp4 -> files/video.mp4)
                            var path = url.substring(serverBase.length + 1);
                            // Transform to attachment server's /_relative/ endpoint
                            return window.__TD_ATTACHMENT_SERVER_URL__ + '/_relative/' + encodeURIComponent(path);
                        }
                    }
                    // Already transformed or external URL (https:// or http:// from different origin)
                    if (url.startsWith('http://') || url.startsWith('https://')) {
                        return url;
                    }
                    // Absolute paths or content:// URIs -> /_file/ endpoint
                    if (url.startsWith('/') || url.startsWith('content://') || url.startsWith('file://')) {
                        var encoded = btoa(url).replace(/\+/g, '-').replace(/\//g, '_').replace(/=/g, '');
                        return window.__TD_ATTACHMENT_SERVER_URL__ + '/_file/' + encoded;
                    }
                    // Relative paths -> /_relative/ endpoint
                    return window.__TD_ATTACHMENT_SERVER_URL__ + '/_relative/' + encodeURIComponent(url);
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
                console.log('[TiddlyDesktop] URL transform observer installed');

                // ========== Import Hook (th-importing-file) - matches Desktop ==========
                function installImportHook() {
                    if (!${'$'}tw.hooks) {
                        setTimeout(installImportHook, 100);
                        return;
                    }

                    ${'$'}tw.hooks.addHook("th-importing-file", function(info) {
                        var file = info.file;
                        var filename = file.name;
                        var type = info.type;

                        console.log('[TiddlyDesktop] th-importing-file hook: filename=' + filename + ', type=' + type + ', isBinary=' + info.isBinary);

                        // Check if there's a deserializer for this file type
                        var hasDeserializer = false;
                        if (${'$'}tw.Wiki.tiddlerDeserializerModules) {
                            if (${'$'}tw.Wiki.tiddlerDeserializerModules[type]) {
                                hasDeserializer = true;
                            }
                            if (!hasDeserializer && ${'$'}tw.utils.getFileExtensionInfo) {
                                var extInfo = ${'$'}tw.utils.getFileExtensionInfo(type);
                                if (extInfo && ${'$'}tw.Wiki.tiddlerDeserializerModules[extInfo.type]) {
                                    hasDeserializer = true;
                                }
                            }
                            if (!hasDeserializer && ${'$'}tw.config.contentTypeInfo && ${'$'}tw.config.contentTypeInfo[type]) {
                                var deserializerType = ${'$'}tw.config.contentTypeInfo[type].deserializerType;
                                if (deserializerType && ${'$'}tw.Wiki.tiddlerDeserializerModules[deserializerType]) {
                                    hasDeserializer = true;
                                }
                            }
                        }

                        // If there's a deserializer, let TiddlyWiki handle it
                        if (hasDeserializer) {
                            console.log('[TiddlyDesktop] Deserializer found for type ' + type + ', letting TiddlyWiki handle import');
                            return false;
                        }

                        // Check if external attachments are enabled
                        var externalEnabled = ${'$'}tw.wiki.getTiddlerText(CONFIG_ENABLE, "yes") === "yes";

                        // Determine if this is a binary file
                        // TiddlyWiki may not recognize all audio/video MIME types, so check explicitly
                        var isBinaryType = info.isBinary ||
                            type.indexOf('audio/') === 0 ||
                            type.indexOf('video/') === 0 ||
                            type.indexOf('image/') === 0 ||
                            type === 'application/pdf' ||
                            type === 'application/octet-stream';

                        // Only handle binary files when external attachments enabled
                        if (!externalEnabled || !isBinaryType) {
                            console.log('[TiddlyDesktop] Letting TiddlyWiki handle import (external=' + externalEnabled + ', binary=' + info.isBinary + ', isBinaryType=' + isBinaryType + ')');
                            return false; // Let TiddlyWiki handle it normally
                        }

                        console.log('[TiddlyDesktop] Intercepting binary import for external attachment: ' + filename);

                        // Get the content:// URI for this file (stored when file was picked)
                        if (typeof window.TiddlyDesktopAttachments === 'undefined' ||
                            typeof window.TiddlyDesktopAttachments.getFileUri !== 'function') {
                            console.log('[TiddlyDesktop] Attachment interface not available, letting TiddlyWiki handle');
                            return false;
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
                            return false; // Let TiddlyWiki handle it (embed)
                        } catch (e) {
                            console.error('[TiddlyDesktop] Error handling import:', e);
                            return false; // Let TiddlyWiki handle it
                        }
                    });

                    console.log('[TiddlyDesktop] Import hook (th-importing-file) installed');
                }

                // ========== Session Auth Configuration - matches Desktop ==========
                var CONFIG_AUTH_URLS = SESSION_AUTH_PREFIX + "urls";

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

                function refreshUrlList() {
                    // Count from pluginTiddlers (before shadow registration)
                    var count = Object.keys(pluginTiddlers).filter(function(title) {
                        return title.indexOf(SESSION_AUTH_PREFIX + "url/") === 0;
                    }).length;
                    addPluginTiddler({
                        title: CONFIG_AUTH_URLS,
                        text: String(count)
                    });
                    updatePlugin();
                }

                function injectSessionAuthUI() {
                    // Load and inject auth URL tiddlers as shadows
                    var authUrls = loadAuthUrls();
                    authUrls.forEach(function(entry, index) {
                        addPluginTiddler({
                            title: SESSION_AUTH_PREFIX + "url/" + index,
                            name: entry.name,
                            url: entry.url,
                            text: ""
                        });
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

                        // Add shadow tiddler for UI
                        var index = authUrls.length - 1;
                        addPluginTiddler({
                            title: SESSION_AUTH_PREFIX + "url/" + index,
                            name: name,
                            url: url,
                            text: ""
                        });

                        // Clear input fields (these are real tiddlers created by edit-text widget)
                        ${'$'}tw.wiki.deleteTiddler(SESSION_AUTH_PREFIX + "new-name");
                        ${'$'}tw.wiki.deleteTiddler(SESSION_AUTH_PREFIX + "new-url");

                        refreshUrlList();
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

                                // Remove shadow tiddler
                                removePluginTiddler(tiddlerTitle);
                                refreshUrlList();
                            }
                        }
                    });

                    // Message handler: open auth URL in browser
                    ${'$'}tw.rootWidget.addEventListener("tm-tiddlydesktop-open-auth-url", function(event) {
                        var tiddlerTitle = event.param;
                        if (tiddlerTitle) {
                            var tiddler = ${'$'}tw.wiki.getTiddler(tiddlerTitle);
                            if (tiddler && tiddler.fields.url) {
                                // Open in system browser
                                window.open(tiddler.fields.url, '_blank');
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

                    refreshUrlList();
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

                // Initialize - add UI tiddlers then register as plugin
                // Capture dirty state before any modifications
                var originalNumChanges = ${'$'}tw.saverHandler ? ${'$'}tw.saverHandler.numChanges : 0;

                injectSettingsUI();
                injectSessionAuthUI();
                registerPlugin();  // Register all shadow tiddlers as a plugin
                installImportHook();

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
                    var contentLength = new Blob([text]).size;
                    console.log('[TiddlyDesktop Saver] Saving ' + text.length + ' chars (' + contentLength + ' bytes) via ' + method + '...');

                    var xhr = new XMLHttpRequest();
                    xhr.timeout = 60000;
                    xhr.open('PUT', '$wikiUrl', true);
                    xhr.setRequestHeader('Content-Type', 'text/html;charset=UTF-8');

                    xhr.onload = function() {
                        if (xhr.status === 200) {
                            console.log('[TiddlyDesktop Saver] Save successful (' + contentLength + ' bytes)');
                            callback(null);
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

                // Function to update system bar colors via native bridge
                function updateSystemBarColors() {
                    var statusBarColor = getColour('page-background', '#ffffff');
                    var navBarColor = getColour('tiddler-background', statusBarColor);
                    var foregroundColor = getColour('foreground', '#333333');

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

        // Script to handle tm-full-screen message for toggling immersive fullscreen
        val fullscreenScript = """
            (function() {
                // Wait for TiddlyWiki to fully load (including after decryption for encrypted wikis)
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.rootWidget) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // tm-full-screen handler - toggle immersive fullscreen mode
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

                console.log('[TiddlyDesktop] tm-full-screen handler installed');
            })();
        """.trimIndent()

        // Script to handle clipboard operations (tm-copy-to-clipboard)
        val clipboardScript = """
            (function() {
                // Wait for TiddlyWiki to fully load (including after decryption for encrypted wikis)
                if (typeof ${'$'}tw === 'undefined' || !${'$'}tw.rootWidget) {
                    setTimeout(arguments.callee, 100);
                    return;
                }

                // tm-copy-to-clipboard handler - use native clipboard API
                ${'$'}tw.rootWidget.addEventListener('tm-copy-to-clipboard', function(event) {
                    var text = event.param || '';
                    console.log('[TiddlyDesktop] tm-copy-to-clipboard received: ' + text.substring(0, 50) + '...');

                    try {
                        if (window.TiddlyDesktopClipboard && window.TiddlyDesktopClipboard.copyText) {
                            var resultJson = window.TiddlyDesktopClipboard.copyText(text);
                            var result = JSON.parse(resultJson);
                            if (result.success) {
                                console.log('[TiddlyDesktop] Text copied to clipboard');
                                // Show notification if TiddlyWiki's notify is available
                                if (${'$'}tw.notifier && ${'$'}tw.notifier.display) {
                                    ${'$'}tw.notifier.display("${'$'}:/core/images/copy-clipboard");
                                }
                            } else {
                                console.error('[TiddlyDesktop] Failed to copy to clipboard:', result.error);
                            }
                        } else {
                            console.warn('[TiddlyDesktop] Clipboard interface not available, using fallback');
                            // Fallback to navigator.clipboard if available
                            if (navigator.clipboard && navigator.clipboard.writeText) {
                                navigator.clipboard.writeText(text).then(function() {
                                    console.log('[TiddlyDesktop] Copied via navigator.clipboard');
                                    if (${'$'}tw.notifier && ${'$'}tw.notifier.display) {
                                        ${'$'}tw.notifier.display("${'$'}:/core/images/copy-clipboard");
                                    }
                                }).catch(function(err) {
                                    console.error('[TiddlyDesktop] navigator.clipboard.writeText failed:', err);
                                });
                            }
                        }
                    } catch (e) {
                        console.error('[TiddlyDesktop] Error copying to clipboard:', e);
                    }
                    return false;
                });

                console.log('[TiddlyDesktop] tm-copy-to-clipboard handler installed');
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

                        // Create full-screen overlay
                        var overlay = document.createElement('div');
                        overlay.className = 'tc-body tc-single-tiddler-window';
                        overlay.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;z-index:10000;background:var(--background,white);overflow:auto;';

                        // Render styles (same as TW5's windows.js)
                        var styleWidgetNode = ${'$'}tw.wiki.makeTranscludeWidget('${'$'}:/core/ui/PageStylesheet', {
                            document: ${'$'}tw.fakeDocument,
                            variables: variables,
                            importPageMacros: true
                        });
                        var styleContainer = ${'$'}tw.fakeDocument.createElement('style');
                        styleWidgetNode.render(styleContainer, null);
                        var styleElement = document.createElement('style');
                        styleElement.innerHTML = styleContainer.textContent;
                        overlay.appendChild(styleElement);

                        // Render the tiddler using the template
                        var parser = ${'$'}tw.wiki.parseTiddler(template);
                        var widgetNode = ${'$'}tw.wiki.makeWidget(parser, {
                            document: document,
                            parentWidget: ${'$'}tw.rootWidget,
                            variables: variables
                        });
                        var contentDiv = document.createElement('div');
                        widgetNode.render(contentDiv, null);
                        overlay.appendChild(contentDiv);

                        // Set up refresh handler so the overlay stays in sync
                        var refreshHandler = function(changes) {
                            if (styleWidgetNode.refresh(changes, styleContainer, null)) {
                                styleElement.innerHTML = styleContainer.textContent;
                            }
                            widgetNode.refresh(changes);
                        };
                        ${'$'}tw.wiki.addEventListener('change', refreshHandler);

                        // Track in windows with a mock window object
                        // saver-handler iterates windows and accesses win.document.body
                        // tm-close-all-windows calls win.close() on each entry
                        ${'$'}tw.windows = ${'$'}tw.windows || {};

                        var stackEntry = {
                            tiddler: title,
                            windowID: windowID,
                            overlay: overlay,
                            refreshHandler: refreshHandler
                        };

                        var fakeWin = {
                            __tdOverlay: true,
                            document: {
                                body: overlay,
                                documentElement: overlay,
                                createElement: function(tag) { return document.createElement(tag); },
                                head: document.head
                            },
                            close: function() {
                                ${'$'}tw.wiki.removeEventListener('change', stackEntry.refreshHandler);
                                delete ${'$'}tw.windows[windowID];
                                if (stackEntry.overlay.parentNode) stackEntry.overlay.parentNode.removeChild(stackEntry.overlay);
                                var idx = window.__tdOpenWindowStack.indexOf(stackEntry);
                                if (idx >= 0) window.__tdOpenWindowStack.splice(idx, 1);
                            },
                            focus: function() {},
                            addEventListener: function() {},
                            haveInitialisedWindow: true
                        };
                        ${'$'}tw.windows[windowID] = fakeWin;

                        // Add to DOM
                        document.body.appendChild(overlay);

                        // Push to stack for back button
                        window.__tdOpenWindowStack.push(stackEntry);

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

                // Try to get favicon tiddler
                var faviconTiddler = ${'$'}tw.wiki.getTiddler('${'$'}:/favicon.ico');
                if (!faviconTiddler) {
                    console.log('[TiddlyDesktop] No favicon tiddler found');
                    return;
                }

                var faviconText = faviconTiddler.fields.text || '';
                var faviconType = faviconTiddler.fields.type || 'image/x-icon';

                if (!faviconText) {
                    console.log('[TiddlyDesktop] Favicon tiddler has no content');
                    return;
                }

                // The favicon is typically stored as base64 data
                // Remove any data URL prefix if present
                var base64Data = faviconText;
                if (base64Data.indexOf('base64,') !== -1) {
                    base64Data = base64Data.split('base64,')[1];
                }

                // Clean up whitespace that might be in the base64 data
                base64Data = base64Data.replace(/\s/g, '');

                if (base64Data && window.TiddlyDesktopFavicon) {
                    console.log('[TiddlyDesktop] Extracting favicon, type:', faviconType, 'length:', base64Data.length);
                    var result = window.TiddlyDesktopFavicon.saveFavicon(base64Data, faviconType);
                    console.log('[TiddlyDesktop] Favicon save result:', result);
                }
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

                // ---- Plyr video enhancement with thumbnail generation ----
                var THUMB_WIDTH = 160;
                var THUMB_HEIGHT = 90;
                var THUMB_INTERVAL = 5; // seconds between thumbnails
                var MAX_THUMBS = 60; // max thumbnails to generate

                // Add CSS to hide native video controls until Plyr is ready
                if (!document.querySelector('#td-plyr-hide-styles')) {
                    var style = document.createElement('style');
                    style.id = 'td-plyr-hide-styles';
                    style.textContent = 'video.td-plyr-pending{visibility:hidden;position:absolute;width:1px;height:1px;overflow:hidden;}';
                    document.head.appendChild(style);
                }

                function generateThumbnails(videoSrc, callback) {
                    var video = document.createElement('video');
                    video.muted = true;
                    video.playsInline = true;
                    video.preload = 'metadata';
                    // Set crossOrigin for CORS-enabled servers (needed for canvas capture)
                    video.crossOrigin = 'anonymous';

                    var thumbs = [];
                    var duration = 0;
                    var currentIndex = 0;
                    var timestamps = [];
                    var timeoutId = null;
                    var done = false;

                    function finish(result) {
                        if (done) return;
                        done = true;
                        if (timeoutId) clearTimeout(timeoutId);
                        video.src = ''; // Stop loading
                        callback(result);
                    }

                    // Timeout after 10 seconds
                    timeoutId = setTimeout(function() {
                        console.warn('[TD-Plyr] Thumbnail generation timed out');
                        finish(null);
                    }, 10000);

                    video.onloadedmetadata = function() {
                        duration = video.duration;
                        if (!duration || !isFinite(duration) || duration < 2) {
                            console.log('[TD-Plyr] Video too short or invalid duration:', duration);
                            finish(null);
                            return;
                        }

                        // Calculate timestamps
                        var interval = Math.max(THUMB_INTERVAL, duration / MAX_THUMBS);
                        for (var t = 0; t < duration; t += interval) {
                            timestamps.push(t);
                        }
                        if (timestamps.length === 0) {
                            finish(null);
                            return;
                        }

                        console.log('[TD-Plyr] Generating ' + timestamps.length + ' thumbnails for ' + duration.toFixed(1) + 's video');
                        captureNext();
                    };

                    video.onerror = function(e) {
                        console.warn('[TD-Plyr] Could not load video for thumbnails:', e);
                        finish(null);
                    };

                    function captureNext() {
                        if (done) return;
                        if (currentIndex >= timestamps.length) {
                            createSpriteAndVtt();
                            return;
                        }
                        video.currentTime = timestamps[currentIndex];
                    }

                    video.onseeked = function() {
                        if (done) return;
                        try {
                            var canvas = document.createElement('canvas');
                            canvas.width = THUMB_WIDTH;
                            canvas.height = THUMB_HEIGHT;
                            var ctx = canvas.getContext('2d');
                            ctx.drawImage(video, 0, 0, THUMB_WIDTH, THUMB_HEIGHT);
                            thumbs.push({
                                time: timestamps[currentIndex],
                                data: canvas.toDataURL('image/jpeg', 0.6)
                            });
                        } catch(e) {
                            console.warn('[TD-Plyr] Could not capture frame at ' + timestamps[currentIndex] + 's:', e.message);
                        }
                        currentIndex++;
                        captureNext();
                    };

                    function createSpriteAndVtt() {
                        if (thumbs.length === 0) {
                            finish(null);
                            return;
                        }

                        // Create sprite sheet (horizontal strip)
                        var spriteCanvas = document.createElement('canvas');
                        spriteCanvas.width = THUMB_WIDTH * thumbs.length;
                        spriteCanvas.height = THUMB_HEIGHT;
                        var spriteCtx = spriteCanvas.getContext('2d');

                        var loadedCount = 0;
                        thumbs.forEach(function(thumb, i) {
                            var img = new Image();
                            img.onload = function() {
                                spriteCtx.drawImage(img, i * THUMB_WIDTH, 0);
                                loadedCount++;
                                if (loadedCount === thumbs.length) finalize();
                            };
                            img.onerror = function() {
                                loadedCount++;
                                if (loadedCount === thumbs.length) finalize();
                            };
                            img.src = thumb.data;
                        });

                        function finalize() {
                            var spriteUrl = spriteCanvas.toDataURL('image/jpeg', 0.7);

                            // Generate VTT content
                            var vtt = 'WEBVTT\n\n';
                            for (var i = 0; i < thumbs.length; i++) {
                                var startTime = thumbs[i].time;
                                var endTime = (i + 1 < thumbs.length) ? thumbs[i + 1].time : duration;
                                vtt += formatVttTime(startTime) + ' --> ' + formatVttTime(endTime) + '\n';
                                vtt += spriteUrl + '#xywh=' + (i * THUMB_WIDTH) + ',0,' + THUMB_WIDTH + ',' + THUMB_HEIGHT + '\n\n';
                            }

                            var vttBlob = new Blob([vtt], { type: 'text/vtt' });
                            var vttUrl = URL.createObjectURL(vttBlob);

                            console.log('[TD-Plyr] Generated thumbnail sprite with ' + thumbs.length + ' frames');
                            finish(vttUrl);
                        }
                    }

                    function formatVttTime(seconds) {
                        var h = Math.floor(seconds / 3600);
                        var m = Math.floor((seconds % 3600) / 60);
                        var s = Math.floor(seconds % 60);
                        var ms = Math.floor((seconds % 1) * 1000);
                        return String(h).padStart(2, '0') + ':' +
                               String(m).padStart(2, '0') + ':' +
                               String(s).padStart(2, '0') + '.' +
                               String(ms).padStart(3, '0');
                    }

                    video.src = videoSrc;
                    video.load();
                }

                // ---- Extract poster frame from video ----
                function extractPosterFrame(videoSrc, callback) {
                    var video = document.createElement('video');
                    video.muted = true;
                    video.playsInline = true;
                    video.preload = 'metadata';
                    video.crossOrigin = 'anonymous';
                    var done = false;
                    var timeoutId = null;

                    function finish(result) {
                        if (done) return;
                        done = true;
                        if (timeoutId) clearTimeout(timeoutId);
                        video.src = '';
                        callback(result);
                    }

                    // Timeout after 5 seconds
                    timeoutId = setTimeout(function() {
                        console.warn('[TD-Plyr] Poster extraction timed out');
                        finish(null);
                    }, 5000);

                    video.onloadeddata = function() {
                        // Seek to 0.5s or 10% of duration, whichever is smaller
                        var seekTime = Math.min(0.5, video.duration * 0.1);
                        video.currentTime = seekTime;
                    };

                    video.onseeked = function() {
                        try {
                            var canvas = document.createElement('canvas');
                            canvas.width = video.videoWidth || 320;
                            canvas.height = video.videoHeight || 180;
                            var ctx = canvas.getContext('2d');
                            ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
                            var posterUrl = canvas.toDataURL('image/jpeg', 0.8);
                            console.log('[TD-Plyr] Extracted poster frame: ' + canvas.width + 'x' + canvas.height);
                            finish(posterUrl);
                        } catch(e) {
                            console.warn('[TD-Plyr] Could not extract poster frame:', e.message);
                            finish(null);
                        }
                    };

                    video.onerror = function(e) {
                        console.warn('[TD-Plyr] Could not load video for poster:', e);
                        finish(null);
                    };

                    video.src = videoSrc;
                    video.load();
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

                    // Hide the native video immediately to prevent flash
                    el.classList.add('td-plyr-pending');

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

                                // Show the video once Plyr is ready
                                el.classList.remove('td-plyr-pending');

                                console.log('[TD-Plyr] Enhanced video:', videoSrc || '(no src)', vttUrl ? '(with thumbnails)' : '');
                            } catch(err) {
                                console.error('[TD-Plyr] Error:', err);
                                // Show video anyway if Plyr fails
                                el.classList.remove('td-plyr-pending');
                            }
                        }

                        if (videoSrc) {
                            // Extract poster frame first, then init Plyr
                            extractPosterFrame(videoSrc, function(posterUrl) {
                                if (posterUrl && !el.hasAttribute('poster')) {
                                    el.setAttribute('poster', posterUrl);
                                    console.log('[TD-Plyr] Set poster attribute');
                                }

                                // Initialize Plyr after poster is set
                                initPlyr(null);

                                // Generate thumbnails in background and update Plyr
                                generateThumbnails(videoSrc, function(vttUrl) {
                                    if (vttUrl && el.plyr) {
                                        try {
                                            el.plyr.config.previewThumbnails = { enabled: true, src: vttUrl };
                                            console.log('[TD-Plyr] Added thumbnails to player');
                                        } catch(e) {
                                            console.warn('[TD-Plyr] Could not add thumbnails:', e);
                                        }
                                    }
                                });
                            });
                        } else {
                            initPlyr(null);
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
                    if (!document.querySelector('link[href*="plyr.css"]')) {
                        loadCSS(TD_BASE + 'plyr/dist/plyr.css');
                    }
                    loadScript(TD_BASE + 'plyr/dist/plyr.min.js', function() {
                        if (typeof Plyr !== 'undefined') {
                            plyrLoaded = true;
                            console.log('[TD-Plyr] Plyr loaded');
                            scanAll();
                        }
                    });
                }

                loadPdfJs();
                loadPlyr();

                console.log('[TiddlyDesktop] Inline media enhancement initialized');
            })();
        """.trimIndent()

        // Add a WebViewClient that injects the script after page load
        webView.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean {
                return false
            }

            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
                val url = request.url
                val path = url.path ?: return super.shouldInterceptRequest(view, request)

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

                return super.shouldInterceptRequest(view, request)
            }

            override fun onPageFinished(view: WebView, url: String) {
                super.onPageFinished(view, url)
                // Inject the external attachment handler
                view.evaluateJavascript(externalAttachmentScript, null)
                // Inject the saver for single-file wikis
                if (saverScript.isNotEmpty()) {
                    view.evaluateJavascript(saverScript, null)
                }
                // Inject the palette monitoring script
                view.evaluateJavascript(paletteScript, null)
                // Inject the fullscreen toggle handler
                view.evaluateJavascript(fullscreenScript, null)
                // Inject the clipboard handler
                view.evaluateJavascript(clipboardScript, null)
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
            }
        }

        // Set initial default system bar colors (light background with dark icons for visibility)
        // This ensures proper contrast before JavaScript sets the palette colors
        updateSystemBarColors("#ffffff", "#ffffff", null)

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
        // Exit fullscreen first
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
                            """.trimIndent()) { saved ->
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

        webView.destroy()
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
}
