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

---

## Update 2026-06-26 — singular fast path + rsonpath reference

The "Finding" above is the **original cold pre-optimisation snapshot**. Since then the path-free value path, streaming traversal, inlining, `$..name` specialisation, and buffer presizing landed (commits up to `ffab473`), making jp-full faster-or-equal to jsonpath_lib on every case. This update adds the **singular fast path** and a third comparison engine.

### Singular fast path (this change)

A query whose every segment is a single child name/index step (`$.a.b[0].c`) selects at most one node, so it needs no worklist and no per-segment `Vec`. `JsonPath` precomputes this form (`compiled::Query::singular`) and threads a single `&Value` down the document — eliminating ~5 small heap allocations per call. The general worklist path is left byte-for-byte unchanged (dispatch lives in `JsonPath::query`/`query_values`), so non-singular queries are algorithmically untouched.

Headline effect (eval-only, jp-full vs jsonpath_lib, Apple M4 Pro, warm):

| size | case | jp-full before | jp-full after | jsonpath_lib | after vs jsonpath_lib |
|---|---|---|---|---|---|
| 10k | child | 75 ns | **37 ns** (−51%) | 228 ns | **6.1× faster** (was ~3×) |

`query/child/*` in `benches/queries.rs` shows the same −52% at every document size (the lookup is size-independent). Other shapes (`wildcard`, `descendant`, filters) move only within the ±5–10% layout/thermal noise floor at these scales — verified by opposite-sign deltas for the same query across harnesses/runs — i.e. no real regression.

### rsonpath in the comparison bench (`scan/` group)

`rsonpath` (0.10) is a raw-bytes + SIMD engine: it never builds a `serde_json::Value`. It can't share the `eval/` group (which times a *pre-parsed* DOM), so the new `scan/*` group times the full **text → matches** pipeline — `serde_json::from_str` + query for the DOM engines, a byte `count` for rsonpath. It covers only the non-filter cases (rsonpath has no filter support) and asserts node-count equivalence before benching.

| scan/10k (text → matches) | jp-full (parse+query) | jsonpath_lib | rsonpath (count) |
|---|---|---|---|
| child | 10.8 ms | 10.9 ms | **435 µs** |
| author_wildcard | 10.9 ms | 11.4 ms | **910 µs** |

The DOM engines spend ~10.7 ms building a `Value` from the 2 MB document; the query itself is microseconds. rsonpath is ~12–25× faster here **only because it skips DOM construction** — the cost a `Value`-based engine pays up front and a byte engine never does. This is the honest framing: when you already hold a `Value` (parse once, query many — the `eval/` group), jp-full is the fast one; when you have raw text and scan once, a byte engine wins. The transferable byte-engine techniques (query→automaton, subtree pruning, allocation discipline) are already applied in `eval.rs`; SIMD structural classification does not apply to a materialised DOM.

### New measurement tooling

`benches/queries.rs` gained a `micro/*` group: `micro/path_overhead/*` runs the same query via `query_values` (no paths) and `query` (builds a `NormalizedPath` per node), so the delta is exactly the per-node `Rc` path-construction cost; `micro/singular/*` exercises the fast path at increasing depth.

### Filter optimisations (validate → POC → A/B)

Two filter cost centres, each validated by a dedicated bench (`filter_exists`, `filter_string_eq` in `queries.rs`), POC'd, then A/B'd against a same-session baseline:

| filter @10k | before | after | change | mechanism |
|---|---|---|---|---|
| `?@.category == 'fiction'` | 456 µs | ~278 µs | **−39%** | precompute the comparison literal as a `serde_json::Value` once at compile time (`Comparable::Literal(Box<Value>)`) and borrow it, instead of rebuilding + heap-cloning the string on every element |
| `?@.isbn` (existence) | 531 µs | ~190 µs | **−64%** (−64% @1k) | a *singular* existence sub-query (`?@.a.b`) becomes a path-free `eval_singular(...).is_some()` presence check (`ExistenceTest::Singular`), skipping the per-element nodelist `Vec`; non-singular existence still walks |

Numeric comparison (`?@.price < 10`) is unchanged-to-−6% (no heap literal to save). Both changes touch only the filter IR (`Comparable`/`ValueArg`/`ExistenceTest`); non-filter query shapes are unaffected (verified within the ±5% noise floor vs the committed baseline). The literal is `Box`ed to keep `Selector`/`Comparable` small (an inline `Value` tripped `large_enum_variant`).

### Borrowed path names (paths API)

`NormalizedPath` construction was the biggest measured cost for the **paths** API
(`micro/path_overhead`: 4.2×–13.8× the value path): each name step did *two*
allocations — the parent-chain `Rc<Link>` plus an `Rc::from(name)` that copied the
member name. But the normalized name of a selected node is always one of the queried
document's own map keys, so it can be *borrowed* instead of copied. `NormalizedPath`
now carries a lifetime (`NormalizedPath<'a>`) and stores `Step::Name(&'a str)`; the
evaluator obtains the document key via `Map::get_key_value` at each name step. This
removes the per-name-step string allocation entirely (only the `Rc<Link>` remains).

A/B (`micro/path_overhead`, query() vs query_values):

| query @10k | paths before | paths after | change |
|---|---|---|---|
| `$.store.book[*].author` | 754 µs | ~571 µs | **−24%** |
| `$..price` | 5.34 ms | ~3.16 ms | **−41%** |
| `$..*` | 10.62 ms | ~6.23 ms | **−41%** |

The value path (`query_values`) is unaffected — `get_key_value` is equivalent work to
`get`, and the `()` position implementation ignores the borrowed name. Verified neutral
vs the committed baseline; CTS 703/703 (values + normalized paths) still passes, so paths
render identically.

API note: `NormalizedPath` gaining a lifetime is a breaking change (a path can no longer
outlive the document it locates into — neither could a `NodeList`, which already borrows
the selected values). Acceptable at 0.1.0 / pre-publication.

### Pruned descendant recursion (lazy paths, the cheap form)

Descendant traversal (`descend`/`descend_name`) recursed into *every* member/element,
building a path step (`Rc<Link>` for the paths API) to do so — but a scalar has no
descendants, so recursing into it can never select anything. Guarding the recursion on
`is_container` (object/array) skips that dead-end work: no path step is built to descend
into a leaf. This is the cheap, low-risk form of "lazy" path construction — don't build a
path you'll throw away — and it captured more than a full trail-based deferral would have:

| micro @10k | before | after | change |
|---|---|---|---|
| `$..price` paths | ~3.16 ms | ~1.4 ms | **−56%** |
| `$..*` paths | ~6.23 ms | ~4.2 ms | **−33%** |
| `$..*` values | ~790 µs | ~535 µs | **−32%** |
| `$..price` values | ~665 µs | ~635 µs | −4% |

It helps the **value** path too (skipping the per-leaf `descend` call — `apply_selector`
+ match — that always selected nothing), which a deferred-materialisation rewrite would
not. CTS 703/703 (values + normalized paths) still passes. Child/wildcard queries don't
recurse, so they're unaffected.

### Pre-compiled regex patterns (`match` / `search`) — the big one

`match()`/`search()` recompiled the regex on **every element**: `regex_test` called
`iregexp::build()` (translate + `Regex::new`) per filter evaluation. Regex *compilation*
dwarfs matching, so a filter over an N-element array paid N compiles. This is the kind of
per-element pathology that makes a JSONPath lib "too slow to use" in a rules engine.

A literal pattern (`match(@.x, "constant")`) is now translated and compiled **once** at
`JsonPath::parse` time, with the call's anchoring baked in, and stored in the IR
(`compiled::Pattern::Literal(Option<regex::Regex>)`); evaluation only matches. A pattern
computed from the document (`match(@.x, @.y)`) can't be precompiled and stays
`Pattern::Dynamic` (compiled per call) — rare, and a ReDoS smell anyway.

A/B (`benches/queries.rs`, eval-only, literal patterns):

| query | 1k before → after | 10k before → after |
|---|---|---|
| `?search(@.title, "Number 1")` | 1.56 ms → 27.5 µs (**−98%**) | 16.7 ms → 455 µs (**−97%**) |
| `?match(@.title, "Book Number [0-9]+")` | 8.81 ms → 37.5 µs (**−99.6%**) | 89.4 ms → 690 µs (**−99.2%**) |

`filter_regex_dynamic` (`?search(@.title, @.category)`, a document-derived pattern) is
**neutral** — verified against a *warm* baseline at +1.4% (the −8% the cool baseline
showed was thermal drift; the dynamic path's per-element compile is unchanged). CTS
703/703 (incl. invalid patterns → false, unicode, and the "regex from the document"
dynamic case) still passes.

Implementation notes: `Pattern` needs a manual `PartialEq`/`Eq` (compares regex source
via `Regex::as_str`) since `regex::Regex` isn't `Eq`. The `Function::Match`/`Search`
variants are now `#[cfg(feature = "regex")]`-gated (they hold a `Pattern`), which as a
bonus removes the two pre-existing "never constructed" dead-code warnings under
`--no-default-features`.

### Known remaining lever (not taken)

A full trail-based deferral (maintain a cheap step-stack during descent, materialise the
`NormalizedPath` only at a selected node) would additionally avoid the `Rc<Link>` for
*container* subtrees that contain no match (e.g. a `reviews` array under `$..price`). With
the `is_container` prune already taking −56% off `$..price` paths, the remaining headroom
is ~10–20% on that one query shape, at the cost of a new `Trail` trait + associated type
and loss of prefix-sharing across matches — not worth it at this point.

