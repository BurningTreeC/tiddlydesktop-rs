package com.burningtreec.tiddlydesktop_rs

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.view.View
import android.view.WindowInsets
import android.widget.FrameLayout
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.io.File

class MainActivity : TauriActivity() {

    companion object {
        private const val TAG = "MainActivity"
        private const val NOTIFICATION_PERMISSION_REQUEST_CODE = 1001
    }

    // Android 15+: Colored views behind transparent system bars
    private var statusBarBgView: View? = null
    private var navBarBgView: View? = null

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
        super.onCreate(savedInstanceState)

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
     * Otherwise, write a pending file so the Tauri frontend can open the wiki
     * through the proper Rust commands (which start servers, etc.).
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
                // Write pending wiki info to a file that the Tauri frontend will read.
                // We can't launch WikiActivity directly because:
                // - Folder wikis need a Node.js server URL (started by Rust)
                // - Single-file wikis need Rust to set up the entry properly
                // The frontend startup.js checks for this file and opens the wiki.
                Log.d(TAG, "Writing pending wiki open file for frontend")
                try {
                    val pendingFile = File(filesDir, "pending_widget_wiki.json")
                    val json = JSONObject().apply {
                        put("path", wikiPath)
                        put("title", wikiTitle ?: "TiddlyWiki")
                        put("is_folder", isFolder)
                    }
                    pendingFile.writeText(json.toString())
                    Log.d(TAG, "Wrote pending wiki to: ${pendingFile.absolutePath}")
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to write pending wiki file: ${e.message}")
                }
            }

            // Clear the extras so we don't reopen on rotation
            intent.removeExtra("open_wiki_path")
            intent.removeExtra("open_wiki_title")
            intent.removeExtra("open_wiki_is_folder")
        }
    }
}
