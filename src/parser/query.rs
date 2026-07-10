//! Top-level query parsing.
//!
//! ```abnf
//! jsonpath-query  = root-identifier segments
//! segments        = *(S segment)
//! root-identifier = "$"
//! ```
//!
//! Implementation notes:
//! * [`query`] is the entry point used by [`super::parse`]; it expects the leading
//!   `$` and then zero or more segments, each optionally preceded by blank space
//!   (`S`). It does **not** itself enforce end-of-input — `super::parse` wraps it in
//!   `all_consuming`, so a trailing remainder becomes a syntax error there.
//! * There is no leading `S` before `root-identifier`: a query may not start with
//!   whitespace.

use super::ParseResult;
use super::s;
use super::segment::segment;
use crate::ast::{Query, Segment};
use nom::Parser;
use nom::character::complete::char;
use nom::multi::many0;
use nom::sequence::preceded;

/// rule: `jsonpath-query = root-identifier segments`.
pub fn query(input: &str) -> ParseResult<'_, Query> {
    let (input, _root) = char('$').parse(input)?;
    let (input, segments) = segments(input)?;
    Ok((input, Query { segments }))
}

/// rule: `segments = *(S segment)` — zero or more segments, each preceded by
/// optional blank space.
pub fn segments(input: &str) -> ParseResult<'_, Vec<Segment>> {
    many0(preceded(s, segment)).parse(input)
}

#[cfg(test)]
mod tests {
    use super::query;
    use crate::ast::{Query, Segment, Selector};
    use crate::parser::parse;

    #[test]
    fn bare_root_is_an_empty_query() {
        let (rest, q) = query("$").expect("should parse");
        assert_eq!(rest, "", "consumes the root identifier");
        assert_eq!(q, Query { segments: vec![] }, "`$` alone selects the root");
    }

    #[test]
    fn root_with_one_child_name() {
        let (_rest, q) = query("$.a").expect("should parse");
        assert_eq!(
            q,
            Query {
                segments: vec![Segment::Child(vec![Selector::Name("a".to_owned())])],
            },
            "`$.a` is one child segment with a name selector"
        );
    }

    #[test]
    fn parses_mixed_segments_end_to_end() {
        let q = parse("$.store.book[0]").expect("should compile");
        assert_eq!(
            q.segments.len(),
            3,
            "`.store`, `.book`, and `[0]` are three segments"
        );
    }

    #[test]
    fn parses_descendant_and_bracket() {
        let q = parse("$..book[0, 1]").expect("should compile");
        assert_eq!(
            q.segments.len(),
            2,
            "`..book` and `[0, 1]` are two segments"
        );
    }

    #[test]
    fn rejects_leading_whitespace() {
        assert!(
            parse(" $").is_err(),
            "a query may not start with whitespace"
        );
    }

    #[test]
    fn rejects_trailing_junk() {
        assert!(
            parse("$.a%%%").is_err(),
            "all-consuming: trailing junk is a syntax error"
        );
    }
}
