# Performance baseline

The committed "where we started" snapshot, captured **before** any evaluation-path optimisation. After changes, compare with `just bench -- --baseline main` / `just bench-compare -- --baseline main` (criterion's own baseline lives in the gitignored `.criterion/`). Numbers are machine-specific — treat the *ratios*, not the absolutes, as the signal.

## Context

| | |
|---|---|
| Date | 2026-06-26 |
| Machine | Apple M4 Pro (macOS, Darwin 25.3.0) |
| Toolchain | rustc 1.92.0 |
| criterion | 0.8.2 |
| jsonpath-rust | 1.0.4 (RFC 9535) |
| jsonpath_lib | 0.3.0 (Goessner dialect) |
| Sampling | quick run: sample-size 20, warm-up 0.4s, measure 1.2s (a full `just bench-save-baseline` uses criterion defaults) |
| Fixtures | benches/data/bookstore-1k.json, bookstore-10k.json |

## Headline: eval-only (compile once, query many) — jp-full vs jsonpath_lib

**Goal: jp-full ≤ jsonpath_lib on every case.** Not met for traversal-heavy queries.

| size | case | jp-full | jsonpath_lib | result |
|---|---|---|---|---|
| 1k | child | 262.20 ns | 222.01 ns | 1.2× slower |
| 1k | author_wildcard | 74.31 µs | 12.58 µs | 5.9× slower |
| 1k | descendant_price | 674.19 µs | 58.65 µs | 11.5× slower |
| 1k | nested_wildcard | 233.64 µs | 36.27 µs | 6.4× slower |
| 1k | filter_cheap | 24.97 µs | 57.64 µs | 2.3× faster |
| 1k | filter_search | 1.67 ms | — | jp-full only (no Goessner regex) |
| 10k | child | 263.68 ns | 222.25 ns | 1.2× slower |
| 10k | author_wildcard | 912.81 µs | 193.22 µs | 4.7× slower |
| 10k | descendant_price | 7.22 ms | 719.70 µs | 10.0× slower |
| 10k | nested_wildcard | 3.13 ms | 513.39 µs | 6.1× slower |
| 10k | filter_cheap | 321.30 µs | 690.45 µs | 2.1× faster |
| 10k | filter_search | 18.62 ms | — | jp-full only (no Goessner regex) |

## End-to-end (parse + evaluate per call) — all three engines

| size | case | jp-full | jsonpath_lib | jsonpath-rust |
|---|---|---|---|---|
| 1k | child | 745.81 ns | 677.35 ns | 2.88 µs |
| 1k | author_wildcard | 76.12 µs | 13.29 µs | 185.99 µs |
| 1k | descendant_price | 683.94 µs | 59.58 µs | 2.03 ms |
| 1k | nested_wildcard | 234.43 µs | 36.65 µs | 517.32 µs |
| 1k | filter_cheap | 25.31 µs | 57.29 µs | 120.17 µs |
| 1k | filter_search | 1.67 ms | — | 1.82 ms |
| 10k | child | 726.44 ns | 673.30 ns | 2.88 µs |
| 10k | author_wildcard | 917.77 µs | 192.09 µs | 1.97 ms |
| 10k | descendant_price | 7.34 ms | 679.18 µs | 21.49 ms |
| 10k | nested_wildcard | 3.07 ms | 531.47 µs | 5.69 ms |
| 10k | filter_cheap | 328.09 µs | 696.27 µs | 1.35 ms |
| 10k | filter_search | 18.78 ms | — | 19.71 ms |

## jp-full scaling (benches/queries.rs) — evaluation only

| query | small | 1k | 10k |
|---|---|---|---|
| child | 267.17 ns | 263.77 ns | 265.08 ns |
| wildcard | 490.73 ns | 74.34 µs | 866.05 µs |
| descendant | 1.45 µs | 688.69 µs | 7.34 ms |
| filter_comparison | 294.76 ns | 25.17 µs | 316.14 µs |
| filter_function | 513.58 ns | 105.62 µs | 1.33 ms |
| filter_search | 6.84 µs | 1.66 ms | 18.35 ms |
| descendant_wildcard | 2.22 µs | 1.19 ms | 12.67 ms |

### parse (compile + §2.4 type-check)

| query | time |
|---|---|
| child | 400.19 ns |
| wildcard | 382.15 ns |
| descendant | 113.01 ns |
| filter_comparison | 573.68 ns |
| filter_function | 977.86 ns |
| filter_search | 1.63 µs |
| descendant_wildcard | 100.30 ns |

## Finding

jp-full is **4–11× slower than jsonpath_lib** on wildcard/descendant queries, ≈1.2× on a single-node lookup, and **~2× faster** on the comparison filter. The gap scales with the number of nodes *visited*.

**Root cause (confirmed in `src/eval.rs`):** every traversal step allocates a `NormalizedPath` link (`Rc::new(Link)` via `child_name`/`child_index`) for each node visited — `collect_descendants` does this for the whole tree on `$..`, `apply_wildcard` for every child — and `query_values()` builds the full path-bearing `NodeList` then discards the paths. jsonpath_lib collects only `&Value`, with no per-node allocation. `search()` is ~on par with jsonpath-rust (regex dominates).

