package com.burningtreec.tiddlydesktop_rs

import android.app.*
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat

/**
 * Foreground Service to keep the main process alive while LAN sync is running.
 *
 * LAN sync (WebSocket server, UDP discovery, encrypted peer connections) runs in
 * the main Tauri process. Without a foreground service, Android will kill the
 * main process when the landing page activity goes to the background.
 *
 * This service runs in the DEFAULT process (main), unlike WikiServerService which
 * runs in the :wiki process.
 *
 * Activity tracking:
 * - mainActivityAlive: set directly by MainActivity (same process)
 * - wikiCount: updated via intent actions from WikiActivity (:wiki process)
 * - Service stops when both mainActivityAlive=false AND wikiCount=0
 */
class LanSyncService : Service() {

    companion object {
        private const val TAG = "LanSyncService"
        private const val NOTIFICATION_ID = 1002
        private const val CHANNEL_ID = "lan_sync_channel"
        private const val NOTIFICATION_CHECK_INTERVAL = 2000L
        private const val WAKELOCK_TAG = "TiddlyDesktopRS::LanSync"

        private const val PREFS_NAME = "lan_sync_state"
        private const val PREF_KEY_ACTIVE = "active"

        const val ACTION_WIKI_OPENED = "com.burningtreec.tiddlydesktop_rs.WIKI_OPENED"
        const val ACTION_WIKI_CLOSED = "com.burningtreec.tiddlydesktop_rs.WIKI_CLOSED"

        @Volatile
        private var isRunning = false

        /**
         * Cross-process check: was LAN sync active before the main process died?
         * Uses SharedPreferences (file-based) so the :wiki process can read it.
         */
        @JvmStatic
        fun isLanSyncActive(context: Context): Boolean {
            return try {
                context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                    .getBoolean(PREF_KEY_ACTIVE, false)
            } catch (e: Exception) {
                false
            }
        }

        private fun setLanSyncActive(context: Context, active: Boolean) {
            try {
                context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                    .edit()
                    .putBoolean(PREF_KEY_ACTIVE, active)
                    .apply()
            } catch (e: Exception) {
                Log.e(TAG, "Failed to write LAN sync state: ${e.message}")
            }
        }

        // Activity tracking (only meaningful in the main process where the service runs)
        @Volatile
        private var mainActivityAlive = false
        private var wikiCount = 0

        /**
         * Start the LAN sync foreground service.
         */
        @JvmStatic
        fun startService(context: Context) {
            try {
                if (!isRunning) {
                    val intent = Intent(context, LanSyncService::class.java)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        context.startForegroundService(intent)
                    } else {
                        context.startService(intent)
                    }
                    Log.d(TAG, "LAN sync service start requested")
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to start LAN sync service: ${e.message}", e)
            }
        }

        /**
         * Stop the LAN sync foreground service.
         */
        @JvmStatic
        fun stopService(context: Context) {
            try {
                wikiCount = 0
                mainActivityAlive = false
                setLanSyncActive(context, false)
                val intent = Intent(context, LanSyncService::class.java)
                context.stopService(intent)
                Log.d(TAG, "LAN sync service stop requested")
            } catch (e: Exception) {
                Log.e(TAG, "Failed to stop LAN sync service: ${e.message}", e)
            }
        }

        /**
         * Check if the service is running.
         */
        @JvmStatic
        fun isServiceRunning(): Boolean = isRunning

        /**
         * Called by MainActivity (same process) to mark itself as alive or finished.
         * When alive=false and no wikis are open, stops the service.
         */
        @JvmStatic
        fun setMainActivityAlive(alive: Boolean, context: Context? = null) {
            mainActivityAlive = alive
            Log.d(TAG, "mainActivityAlive=$alive, wikiCount=$wikiCount")
            if (!alive && isRunning && context != null) {
                // Always stop — LAN sync runs in the main Tauri process and
                // can't survive without MainActivity's runtime
                Log.d(TAG, "MainActivity closed — stopping LAN sync service")
                stopService(context)
            }
        }

        /**
         * Called by WikiActivity (:wiki process) — sends intent to service in main process.
         */
        @JvmStatic
        fun notifyWikiOpened(context: Context) {
            try {
                val intent = Intent(context, LanSyncService::class.java)
                intent.action = ACTION_WIKI_OPENED
                context.startService(intent)
            } catch (e: Exception) {
                Log.e(TAG, "Failed to notify wiki opened: ${e.message}")
            }
        }

        /**
         * Called by WikiActivity (:wiki process) — sends intent to service in main process.
         */
        @JvmStatic
        fun notifyWikiClosed(context: Context) {
            try {
                val intent = Intent(context, LanSyncService::class.java)
                intent.action = ACTION_WIKI_CLOSED
                context.startService(intent)
            } catch (e: Exception) {
                Log.e(TAG, "Failed to notify wiki closed: ${e.message}")
            }
        }
    }

    private val handler = Handler(Looper.getMainLooper())
    private var wakeLock: PowerManager.WakeLock? = null

    private val notificationChecker = object : Runnable {
        override fun run() {
            if (isRunning) {
                ensureNotificationVisible()
                handler.postDelayed(this, NOTIFICATION_CHECK_INTERVAL)
            }
        }
    }

    override fun onCreate() {
        super.onCreate()
        Log.d(TAG, "Service onCreate")
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val action = intent?.action

        when (action) {
            ACTION_WIKI_OPENED -> {
                wikiCount++
                Log.d(TAG, "Wiki opened, wikiCount=$wikiCount")
                return START_NOT_STICKY
            }
            ACTION_WIKI_CLOSED -> {
                wikiCount = (wikiCount - 1).coerceAtLeast(0)
                Log.d(TAG, "Wiki closed, wikiCount=$wikiCount, mainAlive=$mainActivityAlive")
                if (!mainActivityAlive && wikiCount <= 0) {
                    Log.d(TAG, "All activities closed — stopping service")
                    stopSelf()
                }
                return START_NOT_STICKY
            }
            else -> {
                // Normal start
                Log.d(TAG, "Service onStartCommand (normal start)")
                isRunning = true
                setLanSyncActive(this, true)

                try {
                    showNotification()
                    acquireWakeLock()
                    handler.removeCallbacks(notificationChecker)
                    handler.postDelayed(notificationChecker, NOTIFICATION_CHECK_INTERVAL)
                    Log.d(TAG, "LAN sync foreground service started")
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to start foreground: ${e.message}", e)
                    stopSelf()
                }

                return START_NOT_STICKY
            }
        }
    }

    /**
     * Called when ANY of the app's tasks is removed from the recents screen.
     * Each WikiActivity has its own task (documentLaunchMode="always"), so this
     * fires for wiki task removals too — not just the main task.
     * Only treat it as "main activity gone" if the removed task IS the main task.
     */
    override fun onTaskRemoved(rootIntent: Intent?) {
        super.onTaskRemoved(rootIntent)
        val isMainTask = rootIntent?.component?.className?.contains("MainActivity") == true
        Log.d(TAG, "Task removed: isMainTask=$isMainTask, rootIntent=${rootIntent?.component}")
        if (isMainTask) {
            mainActivityAlive = false
            // Always stop — LAN sync runs in the main Tauri process and
            // can't survive without MainActivity's runtime
            Log.d(TAG, "Main task removed — stopping LAN sync service")
            stopSelf()
        }
        // Wiki task removed — don't touch mainActivityAlive, wiki count is
        // tracked via ACTION_WIKI_CLOSED intents from WikiActivity.onDestroy()
    }

    private fun acquireWakeLock() {
        if (wakeLock == null) {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKELOCK_TAG).apply {
                // No timeout — held until service is stopped
                acquire()
            }
            Log.d(TAG, "WakeLock acquired")
        }
    }

    private fun releaseWakeLock() {
        wakeLock?.let {
            if (it.isHeld) {
                it.release()
                Log.d(TAG, "WakeLock released")
            }
        }
        wakeLock = null
    }

    private fun ensureNotificationVisible() {
        val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val activeNotifications = notificationManager.activeNotifications
        val hasOurNotification = activeNotifications.any { it.id == NOTIFICATION_ID }

        if (!hasOurNotification && isRunning) {
            Log.d(TAG, "Notification was dismissed, re-showing")
            showNotification()
        }
    }

    private fun showNotification() {
        val notification = createNotification()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIFICATION_ID, notification,
                android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    override fun onDestroy() {
        Log.d(TAG, "Service onDestroy")
        isRunning = false
        wikiCount = 0
        setLanSyncActive(this, false)
        releaseWakeLock()
        handler.removeCallbacks(notificationChecker)
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                getString(R.string.sync_notif_channel_name),
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = getString(R.string.sync_notif_channel_desc)
                setShowBadge(false)
                setSound(null, null)
            }

            val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }
    }

    private fun createNotification(): Notification {
        val pendingIntent = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.sync_notif_title))
            .setContentText(getString(R.string.sync_notif_text))
            .setSmallIcon(R.drawable.ic_sync)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }
}
