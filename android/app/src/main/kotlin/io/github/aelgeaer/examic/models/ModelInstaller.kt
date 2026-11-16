package io.github.aelgeaer.examic.models

import android.content.Context
import android.content.res.AssetManager
import android.util.Log
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

sealed interface ModelState {
    /** Not looked at yet. */
    data object Checking : ModelState

    /** Unpacking the models from the APK; [progress] runs from 0 to 1. */
    data class Installing(val progress: Float) : ModelState

    data class Ready(val asrModel: File, val mtDir: File) : ModelState

    /** This build carries no models (built with -Pexamic.bundleModels=false). */
    data object NotBundled : ModelState

    data class Failed(val message: String) : ModelState
}

/** The APK's assets, as far as the installer needs them; replaced in tests. */
interface AssetSource {
    /** Names directly under [path]; empty for a file. */
    fun list(path: String): List<String>

    fun open(path: String): InputStream

    /**
     * The size of [path] if it is stored uncompressed, or `null`. Compressed
     * assets cannot report a size without being read.
     */
    fun storedLength(path: String): Long?
}

private class ApkAssets(private val assets: AssetManager) : AssetSource {
    override fun list(path: String): List<String> = assets.list(path).orEmpty().toList()

    override fun open(path: String): InputStream = assets.open(path)

    override fun storedLength(path: String): Long? =
        try {
            // openFd works only on uncompressed assets (noCompress in build.gradle.kts).
            assets.openFd(path).use { it.length }
        } catch (e: IOException) {
            null
        }
}

/**
 * Copies the bundled models out of the APK into the app's files directory.
 *
 * transcribe.cpp memory-maps its model from a real file path, which an asset
 * inside the APK does not have; ONNX Runtime is given files for the same
 * reason. The copy happens once per app version and is stamped when complete,
 * so an interrupted or failed copy is simply redone on the next start.
 */
class ModelInstaller internal constructor(
    private val assets: AssetSource,
    private val root: File,
    private val stampName: () -> String,
) {
    constructor(context: Context) : this(
        ApkAssets(context.assets),
        File(context.filesDir, "models"),
        {
            val info = context.packageManager.getPackageInfo(context.packageName, 0)
            "installed-${info.longVersionCode}-${info.lastUpdateTime}"
        },
    )

    private val mutableState = MutableStateFlow<ModelState>(ModelState.Checking)
    val state: StateFlow<ModelState> = mutableState

    /** Assets under `models/`, recursively, as paths relative to it. */
    private fun bundledFiles(dir: String = ""): List<String> {
        val path = if (dir.isEmpty()) ASSET_ROOT else "$ASSET_ROOT/$dir"
        return assets.list(path).flatMap { name ->
            val relative = if (dir.isEmpty()) name else "$dir/$name"
            if (assets.list("$ASSET_ROOT/$relative").isEmpty()) listOf(relative) else bundledFiles(relative)
        }
    }

    private fun ready(): ModelState.Ready =
        ModelState.Ready(File(root, ASR_MODEL), File(root, MT_DIR))

    /** Installs the models if needed and returns the outcome. Call off the main thread. */
    fun install(): ModelState {
        val result = try {
            installOrSkip()
        } catch (e: IOException) {
            Log.e("examic", "model install failed", e)
            ModelState.Failed(e.message ?: e.javaClass.simpleName)
        }
        mutableState.value = result
        return result
    }

    private fun installOrSkip(): ModelState {
        val stamp = File(root, stampName())
        if (stamp.exists()) return ready()

        val files = bundledFiles()
        if (ASR_MODEL !in files) return ModelState.NotBundled

        // The big files are stored uncompressed and report their size, which
        // is all the progress bar needs; small compressed ones count as 0.
        val total = files.sumOf { assets.storedLength("$ASSET_ROOT/$it") ?: 0L }.coerceAtLeast(1)
        var copied = 0L
        mutableState.value = ModelState.Installing(0f)

        root.deleteRecursively()
        root.mkdirs()
        val buffer = ByteArray(1 shl 20)
        for (name in files) {
            val target = File(root, name)
            target.parentFile?.mkdirs()
            val partial = File(target.parentFile, target.name + ".part")
            assets.open("$ASSET_ROOT/$name").use { input ->
                FileOutputStream(partial).use { output ->
                    while (true) {
                        val n = input.read(buffer)
                        if (n < 0) break
                        output.write(buffer, 0, n)
                        copied += n
                        mutableState.value = ModelState.Installing((copied.toFloat() / total).coerceAtMost(1f))
                    }
                    output.fd.sync()
                }
            }
            if (!partial.renameTo(target)) throw IOException("could not install $name")
        }
        stamp.createNewFile()
        return ready()
    }

    private companion object {
        const val ASSET_ROOT = "models"
        const val ASR_MODEL = "gigaam-v3-e2e-ctc-Q8_0.gguf"
        const val MT_DIR = "opus-mt-ru-en"
    }
}
