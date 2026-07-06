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

use common::multiset;
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

/// One query per plan shape the splitter can produce.
const QUERIES: &[&str] = &[
    "$.a.b",            // singular full scan
    "$..a",             // descendant full scan (overlap possible → adaptive budget)
    "$.a[*].b",         // wildcard full scan
    "$.a[0]",           // index full scan
    "$.a[?@.n > 3]",    // pushdown, comparison leaf
    "$.a[?@.n].b",      // pushdown + residual
    "$.a[?@.n && @.x]", // pushdown, two leaves
    "$[?@.x]",          // pushdown at the root
    "$.a[?@.b[*]]",     // plain scan with predicate (general existence)
    "$..a[?@.n]",       // plain scan with predicate (descendant prefix)
    "$.a[?@ > 1]",      // plain scan with predicate (bare `@`)
    "$.a[1:3]",         // slice in the residual
    "$.a[-1]",          // negative index in the residual
    "$[*]",             // whole-document: adaptive routes to DOM, forced scans
];

proptest! {
    /// Both scan modes agree with DOM evaluation on any generated document.
    #[test]
    fn scan_modes_agree_with_dom(document in arb_doc(), query_index in 0..QUERIES.len()) {
        let Some(query) = QUERIES.get(query_index) else {
            return Err(TestCaseError::fail("query index out of range"));
        };
        let text = serde_json::to_string(&document)
            .map_err(|error| TestCaseError::fail(format!("serialize: {error}")))?;
        let dom_query = JsonPath::parse(query)
            .map_err(|error| TestCaseError::fail(format!("parse: {error}")))?;
        let want: Vec<Value> = dom_query.query_values(&document).into_iter().cloned().collect();

        let compiled = ScanQuery::new(&dom_query);
        for mode in [ScanMode::AlwaysScan, ScanMode::Adaptive, ScanMode::NeverScan] {
            let got = compiled
                .clone()
                .with_mode(mode)
                .query_values(&text)
                .map_err(|error| TestCaseError::fail(format!("{mode:?} evaluation: {error}")))?;
            let matched = got == want
                || (query.contains("..") && multiset(&got) == multiset(&want));
            prop_assert!(
                matched,
                "`{query}` in {mode:?}: scan selected {} nodes, DOM {} over {text}",
                got.len(),
                want.len(),
            );
        }
    }
}
