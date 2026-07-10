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
//!   carrying the offending literal (see [`ParserError`]) — surfaced to [`parse`]'s
//!   caller as [`Error::IntegerOutOfRange`] instead of collapsing into a generic
//!   syntax error.
//!
//! [RFC 9535 Appendix A]: https://www.rfc-editor.org/rfc/rfc9535#appendix-A

use crate::Error;
use crate::ast::Query;
use crate::error::MAX_NESTING_DEPTH;
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
/// with [`ParserError`] as the error type, so a specific failure can travel through
/// nom to [`parse`]'s caller instead of collapsing into a generic syntax error.
pub type ParseResult<'a, O> = nom::IResult<&'a str, O, ParserError<'a>>;

/// The error type threaded through every combinator in this parser.
///
/// Deliberately shaped to cost exactly what nom's own error costs — 24 bytes,
/// `Copy`, no drop glue — because combinators construct and *discard* these on
/// every backtracking `alt` branch, so the error type's width and droppability tax
/// each such return. Two earlier designs measured that tax on `parse/*`: carrying
/// an owned crate [`Error`] inline regressed +7…+33% (every `Result` widened by
/// ~32 bytes), and `Option<Box<Error>>` still regressed the filter rows +6…+11%
/// (drop glue forces every discarded branch error to be checked). Borrowing the
/// offending text from the query keeps this type trivial; the owned [`Error`] is
/// materialized once, in [`map_nom_error`], on the already-failed path.
#[derive(Debug, Clone, Copy)]
pub enum ParserError<'a> {
    /// A generic failure; the payload is the unconsumed remainder, from which
    /// [`parse`] recovers the byte offset ([`Error::Syntax`]).
    Plain(&'a str),
    /// An index or slice integer outside the I-JSON safe range (or overflowing
    /// `i64` outright); the payload is the literal, borrowed from the query
    /// ([`Error::IntegerOutOfRange`]). Raised by [`number::int`] as a
    /// [`nom::Err::Failure`] so it aborts alternation and reaches the caller
    /// instead of being masked by a sibling branch's generic error.
    IntegerOutOfRange(&'a str),
}

impl<'a> nom::error::ParseError<&'a str> for ParserError<'a> {
    fn from_error_kind(input: &'a str, _kind: nom::error::ErrorKind) -> Self {
        Self::Plain(input)
    }

    fn append(_input: &'a str, _kind: nom::error::ErrorKind, other: Self) -> Self {
        other
    }

    fn or(self, other: Self) -> Self {
        // nom's default: keep the later branch's error. A specific failure never
        // reaches a merge — it travels as `Failure`, which short-circuits `alt`.
        other
    }
}

/// Parses a complete JSONPath query string into a syntactic [`Query`].
///
/// This consumes the *entire* input: trailing junk is a syntax error.
///
/// # Errors
///
/// Returns [`Error::Syntax`] (with the byte offset at which parsing failed) if the
/// string is not a grammatically valid JSONPath query, [`Error::IntegerOutOfRange`]
/// if an array index or slice bound lies outside the I-JSON safe range, or
/// [`Error::NestingTooDeep`] if brackets/parentheses nest beyond
/// [`MAX_NESTING_DEPTH`].
pub fn parse(input: &str) -> Result<Query, Error> {
    check_nesting(input)?;
    match all_consuming(query::query).parse(input) {
        Ok((_rest, query)) => Ok(query),
        Err(err) => Err(map_nom_error(input, &err)),
    }
}

/// Rejects input nested deeper than [`MAX_NESTING_DEPTH`] before any recursive
/// grammar rule runs. The parser recurses once per nested filter, parenthesized
/// expression, and function call, so without this gate a ~10 kB hostile query
/// overflows the stack — a process abort, not a catchable panic (measured: aborts
/// near 1 000 levels in debug builds, under 10 000 in release).
///
/// Depth is the maximum number of *simultaneously open* `(`/`[` outside string
/// literals: every construct the grammar recurses into opens one of those two
/// characters, and sequential segments close each bracket before the next opens,
/// so flat queries of any length never accumulate depth. String literals are
/// skipped under the grammar's quote/escape rules — a bracket inside `$['((((']`
/// is content, not structure. Unbalanced input needs no special care: unmatched
/// opens are exactly the attack shape (still counted), and unmatched closes
/// saturate at zero (the parser rejects them later on its own).
fn check_nesting(input: &str) -> Result<(), Error> {
    // Exceeding the limit requires at least MAX_NESTING_DEPTH + 1 opening
    // brackets, i.e. at least that many bytes — shorter input cannot trip it, so
    // the overwhelmingly common short query skips the scan entirely.
    if input.len() <= MAX_NESTING_DEPTH {
        return Ok(());
    }
    let mut depth = 0_usize;
    let mut quote: Option<char> = None;
    let mut chars = input.char_indices();
    while let Some((position, c)) = chars.next() {
        match quote {
            Some(active) => {
                if c == '\\' {
                    chars.next();
                } else if c == active {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' | '[' => {
                    depth += 1;
                    if depth > MAX_NESTING_DEPTH {
                        return Err(Error::NestingTooDeep { position });
                    }
                }
                ')' | ']' => depth = depth.saturating_sub(1),
                _ => {}
            },
        }
    }
    Ok(())
}

/// Converts a nom error into a crate [`Error`] — the single place the owned error
/// is materialized, so the per-branch [`ParserError`] values stay allocation-free.
/// A specific failure maps to its variant; a plain one becomes [`Error::Syntax`]
/// with the byte offset recovered from the unconsumed remainder.
fn map_nom_error(full: &str, err: &nom::Err<ParserError<'_>>) -> Error {
    let syntax = |position: usize| Error::Syntax {
        position,
        message: "the input is not a valid JSONPath query".to_owned(),
    };
    match err {
        nom::Err::Error(inner) | nom::Err::Failure(inner) => match *inner {
            ParserError::Plain(remainder) => syntax(full.len().saturating_sub(remainder.len())),
            ParserError::IntegerOutOfRange(literal) => Error::IntegerOutOfRange {
                repr: literal.to_owned(),
            },
        },
        nom::Err::Incomplete(_) => syntax(full.len()),
    }
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
    use crate::error::MAX_NESTING_DEPTH;

    #[test]
    fn hostile_nesting_is_rejected_not_a_stack_overflow() {
        // Regression: before the depth gate, both shapes overflowed the stack (a
        // process abort) at a few thousand levels — roughly 10 kB of input.
        let parens = format!("$[?{}", "(".repeat(100_000));
        assert!(
            matches!(parse(&parens), Err(Error::NestingTooDeep { .. })),
            "unclosed nested parens are rejected by the depth gate"
        );
        let filters = format!("${}", "[?@".repeat(100_000));
        assert!(
            matches!(parse(&filters), Err(Error::NestingTooDeep { .. })),
            "nested filter brackets are rejected by the depth gate"
        );
    }

    #[test]
    fn nesting_at_the_limit_still_parses() {
        // Exactly MAX_NESTING_DEPTH simultaneous opens: the filter `[` plus the
        // parens. Doubles as proof the limit is safely parseable on a test thread.
        let depth = MAX_NESTING_DEPTH - 1;
        let query = format!("$[?{}@.a{}]", "(".repeat(depth), ")".repeat(depth));
        assert!(
            parse(&query).is_ok(),
            "a query at the depth limit is accepted"
        );

        // One level past the limit is grammatically valid — only the gate rejects it.
        let query = format!(
            "$[?{}@.a{}]",
            "(".repeat(MAX_NESTING_DEPTH),
            ")".repeat(MAX_NESTING_DEPTH)
        );
        assert!(
            matches!(parse(&query), Err(Error::NestingTooDeep { .. })),
            "one level past the limit is rejected by the gate, not the grammar"
        );
    }

    #[test]
    fn flat_query_length_is_not_limited() {
        // Sequential brackets close before the next opens — depth never exceeds 1,
        // so query *length* is unconstrained by the gate.
        let indexes = format!("${}", "[0]".repeat(10_000));
        assert!(
            parse(&indexes).is_ok(),
            "10k sequential index segments accumulate no nesting depth"
        );
        let names = format!("${}", ".a".repeat(10_000));
        assert!(
            parse(&names).is_ok(),
            "10k shorthand name segments accumulate no nesting depth"
        );
    }

    #[test]
    fn brackets_inside_string_literals_do_not_count_as_nesting() {
        let name = format!("$['{}']", "(".repeat(1_000));
        assert!(
            parse(&name).is_ok(),
            "parens inside a quoted member name are content, not nesting"
        );
        let literal = format!("$[?@.a == '{}']", "[".repeat(1_000));
        assert!(
            parse(&literal).is_ok(),
            "brackets inside a filter string literal are content, not nesting"
        );
    }

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
