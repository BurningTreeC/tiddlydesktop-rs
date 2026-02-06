package com.burningtreec.tiddlydesktop_rs

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.DocumentsContract
import android.util.Log
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.documentfile.provider.DocumentFile

/**
 * Activity that handles "Open with" intents for HTML files and folders.
 * This allows users to open TiddlyWiki files directly from file managers.
 *
 * Supported scenarios:
 * - Single-file wiki: .html or .htm files
 * - Folder wiki: Directories (especially those containing tiddlywiki.info)
 */
class OpenWithActivity : AppCompatActivity() {

    companion object {
        private const val TAG = "OpenWithActivity"
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        Log.d(TAG, "OpenWithActivity onCreate")
        Log.d(TAG, "Intent action: ${intent.action}")
        Log.d(TAG, "Intent data: ${intent.data}")
        Log.d(TAG, "Intent type: ${intent.type}")

        when (intent.action) {
            Intent.ACTION_VIEW -> handleViewIntent()
            Intent.ACTION_SEND -> handleSendIntent()
            else -> {
                Log.w(TAG, "Unsupported action: ${intent.action}")
                showError("Unsupported action")
                finish()
            }
        }
    }

    private fun handleViewIntent() {
        val uri = intent.data
        if (uri == null) {
            Log.e(TAG, "No URI in VIEW intent")
            showError("No file specified")
            finish()
            return
        }

        Log.d(TAG, "Processing URI: $uri")

        // Try to take persistable permission for the URI
        try {
            val takeFlags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            contentResolver.takePersistableUriPermission(uri, takeFlags)
            Log.d(TAG, "Took persistable permission for: $uri")
        } catch (e: SecurityException) {
            Log.w(TAG, "Could not take persistable permission: ${e.message}")
            // Continue anyway - we may still have temporary permission
        }

        // Determine if this is a file or directory
        val docFile = DocumentFile.fromSingleUri(this, uri)
        val isDirectory = isDirectoryUri(uri)

        Log.d(TAG, "isDirectory: $isDirectory, docFile: ${docFile?.name}, isFile: ${docFile?.isFile}")

        if (isDirectory) {
            handleDirectoryOpen(uri)
        } else {
            handleFileOpen(uri)
        }
    }

    private fun handleSendIntent() {
        // Handle ACTION_SEND (share) - get URI from extras
        val uri = if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_STREAM)
        }

        if (uri == null) {
            Log.e(TAG, "No URI in SEND intent")
            showError("No file specified")
            finish()
            return
        }

        Log.d(TAG, "Processing shared URI: $uri")
        handleFileOpen(uri)
    }

    /**
     * Check if a URI points to a directory.
     */
    private fun isDirectoryUri(uri: Uri): Boolean {
        // Check MIME type first
        val mimeType = contentResolver.getType(uri)
        Log.d(TAG, "MIME type for $uri: $mimeType")

        if (mimeType == DocumentsContract.Document.MIME_TYPE_DIR ||
            mimeType == "vnd.android.document/directory" ||
            mimeType?.contains("directory") == true) {
            return true
        }

        // Try to create a tree document
        try {
            val treeDoc = DocumentFile.fromTreeUri(this, uri)
            if (treeDoc != null && treeDoc.isDirectory) {
                return true
            }
        } catch (e: Exception) {
            // Not a tree URI
        }

        return false
    }

    /**
     * Handle opening a single HTML file as a wiki.
     */
    private fun handleFileOpen(uri: Uri) {
        Log.d(TAG, "handleFileOpen: $uri")

        // Get the display name for the wiki title
        val displayName = getDisplayName(uri) ?: "TiddlyWiki"
        val wikiTitle = displayName.removeSuffix(".html").removeSuffix(".htm")

        Log.d(TAG, "Opening single-file wiki: $wikiTitle")

        // Build the wiki path in JSON format (same as SAF picker)
        // For single-file wikis, we need the document URI
        val wikiPath = buildWikiPath(uri, null)

        Log.d(TAG, "Wiki path: $wikiPath")

        launchWikiActivity(wikiPath, wikiTitle, isFolder = false)
    }

    /**
     * Handle opening a folder as a wiki.
     */
    private fun handleDirectoryOpen(uri: Uri) {
        Log.d(TAG, "handleDirectoryOpen: $uri")

        // Get the display name for the wiki title
        val displayName = getDisplayName(uri) ?: "TiddlyWiki"

        Log.d(TAG, "Opening folder wiki: $displayName")

        // For folder wikis, we need to find tiddlywiki.info or treat as new wiki
        val treeDoc = try {
            DocumentFile.fromTreeUri(this, uri)
        } catch (e: Exception) {
            Log.e(TAG, "Cannot access directory: ${e.message}")
            showError("Cannot access directory")
            finish()
            return
        }

        if (treeDoc == null) {
            Log.e(TAG, "Could not create DocumentFile for directory")
            showError("Cannot access directory")
            finish()
            return
        }

        // Check if this looks like a TiddlyWiki folder
        val hasTiddlyWikiInfo = treeDoc.findFile("tiddlywiki.info") != null
        Log.d(TAG, "Has tiddlywiki.info: $hasTiddlyWikiInfo")

        // Build the wiki path
        val wikiPath = buildWikiPath(null, uri)

        Log.d(TAG, "Wiki path: $wikiPath")

        // For folder wikis, we need to go through MainActivity/Rust to start Node.js server
        // This is more complex - we'll show a message for now and launch MainActivity
        launchMainActivityWithFolder(uri, displayName)
    }

    /**
     * Build the wiki path JSON string expected by WikiActivity.
     */
    private fun buildWikiPath(documentUri: Uri?, treeUri: Uri?): String {
        val parts = mutableListOf<String>()

        documentUri?.let {
            val escaped = it.toString().replace("\"", "\\\"")
            parts.add("\"document_uri\":\"$escaped\"")
        }

        treeUri?.let {
            val escaped = it.toString().replace("\"", "\\\"")
            parts.add("\"tree_uri\":\"$escaped\"")
        }

        return "{${parts.joinToString(",")}}"
    }

    /**
     * Get the display name of a file/directory from its URI.
     */
    private fun getDisplayName(uri: Uri): String? {
        try {
            contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                    if (nameIndex >= 0) {
                        return cursor.getString(nameIndex)
                    }
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "Could not get display name: ${e.message}")
        }

        // Fall back to last path segment
        return uri.lastPathSegment
    }

    /**
     * Launch WikiActivity to open a single-file wiki.
     */
    private fun launchWikiActivity(wikiPath: String, wikiTitle: String, isFolder: Boolean) {
        Log.d(TAG, "Launching WikiActivity: path=$wikiPath, title=$wikiTitle, isFolder=$isFolder")

        val wikiIntent = Intent(this, WikiActivity::class.java).apply {
            putExtra(WikiActivity.EXTRA_WIKI_PATH, wikiPath)
            putExtra(WikiActivity.EXTRA_WIKI_TITLE, wikiTitle)
            putExtra(WikiActivity.EXTRA_IS_FOLDER, isFolder)
            putExtra(WikiActivity.EXTRA_BACKUPS_ENABLED, true)
            putExtra(WikiActivity.EXTRA_BACKUP_COUNT, 20)
            // Launch in new task so it appears separately in recent apps
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }

        startActivity(wikiIntent)
        finish()
    }

    /**
     * Launch MainActivity to handle folder wiki opening.
     * Folder wikis need Node.js server which is managed by the main process.
     */
    private fun launchMainActivityWithFolder(uri: Uri, title: String) {
        Log.d(TAG, "Launching MainActivity for folder wiki: $uri")

        // For now, just launch MainActivity and show a toast
        // The user will need to use the folder picker in the app
        // A full implementation would pass the folder URI to be handled by Rust

        val mainIntent = Intent(this, MainActivity::class.java).apply {
            putExtra("open_folder_uri", uri.toString())
            putExtra("open_folder_title", title)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        }

        Toast.makeText(this, "Opening folder wiki: $title", Toast.LENGTH_SHORT).show()
        startActivity(mainIntent)
        finish()
    }

    private fun showError(message: String) {
        Toast.makeText(this, message, Toast.LENGTH_LONG).show()
    }
}
