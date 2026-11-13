# REMAINDER 0.3.0 development comparison — November 12, 2026

The tested local build scored **20/50 versus MemPalace's 17/50**. It passed **112 unit and regression tests**, and all 50 repeated query outputs matched exactly.

| Measurement | REMAINDER | MemPalace |
|---|---:|---:|
| Correct answers | 20/50 | 17/50 |
| Median ingestion | 9.97 s | 19.77 s |
| Median warm retrieval | 30.69 ms | 41.43 ms |
| Mean warm retrieval | 42.88 ms | 99.95 ms |
| 95th percentile warm retrieval | 152.69 ms | 180.00 ms |
| Errors | 0 | 0 |

The measured median advantages were **1.98× for ingestion** and **1.35× for warm retrieval** on the tested Windows PC.

## Method and provenance

Both systems used the same 50 evaluation questions, official prompt, reader, top-five retrieval limit, and 4,000 cl100k BPE-token evidence ceiling. The confirmation question was excluded. Reader: `hf.co/QuantFactory/Mistral-Nemo-Instruct-2407-abliterated-GGUF:Q4_K_M`; temperature 0, seed 20261019, context 8192, maximum response 96 tokens.

Warm timings were measured again for both systems on all 50 questions. Ingestion timings come from the earlier freshly built full-coverage cohort; passage construction and embedding inference are unchanged. Subsequent hardening also adds orphan-index cleanup. MemPalace answers are validated controls. Reader answers were reused only for identical evidence, question, model and reader settings. These are retrieval timings; the reader's generation latency is excluded.

Sample SHA-256: `01e4ceff756b3045041c91268729d28162ca24cac90d84b942ada17702f13749`.
MemPalace commit: `bc7557e7ce9ac88594286affe3769292e202d777`.
REMAINDER binary SHA-256: `8831f2623dc3dce4a41825fdc6dd2deff9a3bf0c26fb23c82fe6b2456d676c63`.

Selected settings: character windows, native ONNX Runtime with four CPU threads, lexical query filtering with the original semantic query, RRF k=1, adaptive dense weight, MMR penalty=0, standard 1,000-character excerpts, optional reranker disabled. The adaptive policy uses semantic weight 2 for numeric, quoted, identifier, CamelCase or acronym queries and 0.5 for other prose.

## Limits

This is a **development set inspected during tuning**, including comparison of both directions of query routing. The implementation was verified against the selected fixed-weight controls and repeated in the final build. It is not an untouched holdout. The three-question lead does not establish statistical or universal superiority; the paired exact McNemar p-value was 0.5811.

Rejected defaults include token windows, wider excerpts, evidence packing and TinyBERT reranking. Their optional implementations remain experimental. Exact dense scanning remains in use; there is no ANN index. Historical revision queries and Qwen IDE integration were not added in this performance upgrade.

The source changes are published in version 0.3.0.
