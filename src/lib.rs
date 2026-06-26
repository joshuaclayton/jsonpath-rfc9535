//! `jp-full` — a complete, [RFC 9535] compliant JSONPath query engine for
//! [`serde_json`] values.
//!
//! [RFC 9535] defines *JSONPath*, a query language that selects a set of nodes
//! (the *nodelist*) from a JSON document. This crate compiles a query string once
//! and evaluates it against any number of [`serde_json::Value`] documents,
//! returning the selected nodes together with their *normalized paths*.
//!
//! # Examples
//!
//! Compile a query once, then read back both the selected values and their
//! normalized paths:
//!
//! ```
//! use jp_full::JsonPath;
//! use serde_json::json;
//!
//! let document = json!({
//!     "store": {
//!         "book": [
//!             { "title": "Sayings of the Century", "price": 8.95 },
//!             { "title": "Moby Dick", "price": 12.99 }
//!         ]
//!     }
//! });
//!
//! let query = JsonPath::parse("$.store.book[?@.price < 10].title")?;
//! let nodes = query.query(&document);
//!
//! let titles: Vec<_> = nodes.values().collect();
//! assert_eq!(titles, [&json!("Sayings of the Century")]);
//!
//! let paths: Vec<_> = nodes.paths().map(ToString::to_string).collect();
//! assert_eq!(paths, ["$['store']['book'][0]['title']"]);
//! # Ok::<(), jp_full::Error>(())
//! ```
//!
//! For a one-off query, [`query`] compiles and evaluates in a single call.
//!
//! # Conformance
//!
//! Correctness is pinned to the official [JSONPath Compliance Test Suite][cts]: the
//! engine is exercised against every case, including the ~250 that must be
//! *rejected* at compile time.
//!
//! # Cargo features
//!
//! * **`regex`** *(enabled by default)* — provides the `match()` and `search()`
//!   function extensions, which require an [I-Regexp](https://www.rfc-editor.org/rfc/rfc9485)
//!   engine. With the feature disabled the crate builds without the [`regex`]
//!   dependency, and a query that uses those functions fails to compile with a
//!   clear [`Error`].
//!
//! [RFC 9535]: https://www.rfc-editor.org/rfc/rfc9535
//! [cts]: https://github.com/jsonpath-standard/jsonpath-compliance-test-suite
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod ast;
mod compiled;
pub mod error;
mod eval;
#[cfg(feature = "regex")]
mod iregexp;
mod jsonpath;
mod node;
mod normalized_path;
mod parser;

pub use error::Error;
pub use jsonpath::JsonPath;
pub use node::{LocatedNode, NodeList};
pub use normalized_path::NormalizedPath;

/// Compiles `path` and evaluates it against `root` in a single step.
///
/// Prefer [`JsonPath::parse`] when applying the same query to multiple documents, so
/// it is compiled only once.
///
/// # Errors
///
/// Returns an [`Error`] if `path` is not a syntactically valid, well-typed query.
pub fn query<'a>(path: &str, root: &'a serde_json::Value) -> Result<NodeList<'a>, Error> {
    Ok(JsonPath::parse(path)?.query(root))
}

/// Compiles `path` and evaluates it against `root`, returning just the selected values.
///
/// Like [`query`] but skips normalized-path construction, so it is faster when the
/// caller does not need paths. Prefer [`JsonPath::parse`] + [`JsonPath::query_values`]
/// when applying the same query to multiple documents.
///
/// # Errors
///
/// Returns an [`Error`] if `path` is not a syntactically valid, well-typed query.
pub fn query_values<'a>(
    path: &str,
    root: &'a serde_json::Value,
) -> Result<Vec<&'a serde_json::Value>, Error> {
    Ok(JsonPath::parse(path)?.query_values(root))
}
