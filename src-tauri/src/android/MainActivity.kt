package com.burningtreec.tiddlydesktop_rs

import android.content.Intent
import android.os.Bundle
import android.util.Log

class MainActivity : TauriActivity() {

    companion object {
        private const val TAG = "MainActivity"
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        handleWidgetIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleWidgetIntent(intent)
    }

    /**
     * Handle intents from the home screen widget.
     * If the wiki is already open, bring it to foreground.
     * Otherwise, launch a new WikiActivity.
     */
    private fun handleWidgetIntent(intent: Intent) {
        val wikiPath = intent.getStringExtra("open_wiki_path")
        val wikiTitle = intent.getStringExtra("open_wiki_title")
        val isFolder = intent.getBooleanExtra("open_wiki_is_folder", false)

        if (!wikiPath.isNullOrEmpty()) {
            Log.d(TAG, "Widget intent: opening wiki path=$wikiPath, title=$wikiTitle, isFolder=$isFolder")

            // Try to bring existing wiki to foreground
            if (WikiActivity.bringWikiToFront(this, wikiPath)) {
                Log.d(TAG, "Wiki already open, brought to foreground")
            } else {
                // Wiki is not open, launch new activity
                Log.d(TAG, "Wiki not open, launching new activity")
                launchNewWikiActivity(wikiPath, wikiTitle, isFolder)
            }

            // Clear the extras so we don't reopen on rotation
            intent.removeExtra("open_wiki_path")
            intent.removeExtra("open_wiki_title")
            intent.removeExtra("open_wiki_is_folder")
        }
    }

    private fun launchNewWikiActivity(wikiPath: String, wikiTitle: String?, isFolder: Boolean) {
        val wikiIntent = Intent(this, WikiActivity::class.java).apply {
            putExtra(WikiActivity.EXTRA_WIKI_PATH, wikiPath)
            putExtra(WikiActivity.EXTRA_WIKI_TITLE, wikiTitle ?: "TiddlyWiki")
            putExtra(WikiActivity.EXTRA_IS_FOLDER, isFolder)
        }
        startActivity(wikiIntent)
    }
}
