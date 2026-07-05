//! Helpers shared across the scan test harnesses.

use serde_json::Value;

/// Order-insensitive rendering — sorted compact-JSON strings — for comparisons where
/// RFC 9535 leaves ordering unspecified.
pub fn multiset(values: &[Value]) -> Vec<String> {
    let mut rendered: Vec<String> = values.iter().map(ToString::to_string).collect();
    rendered.sort();
    rendered
}
