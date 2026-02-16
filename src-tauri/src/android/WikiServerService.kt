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
import java.util.concurrent.atomic.AtomicInteger

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
        // Channel name moved to getString(R.string.notif_channel_name) at channel creation
        private const val NOTIFICATION_CHECK_INTERVAL = 2000L  // Check every 2 seconds

        // In-memory count — WikiActivity and WikiServerService share the :wiki process,
        // so AtomicInteger is sufficient and avoids stale SharedPreferences after process kills.
        private val activeCount = AtomicInteger(0)

        private var isRunning = false

        /**
         * Start the foreground service (call when a wiki server starts)
         */
        @JvmStatic
        fun startService(context: Context) {
            try {
                val count = activeCount.incrementAndGet()
                Log.d(TAG, "Wiki started, count: $count")

                if (!isRunning) {
                    val intent = Intent(context, WikiServerService::class.java)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        context.startForegroundService(intent)
                    } else {
                        context.startService(intent)
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to start foreground service: ${e.message}", e)
            }
        }

        /**
         * Notify that a wiki was closed. Stops the service when no wikis are open.
         */
        @JvmStatic
        fun wikiClosed(context: Context) {
            val count = activeCount.decrementAndGet().coerceAtLeast(0)
            if (count == 0) activeCount.set(0)  // clamp
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
            activeCount.set(0)
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
        fun getActiveWikiCount(): Int = activeCount.get()
    }

    private val handler = Handler(Looper.getMainLooper())
    private val notificationChecker = object : Runnable {
        override fun run() {
            if (isRunning && activeCount.get() > 0) {
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
        Log.d(TAG, "Service onStartCommand, activeCount=${activeCount.get()}")

        // If no wikis are active (e.g. spurious restart), stop immediately
        if (activeCount.get() <= 0) {
            Log.d(TAG, "No active wikis, stopping service")
            stopSelf()
            return START_NOT_STICKY
        }

        isRunning = true

        try {
            showNotification()
            handler.removeCallbacks(notificationChecker)
            handler.postDelayed(notificationChecker, NOTIFICATION_CHECK_INTERVAL)
            Log.d(TAG, "Foreground service started successfully")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start foreground: ${e.message}", e)
            stopSelf()
        }

        return START_NOT_STICKY
    }

    private fun ensureNotificationVisible() {
        val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val activeNotifications = notificationManager.activeNotifications
        val hasOurNotification = activeNotifications.any { it.id == NOTIFICATION_ID }

        if (!hasOurNotification && activeCount.get() > 0) {
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
                getString(R.string.notif_channel_name),
                NotificationManager.IMPORTANCE_DEFAULT
            ).apply {
                description = getString(R.string.notif_channel_desc)
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
            .setContentTitle(getString(R.string.notif_title))
            .setContentText(getString(R.string.notif_text))
            .setSmallIcon(R.drawable.ic_notification)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }
}
