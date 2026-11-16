# Examic

An offline voice recorder for Android with on-device Russian transcription and
Russian-to-English translation, built on a Rust core. Nothing it records
leaves the device unless you export it: the app has no network permission,
and speech recognition and translation run on the phone.

## What works

- **Recording** in a foreground service that keeps going with the screen off.
  48 kHz mono from the microphone's unprocessed source, encoded to Opus
  (32 kbit/s, about 14 MB per hour) and stored as a plain `.opus` file.
- **Transcription** of Russian speech with GigaAM v3 (end-to-end CTC, with
  punctuation and capitals) through transcribe.cpp. Before each recording,
  choose **Live** (text appears while you record), **After** (the recording is
  transcribed once you stop) or **Off** (transcribe later with *Transcribe
  now*). The transcript is split into sentences, each tied to its moment;
  tapping one seeks the player there.
- **Translation** into English with Helsinki-NLP's Opus-MT, sentence by
  sentence, live or after recording, or later with *Translate*.
- **Photos** during a recording with the in-app camera, pinned to the moment
  the shutter was pressed. The player shows them as a strip and as marks on
  the timeline; tap one to jump there, tap again to view it.
- **Export** to Markdown (a `.zip` with the photos when there are any) or PDF,
  with or without the translation: transcript, bookmarks and photos merged in
  time order. Save anywhere through the system file picker, or share.
- **Live view**: clock, level waveform, and speech detection drawn in a second
  colour as it happens. Pause and resume.
- **Bookmarks** during recording (one-tap highlight, or a note pinned to the
  moment the button was pressed) and afterwards in the player.
- **Player**: seek bar with a speech/bookmark/photo timeline above it, ±10 s,
  speeds from 0.5× to 3× with pitch preserved, and *skip silence*.
- **Crash recovery**: if the app is killed mid-recording, everything up to the
  last second is kept and the recording reappears as *Recovered*; its
  transcription can be started again.

Measured on a desktop CPU (4 threads) with FLEURS Russian: 7.7 % word error
rate after recording, 9.0 % live, transcription at about 14× real time, and
the int8 translation model matches PyTorch's output on 19 of 20 test
sentences. Phone figures depend on the CPU; see *Verifying on a device*.

## Layout

```text
core/crates/
  examic-audio  lock-free capture pipeline, decimator, VAD, Ogg Opus writer
  examic-db     library schema and queries (SQLite)
  examic-asr    speech recognition: GigaAM via transcribe.cpp, windowing, segmentation
  examic-mt     translation: Opus-MT on ONNX Runtime, pure-Rust SentencePiece
  examic-ffi    libexamic_core.so: engine, transcription workers, export, UniFFI API
core/tools/uniffi-bindgen   generates the Kotlin bindings
tools/models/convert_opus_mt.py   builds the translation model bundle
android/            Jetpack Compose app
```

```text
AudioRecord thread ──JNI──▶ lock-free ring ──▶ capture worker
                                               ├─ level meter ─▶ telemetry (read per frame)
                                               ├─ Opus ─▶ Ogg ─▶ recordings/<id>.opus (fsync every 10 s)
                                               └─ 48→16 kHz ─▶ VAD ─▶ speech intervals ─▶ SQLite
                                                                  └─▶ live chunker ─▶ live worker ─┐
After recording: job queue ─ decode .opus at 16 kHz, window by speech ─────────────────────────────┤
                                                                                                   ▼
                                            GigaAM (transcribe.cpp) ─▶ sentences ─▶ Opus-MT (ONNX Runtime) ─▶ SQLite
Compose UI ──UniFFI──▶ engine (record, library, bookmarks, photos, transcripts, export)
Media3 ExoPlayer ──▶ the .opus file
```

Both models are bundled in the APK and unpacked to the app's files on first
start (transcribe.cpp memory-maps its model from a real path). Model work runs
on low-priority threads, so it never makes the microphone thread late.

## Building

Requirements: Android SDK with NDK 28.2 and CMake 3.22 (both installable from
the SDK Manager), JDK 17+ (Android Studio's bundled JBR works), and Rust:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
```

Then:

```sh
cd android
./gradlew assembleDebug        # builds the Rust core, bindings and APK
```

The first build downloads the models (about 310 MB, pinned by SHA-256 in
`android/models.properties`) into `android/.model-cache`. Pass
`-Pexamic.bundleModels=false` to build without them; the app then records but
does not transcribe.

If `cargo` is not on `PATH` (e.g. Rust installed in a custom location), add to
`android/local.properties`:

```properties
rust.cargoHome=/path/to/.cargo
rust.rustupHome=/path/to/.rustup
```

By default the core is built for `arm64-v8a` (phones) and `x86_64` (emulator);
pass `-Pexamic.abis=arm64-v8a` to build only for devices.

### Release builds

Create a signing key once and keep it safe, outside the repository: losing it
means existing installs can no longer be updated.

```sh
keytool -genkeypair -v -keystore examic-release.jks -alias examic \
    -keyalg RSA -keysize 4096 -validity 10000
```

Point `android/keystore.properties` (ignored by git) at it:

```properties
storeFile=/path/to/examic-release.jks
storePassword=...
keyAlias=examic
keyPassword=...
```

```sh
./gradlew -Pexamic.abis=arm64-v8a assembleRelease
# -> app/build/outputs/apk/release/app-release.apk (about 395 MB)
```

Pushing a `v*` tag runs `.github/workflows/release.yml`, which does the same
with the key from repository secrets (`EXAMIC_KEYSTORE_BASE64`,
`EXAMIC_KEYSTORE_PASSWORD`, `EXAMIC_KEY_ALIAS`, `EXAMIC_KEY_PASSWORD`) and
publishes the APK as a GitHub Release.

### The translation model

The Opus-MT bundle is built from the Hugging Face checkpoint by
`tools/models/convert_opus_mt.py`: ONNX export, dynamic int8 quantization,
then a check that the result translates 20 sentences as PyTorch does. App
builds download the result from this repository's `models-v1` release, so they
need no Python. To rebuild it:

```sh
python -m venv .venv && .venv/bin/pip install -r tools/models/requirements.txt
.venv/bin/python tools/models/convert_opus_mt.py --out build/opus-mt-ru-en \
    --fixtures core/crates/examic-mt/tests/fixtures
./gradlew -Pexamic.mtModelDir=$PWD/build/opus-mt-ru-en assembleDebug
```

## Tests

```sh
cargo test --workspace                  # Rust, on the host
cd android && ./gradlew testDebugUnitTest
```

The Rust tests run on any desktop OS. Off Android, the engine uses stand-ins
for the Opus codec and the models, so `cargo test` needs no CMake. With the
real codec and models it checks the whole pipeline, live and after recording,
on Russian speech from FLEURS:

```sh
EXAMIC_ASR_MODEL=android/.model-cache/gigaam-v3-e2e-ctc-Q8_0.gguf \
EXAMIC_MT_MODEL_DIR=android/.model-cache/opus-mt-ru-en \
ORT_DYLIB_PATH=/path/to/libonnxruntime.so \
EXAMIC_FLEURS_DIR=/path/to/fleurs/ru_ru \
cargo test --release -p examic-ffi --features native -- --ignored --nocapture
```

`EXAMIC_MT_MODEL_DIR` (and `ORT_DYLIB_PATH`) also enable the tokenizer and
translation checks in `cargo test -p examic-mt --features native`, and
`cargo run --release -p examic-asr --features native --example transcribe`
measures the word error rate and speed of speech recognition alone.

### Verifying on a device

On an arm64 phone (`./gradlew installDebug`, or the release APK):

- Record two minutes of Russian in each mode (Off, Live, After). Live shows
  text during the recording without audio dropouts; After produces a
  transcript once stopped; Off offers *Transcribe now*.
- Turn on translation and check the English lines.
- Take two photos mid-recording and check their places on the timeline.
- Export Markdown (a zip with photos) and PDF; open both and check the
  Cyrillic text and the order of the photos.
- Force-stop the app during a recording and check that it comes back as
  *Recovered*.
- After a fresh install, check that the models unpack on first start and that
  everything works in airplane mode.

## Design notes

**Plain files, private storage.** Recordings are standard Ogg Opus files and
the library is a plain SQLite database, both in the app's private storage,
which Android already isolates per app and encrypts at rest on every device
with a lock screen. A crash leaves at most the last second of audio unwritten:
Ogg pages carry their own length and checksum, so recovery reads up to the
last complete page.

**Transcription follows the voice detector.** The speech intervals found while
recording decide what the model hears: after recording they are grouped into
windows of up to 30 s, padded by 200 ms, and silence is never transcribed.
Live, intervals that follow closely are merged until a pause of 1.2 s (or
15 s of speech), because the detector also pauses inside sentences and the
model does better on whole ones. GigaAM reports only token times; segments are
cut at sentence ends and long pauses.

**Translation sentence by sentence.** The tiny Marian model was trained on
single sentences and drops one when given two, so text is split first.

**A SentencePiece tokenizer in Rust.** The C++ sentencepiece library would be
one more CMake build to cross-compile; the unigram model is small enough to
implement directly, and its output is tested against the Python tokenizer.

**ONNX Runtime is loaded, not linked.** The `onnxruntime-android` AAR provides
`libonnxruntime.so` and the Rust core opens it at run time, so no ONNX Runtime
build is needed and the version is the AAR's.

**No `+fp16,+dotprod` in the global Rust flags.** Those ARMv8.2 extensions
would make the library crash with an illegal-instruction fault on the
ARMv8.0 phones that `minSdk 29` still admits. ggml is built for the ARMv8.0
baseline for the same reason.

**`opt-level = 3`, `panic = "unwind"`.** `opt-level = "z"` would disable loop
vectorisation in the DSP loops to save a few hundred KB. And with
`panic = "abort"` a bug anywhere in the core would kill the process and with
it the recording in progress; with unwinding, UniFFI turns a panic into a
Kotlin exception.

**Playback goes through Media3.** Ogg demuxing, Opus decoding,
pitch-preserving speed change, audio focus and routing, all reading the
`.opus` file directly.

**Integer-ratio decimation instead of a general resampler.** The capture rates
used are exact multiples of 16 kHz, so a 96-tap Kaiser-windowed FIR (flat to
7 kHz, about 65 dB of alias rejection) feeds both the VAD and live
transcription. For transcription after recording, libopus decodes straight to
16 kHz.

**Telemetry is one packed `Long`**: level, speech probability, state flags and
the recording position, so the recorder screen needs one native call per frame.

**Bookmarks and photos are timestamped on the recording's timeline**, counted
in samples actually captured. Wall-clock time would drift as soon as the
recording is paused.

## Roadmap

1. Speaker diarization and naming.
2. Silero VAD behind the existing `Vad` trait.
3. Summaries and search over the library (local LLM, FTS5 + vectors).
4. SRT export and audio export.
5. A smaller ONNX Runtime build with only the operators the translation model
   uses (the stock library is 36 MB).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option. The bundled models and native libraries are under their own licenses;
see [THIRD_PARTY_NOTICES.md](android/app/src/main/assets/THIRD_PARTY_NOTICES.md).
