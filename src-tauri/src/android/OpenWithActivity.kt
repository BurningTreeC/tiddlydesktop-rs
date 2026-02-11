package com.burningtreec.tiddlydesktop_rs

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.util.Log
import android.widget.Toast

/**
 * Activity that handles "Open with" intents for HTML and importable files.
 * - HTML files: Opens as a TiddlyWiki (existing flow with folder picker)
 * - JSON, CSV, TID files: Forwards to CaptureActivity for import into a wiki
 *
 * Flow for wiki files:
 * 1. Receive file URI from VIEW/SEND intent
 * 2. Try to take persistable permission; if that fails, fall back to SAF file picker
 * 3. Ask user to select the parent folder (for backups, attachments, saving)
 * 4. Launch WikiActivity with both file URI and folder tree URI
 *
 * Flow for importable files:
 * 1. Receive file URI from VIEW intent
 * 2. Forward to CaptureActivity with the URI (handles wiki selection + TW5 native import)
 */
class OpenWithActivity : Activity() {

    companion object {
        private const val TAG = "OpenWithActivity"
        private const val REQUEST_CODE_PICK_FILE = 1001
        private const val REQUEST_CODE_PICK_FOLDER = 1002
    }

    private var pendingTitle: String = "TiddlyWiki"
    private var authorizedFileUri: Uri? = null

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

        // Check if this is an importable file (JSON, CSV, TID) — forward to CaptureActivity
        if (isImportableFile(uri)) {
            forwardToCaptureActivity(uri)
            return
        }

        // For HTML files, ask user whether to open as wiki or import into a wiki
        if (isHtmlFile(uri)) {
            showHtmlChooser(uri)
            return
        }

        // Default: open as wiki
        proceedOpenAsWiki(uri)
    }

    private fun proceedOpenAsWiki(uri: Uri) {
        val displayName = getDisplayName(uri) ?: "TiddlyWiki"
        pendingTitle = displayName.removeSuffix(".html").removeSuffix(".htm")

        if (tryTakePermission(uri)) {
            Log.d(TAG, "Persistable permission acquired for file")
            onFileAuthorized(uri)
        } else {
            Log.w(TAG, "No persistable permission, falling back to SAF file picker")
            launchFilePicker()
        }
    }

    private fun handleSendIntent() {
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

        // Check if this is an importable file (JSON, CSV, TID) — forward to CaptureActivity
        if (isImportableFile(uri)) {
            forwardToCaptureActivity(uri)
            return
        }

        // For HTML files, ask user whether to open as wiki or import into a wiki
        if (isHtmlFile(uri)) {
            showHtmlChooser(uri)
            return
        }

        // Default: open as wiki
        pendingTitle = (getDisplayName(uri) ?: "TiddlyWiki").removeSuffix(".html").removeSuffix(".htm")

        if (tryTakePermission(uri)) {
            onFileAuthorized(uri)
        } else {
            Log.w(TAG, "No persistable permission for shared file, falling back to SAF file picker")
            launchFilePicker()
        }
    }

    /**
     * Called once we have a file URI with persistable permission.
     * Next step: ask for folder access.
     */
    private fun onFileAuthorized(fileUri: Uri) {
        authorizedFileUri = fileUri
        Log.d(TAG, "File authorized: $fileUri, now requesting folder access")
        Toast.makeText(this, "Now select the folder containing the wiki (for saving & backups)", Toast.LENGTH_LONG).show()
        launchFolderPicker()
    }

    /**
     * Called once we have both file and folder URIs.
     * Launches WikiActivity.
     */
    private fun onFolderAuthorized(treeUri: Uri?) {
        val fileUri = authorizedFileUri
        if (fileUri == null) {
            Log.e(TAG, "No file URI when folder was authorized")
            showError("Internal error: no file URI")
            finish()
            return
        }

        val wikiPath = buildWikiPath(fileUri, treeUri)
        Log.d(TAG, "Launching wiki: path=$wikiPath, title=$pendingTitle")
        launchWikiActivity(wikiPath, pendingTitle, isFolder = false)
    }

    /**
     * Try to take persistable read+write permission for a URI.
     */
    private fun tryTakePermission(uri: Uri): Boolean {
        val alreadyPersisted = contentResolver.persistedUriPermissions.any {
            it.uri == uri && it.isReadPermission
        }
        if (alreadyPersisted) {
            Log.d(TAG, "Already have persisted permission for: $uri")
            return true
        }

        try {
            val takeFlags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            contentResolver.takePersistableUriPermission(uri, takeFlags)
            Log.d(TAG, "Took persistable read+write permission for: $uri")
            return true
        } catch (e: SecurityException) {
            Log.w(TAG, "Could not take read+write permission: ${e.message}")
        }

        try {
            contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION)
            Log.d(TAG, "Took persistable read-only permission for: $uri")
            return true
        } catch (e: SecurityException) {
            Log.w(TAG, "Could not take read-only permission: ${e.message}")
        }

        return false
    }

    private fun launchFilePicker() {
        Toast.makeText(this, "Please select the wiki file to grant access", Toast.LENGTH_LONG).show()
        @Suppress("DEPRECATION")
        startActivityForResult(
            Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "text/html"
            },
            REQUEST_CODE_PICK_FILE
        )
    }

    private fun launchFolderPicker() {
        @Suppress("DEPRECATION")
        startActivityForResult(
            Intent(Intent.ACTION_OPEN_DOCUMENT_TREE),
            REQUEST_CODE_PICK_FOLDER
        )
    }

    @Suppress("DEPRECATION")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)

        when (requestCode) {
            REQUEST_CODE_PICK_FILE -> {
                if (resultCode != RESULT_OK || data?.data == null) {
                    Log.d(TAG, "File picker cancelled")
                    showError("File selection cancelled")
                    finish()
                    return
                }

                val uri = data.data!!
                Log.d(TAG, "File picker result: $uri")

                try {
                    val takeFlags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    contentResolver.takePersistableUriPermission(uri, takeFlags)
                } catch (e: SecurityException) {
                    Log.e(TAG, "Failed to take permission from file picker: ${e.message}")
                    showError("Could not get file access permission")
                    finish()
                    return
                }

                val displayName = getDisplayName(uri)
                if (displayName != null) {
                    pendingTitle = displayName.removeSuffix(".html").removeSuffix(".htm")
                }

                onFileAuthorized(uri)
            }

            REQUEST_CODE_PICK_FOLDER -> {
                if (resultCode != RESULT_OK || data?.data == null) {
                    // User cancelled folder picker — proceed without folder access
                    Log.d(TAG, "Folder picker cancelled, proceeding without tree URI")
                    onFolderAuthorized(null)
                    return
                }

                val treeUri = data.data!!
                Log.d(TAG, "Folder picker result: $treeUri")

                try {
                    val takeFlags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    contentResolver.takePersistableUriPermission(treeUri, takeFlags)
                    Log.d(TAG, "Took persistable permission for folder: $treeUri")
                } catch (e: SecurityException) {
                    Log.w(TAG, "Could not take folder permission: ${e.message}")
                    // Proceed without folder access
                }

                onFolderAuthorized(treeUri)
            }
        }
    }

    private fun buildWikiPath(documentUri: Uri?, treeUri: Uri?): String {
        val parts = mutableListOf<String>()

        documentUri?.let {
            val escaped = it.toString().replace("\"", "\\\"")
            parts.add("\"uri\":\"$escaped\"")
        }

        treeUri?.let {
            val escaped = it.toString().replace("\"", "\\\"")
            parts.add("\"documentTopTreeUri\":\"$escaped\"")
        }

        return "{${parts.joinToString(",")}}"
    }

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
        return uri.lastPathSegment
    }

    private fun launchWikiActivity(wikiPath: String, wikiTitle: String, isFolder: Boolean) {
        Log.d(TAG, "Launching WikiActivity: path=$wikiPath, title=$wikiTitle, isFolder=$isFolder")

        val wikiIntent = Intent(this, WikiActivity::class.java).apply {
            putExtra(WikiActivity.EXTRA_WIKI_PATH, wikiPath)
            putExtra(WikiActivity.EXTRA_WIKI_TITLE, wikiTitle)
            putExtra(WikiActivity.EXTRA_IS_FOLDER, isFolder)
            putExtra(WikiActivity.EXTRA_BACKUPS_ENABLED, true)
            putExtra(WikiActivity.EXTRA_BACKUP_COUNT, 20)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }

        startActivity(wikiIntent)
        finish()
    }

    /**
     * Check if this file should be imported into a wiki (JSON, CSV, TID)
     * rather than opened as a wiki.
     */
    private fun isImportableFile(uri: Uri): Boolean {
        val name = getDisplayName(uri)?.lowercase() ?: ""
        if (name.endsWith(".tid") || name.endsWith(".json") || name.endsWith(".csv")) return true
        val mimeType = intent.type ?: contentResolver.getType(uri) ?: ""
        return mimeType == "application/json" || mimeType == "text/csv"
    }

    /**
     * Check if this file is an HTML file.
     */
    private fun isHtmlFile(uri: Uri): Boolean {
        val name = getDisplayName(uri)?.lowercase() ?: ""
        if (name.endsWith(".html") || name.endsWith(".htm")) return true
        val mimeType = intent.type ?: contentResolver.getType(uri) ?: ""
        return mimeType == "text/html" || mimeType == "application/xhtml+xml"
    }

    /**
     * Show chooser dialog for HTML files: open as wiki or import into a wiki.
     */
    private fun showHtmlChooser(uri: Uri) {
        val displayName = getDisplayName(uri) ?: "file"
        AlertDialog.Builder(this)
            .setTitle(displayName)
            .setItems(arrayOf(getString(R.string.open_as_wiki), getString(R.string.import_into_wiki))) { _, which ->
                when (which) {
                    0 -> proceedOpenAsWiki(uri)
                    1 -> forwardToCaptureActivity(uri)
                }
            }
            .setOnCancelListener { finish() }
            .show()
    }

    /**
     * Forward file to CaptureActivity for import into a wiki via TW5 native import.
     */
    private fun forwardToCaptureActivity(uri: Uri) {
        Log.d(TAG, "Forwarding importable file to CaptureActivity: $uri")
        val captureIntent = Intent(this, CaptureActivity::class.java).apply {
            action = Intent.ACTION_SEND
            putExtra(Intent.EXTRA_STREAM, uri)
            type = contentResolver.getType(uri) ?: intent.type ?: "*/*"
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        startActivity(captureIntent)
        finish()
    }

    private fun showError(message: String) {
        Toast.makeText(this, message, Toast.LENGTH_LONG).show()
    }
}
