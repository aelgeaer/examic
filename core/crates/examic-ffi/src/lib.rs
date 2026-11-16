//! `libexamic_core.so`: the native half of the Examic Android app.
//!
//! Two doors lead in:
//! * the UniFFI API in this file, for everything the app does a few times a
//!   second or less (start, stop, list, bookmark, transcribe, export);
//! * the two raw JNI functions in [`jni_audio`], for microphone samples and
//!   the live meter.

mod backends;
mod codec;
mod engine;
mod export;
mod jni_audio;
mod transcription;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use backends::ModelPaths;
use engine::{Engine, EngineError};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum ExamicError {
    /// The recording, bookmark or photo does not exist (any more).
    #[error("{0}")]
    NotFound(String),
    /// The call makes no sense right now, e.g. stopping when nothing records.
    #[error("{0}")]
    InvalidState(String),
    /// Disk or database failure.
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    InvalidArgument(String),
}

impl From<EngineError> for ExamicError {
    fn from(e: EngineError) -> Self {
        let message = e.to_string();
        match e {
            EngineError::NotFound => Self::NotFound(message),
            EngineError::AlreadyRecording
            | EngineError::NotRecording
            | EngineError::StillRecording
            | EngineError::Busy
            | EngineError::NoTranscript => Self::InvalidState(message),
            EngineError::InvalidArgument(_) => Self::InvalidArgument(message),
            EngineError::Db(_) | EngineError::Capture(_) | EngineError::Io(_) => {
                Self::Storage(message)
            }
        }
    }
}

type Result<T> = std::result::Result<T, ExamicError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RecordingStatus {
    Recording,
    Ready,
    /// Salvaged after the app died mid-recording; the end may be missing.
    Recovered,
}

/// When a recording is transcribed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TranscribeMode {
    Off,
    /// While recording, one stretch of speech at a time.
    Live,
    /// Once the recording has stopped.
    After,
}

impl From<TranscribeMode> for examic_db::TranscribeMode {
    fn from(m: TranscribeMode) -> Self {
        match m {
            TranscribeMode::Off => Self::Off,
            TranscribeMode::Live => Self::Live,
            TranscribeMode::After => Self::After,
        }
    }
}

impl From<examic_db::TranscribeMode> for TranscribeMode {
    fn from(m: examic_db::TranscribeMode) -> Self {
        match m {
            examic_db::TranscribeMode::Off => Self::Off,
            examic_db::TranscribeMode::Live => Self::Live,
            examic_db::TranscribeMode::After => Self::After,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TranscriptStatus {
    /// Never transcribed.
    None,
    /// Waiting for earlier work to finish.
    Pending,
    Running,
    Done,
    /// Stopped by an error or by the app closing; can be started again.
    Failed,
}

impl From<examic_db::TranscriptStatus> for TranscriptStatus {
    fn from(s: examic_db::TranscriptStatus) -> Self {
        match s {
            examic_db::TranscriptStatus::None => Self::None,
            examic_db::TranscriptStatus::Pending => Self::Pending,
            examic_db::TranscriptStatus::Running => Self::Running,
            examic_db::TranscriptStatus::Done => Self::Done,
            examic_db::TranscriptStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Recording {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub duration_ms: i64,
    pub size_bytes: i64,
    pub status: RecordingStatus,
    pub bookmark_count: i32,
    pub photo_count: i32,
    pub transcribe_mode: TranscribeMode,
    pub translate: bool,
    pub transcript_status: TranscriptStatus,
}

impl From<examic_db::Recording> for Recording {
    fn from(r: examic_db::Recording) -> Self {
        Self {
            id: r.id,
            title: r.title,
            created_at_ms: r.created_at_ms,
            duration_ms: r.duration_ms,
            size_bytes: r.size_bytes,
            status: match r.status {
                examic_db::RecordingStatus::Recording => RecordingStatus::Recording,
                examic_db::RecordingStatus::Ready => RecordingStatus::Ready,
                examic_db::RecordingStatus::Recovered => RecordingStatus::Recovered,
            },
            bookmark_count: r.bookmark_count as i32,
            photo_count: r.photo_count as i32,
            transcribe_mode: r.transcribe_mode.into(),
            translate: r.translate,
            transcript_status: r.transcript_status.into(),
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Bookmark {
    pub id: i64,
    pub timestamp_ms: i64,
    pub note: Option<String>,
}

impl From<examic_db::Bookmark> for Bookmark {
    fn from(b: examic_db::Bookmark) -> Self {
        Self {
            id: b.id,
            timestamp_ms: b.timestamp_ms,
            note: b.note,
        }
    }
}

/// A stretch of speech on a recording's timeline.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct SpeechSpan {
    pub start_ms: i64,
    pub end_ms: i64,
}

/// One transcribed stretch of speech.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TranscriptSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    /// What was said, in Russian.
    pub text: String,
    /// Its English translation, if the recording is translated.
    pub translation: Option<String>,
}

impl From<examic_db::TranscriptSegment> for TranscriptSegment {
    fn from(s: examic_db::TranscriptSegment) -> Self {
        Self {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            text: s.text_ru,
            translation: s.text_en,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Photo {
    pub id: i64,
    pub timestamp_ms: i64,
    /// Path relative to the data directory the engine was opened with.
    pub file: String,
}

impl From<examic_db::Photo> for Photo {
    fn from(p: examic_db::Photo) -> Self {
        Self {
            id: p.id,
            timestamp_ms: p.timestamp_ms,
            file: p.file,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TimelineKind {
    Speech,
    Bookmark,
    Photo,
}

/// One entry of a recording's merged timeline, for export.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TimelineEntry {
    pub kind: TimelineKind,
    pub time_ms: i64,
    /// Speech: the transcript. Bookmark: its note, if any.
    pub text: Option<String>,
    /// Speech: the English translation, if included.
    pub translation: Option<String>,
    /// Photo: path relative to the data directory.
    pub file: Option<String>,
}

impl From<export::Entry> for TimelineEntry {
    fn from(e: export::Entry) -> Self {
        let time_ms = e.time_ms();
        match e {
            export::Entry::Speech {
                text, translation, ..
            } => Self {
                kind: TimelineKind::Speech,
                time_ms,
                text: Some(text),
                translation,
                file: None,
            },
            export::Entry::Bookmark { note, .. } => Self {
                kind: TimelineKind::Bookmark,
                time_ms,
                text: note,
                translation: None,
                file: None,
            },
            export::Entry::Photo { file, .. } => Self {
                kind: TimelineKind::Photo,
                time_ms,
                text: None,
                translation: None,
                file: Some(file),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExportFormat {
    /// A plain `.md` file.
    Markdown,
    /// A `.zip` holding `recording.md` and a `photos/` folder.
    Zip,
}

/// Implemented by the app to hear about transcription progress. Called on
/// the engine's worker threads.
#[uniffi::export(with_foreign)]
pub trait TranscriptListener: Send + Sync {
    /// `progress` is `0.0..=1.0` for work on a finished recording, `None`
    /// during live transcription.
    fn on_transcript_changed(
        &self,
        recording_id: String,
        status: TranscriptStatus,
        progress: Option<f32>,
    );
}

struct ListenerAdapter(Arc<dyn TranscriptListener>);

impl transcription::EngineEvents for ListenerAdapter {
    fn transcript_changed(
        &self,
        id: &str,
        status: examic_db::TranscriptStatus,
        progress: Option<f32>,
    ) {
        self.0
            .on_transcript_changed(id.to_owned(), status.into(), progress);
    }
}

#[derive(uniffi::Object)]
pub struct ExamicEngine {
    engine: Engine,
}

#[uniffi::export]
impl ExamicEngine {
    /// Opens the library stored in `data_dir` (the app's private files
    /// directory) and recovers any recording a crash interrupted.
    #[uniffi::constructor]
    pub fn open(data_dir: String) -> Result<Arc<Self>> {
        init_logging();
        Ok(Arc::new(Self {
            engine: Engine::open(Path::new(&data_dir))?,
        }))
    }

    /// Where the bundled models were unpacked: the GigaAM `.gguf` file and the
    /// Opus-MT directory. Until this is called, transcription fails.
    pub fn set_models(&self, asr_model: Option<String>, mt_dir: Option<String>) {
        self.engine.set_models(ModelPaths {
            asr_model: asr_model.map(PathBuf::from),
            mt_dir: mt_dir.map(PathBuf::from),
        });
    }

    pub fn set_transcript_listener(&self, listener: Option<Arc<dyn TranscriptListener>>) {
        self.engine.set_events(
            listener.map(|l| Arc::new(ListenerAdapter(l)) as Arc<dyn transcription::EngineEvents>),
        );
    }

    /// Starts a recording. Afterwards `NativeAudio.pushPcm` accepts mono
    /// 16-bit samples at `sample_rate` (16000, 32000 or 48000 Hz).
    pub fn start_recording(
        &self,
        title: String,
        sample_rate: u32,
        mode: TranscribeMode,
        translate: bool,
    ) -> Result<Recording> {
        let (recording, input) =
            self.engine
                .start_recording(&title, sample_rate, mode.into(), translate)?;
        jni_audio::attach(input);
        Ok(recording.into())
    }

    /// Stops the recording in progress and returns it in its final state.
    /// In after-recording mode its transcription starts now.
    pub fn stop_recording(&self) -> Result<Recording> {
        jni_audio::detach();
        Ok(self.engine.stop_recording()?.into())
    }

    pub fn active_recording_id(&self) -> Option<String> {
        self.engine.active_recording_id()
    }

    /// All recordings, newest first.
    pub fn recordings(&self) -> Result<Vec<Recording>> {
        Ok(self
            .engine
            .recordings()?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn recording(&self, id: String) -> Result<Recording> {
        Ok(self.engine.recording(&id)?.into())
    }

    pub fn rename_recording(&self, id: String, title: String) -> Result<()> {
        Ok(self.engine.rename_recording(&id, &title)?)
    }

    /// Deletes a recording with its audio, photos, bookmarks and transcript.
    pub fn delete_recording(&self, id: String) -> Result<()> {
        Ok(self.engine.delete_recording(&id)?)
    }

    /// Adds a bookmark. With `timestamp_ms` omitted it marks "now" in the
    /// recording in progress.
    pub fn add_bookmark(
        &self,
        recording_id: String,
        timestamp_ms: Option<i64>,
        note: Option<String>,
    ) -> Result<Bookmark> {
        Ok(self
            .engine
            .add_bookmark(&recording_id, timestamp_ms, note.as_deref())?
            .into())
    }

    /// Bookmarks of a recording in timeline order.
    pub fn bookmarks(&self, recording_id: String) -> Result<Vec<Bookmark>> {
        Ok(self
            .engine
            .bookmarks(&recording_id)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn delete_bookmark(&self, bookmark_id: i64) -> Result<()> {
        Ok(self.engine.delete_bookmark(bookmark_id)?)
    }

    /// Where speech occurs in a recording, in timeline order.
    pub fn speech_spans(&self, recording_id: String) -> Result<Vec<SpeechSpan>> {
        Ok(self
            .engine
            .speech_intervals(&recording_id)?
            .into_iter()
            .map(|(start_ms, end_ms)| SpeechSpan { start_ms, end_ms })
            .collect())
    }

    /// Absolute path of a finished recording's Ogg Opus file, for playback.
    pub fn audio_path(&self, recording_id: String) -> Result<String> {
        Ok(self
            .engine
            .audio_path(&recording_id)?
            .to_string_lossy()
            .into_owned())
    }

    /// Queues a finished recording for transcription (replacing any earlier
    /// transcript), optionally translated into English.
    pub fn transcribe_recording(&self, recording_id: String, translate: bool) -> Result<()> {
        Ok(self.engine.transcribe_recording(&recording_id, translate)?)
    }

    /// Queues translation of a transcript's untranslated segments.
    pub fn translate_recording(&self, recording_id: String) -> Result<()> {
        Ok(self.engine.translate_recording(&recording_id)?)
    }

    /// A recording's transcript in timeline order (also while it grows).
    pub fn transcript(&self, recording_id: String) -> Result<Vec<TranscriptSegment>> {
        Ok(self
            .engine
            .transcript(&recording_id)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Progress (`0.0..=1.0`) of work on a finished recording while it runs.
    pub fn transcript_progress(&self, recording_id: String) -> Option<f32> {
        self.engine.transcript_progress(&recording_id)
    }

    /// The folder, relative to the data directory, to save a recording's
    /// photos in. It exists when this returns.
    pub fn photo_dir(&self, recording_id: String) -> Result<String> {
        Ok(self.engine.photo_dir(&recording_id)?)
    }

    /// Registers a photo saved at `file` (relative, inside `photo_dir`).
    /// With `timestamp_ms` omitted it is placed at "now" in the recording in
    /// progress.
    pub fn add_photo(
        &self,
        recording_id: String,
        file: String,
        timestamp_ms: Option<i64>,
    ) -> Result<Photo> {
        Ok(self
            .engine
            .add_photo(&recording_id, &file, timestamp_ms)?
            .into())
    }

    /// Photos of a recording in timeline order.
    pub fn photos(&self, recording_id: String) -> Result<Vec<Photo>> {
        Ok(self
            .engine
            .photos(&recording_id)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn delete_photo(&self, photo_id: i64) -> Result<()> {
        Ok(self.engine.delete_photo(photo_id)?)
    }

    /// Transcript, bookmarks and photos merged in timeline order.
    pub fn export_timeline(
        &self,
        recording_id: String,
        include_translation: bool,
    ) -> Result<Vec<TimelineEntry>> {
        Ok(self
            .engine
            .timeline(&recording_id, include_translation)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Writes the recording as Markdown to `dest_path`; a `.zip` with its
    /// photos when it has any. `date` is printed as given.
    pub fn export_markdown(
        &self,
        recording_id: String,
        include_translation: bool,
        date: String,
        dest_path: String,
    ) -> Result<ExportFormat> {
        let format = self.engine.export_markdown(
            &recording_id,
            include_translation,
            &date,
            Path::new(&dest_path),
        )?;
        Ok(match format {
            export::ExportFormat::Markdown => ExportFormat::Markdown,
            export::ExportFormat::Zip => ExportFormat::Zip,
        })
    }
}

/// Routes the `log` crate to logcat under the tag `examic`.
fn init_logging() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("examic"),
    );
}
