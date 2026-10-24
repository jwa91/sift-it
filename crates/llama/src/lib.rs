//! EmbeddingGemma 2 on llama.cpp (Metal). Text goes straight into the text model; images and audio go through the
//! model's own encoders (llama.cpp's mtmd library), each loaded on its own, and their output rows are fed to the text
//! model in place of tokens. Every input becomes one mean-pooled, L2-normalized 768-dimensional vector in the same
//! space.
//!
//! Inputs are framed the way the reference implementation (transformers) frames them: `<bos> text <eos>`, and
//! `<bos> <image soft tokens> <eos>` (the same for audio). The model has no causal mask and no KV cache to keep, so each input
//! goes through the text model in one `llama_process` call; several short texts share one call as separate sequences.

pub mod imaging;

use anyhow::{Context, Result, anyhow, bail};
use imaging::Image;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Generated bindings to llama.cpp's C API (see build.rs).
#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]
mod ll {
    include!(concat!(env!("OUT_DIR"), "/llama.rs"));
}

/// The width of every vector.
pub const DIM: usize = 768;
/// What the audio encoder listens to: 16 kHz mono, at most 30 seconds per input.
pub const AUDIO_RATE: usize = 16_000;
pub const AUDIO_WINDOW: usize = 30 * AUDIO_RATE;

/// Sequences that may share one call to the text model, when it embeds documents.
const MAX_SEQUENCES: i32 = 16;

/// What a context is made for, which decides its size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Inputs {
    /// One query at a time, of up to QUERY_TOKENS tokens: its compute buffers take 32 MB, a ninth of a document
    /// context's.
    Queries,
    /// Up to MAX_SEQUENCES passages in one call, or a picture, or 30 s of sound, of up to 2,048 tokens together.
    Documents,
}

/// The tokens a query may have. 256 holds a pasted paragraph (about 1,000 characters); a longer query keeps its start.
const QUERY_TOKENS: usize = 256;

/// What makes the vectors: the model, and the version of the pipeline around it (the prompts, the framing, the
/// picture sizing). Stored with the index: vectors from another source are never compared with these. The llama.cpp
/// release isn't in it: one that passes `sift-it selftest` makes the same vectors. Bump the version when they move,
/// on purpose or with a release that doesn't pass, and indexes embed everything again.
pub const VECTOR_SOURCE: &str = "embeddinggemma-2-q8_0/v1";

/// The model's media encoders. Both are in one file (the mmproj), and each loads on its own: the vision encoder for
/// pictures and video frames, the audio encoder for sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    Vision,
    Audio,
}

impl std::fmt::Display for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Encoder::Vision => "vision",
            Encoder::Audio => "audio",
        })
    }
}

pub struct Embedder {
    model: *mut ll::llama_model,
    context: *mut ll::llama_context,
    /// The encoders, by `Encoder`, each while it's loaded (else null).
    encoders: [*mut ll::mtmd_context; 2],
    batch: *mut ll::llama_batch_ext,
    vocab: *const ll::llama_vocab,
    eos: ll::llama_token,
    /// The width of the vectors out of the model, and of the encoder rows into it.
    dim: usize,
    row: usize,
    max_tokens: usize,
    max_sequences: i32,
    // The batch being filled: its sequences, its tokens, and the next position in the current sequence.
    sequences: i32,
    tokens: usize,
    position: ll::llama_pos,
}

// llama.cpp's objects may move between threads, as long as one thread uses them at a time (`&mut self`).
unsafe impl Send for Embedder {}

static VERBOSE: AtomicBool = AtomicBool::new(false);
static LAST_LEVEL: AtomicU32 = AtomicU32::new(0);

/// llama.cpp talks a lot; only errors get through, unless SIFT_IT_VERBOSE is set.
unsafe extern "C" fn log_callback(level: ll::ggml_log_level, text: *const c_char, _: *mut c_void) {
    let level = if level == ll::GGML_LOG_LEVEL_CONT { LAST_LEVEL.load(Ordering::Relaxed) } else { level };
    LAST_LEVEL.store(level, Ordering::Relaxed);
    if (level >= ll::GGML_LOG_LEVEL_ERROR || VERBOSE.load(Ordering::Relaxed))
        && let Ok(text) = unsafe { CStr::from_ptr(text) }.to_str()
    {
        eprint!("llama.cpp: {text}");
    }
}

/// Tells ggml's Metal backend, before it starts, to keep no buffers resident between calls. Its residency sets keep
/// the model's memory wired for three minutes after the last call, renewed by a thread that wakes every 5 ms for as
/// long as the process lives, idle or not. Without them, Metal makes memory resident for the calls that use it, and
/// embedding is as fast (docs/design.md). The backend reads this from the environment, which processes this one starts
/// inherit: the speech helper's ggml has the same residency sets.
///
/// # Safety
///
/// It sets an environment variable: no other thread may be running.
pub unsafe fn without_residency() {
    unsafe { std::env::set_var("GGML_METAL_NO_RESIDENCY", "1") };
}

fn init_backend() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        VERBOSE.store(std::env::var_os("SIFT_IT_VERBOSE").is_some(), Ordering::Relaxed);
        ll::llama_log_set(Some(log_callback), std::ptr::null_mut());
        ll::mtmd_log_set(Some(log_callback), std::ptr::null_mut());
        ll::mtmd_helper_log_set(Some(log_callback), std::ptr::null_mut());
        ll::llama_backend_init();
    });
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.to_str().ok_or_else(|| anyhow!("path isn't UTF-8: {}", path.display()))?)
        .context("path has a NUL byte")
}

/// Frees an mtmd bitmap when dropped.
struct Bitmap(*mut ll::mtmd_bitmap);
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe { ll::mtmd_bitmap_free(self.0) }
    }
}

/// Frees mtmd input chunks when dropped.
struct Chunks(*mut ll::mtmd_input_chunks);
impl Drop for Chunks {
    fn drop(&mut self) {
        unsafe { ll::mtmd_input_chunks_free(self.0) }
    }
}

impl Embedder {
    /// Loads the text model from its GGUF file, with a context made for `inputs`. Its weights are mapped from the
    /// file, not copied: processes that load it share them, and macOS can drop them under pressure and read them again.
    pub fn load(text: &Path, inputs: Inputs) -> Result<Self> {
        init_backend();
        unsafe {
            let mut model_params = ll::llama_model_default_params();
            model_params.n_gpu_layers = 999;
            let model = ll::llama_model_load_from_file(c_path(text)?.as_ptr(), model_params);
            if model.is_null() {
                bail!("llama.cpp couldn't load {}", text.display());
            }
            // Each input must fit in one micro-batch. 2048 tokens holds a long passage, a 1120-token image or a 30 s
            // clip (750 tokens).
            let (max_tokens, sequences) = match inputs {
                Inputs::Queries => (QUERY_TOKENS, 1),
                Inputs::Documents => {
                    let tokens = std::env::var("SIFT_IT_BATCH_TOKENS").ok().and_then(|v| v.parse().ok());
                    (tokens.unwrap_or(2048), MAX_SEQUENCES)
                }
            };
            let mut params = ll::llama_context_default_params();
            params.n_ctx = max_tokens as u32;
            params.n_batch = max_tokens as u32;
            params.n_ubatch = max_tokens as u32;
            params.n_seq_max = sequences as u32;
            params.kv_unified = true;
            params.embeddings = true;
            params.pooling_type = ll::LLAMA_POOLING_TYPE_MEAN;
            let context = ll::llama_init_from_model(model, params);
            if context.is_null() {
                ll::llama_model_free(model);
                bail!("llama.cpp couldn't create a context");
            }
            let vocab = ll::llama_model_get_vocab(model);
            let mut embedder = Self {
                model,
                context,
                encoders: [std::ptr::null_mut(); 2],
                batch: ll::llama_batch_ext_init(context),
                vocab,
                eos: ll::llama_vocab_eos(vocab),
                dim: ll::llama_model_n_embd_out(model) as usize,
                row: ll::llama_model_n_embd_inp(model) as usize,
                max_tokens,
                max_sequences: sequences,
                sequences: 0,
                tokens: 0,
                position: 0,
            };
            if embedder.dim != DIM {
                bail!("the model makes {}-dimensional vectors, not {DIM}", embedder.dim);
            }
            // The first calls compile the Metal pipelines: pay for that now rather than on the first real input.
            embedder.embed_texts(&["warm up"])?;
            Ok(embedder)
        }
    }

    /// Loads an encoder from the model's mmproj, unless it's loaded. mtmd reads it into memory of its own, so it's
    /// loaded only while pictures or sound need it.
    pub fn load_encoder(&mut self, encoder: Encoder, mmproj: &Path) -> Result<()> {
        if self.has_encoder(encoder) {
            return Ok(());
        }
        unsafe {
            let mut params = ll::mtmd_context_params_default();
            params.use_gpu = true;
            params.print_timings = false;
            params.warmup = true;
            // The mmproj holds both encoders: this one alone (scripts/llama.cpp.patch).
            params.use_vision = encoder == Encoder::Vision;
            params.use_audio = encoder == Encoder::Audio;
            // Images arrive already sized for their token budget (imaging::fit): mtmd must take them as they are.
            params.image_min_tokens = 1;
            params.image_max_tokens = 1120;
            let loaded = ll::mtmd_init_from_file(c_path(mmproj)?.as_ptr(), self.model, params);
            if loaded.is_null() {
                bail!("llama.cpp couldn't load the {encoder} encoder from {}", mmproj.display());
            }
            self.encoders[encoder as usize] = loaded;
        }
        Ok(())
    }

    /// Frees the encoders; the text model stays.
    pub fn release_encoders(&mut self) {
        for encoder in &mut self.encoders {
            if !encoder.is_null() {
                unsafe { ll::mtmd_free(*encoder) };
                *encoder = std::ptr::null_mut();
            }
        }
    }

    pub fn has_encoder(&self, encoder: Encoder) -> bool {
        !self.encoders[encoder as usize].is_null()
    }

    /// One vector per text, in order. Prompts (query or document framing) are the caller's: see `prompts`.
    pub fn embed_texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        self.reset();
        for text in texts {
            let tokens = self.tokenize(text);
            if self.sequences == self.max_sequences || self.tokens + tokens.len() > self.max_tokens {
                vectors.extend(self.run()?);
            }
            self.begin();
            for token in tokens {
                self.add_token(token)?;
            }
        }
        if self.sequences > 0 {
            vectors.extend(self.run()?);
        }
        Ok(vectors)
    }

    pub fn embed_text(&mut self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_texts(&[text])?.remove(0))
    }

    /// An image's vector. It must already be sized for its token budget (`imaging::fit`).
    pub fn embed_image(&mut self, image: &Image) -> Result<Vec<f32>> {
        let context = self.encoder(Encoder::Vision)?;
        let expected = image.width as usize * image.height as usize * 3;
        if image.rgb.len() != expected {
            bail!("image buffer is {} bytes, expected {expected}", image.rgb.len());
        }
        let bitmap = unsafe { ll::mtmd_bitmap_init(image.width, image.height, image.rgb.as_ptr()) };
        if bitmap.is_null() {
            bail!("mtmd couldn't take a {}×{} image", image.width, image.height);
        }
        self.embed_media(context, Bitmap(bitmap))
    }

    /// A stretch of 16 kHz mono audio's vector; past 30 seconds is cut off.
    pub fn embed_audio(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        let context = self.encoder(Encoder::Audio)?;
        if samples.is_empty() {
            bail!("no audio samples");
        }
        let samples = &samples[..samples.len().min(AUDIO_WINDOW)];
        let bitmap = unsafe { ll::mtmd_bitmap_init_from_audio(samples.len(), samples.as_ptr()) };
        if bitmap.is_null() {
            bail!("mtmd couldn't take {} audio samples", samples.len());
        }
        self.embed_media(context, Bitmap(bitmap))
    }

    /// A picture as mtmd's own decoder (stb_image) reads it, with the vision encoder. Used to check the pipeline
    /// against reference vectors made from the same decoders.
    pub fn decode_image(&self, path: &Path) -> Result<Image> {
        let bitmap = self.decode(Encoder::Vision, path)?;
        unsafe {
            let data = ll::mtmd_bitmap_get_data(bitmap.0);
            Ok(Image {
                width: ll::mtmd_bitmap_get_nx(bitmap.0),
                height: ll::mtmd_bitmap_get_ny(bitmap.0),
                rgb: std::slice::from_raw_parts(data, ll::mtmd_bitmap_get_n_bytes(bitmap.0)).to_vec(),
            })
        }
    }

    /// Sound as mtmd's own decoder (miniaudio, at 16 kHz) reads it, with the audio encoder.
    pub fn decode_audio(&self, path: &Path) -> Result<Vec<f32>> {
        let bitmap = self.decode(Encoder::Audio, path)?;
        unsafe {
            let data = ll::mtmd_bitmap_get_data(bitmap.0) as *const f32;
            Ok(std::slice::from_raw_parts(data, ll::mtmd_bitmap_get_n_bytes(bitmap.0) / 4).to_vec())
        }
    }

    /// A file decoded by mtmd for `encoder`.
    fn decode(&self, encoder: Encoder, path: &Path) -> Result<Bitmap> {
        let context = self.encoder(encoder)?;
        unsafe {
            let wrapper = ll::mtmd_helper_bitmap_init_from_file(
                context,
                c_path(path)?.as_ptr(),
                false,
                ll::mtmd_helper_init_opt_default(),
            );
            if !wrapper.video_ctx.is_null() {
                ll::mtmd_helper_video_free(wrapper.video_ctx);
            }
            if wrapper.bitmap.is_null() {
                bail!("mtmd can't read {}", path.display());
            }
            let bitmap = Bitmap(wrapper.bitmap);
            if ll::mtmd_bitmap_is_audio(bitmap.0) != (encoder == Encoder::Audio) {
                bail!("{} isn't for the {encoder} encoder", path.display());
            }
            Ok(bitmap)
        }
    }

    /// The encoder's mtmd context, if it's loaded.
    fn encoder(&self, encoder: Encoder) -> Result<*mut ll::mtmd_context> {
        let context = self.encoders[encoder as usize];
        if context.is_null() {
            bail!("the {encoder} encoder isn't loaded");
        }
        Ok(context)
    }

    fn embed_media(&mut self, context: *mut ll::mtmd_context, bitmap: Bitmap) -> Result<Vec<f32>> {
        unsafe {
            let chunks = Chunks(ll::mtmd_input_chunks_init());
            let marker = ll::mtmd_default_marker();
            let text = ll::mtmd_input_text {
                text: marker,
                text_len: CStr::from_ptr(marker).to_bytes().len(),
                add_special: true,
                parse_special: true,
            };
            let bitmaps = [bitmap.0 as *const ll::mtmd_bitmap];
            let status = ll::mtmd_tokenize(context, chunks.0, &text, bitmaps.as_ptr(), 1);
            if status != 0 {
                bail!("mtmd couldn't prepare the input ({status})");
            }
            self.reset();
            self.begin();
            for index in 0..ll::mtmd_input_chunks_size(chunks.0) {
                let chunk = ll::mtmd_input_chunks_get(chunks.0, index);
                if ll::mtmd_input_chunk_get_type(chunk) == ll::MTMD_INPUT_CHUNK_TYPE_TEXT {
                    let mut count = 0usize;
                    let tokens = ll::mtmd_input_chunk_get_tokens_text(chunk, &mut count);
                    if tokens.is_null() {
                        continue;
                    }
                    for &token in std::slice::from_raw_parts(tokens, count) {
                        self.add_token(token)?;
                    }
                } else {
                    if ll::mtmd_encode_chunk(context, chunk) != 0 {
                        self.reset();
                        bail!("the media encoder failed");
                    }
                    let rows = ll::mtmd_get_output_embd(context);
                    if rows.is_null() {
                        self.reset();
                        bail!("the media encoder returned nothing");
                    }
                    let count = ll::mtmd_input_chunk_get_n_tokens(chunk);
                    if self.tokens + count + 1 > self.max_tokens {
                        self.reset();
                        bail!("input of {count} tokens is longer than {}", self.max_tokens);
                    }
                    for row in 0..count {
                        let embd = ll::llama_embd { data: rows.add(row * self.row), n_rows: 1, n_embd: self.row };
                        self.place(ll::llama_batch_ext_add_embd(self.batch, self.sequences - 1, embd))?;
                    }
                }
            }
            Ok(self.run()?.remove(0))
        }
    }

    /// How many tokens the model sees for this text.
    pub fn count_tokens(&self, text: &str) -> usize {
        self.tokenize(text).len()
    }

    /// `<bos> text <eos>`; a text longer than the context keeps its start (and still ends in `<eos>`).
    fn tokenize(&self, text: &str) -> Vec<ll::llama_token> {
        let bytes = text.as_bytes();
        let mut tokens = vec![0; bytes.len() + 8];
        let tokenize = |tokens: &mut Vec<ll::llama_token>| unsafe {
            ll::llama_tokenize(
                self.vocab,
                bytes.as_ptr() as *const c_char,
                bytes.len() as i32,
                tokens.as_mut_ptr(),
                tokens.len() as i32,
                true,
                false,
            )
        };
        let mut count = tokenize(&mut tokens);
        if count < 0 {
            tokens = vec![0; (-count) as usize];
            count = tokenize(&mut tokens);
        }
        tokens.truncate(count.max(0) as usize);
        if tokens.len() > self.max_tokens {
            tokens.truncate(self.max_tokens - 1);
            tokens.push(self.eos);
        }
        tokens
    }

    /// Starts the next input in the batch: each input is its own sequence.
    fn begin(&mut self) {
        self.sequences += 1;
        self.position = 0;
    }

    /// Empties the batch, including an input that failed halfway.
    fn reset(&mut self) {
        unsafe { ll::llama_batch_ext_clear(self.batch) };
        self.sequences = 0;
        self.tokens = 0;
    }

    fn add_token(&mut self, token: ll::llama_token) -> Result<()> {
        let index = unsafe { ll::llama_batch_ext_add_token(self.batch, self.sequences - 1, token) };
        self.place(index)
    }

    fn place(&mut self, index: i32) -> Result<()> {
        if index < 0 {
            bail!("couldn't add to the batch ({index})");
        }
        // Room for M-RoPE's extra positions; this model reads only the first.
        let pos: [ll::llama_pos; 4] = [self.position, 0, 0, 0];
        unsafe {
            if !ll::llama_batch_ext_set_pos(self.batch, index, pos.as_ptr())
                || !ll::llama_batch_ext_set_output_embd(self.batch, index, true)
            {
                bail!("couldn't place token {index}");
            }
        }
        self.position += 1;
        self.tokens += 1;
        Ok(())
    }

    /// Runs the text model over the batch: one normalized vector per sequence, in order.
    fn run(&mut self) -> Result<Vec<Vec<f32>>> {
        let status = unsafe { ll::llama_process(self.context, ll::LLAMA_PROCESS_TYPE_ENCODE, self.batch) };
        let result = if status != 0 {
            Err(anyhow!("llama_process failed ({status})"))
        } else {
            (0..self.sequences)
                .map(|sequence| {
                    let pooled = unsafe { ll::llama_get_embeddings_seq(self.context, sequence) };
                    if pooled.is_null() {
                        bail!("no pooled vector for input {sequence}");
                    }
                    let mut vector = unsafe { std::slice::from_raw_parts(pooled, self.dim) }.to_vec();
                    normalize(&mut vector);
                    Ok(vector)
                })
                .collect()
        };
        self.reset();
        result
    }
}

impl Drop for Embedder {
    fn drop(&mut self) {
        self.release_encoders();
        unsafe {
            ll::llama_batch_ext_free(self.batch);
            ll::llama_free(self.context);
            ll::llama_model_free(self.model);
        }
    }
}

pub fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>().sqrt();
    if norm > 0.0 {
        let scale = (1.0 / norm) as f32;
        vector.iter_mut().for_each(|v| *v *= scale);
    }
}

/// EmbeddingGemma 2's task prompts (from its model card). Images and audio take none.
pub mod prompts {
    pub fn query(text: &str) -> String {
        format!("task: search result | query: {text}")
    }

    pub fn code_query(text: &str) -> String {
        format!("task: code retrieval | query: {text}")
    }

    pub fn document(title: Option<&str>, text: &str) -> String {
        let title = title.filter(|t| !t.is_empty()).unwrap_or("none");
        format!("title: {title} | text: {text}")
    }
}
