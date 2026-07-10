//! Integer and number parsing.
//!
//! Two distinct rules live here:
//! * `int` — the integer used by index and slice selectors. It must be range-checked
//!   into a [`JsonInt`]; values outside the I-JSON safe range are invalid.
//! * `number` — a JSON number literal used inside filter comparisons, returned as a
//!   [`serde_json::Number`].
//!
//! ```abnf
//! int     = "0" / (["-"] DIGIT1 *DIGIT)     ; no leading zeros, optional sign
//! DIGIT1  = %x31-39                         ; 1-9
//! DIGIT   = %x30-39                         ; 0-9
//! number  = (int / "-0") [ frac ] [ exp ]   ; decimal number
//! frac    = "." 1*DIGIT
//! exp     = "e" [ "-" / "+" ] 1*DIGIT
//! ```
//!
//! Implementation notes:
//! * `int` forbids leading zeros, a bare `-`, and a leading `+`. `int` itself
//!   matches *only* `"0"` from an input like `"01"` (leaving `"1"`); the
//!   leading-zero query `$[01]` is rejected because the surrounding bracket is parsed
//!   with `all_consuming`, not because `int` errors.
//! * A valid integer that overflows `i64`, or one within `i64` but outside the
//!   I-JSON safe range, both fail with a [`nom::Err::Failure`] carrying the
//!   offending literal (see [`ParserError`]), surfaced to the caller as
//!   [`Error::IntegerOutOfRange`](crate::Error). A `Failure` aborts alternation on
//!   purpose: once integer text has been consumed in an index or slice position,
//!   the grammar admits no other reading of it, so the specific error must not be
//!   masked by a sibling branch's generic one.
//! * `number` (filter literals) is *not* range-restricted; the recognized text is
//!   handed to `serde_json` to build the [`Number`]. Per ABNF case-insensitivity the
//!   exponent marker may be `e` or `E`.
//!
//! Span capture uses [`recognize_len`] rather than nom's `recognize`: in nom 8.0.0,
//! `recognize`/`consumed` return a too-short span when an inner `digit0`/`take_while`
//! consumes to end-of-input (the underlying parser's remainder is correct, so it is
//! the offset computation that is wrong). `recognize_len` reconstructs the matched
//! slice from the input/remainder *lengths*, which is unaffected.

use super::{ParseResult, ParserError};
use crate::ast::JsonInt;
use nom::Parser;
use nom::branch::alt;
use nom::bytes::complete::tag;
use nom::character::complete::{char, digit0, digit1, one_of};
use nom::combinator::{map_opt, opt};
use nom::sequence::{pair, preceded};
use serde_json::Number;

/// Like nom's `recognize`, but computes the consumed span from input/remainder
/// lengths instead of pointer offsets, working around the nom 8.0.0 span bug
/// described in the module docs. Requires the wrapped parser to leave a remainder
/// that is a suffix of `input` (true for the combinators used here).
fn recognize_len<'a, O, P>(mut parser: P) -> impl FnMut(&'a str) -> ParseResult<'a, &'a str>
where
    P: Parser<&'a str, Output = O, Error = ParserError<'a>>,
{
    move |input: &'a str| {
        let (rest, _) = parser.parse(input)?;
        let consumed = input.len().saturating_sub(rest.len());
        input.get(..consumed).map_or_else(
            || Err(nom::Err::Error(ParserError::Plain(input))),
            |matched| Ok((rest, matched)),
        )
    }
}

/// rule: `int = "0" / (["-"] DIGIT1 *DIGIT)` — recognizes the integer *text* only.
///
/// Shared with [`number`], which prepends this alternative to `"-0"`.
fn int_text(input: &str) -> ParseResult<'_, &str> {
    alt((
        tag("0"),
        recognize_len(preceded(opt(char('-')), pair(one_of("123456789"), digit0))),
    ))
    .parse(input)
}

/// rule: `int` — a decimal integer, range-checked into a [`JsonInt`].
///
/// An integer that parses but lies outside the I-JSON safe range — or overflows
/// `i64` outright, which is a fortiori out of range — raises a
/// [`nom::Err::Failure`] carrying the offending literal (surfaced to the caller as
/// [`Error::IntegerOutOfRange`](crate::Error)): in every position the grammar
/// admits `int`, an out-of-range value has no alternative reading, so the failure
/// is terminal and the specific error survives to the caller (see the module docs).
pub fn int(input: &str) -> ParseResult<'_, JsonInt> {
    let (rest, text) = int_text(input)?;
    text.parse::<i64>()
        .ok()
        .and_then(|value| JsonInt::new(value).ok())
        .map_or_else(
            || Err(nom::Err::Failure(ParserError::IntegerOutOfRange(text))),
            |int| Ok((rest, int)),
        )
}

/// rule: `frac = "." 1*DIGIT`.
fn frac(input: &str) -> ParseResult<'_, &str> {
    recognize_len(pair(char('.'), digit1)).parse(input)
}

/// rule: `exp = "e" [ "-" / "+" ] 1*DIGIT` (the `e` is case-insensitive per ABNF).
fn exp(input: &str) -> ParseResult<'_, &str> {
    recognize_len(pair(one_of("eE"), pair(opt(one_of("+-")), digit1))).parse(input)
}

/// rule: `number = (int / "-0") [ frac ] [ exp ]` — a JSON number literal.
pub fn number(input: &str) -> ParseResult<'_, Number> {
    map_opt(
        recognize_len(pair(alt((int_text, tag("-0"))), pair(opt(frac), opt(exp)))),
        |text: &str| serde_json::from_str::<Number>(text).ok(),
    )
    .parse(input)
}

#[cfg(test)]
mod tests {
    use super::{ParserError, int, number};
    use crate::ast::JsonInt;
    use crate::error::{MAX_SAFE_INTEGER, MIN_SAFE_INTEGER};

    fn ji(value: i64) -> JsonInt {
        JsonInt::new(value).expect("in range")
    }

    #[test]
    fn parses_zero() {
        let (rest, value) = int("0").expect("should parse");
        assert_eq!(rest, "", "should consume the integer");
        assert_eq!(value, ji(0), "should be 0");
    }

    #[test]
    fn parses_negative_index() {
        let (rest, value) = int("-1").expect("should parse");
        assert_eq!(rest, "", "should consume the integer");
        assert_eq!(value, ji(-1), "should be -1");
    }

    #[test]
    fn parses_multi_digit() {
        let (rest, value) = int("-1234").expect("should parse");
        assert_eq!(rest, "", "should consume every digit");
        assert_eq!(value, ji(-1234), "all digits contribute to the value");
    }

    #[test]
    fn matches_only_the_leading_zero() {
        let (rest, value) = int("01").expect("matches the leading 0");
        assert_eq!(value, ji(0), "matches 0 only");
        assert_eq!(rest, "1", "the trailing digit is left for the caller");
    }

    #[test]
    fn rejects_bare_minus() {
        assert!(int("-").is_err(), "a lone `-` is not an integer");
    }

    #[test]
    fn rejects_leading_plus() {
        assert!(int("+1").is_err(), "a leading `+` is not allowed by `int`");
    }

    #[test]
    fn rejects_negative_zero() {
        assert!(
            int("-0").is_err(),
            "`int` does not accept `-0` (only `number` does)"
        );
    }

    #[test]
    fn parses_safe_range_bounds() {
        assert_eq!(
            int("9007199254740991")
                .map(|(_, v)| v)
                .expect("max is in range"),
            ji(MAX_SAFE_INTEGER),
            "2^53-1 is the largest valid index"
        );
        assert_eq!(
            int("-9007199254740991")
                .map(|(_, v)| v)
                .expect("min is in range"),
            ji(MIN_SAFE_INTEGER),
            "-(2^53-1) is the smallest valid index"
        );
    }

    #[test]
    fn rejects_just_outside_safe_range() {
        assert!(
            int("9007199254740992").is_err(),
            "2^53 is outside the I-JSON safe integer range"
        );
    }

    #[test]
    fn out_of_range_int_is_a_terminal_failure_carrying_the_cause() {
        // A `Failure` (not a recoverable `Error`) so alternation cannot mask the
        // typed variant with a generic branch failure.
        let result = int("9007199254740992");
        assert!(
            matches!(
                result,
                Err(nom::Err::Failure(ParserError::IntegerOutOfRange(
                    "9007199254740992"
                )))
            ),
            "expected Failure carrying the offending literal, got {result:?}"
        );
    }

    #[test]
    fn rejects_i64_overflow() {
        assert!(
            int("99999999999999999999").is_err(),
            "an integer overflowing i64 must fail, not panic"
        );
    }

    #[test]
    fn parses_integer_number_literal() {
        let (rest, value) = number("42").expect("should parse");
        assert_eq!(rest, "", "should consume the number");
        assert_eq!(
            value.as_i64(),
            Some(42),
            "integer numbers keep integer form"
        );
    }

    #[test]
    fn parses_decimal_with_exponent() {
        let (rest, value) = number("1.5e2").expect("should parse");
        assert_eq!(rest, "", "should consume the number");
        assert_eq!(value.as_f64(), Some(150.0), "1.5e2 is the number 150");
    }

    #[test]
    fn parses_negative_zero_number() {
        let (_rest, value) = number("-0").expect("`-0` is a valid number literal");
        assert_eq!(value.as_f64(), Some(0.0), "negative zero equals zero");
    }
}
