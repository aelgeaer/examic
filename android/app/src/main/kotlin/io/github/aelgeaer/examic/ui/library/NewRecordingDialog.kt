package io.github.aelgeaer.examic.ui.library

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.selection.toggleable
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.core.TranscribeMode
import io.github.aelgeaer.examic.models.ModelState
import io.github.aelgeaer.examic.settings.RecordingOptions

private val MODES = listOf(TranscribeMode.OFF, TranscribeMode.LIVE, TranscribeMode.AFTER)

/** Chooses how the next recording is transcribed before it starts. */
@Composable
fun NewRecordingDialog(
    initial: RecordingOptions,
    models: ModelState,
    onDismiss: () -> Unit,
    onStart: (RecordingOptions) -> Unit,
) {
    var mode by remember { mutableStateOf(initial.mode) }
    var translate by remember { mutableStateOf(initial.translate) }
    // Without models nothing can be transcribed; recording still works.
    val modelsUsable = models is ModelState.Ready || models is ModelState.Installing || models is ModelState.Checking
    val effectiveMode = if (modelsUsable) mode else TranscribeMode.OFF

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.new_recording)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text(stringResource(R.string.transcription), style = MaterialTheme.typography.titleSmall)
                SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                    MODES.forEachIndexed { index, option ->
                        SegmentedButton(
                            selected = effectiveMode == option,
                            onClick = { mode = option },
                            enabled = modelsUsable || option == TranscribeMode.OFF,
                            shape = SegmentedButtonDefaults.itemShape(index, MODES.size),
                        ) {
                            Text(
                                stringResource(
                                    when (option) {
                                        TranscribeMode.OFF -> R.string.mode_off
                                        TranscribeMode.LIVE -> R.string.mode_live
                                        TranscribeMode.AFTER -> R.string.mode_after
                                    },
                                ),
                            )
                        }
                    }
                }
                Text(
                    stringResource(
                        when (effectiveMode) {
                            TranscribeMode.OFF -> R.string.mode_off_help
                            TranscribeMode.LIVE -> R.string.mode_live_help
                            TranscribeMode.AFTER -> R.string.mode_after_help
                        },
                    ),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .toggleable(
                            value = translate,
                            enabled = effectiveMode != TranscribeMode.OFF,
                            onValueChange = { translate = it },
                        )
                        .padding(vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    Text(stringResource(R.string.translate_to_english))
                    Switch(
                        checked = translate && effectiveMode != TranscribeMode.OFF,
                        onCheckedChange = null,
                        enabled = effectiveMode != TranscribeMode.OFF,
                    )
                }
                when (models) {
                    is ModelState.Installing -> Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Text(stringResource(R.string.models_installing), style = MaterialTheme.typography.bodySmall)
                        LinearProgressIndicator(progress = { models.progress }, modifier = Modifier.fillMaxWidth())
                    }
                    ModelState.NotBundled -> Text(
                        stringResource(R.string.models_not_bundled),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                    is ModelState.Failed -> Text(
                        stringResource(R.string.models_failed, models.message),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                    else -> {}
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                onStart(RecordingOptions(effectiveMode, translate && effectiveMode != TranscribeMode.OFF))
            }) { Text(stringResource(R.string.start_recording)) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}
