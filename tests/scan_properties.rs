//! Property-based tests for the `scan` hybrid evaluator: across arbitrary documents,
//! every execution plan must agree with DOM evaluation.
//!
//! Documents are generated with member names drawn from the same small alphabet the
//! query pool uses (so queries actually select things), and string *values* drawn from
//! arbitrary Rust strings (quotes, backslashes, control characters — the byte engine
//! must extract them intact). The query pool covers every plan the splitter can
//! produce: full scans, plain fragment scans with and without predicates, pushdown
//! (both the direct-parse and leaf-scan finishes, depending on the generated
//! document), residual slices/indexes, and DOM fallbacks.
//!
//! Equivalence rule (same as `scan_equivalence.rs`): exact match, except queries with
//! a descendant segment may reorder where RFC 9535 leaves ordering unspecified —
//! those compare as multisets.
#![cfg(feature = "scan")]

mod common;

use common::{has_descendant_segment, multiset};
use jsonpath_rfc9535::{JsonPath, ScanMode, ScanQuery};
use proptest::prelude::*;
use serde_json::Value;

/// Arbitrary JSON whose member names come from the query pool's alphabet and whose
/// string values are arbitrary (escaping hazards included).
fn arb_doc() -> impl Strategy<Value = Value> {
    let name = prop::sample::select(vec!["a", "b", "k", "n", "x"]);
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-100_i64..100).prop_map(Value::from),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 48, 6, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::hash_map(name.clone().prop_map(str::to_owned), inner, 0..6)
                .prop_map(|members| Value::Object(members.into_iter().collect())),
        ]
    })
}

/// Tokens biased toward JSON structure — stress for the byte engine's classifier.
const JSON_TOKENS: &[&str] = &[
    "{", "}", "[", "]", ",", ":", " ", "\t", "\n", "\"", "\\", "\u{feff}", "\"a\"", "\"b\"", "1",
    "-1", "1e", "true", "null", "\"x",
];

/// Raw scan inputs: pure noise, JSON-flavored token soup, and truncated valid JSON —
/// none guaranteed (or likely) to parse, unlike [`arb_doc`]'s serialized documents.
fn arb_raw_input() -> impl Strategy<Value = String> {
    let noise = any::<String>();
    let token_soup = prop::collection::vec(prop::sample::select(JSON_TOKENS), 0..64)
        .prop_map(|tokens| tokens.concat());
    let truncated = (arb_doc(), any::<prop::sample::Index>()).prop_map(|(document, index)| {
        let chars: Vec<char> = document.to_string().chars().collect();
        let cut = index.index(chars.len() + 1);
        chars.into_iter().take(cut).collect()
    });
    prop_oneof![noise, token_soup, truncated]
}

/// One query per plan shape the splitter can produce.
const QUERIES: &[&str] = &[
    "$.a.b",              // singular full scan
    "$..a",               // descendant full scan (overlap possible → adaptive budget)
    "$.a[*].b",           // wildcard full scan
    "$.a[0]",             // index full scan
    "$.a[?@.n > 3]",      // pushdown, comparison leaf
    "$.a[?@.n].b",        // pushdown + residual
    "$.a[?@.n && @.x]",   // pushdown, two leaves
    "$.a[?@.b && @.b.k]", // pushdown, overlapping leaves (parent + extension)
    "$[?@.x]",            // pushdown at the root
    "$.a[?@.b[*]]",       // plain scan with predicate (general existence)
    "$..a[?@.n]",         // plain scan with predicate (descendant prefix)
    "$.a[?@ > 1]",        // plain scan with predicate (bare `@`)
    "$.a[1:3]",           // slice in the residual
    "$.a[-1]",            // negative index in the residual
    "$[-1]",              // unsplittable: Plan::Dom fallback in every mode
    "$[*]",               // whole-document: adaptive routes to DOM, forced scans
];

proptest! {
    /// All three scan modes agree with DOM evaluation on any generated document.
    #[test]
    fn scan_modes_agree_with_dom(
        document in arb_doc(),
        pad in 0_usize..2048,
        query in prop::sample::select(QUERIES),
    ) {
        // Leading whitespace is valid JSON padding: it grows the input without
        // touching the candidates, steering pushdown's cost model between its two
        // finishes — so both run under this property.
        let text = format!(
            "{}{}",
            " ".repeat(pad),
            serde_json::to_string(&document)
                .map_err(|error| TestCaseError::fail(format!("serialize: {error}")))?
        );
        let dom_query = JsonPath::parse(query)
            .map_err(|error| TestCaseError::fail(format!("parse: {error}")))?;
        let want: Vec<Value> = dom_query.query_values(&document).into_iter().cloned().collect();

        let compiled = ScanQuery::new(dom_query);
        for mode in [ScanMode::AlwaysScan, ScanMode::Adaptive, ScanMode::NeverScan] {
            let got = compiled
                .clone()
                .with_mode(mode)
                .query_values(&text)
                .map_err(|error| TestCaseError::fail(format!("{mode:?} evaluation: {error}")))?;
            let matched = got == want
                || (has_descendant_segment(query) && multiset(&got) == multiset(&want));
            prop_assert!(
                matched,
                "`{query}` in {mode:?}: scan selected {} nodes, DOM {} over {text}",
                got.len(),
                want.len(),
            );
        }
    }

    /// Any raw input — noise, JSON-flavored token soup, truncated JSON — terminates
    /// without panicking in every mode; when the input happens to be valid JSON that
    /// round-trips canonically, the results must also agree with DOM.
    #[test]
    fn raw_input_terminates_and_canonical_json_agrees(
        text in arb_raw_input(),
        query in prop::sample::select(QUERIES),
    ) {
        let compiled = ScanQuery::parse(query)
            .map_err(|error| TestCaseError::fail(format!("parse: {error}")))?;
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        // Differential-assert only on canonical text: duplicate keys and escaped
        // names are documented divergences, and both are erased by a serialization
        // round-trip mismatch.
        // NOT `*value == text` (a `Value`/`str` comparison tests whether the JSON *is*
        // that string scalar); the gate is serialization round-tripping byte-identically.
        let canonical = parsed.as_ref().is_some_and(|value| {
            let round_trip = value.to_string();
            round_trip == text
        });
        for mode in [ScanMode::AlwaysScan, ScanMode::Adaptive, ScanMode::NeverScan] {
            let outcome = compiled.clone().with_mode(mode).query_values(&text);
            let Some(document) = parsed.as_ref().filter(|_| canonical) else {
                // Malformed or non-canonical input: terminating without a panic is
                // the property; the result itself is undefined in scan modes.
                drop(outcome);
                continue;
            };
            let got = outcome.map_err(|error| {
                TestCaseError::fail(format!("{mode:?} errored on canonical JSON: {error}"))
            })?;
            let want: Vec<Value> = JsonPath::parse(query)
                .map_err(|error| TestCaseError::fail(format!("parse: {error}")))?
                .query_values(document)
                .into_iter()
                .cloned()
                .collect();
            let matched = got == want
                || (has_descendant_segment(query) && multiset(&got) == multiset(&want));
            prop_assert!(
                matched,
                "`{query}` in {mode:?}: scan selected {} nodes, DOM {} over {text}",
                got.len(),
                want.len(),
            );
        }
    }
}
