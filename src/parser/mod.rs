//! The JSONPath query parser (nom).
//!
//! This module turns a query string into a syntactic [`crate::ast::Query`]. It is
//! deliberately *only* concerned with syntax — well-typedness of function
//! extensions (RFC 9535 §2.4.3) is checked later, during lowering to the compiled
//! IR. The grammar implemented here is the collected ABNF of [RFC 9535 Appendix A].
//!
//! # Layout / conventions
//!
//! * Each grammar rule has a corresponding combinator, grouped by topic into the
//!   submodules ([`string`], [`number`], [`selector`], [`mod@slice`], [`segment`],
//!   [`filter`], [`function`], [`query`]). Every combinator is annotated with the
//!   verbatim ABNF rule it implements.
//! * Combinators take `&str` and return [`nom::IResult`] over `&str`, e.g.
//!   `fn name_selector(input: &str) -> IResult<&str, String>`.
//! * The ABNF whitespace rules are explicit: `B` is one blank byte
//!   (`%x20 / %x09 / %x0A / %x0D`) and `S = *B` is optional blank space. Apply `S`
//!   **only** where the grammar places it — JSONPath does *not* allow arbitrary
//!   whitespace (e.g. no leading space before `$`, none inside a shorthand name).
//! * Integer range checking: a parsed integer must be turned into a
//!   [`JsonInt`](crate::ast::JsonInt) via the [`int`](number::int) combinator, which
//!   rejects values outside the I-JSON safe range with a [`nom::Err::Failure`]
//!   carrying [`Error::IntegerOutOfRange`] — the typed variant survives to
//!   [`parse`]'s caller instead of collapsing into a generic syntax error (see
//!   [`ParserError`]).
//!
//! [RFC 9535 Appendix A]: https://www.rfc-editor.org/rfc/rfc9535#appendix-A

use crate::Error;
use crate::ast::Query;
use nom::bytes::complete::take_while;
use nom::{Parser, combinator::all_consuming};

pub mod filter;
pub mod function;
pub mod number;
pub mod query;
pub mod segment;
pub mod selector;
pub mod slice;
pub mod string;

/// The result type of every combinator in this parser: [`nom::IResult`] over `&str`
/// with [`ParserError`] as the error type, so a typed crate [`Error`] can travel
/// through nom to [`parse`]'s caller.
pub type ParseResult<'a, O> = nom::IResult<&'a str, O, ParserError<'a>>;

/// The error type threaded through every combinator in this parser.
///
/// Like nom's default error it records *where* parsing failed (the unconsumed
/// remainder, from which [`parse`] recovers the byte offset). Additionally it can
/// carry a typed crate [`Error`] when a rule identified a specific violation —
/// today only [`Error::IntegerOutOfRange`], raised by [`number::int`] as a
/// [`nom::Err::Failure`] so it aborts alternation and reaches the caller verbatim
/// instead of being masked by a sibling branch's generic syntax error.
#[derive(Debug)]
pub struct ParserError<'a> {
    /// The unconsumed remainder at the point of failure.
    input: &'a str,
    /// The specific violation, when a rule identified one.
    cause: Option<Error>,
}

impl<'a> ParserError<'a> {
    /// A failure at `input` carrying the specific violation `cause`.
    pub const fn with_cause(input: &'a str, cause: Error) -> Self {
        Self {
            input,
            cause: Some(cause),
        }
    }

    /// A plain failure at `input`, reported as a generic [`Error::Syntax`].
    pub const fn plain(input: &'a str) -> Self {
        Self { input, cause: None }
    }
}

impl<'a> nom::error::ParseError<&'a str> for ParserError<'a> {
    fn from_error_kind(input: &'a str, _kind: nom::error::ErrorKind) -> Self {
        Self::plain(input)
    }

    fn append(_input: &'a str, _kind: nom::error::ErrorKind, other: Self) -> Self {
        other
    }

    fn or(self, other: Self) -> Self {
        // Prefer the branch that identified a typed cause (nom's default keeps
        // `other`). Defensive: causes currently travel as `Failure`, which
        // short-circuits `alt` before any merge happens.
        if self.cause.is_some() { self } else { other }
    }
}

/// Parses a complete JSONPath query string into a syntactic [`Query`].
///
/// This consumes the *entire* input: trailing junk is a syntax error.
///
/// # Errors
///
/// Returns [`Error::Syntax`] (with the byte offset at which parsing failed) if the
/// string is not a grammatically valid JSONPath query, or [`Error::IntegerOutOfRange`]
/// if an array index or slice bound lies outside the I-JSON safe range.
pub fn parse(input: &str) -> Result<Query, Error> {
    match all_consuming(query::query).parse(input) {
        Ok((_rest, query)) => Ok(query),
        Err(err) => Err(map_nom_error(input, err)),
    }
}

/// Converts a nom error into a crate [`Error`]: a typed cause carried by the
/// [`ParserError`] is returned verbatim; anything else becomes [`Error::Syntax`]
/// with the byte offset of the failure recovered from the unconsumed remainder.
fn map_nom_error(full: &str, err: nom::Err<ParserError<'_>>) -> Error {
    let (remainder_len, cause) = match err {
        nom::Err::Error(inner) | nom::Err::Failure(inner) => (inner.input.len(), inner.cause),
        nom::Err::Incomplete(_) => (0, None),
    };
    cause.unwrap_or_else(|| Error::Syntax {
        position: full.len().saturating_sub(remainder_len),
        message: "the input is not a valid JSONPath query".to_owned(),
    })
}

/// rule: `S = *B` where `B = %x20 / %x09 / %x0A / %x0D` — optional blank space.
///
/// Consumes zero or more blank bytes (space, tab, LF, CR — *not* the full Unicode
/// whitespace set) and returns the consumed span. Apply this only at the exact
/// points the grammar permits whitespace.
pub fn s(input: &str) -> ParseResult<'_, &str> {
    take_while(|c| matches!(c, ' ' | '\t' | '\n' | '\r')).parse(input)
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::Error;

    #[test]
    fn out_of_range_index_reports_the_typed_variant() {
        // 2^53 is one past the I-JSON safe range: the typed variant must survive
        // the trip through nom rather than collapsing into a generic syntax error.
        assert_eq!(
            parse("$[9007199254740992]"),
            Err(Error::IntegerOutOfRange {
                repr: "9007199254740992".to_owned(),
            }),
            "an out-of-range index selector reports IntegerOutOfRange"
        );
    }

    #[test]
    fn out_of_range_slice_bound_reports_the_typed_variant() {
        assert_eq!(
            parse("$.a[:9007199254740992]"),
            Err(Error::IntegerOutOfRange {
                repr: "9007199254740992".to_owned(),
            }),
            "an out-of-range slice bound reports IntegerOutOfRange"
        );
    }

    #[test]
    fn i64_overflowing_index_keeps_its_text_form() {
        // The variant stores the integer as text precisely because values like this
        // have no i64 representation at all.
        assert_eq!(
            parse("$[99999999999999999999]"),
            Err(Error::IntegerOutOfRange {
                repr: "99999999999999999999".to_owned(),
            }),
            "an i64-overflowing index reports IntegerOutOfRange with the verbatim text"
        );
    }

    #[test]
    fn out_of_range_index_inside_a_filter_reports_the_typed_variant() {
        assert_eq!(
            parse("$[?@.a[9007199254740992] == 1]"),
            Err(Error::IntegerOutOfRange {
                repr: "9007199254740992".to_owned(),
            }),
            "an out-of-range singular-query index reports IntegerOutOfRange"
        );
    }

    #[test]
    fn plain_syntax_errors_still_report_syntax() {
        assert!(
            matches!(parse("$.a["), Err(Error::Syntax { .. })),
            "an unclosed bracket is still a generic syntax error"
        );
        // A huge *number literal* in a filter comparison is not an index: it takes
        // the `number` rule, which is deliberately not range-restricted.
        assert!(
            parse("$[?@.a == 99999999999999999999]").is_ok(),
            "filter number literals are not range-checked"
        );
    }
}
