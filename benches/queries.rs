//! Criterion micro-benchmarks for the two hot paths: compiling a query
//! ([`JsonPath::parse`], which also runs the §2.4 well-typedness check) and evaluating
//! a compiled query against documents of increasing size. Run with `cargo bench`.
//!
//! The `query/*` groups sweep three document sizes — the tiny inline bookstore and the
//! 1k / 10k-book fixtures committed under `benches/data/` (see `generate.py`) — so the
//! reports show how evaluation scales, not just fixed per-call overhead.

use criterion::{Criterion, criterion_group, criterion_main};
use jsonpath_rfc9535::JsonPath;
use serde_json::{Value, json};
use std::hint::black_box;
use std::path::Path;

/// The RFC 9535 running-example document (the smallest scale point).
fn bookstore() -> Value {
    json!({
        "store": {
            "book": [
                {
                    "category": "reference",
                    "author": "Nigel Rees",
                    "title": "Sayings of the Century",
                    "price": 8.95
                },
                {
                    "category": "fiction",
                    "author": "Evelyn Waugh",
                    "title": "Sword of Honour",
                    "price": 12.99
                },
                {
                    "category": "fiction",
                    "author": "Herman Melville",
                    "title": "Moby Dick",
                    "isbn": "0-553-21311-3",
                    "price": 8.99
                },
                {
                    "category": "fiction",
                    "author": "J. R. R. Tolkien",
                    "title": "The Lord of the Rings",
                    "isbn": "0-395-19395-8",
                    "price": 22.99
                }
            ],
            "bicycle": { "color": "red", "price": 399 }
        }
    })
}

/// Fixture sizes loaded from `benches/data/` at runtime (generate with
/// `benches/data/generate.py`); any missing size is skipped.
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

/// `(label, document)` at increasing scale: the tiny inline bookstore plus whichever
/// generated fixtures are present.
fn documents() -> Vec<(&'static str, Value)> {
    let mut documents = vec![("small", bookstore())];
    documents.extend(
        FIXTURES
            .iter()
            .filter_map(|&(label, file)| load(file).map(|value| (label, value))),
    );
    documents
}

/// Representative queries: shorthand child, wildcard, descendant search, filters
/// (comparison and function extension), and a full descendant-wildcard walk.
const QUERIES: &[(&str, &str)] = &[
    ("child", "$.store.book[0].title"),
    ("wildcard", "$.store.book[*].author"),
    ("descendant", "$..price"),
    ("filter_comparison", "$.store.book[?@.price < 10]"),
    ("filter_exists", "$.store.book[?@.isbn]"),
    ("filter_string_eq", "$.store.book[?@.category == 'fiction']"),
    (
        "filter_function",
        "$.store.book[?length(@.title) > 10].title",
    ),
    (
        "filter_search",
        r#"$.store.book[?search(@.title, "Number 1")]"#,
    ),
    // Anchored regex (full match) with a literal pattern — exercises the other
    // anchoring path; like `filter_search`, the pattern is compilable once.
    (
        "filter_match",
        r#"$.store.book[?match(@.title, "Book Number [0-9]+")]"#,
    ),
    // Anchored pattern with no metacharacters — takes the plain string-equality fast
    // path (no regex engine); `filter_match` above keeps the compiled-regex path
    // covered.
    (
        "filter_match_plain",
        "$.store.book[?match(@.category, 'fiction')]",
    ),
    // Regex whose pattern is computed from the document (`@.category`), so it cannot be
    // pre-compiled. The computed values here are metacharacter-free, so this row
    // exercises the dynamic path's per-call plain shortcut; a metacharacter-bearing
    // computed pattern would still compile per evaluation.
    (
        "filter_regex_dynamic",
        "$.store.book[?search(@.title, @.category)]",
    ),
    ("descendant_wildcard", "$..*"),
];

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");
    for &(label, query) in QUERIES {
        group.bench_function(label, |b| {
            b.iter(|| JsonPath::parse(black_box(query)));
        });
    }
    group.finish();
}

fn bench_query(c: &mut Criterion) {
    let documents = documents();
    for &(label, query) in QUERIES {
        let Ok(compiled) = JsonPath::parse(query) else {
            continue;
        };
        let mut group = c.benchmark_group(format!("query/{label}"));
        for (size, document) in &documents {
            group.bench_function(*size, |b| {
                b.iter(|| compiled.query_values(black_box(document)));
            });
        }
        group.finish();
    }
}

/// Micro-benchmarks that isolate specific cost centres rather than whole-query timings,
/// so a change can be attributed to the thing it touched.
///
/// * `micro/path_overhead/*` runs the same query over the 10k document twice — once via
///   [`JsonPath::query_values`] (no paths) and once via [`JsonPath::query`] (which builds
///   a `NormalizedPath` per selected node). The delta is exactly the path-construction
///   cost — the per-node `Rc` allocation that the path-free value path avoids.
/// * `micro/singular/*` exercises the singular fast path (pure name/index chains, which
///   skip the worklist entirely) at increasing depth.
fn bench_micro(c: &mut Criterion) {
    let Some(document) = load("bookstore-10k.json") else {
        return;
    };

    for (label, query) in [
        ("wildcard", "$.store.book[*].author"),
        ("descendant", "$..price"),
        ("descendant_wildcard", "$..*"),
    ] {
        let Ok(compiled) = JsonPath::parse(query) else {
            continue;
        };
        let mut group = c.benchmark_group(format!("micro/path_overhead/{label}"));
        group.bench_function("values", |b| {
            b.iter(|| compiled.query_values(black_box(&document)));
        });
        group.bench_function("paths", |b| {
            b.iter(|| compiled.query(black_box(&document)));
        });
        group.finish();
    }

    let mut group = c.benchmark_group("micro/singular");
    for (label, query) in [
        ("depth1", "$.store"),
        ("depth2", "$.store.book"),
        ("depth4", "$.store.book[0].title"),
    ] {
        let Ok(compiled) = JsonPath::parse(query) else {
            continue;
        };
        group.bench_function(label, |b| {
            b.iter(|| compiled.query_values(black_box(&document)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse, bench_query, bench_micro);
criterion_main!(benches);
