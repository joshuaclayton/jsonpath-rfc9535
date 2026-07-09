//! Dual-backend spot-check: times jsonpath-rfc9535 and serde_json_path over the 25k
//! fixture on whichever `serde_json` map backend this build resolved (see
//! `Cargo.toml` for why this crate exists). Run both backend configurations with
//! `just bench-backends`, or `just bench-backends-rayon` for the parallel value path.
//!
//! Methodology: min-of-batches wall time (each batch sized to ~20 ms), which is the
//! least-noise point estimate for a quick spot-check. This is a coarser instrument
//! than the criterion suite — trust it to ±10%, use criterion for fine-grained A/Bs.

use jsonpath_rfc9535::JsonPath;
use serde_json::Value;
use std::hint::black_box;
use std::time::{Duration, Instant};

const QUERIES: &[(&str, &str)] = &[
    ("child", "$.store.book[0].title"),
    ("wildcard", "$.store.book[*].author"),
    ("slice", "$.store.book[100:200:3].title"),
    ("descendant", "$..price"),
    ("filter_comparison", "$.store.book[?@.price < 10]"),
    ("filter_exists", "$.store.book[?@.isbn]"),
    ("filter_string_eq", "$.store.book[?@.category == 'fiction']"),
    ("filter_search", r#"$.store.book[?search(@.title, "Number 1")]"#),
    ("filter_match_plain", "$.store.book[?match(@.category, 'fiction')]"),
    ("nested_wildcard", "$.store.book[*].reviews[*].rating"),
    ("descendant_wildcard", "$..*"),
];

fn best_of_batches(mut run: impl FnMut() -> usize) -> f64 {
    let warm_until = Instant::now() + Duration::from_millis(200);
    let mut sink = 0usize;
    while Instant::now() < warm_until {
        sink = sink.wrapping_add(run());
    }
    let probe = Instant::now();
    sink = sink.wrapping_add(run());
    let one = probe.elapsed().as_nanos().max(1) as f64;
    let batch = ((20_000_000.0 / one).ceil() as usize).clamp(1, 5_000_000);
    let mut best = f64::INFINITY;
    let measure_until = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < measure_until {
        let start = Instant::now();
        for _ in 0..batch {
            sink = sink.wrapping_add(run());
        }
        best = best.min(start.elapsed().as_nanos() as f64 / batch as f64);
    }
    black_box(sink);
    best
}

fn fmt(ns: f64) -> String {
    if ns >= 1e6 {
        format!("{:.2} ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.1} µs", ns / 1e3)
    } else {
        format!("{ns:.0} ns")
    }
}

fn main() {
    let backend = if cfg!(feature = "preserve_order") {
        "preserve_order (IndexMap — what the in-repo criterion suite measures)"
    } else {
        "default (BTreeMap — what default-feature users get)"
    };
    let parallel = if cfg!(feature = "rayon") {
        "rayon"
    } else {
        "serial"
    };
    println!("backend: {backend}");
    println!("jsonpath-rfc9535 mode: {parallel}");
    println!();

    let manifest = env!("CARGO_MANIFEST_DIR");
    let text = std::fs::read_to_string(format!("{manifest}/../data/bookstore-25k.json"))
        .expect("run `just bench-fixtures` first: benches/data/bookstore-25k.json missing");
    let document: Value = serde_json::from_str(&text).expect("fixture parses");

    println!(
        "{:22} {:>12} {:>17} {:>8}",
        "query @25k", "us", "serde_json_path", "ratio"
    );
    for (label, query) in QUERIES {
        let ours = JsonPath::parse(query).expect("query compiles");
        let us = best_of_batches(|| ours.query_values(black_box(&document)).len());

        let theirs = serde_json_path::JsonPath::parse(query).ok();
        let (them_s, ratio) = match &theirs {
            Some(path) => {
                let them =
                    best_of_batches(|| path.query(black_box(&document)).all().len());
                (fmt(them), format!("{:.1}x", them / us))
            }
            None => ("unsupported".to_owned(), "—".to_owned()),
        };
        println!("{label:22} {:>12} {them_s:>17} {ratio:>8}", fmt(us));
    }
}
