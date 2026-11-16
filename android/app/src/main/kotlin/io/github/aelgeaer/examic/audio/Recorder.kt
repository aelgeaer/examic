package io.github.aelgeaer.examic.audio

import android.content.Context
import android.content.Intent
import io.github.aelgeaer.examic.core.Recording
import io.github.aelgeaer.examic.settings.RecordingOptions
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow

sealed interface RecorderStatus {
    data object Idle : RecorderStatus
    data object Starting : RecorderStatus
    data class Recording(val recordingId: String, val paused: Boolean) : RecorderStatus
    data object Stopping : RecorderStatus
}

sealed interface RecorderEvent {
    data class Finished(val recording: Recording) : RecorderEvent
    data class Failed(val message: String) : RecorderEvent
}

/**
 * What the UI knows about recording. The work happens in [RecorderService];
 * this object is the process-wide state both sides share, so a recording that
 * outlives the activity is still there when the activity comes back.
 */
object Recorder {
    internal val mutableStatus = MutableStateFlow<RecorderStatus>(RecorderStatus.Idle)
    val status: StateFlow<RecorderStatus> = mutableStatus

    internal val mutableEvents = MutableSharedFlow<RecorderEvent>(
        extraBufferCapacity = 4,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val events: SharedFlow<RecorderEvent> = mutableEvents

    /** Starts recording. Requires the RECORD_AUDIO permission and a visible activity. */
    fun start(context: Context, title: String, options: RecordingOptions) {
        // Flipped here rather than in the service so the UI never observes
        // "idle" between asking for a recording and the service starting.
        if (!mutableStatus.compareAndSet(RecorderStatus.Idle, RecorderStatus.Starting)) return
        val intent = Intent(context, RecorderService::class.java)
            .setAction(RecorderService.ACTION_START)
            .putExtra(RecorderService.EXTRA_TITLE, title)
            .putExtra(RecorderService.EXTRA_MODE, options.mode.name)
            .putExtra(RecorderService.EXTRA_TRANSLATE, options.translate)
        try {
            context.startForegroundService(intent)
        } catch (e: RuntimeException) {
            mutableStatus.value = RecorderStatus.Idle
            mutableEvents.tryEmit(RecorderEvent.Failed(e.message ?: e.javaClass.simpleName))
        }
    }

    fun stop(context: Context) = send(context, RecorderService.ACTION_STOP)

    fun setPaused(context: Context, paused: Boolean) =
        send(context, if (paused) RecorderService.ACTION_PAUSE else RecorderService.ACTION_RESUME)

    private fun send(context: Context, action: String) {
        if (status.value !is RecorderStatus.Recording) return
        context.startService(Intent(context, RecorderService::class.java).setAction(action))
    }
}
