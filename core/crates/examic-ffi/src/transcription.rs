//! Transcription and translation work, off the caller's thread.
//!
//! Two kinds of worker:
//! * the job queue, one thread that transcribes finished recordings one at a
//!   time (after-recording mode, "Transcribe now", "Translate");
//! * a live worker per recording in live mode, fed speech by the capture
//!   session through [`LiveListener`] while the recording runs.
//!
//! Both run at low priority and store each window's segments as soon as they
//! exist, so the UI can show a transcript growing.

use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use examic_asr::{LiveChunker, Segment, Transcriber, Window, ms_to_samples, plan_windows};
use examic_audio::{SpeechInterval, SpeechListener};
use examic_db::{TranscriptSegment, TranscriptStatus};
use examic_mt::{Translator, translate_text};

use crate::codec;
use crate::engine::Shared;

/// Told whenever a recording's transcript changes.
pub trait EngineEvents: Send + Sync {
    /// `progress` is `0.0..=1.0` for work on a finished recording and `None`
    /// for live transcription, whose end is not known.
    fn transcript_changed(
        &self,
        recording_id: &str,
        status: TranscriptStatus,
        progress: Option<f32>,
    );
}

pub(crate) enum Job {
    /// Transcribe a finished recording from scratch.
    Transcribe(String),
    /// Translate the segments of a transcript that have no translation yet.
    Translate(String),
}

/// The queue of work on finished recordings.
pub(crate) struct Jobs {
    sender: Sender<Job>,
}

impl Jobs {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("examic-jobs".into())
            .spawn(move || {
                lower_priority();
                for job in receiver {
                    let id = match &job {
                        Job::Transcribe(id) | Job::Translate(id) => id.clone(),
                    };
                    let result = match job {
                        Job::Transcribe(id) => transcribe(&shared, &id),
                        Job::Translate(id) => translate(&shared, &id),
                    };
                    shared.clear_progress(&id);
                    let status = match result {
                        Ok(()) => TranscriptStatus::Done,
                        Err(e) => {
                            log::error!("transcription of {id} failed: {e}");
                            TranscriptStatus::Failed
                        }
                    };
                    // The recording may have been deleted meanwhile.
                    if shared.db.set_transcript_status(&id, status).is_ok() {
                        shared.notify(&id, status, None);
                    }
                }
            })?;
        Ok(Self { sender })
    }

    pub fn submit(&self, job: Job) {
        // The worker only stops when this queue is dropped.
        let _ = self.sender.send(job);
    }
}

type JobResult = Result<(), String>;

fn store(
    shared: &Shared,
    id: &str,
    window: Window,
    segments: Vec<Segment>,
    translator: Option<&mut Box<dyn Translator>>,
) -> JobResult {
    let mut translator = translator;
    let rows: Vec<TranscriptSegment> = segments
        .into_iter()
        .map(|s| {
            let text_en =
                translator
                    .as_mut()
                    .and_then(|mt| match translate_text(mt.as_mut(), &s.text) {
                        Ok(text) => Some(text),
                        Err(e) => {
                            log::warn!("translation failed: {e}");
                            None
                        }
                    });
            TranscriptSegment {
                start_ms: window.start_ms + s.start_ms,
                end_ms: window.start_ms + s.end_ms,
                text_ru: s.text,
                text_en,
            }
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    shared
        .db
        .add_transcript_segments(id, &rows)
        .map_err(|e| e.to_string())
}

/// A translator if the recording asks for one. Without the model the
/// transcript is still made, untranslated.
fn optional_translator(shared: &Shared, wanted: bool) -> Option<Box<dyn Translator>> {
    if !wanted {
        return None;
    }
    shared
        .backends
        .translator()
        .inspect_err(|e| log::error!("{e}; transcribing without translation"))
        .ok()
}

fn transcribe(shared: &Shared, id: &str) -> JobResult {
    let recording = shared.db.recording(id).map_err(|e| e.to_string())?;
    shared
        .db
        .set_transcript_status(id, TranscriptStatus::Running)
        .map_err(|e| e.to_string())?;
    shared.db.clear_transcript(id).map_err(|e| e.to_string())?;
    shared.set_progress(id, 0.0);
    shared.notify(id, TranscriptStatus::Running, Some(0.0));

    let mut asr = shared
        .backends
        .transcriber(false)
        .map_err(|e| e.to_string())?;
    let mut mt = optional_translator(shared, recording.translate);
    let speech = shared.db.speech_intervals(id).map_err(|e| e.to_string())?;
    let duration_ms = recording.duration_ms.max(1);
    let windows = plan_windows(&speech, recording.duration_ms);

    let file =
        File::open(shared.recordings_dir.join(&recording.audio_file)).map_err(|e| e.to_string())?;
    // Decoded audio is kept only from the start of the next window on, so a
    // long recording never sits in memory as a whole.
    let mut buffer: Vec<f32> = Vec::new();
    let mut buffer_start = 0usize;
    let mut next = 0usize;
    let mut error: Option<String> = None;

    let mut run_window = |buffer: &[f32], buffer_start: usize, window: Window| -> JobResult {
        let from = ms_to_samples(window.start_ms)
            .saturating_sub(buffer_start)
            .min(buffer.len());
        let to = ms_to_samples(window.end_ms)
            .saturating_sub(buffer_start)
            .min(buffer.len());
        let segments = asr
            .transcribe(&buffer[from..to])
            .map_err(|e| e.to_string())?;
        store(shared, id, window, segments, mt.as_mut())?;
        let progress = (window.end_ms as f32 / duration_ms as f32).clamp(0.0, 1.0);
        shared.set_progress(id, progress);
        shared.notify(id, TranscriptStatus::Running, Some(progress));
        Ok(())
    };

    let total = ms_to_samples(recording.duration_ms) as u64;
    let decoded = codec::decode_16k(file, total, &mut |pcm| {
        buffer.extend_from_slice(pcm);
        while next < windows.len()
            && buffer_start + buffer.len() >= ms_to_samples(windows[next].end_ms)
        {
            if let Err(e) = run_window(&buffer, buffer_start, windows[next]) {
                error = Some(e);
                return Ok(false);
            }
            next += 1;
        }
        let keep_from = windows
            .get(next)
            .map_or(usize::MAX, |w| ms_to_samples(w.start_ms));
        let drop = keep_from.saturating_sub(buffer_start).min(buffer.len());
        buffer.drain(..drop);
        buffer_start += drop;
        Ok(true)
    });
    if let Some(e) = error {
        return Err(e);
    }
    decoded.map_err(|e| e.to_string())?;
    // A recording shorter than its stated length (a torn end): transcribe
    // what there is.
    for &window in &windows[next..] {
        if ms_to_samples(window.start_ms) < buffer_start + buffer.len() {
            run_window(&buffer, buffer_start, window)?;
        }
    }
    Ok(())
}

fn translate(shared: &Shared, id: &str) -> JobResult {
    shared
        .db
        .set_transcript_status(id, TranscriptStatus::Running)
        .map_err(|e| e.to_string())?;
    shared.set_progress(id, 0.0);
    shared.notify(id, TranscriptStatus::Running, Some(0.0));
    let mut mt = shared.backends.translator().map_err(|e| e.to_string())?;
    let segments = shared.db.transcript(id).map_err(|e| e.to_string())?;
    let count = segments.len().max(1);
    for (i, segment) in segments.iter().enumerate() {
        if segment.text_en.is_none() {
            let text = translate_text(mt.as_mut(), &segment.text_ru).map_err(|e| e.to_string())?;
            shared
                .db
                .set_segment_translation(id, segment.start_ms, Some(&text))
                .map_err(|e| e.to_string())?;
        }
        let progress = (i + 1) as f32 / count as f32;
        shared.set_progress(id, progress);
        shared.notify(id, TranscriptStatus::Running, Some(progress));
    }
    Ok(())
}

/// The capture session's side of live transcription: cuts speech out of the
/// stream and hands it to the live worker. Never blocks the capture thread.
pub(crate) struct LiveListener {
    chunker: LiveChunker,
    sender: Sender<(Window, Vec<f32>)>,
}

impl SpeechListener for LiveListener {
    fn audio(&mut self, pcm: &[f32], speech_open: bool) {
        if let Some(window) = self.chunker.push(pcm, speech_open) {
            let _ = self.sender.send(window);
        }
    }

    fn speech(&mut self, interval: SpeechInterval) {
        if let Some(window) = self
            .chunker
            .speech(interval.start_ms as i64, interval.end_ms as i64)
        {
            let _ = self.sender.send(window);
        }
    }
}

impl Drop for LiveListener {
    /// The capture session drops its listener when the recording ends.
    fn drop(&mut self) {
        if let Some(window) = self.chunker.finish() {
            let _ = self.sender.send(window);
        }
    }
}

/// Starts the live worker for recording `id`. Windows queue up if the worker
/// falls behind and are finished after the recording stops; the transcript
/// is marked done once the listener is dropped and the queue is empty.
pub(crate) fn start_live(
    shared: Arc<Shared>,
    id: String,
    translate: bool,
) -> std::io::Result<LiveListener> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("examic-live".into())
        .spawn(move || live_worker(&shared, &id, translate, receiver))?;
    Ok(LiveListener {
        chunker: LiveChunker::new(),
        sender,
    })
}

fn live_worker(shared: &Shared, id: &str, translate: bool, windows: Receiver<(Window, Vec<f32>)>) {
    lower_priority();
    let mut asr: Box<dyn Transcriber> = match shared.backends.transcriber(true) {
        Ok(asr) => asr,
        Err(e) => {
            log::error!("live transcription unavailable: {e}");
            // Let the recording go on; the queue drains into nothing.
            for _ in windows {}
            let _ = shared
                .db
                .set_transcript_status(id, TranscriptStatus::Failed);
            shared.notify(id, TranscriptStatus::Failed, None);
            return;
        }
    };
    let mut mt = optional_translator(shared, translate);
    let mut failed = false;
    for (window, pcm) in windows {
        if failed {
            continue;
        }
        let result = asr
            .transcribe(&pcm)
            .map_err(|e| e.to_string())
            .and_then(|segments| store(shared, id, window, segments, mt.as_mut()));
        match result {
            Ok(()) => shared.notify(id, TranscriptStatus::Running, None),
            Err(e) => {
                log::error!("live transcription of {id} failed: {e}");
                failed = true;
            }
        }
    }
    let status = if failed {
        TranscriptStatus::Failed
    } else {
        TranscriptStatus::Done
    };
    if shared.db.set_transcript_status(id, status).is_ok() {
        shared.notify(id, status, None);
    }
}

/// Model work must never starve the capture thread or the UI.
fn lower_priority() {
    #[cfg(target_os = "android")]
    // SAFETY: plain syscalls on the calling thread; no memory is shared.
    unsafe {
        // Threads that ggml and ONNX Runtime start from here inherit this.
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
    }
}
