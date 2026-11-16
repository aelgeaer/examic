package io.github.aelgeaer.examic.export

import io.github.aelgeaer.examic.core.Recording
import io.github.aelgeaer.examic.core.RecordingStatus
import io.github.aelgeaer.examic.core.TranscribeMode
import io.github.aelgeaer.examic.core.TranscriptStatus
import org.junit.Assert.assertEquals
import org.junit.Test

class ExportsTest {
    private fun recording(title: String, photos: Int = 0) = Recording(
        id = "r",
        title = title,
        createdAtMs = 0,
        durationMs = 1_000,
        sizeBytes = 0,
        status = RecordingStatus.READY,
        bookmarkCount = 0,
        photoCount = photos,
        transcribeMode = TranscribeMode.AFTER,
        translate = false,
        transcriptStatus = TranscriptStatus.DONE,
    )

    @Test
    fun markdownBecomesAZipWhenThereArePhotos() {
        assertEquals(ExportTarget("Лекция 3.md", "text/markdown"), Exports.target(recording("Лекция 3"), ExportType.MARKDOWN))
        assertEquals(
            ExportTarget("Лекция 3.zip", "application/zip"),
            Exports.target(recording("Лекция 3", photos = 2), ExportType.MARKDOWN),
        )
        assertEquals(
            ExportTarget("Лекция 3.pdf", "application/pdf"),
            Exports.target(recording("Лекция 3", photos = 2), ExportType.PDF),
        )
    }

    @Test
    fun fileNamesAreSafe() {
        assertEquals("a b c.md", Exports.target(recording("a/b:c"), ExportType.MARKDOWN).fileName)
        assertEquals("recording.md", Exports.target(recording("  "), ExportType.MARKDOWN).fileName)
    }
}
