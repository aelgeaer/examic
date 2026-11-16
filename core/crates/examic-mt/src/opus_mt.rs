//! Opus-MT on ONNX Runtime: encoder once, then the merged decoder one token
//! at a time with its key/value cache.

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use ort::environment::Environment;
use ort::session::{Session, SessionInputValue, builder::GraphOptimizationLevel};
use ort::value::{DynValue, Tensor};

use crate::{MtError, SpmTokenizer, Translator};

/// Longest output, in tokens.
const MAX_NEW_TOKENS: usize = 256;

static ENVIRONMENT: OnceLock<Environment> = OnceLock::new();
static INIT: Mutex<()> = Mutex::new(());

/// Loads ONNX Runtime from `library` (on Android just `libonnxruntime.so`,
/// which the onnxruntime-android AAR packages next to the app's own native
/// libraries). Later calls do nothing.
pub fn init_runtime(library: &Path) -> Result<(), MtError> {
    let _guard = INIT.lock().unwrap_or_else(|e| e.into_inner());
    if ENVIRONMENT.get().is_some() {
        return Ok(());
    }
    let environment = ort::init_from(library)
        .map_err(|e| MtError::Load(format!("ONNX Runtime ({}): {e}", library.display())))?
        .with_name("examic")
        .build()
        .map_err(|e| MtError::Load(e.to_string()))?;
    let _ = ENVIRONMENT.set(environment);
    Ok(())
}

fn environment() -> Result<&'static Environment, MtError> {
    ENVIRONMENT
        .get()
        .ok_or_else(|| MtError::Load("ONNX Runtime has not been loaded".into()))
}

/// The ids and sizes the decoding loop needs, from the Marian `config.json`.
struct Config {
    decoder_start: i64,
    pad: i64,
    eos: i64,
    layers: usize,
    heads: usize,
    head_dim: usize,
    max_positions: usize,
}

impl Config {
    fn load(path: &Path) -> Result<Self, MtError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| MtError::Load(format!("{}: {e}", path.display())))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| MtError::Load(e.to_string()))?;
        let int = |key: &str| {
            json[key]
                .as_i64()
                .ok_or_else(|| MtError::Load(format!("config.json has no {key}")))
        };
        let heads = int("decoder_attention_heads")? as usize;
        Ok(Self {
            decoder_start: int("decoder_start_token_id")?,
            pad: int("pad_token_id")?,
            eos: int("eos_token_id")?,
            layers: int("decoder_layers")? as usize,
            heads,
            head_dim: int("d_model")? as usize / heads,
            max_positions: int("max_position_embeddings")? as usize,
        })
    }
}

/// A loaded translation model.
pub struct OpusMt {
    encoder: Session,
    decoder: Session,
    tokenizer: SpmTokenizer,
    config: Config,
}

fn run_error(e: impl std::fmt::Display) -> MtError {
    MtError::Run(e.to_string())
}

impl OpusMt {
    /// Loads the bundle written by `convert_opus_mt.py` from `dir`, using
    /// `threads` CPU threads per model. Call [`init_runtime`] first.
    pub fn load(dir: &Path, threads: usize) -> Result<Self, MtError> {
        let environment = environment()?;
        let session = |file: &str| -> Result<Session, MtError> {
            let load = |e: ort::Error| MtError::Load(format!("{file}: {e}"));
            Session::builder(environment)
                .map_err(load)?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| load(e.into()))?
                .with_intra_threads(threads.max(1))
                .map_err(|e| load(e.into()))?
                .commit_from_file(dir.join(file))
                .map_err(load)
        };
        Ok(Self {
            encoder: session("encoder.onnx")?,
            decoder: session("decoder.onnx")?,
            tokenizer: SpmTokenizer::load(&dir.join("spm.model"), &dir.join("vocab.json"))?,
            config: Config::load(&dir.join("config.json"))?,
        })
    }

    /// Output token ids for input ids, by greedy decoding.
    pub fn generate(&mut self, input_ids: &[i64]) -> Result<Vec<i64>, MtError> {
        let c = &self.config;
        let len = input_ids.len();
        let ids = Tensor::from_array(([1usize, len], input_ids.to_vec())).map_err(run_error)?;
        let mask = Tensor::from_array(([1usize, len], vec![1i64; len])).map_err(run_error)?;
        let mut encoded = self
            .encoder
            .run(ort::inputs! { "input_ids" => ids, "attention_mask" => &mask })
            .map_err(run_error)?;
        let hidden: DynValue = encoded
            .remove("last_hidden_state")
            .ok_or_else(|| run_error("encoder produced no output"))?;
        drop(encoded);

        let empty = || -> Result<DynValue, MtError> {
            Ok(
                Tensor::from_array(([1usize, c.heads, 0, c.head_dim], Vec::<f32>::new()))
                    .map_err(run_error)?
                    .into_dyn(),
            )
        };
        // past[layer][decoder key, decoder value, encoder key, encoder value]
        let mut past: Vec<[DynValue; 4]> = (0..c.layers)
            .map(|_| Ok([empty()?, empty()?, empty()?, empty()?]))
            .collect::<Result<_, MtError>>()?;
        const KINDS: [&str; 4] = [
            "decoder.key",
            "decoder.value",
            "encoder.key",
            "encoder.value",
        ];

        let mut token = c.decoder_start;
        let mut output = Vec::new();
        let budget = MAX_NEW_TOKENS.min(c.max_positions.saturating_sub(1));
        for step in 0..budget {
            let mut inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)> = vec![
                (
                    "input_ids".into(),
                    Tensor::from_array(([1usize, 1], vec![token]))
                        .map_err(run_error)?
                        .into(),
                ),
                ("encoder_attention_mask".into(), (&mask).into()),
                ("encoder_hidden_states".into(), (&hidden).into()),
                (
                    "use_cache_branch".into(),
                    Tensor::from_array(([1usize], vec![step > 0]))
                        .map_err(run_error)?
                        .into(),
                ),
            ];
            for (layer, values) in past.iter().enumerate() {
                for (kind, value) in KINDS.iter().zip(values) {
                    inputs.push((
                        format!("past_key_values.{layer}.{kind}").into(),
                        value.into(),
                    ));
                }
            }
            let mut outputs = self.decoder.run(inputs).map_err(run_error)?;

            let (shape, logits) = outputs["logits"]
                .try_extract_tensor::<f32>()
                .map_err(run_error)?;
            let vocab = *shape
                .last()
                .ok_or_else(|| run_error("logits have no shape"))? as usize;
            let last = &logits[logits.len() - vocab..];
            token = last
                .iter()
                .enumerate()
                // generation_config.json: bad_words_ids = [[pad]].
                .filter(|&(id, _)| id as i64 != c.pad)
                .fold((c.eos, f32::NEG_INFINITY), |best, (id, &score)| {
                    if score > best.1 {
                        (id as i64, score)
                    } else {
                        best
                    }
                })
                .0;
            if token == c.eos {
                break;
            }
            output.push(token);

            for (layer, values) in past.iter_mut().enumerate() {
                // Cross-attention keys and values are computed on the first
                // step only; later steps return them unchanged or empty.
                let kinds = if step == 0 { 4 } else { 2 };
                for (slot, kind) in KINDS.iter().enumerate().take(kinds) {
                    values[slot] = outputs
                        .remove(format!("present.{layer}.{kind}"))
                        .ok_or_else(|| {
                            run_error(format!("decoder produced no present.{layer}.{kind}"))
                        })?;
                }
            }
        }
        Ok(output)
    }
}

impl Translator for OpusMt {
    fn translate(&mut self, text: &str) -> Result<String, MtError> {
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        let ids = self.tokenizer.encode(text, self.config.max_positions);
        let output = self.generate(&ids)?;
        Ok(self.tokenizer.decode(&output))
    }
}
