//! Helpers shared across the scan test harnesses.

use serde_json::Value;

/// Order-insensitive rendering — sorted compact-JSON strings — for comparisons where
/// RFC 9535 leaves ordering unspecified.
pub fn multiset(values: &[Value]) -> Vec<String> {
    let mut rendered: Vec<String> = values.iter().map(ToString::to_string).collect();
    rendered.sort();
    rendered
}

/// Whether the selector contains a descendant segment — `..` *outside* any string
/// literal. A plain substring test would also match quoted names like `$['..']`,
/// whose ordering is fully specified and must not get the multiset waiver.
///
/// Deliberately syntactic and over-inclusive for filters: a descendant inside a
/// filter sub-query (`$[?@..x]`) does not reorder the outer selection, but it still
/// gets the waiver — the harnesses only need the waiver to never *miss* a real
/// descendant segment.
pub fn has_descendant_segment(selector: &str) -> bool {
    let mut chars = selector.chars();
    let mut quote: Option<char> = None;
    let mut previous_was_dot = false;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    previous_was_dot = false;
                }
                '.' if previous_was_dot => return true,
                '.' => previous_was_dot = true,
                _ => previous_was_dot = false,
            },
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::has_descendant_segment;

    #[test]
    fn detects_descendant_segments() {
        // The last case is the deliberate over-match: a descendant inside a filter
        // sub-query gets the waiver even though it cannot reorder the outer selection.
        for query in ["$..a", "$..*", "$.x..[?@.a]", "$.a..b[0]", "$[?@..x]"] {
            assert!(
                has_descendant_segment(query),
                "`{query}` contains a descendant segment"
            );
        }
    }

    #[test]
    fn single_dots_are_not_descendant_segments() {
        for query in ["$", "$.a.b.c", "$.a[1:3]", "$.a[*].b"] {
            assert!(
                !has_descendant_segment(query),
                "`{query}` has no descendant segment"
            );
        }
    }

    #[test]
    fn dots_inside_string_literals_are_ignored() {
        for query in [
            "$['..']",
            r#"$[".."]"#,
            "$['a..b'].c",
            r#"$[?@.a == "x..y"]"#,
        ] {
            assert!(
                !has_descendant_segment(query),
                "`{query}`'s `..` is inside a string literal"
            );
        }
    }

    #[test]
    fn escaped_quote_does_not_close_the_literal() {
        assert!(
            !has_descendant_segment(r"$['a\'..b']"),
            "an escaped active quote keeps the literal open, so the `..` stays quoted"
        );
        assert!(
            has_descendant_segment(r"$['a\'b']..c"),
            "a `..` after the literal genuinely closes is a descendant segment"
        );
    }

    #[test]
    fn dot_before_a_literal_does_not_pair_with_a_dot_after_it() {
        // `.` then a quoted name then `.`: the two dots are separated by the literal
        // and must not read as `..`.
        assert!(
            !has_descendant_segment("$.a['x'].b"),
            "dots on either side of a bracketed name are independent"
        );
    }
}
