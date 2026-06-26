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
//! [RFC 9535 §2.4.3]: https://www.rfc-editor.org/rfc/rfc9535#section-2.4.3

use crate::ast;
use crate::error::Error;

/// A compiled query — the root of the IR (implicitly rooted at `$`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub segments: Vec<Segment>,
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
    Existence(FilterQuery),
    /// A `LogicalType` function used as a test (`match`/`search`).
    Test(Function),
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
    Literal(ast::Literal),
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
    /// `match(ValueType, ValueType) -> LogicalType` (anchored regex match).
    Match(ValueArg, ValueArg),
    /// `search(ValueType, ValueType) -> LogicalType` (substring regex search).
    Search(ValueArg, ValueArg),
}

/// An argument occupying a `ValueType` parameter slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueArg {
    Literal(ast::Literal),
    Singular(ast::SingularQuery),
    /// A nested `ValueType` function.
    Function(Box<Function>),
}

/// The declared result type of a function, used for context checks. No standard
/// function returns `NodesType`, so only these two are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultType {
    Value,
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
    Ok(Query {
        segments: lower_segments(query.segments)?,
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
            Ok(LogicalExpr::Existence(lower_filter_query(query)?))
        }
        ast::LogicalExpr::FunctionTest(function) => {
            let (function, result) = lower_function(function)?;
            match result {
                ResultType::Logical => Ok(LogicalExpr::Test(function)),
                ResultType::Value => Err(Error::IllTyped {
                    message: "a ValueType function result cannot be used as a bare test".to_owned(),
                }),
            }
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
        ast::Comparable::Literal(literal) => Ok(Comparable::Literal(literal)),
        ast::Comparable::SingularQuery(query) => Ok(Comparable::Singular(query)),
        ast::Comparable::Function(function) => {
            let (function, result) = lower_function(function)?;
            match result {
                ResultType::Value => Ok(Comparable::Function(function)),
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
            let [pattern_target, pattern] = take(&name, args)?;
            Ok((
                Function::Match(value_arg(pattern_target)?, value_arg(pattern)?),
                ResultType::Logical,
            ))
        }
        #[cfg(feature = "regex")]
        "search" => {
            let [pattern_target, pattern] = take(&name, args)?;
            Ok((
                Function::Search(value_arg(pattern_target)?, value_arg(pattern)?),
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
        ast::FunctionArg::Literal(literal) => Ok(ValueArg::Literal(literal)),
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
