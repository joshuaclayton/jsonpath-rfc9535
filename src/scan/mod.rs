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

mod rsonpath_boundary;
mod split;

use crate::{Error, JsonPath};
use core::str::FromStr;
use rsonpath::input::BorrowedBytes;
use rsonpath::result::{Match, MatchSpan, Sink};
use serde_json::Value;
use split::{Plan, PushdownPlan, RootFreeSegments, ScanPlan};
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
    /// bulk of every document, say so with [`ScanMode::NeverScan`].
    #[default]
    Adaptive,
    /// Always byte-scan when the query splits, however non-selective the prefix turns
    /// out to be on a given document.
    AlwaysScan,
    /// Never byte-scan: always parse the whole document and evaluate normally.
    NeverScan,
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
        Ok(Self::new(JsonPath::parse(query)?))
    }

    /// Builds a scan plan from an already-compiled query. Infallible: worst case the
    /// plan is a transparent DOM fallback. Runs in [`ScanMode::Adaptive`]; see
    /// [`with_mode`](Self::with_mode). Takes the path by value — it becomes the plan's
    /// DOM-fallback query; callers keeping their own copy can clone at the call site.
    #[must_use]
    pub fn new(path: JsonPath) -> Self {
        let plan = split::split(path.compiled());
        Self {
            fallback: path,
            plan,
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
    ///   are *undefined* — evaluation terminates without panicking, but may return
    ///   `Ok` with meaningless values rather than an error. The engine has been
    ///   observed to panic internally on malformed input (an upstream rsonpath bug,
    ///   found by this crate's fuzz harness); those panics are caught at the engine
    ///   boundary and surfaced as [`ScanError::Engine`] — under `panic = "abort"`
    ///   they abort the process instead, and the global panic hook may still log a
    ///   message. DOM-fallback mode always reports invalid JSON as
    ///   [`ScanError::InvalidJson`]. Validate untrusted input separately if you rely
    ///   on rejection.
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
    /// * **Duplicate member names.** RFC 8259 calls their behavior unpredictable, and
    ///   the pipelines here genuinely differ: DOM evaluation sees `serde_json`'s
    ///   last-occurrence value, while a pushed-down filter judges the first *or* the
    ///   last occurrence depending on which finish its cost model picks (the byte
    ///   engine reports only the first; the direct-parse finish re-parses with
    ///   `serde_json`). Documents with duplicated keys get no consistency guarantee
    ///   in any mode.
    /// * **Nesting depth.** Deeply nested documents error rather than evaluate: each
    ///   fragment is parsed with `serde_json` (its recursion limit applies), and the
    ///   engine has a depth cap of its own. DOM mode hits the same `serde_json` limit
    ///   on the whole document.
    /// * **Pathological nesting.** Under a descendant prefix, self-nested structures
    ///   are extracted once per nesting level, so total fragment bytes can exceed the
    ///   document size. [`ScanMode::Adaptive`] (the default) detects this mid-scan and
    ///   falls back to a whole-document parse; [`ScanMode::AlwaysScan`] pays the full cost.
    /// * **Adversarial input.** The divergences above (escaped names, duplicate
    ///   members, undefined results on malformed text) are all trivially craftable in
    ///   valid JSON — an attacker can hide a member from scan-mode filters by writing
    ///   its name with a Unicode escape — and [`ScanMode::AlwaysScan`] additionally
    ///   drops the adaptive memory bound. Do not gate security decisions (redaction,
    ///   deny-listing, authorization) on scan-mode results over untrusted text;
    ///   evaluate with [`ScanMode::NeverScan`], or [`JsonPath`] over parsed input,
    ///   instead.
    ///
    /// # Errors
    ///
    /// [`ScanError::InvalidJson`] when the whole document fails to parse (DOM mode),
    /// [`ScanError::InvalidFragment`] when an extracted fragment does (scan mode), and
    /// [`ScanError::Engine`] when the byte engine fails (detected malformed input,
    /// depth above its limit).
    pub fn query_values(&self, json_text: &str) -> Result<Vec<Value>, ScanError> {
        match self.route() {
            Route::Dom => self.run_dom(json_text),
            Route::Pushdown(plan) => run_pushdown(plan, json_text),
            Route::Scan(plan) => run_scan(plan, json_text),
            Route::BudgetedScan(plan) => match run_scan_within_budget(plan, json_text) {
                BudgetOutcome::Done(result) => result,
                BudgetOutcome::Exceeded => self.run_dom(json_text),
            },
        }
    }

    /// Resolves this query's `(plan, mode)` pair to the pipeline an evaluation takes
    /// — the one place the strategy decision is made.
    /// [`query_values`](Self::query_values) dispatches on it;
    /// [`uses_scan`](Self::uses_scan) reports it.
    fn route(&self) -> Route<'_> {
        match (&self.plan, self.mode) {
            (Plan::Dom, _) | (Plan::Scan(_) | Plan::Pushdown(_), ScanMode::NeverScan) => Route::Dom,
            // Pushdown needs no adaptive budget: candidates are disjoint (child-only
            // prefix), leaf values are scalars-or-small, and candidate bytes are only
            // touched for candidates that pass — every cost is bounded by the input.
            (Plan::Pushdown(plan), ScanMode::AlwaysScan | ScanMode::Adaptive) => {
                Route::Pushdown(plan)
            }
            (Plan::Scan(plan), ScanMode::AlwaysScan) => Route::Scan(plan),
            // Provably non-selective for any document: don't touch the bytes.
            (Plan::Scan(plan), ScanMode::Adaptive) if plan.whole_document => Route::Dom,
            (Plan::Scan(plan), ScanMode::Adaptive) => Route::BudgetedScan(plan),
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
    /// the query cannot split, when the mode is [`ScanMode::NeverScan`], or when the mode is
    /// [`ScanMode::Adaptive`] and the prefix provably covers the whole document. Under
    /// [`ScanMode::Adaptive`] a `true` still means *attempts*: a non-selective document
    /// can make an individual evaluation fall back mid-scan.
    #[must_use]
    pub fn uses_scan(&self) -> bool {
        !matches!(self.route(), Route::Dom)
    }
}

/// The pipeline a [`ScanQuery`]'s `(plan, mode)` pair resolves to for one evaluation.
/// Computed in exactly one place ([`ScanQuery::route`]) so dispatch
/// ([`ScanQuery::query_values`]) and introspection ([`ScanQuery::uses_scan`]) cannot
/// drift apart.
enum Route<'a> {
    /// Parse the whole document and evaluate normally: unsplittable plan, forced DOM
    /// mode, or an adaptive prefix that provably covers any document.
    Dom,
    /// Unbudgeted byte scan ([`ScanMode::AlwaysScan`]).
    Scan(&'a ScanPlan),
    /// Byte scan under the adaptive fragment-byte budget; DOM fallback on overflow.
    BudgetedScan(&'a ScanPlan),
    /// Filter pushdown: auxiliary leaf scans decide the predicate first.
    Pushdown(&'a PushdownPlan),
}

impl FromStr for ScanQuery {
    type Err = Error;

    fn from_str(query: &str) -> Result<Self, Self::Err> {
        Self::parse(query)
    }
}

/// The unbudgeted scan pipeline ([`ScanMode::AlwaysScan`]): extract every prefix-matched
/// fragment, then run the per-fragment residual.
fn run_scan(plan: &ScanPlan, json_text: &str) -> Result<Vec<Value>, ScanError> {
    let mut matches: Vec<Match> = Vec::new();
    rsonpath_boundary::collect_matches(
        &plan.engine,
        &BorrowedBytes::new(json_text.as_bytes()),
        &mut matches,
    )?;
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
    let mut sink = BudgetedSink::overlap_tripwire(json_text.len());
    let outcome = rsonpath_boundary::collect_matches(
        &plan.engine,
        &BorrowedBytes::new(json_text.as_bytes()),
        &mut sink,
    );
    if sink.exceeded {
        // Checked before the engine's own result on purpose: once the sink asked to
        // abort, the collected matches are truncated and must not be evaluated —
        // even if the engine somehow finished (or failed differently) after the
        // abort request.
        return BudgetOutcome::Exceeded;
    }
    match outcome {
        Ok(()) => BudgetOutcome::Done(process_fragments(plan, sink.matches)),
        Err(error) => BudgetOutcome::Done(Err(error)),
    }
}

/// Per-fragment residual evaluation, shared by both scan pipelines: parse each
/// fragment, filter by the predicate, then walk the residual segments with the
/// fragment as current node *and* root (the plan's root-free types guarantee no
/// `$`-rooted sub-query can observe the difference).
fn process_fragments(plan: &ScanPlan, matches: Vec<Match>) -> Result<Vec<Value>, ScanError> {
    let mut out = Vec::new();
    for found in matches {
        let fragment: Value = serde_json::from_slice(found.bytes())
            .map_err(|source| ScanError::InvalidFragment { source })?;
        if let Some(predicate) = &plan.predicate
            && !crate::eval::eval_logical(predicate.expr(), &fragment, &fragment)
        {
            continue;
        }
        emit_through_residual(&plan.residual, fragment, &mut out);
    }
    Ok(out)
}

/// Emits one predicate-passing fragment, shared by every scan pipeline: the fragment
/// itself when the residual is empty, otherwise every value the residual segments
/// select from it (fragment = start = root).
fn emit_through_residual(residual: &RootFreeSegments, fragment: Value, out: &mut Vec<Value>) {
    if residual.is_empty() {
        out.push(fragment);
        return;
    }
    out.extend(
        crate::eval::walk_values(residual.segments(), &fragment, &fragment)
            .into_iter()
            .cloned(),
    );
}

/// The pushdown pipeline ([`Plan::Pushdown`]): decide the filter from auxiliary leaf
/// scans, extract only the candidates that pass.
///
/// 1. `approximate_spans` yields each candidate's span — starts exact, ends possibly
///    padded with JSON whitespace (and possibly past the input's end) — with no byte
///    copying. Child-only prefixes guarantee the spans are disjoint and in document
///    order.
/// 2. Each leaf engine extracts its path's values across all candidates, in document
///    order. A leaf value is assigned to the candidate whose span contains its start;
///    when a candidate somehow yields several (duplicate member names), the last one
///    wins (though the engine itself reports only the first — see the caveat on
///    [`ScanQuery::query_values`]).
/// 3. Per candidate, the leaf values are assembled into a synthetic fragment — an
///    object holding just the paths the predicate consults; a missing leaf is simply
///    absent, which evaluates as RFC 9535 "Nothing" — and the predicate runs on it.
/// 4. Only passing candidates are sliced out of the input and parsed; the trailing
///    whitespace an approximate span may include is accepted by `serde_json`.
fn run_pushdown(plan: &PushdownPlan, json_text: &str) -> Result<Vec<Value>, ScanError> {
    let input = BorrowedBytes::new(json_text.as_bytes());
    let mut candidate_spans: Vec<MatchSpan> = Vec::new();
    rsonpath_boundary::collect_approximate_spans(&plan.candidates, &input, &mut candidate_spans)?;

    // Saturating on purpose: provably non-overflowing only via the span-disjointness
    // invariant, and the cost model should stay total even if that ever broke.
    let candidate_bytes = candidate_spans
        .iter()
        .map(MatchSpan::len)
        .fold(0_usize, usize::saturating_add);
    match choose_finish(candidate_bytes, json_text.len(), plan.leaves.len()) {
        PushdownFinish::ParseCandidates => {
            finish_by_parsing_candidates(plan, json_text, &candidate_spans)
        }
        PushdownFinish::LeafScans => finish_by_leaf_scans(plan, json_text, &input, candidate_spans),
    }
}

/// Measured cost of parsing one byte of JSON into a [`Value`], in units of
/// byte-scanning that byte. Deliberately *understates* the measured ratio so
/// [`PushdownFinish::ParseCandidates`] is chosen only when it clearly wins.
const PARSE_TO_SCAN_COST_RATIO: usize = 32;

/// How a pushdown run finishes once the span pass has measured the candidate set.
/// Chosen by [`choose_finish`].
enum PushdownFinish {
    /// Parse every candidate outright and judge the real fragments: cheaper when the
    /// candidates are a sliver of the input.
    ParseCandidates,
    /// Extract the predicate's leaf values with auxiliary scans and parse only the
    /// passing candidates: cheaper when the candidates span most of the input.
    LeafScans,
}

/// The pushdown cost model: compares parsing all candidates
/// (`candidate_bytes × PARSE_TO_SCAN_COST_RATIO` scan-units) against deciding the
/// predicate from auxiliary scans (one whole-input scan per leaf), and picks the
/// cheaper finish.
fn choose_finish(candidate_bytes: usize, input_len: usize, leaf_count: usize) -> PushdownFinish {
    let leaf_scan_equivalent = input_len.saturating_mul(leaf_count.max(1));
    if candidate_bytes.saturating_mul(PARSE_TO_SCAN_COST_RATIO) <= leaf_scan_equivalent {
        PushdownFinish::ParseCandidates
    } else {
        PushdownFinish::LeafScans
    }
}

/// Small candidate set: parse every candidate and run the predicate on the real
/// fragment — one scan pass total, the old fragment-scan cost profile without the
/// byte copies.
fn finish_by_parsing_candidates(
    plan: &PushdownPlan,
    json_text: &str,
    candidate_spans: &[MatchSpan],
) -> Result<Vec<Value>, ScanError> {
    let bytes = json_text.as_bytes();
    let mut out = Vec::new();
    for span in candidate_spans {
        let Some(candidate) = parse_candidate(bytes, span)? else {
            continue;
        };
        if !crate::eval::eval_logical(plan.predicate.expr(), &candidate, &candidate) {
            continue;
        }
        emit_through_residual(&plan.residual, candidate, &mut out);
    }
    Ok(out)
}

/// Slices a candidate's approximate span out of the input and parses it. The span's
/// end may be padded past the input's length with JSON whitespace (`serde_json`
/// accepts trailing whitespace inside the slice); a span lying outside the input
/// entirely — which engine-produced spans never do — yields `None` rather than
/// panicking.
fn parse_candidate(bytes: &[u8], span: &MatchSpan) -> Result<Option<Value>, ScanError> {
    // Only a span's end may legally exceed the input (whitespace padding). A start
    // outside the input cannot happen on valid JSON, but malformed input makes the
    // engine's spans undefined — `bytes.get` below turns that into a skipped
    // candidate rather than a panic.
    let end = span.end_idx().min(bytes.len());
    let Some(slice) = bytes.get(span.start_idx()..end) else {
        return Ok(None);
    };
    serde_json::from_slice(slice)
        .map(Some)
        .map_err(|source| ScanError::InvalidFragment { source })
}

/// Large candidate set: extract only the predicate's leaf values with auxiliary scans
/// and parse just the candidates that pass.
fn finish_by_leaf_scans(
    plan: &PushdownPlan,
    json_text: &str,
    input: &BorrowedBytes<'_>,
    candidate_spans: Vec<MatchSpan>,
) -> Result<Vec<Value>, ScanError> {
    let leaf_matches = scan_leaves(plan, input)?;
    let bytes = json_text.as_bytes();
    let mut cursors = vec![0_usize; plan.leaves.len()];
    let mut out = Vec::new();
    let mut prev_end = 0_usize;
    for span in candidate_spans {
        // The cursor alignment in `assemble_synthetic` is sound only for disjoint,
        // document-ordered spans. On valid JSON the child-only prefix guarantees
        // that; on malformed input the engine's spans are undefined and can overlap
        // or regress (found by the differential fuzzer), and the monotone cursors
        // cannot rewind — skip such spans instead. Results on malformed input are
        // documented as undefined, and an engine regression on *valid* JSON would
        // surface as a scan/DOM divergence in the differential harnesses.
        if span.start_idx() < prev_end {
            continue;
        }
        prev_end = span.end_idx();
        let synthetic = assemble_synthetic(plan, &leaf_matches, &mut cursors, &span)?;
        if !crate::eval::eval_logical(plan.predicate.expr(), &synthetic, &synthetic) {
            continue;
        }
        let Some(candidate) = parse_candidate(bytes, &span)? else {
            continue;
        };
        emit_through_residual(&plan.residual, candidate, &mut out);
    }
    Ok(out)
}

/// Runs every auxiliary leaf engine over the whole input, collecting each path's
/// values across all candidates in document order.
fn scan_leaves(
    plan: &PushdownPlan,
    input: &BorrowedBytes<'_>,
) -> Result<Vec<Vec<Match>>, ScanError> {
    let mut leaf_matches: Vec<Vec<Match>> = Vec::with_capacity(plan.leaves.len());
    for leaf in plan.leaves.iter() {
        let mut sink: Vec<Match> = Vec::new();
        rsonpath_boundary::collect_matches(&leaf.engine, input, &mut sink)?;
        leaf_matches.push(sink);
    }
    Ok(leaf_matches)
}

/// Builds one candidate's synthetic fragment: for each leaf, consumes the matches
/// whose start falls inside the candidate's span (cursors only ever advance — spans
/// and leaf matches are both in document order) and places the last such value at the
/// leaf's path. A leaf with no match in the span is simply absent, which evaluates as
/// RFC 9535 "Nothing". `leaf_matches` and `cursors` are indexed in lockstep with
/// `plan.leaves`.
fn assemble_synthetic(
    plan: &PushdownPlan,
    leaf_matches: &[Vec<Match>],
    cursors: &mut [usize],
    span: &MatchSpan,
) -> Result<Value, ScanError> {
    let mut synthetic = serde_json::Map::new();
    for ((leaf, matches_list), cursor) in
        plan.leaves.iter().zip(leaf_matches).zip(cursors.iter_mut())
    {
        let mut found = None;
        while let Some(matched) = matches_list.get(*cursor) {
            let start = matched.span().start_idx();
            if start < span.start_idx() {
                // A leaf outside any candidate cannot occur by construction;
                // skip defensively rather than misassign it.
                *cursor += 1;
                continue;
            }
            if start >= span.end_idx() {
                break;
            }
            // Several matches inside one span means duplicate member names: the last
            // wins here, though the engine reports only the first — see the caveat
            // on `ScanQuery::query_values`.
            found = Some(
                serde_json::from_slice::<Value>(matched.bytes())
                    .map_err(|source| ScanError::InvalidFragment { source })?,
            );
            *cursor += 1;
        }
        if let Some(value) = found {
            insert_leaf(&mut synthetic, &leaf.path, value);
        }
    }
    Ok(Value::Object(synthetic))
}

/// Places a leaf value at its member-name path in the synthetic fragment, creating
/// intermediate objects as needed. Leaves arrive in the plan's lexicographic path
/// order — a path before any extension of itself — so an overlapping deeper leaf
/// lands inside its parent's already-inserted object. A non-object intermediate
/// cannot occur for documents without duplicated member names (a deeper leaf cannot
/// byte-match through a scalar); with duplicates it can — the parent leaf may hold a
/// scalar first occurrence while the deeper leaf matched through a later duplicate —
/// and the deeper leaf is dropped, inside the documented no-consistency zone for
/// duplicated keys. A loop rather than recursion on purpose: path length is
/// query-controlled, and iteration keeps hostile queries from turning placement into
/// deep recursion.
fn insert_leaf(target: &mut serde_json::Map<String, Value>, path: &[String], value: Value) {
    let Some((leaf_name, parents)) = path.split_last() else {
        return;
    };
    let mut current = target;
    for name in parents {
        let entry = current
            .entry(name.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        let Value::Object(inner) = entry else {
            return;
        };
        current = inner;
    }
    current.insert(leaf_name.clone(), value);
}

/// A [`Sink`] that aborts the engine run once cumulative fragment bytes pass `budget`.
/// The abort travels as an engine error; `exceeded` is the authoritative signal that
/// the budget tripped, and [`run_scan_within_budget`] consults it before the engine's
/// own result so a truncated match set can never be evaluated.
struct BudgetedSink {
    matches: Vec<Match>,
    budget: usize,
    spent: usize,
    exceeded: bool,
}

impl BudgetedSink {
    /// A sink whose budget is the input's own length. Only overlapping (self-nested)
    /// matches can accumulate more fragment bytes than the document holds, so this
    /// trips precisely on the overlap pathology and never on a flat document, whose
    /// disjoint fragments sum to at most the input length.
    const fn overlap_tripwire(input_len: usize) -> Self {
        Self {
            matches: Vec::new(),
            budget: input_len,
            spent: 0,
            exceeded: false,
        }
    }
}

impl Sink<Match> for BudgetedSink {
    type Error = BudgetExceeded;

    fn add_match(&mut self, data: Match) -> Result<(), BudgetExceeded> {
        // Saturating: overflow would need ~2^64 cumulative bytes, but a wrapped
        // counter would disable the budget on exactly the inputs it exists for.
        self.spent = self.spent.saturating_add(data.bytes().len());
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
        /// The underlying engine failure, boxed so the engine's error type stays out
        /// of this crate's public API (its semver is not ours).
        source: Box<dyn std::error::Error + Send + Sync>,
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
            Self::Engine { source } => {
                write!(f, "the byte-scanning engine failed: {source}")
            }
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidJson { source } | Self::InvalidFragment { source } => Some(source),
            Self::Engine { source } => Some(source.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BudgetOutcome, Plan, PushdownFinish, ScanError, ScanMode, ScanQuery, choose_finish,
        run_scan_within_budget,
    };
    use crate::JsonPath;
    use serde_json::{Value, json};

    /// Scan-path values and DOM-path values for the same query over the same text.
    /// Forces [`ScanMode::AlwaysScan`]: these behavioral tests pin the byte pipeline itself,
    /// and the tiny documents here would otherwise trip the adaptive budget and
    /// silently exercise the DOM path instead. Asserts the byte engine actually runs —
    /// a splitter regression demoting the shape to DOM would otherwise make the
    /// comparison trivially true; the deliberate DOM-plan cases use
    /// [`both_expecting`] with `false`.
    fn both(query: &str, text: &str) -> (Vec<Value>, Vec<Value>) {
        both_expecting(query, text, true)
    }

    /// [`both`], with an explicit expectation of whether the query byte-scans.
    fn both_expecting(query: &str, text: &str, expect_scan: bool) -> (Vec<Value>, Vec<Value>) {
        let scan = ScanQuery::parse(query)
            .expect("test query must compile")
            .with_mode(ScanMode::AlwaysScan);
        assert_eq!(
            scan.uses_scan(),
            expect_scan,
            "`{query}`: expected uses_scan() == {expect_scan}"
        );
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
        let (scanned, dom) = both_expecting(r#"$['a"b']"#, text, false);
        assert_eq!(
            scanned, dom,
            "a key containing a double quote resolves via DOM"
        );
        assert_eq!(scanned, [json!(1)], "the quoted key selects its value");

        let (scanned, dom) = both_expecting(r"$['c\\d']", text, false);
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
            .with_mode(ScanMode::AlwaysScan);
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
        // Degenerate shapes: truncated JSON, empty input, whitespace-only, and
        // BOM-prefixed text. DOM mode must report each as `InvalidJson`; scan mode's
        // contract is undefined results with guaranteed panic-free termination —
        // completing the loop IS the property under test.
        let dom_mode = ScanQuery::parse("$").expect("`$` compiles");
        assert!(!dom_mode.uses_scan(), "`$` runs in DOM-fallback mode");
        let scan_mode = ScanQuery::parse("$.a[*]")
            .expect("`$.a[*]` compiles")
            .with_mode(ScanMode::AlwaysScan);
        assert!(scan_mode.uses_scan(), "`$.a[*]` runs in scan mode");
        for malformed in [r#"{"a": [1, 2"#, "", " ", "\u{feff}{\"a\": [1]}"] {
            assert!(
                matches!(
                    dom_mode.query_values(malformed),
                    Err(ScanError::InvalidJson { .. })
                ),
                "DOM mode reports {malformed:?} as InvalidJson"
            );
            drop(scan_mode.query_values(malformed));
        }

        // Malformed input the byte engine itself detects surfaces as the `Engine`
        // variant (verified: a lone closing bracket trips its depth tracking).
        assert!(
            matches!(scan_mode.query_values("]"), Err(ScanError::Engine { .. })),
            "engine-detected malformed input maps to ScanError::Engine"
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
            .with_mode(ScanMode::AlwaysScan);
        assert!(query.uses_scan(), "an index-only prefix runs in scan mode");
        assert!(
            matches!(
                query.query_values(&text),
                Err(ScanError::InvalidFragment { .. })
            ),
            "the ~298-deep fragment exceeds serde_json's recursion limit (InvalidFragment)"
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
                .with_mode(ScanMode::AlwaysScan)
                .query_values(text)
                .expect("forced-scan evaluation must succeed on valid JSON");
            let forced_dom = base
                .with_mode(ScanMode::NeverScan)
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
        // `$[*]` selects every root child: fragments cover the whole document for any
        // input, so adaptive mode must not even attempt the byte engine.
        let query = ScanQuery::parse("$[*]").expect("`$[*]` compiles");
        assert!(
            !query.uses_scan(),
            "adaptive: `$[*]` is statically non-selective, so it goes DOM"
        );
        assert!(
            query.clone().with_mode(ScanMode::AlwaysScan).uses_scan(),
            "forcing ScanMode::AlwaysScan overrides the static routing"
        );
        assert!(
            !query.clone().with_mode(ScanMode::NeverScan).uses_scan(),
            "ScanMode::NeverScan never scans"
        );
        assert_eq!(
            query
                .query_values(r#"[{"x": 1}, 2]"#)
                .expect("evaluation must succeed on valid JSON"),
            [json!({"x": 1}), json!(2)],
            "the DOM route still answers the query correctly"
        );
    }

    #[test]
    fn root_level_filters_push_down_instead_of_dom_routing() {
        // `$[?P]` used to be statically whole-document; with pushdown only the leaf
        // values and the passing candidates are ever touched, so it scans again.
        let query = ScanQuery::parse("$[?@.x]").expect("`$[?@.x]` compiles");
        assert!(
            query.uses_scan(),
            "a pushable root-level filter takes the byte engine via pushdown"
        );
        assert_eq!(
            query
                .query_values(r#"[{"x": 1}, {"y": 2}, {"x": 3}]"#)
                .expect("evaluation must succeed on valid JSON"),
            [json!({"x": 1}), json!({"x": 3})],
            "pushdown answers the root-level filter correctly"
        );
    }

    #[test]
    fn pushdown_agrees_with_dom_across_predicate_shapes() {
        let text = r#"{"items": [
            {"n": 1, "tag": "keep", "meta": {"depth": 2}},
            {"n": 5, "tag": "drop"},
            {"n": 9, "meta": {"depth": 1}},
            {"tag": "keep", "meta": {"depth": 9}}
        ]}"#;
        for query in [
            "$.items[?@.n > 2]",
            "$.items[?@.n]",
            "$.items[?!@.n]",
            r#"$.items[?@.tag == "keep"]"#,
            "$.items[?@.meta.depth > 1]",
            r#"$.items[?@.n < 6 && @.tag == "keep"]"#,
            r#"$.items[?@.tag == "keep" || @.meta.depth > 1]"#,
            "$.items[?@.n > 2].n",
        ] {
            let scan = ScanQuery::parse(query).expect("test query must compile");
            assert!(scan.uses_scan(), "`{query}` must push down");
            let (scanned, dom) = both(query, text);
            assert_eq!(scanned, dom, "`{query}`: pushdown and DOM agree");
            assert!(
                !scanned.is_empty(),
                "`{query}` selects something (non-vacuous test)"
            );
        }
    }

    #[test]
    fn engine_panic_on_malformed_input_is_absorbed() {
        // Found by the differential fuzzer, then minimized: rsonpath 0.10 panics
        // internally on this input (a slice-index panic in its match-writing path).
        // The boundary must absorb the panic into `ScanError::Engine` so the
        // documented no-panic contract holds. Bytes kept verbatim — the shape that
        // trips the engine is not otherwise reproducible.
        const REPRO: &[u8] = &[
            123, 91, 34, 97, 34, 52, 91, 50, 52, 52, 50, 58, 7, 91, 97, 13, 93, 61, 58, 9, 58, 61,
            13, 26, 9, 11, 123, 91, 10, 10, 61, 58, 9, 58, 61, 9, 58, 61, 13, 10, 61, 9, 58, 61,
            13, 93, 61, 58, 9, 13, 74, 93, 61, 58, 9, 61, 13, 13, 13, 58, 74, 93, 61, 58, 9, 58,
            13, 13,
        ];
        let text = core::str::from_utf8(REPRO).expect("repro bytes are ASCII");
        for query in ["$.a.b", "$..a", "$.a[*].b", "$[?@.x]", "$.a[?@.n > 3]"] {
            let compiled = ScanQuery::parse(query).expect("test query must compile");
            for mode in [
                ScanMode::AlwaysScan,
                ScanMode::Adaptive,
                ScanMode::NeverScan,
            ] {
                drop(compiled.clone().with_mode(mode).query_values(text));
            }
        }
    }

    #[test]
    fn malformed_input_with_disordered_spans_terminates() {
        // Found by the differential fuzzer (`just fuzz`): on this malformed input
        // the engine emits overlapping/out-of-order candidate spans, which the
        // pushdown finishes must skip — never panic over.
        for query in ["$[?@.x]", "$.a[?@.n > 3]", "$.a[?@.b && @.b.k]"] {
            let compiled = ScanQuery::parse(query).expect("test query must compile");
            for mode in [
                ScanMode::AlwaysScan,
                ScanMode::Adaptive,
                ScanMode::NeverScan,
            ] {
                drop(compiled.clone().with_mode(mode).query_values("[S[}}"));
            }
        }
    }

    #[test]
    fn from_str_delegates_to_parse() {
        let query: ScanQuery = "$.a.b".parse().expect("`$.a.b` compiles via FromStr");
        assert!(query.uses_scan(), "the parsed query plans a byte scan");
        assert!(
            "$[".parse::<ScanQuery>().is_err(),
            "FromStr rejects invalid queries exactly like parse()"
        );
    }

    #[test]
    fn pushdown_finish_boundary_follows_the_cost_ratio() {
        assert!(
            matches!(choose_finish(1, 32, 1), PushdownFinish::ParseCandidates),
            "candidate bytes at exactly input×leaves/ratio parse directly"
        );
        assert!(
            matches!(choose_finish(2, 32, 1), PushdownFinish::LeafScans),
            "candidate bytes past the boundary take the leaf scans"
        );
        assert!(
            matches!(choose_finish(2, 32, 2), PushdownFinish::ParseCandidates),
            "each additional leaf raises the cost of the leaf-scan finish"
        );
        assert!(
            matches!(choose_finish(0, 0, 0), PushdownFinish::ParseCandidates),
            "an empty input with no leaves degenerates to the direct parse"
        );
    }

    #[test]
    fn pushdown_direct_and_leaf_scan_finishes_agree() {
        // Same query, two documents: in the padded one the candidates are a sliver of
        // the input (the span pass chooses the direct parse — no leaf scans); in the
        // bare one they are essentially the whole input (leaf scans + synthetic
        // fragments). Both must match DOM.
        let items = r#"[{"n": 1}, {"n": 5}, {"n": 9}]"#;
        let padded = format!(r#"{{"pad": "{}", "items": {items}}}"#, "x".repeat(4096));
        let bare = format!(r#"{{"items": {items}}}"#);
        for text in [padded.as_str(), bare.as_str()] {
            let (scanned, dom) = both("$.items[?@.n > 2]", text);
            assert_eq!(scanned, dom, "both pushdown finishes agree with DOM");
            assert_eq!(scanned.len(), 2, "two items pass the predicate");
        }
    }

    #[test]
    fn pushdown_duplicate_member_verdict_depends_on_the_finish() {
        // Duplicate member names are "unpredictable behavior" per RFC 8259, and the
        // two pushdown finishes genuinely diverge (verified against rsonpath 0.10):
        // the leaf-scan finish judges the engine's FIRST occurrence, while the
        // direct-parse finish judges serde's LAST (agreeing with DOM). Which finish
        // runs depends on candidate bytes vs input size, so both are pinned here so
        // a change in either engine surfaces loudly; the caveat is documented on
        // `query_values`.
        let items = r#"[{"x": 1, "x": 9}, {"x": 2}]"#;
        let query = "$.a[?@.x > 5]";

        // Bare document: candidates span most of the input, so the leaf-scan finish
        // runs and judges the first occurrence (1 > 5 is false).
        let bare = format!(r#"{{"a": {items}}}"#);
        let scanned = ScanQuery::parse(query)
            .expect("test query must compile")
            .with_mode(ScanMode::AlwaysScan)
            .query_values(&bare)
            .expect("scan evaluation must succeed");
        assert!(
            scanned.is_empty(),
            "the leaf-scan finish judges the first duplicate (1 > 5 is false)"
        );

        // Padded document: candidates are a sliver of the input, so the direct-parse
        // finish runs, judges the real fragment (serde last-wins), and agrees with DOM.
        let padded = format!(r#"{{"pad": "{}", "a": {items}}}"#, "y".repeat(4096));
        let scanned = ScanQuery::parse(query)
            .expect("test query must compile")
            .with_mode(ScanMode::AlwaysScan)
            .query_values(&padded)
            .expect("scan evaluation must succeed");
        assert_eq!(
            scanned,
            [json!({"x": 9})],
            "the direct-parse finish judges serde's last-wins value, agreeing with DOM"
        );

        let document: Value = serde_json::from_str(&bare).expect("test document must parse");
        let dom: Vec<Value> = JsonPath::parse(query)
            .expect("test query must compile")
            .query_values(&document)
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(
            dom,
            [json!({"x": 9})],
            "DOM judges serde's last-wins value (9 > 5 is true)"
        );
    }

    #[cfg(feature = "regex")]
    #[test]
    fn pushdown_evaluates_regex_functions_on_extracted_leaves() {
        let text = r#"{"books": [
            {"title": "Rust in Action"},
            {"title": "The C Programming Language"},
            {"title": "Rust for Rustaceans"}
        ]}"#;
        let query = r#"$.books[?search(@.title, "Rust")].title"#;
        let scan = ScanQuery::parse(query).expect("test query must compile");
        assert!(scan.uses_scan(), "a literal-pattern search pushes down");
        let (scanned, dom) = both(query, text);
        assert_eq!(
            scanned, dom,
            "search() over an extracted string leaf agrees"
        );
        assert_eq!(scanned.len(), 2, "both Rust titles match");
    }

    #[test]
    fn adaptive_budget_trips_only_on_overlapping_fragments() {
        // Directly observes the budget decision (`Exceeded` vs `Done`) — the
        // end-to-end fallback test below cannot: its results are identical whether
        // or not the budget actually tripped.
        let query = ScanQuery::parse("$..a").expect("test query must compile");
        assert!(
            matches!(query.plan, Plan::Scan(_)),
            "`$..a` splits to a plain fragment-scan plan"
        );
        let Plan::Scan(plan) = &query.plan else {
            return;
        };
        let overlapping =
            r#"{"a": {"pad": "xxxxxxxxxxxxxxxx", "a": {"pad": "yyyyyyyyyyyyyyyy", "b": 1}}}"#;
        assert!(
            matches!(
                run_scan_within_budget(plan, overlapping),
                BudgetOutcome::Exceeded
            ),
            "self-nested `a` fragments overlap: cumulative bytes exceed the input and trip the budget"
        );
        let flat = r#"{"x": {"a": {"b": 1}}, "y": {"a": {"c": 2}}}"#;
        assert!(
            matches!(
                run_scan_within_budget(plan, flat),
                BudgetOutcome::Done(Ok(_))
            ),
            "disjoint fragments sum to at most the input length and never trip the budget"
        );
    }

    #[test]
    fn pushdown_with_nested_leaf_paths_matches_dom() {
        // One leaf path extends another (`b` and `b.c`): the exact shape
        // `LexOrderedLeaves` protects — the parent's extracted object must be placed
        // in the synthetic fragment before the deeper leaf is nested inside it.
        let text = r#"{"a": [
            {"k": "both", "b": {"c": 1}},
            {"k": "scalar-parent", "b": 2},
            {"k": "missing-parent"},
            {"k": "deep", "b": {"c": 9}}
        ]}"#;
        for query in [
            "$.a[?@.b && @.b.c]",
            "$.a[?@.b.c > 1]",
            "$.a[?@.b && !@.b.c].k",
        ] {
            let scan = ScanQuery::parse(query).expect("test query must compile");
            assert!(scan.uses_scan(), "`{query}` must push down");
            let (scanned, dom) = both(query, text);
            assert_eq!(
                scanned, dom,
                "`{query}`: nested-leaf pushdown agrees with DOM"
            );
            assert!(
                !scanned.is_empty(),
                "`{query}` selects something (non-vacuous test)"
            );
        }
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
            .with_mode(ScanMode::NeverScan)
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
