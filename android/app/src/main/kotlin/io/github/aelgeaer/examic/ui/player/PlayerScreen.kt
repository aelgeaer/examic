package io.github.aelgeaer.examic.ui.player

import android.content.Context
import android.net.Uri
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.PrimaryTabRow
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.media3.common.MediaItem
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.FileDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.MediaSource
import androidx.media3.exoplayer.source.ProgressiveMediaSource
import androidx.media3.extractor.ExtractorsFactory
import androidx.media3.extractor.ogg.OggExtractor
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.core.Bookmark
import io.github.aelgeaer.examic.core.Photo
import io.github.aelgeaer.examic.core.Recording
import io.github.aelgeaer.examic.core.RecordingStatus
import io.github.aelgeaer.examic.core.SpeechSpan
import io.github.aelgeaer.examic.core.TranscriptSegment
import io.github.aelgeaer.examic.core.TranscriptStatus
import io.github.aelgeaer.examic.examicApp
import io.github.aelgeaer.examic.export.decodePhoto
import io.github.aelgeaer.examic.playback.skipTarget
import io.github.aelgeaer.examic.transcription.TranscriptionService
import io.github.aelgeaer.examic.ui.attempt
import io.github.aelgeaer.examic.ui.common.ExportDialog
import io.github.aelgeaer.examic.ui.formatDuration
import io.github.aelgeaer.examic.ui.recorder.NoteDialog
import io.github.aelgeaer.examic.withEngine
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private val SPEEDS = listOf(0.5f, 0.8f, 1f, 1.25f, 1.5f, 2f, 3f)
private const val SEEK_STEP_MS = 10_000L
private const val POSITION_POLL_MS = 50L

private class Loaded(
    val recording: Recording,
    val bookmarks: List<Bookmark>,
    val speech: List<SpeechSpan>,
    val transcript: List<TranscriptSegment>,
    val photos: List<Photo>,
    val source: MediaSource,
)

/** Plays a recording's Ogg Opus file; the container is known, so nothing is probed. */
@androidx.annotation.OptIn(UnstableApi::class)
private fun mediaSource(path: String): MediaSource =
    ProgressiveMediaSource.Factory(FileDataSource.Factory(), ExtractorsFactory { arrayOf(OggExtractor()) })
        .createMediaSource(MediaItem.fromUri(Uri.fromFile(File(path))))

private enum class Tab { TRANSCRIPT, BOOKMARKS }

private fun buildPlayer(context: Context): ExoPlayer =
    ExoPlayer.Builder(context).build().apply {
        // Speech content, and the player yields to calls and other media.
        val attributes = AudioAttributes.Builder()
            .setUsage(C.USAGE_MEDIA)
            .setContentType(C.AUDIO_CONTENT_TYPE_SPEECH)
            .build()
        setAudioAttributes(attributes, /* handleAudioFocus = */ true)
    }

@androidx.annotation.OptIn(UnstableApi::class)
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PlayerScreen(recordingId: String, onBack: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }
    val player = remember(recordingId) { buildPlayer(context) }

    var recording by remember(recordingId) { mutableStateOf<Recording?>(null) }
    var bookmarks by remember(recordingId) { mutableStateOf(emptyList<Bookmark>()) }
    var speech by remember(recordingId) { mutableStateOf(emptyList<SpeechSpan>()) }
    var isPlaying by remember { mutableStateOf(false) }
    var positionMs by remember { mutableLongStateOf(0L) }
    var durationMs by remember { mutableLongStateOf(0L) }
    var speed by remember { mutableFloatStateOf(1f) }
    var skipSilence by remember { mutableStateOf(false) }
    var dragging by remember { mutableStateOf<Float?>(null) }
    var bookmarkAtMs by remember { mutableStateOf<Long?>(null) }
    var transcript by remember(recordingId) { mutableStateOf(emptyList<TranscriptSegment>()) }
    var photos by remember(recordingId) { mutableStateOf(emptyList<Photo>()) }
    var progress by remember(recordingId) { mutableStateOf<Float?>(null) }
    var tab by remember { mutableStateOf(Tab.TRANSCRIPT) }
    var selectedPhoto by remember { mutableStateOf<Long?>(null) }
    var viewing by remember { mutableStateOf<Photo?>(null) }
    var exporting by remember { mutableStateOf(false) }

    fun showError(message: String) {
        scope.launch { snackbar.showSnackbar(context.getString(R.string.error_generic, message)) }
    }

    DisposableEffect(player) {
        val listener = object : Player.Listener {
            override fun onIsPlayingChanged(playing: Boolean) {
                isPlaying = playing
            }

            override fun onPlayerError(error: PlaybackException) {
                showError(error.message ?: error.errorCodeName)
            }
        }
        player.addListener(listener)
        onDispose {
            player.removeListener(listener)
            player.release()
        }
    }

    LaunchedEffect(player) {
        val loaded = attempt(::showError) {
            context.withEngine { engine ->
                Loaded(
                    recording = engine.recording(recordingId),
                    bookmarks = engine.bookmarks(recordingId),
                    speech = engine.speechSpans(recordingId),
                    transcript = engine.transcript(recordingId),
                    photos = engine.photos(recordingId),
                    source = mediaSource(engine.audioPath(recordingId)),
                )
            }
        } ?: return@LaunchedEffect
        recording = loaded.recording
        bookmarks = loaded.bookmarks
        speech = loaded.speech
        transcript = loaded.transcript
        photos = loaded.photos
        durationMs = loaded.recording.durationMs
        player.setMediaSource(loaded.source)
        player.prepare()
    }

    // The player has no position callback; poll it while the screen is up.
    LaunchedEffect(player) {
        while (true) {
            positionMs = player.currentPosition
            player.duration.takeIf { it != C.TIME_UNSET }?.let { durationMs = it }
            if (skipSilence && player.isPlaying) {
                skipTarget(speech, positionMs)?.let(player::seekTo)
            }
            delay(POSITION_POLL_MS)
        }
    }

    // The transcript may still be growing (live mode, or work in progress).
    LaunchedEffect(recordingId) {
        context.examicApp.transcriptEvents
            .filter { it.recordingId == recordingId }
            .collect { event ->
                progress = event.progress.takeIf { event.status == TranscriptStatus.RUNNING }
                attempt(::showError) {
                    context.withEngine { it.recording(recordingId) to it.transcript(recordingId) }
                }?.let { (r, t) ->
                    recording = r
                    transcript = t
                }
            }
    }

    fun reloadPhotos() {
        scope.launch {
            attempt(::showError) { context.withEngine { it.photos(recordingId) } }?.let { photos = it }
        }
    }

    fun startTranscription(translateOnly: Boolean) {
        val current = recording ?: return
        scope.launch {
            attempt(::showError) {
                context.withEngine { engine ->
                    if (translateOnly) {
                        engine.translateRecording(recordingId)
                    } else {
                        engine.transcribeRecording(recordingId, current.translate)
                    }
                    engine.recording(recordingId)
                }
            }?.let {
                recording = it
                TranscriptionService.start(context)
            }
        }
    }

    fun reloadBookmarks() {
        scope.launch {
            attempt(::showError) { context.withEngine { it.bookmarks(recordingId) } }?.let { bookmarks = it }
        }
    }

    fun togglePlayback() {
        when {
            player.isPlaying -> player.pause()
            else -> {
                if (player.playbackState == Player.STATE_ENDED) player.seekTo(0)
                player.play()
            }
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(recording?.title.orEmpty(), maxLines = 1, overflow = TextOverflow.Ellipsis) },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(painterResource(R.drawable.ic_back), stringResource(R.string.back))
                    }
                },
                actions = {
                    IconButton(onClick = { exporting = true }, enabled = recording != null) {
                        Icon(painterResource(R.drawable.ic_export), stringResource(R.string.export))
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        LazyColumn(modifier = Modifier.fillMaxSize(), contentPadding = padding) {
            item(key = "controls") {
                Column(Modifier.padding(horizontal = 16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (recording?.status == RecordingStatus.RECOVERED) {
                        Text(
                            stringResource(R.string.recovered_notice),
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.error,
                        )
                    }

                    Spacer(Modifier.height(8.dp))
                    Timeline(
                        speech = speech,
                        bookmarks = bookmarks,
                        photos = photos,
                        durationMs = durationMs,
                        speechColor = MaterialTheme.colorScheme.primary,
                        trackColor = MaterialTheme.colorScheme.surfaceVariant,
                        bookmarkColor = MaterialTheme.colorScheme.tertiary,
                        photoColor = MaterialTheme.colorScheme.secondary,
                        modifier = Modifier.fillMaxWidth().height(16.dp),
                    )
                    val sliderEnd = durationMs.coerceAtLeast(1L).toFloat()
                    Slider(
                        value = dragging ?: positionMs.toFloat().coerceIn(0f, sliderEnd),
                        onValueChange = { dragging = it },
                        onValueChangeFinished = {
                            dragging?.let { player.seekTo(it.toLong()) }
                            dragging = null
                        },
                        valueRange = 0f..sliderEnd,
                    )
                    val clock = MaterialTheme.typography.labelLarge.merge(TextStyle(fontFeatureSettings = "tnum"))
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                        Text(formatDuration(dragging?.toLong() ?: positionMs), style = clock)
                        Text(formatDuration(durationMs), style = clock)
                    }

                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        IconButton(onClick = { player.seekTo((player.currentPosition - SEEK_STEP_MS).coerceAtLeast(0)) }) {
                            Icon(painterResource(R.drawable.ic_replay_10), stringResource(R.string.back_10_seconds))
                        }
                        Spacer(Modifier.width(24.dp))
                        FilledIconButton(onClick = ::togglePlayback, modifier = Modifier.size(72.dp)) {
                            Icon(
                                painterResource(if (isPlaying) R.drawable.ic_pause else R.drawable.ic_play),
                                stringResource(if (isPlaying) R.string.pause else R.string.play),
                                modifier = Modifier.size(36.dp),
                            )
                        }
                        Spacer(Modifier.width(24.dp))
                        IconButton(onClick = { player.seekTo(player.currentPosition + SEEK_STEP_MS) }) {
                            Icon(painterResource(R.drawable.ic_forward_10), stringResource(R.string.forward_10_seconds))
                        }
                    }

                    Row(
                        modifier = Modifier.horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        FilterChip(
                            selected = skipSilence,
                            onClick = { skipSilence = !skipSilence },
                            // Without detected speech there is nothing to skip to.
                            enabled = speech.isNotEmpty(),
                            label = { Text(stringResource(R.string.skip_silence)) },
                        )
                        SPEEDS.forEach { option ->
                            FilterChip(
                                selected = speed == option,
                                onClick = {
                                    speed = option
                                    player.setPlaybackSpeed(option)
                                },
                                label = { Text(speedLabel(option)) },
                            )
                        }
                    }

                }
            }

            if (photos.isNotEmpty()) {
                item(key = "photos") {
                    PhotoStrip(
                        photos = photos,
                        selected = selectedPhoto,
                        onTap = { photo ->
                            // First tap jumps there; a second opens it.
                            if (selectedPhoto == photo.id) {
                                viewing = photo
                            } else {
                                selectedPhoto = photo.id
                                player.seekTo(photo.timestampMs)
                            }
                        },
                    )
                }
            }

            item(key = "tabs") {
                PrimaryTabRow(selectedTabIndex = tab.ordinal, modifier = Modifier.padding(top = 8.dp)) {
                    Tab(selected = tab == Tab.TRANSCRIPT, onClick = { tab = Tab.TRANSCRIPT }, text = {
                        Text(stringResource(R.string.transcript))
                    })
                    Tab(selected = tab == Tab.BOOKMARKS, onClick = { tab = Tab.BOOKMARKS }, text = {
                        Text(stringResource(R.string.bookmarks))
                    })
                }
            }

            if (tab == Tab.TRANSCRIPT) {
                item(key = "transcript-status") {
                    TranscriptHeader(
                        recording = recording,
                        hasTranscript = transcript.isNotEmpty(),
                        progress = progress,
                        onTranscribe = { startTranscription(translateOnly = false) },
                        onTranslate = { startTranscription(translateOnly = true) },
                    )
                }
                items(transcript, key = { "seg-${it.startMs}" }) { segment ->
                    val current = positionMs >= segment.startMs && positionMs < segment.endMs
                    TranscriptRow(segment, current) { player.seekTo(segment.startMs) }
                }
            } else {
                item(key = "add-bookmark") {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp),
                        horizontalArrangement = Arrangement.End,
                    ) {
                        TextButton(onClick = { bookmarkAtMs = player.currentPosition }) {
                            Text(stringResource(R.string.add_bookmark))
                        }
                    }
                }
            }

            if (tab == Tab.BOOKMARKS && bookmarks.isEmpty()) {
                item(key = "none") {
                    Text(
                        stringResource(R.string.no_bookmarks),
                        modifier = Modifier.padding(PaddingValues(horizontal = 16.dp, vertical = 8.dp)),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            if (tab == Tab.BOOKMARKS) items(bookmarks, key = { it.id }) { bookmark ->
                ListItem(
                    modifier = Modifier.clickable { player.seekTo(bookmark.timestampMs) },
                    leadingContent = {
                        Text(
                            formatDuration(bookmark.timestampMs),
                            style = MaterialTheme.typography.labelLarge,
                            color = MaterialTheme.colorScheme.primary,
                        )
                    },
                    headlineContent = { Text(bookmark.note ?: stringResource(R.string.highlight_label)) },
                    trailingContent = {
                        IconButton(onClick = {
                            scope.launch {
                                attempt(::showError) { context.withEngine { it.deleteBookmark(bookmark.id) } }
                                reloadBookmarks()
                            }
                        }) {
                            Icon(painterResource(R.drawable.ic_delete), stringResource(R.string.delete_bookmark))
                        }
                    },
                )
                HorizontalDivider()
            }
        }
    }

    viewing?.let { photo ->
        PhotoViewer(
            photo = photo,
            file = File(context.filesDir, photo.file),
            onDismiss = { viewing = null },
            onDelete = {
                viewing = null
                scope.launch {
                    attempt(::showError) { context.withEngine { it.deletePhoto(photo.id) } }
                    reloadPhotos()
                }
            },
        )
    }

    if (exporting) {
        recording?.let { current ->
            ExportDialog(
                recording = current,
                onDismiss = { exporting = false },
                onMessage = { message -> scope.launch { snackbar.showSnackbar(message) } },
            )
        }
    }

    bookmarkAtMs?.let { timestampMs ->
        NoteDialog(
            title = stringResource(R.string.bookmark_title, formatDuration(timestampMs)),
            label = stringResource(R.string.note_optional_label),
            allowEmpty = true,
            onDismiss = { bookmarkAtMs = null },
            onConfirm = { note ->
                bookmarkAtMs = null
                scope.launch {
                    attempt(::showError) { context.withEngine { it.addBookmark(recordingId, timestampMs, note) } }
                    reloadBookmarks()
                }
            },
        )
    }
}

/** `1×`, `1.25×`: no trailing zeros. */
private fun speedLabel(speed: Float): String =
    (if (speed % 1f == 0f) speed.toInt().toString() else speed.toString()) + "×"

/**
 * The recording at a glance, aligned with the seek bar below it: where
 * people speak, and where the bookmarks are.
 */
@Composable
private fun Timeline(
    speech: List<SpeechSpan>,
    bookmarks: List<Bookmark>,
    photos: List<Photo>,
    durationMs: Long,
    speechColor: Color,
    trackColor: Color,
    bookmarkColor: Color,
    photoColor: Color,
    modifier: Modifier,
) {
    Canvas(modifier) {
        val band = size.height / 2
        val top = (size.height - band) / 2
        drawRoundRect(trackColor, Offset(0f, top), Size(size.width, band), CornerRadius(band / 2))
        if (durationMs <= 0) return@Canvas
        val pixelsPerMs = size.width / durationMs
        speech.forEach { span ->
            val start = (span.startMs * pixelsPerMs).coerceIn(0f, size.width)
            val end = (span.endMs * pixelsPerMs).coerceIn(start, size.width)
            // At least a pixel wide, so short remarks in a long recording show.
            drawRect(speechColor, Offset(start, top), Size((end - start).coerceAtLeast(1f), band))
        }
        val tick = 2.dp.toPx()
        bookmarks.forEach { bookmark ->
            val x = (bookmark.timestampMs * pixelsPerMs).coerceIn(0f, size.width - tick)
            drawRect(bookmarkColor, Offset(x, 0f), Size(tick, size.height))
        }
        // Photos as dots above and below the band, so they stand apart from bookmarks.
        val radius = 2.5.dp.toPx()
        photos.forEach { photo ->
            val x = (photo.timestampMs * pixelsPerMs).coerceIn(radius, size.width - radius)
            drawCircle(photoColor, radius, Offset(x, radius))
            drawCircle(photoColor, radius, Offset(x, size.height - radius))
        }
    }
}

/** A photo decoded off the main thread, small enough for [maxPixels]. */
@Composable
private fun rememberPhoto(file: File, maxPixels: Int): ImageBitmap? =
    produceState<ImageBitmap?>(null, file, maxPixels) {
        value = withContext(Dispatchers.IO) { decodePhoto(file, maxPixels)?.asImageBitmap() }
    }.value

@Composable
private fun PhotoStrip(photos: List<Photo>, selected: Long?, onTap: (Photo) -> Unit) {
    val context = LocalContext.current
    LazyRow(
        contentPadding = PaddingValues(horizontal = 16.dp, vertical = 8.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        items(photos, key = { it.id }) { photo ->
            val bitmap = rememberPhoto(File(context.filesDir, photo.file), maxPixels = 320)
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Box(
                    modifier = Modifier
                        .size(88.dp)
                        .clip(RoundedCornerShape(12.dp))
                        .background(MaterialTheme.colorScheme.surfaceVariant)
                        .then(
                            if (selected == photo.id) {
                                Modifier.border(BorderStroke(3.dp, MaterialTheme.colorScheme.primary), RoundedCornerShape(12.dp))
                            } else {
                                Modifier
                            },
                        )
                        .clickable { onTap(photo) },
                ) {
                    bitmap?.let {
                        Image(it, contentDescription = null, contentScale = ContentScale.Crop, modifier = Modifier.fillMaxSize())
                    }
                }
                Text(formatDuration(photo.timestampMs), style = MaterialTheme.typography.labelSmall)
            }
        }
    }
}

@Composable
private fun PhotoViewer(photo: Photo, file: File, onDismiss: () -> Unit, onDelete: () -> Unit) {
    val bitmap = rememberPhoto(file, maxPixels = 2048)
    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Box(Modifier.fillMaxSize().background(Color.Black).clickable(onClick = onDismiss)) {
            bitmap?.let {
                Image(it, stringResource(R.string.photo_at, formatDuration(photo.timestampMs)), Modifier.fillMaxSize())
            }
            Row(
                modifier = Modifier.fillMaxWidth().align(Alignment.TopCenter).padding(16.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    stringResource(R.string.photo_at, formatDuration(photo.timestampMs)),
                    color = Color.White,
                    style = MaterialTheme.typography.titleMedium,
                )
                IconButton(onClick = onDelete) {
                    Icon(painterResource(R.drawable.ic_delete), stringResource(R.string.delete_photo), tint = Color.White)
                }
            }
        }
    }
}

/** What the transcript is doing, and the buttons that start work on it. */
@Composable
private fun TranscriptHeader(
    recording: Recording?,
    hasTranscript: Boolean,
    progress: Float?,
    onTranscribe: () -> Unit,
    onTranslate: () -> Unit,
) {
    val status = recording?.transcriptStatus ?: return
    val busy = status == TranscriptStatus.PENDING || status == TranscriptStatus.RUNNING
    Column(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        val message = when (status) {
            TranscriptStatus.NONE -> stringResource(R.string.transcript_none)
            TranscriptStatus.PENDING -> stringResource(R.string.status_queued)
            TranscriptStatus.RUNNING -> progress?.let { stringResource(R.string.status_transcribing_percent, (it * 100).toInt()) }
                ?: stringResource(R.string.transcript_running_live)
            TranscriptStatus.FAILED -> stringResource(R.string.transcript_failed)
            TranscriptStatus.DONE -> if (hasTranscript) null else stringResource(R.string.transcript_empty)
        }
        message?.let {
            Text(it, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        if (busy) {
            if (progress != null) {
                LinearProgressIndicator(progress = { progress }, modifier = Modifier.fillMaxWidth())
            } else {
                LinearProgressIndicator(Modifier.fillMaxWidth())
            }
        } else {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (status == TranscriptStatus.DONE && hasTranscript) {
                    OutlinedButton(onClick = onTranscribe) { Text(stringResource(R.string.transcribe_again)) }
                } else {
                    FilledTonalButton(onClick = onTranscribe) { Text(stringResource(R.string.transcribe_now)) }
                }
                if (hasTranscript && !recording.translate) {
                    FilledTonalButton(onClick = onTranslate) { Text(stringResource(R.string.translate)) }
                }
            }
        }
    }
}

@Composable
private fun TranscriptRow(segment: TranscriptSegment, current: Boolean, onClick: () -> Unit) {
    val background = if (current) MaterialTheme.colorScheme.secondaryContainer else Color.Transparent
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(background)
            .clickable(onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 8.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            formatDuration(segment.startMs),
            style = MaterialTheme.typography.labelLarge.merge(TextStyle(fontFeatureSettings = "tnum")),
            color = MaterialTheme.colorScheme.primary,
        )
        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(segment.text, style = MaterialTheme.typography.bodyLarge)
            segment.translation?.let {
                Text(it, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}
