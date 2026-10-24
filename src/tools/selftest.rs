//! `sift-it selftest`: the vectors this build makes for fixed inputs, against reference vectors made by a known-good
//! pipeline (tests/reference). Any change to llama.cpp, the model files, the image sizing or the prompts must still
//! pass this, or an existing index would no longer agree with new queries.

use crate::models;
use anyhow::{Context, Result, bail};
use embedder::imaging;
use embedder::{Embedder, Encoder, Inputs, prompts};
use std::collections::BTreeMap;
use std::path::Path;

/// The reference inputs and vectors (tests/reference), built in, so an installed sift-it can check itself.
const BUILT_IN: [(&str, &[u8]); 4] = [
    ("vectors.json", include_bytes!("../../tests/reference/vectors.json")),
    ("photo.jpg", include_bytes!("../../tests/reference/photo.jpg")),
    ("screen.png", include_bytes!("../../tests/reference/screen.png")),
    ("speech.wav", include_bytes!("../../tests/reference/speech.wav")),
];

/// Runs against the built-in reference (written to a temporary folder: mtmd's decoders read files).
pub fn run_built_in() -> Result<()> {
    let folder = std::env::temp_dir().join(format!("sift-it-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&folder)?;
    for (name, bytes) in BUILT_IN {
        std::fs::write(folder.join(name), bytes)?;
    }
    let result = run(&folder);
    let _ = std::fs::remove_dir_all(&folder);
    result
}

/// Runtimes that agree measure ≥ 0.999; less is a change people would see in their results.
const AGREEMENT: f32 = 0.999;

pub fn run(folder: &Path) -> Result<()> {
    let reference: serde_json::Value = serde_json::from_slice(
        &std::fs::read(folder.join("vectors.json"))
            .with_context(|| format!("no vectors.json in {}", folder.display()))?,
    )?;
    let expected: BTreeMap<String, Vec<f32>> = serde_json::from_value(reference["vectors"].clone())?;

    let text = models::TEXT.require()?;
    let started = std::time::Instant::now();
    let mut embedder = Embedder::load(&text, Inputs::Documents)?;
    eprintln!("model loaded in {:.2} s", started.elapsed().as_secs_f64());
    let mmproj = models::MEDIA.require()?;

    let long = (1..=60)
        .map(|n| {
            format!(
                "Note {n}: the review moved to room {} because the projector in the main hall failed again, and the \
                 team agreed to send the slides the night before.",
                n % 7 + 2
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let texts = [
        ("query", prompts::query("payment declined error")),
        ("query-bn", prompts::query("ইলিশ মাছ রান্না")),
        ("query-ar", prompts::query("متى تغلق المكتبة")),
        (
            "document",
            prompts::document(
                Some("Tokyo itinerary"),
                "Flight to Tokyo Haneda on November 3. Then the train to Kamakura to see the Great Buddha, and back \
                 for dinner in Shibuya.",
            ),
        ),
        ("document-long", prompts::document(None, &long)),
    ];
    let mut made: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let started = std::time::Instant::now();
    let vectors = embedder.embed_texts(&texts.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>())?;
    eprintln!("5 texts in {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
    for ((name, _), vector) in texts.iter().zip(vectors) {
        made.insert(name.to_string(), vector);
    }
    // The queries again, as the engine embeds them: one at a time, in a context made for queries.
    let mut queries = Embedder::load(&text, Inputs::Queries)?;
    for (name, text) in texts.iter().filter(|(name, _)| name.starts_with("query")) {
        made.insert(format!("{name} (engine)"), queries.embed_text(text)?);
    }
    drop(queries);

    // Each encoder on its own, as indexing loads them.
    load_encoder(&mut embedder, Encoder::Vision, &mmproj)?;
    for (name, file, budget) in
        [("screen-280", "screen.png", 280), ("photo-140", "photo.jpg", 140), ("frame-70", "photo.jpg", 70)]
    {
        let image = embedder.decode_image(&folder.join(file))?;
        let started = std::time::Instant::now();
        let vector = embedder.embed_image(&imaging::fit(&image, budget)?)?;
        eprintln!("{name} in {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
        made.insert(name.into(), vector);
    }
    embedder.release_encoders();
    load_encoder(&mut embedder, Encoder::Audio, &mmproj)?;
    let samples = embedder.decode_audio(&folder.join("speech.wav"))?;
    let started = std::time::Instant::now();
    made.insert("speech".into(), embedder.embed_audio(&samples)?);
    eprintln!(
        "speech ({:.1} s of audio) in {:.0} ms",
        samples.len() as f64 / embedder::AUDIO_RATE as f64,
        started.elapsed().as_secs_f64() * 1000.0
    );

    let mut worst = 1.0f32;
    for (name, vector) in &made {
        let reference = expected.get(name.trim_end_matches(" (engine)"));
        let Some(reference) = reference else { bail!("no reference vector for {name}") };
        let cosine: f32 = vector.iter().zip(reference).map(|(a, b)| a * b).sum();
        worst = worst.min(cosine);
        println!("{name:<20} cosine {cosine:.5}{}", if cosine < AGREEMENT { "  ✗" } else { "" });
    }
    if worst < AGREEMENT {
        bail!("vectors differ from the reference (worst cosine {worst:.5} < {AGREEMENT})");
    }
    println!("all vectors match the reference (worst cosine {worst:.5})");
    Ok(())
}

/// Loads an encoder, and says how long that took.
fn load_encoder(embedder: &mut Embedder, encoder: Encoder, mmproj: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    embedder.load_encoder(encoder, mmproj)?;
    eprintln!("{encoder} encoder loaded in {:.2} s", started.elapsed().as_secs_f64());
    Ok(())
}
