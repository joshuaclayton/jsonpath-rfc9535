//! Array slice selector parsing.
//!
//! ```abnf
//! slice-selector = [start S] ":" S [end S] [":" [S step]]
//! start          = int       ; included in selection
//! end            = int       ; not included in selection
//! step           = int       ; default: 1
//! ```
//!
//! Implementation notes:
//! * Every component is optional; the distinguishing feature of a slice is the first
//!   `:`. `start`, `end`, and `step` are each an [`int`], so each is range-checked
//!   into a [`JsonInt`](crate::ast::JsonInt).
//! * Note the asymmetric whitespace in the ABNF: `S` may appear after `start`, after
//!   the first `:`, after `end`, and before `step` — this follows it exactly.
//! * Defaults (`step` → 1, and the `start`/`end` defaults that depend on the sign of
//!   `step`) are applied during evaluation, not here, so missing components stay
//!   `None` in the resulting [`Slice`].

use super::ParseResult;
use super::number::int;
use super::s;
use crate::ast::Slice;
use nom::Parser;
use nom::character::complete::char;
use nom::combinator::opt;
use nom::sequence::{preceded, terminated};

/// rule: `slice-selector` — `start:end:step` with all parts optional.
pub fn slice(input: &str) -> ParseResult<'_, Slice> {
    let (input, start) = opt(terminated(int, s)).parse(input)?;
    let (input, _colon) = char(':').parse(input)?;
    let (input, _ws) = s(input)?;
    let (input, end) = opt(terminated(int, s)).parse(input)?;
    let (input, step) = opt(preceded(char(':'), opt(preceded(s, int)))).parse(input)?;
    Ok((
        input,
        Slice {
            start,
            end,
            step: step.flatten(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::slice;
    use crate::ast::JsonInt;

    fn ji(value: i64) -> JsonInt {
        JsonInt::new(value).expect("in range")
    }

    #[test]
    fn parses_full_slice() {
        let (rest, s) = slice("1:5:2").expect("should parse");
        assert_eq!(rest, "", "should consume the slice");
        assert_eq!(s.start, Some(ji(1)), "start = 1");
        assert_eq!(s.end, Some(ji(5)), "end = 5");
        assert_eq!(s.step, Some(ji(2)), "step = 2");
    }

    #[test]
    fn parses_empty_slice() {
        let (_rest, s) = slice(":").expect("should parse");
        assert_eq!(s.start, None, "missing start stays None");
        assert_eq!(s.end, None, "missing end stays None");
        assert_eq!(s.step, None, "missing step stays None");
    }

    #[test]
    fn parses_reverse_slice() {
        let (_rest, s) = slice("::-1").expect("should parse");
        assert_eq!(s.start, None, "no start");
        assert_eq!(s.end, None, "no end");
        assert_eq!(s.step, Some(ji(-1)), "negative step is allowed");
    }

    #[test]
    fn parses_start_only() {
        let (rest, s) = slice("2:").expect("should parse");
        assert_eq!(rest, "", "consumes `2:`");
        assert_eq!(s.start, Some(ji(2)), "start = 2");
        assert_eq!(s.end, None, "open-ended");
    }

    #[test]
    fn allows_whitespace_around_components() {
        let (rest, s) = slice("1 : 5 : 2").expect("should parse");
        assert_eq!(rest, "", "consumes the spaced slice");
        assert_eq!(s.start, Some(ji(1)), "start = 1");
        assert_eq!(s.end, Some(ji(5)), "end = 5");
        assert_eq!(s.step, Some(ji(2)), "step = 2");
    }

    #[test]
    fn requires_a_colon() {
        assert!(
            slice("3").is_err(),
            "a bare integer is not a slice (no colon)"
        );
    }
}
