//! Differential fuzzing of the scan pipelines against DOM evaluation.
//!
//! Two properties, matching the crate's documented contracts:
//!
//! 1. **Termination without panics** on arbitrary bytes, in every [`ScanMode`] — the
//!    property the crate asserts but cannot locally prove at the rsonpath boundary.
//! 2. **Scan/DOM agreement** whenever the input is valid JSON whose serialization
//!    round-trips byte-identically. The round-trip gate excludes the documented
//!    divergences (duplicate member names, escape-written names) without having to
//!    detect them syntactically; descendant queries compare as multisets because
//!    RFC 9535 leaves their ordering unspecified.
//!
//! Run via `just fuzz` (requires nightly + cargo-fuzz), e.g.:
//! `just fuzz -- -max_total_time=300`.
#![no_main]

use jsonpath_rfc9535::{JsonPath, ScanMode, ScanQuery};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use std::sync::LazyLock;

// The plan-shape query pool, shared verbatim with `tests/scan_properties.rs`.
include!("../../tests/common/plan_shape_queries.rs");

/// Compiled once per process: (scan query, DOM oracle, has descendant segment).
static COMPILED: LazyLock<Vec<(ScanQuery, JsonPath, bool)>> = LazyLock::new(|| {
    QUERIES
        .iter()
        .map(|query| {
            (
                ScanQuery::parse(query).expect("fuzz query compiles"),
                JsonPath::parse(query).expect("fuzz query compiles"),
                query.contains(".."),
            )
        })
        .collect()
});

/// Installs the panic-hook override exactly once.
static PANIC_HOOK: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    // libfuzzer-sys installs an abort-before-unwind panic hook, which would turn the
    // engine panics the library deliberately absorbs (surfaced as `ScanError::Engine`;
    // see `absorb_engine_panic`) into crashes before `catch_unwind` can run. Replace
    // it: panics originating inside rsonpath are the absorbed class and must not
    // abort; everything else — this target's own divergence asserts included — keeps
    // abort-on-panic fuzzing semantics via the previous hook. An rsonpath panic that
    // ESCAPES the library boundary still aborts (the fuzz harness aborts on any
    // unwind out of the target), so unwrapped engine calls remain findings.
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let absorbed_by_library = info
                .location()
                .is_some_and(|location| location.file().contains("rsonpath"));
            if !absorbed_by_library {
                previous(info);
            }
        }));
    });
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let canonical = parsed
        .as_ref()
        .is_some_and(|value| value.to_string() == text);
    for (scan, dom, descendant) in COMPILED.iter() {
        for mode in [
            ScanMode::AlwaysScan,
            ScanMode::Adaptive,
            ScanMode::NeverScan,
        ] {
            let outcome = scan.clone().with_mode(mode).query_values(text);
            let (true, Some(document)) = (canonical, parsed.as_ref()) else {
                // Malformed / non-canonical input: reaching the next iteration
                // without a panic is the whole property.
                continue;
            };
            let got = outcome.unwrap_or_else(|error| {
                panic!("{mode:?} errored on canonical valid JSON: {error}")
            });
            let want: Vec<Value> = dom.query_values(document).into_iter().cloned().collect();
            let matched = got == want || (*descendant && multiset(&got) == multiset(&want));
            assert!(
                matched,
                "scan/DOM divergence in {mode:?}: {} vs {} nodes",
                got.len(),
                want.len()
            );
        }
    }
});

/// Order-insensitive rendering, for descendant queries whose ordering is unspecified.
fn multiset(values: &[Value]) -> Vec<String> {
    let mut rendered: Vec<String> = values.iter().map(ToString::to_string).collect();
    rendered.sort();
    rendered
}
