# Reference vectors

`photo.jpg`, `screen.png`, `speech.wav` and `vectors.json` come from DigUp's test suite
(https://github.com/ARahim3/DigUp, `Tests/LlamaRuntimeTests/Reference`, MIT License, © Abdur Rahim). The vectors were
made by EmbeddingGemma 2 (ggml-org Q8_0) on llama.cpp b11461, which DigUp measured at cosine ≥ 0.999 against the
transformers/MLX reference. `sift selftest` checks that this engine makes the same vectors for the same inputs.
