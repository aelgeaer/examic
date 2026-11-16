package io.github.aelgeaer.examic.ui.recorder

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.audio.NativeAudio
import io.github.aelgeaer.examic.audio.Recorder
import io.github.aelgeaer.examic.audio.RecorderStatus
import io.github.aelgeaer.examic.core.TranscribeMode
import io.github.aelgeaer.examic.core.TranscriptSegment
import io.github.aelgeaer.examic.examicApp
import io.github.aelgeaer.examic.ui.attempt
import io.github.aelgeaer.examic.ui.formatDuration
import io.github.aelgeaer.examic.withEngine
import java.io.File
import kotlin.math.max
import kotlin.math.min
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch

/** One waveform bar per this much audio. */
private const val BAR_INTERVAL_NANOS = 50_000_000L
private const val SPEECH_THRESHOLD = 0.5f
/** Live transcript segments shown on the recorder; older ones scroll away. */
private const val LIVE_SEGMENTS = 3

/** The most recent bars of the live waveform, newest first. */
private class Waveform(private val capacity: Int) {
    private val levels = FloatArray(capacity)
    private val speech = BooleanArray(capacity)
    private var next = 0

    var size = 0
        private set

    /** Bumped on every change; reading it makes the canvas redraw. */
    var version by mutableIntStateOf(0)
        private set

    fun push(level: Float, isSpeech: Boolean) {
        levels[next] = level
        speech[next] = isSpeech
        next = (next + 1) % capacity
        size = min(size + 1, capacity)
        version++
    }

    private fun index(age: Int) = (next - 1 - age + 2 * capacity) % capacity
    fun level(age: Int) = levels[index(age)]
    fun isSpeech(age: Int) = speech[index(age)]
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RecorderScreen(onBack: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }
    val status by Recorder.status.collectAsStateWithLifecycle()
    val recording = status as? RecorderStatus.Recording
    val paused by rememberUpdatedState(recording?.paused == true)

    var positionMs by remember { mutableLongStateOf(0L) }
    var speaking by remember { mutableStateOf(false) }
    val waveform = remember { Waveform(capacity = 400) }
    var noteAtMs by remember { mutableStateOf<Long?>(null) }
    var cameraOpen by remember { mutableStateOf(false) }
    var mode by remember { mutableStateOf(TranscribeMode.OFF) }
    var liveTranscript by remember { mutableStateOf(emptyList<TranscriptSegment>()) }

    // Nothing to show without a recording, e.g. when the screen is restored
    // after the process was killed.
    LaunchedEffect(status) { if (status == RecorderStatus.Idle) onBack() }

    // Reads the engine's meter once per display frame and folds it into one
    // bar per 50 ms, keeping the loudest moment of each interval.
    LaunchedEffect(recording?.recordingId) {
        if (recording == null) return@LaunchedEffect
        var peak = 0f
        var speech = 0f
        var barStartedAt = 0L
        while (true) {
            val now = withFrameNanos { it }
            val telemetry = NativeAudio.snapshot()
            positionMs = telemetry.positionMs
            peak = max(peak, telemetry.level)
            speech = max(speech, telemetry.speech)
            if (now - barStartedAt >= BAR_INTERVAL_NANOS) {
                speaking = !paused && speech >= SPEECH_THRESHOLD
                if (!paused) waveform.push(peak, speaking)
                peak = 0f
                speech = 0f
                barStartedAt = now
            }
        }
    }

    // In live mode the transcript grows while recording; the engine says when.
    LaunchedEffect(recording?.recordingId) {
        val id = recording?.recordingId ?: return@LaunchedEffect
        mode = attempt({}) { context.withEngine { it.recording(id).transcribeMode } } ?: TranscribeMode.OFF
        if (mode != TranscribeMode.LIVE) return@LaunchedEffect
        context.examicApp.transcriptEvents
            .filter { it.recordingId == id }
            .collect {
                attempt({}) { context.withEngine { engine -> engine.transcript(id) } }
                    ?.let { segments -> liveTranscript = segments.takeLast(LIVE_SEGMENTS) }
            }
    }

    fun announce(message: String) {
        scope.launch {
            snackbar.currentSnackbarData?.dismiss()
            snackbar.showSnackbar(message)
        }
    }

    fun bookmark(timestampMs: Long?, note: String?, confirmation: Int) {
        val id = recording?.recordingId ?: return
        scope.launch {
            val added = attempt({ announce(context.getString(R.string.error_generic, it)) }) {
                context.withEngine { it.addBookmark(id, timestampMs, note) }
            } ?: return@launch
            announce(context.getString(confirmation, formatDuration(added.timestampMs)))
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {},
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(painterResource(R.drawable.ic_back), stringResource(R.string.back))
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(
            modifier = Modifier.fillMaxSize().padding(padding).padding(horizontal = 24.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Text(
                text = formatDuration(positionMs),
                // Tabular digits keep the clock from jittering as it counts.
                style = MaterialTheme.typography.displayLarge.merge(TextStyle(fontFeatureSettings = "tnum")),
            )
            Text(
                text = stringResource(
                    when {
                        status == RecorderStatus.Starting -> R.string.recorder_starting
                        status == RecorderStatus.Stopping -> R.string.recorder_saving
                        paused -> R.string.recorder_paused
                        speaking -> R.string.speech_detected
                        else -> R.string.no_speech_detected
                    },
                ),
                style = MaterialTheme.typography.titleMedium,
                color = if (speaking) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant,
            )

            Spacer(Modifier.height(32.dp))
            WaveformCanvas(
                waveform = waveform,
                speechColor = MaterialTheme.colorScheme.primary,
                quietColor = MaterialTheme.colorScheme.outlineVariant,
                modifier = Modifier.fillMaxWidth().height(160.dp),
            )
            if (mode == TranscribeMode.LIVE) {
                Spacer(Modifier.height(16.dp))
                LiveTranscript(liveTranscript)
            }
            Spacer(Modifier.height(40.dp))

            if (recording == null) {
                CircularProgressIndicator()
            } else {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceEvenly,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    LabelledButton(stringResource(R.string.highlight)) {
                        FilledTonalIconButton(onClick = { bookmark(null, null, R.string.highlight_added) }) {
                            Icon(painterResource(R.drawable.ic_flag), contentDescription = null)
                        }
                    }
                    LabelledButton(stringResource(if (paused) R.string.resume else R.string.pause)) {
                        FilledTonalIconButton(onClick = { Recorder.setPaused(context, !paused) }) {
                            Icon(
                                painterResource(if (paused) R.drawable.ic_mic else R.drawable.ic_pause),
                                contentDescription = null,
                            )
                        }
                    }
                    LabelledButton(stringResource(R.string.stop)) {
                        FilledIconButton(
                            onClick = { Recorder.stop(context) },
                            modifier = Modifier.size(72.dp),
                            colors = IconButtonDefaults.filledIconButtonColors(
                                containerColor = MaterialTheme.colorScheme.error,
                                contentColor = MaterialTheme.colorScheme.onError,
                            ),
                        ) {
                            Icon(
                                painterResource(R.drawable.ic_stop),
                                contentDescription = null,
                                modifier = Modifier.size(36.dp),
                            )
                        }
                    }
                    LabelledButton(stringResource(R.string.add_note)) {
                        // The note belongs to the moment the button was
                        // pressed, not to when typing finished.
                        FilledTonalIconButton(onClick = { noteAtMs = positionMs }) {
                            Icon(painterResource(R.drawable.ic_note), contentDescription = null)
                        }
                    }
                    LabelledButton(stringResource(R.string.take_photo)) {
                        FilledTonalIconButton(onClick = { cameraOpen = true }) {
                            Icon(painterResource(R.drawable.ic_camera), contentDescription = null)
                        }
                    }
                }
            }
        }
    }

    val recordingId = recording?.recordingId
    if (cameraOpen && recordingId != null) {
        CameraSheet(
            photoFile = {
                context.withEngine { engine -> File(context.filesDir, engine.photoDir(recordingId)) }
                    .resolve(newPhotoName())
            },
            onCaptured = { file, timestampMs ->
                scope.launch {
                    val added = attempt({ announce(context.getString(R.string.error_generic, it)) }) {
                        context.withEngine { engine ->
                            val relative = engine.photoDir(recordingId) + "/" + file.name
                            engine.addPhoto(recordingId, relative, timestampMs)
                        }
                    } ?: return@launch
                    announce(context.getString(R.string.photo_added, formatDuration(added.timestampMs)))
                }
            },
            onError = ::announce,
            onDismiss = { cameraOpen = false },
        )
    }

    noteAtMs?.let { timestampMs ->
        NoteDialog(
            title = stringResource(R.string.note_title, formatDuration(timestampMs)),
            label = stringResource(R.string.note_label),
            allowEmpty = false,
            onDismiss = { noteAtMs = null },
            onConfirm = { note ->
                noteAtMs = null
                bookmark(timestampMs, note, R.string.note_added)
            },
        )
    }
}

/** The newest live transcript lines, newest at the bottom. */
@Composable
private fun LiveTranscript(segments: List<TranscriptSegment>) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            if (segments.isEmpty()) {
                Text(
                    stringResource(R.string.live_transcript_waiting),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            segments.forEach { segment ->
                Text(segment.text, style = MaterialTheme.typography.bodyMedium, maxLines = 3, overflow = TextOverflow.Ellipsis)
                segment.translation?.let {
                    Text(
                        it,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
        }
    }
}

@Composable
private fun LabelledButton(label: String, button: @Composable () -> Unit) {
    Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(4.dp)) {
        button()
        Text(label, style = MaterialTheme.typography.labelMedium)
    }
}

/** Bars scroll in from the right; speech is drawn in the accent colour. */
@Composable
private fun WaveformCanvas(waveform: Waveform, speechColor: Color, quietColor: Color, modifier: Modifier) {
    Canvas(modifier) {
        waveform.version
        val barWidth = 3.dp.toPx()
        val step = barWidth + 2.dp.toPx()
        val visible = min(waveform.size, (size.width / step).toInt())
        for (age in 0 until visible) {
            val barHeight = max(barWidth, waveform.level(age) * size.height)
            drawRoundRect(
                color = if (waveform.isSpeech(age)) speechColor else quietColor,
                topLeft = Offset(size.width - (age + 1) * step, (size.height - barHeight) / 2),
                size = Size(barWidth, barHeight),
                cornerRadius = CornerRadius(barWidth / 2),
            )
        }
    }
}

/** A single-field dialog for a bookmark's note, shared with the player. */
@Composable
fun NoteDialog(
    title: String,
    label: String,
    allowEmpty: Boolean,
    onDismiss: () -> Unit,
    onConfirm: (note: String?) -> Unit,
) {
    var note by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(
                value = note,
                onValueChange = { note = it },
                label = { Text(label) },
                minLines = 2,
                maxLines = 5,
            )
        },
        confirmButton = {
            TextButton(
                onClick = { onConfirm(note.trim().ifEmpty { null }) },
                enabled = allowEmpty || note.isNotBlank(),
            ) { Text(stringResource(R.string.save)) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}
