//! RFC 9535 conformance harness, driven by the official JSONPath Compliance Test
//! Suite (`tests/data/compliance_test_suite.json`, 703 cases).
//!
//! The suite exercises the full RFC — including `match()`/`search()` — so it is
//! meaningful only with the `regex` feature, and the whole harness is gated on it.
//! Behavioral tests for the feature boundary itself live in `regex_functions.rs`.
//!
//! Each case carries a `selector` plus one of:
//! * `invalid_selector: true` — `JsonPath::parse` must reject it;
//! * `result` + `result_paths` — the query must produce exactly that nodelist;
//! * `results` + `results_paths` — the query must match one of several accepted
//!   orderings (used where member/wildcard order is unspecified).
//!
//! This harness checks both the selected *values* and their *normalized paths*, in
//! lockstep: a case passes only when the value at each position and its path both
//! match. It runs every case and reports a pass/fail summary in the assertion message.
#![cfg(feature = "regex")]

use jsonpath_rfc9535::{JsonPath, SingularSegment};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Suite {
    tests: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    selector: String,
    #[serde(default)]
    document: Option<Value>,
    #[serde(default)]
    result: Option<Vec<Value>>,
    #[serde(default)]
    result_paths: Option<Vec<String>>,
    #[serde(default)]
    results: Option<Vec<Vec<Value>>>,
    #[serde(default)]
    results_paths: Option<Vec<Vec<String>>>,
    #[serde(default)]
    invalid_selector: bool,
}

const SUITE: &str = include_str!("data/compliance_test_suite.json");

/// Maximum number of individual failures to list in the summary (keeps the message
/// readable while iterating).
const MAX_LISTED: usize = 40;

#[test]
fn suite_loaded() {
    let suite: Suite =
        serde_json::from_str(SUITE).expect("compliance_test_suite.json is valid JSON");
    assert_eq!(
        suite.tests.len(),
        703,
        "the vendored compliance suite should contain 703 cases"
    );
    let invalid = suite
        .tests
        .iter()
        .filter(|case| case.invalid_selector)
        .count();
    assert_eq!(
        invalid, 247,
        "247 of the cases must be rejected as invalid selectors"
    );
}

#[test]
fn compliance() {
    let suite: Suite =
        serde_json::from_str(SUITE).expect("compliance_test_suite.json is valid JSON");
    let total = suite.tests.len();
    let mut failures: Vec<String> = Vec::new();
    for case in &suite.tests {
        if let Err(reason) = run_case(case) {
            failures.push(format!("  [{}] {reason}", case.name));
        }
    }

    let passed = total - failures.len();
    let shown: Vec<&String> = failures.iter().take(MAX_LISTED).collect();
    let omitted = failures.len().saturating_sub(MAX_LISTED);
    let listing = shown
        .iter()
        .map(|line| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        failures.is_empty(),
        "Compliance suite: {passed}/{total} passed, {} failed (showing {}{}):\n{listing}",
        failures.len(),
        shown.len(),
        if omitted > 0 {
            format!(", {omitted} more omitted")
        } else {
            String::new()
        },
    );
}

#[test]
fn singular_steps_walk_to_the_selected_node() {
    let suite: Suite =
        serde_json::from_str(SUITE).expect("compliance_test_suite.json is valid JSON");
    let mut walked = 0;
    let mut failures: Vec<String> = Vec::new();
    for case in suite.tests.iter().filter(|case| !case.invalid_selector) {
        let (Ok(path), Some(document)) = (JsonPath::parse(&case.selector), &case.document) else {
            continue;
        };
        let Some(steps) = path.singular_steps() else {
            continue;
        };
        walked += 1;
        let selected = path.query_values(document);
        let by_hand: Vec<&Value> = walk(document, steps).into_iter().collect();
        if selected != by_hand {
            failures.push(format!(
                "  [{}] `{}`: query selected {} nodes, walking the steps reached {}",
                case.name,
                case.selector,
                selected.len(),
                by_hand.len()
            ));
        }
    }
    assert!(walked > 0, "no compliance case has singular steps");
    assert!(
        failures.is_empty(),
        "{} of {walked} singular cases disagree:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Follows member-name and array-index steps through `document` without the evaluator.
fn walk<'a>(document: &'a Value, steps: &[SingularSegment]) -> Option<&'a Value> {
    steps.iter().try_fold(document, |node, step| match step {
        SingularSegment::Name(name) => node.as_object()?.get(name),
        SingularSegment::Index(index) => {
            let array = node.as_array()?;
            let offset = usize::try_from(index.get().unsigned_abs()).ok()?;
            let position = if index.get() < 0 {
                array.len().checked_sub(offset)?
            } else {
                offset
            };
            array.get(position)
        }
    })
}

/// Runs a single case, returning `Err(reason)` if it does not conform.
fn run_case(case: &Case) -> Result<(), String> {
    let compiled = JsonPath::parse(&case.selector);

    if case.invalid_selector {
        return match compiled {
            Ok(_) => Err("expected the selector to be rejected, but it compiled".to_owned()),
            Err(_) => Ok(()),
        };
    }

    let path = compiled.map_err(|error| format!("expected a valid selector, got: {error}"))?;
    let document = case
        .document
        .as_ref()
        .ok_or_else(|| "valid case is missing its `document`".to_owned())?;
    let nodes = path.query(document);
    let got_values: Vec<&Value> = nodes.values().collect();
    let got_paths: Vec<String> = nodes.paths().map(ToString::to_string).collect();

    // The path-free evaluator (`query_values`) must agree, value-for-value and in order,
    // with the path-building one (`query`).
    let values_only = path.query_values(document);
    if values_only != got_values {
        return Err(format!(
            "query_values disagrees with query ({} vs {} values)",
            values_only.len(),
            got_values.len()
        ));
    }

    match (&case.result, &case.results) {
        (Some(values), _) => {
            let paths = case
                .result_paths
                .as_ref()
                .ok_or_else(|| "case has `result` but no `result_paths`".to_owned())?;
            check_values(&got_values, values)?;
            check_paths(&got_paths, paths)
        }
        (None, Some(value_orderings)) => {
            let path_orderings = case
                .results_paths
                .as_ref()
                .ok_or_else(|| "case has `results` but no `results_paths`".to_owned())?;
            // Values and paths come from the same nodelist, so an accepted ordering must
            // match both at the *same* index — guarding against equal values reached by
            // different paths.
            let matched = value_orderings
                .iter()
                .zip(path_orderings)
                .any(|(values, paths)| values_match(&got_values, values) && got_paths == *paths);
            if matched {
                Ok(())
            } else {
                Err(format!(
                    "no accepted ordering matched (got {} nodes)",
                    got_values.len()
                ))
            }
        }
        (None, None) => Err("valid case has neither `result` nor `results`".to_owned()),
    }
}

fn check_values(got: &[&Value], expected: &[Value]) -> Result<(), String> {
    if values_match(got, expected) {
        Ok(())
    } else {
        Err(format!(
            "value mismatch (got {} nodes, want {})",
            got.len(),
            expected.len()
        ))
    }
}

fn check_paths(got: &[String], expected: &[String]) -> Result<(), String> {
    if got == expected {
        return Ok(());
    }
    let first = got
        .iter()
        .zip(expected)
        .enumerate()
        .find(|(_, (g, e))| g != e)
        .map(|(index, (g, e))| format!("; first diff at [{index}]: got `{g}`, want `{e}`"))
        .unwrap_or_default();
    Err(format!(
        "path mismatch (got {} paths, want {}){first}",
        got.len(),
        expected.len()
    ))
}

fn values_match(got: &[&Value], expected: &[Value]) -> bool {
    got.len() == expected.len() && got.iter().zip(expected).all(|(node, want)| *node == want)
}
