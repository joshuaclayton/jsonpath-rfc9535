//! String-literal parsing (the `name-selector` payload and string `literal`s).
//!
//! Produces the **decoded** string — escapes are resolved and surrogate pairs
//! combined, so the returned `String` is the actual member name / literal value, not
//! the source text. Both single- and double-quoted forms are supported; the only
//! difference is which quote is the active (escapable) one.
//!
//! ```abnf
//! string-literal  = %x22 *double-quoted %x22 /     ; "string"
//!                   %x27 *single-quoted %x27       ; 'string'
//! double-quoted   = unescaped / %x27 / ESC %x22 / ESC escapable
//! single-quoted   = unescaped / %x22 / ESC %x27 / ESC escapable
//! ESC             = %x5C                           ; \ backslash
//! unescaped       = %x20-21 / %x23-26 / %x28-5B / %x5D-D7FF / %xE000-10FFFF
//! escapable       = %x62 / %x66 / %x6E / %x72 / %x74 /   ; b f n r t
//!                   "/" / "\" / (%x75 hexchar)           ; / \ uXXXX
//! hexchar         = non-surrogate /
//!                   (high-surrogate "\" %x75 low-surrogate)
//! ```
//!
//! Because the value is built character by character with `fold_many0`, this module
//! never uses `recognize` and so is unaffected by the nom 8.0.0 span bug noted in
//! [`super::number`]. The only non-combinator logic is [`combine_surrogate`], the
//! surrogate-pair arithmetic RFC 9535 requires and that no parser library performs
//! for you.

use super::{ParseResult, ParserError};
use nom::Parser;
use nom::branch::alt;
use nom::bytes::complete::{tag, take_while_m_n};
use nom::character::complete::{char, satisfy};
use nom::combinator::{map_opt, value};
use nom::multi::fold_many0;
use nom::sequence::{delimited, preceded};

/// Builds a recoverable nom error positioned at `input` (used for the surrogate /
/// scalar-validity failures the grammar requires).
const fn err(input: &str) -> nom::Err<ParserError<'_>> {
    nom::Err::Error(ParserError::Plain(input))
}

/// Combines a UTF-16 surrogate pair into a scalar value, or returns `None` if `low`
/// is not a valid low surrogate (`%xDC00-DFFF`).
fn combine_surrogate(high: u16, low: u16) -> Option<char> {
    if (0xDC00..=0xDFFF).contains(&low) {
        let code = 0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
        char::from_u32(code)
    } else {
        None
    }
}

/// rule: `4HEXDIG` — exactly four hex digits, as a `u16`. Hex digits are
/// case-insensitive per RFC 9535 §2.3.1.2.
fn hex4(input: &str) -> ParseResult<'_, u16> {
    map_opt(
        take_while_m_n(4, 4, |c: char| c.is_ascii_hexdigit()),
        |digits: &str| u16::from_str_radix(digits, 16).ok(),
    )
    .parse(input)
}

/// rule: `hexchar` (with the leading `\u` already consumed) — one Unicode scalar,
/// combining a high surrogate with the `\u` low surrogate that must follow it.
fn unicode(input: &str) -> ParseResult<'_, char> {
    let (rest, high) = hex4(input)?;
    match high {
        0xD800..=0xDBFF => map_opt(preceded(tag(r"\u"), hex4), move |low| {
            combine_surrogate(high, low)
        })
        .parse(rest),
        0xDC00..=0xDFFF => Err(err(input)),
        scalar => char::from_u32(u32::from(scalar))
            .map(|c| (rest, c))
            .ok_or_else(|| err(input)),
    }
}

/// rule: `ESC escapable` / `ESC %x22` / `ESC %x27` — one escape sequence, decoded to
/// the character it denotes. Only the *active* quote `q` is escapable, so `\"` is
/// invalid inside `'...'` (and `\'` is invalid inside `"..."`).
fn escape(q: char) -> impl FnMut(&str) -> ParseResult<'_, char> {
    move |input| {
        preceded(
            char('\\'),
            alt((
                value('\u{8}', char('b')),
                value('\u{c}', char('f')),
                value('\n', char('n')),
                value('\r', char('r')),
                value('\t', char('t')),
                value('/', char('/')),
                value('\\', char('\\')),
                value(q, char(q)),
                preceded(char('u'), unicode),
            )),
        )
        .parse(input)
    }
}

/// rule: `unescaped` — one literal character: anything except the active quote, the
/// backslash, or a control character below `%x20`.
fn unescaped(q: char) -> impl FnMut(&str) -> ParseResult<'_, char> {
    move |input| satisfy(|c| c != q && c != '\\' && c >= '\u{20}').parse(input)
}

/// A complete quoted string using the quote character `q`, decoded to a `String`.
fn quoted(q: char) -> impl FnMut(&str) -> ParseResult<'_, String> {
    move |input| {
        delimited(
            char(q),
            fold_many0(alt((escape(q), unescaped(q))), String::new, |mut acc, c| {
                acc.push(c);
                acc
            }),
            char(q),
        )
        .parse(input)
    }
}

/// rule: `name-selector = string-literal` — a single- or double-quoted, escape-decoded
/// string.
pub fn string_literal(input: &str) -> ParseResult<'_, String> {
    alt((quoted('"'), quoted('\''))).parse(input)
}

#[cfg(test)]
mod tests {
    use super::string_literal;

    fn parse(input: &str) -> String {
        let (rest, value) = string_literal(input).expect("should parse");
        assert_eq!(rest, "", "should consume the whole literal: {input}");
        value
    }

    #[test]
    fn parses_double_and_single_quoted() {
        assert_eq!(parse(r#""ab""#), "ab", "double-quoted");
        assert_eq!(parse(r"'ab'"), "ab", "single-quoted");
    }

    #[test]
    fn parses_empty() {
        assert_eq!(parse(r#""""#), "", "empty double-quoted string");
    }

    #[test]
    fn stops_at_closing_quote() {
        let (rest, value) = string_literal(r#""ab"cd"#).expect("should parse");
        assert_eq!(value, "ab", "decodes up to the closing quote");
        assert_eq!(rest, "cd", "leaves the trailing input for the caller");
    }

    #[test]
    fn decodes_simple_escapes() {
        assert_eq!(
            parse(r#""\b\f\n\r\t\/\\""#),
            "\u{8}\u{c}\n\r\t/\\",
            "all single-character escapes decode"
        );
    }

    #[test]
    fn decodes_unicode_escape() {
        assert_eq!(parse("\"\\u0041\""), "A", "\\u0041 is 'A'");
        assert_eq!(
            parse("\"\\u004a\""),
            "J",
            "lower-case hex digits are allowed"
        );
    }

    #[test]
    fn combines_surrogate_pair() {
        assert_eq!(
            parse("\"\\uD83D\\uDE00\""),
            "\u{1F600}",
            "a high+low surrogate pair combines into one scalar"
        );
    }

    #[test]
    fn accepts_literal_astral_character() {
        assert_eq!(
            parse("'\u{1F600}'"),
            "\u{1F600}",
            "a non-ASCII char passes through"
        );
    }

    #[test]
    fn rejects_lone_surrogates() {
        assert!(
            string_literal(r#""\uD800""#).is_err(),
            "a high surrogate with no following low surrogate is invalid"
        );
        assert!(
            string_literal(r#""\uDC00""#).is_err(),
            "a lone low surrogate is invalid"
        );
    }

    #[test]
    fn escapes_and_passes_the_quotes() {
        assert_eq!(
            parse(r#""a\"b""#),
            "a\"b",
            "escaped active quote in double-quoted"
        );
        assert_eq!(
            parse(r"'a\'b'"),
            "a'b",
            "escaped active quote in single-quoted"
        );
        assert_eq!(
            parse(r#""a'b""#),
            "a'b",
            "the other quote is literal in double-quoted"
        );
        assert_eq!(
            parse(r#"'a"b'"#),
            "a\"b",
            "the other quote is literal in single-quoted"
        );
    }

    #[test]
    fn rejects_escaping_the_inactive_quote() {
        assert!(
            string_literal(r#"'\"'"#).is_err(),
            "`\\\"` is not a valid escape inside a single-quoted string"
        );
    }

    #[test]
    fn rejects_raw_control_character() {
        assert!(
            string_literal("\"a\nb\"").is_err(),
            "a raw newline must be written as \\n, not embedded literally"
        );
    }

    #[test]
    fn rejects_unterminated() {
        assert!(
            string_literal(r#""abc"#).is_err(),
            "a string with no closing quote is invalid"
        );
    }
}
