//! Criterion micro-benchmarks for the two hot paths: compiling a query
//! ([`JsonPath::parse`], which also runs the §2.4 well-typedness check) and evaluating
//! a compiled query against documents of increasing size. Run with `cargo bench`.
//!
//! The `query/*` groups sweep three document sizes — the tiny inline bookstore and the
//! 1k / 10k-book fixtures committed under `benches/data/` (see `generate.py`) — so the
//! reports show how evaluation scales, not just fixed per-call overhead.

use criterion::{Criterion, criterion_group, criterion_main};
use jp_full::JsonPath;
use serde_json::{Value, json};
use std::hint::black_box;

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

const BOOKSTORE_1K: &str = include_str!("data/bookstore-1k.json");
const BOOKSTORE_10K: &str = include_str!("data/bookstore-10k.json");

/// `(label, document)` at increasing scale; the two large entries are parsed from the
/// committed fixtures.
fn documents() -> Vec<(&'static str, Value)> {
    let mut documents = vec![("small", bookstore())];
    for (label, raw) in [("1k", BOOKSTORE_1K), ("10k", BOOKSTORE_10K)] {
        if let Ok(value) = serde_json::from_str::<Value>(raw) {
            documents.push((label, value));
        }
    }
    documents
}

/// Representative queries: shorthand child, wildcard, descendant search, filters
/// (comparison and function extension), and a full descendant-wildcard walk.
const QUERIES: &[(&str, &str)] = &[
    ("child", "$.store.book[0].title"),
    ("wildcard", "$.store.book[*].author"),
    ("descendant", "$..price"),
    ("filter_comparison", "$.store.book[?@.price < 10]"),
    (
        "filter_function",
        "$.store.book[?length(@.title) > 10].title",
    ),
    (
        "filter_search",
        r#"$.store.book[?search(@.title, "Number 1")]"#,
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
                b.iter(|| compiled.query(black_box(document)));
            });
        }
        group.finish();
    }
}

criterion_group!(benches, bench_parse, bench_query);
criterion_main!(benches);
