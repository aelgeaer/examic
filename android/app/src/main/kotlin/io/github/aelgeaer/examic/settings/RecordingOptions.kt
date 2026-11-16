package io.github.aelgeaer.examic.settings

import android.content.Context
import io.github.aelgeaer.examic.core.TranscribeMode

/** What the user chose for the next recording; the last choice is remembered. */
data class RecordingOptions(val mode: TranscribeMode, val translate: Boolean) {
    companion object {
        private const val PREFS = "recording_options"
        private const val KEY_MODE = "mode"
        private const val KEY_TRANSLATE = "translate"

        fun load(context: Context): RecordingOptions {
            val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            val mode = prefs.getString(KEY_MODE, null)
                ?.let { saved -> TranscribeMode.entries.firstOrNull { it.name == saved } }
                ?: TranscribeMode.AFTER
            return RecordingOptions(mode, prefs.getBoolean(KEY_TRANSLATE, false))
        }

        fun save(context: Context, options: RecordingOptions) {
            context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
                .putString(KEY_MODE, options.mode.name)
                .putBoolean(KEY_TRANSLATE, options.translate)
                .apply()
        }
    }
}
