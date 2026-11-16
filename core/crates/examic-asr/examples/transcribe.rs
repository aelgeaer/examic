//! Desktop check of the speech pipeline on real audio.
//!
//!     cargo run --release -p examic-asr --features native --example transcribe -- \
//!         gigaam-v3-e2e-ctc-Q8_0.gguf fleurs/ru_ru [clips]
//!
//! The second argument is a FLEURS split directory (`dev.tsv` plus `dev/*.wav`,
//! from huggingface.co/datasets/google/fleurs). Each clip is transcribed through
//! `GigaAm` and `segments_from_tokens` exactly as the app does; the word error
//! rate against the reference and the speed are printed. Then the clips are
//! joined, with silence between them, and transcribed as one long recording
//! through `plan_windows`, to check the windowing end to end.

use std::path::Path;
use std::time::Instant;

use examic_asr::{GigaAm, Transcriber, ms_to_samples, plan_windows, samples_to_ms};

fn load_wav(path: &Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    assert_eq!(reader.spec().sample_rate, 16_000, "FLEURS clips are 16 kHz");
    match reader.spec().sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(Result::unwrap).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
    }
}

/// Lower case, `ё` as `е`, punctuation dropped: the usual FLEURS scoring.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace('ё', "е")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(String::from)
        .collect()
}

fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut diagonal = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let above = row[j];
            row[j] = (row[j] + 1)
                .min(row[j - 1] + 1)
                .min(diagonal + usize::from(a[i - 1] != b[j - 1]));
            diagonal = above;
        }
    }
    row[b.len()]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (model_path, dir) = (Path::new(&args[1]), Path::new(&args[2]));
    let clips: usize = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(20);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(4));

    let started = Instant::now();
    let model = GigaAm::load(model_path).expect("load model");
    let mut asr = model.transcriber(threads).expect("session");
    println!("model loaded in {:?}, {threads} threads", started.elapsed());

    let tsv = std::fs::read_to_string(dir.join("dev.tsv")).expect("dev.tsv");
    let (mut errors, mut total, mut audio_ms, mut busy) = (0, 0, 0i64, 0.0);
    let mut long = Vec::new();
    let mut speech = Vec::new();
    for line in tsv.lines().step_by(17).take(clips) {
        let fields: Vec<&str> = line.split('\t').collect();
        let pcm = load_wav(&dir.join("dev").join(fields[1]));
        let t = Instant::now();
        let segments = asr.transcribe(&pcm).expect("transcribe");
        busy += t.elapsed().as_secs_f64();
        audio_ms += samples_to_ms(pcm.len());
        let hypothesis: String = segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let (reference, hypothesis_words) = (words(fields[2]), words(&hypothesis));
        errors += edit_distance(&reference, &hypothesis_words);
        total += reference.len();
        println!(
            "REF {}\nHYP {hypothesis}  [{} segment(s)]\n",
            fields[2],
            segments.len()
        );

        // The long recording: this clip, then 3 s of silence.
        let start = samples_to_ms(long.len());
        speech.push((start, start + samples_to_ms(pcm.len())));
        long.extend_from_slice(&pcm);
        long.resize(long.len() + ms_to_samples(3_000), 0.0);
    }
    println!(
        "WER {:.2}% over {total} words; {:.1}x faster than real time",
        100.0 * errors as f64 / total as f64,
        audio_ms as f64 / 1000.0 / busy
    );

    let duration_ms = samples_to_ms(long.len());
    let windows = plan_windows(&speech, duration_ms);
    let t = Instant::now();
    let mut segments = Vec::new();
    for w in &windows {
        let pcm = &long[ms_to_samples(w.start_ms)..ms_to_samples(w.end_ms).min(long.len())];
        for mut s in asr.transcribe(pcm).expect("transcribe") {
            s.start_ms += w.start_ms;
            s.end_ms += w.start_ms;
            segments.push(s);
        }
    }
    println!(
        "\nlong recording: {:.0} s in {} windows, transcribed in {:.1} s, {} segments",
        duration_ms as f64 / 1000.0,
        windows.len(),
        t.elapsed().as_secs_f64(),
        segments.len()
    );
    for s in segments.iter().take(8) {
        println!("  [{:>6} - {:>6}] {}", s.start_ms, s.end_ms, s.text);
    }
    // Every segment must fall inside a clip, not in the silences between.
    let misplaced = segments
        .iter()
        .filter(|s| {
            !speech
                .iter()
                .any(|&(a, b)| s.start_ms >= a - 300 && s.end_ms <= b + 300)
        })
        .count();
    println!("segments outside their clip: {misplaced}");
}
