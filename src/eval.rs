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

/// A located node during evaluation: its path and a borrow of its value.
type Located<'a> = (NormalizedPath, &'a Value);

/// Evaluates `query` against `root`, returning the selected nodelist.
pub fn evaluate<'a>(query: &Query, root: &'a Value) -> NodeList<'a> {
    let mut nodes: Vec<Located<'a>> = vec![(NormalizedPath::root(), root)];
    for segment in &query.segments {
        nodes = apply_segment(segment, &nodes, root);
    }
    NodeList::new(
        nodes
            .into_iter()
            .map(|(path, value)| LocatedNode::new(path, value))
            .collect(),
    )
}

fn apply_segment<'a>(
    segment: &Segment,
    input: &[Located<'a>],
    root: &'a Value,
) -> Vec<Located<'a>> {
    let mut result = Vec::new();
    match segment {
        Segment::Child(selectors) => {
            for (path, value) in input {
                for selector in selectors {
                    result.extend(apply_selector(selector, path, value, root));
                }
            }
        }
        Segment::Descendant(selectors) => {
            for (path, value) in input {
                let mut visited = Vec::new();
                collect_descendants(path, value, &mut visited);
                for (descendant_path, descendant_value) in &visited {
                    for selector in selectors {
                        result.extend(apply_selector(
                            selector,
                            descendant_path,
                            descendant_value,
                            root,
                        ));
                    }
                }
            }
        }
    }
    result
}

/// Collects a node and all of its descendants in pre-order (a node before its
/// descendants; array elements in order; object members in map order).
fn collect_descendants<'a>(path: &NormalizedPath, value: &'a Value, out: &mut Vec<Located<'a>>) {
    out.push((path.clone(), value));
    match value {
        Value::Array(elements) => {
            for (index, element) in elements.iter().enumerate() {
                collect_descendants(&path.child_index(index), element, out);
            }
        }
        Value::Object(members) => {
            for (key, member) in members {
                collect_descendants(&path.child_name(key), member, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn apply_selector<'a>(
    selector: &Selector,
    path: &NormalizedPath,
    value: &'a Value,
    root: &'a Value,
) -> Vec<Located<'a>> {
    match selector {
        Selector::Name(name) => apply_name(name, path, value).into_iter().collect(),
        Selector::Wildcard => apply_wildcard(path, value),
        Selector::Index(index) => apply_index(*index, path, value).into_iter().collect(),
        Selector::Slice(slice) => apply_slice(slice, path, value),
        Selector::Filter(expr) => apply_filter(expr, path, value, root),
    }
}

fn apply_name<'a>(name: &str, path: &NormalizedPath, value: &'a Value) -> Option<Located<'a>> {
    value
        .as_object()
        .and_then(|members| members.get(name))
        .map(|member| (path.child_name(name), member))
}

fn apply_wildcard<'a>(path: &NormalizedPath, value: &'a Value) -> Vec<Located<'a>> {
    match value {
        Value::Array(elements) => elements
            .iter()
            .enumerate()
            .map(|(index, element)| (path.child_index(index), element))
            .collect(),
        Value::Object(members) => members
            .iter()
            .map(|(key, member)| (path.child_name(key), member))
            .collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Vec::new(),
    }
}

fn apply_index<'a>(
    index: ast::JsonInt,
    path: &NormalizedPath,
    value: &'a Value,
) -> Option<Located<'a>> {
    let elements = value.as_array()?;
    let (resolved, element) = element_at(elements, normalize(index.get(), elements.len())?)?;
    Some((path.child_index(resolved), element))
}

fn apply_slice<'a>(
    slice: &ast::Slice,
    path: &NormalizedPath,
    value: &'a Value,
) -> Vec<Located<'a>> {
    let Some(elements) = value.as_array() else {
        return Vec::new();
    };
    let Ok(len) = i64::try_from(elements.len()) else {
        return Vec::new();
    };
    let step = slice.step.map_or(1, ast::JsonInt::get);
    if step == 0 {
        return Vec::new();
    }
    let (start_default, end_default) = if step >= 0 {
        (0, len)
    } else {
        (len - 1, -len - 1)
    };
    let start = slice.start.map_or(start_default, ast::JsonInt::get);
    let end = slice.end.map_or(end_default, ast::JsonInt::get);
    let (lower, upper) = bounds(start, end, step, len);

    let mut out = Vec::new();
    let mut index = if step > 0 { lower } else { upper };
    while (step > 0 && index < upper) || (step < 0 && lower < index) {
        if let Some((resolved, element)) = element_at(elements, index) {
            out.push((path.child_index(resolved), element));
        }
        index = index.saturating_add(step);
    }
    out
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

fn apply_filter<'a>(
    expr: &LogicalExpr,
    path: &NormalizedPath,
    value: &'a Value,
    root: &'a Value,
) -> Vec<Located<'a>> {
    match value {
        Value::Array(elements) => elements
            .iter()
            .enumerate()
            .filter(|(_, element)| eval_logical(expr, element, root))
            .map(|(index, element)| (path.child_index(index), element))
            .collect(),
        Value::Object(members) => members
            .iter()
            .filter(|(_, member)| eval_logical(expr, member, root))
            .map(|(key, member)| (path.child_name(key), member))
            .collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Vec::new(),
    }
}

// ---- Filter expression evaluation ------------------------------------------------

/// Evaluates a relative (`@`) or absolute (`$`) query inside a filter, returning its
/// nodelist (paths are placeholders here, as filters observe only values/cardinality).
fn eval_filter_query<'a>(
    query: &FilterQuery,
    current: &'a Value,
    root: &'a Value,
) -> Vec<Located<'a>> {
    let start = match query.root {
        ast::QueryRoot::Current => current,
        ast::QueryRoot::Root => root,
    };
    let mut nodes: Vec<Located<'a>> = vec![(NormalizedPath::root(), start)];
    for segment in &query.segments {
        nodes = apply_segment(segment, &nodes, root);
    }
    nodes
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
            [(_, value)] => Comparand::Value(Cow::Borrowed(value)),
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
