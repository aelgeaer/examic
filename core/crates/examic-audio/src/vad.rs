//! Voice activity detection and grouping of per-frame decisions into speech
//! intervals.
//!
//! [`Vad`] is the seam for a neural detector (Silero) once ONNX Runtime joins
//! the build. [`EnergyVad`] is the detector that needs no model: it is what
//! drives the live ribbon and skip-silence today, and it stays useful later as
//! the cheap gate that decides whether a frame is worth running the neural
//! model on at all.

/// Sample rate every [`Vad`] works at.
pub const VAD_SAMPLE_RATE: u32 = 16_000;
/// Samples per frame: 32 ms, the frame size Silero VAD uses at 16 kHz.
pub const VAD_FRAME_SAMPLES: usize = 512;

pub trait Vad: Send {
    /// Speech probability in `0.0..=1.0` for one frame of
    /// [`VAD_FRAME_SAMPLES`] samples in `-1.0..1.0`.
    fn speech_probability(&mut self, frame: &[f32]) -> f32;
}

/// First-order high-pass at roughly 115 Hz: removes DC, handling rumble and
/// mains hum, none of which say anything about speech.
const HIGH_PASS_COEFF: f32 = 0.955;
/// Frames quieter than this are digital silence (a muted or not yet started
/// microphone) rather than a quiet room, and must not drag the noise floor
/// down to a level no real room reaches.
const DIGITAL_SILENCE_DB: f32 = -75.0;
/// The noise floor is the minimum frame energy over the last
/// `FLOOR_BLOCKS * FLOOR_BLOCK_FRAMES` frames (2.56 s). Speech dips to the
/// room level between words far more often than that; steady noise never does.
const FLOOR_BLOCKS: usize = 8;
const FLOOR_BLOCK_FRAMES: u32 = 10;
/// Energy above the floor at which a frame is as likely speech as not, and how
/// quickly the probability moves around that point.
const SNR_MIDPOINT_DB: f32 = 9.0;
const SNR_SLOPE_DB: f32 = 2.0;

/// Detects speech as energy standing out above a tracked noise floor.
pub struct EnergyVad {
    prev_in: f32,
    prev_out: f32,
    block_minima: [f32; FLOOR_BLOCKS],
    next_block: usize,
    current_min: f32,
    frames_in_block: u32,
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self {
            prev_in: 0.0,
            prev_out: 0.0,
            block_minima: [f32::INFINITY; FLOOR_BLOCKS],
            next_block: 0,
            current_min: f32::INFINITY,
            frames_in_block: 0,
        }
    }
}

impl EnergyVad {
    pub fn new() -> Self {
        Self::default()
    }

    fn noise_floor_db(&self) -> f32 {
        self.block_minima
            .iter()
            .fold(self.current_min, |floor, &block| floor.min(block))
    }
}

impl Vad for EnergyVad {
    fn speech_probability(&mut self, frame: &[f32]) -> f32 {
        let mut energy = 0.0f32;
        for &x in frame {
            let y = HIGH_PASS_COEFF * (self.prev_out + x - self.prev_in);
            self.prev_in = x;
            self.prev_out = y;
            energy += y * y;
        }
        let level_db = 10.0 * (energy / frame.len().max(1) as f32 + 1e-12).log10();
        if level_db < DIGITAL_SILENCE_DB {
            return 0.0;
        }

        self.current_min = self.current_min.min(level_db);
        let snr_db = level_db - self.noise_floor_db();

        self.frames_in_block += 1;
        if self.frames_in_block == FLOOR_BLOCK_FRAMES {
            self.block_minima[self.next_block] = self.current_min;
            self.next_block = (self.next_block + 1) % FLOOR_BLOCKS;
            self.current_min = f32::INFINITY;
            self.frames_in_block = 0;
        }

        1.0 / (1.0 + (-(snr_db - SNR_MIDPOINT_DB) / SNR_SLOPE_DB).exp())
    }
}

/// A stretch of speech on the recording's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechInterval {
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SegmenterConfig {
    /// Probability at which speech starts.
    pub onset: f32,
    /// Probability below which a frame no longer counts as speech. Lower than
    /// `onset`, so a decision does not flicker around a single threshold.
    pub offset: f32,
    /// Shorter bursts are discarded as clicks and knocks.
    pub min_speech_ms: u64,
    /// Silence this long ends an interval; shorter pauses stay inside it.
    pub hangover_ms: u64,
    /// Added on both sides so word onsets and tails are not clipped.
    pub pad_ms: u64,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            onset: 0.6,
            offset: 0.35,
            min_speech_ms: 200,
            hangover_ms: 400,
            pad_ms: 120,
        }
    }
}

/// Turns a stream of per-frame speech probabilities into non-overlapping,
/// time-ordered [`SpeechInterval`]s.
pub struct SpeechSegmenter {
    config: SegmenterConfig,
    /// `(start_ms, end of the last voiced frame)` of the interval being built.
    open: Option<(u64, u64)>,
    last_emitted_end_ms: u64,
}

impl SpeechSegmenter {
    pub fn new(config: SegmenterConfig) -> Self {
        Self {
            config,
            open: None,
            last_emitted_end_ms: 0,
        }
    }

    /// Feeds the probability of the frame covering `start_ms..end_ms`.
    /// Returns an interval once the silence after it has lasted long enough.
    pub fn push(&mut self, probability: f32, start_ms: u64, end_ms: u64) -> Option<SpeechInterval> {
        match &mut self.open {
            None => {
                if probability >= self.config.onset {
                    self.open = Some((start_ms, end_ms));
                }
                None
            }
            Some((_, voiced_end_ms)) => {
                if probability >= self.config.offset {
                    *voiced_end_ms = end_ms;
                    None
                } else if end_ms - *voiced_end_ms >= self.config.hangover_ms {
                    self.close(u64::MAX)
                } else {
                    None
                }
            }
        }
    }

    /// Whether speech is in progress: an interval has started and not closed.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Closes an interval still open when the recording ends at `total_ms`.
    pub fn finish(&mut self, total_ms: u64) -> Option<SpeechInterval> {
        self.close(total_ms)
    }

    fn close(&mut self, limit_ms: u64) -> Option<SpeechInterval> {
        let (start_ms, voiced_end_ms) = self.open.take()?;
        if voiced_end_ms - start_ms < self.config.min_speech_ms {
            return None;
        }
        let start_ms = start_ms
            .saturating_sub(self.config.pad_ms)
            .max(self.last_emitted_end_ms);
        let end_ms = (voiced_end_ms + self.config.pad_ms).min(limit_ms);
        self.last_emitted_end_ms = end_ms;
        Some(SpeechInterval { start_ms, end_ms })
    }
}

#[cfg(test)]
pub(crate) mod test_signals {
    use super::*;

    /// Deterministic white noise at `level_db` dBFS RMS.
    pub struct Noise(pub u32);

    impl Noise {
        pub fn next(&mut self, level_db: f32) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let uniform = (self.0 >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            // A uniform distribution on -1..1 has an RMS of 1/sqrt(3).
            uniform * 3f32.sqrt() * 10f32.powf(level_db / 20.0)
        }
    }

    /// A voiced-sounding tone complex at `level_db` dBFS RMS, sample `i`.
    pub fn voice(i: usize, level_db: f32) -> f32 {
        let t = i as f32 / VAD_SAMPLE_RATE as f32;
        let tone = (t * 220.0 * std::f32::consts::TAU).sin()
            + (t * 440.0 * std::f32::consts::TAU).sin()
            + (t * 1_100.0 * std::f32::consts::TAU).sin();
        // Three unit sines have an RMS of sqrt(3/2).
        tone / 1.5f32.sqrt() * 10f32.powf(level_db / 20.0)
    }
}

#[cfg(test)]
mod tests {
    use super::test_signals::*;
    use super::*;

    /// Runs the detector over `frames` frames and returns each probability.
    /// `signal(sample_index)` supplies the samples.
    fn run(vad: &mut EnergyVad, frames: usize, mut signal: impl FnMut(usize) -> f32) -> Vec<f32> {
        (0..frames)
            .map(|f| {
                let frame: Vec<f32> = (0..VAD_FRAME_SAMPLES)
                    .map(|i| signal(f * VAD_FRAME_SAMPLES + i))
                    .collect();
                vad.speech_probability(&frame)
            })
            .collect()
    }

    #[test]
    fn steady_room_noise_is_not_speech() {
        let mut noise = Noise(1);
        let probs = run(&mut EnergyVad::new(), 150, |_| noise.next(-50.0));
        assert!(probs.iter().all(|&p| p < 0.2), "{probs:?}");
    }

    #[test]
    fn digital_silence_is_not_speech() {
        let probs = run(&mut EnergyVad::new(), 20, |_| 0.0);
        assert!(probs.iter().all(|&p| p == 0.0));
    }

    #[test]
    fn voice_over_noise_is_speech_and_silence_after_it_is_not() {
        let mut noise = Noise(2);
        // Frames 0..30 noise, 30..60 voice, 60..90 noise.
        let probs = run(&mut EnergyVad::new(), 90, |i| {
            let frame = i / VAD_FRAME_SAMPLES;
            let voiced = (30..60).contains(&frame);
            noise.next(-50.0) + if voiced { voice(i, -20.0) } else { 0.0 }
        });
        assert!(probs[..30].iter().all(|&p| p < 0.2), "before");
        assert!(probs[31..59].iter().all(|&p| p > 0.9), "during");
        assert!(probs[61..].iter().all(|&p| p < 0.2), "after");
    }

    #[test]
    fn a_leading_muted_stretch_does_not_make_the_room_sound_like_speech() {
        let mut noise = Noise(3);
        let probs = run(&mut EnergyVad::new(), 60, |i| {
            if i < 5 * VAD_FRAME_SAMPLES {
                0.0
            } else {
                noise.next(-55.0)
            }
        });
        assert!(probs.iter().all(|&p| p < 0.2), "{probs:?}");
    }

    #[test]
    fn adapts_when_the_room_gets_louder() {
        let mut noise = Noise(4);
        let mut vad = EnergyVad::new();
        run(&mut vad, 60, |_| noise.next(-60.0));
        let probs = run(&mut vad, 200, |_| noise.next(-40.0));
        assert!(probs[0] > 0.9, "a 20 dB jump first reads as speech");
        // Within three seconds the louder level is the new floor.
        assert!(probs[94..].iter().all(|&p| p < 0.2));
    }

    fn segment(probs: &[f32]) -> Vec<SpeechInterval> {
        let mut seg = SpeechSegmenter::new(SegmenterConfig::default());
        let mut out: Vec<_> = probs
            .iter()
            .enumerate()
            .filter_map(|(i, &p)| seg.push(p, i as u64 * 32, (i as u64 + 1) * 32))
            .collect();
        out.extend(seg.finish(probs.len() as u64 * 32));
        out
    }

    fn pattern(parts: &[(usize, f32)]) -> Vec<f32> {
        parts.iter().flat_map(|&(n, p)| vec![p; n]).collect()
    }

    #[test]
    fn pads_an_interval_and_waits_out_the_hangover() {
        // Speech in frames 10..30, i.e. 320..960 ms.
        let got = segment(&pattern(&[(10, 0.0), (20, 0.9), (40, 0.0)]));
        assert_eq!(
            got,
            [SpeechInterval {
                start_ms: 200,
                end_ms: 1080
            }]
        );
    }

    #[test]
    fn short_pauses_stay_inside_one_interval() {
        // A 256 ms pause is shorter than the 400 ms hangover.
        let got = segment(&pattern(&[
            (5, 0.0),
            (20, 0.9),
            (8, 0.0),
            (20, 0.9),
            (40, 0.0),
        ]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].end_ms, (5 + 20 + 8 + 20) * 32 + 120);
    }

    #[test]
    fn long_pauses_split_intervals_without_overlap() {
        let got = segment(&pattern(&[(20, 0.9), (15, 0.0), (20, 0.9), (40, 0.0)]));
        assert_eq!(got.len(), 2);
        assert!(got[0].end_ms <= got[1].start_ms);
    }

    #[test]
    fn clicks_are_discarded() {
        assert!(segment(&pattern(&[(10, 0.0), (3, 0.9), (40, 0.0)])).is_empty());
    }

    #[test]
    fn hysteresis_holds_speech_between_the_thresholds() {
        // 0.5 is below the onset but above the offset: it cannot start an
        // interval, but it keeps one going.
        assert!(segment(&pattern(&[(40, 0.5)])).is_empty());
        let got = segment(&pattern(&[(10, 0.9), (30, 0.5), (40, 0.0)]));
        assert_eq!(got[0].end_ms, 40 * 32 + 120);
    }

    #[test]
    fn speech_running_to_the_end_is_closed_at_the_end() {
        let got = segment(&pattern(&[(10, 0.0), (20, 0.9)]));
        assert_eq!(
            got,
            [SpeechInterval {
                start_ms: 200,
                end_ms: 960
            }]
        );
    }
}
