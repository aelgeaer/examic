//! Russian speech recognition for Examic.
//!
//! The model is GigaAM v3 (end-to-end CTC, with punctuation and casing) run by
//! transcribe.cpp on the CPU. This crate decides *what* audio goes to the
//! model and turns what comes back into transcript segments:
//!
//! * [`plan_windows`] groups a finished recording's speech intervals into
//!   windows of at most [`MAX_WINDOW_MS`];
//! * [`LiveChunker`] does the same while a recording is still running, one
//!   speech interval at a time;
//! * [`segments_from_tokens`] splits the model's token stream into
//!   sentence-sized segments with timestamps.
//!
//! Only [`GigaAm`] (feature `native`) touches the model; everything else is
//! pure Rust and tested on the host.

mod chunk;
mod segment;

#[cfg(feature = "native")]
mod gigaam;

pub use chunk::{LiveChunker, MAX_WINDOW_MS, WINDOW_PAD_MS, Window, plan_windows};
#[cfg(feature = "native")]
pub use gigaam::{GigaAm, GigaAmSession};
pub use segment::{Token, segments_from_tokens};

/// Sample rate the model takes.
pub const SAMPLE_RATE: u32 = 16_000;

/// One stretch of transcribed speech. Times are milliseconds on the timeline
/// of the audio that was passed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("speech model could not be loaded: {0}")]
    Load(String),
    #[error("speech recognition failed: {0}")]
    Run(String),
    #[error("speech recognition is not available in this build")]
    Unavailable,
}

/// Turns 16 kHz mono audio into segments.
pub trait Transcriber: Send {
    /// `pcm` is 16 kHz mono in `-1.0..1.0`. Returned times are relative to
    /// its first sample.
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, AsrError>;
}

/// Milliseconds of audio in `samples` samples at [`SAMPLE_RATE`].
pub fn samples_to_ms(samples: usize) -> i64 {
    (samples as u64 * 1000 / SAMPLE_RATE as u64) as i64
}

/// Samples at [`SAMPLE_RATE`] in `ms` milliseconds.
pub fn ms_to_samples(ms: i64) -> usize {
    (ms.max(0) as u64 * SAMPLE_RATE as u64 / 1000) as usize
}
