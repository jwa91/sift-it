# Energy and speed experiments (Phase 3)

2026-10-24, branch `experiment/energy-and-speed` (from main at eea8941). Research only: nothing here is meant to merge.

**This Mac:** MacBook Air M5 (Mac17,4: fanless, 10-core GPU, 24 GB), macOS 26.6.2.

**How energy was measured:** `powermetrics --samplers cpu_power,gpu_power,ane_power,thermal -i 1000`, on battery, with
other apps quit. The short suite (`suite.sh short`) runs each workload as fixed work, with 20 s cool-downs, and repeats
the comparisons in reverse order. Energy is the chip's (CPU+GPU+ANE) above the idle baseline (0.31 W), over the work
itself, without loading the model. It leaves out the display, DRAM and SSD. Thermal pressure stayed Nominal and the
background-load log shows nothing competing.

**Speed figures:** measured both on the charger and on battery; each table says which. Runs that overlapped another
session's compile were run again. The longer full suite (40 min, with four-minute sustained runs) wasn't run, so
nothing here measures throttling under sustained load.

## 1. Text on the Neural Engine (Core ML) vs llama.cpp

FluidInference/embeddinggemma-2-coreml @ 3f978dd, called in-process from Rust through objc2-core-ml
(`crates/llama/src/coreml.rs`, about 230 lines). Tokens come from llama.cpp's tokenizer. Rows come from the
conversion's bf16 table, memory-mapped. Each text goes to the smallest fixed shape that fits it.

| | llama.cpp b11461, GPU (today) | Core ML, ANE, shapes 256 + 512 |
|---|---|---|
| Cosine to llama.cpp, 2,856 demo passages | — | min 0.9995, mean 0.99987 |
| `make eval` | 53/54 top-1, MRR 0.981, 18/19, 0/4 | identical, query by query; also identical with llama.cpp queries on a Core ML index |
| Passages/s, 1,000 passages (avg. 396 tokens), battery | 26.6 | 36.2–37.6 (1.4×) |
| Passages/s, 2,856 passages (avg. 290 tokens), charger | 31.6 (the indexer's setup), 36.4 (text-only) | 44.0 (47.5 with all six shapes) |
| **Energy per 1,000 passages**, battery | **500 J** (13.6 W, almost all GPU) | **140 J** (5.5 W, 4.9 W of it ANE): **3.6× less** |
| Memory, one process | 215 MB footprint text-only, plus the 281 MB GGUF mapped (shared); 867 MB as the indexer loads it (text + mmproj) | 120–300 MB footprint (mostly malloc that grows and shrinks with calls), **plus 620 MB owned by the ANE**: not counted in `footprint`, clean, and reclaimable while a shape sits idle |
| Two processes | share the mapped GGUF | share the mapped table and the ANE copy (the second process is charged only 54 MB on the ANE) |
| Load | 0.3–0.7 s | 1.3 s warm. **First load on a Mac: ~31 s** (ANE compile, cached by macOS); 73 s with all six shapes |
| Download / disk | 310 MB (text GGUF) | +286 MB mlpackage, +256 MB bf16 table, +271 MB compiled `.mlmodelc`. The text GGUF stays, for the tokenizer and for pictures and sound |
| macOS | 14 | **15** (multifunction model) |
| Longest text | 2,048 tokens | 512 (6% of the tokens in the 1,000-passage set were cut) |

Findings along the way:

- **Each loaded shape keeps its own ~300 MB copy of the weights on the ANE.** One shape: 326 MB, two: 620 MB, all
  six: 1.69 GB. Two shapes (256 and 512) give 93% of the speed of six, at a third of the memory and half the compile.
- **Core ML can't compile an `.mlpackage` straight from the Hugging Face cache:** its symlinks break the compiler. It
  has to compile a real copy (the prototype does). Precompiled `.mlmodelc` folders (as in parakeet-ultra-coreml) load
  from the cache as they are.
- **The energy gain is the Neural Engine's.** Core ML's GPU path is about as fast in a short run (46.6 passages/s on
  64 passages, where llama.cpp does 37–42), but its energy wasn't measured. The ANE does the work at 4.9 W, where the
  GPU draws 12–13 W.
- **For scale:** indexing 100,000 such passages takes the chip about 14 Wh on llama.cpp b11461, 11 Wh on b11856 and
  4 Wh on the ANE.

## 2. Speech: FluidAudio (Parakeet Ultra on the ANE) vs transcribe.cpp 0.4.1

`fluid-transcribe/` is a Swift helper speaking `sift-it-transcribe`'s stdin/stdout protocol. Both were fed the same
three-minute chunks, cut the way `ingest/extract/speech.rs` cuts them (`speech.py`).

| | transcribe.cpp 0.4.1, GPU | FluidAudio Ultra, ANE | FluidAudio, GPU |
|---|---|---|---|
| Speed, 36-min English podcast, battery | 93× real time | **286×** (3.1×) | — |
| Speed, 62 min (36 English + 25 Dutch TTS), charger | 62× | 170× | 112× |
| **Energy per hour of audio**, battery | **553 J** (14.6 W: CPU 5.6, GPU 9.0) | **116 J** (9.5 W: CPU 6.3, ANE 3.1): **4.8× less** | — |
| Peak footprint | 273–332 MB (0.4.0: 955 MB) | 235–270 MB | 1.88 GB |
| Load | 0.2 s | 0.2–0.4 s; 8 s the first time on a Mac | 11 s first time |
| Model | 740 MB GGUF Q8_0 | 603 MB `.mlmodelc` (int8 encoder) | same |
| Helper binary | 9.0 MB | 19.4 MB (FluidAudio static, system frameworks only) | same |
| macOS | 14 | 14 | 14 |
| Transcripts vs transcribe.cpp (WER-style) | — | 7.5% English, 6.8% Dutch TTS; 6.4% / 6.9% without filler words | 0.7% / 0.55% vs FluidAudio ANE |

Neither transcript is ground truth. The differences are small words, fillers, and numbers: transcribe.cpp writes
"15", FluidAudio writes "fifteen". The demo data has no Dutch speech (all 30 podcasts and the NASA recordings are
English), so the Dutch in this table is 25 minutes of macOS TTS. Real Dutch follows.

### Real Dutch: FLEURS and two NOS podcast episodes

Measured on the charger, with the machine quiet. Both engines give identical transcripts on a second run.

**FLEURS Dutch test split, with human transcripts** (google/fleurs `nl_nl`, CC BY 4.0: 364 read sentences, 58 min).
Each clip went to the helper as its own chunk (`fleurs.py`).

| | WER, plain | WER, compounds merged | Speed | Peak footprint |
|---|---|---|---|---|
| transcribe.cpp 0.4.1 | 6.55% | **5.49%** | 103× | 250 MB |
| FluidAudio, ANE | 6.66% | **5.65%** | 150× | 58 MB |
| Published, moondream/parakeet-ultra model card | — | 5.46% | — | — |

- **Both ports match the published figure.** The model card scores with the Open ASR Leaderboard's normalizer and
  compound merging, which is closest to the "compounds merged" column. (The brief's "about 6.7%" isn't on the card:
  v3 scores 6.18% there.) The two engines disagree with each other on 1.2% of the words.
- **Normalization** (`dutch.py`), applied the same way to the reference and to both transcripts:
  - lower case, accents dropped (één = een), punctuation removed, hyphens and apostrophes split words;
  - numbers written as they're said: 1100–1999 as years ("1940" = negentienveertig), other numbers as cardinals,
    "11e" as an ordinal, "1,5" as één komma vijf, "10.000" as tienduizend, "%" as procent;
  - "Compounds merged" joins two neighbouring words when the other side has them as one word (verkeers stroom =
    verkeersstroom).
- **Inverse text normalization isn't a switch here.** FluidAudio's (NemoTextProcessing) only knows en, fr, es, de, zh,
  hi and ja, not Dutch. It's also a separate call, not an ASR option.
- With short clips, each call's fixed cost weighs more, so the ANE's lead shrinks to 1.5×.

**NOS "De Dag", two episodes of conversation** (a host with reporters and guests, 47 min). Fed in sift-it's
three-minute chunks. There's no reference, so this table shows agreement only.

| | transcribe.cpp 0.4.1 | FluidAudio, ANE |
|---|---|---|
| Speed | 83–90× real time | **256–257×** (3×) |
| Peak footprint | 313–316 MB | 294–295 MB |
| Disagreement (WER-style) | | 3.3% of 7,891 words (3.1% without fillers), in 186 spans |

I sorted 60 of the 186 spans (spread evenly over both episodes) from their context. I didn't listen to the audio, so
the calls are about plausibility:

| Kind | Spans | Examples |
|---|---|---|
| number or format | 4 | je/jij, word/wordt, "Huize Soria" spelled two ways |
| filler or repeated word | 9 | ja, nee, "dan dan", "een een" |
| **error by transcribe.cpp** | **10** | "ronde brokken" for hondenbrokken, "staande" for staartje, "afkleden" for afknippen, two garbled clauses |
| **error by FluidAudio** | **19** | the programme name as "nieuwshuur" (twice), the DJ Isis as "ises", three dropped phrases (one English), garbled words ("vrouwenondrukking", "samend draaien") |
| unclear | 18 | both read naturally: de/een, al/wel, singular or plural, he or she |

- **What it means, with FluidAudio's defaults:** on read Dutch the two are equally accurate. On conversation,
  FluidAudio makes about twice as many of the clear errors: roughly 0.5 point more WER, spread over the whole
  disagreement.
- **Names and dropped phrases matter most for search.** FluidAudio decodes a long chunk in 15 s windows, while
  transcribe.cpp takes the whole chunk at once. Sent 14 s chunks instead (one window each), two of FluidAudio's three
  dropped phrases come back, but overall disagreement rises (3.75% against 3.38%, file by file).

### FluidAudio's window settings

FluidAudio cuts a long chunk into windows of 15 s (`ASRConstants.maxModelSamples`), with a 2 s overlap. On v3-family
models like Ultra, it starts each window at a nearby silence. It merges neighbouring windows by matching token IDs
within a time tolerance (an LCS fallback, then the overlap's midpoint), and then re-decodes gaps at the seams
(`Documentation/ASR/LongTranscription.md`).

- **Not configurable through its API:** the window, the overlap, the stride and the merge are constants.
- **Configurable through `ASRConfig`:**
  - `melChunkContext`: an 80 ms mel prefix and fixed strides, instead of silence-aligned starts; off by default for
    v3-family models;
  - `dualDecodeArbitration`: probe three seam strategies, keep one per file;
  - `seamGapRepair` and its gap threshold (1.5 s).
- **Not tried:** its overlap-free "pause-cut" windows only work with a multifunction encoder, which Ultra doesn't ship.
  Instead, our helper can do the same: cut each chunk at the latest pause (FluidAudio's own rule) into windows of at
  most 15 s, and decode each one alone (`FLUID_PAUSE_CUT=1`).

Each setting ran on the same NOS chunks. The 60 spans were judged again in each new transcript
(`rejudge.py`). A sample of 20 of the new disagreements was judged too, for the two settings that changed much.

| FluidAudio setting | Disagreement (all / no fillers) | Of the 60 spans: FluidAudio / transcribe.cpp errors | New disagreements (20 judged: FA / tcpp errors) | Estimated clear errors, all spans: FA / tcpp | Speed |
|---|---|---|---|---|---|
| defaults | 3.32% / 3.09% | 19 / 10 | — | ~59 / ~31 | 258× |
| **`melChunkContext: true`** | 3.43% / 3.14% | **12** / 10 (7 fixed, none broken) | 53 (4 / 2) | **~48 / ~36** | 269× |
| `dualDecodeArbitration: true` | 3.32% / 3.06% | 19 / 10 (one filler changed) | — | ~59 / ~31 | 125× |
| seam repair from 0.5 s gaps | 3.40% / 3.18% | 18 / 10 | — | — | 194× |
| seam repair off | 3.32% / 3.06% | 19 / 10 | — | — | 376× |
| pause-cut windows, in our helper | 3.76% / 3.54% | 12 / 10 (10 fixed, 4 broken) | 88 (5 / 1) | ~59 / ~29 | 183× |

- **Mel context narrows the gap, without closing it.** It fixes 7 of the 19 errors in the sample: "Nieuwsuur" once,
  "Isis", two of the three dropped phrases, "bereikt" and two small words. It breaks none of the spans where
  FluidAudio was right. Its new disagreements are mostly fillers and small words; in the sample, 4 are its errors
  ("gasthouder" for the Gashouder) and 2 are transcribe.cpp's.
- **The gap goes from roughly 0.5 to roughly 0.2 points of WER**, at no cost in speed. These are estimates from
  samples of 60 and 20 spans, judged from text.
- **The same check elsewhere:**
  - FLEURS Dutch: 5.67% against 5.65%, unchanged. Its clips each fit one window, where the setting doesn't apply.
  - The English podcast: agreement without fillers improves (6.28% against 6.42%), and one dropped run of words comes
    back. Its extra deletions are fillers.
  - No language drift showed up in either. That's the reason FluidAudio leaves it off by default (its issues
    #594 and #803).
- **Pause-cut windows aren't worth the code.** They fix as many errors as they make elsewhere.
- **Dual decode and the seam-repair settings change almost nothing** on these recordings. The repair pass only costs
  speed: 258× with it, 376× without.
- **Caveat:** FluidAudio's docs say which seams fail can shift after the ANE recompiles the model, so results may vary
  a little across Macs and OS updates. On this Mac, the same build gives identical transcripts run after run.

## 3. llama.cpp b11856 (today's latest) vs b11461

| | b11461 | b11856 |
|---|---|---|
| Text, passages/s, 1,000 passages, battery | 26.6 | **31.4** (+18%) |
| Text, energy per 1,000 passages | 500 J | **408 J** (−19%; one run) |
| Photo, 140 tokens | 120 ms, 1.88 J | 123 ms, 1.84 J |
| Video frame, 70 tokens | 59 ms, 0.81 J | 62 ms, 0.79 J |
| 30 s of sound | 222 ms, 2.90 J | 211 ms, 2.59 J (−11%) |
| Vectors | — | **cosine 1.00000 to b11461** on 2,856 passages; `selftest` worst 0.99911 on both |
| `make eval` | 53/54, MRR 0.981 | identical |
| Builds with sift-it as it is | yes | yes: same C API; the logits patch is upstream now (`has_logits = !cparams.embeddings \|\| …`) |
| Residency | 180 s, fixed | `GGML_METAL_RESIDENCY_KEEP_ALIVE_S` knob |

The Metal 4 tensor API is on for M5 in **both** versions, so b11461 already uses it. Turning it off
(`GGML_METAL_TENSOR_DISABLE=1`) makes the matmuls 3.3× slower. The upgrade's gain comes from elsewhere.

## 4. Where prefill time goes (llama.cpp Metal, b11856)

How time scales with input length (one text a call): there's a **~10 ms fixed cost per call**. A token costs
0.078–0.085 ms between 224 and 840 tokens, and attention adds a little beyond that. Packing up to 16 passages into
one 2,048-token call (as the indexer does) costs nothing: the same tokens/s as one passage a call, because flash
attention skips the masked blocks. So there's nothing to recover there.

The per-operation profile comes from llama.cpp's eval callback (`SIFT_IT_PROFILE_OPS`). It waits after every node; the
floor of ~205 µs per node was subtracted, and the result checked against the unprofiled run (135 ms per call of about
1,700 tokens). Matmul and attention throughput come from `opbench/` (the model's own shapes, alone on the GPU):

| Part of a call | Share | Throughput |
|---|---|---|
| Matmul (219 per call, Q8_0) | ~38% | 8–10 TFLOPS, **already at this GPU's large-GEMM yardstick** (4096³: 9.5 TFLOPS f16, 7.9 TFLOPS Q8_0) |
| Flash attention (24, heads of 256 and 512) | ~17% | 1.8 TFLOPS at 290 tokens, 4.6–5.3 at 1,024–2,048 |
| Element-wise and copies: RMS norm, MUL, the KV-cache copies, GEGLU, RoPE, PAD, CONT, GET_ROWS | ~45% | memory-bound, spread over ~1,200 nodes a call |

**Per-device tuning, Magnitude-style:** matmul is at the yardstick, so its tiles have ≤10% left. Flash attention
might give 1.5×, about 6% of the total. The element-wise half is a matter of fusion, not of tiles. **A realistic total
is ≤10%.** It would also take a fork: the tile sizes are compile-time constants in `kernels/mul_mm.metal` (64×32×32,
plus a fixed `matmul2d_descriptor` on the tensor path), and the dispatch is hard-coded in `ggml-metal-ops.cpp`.
Upstream is already adding offline tables per GPU family (`ggml-metal-tuning.cpp`, for decode-time attention). One
llama.cpp upgrade gave more (+18%) than tuning could. Not worth building.

## Recommendation

1. **Upgrade llama.cpp to the latest release (worth doing now; small).** +18% text speed, −19% text energy, −11%
   sound energy. Vectors are identical and the eval doesn't change. The logits patch can go. Cost: bump TAG/COMMIT in
   `scripts/build-llama.sh`. `vector_source` carries the build tag, so indexes would re-embed for nothing:
   consider keeping the tag out of it while `selftest` holds.
2. **Move speech to FluidAudio on the ANE (worth building, as the default).** It's 3× faster with 4.8× less energy,
   about the same memory, a smaller model, and still macOS 14. Cost:
   - a Swift helper (~90 lines; the prototype already speaks the protocol) replaces the `transcribe/` crate and
     transcribe.cpp's second copy of ggml;
   - FluidAudio, pinned through SwiftPM;
   - a 19 MB binary to sign;
   - the model comes from `FluidInference/parakeet-ultra-coreml` in the HF cache;
   - bump the speech `SOURCE`, so transcripts are written again.

   **It still stands for Dutch, with `melChunkContext: true`.** On FLEURS both engines reach the published WER
   (5.65–5.67% vs 5.49%). On conversational NOS Dutch, FluidAudio with its defaults makes about twice as many clear
   errors as transcribe.cpp, roughly 0.5 point of WER. Mel context brings that to roughly 0.2 points, at no speed
   cost, against 3× the speed and 4.8× less energy. It's one line in the helper:
   `ASRConfig(…, melChunkContext: true)`. The rest of FluidAudio's seam settings, and windows of our own, aren't worth
   it. Its inverse text normalization has no Dutch, so Dutch numbers come out as the model writes them.
3. **Core ML for text (worth building for the indexer, but it's the costly one; after 1 and 2).** It uses 3.6× less
   energy for 1.2–1.4× the speed, with an identical eval. The cost:
   - a second runtime for one model, while pictures and sound stay on llama.cpp;
   - ~800 MB more on disk;
   - ~620 MB of ANE memory while indexing (freed when idle, shared between processes);
   - a ~30 s first load on each Mac;
   - macOS 15+, with llama.cpp kept for macOS 14.

   **Make it the indexer's text path whenever it's available, not a battery-only one.** It's faster on the charger
   too, and at 5.5 W vs 13.6 W it runs much cooler on a fanless Air. Switching by power source would mix two
   pipelines' vectors in one index (they agree at cosine 0.9999, but `vector_source` can't say so) for no gain.
   **Keep queries on llama.cpp:** 0.3 s to load, ~10 ms per query, no ANE memory, and the eval is the same with
   mixed vectors. If footprint and one runtime matter more than battery while indexing, skip it: the llama.cpp
   upgrade takes 19% off on its own.
4. **Per-device kernel tuning: don't.** At most ~10%, it needs a ggml-metal fork, and upstream is already catching up.

## Not measured

- Sustained runs. Thermal pressure stayed Nominal in 30–40 s bursts at 13–15 W. Four-minute runs (in `suite.sh full`)
  would show whether the GPU path throttles on this Air, which would strengthen the case for the ANE.
- Energy on real Dutch speech: only its speed and accuracy were measured. Energy should follow the speed, as it did
  for English.
- FluidAudio's CPU share (6.3 W of 9.5 W, likely the decoder loop). It may leave room for improvement there.
- Energy with Phase 1's `GGML_METAL_NO_RESIDENCY=1`. These runs use main's residency sets.
- The design docs' "17.7 passages/s" is another Mac's figure: `bench-embed` gives 46.6 passages/s here.

## The code in this branch

| Where | What |
|---|---|
| `crates/llama/src/coreml.rs`, `lib.rs` | Core ML text encoder: `SIFT_IT_COREML=<folder>`, `SIFT_IT_COREML_FUNCTIONS=256,512`, `SIFT_IT_COREML_UNITS=ane\|gpu`; op profile (`SIFT_IT_PROFILE_OPS`) |
| `src/tools/bench.rs`, `src/main.rs` | `bench-texts <file> [--text-only] [--batch N] [--seconds S] [--out vectors.f32]`, `bench-media <count>` |
| `src/ingest/embed.rs` | `SIFT_IT_DUMP_TEXTS=<file>`: the texts the indexer embeds, for `bench-texts` |
| `scripts/build-llama.sh`, `crates/llama/build.rs` | `LLAMA_TAG/LLAMA_COMMIT` build another release; `SIFT_LLAMA_BUILD` links it |
| `transcribe/Cargo.toml` | transcribe-cpp 0.4.1 |
| `experiments/energy-and-speed/` | `suite.sh` (energy suite), `power.py` (energy per workload), `live.py` (watch it live), `speech.py` + `speech-set.sh` + `wer.py` (speech), `fleurs.py` + `dutch.py` + `disagree.py` + `rejudge.py` (Dutch WER, agreement, judged spans), `fluid-transcribe/` (Swift helper), `opbench/` (ggml op benchmark), `cosine.py`, `eval.sh`, `memory.sh` |

To run it again: `suite.sh` lists what it needs in the environment. Point `SIFT_IT_MODELS` at the GGUF snapshot in
the HF cache, and start the suite with `POWER=<file>` while `sudo powermetrics … -o <file>` runs.
