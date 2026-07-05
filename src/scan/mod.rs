//! Hybrid byte-scanning evaluation over raw JSON text (the `scan` feature).
//!
//! [`ScanQuery`] answers a different question than [`JsonPath`]: *"I have JSON text and
//! one query — what does it select?"* — without building a whole-document
//! [`serde_json::Value`]. The pipeline:
//!
//! 1. [`split`](split::split) divides the compiled query into a structural **prefix**
//!    (names, indexes, wildcards, descendants) and a **residual** (filters, everything
//!    else).
//! 2. An [rsonpath](https://docs.rs/rsonpath-lib) engine scans the raw bytes for the
//!    prefix and extracts each matched node's JSON text — a *fragment*.
//! 3. Each fragment alone is parsed into an owned [`Value`]; the residual (predicate and
//!    tail segments) is evaluated against it with the crate's own evaluator, the
//!    fragment acting as both current node and root.
//!
//! Only matched fragments are ever parsed, so the cost scales with the query's
//! selectivity instead of the document's size. Queries that cannot be split (see the
//! caveats on [`ScanQuery::query_values`]) transparently fall back to parsing the whole
//! document and evaluating normally — [`ScanQuery::uses_scan`] reports which path a
//! query got.
//!
//! Correctness of per-fragment evaluation: segment application distributes over
//! nodelist concatenation — applying the residual to each prefix-selected node
//! independently, in emission order, yields the same nodelist as evaluating the whole
//! query over the document (up to orderings RFC 9535 leaves unspecified). Fragments
//! never see `$`-rooted sub-queries; [`split`](split::split) rejects those at
//! construction.

mod split;

use crate::{Error, JsonPath};
use core::str::FromStr;
use rsonpath::engine::Engine as _;
use rsonpath::input::BorrowedBytes;
use rsonpath::result::{Match, Sink};
use serde_json::Value;
use split::{Plan, ScanPlan};
use std::fmt::{self, Display};

/// How a [`ScanQuery`] chooses between the byte engine and a whole-document DOM parse.
///
/// The byte engine wins when the query's structural prefix is *selective* — its cost
/// scales with the bytes the prefix selects, while a DOM parse always pays for the
/// whole document. Selectivity is a property of each document, not of the query, so no
/// fixed choice is right for every input; this knob picks the strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ScanMode {
    /// Decide per document (the default). Byte-scan, except: a prefix that provably
    /// covers the whole document (`$[*]`, `$..*`, root-level filters) goes straight to
    /// DOM, and a scan whose extracted fragments exceed the input's own length —
    /// possible only when nested matches overlap — aborts and falls back to one DOM
    /// parse, capping the pathological case.
    ///
    /// What this deliberately does *not* do is abort a flat-but-barely-selective scan
    /// (fragments at, say, 90% of the document): measured on real documents, bailing
    /// mid-scan and re-parsing costs more than just finishing, and the finished scan
    /// trails a plain DOM parse by only ~10-20%. When you know your prefix spans the
    /// bulk of every document, say so with [`ScanMode::Dom`].
    #[default]
    Adaptive,
    /// Always byte-scan when the query splits, however non-selective the prefix turns
    /// out to be on a given document.
    Scan,
    /// Always parse the whole document and evaluate normally — byte-scanning off.
    Dom,
}

/// A compiled JSONPath query that evaluates against **raw JSON text**.
///
/// A byte-scanning engine extracts prefix-matched fragments straight from the bytes;
/// filters and tail segments are evaluated per fragment — no whole-document DOM is
/// ever built.
///
/// Use this when the JSON exists as text and each document is queried once; stick with
/// [`JsonPath`] when you already hold a [`Value`] or query the same document many
/// times.
///
/// ```
/// use jsonpath_rfc9535::ScanQuery;
///
/// let text = r#"{"store": {"book": [
///     {"title": "A", "price": 5},
///     {"title": "B", "price": 15}
/// ]}}"#;
///
/// let query = ScanQuery::parse("$.store.book[?@.price < 10].title")?;
/// assert!(query.uses_scan(), "a trailing filter splits into prefix + predicate");
/// assert_eq!(query.query_values(text)?, [serde_json::json!("A")]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct ScanQuery {
    /// The complete compiled query, evaluated verbatim in DOM-fallback mode.
    fallback: JsonPath,
    /// The split execution plan derived from `fallback` at construction.
    plan: Plan,
    /// The strategy for choosing between the byte engine and a DOM parse.
    mode: ScanMode,
}

impl ScanQuery {
    /// Compiles a JSONPath query string for evaluation against raw JSON text.
    ///
    /// # Errors
    ///
    /// Exactly the errors of [`JsonPath::parse`] — an invalid or ill-typed query.
    /// Splitting never fails: a query the byte engine cannot express simply runs in
    /// DOM-fallback mode.
    pub fn parse(query: &str) -> Result<Self, Error> {
        Ok(Self::new(&JsonPath::parse(query)?))
    }

    /// Builds a scan plan from an already-compiled query. Infallible: worst case the
    /// plan is a transparent DOM fallback. Runs in [`ScanMode::Adaptive`]; see
    /// [`with_mode`](Self::with_mode).
    #[must_use]
    pub fn new(path: &JsonPath) -> Self {
        Self {
            fallback: path.clone(),
            plan: split::split(path.compiled()),
            mode: ScanMode::default(),
        }
    }

    /// Sets the [`ScanMode`] — force the byte engine, force DOM parsing, or (the
    /// default) decide per document.
    #[must_use]
    pub const fn with_mode(mut self, mode: ScanMode) -> Self {
        self.mode = mode;
        self
    }

    /// Evaluates the query against raw JSON text, returning the selected values as
    /// owned [`Value`]s (there is no document to borrow from).
    ///
    /// Which pipeline runs is governed by the [`ScanMode`] (see
    /// [`with_mode`](Self::with_mode)); the default, [`ScanMode::Adaptive`], byte-scans
    /// but falls back to one whole-document parse when the prefix provably covers the
    /// whole document, or mid-scan when overlapping nested matches make extraction
    /// pathological.
    ///
    /// # Caveats
    ///
    /// * **Malformed input.** In scan mode the byte engine's results over invalid JSON
    ///   are *undefined* — evaluation is guaranteed to terminate without panicking, but
    ///   may return `Ok` with meaningless values rather than an error. DOM-fallback
    ///   mode always reports invalid JSON as [`ScanError::InvalidJson`]. Validate
    ///   untrusted input separately if you rely on rejection.
    /// * **Ordering.** Results are in document order; this can differ from
    ///   [`JsonPath::query_values`] only where RFC 9535 leaves ordering unspecified
    ///   (descendant segments, object member order).
    /// * **Exotically escaped names.** The byte engine matches member names in their
    ///   plain byte form. Names that JSON *requires* escaping never byte-scan (the
    ///   splitter routes them through DOM evaluation automatically), but a document
    ///   that writes an ordinary name in an escaped form (say, a Unicode escape
    ///   spelling out the letter `a`) is missed in scan mode. Serializers essentially
    ///   never do this (`serde_json` does not), but it is a real divergence from DOM
    ///   evaluation.
    /// * **Nesting depth.** Deeply nested documents error rather than evaluate: each
    ///   fragment is parsed with `serde_json` (its recursion limit applies), and the
    ///   engine has a depth cap of its own. DOM mode hits the same `serde_json` limit
    ///   on the whole document.
    /// * **Pathological nesting.** Under a descendant prefix, self-nested structures
    ///   are extracted once per nesting level, so total fragment bytes can exceed the
    ///   document size. [`ScanMode::Adaptive`] (the default) detects this mid-scan and
    ///   falls back to a whole-document parse; [`ScanMode::Scan`] pays the full cost.
    ///
    /// # Errors
    ///
    /// [`ScanError::InvalidJson`] when the whole document fails to parse (DOM mode),
    /// [`ScanError::InvalidFragment`] when an extracted fragment does (scan mode), and
    /// [`ScanError::Engine`] when the byte engine fails (detected malformed input,
    /// depth above its limit).
    pub fn query_values(&self, json_text: &str) -> Result<Vec<Value>, ScanError> {
        match (&self.plan, self.mode) {
            (Plan::Dom, _) | (Plan::Scan(_), ScanMode::Dom) => self.run_dom(json_text),
            (Plan::Scan(plan), ScanMode::Scan) => run_scan(plan, json_text),
            (Plan::Scan(plan), ScanMode::Adaptive) => {
                if plan.whole_document {
                    // Provably non-selective for any document: don't touch the bytes.
                    return self.run_dom(json_text);
                }
                match run_scan_within_budget(plan, json_text) {
                    BudgetOutcome::Done(result) => result,
                    BudgetOutcome::Exceeded => self.run_dom(json_text),
                }
            }
        }
    }

    /// Whole-document parse + normal evaluation: the [`Plan::Dom`] path, forced DOM
    /// mode, and the adaptive fallback all land here.
    fn run_dom(&self, json_text: &str) -> Result<Vec<Value>, ScanError> {
        let document: Value =
            serde_json::from_str(json_text).map_err(|source| ScanError::InvalidJson { source })?;
        Ok(self
            .fallback
            .query_values(&document)
            .into_iter()
            .cloned()
            .collect())
    }

    /// Whether evaluation will reach for the byte-scanning engine at all. `false` when
    /// the query cannot split, when the mode is [`ScanMode::Dom`], or when the mode is
    /// [`ScanMode::Adaptive`] and the prefix provably covers the whole document. Under
    /// [`ScanMode::Adaptive`] a `true` still means *attempts*: a non-selective document
    /// can make an individual evaluation fall back mid-scan.
    #[must_use]
    pub fn uses_scan(&self) -> bool {
        match (&self.plan, self.mode) {
            (Plan::Dom, _) | (Plan::Scan(_), ScanMode::Dom) => false,
            (Plan::Scan(plan), ScanMode::Adaptive) => !plan.whole_document,
            (Plan::Scan(_), ScanMode::Scan) => true,
        }
    }
}

impl FromStr for ScanQuery {
    type Err = Error;

    fn from_str(query: &str) -> Result<Self, Self::Err> {
        Self::parse(query)
    }
}

/// The unbudgeted scan pipeline ([`ScanMode::Scan`]): extract every prefix-matched
/// fragment, then run the per-fragment residual.
fn run_scan(plan: &ScanPlan, json_text: &str) -> Result<Vec<Value>, ScanError> {
    let mut matches: Vec<Match> = Vec::new();
    plan.engine
        .matches(&BorrowedBytes::new(json_text.as_bytes()), &mut matches)
        .map_err(|error| ScanError::Engine {
            message: error.to_string(),
        })?;
    process_fragments(plan, matches)
}

/// What a budgeted scan attempt concluded.
enum BudgetOutcome {
    /// The scan stayed within budget; here is its result (which may still be an
    /// evaluation error — a budget-respecting run is not necessarily a successful one).
    Done(Result<Vec<Value>, ScanError>),
    /// Extracted fragment bytes crossed the budget: the prefix is non-selective on
    /// this document and the caller should parse it whole instead.
    Exceeded,
}

/// The budgeted scan pipeline ([`ScanMode::Adaptive`]): like [`run_scan`], but the
/// sink aborts the engine once cumulative fragment bytes exceed the input's own
/// length, signalling the caller to fall back to a whole-document parse.
///
/// The budget targets exactly one failure mode: **overlapping matches** under a
/// descendant prefix over self-nested data, where the same bytes are re-extracted once
/// per nesting level and total fragment bytes are unbounded (O(depth × size)). A flat
/// document can never trip it — its fragments are disjoint, so they sum to at most the
/// input length. Deliberately NOT a lower threshold: aborting a flat-but-non-selective
/// scan midway was measured slower than finishing it (the abort throws away copied
/// fragments and re-parses from scratch), while the break-even against a DOM parse
/// sits near total coverage anyway.
fn run_scan_within_budget(plan: &ScanPlan, json_text: &str) -> BudgetOutcome {
    let mut sink = BudgetedSink::new(json_text.len());
    match plan
        .engine
        .matches(&BorrowedBytes::new(json_text.as_bytes()), &mut sink)
    {
        Ok(()) => BudgetOutcome::Done(process_fragments(plan, sink.matches)),
        Err(_) if sink.exceeded => BudgetOutcome::Exceeded,
        Err(error) => BudgetOutcome::Done(Err(ScanError::Engine {
            message: error.to_string(),
        })),
    }
}

/// Per-fragment residual evaluation, shared by both scan pipelines: parse each
/// fragment, filter by the predicate, then walk the residual segments with the
/// fragment as current node *and* root (sound because the splitter rejected
/// `$`-rooted sub-queries).
fn process_fragments(plan: &ScanPlan, matches: Vec<Match>) -> Result<Vec<Value>, ScanError> {
    let mut out = Vec::new();
    for found in matches {
        let fragment: Value = serde_json::from_slice(found.bytes())
            .map_err(|source| ScanError::InvalidFragment { source })?;
        if let Some(expr) = &plan.predicate
            && !crate::eval::eval_logical(expr, &fragment, &fragment)
        {
            continue;
        }
        if plan.residual.is_empty() {
            out.push(fragment);
        } else {
            out.extend(
                crate::eval::walk_values(&plan.residual, &fragment, &fragment)
                    .into_iter()
                    .cloned(),
            );
        }
    }
    Ok(out)
}

/// A [`Sink`] that aborts the engine run once cumulative fragment bytes pass `budget`.
/// The abort travels as an engine error; `exceeded` is the authoritative signal that
/// the error was this sink's abort rather than a real engine failure.
struct BudgetedSink {
    matches: Vec<Match>,
    budget: usize,
    spent: usize,
    exceeded: bool,
}

impl BudgetedSink {
    const fn new(budget: usize) -> Self {
        Self {
            matches: Vec::new(),
            budget,
            spent: 0,
            exceeded: false,
        }
    }
}

impl Sink<Match> for BudgetedSink {
    type Error = BudgetExceeded;

    fn add_match(&mut self, data: Match) -> Result<(), BudgetExceeded> {
        self.spent += data.bytes().len();
        if self.spent > self.budget {
            self.exceeded = true;
            return Err(BudgetExceeded);
        }
        self.matches.push(data);
        Ok(())
    }
}

/// The sentinel error [`BudgetedSink`] raises to abort the engine; never surfaced to
/// callers (the fallback swallows it).
#[derive(Debug)]
struct BudgetExceeded;

impl Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "adaptive scan fragment-byte budget exceeded")
    }
}

impl std::error::Error for BudgetExceeded {}

/// An error produced while evaluating a [`ScanQuery`] against raw JSON text.
///
/// Unlike DOM evaluation ([`JsonPath::query`], which is infallible once compiled), scan
/// evaluation can fail: the input text may not be valid JSON, and the byte engine has
/// runtime limits (e.g. maximum nesting depth).
#[derive(Debug)]
#[non_exhaustive]
pub enum ScanError {
    /// The document is not valid JSON (whole-document parse, DOM-fallback mode).
    InvalidJson {
        /// The underlying JSON parse error.
        source: serde_json::Error,
    },
    /// A prefix-matched fragment is not valid JSON (malformed input surfaced mid-scan).
    InvalidFragment {
        /// The underlying JSON parse error.
        source: serde_json::Error,
    },
    /// The byte-scanning engine failed: it detected malformed input, or the document
    /// nests beyond its depth limit.
    Engine {
        /// Human-readable engine failure description.
        message: String,
    },
}

impl Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson { source } => {
                write!(f, "the document is not valid JSON: {source}")
            }
            Self::InvalidFragment { source } => {
                write!(f, "an extracted fragment is not valid JSON: {source}")
            }
            Self::Engine { message } => {
                write!(f, "the byte-scanning engine failed: {message}")
            }
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidJson { source } | Self::InvalidFragment { source } => Some(source),
            Self::Engine { message: _ } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ScanMode, ScanQuery};
    use crate::JsonPath;
    use serde_json::{Value, json};

    /// Scan-path values and DOM-path values for the same query over the same text.
    /// Forces [`ScanMode::Scan`]: these behavioral tests pin the byte pipeline itself,
    /// and the tiny documents here would otherwise trip the adaptive budget and
    /// silently exercise the DOM path instead.
    fn both(query: &str, text: &str) -> (Vec<Value>, Vec<Value>) {
        let scan = ScanQuery::parse(query)
            .expect("test query must compile")
            .with_mode(ScanMode::Scan);
        let scanned = scan
            .query_values(text)
            .expect("scan evaluation must succeed on valid JSON");
        let document: Value = serde_json::from_str(text).expect("test document must parse");
        let dom: Vec<Value> = JsonPath::parse(query)
            .expect("test query must compile")
            .query_values(&document)
            .into_iter()
            .cloned()
            .collect();
        (scanned, dom)
    }

    /// Order-insensitive rendering for cases where RFC 9535 leaves ordering unspecified.
    fn multiset(values: &[Value]) -> Vec<String> {
        let mut rendered: Vec<String> = values.iter().map(ToString::to_string).collect();
        rendered.sort();
        rendered
    }

    #[test]
    fn child_only_prefix_matches_dom_exactly() {
        let text = r#"{"a": [1, 2, {"x": 3}]}"#;
        let (scanned, dom) = both("$.a[?@ > 1]", text);
        assert_eq!(
            scanned, dom,
            "a child-only prefix over an array preserves exact order, scalars included"
        );
        assert_eq!(
            scanned,
            [json!(2)],
            "only the scalar 2 passes the predicate"
        );
    }

    #[test]
    fn nested_self_referencing_fragments_match_dom() {
        // Every `a` nests another `a`, so descendant fragments overlap: the outer
        // fragment's bytes contain the inner fragment. Duplicates and multiplicity must
        // match DOM evaluation.
        let text =
            r#"{"a": {"f": true, "b": 1, "a": {"f": false, "b": 2, "a": {"f": true, "b": 3}}}}"#;

        let (scanned, dom) = both("$..a.b", text);
        assert!(
            !scanned.is_empty(),
            "the descendant query selects something"
        );
        assert_eq!(
            multiset(&scanned),
            multiset(&dom),
            "descendant fragments (including nested ones) select the same multiset as DOM"
        );

        let (scanned, dom) = both("$..a[?@.f].b", text);
        assert!(
            !scanned.is_empty(),
            "the filtered variant selects something"
        );
        assert_eq!(
            multiset(&scanned),
            multiset(&dom),
            "predicate-over-fragments reproduces DOM filter semantics on nested matches"
        );
    }

    #[test]
    fn scalar_fragments_parse_and_match_dom() {
        let text = r#"{"store": {"bicycle": {"price": 399}, "book": [{"price": 8.95}, {"price": 12.99}]}}"#;
        let (scanned, dom) = both("$..price", text);
        assert_eq!(scanned.len(), 3, "all three prices are selected");
        assert_eq!(
            multiset(&scanned),
            multiset(&dom),
            "scalar fragments (numbers) round-trip through extraction"
        );
    }

    #[test]
    fn escape_requiring_member_names_fall_back_but_stay_correct() {
        // The byte engine cannot match names that JSON requires escaping (verified:
        // rsonpath 0.10 selects nothing for them). The splitter must keep them out of
        // the prefix so results stay correct via the DOM paths.
        let text = r#"{"a\"b": 1, "c\\d": 2}"#;

        let quote = ScanQuery::parse(r#"$['a"b']"#).expect("test query must compile");
        assert!(
            !quote.uses_scan(),
            "an escape-requiring name at the root falls back to DOM mode"
        );
        let (scanned, dom) = both(r#"$['a"b']"#, text);
        assert_eq!(
            scanned, dom,
            "a key containing a double quote resolves via DOM"
        );
        assert_eq!(scanned, [json!(1)], "the quoted key selects its value");

        let (scanned, dom) = both(r"$['c\\d']", text);
        assert_eq!(
            scanned, dom,
            "a key containing a backslash resolves via DOM"
        );
        assert_eq!(scanned, [json!(2)], "the backslash key selects its value");

        // A plain prefix still scans; the escape-requiring name resolves in the
        // residual, over the parsed fragment where serde has unescaped it.
        let nested = r#"{"x": {"a\"b": 1}, "y": {"a\"b": 2}}"#;
        let mixed = ScanQuery::parse(r#"$.x['a"b']"#)
            .expect("test query must compile")
            .with_mode(ScanMode::Scan);
        assert!(
            mixed.uses_scan(),
            "the plain `$.x` prefix scans; the escaped name is residual"
        );
        assert_eq!(
            mixed
                .query_values(nested)
                .expect("scan evaluation must succeed on valid JSON"),
            [json!(1)],
            "the residual name lookup runs over the parsed fragment"
        );
    }

    #[test]
    fn malformed_json_errors_in_dom_mode_and_terminates_in_scan_mode() {
        let malformed = r#"{"a": [1, 2"#;

        let dom_mode = ScanQuery::parse("$").expect("`$` compiles");
        assert!(!dom_mode.uses_scan(), "`$` runs in DOM-fallback mode");
        assert!(
            dom_mode.query_values(malformed).is_err(),
            "DOM mode reports malformed JSON as an error"
        );

        let scan_mode = ScanQuery::parse("$.a[*]")
            .expect("`$.a[*]` compiles")
            .with_mode(ScanMode::Scan);
        assert!(scan_mode.uses_scan(), "`$.a[*]` runs in scan mode");
        // The engine's contract on malformed input: undefined results, guaranteed
        // termination, no panic. Either outcome is acceptable; reaching this assert at
        // all is the property under test.
        let outcome = scan_mode.query_values(malformed);
        assert!(
            outcome.is_ok() || outcome.is_err(),
            "scan mode terminates without panicking on malformed input"
        );
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_panic() {
        // 300 levels: the engine scans it, but the extracted fragment (a ~298-deep
        // array) exceeds serde_json's recursion limit when parsed. Deep documents must
        // surface as an error — whichever limit trips first — never a panic.
        let text = format!("{}1{}", "[".repeat(300), "]".repeat(300));
        let query = ScanQuery::parse("$[0][0]")
            .expect("`$[0][0]` compiles")
            .with_mode(ScanMode::Scan);
        assert!(query.uses_scan(), "an index-only prefix runs in scan mode");
        assert!(
            query.query_values(&text).is_err(),
            "a document nested beyond the parse limits reports an error"
        );
    }

    #[test]
    fn all_modes_agree_on_results() {
        let text =
            r#"{"pad": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "a": [{"x": 1}, {"y": 2}, {"x": 3}]}"#;
        for query in ["$.a[?@.x]", "$..x", "$.a[0].x", "$[?@.x]"] {
            let base = ScanQuery::parse(query).expect("test query must compile");
            let adaptive = base
                .clone()
                .query_values(text)
                .expect("adaptive evaluation must succeed on valid JSON");
            let forced_scan = base
                .clone()
                .with_mode(ScanMode::Scan)
                .query_values(text)
                .expect("forced-scan evaluation must succeed on valid JSON");
            let forced_dom = base
                .with_mode(ScanMode::Dom)
                .query_values(text)
                .expect("forced-DOM evaluation must succeed on valid JSON");
            assert_eq!(
                adaptive, forced_scan,
                "`{query}`: adaptive and forced-scan agree"
            );
            assert_eq!(
                adaptive, forced_dom,
                "`{query}`: adaptive and forced-DOM agree"
            );
        }
    }

    #[test]
    fn adaptive_routes_whole_document_prefixes_to_dom() {
        // `$[?P]` renders to prefix `$[*]`: fragments cover the whole document for any
        // input, so adaptive mode must not even attempt the byte engine.
        let query = ScanQuery::parse("$[?@.x]").expect("`$[?@.x]` compiles");
        assert!(
            !query.uses_scan(),
            "adaptive: a root-level filter is statically non-selective, so it goes DOM"
        );
        assert!(
            query.clone().with_mode(ScanMode::Scan).uses_scan(),
            "forcing ScanMode::Scan overrides the static routing"
        );
        assert!(
            !query.clone().with_mode(ScanMode::Dom).uses_scan(),
            "ScanMode::Dom never scans"
        );
        assert_eq!(
            query
                .query_values(r#"[{"x": 1}, {"y": 2}]"#)
                .expect("evaluation must succeed on valid JSON"),
            [json!({"x": 1})],
            "the DOM route still answers the query correctly"
        );
    }

    #[test]
    fn adaptive_bails_out_on_overlapping_fragments_and_stays_correct() {
        // Self-nested `a`s under a descendant prefix: every level's `a` is extracted
        // whole, so cumulative fragment bytes exceed the document's own length — the
        // adaptive budget trips mid-scan and evaluation falls back to a DOM parse.
        // (A flat document can never trip it: disjoint fragments sum to at most the
        // input length.)
        let text = r#"{"a": {"pad": "xxxxxxxxxxxxxxxx", "a": {"pad": "yyyyyyyyyyyyyyyy", "a": {"pad": "zzzzzzzzzzzzzzzz", "b": 1}}}}"#;
        let adaptive = ScanQuery::parse("$..a").expect("test query must compile");
        assert!(
            adaptive.uses_scan(),
            "statically the query may scan — overlap is only known per document"
        );
        let expected = adaptive
            .clone()
            .with_mode(ScanMode::Dom)
            .query_values(text)
            .expect("forced-DOM evaluation must succeed on valid JSON");
        assert_eq!(
            adaptive
                .query_values(text)
                .expect("adaptive evaluation must succeed on valid JSON"),
            expected,
            "the mid-scan fallback returns identical results"
        );
        assert_eq!(
            expected.len(),
            3,
            "all three nested `a` objects are selected"
        );
    }

    #[test]
    fn filter_over_scalar_fragments_selects_their_children_like_dom() {
        // `[?P]` selects among a node's *children*; scalars have none. The appended
        // `[*]` in the scan prefix reproduces exactly that.
        let text = r#"{"a": [7, {"x": 1}, [42]]}"#;
        let (scanned, dom) = both("$.a[*][?@ == 42]", text);
        assert_eq!(
            scanned, dom,
            "scalar prefix matches contribute no children in either mode"
        );
        assert_eq!(
            scanned,
            [json!(42)],
            "only the array element's child matches"
        );
    }
}
