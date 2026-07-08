//! I-Regexp ([RFC 9485]) support for the `match()` and `search()` function
//! extensions, backed by the [`regex`] crate.
//!
//! Per [RFC 9485 §5.4], an I-Regexp is mapped to an `RE2`/PCRE-style regexp by
//! replacing each unescaped `.` outside a character class with `[^\n\r]` (an
//! I-Regexp dot matches any character except the line terminators), then anchoring
//! with `\A(?:…)\z` for a full match (`match()`) or leaving it unanchored for a
//! substring search (`search()`). The remaining I-Regexp constructs are a subset the
//! `regex` crate already accepts.
//!
//! [RFC 9485]: https://www.rfc-editor.org/rfc/rfc9485
//! [RFC 9485 §5.4]: https://www.rfc-editor.org/rfc/rfc9485#section-5.4

/// Whether `pattern` is free of every I-Regexp metacharacter, so matching it reduces
/// to plain string comparison: `match()` is equality, `search()` is substring
/// containment — no regex engine involved.
///
/// The excluded set is exactly the complement of RFC 9485's `NormalChar` rule
/// (`( ) * + . ? [ \ ] { | }`), plus `^` and `$`: the ABNF admits those two as
/// ordinary characters, but the jsonpath-compliance-test-suite pins them to their
/// PCRE *anchor* semantics ("explicit caret"/"explicit dollar" cases), which is what
/// the regex-backed path implements — so a pattern containing them must keep taking
/// that path. A byte scan is exact here: every metacharacter is ASCII, and no UTF-8
/// continuation byte overlaps the ASCII range.
///
/// A consequence of matching the `NormalChar` complement: every plain pattern is
/// also a *valid* I-Regexp, so the "invalid pattern yields false" rule (RFC 9535
/// §2.4.6) can never apply to one.
pub fn is_plain(pattern: &str) -> bool {
    !pattern.bytes().any(|byte| {
        matches!(
            byte,
            b'(' | b')'
                | b'*'
                | b'+'
                | b'.'
                | b'?'
                | b'['
                | b'\\'
                | b']'
                | b'{'
                | b'|'
                | b'}'
                | b'^'
                | b'$'
        )
    })
}

/// Translates `pattern` from I-Regexp to a `regex` pattern and compiles it, anchoring
/// for a full match when `anchored` is true. Returns `None` if the pattern is not a
/// valid regular expression (so the caller yields `LogicalFalse`, per RFC 9535 §2.4.6).
pub fn build(pattern: &str, anchored: bool) -> Option<regex::Regex> {
    let translated = translate(pattern);
    let assembled = if anchored {
        format!(r"\A(?:{translated})\z")
    } else {
        translated
    };
    regex::Regex::new(&assembled).ok()
}

/// Replaces each unescaped `.` outside a character class with `[^\n\r]`, leaving every
/// other construct for the `regex` crate to interpret.
fn translate(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut in_class = false;
    let mut escaped = false;
    for c in pattern.chars() {
        if escaped {
            out.push('\\');
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '[' if !in_class => {
                in_class = true;
                out.push('[');
            }
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            '.' if !in_class => out.push_str(r"[^\n\r]"),
            other => out.push(other),
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{build, is_plain};

    #[test]
    fn plain_patterns_have_no_metacharacters() {
        assert!(is_plain("Number 1"), "letters, digits, space");
        assert!(is_plain(""), "empty pattern is plain");
        assert!(is_plain("a-b,c/d"), "NormalChar punctuation is plain");
        assert!(is_plain("naïve café ✓"), "non-ASCII is plain");
        for pattern in [
            "a.c", "a*", "a+", "a?", "[jk]", "a]b", "(x)", "x)", "a{2}", "b}", "a|b", r"a\.b",
            "^ab", "bc$",
        ] {
            assert!(
                !is_plain(pattern),
                "{pattern:?} contains a metacharacter (or CTS-pinned anchor)"
            );
        }
    }

    #[test]
    fn match_is_anchored_and_dot_excludes_newline() {
        let re = build("a.c", true).expect("valid pattern");
        assert!(re.is_match("abc"), "full match of `a.c`");
        assert!(!re.is_match("xabcx"), "anchored: no substring match");
        assert!(
            !re.is_match("a\nc"),
            "an I-Regexp dot does not match a newline"
        );
    }

    #[test]
    fn search_matches_a_substring() {
        let re = build("[jk]", false).expect("valid pattern");
        assert!(re.is_match("kilo"), "substring search succeeds");
        assert!(!re.is_match("xyz"), "no matching substring");
    }

    #[test]
    fn dot_inside_a_class_is_literal() {
        let re = build("[.]", true).expect("valid pattern");
        assert!(re.is_match("."), "a dot in a character class is literal");
        assert!(!re.is_match("x"), "it matches only a literal dot");
    }

    #[test]
    fn invalid_pattern_returns_none() {
        assert!(
            build("(", true).is_none(),
            "an unbalanced group is not a valid regexp"
        );
    }
}
