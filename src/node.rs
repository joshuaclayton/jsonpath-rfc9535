//! Query results: located nodes and node lists.
//!
//! Evaluating a query yields a [`NodeList`] — an ordered sequence of
//! [`LocatedNode`]s, each pairing a selected [`serde_json::Value`] (borrowed from the
//! queried document) with its [`NormalizedPath`]. Results borrow the input, so a
//! `NodeList` may not outlive the document it was produced from.

use crate::normalized_path::NormalizedPath;
use serde_json::Value;

/// A single selected node: the JSON value plus its normalized location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedNode<'a> {
    path: NormalizedPath<'a>,
    node: &'a Value,
}

impl<'a> LocatedNode<'a> {
    pub(crate) const fn new(path: NormalizedPath<'a>, node: &'a Value) -> Self {
        Self { path, node }
    }

    /// The normalized path identifying this node within the queried document.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath<'a> {
        &self.path
    }

    /// The selected JSON value, borrowed from the queried document.
    #[must_use]
    pub const fn value(&self) -> &'a Value {
        self.node
    }
}

/// The ordered list of nodes selected by a query (a *nodelist*).
///
/// Order follows RFC 9535: array elements in array order, with the relative order of
/// object members and of distinct filter/wildcard results left unspecified.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeList<'a> {
    nodes: Vec<LocatedNode<'a>>,
}

impl<'a> NodeList<'a> {
    pub(crate) const fn new(nodes: Vec<LocatedNode<'a>>) -> Self {
        Self { nodes }
    }

    /// The number of selected nodes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the nodelist is empty (the query selected nothing).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Iterates over the located nodes.
    pub fn iter(&self) -> core::slice::Iter<'_, LocatedNode<'a>> {
        self.nodes.iter()
    }

    /// Iterates over just the selected values.
    pub fn values(&self) -> impl Iterator<Item = &'a Value> + '_ {
        self.nodes.iter().map(LocatedNode::value)
    }

    /// Iterates over the normalized paths of the selected nodes.
    pub fn paths(&self) -> impl Iterator<Item = &NormalizedPath<'a>> + '_ {
        self.nodes.iter().map(LocatedNode::path)
    }

    /// Returns the sole selected node, or `None` unless exactly one was selected.
    #[must_use]
    pub fn exactly_one(&self) -> Option<&LocatedNode<'a>> {
        match self.nodes.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// Consumes the nodelist, returning its located nodes.
    #[must_use]
    pub fn into_vec(self) -> Vec<LocatedNode<'a>> {
        self.nodes
    }
}

impl<'a> IntoIterator for NodeList<'a> {
    type Item = LocatedNode<'a>;
    type IntoIter = std::vec::IntoIter<LocatedNode<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes.into_iter()
    }
}

impl<'a, 'b> IntoIterator for &'b NodeList<'a> {
    type Item = &'b LocatedNode<'a>;
    type IntoIter = core::slice::Iter<'b, LocatedNode<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::{LocatedNode, NodeList};
    use crate::normalized_path::NormalizedPath;
    use serde_json::json;

    #[test]
    fn collects_values_and_paths_in_order() {
        let first = json!(1);
        let second = json!(2);
        let list = NodeList::new(vec![
            LocatedNode::new(NormalizedPath::root().child_index(0), &first),
            LocatedNode::new(NormalizedPath::root().child_index(1), &second),
        ]);
        assert_eq!(list.len(), 2, "two nodes");
        assert!(!list.is_empty(), "not empty");
        let values: Vec<&serde_json::Value> = list.values().collect();
        assert_eq!(values, vec![&first, &second], "values in order");
        let paths: Vec<String> = list.paths().map(ToString::to_string).collect();
        assert_eq!(
            paths,
            vec!["$[0]".to_owned(), "$[1]".to_owned()],
            "paths in order"
        );
        assert!(list.exactly_one().is_none(), "two nodes is not exactly one");
    }

    #[test]
    fn exactly_one_returns_the_sole_node() {
        let value = json!("x");
        let list = NodeList::new(vec![LocatedNode::new(NormalizedPath::root(), &value)]);
        let only = list.exactly_one().expect("one node");
        assert_eq!(only.value(), &value, "the sole value");
        assert_eq!(only.path().to_string(), "$", "the sole path is the root");
    }

    #[test]
    fn default_is_empty() {
        let list = NodeList::default();
        assert!(list.is_empty(), "default is empty");
        assert_eq!(list.len(), 0, "len is zero");
        assert!(list.exactly_one().is_none(), "no sole node");
    }
}
