//! Checks against the Python reference, using the fixtures that
//! `tools/models/convert_opus_mt.py` writes.
//!
//! The tokenizer test needs the model bundle's `spm.model` and `vocab.json`;
//! the translation test also needs its ONNX files and an ONNX Runtime library.
//! Both are skipped unless these are set:
//!
//!     EXAMIC_MT_MODEL_DIR=/path/to/opus-mt-ru-en
//!     ORT_DYLIB_PATH=/path/to/libonnxruntime.so   (translation only)
//!
//! e.g. `cargo test -p examic-mt --features native --release`.

use std::path::{Path, PathBuf};

use examic_mt::SpmTokenizer;

fn model_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("EXAMIC_MT_MODEL_DIR")?);
    dir.join("spm.model").exists().then_some(dir)
}

fn fixture(name: &str) -> Vec<serde_json::Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn tokenizer_matches_marian_tokenizer() {
    let Some(dir) = model_dir() else {
        eprintln!("skipped: EXAMIC_MT_MODEL_DIR not set");
        return;
    };
    let tokenizer = SpmTokenizer::load(&dir.join("spm.model"), &dir.join("vocab.json")).unwrap();
    for case in fixture("tokenizer_fixture.json") {
        let text = case["text"].as_str().unwrap();
        let expected: Vec<i64> = case["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(tokenizer.encode(text, 512), expected, "{text}");
    }
    for case in fixture("translation_fixture.json") {
        let ids: Vec<i64> = case["output_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(
            tokenizer.decode(&ids),
            case["translation"].as_str().unwrap()
        );
    }
}

#[cfg(feature = "native")]
#[test]
fn translations_match_the_python_reference() {
    use examic_mt::{OpusMt, Translator, init_runtime};

    let (Some(dir), Some(library)) = (model_dir(), std::env::var_os("ORT_DYLIB_PATH")) else {
        eprintln!("skipped: EXAMIC_MT_MODEL_DIR or ORT_DYLIB_PATH not set");
        return;
    };
    init_runtime(Path::new(&library)).unwrap();
    let mut mt = OpusMt::load(&dir, 2).unwrap();
    let cases = fixture("translation_fixture.json");
    let mut same = 0;
    for case in &cases {
        let text = case["text"].as_str().unwrap();
        let translation = mt.translate(text).unwrap();
        println!("{text}\n  -> {translation}");
        same += usize::from(translation == case["translation"].as_str().unwrap());
    }
    // Same graphs, same greedy loop; a different ONNX Runtime build may round
    // differently, so allow the odd near-tie to go the other way.
    assert!(same + 1 >= cases.len(), "{same}/{} identical", cases.len());
}
