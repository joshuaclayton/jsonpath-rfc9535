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
