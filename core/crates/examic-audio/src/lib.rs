//! Examic's capture engine.
//!
//! ```text
//! audio thread --push--> ring buffer --> worker thread
//!                                          |- level meter ----------> Telemetry
//!                                          |- Opus encoder -> Ogg --> AudioSink
//!                                          '- 16 kHz decimator -> VAD -> speech intervals
//!                                                                \-> SpeechListener (live transcription)
//! ```

pub mod decimate;
pub mod ogg_opus;
pub mod session;
pub mod vad;

#[cfg(feature = "opus")]
pub use ogg_opus::OpusEncoder;
pub use ogg_opus::{EncodeError, OggOpusWriter, PacketEncoder, stream_duration_ms};
pub use session::{
    AudioSink, CaptureError, CaptureSession, DEFAULT_RING_MS, PcmInput, SessionReport,
    SpeechListener, Telemetry,
};
pub use vad::{EnergyVad, SpeechInterval, Vad};
