package io.github.aelgeaer.examic.ui.common

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContract
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.selection.selectable
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.core.Recording
import io.github.aelgeaer.examic.export.ExportTarget
import io.github.aelgeaer.examic.export.ExportType
import io.github.aelgeaer.examic.export.Exports
import io.github.aelgeaer.examic.ui.attempt
import java.io.File
import kotlinx.coroutines.launch

/** ACTION_CREATE_DOCUMENT with the MIME type chosen at launch time. */
private object CreateDocument : ActivityResultContract<ExportTarget, Uri?>() {
    override fun createIntent(context: Context, input: ExportTarget): Intent =
        Intent(Intent.ACTION_CREATE_DOCUMENT)
            .addCategory(Intent.CATEGORY_OPENABLE)
            .setType(input.mimeType)
            .putExtra(Intent.EXTRA_TITLE, input.fileName)

    override fun parseResult(resultCode: Int, intent: Intent?): Uri? =
        intent?.data.takeIf { resultCode == Activity.RESULT_OK }
}

/**
 * Lets the user export [recording] as Markdown or PDF, with or without the
 * English translation, to a file of their choice or through the share sheet.
 * [onMessage] reports the outcome.
 */
@Composable
fun ExportDialog(recording: Recording, onDismiss: () -> Unit, onMessage: (String) -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var type by remember { mutableStateOf(ExportType.MARKDOWN) }
    var includeTranslation by remember { mutableStateOf(recording.translate) }
    var busy by remember { mutableStateOf(false) }
    var rendered by remember { mutableStateOf<File?>(null) }
    val exported = stringResource(R.string.export_saved)
    val failed: (String) -> Unit = { onMessage(context.getString(R.string.error_generic, it)) }

    val saveLauncher = rememberLauncherForActivityResult(CreateDocument) { uri ->
        val file = rendered
        if (uri == null || file == null) {
            busy = false
            return@rememberLauncherForActivityResult
        }
        scope.launch {
            attempt(failed) { Exports.save(context, file, uri) }?.let { onMessage(exported) }
            busy = false
            onDismiss()
        }
    }

    fun run(share: Boolean) {
        busy = true
        scope.launch {
            val file = attempt(failed) { Exports.render(context, recording, type, includeTranslation) }
            if (file == null) {
                busy = false
                return@launch
            }
            rendered = file
            val target = Exports.target(recording, type)
            if (share) {
                attempt(failed) { Exports.share(context, file, target.mimeType) }
                busy = false
                onDismiss()
            } else {
                saveLauncher.launch(target)
            }
        }
    }

    AlertDialog(
        onDismissRequest = { if (!busy) onDismiss() },
        title = { Text(stringResource(R.string.export_title, recording.title)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                ExportType.entries.forEach { option ->
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .selectable(selected = type == option, onClick = { type = option })
                            .padding(vertical = 4.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        RadioButton(selected = type == option, onClick = null)
                        Column(Modifier.padding(start = 12.dp)) {
                            Text(stringResource(if (option == ExportType.PDF) R.string.export_pdf else R.string.export_markdown))
                            Text(
                                stringResource(
                                    if (option == ExportType.PDF) R.string.export_pdf_help else R.string.export_markdown_help,
                                ),
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .selectable(selected = includeTranslation, onClick = { includeTranslation = !includeTranslation })
                        .padding(vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Checkbox(checked = includeTranslation, onCheckedChange = null)
                    Text(stringResource(R.string.export_include_translation), Modifier.padding(start = 12.dp))
                }
                if (busy) CircularProgressIndicator(Modifier.align(Alignment.CenterHorizontally).padding(8.dp))
            }
        },
        confirmButton = {
            TextButton(onClick = { run(share = false) }, enabled = !busy) { Text(stringResource(R.string.export_save)) }
        },
        dismissButton = {
            TextButton(onClick = { run(share = true) }, enabled = !busy) { Text(stringResource(R.string.export_share)) }
        },
    )
}
