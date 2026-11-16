//! GigaAM v3 through transcribe.cpp.

use std::path::Path;
use std::sync::Once;

use transcribe_cpp::{Model, RunOptions, SessionOptions, TimestampKind, Transcript};

use crate::segment::{Token, segments_from_tokens};
use crate::{AsrError, Segment, Transcriber, samples_to_ms};

/// Below this the model has nothing to work with; such snippets are clicks.
const MIN_AUDIO_MS: i64 = 250;

/// A loaded model. Cheap to clone: every clone shares the weights, which are
/// memory-mapped from the model file.
#[derive(Clone)]
pub struct GigaAm {
    model: Model,
}

impl GigaAm {
    /// Loads a GigaAM GGUF file (`gigaam-v3-e2e-ctc-Q8_0.gguf`).
    pub fn load(path: &Path) -> Result<Self, AsrError> {
        static LOGGING: Once = Once::new();
        // transcribe.cpp logs to stderr by default; send it through `log`.
        LOGGING.call_once(transcribe_cpp::init_logging);
        let model = Model::load(path).map_err(|e| AsrError::Load(e.to_string()))?;
        if model.arch() != "gigaam" {
            return Err(AsrError::Load(format!(
                "expected a GigaAM model, got {}",
                model.arch()
            )));
        }
        Ok(Self { model })
    }

    /// A transcriber using `threads` CPU threads (0: the library's choice).
    /// Sessions of one model can live on different threads; their runs take
    /// turns.
    pub fn transcriber(&self, threads: usize) -> Result<GigaAmSession, AsrError> {
        let options = SessionOptions {
            n_threads: threads as i32,
            ..Default::default()
        };
        let session = self
            .model
            .session_with(&options)
            .map_err(|e| AsrError::Load(e.to_string()))?;
        Ok(GigaAmSession { session })
    }
}

/// One thread's handle on a [`GigaAm`] model.
pub struct GigaAmSession {
    session: transcribe_cpp::Session,
}

impl Transcriber for GigaAmSession {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, AsrError> {
        if samples_to_ms(pcm.len()) < MIN_AUDIO_MS {
            return Ok(Vec::new());
        }
        let options = RunOptions {
            timestamps: TimestampKind::Token,
            language: Some("ru".into()),
            ..Default::default()
        };
        let transcript = match self.session.run(pcm, &options) {
            Ok(t) => t,
            // A runaway decode still produced usable text up to that point.
            Err(e) => match e.partial() {
                Some(partial) => {
                    log::warn!("speech recognition stopped early: {e}");
                    partial.clone()
                }
                None => return Err(AsrError::Run(e.to_string())),
            },
        };
        Ok(segments(&transcript, samples_to_ms(pcm.len())))
    }
}

fn segments(transcript: &Transcript, duration_ms: i64) -> Vec<Segment> {
    let tokens: Vec<Token> = transcript
        .tokens
        .iter()
        .map(|t| Token {
            start_ms: t.t0_ms,
            end_ms: t.t1_ms,
            text: t.text.clone(),
        })
        .collect();
    let segments = segments_from_tokens(&tokens, 0);
    if !segments.is_empty() {
        return segments;
    }
    // No token timing: one segment spanning the window.
    let text = transcript.text.trim();
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Segment {
            start_ms: 0,
            end_ms: duration_ms,
            text: text.to_owned(),
        }]
    }
}
