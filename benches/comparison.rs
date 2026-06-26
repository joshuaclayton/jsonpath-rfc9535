//! Cross-library performance comparison (feature `compare`).
//!
//! Runs jp-full against three other engines on **equivalent queries**:
//! `jsonpath_lib` (Goessner dialect — fast, the DOM baseline we want to match or beat),
//! `jsonpath-rust` (also RFC 9535, DOM-based), and `rsonpath` (RFC 9535, but a
//! fundamentally different *raw-bytes + SIMD* engine — see the `scan/` note below). Run
//! with:
//!
//! ```text
//! cargo bench --features compare --bench comparison
//! ```
//!
//! Three groups, measuring three different things:
//!
//! * **`eval/*` (headline)** — jp-full vs `jsonpath_lib`, each query *pre-compiled once*
//!   over a *pre-parsed* [`Value`], only evaluation timed. This is the realistic
//!   compile-once/query-many usage and the metric the project optimises for. Goal:
//!   jp-full ≤ `jsonpath_lib` on every case.
//! * **`e2e/*`** — parse-query-string + evaluate per call across the three DOM engines,
//!   over a pre-parsed document.
//! * **`scan/*`** — the full **text → matches** pipeline, the only fair place for
//!   `rsonpath`. The DOM engines pay `serde_json::from_str` (building a `Value`) plus the
//!   query; `rsonpath` scans the raw bytes with SIMD and never builds a DOM. This is not
//!   apples-to-apples and is not meant to be: it quantifies the *DOM-construction tax* a
//!   `Value`-based engine pays and a byte engine skips. `rsonpath` also returns a count /
//!   byte spans, not borrowed `&Value`s, and supports no filter expressions — so `scan/`
//!   covers only the non-filter cases and uses `rsonpath`'s cheapest `count` mode.
//!
//! Each case asserts that the engines select the *same number of nodes* before benching,
//! so a non-equivalent query fails loudly rather than comparing apples to oranges.

use criterion::{Criterion, criterion_group, criterion_main};
use jp_full::JsonPath as JpFull;
use jsonpath_rust::JsonPath as _;
use rsonpath::engine::{Compiler, Engine, RsonpathEngine};
use rsonpath::input::BorrowedBytes;
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
/// jp-full and jsonpath-rust; `goessner` drives `jsonpath_lib` (its filters need the
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
    // `search()` is an RFC 9535 function extension; jsonpath_lib has no regex, so this
    // case compares jp-full against jsonpath-rust only.
    Case {
        label: "filter_search",
        rfc: r#"$.store.book[?search(@.title, "Number 1")]"#,
        goessner: None,
        rsonpath: None,
    },
];

fn count_jp_full(document: &Value, rfc: &str) -> usize {
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

/// Fails the bench run if the engines do not select the same number of nodes
/// (each engine only when it can express the query).
fn assert_equivalent(size: &str, document: &Value, case: &Case) {
    let jp = count_jp_full(document, case.rfc);
    let jr = count_jsonpath_rust(document, case.rfc);
    assert_eq!(
        jp, jr,
        "jp-full vs jsonpath-rust disagree on `{}` over {size}: {jp} vs {jr} nodes",
        case.label
    );
    if let Some(goessner) = case.goessner {
        let gn = count_jsonpath_lib(document, goessner);
        assert_eq!(
            jp, gn,
            "jp-full vs jsonpath_lib disagree on `{}` over {size}: {jp} vs {gn} nodes",
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

/// Pre-compiled evaluation only: jp-full vs `jsonpath_lib` (both compile once, query
/// many). This is the metric that matters for steady-state use.
fn bench_eval(c: &mut Criterion) {
    for (size, document) in fixtures() {
        for case in CASES {
            assert_equivalent(size, &document, case);
            let mut group = c.benchmark_group(format!("eval/{size}/{}", case.label));
            if let Ok(query) = JpFull::parse(case.rfc) {
                group.bench_function("jp-full", |b| {
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
            group.bench_function("jp-full", |b| {
                b.iter(|| jp_full::query_values(black_box(case.rfc), black_box(&document)));
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
            // Cross-check rsonpath against jp-full on the parsed document before benching.
            if let Some(count) = count_rsonpath(&engine, &text) {
                let jp = count_jp_full(&document, case.rfc);
                assert_eq!(
                    u64::try_from(jp).unwrap_or(u64::MAX),
                    count,
                    "jp-full vs rsonpath disagree on `{}` over {size}: {jp} vs {count} nodes",
                    case.label
                );
            }

            let mut group = c.benchmark_group(format!("scan/{size}/{}", case.label));
            if let Ok(compiled) = JpFull::parse(case.rfc) {
                group.bench_function("jp-full (parse+query)", |b| {
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

criterion_group!(benches, bench_eval, bench_end_to_end, bench_scan);
criterion_main!(benches);
