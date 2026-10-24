// Where EmbeddingGemma 2's prefill time goes on Metal: its matrix multiplications (all 24 layers' weights, Q8_0, at
// their real shapes) and its attention (flash attention at its head sizes), each timed alone on the GPU at a number of
// tokens. Compared with the whole model's time at the same tokens (sift-it bench-texts), the rest is everything else:
// norms, RoPE, activations, the per-layer inputs, and launching ~1,000 kernels.
//
//   opbench [tokens…]        (default 128 512 2048)
//
// Built against the static ggml from scripts/build-llama.sh: see build.sh.
#include "ggml-alloc.h"
#include "ggml-backend.h"
#include "ggml-cpu.h"
#include "ggml-metal.h"
#include "ggml.h"

#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <random>
#include <vector>

struct Shape {
    int k, m, count;  // input width, output width, how many of them in the model
};

// From the GGUF (embeddinggemma-2-Q8_0): 24 layers, hidden 512, 20 sliding-window layers (4 query heads of 256,
// 2 key/value heads) and 4 global ones (4 query heads of 512, 1 key/value head of 512).
static const std::vector<Shape> MATMULS = {
    {512, 1024, 20}, {512, 2048, 4},  // attn_q
    {512, 512, 24},                   // attn_k
    {512, 512, 24},                   // attn_v
    {1024, 512, 20}, {2048, 512, 4},  // attn_output
    {512, 2048, 24},                  // ffn_gate
    {512, 2048, 24},                  // ffn_up
    {2048, 512, 24},                  // ffn_down
    {512, 512, 24},                   // inp_gate
    {512, 512, 24},                   // proj
    {512, 768, 1},                    // output
};

struct Attention {
    int head, heads, kv_heads, count;
};
static const std::vector<Attention> ATTENTION = {{256, 4, 2, 20}, {512, 4, 1, 4}};

static double now() {
    return std::chrono::duration<double>(std::chrono::steady_clock::now().time_since_epoch()).count();
}

static void fill(ggml_tensor * t, std::mt19937 & rng) {
    std::normal_distribution<float> normal(0.0f, 0.05f);
    const int64_t n = ggml_nelements(t);
    std::vector<float> values(n);
    for (auto & v : values) v = normal(rng);
    if (t->type == GGML_TYPE_F32) {
        ggml_backend_tensor_set(t, values.data(), 0, ggml_nbytes(t));
    } else if (t->type == GGML_TYPE_F16) {
        std::vector<ggml_fp16_t> half(n);
        ggml_fp32_to_fp16_row(values.data(), half.data(), n);
        ggml_backend_tensor_set(t, half.data(), 0, ggml_nbytes(t));
    } else {
        std::vector<char> data(ggml_nbytes(t));
        ggml_quantize_chunk(t->type, values.data(), data.data(), 0, t->ne[1], t->ne[0], nullptr);
        ggml_backend_tensor_set(t, data.data(), 0, data.size());
    }
}

// Times `build`'s graph: a warm-up, then the median of 10 runs.
template <typename Build>
static double time_graph(ggml_backend_t backend, Build build, double * flops) {
    ggml_init_params params = {ggml_tensor_overhead() * 4096 + ggml_graph_overhead_custom(8192, false), nullptr, true};
    ggml_context * ctx = ggml_init(params);
    ggml_cgraph * graph = ggml_new_graph_custom(ctx, 8192, false);
    std::vector<ggml_tensor *> inputs;
    *flops = build(ctx, graph, inputs);
    ggml_backend_buffer_t buffer = ggml_backend_alloc_ctx_tensors(ctx, backend);
    std::mt19937 rng(42);
    for (ggml_tensor * t : inputs) fill(t, rng);
    ggml_backend_graph_compute(backend, graph);
    std::vector<double> runs;
    for (int i = 0; i < 10; i++) {
        double started = now();
        ggml_backend_graph_compute(backend, graph);
        runs.push_back(now() - started);
    }
    std::sort(runs.begin(), runs.end());
    ggml_backend_buffer_free(buffer);
    ggml_free(ctx);
    return runs[runs.size() / 2];
}

int main(int argc, char ** argv) {
    std::vector<int> tokens;
    for (int i = 1; i < argc; i++) tokens.push_back(atoi(argv[i]));
    if (tokens.empty()) tokens = {128, 512, 2048};
    ggml_backend_t backend = ggml_backend_metal_init();
    if (!backend) {
        fprintf(stderr, "no Metal backend\n");
        return 1;
    }
    // What this GPU reaches on one large multiplication, as a yardstick for the model's small ones.
    for (ggml_type type : {GGML_TYPE_F16, GGML_TYPE_Q8_0}) {
        double flops = 0;
        double t = time_graph(backend, [&](ggml_context * ctx, ggml_cgraph * graph, std::vector<ggml_tensor *> & inputs) {
            ggml_tensor * w = ggml_new_tensor_2d(ctx, type, 4096, 4096);
            ggml_tensor * x = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, 4096, 4096);
            inputs.insert(inputs.end(), {w, x});
            ggml_build_forward_expand(graph, ggml_mul_mat(ctx, w, x));
            return 2.0 * 4096 * 4096 * 4096;
        }, &flops);
        printf("yardstick: 4096³ %s: %.1f ms, %.2f TFLOPS\n", ggml_type_name(type), t * 1e3, flops / t / 1e12);
    }
    printf("%6s  %10s %8s  %10s %8s\n", "tokens", "matmul ms", "TFLOPS", "attn ms", "TFLOPS");
    for (int n : tokens) {
        double mm_flops = 0, at_flops = 0;
        double mm = time_graph(backend, [&](ggml_context * ctx, ggml_cgraph * graph, std::vector<ggml_tensor *> & inputs) {
            double flops = 0;
            ggml_tensor * x[3] = {};
            for (int i = 0; i < 3; i++) {
                x[i] = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, 512 << i, n);
                inputs.push_back(x[i]);
            }
            for (const Shape & s : MATMULS) {
                for (int c = 0; c < s.count; c++) {
                    ggml_tensor * w = ggml_new_tensor_2d(ctx, GGML_TYPE_Q8_0, s.k, s.m);
                    inputs.push_back(w);
                    ggml_tensor * in = x[s.k == 512 ? 0 : s.k == 1024 ? 1 : 2];
                    ggml_build_forward_expand(graph, ggml_mul_mat(ctx, w, in));
                    flops += 2.0 * s.k * s.m * n;
                }
            }
            return flops;
        }, &mm_flops);
        double at = time_graph(backend, [&](ggml_context * ctx, ggml_cgraph * graph, std::vector<ggml_tensor *> & inputs) {
            double flops = 0;
            for (const Attention & a : ATTENTION) {
                for (int c = 0; c < a.count; c++) {
                    ggml_tensor * q = ggml_new_tensor_3d(ctx, GGML_TYPE_F32, a.head, n, a.heads);
                    ggml_tensor * k = ggml_new_tensor_3d(ctx, GGML_TYPE_F16, a.head, n, a.kv_heads);
                    ggml_tensor * v = ggml_new_tensor_3d(ctx, GGML_TYPE_F16, a.head, n, a.kv_heads);
                    ggml_tensor * mask = ggml_new_tensor_2d(ctx, GGML_TYPE_F16, n, n);
                    inputs.insert(inputs.end(), {q, k, v, mask});
                    ggml_tensor * out = ggml_flash_attn_ext(ctx, q, k, v, mask, 1.0f, 0.0f, 0.0f);
                    ggml_flash_attn_ext_set_prec(out, GGML_PREC_DEFAULT);
                    ggml_build_forward_expand(graph, out);
                    flops += 4.0 * n * n * a.head * a.heads;
                }
            }
            return flops;
        }, &at_flops);
        printf("%6d  %10.2f %8.2f  %10.2f %8.2f\n", n, mm * 1e3, mm_flops / mm / 1e12, at * 1e3, at_flops / at / 1e12);
    }
    ggml_backend_free(backend);
    return 0;
}
