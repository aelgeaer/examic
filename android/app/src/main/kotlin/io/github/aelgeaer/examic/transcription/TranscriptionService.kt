package io.github.aelgeaer.examic.transcription

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.core.TranscriptStatus
import io.github.aelgeaer.examic.examicApp
import io.github.aelgeaer.examic.ui.MainActivity
import io.github.aelgeaer.examic.withEngine
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.onStart
import kotlinx.coroutines.launch

/**
 * Keeps the app alive while the Rust core transcribes or translates, so that
 * leaving the app after a long lecture does not lose the work.
 *
 * The work itself runs on the engine's own threads; this service only holds a
 * foreground notification with the progress, and stops once no recording is
 * waiting for or undergoing transcription.
 */
class TranscriptionService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var watcher: Job? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notification_channel_transcription),
            NotificationManager.IMPORTANCE_LOW,
        )
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        try {
            ServiceCompat.startForeground(
                this, NOTIFICATION_ID, notification(null), ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } catch (e: RuntimeException) {
            // Not allowed from the background; the work goes on regardless,
            // only without the process being protected.
            Log.w("examic", "transcription service not started", e)
            stopSelf()
            return START_NOT_STICKY
        }
        if (watcher == null) watcher = scope.launch { watch() }
        return START_NOT_STICKY
    }

    private suspend fun watch() {
        examicApp.transcriptEvents
            .onStart { emit(io.github.aelgeaer.examic.TranscriptEvent("", TranscriptStatus.NONE, null)) }
            .collect { event ->
                val busy = runCatching {
                    withEngine { engine ->
                        engine.recordings().filter {
                            it.transcriptStatus == TranscriptStatus.PENDING ||
                                it.transcriptStatus == TranscriptStatus.RUNNING
                        }
                    }
                }.getOrDefault(emptyList())
                if (busy.isEmpty()) {
                    stopForeground(STOP_FOREGROUND_REMOVE)
                    stopSelf()
                } else {
                    val progress = event.progress?.takeIf { event.status == TranscriptStatus.RUNNING }
                    getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(progress))
                }
            }
    }

    override fun onTimeout(startId: Int, fgsType: Int) {
        // Android 15 caps data-sync services at six hours a day.
        stopSelf()
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    private fun notification(progress: Float?): Notification {
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_transcript)
            .setContentTitle(getString(R.string.notification_transcribing))
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setProgress(100, ((progress ?: 0f) * 100).toInt(), progress == null)
            .build()
    }

    companion object {
        private const val CHANNEL_ID = "transcription"
        private const val NOTIFICATION_ID = 2

        /** Starts the service; safe to call while it runs. */
        fun start(context: Context) {
            try {
                context.startForegroundService(Intent(context, TranscriptionService::class.java))
            } catch (e: RuntimeException) {
                Log.w("examic", "transcription service not started", e)
            }
        }
    }
}
