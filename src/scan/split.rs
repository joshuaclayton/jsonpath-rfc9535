//! Splits a compiled query into a byte-scannable **prefix** and a DOM-evaluated
//! **residual**.
//!
//! The prefix is the longest leading run of segments the rsonpath byte engine can
//! express: single-selector child/descendant segments of names, wildcards, and
//! non-negative indexes. Everything from the first inexpressible segment onward is the
//! residual, evaluated per extracted fragment by the crate's own evaluator.
//!
//! One cut shape gets special treatment: when the first residual segment is a child
//! segment holding exactly one filter (`…[?P]…`), the filter's iteration moves into the
//! prefix as a `[*]` and `P` becomes a per-fragment predicate — RFC 9535 defines `[?P]`
//! as testing each child the way `[*]` would select it.
//!
//! A residual is only sound if it never references `$`: fragments are evaluated with
//! themselves as the root, so an absolute sub-query (`[?@.price < $.max]`) would resolve
//! against the wrong document. The check walks every corner of the IR that can embed a
//! sub-query, and the [`RootFreePredicate`]/[`RootFreeSegments`] wrappers make passing
//! it the only way into a plan's predicate/residual slots — any `$` reference falls
//! back to [`Plan::Dom`].
//!
//! [`RsonpathEngine::compile_query`] is the final oracle: whatever the eligibility rules
//! missed (engine limits, unsupported shapes) surfaces as a `CompilerError` and demotes
//! the plan to [`Plan::Dom`] — construction never fails.

use crate::ast;
#[cfg(feature = "regex")]
use crate::compiled::Pattern;
use crate::compiled::{
    Comparable, ExistenceTest, Function, LogicalExpr, Query, Segment, Selector, ValueArg,
};
use rsonpath::engine::{Compiler, RsonpathEngine};

/// How a [`ScanQuery`](crate::ScanQuery) executes.
#[derive(Debug, Clone)]
pub enum Plan {
    /// The prefix runs on the byte engine; each extracted fragment is optionally
    /// filtered by the predicate, then walked with the residual segments. Boxed:
    /// the engine's automaton dwarfs the `Dom` variant.
    Scan(Box<ScanPlan>),
    /// The filter is decided from auxiliary scans over its singular leaf paths;
    /// only candidates that pass are ever extracted and parsed.
    Pushdown(Box<PushdownPlan>),
    /// Unsplittable query: parse the whole document and evaluate normally.
    Dom,
}

/// The byte-scanning execution plan: prefix automaton, per-fragment predicate, and
/// residual segments.
#[derive(Debug, Clone)]
pub struct ScanPlan {
    /// Compiled rsonpath automaton for the prefix (plus a trailing `[*]` when
    /// `predicate` is present).
    pub engine: RsonpathEngine,
    /// The filter cut out of the first residual segment, applied to each fragment.
    pub predicate: Option<RootFreePredicate>,
    /// Segments evaluated over each surviving fragment (fragment = start = root).
    pub residual: RootFreeSegments,
    /// The rendered prefix selects every child of the root (`$[*]`) or every node
    /// (`$..*`), so its fragments cover essentially the whole document *for any
    /// document* — byte-scanning cannot beat one DOM parse. Statically known at
    /// construction; [`ScanMode::Adaptive`](crate::ScanMode) routes these straight
    /// to DOM without touching the input.
    pub whole_document: bool,
}

/// A filter-pushdown execution plan: instead of extracting and parsing every
/// candidate to decide the filter, the predicate's *leaf values* are pulled out with
/// auxiliary byte scans, the filter is decided on a synthetic fragment assembled from
/// them, and only passing candidates are extracted at all. Parse cost scales with the
/// filter's pass rate rather than the candidate set's size.
#[derive(Debug, Clone)]
pub struct PushdownPlan {
    /// Engine yielding each candidate's span: the prefix plus the filter's `[*]`.
    /// Spans only — candidate bytes are never copied unless the candidate passes.
    pub candidates: RsonpathEngine,
    /// One auxiliary engine per distinct singular path the predicate references.
    pub leaves: LexOrderedLeaves,
    /// The filter, evaluated per candidate against the synthetic fragment.
    pub predicate: RootFreePredicate,
    /// Segments evaluated over each passing candidate (candidate = start = root).
    pub residual: RootFreeSegments,
}

/// One predicate leaf: the `@`-relative member-name chain and the engine that
/// extracts its values (`prefix[*].<path>`).
#[derive(Debug, Clone)]
pub struct LeafPlan {
    /// Engine for the leaf's values across all candidates, in document order.
    pub engine: RsonpathEngine,
    /// The member-name steps, used to place extracted values in the synthetic
    /// fragment (see [`LexOrderedLeaves`] for the ordering `insert_leaf` relies on).
    pub path: Vec<String>,
}

/// A pushdown plan's leaf scans, held in lexicographic path order — a path before any
/// extension of itself. That is the placement order `insert_leaf` relies on to nest
/// deeper leaves inside their parents' already-placed objects; a reordering (say, by
/// selectivity) would silently drop nested leaves from synthetic fragments.
/// [`sorted`](Self::sorted) is the only constructor.
#[derive(Debug, Clone)]
pub struct LexOrderedLeaves(Vec<LeafPlan>);

impl LexOrderedLeaves {
    /// Wraps `leaves`, restoring the lexicographic path order if construction
    /// disturbed it.
    fn sorted(mut leaves: Vec<LeafPlan>) -> Self {
        leaves.sort_by(|a, b| a.path.cmp(&b.path));
        Self(leaves)
    }

    /// The number of leaf scans the plan runs.
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// The leaves, in their guaranteed order.
    pub fn iter(&self) -> std::slice::Iter<'_, LeafPlan> {
        self.0.iter()
    }
}

/// A filter expression verified at construction to reference no `$`-rooted sub-query,
/// however deeply nested.
///
/// Fragments are evaluated as their own root, so an absolute sub-query would resolve
/// against the wrong document; [`checked`](Self::checked) refuses such expressions and
/// is the only constructor — holding one of these *is* the soundness proof.
#[derive(Debug, Clone)]
pub struct RootFreePredicate(LogicalExpr);

impl RootFreePredicate {
    /// Verifies and wraps `expr`; `None` when it references `$` anywhere.
    fn checked(expr: &LogicalExpr) -> Option<Self> {
        (!expr_has_root_query(expr)).then(|| Self(expr.clone()))
    }

    /// The verified expression, for per-fragment evaluation.
    pub const fn expr(&self) -> &LogicalExpr {
        &self.0
    }
}

/// Residual segments verified at construction to embed no `$`-rooted sub-query in any
/// filter, however deeply nested — the only segment shape that may soundly walk a
/// fragment acting as its own root. [`checked`](Self::checked) is the only
/// constructor.
#[derive(Debug, Clone)]
pub struct RootFreeSegments(Vec<Segment>);

impl RootFreeSegments {
    /// Verifies and wraps `segments`; `None` when any embedded filter references `$`.
    fn checked(segments: &[Segment]) -> Option<Self> {
        (!segments_have_root(segments)).then(|| Self(segments.to_vec()))
    }

    /// Whether there is nothing to walk (fragments are emitted as-is).
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The verified segments, for the evaluator's walk.
    pub fn segments(&self) -> &[Segment] {
        &self.0
    }
}

/// Auxiliary scans are cheap (~5% of a parse each) but not free; a predicate
/// consulting more paths than this falls back to the plain fragment scan.
const MAX_PUSHDOWN_LEAVES: usize = 8;

/// Splits `query` into a byte-scannable prefix and a residual plan, falling back to
/// [`Plan::Dom`] whenever the split would be unsound or worthless.
pub fn split(query: &Query) -> Plan {
    let segments = &query.segments;
    if segments.is_empty() {
        // `$` alone: the only fragment would be the whole document — scanning adds
        // nothing over parsing it directly.
        return Plan::Dom;
    }
    let cut = segments
        .iter()
        .position(|segment| !prefix_eligible(segment))
        .unwrap_or(segments.len());
    let filter_cut = filter_at_cut(segments.get(cut));
    let residual_start = if filter_cut.is_some() { cut + 1 } else { cut };

    // Soundness: fragments are their own root, so nothing past the cut may mention
    // `$`. The checked constructors are the only way into the plans' slots; a `$`
    // reference anywhere past the cut demotes to DOM.
    let Some(residual) =
        RootFreeSegments::checked(segments.get(residual_start..).unwrap_or_default())
    else {
        return Plan::Dom;
    };
    let predicate = match filter_cut.map(RootFreePredicate::checked) {
        Some(None) => return Plan::Dom,
        Some(Some(predicate)) => Some(predicate),
        None => None,
    };
    let Some(prefix) = segments.get(..cut) else {
        return Plan::Dom;
    };

    // A pushable filter beats the plain fragment scan: leaf scans decide the
    // predicate and only passing candidates are extracted, so even a filter at the
    // root (`$[?P]`) is worth scanning this way.
    if let Some(predicate) = &predicate
        && let Some(plan) = build_pushdown(prefix, predicate, &residual)
    {
        return Plan::Pushdown(Box::new(plan));
    }
    if prefix.is_empty() && predicate.is_none() {
        // The prefix would be a bare `$`: one whole-document fragment, i.e. a DOM parse
        // with extra steps. (An empty prefix *with* a predicate stays: `$[?P]` → `$[*]` + P.)
        return Plan::Dom;
    }
    let whole_document = statically_covers_whole_document(prefix, predicate.is_some());
    build_scan(prefix, predicate, residual, whole_document)
}

/// Whether the rendered prefix selects essentially the whole document *for any
/// document*. Exactly two shapes do: a predicate cut with an empty prefix (`$[?P]…`
/// renders to `$[*]` — fragments are every child of the root), and a lone bare
/// wildcard step (`$[*]` / `$..*`), which does the same or worse. Deeper wildcards
/// (`$.a[*]`) are data-dependent and stay unmarked.
fn statically_covers_whole_document(prefix: &[Segment], has_predicate: bool) -> bool {
    if has_predicate {
        prefix.is_empty()
    } else {
        matches!(prefix, [segment] if is_bare_wildcard(segment))
    }
}

/// The filter a cut segment contributes as a per-fragment predicate: present exactly
/// when the segment is a child segment holding a single filter selector (the prefix
/// then gains the `[*]` the filter iterates as).
fn filter_at_cut(segment: Option<&Segment>) -> Option<&LogicalExpr> {
    match segment {
        Some(Segment::Child(selectors)) => match selectors.as_slice() {
            [Selector::Filter(expr)] => Some(expr),
            _ => None,
        },
        Some(Segment::Descendant(_)) | None => None,
    }
}

/// Compiles the plain fragment-scan plan: render the prefix (plus the `[*]` a
/// predicate iterates over) and let the engine's compiler have the final word on
/// expressibility — anything the eligibility rules let through that it cannot compile
/// (engine limits, exotic shapes) demotes to DOM.
fn build_scan(
    prefix: &[Segment],
    predicate: Option<RootFreePredicate>,
    residual: RootFreeSegments,
    whole_document: bool,
) -> Plan {
    let Some(rendered) = render_prefix(prefix, predicate.is_some()) else {
        return Plan::Dom;
    };
    RsonpathEngine::compile_query(&rendered).map_or(Plan::Dom, |engine| {
        Plan::Scan(Box::new(ScanPlan {
            engine,
            predicate,
            residual,
            whole_document,
        }))
    })
}

/// Builds a pushdown plan when the shape allows deciding the filter from auxiliary
/// scans alone. Requirements:
///
/// * **Child-only prefix** — descendant steps could nest one candidate inside
///   another, making span-containment alignment of leaf values ambiguous.
/// * **Pushable predicate** — every query embedded in the filter is a singular,
///   `@`-rooted chain of scannable member names (see [`collect_pushdown_paths`]).
///
/// `None` falls back to the plain fragment scan.
fn build_pushdown(
    prefix: &[Segment],
    predicate: &RootFreePredicate,
    residual: &RootFreeSegments,
) -> Option<PushdownPlan> {
    if prefix
        .iter()
        .any(|segment| matches!(segment, Segment::Descendant(_)))
    {
        return None;
    }
    let mut paths = Vec::new();
    if !collect_pushdown_paths(predicate.expr(), &mut paths) {
        return None;
    }
    // Lexicographic order puts a path before its own extensions — the placement
    // order `insert_leaf` relies on to nest deeper leaves inside already-placed
    // parents. `dedup` then folds repeated references into a single scan.
    paths.sort();
    paths.dedup();
    if paths.len() > MAX_PUSHDOWN_LEAVES {
        return None;
    }
    let candidates = RsonpathEngine::compile_query(&render_prefix(prefix, true)?).ok()?;
    let leaves = paths
        .into_iter()
        .map(|path| {
            let engine = RsonpathEngine::compile_query(&render_leaf(prefix, &path)?).ok()?;
            Some(LeafPlan { engine, path })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(PushdownPlan {
        candidates,
        leaves: LexOrderedLeaves::sorted(leaves),
        predicate: predicate.clone(),
        residual: residual.clone(),
    })
}

// The pushability walk: mirrors the `$`-reference check's shape, but additionally
// collects every singular path the predicate consults. A filter is pushable iff every
// embedded query is a singular `@`-rooted chain of scannable member names — anything
// else (general sub-queries, `count`/`value`, index steps, bare `@`) needs the real
// fragment and falls back to the plain scan.

fn collect_pushdown_paths(expr: &LogicalExpr, paths: &mut Vec<Vec<String>>) -> bool {
    match expr {
        LogicalExpr::Or(left, right) | LogicalExpr::And(left, right) => {
            collect_pushdown_paths(left, paths) && collect_pushdown_paths(right, paths)
        }
        LogicalExpr::Not(inner) => collect_pushdown_paths(inner, paths),
        LogicalExpr::Comparison(comparison) => {
            comparable_paths(&comparison.left, paths) && comparable_paths(&comparison.right, paths)
        }
        // The literal side consults no document path, so pushability is the query's alone.
        LogicalExpr::SingularLiteral { query, .. } => singular_leaf_path(query, paths),
        LogicalExpr::Existence(test) => match test {
            ExistenceTest::Singular(query) => singular_leaf_path(query, paths),
            ExistenceTest::General(_) => false,
        },
        #[cfg(feature = "regex")]
        LogicalExpr::Test(function) => function_paths(function, paths),
    }
}

fn comparable_paths(comparable: &Comparable, paths: &mut Vec<Vec<String>>) -> bool {
    match comparable {
        Comparable::Literal(_) => true,
        Comparable::Singular(query) => singular_leaf_path(query, paths),
        Comparable::Function(function) => function_paths(function, paths),
    }
}

fn function_paths(function: &Function, paths: &mut Vec<Vec<String>>) -> bool {
    match function {
        Function::Length(arg) => value_arg_paths(arg, paths),
        // `count`/`value` take general (non-singular) sub-queries: not pushable.
        Function::Count(_) | Function::Value(_) => false,
        #[cfg(feature = "regex")]
        Function::Match(arg, pattern) | Function::Search(arg, pattern) => {
            value_arg_paths(arg, paths) && pattern_paths(pattern, paths)
        }
    }
}

#[cfg(feature = "regex")]
fn pattern_paths(pattern: &Pattern, paths: &mut Vec<Vec<String>>) -> bool {
    match pattern {
        Pattern::Plain(_) | Pattern::Substring(_) | Pattern::Literal(_) => true,
        Pattern::Dynamic(arg) => value_arg_paths(arg, paths),
    }
}

fn value_arg_paths(arg: &ValueArg, paths: &mut Vec<Vec<String>>) -> bool {
    match arg {
        ValueArg::Literal(_) => true,
        ValueArg::Singular(query) => singular_leaf_path(query, paths),
        ValueArg::Function(function) => function_paths(function, paths),
    }
}

/// Records a singular query as a leaf path, or rejects pushdown: `$`-rooted queries
/// resolve outside the candidate; index steps cannot be expressed in the synthetic
/// fragment without inventing sparse-array members (false existence); unscannable
/// names cannot be byte-matched; and a bare `@` (empty path) would make the leaf scan
/// re-extract every candidate — exactly what pushdown exists to avoid.
fn singular_leaf_path(query: &ast::SingularQuery, paths: &mut Vec<Vec<String>>) -> bool {
    if matches!(query.root, ast::QueryRoot::Root) || query.segments.is_empty() {
        return false;
    }
    let mut path = Vec::with_capacity(query.segments.len());
    for segment in &query.segments {
        match segment {
            ast::SingularSegment::Name(name) => {
                if !name_scans_verbatim(name) {
                    return false;
                }
                path.push(name.clone());
            }
            ast::SingularSegment::Index(_) => return false,
        }
    }
    paths.push(path);
    true
}

/// A single-wildcard segment (`[*]` or `..*`) — as the *entire* prefix it selects every
/// child of the root / every node.
fn is_bare_wildcard(segment: &Segment) -> bool {
    let (Segment::Child(selectors) | Segment::Descendant(selectors)) = segment;
    matches!(selectors.as_slice(), [Selector::Wildcard])
}

/// Whether `segment` can live in the byte-scanned prefix: a single verbatim-safe name,
/// wildcard, or non-negative index selector. Slices are excluded by policy: the engine
/// compiles only forward, from-start forms (from-end bounds and backward steps are
/// `UnsupportedFeatureError`s), so admitting slices would make eligibility depend on
/// each slice's internals — keeping them all in the residual keeps the rule
/// shape-independent. Negative indexes are excluded because the engine cannot count
/// from the end.
fn prefix_eligible(segment: &Segment) -> bool {
    let (Segment::Child(selectors) | Segment::Descendant(selectors)) = segment;
    let [selector] = selectors.as_slice() else {
        return false;
    };
    match selector {
        Selector::Name(name) => name_scans_verbatim(name),
        Selector::Wildcard => true,
        Selector::Index(index) => index.get() >= 0,
        Selector::Slice(_) | Selector::Filter(_) => false,
    }
}

/// The byte engine compares member names byte-for-byte against their plain (unescaped)
/// form. A name containing a character that JSON *requires* escaping (`"`, `\`, or a
/// control character) can never appear in that plain form in a document, so scanning
/// for it would silently select nothing — such names stay out of the prefix and are
/// resolved by the DOM walk over parsed fragments instead. (Verified empirically
/// against rsonpath 0.10: `$['a"b']` selects nothing even when the document uses the
/// canonical escape.)
fn name_scans_verbatim(name: &str) -> bool {
    name.chars().all(|c| c != '"' && c != '\\' && c >= '\u{20}')
}

/// Renders the prefix segments as an `rsonpath_syntax` query via its builder (no string
/// assembly, so member names need no escaping here). `child_wildcard_tail` appends the
/// `[*]` a predicate cut iterates over. `None` if any segment is unexpressible — the
/// eligibility rules should have prevented that, but this stays total rather than panic.
fn render_prefix(
    prefix: &[Segment],
    child_wildcard_tail: bool,
) -> Option<rsonpath_syntax::JsonPathQuery> {
    let mut builder = prefix_builder(prefix)?;
    if child_wildcard_tail {
        builder.child_wildcard();
    }
    Some(builder.to_query())
}

/// Renders a pushdown leaf query: the prefix, the filter's `[*]`, then the leaf's
/// member-name steps.
fn render_leaf(prefix: &[Segment], path: &[String]) -> Option<rsonpath_syntax::JsonPathQuery> {
    let mut builder = prefix_builder(prefix)?;
    builder.child_wildcard();
    for name in path {
        builder.child_name(name.as_str());
    }
    Some(builder.to_query())
}

/// The shared builder walk over prefix segments.
fn prefix_builder(prefix: &[Segment]) -> Option<rsonpath_syntax::builder::JsonPathQueryBuilder> {
    let mut builder = rsonpath_syntax::builder::JsonPathQueryBuilder::new();
    for segment in prefix {
        let descendant = matches!(segment, Segment::Descendant(_));
        let (Segment::Child(selectors) | Segment::Descendant(selectors)) = segment;
        let [selector] = selectors.as_slice() else {
            return None;
        };
        match selector {
            Selector::Name(name) => {
                if descendant {
                    builder.descendant_name(name.as_str());
                } else {
                    builder.child_name(name.as_str());
                }
            }
            Selector::Wildcard => {
                if descendant {
                    builder.descendant_wildcard();
                } else {
                    builder.child_wildcard();
                }
            }
            Selector::Index(index) => {
                let index = rsonpath_syntax::num::JsonInt::try_from(index.get()).ok()?;
                if descendant {
                    builder.descendant_index(index);
                } else {
                    builder.child_index(index);
                }
            }
            Selector::Slice(_) | Selector::Filter(_) => return None,
        }
    }
    Some(builder)
}

// The `$`-reference check: one function per IR layer that can embed a sub-query. Every
// match is exhaustive on purpose — a new IR variant must fail compilation here so this
// check is revisited rather than silently treating it as `$`-free.

/// Whether any filter anywhere in `segments` embeds a `$`-rooted sub-query.
fn segments_have_root(segments: &[Segment]) -> bool {
    segments.iter().any(|segment| {
        let (Segment::Child(selectors) | Segment::Descendant(selectors)) = segment;
        selectors.iter().any(selector_has_root)
    })
}

fn selector_has_root(selector: &Selector) -> bool {
    match selector {
        Selector::Name(_) | Selector::Wildcard | Selector::Index(_) | Selector::Slice(_) => false,
        Selector::Filter(expr) => expr_has_root_query(expr),
    }
}

/// Whether a filter expression references `$` anywhere, however deeply nested.
fn expr_has_root_query(expr: &LogicalExpr) -> bool {
    match expr {
        LogicalExpr::Or(left, right) | LogicalExpr::And(left, right) => {
            expr_has_root_query(left) || expr_has_root_query(right)
        }
        LogicalExpr::Not(inner) => expr_has_root_query(inner),
        LogicalExpr::Comparison(comparison) => {
            comparable_has_root(&comparison.left) || comparable_has_root(&comparison.right)
        }
        LogicalExpr::SingularLiteral { query, .. } => singular_has_root(query),
        LogicalExpr::Existence(test) => match test {
            ExistenceTest::Singular(query) => singular_has_root(query),
            ExistenceTest::General(query) => filter_query_has_root(query),
        },
        #[cfg(feature = "regex")]
        LogicalExpr::Test(function) => function_has_root(function),
    }
}

fn comparable_has_root(comparable: &Comparable) -> bool {
    match comparable {
        Comparable::Literal(_) => false,
        Comparable::Singular(query) => singular_has_root(query),
        Comparable::Function(function) => function_has_root(function),
    }
}

fn function_has_root(function: &Function) -> bool {
    match function {
        Function::Length(arg) => value_arg_has_root(arg),
        Function::Count(query) | Function::Value(query) => filter_query_has_root(query),
        #[cfg(feature = "regex")]
        Function::Match(arg, pattern) | Function::Search(arg, pattern) => {
            value_arg_has_root(arg) || pattern_has_root(pattern)
        }
    }
}

#[cfg(feature = "regex")]
fn pattern_has_root(pattern: &Pattern) -> bool {
    match pattern {
        // A literal pattern is a pre-compiled regex; there is no query inside.
        Pattern::Plain(_) | Pattern::Substring(_) | Pattern::Literal(_) => false,
        Pattern::Dynamic(arg) => value_arg_has_root(arg),
    }
}

fn value_arg_has_root(arg: &ValueArg) -> bool {
    match arg {
        ValueArg::Literal(_) => false,
        ValueArg::Singular(query) => singular_has_root(query),
        ValueArg::Function(function) => function_has_root(function),
    }
}

/// A singular query's segments are pure name/index steps — only its root can be `$`.
const fn singular_has_root(query: &ast::SingularQuery) -> bool {
    matches!(query.root, ast::QueryRoot::Root)
}

/// The recursion that makes the check watertight: an `@`-rooted sub-query can still
/// nest a `[?$…]` filter deeper in its own segments.
fn filter_query_has_root(query: &crate::compiled::FilterQuery) -> bool {
    matches!(query.root, ast::QueryRoot::Root) || segments_have_root(&query.segments)
}

#[cfg(test)]
mod tests {
    use super::{LeafPlan, LexOrderedLeaves, Plan, split};
    use crate::JsonPath;
    use rsonpath::engine::{Compiler, RsonpathEngine};

    /// The split decision for `query`, reduced to what the tests assert on.
    #[derive(Debug, PartialEq, Eq)]
    enum Kind {
        /// Plain fragment scan: `(has_predicate, residual_len)`.
        Scan(bool, usize),
        /// Filter pushdown: `(leaf_count, residual_len)`.
        Pushdown(usize, usize),
        Dom,
    }

    fn kind(query: &str) -> Kind {
        let path = JsonPath::parse(query).expect("test query must compile");
        match split(path.compiled()) {
            Plan::Scan(plan) => {
                Kind::Scan(plan.predicate.is_some(), plan.residual.segments().len())
            }
            Plan::Pushdown(plan) => {
                Kind::Pushdown(plan.leaves.len(), plan.residual.segments().len())
            }
            Plan::Dom => Kind::Dom,
        }
    }

    /// Back-compat shape for the plain-scan assertions below.
    fn decision(query: &str) -> Option<(bool, usize)> {
        match kind(query) {
            Kind::Scan(predicate, residual) => Some((predicate, residual)),
            Kind::Pushdown(..) | Kind::Dom => None,
        }
    }

    #[test]
    fn structural_queries_scan_in_full() {
        for query in ["$.a.b", "$.a[0].b", "$..a.b", "$.a[*]..b", "$..a[*]"] {
            assert_eq!(
                decision(query),
                Some((false, 0)),
                "`{query}` is fully byte-scannable: no predicate, empty residual"
            );
        }
    }

    #[test]
    fn pushable_filters_take_the_pushdown_plan() {
        assert_eq!(
            kind("$.store.book[?@.price < 10]"),
            Kind::Pushdown(1, 0),
            "a singular-comparison filter pushes down with one leaf scan"
        );
        assert_eq!(
            kind("$.store.book[?@.p].title"),
            Kind::Pushdown(1, 1),
            "segments after the pushed filter stay in the residual"
        );
        assert_eq!(
            kind("$[?@.x]"),
            Kind::Pushdown(1, 0),
            "a root-level filter pushes down: only passing root children are parsed"
        );
        assert_eq!(
            kind("$.a[?@.b][?@.c]"),
            Kind::Pushdown(1, 1),
            "only the first filter pushes down; the second stays residual"
        );
        assert_eq!(
            kind("$.a[?@.b][2]"),
            Kind::Pushdown(1, 1),
            "an index after the filter applies to each passing node, in the residual"
        );
        assert_eq!(
            kind("$.a[?@.b && @.c.d]"),
            Kind::Pushdown(2, 0),
            "each distinct singular path becomes one leaf scan"
        );
        assert_eq!(
            kind("$.a[?@.b && @.b.c]"),
            Kind::Pushdown(2, 0),
            "a leaf path extending another still yields two scans (nested placement)"
        );
        assert_eq!(
            kind("$.a[?@.b < 3 || @.b > 7]"),
            Kind::Pushdown(1, 0),
            "a path referenced twice is scanned once"
        );
    }

    #[test]
    fn unpushable_filters_fall_back_to_the_fragment_scan() {
        assert_eq!(
            kind("$.a[?@.b[*]]"),
            Kind::Scan(true, 0),
            "a general (non-singular) existence test needs the real fragment"
        );
        assert_eq!(
            kind("$..a[?@.x]"),
            Kind::Scan(true, 0),
            "a descendant prefix nests candidates: span alignment would be ambiguous"
        );
        assert_eq!(
            kind("$.a[?@[0] == 1]"),
            Kind::Scan(true, 0),
            "an index leaf cannot be expressed in the synthetic fragment"
        );
        assert_eq!(
            kind("$.a[?@ > 1]"),
            Kind::Scan(true, 0),
            "a bare `@` leaf would re-extract every candidate — no gain over the scan"
        );
        assert_eq!(
            kind("$.a[?count(@.b[*]) == 1]"),
            Kind::Scan(true, 0),
            "count() takes a general sub-query: not pushable"
        );
        assert_eq!(
            kind(r#"$.a[?@['b"c']]"#),
            Kind::Scan(true, 0),
            "an escape-requiring leaf name cannot be byte-matched"
        );
    }

    #[test]
    fn slices_stay_in_the_residual() {
        assert_eq!(
            decision("$.a[1:2]"),
            Some((false, 1)),
            "a slice after a scannable step lands in the residual"
        );
        assert_eq!(
            kind("$[1:2]"),
            Kind::Dom,
            "a root-level slice leaves a bare-`$` prefix: not worth scanning"
        );
    }

    #[test]
    fn descendant_filters_stay_whole_in_the_residual() {
        assert_eq!(
            decision("$.x..[?@.a]"),
            Some((false, 1)),
            "a descendant filter is never converted to a predicate, but scans after a prefix"
        );
        assert_eq!(
            kind("$..[?@.a]"),
            Kind::Dom,
            "a root-level descendant filter leaves a bare-`$` prefix"
        );
    }

    #[test]
    fn root_references_force_dom_evaluation() {
        for query in [
            "$[?@.a == $.b]",
            "$.a[?@.x].b[?$.y]",
            "$.a[?count($..x) > 1]",
            "$.a[?@.b[?$.c]]",
            "$.a[?length($.b) == 1]",
        ] {
            assert_eq!(
                kind(query),
                Kind::Dom,
                "`{query}` references `$` past the cut and must fall back to DOM"
            );
        }
    }

    #[cfg(feature = "regex")]
    #[test]
    fn root_references_inside_regex_functions_force_dom() {
        assert_eq!(
            kind(r"$.a[?match(@.name, $.pattern)]"),
            Kind::Dom,
            "a dynamic pattern computed from `$` must fall back to DOM"
        );
        assert_eq!(
            kind(r#"$.a[?search(@.name, "ada")]"#),
            Kind::Pushdown(1, 0),
            "a literal pattern embeds no query: the string leaf pushes down"
        );
    }

    #[test]
    fn inexpressible_shapes_fall_back_to_dom() {
        for query in ["$", "$[-1]", "$['a','b']", r#"$['a"b']"#] {
            assert_eq!(
                kind(query),
                Kind::Dom,
                "`{query}` has no useful byte-scannable prefix"
            );
        }
    }

    #[test]
    fn negative_index_after_a_prefix_stays_residual() {
        assert_eq!(
            decision("$.a.b[-1]"),
            Some((false, 1)),
            "the from-end index is residual; the name steps still scan"
        );
        assert_eq!(
            decision("$.a[-1].b"),
            Some((false, 2)),
            "everything from the from-end index onward is residual"
        );
    }

    #[test]
    fn escape_requiring_names_stay_in_the_residual() {
        // The engine matches names byte-for-byte in their plain form; a name containing
        // a quote, backslash, or control character can never appear that way in a
        // document, so it must not be byte-scanned.
        assert_eq!(
            decision(r#"$.books['a"b'].title"#),
            Some((false, 2)),
            "the plain prefix scans; the escape-requiring name and its tail are residual"
        );
        assert_eq!(
            kind(r"$['c\\d']"),
            Kind::Dom,
            "an escape-requiring name at the root leaves a bare-`$` prefix"
        );
    }

    #[test]
    fn whole_document_prefixes_are_marked() {
        let whole = |query: &str| {
            let path = JsonPath::parse(query).expect("test query must compile");
            match split(path.compiled()) {
                Plan::Scan(plan) => Some(plan.whole_document),
                Plan::Pushdown(_) | Plan::Dom => None,
            }
        };
        assert_eq!(
            whole("$[?count(@.x[*]) > 0]"),
            Some(true),
            "an unpushable root-level filter renders to `$[*]`: fragments cover the whole document"
        );
        assert_eq!(whole("$[*]"), Some(true), "`$[*]` selects every root child");
        assert_eq!(whole("$..*"), Some(true), "`$..*` selects every node");
        assert_eq!(
            whole("$[*].name"),
            Some(false),
            "a wildcard followed by a name narrows: fragment size is data-dependent"
        );
        assert_eq!(
            whole("$.store.book[?@.p[*]]"),
            Some(false),
            "a named prefix is data-dependent, never statically whole-document"
        );
    }

    #[test]
    fn lex_ordered_leaves_restores_prefix_before_extension_order() {
        // `build_pushdown` happens to pre-sort its paths, so the constructor's own
        // sort is the only guard against a future construction path that does not —
        // pin it with deliberately out-of-order input.
        let engine = |query: &str| {
            RsonpathEngine::compile_query(
                &rsonpath_syntax::parse(query).expect("test query must parse"),
            )
            .expect("test query must compile")
        };
        let path = |names: &[&str]| {
            names
                .iter()
                .map(|&name| name.to_owned())
                .collect::<Vec<_>>()
        };
        let shuffled = vec![
            LeafPlan {
                engine: engine("$.x"),
                path: path(&["b", "c"]),
            },
            LeafPlan {
                engine: engine("$.x"),
                path: path(&["b"]),
            },
            LeafPlan {
                engine: engine("$.x"),
                path: path(&["a"]),
            },
        ];
        let ordered: Vec<Vec<String>> = LexOrderedLeaves::sorted(shuffled)
            .iter()
            .map(|leaf| leaf.path.clone())
            .collect();
        assert_eq!(
            ordered,
            [path(&["a"]), path(&["b"]), path(&["b", "c"])],
            "sorted() puts a path before its extensions regardless of construction order"
        );
    }

    #[test]
    fn multi_selector_segment_after_a_prefix_stays_residual() {
        assert_eq!(
            decision("$.a['x','y']"),
            Some((false, 1)),
            "a selector union is residual; the name step still scans"
        );
    }
}
