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
//! against the wrong document. [`split`] walks every corner of the IR that can embed a
//! sub-query and falls back to [`Plan::Dom`] on any `$` reference.
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
    pub predicate: Option<LogicalExpr>,
    /// Segments evaluated over each surviving fragment (fragment = start = root).
    pub residual: Vec<Segment>,
    /// The rendered prefix selects every child of the root (`$[*]`) or every node
    /// (`$..*`), so its fragments cover essentially the whole document *for any
    /// document* — byte-scanning cannot beat one DOM parse. Statically known at
    /// construction; [`ScanMode::Adaptive`](crate::ScanMode) routes these straight
    /// to DOM without touching the input.
    pub whole_document: bool,
}

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

    // A cut at a child segment holding exactly one filter becomes a per-fragment
    // predicate (the prefix gains the `[*]` the filter iterates as).
    let filter_cut = match segments.get(cut) {
        Some(Segment::Child(selectors)) => match selectors.as_slice() {
            [Selector::Filter(expr)] => Some(expr),
            _ => None,
        },
        Some(Segment::Descendant(_)) | None => None,
    };
    let residual_start = if filter_cut.is_some() { cut + 1 } else { cut };
    let residual = segments.get(residual_start..).unwrap_or_default();

    // Soundness: fragments are their own root, so nothing past the cut may mention `$`.
    if filter_cut.is_some_and(expr_has_root_query) || segments_have_root(residual) {
        return Plan::Dom;
    }
    if cut == 0 && filter_cut.is_none() {
        // The prefix would be a bare `$`: one whole-document fragment, i.e. a DOM parse
        // with extra steps. (`cut == 0` *with* a predicate stays: `$[?P]` → `$[*]` + P.)
        return Plan::Dom;
    }

    let Some(prefix) = segments.get(..cut) else {
        return Plan::Dom;
    };
    // Exactly two rendered-prefix shapes select ~the whole document no matter what the
    // document contains: `$[?P]…` renders to `$[*]` (fragments = every child of the
    // root), and a lone bare wildcard step (`$[*]` / `$..*`) does the same or worse.
    // Deeper wildcards (`$.a[*]`) are data-dependent and stay unmarked.
    let whole_document = if filter_cut.is_some() {
        prefix.is_empty()
    } else {
        matches!(prefix, [segment] if is_bare_wildcard(segment))
    };
    let Some(rendered) = render_prefix(prefix, filter_cut.is_some()) else {
        return Plan::Dom;
    };
    // The engine is the last word on expressibility: anything the rules above let
    // through that it cannot compile (engine limits, exotic shapes) demotes to DOM.
    RsonpathEngine::compile_query(&rendered).map_or(Plan::Dom, |engine| {
        Plan::Scan(Box::new(ScanPlan {
            engine,
            predicate: filter_cut.cloned(),
            residual: residual.to_vec(),
            whole_document,
        }))
    })
}

/// A single-wildcard segment (`[*]` or `..*`) — as the *entire* prefix it selects every
/// child of the root / every node.
fn is_bare_wildcard(segment: &Segment) -> bool {
    let (Segment::Child(selectors) | Segment::Descendant(selectors)) = segment;
    matches!(selectors.as_slice(), [Selector::Wildcard])
}

/// Whether `segment` can live in the byte-scanned prefix: a single verbatim-safe name,
/// wildcard, or non-negative index selector. Slices are excluded by policy (they stay
/// in the residual), negative indexes because the engine cannot count from the end.
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
    if child_wildcard_tail {
        builder.child_wildcard();
    }
    Some(builder.to_query())
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
        Pattern::Literal(_) => false,
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
    use super::{Plan, split};
    use crate::JsonPath;

    /// The split decision for `query`, reduced to what the tests assert on:
    /// `Some((has_predicate, residual_len))` when it scans, `None` for [`Plan::Dom`].
    fn decision(query: &str) -> Option<(bool, usize)> {
        let path = JsonPath::parse(query).expect("test query must compile");
        match split(path.compiled()) {
            Plan::Scan(plan) => Some((plan.predicate.is_some(), plan.residual.len())),
            Plan::Dom => None,
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
    fn trailing_filter_becomes_the_predicate() {
        assert_eq!(
            decision("$.store.book[?@.price < 10]"),
            Some((true, 0)),
            "a trailing filter cuts into a predicate with nothing left over"
        );
        assert_eq!(
            decision("$.store.book[?@.p].title"),
            Some((true, 1)),
            "segments after the filter stay in the residual"
        );
        assert_eq!(
            decision("$[?@.x]"),
            Some((true, 0)),
            "a root-level filter scans as `$[*]` plus a predicate"
        );
        assert_eq!(
            decision("$.a[?@.b][?@.c]"),
            Some((true, 1)),
            "only the first filter becomes the predicate; the second stays residual"
        );
        assert_eq!(
            decision("$.a[?@.b][2]"),
            Some((true, 1)),
            "an index after the filter applies to each filtered node, in the residual"
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
            decision("$[1:2]"),
            None,
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
            decision("$..[?@.a]"),
            None,
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
                decision(query),
                None,
                "`{query}` references `$` past the cut and must fall back to DOM"
            );
        }
    }

    #[cfg(feature = "regex")]
    #[test]
    fn root_references_inside_regex_functions_force_dom() {
        assert_eq!(
            decision(r"$.a[?match(@.name, $.pattern)]"),
            None,
            "a dynamic pattern computed from `$` must fall back to DOM"
        );
        assert_eq!(
            decision(r#"$.a[?search(@.name, "ada")]"#),
            Some((true, 0)),
            "a literal pattern embeds no query and scans fine"
        );
    }

    #[test]
    fn inexpressible_shapes_fall_back_to_dom() {
        for query in ["$", "$[-1]", "$['a','b']", r#"$['a"b']"#] {
            assert_eq!(
                decision(query),
                None,
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
            decision(r"$['c\\d']"),
            None,
            "an escape-requiring name at the root leaves a bare-`$` prefix"
        );
    }

    #[test]
    fn whole_document_prefixes_are_marked() {
        let whole = |query: &str| {
            let path = JsonPath::parse(query).expect("test query must compile");
            match split(path.compiled()) {
                Plan::Scan(plan) => Some(plan.whole_document),
                Plan::Dom => None,
            }
        };
        assert_eq!(
            whole("$[?@.x]"),
            Some(true),
            "a root-level filter renders to `$[*]`: fragments cover the whole document"
        );
        assert_eq!(whole("$[*]"), Some(true), "`$[*]` selects every root child");
        assert_eq!(whole("$..*"), Some(true), "`$..*` selects every node");
        assert_eq!(
            whole("$[*].name"),
            Some(false),
            "a wildcard followed by a name narrows: fragment size is data-dependent"
        );
        assert_eq!(
            whole("$.store.book[?@.p]"),
            Some(false),
            "a named prefix is data-dependent, never statically whole-document"
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
