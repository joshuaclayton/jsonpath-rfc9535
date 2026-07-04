//! Selector parsing — the things that appear inside `[ ... ]` (and their
//! shorthands).
//!
//! ```abnf
//! selector          = name-selector /
//!                     wildcard-selector /
//!                     slice-selector /
//!                     index-selector /
//!                     filter-selector
//! wildcard-selector = "*"
//! index-selector    = int                  ; decimal integer
//! ```
//!
//! Implementation notes:
//! * Ordering matters: `slice-selector` is tried before `index-selector`, because a
//!   bare `int` is a prefix of a slice (`1` vs `1:3`). When the slice branch fails
//!   for lack of a `:`, `alt` backtracks and the index branch matches.
//! * `name-selector` delegates to [`string_literal`], the slice to [`slice()`], the index
//!   to [`int`], and the filter to [`filter_selector`].

use super::filter::filter_selector;
use super::number::int;
use super::slice::slice;
use super::string::string_literal;
use crate::ast::Selector;
use nom::branch::alt;
use nom::character::complete::char;
use nom::combinator::{map, value};
use nom::{IResult, Parser};

/// rule: `selector` — any one of the five selector kinds.
pub fn selector(input: &str) -> IResult<&str, Selector> {
    alt((
        map(string_literal, Selector::Name),
        wildcard,
        map(slice, Selector::Slice),
        map(int, Selector::Index),
        map(filter_selector, Selector::Filter),
    ))
    .parse(input)
}

/// rule: `wildcard-selector = "*"`.
pub fn wildcard(input: &str) -> IResult<&str, Selector> {
    value(Selector::Wildcard, char('*')).parse(input)
}

#[cfg(test)]
mod tests {
    use super::selector;
    use crate::ast::{JsonInt, Selector};

    #[test]
    fn parses_name_selector() {
        let (rest, sel) = selector("'name'").expect("should parse");
        assert_eq!(rest, "", "should consume the selector");
        assert_eq!(
            sel,
            Selector::Name("name".to_owned()),
            "single-quoted name selector"
        );
    }

    #[test]
    fn parses_wildcard() {
        let (_rest, sel) = selector("*").expect("should parse");
        assert_eq!(sel, Selector::Wildcard, "`*` is the wildcard selector");
    }

    #[test]
    fn prefers_slice_over_index() {
        let (rest, sel) = selector("1:3").expect("should parse");
        assert_eq!(rest, "", "consumes the whole slice");
        assert!(
            matches!(sel, Selector::Slice(_)),
            "`1:3` must parse as a slice, not the index 1 with trailing input"
        );
    }

    #[test]
    fn parses_index() {
        let (_rest, sel) = selector("-2").expect("should parse");
        assert_eq!(
            sel,
            Selector::Index(JsonInt::new(-2).expect("in range")),
            "negative index selector"
        );
    }
}
