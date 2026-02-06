package com.burningtreec.tiddlydesktop_rs

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {

    companion object {
        private const val TAG = "MainActivity"
        private const val NOTIFICATION_PERMISSION_REQUEST_CODE = 1001
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        requestNotificationPermission()
        handleWidgetIntent(intent)
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
