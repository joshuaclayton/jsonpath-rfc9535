//! Equivalence harness for the `scan` hybrid evaluator, driven by the same official
//! JSONPath Compliance Test Suite as `compliance_test_suite.rs`.
//!
//! Ground truth here is the crate's own DOM evaluator — the other harness already
//! verifies it against the suite's expected nodelists, values and paths in lockstep —
//! so `ScanQuery` agreeing with `JsonPath` on every case transitively pins the scan
//! path to the suite.
//!
//! Ordering: scan mode emits strict document order. That equals DOM order except where
//! RFC 9535 leaves ordering unspecified, which in this corpus only descendant segments
//! exercise (object-member iteration follows document order in test builds via
//! `preserve_order`). So a case must match **exactly**, unless its selector contains
//! `..`, where a multiset match is accepted instead.
//!
//! Each case runs twice: once in **forced [`ScanMode::Scan`]** — the suite's documents
//! are tiny, so the adaptive fragment-byte budget would demote most of them to DOM and
//! the byte pipeline would go untested — and once in the default **adaptive** mode,
//! which pins the static routing and mid-scan fallback to the same answers.
//!
//! The harness also pins a **coverage floor**: the number of valid cases that actually
//! take the byte-scanning path (in forced mode). A splitter regression that silently
//! demotes everything to DOM fallback would keep every equivalence check green — the
//! floor is what fails.
#![cfg(all(feature = "scan", feature = "regex"))]

mod common;

use common::multiset;
use jsonpath_rfc9535::{JsonPath, ScanMode, ScanQuery};
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
    invalid_selector: bool,
}

const SUITE: &str = include_str!("data/compliance_test_suite.json");

/// Maximum number of individual failures to list in the summary.
const MAX_LISTED: usize = 40;

/// Measured on the vendored suite: the number of valid cases whose query actually runs
/// on the byte-scanning engine. Falls only if the splitter regresses (or the suite
/// changes); update deliberately, not to make the build pass.
const SCAN_COVERAGE_FLOOR: usize = 342;

#[test]
fn scan_agrees_with_dom_on_every_compliance_case() {
    let suite: Suite =
        serde_json::from_str(SUITE).expect("compliance_test_suite.json is valid JSON");
    let mut failures: Vec<String> = Vec::new();
    let mut valid = 0_usize;
    let mut scanned = 0_usize;

    for case in &suite.tests {
        if case.invalid_selector {
            if ScanQuery::parse(&case.selector).is_ok() {
                failures.push(format!(
                    "  [{}] expected the selector to be rejected, but it compiled",
                    case.name
                ));
            }
            continue;
        }
        valid += 1;
        match run_case(case) {
            Ok(used_scan) => scanned += usize::from(used_scan),
            Err(reason) => failures.push(format!("  [{}] {reason}", case.name)),
        }
    }

    let shown: Vec<&str> = failures
        .iter()
        .take(MAX_LISTED)
        .map(String::as_str)
        .collect();
    let omitted = failures.len().saturating_sub(MAX_LISTED);
    assert!(
        failures.is_empty(),
        "scan/DOM equivalence: {} of {valid} valid cases failed (showing {}{}):\n{}",
        failures.len(),
        shown.len(),
        if omitted > 0 {
            format!(", {omitted} more omitted")
        } else {
            String::new()
        },
        shown.join("\n"),
    );
    assert!(
        scanned >= SCAN_COVERAGE_FLOOR,
        "only {scanned} of {valid} valid cases took the byte-scanning path \
         (floor: {SCAN_COVERAGE_FLOOR}) — did the splitter regress into always falling back?"
    );
}

/// Runs one valid case through the byte pipeline (forced scan) and the adaptive mode,
/// comparing both against DOM ground truth; `Ok(used_scan)` when everything agrees.
fn run_case(case: &Case) -> Result<bool, String> {
    let forced = ScanQuery::parse(&case.selector)
        .map_err(|error| format!("expected a valid selector, got: {error}"))?
        .with_mode(ScanMode::Scan);
    let adaptive = forced.clone().with_mode(ScanMode::Adaptive);
    let dom_query = JsonPath::parse(&case.selector)
        .map_err(|error| format!("expected a valid selector, got: {error}"))?;
    let document = case
        .document
        .as_ref()
        .ok_or_else(|| "valid case is missing its `document`".to_owned())?;
    let text = serde_json::to_string(document)
        .map_err(|error| format!("case document failed to serialize: {error}"))?;

    let want: Vec<Value> = dom_query
        .query_values(document)
        .into_iter()
        .cloned()
        .collect();

    for (mode, query) in [("forced-scan", &forced), ("adaptive", &adaptive)] {
        let got = query
            .query_values(&text)
            .map_err(|error| format!("{mode} evaluation failed: {error}"))?;
        let matched = got == want
            // Document order may legally differ from DOM visit order only under
            // descendant segments; everywhere else an order difference is a real bug.
            || (case.selector.contains("..") && multiset(&got) == multiset(&want));
        if !matched {
            return Err(format!(
                "{mode}/DOM mismatch: {mode} selected {} nodes, DOM {}",
                got.len(),
                want.len(),
            ));
        }
    }
    Ok(forced.uses_scan())
}
