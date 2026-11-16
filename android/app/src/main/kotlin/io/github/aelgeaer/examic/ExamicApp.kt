package io.github.aelgeaer.examic

import android.app.Application
import android.content.Context
import android.util.Log
import io.github.aelgeaer.examic.core.ExamicEngine
import io.github.aelgeaer.examic.core.TranscriptListener
import io.github.aelgeaer.examic.core.TranscriptStatus
import io.github.aelgeaer.examic.models.ModelInstaller
import io.github.aelgeaer.examic.models.ModelState
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** A change to a recording's transcript, as the engine reports it. */
data class TranscriptEvent(val recordingId: String, val status: TranscriptStatus, val progress: Float?)

class ExamicApp : Application() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /** The bundled models, unpacked to real files on first launch. */
    val models: ModelInstaller by lazy { ModelInstaller(this) }

    private val mutableTranscriptEvents = MutableSharedFlow<TranscriptEvent>(
        extraBufferCapacity = 64,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    /** Every transcript change: segments added, work started or finished. */
    val transcriptEvents: SharedFlow<TranscriptEvent> = mutableTranscriptEvents

    /**
     * The Rust core, opened once and off the main thread. If opening fails
     * (a full disk) every caller gets that failure from `await`.
     */
    val engine: Deferred<ExamicEngine> by lazy {
        scope.async {
            ExamicEngine.open(filesDir.absolutePath).also { engine ->
                engine.setTranscriptListener(object : TranscriptListener {
                    override fun onTranscriptChanged(recordingId: String, status: TranscriptStatus, progress: Float?) {
                        mutableTranscriptEvents.tryEmit(TranscriptEvent(recordingId, status, progress))
                    }
                })
            }
        }
    }

    override fun onCreate() {
        super.onCreate()
        scope.launch {
            val state = models.install()
            if (state is ModelState.Ready) {
                runCatching { engine.await().setModels(state.asrModel.absolutePath, state.mtDir.absolutePath) }
                    .onFailure { Log.e("examic", "could not open the library", it) }
            }
        }
    }

    val modelState: StateFlow<ModelState> get() = models.state
}

val Context.examicApp: ExamicApp get() = applicationContext as ExamicApp

/** Runs [block] against the Rust core on the I/O dispatcher. */
suspend fun <T> Context.withEngine(block: (ExamicEngine) -> T): T {
    val engine = examicApp.engine.await()
    return withContext(Dispatchers.IO) { block(engine) }
}
