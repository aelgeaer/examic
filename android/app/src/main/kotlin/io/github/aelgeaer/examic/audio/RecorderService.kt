package io.github.aelgeaer.examic.audio

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.IBinder
import android.os.PowerManager
import android.os.Process
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.core.TranscribeMode
import io.github.aelgeaer.examic.transcription.TranscriptionService
import io.github.aelgeaer.examic.ui.MainActivity
import io.github.aelgeaer.examic.withEngine
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Owns the microphone while a recording is in progress.
 *
 * A foreground service of type `microphone` is what lets capture continue
 * with the screen off or the app in the background. The service does three
 * things: read PCM from [AudioRecord] on a dedicated thread, hand it to the
 * Rust core, and keep [Recorder.status] and the notification truthful.
 */
class RecorderService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var capture: CaptureThread? = null
    private var wakeLock: PowerManager.WakeLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notification_channel),
            NotificationManager.IMPORTANCE_LOW,
        )
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> start(
                title = intent.getStringExtra(EXTRA_TITLE).orEmpty(),
                mode = intent.getStringExtra(EXTRA_MODE)
                    ?.let { name -> TranscribeMode.entries.firstOrNull { it.name == name } }
                    ?: TranscribeMode.OFF,
                translate = intent.getBooleanExtra(EXTRA_TRANSLATE, false),
            )
            ACTION_STOP -> stop(failure = null)
            ACTION_PAUSE -> setPaused(true)
            ACTION_RESUME -> setPaused(false)
        }
        return START_NOT_STICKY
    }

    private fun start(title: String, mode: TranscribeMode, translate: Boolean) {
        if (capture != null || Recorder.status.value != RecorderStatus.Starting) return
        try {
            // Must happen within seconds of startForegroundService().
            ServiceCompat.startForeground(
                this,
                NOTIFICATION_ID,
                notification(paused = false),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
            )
        } catch (e: RuntimeException) {
            // Missing permission, or started while the app was not visible.
            fail(e)
            return
        }
        scope.launch {
            try {
                val thread = withEngine { engine ->
                    val (record, sampleRate) = openMicrophone()
                    try {
                        val recording = engine.startRecording(title, sampleRate.toUInt(), mode, translate)
                        CaptureThread(record, sampleRate, recording.id, mode)
                    } catch (e: Exception) {
                        record.release()
                        throw e
                    }
                }
                capture = thread
                wakeLock = getSystemService(PowerManager::class.java)
                    .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "examic:recording")
                    .apply { acquire(MAX_RECORDING_MS) }
                thread.start()
                Recorder.mutableStatus.value = RecorderStatus.Recording(thread.recordingId, paused = false)
            } catch (e: Exception) {
                fail(e)
            }
        }
    }

    /**
     * Opens the microphone at 48 kHz, the rate phone hardware runs at
     * natively, falling back to 16 kHz.
     *
     * `VOICE_RECOGNITION` asks for the unprocessed signal: no automatic gain
     * and no noise suppression, both of which hurt speech recognition more
     * than the noise they remove.
     */
    private fun openMicrophone(): Pair<AudioRecord, Int> {
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            throw SecurityException("microphone permission not granted")
        }
        for (sampleRate in intArrayOf(48_000, 16_000)) {
            val minBytes = AudioRecord.getMinBufferSize(
                sampleRate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
            )
            if (minBytes <= 0) continue
            // At least 200 ms, so a late read does not overrun the device buffer.
            val bufferBytes = maxOf(minBytes * 2, sampleRate / 5 * BYTES_PER_SAMPLE)
            val record = AudioRecord(
                MediaRecorder.AudioSource.VOICE_RECOGNITION,
                sampleRate,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
                bufferBytes,
            )
            if (record.state == AudioRecord.STATE_INITIALIZED) return record to sampleRate
            record.release()
        }
        throw IllegalStateException("the microphone is not available")
    }

    private fun setPaused(paused: Boolean) {
        val thread = capture ?: return
        thread.paused = paused
        Recorder.mutableStatus.value = RecorderStatus.Recording(thread.recordingId, paused)
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(paused))
    }

    /** Ends the recording. [failure] is set when capture broke on its own. */
    private fun stop(failure: String?) {
        val thread = capture ?: return
        capture = null
        Recorder.mutableStatus.value = RecorderStatus.Stopping
        scope.launch {
            val result = runCatching {
                withEngine { engine ->
                    // The engine is told to finish only after the last sample
                    // has been delivered.
                    thread.running = false
                    thread.join()
                    engine.stopRecording()
                }
            }
            failure?.let { Recorder.mutableEvents.tryEmit(RecorderEvent.Failed(it)) }
            // Transcription goes on after the recording: hand over to a
            // service that keeps the process alive for it. Started from here,
            // while this service is still in the foreground, the system allows
            // it even with the screen off.
            if (thread.mode != TranscribeMode.OFF) TranscriptionService.start(this@RecorderService)
            result
                .onSuccess { Recorder.mutableEvents.tryEmit(RecorderEvent.Finished(it)) }
                .onFailure { if (failure == null) Recorder.mutableEvents.tryEmit(RecorderEvent.Failed(it.describe())) }
            shutDown()
        }
    }

    private fun fail(error: Throwable) {
        Recorder.mutableEvents.tryEmit(RecorderEvent.Failed(error.describe()))
        shutDown()
    }

    private fun shutDown() {
        wakeLock?.takeIf { it.isHeld }?.release()
        wakeLock = null
        Recorder.mutableStatus.value = RecorderStatus.Idle
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onDestroy() {
        // Reached with a live capture only if the system tears the service
        // down. The audio thread is stopped; the file is left unsealed and is
        // recovered the next time the engine opens.
        capture?.running = false
        wakeLock?.takeIf { it.isHeld }?.release()
        scope.cancel()
        super.onDestroy()
    }

    private fun notification(paused: Boolean): Notification {
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this, 1,
            Intent(this, RecorderService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentTitle(getString(if (paused) R.string.notification_paused else R.string.notification_recording))
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .addAction(R.drawable.ic_stop, getString(R.string.stop), stop)
        if (paused) {
            builder.setShowWhen(false)
        } else {
            // A running clock that shows the length of the recording so far.
            builder.setUsesChronometer(true)
                .setWhen(System.currentTimeMillis() - NativeAudio.snapshot().positionMs)
        }
        return builder.build()
    }

    /** Reads the microphone in 20 ms blocks and pushes them to the Rust core. */
    private inner class CaptureThread(
        private val record: AudioRecord,
        sampleRate: Int,
        val recordingId: String,
        val mode: TranscribeMode,
    ) : Thread("examic-mic") {
        @Volatile var running = true
        @Volatile var paused = false
        private val blockBytes = sampleRate / 50 * BYTES_PER_SAMPLE

        override fun run() {
            Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
            // Allocated once; the loop below creates no garbage.
            val buffer = ByteBuffer.allocateDirect(blockBytes).order(ByteOrder.nativeOrder())
            var failure: String? = null
            try {
                while (running) {
                    val capturing = record.recordingState == AudioRecord.RECORDSTATE_RECORDING
                    if (paused) {
                        if (capturing) record.stop()
                        sleep(PAUSE_POLL_MS)
                        continue
                    }
                    if (!capturing) record.startRecording()

                    val bytes = record.read(buffer, blockBytes)
                    if (bytes < 0) {
                        failure = "microphone error $bytes"
                        break
                    }
                    if (bytes > 0 && NativeAudio.pushPcm(buffer, bytes / BYTES_PER_SAMPLE) == NativeAudio.ERR_CAPTURE_FAILED) {
                        failure = "the audio could not be written to storage"
                        break
                    }
                }
            } catch (e: RuntimeException) {
                failure = e.describe()
            } finally {
                runCatching { record.stop() }
                record.release()
            }
            failure?.let { reason -> scope.launch { stop(reason) } }
        }
    }

    companion object {
        const val ACTION_START = "io.github.aelgeaer.examic.action.START"
        const val ACTION_STOP = "io.github.aelgeaer.examic.action.STOP"
        const val ACTION_PAUSE = "io.github.aelgeaer.examic.action.PAUSE"
        const val ACTION_RESUME = "io.github.aelgeaer.examic.action.RESUME"
        const val EXTRA_TITLE = "title"
        const val EXTRA_MODE = "mode"
        const val EXTRA_TRANSLATE = "translate"

        private const val CHANNEL_ID = "recording"
        private const val NOTIFICATION_ID = 1
        private const val BYTES_PER_SAMPLE = 2
        private const val PAUSE_POLL_MS = 20L
        /** Upper bound for the wake lock, in case it is never released. */
        private const val MAX_RECORDING_MS = 12 * 60 * 60 * 1000L
    }
}

private fun Throwable.describe(): String = message ?: javaClass.simpleName
