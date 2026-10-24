//! Experiment: EmbeddingGemma 2's text model on the Neural Engine, through Core ML, with FluidInference's conversion
//! (FluidInference/embeddinggemma-2-coreml: fp16, fixed shapes of 32 to 512 tokens, one function per shape).
//!
//! Tokens come from llama.cpp's tokenizer, so they are the ones the GGUF path sees. Their rows come from the
//! conversion's own bf16 table (memory-mapped), times √512, as fp16; each text goes to the smallest function that
//! holds it, padded on the right with a mask. The model mean-pools and normalizes inside.
//!
//! `SIFT_IT_COREML=<folder with EmbeddingGemma2Text.mlpackage and embeddings.bf16>` turns it on;
//! `SIFT_IT_COREML_UNITS=ane|gpu|cpu|all` picks where it runs (ane: CPU and Neural Engine, the default);
//! `SIFT_IT_COREML_FUNCTIONS=128,256,512` loads only those shapes.

use anyhow::{Context, Result, anyhow, bail};
use half::{bf16, f16};
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_core_ml::{
    MLComputeUnits, MLDictionaryFeatureProvider, MLFeatureProvider, MLFeatureValue, MLModel, MLModelConfiguration,
    MLMultiArray, MLMultiArrayDataType,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const PACKAGE: &str = "EmbeddingGemma2Text.mlpackage";
pub const TABLE: &str = "embeddings.bf16";
const HIDDEN: usize = 512;
const VOCABULARY: usize = 262_144;
const SCALE: f32 = 22.627_417;
pub const SHAPES: [usize; 6] = [32, 48, 64, 128, 256, 512];
/// The longest text a function takes.
pub const MAX_TOKENS: usize = 512;

/// The bf16 token table, memory-mapped read-only (shared with every process that maps it).
struct Table {
    rows: *const u16,
    bytes: usize,
}

impl Table {
    fn open(path: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let bytes = file.metadata()?.len() as usize;
        if bytes != VOCABULARY * HIDDEN * 2 {
            bail!("{} is {bytes} bytes, not a {VOCABULARY}×{HIDDEN} bf16 table", path.display());
        }
        let rows =
            unsafe { libc::mmap(std::ptr::null_mut(), bytes, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if rows == libc::MAP_FAILED {
            bail!("couldn't map {}", path.display());
        }
        Ok(Self { rows: rows as *const u16, bytes })
    }

    fn row(&self, token: i32) -> &[bf16] {
        let token = (token.max(0) as usize).min(VOCABULARY - 1);
        unsafe { std::slice::from_raw_parts(self.rows.add(token * HIDDEN) as *const bf16, HIDDEN) }
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.rows as *mut libc::c_void, self.bytes) };
    }
}

unsafe impl Send for Table {}

/// One loaded function: a fixed number of tokens, and its model.
struct Function {
    tokens: usize,
    model: Retained<MLModel>,
}

pub struct TextEncoder {
    functions: Vec<Function>,
    table: Table,
    /// What loading took: compiling the package (once per Mac, cached), and loading each function.
    pub load_report: String,
}

// Core ML models may be used from any thread, one call at a time here (`&mut self`).
unsafe impl Send for TextEncoder {}

fn units() -> MLComputeUnits {
    match std::env::var("SIFT_IT_COREML_UNITS").as_deref() {
        Ok("gpu") => MLComputeUnits::CPUAndGPU,
        Ok("cpu") => MLComputeUnits::CPUOnly,
        Ok("all") => MLComputeUnits::All,
        _ => MLComputeUnits::CPUAndNeuralEngine,
    }
}

fn ns_error(error: Retained<objc2_foundation::NSError>) -> anyhow::Error {
    anyhow!("{}", error.localizedDescription())
}

fn shape(dims: &[usize]) -> Retained<NSArray<NSNumber>> {
    let numbers: Vec<Retained<NSNumber>> = dims.iter().map(|&d| NSNumber::new_isize(d as isize)).collect();
    NSArray::from_retained_slice(&numbers)
}

/// A contiguous fp16 array of `dims`, filled by `fill`.
fn half_array(dims: &[usize], fill: impl FnOnce(&mut [f16])) -> Result<Retained<MLMultiArray>> {
    let array = unsafe {
        MLMultiArray::initWithShape_dataType_error(MLMultiArray::alloc(), &shape(dims), MLMultiArrayDataType::Float16)
    }
    .map_err(ns_error)?;
    let count: usize = dims.iter().product();
    #[allow(deprecated)]
    let data = unsafe { std::slice::from_raw_parts_mut(array.dataPointer().as_ptr() as *mut f16, count) };
    fill(data);
    Ok(array)
}

impl TextEncoder {
    pub fn load(folder: &Path) -> Result<Self> {
        let package = folder.join(PACKAGE);
        let table = Table::open(&folder.join(TABLE))?;
        let started = Instant::now();
        let compiled = compiled(&package)?;
        let compile_seconds = started.elapsed().as_secs_f64();
        let wanted: Vec<usize> = match std::env::var("SIFT_IT_COREML_FUNCTIONS") {
            Ok(list) => list.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
            Err(_) => SHAPES.to_vec(),
        };
        let url = NSURL::fileURLWithPath(&NSString::from_str(compiled.to_str().context("path isn't UTF-8")?));
        let mut functions = Vec::new();
        let mut report = format!("compile {compile_seconds:.1} s;");
        for tokens in SHAPES.into_iter().filter(|t| wanted.contains(t)) {
            let started = Instant::now();
            let model = autoreleasepool(|_| unsafe {
                let configuration = MLModelConfiguration::new();
                configuration.setComputeUnits(units());
                configuration.setFunctionName(Some(&NSString::from_str(&format!("embed_{tokens}"))));
                MLModel::modelWithContentsOfURL_configuration_error(&url, &configuration)
            })
            .map_err(ns_error)
            .with_context(|| format!("loading embed_{tokens}"))?;
            report.push_str(&format!(" embed_{tokens} {:.1} s", started.elapsed().as_secs_f64()));
            functions.push(Function { tokens, model });
        }
        if functions.is_empty() {
            bail!("no Core ML functions loaded");
        }
        Ok(Self { functions, table, load_report: report })
    }

    pub fn max_tokens(&self) -> usize {
        self.functions.last().map(|f| f.tokens).unwrap_or(0)
    }

    /// One normalized vector per text (`<bos> … <eos>` token ids; longer than the largest function, cut to fit and
    /// still ending in the last token).
    pub fn embed(&mut self, texts: &[Vec<i32>]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|tokens| self.embed_one(tokens)).collect()
    }

    fn embed_one(&mut self, tokens: &[i32]) -> Result<Vec<f32>> {
        let max = self.max_tokens();
        let tokens: Vec<i32> = if tokens.len() > max {
            tokens[..max - 1].iter().copied().chain(tokens.last().copied()).collect()
        } else {
            tokens.to_vec()
        };
        let function = self.functions.iter().find(|f| f.tokens >= tokens.len()).expect("the largest holds it");
        let length = function.tokens;
        autoreleasepool(|_| {
            let embeds = half_array(&[1, length, HIDDEN], |data| {
                data.fill(f16::ZERO);
                for (at, &token) in tokens.iter().enumerate() {
                    let row = self.table.row(token);
                    for (out, value) in data[at * HIDDEN..(at + 1) * HIDDEN].iter_mut().zip(row) {
                        *out = f16::from_f32(value.to_f32() * SCALE);
                    }
                }
            })?;
            let mask = half_array(&[1, length], |data| {
                for (at, out) in data.iter_mut().enumerate() {
                    *out = if at < tokens.len() { f16::ONE } else { f16::ZERO };
                }
            })?;
            let keys = [NSString::from_str("inputs_embeds"), NSString::from_str("attention_mask")];
            let values = unsafe {
                [MLFeatureValue::featureValueWithMultiArray(&embeds), MLFeatureValue::featureValueWithMultiArray(&mask)]
            };
            let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
            let values: Vec<&AnyObject> = values.iter().map(|v| v.as_ref()).collect();
            let dictionary = NSDictionary::from_slices(&keys, &values);
            let input = unsafe {
                MLDictionaryFeatureProvider::initWithDictionary_error(MLDictionaryFeatureProvider::alloc(), &dictionary)
            }
            .map_err(ns_error)?;
            let output = unsafe { function.model.predictionFromFeatures_error(ProtocolObject::from_ref(&*input)) }
                .map_err(ns_error)?;
            let array = unsafe { output.featureValueForName(&NSString::from_str("embedding")) }
                .and_then(|value| unsafe { value.multiArrayValue() })
                .ok_or_else(|| anyhow!("no embedding in the output"))?;
            let mut vector = read_floats(&array)?;
            crate::normalize(&mut vector);
            Ok(vector)
        })
    }
}

fn read_floats(array: &MLMultiArray) -> Result<Vec<f32>> {
    let count = unsafe { array.count() } as usize;
    #[allow(deprecated)]
    let data = unsafe { array.dataPointer() }.as_ptr();
    let kind = unsafe { array.dataType() };
    Ok(if kind == MLMultiArrayDataType::Float16 {
        unsafe { std::slice::from_raw_parts(data as *const f16, count) }.iter().map(|v| v.to_f32()).collect()
    } else if kind == MLMultiArrayDataType::Float32 {
        unsafe { std::slice::from_raw_parts(data as *const f32, count) }.to_vec()
    } else {
        bail!("unexpected output type {kind:?}");
    })
}

/// The package compiled for this Mac (`.mlmodelc`), once: kept in SIFT_IT_COREML_CACHE (default: the system's
/// temporary folder), named after the package's size and time.
fn compiled(package: &Path) -> Result<PathBuf> {
    let weights = package.join("Data/com.apple.CoreML/weights/weight.bin");
    let stamp = std::fs::metadata(&weights).with_context(|| format!("no {}", weights.display()))?.len();
    let cache = std::env::var_os("SIFT_IT_COREML_CACHE").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let target = cache.join(format!("EmbeddingGemma2Text-{stamp}.mlmodelc"));
    if target.exists() {
        return Ok(target);
    }
    std::fs::create_dir_all(&cache)?;
    // Core ML's compiler fails on a package of symlinks (the Hugging Face cache's layout): compile a real copy.
    let copy = cache.join(format!("EmbeddingGemma2Text-{stamp}.mlpackage"));
    if !copy.exists() {
        let status = std::process::Command::new("cp").arg("-RL").arg(package).arg(&copy).status()?;
        if !status.success() {
            bail!("couldn't copy {}", package.display());
        }
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(copy.to_str().context("path isn't UTF-8")?));
    #[allow(deprecated)]
    let temporary = unsafe { MLModel::compileModelAtURL_error(&url) }.map_err(ns_error)?;
    let temporary = PathBuf::from(temporary.path().context("compiled model has no path")?.to_string());
    std::fs::rename(&temporary, &target)
        .or_else(|_| std::process::Command::new("cp").arg("-R").arg(&temporary).arg(&target).status().map(drop))?;
    std::fs::remove_dir_all(&copy)?;
    Ok(target)
}
