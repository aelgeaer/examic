import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest
import java.util.Properties
import java.util.zip.ZipInputStream
import javax.inject.Inject
import org.gradle.process.ExecOperations

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

val rustAbis = providers.gradleProperty("examic.abis").get().split(",").map { it.trim() }
/** Whether the speech and translation models go into the APK (about 310 MB). */
val bundleModels = providers.gradleProperty("examic.bundleModels").getOrElse("true").toBoolean()

/**
 * Release signing, from `android/keystore.properties` (never committed) or,
 * on CI, from environment variables. Without either, release builds are
 * unsigned.
 */
val signingProperties = Properties().apply {
    val file = rootProject.file("keystore.properties")
    if (file.exists()) file.inputStream().use(::load)
    System.getenv("EXAMIC_KEYSTORE")?.let { setProperty("storeFile", it) }
    System.getenv("EXAMIC_KEYSTORE_PASSWORD")?.let { setProperty("storePassword", it) }
    System.getenv("EXAMIC_KEY_ALIAS")?.let { setProperty("keyAlias", it) }
    System.getenv("EXAMIC_KEY_PASSWORD")?.let { setProperty("keyPassword", it) }
}

android {
    namespace = "io.github.aelgeaer.examic"
    compileSdk = 36
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "io.github.aelgeaer.examic"
        // 29 is the first release with a predictable foreground-service
        // microphone model; it also lets the manifest skip legacy storage.
        minSdk = 29
        targetSdk = 36
        versionCode = 2
        versionName = "0.1.1"
        ndk { abiFilters += rustAbis }
    }

    signingConfigs {
        if (signingProperties.getProperty("storeFile") != null) {
            create("release") {
                storeFile = rootProject.file(signingProperties.getProperty("storeFile"))
                storePassword = signingProperties.getProperty("storePassword")
                keyAlias = signingProperties.getProperty("keyAlias")
                keyPassword = signingProperties.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            // UniFFI's generated bindings are reached through JNA by name, so
            // shrinking needs keep rules that are not written yet.
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("release")
        }
    }

    // Every model file is stored uncompressed: the big ones then copy out of
    // the APK at disk speed on first launch, and all of them can report their
    // size through AssetManager.openFd(), which fails on compressed assets.
    androidResources { noCompress += listOf("gguf", "onnx", "model", "json", "txt") }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures { compose = true }

    // android.util.Log and friends do nothing in JVM unit tests.
    testOptions { unitTests.isReturnDefaultValues = true }

    // Stored uncompressed and page-aligned, so the loader maps the library
    // straight from the APK (required for 16 KB page devices).
    packaging { jniLibs { useLegacyPackaging = false } }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.02.01"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.10.0")
    implementation("androidx.core:core-ktx:1.18.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")
    // Playback: demuxing, decoding, speed change, audio focus.
    implementation("androidx.media3:media3-exoplayer:1.11.1")
    // Runtime of the UniFFI-generated Kotlin bindings.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    // libonnxruntime.so for the translation model; the Rust core loads it.
    implementation("com.microsoft.onnxruntime:onnxruntime-android:1.32.0")
    // In-app camera for photos taken during a recording.
    implementation("androidx.camera:camera-camera2:1.6.2")
    implementation("androidx.camera:camera-lifecycle:1.6.2")
    implementation("androidx.camera:camera-view:1.6.2")
    implementation("androidx.exifinterface:exifinterface:1.4.2")

    testImplementation("junit:junit:4.13.2")
}

// ---------------------------------------------------------------------------
// Rust core
//
// `cargo ndk` cross-compiles the workspace's examic-ffi crate into a jniLibs
// directory, then uniffi-bindgen reads the compiled library and writes the
// Kotlin API for it. Both outputs are registered with the variant, so a plain
// `./gradlew assembleDebug` builds everything.
//
// Requirements: a Rust toolchain with the Android targets and `cargo-ndk`
// (see the README). If cargo is not on PATH, point to it in local.properties:
//     rust.cargoHome=/path/to/.cargo
//     rust.rustupHome=/path/to/.rustup
// ---------------------------------------------------------------------------

val localProperties = Properties().apply {
    val file = rootProject.file("local.properties")
    if (file.exists()) file.inputStream().use(::load)
}
val cargoHome: String? = localProperties.getProperty("rust.cargoHome")
val rustupHome: String? = localProperties.getProperty("rust.rustupHome")
val isWindows = System.getProperty("os.name").startsWith("Windows")
val workspaceDir = rootProject.layout.projectDirectory.dir("..")

/** Runs cargo with the tools it needs on PATH. */
abstract class CargoTask @Inject constructor(private val execOperations: ExecOperations) : DefaultTask() {
    @get:Internal abstract val workspace: DirectoryProperty
    @get:Internal abstract val cargoExecutable: Property<String>
    @get:Internal abstract val toolEnvironment: MapProperty<String, String>
    @get:Internal abstract val toolPath: ListProperty<String>

    protected fun cargo(vararg arguments: String) {
        execOperations.exec {
            workingDir = workspace.get().asFile
            environment(toolEnvironment.get())
            // Windows spells the variable "Path"; reuse whichever key exists.
            val pathKey = environment.keys.firstOrNull { it.equals("PATH", ignoreCase = true) } ?: "PATH"
            val inherited = environment[pathKey]?.toString().orEmpty()
            environment(pathKey, (toolPath.get() + inherited).joinToString(File.pathSeparator))
            commandLine(listOf(cargoExecutable.get()) + arguments)
        }
    }
}

abstract class CargoNdkBuild @Inject constructor(exec: ExecOperations) : CargoTask(exec) {
    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val sources: ConfigurableFileCollection

    @get:Input abstract val abis: ListProperty<String>
    @get:Input abstract val minSdk: Property<Int>
    @get:OutputDirectory abstract val jniLibs: DirectoryProperty

    @TaskAction
    fun build() {
        val targets = abis.get().flatMap { listOf("-t", it) }.toTypedArray()
        cargo(
            "ndk", *targets,
            "--platform", minSdk.get().toString(),
            "-o", jniLibs.get().asFile.absolutePath,
            "build", "--release", "--locked", "-p", "examic-ffi",
        )
    }
}

abstract class UniffiBindgen @Inject constructor(exec: ExecOperations) : CargoTask(exec) {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val jniLibs: DirectoryProperty

    @get:Input abstract val abi: Property<String>
    @get:OutputDirectory abstract val kotlinSources: DirectoryProperty

    @TaskAction
    fun generate() {
        val library = jniLibs.get().file("${abi.get()}/libexamic_core.so").asFile
        cargo(
            "run", "--quiet", "--locked", "-p", "uniffi-bindgen", "--",
            "generate", "--language", "kotlin", "--no-format",
            "--out-dir", kotlinSources.get().asFile.absolutePath,
            library.absolutePath,
        )
    }
}

// ---------------------------------------------------------------------------
// Models
//
// `fetchModels` downloads the pinned model files (models.properties) into
// android/.model-cache once, verifies their SHA-256 and lays them out as
// assets/models/... for the APK. Builds with -Pexamic.bundleModels=false skip
// it; the app then runs without transcription.
//
// -Pexamic.mtModelDir=<dir> takes the translation model from a local
// convert_opus_mt.py output instead of downloading it.
// ---------------------------------------------------------------------------

abstract class FetchModels : DefaultTask() {
    @get:InputFile
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val pins: RegularFileProperty

    @get:Internal abstract val cache: DirectoryProperty
    @get:Internal abstract val localMtDir: Property<String>
    @get:OutputDirectory abstract val assets: DirectoryProperty

    private fun sha256(file: File): String {
        val digest = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buffer = ByteArray(1 shl 20)
            while (true) {
                val n = input.read(buffer)
                if (n < 0) break
                digest.update(buffer, 0, n)
            }
        }
        return digest.digest().joinToString("") { "%02x".format(it) }
    }

    private fun valid(file: File, sha: String) = file.isFile && sha256(file) == sha

    private fun download(url: String, target: File) {
        logger.lifecycle("Downloading $url")
        target.parentFile.mkdirs()
        val partial = File(target.parentFile, target.name + ".part")
        val client = HttpClient.newBuilder().followRedirects(HttpClient.Redirect.NORMAL).build()
        val response = client.send(HttpRequest.newBuilder(URI(url)).build(), HttpResponse.BodyHandlers.ofFile(partial.toPath()))
        if (response.statusCode() != 200) {
            partial.delete()
            throw GradleException("$url: HTTP ${response.statusCode()}")
        }
        Files.move(partial.toPath(), target.toPath(), StandardCopyOption.REPLACE_EXISTING)
    }

    @TaskAction
    fun fetch() {
        val props = Properties().apply { pins.get().asFile.inputStream().use(::load) }
        val cacheDir = cache.get().asFile
        val out = assets.get().asFile.resolve("models")
        out.deleteRecursively()
        out.mkdirs()

        // Speech recognition: one GGUF file.
        val asrFile = props.getProperty("asr.file")
        val asrSha = props.getProperty("asr.sha256")
        val asrCached = cacheDir.resolve(asrFile)
        if (!valid(asrCached, asrSha)) {
            download(props.getProperty("asr.url"), asrCached)
            if (!valid(asrCached, asrSha)) throw GradleException("$asrFile does not match its pinned SHA-256")
        }

        // Translation: a directory of files, each pinned.
        val mtDirName = props.getProperty("mt.dir")
        val mtFiles = props.getProperty("mt.files").split(",").map { it.trim() }
        val mtCached = cacheDir.resolve(mtDirName)
        fun mtValid() = mtFiles.all { valid(mtCached.resolve(it), props.getProperty("mt.sha256.$it")) }
        if (!mtValid()) {
            mtCached.deleteRecursively()
            mtCached.mkdirs()
            val local = localMtDir.orNull
            if (local != null) {
                mtFiles.forEach { File(local, it).copyTo(mtCached.resolve(it), overwrite = true) }
            } else {
                val zip = cacheDir.resolve("$mtDirName.zip")
                download(props.getProperty("mt.url"), zip)
                ZipInputStream(zip.inputStream()).use { input ->
                    generateSequence { input.nextEntry }.forEach { entry ->
                        val name = entry.name.substringAfterLast('/')
                        if (!entry.isDirectory && name in mtFiles) {
                            mtCached.resolve(name).outputStream().use { input.copyTo(it) }
                        }
                    }
                }
                zip.delete()
            }
            if (!mtValid()) throw GradleException("the translation model does not match its pinned SHA-256 values")
        }

        asrCached.copyTo(out.resolve(asrFile))
        mtFiles.forEach { mtCached.resolve(it).copyTo(out.resolve(mtDirName).resolve(it)) }
    }
}

val fetchModels = tasks.register<FetchModels>("fetchModels") {
    group = "models"
    description = "Downloads and verifies the bundled speech and translation models."
    pins.set(rootProject.layout.projectDirectory.file("models.properties"))
    cache.set(rootProject.layout.projectDirectory.dir(".model-cache"))
    localMtDir.set(providers.gradleProperty("examic.mtModelDir"))
    assets.set(layout.buildDirectory.dir("generated/models"))
}

androidComponents {
    val sdk = sdkComponents
    onVariants { variant ->
        val suffix = variant.name.replaceFirstChar(Char::uppercase)

        fun CargoTask.configureTools() {
            workspace.set(workspaceDir)
            val executable = if (isWindows) "cargo.exe" else "cargo"
            cargoExecutable.set(cargoHome?.let { File(it, "bin/$executable").absolutePath } ?: "cargo")
            toolEnvironment.put("ANDROID_NDK_HOME", sdk.ndkDirectory.map { it.asFile.absolutePath })
            // CMake's own Android support (used for libopus and
            // transcribe.cpp) finds the NDK through this variable.
            toolEnvironment.put("ANDROID_NDK_ROOT", sdk.ndkDirectory.map { it.asFile.absolutePath })
            cargoHome?.let { toolEnvironment.put("CARGO_HOME", it) }
            rustupHome?.let { toolEnvironment.put("RUSTUP_HOME", it) }
            cargoHome?.let { toolPath.add(File(it, "bin").absolutePath) }
            // libopus is built with CMake. The SDK's own CMake and Ninja are
            // used when present, so nothing extra has to be installed.
            val sdkCmake = sdk.sdkDirectory.map { sdkDir ->
                sdkDir.dir("cmake").asFile.listFiles()
                    ?.filter { File(it, "bin").isDirectory }
                    ?.maxByOrNull { it.name }
                    ?.let { listOf(File(it, "bin").absolutePath) }
                    ?: emptyList()
            }
            toolPath.addAll(sdkCmake)
            toolEnvironment.putAll(sdkCmake.map { if (it.isEmpty()) emptyMap() else mapOf("CMAKE_GENERATOR" to "Ninja") })
        }

        val cargoBuild = tasks.register<CargoNdkBuild>("cargoBuild$suffix") {
            group = "rust"
            description = "Cross-compiles the Rust core for ${rustAbis.joinToString()}."
            configureTools()
            sources.from(
                workspaceDir.file("Cargo.toml"),
                workspaceDir.file("Cargo.lock"),
                workspaceDir.dir(".cargo"),
                fileTree(workspaceDir.dir("core")) { exclude("**/target/**") },
            )
            abis.set(rustAbis)
            minSdk.set(variant.minSdk.apiLevel)
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargoBuild, CargoNdkBuild::jniLibs)

        val bindgen = tasks.register<UniffiBindgen>("uniffiBindgen$suffix") {
            group = "rust"
            description = "Generates the Kotlin bindings for the Rust core."
            configureTools()
            jniLibs.set(cargoBuild.flatMap { it.jniLibs })
            abi.set(rustAbis.first())
        }
        variant.sources.kotlin?.addGeneratedSourceDirectory(bindgen, UniffiBindgen::kotlinSources)

        if (bundleModels) {
            variant.sources.assets?.addGeneratedSourceDirectory(fetchModels, FetchModels::assets)
        }
    }
}
