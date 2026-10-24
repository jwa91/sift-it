# Reference vectors

`photo.jpg`, `screen.png`, `speech.wav`, and `vectors.json` come from
[DigUp's test suite](https://github.com/ARahim3/DigUp/tree/main/Tests/LlamaRuntimeTests/Reference).
Abdur Rahim holds the copyright for this material. [LICENSE](LICENSE) contains DigUp's original MIT notice.

DigUp generated the vectors with EmbeddingGemma 2 (ggml-org Q8_0) on llama.cpp b11461.
DigUp reports cosine ≥ 0.999 against the transformers/MLX reference.
`sift-it selftest` compares its output with these vectors for the same inputs, whichever llama.cpp release sift-it
is built with.
