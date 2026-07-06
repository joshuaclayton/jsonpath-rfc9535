//! Cross-library performance comparison (feature `compare`).
//!
//! Runs jsonpath-rfc9535 against three other engines on **equivalent queries**:
//! `jsonpath_lib` (Goessner dialect — fast, the DOM baseline we want to match or beat),
//! `jsonpath-rust` (also RFC 9535, DOM-based), and `rsonpath` (RFC 9535, but a
//! fundamentally different *raw-bytes + SIMD* engine — see the `scan/` note below). Run
//! with:
//!
//! ```text
//! cargo bench --features compare --bench comparison
//! ```
//!
//! Four groups, measuring four different things:
//!
//! * **`eval/*` (headline)** — jsonpath-rfc9535 vs `jsonpath_lib`, each query *pre-compiled once*
//!   over a *pre-parsed* [`Value`], only evaluation timed. This is the realistic
//!   compile-once/query-many usage and the metric the project optimises for. Goal:
//!   jsonpath-rfc9535 ≤ `jsonpath_lib` on every case.
//! * **`e2e/*`** — parse-query-string + evaluate per call across the three DOM engines,
//!   over a pre-parsed document.
//! * **`scan/*`** — the full **text → matches** pipeline, the only fair place for
//!   `rsonpath`. The DOM engines pay `serde_json::from_str` (building a `Value`) plus the
//!   query; `rsonpath` scans the raw bytes with SIMD and never builds a DOM. This is not
//!   apples-to-apples and is not meant to be: it quantifies the *DOM-construction tax* a
//!   `Value`-based engine pays and a byte engine skips. `rsonpath` also returns a count /
//!   byte spans, not borrowed `&Value`s, and supports no filter expressions — so `scan/`
//!   covers only the non-filter cases and uses `rsonpath`'s cheapest `count` mode.
//! * **`extract/*`** — the same **text → results** pipeline, but every engine ends at
//!   *usable, materialised values*. The DOM engines parse and return `Vec<&Value>`;
//!   `rsonpath` scans the bytes, copies out each matched node's JSON text, and we parse
//!   every fragment back into an owned `Value`. Where `scan/` lets `rsonpath` stop at a
//!   bare count, this makes both paradigms pay to produce results you can actually read —
//!   the fairest cross-paradigm unit of work. `rsonpath` still skips the whole-document
//!   DOM, so it stays ahead, just by a smaller and more honest margin.
//!
//! Each case asserts that the engines select the *same number of nodes* before benching,
//! so a non-equivalent query fails loudly rather than comparing apples to oranges.

use criterion::{Criterion, criterion_group, criterion_main};
use jsonpath_rfc9535::{JsonPath as JpFull, ScanMode, ScanQuery};
use jsonpath_rust::JsonPath as _;
use rsonpath::engine::{Compiler, Engine, RsonpathEngine};
use rsonpath::input::BorrowedBytes;
use rsonpath::result::Match;
use serde_json::Value;
use std::hint::black_box;
use std::path::Path;

/// Fixture sizes (book counts) loaded from `benches/data/` at runtime. Generate them
/// with `benches/data/generate.py`; any missing size is skipped.
const FIXTURES: &[(&str, &str)] = &[
    ("1k", "bookstore-1k.json"),
    ("10k", "bookstore-10k.json"),
    ("25k", "bookstore-25k.json"),
    ("50k", "bookstore-50k.json"),
    ("100k", "bookstore-100k.json"),
];

fn read(file: &str) -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/data")
        .join(file);
    std::fs::read_to_string(path).ok()
}

fn load(file: &str) -> Option<Value> {
    read(file).and_then(|text| serde_json::from_str(&text).ok())
}

/// One comparison case: the same selection expressed in each dialect. `rfc` drives
/// jsonpath-rfc9535 and jsonpath-rust; `goessner` drives `jsonpath_lib` (its filters need the
/// `?(...)` parentheses); `rsonpath` drives the byte engine (standard JSONPath syntax,
/// `None` when the engine cannot express the query — it has no filter support). A field
/// is `None` whenever that engine cannot express the selection.
struct Case {
    label: &'static str,
    rfc: &'static str,
    goessner: Option<&'static str>,
    rsonpath: Option<&'static str>,
}

/// Only unambiguously-equivalent selections (no `$..*`, whose recursive-wildcard node
/// count differs between dialects).
const CASES: &[Case] = &[
    Case {
        label: "child",
        rfc: "$.store.book[0].title",
        goessner: Some("$.store.book[0].title"),
        rsonpath: Some("$.store.book[0].title"),
    },
    Case {
        label: "author_wildcard",
        rfc: "$.store.book[*].author",
        goessner: Some("$.store.book[*].author"),
        rsonpath: Some("$.store.book[*].author"),
    },
    Case {
        label: "descendant_price",
        rfc: "$..price",
        goessner: Some("$..price"),
        rsonpath: Some("$..price"),
    },
    Case {
        label: "nested_wildcard",
        rfc: "$.store.book[*].reviews[*].rating",
        goessner: Some("$.store.book[*].reviews[*].rating"),
        rsonpath: Some("$.store.book[*].reviews[*].rating"),
    },
    Case {
        label: "filter_cheap",
        rfc: "$.store.book[?@.price < 10]",
        goessner: Some("$.store.book[?(@.price < 10)]"),
        rsonpath: None,
    },
    // A filter behind a *selective* structural prefix: `$.store.book[0].reviews[*]`
    // narrows to a handful of nodes before the predicate runs, where `filter_cheap`'s
    // `book[*]` spans essentially every byte of the document. The pair brackets the two
    // extremes of how much data the structural part of a filter query touches — a shape
    // no case covered before.
    Case {
        label: "filter_selective",
        rfc: "$.store.book[0].reviews[?@.rating >= 4]",
        goessner: Some("$.store.book[0].reviews[?(@.rating >= 4)]"),
        rsonpath: None,
    },
    // `search()` is an RFC 9535 function extension; jsonpath_lib has no regex, so this
    // case compares jsonpath-rfc9535 against jsonpath-rust only.
    Case {
        label: "filter_search",
        rfc: r#"$.store.book[?search(@.title, "Number 1")]"#,
        goessner: None,
        rsonpath: None,
    },
];

fn count_jsonpath_rfc9535(document: &Value, rfc: &str) -> usize {
    JpFull::parse(rfc).map_or(0, |query| query.query_values(document).len())
}

fn count_jsonpath_lib(document: &Value, goessner: &str) -> usize {
    jsonpath_lib::Compiled::compile(goessner)
        .ok()
        .and_then(|compiled| compiled.select(document).ok())
        .map_or(0, |nodes| nodes.len())
}

fn count_jsonpath_rust(document: &Value, rfc: &str) -> usize {
    document.query(rfc).map_or(0, |nodes| nodes.len())
}

fn rsonpath_engine(query: &str) -> Option<RsonpathEngine> {
    let parsed = rsonpath_syntax::parse(query).ok()?;
    RsonpathEngine::compile_query(&parsed).ok()
}

fn count_rsonpath(engine: &RsonpathEngine, text: &str) -> Option<u64> {
    engine.count(&BorrowedBytes::new(text.as_bytes())).ok()
}

/// Extract-and-use: `rsonpath` materialises each matched node's JSON text, then we parse
/// every fragment back into an owned `Value`. This is the fair counterpart to a DOM
/// engine's `Vec<&Value>` — both sides end at values you can actually read, rather than
/// letting `rsonpath` stop at a bare `count`. `None` if the scan or any fragment-parse fails.
fn extract_rsonpath(engine: &RsonpathEngine, text: &str) -> Option<Vec<Value>> {
    let mut sink: Vec<Match> = Vec::new();
    engine
        .matches(&BorrowedBytes::new(text.as_bytes()), &mut sink)
        .ok()?;
    sink.into_iter()
        .map(|m| serde_json::from_slice::<Value>(m.bytes()).ok())
        .collect()
}

/// Fails the bench run if the engines do not select the same number of nodes
/// (each engine only when it can express the query).
fn assert_equivalent(size: &str, document: &Value, case: &Case) {
    let jp = count_jsonpath_rfc9535(document, case.rfc);
    let jr = count_jsonpath_rust(document, case.rfc);
    assert_eq!(
        jp, jr,
        "jsonpath-rfc9535 vs jsonpath-rust disagree on `{}` over {size}: {jp} vs {jr} nodes",
        case.label
    );
    if let Some(goessner) = case.goessner {
        let gn = count_jsonpath_lib(document, goessner);
        assert_eq!(
            jp, gn,
            "jsonpath-rfc9535 vs jsonpath_lib disagree on `{}` over {size}: {jp} vs {gn} nodes",
            case.label
        );
    }
}

fn fixtures() -> Vec<(&'static str, Value)> {
    FIXTURES
        .iter()
        .filter_map(|&(label, file)| load(file).map(|value| (label, value)))
        .collect()
}

/// `(label, raw JSON text)` for the `scan/` pipeline (rsonpath needs the bytes).
fn fixture_texts() -> Vec<(&'static str, String)> {
    FIXTURES
        .iter()
        .filter_map(|&(label, file)| read(file).map(|text| (label, text)))
        .collect()
}

/// Pre-compiled evaluation only: jsonpath-rfc9535 vs `jsonpath_lib` (both compile once, query
/// many). This is the metric that matters for steady-state use.
fn bench_eval(c: &mut Criterion) {
    for (size, document) in fixtures() {
        for case in CASES {
            assert_equivalent(size, &document, case);
            let mut group = c.benchmark_group(format!("eval/{size}/{}", case.label));
            if let Ok(query) = JpFull::parse(case.rfc) {
                group.bench_function("jsonpath-rfc9535", |b| {
                    b.iter(|| query.query_values(black_box(&document)));
                });
            }
            if let Some(goessner) = case.goessner
                && let Ok(compiled) = jsonpath_lib::Compiled::compile(goessner)
            {
                group.bench_function("jsonpath_lib", |b| {
                    b.iter(|| compiled.select(black_box(&document)));
                });
            }
            group.finish();
        }
    }
}

/// End-to-end (parse + evaluate per call) across the three DOM engines.
fn bench_end_to_end(c: &mut Criterion) {
    for (size, document) in fixtures() {
        for case in CASES {
            let mut group = c.benchmark_group(format!("e2e/{size}/{}", case.label));
            group.bench_function("jsonpath-rfc9535", |b| {
                b.iter(|| {
                    jsonpath_rfc9535::query_values(black_box(case.rfc), black_box(&document))
                });
            });
            if let Some(goessner) = case.goessner {
                group.bench_function("jsonpath_lib", |b| {
                    b.iter(|| jsonpath_lib::select(black_box(&document), black_box(goessner)));
                });
            }
            group.bench_function("jsonpath-rust", |b| {
                b.iter(|| document.query(black_box(case.rfc)));
            });
            group.finish();
        }
    }
}

/// Full **text → matches** pipeline, including `rsonpath`. The DOM engines parse the
/// JSON text into a `Value` and then query it; `rsonpath` scans the raw bytes (no DOM,
/// SIMD-accelerated) and returns a match count. See the module note: this measures the
/// DOM-construction tax, not an equivalent unit of work — `rsonpath` is expected to win
/// because it never materialises a `Value`.
fn bench_scan(c: &mut Criterion) {
    for (size, text) in fixture_texts() {
        let Ok(document) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for case in CASES {
            let Some(query) = case.rsonpath else {
                continue;
            };
            let Some(engine) = rsonpath_engine(query) else {
                continue;
            };
            // Cross-check rsonpath against jsonpath-rfc9535 on the parsed document before benching.
            if let Some(count) = count_rsonpath(&engine, &text) {
                let jp = count_jsonpath_rfc9535(&document, case.rfc);
                assert_eq!(
                    u64::try_from(jp).unwrap_or(u64::MAX),
                    count,
                    "jsonpath-rfc9535 vs rsonpath disagree on `{}` over {size}: {jp} vs {count} nodes",
                    case.label
                );
            }

            let mut group = c.benchmark_group(format!("scan/{size}/{}", case.label));
            if let Ok(compiled) = JpFull::parse(case.rfc) {
                group.bench_function("jsonpath-rfc9535 (parse+query)", |b| {
                    b.iter(|| {
                        serde_json::from_str::<Value>(black_box(&text))
                            .map_or(0, |document| compiled.query_values(&document).len())
                    });
                });
            }
            if let Some(goessner) = case.goessner {
                group.bench_function("jsonpath_lib (parse+query)", |b| {
                    b.iter(|| {
                        serde_json::from_str::<Value>(black_box(&text)).map_or(0, |document| {
                            jsonpath_lib::select(&document, goessner).map_or(0, |n| n.len())
                        })
                    });
                });
            }
            group.bench_function("rsonpath (count)", |b| {
                b.iter(|| count_rsonpath(&engine, black_box(&text)));
            });
            group.finish();
        }
    }
}

/// **Extract-and-use** variant of `scan/`: every engine ends at usable, materialised
/// values, so both paradigms pay for producing results you can read. The DOM engines parse
/// the text and return `Vec<&Value>`; `rsonpath` scans the bytes, copies out each matched
/// node's JSON text, and we parse every fragment back into an owned `Value`. `rsonpath`
/// still skips building a whole-document DOM, so it is expected to stay ahead — just by a
/// smaller, more honest margin than `scan/`, which lets it stop at a bare count.
///
/// This group also runs jsonpath-rfc9535's own **hybrid scan** (`ScanQuery`, the `scan`
/// feature): rsonpath extracts the structural prefix, the residual (filters included)
/// runs per fragment. Unlike raw `rsonpath` it covers *every* case — the filter cases
/// raw `rsonpath` cannot express are exactly where the hybrid earns its keep.
fn bench_extract(c: &mut Criterion) {
    for (size, text) in fixture_texts() {
        let Ok(document) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for case in CASES {
            // Raw rsonpath only covers the cases it can express; the hybrid and DOM
            // rows run for every case.
            let raw_engine = case.rsonpath.and_then(rsonpath_engine);
            // Cross-check both byte pipelines against jsonpath-rfc9535 before benching.
            if let Some(engine) = &raw_engine
                && let Some(values) = extract_rsonpath(engine, &text)
            {
                let jp = count_jsonpath_rfc9535(&document, case.rfc);
                assert_eq!(
                    jp,
                    values.len(),
                    "jsonpath-rfc9535 vs rsonpath disagree on `{}` over {size}: {jp} vs {} nodes",
                    case.label,
                    values.len()
                );
            }
            // Two hybrid rows: the default adaptive mode (decides per document) and
            // forced scan (always the byte engine) — their gap is what the adaptive
            // heuristic buys or costs on each case.
            let hybrid = ScanQuery::parse(case.rfc)
                .ok()
                .map(|adaptive| (adaptive.clone(), adaptive.with_mode(ScanMode::AlwaysScan)));
            if let Some((adaptive, forced)) = &hybrid {
                assert!(
                    forced.uses_scan(),
                    "`{}` unexpectedly fell back to DOM mode — the hybrid rows would not \
                     measure byte scanning",
                    case.label
                );
                let jp = count_jsonpath_rfc9535(&document, case.rfc);
                for (mode, query) in [("adaptive", adaptive), ("scan", forced)] {
                    let extracted = query
                        .query_values(&text)
                        .map(|values| values.len())
                        .map_err(|error| error.to_string());
                    assert_eq!(
                        extracted,
                        Ok(jp),
                        "jsonpath-rfc9535 vs hybrid ({mode}) disagree on `{}` over {size}",
                        case.label
                    );
                }
            }

            let mut group = c.benchmark_group(format!("extract/{size}/{}", case.label));
            if let Ok(compiled) = JpFull::parse(case.rfc) {
                group.bench_function("jsonpath-rfc9535 (parse+query)", |b| {
                    b.iter(|| {
                        serde_json::from_str::<Value>(black_box(&text))
                            .map_or(0, |document| compiled.query_values(&document).len())
                    });
                });
            }
            if let Some(goessner) = case.goessner {
                group.bench_function("jsonpath_lib (parse+query)", |b| {
                    b.iter(|| {
                        serde_json::from_str::<Value>(black_box(&text)).map_or(0, |document| {
                            jsonpath_lib::select(&document, goessner).map_or(0, |n| n.len())
                        })
                    });
                });
            }
            if let Some(engine) = &raw_engine {
                group.bench_function("rsonpath (matches+parse)", |b| {
                    b.iter(|| extract_rsonpath(engine, black_box(&text)));
                });
            }
            if let Some((adaptive, forced)) = &hybrid {
                group.bench_function("jsonpath-rfc9535 (hybrid adaptive)", |b| {
                    b.iter(|| adaptive.query_values(black_box(&text)));
                });
                group.bench_function("jsonpath-rfc9535 (hybrid scan)", |b| {
                    b.iter(|| forced.query_values(black_box(&text)));
                });
            }
            group.finish();
        }
    }
}

criterion_group!(
    benches,
    bench_eval,
    bench_end_to_end,
    bench_scan,
    bench_extract
);
criterion_main!(benches);
