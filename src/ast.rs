//! The JSONPath query abstract syntax tree.
//!
//! These types are the *contract* between the parser and the evaluator: the parser
//! produces a [`Query`], the function well-typedness checker validates it, and the
//! evaluator walks it against a [`serde_json::Value`]. The tree is a faithful,
//! desugared model of the RFC 9535 grammar ([Appendix A]):
//!
//! * Shorthand syntax is normalized away. `.name` and `['name']` both become a
//!   child [`Segment`] holding a single [`Selector::Name`]; `.*` / `[*]` become
//!   [`Selector::Wildcard`]; `..name` / `..*` / `..[…]` become a
//!   [`Segment::Descendant`]. The surface form is not retained — semantically
//!   equivalent queries produce identical trees.
//! * Integers that index or slice arrays are carried as [`JsonInt`], which enforces
//!   the RFC's I-JSON safe-integer range at construction time.
//! * Operator precedence (`||` binds looser than `&&`, `!` binds tightest) is
//!   encoded in the *shape* of the [`LogicalExpr`] tree, so parentheses carry no
//!   information and are not represented.
//!
//! Whether a function expression is *well-typed* (RFC 9535 §2.4.3) is **not**
//! enforced by these types — a syntactically valid but ill-typed query such as
//! `length(@.*)` is representable here and is rejected later by the type checker.
//!
//! [Appendix A]: https://www.rfc-editor.org/rfc/rfc9535#appendix-A

use crate::error::{Error, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER};
use core::fmt;
use serde_json::Number;

/// An integer that is valid as a JSONPath array index or slice bound.
///
/// RFC 9535 requires index and slice (`start`/`end`/`step`) values to fall within
/// the I-JSON interoperable integer range `[-(2^53)+1, (2^53)-1]`. `JsonInt`
/// enforces that invariant at its construction site — once you hold a `JsonInt`,
/// its value is guaranteed to be in range — so the parser and evaluator never have
/// to re-check it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JsonInt(i64);

impl JsonInt {
    /// Constructs a `JsonInt`, validating that `value` lies within the I-JSON
    /// safe-integer range (see [`MIN_SAFE_INTEGER`]/[`MAX_SAFE_INTEGER`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::IntegerOutOfRange`] if `value` is outside that range.
    pub fn new(value: i64) -> Result<Self, Error> {
        if (MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
            Ok(Self(value))
        } else {
            Err(Error::IntegerOutOfRange {
                repr: value.to_string(),
            })
        }
    }

    /// Returns the underlying [`i64`], guaranteed to be in the I-JSON safe range.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for JsonInt {
    type Error = Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl fmt::Display for JsonInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A complete, compiled JSONPath query — the root of the AST.
///
/// A query is implicitly rooted at the document root identifier `$`; the `$` is not
/// stored. Applying the query means feeding the root node through `segments` in
/// order, each segment transforming the working nodelist (see
/// [RFC 9535 §2.1](https://www.rfc-editor.org/rfc/rfc9535#section-2.1)).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Query {
    /// The segments applied, left to right, to the root node `$`.
    pub segments: Vec<Segment>,
}

/// One segment of a query: a step that maps each input node to zero or more nodes.
///
/// See [RFC 9535 §2.5](https://www.rfc-editor.org/rfc/rfc9535#section-2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Segment {
    /// A child segment (`[…]`, `.name`, or `.*`): applies its selectors to the
    /// children of each input node. The resulting nodelist is the concatenation of
    /// each selector's results, in selector order.
    Child(Vec<Selector>),

    /// A descendant segment (`..[…]`, `..name`, or `..*`): applies its selectors to
    /// each input node *and* all of its descendants, visited in pre-order (a node
    /// before its descendants; array elements in order; object members in an
    /// unspecified order).
    Descendant(Vec<Selector>),
}

/// A single selector within a [`Segment`].
///
/// See [RFC 9535 §2.3](https://www.rfc-editor.org/rfc/rfc9535#section-2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Selector {
    /// A name selector (`'key'` / `"key"`): selects the value of the named member
    /// of an object. Selects nothing from a non-object. The stored string is the
    /// decoded member name (escape sequences already resolved).
    Name(String),

    /// The wildcard selector (`*`): selects every member value of an object or
    /// every element of an array.
    Wildcard,

    /// An index selector (e.g. `0`, `-1`): selects one element of an array by
    /// zero-based position; negative values count from the end. Selects nothing
    /// from a non-array or an out-of-bounds index.
    Index(JsonInt),

    /// An array slice selector (`start:end:step`): selects a sub-sequence of an
    /// array's elements.
    Slice(Slice),

    /// A filter selector (`?<expr>`): selects each child of an array or object for
    /// which the logical expression evaluates to true.
    Filter(LogicalExpr),
}

/// An array slice `start:end:step`, as in [RFC 9535 §2.3.4].
///
/// Each component is optional in the syntax. The defaults are applied during
/// evaluation, not here, because the defaults for `start`/`end` depend on the sign
/// of `step` (the RFC's `Bounds` algorithm); a missing `step` defaults to `1`.
///
/// [RFC 9535 §2.3.4]: https://www.rfc-editor.org/rfc/rfc9535#section-2.3.4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Slice {
    /// The first index included in the selection, if specified.
    pub start: Option<JsonInt>,
    /// The first index *excluded* from the selection, if specified.
    pub end: Option<JsonInt>,
    /// The iteration step, if specified. A step of `0` selects nothing.
    pub step: Option<JsonInt>,
}

/// A filter logical expression — the body of a [`Selector::Filter`] (`?<expr>`).
///
/// Operator precedence is encoded structurally: an [`Or`](LogicalExpr::Or) node is
/// never nested directly inside an [`And`](LogicalExpr::And) without the grammar
/// having required it, and [`Not`](LogicalExpr::Not) binds tightest. See
/// [RFC 9535 §2.3.5](https://www.rfc-editor.org/rfc/rfc9535#section-2.3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogicalExpr {
    /// Disjunction `lhs || rhs`. True if either operand is true.
    Or(Box<Self>, Box<Self>),

    /// Conjunction `lhs && rhs`. True only if both operands are true.
    And(Box<Self>, Box<Self>),

    /// Negation `!expr`. In the grammar `!` applies only to a parenthesized
    /// expression or a test expression, but the evaluator handles the negation of
    /// any contained expression uniformly.
    Not(Box<Self>),

    /// A comparison such as `@.price < 10`.
    Comparison(Comparison),

    /// An existence test: a bare query (`@.foo`, `$.bar[*]`) used in a logical
    /// context, true when the query selects at least one node.
    Existence(FilterQuery),

    /// A function extension used as a test, e.g. `match(@.s, 'ab.*')`. Its declared
    /// result type must be `LogicalType`, or `NodesType` (converted: a non-empty
    /// nodelist is true). Enforced by the type checker, not by this type.
    FunctionTest(FunctionExpr),
}

/// A comparison expression: two comparables joined by a comparison operator.
///
/// See [RFC 9535 §2.3.5.2.2](https://www.rfc-editor.org/rfc/rfc9535#section-2.3.5.2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Comparison {
    /// The left-hand comparable.
    pub left: Comparable,
    /// The comparison operator.
    pub op: ComparisonOp,
    /// The right-hand comparable.
    pub right: Comparable,
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ComparisonOp {
    /// `==` — equality.
    Eq,
    /// `!=` — inequality.
    Ne,
    /// `<` — strictly less than.
    Lt,
    /// `<=` — less than or equal.
    Le,
    /// `>` — strictly greater than.
    Gt,
    /// `>=` — greater than or equal.
    Ge,
}

/// An operand of a [`Comparison`].
///
/// The grammar admits only these three forms as comparables; in particular a
/// general (non-singular) query is **not** a comparable, which is how RFC 9535
/// structurally enforces "comparisons operate on singular queries".
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Comparable {
    /// A literal value (`42`, `'text'`, `true`, `null`).
    Literal(Literal),

    /// A singular query — one guaranteed to select at most one node. Its value (or
    /// the special "Nothing" if it selects no node) is used in the comparison.
    SingularQuery(SingularQuery),

    /// A function expression whose declared result type is `ValueType`.
    Function(FunctionExpr),
}

/// A primitive literal value usable in a filter.
///
/// JSONPath literals are restricted to primitive JSON values (no arrays or
/// objects), per the `literal` grammar rule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Literal {
    /// A JSON number literal. Stored as [`serde_json::Number`]; numeric *value*
    /// comparison (so that `1` and `1.0` compare equal) is performed during
    /// evaluation, not by this type's `PartialEq`.
    Number(Number),
    /// A string literal, already decoded from its single- or double-quoted form.
    String(String),
    /// A boolean literal (`true` / `false`).
    Bool(bool),
    /// The `null` literal.
    Null,
}

/// The identifier a query is rooted at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryRoot {
    /// The document root `$` (an absolute query).
    Root,
    /// The current node `@` (a query relative to the node being filtered).
    Current,
}

/// A query appearing inside a filter — either an absolute (`$…`) or relative
/// (`@…`) query, used as an existence test or as a function argument.
///
/// Unlike a singular query, a `FilterQuery` may select any number of nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FilterQuery {
    /// Whether the query is rooted at `$` or `@`.
    pub root: QueryRoot,
    /// The segments applied to the root node.
    pub segments: Vec<Segment>,
}

/// A query restricted to selecting at most one node — used as a [`Comparable`].
///
/// Every segment is a single name or index applied to a child, which guarantees a
/// nodelist of at most one node (RFC 9535 `singular-query`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SingularQuery {
    /// Whether the query is rooted at `$` or `@`.
    pub root: QueryRoot,
    /// The name/index steps applied to the root node.
    pub segments: Vec<SingularSegment>,
}

/// One step of a [`SingularQuery`]: a single name or index.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SingularSegment {
    /// A member name step (`['name']` or `.name`).
    Name(String),
    /// An array index step (`[3]`, `[-1]`).
    Index(JsonInt),
}

/// A call to a function extension, e.g. `length(@.items)` or `match(@.s, 'a.*')`.
///
/// The `name` is stored verbatim; it is resolved to one of the registered
/// extensions (`length`, `count`, `match`, `search`, `value`) and checked for
/// arity and well-typedness by the type checker.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FunctionExpr {
    /// The function name as written in the query.
    pub name: String,
    /// The argument expressions, in order.
    pub args: Vec<FunctionArg>,
}

/// An argument to a [`FunctionExpr`].
///
/// The variant records how the argument was parsed; the type checker maps it to the
/// parameter's declared type (`ValueType` / `LogicalType` / `NodesType`) following
/// RFC 9535 §2.4.3. A bare function call is a [`Function`](FunctionArg::Function)
/// (classified by its own result type); a bare query is a
/// [`Query`](FunctionArg::Query); anything built from operators, negation, or
/// parentheses is a [`Logical`](FunctionArg::Logical).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FunctionArg {
    /// A literal argument (only valid for a `ValueType` parameter).
    Literal(Literal),

    /// A query argument. Supplies a `NodesType`; if the parameter is `ValueType`
    /// the query must be singular (its single node's value, or Nothing, is used);
    /// if `LogicalType`, it is converted via an existence test.
    Query(FilterQuery),

    /// A logical expression argument (only valid for a `LogicalType` parameter).
    Logical(LogicalExpr),

    /// A nested function call. Valid for a parameter whose type matches the nested
    /// function's result type (with `NodesType` → `LogicalType` conversion allowed).
    Function(FunctionExpr),
}
