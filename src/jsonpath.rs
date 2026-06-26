//! The compiled, runnable JSONPath query type.

use crate::compiled::{self, Query};
use crate::{Error, NodeList};
use core::str::FromStr;
use serde_json::Value;

/// A compiled JSONPath query, ready to evaluate against any number of documents.
///
/// Compiling with [`JsonPath::parse`] performs both parsing and the RFC 9535 §2.4
/// function well-typedness check, so a `JsonPath` value is guaranteed to be a valid,
/// well-typed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPath {
    query: Query,
}

impl JsonPath {
    /// Compiles a JSONPath query string.
    ///
    /// This parses the query against the RFC 9535 grammar and type-checks any
    /// function extensions; the resulting `JsonPath` can be evaluated repeatedly.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if `query` is not syntactically valid JSONPath, contains
    /// an out-of-range integer, or uses a function extension that is unregistered,
    /// has the wrong arity, or is not well-typed.
    pub fn parse(query: &str) -> Result<Self, Error> {
        let ast = crate::parser::parse(query)?;
        Ok(Self {
            query: compiled::lower(ast)?,
        })
    }

    /// Evaluates the query against `root`, returning the selected nodelist (each value
    /// paired with its normalized path). Borrows from `root`; never fails.
    #[must_use]
    pub fn query<'a>(&self, root: &'a Value) -> NodeList<'a> {
        crate::eval::evaluate(self.compiled(), root)
    }

    /// Evaluates the query against `root`, returning just the selected values in order.
    ///
    /// This skips normalized-path construction entirely, so it is substantially faster
    /// than [`query`](Self::query) for wildcard- and descendant-heavy queries where the
    /// caller does not need paths.
    #[must_use]
    pub fn query_values<'a>(&self, root: &'a Value) -> Vec<&'a Value> {
        crate::eval::evaluate_values(self.compiled(), root)
    }

    /// Returns the compiled query IR (consumed by the evaluator).
    const fn compiled(&self) -> &Query {
        &self.query
    }
}

impl FromStr for JsonPath {
    type Err = Error;

    fn from_str(query: &str) -> Result<Self, Self::Err> {
        Self::parse(query)
    }
}

#[cfg(test)]
mod tests {
    use super::JsonPath;
    use core::str::FromStr;

    #[test]
    fn compiles_a_valid_query() {
        assert!(
            JsonPath::parse("$.store.book[?@.price < 10]").is_ok(),
            "a valid, well-typed query compiles"
        );
    }

    #[test]
    fn rejects_a_syntax_error() {
        assert!(
            JsonPath::parse(" $").is_err(),
            "leading whitespace is a syntax error"
        );
    }

    #[test]
    fn rejects_an_ill_typed_query() {
        assert!(
            JsonPath::parse("$[?length(@.*) < 3]").is_err(),
            "a non-singular query in a ValueType slot is rejected at compile time"
        );
    }

    #[test]
    fn parses_via_from_str() {
        assert!(
            JsonPath::from_str("$..*").is_ok(),
            "FromStr delegates to parse"
        );
    }

    #[test]
    fn evaluates_index_and_filter() {
        let doc = serde_json::json!({"a": {"b": [10, 20, 30]}});
        let by_index = JsonPath::parse("$.a.b[1]").expect("compiles");
        assert_eq!(
            by_index.query_values(&doc),
            vec![&serde_json::json!(20)],
            "index selector picks the second element"
        );

        let filtered = JsonPath::parse("$.a.b[?@ > 15]").expect("compiles");
        assert_eq!(
            filtered.query_values(&doc),
            vec![&serde_json::json!(20), &serde_json::json!(30)],
            "filter keeps elements greater than 15"
        );
    }

    #[test]
    fn query_reports_normalized_paths() {
        let doc = serde_json::json!({"x": [1, 2]});
        let path = JsonPath::parse("$.x[*]").expect("compiles");
        let paths: Vec<String> = path.query(&doc).paths().map(ToString::to_string).collect();
        assert_eq!(
            paths,
            vec!["$['x'][0]".to_owned(), "$['x'][1]".to_owned()],
            "wildcard paths"
        );
    }
}
