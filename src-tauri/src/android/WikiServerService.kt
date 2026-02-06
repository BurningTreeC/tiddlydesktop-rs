package com.burningtreec.tiddlydesktop_rs

import android.app.*
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationCompat

/**
 * Foreground Service to keep the wiki servers alive when the app is in background.
 *
 * Android will kill background processes after a short time, but a foreground service
 * with a visible notification keeps the process alive.
 *
 * This service should be started when wiki servers are running and stopped when
 * all wikis are closed.
 */
class WikiServerService : Service() {

    companion object {
        private const val TAG = "WikiServerService"
        private const val NOTIFICATION_ID = 1001
        private const val CHANNEL_ID = "wiki_server_channel"
        private const val CHANNEL_NAME = "Wiki Server"
        private const val NOTIFICATION_CHECK_INTERVAL = 2000L  // Check every 2 seconds

        // SharedPreferences for cross-process wiki count
        private const val PREFS_NAME = "wiki_server_prefs"
        private const val KEY_ACTIVE_COUNT = "active_wiki_count"

        private var isRunning = false

        /**
         * Get the current active wiki count from SharedPreferences.
         * Works across processes.
         */
        private fun getActiveCount(context: Context): Int {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            return prefs.getInt(KEY_ACTIVE_COUNT, 0)
        }

        /**
         * Set the active wiki count in SharedPreferences.
         * Works across processes.
         */
        private fun setActiveCount(context: Context, count: Int) {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            prefs.edit().putInt(KEY_ACTIVE_COUNT, count).apply()
        }

        /**
         * Start the foreground service (call when a wiki server starts)
         */
        @JvmStatic
        fun startService(context: Context) {
            try {
                val count = getActiveCount(context) + 1
                setActiveCount(context, count)
                Log.d(TAG, "Wiki started, count: $count")

                if (count == 1) {
                    // First wiki - start the foreground service
                    val intent = Intent(context, WikiServerService::class.java)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        context.startForegroundService(intent)
                    } else {
                        context.startService(intent)
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to start foreground service: ${e.message}", e)
                // Don't crash - just log the error
            }
        }

        /**
         * Notify that a wiki was closed. Stops the service when no wikis are open.
         */
        @JvmStatic
        fun wikiClosed(context: Context) {
            val count = maxOf(0, getActiveCount(context) - 1)
            setActiveCount(context, count)
            Log.d(TAG, "Wiki closed, count: $count")

            if (count == 0) {
                val intent = Intent(context, WikiServerService::class.java)
                context.stopService(intent)
            }
        }

        /**
         * Force stop the service
         */
        @JvmStatic
        fun stopService(context: Context) {
            setActiveCount(context, 0)
            val intent = Intent(context, WikiServerService::class.java)
            context.stopService(intent)
        }

        /**
         * Check if service is running
         */
        @JvmStatic
        fun isServiceRunning(): Boolean = isRunning

        /**
         * Get the current active wiki count
         */
        @JvmStatic
        fun getActiveWikiCount(context: Context): Int = getActiveCount(context)
    }

    private val handler = Handler(Looper.getMainLooper())
    private val notificationChecker = object : Runnable {
        override fun run() {
            if (isRunning && getActiveCount(this@WikiServerService) > 0) {
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
        Log.d(TAG, "Service onStartCommand")

        isRunning = true

        try {
            // Create and show notification
            showNotification()
            // Start periodic check to ensure notification stays visible
            handler.removeCallbacks(notificationChecker)
            handler.postDelayed(notificationChecker, NOTIFICATION_CHECK_INTERVAL)
            Log.d(TAG, "Foreground service started successfully")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start foreground: ${e.message}", e)
            // If we can't start foreground, stop the service
            stopSelf()
        }

        return START_STICKY
    }

    private fun ensureNotificationVisible() {
        val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val activeNotifications = notificationManager.activeNotifications
        val hasOurNotification = activeNotifications.any { it.id == NOTIFICATION_ID }

        if (!hasOurNotification && getActiveCount(this) > 0) {
            Log.d(TAG, "Notification was dismissed, re-showing")
            showNotification()
        }
    }

    private fun showNotification() {
        val notification = createNotification()

        // Start as foreground service with type on Android 14+
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
        handler.removeCallbacks(notificationChecker)
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                CHANNEL_NAME,
                NotificationManager.IMPORTANCE_DEFAULT
            ).apply {
                description = "Keeps wiki servers running in the background"
                setShowBadge(false)
                setSound(null, null)  // No sound for this notification
            }

            val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }
    }

    private fun createNotification(): Notification {
        // Intent to open the main activity when notification is tapped
        val pendingIntent = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("TiddlyDesktop-RS")
            .setContentText("Wiki is running in the background")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }
}
