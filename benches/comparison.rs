//! Cross-library performance comparison (feature `compare`).
//!
//! Runs jp-full against two other engines on **equivalent queries over the same
//! documents**: `jsonpath_lib` (Goessner dialect — fast, the baseline we want to match
//! or beat) and `jsonpath-rust` (also RFC 9535). Run with:
//!
//! ```text
//! cargo bench --features compare --bench comparison
//! ```
//!
//! The headline metric is the **`eval/*` groups**: jp-full vs `jsonpath_lib`, each with
//! its query *pre-compiled once* and only evaluation timed — the realistic
//! compile-once/query-many usage. The goal is jp-full ≤ `jsonpath_lib` on every case.
//! The `e2e/*` groups time parse+evaluate per call across all three engines for context
//! (`jsonpath-rust` 1.x exposes no precompiled evaluator, so it appears only here).
//!
//! Each case is asserted to select the *same number of nodes* in all three engines
//! before benching, so a non-equivalent query fails loudly rather than comparing apples
//! to oranges.

use criterion::{Criterion, criterion_group, criterion_main};
use jp_full::JsonPath as JpFull;
use jsonpath_rust::JsonPath as _;
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

fn load(file: &str) -> Option<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/data")
        .join(file);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// One comparison case: the same selection expressed in each dialect. `rfc` drives
/// jp-full and jsonpath-rust; `goessner` drives `jsonpath_lib` (its filters need the
/// `?(...)` parentheses). `goessner` is `None` when `jsonpath_lib` cannot express the
/// query — notably `search()`, which the Goessner dialect has no regex equivalent for.
struct Case {
    label: &'static str,
    rfc: &'static str,
    goessner: Option<&'static str>,
}

/// Only unambiguously-equivalent selections (no `$..*`, whose recursive-wildcard node
/// count differs between dialects).
const CASES: &[Case] = &[
    Case {
        label: "child",
        rfc: "$.store.book[0].title",
        goessner: Some("$.store.book[0].title"),
    },
    Case {
        label: "author_wildcard",
        rfc: "$.store.book[*].author",
        goessner: Some("$.store.book[*].author"),
    },
    Case {
        label: "descendant_price",
        rfc: "$..price",
        goessner: Some("$..price"),
    },
    Case {
        label: "nested_wildcard",
        rfc: "$.store.book[*].reviews[*].rating",
        goessner: Some("$.store.book[*].reviews[*].rating"),
    },
    Case {
        label: "filter_cheap",
        rfc: "$.store.book[?@.price < 10]",
        goessner: Some("$.store.book[?(@.price < 10)]"),
    },
    // `search()` is an RFC 9535 function extension; jsonpath_lib has no regex, so this
    // case compares jp-full against jsonpath-rust only.
    Case {
        label: "filter_search",
        rfc: r#"$.store.book[?search(@.title, "Number 1")]"#,
        goessner: None,
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

/// Fails the bench run if the engines do not select the same number of nodes
/// (`jsonpath_lib` only when it can express the query).
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

/// End-to-end (parse + evaluate per call) across all three engines.
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

criterion_group!(benches, bench_eval, bench_end_to_end);
criterion_main!(benches);
