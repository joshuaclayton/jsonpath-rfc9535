//! `jsonpath-rfc9535` — a complete [RFC 9535] JSONPath query engine for
//! [`serde_json`] values.
//!
//! [RFC 9535] defines *JSONPath*: a query language that selects a set of nodes — the
//! *nodelist* — from a JSON document. This crate parses and type-checks a query once
//! into a [`JsonPath`], then evaluates it against any number of [`serde_json::Value`]
//! documents, returning the selected values and, on request, their *normalized paths*.
//!
//! # Quick start
//!
//! Compile a query with [`JsonPath::parse`], then evaluate it. [`JsonPath::query`]
//! returns a [`NodeList`] — each selected value paired with its [`NormalizedPath`]:
//!
//! ```
//! use jsonpath_rfc9535::JsonPath;
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
//! # Ok::<(), jsonpath_rfc9535::Error>(())
//! ```
//!
//! # Values, or values and paths
//!
//! A compiled query evaluates two ways:
//!
//! * [`JsonPath::query`] returns a [`NodeList`] of [`LocatedNode`]s — each a selected
//!   value plus its [`NormalizedPath`], the node's canonical location within the
//!   document (e.g. `$['store']['book'][0]`, per [RFC 9535 §2.7]).
//! * [`JsonPath::query_values`] returns just the selected values
//!   (`Vec<&serde_json::Value>`). It constructs no paths, so it is appreciably faster
//!   for wildcard- and descendant-heavy queries where locations aren't needed.
//!
//! Either way the results *borrow* from the input document — no JSON value is cloned.
//!
//! # One-off queries
//!
//! When a query won't be reused, the crate-level [`query`] and [`query_values`]
//! functions parse and evaluate in a single call. Prefer [`JsonPath::parse`] plus a
//! query method when applying the same query to many documents, so the parse and
//! well-typedness check happen only once.
//!
//! ```
//! use serde_json::json;
//!
//! let document = json!({ "a": [1, 2, 3] });
//! let values = jsonpath_rfc9535::query_values("$.a[*]", &document)?;
//! assert_eq!(values, [&json!(1), &json!(2), &json!(3)]);
//! # Ok::<(), jsonpath_rfc9535::Error>(())
//! ```
//!
//! # Cargo features
//!
//! * **`regex`** *(enabled by default)* — provides the `match()` and `search()`
//!   function extensions, which require an [I-Regexp] (RFC 9485) engine backed by the
//!   [`regex`] crate. With the feature disabled the crate builds without that
//!   dependency, and a query using those functions is rejected by [`JsonPath::parse`]
//!   with an [`Error`].
//!
//! ```
//! # #[cfg(feature = "regex")] {
//! use serde_json::json;
//!
//! let users = json!([{ "name": "Ada" }, { "name": "linus" }]);
//! // `match()` tests the whole string (anchored); `search()` tests any substring.
//! let capitalized =
//!     jsonpath_rfc9535::query_values(r#"$[?match(@.name, "[A-Z].*")]"#, &users)?;
//! assert_eq!(capitalized, [&json!({ "name": "Ada" })]);
//! # }
//! # Ok::<(), jsonpath_rfc9535::Error>(())
//! ```
//!
//! * **`scan`** *(experimental, off by default)* — provides `ScanQuery`: hybrid
//!   evaluation over **raw JSON text** that byte-scans a structural query prefix
//!   (via [rsonpath](https://docs.rs/rsonpath-lib)) and evaluates the residual per
//!   extracted fragment, never building a whole-document DOM. See the type's docs for
//!   semantics and caveats.
//!
//! * **`rayon`** *(off by default)* — parallelises [`JsonPath::query_values`] over
//!   large documents (arrays or frontiers of thousands of elements) on the
//!   [rayon](https://docs.rs/rayon) global thread pool: 4–9× wall-clock on
//!   100k-element documents, at correspondingly higher CPU use. Results keep exact
//!   document order. Small documents and [`JsonPath::query`] (the paths API) always
//!   evaluate serially.
//!
//! # Conformance
//!
//! Correctness is pinned to the official [JSONPath Compliance Test Suite][cts]: every
//! case is exercised — selected values *and* normalized paths checked in lockstep —
//! including the queries that must be *rejected* at compile time.
//!
//! [RFC 9535]: https://www.rfc-editor.org/rfc/rfc9535
//! [RFC 9535 §2.7]: https://www.rfc-editor.org/rfc/rfc9535#section-2.7
//! [I-Regexp]: https://www.rfc-editor.org/rfc/rfc9485
//! [cts]: https://github.com/jsonpath-standard/jsonpath-compliance-test-suite
#![cfg_attr(docsrs, feature(doc_cfg))]

mod ast;
mod compiled;
mod error;
mod eval;
#[cfg(feature = "regex")]
mod iregexp;
mod jsonpath;
mod node;
mod normalized_path;
mod parser;
#[cfg(feature = "scan")]
mod scan;

pub use error::Error;
pub use jsonpath::JsonPath;
pub use node::{LocatedNode, NodeList};
pub use normalized_path::NormalizedPath;
#[cfg(feature = "scan")]
#[cfg_attr(docsrs, doc(cfg(feature = "scan")))]
pub use scan::{ScanError, ScanMode, ScanQuery};

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
