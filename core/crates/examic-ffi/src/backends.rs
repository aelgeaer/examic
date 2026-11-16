//! Where the speech and translation models come from.
//!
//! The engine asks a [`Backends`] for a transcriber or translator each time it
//! starts a piece of work. In the app that is [`ModelBackends`], which loads
//! the bundled model files; the engine's tests substitute fakes.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use examic_asr::{AsrError, Transcriber};
use examic_mt::{MtError, Translator};

pub trait Backends: Send + Sync + 'static {
    /// A transcriber for one thread's use. `live` asks for one that leaves
    /// most of the CPU to the recording in progress.
    fn transcriber(&self, live: bool) -> Result<Box<dyn Transcriber>, AsrError>;
    fn translator(&self) -> Result<Box<dyn Translator>, MtError>;
}

/// Model locations, set once the app has unpacked its bundled models.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(any(target_os = "android", feature = "native")), allow(dead_code))]
pub struct ModelPaths {
    /// The GigaAM GGUF file.
    pub asr_model: Option<PathBuf>,
    /// The directory of the Opus-MT bundle.
    pub mt_dir: Option<PathBuf>,
}

#[derive(Default)]
pub struct ModelBackends {
    paths: Mutex<ModelPaths>,
    /// Loaded once and shared: the weights are memory-mapped and every
    /// transcriber is a session on the same model.
    #[cfg(any(target_os = "android", feature = "native"))]
    asr: Mutex<Option<examic_asr::GigaAm>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ModelBackends {
    pub fn set_paths(&self, paths: ModelPaths) {
        *lock(&self.paths) = paths;
        #[cfg(any(target_os = "android", feature = "native"))]
        {
            *lock(&self.asr) = None;
        }
    }

    #[cfg_attr(not(any(target_os = "android", feature = "native")), allow(dead_code))]
    pub fn paths(&self) -> ModelPaths {
        lock(&self.paths).clone()
    }
}

/// CPU threads for model work: leave one core to the rest of the phone, and
/// beyond four the gain on phone cores is small.
#[cfg(any(target_os = "android", feature = "native"))]
fn threads(live: bool) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    if live {
        2
    } else {
        cores.saturating_sub(1).clamp(1, 4)
    }
}

#[cfg(any(target_os = "android", feature = "native"))]
impl Backends for ModelBackends {
    fn transcriber(&self, live: bool) -> Result<Box<dyn Transcriber>, AsrError> {
        let mut asr = lock(&self.asr);
        if asr.is_none() {
            let path = self
                .paths()
                .asr_model
                .ok_or_else(|| AsrError::Load("the speech model is not installed yet".into()))?;
            *asr = Some(examic_asr::GigaAm::load(&path)?);
        }
        let model = asr.as_ref().expect("loaded above");
        Ok(Box::new(model.transcriber(threads(live))?))
    }

    fn translator(&self) -> Result<Box<dyn Translator>, MtError> {
        let dir = self
            .paths()
            .mt_dir
            .ok_or_else(|| MtError::Load("the translation model is not installed yet".into()))?;
        // The onnxruntime-android AAR puts the library next to ours; a host
        // build names it in ORT_DYLIB_PATH.
        let library =
            std::env::var_os("ORT_DYLIB_PATH").unwrap_or_else(|| "libonnxruntime.so".into());
        examic_mt::init_runtime(std::path::Path::new(&library))?;
        Ok(Box::new(examic_mt::OpusMt::load(&dir, 2)?))
    }
}

#[cfg(not(any(target_os = "android", feature = "native")))]
impl Backends for ModelBackends {
    fn transcriber(&self, _live: bool) -> Result<Box<dyn Transcriber>, AsrError> {
        Err(AsrError::Unavailable)
    }

    fn translator(&self) -> Result<Box<dyn Translator>, MtError> {
        Err(MtError::Unavailable)
    }
}
