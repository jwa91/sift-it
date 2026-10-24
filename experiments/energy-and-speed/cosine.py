"""Cosine between two vector files from `sift-it bench-texts --out` (768 little-endian f32 per text, same order).

    uv run --with numpy python cosine.py a.f32 b.f32 [texts.jsonl]
"""
import json
import sys

import numpy as np

a = np.fromfile(sys.argv[1], dtype="<f4").reshape(-1, 768)
b = np.fromfile(sys.argv[2], dtype="<f4").reshape(-1, 768)
assert a.shape == b.shape, (a.shape, b.shape)
a /= np.linalg.norm(a, axis=1, keepdims=True)
b /= np.linalg.norm(b, axis=1, keepdims=True)
cos = (a * b).sum(axis=1)
print(f"n={len(cos)} min={cos.min():.5f} p1={np.percentile(cos, 1):.5f} p5={np.percentile(cos, 5):.5f} "
      f"median={np.median(cos):.5f} mean={cos.mean():.5f} below_0.99={int((cos < 0.99).sum())} below_0.999={int((cos < 0.999).sum())}")
# Does each vector's nearest neighbours among the others stay the same? (a cheap stand-in for ranking)
sa, sb = a @ a.T, b @ b.T
np.fill_diagonal(sa, -2)
np.fill_diagonal(sb, -2)
top1 = (sa.argmax(axis=1) == sb.argmax(axis=1)).mean()
k = 10
ta, tb = np.argsort(-sa, axis=1)[:, :k], np.argsort(-sb, axis=1)[:, :k]
overlap = np.mean([len(set(x) & set(y)) / k for x, y in zip(ta, tb)])
print(f"nearest neighbour kept: {top1:.3f}, top-{k} overlap: {overlap:.3f}")
if len(sys.argv) > 3:
    texts = [json.loads(line) for line in open(sys.argv[3])]
    for i in np.argsort(cos)[:3]:
        print(f"  {cos[i]:.4f} {len(texts[i])} chars: {texts[i][:100]!r}")
