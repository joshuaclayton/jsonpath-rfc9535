//! Property-based tests (proptest) for invariants that must hold across *all* inputs,
//! complementing the example-driven compliance harness in `compliance_test_suite.rs`.
//!
//! Four properties:
//! * **parse never panics** — `JsonPath::parse` returns `Ok`/`Err` for any input,
//!   including adversarial token soup, but never panics.
//! * **normalized paths round-trip** — every path the engine emits re-parses as a
//!   JSONPath query and re-renders to the identical string (RFC 9535 §2.7 paths are
//!   canonical, so normalization is idempotent).
//! * **a normalized path re-selects its own node** — re-querying the document with a
//!   node's path yields exactly that node, with the same value.
//! * **a path's elements walk back to its node** — following `elements()` step by
//!   step through the document lands on the same value, so the structural form is
//!   as faithful as the string form.

use jsonpath_rfc9535::{Element, JsonPath};
use proptest::prelude::*;
use serde_json::Value;

/// A recursive strategy producing arbitrary JSON documents: scalars at the leaves
/// (including strings full of escaping hazards — quotes, backslashes, control
/// characters) nested inside arbitrary arrays and objects.
fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_filter_map("JSON numbers are finite", |float| {
            serde_json::Number::from_f64(float).map(Value::Number)
        }),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
            prop::collection::hash_map(any::<String>(), inner, 0..8)
                .prop_map(|members| Value::Object(members.into_iter().collect())),
        ]
    })
}

/// Concatenates JSONPath tokens into syntactically plausible — but usually invalid —
/// query strings, to drive the parser deep before it rejects them.
fn arb_query_soup() -> impl Strategy<Value = String> {
    let token = prop_oneof![
        Just("$"),
        Just("@"),
        Just("."),
        Just(".."),
        Just("*"),
        Just(","),
        Just(":"),
        Just("["),
        Just("]"),
        Just("("),
        Just(")"),
        Just("?"),
        Just("!"),
        Just("=="),
        Just("!="),
        Just("<="),
        Just(">="),
        Just("<"),
        Just(">"),
        Just("&&"),
        Just("||"),
        Just("length"),
        Just("count"),
        Just("value"),
        Just("match"),
        Just("search"),
        Just("true"),
        Just("false"),
        Just("null"),
        Just("'x'"),
        Just("\"y\""),
        Just("0"),
        Just("-1"),
        Just("42"),
        Just("1e9"),
        Just("a"),
        Just(" "),
    ];
    prop::collection::vec(token, 0..32).prop_map(|tokens| tokens.concat())
}

/// Every node of `document` (via `$..*`), paired as `(normalized path string, value)`.
/// Returns empty if the fixed query somehow fails to compile (it never does), keeping
/// this helper free of the `expect`/`unwrap` the strict lint profile forbids outside
/// `#[test]` functions.
fn located_nodes(document: &Value) -> Vec<(String, &Value)> {
    JsonPath::parse("$..*").map_or_else(
        |_| Vec::new(),
        |all| {
            all.query(document)
                .iter()
                .map(|node| (node.path().to_string(), node.value()))
                .collect()
        },
    )
}

proptest! {
    /// Parsing arbitrary text must terminate with a result, never a panic.
    #[test]
    fn parse_never_panics_on_arbitrary_text(input in any::<String>()) {
        let _ = JsonPath::parse(&input);
    }

    /// Parsing structured token soup must likewise never panic.
    #[test]
    fn parse_never_panics_on_token_soup(input in arb_query_soup()) {
        let _ = JsonPath::parse(&input);
    }

    /// Every normalized path the engine emits is itself a valid JSONPath query, and
    /// re-querying with it reproduces that one canonical path — nothing more.
    #[test]
    fn normalized_paths_round_trip(document in arb_json()) {
        for (rendered, _value) in located_nodes(&document) {
            let Ok(reparsed) = JsonPath::parse(&rendered) else {
                return Err(TestCaseError::fail(format!(
                    "normalized path `{rendered}` should be a valid JSONPath query"
                )));
            };
            let again: Vec<String> = reparsed
                .query(&document)
                .paths()
                .map(ToString::to_string)
                .collect();
            prop_assert_eq!(
                again,
                vec![rendered.clone()],
                "re-querying `{}` should yield exactly that canonical path",
                rendered
            );
        }
    }

    /// Re-querying the document with a node's normalized path selects exactly that
    /// node — same value, nothing else.
    #[test]
    fn normalized_path_reselects_its_node(document in arb_json()) {
        for (rendered, value) in located_nodes(&document) {
            let Ok(reparsed) = JsonPath::parse(&rendered) else {
                return Err(TestCaseError::fail(format!(
                    "normalized path `{rendered}` should be a valid JSONPath query"
                )));
            };
            let selected = reparsed.query(&document);
            prop_assert_eq!(
                selected.len(),
                1,
                "path `{}` should select exactly one node",
                rendered
            );
            if let Some(only) = selected.exactly_one() {
                prop_assert_eq!(
                    only.value(),
                    value,
                    "path `{}` re-selected a different value",
                    rendered
                );
            }
        }
    }

    /// Following a node's `elements()` through the document — `get(name)` /
    /// `get(index)` per step — lands on exactly the node the path identifies.
    /// This is the contract structural consumers (e.g. locating a parent
    /// container for removal) depend on.
    #[test]
    fn elements_walk_reselects_its_node(document in arb_json()) {
        let Ok(all) = JsonPath::parse("$..*") else {
            return Err(TestCaseError::fail("the fixed $..* query always parses"));
        };
        for node in &all.query(&document) {
            let mut current = &document;
            for element in node.path().elements() {
                let stepped = match element {
                    Element::Name(name) => current.get(name),
                    Element::Index(index) => current.get(index),
                };
                let Some(next) = stepped else {
                    return Err(TestCaseError::fail(format!(
                        "a step of `{}` selects nothing in the document", node.path()
                    )));
                };
                current = next;
            }
            prop_assert_eq!(
                current,
                node.value(),
                "walking the elements of `{}` reached a different node",
                node.path()
            );
        }
    }
}
