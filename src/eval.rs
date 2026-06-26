//! The query evaluator: walks a compiled [`Query`](crate::compiled::Query) over a
//! [`serde_json::Value`] and produces a [`NodeList`].
//!
//! Evaluation is infallible — a compiled query always yields a (possibly empty)
//! nodelist. Results borrow from the input document. The comparison semantics
//! (RFC 9535 §2.3.5.2.2), the slice algorithm (§2.3.4.2.2), descendant pre-order
//! traversal (§2.5.2.2), and the function extensions (§2.4) are all implemented here.

use crate::ast;
use crate::compiled::FilterQuery;
use crate::compiled::{
    Comparable, Comparison, Function, LogicalExpr, Query, Segment, Selector, ValueArg,
};
use crate::node::{LocatedNode, NodeList};
use crate::normalized_path::NormalizedPath;
use core::cmp::Ordering;
use serde_json::{Number, Value};
use std::borrow::Cow;

/// How a node's location is tracked during traversal.
///
/// [`NormalizedPath`] builds the real path (for [`evaluate`], which fills a
/// [`NodeList`]). The no-op [`()`](unit) implementation is zero-sized, so value-only
/// queries ([`evaluate_values`]) and every filter sub-query skip path construction
/// entirely — the per-node `Rc` allocation that dominates wildcard/descendant traversal.
trait Position: Clone {
    /// The position of object member `name` reached from here.
    fn descend_name(&self, name: &str) -> Self;
    /// The position of array element `index` reached from here.
    fn descend_index(&self, index: usize) -> Self;
}

impl Position for NormalizedPath {
    #[inline]
    fn descend_name(&self, name: &str) -> Self {
        self.child_name(name)
    }
    #[inline]
    fn descend_index(&self, index: usize) -> Self {
        self.child_index(index)
    }
}

impl Position for () {
    #[inline]
    fn descend_name(&self, _name: &str) -> Self {}
    #[inline]
    fn descend_index(&self, _index: usize) -> Self {}
}

/// Evaluates `query` against `root`, returning the selected nodelist with normalized
/// paths.
pub fn evaluate<'a>(query: &Query, root: &'a Value) -> NodeList<'a> {
    NodeList::new(
        walk(&query.segments, NormalizedPath::root(), root, root)
            .into_iter()
            .map(|(path, value)| LocatedNode::new(path, value))
            .collect(),
    )
}

/// Evaluates `query` against `root`, returning just the selected values in order.
/// Tracks no paths (`P = ()`), so traversal performs no path allocation.
pub fn evaluate_values<'a>(query: &Query, root: &'a Value) -> Vec<&'a Value> {
    walk(&query.segments, (), root, root)
        .into_iter()
        .map(|((), value)| value)
        .collect()
}

/// Threads `segments` from `(start, start_value)`, carrying `root` for absolute filter
/// sub-queries. Generic over how positions are tracked (see [`Position`]).
fn walk<'a, P: Position>(
    segments: &[Segment],
    start: P,
    start_value: &'a Value,
    root: &'a Value,
) -> Vec<(P, &'a Value)> {
    let mut nodes: Vec<(P, &'a Value)> = vec![(start, start_value)];
    for segment in segments {
        // Most segments select roughly one node per input node (every name/index/filter
        // match), so pre-size to avoid the small-Vec realloc chain; wildcard/descendant
        // grow further via their own `reserve`/pushes.
        let mut next = Vec::with_capacity(nodes.len());
        apply_segment(segment, &nodes, root, &mut next);
        nodes = next;
    }
    nodes
}

/// Applies `segment` to every node in `input`, pushing selected nodes into `out`.
/// Selectors push directly into `out` — no per-node or per-selector intermediate
/// vector — and the descendant walk is fused with selection (see [`descend`]).
fn apply_segment<'a, P: Position>(
    segment: &Segment,
    input: &[(P, &'a Value)],
    root: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    match segment {
        Segment::Child(selectors) => {
            for (path, value) in input {
                for selector in selectors {
                    apply_selector(selector, path, value, root, out);
                }
            }
        }
        Segment::Descendant(selectors) => {
            for (path, value) in input {
                // Specialize the common `$..name` shape to a tight recursion that
                // skips the per-node selector-slice loop and enum dispatch of `descend`.
                match selectors.as_slice() {
                    [Selector::Name(name)] => descend_name(name, path, value, out),
                    _ => descend(selectors, path, value, root, out),
                }
            }
        }
    }
}

/// Specialized `$..name` descendant: pushes every object member named `name`, reachable
/// at any depth, in pre-order. Mirrors the general `descend` for a single name selector
/// but without per-node dispatch.
fn descend_name<'a, P: Position>(
    name: &str,
    path: &P,
    value: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    match value {
        Value::Object(members) => {
            if let Some(member) = members.get(name) {
                out.push((path.descend_name(name), member));
            }
            for (key, member) in members {
                descend_name(name, &path.descend_name(key), member, out);
            }
        }
        Value::Array(elements) => {
            for (index, element) in elements.iter().enumerate() {
                descend_name(name, &path.descend_index(index), element, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Applies `selectors` to `value` and every descendant in pre-order (a node before its
/// descendants; array elements in order; object members in map order), pushing matches
/// into `out`. Fused with the traversal, so the full descendant set is never collected.
fn descend<'a, P: Position>(
    selectors: &[Selector],
    path: &P,
    value: &'a Value,
    root: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    for selector in selectors {
        apply_selector(selector, path, value, root, out);
    }
    match value {
        Value::Array(elements) => {
            for (index, element) in elements.iter().enumerate() {
                descend(selectors, &path.descend_index(index), element, root, out);
            }
        }
        Value::Object(members) => {
            for (key, member) in members {
                descend(selectors, &path.descend_name(key), member, root, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[inline]
fn apply_selector<'a, P: Position>(
    selector: &Selector,
    path: &P,
    value: &'a Value,
    root: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    match selector {
        Selector::Name(name) => apply_name(name, path, value, out),
        Selector::Wildcard => apply_wildcard(path, value, out),
        Selector::Index(index) => apply_index(*index, path, value, out),
        Selector::Slice(slice) => apply_slice(slice, path, value, out),
        Selector::Filter(expr) => apply_filter(expr, path, value, root, out),
    }
}

#[inline]
fn apply_name<'a, P: Position>(
    name: &str,
    path: &P,
    value: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    if let Some(member) = value.as_object().and_then(|members| members.get(name)) {
        out.push((path.descend_name(name), member));
    }
}

#[inline]
fn apply_wildcard<'a, P: Position>(path: &P, value: &'a Value, out: &mut Vec<(P, &'a Value)>) {
    match value {
        Value::Array(elements) => {
            out.reserve(elements.len());
            for (index, element) in elements.iter().enumerate() {
                out.push((path.descend_index(index), element));
            }
        }
        Value::Object(members) => {
            out.reserve(members.len());
            for (key, member) in members {
                out.push((path.descend_name(key), member));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[inline]
fn apply_index<'a, P: Position>(
    index: ast::JsonInt,
    path: &P,
    value: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    if let Some(elements) = value.as_array()
        && let Some(position) = normalize(index.get(), elements.len())
        && let Some((resolved, element)) = element_at(elements, position)
    {
        out.push((path.descend_index(resolved), element));
    }
}

fn apply_slice<'a, P: Position>(
    slice: &ast::Slice,
    path: &P,
    value: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    let Some(elements) = value.as_array() else {
        return;
    };
    let Ok(len) = i64::try_from(elements.len()) else {
        return;
    };
    let step = slice.step.map_or(1, ast::JsonInt::get);
    if step == 0 {
        return;
    }
    let (start_default, end_default) = if step >= 0 {
        (0, len)
    } else {
        (len - 1, -len - 1)
    };
    let start = slice.start.map_or(start_default, ast::JsonInt::get);
    let end = slice.end.map_or(end_default, ast::JsonInt::get);
    let (lower, upper) = bounds(start, end, step, len);

    let mut index = if step > 0 { lower } else { upper };
    while (step > 0 && index < upper) || (step < 0 && lower < index) {
        if let Some((resolved, element)) = element_at(elements, index) {
            out.push((path.descend_index(resolved), element));
        }
        index = index.saturating_add(step);
    }
}

/// Normalizes a possibly-negative index against `len`, returning `None` if the array
/// length does not fit in `i64` (impossible for real documents).
fn normalize(index: i64, len: usize) -> Option<i64> {
    let len = i64::try_from(len).ok()?;
    Some(if index >= 0 { index } else { len + index })
}

/// The slice `Bounds` algorithm of RFC 9535 §2.3.4.2.2.
fn bounds(start: i64, end: i64, step: i64, len: i64) -> (i64, i64) {
    let normalize_bound = |bound: i64| if bound >= 0 { bound } else { len + bound };
    let normalized_start = normalize_bound(start);
    let normalized_end = normalize_bound(end);
    if step >= 0 {
        (normalized_start.clamp(0, len), normalized_end.clamp(0, len))
    } else {
        (
            normalized_end.clamp(-1, len - 1),
            normalized_start.clamp(-1, len - 1),
        )
    }
}

/// Returns the `(index, element)` at signed position `index`, if in bounds.
fn element_at(elements: &[Value], index: i64) -> Option<(usize, &Value)> {
    let resolved = usize::try_from(index).ok()?;
    elements.get(resolved).map(|element| (resolved, element))
}

#[inline]
fn apply_filter<'a, P: Position>(
    expr: &LogicalExpr,
    path: &P,
    value: &'a Value,
    root: &'a Value,
    out: &mut Vec<(P, &'a Value)>,
) {
    match value {
        Value::Array(elements) => {
            for (index, element) in elements.iter().enumerate() {
                if eval_logical(expr, element, root) {
                    out.push((path.descend_index(index), element));
                }
            }
        }
        Value::Object(members) => {
            for (key, member) in members {
                if eval_logical(expr, member, root) {
                    out.push((path.descend_name(key), member));
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

// ---- Filter expression evaluation ------------------------------------------------

/// Evaluates a relative (`@`) or absolute (`$`) query inside a filter, returning the
/// values it selects. Filters observe only values and cardinality, so this skips path
/// construction entirely (`P = ()`).
fn eval_filter_query<'a>(
    query: &FilterQuery,
    current: &'a Value,
    root: &'a Value,
) -> Vec<((), &'a Value)> {
    let start = match query.root {
        ast::QueryRoot::Current => current,
        ast::QueryRoot::Root => root,
    };
    walk(&query.segments, (), start, root)
}

fn eval_logical<'a>(expr: &LogicalExpr, current: &'a Value, root: &'a Value) -> bool {
    match expr {
        LogicalExpr::Or(left, right) => {
            eval_logical(left, current, root) || eval_logical(right, current, root)
        }
        LogicalExpr::And(left, right) => {
            eval_logical(left, current, root) && eval_logical(right, current, root)
        }
        LogicalExpr::Not(inner) => !eval_logical(inner, current, root),
        LogicalExpr::Comparison(comparison) => eval_comparison(comparison, current, root),
        LogicalExpr::Existence(query) => !eval_filter_query(query, current, root).is_empty(),
        LogicalExpr::Test(function) => eval_logical_function(function, current, root),
    }
}

fn eval_comparison<'a>(comparison: &Comparison, current: &'a Value, root: &'a Value) -> bool {
    let left = eval_comparable(&comparison.left, current, root);
    let right = eval_comparable(&comparison.right, current, root);
    compare(&left, comparison.op, &right)
}

/// A comparison operand: a JSON value (borrowed from the document or owned for a
/// literal/computed result) or the special "Nothing".
enum Comparand<'a> {
    Nothing,
    Value(Cow<'a, Value>),
}

fn eval_comparable<'a>(
    comparable: &Comparable,
    current: &'a Value,
    root: &'a Value,
) -> Comparand<'a> {
    match comparable {
        Comparable::Literal(literal) => Comparand::Value(Cow::Owned(literal_to_value(literal))),
        Comparable::Singular(query) => eval_singular(query, current, root)
            .map_or(Comparand::Nothing, |value| {
                Comparand::Value(Cow::Borrowed(value))
            }),
        Comparable::Function(function) => eval_value_function(function, current, root),
    }
}

/// Resolves a singular query to the at-most-one node it selects.
fn eval_singular<'a>(
    query: &ast::SingularQuery,
    current: &'a Value,
    root: &'a Value,
) -> Option<&'a Value> {
    let mut value = match query.root {
        ast::QueryRoot::Current => current,
        ast::QueryRoot::Root => root,
    };
    for segment in &query.segments {
        value = match segment {
            ast::SingularSegment::Name(name) => value.as_object()?.get(name)?,
            ast::SingularSegment::Index(index) => index_into(value, index.get())?,
        };
    }
    Some(value)
}

fn index_into(value: &Value, index: i64) -> Option<&Value> {
    let elements = value.as_array()?;
    let resolved = normalize(index, elements.len())?;
    element_at(elements, resolved).map(|(_, element)| element)
}

fn literal_to_value(literal: &ast::Literal) -> Value {
    match literal {
        ast::Literal::Number(number) => Value::Number(number.clone()),
        ast::Literal::String(string) => Value::String(string.clone()),
        ast::Literal::Bool(boolean) => Value::Bool(*boolean),
        ast::Literal::Null => Value::Null,
    }
}

// ---- Function extensions ---------------------------------------------------------

fn eval_value_function<'a>(
    function: &Function,
    current: &'a Value,
    root: &'a Value,
) -> Comparand<'a> {
    match function {
        Function::Length(arg) => length(&eval_value_arg(arg, current, root)),
        Function::Count(query) => Comparand::Value(Cow::Owned(json_number(
            eval_filter_query(query, current, root).len(),
        ))),
        Function::Value(query) => match eval_filter_query(query, current, root).as_slice() {
            [((), value)] => Comparand::Value(Cow::Borrowed(value)),
            _ => Comparand::Nothing,
        },
        Function::Match(..) | Function::Search(..) => Comparand::Nothing,
    }
}

fn eval_value_arg<'a>(arg: &ValueArg, current: &'a Value, root: &'a Value) -> Comparand<'a> {
    match arg {
        ValueArg::Literal(literal) => Comparand::Value(Cow::Owned(literal_to_value(literal))),
        ValueArg::Singular(query) => eval_singular(query, current, root)
            .map_or(Comparand::Nothing, |value| {
                Comparand::Value(Cow::Borrowed(value))
            }),
        ValueArg::Function(function) => eval_value_function(function, current, root),
    }
}

/// `length()`: Unicode scalar count of a string, element count of an array, member
/// count of an object; otherwise Nothing.
fn length<'a>(arg: &Comparand<'_>) -> Comparand<'a> {
    let Comparand::Value(value) = arg else {
        return Comparand::Nothing;
    };
    match value.as_ref() {
        Value::String(string) => Comparand::Value(Cow::Owned(json_number(string.chars().count()))),
        Value::Array(elements) => Comparand::Value(Cow::Owned(json_number(elements.len()))),
        Value::Object(members) => Comparand::Value(Cow::Owned(json_number(members.len()))),
        Value::Null | Value::Bool(_) | Value::Number(_) => Comparand::Nothing,
    }
}

fn json_number(count: usize) -> Value {
    Value::Number(Number::from(count))
}

#[cfg(feature = "regex")]
fn eval_logical_function(function: &Function, current: &Value, root: &Value) -> bool {
    match function {
        Function::Length(..) | Function::Count(..) | Function::Value(..) => false,
        Function::Match(target, pattern) => regex_test(target, pattern, current, root, true),
        Function::Search(target, pattern) => regex_test(target, pattern, current, root, false),
    }
}

// Without the `regex` feature, no `LogicalType` function can be compiled (the type
// checker rejects `match`/`search`), so this is never reached with a real value.
#[cfg(not(feature = "regex"))]
const fn eval_logical_function(_function: &Function, _current: &Value, _root: &Value) -> bool {
    false
}

#[cfg(feature = "regex")]
fn regex_test(
    target: &ValueArg,
    pattern: &ValueArg,
    current: &Value,
    root: &Value,
    anchored: bool,
) -> bool {
    let target_value = eval_value_arg(target, current, root);
    let pattern_value = eval_value_arg(pattern, current, root);
    let (Some(text), Some(expression)) =
        (comparand_str(&target_value), comparand_str(&pattern_value))
    else {
        return false;
    };
    crate::iregexp::build(expression, anchored).is_some_and(|regex| regex.is_match(text))
}

#[cfg(feature = "regex")]
fn comparand_str<'a>(comparand: &'a Comparand<'_>) -> Option<&'a str> {
    match comparand {
        Comparand::Value(value) => value.as_str(),
        Comparand::Nothing => None,
    }
}

// ---- Comparison semantics (RFC 9535 §2.3.5.2.2) ----------------------------------

fn compare(left: &Comparand, op: ast::ComparisonOp, right: &Comparand) -> bool {
    match op {
        ast::ComparisonOp::Eq => equal(left, right),
        ast::ComparisonOp::Ne => !equal(left, right),
        ast::ComparisonOp::Lt => less(left, right),
        ast::ComparisonOp::Le => less(left, right) || equal(left, right),
        ast::ComparisonOp::Gt => less(right, left),
        ast::ComparisonOp::Ge => less(right, left) || equal(left, right),
    }
}

fn equal(left: &Comparand, right: &Comparand) -> bool {
    match (left, right) {
        (Comparand::Nothing, Comparand::Nothing) => true,
        (Comparand::Value(left), Comparand::Value(right)) => value_eq(left, right),
        (Comparand::Nothing, Comparand::Value(_)) | (Comparand::Value(_), Comparand::Nothing) => {
            false
        }
    }
}

fn less(left: &Comparand, right: &Comparand) -> bool {
    match (left, right) {
        (Comparand::Value(left), Comparand::Value(right)) => value_less(left, right),
        (Comparand::Nothing, _) | (_, Comparand::Nothing) => false,
    }
}

fn value_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => number_eq(left, right),
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(l, r)| value_eq(l, r))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|other| value_eq(value, other)))
        }
        _ => false,
    }
}

fn value_less(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => number_less(left, right),
        (Value::String(left), Value::String(right)) => left < right,
        _ => false,
    }
}

fn number_eq(left: &Number, right: &Number) -> bool {
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        return left == right;
    }
    if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
        return left == right;
    }
    matches!(
        (left.as_f64(), right.as_f64()),
        (Some(left), Some(right)) if left.partial_cmp(&right) == Some(Ordering::Equal)
    )
}

fn number_less(left: &Number, right: &Number) -> bool {
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        return left < right;
    }
    if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
        return left < right;
    }
    matches!(
        (left.as_f64(), right.as_f64()),
        (Some(left), Some(right)) if left.partial_cmp(&right) == Some(Ordering::Less)
    )
}
