//! The compiled, type-checked intermediate representation consumed by the evaluator.
//!
//! [`lower`] takes the loose syntactic [`crate::ast::Query`] and produces a
//! [`Query`] in which every function call is resolved to a strongly-typed [`Function`]
//! with exactly-typed argument slots. The lowering pass *is* the function
//! well-typedness check of [RFC 9535 §2.4.3]: it resolves names, checks arity,
//! enforces that `ValueType` slots receive singular queries / literals, that
//! `NodesType` slots receive queries, and that a function's result type fits its
//! context (only a `LogicalType` result may be a bare test; only a `ValueType` result
//! may be a comparable).
//!
//! Structurally the IR mirrors the AST; the leaf types that contain no function
//! ([`JsonInt`](crate::ast::JsonInt), [`Slice`](crate::ast::Slice),
//! [`ComparisonOp`](crate::ast::ComparisonOp), [`Literal`](crate::ast::Literal),
//! [`QueryRoot`](crate::ast::QueryRoot), [`SingularQuery`](crate::ast::SingularQuery))
//! are reused from [`crate::ast`] directly.
//!
//! Keeping the IR separate from the AST means the well-typedness check runs once, here in
//! [`lower`] — the evaluator only ever operates on a query already proven valid.
//!
//! [RFC 9535 §2.4.3]: https://www.rfc-editor.org/rfc/rfc9535#section-2.4.3

use crate::ast;
use crate::error::Error;
use serde_json::Value;

/// A compiled query — the root of the IR (implicitly rooted at `$`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub segments: Vec<Segment>,
    /// The singular form of this query, if every segment is a single child name or
    /// index step (so it selects at most one node). Precomputed at compile time so the
    /// evaluator can take a path-free, allocation-free fast path for the common
    /// `$.a.b[0].c` shape instead of running the general worklist traversal.
    pub singular: Option<ast::SingularQuery>,
}

/// A compiled segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Child(Vec<Selector>),
    Descendant(Vec<Selector>),
}

/// A compiled selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Name(String),
    Wildcard,
    Index(ast::JsonInt),
    Slice(ast::Slice),
    Filter(LogicalExpr),
}

/// A compiled filter logical expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalExpr {
    Or(Box<Self>, Box<Self>),
    And(Box<Self>, Box<Self>),
    Not(Box<Self>),
    Comparison(Comparison),
    /// A bare query used as an existence test.
    Existence(ExistenceTest),
    /// A `LogicalType` function used as a test (`match`/`search`). Only `match`/`search`
    /// return `LogicalType`, so this exists only when the `regex` feature is enabled.
    #[cfg(feature = "regex")]
    Test(Function),
}

/// A compiled existence test (`?@.a.b`, `?@.items[*]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistenceTest {
    /// The sub-query is singular (only single name/index steps), so it selects at most
    /// one node: presence is a path-free `eval_singular(...).is_some()` check with no
    /// nodelist allocation — the common `?@.field` / `?@.a.b` case.
    Singular(ast::SingularQuery),
    /// A general sub-query that may select many nodes; presence is decided by evaluating
    /// it and testing for non-emptiness.
    General(FilterQuery),
}

/// A compiled comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub left: Comparable,
    pub op: ast::ComparisonOp,
    pub right: Comparable,
}

/// A compiled comparable — guaranteed to denote at most one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparable {
    /// A literal, pre-converted to its `serde_json::Value` at compile time so the
    /// evaluator borrows it instead of rebuilding (and heap-cloning) it per comparison.
    /// Boxed to keep `Selector`/`Comparable` small (a `Value` is far larger than the
    /// other variants).
    Literal(Box<Value>),
    Singular(ast::SingularQuery),
    /// A `ValueType` function (`length`/`count`/`value`).
    Function(Function),
}

/// A compiled relative/absolute query used as a nodelist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterQuery {
    pub root: ast::QueryRoot,
    pub segments: Vec<Segment>,
}

/// A resolved, well-typed function-extension call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Function {
    /// `length(ValueType) -> ValueType`.
    Length(ValueArg),
    /// `count(NodesType) -> ValueType`.
    Count(FilterQuery),
    /// `value(NodesType) -> ValueType`.
    Value(FilterQuery),
    /// `match(ValueType, Pattern) -> LogicalType` (anchored regex match).
    #[cfg(feature = "regex")]
    Match(ValueArg, Pattern),
    /// `search(ValueType, Pattern) -> LogicalType` (substring regex search).
    #[cfg(feature = "regex")]
    Search(ValueArg, Pattern),
}

/// The pattern argument of a `match`/`search` call.
///
/// A literal pattern is translated and compiled to a [`regex::Regex`] *once*, here at
/// compile time, with the call's anchoring (`match` = full, `search` = substring) baked
/// in — so evaluating the filter over an N-element array no longer recompiles the regex
/// N times. A pattern computed from the document is necessarily compiled per evaluation.
#[cfg(feature = "regex")]
#[derive(Debug, Clone)]
pub enum Pattern {
    /// A literal pattern, pre-compiled. `None` if it is not a valid I-Regexp, in which
    /// case the function always yields false (RFC 9535 §2.4.6).
    Literal(Option<regex::Regex>),
    /// A pattern whose value is computed at evaluation time; compiled per call.
    Dynamic(ValueArg),
}

// `regex::Regex` is not `PartialEq`/`Eq`; compare patterns by their source so the
// surrounding IR can keep deriving `Eq`. Two literals are equal iff they compiled from
// the same (anchored) regex source.
#[cfg(feature = "regex")]
impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Literal(left), Self::Literal(right)) => {
                left.as_ref().map(regex::Regex::as_str) == right.as_ref().map(regex::Regex::as_str)
            }
            (Self::Dynamic(left), Self::Dynamic(right)) => left == right,
            (Self::Literal(_), Self::Dynamic(_)) | (Self::Dynamic(_), Self::Literal(_)) => false,
        }
    }
}

#[cfg(feature = "regex")]
impl Eq for Pattern {}

/// An argument occupying a `ValueType` parameter slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueArg {
    /// A literal, pre-converted to its `serde_json::Value` at compile time (see
    /// [`Comparable::Literal`]).
    Literal(Box<Value>),
    Singular(ast::SingularQuery),
    /// A nested `ValueType` function.
    Function(Box<Function>),
}

/// The declared result type of a function, used for context checks. No standard
/// function returns `NodesType`, so only these two are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultType {
    Value,
    #[cfg(feature = "regex")]
    Logical,
}

/// Lowers a syntactic query into the type-checked IR, performing the RFC 9535 §2.4.3
/// well-typedness check on every function expression.
///
/// # Errors
///
/// Returns [`Error::UnknownFunction`], [`Error::FunctionArity`], or [`Error::IllTyped`]
/// if a function extension is unregistered, called with the wrong arity, or used in a
/// way that violates the type system (e.g. a non-singular query in a `ValueType` slot,
/// or a `LogicalType` result used as a comparable).
pub fn lower(query: ast::Query) -> Result<Query, Error> {
    let segments = lower_segments(query.segments)?;
    let singular = singular_form(&segments);
    Ok(Query { segments, singular })
}

/// Returns the singular-query form of a top-level query if every segment is a single
/// child name or index step, or `None` if it may select more than one node. Mirrors
/// [`to_singular`] but works on the *compiled* top-level segments (which are always
/// rooted at `$`).
fn singular_form(segments: &[Segment]) -> Option<ast::SingularQuery> {
    let mut steps = Vec::with_capacity(segments.len());
    for segment in segments {
        match segment {
            Segment::Child(selectors) => match selectors.as_slice() {
                [Selector::Name(name)] => steps.push(ast::SingularSegment::Name(name.clone())),
                [Selector::Index(index)] => steps.push(ast::SingularSegment::Index(*index)),
                _ => return None,
            },
            Segment::Descendant(_) => return None,
        }
    }
    Some(ast::SingularQuery {
        root: ast::QueryRoot::Root,
        segments: steps,
    })
}

fn lower_segments(segments: Vec<ast::Segment>) -> Result<Vec<Segment>, Error> {
    segments.into_iter().map(lower_segment).collect()
}

fn lower_segment(segment: ast::Segment) -> Result<Segment, Error> {
    match segment {
        ast::Segment::Child(selectors) => Ok(Segment::Child(lower_selectors(selectors)?)),
        ast::Segment::Descendant(selectors) => Ok(Segment::Descendant(lower_selectors(selectors)?)),
    }
}

fn lower_selectors(selectors: Vec<ast::Selector>) -> Result<Vec<Selector>, Error> {
    selectors.into_iter().map(lower_selector).collect()
}

fn lower_selector(selector: ast::Selector) -> Result<Selector, Error> {
    match selector {
        ast::Selector::Name(name) => Ok(Selector::Name(name)),
        ast::Selector::Wildcard => Ok(Selector::Wildcard),
        ast::Selector::Index(index) => Ok(Selector::Index(index)),
        ast::Selector::Slice(slice) => Ok(Selector::Slice(slice)),
        ast::Selector::Filter(expr) => Ok(Selector::Filter(lower_logical(expr)?)),
    }
}

fn lower_filter_query(query: ast::FilterQuery) -> Result<FilterQuery, Error> {
    Ok(FilterQuery {
        root: query.root,
        segments: lower_segments(query.segments)?,
    })
}

fn lower_logical(expr: ast::LogicalExpr) -> Result<LogicalExpr, Error> {
    match expr {
        ast::LogicalExpr::Or(left, right) => Ok(LogicalExpr::Or(
            Box::new(lower_logical(*left)?),
            Box::new(lower_logical(*right)?),
        )),
        ast::LogicalExpr::And(left, right) => Ok(LogicalExpr::And(
            Box::new(lower_logical(*left)?),
            Box::new(lower_logical(*right)?),
        )),
        ast::LogicalExpr::Not(inner) => Ok(LogicalExpr::Not(Box::new(lower_logical(*inner)?))),
        ast::LogicalExpr::Comparison(comparison) => {
            Ok(LogicalExpr::Comparison(lower_comparison(comparison)?))
        }
        ast::LogicalExpr::Existence(query) => {
            // A singular sub-query selects ≤1 node, so existence is an allocation-free
            // presence check; only a genuinely multi-node query needs the worklist walk.
            let test = match to_singular(&query) {
                Some(singular) => ExistenceTest::Singular(singular),
                None => ExistenceTest::General(lower_filter_query(query)?),
            };
            Ok(LogicalExpr::Existence(test))
        }
        ast::LogicalExpr::FunctionTest(function) => match lower_function(function)? {
            #[cfg(feature = "regex")]
            (function, ResultType::Logical) => Ok(LogicalExpr::Test(function)),
            // Without `regex`, every function returns `ValueType`, so a bare function
            // test is always ill-typed. `lower_function` still ran, so arity/unknown-name
            // errors have already been propagated above.
            (_function, ResultType::Value) => Err(Error::IllTyped {
                message: "a ValueType function result cannot be used as a bare test".to_owned(),
            }),
        },
    }
}

/// Converts an AST literal to its `serde_json::Value` once, at compile time. The
/// evaluator then borrows the stored value rather than rebuilding it (and heap-cloning
/// any string) on every comparison.
fn literal_to_value(literal: &ast::Literal) -> Value {
    match literal {
        ast::Literal::Number(number) => Value::Number(number.clone()),
        ast::Literal::String(string) => Value::String(string.clone()),
        ast::Literal::Bool(boolean) => Value::Bool(*boolean),
        ast::Literal::Null => Value::Null,
    }
}

/// Lowers a `match`/`search` pattern argument: a literal string is translated and
/// compiled once (with `anchored` controlling full-match vs substring); anything else
/// becomes a [`Pattern::Dynamic`] compiled per evaluation.
#[cfg(feature = "regex")]
fn lower_pattern(arg: ast::FunctionArg, anchored: bool) -> Result<Pattern, Error> {
    match value_arg(arg)? {
        ValueArg::Literal(value) => Ok(Pattern::Literal(compile_literal(value.as_ref(), anchored))),
        dynamic @ (ValueArg::Singular(_) | ValueArg::Function(_)) => Ok(Pattern::Dynamic(dynamic)),
    }
}

/// Compiles a literal pattern value to a regex, or `None` when it is not a string or not
/// a valid I-Regexp (both cases make the function always yield false).
#[cfg(feature = "regex")]
fn compile_literal(value: &Value, anchored: bool) -> Option<regex::Regex> {
    match value {
        Value::String(pattern) => crate::iregexp::build(pattern, anchored),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            None
        }
    }
}

fn lower_comparison(comparison: ast::Comparison) -> Result<Comparison, Error> {
    Ok(Comparison {
        left: lower_comparable(comparison.left)?,
        op: comparison.op,
        right: lower_comparable(comparison.right)?,
    })
}

fn lower_comparable(comparable: ast::Comparable) -> Result<Comparable, Error> {
    match comparable {
        ast::Comparable::Literal(literal) => {
            Ok(Comparable::Literal(Box::new(literal_to_value(&literal))))
        }
        ast::Comparable::SingularQuery(query) => Ok(Comparable::Singular(query)),
        ast::Comparable::Function(function) => {
            let (function, result) = lower_function(function)?;
            match result {
                ResultType::Value => Ok(Comparable::Function(function)),
                #[cfg(feature = "regex")]
                ResultType::Logical => Err(Error::IllTyped {
                    message: "a LogicalType function result cannot be used in a comparison"
                        .to_owned(),
                }),
            }
        }
    }
}

fn lower_function(function: ast::FunctionExpr) -> Result<(Function, ResultType), Error> {
    let ast::FunctionExpr { name, args } = function;
    match name.as_str() {
        "length" => {
            let [arg] = take(&name, args)?;
            Ok((Function::Length(value_arg(arg)?), ResultType::Value))
        }
        "count" => {
            let [arg] = take(&name, args)?;
            Ok((Function::Count(nodes_arg(arg)?), ResultType::Value))
        }
        "value" => {
            let [arg] = take(&name, args)?;
            Ok((Function::Value(nodes_arg(arg)?), ResultType::Value))
        }
        #[cfg(feature = "regex")]
        "match" => {
            let [target, pattern] = take(&name, args)?;
            Ok((
                Function::Match(value_arg(target)?, lower_pattern(pattern, true)?),
                ResultType::Logical,
            ))
        }
        #[cfg(feature = "regex")]
        "search" => {
            let [target, pattern] = take(&name, args)?;
            Ok((
                Function::Search(value_arg(target)?, lower_pattern(pattern, false)?),
                ResultType::Logical,
            ))
        }
        #[cfg(not(feature = "regex"))]
        "match" | "search" => Err(Error::IllTyped {
            message: "the `match` and `search` functions require the `regex` cargo feature"
                .to_owned(),
        }),
        _ => Err(Error::UnknownFunction { name }),
    }
}

/// Extracts exactly `N` arguments, or reports an arity error naming the function.
fn take<const N: usize>(
    name: &str,
    args: Vec<ast::FunctionArg>,
) -> Result<[ast::FunctionArg; N], Error> {
    let found = args.len();
    match <[ast::FunctionArg; N]>::try_from(args) {
        Ok(array) => Ok(array),
        Err(_returned) => Err(Error::FunctionArity {
            name: name.to_owned(),
            expected: N,
            found,
        }),
    }
}

/// Lowers an argument occupying a `ValueType` parameter (literal, singular query, or
/// a nested `ValueType` function).
fn value_arg(arg: ast::FunctionArg) -> Result<ValueArg, Error> {
    match arg {
        ast::FunctionArg::Literal(literal) => {
            Ok(ValueArg::Literal(Box::new(literal_to_value(&literal))))
        }
        ast::FunctionArg::Query(query) => to_singular(&query).map_or_else(
            || {
                Err(Error::IllTyped {
                    message: "a non-singular query cannot be used where a ValueType is expected"
                        .to_owned(),
                })
            },
            |singular| Ok(ValueArg::Singular(singular)),
        ),
        ast::FunctionArg::Function(function) => {
            let (function, result) = lower_function(function)?;
            match result {
                ResultType::Value => Ok(ValueArg::Function(Box::new(function))),
                #[cfg(feature = "regex")]
                ResultType::Logical => Err(Error::IllTyped {
                    message: "a LogicalType function result cannot be used where a ValueType is \
                              expected"
                        .to_owned(),
                }),
            }
        }
        ast::FunctionArg::Logical(_) => Err(Error::IllTyped {
            message: "a logical expression cannot be used where a ValueType is expected".to_owned(),
        }),
    }
}

/// Lowers an argument occupying a `NodesType` parameter (which must be a query — no
/// standard function returns `NodesType`).
fn nodes_arg(arg: ast::FunctionArg) -> Result<FilterQuery, Error> {
    match arg {
        ast::FunctionArg::Query(query) => lower_filter_query(query),
        ast::FunctionArg::Literal(_)
        | ast::FunctionArg::Logical(_)
        | ast::FunctionArg::Function(_) => Err(Error::IllTyped {
            message: "a NodesType argument must be a query".to_owned(),
        }),
    }
}

/// Returns the singular-query form of `query` if every segment is a single child name
/// or index step, or `None` if the query may select more than one node.
fn to_singular(query: &ast::FilterQuery) -> Option<ast::SingularQuery> {
    let mut segments = Vec::with_capacity(query.segments.len());
    for segment in &query.segments {
        match segment {
            ast::Segment::Child(selectors) => match selectors.as_slice() {
                [ast::Selector::Name(name)] => {
                    segments.push(ast::SingularSegment::Name(name.clone()));
                }
                [ast::Selector::Index(index)] => {
                    segments.push(ast::SingularSegment::Index(*index));
                }
                _ => return None,
            },
            ast::Segment::Descendant(_) => return None,
        }
    }
    Some(ast::SingularQuery {
        root: query.root,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::lower;
    use crate::parser::parse;

    fn compiles(query: &str) -> bool {
        parse(query).is_ok_and(|ast| lower(ast).is_ok())
    }

    #[test]
    fn structural_queries_compile() {
        assert!(compiles("$.store.book[0]"), "plain segments");
        assert!(
            compiles("$..book[?@.price < 10]"),
            "descendant + filter comparison"
        );
        assert!(
            compiles("$[?@.a && (@.b || !@.c)]"),
            "logical operators and grouping"
        );
    }

    #[test]
    fn well_typed_functions_compile() {
        assert!(
            compiles("$[?length(@.tags) >= 3]"),
            "length of a singular query in a comparison"
        );
        assert!(
            compiles("$[?count(@.*) == 1]"),
            "count of a non-singular query"
        );
        assert!(
            compiles("$[?value(@..color) == 'red']"),
            "value() result compared"
        );
        #[cfg(feature = "regex")]
        assert!(
            compiles(r#"$[?match(@.date, "....")]"#),
            "match() as a test"
        );
        #[cfg(feature = "regex")]
        assert!(
            compiles("$[?search(@.author, 'Bob')]"),
            "search() as a test"
        );
    }

    #[test]
    fn non_singular_query_in_value_slot_is_rejected() {
        // RFC 9535 §2.4.9: length(@.*) is not well-typed.
        assert!(
            !compiles("$[?length(@.*) < 3]"),
            "@.* is not a singular query"
        );
    }

    #[test]
    fn logical_result_compared_is_rejected() {
        // RFC 9535 §2.4.9: match(...) == true is not well-typed.
        assert!(
            !compiles("$[?match(@.a, 'x') == true]"),
            "LogicalType may not be compared"
        );
    }

    #[test]
    fn value_result_as_bare_test_is_rejected() {
        // RFC 9535 §2.4.9: value(@..color) as a test is not well-typed.
        assert!(
            !compiles("$[?value(@..color)]"),
            "ValueType may not be a bare test"
        );
    }

    #[test]
    fn non_query_in_nodes_slot_is_rejected() {
        // RFC 9535 §2.4.9: count(1) is not well-typed.
        assert!(
            !compiles("$[?count(1) == 1]"),
            "a NodesType slot needs a query"
        );
    }

    #[test]
    fn unknown_function_is_rejected() {
        assert!(
            !compiles("$[?bogus(@.a)]"),
            "unregistered function names are errors"
        );
    }

    #[test]
    fn wrong_arity_is_rejected() {
        assert!(
            !compiles("$[?match(@.a) == 1]"),
            "match needs two arguments"
        );
        assert!(
            !compiles("$[?length(@.a, @.b) == 1]"),
            "length takes one argument"
        );
    }
}
