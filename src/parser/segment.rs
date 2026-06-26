//! Segment parsing — child and descendant segments, including bracketed selection
//! lists and the dot/double-dot shorthands.
//!
//! ```abnf
//! segment               = child-segment / descendant-segment
//! child-segment         = bracketed-selection /
//!                         ("." (wildcard-selector / member-name-shorthand))
//! bracketed-selection   = "[" S selector *(S "," S selector) S "]"
//! descendant-segment    = ".." (bracketed-selection /
//!                               wildcard-selector /
//!                               member-name-shorthand)
//! member-name-shorthand = name-first *name-char
//! name-first            = ALPHA / "_" / %x80-D7FF / %xE000-10FFFF
//! name-char             = name-first / DIGIT
//! ALPHA                 = %x41-5A / %x61-7A    ; A-Z / a-z
//! ```
//!
//! Implementation notes:
//! * Shorthands are desugared into the bracketed form: `.name` / `..name` carry a
//!   single [`Selector::Name`]; `.*` / `..*` carry a single [`Selector::Wildcard`].
//! * `descendant-segment` is tried before `child-segment` so `..` is not mistaken
//!   for a child `.`; `..` on its own is not a valid segment (it must be followed by
//!   a selection, wildcard, or shorthand name).
//! * `member-name-shorthand` is an unquoted, unescaped identifier; it is built from
//!   its first character and the run of following name characters (no `recognize`).

use super::s;
use super::selector::{selector, wildcard};
use crate::ast::{Segment, Selector};
use nom::branch::alt;
use nom::bytes::complete::{tag, take_while};
use nom::character::complete::{char, satisfy};
use nom::combinator::map;
use nom::multi::separated_list1;
use nom::sequence::{delimited, pair, preceded};
use nom::{IResult, Parser};

/// rule: `segment = child-segment / descendant-segment`.
pub fn segment(input: &str) -> IResult<&str, Segment> {
    alt((descendant_segment, child_segment)).parse(input)
}

fn child_segment(input: &str) -> IResult<&str, Segment> {
    alt((
        map(bracketed_selection, Segment::Child),
        map(preceded(char('.'), shorthand_selectors), Segment::Child),
    ))
    .parse(input)
}

fn descendant_segment(input: &str) -> IResult<&str, Segment> {
    let (input, _dots) = tag("..").parse(input)?;
    let (input, selectors) = alt((bracketed_selection, shorthand_selectors)).parse(input)?;
    Ok((input, Segment::Descendant(selectors)))
}

/// `wildcard-selector / member-name-shorthand`, wrapped as a one-element selector
/// list (the shared tail of both the `.`/`..` shorthand forms).
fn shorthand_selectors(input: &str) -> IResult<&str, Vec<Selector>> {
    alt((
        map(wildcard, |w| vec![w]),
        map(member_name_shorthand, |name| vec![Selector::Name(name)]),
    ))
    .parse(input)
}

/// rule: `bracketed-selection = "[" S selector *(S "," S selector) S "]"`.
///
/// Returns the comma-separated selector list (used by both child and descendant
/// segments). At least one selector is required.
pub fn bracketed_selection(input: &str) -> IResult<&str, Vec<Selector>> {
    delimited(
        pair(char('['), s),
        separated_list1(delimited(s, char(','), s), selector),
        pair(s, char(']')),
    )
    .parse(input)
}

const fn is_name_first(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c >= '\u{80}'
}

const fn is_name_char(c: char) -> bool {
    is_name_first(c) || c.is_ascii_digit()
}

/// rule: `member-name-shorthand = name-first *name-char` — an unquoted member name.
pub fn member_name_shorthand(input: &str) -> IResult<&str, String> {
    let (rest, (first, tail)) =
        pair(satisfy(is_name_first), take_while(is_name_char)).parse(input)?;
    let mut name = String::with_capacity(first.len_utf8() + tail.len());
    name.push(first);
    name.push_str(tail);
    Ok((rest, name))
}

#[cfg(test)]
mod tests {
    use super::{bracketed_selection, member_name_shorthand, segment};
    use crate::ast::{Segment, Selector};

    #[test]
    fn dot_name_desugars_to_child_name() {
        let (rest, seg) = segment(".store").expect("should parse");
        assert_eq!(rest, "", "should consume the segment");
        assert_eq!(
            seg,
            Segment::Child(vec![Selector::Name("store".to_owned())]),
            "`.store` is a child segment with one name selector"
        );
    }

    #[test]
    fn dot_wildcard_desugars() {
        let (_rest, seg) = segment(".*").expect("should parse");
        assert_eq!(
            seg,
            Segment::Child(vec![Selector::Wildcard]),
            "`.*` is a child segment with a wildcard"
        );
    }

    #[test]
    fn descendant_wildcard_desugars() {
        let (_rest, seg) = segment("..*").expect("should parse");
        assert_eq!(
            seg,
            Segment::Descendant(vec![Selector::Wildcard]),
            "`..*` is a descendant segment with a wildcard"
        );
    }

    #[test]
    fn descendant_bracketed() {
        let (_rest, seg) = segment("..['a']").expect("should parse");
        assert_eq!(
            seg,
            Segment::Descendant(vec![Selector::Name("a".to_owned())]),
            "`..['a']` is a descendant segment with a name selector"
        );
    }

    #[test]
    fn bare_double_dot_is_invalid() {
        assert!(
            segment("..").is_err(),
            "`..` on its own is not a valid segment"
        );
    }

    #[test]
    fn bracketed_list_allows_internal_whitespace() {
        let (rest, selectors) = bracketed_selection("[ 0 , 1 ]").expect("should parse");
        assert_eq!(rest, "", "consumes the whole bracket");
        assert_eq!(selectors.len(), 2, "two index selectors, comma-separated");
    }

    #[test]
    fn empty_brackets_are_invalid() {
        assert!(
            bracketed_selection("[]").is_err(),
            "a bracketed selection requires at least one selector"
        );
    }

    #[test]
    fn shorthand_name_stops_at_dot() {
        let (rest, name) = member_name_shorthand("a1_b.c").expect("should parse");
        assert_eq!(
            name, "a1_b",
            "name chars are letters, digits, and underscore"
        );
        assert_eq!(rest, ".c", "the `.` terminates the shorthand name");
    }

    #[test]
    fn shorthand_name_rejects_leading_digit() {
        assert!(
            member_name_shorthand("1abc").is_err(),
            "a shorthand name may not start with a digit"
        );
    }
}
