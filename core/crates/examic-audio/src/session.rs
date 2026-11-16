//! A capture session: PCM goes in on the caller's audio thread, an encoded
//! file comes out on a worker thread.
//!
//! The audio thread only copies samples into a lock-free ring buffer. Encoding,
//! voice activity detection and disk writes happen on the worker, so a slow
//! flash write or a busy CPU never makes the microphone thread late.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::decimate::Decimator;
use crate::ogg_opus::{EncodeError, GRANULE_RATE, OggOpusWriter, PacketEncoder};
use crate::vad::{
    SegmenterConfig, SpeechInterval, SpeechSegmenter, VAD_FRAME_SAMPLES, VAD_SAMPLE_RATE, Vad,
};

/// How much audio the ring buffer holds if the worker stalls.
pub const DEFAULT_RING_MS: u32 = 10_000;
/// The level meter maps this many dB below full scale onto `0.0..=1.0`.
const METER_RANGE_DB: f32 = 60.0;
/// How often written audio is forced to stable storage, in frames (10 s).
const CHECKPOINT_FRAMES: u32 = 500;
const IDLE_WAIT: Duration = Duration::from_millis(20);

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("unsupported input sample rate {0} Hz")]
    UnsupportedRate(u32),
    #[error("encoder runs at {encoder} Hz but input is {input} Hz")]
    RateMismatch { encoder: u32, input: u32 },
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error("capture worker panicked")]
    WorkerPanicked,
}

/// Where the encoded stream goes.
pub trait AudioSink: Write + Send + 'static {
    /// Makes everything written so far survive a power loss.
    fn checkpoint(&mut self) -> io::Result<()>;
    /// Seals the stream after the last byte has been written.
    fn finalize(self) -> io::Result<()>;
}

/// Receives speech as it is detected, on the capture worker thread.
///
/// Used for live transcription. Calls must return quickly: they run between
/// encoding one frame and the next, so anything slow belongs on another
/// thread. The listener is dropped when the session ends, after its last call.
pub trait SpeechListener: Send + 'static {
    /// The recording's audio at 16 kHz in `-1.0..1.0`, in order from the
    /// start of the recording. `speech_open` says whether the detector was
    /// inside a speech interval when this audio began.
    fn audio(&mut self, pcm: &[f32], speech_open: bool);
    /// A speech interval has closed.
    fn speech(&mut self, interval: SpeechInterval);
}

/// Live state of a session, readable from any thread without locking.
pub struct Telemetry {
    sample_rate: u32,
    level: AtomicU32,
    vad: AtomicU32,
    accepted_samples: AtomicU64,
    dropped_samples: AtomicU64,
    failed: AtomicBool,
}

impl Telemetry {
    /// Input loudness in `0.0..=1.0`, linear in dB over the meter range.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Speech probability of the most recent VAD frame.
    pub fn speech_probability(&self) -> f32 {
        f32::from_bits(self.vad.load(Ordering::Relaxed))
    }

    /// Length of the recording so far. This is the timeline bookmarks use.
    pub fn position_ms(&self) -> u64 {
        self.accepted_samples.load(Ordering::Relaxed) * 1000 / self.sample_rate as u64
    }

    /// Samples lost because the ring buffer was full.
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples.load(Ordering::Relaxed)
    }

    /// True once the worker hit an error (typically a full disk). Audio pushed
    /// after that is discarded; stopping the session returns the error.
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

/// The audio thread's end of a session.
pub struct PcmInput {
    producer: HeapProd<i16>,
    telemetry: Arc<Telemetry>,
    worker: Thread,
}

impl PcmInput {
    /// Hands samples to the worker and returns how many were accepted. Never
    /// blocks and never allocates.
    pub fn push(&mut self, pcm: &[i16]) -> usize {
        let accepted = self.producer.push_slice(pcm);
        self.telemetry
            .accepted_samples
            .fetch_add(accepted as u64, Ordering::Relaxed);
        if accepted < pcm.len() {
            self.telemetry
                .dropped_samples
                .fetch_add((pcm.len() - accepted) as u64, Ordering::Relaxed);
        }
        self.worker.unpark();
        accepted
    }

    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.telemetry
    }
}

/// What a finished session produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReport {
    pub duration_ms: u64,
    pub samples: u64,
    pub dropped_samples: u64,
    pub speech: Vec<SpeechInterval>,
}

pub struct CaptureSession {
    telemetry: Arc<Telemetry>,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Result<SessionReport, CaptureError>>,
}

impl CaptureSession {
    /// Writes the stream headers to `sink` and starts the worker thread.
    ///
    /// `input_rate` must be a multiple (1x to 6x) of 16 kHz and equal to the
    /// encoder's rate.
    pub fn start<E, S>(
        input_rate: u32,
        ring_ms: u32,
        encoder: E,
        sink: S,
        vad: Box<dyn Vad>,
    ) -> Result<(PcmInput, CaptureSession), CaptureError>
    where
        E: PacketEncoder + 'static,
        S: AudioSink,
    {
        Self::start_with_listener(input_rate, ring_ms, encoder, sink, vad, None)
    }

    /// [`start`](Self::start), with a [`SpeechListener`] that is told about
    /// speech while it is being recorded.
    pub fn start_with_listener<E, S>(
        input_rate: u32,
        ring_ms: u32,
        encoder: E,
        sink: S,
        vad: Box<dyn Vad>,
        listener: Option<Box<dyn SpeechListener>>,
    ) -> Result<(PcmInput, CaptureSession), CaptureError>
    where
        E: PacketEncoder + 'static,
        S: AudioSink,
    {
        let decimator = Decimator::for_rates(input_rate, VAD_SAMPLE_RATE)
            .ok_or(CaptureError::UnsupportedRate(input_rate))?;
        if encoder.sample_rate() != input_rate {
            return Err(CaptureError::RateMismatch {
                encoder: encoder.sample_rate(),
                input: input_rate,
            });
        }

        let to_48k = GRANULE_RATE / input_rate as u64;
        let pre_skip = (encoder.lookahead_samples() as u64 * to_48k) as u16;
        let writer = OggOpusWriter::new(sink, input_rate, pre_skip)?;

        let ring_samples = (input_rate as u64 * ring_ms as u64 / 1000) as usize;
        let (producer, consumer) = HeapRb::<i16>::new(ring_samples.max(1)).split();
        let telemetry = Arc::new(Telemetry {
            sample_rate: input_rate,
            level: AtomicU32::new(0f32.to_bits()),
            vad: AtomicU32::new(0f32.to_bits()),
            accepted_samples: AtomicU64::new(0),
            dropped_samples: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        });
        let stop = Arc::new(AtomicBool::new(false));

        let worker = Worker {
            consumer,
            frame: vec![0; encoder.frame_samples()],
            encoder,
            writer: Some(writer),
            decimator,
            vad,
            listener,
            segmenter: SpeechSegmenter::new(SegmenterConfig::default()),
            vad_input: Vec::with_capacity(VAD_FRAME_SAMPLES * 2),
            vad_frames: 0,
            speech: Vec::new(),
            packet: Vec::new(),
            samples: 0,
            frames_since_checkpoint: 0,
            input_rate,
            to_48k,
            telemetry: telemetry.clone(),
            stop: stop.clone(),
        };
        let worker = thread::Builder::new()
            .name("examic-capture".into())
            .spawn(move || worker.run())?;

        let input = PcmInput {
            producer,
            telemetry: telemetry.clone(),
            worker: worker.thread().clone(),
        };
        Ok((
            input,
            CaptureSession {
                telemetry,
                stop,
                worker,
            },
        ))
    }

    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.telemetry
    }

    /// Encodes whatever is still buffered, seals the file and returns the
    /// session's totals. Call this after the last [`PcmInput::push`].
    pub fn stop(self) -> Result<SessionReport, CaptureError> {
        self.stop.store(true, Ordering::Release);
        self.worker.thread().unpark();
        self.worker
            .join()
            .map_err(|_| CaptureError::WorkerPanicked)?
    }
}

struct Worker<E: PacketEncoder, S: AudioSink> {
    consumer: HeapCons<i16>,
    frame: Vec<i16>,
    encoder: E,
    /// `None` after a write failed: the file is left as it is on disk.
    writer: Option<OggOpusWriter<S>>,
    decimator: Decimator,
    vad: Box<dyn Vad>,
    listener: Option<Box<dyn SpeechListener>>,
    segmenter: SpeechSegmenter,
    vad_input: Vec<f32>,
    vad_frames: u64,
    speech: Vec<SpeechInterval>,
    packet: Vec<u8>,
    /// Real (unpadded) samples encoded so far.
    samples: u64,
    frames_since_checkpoint: u32,
    input_rate: u32,
    to_48k: u64,
    telemetry: Arc<Telemetry>,
    stop: Arc<AtomicBool>,
}

impl<E: PacketEncoder, S: AudioSink> Worker<E, S> {
    fn run(mut self) -> Result<SessionReport, CaptureError> {
        let mut error = None;
        let mut filled = 0;
        loop {
            // Read the flag before draining: samples pushed before `stop()`
            // was called are then guaranteed to be seen by this pass.
            let stopping = self.stop.load(Ordering::Acquire);
            filled += self.consumer.pop_slice(&mut self.frame[filled..]);
            if filled == self.frame.len() {
                filled = 0;
                if error.is_none()
                    && let Err(e) = self.process_frame()
                {
                    log::error!("capture failed, discarding further audio: {e}");
                    self.telemetry.failed.store(true, Ordering::Relaxed);
                    self.writer = None;
                    error = Some(e);
                }
            } else if stopping {
                break;
            } else {
                thread::park_timeout(IDLE_WAIT);
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        self.finish(filled)
    }

    fn process_frame(&mut self) -> Result<(), CaptureError> {
        self.telemetry
            .level
            .store(meter_level(&self.frame).to_bits(), Ordering::Relaxed);

        self.encode_frame()?;
        self.samples += self.frame.len() as u64;
        self.run_vad(self.frame.len());

        self.frames_since_checkpoint += 1;
        if self.frames_since_checkpoint == CHECKPOINT_FRAMES {
            self.frames_since_checkpoint = 0;
            if let Some(writer) = &mut self.writer {
                writer.sink_mut().checkpoint()?;
            }
        }
        Ok(())
    }

    fn encode_frame(&mut self) -> Result<(), CaptureError> {
        self.encoder.encode(&self.frame, &mut self.packet)?;
        if let Some(writer) = &mut self.writer {
            writer.write_packet(&self.packet, self.frame.len() as u64 * self.to_48k)?;
        }
        Ok(())
    }

    /// Runs detection over the first `len` samples of the current frame.
    fn run_vad(&mut self, len: usize) {
        let before = self.vad_input.len();
        self.decimator
            .process(&self.frame[..len], &mut self.vad_input);
        if let Some(listener) = &mut self.listener {
            listener.audio(&self.vad_input[before..], self.segmenter.is_open());
        }
        let mut consumed = 0;
        while self.vad_input.len() - consumed >= VAD_FRAME_SAMPLES {
            let frame = &self.vad_input[consumed..consumed + VAD_FRAME_SAMPLES];
            let probability = self.vad.speech_probability(frame).clamp(0.0, 1.0);
            consumed += VAD_FRAME_SAMPLES;
            self.telemetry
                .vad
                .store(probability.to_bits(), Ordering::Relaxed);

            let frame_ms = |n: u64| n * VAD_FRAME_SAMPLES as u64 * 1000 / VAD_SAMPLE_RATE as u64;
            let (start_ms, end_ms) = (frame_ms(self.vad_frames), frame_ms(self.vad_frames + 1));
            self.vad_frames += 1;
            if let Some(interval) = self.segmenter.push(probability, start_ms, end_ms) {
                if let Some(listener) = &mut self.listener {
                    listener.speech(interval);
                }
                self.speech.push(interval);
            }
        }
        self.vad_input.drain(..consumed);
    }

    fn finish(mut self, filled: usize) -> Result<SessionReport, CaptureError> {
        if self.samples > 0 || filled > 0 {
            // The encoder delays its output by `lookahead` samples, so the
            // stream must be padded with at least that much silence or the
            // end of the recording would be cut off.
            let mut padding = 0;
            if filled > 0 {
                self.run_vad(filled);
                self.samples += filled as u64;
                padding = self.frame.len() - filled;
                self.frame[filled..].fill(0);
                self.encode_frame()?;
            }
            if padding < self.encoder.lookahead_samples() as usize {
                self.frame.fill(0);
                self.encode_frame()?;
            }
        }

        let duration_ms = self.samples * 1000 / self.input_rate as u64;
        if let Some(interval) = self.segmenter.finish(duration_ms) {
            if let Some(listener) = &mut self.listener {
                listener.speech(interval);
            }
            self.speech.push(interval);
        }
        // Dropping the listener tells it the recording is over.
        self.listener = None;

        let writer = self.writer.take().expect("no write has failed");
        writer.finish(self.samples * self.to_48k)?.finalize()?;

        Ok(SessionReport {
            duration_ms,
            samples: self.samples,
            dropped_samples: self.telemetry.dropped_samples(),
            speech: self.speech,
        })
    }
}

/// RMS level of a frame mapped onto `0.0..=1.0` across the meter range.
fn meter_level(frame: &[i16]) -> f32 {
    let sum_squares: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_squares / frame.len().max(1) as f64).sqrt() / 32768.0;
    let db = 20.0 * (rms as f32 + 1e-9).log10();
    ((db + METER_RANGE_DB) / METER_RANGE_DB).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vad::EnergyVad;
    use crate::vad::test_signals::{Noise, voice};
    use ogg::reading::PacketReader;
    use std::io::Cursor;
    use std::sync::Mutex;

    const RATE: u32 = 48_000;
    const FRAME: usize = 960;
    const LOOKAHEAD: u32 = 312;

    /// Stands in for libopus: each packet is the frame's first sample.
    struct FakeEncoder;

    impl PacketEncoder for FakeEncoder {
        fn sample_rate(&self) -> u32 {
            RATE
        }
        fn frame_samples(&self) -> usize {
            FRAME
        }
        fn lookahead_samples(&self) -> u32 {
            LOOKAHEAD
        }
        fn encode(&mut self, pcm: &[i16], packet: &mut Vec<u8>) -> Result<(), EncodeError> {
            assert_eq!(pcm.len(), FRAME);
            packet.clear();
            packet.extend_from_slice(&pcm[0].to_le_bytes());
            Ok(())
        }
    }

    #[derive(Default)]
    struct SinkState {
        bytes: Vec<u8>,
        finalized: bool,
    }

    /// Collects output in memory; fails once more than `limit` bytes arrive.
    #[derive(Clone)]
    struct TestSink {
        state: Arc<Mutex<SinkState>>,
        limit: usize,
    }

    impl TestSink {
        fn new(limit: usize) -> Self {
            Self {
                state: Arc::default(),
                limit,
            }
        }
    }

    impl Write for TestSink {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();
            if state.bytes.len() + data.len() > self.limit {
                return Err(io::Error::other("disk full"));
            }
            state.bytes.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AudioSink for TestSink {
        fn checkpoint(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn finalize(self) -> io::Result<()> {
            self.state.lock().unwrap().finalized = true;
            Ok(())
        }
    }

    fn start(sink: TestSink, ring_ms: u32) -> (PcmInput, CaptureSession) {
        CaptureSession::start(RATE, ring_ms, FakeEncoder, sink, Box::new(EnergyVad::new())).unwrap()
    }

    /// Pushes `pcm` in 10 ms blocks, as an audio callback would.
    fn feed(input: &mut PcmInput, pcm: &[i16]) {
        for block in pcm.chunks(480) {
            assert_eq!(input.push(block), block.len());
        }
    }

    fn audio_packets(bytes: &[u8]) -> Vec<ogg::Packet> {
        let mut reader = PacketReader::new(Cursor::new(bytes));
        let mut packets = Vec::new();
        while let Some(packet) = reader.read_packet().unwrap() {
            packets.push(packet);
        }
        packets.split_off(2) // drop OpusHead and OpusTags
    }

    #[test]
    fn records_every_sample_and_seals_the_stream() {
        let sink = TestSink::new(usize::MAX);
        let (mut input, session) = start(sink.clone(), DEFAULT_RING_MS);
        // 2.5 s plus half a frame; the sample value is its frame number.
        let pcm: Vec<i16> = (0..120_000 + 480).map(|i| (i / FRAME) as i16).collect();
        feed(&mut input, &pcm);
        assert_eq!(input.telemetry().position_ms(), 2510);

        let report = session.stop().unwrap();
        assert_eq!(report.samples, 120_480);
        assert_eq!(report.duration_ms, 2510);
        assert_eq!(report.dropped_samples, 0);

        let state = sink.state.lock().unwrap();
        assert!(state.finalized);
        let packets = audio_packets(&state.bytes);
        // 125 full frames + the padded half frame; its 480 samples of padding
        // cover the encoder's lookahead, so no extra frame follows.
        assert_eq!(packets.len(), 126);
        for (i, packet) in packets.iter().enumerate() {
            assert_eq!(packet.data, (i as i16).to_le_bytes(), "packet {i}");
        }
        let last = packets.last().unwrap();
        assert!(last.last_in_stream());
        assert_eq!(last.absgp_page(), LOOKAHEAD as u64 + 120_480);
    }

    #[test]
    fn pads_an_extra_frame_when_the_audio_ends_on_a_frame_boundary() {
        let sink = TestSink::new(usize::MAX);
        let (mut input, session) = start(sink.clone(), DEFAULT_RING_MS);
        feed(&mut input, &vec![7; 10 * FRAME]);
        let report = session.stop().unwrap();
        assert_eq!(report.samples, 10 * FRAME as u64);

        let state = sink.state.lock().unwrap();
        let packets = audio_packets(&state.bytes);
        assert_eq!(packets.len(), 11);
        assert_eq!(packets[10].data, 0i16.to_le_bytes());
        assert_eq!(
            packets[10].absgp_page(),
            LOOKAHEAD as u64 + 10 * FRAME as u64
        );
    }

    #[test]
    fn an_empty_session_still_produces_a_valid_sealed_file() {
        let sink = TestSink::new(usize::MAX);
        let (_input, session) = start(sink.clone(), DEFAULT_RING_MS);
        let report = session.stop().unwrap();
        assert_eq!((report.samples, report.duration_ms), (0, 0));
        let state = sink.state.lock().unwrap();
        assert!(state.finalized);
        assert!(audio_packets(&state.bytes).is_empty());
    }

    #[test]
    fn finds_speech_and_reports_it_on_the_recording_timeline() {
        let sink = TestSink::new(usize::MAX);
        let (mut input, session) = start(sink, DEFAULT_RING_MS);
        let mut noise = Noise(9);
        // 1 s of room noise, 1 s of voice, 1 s of room noise.
        let pcm: Vec<i16> = (0..3 * RATE as usize)
            .map(|i| {
                let voiced = (RATE as usize..2 * RATE as usize).contains(&i);
                let v = if voiced { voice(i / 3, -20.0) } else { 0.0 };
                ((noise.next(-55.0) + v) * 32767.0) as i16
            })
            .collect();
        feed(&mut input, &pcm);

        let report = session.stop().unwrap();
        assert_eq!(report.speech.len(), 1, "{:?}", report.speech);
        let interval = report.speech[0];
        assert!((800..=1000).contains(&interval.start_ms), "{interval:?}");
        assert!((2000..=2250).contains(&interval.end_ms), "{interval:?}");
    }

    #[derive(Default)]
    struct Heard {
        samples: usize,
        open_samples: usize,
        intervals: Vec<SpeechInterval>,
        dropped: bool,
    }

    struct Recorder(Arc<Mutex<Heard>>);

    impl SpeechListener for Recorder {
        fn audio(&mut self, pcm: &[f32], speech_open: bool) {
            let mut heard = self.0.lock().unwrap();
            heard.samples += pcm.len();
            if speech_open {
                heard.open_samples += pcm.len();
            }
        }
        fn speech(&mut self, interval: SpeechInterval) {
            self.0.lock().unwrap().intervals.push(interval);
        }
    }

    impl Drop for Recorder {
        fn drop(&mut self) {
            self.0.lock().unwrap().dropped = true;
        }
    }

    #[test]
    fn a_listener_hears_all_audio_at_16_khz_and_every_interval() {
        let heard = Arc::new(Mutex::new(Heard::default()));
        let (mut input, session) = CaptureSession::start_with_listener(
            RATE,
            DEFAULT_RING_MS,
            FakeEncoder,
            TestSink::new(usize::MAX),
            Box::new(EnergyVad::new()),
            Some(Box::new(Recorder(heard.clone()))),
        )
        .unwrap();
        let mut noise = Noise(5);
        // Room noise, voice from 1 s to 2 s, then voice from 2.5 s to the end:
        // the last interval is still open when recording stops.
        let pcm: Vec<i16> = (0..3 * RATE as usize)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let voiced = (1.0..2.0).contains(&t) || t >= 2.5;
                let v = if voiced { voice(i / 3, -20.0) } else { 0.0 };
                ((noise.next(-55.0) + v) * 32767.0) as i16
            })
            .collect();
        feed(&mut input, &pcm);
        let report = session.stop().unwrap();

        let heard = heard.lock().unwrap();
        assert!(heard.dropped);
        assert_eq!(heard.samples, 3 * 16_000);
        assert!(heard.open_samples > 16_000 && heard.open_samples < 3 * 16_000);
        assert_eq!(heard.intervals, report.speech);
        assert_eq!(heard.intervals.len(), 2, "{:?}", heard.intervals);
        assert_eq!(heard.intervals[1].end_ms, 3_000);
    }

    #[test]
    fn counts_samples_dropped_when_the_ring_is_full() {
        let sink = TestSink::new(usize::MAX);
        let (mut input, session) = start(sink, 20);
        // One push far larger than the 20 ms ring: most of it cannot fit.
        let accepted = input.push(&vec![0; RATE as usize]);
        assert!(accepted <= FRAME);
        let dropped = input.telemetry().dropped_samples();
        assert_eq!(dropped, RATE as u64 - accepted as u64);
        let report = session.stop().unwrap();
        assert_eq!(report.dropped_samples, dropped);
        assert_eq!(report.samples, accepted as u64);
    }

    #[test]
    fn a_failing_sink_is_flagged_and_reported_on_stop() {
        // Room for the headers and the first pages, then the "disk" is full.
        let sink = TestSink::new(600);
        let (mut input, session) = start(sink.clone(), DEFAULT_RING_MS);
        feed(&mut input, &vec![1; 5 * RATE as usize]);

        let telemetry = session.telemetry().clone();
        let result = session.stop();
        assert!(matches!(result, Err(CaptureError::Io(_))), "{result:?}");
        assert!(telemetry.has_failed());
        assert!(!sink.state.lock().unwrap().finalized);
    }

    #[test]
    fn rejects_rates_it_cannot_decimate_and_mismatched_encoders() {
        let start = |rate| {
            CaptureSession::start(
                rate,
                DEFAULT_RING_MS,
                FakeEncoder,
                TestSink::new(usize::MAX),
                Box::new(EnergyVad::new()),
            )
            .map(|_| ())
        };
        assert!(matches!(
            start(44_100),
            Err(CaptureError::UnsupportedRate(44_100))
        ));
        assert!(matches!(
            start(16_000),
            Err(CaptureError::RateMismatch { .. })
        ));
    }

    #[test]
    fn meter_maps_decibels_onto_zero_to_one() {
        assert_eq!(meter_level(&[0; 960]), 0.0);
        assert!((meter_level(&[i16::MAX; 960]) - 1.0).abs() < 0.01);
        // -30 dBFS sits in the middle of a 60 dB range.
        let quiet = (32768.0 * 10f32.powf(-30.0 / 20.0)) as i16;
        assert!((meter_level(&[quiet; 960]) - 0.5).abs() < 0.01);
    }
}
