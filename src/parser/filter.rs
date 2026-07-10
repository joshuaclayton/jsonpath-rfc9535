//! Filter expression parsing — the body of a `?<expr>` filter selector, plus the
//! comparables, singular queries, and (relative/absolute) filter queries that
//! appear inside it.
//!
//! ```abnf
//! filter-selector  = "?" S logical-expr
//! logical-expr     = logical-or-expr
//! logical-or-expr  = logical-and-expr *(S "||" S logical-and-expr)
//! logical-and-expr = basic-expr *(S "&&" S basic-expr)
//! basic-expr       = paren-expr / comparison-expr / test-expr
//! paren-expr       = [logical-not-op S] "(" S logical-expr S ")"
//! logical-not-op   = "!"
//! test-expr        = [logical-not-op S] (filter-query / function-expr)
//! filter-query     = rel-query / jsonpath-query
//! rel-query        = current-node-identifier segments
//! current-node-identifier = "@"
//! comparison-expr  = comparable S comparison-op S comparable
//! comparable       = literal / singular-query / function-expr
//! literal          = number / string-literal / true / false / null
//! comparison-op    = "==" / "!=" / "<=" / ">=" / "<" / ">"
//! singular-query   = rel-singular-query / abs-singular-query
//! singular-query-segments = *(S (name-segment / index-segment))
//! name-segment     = ("[" name-selector "]") / ("." member-name-shorthand)
//! index-segment    = "[" index-selector "]"
//! ```
//!
//! Implementation notes:
//! * Precedence is structural: `logical-or-expr` folds `&&`-groups with `Or`, and
//!   `logical-and-expr` folds `basic-expr`s with `And`, so `||` binds looser than
//!   `&&`, and `!` (handled inside `paren-expr`/`test-expr`) binds tightest.
//! * `basic-expr` tries `comparison-expr` before `test-expr`: a bare query is a test,
//!   but the same query on the left of an operator is a comparison — trying the test
//!   first would consume the query and orphan the operator.
//! * `comparable` admits only a `singular-query` (not a general query), which is how
//!   the grammar forbids comparing non-singular queries: `@.* == 1` fails to parse.
//! * `true`/`false`/`null` are matched only when not followed by a function-name
//!   character, so they are not mistaken for the prefix of a function name.

use super::ParseResult;
use super::function::function_expr;
use super::number::{int, number};
use super::query::segments;
use super::s;
use super::segment::member_name_shorthand;
use super::string::string_literal;
use crate::ast::{
    Comparable, Comparison, ComparisonOp, FilterQuery, Literal, LogicalExpr, QueryRoot,
    SingularQuery, SingularSegment,
};
use nom::Parser;
use nom::branch::alt;
use nom::bytes::complete::tag;
use nom::character::complete::{char, satisfy};
use nom::combinator::{map, not, opt, value};
use nom::multi::many0;
use nom::sequence::{delimited, pair, preceded, terminated};

/// rule: `filter-selector = "?" S logical-expr` — returns the logical expression
/// (the leading `?` is consumed here).
pub fn filter_selector(input: &str) -> ParseResult<'_, LogicalExpr> {
    preceded(pair(char('?'), s), logical_expr).parse(input)
}

/// rule: `logical-expr = logical-or-expr`.
pub fn logical_expr(input: &str) -> ParseResult<'_, LogicalExpr> {
    logical_or_expr(input)
}

/// rule: `logical-or-expr = logical-and-expr *(S "||" S logical-and-expr)`.
fn logical_or_expr(input: &str) -> ParseResult<'_, LogicalExpr> {
    let (input, first) = logical_and_expr(input)?;
    let (input, rest) =
        many0(preceded(delimited(s, tag("||"), s), logical_and_expr)).parse(input)?;
    let expr = rest.into_iter().fold(first, |acc, next| {
        LogicalExpr::Or(Box::new(acc), Box::new(next))
    });
    Ok((input, expr))
}

/// rule: `logical-and-expr = basic-expr *(S "&&" S basic-expr)`.
fn logical_and_expr(input: &str) -> ParseResult<'_, LogicalExpr> {
    let (input, first) = basic_expr(input)?;
    let (input, rest) = many0(preceded(delimited(s, tag("&&"), s), basic_expr)).parse(input)?;
    let expr = rest.into_iter().fold(first, |acc, next| {
        LogicalExpr::And(Box::new(acc), Box::new(next))
    });
    Ok((input, expr))
}

/// rule: `basic-expr = paren-expr / comparison-expr / test-expr`.
fn basic_expr(input: &str) -> ParseResult<'_, LogicalExpr> {
    alt((
        paren_expr,
        map(comparison, LogicalExpr::Comparison),
        test_expr,
    ))
    .parse(input)
}

/// rule: `paren-expr = [logical-not-op S] "(" S logical-expr S ")"`.
fn paren_expr(input: &str) -> ParseResult<'_, LogicalExpr> {
    let (input, negated) = opt(terminated(char('!'), s)).parse(input)?;
    let (input, inner) =
        delimited(pair(char('('), s), logical_expr, pair(s, char(')'))).parse(input)?;
    Ok((input, negate_if(negated.is_some(), inner)))
}

/// rule: `test-expr = [logical-not-op S] (filter-query / function-expr)`.
fn test_expr(input: &str) -> ParseResult<'_, LogicalExpr> {
    let (input, negated) = opt(terminated(char('!'), s)).parse(input)?;
    let (input, inner) = alt((
        map(filter_query, LogicalExpr::Existence),
        map(function_expr, LogicalExpr::FunctionTest),
    ))
    .parse(input)?;
    Ok((input, negate_if(negated.is_some(), inner)))
}

fn negate_if(negated: bool, expr: LogicalExpr) -> LogicalExpr {
    if negated {
        LogicalExpr::Not(Box::new(expr))
    } else {
        expr
    }
}

/// rule: `comparison-expr = comparable S comparison-op S comparable`.
pub fn comparison(input: &str) -> ParseResult<'_, Comparison> {
    let (input, left) = comparable(input)?;
    let (input, _lws) = s(input)?;
    let (input, op) = comparison_op(input)?;
    let (input, _rws) = s(input)?;
    let (input, right) = comparable(input)?;
    Ok((input, Comparison { left, op, right }))
}

/// rule: `comparison-op = "==" / "!=" / "<=" / ">=" / "<" / ">"`.
pub fn comparison_op(input: &str) -> ParseResult<'_, ComparisonOp> {
    alt((
        value(ComparisonOp::Eq, tag("==")),
        value(ComparisonOp::Ne, tag("!=")),
        value(ComparisonOp::Le, tag("<=")),
        value(ComparisonOp::Ge, tag(">=")),
        value(ComparisonOp::Lt, tag("<")),
        value(ComparisonOp::Gt, tag(">")),
    ))
    .parse(input)
}

/// rule: `comparable = literal / singular-query / function-expr`.
pub fn comparable(input: &str) -> ParseResult<'_, Comparable> {
    alt((
        map(literal, Comparable::Literal),
        map(singular_query, Comparable::SingularQuery),
        map(function_expr, Comparable::Function),
    ))
    .parse(input)
}

/// rule: `literal = number / string-literal / true / false / null`.
pub fn literal(input: &str) -> ParseResult<'_, Literal> {
    alt((
        map(number, Literal::Number),
        map(string_literal, Literal::String),
        value(Literal::Bool(true), keyword("true")),
        value(Literal::Bool(false), keyword("false")),
        value(Literal::Null, keyword("null")),
    ))
    .parse(input)
}

/// Matches the keyword `word` only when it is not immediately followed by a
/// function-name character, so a keyword is never read as the prefix of a longer name.
fn keyword(word: &'static str) -> impl FnMut(&str) -> ParseResult<'_, &str> {
    move |input| {
        terminated(
            tag(word),
            not(satisfy(|c: char| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
            })),
        )
        .parse(input)
    }
}

/// rule: `singular-query` — a query selecting at most one node.
pub fn singular_query(input: &str) -> ParseResult<'_, SingularQuery> {
    let (input, root) = query_root(input)?;
    let (input, segments) = many0(preceded(s, singular_segment)).parse(input)?;
    Ok((input, SingularQuery { root, segments }))
}

/// `name-segment / index-segment` — a single name or index step.
fn singular_segment(input: &str) -> ParseResult<'_, SingularSegment> {
    alt((
        delimited(
            char('['),
            alt((
                map(string_literal, SingularSegment::Name),
                map(int, SingularSegment::Index),
            )),
            char(']'),
        ),
        map(
            preceded(char('.'), member_name_shorthand),
            SingularSegment::Name,
        ),
    ))
    .parse(input)
}

/// rule: `filter-query = rel-query / jsonpath-query` — a relative (`@`) or absolute
/// (`$`) query usable as an existence test or function argument.
pub fn filter_query(input: &str) -> ParseResult<'_, FilterQuery> {
    let (input, root) = query_root(input)?;
    let (input, segments) = segments(input)?;
    Ok((input, FilterQuery { root, segments }))
}

/// `current-node-identifier / root-identifier` — `@` or `$`.
fn query_root(input: &str) -> ParseResult<'_, QueryRoot> {
    alt((
        value(QueryRoot::Current, char('@')),
        value(QueryRoot::Root, char('$')),
    ))
    .parse(input)
}

#[cfg(test)]
mod tests {
    use super::{comparison_op, filter_selector, literal, logical_expr, singular_query};
    use crate::ast::{
        Comparable, ComparisonOp, JsonInt, Literal, LogicalExpr, QueryRoot, SingularQuery,
        SingularSegment,
    };

    #[test]
    fn two_char_ops_win_over_one_char() {
        let (rest, op) = comparison_op("<=1").expect("should parse");
        assert_eq!(op, ComparisonOp::Le, "`<=` must not be read as `<`");
        assert_eq!(rest, "1", "only the operator is consumed");
    }

    #[test]
    fn parses_keyword_literals() {
        assert_eq!(
            literal("null").map(|(_, l)| l).expect("should parse"),
            Literal::Null,
            "null"
        );
        assert_eq!(
            literal("true").map(|(_, l)| l).expect("should parse"),
            Literal::Bool(true),
            "true"
        );
    }

    #[test]
    fn keyword_is_not_a_function_name_prefix() {
        // `nullx` is a valid function name, so `null` must not match its prefix here.
        assert!(
            literal("nullx").is_err(),
            "`null` must not match the prefix of `nullx`"
        );
    }

    #[test]
    fn relative_singular_query_of_name_and_index() {
        let (_rest, q) = singular_query("@.a[0]").expect("should parse");
        assert_eq!(
            q,
            SingularQuery {
                root: QueryRoot::Current,
                segments: vec![
                    SingularSegment::Name("a".to_owned()),
                    SingularSegment::Index(JsonInt::new(0).expect("in range")),
                ],
            },
            "`@.a[0]` is a relative singular query"
        );
    }

    #[test]
    fn filter_selector_consumes_leading_question_mark() {
        let (_rest, expr) = filter_selector("?@.a").expect("should parse");
        assert!(
            matches!(expr, LogicalExpr::Existence(_)),
            "`?@.a` is an existence test"
        );
    }

    #[test]
    fn parses_comparison() {
        let (_rest, expr) = logical_expr("@.price < 10").expect("should parse");
        assert!(
            matches!(&expr, LogicalExpr::Comparison(_)),
            "expected a comparison"
        );
        if let LogicalExpr::Comparison(c) = expr {
            assert_eq!(c.op, ComparisonOp::Lt, "operator is `<`");
            assert!(
                matches!(c.left, Comparable::SingularQuery(_)),
                "lhs is a singular query"
            );
            assert!(
                matches!(c.right, Comparable::Literal(Literal::Number(_))),
                "rhs is a number"
            );
        }
    }

    #[test]
    fn precedence_or_binds_looser_than_and() {
        // a && b || c  parses as  (a && b) || c
        let (_rest, expr) = logical_expr("@.a && @.b || @.c").expect("should parse");
        assert!(
            matches!(expr, LogicalExpr::Or(left, _) if matches!(*left, LogicalExpr::And(..))),
            "`&&` groups under the top-level `||`"
        );
    }

    #[test]
    fn negation_and_parentheses() {
        let (_rest, expr) = logical_expr("!(@.a == 1)").expect("should parse");
        assert!(
            matches!(expr, LogicalExpr::Not(inner) if matches!(*inner, LogicalExpr::Comparison(_))),
            "`!( ... )` negates the parenthesized comparison"
        );
    }

    #[test]
    fn rejects_non_singular_comparable() {
        // `@.*` is not a singular query, so it cannot appear in a comparison.
        assert!(
            super::super::parse("$[?@.* == 1]").is_err(),
            "comparing a non-singular query is a syntax error"
        );
    }

    #[test]
    fn parses_function_in_comparison() {
        let (_rest, expr) = logical_expr("length(@.tags) >= 3").expect("should parse");
        assert!(
            matches!(expr, LogicalExpr::Comparison(c) if matches!(c.left, Comparable::Function(_))),
            "a function expression is a valid comparable"
        );
    }
}
