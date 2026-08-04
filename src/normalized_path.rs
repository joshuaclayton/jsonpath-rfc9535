//! Normalized Paths — the canonical location of a node within a document.
//!
//! A [`NormalizedPath`] is the unique identity of a node, rendered in the canonical
//! bracket form defined by [RFC 9535 §2.7] (`$['a'][3]`). During evaluation the
//! engine threads a path alongside each node and extends it as it descends; because
//! the path is stored as a shared parent-chain ([`Rc`]), extending it is O(1) and no
//! string is built until [`Display`](fmt::Display) is invoked.
//!
//! [RFC 9535 §2.7]: https://www.rfc-editor.org/rfc/rfc9535#section-2.7

use core::fmt;
use std::rc::Rc;

/// One step of a normalized path: a member name or an array index.
///
/// The member name is borrowed (`&'a str`) directly from the queried document's map
/// keys — the normalized name of a selected node is always one of those keys — so
/// extending a path costs only the parent-chain [`Rc`], never a string allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step<'a> {
    /// An object member, rendered as `['name']` with §2.7 escaping.
    Name(&'a str),
    /// An array element, rendered as `[index]`.
    Index(usize),
}

/// A node in the shared parent-chain. The chain runs leaf → root; the root itself
/// is represented by `None` (an empty [`NormalizedPath`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link<'a> {
    step: Step<'a>,
    parent: Option<Rc<Self>>,
}

/// One element of a [`NormalizedPath`], as structural data: an object member name
/// or an array index.
///
/// This is the machine-readable counterpart to the path's
/// [`Display`](fmt::Display) form — member names are returned verbatim (the §2.7
/// quoting and escaping in `$['a\'b'][3]` is a rendering concern only), borrowed
/// from the queried document's map keys like the path itself.
#[expect(
    clippy::exhaustive_enums,
    reason = "RFC 9535 §2.7 defines a normalized path as exactly member names and \
              array indexes; a hypothetical new element kind must break every \
              consumer that walks documents by these steps"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element<'a> {
    /// An object member name, verbatim (unescaped).
    Name(&'a str),
    /// An array element index.
    Index(usize),
}

/// The canonical, unique location of a node within a JSON document.
///
/// You obtain a `NormalizedPath` from a query result — [`LocatedNode::path`] or
/// [`NodeList::paths`] — and render it with [`Display`](fmt::Display) (or
/// [`ToString`]) to get the canonical bracket form defined by [RFC 9535 §2.7], e.g.
/// `$['a'][3]`. Two paths compare equal exactly when they identify the same node.
///
/// The lifetime `'a` ties a path to the document it locates a node in: member-name
/// steps borrow the document's map keys rather than copying them, so a path cannot
/// outlive that document (neither can a [`NodeList`], which borrows the selected
/// values themselves).
///
/// [`LocatedNode::path`]: crate::LocatedNode::path
/// [`NodeList::paths`]: crate::NodeList::paths
/// [`NodeList`]: crate::NodeList
/// [RFC 9535 §2.7]: https://www.rfc-editor.org/rfc/rfc9535#section-2.7
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedPath<'a> {
    head: Option<Rc<Link<'a>>>,
}

impl<'a> NormalizedPath<'a> {
    /// The path of the document root, `$`.
    pub(crate) const fn root() -> Self {
        Self { head: None }
    }

    /// Returns the path of the object member `name` reached from this path.
    pub(crate) fn child_name(&self, name: &'a str) -> Self {
        self.push(Step::Name(name))
    }

    /// Returns the path of the array element at `index` reached from this path.
    pub(crate) fn child_index(&self, index: usize) -> Self {
        self.push(Step::Index(index))
    }

    fn push(&self, step: Step<'a>) -> Self {
        Self {
            head: Some(Rc::new(Link {
                step,
                parent: self.head.clone(),
            })),
        }
    }

    /// Returns the path's elements as structural data, root → leaf. The root path
    /// `$` has no elements.
    ///
    /// Use this to walk a document along the path instead of parsing the
    /// [`Display`](fmt::Display) form — e.g. to reach a node's **parent container**
    /// (for removal or replacement), split off the last element:
    ///
    /// ```
    /// use jsonpath_rfc9535::{Element, JsonPath};
    ///
    /// let doc = serde_json::json!({"parts": [{"body": "a"}, {"body": "b"}]});
    /// let query = JsonPath::parse("$.parts[1].body")?;
    /// let nodes = query.query(&doc);
    /// let node = nodes.exactly_one().expect("one match");
    ///
    /// let elements = node.path().elements();
    /// assert_eq!(
    ///     elements,
    ///     [Element::Name("parts"), Element::Index(1), Element::Name("body")],
    /// );
    /// let (leaf, parents) = elements.split_last().expect("non-root path");
    /// assert_eq!(*leaf, Element::Name("body"));
    /// assert_eq!(parents, [Element::Name("parts"), Element::Index(1)]);
    /// # Ok::<(), jsonpath_rfc9535::Error>(())
    /// ```
    #[must_use]
    pub fn elements(&self) -> Vec<Element<'a>> {
        // The chain is stored leaf → root; collect and reverse to present the
        // order a document walk needs.
        let mut elements: Vec<Element<'a>> =
            core::iter::successors(self.head.as_deref(), |link| link.parent.as_deref())
                .map(|link| match link.step {
                    Step::Name(name) => Element::Name(name),
                    Step::Index(index) => Element::Index(index),
                })
                .collect();
        elements.reverse();
        elements
    }
}

impl Default for NormalizedPath<'_> {
    fn default() -> Self {
        Self::root()
    }
}

impl fmt::Display for NormalizedPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("$")?;
        for element in self.elements() {
            match element {
                Element::Name(name) => {
                    f.write_str("['")?;
                    write_escaped_name(f, name)?;
                    f.write_str("']")?;
                }
                Element::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// Writes a member name using the `normal-name-selector` escaping of RFC 9535 §2.7:
/// single quotes and backslashes are escaped, the five control characters with a
/// short escape use it, and any remaining control character (`< U+0020`) is written
/// as a lower-case `\u00XX` escape. Everything else is emitted verbatim.
fn write_escaped_name(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    for c in name.chars() {
        match c {
            '\'' => f.write_str("\\'")?,
            '\\' => f.write_str("\\\\")?,
            '\u{8}' => f.write_str("\\b")?,
            '\u{c}' => f.write_str("\\f")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            other if u32::from(other) < 0x20 => write!(f, "\\u{:04x}", u32::from(other))?,
            other => write!(f, "{other}")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Element, NormalizedPath};

    #[test]
    fn elements_run_root_to_leaf() {
        let path = NormalizedPath::root()
            .child_name("a")
            .child_index(3)
            .child_name("b");
        assert_eq!(
            path.elements(),
            vec![Element::Name("a"), Element::Index(3), Element::Name("b")],
            "elements are structural data in root → leaf order"
        );
    }

    #[test]
    fn root_has_no_elements() {
        assert_eq!(
            NormalizedPath::root().elements(),
            Vec::new(),
            "the root path `$` has no elements"
        );
    }

    #[test]
    fn elements_carry_names_verbatim() {
        // §2.7 escaping is a rendering concern; the structural API returns
        // the member name exactly as it appears in the document.
        assert_eq!(
            NormalizedPath::root().child_name("a'\\b").elements(),
            vec![Element::Name("a'\\b")],
            "no escaping in the structural form"
        );
    }

    #[test]
    fn split_last_walks_to_the_parent_container() {
        let path = NormalizedPath::root().child_name("parts").child_index(2);
        let elements = path.elements();
        let (leaf, parents) = elements.split_last().expect("non-root path");
        assert_eq!(*leaf, Element::Index(2), "leaf identifies the node itself");
        assert_eq!(
            parents,
            &[Element::Name("parts")],
            "prefix locates the parent container"
        );
    }

    #[test]
    fn root_is_dollar() {
        assert_eq!(
            NormalizedPath::root().to_string(),
            "$",
            "the root path is `$`"
        );
    }

    #[test]
    fn renders_names_and_indices() {
        let path = NormalizedPath::root()
            .child_name("a")
            .child_name("b")
            .child_index(1);
        assert_eq!(
            path.to_string(),
            "$['a']['b'][1]",
            "nested object then array"
        );
    }

    #[test]
    fn escapes_quote_and_backslash() {
        let path = NormalizedPath::root().child_name("a'\\b");
        assert_eq!(
            path.to_string(),
            r"$['a\'\\b']",
            "single quote and backslash are escaped"
        );
    }

    #[test]
    fn escapes_control_characters() {
        // RFC 9535 Table 18: $["\u000b"] normalizes to $['\u000b'].
        assert_eq!(
            NormalizedPath::root().child_name("\u{B}").to_string(),
            r"$['\u000b']",
            "U+000B has no short escape, so it uses lower-case \\u00XX"
        );
        assert_eq!(
            NormalizedPath::root().child_name("\n\t").to_string(),
            r"$['\n\t']",
            "newline and tab use their short escapes"
        );
    }

    #[test]
    fn shares_ancestors_without_mutating() {
        let base = NormalizedPath::root().child_name("a");
        let left = base.child_index(0);
        let right = base.child_name("b");
        assert_eq!(left.to_string(), "$['a'][0]", "first descendant");
        assert_eq!(
            right.to_string(),
            "$['a']['b']",
            "sibling descendant is independent"
        );
        assert_eq!(base.to_string(), "$['a']", "the shared base is unchanged");
    }

    #[test]
    fn equal_paths_compare_equal() {
        let a = NormalizedPath::root().child_name("x").child_index(2);
        let b = NormalizedPath::root().child_name("x").child_index(2);
        assert_eq!(a, b, "paths with the same steps are equal");
    }
}
