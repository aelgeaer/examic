package io.github.aelgeaer.examic.models

import java.io.File
import java.io.IOException
import java.io.InputStream
import java.nio.file.Files
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ModelInstallerTest {
    private val dir: File = Files.createTempDirectory("models-test").toFile()

    @After
    fun cleanUp() {
        dir.deleteRecursively()
    }

    /**
     * Assets as the release APK holds them: the models stored uncompressed,
     * the small JSON and text files compressed (no size without reading).
     */
    private class FakeAssets(val files: Map<String, ByteArray>, val compressed: Set<String>) : AssetSource {
        var failOn: String? = null

        override fun list(path: String): List<String> =
            files.keys.filter { it.startsWith("$path/") }
                .map { it.removePrefix("$path/").substringBefore('/') }
                .distinct()

        override fun open(path: String): InputStream {
            if (path == failOn) throw IOException("disk full")
            return files.getValue(path).inputStream()
        }

        override fun storedLength(path: String): Long? =
            if (path in compressed) null else files.getValue(path).size.toLong()
    }

    private val bundle = mapOf(
        "models/gigaam-v3-e2e-ctc-Q8_0.gguf" to ByteArray(3_000_000) { it.toByte() },
        "models/opus-mt-ru-en/encoder.onnx" to ByteArray(500_000) { (it * 7).toByte() },
        "models/opus-mt-ru-en/config.json" to "{\"d_model\": 256}".toByteArray(),
        "models/opus-mt-ru-en/vocab.json" to "{\"</s>\": 0}".toByteArray(),
    )
    private val compressed = setOf("models/opus-mt-ru-en/config.json", "models/opus-mt-ru-en/vocab.json")

    @Test
    fun installsStoredAndCompressedAssets() {
        val assets = FakeAssets(bundle, compressed)
        val installer = ModelInstaller(assets, File(dir, "models")) { "installed-1" }
        val state = installer.install()
        assertTrue(state.toString(), state is ModelState.Ready)
        for ((path, bytes) in bundle) {
            val copied = File(dir, path)
            assertTrue(path, copied.readBytes().contentEquals(bytes))
        }
        assertEquals(File(dir, "models/opus-mt-ru-en"), (state as ModelState.Ready).mtDir)

        // Installed once per version: no asset is read again.
        assets.failOn = "models/gigaam-v3-e2e-ctc-Q8_0.gguf"
        assertTrue(installer.install() is ModelState.Ready)
    }

    @Test
    fun aFailedInstallIsRetriedOnTheNextStart() {
        val assets = FakeAssets(bundle, compressed)
        assets.failOn = "models/opus-mt-ru-en/encoder.onnx"
        val first = ModelInstaller(assets, File(dir, "models")) { "installed-1" }.install()
        assertTrue(first.toString(), first is ModelState.Failed)

        assets.failOn = null
        val second = ModelInstaller(assets, File(dir, "models")) { "installed-1" }.install()
        assertTrue(second.toString(), second is ModelState.Ready)
    }

    @Test
    fun aBuildWithoutModelsSaysSo() {
        val assets = FakeAssets(mapOf("models/README" to ByteArray(1)), emptySet())
        assertEquals(ModelState.NotBundled, ModelInstaller(assets, File(dir, "models")) { "x" }.install())
    }
}
