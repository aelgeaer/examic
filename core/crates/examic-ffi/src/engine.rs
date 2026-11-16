//! The engine behind the FFI surface: owns the library database, the
//! recordings directory, the capture session in progress and the
//! transcription workers.
//!
//! Plain Rust with no JVM types, so all of it is tested on the host.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use examic_audio::{
    AudioSink, CaptureError, CaptureSession, DEFAULT_RING_MS, EnergyVad, PcmInput, SpeechListener,
    stream_duration_ms,
};
use examic_db::{
    Bookmark, Database, DbError, Photo, Recording, RecordingStatus, TranscribeMode,
    TranscriptSegment, TranscriptStatus,
};

use crate::backends::{Backends, ModelBackends, ModelPaths};
use crate::codec;
use crate::export::{self, Entry, ExportFormat};
use crate::transcription::{self, EngineEvents, Job, Jobs};

const DATABASE_FILE: &str = "examic.db";
const RECORDINGS_DIR: &str = "recordings";
/// Photos live in `photos/<recording id>/`, relative to the data directory.
pub const PHOTOS_DIR: &str = "photos";
const AUDIO_EXTENSION: &str = "opus";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("a recording is already in progress")]
    AlreadyRecording,
    #[error("no recording is in progress")]
    NotRecording,
    #[error("the recording is still in progress")]
    StillRecording,
    #[error("the recording is already being transcribed")]
    Busy,
    #[error("the recording has no transcript to translate")]
    NoTranscript,
    #[error("not found")]
    NotFound,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error(transparent)]
    Db(DbError),
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

impl From<DbError> for EngineError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => Self::NotFound,
            other => Self::Db(other),
        }
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// What the engine and its worker threads share.
pub(crate) struct Shared {
    pub db: Database,
    pub backends: Arc<dyn Backends>,
    pub recordings_dir: PathBuf,
    data_dir: PathBuf,
    events: RwLock<Option<Arc<dyn EngineEvents>>>,
    progress: Mutex<HashMap<String, f32>>,
}

impl Shared {
    pub fn notify(&self, id: &str, status: TranscriptStatus, progress: Option<f32>) {
        let events = self
            .events
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(events) = events {
            events.transcript_changed(id, status, progress);
        }
    }

    fn progress(&self) -> MutexGuard<'_, HashMap<String, f32>> {
        self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_progress(&self, id: &str, value: f32) {
        self.progress().insert(id.to_owned(), value);
    }

    pub fn clear_progress(&self, id: &str) {
        self.progress().remove(id);
    }
}

struct Active {
    id: String,
    session: CaptureSession,
}

pub struct Engine {
    shared: Arc<Shared>,
    /// The real model loader, when the engine uses it (not in tests).
    models: Option<Arc<ModelBackends>>,
    jobs: Jobs,
    active: Mutex<Option<Active>>,
}

impl Engine {
    /// Opens the library in `data_dir` and salvages any recording that was
    /// cut short by the app dying. Models are found once
    /// [`set_models`](Self::set_models) says where they are.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let models = Arc::new(ModelBackends::default());
        let mut engine = Self::open_with(data_dir, models.clone())?;
        engine.models = Some(models);
        Ok(engine)
    }

    /// Opens with other model backends; the tests use this.
    pub(crate) fn open_with(data_dir: &Path, backends: Arc<dyn Backends>) -> Result<Self> {
        let recordings_dir = data_dir.join(RECORDINGS_DIR);
        fs::create_dir_all(&recordings_dir)?;
        fs::create_dir_all(data_dir.join(PHOTOS_DIR))?;
        let db = Database::open(&data_dir.join(DATABASE_FILE))?;
        let shared = Arc::new(Shared {
            db,
            backends,
            recordings_dir,
            data_dir: data_dir.to_owned(),
            events: RwLock::new(None),
            progress: Mutex::new(HashMap::new()),
        });
        let engine = Self {
            jobs: Jobs::start(shared.clone())?,
            shared,
            models: None,
            active: Mutex::new(None),
        };
        engine.recover_interrupted()?;
        // Work a previous run did not finish can be started again by hand.
        engine.shared.db.fail_unfinished_transcripts()?;
        Ok(engine)
    }

    fn db(&self) -> &Database {
        &self.shared.db
    }

    fn active(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn audio_path_of(&self, recording: &Recording) -> PathBuf {
        self.shared.recordings_dir.join(&recording.audio_file)
    }

    /// Tells the engine where the bundled models were unpacked.
    pub fn set_models(&self, paths: ModelPaths) {
        if let Some(models) = &self.models {
            models.set_paths(paths);
        }
    }

    pub fn set_events(&self, events: Option<Arc<dyn EngineEvents>>) {
        *self
            .shared
            .events
            .write()
            .unwrap_or_else(|e| e.into_inner()) = events;
    }

    pub fn active_recording_id(&self) -> Option<String> {
        self.active().as_ref().map(|a| a.id.clone())
    }

    /// Starts capturing. The returned [`PcmInput`] is where the audio thread
    /// delivers samples at `sample_rate`.
    pub fn start_recording(
        &self,
        title: &str,
        sample_rate: u32,
        mode: TranscribeMode,
        translate: bool,
    ) -> Result<(Recording, PcmInput)> {
        let mut active = self.active();
        if active.is_some() {
            return Err(EngineError::AlreadyRecording);
        }
        let id = uuid::Uuid::now_v7().to_string();
        let audio_file = format!("{id}.{AUDIO_EXTENSION}");
        let path = self.shared.recordings_dir.join(&audio_file);

        // The row goes in first: if the app dies right after, recovery finds a
        // row without audio and drops it. The other order could leave an audio
        // file nothing refers to.
        self.db().create_recording(
            &id,
            title,
            now_ms(),
            sample_rate,
            &audio_file,
            mode,
            translate,
        )?;
        match self.begin_capture(&id, &path, sample_rate, mode, translate) {
            Ok((input, session)) => {
                *active = Some(Active {
                    id: id.clone(),
                    session,
                });
                Ok((self.db().recording(&id)?, input))
            }
            Err(e) => {
                let _ = fs::remove_file(&path);
                let _ = self.db().delete_recording(&id);
                Err(e)
            }
        }
    }

    fn begin_capture(
        &self,
        id: &str,
        path: &Path,
        sample_rate: u32,
        mode: TranscribeMode,
        translate: bool,
    ) -> Result<(PcmInput, CaptureSession)> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let sink = FileSink(BufWriter::new(file));
        let encoder = codec::new_encoder(sample_rate).map_err(CaptureError::from)?;
        let listener: Option<Box<dyn SpeechListener>> = match mode {
            TranscribeMode::Live => Some(Box::new(transcription::start_live(
                self.shared.clone(),
                id.to_owned(),
                translate,
            )?)),
            _ => None,
        };
        Ok(CaptureSession::start_with_listener(
            sample_rate,
            DEFAULT_RING_MS,
            encoder,
            sink,
            Box::new(EnergyVad::new()),
            listener,
        )?)
    }

    /// Finishes the recording in progress. Call after the audio thread has
    /// delivered its last samples.
    ///
    /// If capture failed part-way (a full disk, typically) whatever reached
    /// the file is kept and the recording comes back as
    /// [`RecordingStatus::Recovered`]; the error is returned only when nothing
    /// could be kept.
    pub fn stop_recording(&self) -> Result<Recording> {
        let Active { id, session } = self.active().take().ok_or(EngineError::NotRecording)?;
        let recording = self.db().recording(&id)?;
        match session.stop() {
            Ok(report) => {
                let speech: Vec<(i64, i64)> = report
                    .speech
                    .iter()
                    .map(|s| (s.start_ms as i64, s.end_ms as i64))
                    .collect();
                let size = fs::metadata(self.audio_path_of(&recording))?.len();
                self.db().finish_recording(
                    &id,
                    RecordingStatus::Ready,
                    report.duration_ms as i64,
                    size as i64,
                    &speech,
                )?;
            }
            Err(e) => {
                log::error!("capture of {id} failed: {e}");
                if !self.salvage(&recording)? {
                    return Err(e.into());
                }
            }
        }
        if recording.transcribe_mode == TranscribeMode::After {
            self.jobs.submit(Job::Transcribe(id.clone()));
        }
        Ok(self.db().recording(&id)?)
    }

    /// Finishes off recordings left in the `Recording` state by a previous
    /// run. Returns how many were kept.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let active_id = self.active_recording_id();
        let mut kept = 0;
        for recording in self.db().unfinished_recordings()? {
            if Some(&recording.id) == active_id.as_ref() {
                continue;
            }
            match self.salvage(&recording) {
                Ok(true) => kept += 1,
                Ok(false) => log::info!("dropped empty recording {}", recording.id),
                Err(e) => log::error!("could not recover {}: {e}", recording.id),
            }
        }
        Ok(kept)
    }

    /// Turns an unsealed audio file into a finished recording.
    ///
    /// Ogg pages carry their own length and checksum, so a file cut off
    /// mid-page still reads up to its last complete page.
    ///
    /// Returns `false`, having removed the recording, only when the file
    /// provably holds no audio. A file that cannot be read for any other
    /// reason is kept and listed with a zero duration: deleting someone's
    /// recording must never be the response to an error.
    fn salvage(&self, recording: &Recording) -> Result<bool> {
        let path = self.audio_path_of(recording);
        let duration_ms = match File::open(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                log::error!("cannot open {}: {e}", recording.audio_file);
                Some(0)
            }
            Ok(mut file) => match stream_duration_ms(&mut file) {
                Ok(Some(duration)) => Some(duration),
                // Shorter than its headers: the app died while creating it.
                Ok(None) if file.metadata().map_or(0, |m| m.len()) < 4096 => None,
                Ok(None) => {
                    log::error!("{} holds no readable audio", recording.audio_file);
                    Some(0)
                }
                Err(e) => {
                    log::error!("cannot scan {}: {e}", recording.audio_file);
                    Some(0)
                }
            },
        };
        match duration_ms {
            Some(duration_ms) => {
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.db().finish_recording(
                    &recording.id,
                    RecordingStatus::Recovered,
                    duration_ms as i64,
                    size as i64,
                    &[],
                )?;
                Ok(true)
            }
            None => {
                remove_if_present(&path)?;
                self.db().delete_recording(&recording.id)?;
                Ok(false)
            }
        }
    }

    pub fn recordings(&self) -> Result<Vec<Recording>> {
        Ok(self.db().recordings()?)
    }

    pub fn recording(&self, id: &str) -> Result<Recording> {
        Ok(self.db().recording(id)?)
    }

    pub fn rename_recording(&self, id: &str, title: &str) -> Result<()> {
        Ok(self.db().rename_recording(id, title)?)
    }

    pub fn delete_recording(&self, id: &str) -> Result<()> {
        self.ensure_not_active(id)?;
        let recording = self.db().recording(id)?;
        let photos = self.db().photos(id)?;
        // Files first: if removing one fails the recording stays listed and
        // can be deleted again, instead of leaving files nothing refers to.
        remove_if_present(&self.audio_path_of(&recording))?;
        for photo in &photos {
            remove_if_present(&self.shared.data_dir.join(&photo.file))?;
        }
        let _ = fs::remove_dir(self.shared.data_dir.join(PHOTOS_DIR).join(id));
        self.db().delete_recording(id)?;
        self.shared.clear_progress(id);
        Ok(())
    }

    /// The playback position of the recording in progress, if it is `id`.
    fn live_position_ms(&self, id: &str) -> Result<i64> {
        match self.active().as_ref() {
            Some(a) if a.id == id => Ok(a.session.telemetry().position_ms() as i64),
            _ => Err(EngineError::NotRecording),
        }
    }

    /// Adds a bookmark. Without `timestamp_ms` it is placed at the current
    /// position of the recording in progress, which must be `recording_id`.
    pub fn add_bookmark(
        &self,
        recording_id: &str,
        timestamp_ms: Option<i64>,
        note: Option<&str>,
    ) -> Result<Bookmark> {
        let timestamp_ms = match timestamp_ms {
            Some(ms) => ms.max(0),
            None => self.live_position_ms(recording_id)?,
        };
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        let id = self.db().add_bookmark(recording_id, timestamp_ms, note)?;
        Ok(Bookmark {
            id,
            recording_id: recording_id.to_owned(),
            timestamp_ms,
            note: note.map(str::to_owned),
        })
    }

    pub fn bookmarks(&self, recording_id: &str) -> Result<Vec<Bookmark>> {
        Ok(self.db().bookmarks(recording_id)?)
    }

    pub fn delete_bookmark(&self, id: i64) -> Result<()> {
        Ok(self.db().delete_bookmark(id)?)
    }

    pub fn speech_intervals(&self, recording_id: &str) -> Result<Vec<(i64, i64)>> {
        Ok(self.db().speech_intervals(recording_id)?)
    }

    /// The finished recording's Ogg Opus file, for playback.
    pub fn audio_path(&self, id: &str) -> Result<PathBuf> {
        self.ensure_not_active(id)?;
        let recording = self.db().recording(id)?;
        let path = self.audio_path_of(&recording);
        if !path.exists() {
            return Err(EngineError::NotFound);
        }
        Ok(path)
    }

    fn ensure_not_active(&self, id: &str) -> Result<()> {
        match self.active().as_ref() {
            Some(a) if a.id == id => Err(EngineError::StillRecording),
            _ => Ok(()),
        }
    }

    // ---- Transcripts -------------------------------------------------------

    fn ensure_idle_transcript(&self, recording: &Recording) -> Result<()> {
        self.ensure_not_active(&recording.id)?;
        if matches!(
            recording.transcript_status,
            TranscriptStatus::Pending | TranscriptStatus::Running
        ) {
            return Err(EngineError::Busy);
        }
        Ok(())
    }

    /// Queues a finished recording for transcription, replacing any transcript
    /// it has. `translate` also translates it into English.
    pub fn transcribe_recording(&self, id: &str, translate: bool) -> Result<()> {
        let recording = self.db().recording(id)?;
        self.ensure_idle_transcript(&recording)?;
        self.db().set_translate(id, translate)?;
        self.db()
            .set_transcript_status(id, TranscriptStatus::Pending)?;
        self.shared.notify(id, TranscriptStatus::Pending, None);
        self.jobs.submit(Job::Transcribe(id.to_owned()));
        Ok(())
    }

    /// Queues translation of the segments of a transcript not yet translated.
    pub fn translate_recording(&self, id: &str) -> Result<()> {
        let recording = self.db().recording(id)?;
        self.ensure_idle_transcript(&recording)?;
        if self.db().transcript(id)?.is_empty() {
            return Err(EngineError::NoTranscript);
        }
        self.db().set_translate(id, true)?;
        self.db()
            .set_transcript_status(id, TranscriptStatus::Pending)?;
        self.shared.notify(id, TranscriptStatus::Pending, None);
        self.jobs.submit(Job::Translate(id.to_owned()));
        Ok(())
    }

    pub fn transcript(&self, id: &str) -> Result<Vec<TranscriptSegment>> {
        Ok(self.db().transcript(id)?)
    }

    /// How far work on a finished recording has got, `0.0..=1.0`, while it runs.
    pub fn transcript_progress(&self, id: &str) -> Option<f32> {
        self.shared.progress().get(id).copied()
    }

    // ---- Photos ------------------------------------------------------------

    /// Where the app saves the photos of `recording_id`, relative to the data
    /// directory. The directory exists when this returns.
    pub fn photo_dir(&self, recording_id: &str) -> Result<String> {
        self.db().recording(recording_id)?;
        let relative = format!("{PHOTOS_DIR}/{recording_id}");
        fs::create_dir_all(self.shared.data_dir.join(&relative))?;
        Ok(relative)
    }

    /// Records a photo the app has saved at `file` (relative to the data
    /// directory, inside [`photo_dir`](Self::photo_dir)). Without
    /// `timestamp_ms` it is placed at the current position of the recording
    /// in progress.
    pub fn add_photo(
        &self,
        recording_id: &str,
        file: &str,
        timestamp_ms: Option<i64>,
    ) -> Result<Photo> {
        let relative = Path::new(file);
        let inside = relative.starts_with(Path::new(PHOTOS_DIR).join(recording_id))
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !inside {
            return Err(EngineError::InvalidArgument(format!(
                "{file} is not in the photo directory"
            )));
        }
        if !self.shared.data_dir.join(relative).is_file() {
            return Err(EngineError::NotFound);
        }
        let timestamp_ms = match timestamp_ms {
            Some(ms) => ms.max(0),
            None => self.live_position_ms(recording_id)?,
        };
        let id = self.db().add_photo(recording_id, timestamp_ms, file)?;
        Ok(Photo {
            id,
            recording_id: recording_id.to_owned(),
            timestamp_ms,
            file: file.to_owned(),
        })
    }

    pub fn photos(&self, recording_id: &str) -> Result<Vec<Photo>> {
        Ok(self.db().photos(recording_id)?)
    }

    pub fn delete_photo(&self, id: i64) -> Result<()> {
        let photo = self.db().delete_photo(id)?;
        remove_if_present(&self.shared.data_dir.join(photo.file))?;
        Ok(())
    }

    // ---- Export ------------------------------------------------------------

    pub fn timeline(&self, id: &str, include_translation: bool) -> Result<Vec<Entry>> {
        Ok(export::timeline(
            &self.db().transcript(id)?,
            &self.db().bookmarks(id)?,
            &self.db().photos(id)?,
            include_translation,
        ))
    }

    /// Writes the recording as Markdown to `dest`: a `.md` file, or a `.zip`
    /// with the photos when it has any. `date` is the recording date as the
    /// app wants it shown.
    pub fn export_markdown(
        &self,
        id: &str,
        include_translation: bool,
        date: &str,
        dest: &Path,
    ) -> Result<ExportFormat> {
        let recording = self.db().recording(id)?;
        let entries = self.timeline(id, include_translation)?;
        let markdown = export::markdown(&recording, date, &entries);
        let photos: Vec<(String, PathBuf)> = self
            .db()
            .photos(id)?
            .iter()
            .map(|p| {
                (
                    export::photo_name(&p.file),
                    self.shared.data_dir.join(&p.file),
                )
            })
            .collect();
        Ok(export::write_export(&markdown, &photos, dest)?)
    }
}

/// Writes the capture session's output to the recording's file.
struct FileSink(BufWriter<File>);

impl Write for FileSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl AudioSink for FileSink {
    fn checkpoint(&mut self) -> io::Result<()> {
        self.0.flush()?;
        self.0.get_ref().sync_data()
    }

    fn finalize(self) -> io::Result<()> {
        self.0.into_inner().map_err(|e| e.into_error())?.sync_all()
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use examic_asr::{AsrError, Segment, Transcriber, samples_to_ms};
    use examic_mt::{MtError, Translator};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    const RATE: u32 = 48_000;

    /// A scratch data directory that is removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "examic-engine-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// "Transcribes" a window as its own length, so tests can see exactly
    /// which audio reached the model.
    struct LengthTranscriber;

    impl Transcriber for LengthTranscriber {
        fn transcribe(&mut self, pcm: &[f32]) -> std::result::Result<Vec<Segment>, AsrError> {
            let ms = samples_to_ms(pcm.len());
            Ok(vec![Segment {
                start_ms: 0,
                end_ms: ms,
                text: format!("{ms} мс."),
            }])
        }
    }

    struct TaggingTranslator;

    impl Translator for TaggingTranslator {
        fn translate(&mut self, text: &str) -> std::result::Result<String, MtError> {
            Ok(format!("EN[{text}]"))
        }
    }

    #[derive(Default)]
    struct FakeBackends {
        missing_models: AtomicBool,
    }

    impl Backends for FakeBackends {
        fn transcriber(&self, _live: bool) -> std::result::Result<Box<dyn Transcriber>, AsrError> {
            if self.missing_models.load(Ordering::Relaxed) {
                return Err(AsrError::Unavailable);
            }
            Ok(Box::new(LengthTranscriber))
        }

        fn translator(&self) -> std::result::Result<Box<dyn Translator>, MtError> {
            if self.missing_models.load(Ordering::Relaxed) {
                return Err(MtError::Unavailable);
            }
            Ok(Box::new(TaggingTranslator))
        }
    }

    #[derive(Default)]
    struct EventLog(Mutex<Vec<(String, TranscriptStatus)>>);

    impl EngineEvents for EventLog {
        fn transcript_changed(&self, id: &str, status: TranscriptStatus, _progress: Option<f32>) {
            self.0.lock().unwrap().push((id.to_owned(), status));
        }
    }

    fn open(dir: &TempDir) -> Engine {
        Engine::open_with(&dir.0, Arc::new(FakeBackends::default())).unwrap()
    }

    /// Delivers audio as fast as the worker takes it, losing nothing.
    fn feed(input: &mut PcmInput, pcm: &[i16]) {
        for block in pcm.chunks(480) {
            let mut block = block;
            while !block.is_empty() {
                let accepted = input.push(block);
                block = &block[accepted..];
                if !block.is_empty() {
                    std::thread::yield_now();
                }
            }
        }
    }

    fn feed_seconds(input: &mut PcmInput, seconds: u32) {
        feed(input, &vec![1000; (seconds * RATE) as usize]);
    }

    /// Room noise with a voice-like tone complex during `voiced` (seconds).
    fn speech_signal(seconds: f32, voiced: &[(f32, f32)]) -> Vec<i16> {
        let mut seed = 7u32;
        (0..(seconds * RATE as f32) as usize)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 0.003;
                let t = i as f32 / RATE as f32;
                let tone = if voiced.iter().any(|&(a, b)| (a..b).contains(&t)) {
                    [220.0, 440.0, 1100.0]
                        .iter()
                        .map(|f| (t * f * std::f32::consts::TAU).sin())
                        .sum::<f32>()
                        * 0.08
                } else {
                    0.0
                };
                ((noise + tone) * 32767.0) as i16
            })
            .collect()
    }

    fn record(engine: &Engine, title: &str, seconds: u32) -> Recording {
        let (_, mut input) = engine
            .start_recording(title, RATE, TranscribeMode::Off, false)
            .unwrap();
        feed_seconds(&mut input, seconds);
        drop(input);
        engine.stop_recording().unwrap()
    }

    /// Waits for transcription work on `id` to end.
    fn settled(engine: &Engine, id: &str) -> Recording {
        settled_within(engine, id, Duration::from_secs(20))
    }

    fn settled_within(engine: &Engine, id: &str, limit: Duration) -> Recording {
        let deadline = Instant::now() + limit;
        loop {
            let rec = engine.recording(id).unwrap();
            if !matches!(
                rec.transcript_status,
                TranscriptStatus::Pending | TranscriptStatus::Running
            ) {
                return rec;
            }
            assert!(
                Instant::now() < deadline,
                "transcription never finished: {rec:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn records_a_plain_ogg_opus_file() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (started, mut input) = engine
            .start_recording("Standup", RATE, TranscribeMode::Off, false)
            .unwrap();
        assert_eq!(started.status, RecordingStatus::Recording);
        assert_eq!(
            engine.active_recording_id().as_deref(),
            Some(started.id.as_str())
        );
        feed_seconds(&mut input, 3);
        assert_eq!(input.telemetry().position_ms(), 3000);
        drop(input);

        let done = engine.stop_recording().unwrap();
        assert_eq!(done.status, RecordingStatus::Ready);
        assert_eq!((done.title.as_str(), done.duration_ms), ("Standup", 3000));
        assert_eq!(done.transcript_status, TranscriptStatus::None);
        assert_eq!(engine.active_recording_id(), None);

        let path = engine.audio_path(&done.id).unwrap();
        assert_eq!(path, dir.0.join(RECORDINGS_DIR).join(&done.audio_file));
        assert!(done.audio_file.ends_with(".opus"));
        let on_disk = fs::read(&path).unwrap();
        assert_eq!(on_disk.len() as i64, done.size_bytes);
        assert_eq!(&on_disk[..4], b"OggS");
        assert_eq!(&on_disk[28..36], b"OpusHead");
    }

    #[test]
    fn only_one_recording_at_a_time() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let _first = engine
            .start_recording("a", RATE, TranscribeMode::Off, false)
            .unwrap();
        assert!(matches!(
            engine.start_recording("b", RATE, TranscribeMode::Off, false),
            Err(EngineError::AlreadyRecording)
        ));
        engine.stop_recording().unwrap();
        assert!(matches!(
            engine.stop_recording(),
            Err(EngineError::NotRecording)
        ));
        assert_eq!(engine.recordings().unwrap().len(), 1);
    }

    #[test]
    fn a_rejected_start_leaves_nothing_behind() {
        let dir = TempDir::new();
        let engine = open(&dir);
        assert!(
            engine
                .start_recording("bad rate", 44_100, TranscribeMode::Live, false)
                .is_err()
        );
        assert!(engine.recordings().unwrap().is_empty());
        assert_eq!(fs::read_dir(dir.0.join(RECORDINGS_DIR)).unwrap().count(), 0);
        assert_eq!(engine.active_recording_id(), None);
    }

    #[test]
    fn live_bookmarks_land_on_the_recording_timeline() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (rec, mut input) = engine
            .start_recording("Lecture", RATE, TranscribeMode::Off, false)
            .unwrap();
        feed_seconds(&mut input, 2);
        let flag = engine.add_bookmark(&rec.id, None, None).unwrap();
        feed_seconds(&mut input, 1);
        let note = engine
            .add_bookmark(&rec.id, None, Some("  exam hint  "))
            .unwrap();
        let blank = engine.add_bookmark(&rec.id, None, Some("   ")).unwrap();
        drop(input);
        engine.stop_recording().unwrap();

        assert_eq!((flag.timestamp_ms, note.timestamp_ms), (2000, 3000));
        assert_eq!(note.note.as_deref(), Some("exam hint"));
        assert_eq!(blank.note, None);
        assert_eq!(engine.recording(&rec.id).unwrap().bookmark_count, 3);

        // Once finished, "now" has no meaning; an explicit time still works.
        assert!(matches!(
            engine.add_bookmark(&rec.id, None, None),
            Err(EngineError::NotRecording)
        ));
        let later = engine.add_bookmark(&rec.id, Some(500), None).unwrap();
        assert_eq!(engine.bookmarks(&rec.id).unwrap()[0].id, later.id);
    }

    #[test]
    fn the_recording_in_progress_cannot_be_played_deleted_or_transcribed() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (rec, _input) = engine
            .start_recording("busy", RATE, TranscribeMode::Off, false)
            .unwrap();
        assert!(matches!(
            engine.audio_path(&rec.id),
            Err(EngineError::StillRecording)
        ));
        assert!(matches!(
            engine.delete_recording(&rec.id),
            Err(EngineError::StillRecording)
        ));
        assert!(matches!(
            engine.transcribe_recording(&rec.id, false),
            Err(EngineError::StillRecording)
        ));
        engine.stop_recording().unwrap();
    }

    #[test]
    fn deleting_removes_the_audio_and_photo_files() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let keep = record(&engine, "keep", 1);
        let gone = record(&engine, "gone", 1);
        let photo_dir = engine.photo_dir(&gone.id).unwrap();
        let photo_file = format!("{photo_dir}/a.jpg");
        fs::write(dir.0.join(&photo_file), b"jpeg").unwrap();
        engine.add_photo(&gone.id, &photo_file, Some(10)).unwrap();
        engine.delete_recording(&gone.id).unwrap();

        let files: Vec<_> = fs::read_dir(dir.0.join(RECORDINGS_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, [keep.audio_file]);
        assert!(!dir.0.join(&photo_dir).exists());
        assert!(matches!(
            engine.recording(&gone.id),
            Err(EngineError::NotFound)
        ));
        assert!(matches!(
            engine.audio_path(&gone.id),
            Err(EngineError::NotFound)
        ));
    }

    /// Records `seconds` of audio and "crashes": the engine is leaked once
    /// most of it is on disk, so nothing is finished or sealed.
    fn record_and_crash(dir: &TempDir, mode: TranscribeMode, seconds: u32) -> (String, PathBuf) {
        let engine = open(dir);
        let (rec, mut input) = engine
            .start_recording("Interrupted", RATE, mode, false)
            .unwrap();
        feed_seconds(&mut input, seconds);
        let path = dir.0.join(RECORDINGS_DIR).join(&rec.audio_file);
        while fs::metadata(&path).unwrap().len() < 4096 {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::mem::forget(input);
        std::mem::forget(engine);
        (rec.id, path)
    }

    #[test]
    fn a_recording_cut_short_by_a_crash_is_recovered_on_the_next_start() {
        let dir = TempDir::new();
        let (id, path) = record_and_crash(&dir, TranscribeMode::After, 20);

        let engine = open(&dir);
        let rec = engine.recording(&id).unwrap();
        assert_eq!(rec.status, RecordingStatus::Recovered);
        assert!(rec.duration_ms >= 1000, "{rec:?}");
        assert!(rec.duration_ms <= 20_000, "{rec:?}");
        assert!(rec.size_bytes > 0);
        // The transcription it was waiting for can be started again.
        assert_eq!(rec.transcript_status, TranscriptStatus::Failed);
        assert_eq!(&fs::read(&path).unwrap()[..4], b"OggS");

        engine.transcribe_recording(&id, false).unwrap();
        let rec = settled(&engine, &id);
        assert_eq!(rec.transcript_status, TranscriptStatus::Done);
        // No speech intervals after a crash: the whole length is covered.
        let text: Vec<_> = engine
            .transcript(&id)
            .unwrap()
            .into_iter()
            .map(|s| s.text_ru)
            .collect();
        let covered: i64 = text
            .iter()
            .map(|t| t.trim_end_matches(" мс.").parse::<i64>().unwrap())
            .sum();
        assert!(
            (rec.duration_ms - covered).abs() <= 40,
            "{covered} of {}",
            rec.duration_ms
        );
    }

    #[test]
    fn a_crash_before_any_audio_leaves_no_ghost_recording() {
        let dir = TempDir::new();
        {
            let engine = open(&dir);
            let started = engine
                .start_recording("Ghost", RATE, TranscribeMode::Off, false)
                .unwrap();
            std::mem::forget(started);
            std::mem::forget(engine);
        }
        let engine = open(&dir);
        assert!(engine.recordings().unwrap().is_empty());
        assert_eq!(fs::read_dir(dir.0.join(RECORDINGS_DIR)).unwrap().count(), 0);
    }

    #[test]
    fn an_unreadable_recording_is_kept_not_deleted() {
        let dir = TempDir::new();
        let (id, path) = record_and_crash(&dir, TranscribeMode::Off, 20);
        // Overwrite the start of a copy of the file with garbage; the leaked
        // engine above still holds the original open.
        let mut bytes = fs::read(&path).unwrap();
        bytes[..64].fill(0xAA);
        let dir2 = TempDir::new();
        fs::create_dir_all(dir2.0.join(RECORDINGS_DIR)).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let name = format!("{DATABASE_FILE}{suffix}");
            let _ = fs::copy(dir.0.join(&name), dir2.0.join(&name));
        }
        fs::write(
            dir2.0.join(RECORDINGS_DIR).join(path.file_name().unwrap()),
            bytes,
        )
        .unwrap();

        let engine = open(&dir2);
        let rec = engine.recording(&id).unwrap();
        assert_eq!(
            (rec.status, rec.duration_ms),
            (RecordingStatus::Recovered, 0)
        );
        assert!(dir2.0.join(RECORDINGS_DIR).join(&rec.audio_file).exists());
    }

    #[test]
    fn the_library_persists() {
        let dir = TempDir::new();
        let id = record(&open(&dir), "Kept", 1).id;
        let engine = open(&dir);
        assert_eq!(
            engine.recording(&id).unwrap().status,
            RecordingStatus::Ready
        );
        engine.rename_recording(&id, "Renamed").unwrap();
        assert_eq!(engine.recordings().unwrap()[0].title, "Renamed");
    }

    #[test]
    fn after_recording_mode_transcribes_and_translates_once_stopped() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let events = Arc::new(EventLog::default());
        engine.set_events(Some(events.clone()));
        let (rec, mut input) = engine
            .start_recording("After", RATE, TranscribeMode::After, true)
            .unwrap();
        assert_eq!(rec.transcript_status, TranscriptStatus::Pending);
        feed(&mut input, &speech_signal(6.0, &[(1.0, 2.0), (3.5, 5.0)]));
        drop(input);
        let stopped = engine.stop_recording().unwrap();

        let rec = settled(&engine, &stopped.id);
        assert_eq!(rec.transcript_status, TranscriptStatus::Done);
        let transcript = engine.transcript(&rec.id).unwrap();
        let speech = engine.speech_intervals(&rec.id).unwrap();
        assert_eq!(speech.len(), 2, "{speech:?}");
        // Both intervals fit one window, which spans them padded.
        assert_eq!(transcript.len(), 1, "{transcript:?}");
        assert_eq!(
            transcript[0].start_ms,
            speech[0].0 - examic_asr::WINDOW_PAD_MS
        );
        let en = transcript[0].text_en.as_deref().unwrap();
        assert_eq!(en, format!("EN[{}]", transcript[0].text_ru));
        assert_eq!(engine.transcript_progress(&rec.id), None);

        let log = events.0.lock().unwrap();
        assert_eq!(log.first().map(|e| e.1), Some(TranscriptStatus::Running));
        assert_eq!(log.last().map(|e| e.1), Some(TranscriptStatus::Done));
    }

    #[test]
    fn live_mode_transcribes_each_stretch_of_speech_while_recording() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (rec, mut input) = engine
            .start_recording("Live", RATE, TranscribeMode::Live, false)
            .unwrap();
        assert_eq!(rec.transcript_status, TranscriptStatus::Running);
        feed(&mut input, &speech_signal(6.0, &[(1.0, 2.0), (3.5, 5.0)]));
        // The first stretch is transcribed before the recording ends.
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.transcript(&rec.id).unwrap().is_empty() {
            assert!(Instant::now() < deadline, "no live segment");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(input);
        engine.stop_recording().unwrap();

        let rec = settled(&engine, &rec.id);
        assert_eq!(rec.transcript_status, TranscriptStatus::Done);
        let transcript = engine.transcript(&rec.id).unwrap();
        let speech = engine.speech_intervals(&rec.id).unwrap();
        assert_eq!(transcript.len(), speech.len());
        for (segment, interval) in transcript.iter().zip(&speech) {
            assert_eq!(segment.start_ms, interval.0 - examic_asr::WINDOW_PAD_MS);
            assert!(segment.text_en.is_none());
        }
    }

    #[test]
    fn missing_models_fail_the_transcript_not_the_recording() {
        let dir = TempDir::new();
        let backends = Arc::new(FakeBackends::default());
        backends.missing_models.store(true, Ordering::Relaxed);
        let engine = Engine::open_with(&dir.0, backends.clone()).unwrap();
        for mode in [TranscribeMode::Live, TranscribeMode::After] {
            let (_, mut input) = engine.start_recording("x", RATE, mode, true).unwrap();
            feed(&mut input, &speech_signal(3.0, &[(1.0, 2.0)]));
            drop(input);
            let rec = engine.stop_recording().unwrap();
            assert_eq!(rec.status, RecordingStatus::Ready);
            assert_eq!(
                settled(&engine, &rec.id).transcript_status,
                TranscriptStatus::Failed
            );
        }
    }

    #[test]
    fn transcribe_now_and_translate_later() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let rec = record(&engine, "Off", 2);
        assert!(matches!(
            engine.translate_recording(&rec.id),
            Err(EngineError::NoTranscript)
        ));
        engine.transcribe_recording(&rec.id, false).unwrap();
        settled(&engine, &rec.id);
        let transcript = engine.transcript(&rec.id).unwrap();
        assert!(!transcript.is_empty());
        assert!(transcript.iter().all(|s| s.text_en.is_none()));

        engine.translate_recording(&rec.id).unwrap();
        let rec = settled(&engine, &rec.id);
        assert!(rec.translate);
        assert!(
            engine
                .transcript(&rec.id)
                .unwrap()
                .iter()
                .all(|s| s.text_en.is_some())
        );
    }

    #[test]
    fn photos_are_timestamped_like_bookmarks_and_stay_in_their_folder() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (rec, mut input) = engine
            .start_recording("Board", RATE, TranscribeMode::Off, false)
            .unwrap();
        feed_seconds(&mut input, 2);
        let folder = engine.photo_dir(&rec.id).unwrap();
        let file = format!("{folder}/1.jpg");
        fs::write(dir.0.join(&file), b"jpeg").unwrap();
        let now = engine.add_photo(&rec.id, &file, None).unwrap();
        assert_eq!(now.timestamp_ms, 2000);
        drop(input);
        engine.stop_recording().unwrap();

        assert!(matches!(
            engine.add_photo(&rec.id, &format!("{folder}/../../examic.db"), Some(0)),
            Err(EngineError::InvalidArgument(_))
        ));
        assert!(matches!(
            engine.add_photo(&rec.id, &format!("{folder}/none.jpg"), Some(0)),
            Err(EngineError::NotFound)
        ));
        assert_eq!(engine.recording(&rec.id).unwrap().photo_count, 1);
        engine.delete_photo(now.id).unwrap();
        assert!(!dir.0.join(&file).exists());
        assert!(engine.photos(&rec.id).unwrap().is_empty());
    }

    #[test]
    fn exports_markdown_with_photos_as_a_zip() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let rec = record(&engine, "Export", 2);
        engine.transcribe_recording(&rec.id, true).unwrap();
        settled(&engine, &rec.id);
        engine
            .add_bookmark(&rec.id, Some(100), Some("start"))
            .unwrap();

        let md = dir.0.join("out.md");
        assert_eq!(
            engine.export_markdown(&rec.id, true, "today", &md).unwrap(),
            ExportFormat::Markdown
        );
        let text = fs::read_to_string(&md).unwrap();
        assert!(text.starts_with("# Export\n"), "{text}");
        assert!(text.contains("🔖 **[0:00]** start"));
        assert!(text.contains("> EN: EN["));
        let without = dir.0.join("plain.md");
        engine
            .export_markdown(&rec.id, false, "today", &without)
            .unwrap();
        assert!(!fs::read_to_string(&without).unwrap().contains("EN:"));

        let file = format!("{}/p.jpg", engine.photo_dir(&rec.id).unwrap());
        fs::write(dir.0.join(&file), b"jpeg").unwrap();
        engine.add_photo(&rec.id, &file, Some(1500)).unwrap();
        let zip = dir.0.join("out.zip");
        assert_eq!(
            engine
                .export_markdown(&rec.id, true, "today", &zip)
                .unwrap(),
            ExportFormat::Zip
        );
        assert_eq!(&fs::read(&zip).unwrap()[..2], b"PK");
    }

    /// The whole pipeline with the real codec and models, as on a phone:
    /// Russian speech recorded through libopus, cut by the voice detector,
    /// transcribed by GigaAM and translated by Opus-MT, live and after
    /// recording. Run with
    ///
    ///     EXAMIC_ASR_MODEL=.../gigaam-v3-e2e-ctc-Q8_0.gguf \
    ///     EXAMIC_MT_MODEL_DIR=.../opus-mt-ru-en ORT_DYLIB_PATH=.../libonnxruntime.so \
    ///     EXAMIC_FLEURS_DIR=.../fleurs/ru_ru \
    ///     cargo test --release -p examic-ffi --features native -- --ignored --nocapture
    #[cfg(feature = "native")]
    #[test]
    #[ignore = "needs the models and FLEURS audio; see the doc comment"]
    fn end_to_end_with_the_real_models() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let fleurs = PathBuf::from(var("EXAMIC_FLEURS_DIR"));
        let tsv = fs::read_to_string(fleurs.join("dev.tsv")).unwrap();
        let mut pcm: Vec<i16> = Vec::new();
        let mut reference = String::new();
        let mut seed = 1u32;
        let mut quiet = |n: usize, pcm: &mut Vec<i16>| {
            // Room noise at about -60 dBFS rather than digital silence.
            for _ in 0..n {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                pcm.push(((seed >> 16) as i16) / 1024);
            }
        };
        for line in tsv.lines().step_by(17).take(8) {
            let fields: Vec<&str> = line.split('\t').collect();
            let mut wav = hound::WavReader::open(fleurs.join("dev").join(fields[1])).unwrap();
            assert_eq!(wav.spec().sample_rate, 16_000);
            quiet(24_000, &mut pcm);
            pcm.extend(wav.samples::<f32>().map(|s| (s.unwrap() * 32767.0) as i16));
            reference.push_str(fields[2]);
            reference.push(' ');
        }
        quiet(24_000, &mut pcm);

        let words = |text: &str| -> Vec<String> {
            text.to_lowercase()
                .replace('ё', "е")
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { ' ' })
                .collect::<String>()
                .split_whitespace()
                .map(String::from)
                .collect()
        };
        let wer = |hypothesis: &str| -> f64 {
            let (r, h) = (words(&reference), words(hypothesis));
            let mut row: Vec<usize> = (0..=h.len()).collect();
            for i in 1..=r.len() {
                let mut diagonal = row[0];
                row[0] = i;
                for j in 1..=h.len() {
                    let above = row[j];
                    row[j] = (row[j] + 1)
                        .min(row[j - 1] + 1)
                        .min(diagonal + usize::from(r[i - 1] != h[j - 1]));
                    diagonal = above;
                }
            }
            row[h.len()] as f64 / r.len() as f64
        };

        let dir = TempDir::new();
        let engine = Engine::open(&dir.0).unwrap();
        engine.set_models(ModelPaths {
            asr_model: Some(var("EXAMIC_ASR_MODEL").into()),
            mt_dir: Some(var("EXAMIC_MT_MODEL_DIR").into()),
        });
        for mode in [TranscribeMode::Live, TranscribeMode::After] {
            let started = Instant::now();
            let (rec, mut input) = engine
                .start_recording("FLEURS", 16_000, mode, true)
                .unwrap();
            feed(&mut input, &pcm);
            drop(input);
            engine.stop_recording().unwrap();
            let rec = settled_within(&engine, &rec.id, Duration::from_secs(600));
            let transcript = engine.transcript(&rec.id).unwrap();
            let text: Vec<&str> = transcript.iter().map(|s| s.text_ru.as_str()).collect();
            let error_rate = wer(&text.join(" "));
            println!(
                "{mode:?}: {} s of audio, {} segments, WER {:.1}%, done after {:.1} s",
                rec.duration_ms / 1000,
                transcript.len(),
                error_rate * 100.0,
                started.elapsed().as_secs_f64()
            );
            for s in transcript.iter().take(4) {
                println!(
                    "  [{}] {}\n      EN: {}",
                    export::clock(s.start_ms),
                    s.text_ru,
                    s.text_en.as_deref().unwrap_or("-")
                );
            }
            assert_eq!(rec.transcript_status, TranscriptStatus::Done);
            assert!(error_rate < 0.15, "WER {error_rate}");
            assert!(
                transcript
                    .iter()
                    .all(|s| s.text_en.as_deref().is_some_and(|t| !t.is_empty()))
            );
            for pair in transcript.windows(2) {
                assert!(pair[0].start_ms < pair[1].start_ms);
            }
        }
    }
}
