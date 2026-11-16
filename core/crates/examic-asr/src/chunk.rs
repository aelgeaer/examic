//! Which audio goes to the model, and in what pieces.
//!
//! The model is run on speech only, in windows of up to half a minute: short
//! enough to keep memory and latency low, long enough to give the model
//! context. Window edges are placed in the silences the voice activity
//! detector found, so words are not cut.

use crate::{ms_to_samples, samples_to_ms};

/// Longest window that several speech intervals are grouped into.
pub const MAX_WINDOW_MS: i64 = 30_000;
/// Added before and after each window, so word onsets and tails are kept.
pub const WINDOW_PAD_MS: i64 = 200;
/// A single uninterrupted interval longer than this (a noisy room the
/// detector never hears as silent) is cut into pieces of at most this length.
/// The model handles long input itself, so this only bounds memory.
const MAX_INTERVAL_MS: i64 = 90_000;
/// Without speech intervals (a recovered recording), the whole recording is
/// cut into windows of this length.
const BLIND_WINDOW_MS: i64 = 25_000;

/// A piece of the recording to transcribe: `start_ms..end_ms` on its timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Groups speech intervals (sorted, non-overlapping) into windows of at most
/// [`MAX_WINDOW_MS`], each padded by [`WINDOW_PAD_MS`] without overlapping its
/// neighbours or leaving `0..duration_ms`.
///
/// With no intervals at all, the whole recording is covered blindly.
pub fn plan_windows(speech: &[(i64, i64)], duration_ms: i64) -> Vec<Window> {
    if duration_ms <= 0 {
        return Vec::new();
    }
    let mut pieces: Vec<(i64, i64)> = Vec::new();
    if speech.is_empty() {
        let mut start = 0;
        while start < duration_ms {
            let end = (start + BLIND_WINDOW_MS).min(duration_ms);
            pieces.push((start, end));
            start = end;
        }
        return pieces
            .into_iter()
            .map(|(start_ms, end_ms)| Window { start_ms, end_ms })
            .collect();
    }

    for &(start, end) in speech {
        let (start, end) = (start.clamp(0, duration_ms), end.clamp(0, duration_ms));
        if end <= start {
            continue;
        }
        // Split over-long intervals into equal parts.
        let parts = ((end - start + MAX_INTERVAL_MS - 1) / MAX_INTERVAL_MS).max(1);
        let step = (end - start) / parts;
        for i in 0..parts {
            let s = start + i * step;
            let e = if i == parts - 1 { end } else { s + step };
            pieces.push((s, e));
        }
    }

    // Greedily merge neighbours while the window stays short enough.
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (s, e) in pieces {
        match merged.last_mut() {
            Some(last) if e - last.0 <= MAX_WINDOW_MS => last.1 = e,
            _ => merged.push((s, e)),
        }
    }

    let mut windows = Vec::with_capacity(merged.len());
    let mut previous_end = 0;
    for (i, &(s, e)) in merged.iter().enumerate() {
        let start_ms = (s - WINDOW_PAD_MS).max(previous_end).max(0);
        // Pad into the following gap, but at most halfway, so the next
        // window gets its share of the gap too.
        let limit = merged.get(i + 1).map_or(duration_ms, |n| e + (n.0 - e) / 2);
        let end_ms = (e + WINDOW_PAD_MS).min(limit).min(duration_ms);
        windows.push(Window { start_ms, end_ms });
        previous_end = end_ms;
    }
    windows
}

/// Cuts speech out of a live 16 kHz stream as it is being recorded.
///
/// Fed with every sample of the recording (in order, starting at zero) and
/// with each speech interval once the detector has closed it, it hands out
/// the padded audio of that speech, ready for the model. Intervals that follow
/// each other closely are held back and merged, because the detector also
/// closes intervals at short pauses inside a sentence, and the model
/// transcribes a whole sentence better than its pieces. Audio that cannot
/// belong to speech any more is dropped, so memory stays bounded however long
/// the recording runs.
pub struct LiveChunker {
    buffer: Vec<f32>,
    /// Timeline sample index of `buffer[0]`.
    buffer_start: usize,
    /// Nothing before this sample is handed out again.
    emitted_until: usize,
    /// Closed intervals not yet handed out, merged: `(start_ms, end_ms)`.
    pending: Option<(i64, i64)>,
}

/// While no speech is in progress, this much recent audio is kept: enough to
/// cover the detector's onset delay and the padding before an interval.
const IDLE_KEEP_MS: i64 = 2_000;
/// Held-back speech is handed out once it is this long...
const LIVE_MIN_MS: i64 = 15_000;
/// ...or once the silence after it has lasted this long.
const LIVE_PAUSE_MS: i64 = 1_200;

impl Default for LiveChunker {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveChunker {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            buffer_start: 0,
            emitted_until: 0,
            pending: None,
        }
    }

    fn end(&self) -> usize {
        self.buffer_start + self.buffer.len()
    }

    /// Appends audio. `speech_open` says whether the detector was inside a
    /// speech interval when this audio began; an interval that closes during
    /// it is reported through [`speech`](Self::speech) right after this call.
    /// Returns a window when held-back speech is due, or when speech has gone
    /// on for so long that it is transcribed in parts.
    pub fn push(&mut self, pcm: &[f32], speech_open: bool) -> Option<(Window, Vec<f32>)> {
        self.buffer.extend_from_slice(pcm);
        if !speech_open {
            let mut keep_from = self.end().saturating_sub(ms_to_samples(IDLE_KEEP_MS));
            if let Some((start_ms, end_ms)) = self.pending {
                if samples_to_ms(self.end()) - end_ms >= LIVE_PAUSE_MS {
                    return self.flush();
                }
                keep_from = keep_from.min(ms_to_samples(start_ms - WINDOW_PAD_MS));
            }
            self.drop_before(keep_from.max(self.buffer_start));
            return None;
        }
        let from = self.emitted_until.max(self.buffer_start);
        if self.end() - from >= ms_to_samples(MAX_WINDOW_MS) {
            self.pending = None;
            let end = self.end();
            return Some(self.take(from, end));
        }
        None
    }

    /// The detector closed a speech interval. Returns audio if held-back
    /// speech is now long enough to hand out.
    pub fn speech(&mut self, start_ms: i64, end_ms: i64) -> Option<(Window, Vec<f32>)> {
        let mut ready = None;
        self.pending = match self.pending {
            Some((first, _)) if end_ms - first <= MAX_WINDOW_MS => Some((first, end_ms)),
            Some(_) => {
                ready = self.flush();
                Some((start_ms, end_ms))
            }
            None => Some((start_ms, end_ms)),
        };
        if ready.is_none()
            && self
                .pending
                .is_some_and(|(first, last)| last - first >= LIVE_MIN_MS)
        {
            ready = self.flush();
        }
        ready
    }

    /// The recording has ended: hands out whatever speech is held back.
    pub fn finish(&mut self) -> Option<(Window, Vec<f32>)> {
        self.flush()
    }

    fn flush(&mut self) -> Option<(Window, Vec<f32>)> {
        let (start_ms, end_ms) = self.pending.take()?;
        let start = ms_to_samples(start_ms - WINDOW_PAD_MS)
            .max(self.emitted_until)
            .max(self.buffer_start);
        let end = ms_to_samples(end_ms + WINDOW_PAD_MS).min(self.end());
        if end <= start {
            return None;
        }
        let window = self.take(start, end);
        // The next interval starts after this one ended; keep the audio from
        // there, padding included, in case it follows closely.
        let keep_from = ms_to_samples(end_ms).max(self.buffer_start).min(self.end());
        self.drop_before(keep_from);
        Some(window)
    }

    fn take(&mut self, start: usize, end: usize) -> (Window, Vec<f32>) {
        let pcm = self.buffer[start - self.buffer_start..end - self.buffer_start].to_vec();
        self.emitted_until = end;
        (
            Window {
                start_ms: samples_to_ms(start),
                end_ms: samples_to_ms(end),
            },
            pcm,
        )
    }

    fn drop_before(&mut self, sample: usize) {
        let n = sample
            .saturating_sub(self.buffer_start)
            .min(self.buffer.len());
        if n > 0 {
            self.buffer.drain(..n);
            self.buffer_start += n;
        }
    }

    /// Samples currently held, for tests and diagnostics.
    pub fn buffered_samples(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(start_ms: i64, end_ms: i64) -> Window {
        Window { start_ms, end_ms }
    }

    #[test]
    fn groups_nearby_speech_into_windows_of_at_most_thirty_seconds() {
        let speech = [
            (1_000, 5_000),
            (6_000, 20_000),
            (21_000, 29_000),
            (40_000, 45_000),
        ];
        let windows = plan_windows(&speech, 60_000);
        assert_eq!(windows, [w(800, 29_200), w(39_800, 45_200)]);
        for win in &windows {
            assert!(win.end_ms - win.start_ms <= MAX_WINDOW_MS + 2 * WINDOW_PAD_MS);
        }
    }

    #[test]
    fn padding_never_overlaps_a_neighbour_or_leaves_the_recording() {
        // 20 s + 15 s cannot share a window; the 100 ms gap is split.
        let windows = plan_windows(&[(50, 20_000), (20_100, 35_000)], 35_050);
        assert_eq!(windows, [w(0, 20_050), w(20_050, 35_050)]);
    }

    #[test]
    fn a_very_long_interval_is_cut_into_parts() {
        let windows = plan_windows(&[(0, 200_000)], 200_000);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].start_ms, 0);
        assert_eq!(windows[2].end_ms, 200_000);
        for pair in windows.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms);
        }
    }

    #[test]
    fn without_speech_intervals_the_whole_recording_is_covered() {
        assert_eq!(
            plan_windows(&[], 60_000),
            [w(0, 25_000), w(25_000, 50_000), w(50_000, 60_000)]
        );
        assert!(plan_windows(&[], 0).is_empty());
    }

    /// 16 kHz samples whose value is their own timeline position in ms, so a
    /// window's content shows exactly where it came from.
    fn timeline(from_ms: i64, to_ms: i64) -> Vec<f32> {
        (ms_to_samples(from_ms)..ms_to_samples(to_ms))
            .map(|i| samples_to_ms(i) as f32)
            .collect()
    }

    #[test]
    fn live_chunks_carry_the_padded_audio_of_each_interval() {
        let mut chunker = LiveChunker::new();
        assert!(chunker.push(&timeline(0, 3_000), false).is_none());
        // Speech from 3.0 s to 4.5 s; the detector closes it 400 ms later.
        assert!(chunker.push(&timeline(3_000, 4_400), true).is_none());
        assert!(chunker.push(&timeline(4_400, 4_900), true).is_none());
        // Short, so it is held back until the pause after it is long enough.
        assert!(chunker.speech(3_000, 4_500).is_none());
        assert!(chunker.push(&timeline(4_900, 5_600), false).is_none());
        let (window, pcm) = chunker.push(&timeline(5_600, 5_800), false).unwrap();
        assert_eq!(window, w(2_800, 4_700));
        assert_eq!(pcm.first().copied(), Some(2_800.0));
        assert_eq!(pcm.last().copied(), Some(4_699.0));
        assert_eq!(pcm.len(), ms_to_samples(1_900));
    }

    #[test]
    fn idle_audio_is_dropped_so_memory_stays_bounded() {
        let mut chunker = LiveChunker::new();
        for s in 0..600 {
            chunker.push(&timeline(s * 1000, (s + 1) * 1000), false);
        }
        assert!(chunker.buffered_samples() <= ms_to_samples(IDLE_KEEP_MS));
        assert!(chunker.speech(0, 500).is_none());
        assert!(chunker.finish().is_none(), "long gone");
    }

    #[test]
    fn pieces_of_a_sentence_are_merged_into_one_window() {
        let mut chunker = LiveChunker::new();
        chunker.push(&timeline(0, 1_000), false);
        // Three pieces with short pauses are held back...
        chunker.push(&timeline(1_000, 3_500), true);
        assert!(chunker.speech(1_000, 3_000).is_none());
        chunker.push(&timeline(3_500, 5_800), true);
        assert!(chunker.speech(3_600, 5_300).is_none());
        chunker.push(&timeline(5_800, 7_900), true);
        assert!(chunker.speech(5_900, 7_400).is_none());
        // ...until a real pause follows.
        assert!(chunker.push(&timeline(7_900, 8_500), false).is_none());
        let (window, pcm) = chunker.push(&timeline(8_500, 8_700), false).unwrap();
        assert_eq!(window, w(800, 7_600));
        assert_eq!(pcm[0], 800.0);
        // A last short piece comes out when the recording ends.
        chunker.push(&timeline(8_700, 10_000), true);
        assert!(chunker.speech(9_000, 9_500).is_none());
        let (last, _) = chunker.finish().unwrap();
        assert_eq!(last, w(8_800, 9_700));
    }

    #[test]
    fn long_runs_of_speech_come_out_without_waiting_for_a_pause() {
        let mut chunker = LiveChunker::new();
        let mut out = None;
        for s in 0..20 {
            chunker.push(&timeline(s * 1000, (s + 1) * 1000), true);
            // A short gap every second second, never long enough to flush.
            if s % 2 == 1 {
                out = out.or(chunker.speech((s - 1) * 1000, s * 1000 + 500));
            }
        }
        let (window, _) = out.unwrap();
        assert_eq!(window.start_ms, 0);
        assert!(
            window.end_ms >= 15_000 && window.end_ms <= 17_000,
            "{window:?}"
        );
    }

    #[test]
    fn endless_speech_is_handed_out_in_parts_without_overlap() {
        let mut chunker = LiveChunker::new();
        let mut windows = Vec::new();
        for s in 0..70 {
            if let Some((win, pcm)) = chunker.push(&timeline(s * 1000, (s + 1) * 1000), true) {
                assert_eq!(pcm.len(), ms_to_samples(win.end_ms - win.start_ms));
                windows.push(win);
            }
        }
        assert_eq!(windows, [w(0, 30_000), w(30_000, 60_000)]);
        // When the interval finally closes, only the rest comes out.
        let (last, pcm) = chunker.speech(0, 70_000).unwrap();
        assert!(chunker.finish().is_none());
        assert_eq!(last, w(60_000, 70_000));
        assert_eq!(pcm[0], 60_000.0);
    }

    #[test]
    fn consecutive_windows_do_not_share_audio() {
        let mut chunker = LiveChunker::new();
        chunker.push(&timeline(0, 2_000), true);
        assert!(chunker.speech(500, 1_000).is_none());
        let (a, _) = chunker.push(&timeline(2_000, 2_300), false).unwrap();
        chunker.push(&timeline(2_300, 9_000), true);
        assert!(chunker.speech(2_300, 8_500).is_none());
        let (b, pcm) = chunker.finish().unwrap();
        assert_eq!(a, w(300, 1_200));
        assert_eq!(b, w(2_100, 8_700));
        assert_eq!(pcm[0], 2_100.0);
    }
}
