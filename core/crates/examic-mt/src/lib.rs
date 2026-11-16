//! Russian to English translation for Examic.
//!
//! The model is Helsinki-NLP's `opus-mt_tiny_rus-eng` (Marian, 25M
//! parameters), converted to int8 ONNX by `tools/models/convert_opus_mt.py`
//! and run greedily with ONNX Runtime ([`OpusMt`], feature `native`).
//!
//! Tokenization is [`SpmTokenizer`], a pure-Rust SentencePiece unigram
//! encoder: it needs no C++ library, so it cross-compiles anywhere, and its
//! output is tested against the Python tokenizer's.

mod sentences;
mod spm;

#[cfg(feature = "native")]
mod opus_mt;

#[cfg(feature = "native")]
pub use opus_mt::{OpusMt, init_runtime};
pub use sentences::split_sentences;
pub use spm::{SpmError, SpmTokenizer};

#[derive(Debug, thiserror::Error)]
pub enum MtError {
    #[error("translation model could not be loaded: {0}")]
    Load(String),
    #[error("translation failed: {0}")]
    Run(String),
    #[error("translation is not available in this build")]
    Unavailable,
    #[error(transparent)]
    Tokenizer(#[from] SpmError),
}

/// Translates Russian text into English.
pub trait Translator: Send {
    fn translate(&mut self, text: &str) -> Result<String, MtError>;
}

/// Translates sentence by sentence and joins the results.
///
/// The model was trained on single sentences; given two at once it tends to
/// drop one of them, so longer text is always split first.
pub fn translate_text(translator: &mut dyn Translator, text: &str) -> Result<String, MtError> {
    let mut out = Vec::new();
    for sentence in split_sentences(text) {
        let translated = translator.translate(sentence)?;
        if !translated.trim().is_empty() {
            out.push(translated.trim().to_owned());
        }
    }
    Ok(out.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo(Vec<String>);

    impl Translator for Echo {
        fn translate(&mut self, text: &str) -> Result<String, MtError> {
            self.0.push(text.to_owned());
            Ok(format!("<{text}>"))
        }
    }

    #[test]
    fn translates_one_sentence_at_a_time() {
        let mut echo = Echo(Vec::new());
        let out = translate_text(&mut echo, "Это не так. Конечно, нет!").unwrap();
        assert_eq!(echo.0, ["Это не так.", "Конечно, нет!"]);
        assert_eq!(out, "<Это не так.> <Конечно, нет!>");
        assert_eq!(translate_text(&mut echo, "  ").unwrap(), "");
    }
}
