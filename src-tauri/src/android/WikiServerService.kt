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
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicInteger

/**
 * Foreground Service to keep wiki servers alive when the app is in background.
 *
 * Each open wiki gets its own notification showing the wiki title.
 * The service stops when all wikis are closed.
 */
class WikiServerService : Service() {

    companion object {
        private const val TAG = "WikiServerService"
        // Base notification ID — each wiki gets NOTIFICATION_ID_BASE + sequential index
        private const val NOTIFICATION_ID_BASE = 1001
        private const val CHANNEL_ID = "wiki_server_channel"
        private const val NOTIFICATION_CHECK_INTERVAL = 2000L

        // Track active wikis: wikiKey -> WikiNotification
        // WikiActivity and WikiServerService share the :wiki process,
        // so ConcurrentHashMap is sufficient and avoids stale SharedPreferences after process kills.
        private val activeWikis = ConcurrentHashMap<String, WikiNotification>()
        private val nextNotificationId = AtomicInteger(NOTIFICATION_ID_BASE)

        private var isRunning = false

        data class WikiNotification(
            val notificationId: Int,
            val wikiTitle: String
        )

        /**
         * Start the foreground service for a specific wiki.
         * @param wikiKey Unique identifier for the wiki (e.g. wiki path)
         * @param wikiTitle Display name for the wiki
         */
        @JvmStatic
        fun wikiOpened(context: Context, wikiKey: String, wikiTitle: String) {
            try {
                // Don't add duplicates
                if (activeWikis.containsKey(wikiKey)) {
                    Log.d(TAG, "Wiki already tracked: $wikiTitle ($wikiKey)")
                    return
                }

                val notifId = nextNotificationId.getAndIncrement()
                activeWikis[wikiKey] = WikiNotification(notifId, wikiTitle)
                Log.d(TAG, "Wiki opened: $wikiTitle ($wikiKey), notifId=$notifId, count=${activeWikis.size}")

                if (!isRunning) {
                    val intent = Intent(context, WikiServerService::class.java)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        context.startForegroundService(intent)
                    } else {
                        context.startService(intent)
                    }
                } else {
                    // Service already running — post notification for this wiki
                    val notificationManager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                    notificationManager.notify(notifId, createWikiNotification(context, wikiTitle))
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to start foreground service: ${e.message}", e)
            }
        }

        /**
         * Notify that a wiki was closed. Removes its notification and stops service when none remain.
         */
        @JvmStatic
        fun wikiClosed(context: Context, wikiKey: String) {
            val removed = activeWikis.remove(wikiKey)
            if (removed != null) {
                Log.d(TAG, "Wiki closed: ${removed.wikiTitle} ($wikiKey), remaining=${activeWikis.size}")
                // Cancel this wiki's notification
                val notificationManager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                notificationManager.cancel(removed.notificationId)
            }

            if (activeWikis.isEmpty()) {
                val intent = Intent(context, WikiServerService::class.java)
                context.stopService(intent)
            }
        }

        /**
         * Force stop the service
         */
        @JvmStatic
        fun stopService(context: Context) {
            activeWikis.clear()
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
        fun getActiveWikiCount(): Int = activeWikis.size

        /**
         * Create a notification for a specific wiki (static helper for use from companion)
         */
        private fun createWikiNotification(context: Context, wikiTitle: String): Notification {
            val pendingIntent = PendingIntent.getActivity(
                context,
                0,
                Intent(context, MainActivity::class.java),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )

            return NotificationCompat.Builder(context, CHANNEL_ID)
                .setContentTitle(wikiTitle)
                .setContentText(context.getString(R.string.notif_text))
                .setSmallIcon(R.drawable.ic_notification)
                .setContentIntent(pendingIntent)
                .setOngoing(true)
                .setPriority(NotificationCompat.PRIORITY_LOW)
                .setCategory(NotificationCompat.CATEGORY_SERVICE)
                .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
                .build()
        }
    }

    private val handler = Handler(Looper.getMainLooper())
    private val notificationChecker = object : Runnable {
        override fun run() {
            if (isRunning && activeWikis.isNotEmpty()) {
                ensureNotificationsVisible()
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
        Log.d(TAG, "Service onStartCommand, activeWikis=${activeWikis.size}")

        if (activeWikis.isEmpty()) {
            Log.d(TAG, "No active wikis, stopping service")
            stopSelf()
            return START_NOT_STICKY
        }

        isRunning = true

        try {
            // Use the first wiki's notification as the foreground notification
            val firstEntry = activeWikis.entries.firstOrNull()
            if (firstEntry != null) {
                val notification = createWikiNotification(this, firstEntry.value.wikiTitle)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    startForeground(firstEntry.value.notificationId, notification,
                        android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
                } else {
                    startForeground(firstEntry.value.notificationId, notification)
                }

                // Show notifications for any additional wikis
                val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                for (entry in activeWikis.entries) {
                    if (entry.key != firstEntry.key) {
                        notificationManager.notify(
                            entry.value.notificationId,
                            createWikiNotification(this, entry.value.wikiTitle)
                        )
                    }
                }
            }

            handler.removeCallbacks(notificationChecker)
            handler.postDelayed(notificationChecker, NOTIFICATION_CHECK_INTERVAL)
            Log.d(TAG, "Foreground service started successfully")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start foreground: ${e.message}", e)
            stopSelf()
        }

        return START_NOT_STICKY
    }

    private fun ensureNotificationsVisible() {
        val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val activeNotifications = notificationManager.activeNotifications
        val activeIds = activeNotifications.map { it.id }.toSet()

        for (entry in activeWikis.entries) {
            if (entry.value.notificationId !in activeIds) {
                Log.d(TAG, "Notification was dismissed for ${entry.value.wikiTitle}, re-showing")
                notificationManager.notify(
                    entry.value.notificationId,
                    createWikiNotification(this, entry.value.wikiTitle)
                )
            }
        }
    }

    override fun onDestroy() {
        Log.d(TAG, "Service onDestroy")
        isRunning = false
        handler.removeCallbacks(notificationChecker)
        // Cancel all wiki notifications
        val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        for (entry in activeWikis.entries) {
            notificationManager.cancel(entry.value.notificationId)
        }
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
                setSound(null, null)
            }

            val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }
    }
}
