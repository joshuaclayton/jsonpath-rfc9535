//! Function-extension call parsing.
//!
//! ```abnf
//! function-name       = function-name-first *function-name-char
//! function-name-first = LCALPHA
//! function-name-char  = function-name-first / "_" / DIGIT
//! LCALPHA             = %x61-7A  ; "a".."z"
//! function-expr       = function-name "(" S [function-argument
//!                          *(S "," S function-argument)] S ")"
//! function-argument   = literal /
//!                       filter-query / ; (includes singular-query)
//!                       logical-expr /
//!                       function-expr
//! ```
//!
//! Implementation notes:
//! * The parser stays *syntactic*: it records the `name` verbatim (any lowercase
//!   identifier is well-formed). Whether the name is one of the five registered
//!   functions, the argument count, and well-typedness are all checked later, during
//!   lowering — unknown names are not rejected here.
//! * Argument classification is handled by parsing a [`logical_expr`] first and then
//!   *unwrapping* the trivial cases: a bare query becomes [`FunctionArg::Query`] and a
//!   bare function call becomes [`FunctionArg::Function`] (so they retain their
//!   `NodesType` / result-type identity for the type checker), while anything with an
//!   operator, negation, or parentheses stays [`FunctionArg::Logical`]. A bare
//!   literal is not a `logical-expr`, so it falls through to the [`literal`] arm.

use super::ParseResult;
use super::filter::{literal, logical_expr};
use super::s;
use crate::ast::{FunctionArg, FunctionExpr, LogicalExpr};
use nom::Parser;
use nom::branch::alt;
use nom::bytes::complete::take_while;
use nom::character::complete::{char, satisfy};
use nom::combinator::map;
use nom::multi::separated_list0;
use nom::sequence::{delimited, pair};

const fn is_function_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

/// rule: `function-name` — a lowercase identifier (`[a-z][a-z0-9_]*`).
pub fn function_name(input: &str) -> ParseResult<'_, String> {
    let (rest, (first, tail)) = pair(
        satisfy(|c: char| c.is_ascii_lowercase()),
        take_while(is_function_name_char),
    )
    .parse(input)?;
    let mut name = String::with_capacity(first.len_utf8() + tail.len());
    name.push(first);
    name.push_str(tail);
    Ok((rest, name))
}

/// rule: `function-expr` — a function name applied to a parenthesized argument list.
pub fn function_expr(input: &str) -> ParseResult<'_, FunctionExpr> {
    let (input, name) = function_name(input)?;
    let (input, args) = delimited(
        pair(char('('), s),
        separated_list0(delimited(s, char(','), s), function_argument),
        pair(s, char(')')),
    )
    .parse(input)?;
    Ok((input, FunctionExpr { name, args }))
}

/// rule: `function-argument` — one argument to a function call.
pub fn function_argument(input: &str) -> ParseResult<'_, FunctionArg> {
    alt((
        map(logical_expr, classify_logical),
        map(literal, FunctionArg::Literal),
    ))
    .parse(input)
}

/// Reclassifies a parsed [`LogicalExpr`] into the narrowest [`FunctionArg`]: a bare
/// existence test is really a `NodesType` query argument, and a bare function test is
/// really a function argument (classified by its own result type downstream).
fn classify_logical(expr: LogicalExpr) -> FunctionArg {
    match expr {
        LogicalExpr::Existence(query) => FunctionArg::Query(query),
        LogicalExpr::FunctionTest(function) => FunctionArg::Function(function),
        compound @ (LogicalExpr::Or(..)
        | LogicalExpr::And(..)
        | LogicalExpr::Not(..)
        | LogicalExpr::Comparison(..)) => FunctionArg::Logical(compound),
    }
}

#[cfg(test)]
mod tests {
    use super::{function_expr, function_name};
    use crate::ast::{FunctionArg, Literal};

    #[test]
    fn name_allows_digits_and_underscores_after_first() {
        let (rest, name) = function_name("a_fn2(").expect("should parse");
        assert_eq!(name, "a_fn2", "name is [a-z][a-z0-9_]*");
        assert_eq!(rest, "(", "stops at the opening paren");
    }

    #[test]
    fn parses_two_argument_call_verbatim_name() {
        let (rest, call) = function_expr("match(@.s, 'a.*')").expect("should parse");
        assert_eq!(rest, "", "should consume the whole call");
        assert_eq!(call.name, "match", "name is recorded verbatim, unresolved");
        assert_eq!(call.args.len(), 2, "two arguments");
    }

    #[test]
    fn records_unknown_function_name_without_error() {
        let (_rest, call) = function_expr("foo(@)").expect("syntax is valid");
        assert_eq!(
            call.name, "foo",
            "unknown names are a type-check concern, not a parse error"
        );
    }

    #[test]
    fn parses_zero_argument_call() {
        let (_rest, call) = function_expr("length()").expect("should parse");
        assert!(
            call.args.is_empty(),
            "an empty argument list is well-formed syntax"
        );
    }

    #[test]
    fn classifies_argument_kinds() {
        let (_rest, call) = function_expr("f(@.a, 'x', 1 == 1, length(@))").expect("should parse");
        assert!(
            matches!(call.args.first(), Some(FunctionArg::Query(_))),
            "bare query -> Query"
        );
        assert!(
            matches!(
                call.args.get(1),
                Some(FunctionArg::Literal(Literal::String(_)))
            ),
            "bare literal -> Literal"
        );
        assert!(
            matches!(call.args.get(2), Some(FunctionArg::Logical(_))),
            "comparison -> Logical"
        );
        assert!(
            matches!(call.args.get(3), Some(FunctionArg::Function(_))),
            "bare call -> Function"
        );
    }
}
