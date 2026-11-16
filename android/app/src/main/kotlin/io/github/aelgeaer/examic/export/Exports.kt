package io.github.aelgeaer.examic.export

import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.core.content.FileProvider
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.core.ExportFormat
import io.github.aelgeaer.examic.core.Recording
import io.github.aelgeaer.examic.ui.formatDateTime
import io.github.aelgeaer.examic.withEngine
import java.io.File
import java.io.IOException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

enum class ExportType { MARKDOWN, PDF }

/** A file name for the export, its MIME type, and whether it is a zip. */
data class ExportTarget(val fileName: String, val mimeType: String)

/**
 * Exports run in two steps: the content is written to a file in the cache,
 * then copied wherever the user chose (the Storage Access Framework) or handed
 * to the share sheet.
 */
object Exports {
    private fun baseName(recording: Recording): String =
        recording.title.replace(Regex("""[\\/:*?"<>|\n\r\t]"""), " ").trim().ifEmpty { "recording" }.take(80)

    /** Markdown becomes a zip when the recording has photos. */
    fun target(recording: Recording, type: ExportType): ExportTarget = when {
        type == ExportType.PDF -> ExportTarget("${baseName(recording)}.pdf", "application/pdf")
        recording.photoCount > 0 -> ExportTarget("${baseName(recording)}.zip", "application/zip")
        else -> ExportTarget("${baseName(recording)}.md", "text/markdown")
    }

    private fun exportDir(context: Context): File =
        File(context.cacheDir, "exports").apply {
            deleteRecursively()
            mkdirs()
        }

    /** Writes the export to a cache file and returns it. */
    suspend fun render(context: Context, recording: Recording, type: ExportType, includeTranslation: Boolean): File {
        val file = File(exportDir(context), target(recording, type).fileName)
        val date = formatDateTime(recording.createdAtMs)
        when (type) {
            ExportType.MARKDOWN -> context.withEngine { engine ->
                val format = engine.exportMarkdown(recording.id, includeTranslation, date, file.absolutePath)
                check((format == ExportFormat.ZIP) == (recording.photoCount > 0)) { "photos changed during export" }
            }
            ExportType.PDF -> {
                val entries = context.withEngine { it.exportTimeline(recording.id, includeTranslation) }
                val labels = PdfLabels(
                    date = context.getString(R.string.export_date),
                    duration = context.getString(R.string.export_duration),
                    noTranscript = context.getString(R.string.export_no_transcript),
                    highlight = context.getString(R.string.highlight_label),
                )
                withContext(Dispatchers.IO) {
                    file.outputStream().use { out ->
                        PdfExporter(recording.title, date, recording.durationMs, labels) {
                            File(context.filesDir, it)
                        }.write(entries, out)
                    }
                }
            }
        }
        return file
    }

    /** Copies a rendered export to [uri] (from ACTION_CREATE_DOCUMENT). */
    suspend fun save(context: Context, file: File, uri: Uri) = withContext(Dispatchers.IO) {
        val out = context.contentResolver.openOutputStream(uri, "wt") ?: throw IOException("cannot write $uri")
        out.use { stream -> file.inputStream().use { it.copyTo(stream) } }
    }

    /** Opens the share sheet for a rendered export. */
    fun share(context: Context, file: File, mimeType: String) {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.exports", file)
        val send = Intent(Intent.ACTION_SEND)
            .setType(mimeType)
            .putExtra(Intent.EXTRA_STREAM, uri)
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        send.clipData = ClipData.newRawUri(file.name, uri)
        context.startActivity(
            Intent.createChooser(send, context.getString(R.string.export_share)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
        )
    }
}
