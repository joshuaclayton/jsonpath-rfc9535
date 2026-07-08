# Performance baseline

The committed baseline snapshot, captured **before** any evaluation-path optimisation. After changes, compare with `just bench -- --baseline main` / `just bench-compare -- --baseline main` (criterion's own baseline lives in the gitignored `.criterion/`). Numbers are machine-specific; compare ratios, not absolute times.

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

## Headline: eval-only (compile once, query many) — jsonpath-rfc9535 vs jsonpath_lib

**Goal: jsonpath-rfc9535 ≤ jsonpath_lib on every case.** Not met for traversal-heavy queries.

| size | case | jsonpath-rfc9535 | jsonpath_lib | result |
|---|---|---|---|---|
| 1k | child | 262.20 ns | 222.01 ns | 1.2× slower |
| 1k | author_wildcard | 74.31 µs | 12.58 µs | 5.9× slower |
| 1k | descendant_price | 674.19 µs | 58.65 µs | 11.5× slower |
| 1k | nested_wildcard | 233.64 µs | 36.27 µs | 6.4× slower |
| 1k | filter_cheap | 24.97 µs | 57.64 µs | 2.3× faster |
| 1k | filter_search | 1.67 ms | — | jsonpath-rfc9535 only (no Goessner regex) |
| 10k | child | 263.68 ns | 222.25 ns | 1.2× slower |
| 10k | author_wildcard | 912.81 µs | 193.22 µs | 4.7× slower |
| 10k | descendant_price | 7.22 ms | 719.70 µs | 10.0× slower |
| 10k | nested_wildcard | 3.13 ms | 513.39 µs | 6.1× slower |
| 10k | filter_cheap | 321.30 µs | 690.45 µs | 2.1× faster |
| 10k | filter_search | 18.62 ms | — | jsonpath-rfc9535 only (no Goessner regex) |

## End-to-end (parse + evaluate per call) — all three engines

| size | case | jsonpath-rfc9535 | jsonpath_lib | jsonpath-rust |
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

## jsonpath-rfc9535 scaling (benches/queries.rs) — evaluation only

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

jsonpath-rfc9535 is **4–11× slower than jsonpath_lib** on wildcard/descendant queries, ≈1.2× on a single-node lookup, and **~2× faster** on the comparison filter. The gap scales with the number of nodes *visited*.

**Root cause (`src/eval.rs`):** every traversal step allocates a `NormalizedPath` link (`Rc::new(Link)` via `child_name`/`child_index`) for each node visited — `collect_descendants` does this for the whole tree on `$..`, `apply_wildcard` for every child — and `query_values()` builds the full path-bearing `NodeList` then discards the paths. jsonpath_lib collects only `&Value`, with no per-node allocation. `search()` is ~on par with jsonpath-rust (regex dominates).

---

## Update 2026-06-26 — singular fast path + rsonpath reference

The "Finding" above is the **original cold pre-optimisation snapshot**. Since then the path-free value path, streaming traversal, inlining, `$..name` specialisation, and buffer presizing landed (commits up to `ffab473`), making jsonpath-rfc9535 at least as fast as jsonpath_lib on every case. This update adds the singular fast path and a third comparison engine.

### Singular fast path

A query whose every segment is a single child name/index step (`$.a.b[0].c`) selects at most one node, so it needs no worklist and no per-segment `Vec`. `JsonPath` precomputes this form (`compiled::Query::singular`) and threads a single `&Value` down the document — eliminating ~5 small heap allocations per call. The general worklist path is left byte-for-byte unchanged (dispatch lives in `JsonPath::query`/`query_values`), so non-singular queries are algorithmically untouched.

Headline effect (eval-only, jsonpath-rfc9535 vs jsonpath_lib, Apple M4 Pro, warm):

| size | case | jsonpath-rfc9535 before | jsonpath-rfc9535 after | jsonpath_lib | after vs jsonpath_lib |
|---|---|---|---|---|---|
| 10k | child | 75 ns | **37 ns** (−51%) | 228 ns | **6.1× faster** (was ~3×) |

`query/child/*` in `benches/queries.rs` shows the same −52% at every document size (the lookup is size-independent). Other shapes (`wildcard`, `descendant`, filters) move only within the ±5–10% layout/thermal noise floor at these scales — verified by opposite-sign deltas for the same query across harnesses/runs (noise, not a regression).

### rsonpath in the comparison bench (`scan/` group)

`rsonpath` (0.10) is a raw-bytes + SIMD engine: it never builds a `serde_json::Value`. It can't share the `eval/` group (which times a *pre-parsed* DOM), so the new `scan/*` group times the full **text → matches** pipeline — `serde_json::from_str` + query for the DOM engines, a byte `count` for rsonpath. It covers only the non-filter cases (rsonpath has no filter support) and asserts node-count equivalence before benching.

| scan/10k (text → matches) | jsonpath-rfc9535 (parse+query) | jsonpath_lib | rsonpath (count) |
|---|---|---|---|
| child | 10.8 ms | 10.9 ms | **435 µs** |
| author_wildcard | 10.9 ms | 11.4 ms | **910 µs** |

The DOM engines spend ~10.7 ms building a `Value` from the 2 MB document; the query itself is microseconds. rsonpath is ~12–25× faster here **only because it skips DOM construction** — the cost a `Value`-based engine pays up front and a byte engine never does. When you already hold a `Value` (parse once, query many — the `eval/` group), jsonpath-rfc9535 is faster; when you have raw text and scan once, a byte engine wins. The transferable byte-engine techniques (query→automaton, subtree pruning, allocation discipline) are already applied in `eval.rs`; SIMD structural classification does not apply to a materialised DOM.

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
the selected values). Made before 1.0.

### Pruned descendant recursion

Descendant traversal (`descend`/`descend_name`) recursed into *every* member/element,
building a path step (`Rc<Link>` for the paths API) to do so — but a scalar has no
descendants, so recursing into it can never select anything. Guarding the recursion on
`is_container` (object/array) skips that dead-end work: no path step is built to descend
into a leaf. This captures more than a full trail-based deferral would have:

| micro @10k | before | after | change |
|---|---|---|---|
| `$..price` paths | ~3.16 ms | ~1.4 ms | **−56%** |
| `$..*` paths | ~6.23 ms | ~4.2 ms | **−33%** |
| `$..*` values | ~790 µs | ~535 µs | **−32%** |
| `$..price` values | ~665 µs | ~635 µs | −4% |

It also benefits the value path — skipping the per-leaf `descend` call (`apply_selector`
+ match) that always selected nothing, which a deferred-materialisation rewrite would
not. CTS 703/703 (values + normalized paths) still passes. Child/wildcard queries don't
recurse, so they're unaffected.

### Pre-compiled regex patterns (`match` / `search`)

`match()`/`search()` recompiled the regex on **every element**: `regex_test` called
`iregexp::build()` (translate + `Regex::new`) per filter evaluation. Regex *compilation*
dwarfs matching, so a filter over an N-element array paid N compilations — impractically
slow over large arrays.

A literal pattern (`match(@.x, "constant")`) is now translated and compiled **once** at
`JsonPath::parse` time, with the call's anchoring baked in, and stored in the IR
(`compiled::Pattern::Literal(Option<regex::Regex>)`); evaluation only matches. A pattern
computed from the document (`match(@.x, @.y)`) can't be precompiled and stays
`Pattern::Dynamic` (compiled per call) — uncommon, and a ReDoS risk regardless.

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

### Remaining optimisation (not pursued)

A full trail-based deferral (maintain a cheap step-stack during descent, materialise the
`NormalizedPath` only at a selected node) would additionally avoid the `Rc<Link>` for
*container* subtrees that contain no match (e.g. a `reviews` array under `$..price`). With
the `is_container` prune already taking −56% off `$..price` paths, the remaining headroom
is ~10–20% on that one query shape, at the cost of a new `Trail` trait + associated type
and loss of prefix-sharing across matches; not pursued.


## Update 2026-07-04 — the `scan` feature: hybrid byte-scanning over raw text

New `extract/` rows for `ScanQuery` (feature `scan`): rsonpath byte-scans the query's
structural prefix; filters run per extracted fragment, or — when the predicate only
consults singular `@`-rooted member paths — are *pushed down* into auxiliary scans so
only passing candidates are ever parsed. Full criterion sampling (not `--quick`),
Apple M4 Pro, rustc 1.92.0, rsonpath-lib 0.10.1 (scalar path on aarch64 — SIMD
acceleration is x86-only, so these ratios understate x86 results).

Context for the ratios: in `extract/` (text → usable values) the DOM engines were
tied (~1.0×) — a whole-document `serde_json` parse dominates both identically. The
hybrid's entire margin is new capability, not an increment: it never builds the DOM.
`eval/` (query a `Value` you already hold) is untouched by the feature and remains the
headline metric for compile-once/query-many use; an A/B against a pre-scan baseline
showed no change on any existing row.

### 1k

| case | parse+query | jsonpath_lib | raw rsonpath | hybrid (adaptive) | hybrid vs parse+query |
|---|---|---|---|---|---|
| child | 1.04 ms | 1.05 ms | 45.5 µs | 46.0 µs | **22.7× faster** |
| author_wildcard | 1.07 ms | 1.07 ms | 175.5 µs | 177.8 µs | **6.0× faster** |
| descendant_price | 1.10 ms | 1.12 ms | 125.1 µs | 119.0 µs | **9.2× faster** |
| nested_wildcard | 1.09 ms | 1.10 ms | 218.0 µs | 221.2 µs | **4.9× faster** |
| filter_cheap | 1.08 ms | 1.12 ms | — | 629.5 µs | **1.7× faster** |
| filter_selective | 1.06 ms | 1.07 ms | — | 45.3 µs | **23.4× faster** |
| filter_search | 1.09 ms | — | — | 536.4 µs | **2.0× faster** |

### 100k

| case | parse+query | jsonpath_lib | raw rsonpath | hybrid (adaptive) | hybrid vs parse+query |
|---|---|---|---|---|---|
| child | 114.84 ms | 115.39 ms | 4.56 ms | 4.62 ms | **24.9× faster** |
| author_wildcard | 120.37 ms | 119.58 ms | 18.27 ms | 18.55 ms | **6.5× faster** |
| descendant_price | 124.37 ms | 124.89 ms | 13.15 ms | 12.72 ms | **9.8× faster** |
| nested_wildcard | 126.77 ms | 127.87 ms | 24.32 ms | 24.29 ms | **5.2× faster** |
| filter_cheap | 120.04 ms | 126.36 ms | — | 62.87 ms | **1.9× faster** |
| filter_selective | 112.74 ms | 112.67 ms | — | 4.50 ms | **25.0× faster** |
| filter_search | 125.24 ms | — | — | 55.32 ms | **2.3× faster** |

Reading the table:

* **Structural queries ride the byte engine at cost parity**: the hybrid matches raw
  rsonpath within ~1-2% everywhere both can run — the residual machinery is free.
* **Filter queries are the new capability** (raw rsonpath cannot express them at all).
  With a selective prefix (`filter_selective`) the hybrid behaves like a structural
  query: ~25×. With a whole-document candidate set (`filter_cheap`, `filter_search`),
  pushdown decides the predicate from leaf scans and parses only passing candidates:
  ~2× where the pre-pushdown fragment scan *lost* to DOM by ~10%.
* **The forced-scan rows (not shown) equal adaptive within noise** on every case:
  adaptive's routing (whole-document prefixes → DOM, overlap budget, pushdown finish
  selection) costs nothing on flat documents; its value is bounding the tails.

Remaining known losses: none in `extract/`. The pathological shapes (self-nested
documents under descendant prefixes, duplicate member names) degrade or diverge as
documented on `ScanQuery::query_values`.

## Update 2026-07-07 — small-object linear-scan member lookups; serde_json_path rows

### The bench build measures the `preserve_order` backend

Sample-profiling `query/wildcard/25k` showed **~58% of self time in SipHash**
(`BuildHasher::hash_one`): the `jsonpath_lib` dev-dependency enables
`serde_json/preserve_order`, so every bench and test in this repo builds `serde_json`
with the IndexMap backend, where each `Map::get`/`get_key_value` hashes the name.
Every number in this file was collected on that backend. Feature unification applies
to all engines in the comparison bench equally, so *cross-engine ratios* are fair —
but users of jsonpath-rfc9535 without `preserve_order` get the BTreeMap backend, which
this repo's harnesses never measured until now (see the out-of-tree check below).

### Linear scan for small objects (`get_member`)

A query re-looks-up the *same literal name* once per node visited, so the lookup is
the hot instruction stream of every name-bearing shape. `eval.rs` now scans objects of
≤ 16 members linearly (a handful of short-key memcmps — cheaper than one SipHash or a
BTree node walk on either backend); larger objects keep the map's own lookup. Applied
at every name site: `apply_name`, `descend_name`, both singular fast paths, and filter
sub-queries.

A/B vs a same-session baseline, `query/*/25k` (preserve_order build, quiet machine):

| query @25k | before | after | change |
|---|---|---|---|
| child | 33.5 ns | 20.2 ns | **−40%** |
| wildcard | 461 µs | 205 µs | **−56%** |
| descendant | 2.17 ms | 1.77 ms | **−21%** |
| filter_comparison | 921 µs | 701 µs | **−23%** |
| filter_exists | 482 µs | 332 µs | **−33%** |
| filter_string_eq | 948 µs | 482 µs | **−49%** |
| filter_function | 1.98 ms | 1.12 ms | **−43%** |
| filter_search | 1.38 ms | 1.11 ms | **−16%** |
| filter_match | 2.26 ms | 1.88 ms | **−17%** |
| filter_regex_dynamic | 51.5 ms | 44.6 ms | **−13%** |
| descendant_wildcard | 2.12 ms | 2.03 ms | −4% (no name lookups in `$..*`) |

The full-suite run vs the committed `main` baseline confirms the effect at every
document size (child −40% flat across small→100k; wildcard −50…−58%; paths API too:
`micro/path_overhead/wildcard/paths` −24%, `micro/singular/depth4` −39%).

Measurement notes, so future A/Bs don't chase ghosts: (1) `parse/*` moved ±10% in
these runs — the parser never executes `get_member`; ns-scale parse benches shift
with binary layout whenever `eval.rs` changes size. (2) The 50k rows have a
demonstrated ±30% run-to-run drift band on this machine (same code, same baseline,
20 minutes apart: `filter_comparison/50k` "+32%"), so they cannot resolve effects
of the size measured here; neighbouring sizes (25k, 100k) reproduce the improvement.
(3) A background video call (krisp/zoom) inflated one measurement leg by up to +85%
— check `ps`/load before trusting a surprising number.

**Default (BTreeMap) backend**: an out-of-tree harness (path-dep on this crate only,
so `preserve_order` stays off) shows the change is neutral-to-positive there as well
(descendant −8%, filter_comparison −16%, filter_exists −20%, nested_wildcard −18%,
child/wildcard within noise). CTS 703/703 and the full feature-matrix test suite pass.

### serde_json_path and jsonpath-rust added to the `eval/` comparison

Competitive context (crates.io, 2026-07-07): `jsonpath-rust` is by far the
most-downloaded JSONPath crate (76.4M all-time, 12.7M last-90d, +20% half-over-half,
69 dependents, maintained); `serde_json_path` is far smaller (1.7M all-time,
569k/90d, 52 dependents, last release 2025-02) but the fastest-growing (+49%
half-over-half) and carries the strict-RFC-compliance reputation; `jsonpath_lib` is
unmaintained since 2021 and declining (−10% half-over-half) but remains the
historical DOM speed baseline. Both RFC crates now run in `eval/` compile-once rows
(`jsonpath-rust` via `parse_json_path` + `js_path_process`) alongside `e2e/`.

Standing vs `serde_json_path` at 25k eval-only after this change (same run, quiet
machine): child **20.9 ns vs 200 ns** (9.6×), author_wildcard **208 µs vs 1.38 ms**
(6.6×), descendant_price **1.96 ms vs 4.76 ms** (2.4×), nested_wildcard **1.66 ms vs
6.17 ms** (3.7×), filter_cheap **729 µs vs 876 µs** (1.2×), filter_selective **115 ns
vs 326 ns** (2.8×), filter_search **2.11 ms vs 212 ms** (100×; serde_json_path
recompiles the regex per element). jsonpath_lib is behind on every case as well
(author_wildcard 2.6×, filter_cheap 2.5×, child 10.7×).

### Segment fusion (tried, rejected)

A depth-first rewrite of `walk` — each selected node threaded immediately through the
remaining segments, no per-segment frontier vector — was implemented behind a generic
emitter (final segment = direct push, identical to the status quo; earlier segments =
recurse). Stash-flip A/B on a quiet machine (load < 1.5, both legs): **wildcard
values +26% (25k) / +45% (10k micro)**, filter_function +8.6%; single-segment shapes
neutral; no reproducible paths-mode win. Cause: after the linear-scan lookup landed,
a values-mode frontier entry is one 8-byte sequential write (`(NoPath, &Value)` — the
ZST vanishes), while the fused continuation pays a `walk_from` call + segment match +
selector-slice loop **per node** (~5–10 ns) on every high-fanout expansion. Rejected;
the frontier walk stays.

### Singular-vs-literal comparison fast path

The dominant filter shape — a singular `@`-rooted query against a literal
(`?@.price < 10`, `?@.category == 'fiction'`) — is now split out at lowering time
(`LogicalExpr::SingularLiteral`; a `literal op query` source stores the mirrored
operator). Evaluation is one member lookup plus an in-place `value_eq`/`value_less`,
with no per-element `Comparand`/`Cow` construction or drop glue — machinery the
filter profile put at ~28% of self time. Scan pushdown recognises the new node, so
pushable predicates stay pushable (the scan differential suite covers it).

A/B vs the clean post-lookup baseline @25k (quiet machine): **filter_string_eq
−35%** (482 → 295 µs; −69% cumulative from the session start), **filter_comparison
−7.7%** (587 µs; numeric compares are bounded by the lookup + `Number` unwrapping,
not the wrappers); control rows within the ±7% layout-jitter band. Numeric-literal pre-decoding (skip the `as_i64/as_f64` chains
per element) is the remaining known headroom on this shape.

### Plain patterns: equality for `match()`, per-call shortcut for dynamic — and a
### falsified assumption about `str::contains`

A pattern containing no I-Regexp metacharacter (`( ) * + . ? [ \ ] { | }` — plus
`^`/`$`, which the CTS pins to their PCRE anchor semantics) has no regex behaviour:
`match()` on it is string equality, `search()` is substring containment; such a
pattern is also always a *valid* I-Regexp (the excluded set is exactly the complement
of RFC 9485's `NormalChar`), so the invalid-pattern-yields-false rule can't apply.
`iregexp::is_plain` detects this with one ASCII byte-scan.

Where it pays (A/B vs the fresh `main` baseline @25k):

* **`match()` with a plain literal → `Pattern::Plain`, evaluated as `==`**: the new
  `filter_match_plain` row (`?match(@.category, 'fiction')`) runs at **~500 µs vs
  ~2 ms** for the equivalent compiled-regex evaluation (~4×). Nothing beats a length
  check and a memcmp.
* **Dynamic patterns get the same check per call, for both anchorings**:
  `filter_regex_dynamic` (`?search(@.title, @.category)` — computed patterns that
  happen to be metacharacter-free) collapsed **−96%** (44.5 ms → 1.67 ms): the
  alternative there is a full regex *compile* per element, and the per-call
  containment check is a one-shot `memmem::find` (SIMD; −36% vs the `str::contains`
  first cut).
* **`search()` with a plain literal → `Pattern::Substring`**, a
  `memchr::memmem::Finder` built once at compile time. Two A/B lessons landed here:
  `str::contains` was *not* an acceptable shortcut (+117% — std's scalar two-way
  search loses badly to SIMD `memmem`), and the finder itself is **neutral (−2%)**
  versus the compiled regex, because the regex crate's literal strategy is already a
  nearly-direct `memmem` call. Kept for the smaller machinery on the path (no regex
  `Pool`/lazy-DFA involvement) at zero cost. `filter_match`'s real regex keeps that
  path covered as the control row. `memchr` was already a transitive dependency of
  `regex`; declaring it adds no crates.

Measurement note: during verification the machine went through a ~45-minute episode
where regex-DFA-heavy rows (only) ran up to 2× slower system-wide — reproduced on
stash-flipped, baseline-identical source, cold-start, quiet load, no thermal flag,
normal profile shape, then decayed on its own. If regex rows ever read +40…+110%
against every code state at once, wait it out before believing anything.

## Update 2026-07-08 — `rayon` feature: parallel value-path evaluation

The value path (`query_values`) now parallelises over the rayon global pool behind an
opt-in `rayon` feature (named per ecosystem convention — indexmap, hashbrown). Three
dispatch points, all gated on ≥2048 elements: large frontiers (chunked through the
ordinary serial applicators), filters over large arrays (predicate per chunk), and
descendant recursion at large-array fan-outs (each subtree walked serially). Results
keep exact document order via chunk-ordered concatenation — pinned by the CTS
harness's `query_values`-vs-`query` cross-check on every case. The paths API and
filter sub-queries stay serial (`NormalizedPath` shares `Rc` links, not `Send`);
small documents never touch the pool (`small` fixture rows: ±6%).

A/B vs the serial `main` baseline (13-worker M4 Pro, quiet):

| query | 25k | 100k | 100k absolute |
|---|---|---|---|
| descendant | −76% | −82% | 8.45 ms → 1.55 ms |
| filter_comparison | −75% | −84% | 5.25 ms → 844 µs |
| filter_search | −76% | −86% | 8.92 ms → 1.23 ms |
| filter_regex_dynamic | −85% | −89% | 14.4 ms → 1.61 ms |
| descendant_wildcard | −69% | −73% | 11.7 ms → 3.15 ms |
| wildcard | −38% | −74% | 1.71 ms → 446 µs |

That is 4–9× wall-clock at ~10× CPU — the right trade for latency, the wrong one for
saturated multi-tenant servers, hence opt-in. `just bench`/`bench-save-baseline` now
measure `--features rayon` (the configuration of record); the cross-library
`compare` bench deliberately stays serial so engine-vs-engine ratios remain
single-core apples-to-apples, with this section carrying the parallel story.

### Bench-profile LTO (tried, rejected) and the suite trim

`[profile.bench] lto = "thin", codegen-units = 1` was A/B'd on a 6-row subset with a
dead-function perturbation probe on each side: **no jitter damping** (mean |swing|
1.9% without vs 2.5% with — and the probe itself barely moved anything, so the
historical ±7–16% swings require real-code-sized inlining changes or machine state,
not mere binary perturbation), **hot query rows +4…+12% slower** (global inlining
fights the hand-placed `#[inline]` structure), parse rows −23% (real, but parse is
nobody's bottleneck), and builds 1 s → 19 s. Rejected.

The suite itself was trimmed the same day to what the optimisation sessions actually
consulted: `queries` runs small/25k/100k (10k/50k dropped — 50k sits on a cache
cliff with a ±30% noise floor; regenerate via `generate.py` for occasional
staircase sweeps), `micro/` moved to the 25k fixture, and the comparison bench keeps
`eval/` (25k/100k) and `extract/` (1k/25k/100k) while dropping `e2e/` (arithmetic:
parse + eval) and the count-only `scan/` group (superseded by `extract/`). A full
`bench-save-baseline` drops from ~2 h to ~40 min with no loss of decision power.

### Frontier batching (tried, rejected)

The follow-up hypothesis — make the frontier cheaper still via bulk insertion and
buffer reuse — also failed its A/B (quiet machine, vs the same clean baseline).
Replacing `apply_wildcard`'s reserve+push loop with a `TrustedLen` `Vec::extend`
cost **+43.7% on wildcard/25k** (+36% at 10k micro; `$..*` values +16%): the
hand-written loop was already compiling to better code than the iterator-adapter
chain, and per-call `extend` setup is pure overhead on the fixture's many 0–3
element arrays. Double-buffering `walk`'s frontier (two ping-ponged buffers instead
of one `with_capacity` per segment) bisected to a further +1…+9%. Both reverted.
Conclusion: after the linear-scan lookup, the segment-at-a-time frontier walk with
reserve+push sinks is a measured local optimum — remaining eval headroom is in
*what runs per node* (filter machinery, regex dispatch), not in how selected nodes
are buffered.

Standing vs `jsonpath-rust` (compile-once via `parse_json_path` + `js_path_process`)
at 25k: child **20.9 ns vs 322 ns** (15×), author_wildcard **208 µs vs 5.21 ms**
(25×), descendant_price **1.96 ms vs 49.6 ms** (25×), nested_wildcard **1.66 ms vs
14.5 ms** (8.7×), filter_cheap **729 µs vs 3.83 ms** (5.3×), filter_selective
**115 ns vs 668 ns** (5.8×), filter_search **2.11 ms vs 48.3 ms** (23×). Note its
`js_path_process` builds a path per selected node, so these rows carry path overhead
our `query_values` row does not — but `micro/path_overhead` puts our paths API at
2–3× the values API on these shapes, which still leaves every row several times
ahead of jsonpath-rust.
