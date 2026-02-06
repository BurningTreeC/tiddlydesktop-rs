package com.burningtreec.tiddlydesktop_rs

import android.app.PendingIntent
import android.appwidget.AppWidgetManager
import android.appwidget.AppWidgetProvider
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.util.Log
import android.widget.RemoteViews

/**
 * AppWidgetProvider for the Recent Wikis home screen widget.
 * Displays a list of recently opened wikis for quick access.
 */
class RecentWikisWidgetProvider : AppWidgetProvider() {

    companion object {
        private const val TAG = "RecentWikisWidget"
        const val ACTION_OPEN_WIKI = "com.burningtreec.tiddlydesktop_rs.ACTION_OPEN_WIKI"
        const val EXTRA_WIKI_PATH = "wiki_path"
        const val EXTRA_WIKI_TITLE = "wiki_title"
        const val EXTRA_IS_FOLDER = "is_folder"

        /**
         * Request an update of all widget instances.
         * Call this when recent_wikis.json changes.
         */
        fun requestUpdate(context: Context) {
            val intent = Intent(context, RecentWikisWidgetProvider::class.java).apply {
                action = AppWidgetManager.ACTION_APPWIDGET_UPDATE
            }
            val appWidgetManager = AppWidgetManager.getInstance(context)
            val componentName = ComponentName(context, RecentWikisWidgetProvider::class.java)
            val appWidgetIds = appWidgetManager.getAppWidgetIds(componentName)
            intent.putExtra(AppWidgetManager.EXTRA_APPWIDGET_IDS, appWidgetIds)
            context.sendBroadcast(intent)
        }
    }

    override fun onUpdate(
        context: Context,
        appWidgetManager: AppWidgetManager,
        appWidgetIds: IntArray
    ) {
        Log.d(TAG, "onUpdate: updating ${appWidgetIds.size} widgets")

        for (appWidgetId in appWidgetIds) {
            updateAppWidget(context, appWidgetManager, appWidgetId)
        }
    }

    override fun onReceive(context: Context, intent: Intent) {
        super.onReceive(context, intent)

        if (intent.action == ACTION_OPEN_WIKI) {
            val wikiPath = intent.getStringExtra(EXTRA_WIKI_PATH)
            val wikiTitle = intent.getStringExtra(EXTRA_WIKI_TITLE)
            val isFolder = intent.getBooleanExtra(EXTRA_IS_FOLDER, false)

            Log.d(TAG, "onReceive: opening wiki path=$wikiPath, title=$wikiTitle, isFolder=$isFolder")

            if (!wikiPath.isNullOrEmpty()) {
                // Launch MainActivity with the wiki path to open
                val openIntent = Intent(context, MainActivity::class.java).apply {
                    flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
                    putExtra("open_wiki_path", wikiPath)
                    putExtra("open_wiki_title", wikiTitle)
                    putExtra("open_wiki_is_folder", isFolder)
                }
                context.startActivity(openIntent)
            }
        }
    }

    private fun updateAppWidget(
        context: Context,
        appWidgetManager: AppWidgetManager,
        appWidgetId: Int
    ) {
        Log.d(TAG, "updateAppWidget: id=$appWidgetId")

        // Create RemoteViews
        val views = RemoteViews(context.packageName, R.layout.widget_layout)

        // Set up the intent that starts the WikiWidgetService
        val serviceIntent = Intent(context, WikiWidgetService::class.java).apply {
            putExtra(AppWidgetManager.EXTRA_APPWIDGET_ID, appWidgetId)
            // Use a unique data URI to ensure the intent is not reused
            data = Uri.parse(toUri(Intent.URI_INTENT_SCHEME))
        }

        // Set up the RemoteViews adapter
        views.setRemoteAdapter(R.id.wiki_list, serviceIntent)

        // Set empty view (shown when list is empty)
        views.setEmptyView(R.id.wiki_list, R.id.empty_view)

        // Create a pending intent template for list item clicks
        val clickIntent = Intent(context, RecentWikisWidgetProvider::class.java).apply {
            action = ACTION_OPEN_WIKI
        }
        val clickPendingIntent = PendingIntent.getBroadcast(
            context,
            0,
            clickIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
        )
        views.setPendingIntentTemplate(R.id.wiki_list, clickPendingIntent)

        // Also make the widget title clickable to open the app
        val launchIntent = Intent(context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK
        }
        val launchPendingIntent = PendingIntent.getActivity(
            context,
            0,
            launchIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        views.setOnClickPendingIntent(R.id.widget_title, launchPendingIntent)

        // Update the widget
        appWidgetManager.updateAppWidget(appWidgetId, views)

        // Notify that data has changed (triggers RemoteViewsFactory.onDataSetChanged)
        appWidgetManager.notifyAppWidgetViewDataChanged(appWidgetId, R.id.wiki_list)
    }

    override fun onEnabled(context: Context) {
        Log.d(TAG, "onEnabled: first widget added")
    }

    override fun onDisabled(context: Context) {
        Log.d(TAG, "onDisabled: last widget removed")
    }
}
