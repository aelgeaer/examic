package io.github.aelgeaer.examic.export

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.graphics.Typeface
import android.graphics.pdf.PdfDocument
import android.text.Layout
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.StaticLayout
import android.text.TextPaint
import android.text.style.ForegroundColorSpan
import android.text.style.StyleSpan
import io.github.aelgeaer.examic.core.TimelineEntry
import io.github.aelgeaer.examic.core.TimelineKind
import io.github.aelgeaer.examic.ui.formatDuration
import java.io.File
import java.io.OutputStream

/** Labels the PDF needs in the user's language. */
data class PdfLabels(
    val date: String,
    val duration: String,
    val noTranscript: String,
    val highlight: String,
)

/**
 * Lays a recording's timeline out on A4 pages with the platform's
 * [PdfDocument]: no library, and the system fonts cover Cyrillic.
 *
 * Text is set with [StaticLayout] and may break across pages line by line;
 * photos are scaled to the text width and placed at their moment.
 */
class PdfExporter(
    private val title: String,
    private val dateText: String,
    private val durationMs: Long,
    private val labels: PdfLabels,
    /** Resolves a photo's relative path to its file. */
    private val photoFile: (String) -> File,
) {
    private val document = PdfDocument()
    private var page: PdfDocument.Page? = null
    private var pageNumber = 0
    private var y = 0f

    private val titlePaint = TextPaint(Paint.ANTI_ALIAS_FLAG).apply {
        textSize = 20f
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        color = Color.BLACK
    }
    private val bodyPaint = TextPaint(Paint.ANTI_ALIAS_FLAG).apply {
        textSize = 11f
        color = Color.BLACK
    }
    private val metaPaint = TextPaint(bodyPaint).apply { color = GREY }
    private val translationPaint = TextPaint(bodyPaint).apply {
        color = GREY
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.ITALIC)
    }
    private val footerPaint = TextPaint(metaPaint).apply { textSize = 8f }
    private val imagePaint = Paint(Paint.FILTER_BITMAP_FLAG or Paint.ANTI_ALIAS_FLAG)

    private val contentWidth = PAGE_WIDTH - 2 * MARGIN

    private fun canvas(): Canvas = page?.canvas ?: newPage()

    private fun newPage(): Canvas {
        finishPage()
        pageNumber++
        val info = PdfDocument.PageInfo.Builder(PAGE_WIDTH.toInt(), PAGE_HEIGHT.toInt(), pageNumber).create()
        val started = document.startPage(info)
        page = started
        y = MARGIN
        return started.canvas
    }

    private fun finishPage() {
        page?.let {
            val footer = "$title · $pageNumber"
            it.canvas.drawText(footer, MARGIN, PAGE_HEIGHT - MARGIN / 2, footerPaint)
            document.finishPage(it)
        }
        page = null
    }

    private val bottom get() = PAGE_HEIGHT - MARGIN

    private fun layout(text: CharSequence, paint: TextPaint, width: Float = contentWidth): StaticLayout =
        StaticLayout.Builder.obtain(text, 0, text.length, paint, width.toInt())
            .setAlignment(Layout.Alignment.ALIGN_NORMAL)
            .setLineSpacing(0f, 1.15f)
            .build()

    /** Draws a text block, continuing on new pages where it does not fit. */
    private fun drawText(text: CharSequence, paint: TextPaint, spaceAfter: Float = 6f) {
        val layout = layout(text, paint)
        var line = 0
        while (line < layout.lineCount) {
            var c = canvas()
            if (y + layout.getLineBottom(line) - layout.getLineTop(line) > bottom) c = newPage()
            // As many lines as fit on this page.
            val firstTop = layout.getLineTop(line)
            var last = line
            while (last + 1 < layout.lineCount && y + layout.getLineBottom(last + 1) - firstTop <= bottom) last++
            val height = (layout.getLineBottom(last) - firstTop).toFloat()
            c.save()
            c.translate(MARGIN, y - firstTop)
            c.clipRect(0f, firstTop.toFloat(), contentWidth, firstTop + height)
            layout.draw(c)
            c.restore()
            y += height
            line = last + 1
        }
        y += spaceAfter
    }

    private fun drawPhoto(entry: TimelineEntry) {
        val file = photoFile(entry.file ?: return)
        val maxHeight = (bottom - MARGIN) * 0.6f
        val bitmap: Bitmap = decodePhoto(file, maxPixels = 1600) ?: return
        val scale = minOf(contentWidth / bitmap.width, maxHeight / bitmap.height)
        val width = bitmap.width * scale
        val height = bitmap.height * scale
        val caption = layout("[${formatDuration(entry.timeMs)}]", metaPaint)
        if (y + height + caption.height > bottom) newPage()
        val c = canvas()
        c.drawBitmap(bitmap, null, RectF(MARGIN, y, MARGIN + width, y + height), imagePaint)
        bitmap.recycle()
        y += height + 2f
        drawText("[${formatDuration(entry.timeMs)}]", metaPaint, spaceAfter = 10f)
    }

    private fun stamped(time: Long, text: String, accent: Int): CharSequence {
        val stamp = "[${formatDuration(time)}] "
        return SpannableStringBuilder(stamp + text).apply {
            setSpan(ForegroundColorSpan(accent), 0, stamp.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            setSpan(StyleSpan(Typeface.BOLD), 0, stamp.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
        }
    }

    /** Writes the document for [entries] to [out]. */
    fun write(entries: List<TimelineEntry>, out: OutputStream) {
        try {
            newPage()
            drawText(title, titlePaint, spaceAfter = 4f)
            drawText("${labels.date}: $dateText    ${labels.duration}: ${formatDuration(durationMs)}", metaPaint, 16f)
            if (entries.none { it.kind == TimelineKind.SPEECH }) drawText(labels.noTranscript, metaPaint, 12f)
            for (entry in entries) {
                when (entry.kind) {
                    TimelineKind.SPEECH -> {
                        drawText(stamped(entry.timeMs, entry.text.orEmpty(), ACCENT), bodyPaint, 2f)
                        entry.translation?.let { drawText(it, translationPaint, 2f) }
                        y += 6f
                    }
                    TimelineKind.BOOKMARK ->
                        drawText(stamped(entry.timeMs, "★ " + (entry.text ?: labels.highlight), BOOKMARK), bodyPaint, 8f)
                    TimelineKind.PHOTO -> drawPhoto(entry)
                }
            }
            finishPage()
            document.writeTo(out)
        } finally {
            document.close()
        }
    }

    private companion object {
        // A4 in PostScript points.
        const val PAGE_WIDTH = 595f
        const val PAGE_HEIGHT = 842f
        const val MARGIN = 48f
        val GREY = Color.rgb(0x55, 0x55, 0x55)
        val ACCENT = Color.rgb(0x1F, 0x5F, 0xBF)
        val BOOKMARK = Color.rgb(0xB0, 0x5A, 0x00)
    }
}
