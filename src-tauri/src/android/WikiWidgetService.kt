package com.burningtreec.tiddlydesktop_rs

import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.os.FileObserver
import android.util.Log
import android.widget.RemoteViews
import android.widget.RemoteViewsService
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * RemoteViewsService that provides the WikiWidgetFactory for the widget's ListView.
 */
class WikiWidgetService : RemoteViewsService() {

    override fun onGetViewFactory(intent: Intent): RemoteViewsFactory {
        return WikiWidgetFactory(applicationContext)
    }
}

/**
 * RemoteViewsFactory that creates RemoteViews for each wiki item in the widget.
 * Reads recent_wikis.json to populate the list.
 * Uses a FileObserver to watch for changes and auto-refresh the widget.
 */
class WikiWidgetFactory(private val context: Context) : RemoteViewsService.RemoteViewsFactory {

    companion object {
        private const val TAG = "WikiWidgetFactory"
        private const val MAX_WIKIS = 10  // Maximum number of wikis to show in widget
    }

    // Data class to hold wiki information
    data class WikiInfo(
        val path: String,
        val title: String,
        val isFolder: Boolean,
        val lastOpened: Long,
        val faviconPath: String? = null
    )

    private var wikis: List<WikiInfo> = emptyList()
    private var fileObserver: FileObserver? = null

    override fun onCreate() {
        Log.d(TAG, "onCreate")
        loadWikiData()
        startFileObserver()
    }

    override fun onDataSetChanged() {
        Log.d(TAG, "onDataSetChanged")
        loadWikiData()
    }

    override fun onDestroy() {
        Log.d(TAG, "onDestroy")
        stopFileObserver()
        wikis = emptyList()
    }

    override fun getCount(): Int = wikis.size

    override fun getViewAt(position: Int): RemoteViews {
        if (position >= wikis.size) {
            return RemoteViews(context.packageName, R.layout.widget_item)
        }

        val wiki = wikis[position]
        val views = RemoteViews(context.packageName, R.layout.widget_item)

        // Set the wiki title
        views.setTextViewText(R.id.wiki_name, wiki.title)

        // Load and set favicon if available
        if (!wiki.faviconPath.isNullOrEmpty()) {
            try {
                val faviconFile = File(wiki.faviconPath)
                if (faviconFile.exists()) {
                    val bitmap = BitmapFactory.decodeFile(wiki.faviconPath)
                    if (bitmap != null) {
                        // Scale bitmap to appropriate size for widget (32dp)
                        val scaledBitmap = Bitmap.createScaledBitmap(bitmap, 64, 64, true)
                        views.setImageViewBitmap(R.id.wiki_icon, scaledBitmap)
                        Log.d(TAG, "Set favicon for ${wiki.title}")
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to load favicon for ${wiki.title}: ${e.message}")
                // Fall back to default icon (already set in layout)
            }
        }

        // Set up the fill-in intent for this item
        val fillInIntent = Intent().apply {
            putExtra(RecentWikisWidgetProvider.EXTRA_WIKI_PATH, wiki.path)
            putExtra(RecentWikisWidgetProvider.EXTRA_WIKI_TITLE, wiki.title)
            putExtra(RecentWikisWidgetProvider.EXTRA_IS_FOLDER, wiki.isFolder)
        }
        views.setOnClickFillInIntent(R.id.widget_item, fillInIntent)

        return views
    }

    override fun getLoadingView(): RemoteViews? = null

    override fun getViewTypeCount(): Int = 1

    override fun getItemId(position: Int): Long = position.toLong()

    override fun hasStableIds(): Boolean = false

    /**
     * Start a FileObserver to watch for recent_wikis.json modifications.
     * Watches the directory (not the file) so it works even if the file
     * doesn't exist yet or gets recreated with a new inode.
     * When the Rust side updates the file, this triggers a widget refresh.
     */
    private fun startFileObserver() {
        try {
            val dataDir = context.filesDir
            fileObserver = object : FileObserver(dataDir, CLOSE_WRITE or MOVED_TO or CREATE) {
                override fun onEvent(event: Int, path: String?) {
                    if (path == "recent_wikis.json") {
                        Log.d(TAG, "FileObserver: recent_wikis.json changed (event=$event), requesting widget update")
                        try {
                            RecentWikisWidgetProvider.requestUpdate(context)
                        } catch (e: Exception) {
                            Log.w(TAG, "FileObserver: failed to request widget update: ${e.message}")
                        }
                    }
                }
            }
            fileObserver?.startWatching()
            Log.d(TAG, "FileObserver started for directory: ${dataDir.absolutePath}")
        } catch (e: Exception) {
            Log.w(TAG, "Failed to start FileObserver: ${e.message}")
        }
    }

    /**
     * Stop the FileObserver.
     */
    private fun stopFileObserver() {
        try {
            fileObserver?.stopWatching()
            fileObserver = null
            Log.d(TAG, "FileObserver stopped")
        } catch (e: Exception) {
            Log.w(TAG, "Failed to stop FileObserver: ${e.message}")
        }
    }

    /**
     * Load wiki data from recent_wikis.json file.
     */
    private fun loadWikiData() {
        try {
            // Look for recent_wikis.json in the app's data directory
            val dataDir = context.filesDir
            val recentWikisFile = File(dataDir, "recent_wikis.json")

            Log.d(TAG, "Looking for recent_wikis.json at: ${recentWikisFile.absolutePath}")

            if (!recentWikisFile.exists()) {
                Log.d(TAG, "recent_wikis.json not found")
                wikis = emptyList()
                return
            }

            val jsonContent = recentWikisFile.readText()
            Log.d(TAG, "Read recent_wikis.json: ${jsonContent.take(200)}...")

            val jsonArray = JSONArray(jsonContent)
            val loadedWikis = mutableListOf<WikiInfo>()

            for (i in 0 until minOf(jsonArray.length(), MAX_WIKIS)) {
                try {
                    val wikiObj = jsonArray.getJSONObject(i)
                    val path = wikiObj.optString("path", "")
                    val title = wikiObj.optString("title", wikiObj.optString("name", "Unknown Wiki"))
                    val isFolder = wikiObj.optBoolean("is_folder", false)
                    val lastOpened = wikiObj.optLong("last_opened", 0)
                    val faviconPath: String? = if (wikiObj.has("favicon_path")) wikiObj.getString("favicon_path") else null

                    if (path.isNotEmpty()) {
                        loadedWikis.add(WikiInfo(path, title, isFolder, lastOpened, faviconPath))
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "Error parsing wiki entry at index $i: ${e.message}")
                }
            }

            // Sort by last opened (most recent first)
            wikis = loadedWikis.sortedByDescending { it.lastOpened }
            Log.d(TAG, "Loaded ${wikis.size} wikis")

        } catch (e: Exception) {
            Log.e(TAG, "Error loading wiki data: ${e.message}")
            wikis = emptyList()
        }
    }
}
