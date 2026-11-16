package io.github.aelgeaer.examic.ui.library

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
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
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.audio.Recorder
import io.github.aelgeaer.examic.audio.RecorderStatus
import io.github.aelgeaer.examic.core.Recording
import io.github.aelgeaer.examic.core.RecordingStatus
import io.github.aelgeaer.examic.core.TranscriptStatus
import io.github.aelgeaer.examic.examicApp
import io.github.aelgeaer.examic.settings.RecordingOptions
import io.github.aelgeaer.examic.ui.attempt
import io.github.aelgeaer.examic.ui.common.ExportDialog
import io.github.aelgeaer.examic.ui.formatDateTime
import io.github.aelgeaer.examic.ui.formatDuration
import io.github.aelgeaer.examic.withEngine
import kotlinx.coroutines.launch

private sealed interface LibraryState {
    data object Loading : LibraryState
    data class Failed(val message: String) : LibraryState
    data class Loaded(val recordings: List<Recording>) : LibraryState
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LibraryScreen(
    snackbar: SnackbarHostState,
    onRecord: (title: String, options: RecordingOptions) -> Unit,
    onOpenRecorder: () -> Unit,
    onOpenRecording: (id: String) -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val recorderStatus by Recorder.status.collectAsStateWithLifecycle()
    var state by remember { mutableStateOf<LibraryState>(LibraryState.Loading) }
    var reload by remember { mutableIntStateOf(0) }
    var renaming by remember { mutableStateOf<Recording?>(null) }
    var deleting by remember { mutableStateOf<Recording?>(null) }
    var exporting by remember { mutableStateOf<Recording?>(null) }
    var choosingOptions by remember { mutableStateOf(false) }
    var showingLicenses by remember { mutableStateOf(false) }
    var appMenuOpen by remember { mutableStateOf(false) }
    var pendingOptions by remember { mutableStateOf(RecordingOptions.load(context)) }
    val models by context.examicApp.models.state.collectAsStateWithLifecycle()
    // Transcription progress, kept live from the engine's events.
    var progress by remember { mutableStateOf(emptyMap<String, Float>()) }

    // Reloads after an edit and whenever a recording starts or finishes.
    LaunchedEffect(reload, recorderStatus == RecorderStatus.Idle) {
        state = attempt({ state = LibraryState.Failed(it) }) {
            LibraryState.Loaded(context.withEngine { it.recordings() })
        } ?: return@LaunchedEffect
    }

    LaunchedEffect(Unit) {
        context.examicApp.transcriptEvents.collect { event ->
            progress = if (event.progress != null && event.status == TranscriptStatus.RUNNING) {
                progress + (event.recordingId to event.progress!!)
            } else {
                progress - event.recordingId
            }
            // Only status changes alter the list; segment updates do not.
            if (event.progress == null || event.status != TranscriptStatus.RUNNING) reload++
        }
    }

    fun showError(message: String) {
        scope.launch { snackbar.showSnackbar(context.getString(R.string.error_generic, message)) }
    }

    val defaultTitle = stringResource(R.string.default_title, formatDateTime(System.currentTimeMillis()))
    val microphoneDenied = stringResource(R.string.microphone_denied)
    val permissions = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { granted ->
        // Notifications are optional: recording works without the status
        // notification being visible. The microphone is not.
        if (granted[Manifest.permission.RECORD_AUDIO] == true) {
            onRecord(defaultTitle, pendingOptions)
        } else {
            scope.launch { snackbar.showSnackbar(microphoneDenied) }
        }
    }

    fun record(options: RecordingOptions) {
        pendingOptions = options
        RecordingOptions.save(context, options)
        val needed = buildList {
            add(Manifest.permission.RECORD_AUDIO)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) add(Manifest.permission.POST_NOTIFICATIONS)
        }.filter { context.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (Manifest.permission.RECORD_AUDIO in needed) {
            permissions.launch(needed.toTypedArray())
        } else {
            onRecord(defaultTitle, options)
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.app_name)) },
                actions = {
                    Box {
                        IconButton(onClick = { appMenuOpen = true }) {
                            Icon(painterResource(R.drawable.ic_more), stringResource(R.string.more_options))
                        }
                        DropdownMenu(expanded = appMenuOpen, onDismissRequest = { appMenuOpen = false }) {
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.licenses)) },
                                onClick = { appMenuOpen = false; showingLicenses = true },
                            )
                        }
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
        floatingActionButton = {
            if (recorderStatus == RecorderStatus.Idle && state is LibraryState.Loaded) {
                ExtendedFloatingActionButton(
                    onClick = { choosingOptions = true },
                    icon = { Icon(painterResource(R.drawable.ic_mic), contentDescription = null) },
                    text = { Text(stringResource(R.string.record)) },
                )
            }
        },
    ) { padding ->
        when (val current = state) {
            LibraryState.Loading -> CenteredBox(padding) { CircularProgressIndicator() }
            is LibraryState.Failed -> CenteredBox(padding) {
                Message(stringResource(R.string.library_unlock_failed), current.message)
            }
            is LibraryState.Loaded -> {
                val finished = current.recordings.filter { it.status != RecordingStatus.RECORDING }
                LazyColumn(
                    modifier = Modifier.fillMaxSize(),
                    // Room below the last row so the button never covers it.
                    contentPadding = PaddingValues(
                        top = padding.calculateTopPadding(),
                        bottom = padding.calculateBottomPadding() + 88.dp,
                    ),
                ) {
                    if (recorderStatus != RecorderStatus.Idle) {
                        item(key = "active") { ActiveRecordingCard(onOpenRecorder) }
                    }
                    if (finished.isEmpty() && recorderStatus == RecorderStatus.Idle) {
                        item(key = "empty") {
                            CenteredBox(PaddingValues(top = 96.dp)) {
                                Message(
                                    stringResource(R.string.library_empty_title),
                                    stringResource(R.string.library_empty_body),
                                )
                            }
                        }
                    }
                    items(finished, key = { it.id }) { recording ->
                        RecordingRow(
                            recording = recording,
                            progress = progress[recording.id],
                            onOpen = { onOpenRecording(recording.id) },
                            onRename = { renaming = recording },
                            onExport = { exporting = recording },
                            onDelete = { deleting = recording },
                        )
                        HorizontalDivider()
                    }
                }
            }
        }
    }

    if (showingLicenses) {
        val notices = remember {
            runCatching { context.assets.open("THIRD_PARTY_NOTICES.md").bufferedReader().use { it.readText() } }
                .getOrDefault("")
        }
        AlertDialog(
            onDismissRequest = { showingLicenses = false },
            title = { Text(stringResource(R.string.licenses)) },
            text = {
                Text(
                    notices,
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.verticalScroll(rememberScrollState()),
                )
            },
            confirmButton = {
                TextButton(onClick = { showingLicenses = false }) { Text(stringResource(android.R.string.ok)) }
            },
        )
    }

    if (choosingOptions) {
        NewRecordingDialog(
            initial = pendingOptions,
            models = models,
            onDismiss = { choosingOptions = false },
            onStart = { options ->
                choosingOptions = false
                record(options)
            },
        )
    }

    exporting?.let { recording ->
        ExportDialog(
            recording = recording,
            onDismiss = { exporting = null },
            onMessage = { message -> scope.launch { snackbar.showSnackbar(message) } },
        )
    }

    renaming?.let { recording ->
        RenameDialog(
            current = recording.title,
            onDismiss = { renaming = null },
            onConfirm = { title ->
                renaming = null
                scope.launch {
                    attempt(::showError) { context.withEngine { it.renameRecording(recording.id, title) } }
                    reload++
                }
            },
        )
    }

    deleting?.let { recording ->
        AlertDialog(
            onDismissRequest = { deleting = null },
            title = { Text(stringResource(R.string.delete_title)) },
            text = { Text(stringResource(R.string.delete_body, recording.title)) },
            confirmButton = {
                TextButton(onClick = {
                    deleting = null
                    scope.launch {
                        attempt(::showError) { context.withEngine { it.deleteRecording(recording.id) } }
                        reload++
                    }
                }) { Text(stringResource(R.string.delete)) }
            },
            dismissButton = {
                TextButton(onClick = { deleting = null }) { Text(stringResource(R.string.cancel)) }
            },
        )
    }
}

@Composable
private fun CenteredBox(padding: PaddingValues, content: @Composable () -> Unit) {
    Box(
        modifier = Modifier.fillMaxSize().padding(padding).padding(horizontal = 32.dp),
        contentAlignment = Alignment.Center,
    ) { content() }
}

@Composable
private fun Message(title: String, body: String) {
    Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(title, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
        Text(
            body,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}

@Composable
private fun ActiveRecordingCard(onOpen: () -> Unit) {
    Card(
        onClick = onOpen,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer),
    ) {
        ListItem(
            colors = androidx.compose.material3.ListItemDefaults.colors(
                containerColor = MaterialTheme.colorScheme.errorContainer,
            ),
            leadingContent = { Icon(painterResource(R.drawable.ic_mic), contentDescription = null) },
            headlineContent = { Text(stringResource(R.string.recording_in_progress)) },
            trailingContent = { Text(stringResource(R.string.open), style = MaterialTheme.typography.labelLarge) },
        )
    }
}

@Composable
private fun RecordingRow(
    recording: Recording,
    progress: Float?,
    onOpen: () -> Unit,
    onRename: () -> Unit,
    onExport: () -> Unit,
    onDelete: () -> Unit,
) {
    var menuOpen by remember { mutableStateOf(false) }
    val details = buildList {
        add(formatDateTime(recording.createdAtMs))
        add(formatDuration(recording.durationMs))
        if (recording.bookmarkCount > 0) {
            add(pluralStringResource(R.plurals.bookmark_count, recording.bookmarkCount, recording.bookmarkCount))
        }
        if (recording.photoCount > 0) {
            add(pluralStringResource(R.plurals.photo_count, recording.photoCount, recording.photoCount))
        }
        when (recording.transcriptStatus) {
            TranscriptStatus.DONE -> add(stringResource(R.string.status_transcribed))
            TranscriptStatus.PENDING -> add(stringResource(R.string.status_queued))
            TranscriptStatus.RUNNING -> add(
                progress?.let { stringResource(R.string.status_transcribing_percent, (it * 100).toInt()) }
                    ?: stringResource(R.string.status_transcribing),
            )
            TranscriptStatus.FAILED -> add(stringResource(R.string.status_transcript_failed))
            TranscriptStatus.NONE -> {}
        }
        if (recording.status == RecordingStatus.RECOVERED) add(stringResource(R.string.recovered))
    }
    ListItem(
        modifier = Modifier.clickable(onClick = onOpen),
        headlineContent = { Text(recording.title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
        supportingContent = { Text(details.joinToString("  ·  "), maxLines = 2, overflow = TextOverflow.Ellipsis) },
        trailingContent = {
            Box {
                IconButton(onClick = { menuOpen = true }) {
                    Icon(painterResource(R.drawable.ic_more), stringResource(R.string.more_options))
                }
                DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.rename)) },
                        onClick = { menuOpen = false; onRename() },
                    )
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.export)) },
                        onClick = { menuOpen = false; onExport() },
                    )
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.delete)) },
                        onClick = { menuOpen = false; onDelete() },
                    )
                }
            }
        },
    )
}

@Composable
private fun RenameDialog(current: String, onDismiss: () -> Unit, onConfirm: (String) -> Unit) {
    var title by remember { mutableStateOf(current) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.rename_title)) },
        text = {
            OutlinedTextField(
                value = title,
                onValueChange = { title = it },
                label = { Text(stringResource(R.string.title_label)) },
                singleLine = true,
            )
        },
        confirmButton = {
            TextButton(onClick = { onConfirm(title.trim()) }, enabled = title.isNotBlank()) {
                Text(stringResource(R.string.save))
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}
