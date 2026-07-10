//! Errors raised while compiling a JSONPath query.
//!
//! Compiling a query (parsing plus the function well-typedness check mandated by
//! [RFC 9535 §2.4.3]) is the only fallible operation in this crate; *evaluating* a
//! compiled query against a document never fails. Every failure mode is therefore
//! represented by the single [`Error`] type returned when compiling a query.
//!
//! [RFC 9535 §2.4.3]: https://www.rfc-editor.org/rfc/rfc9535#section-2.4.3

use core::fmt;

/// The safe-integer range that JSONPath index and slice values must fall within.
///
/// RFC 9535 requires that index, slice (`start`/`end`/`step`) values lie in the
/// I-JSON interoperable integer range of `[-(2^53)+1, (2^53)-1]`
/// (see [RFC 9535 §2.1] and [RFC 7493 §2.2]). This is the upper bound, `(2^53)-1`.
///
/// [RFC 9535 §2.1]: https://www.rfc-editor.org/rfc/rfc9535#section-2.1
/// [RFC 7493 §2.2]: https://www.rfc-editor.org/rfc/rfc7493#section-2.2
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The lower bound of the safe-integer range, `-(2^53)+1`.
///
/// See [`MAX_SAFE_INTEGER`] for details.
pub const MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991;

/// The maximum bracket/parenthesis nesting depth a query may use.
///
/// The parser recurses once per nested filter (`[?…]`), parenthesized filter
/// expression, and function call, so unbounded nesting would let a ~10 kB hostile
/// query overflow the stack — a process abort, not a catchable panic. Real queries
/// never approach this limit: the deepest compliance-suite case nests 4 levels.
/// The value matches `serde_json`'s default document recursion limit and sits
/// several times below the measured overflow point in debug builds (the tighter
/// configuration). Only *simultaneous* nesting counts — sequential segments
/// (`$[0][1]…`) are parsed iteratively and are unlimited.
pub const MAX_NESTING_DEPTH: usize = 128;

/// An error produced while compiling a JSONPath query string into a runnable query.
///
/// Errors fall into two broad categories:
///
/// * **Syntax** — the query text is unacceptable: it does not conform to the
///   RFC 9535 grammar ([`Error::Syntax`]), an index or slice bound is out of range
///   ([`Error::IntegerOutOfRange`]), or it nests deeper than the parser supports
///   ([`Error::NestingTooDeep`]).
/// * **Well-typedness** — the query parses, but a function extension is used in a
///   way that RFC 9535 §2.4 forbids ([`Error::UnknownFunction`],
///   [`Error::FunctionArity`], [`Error::IllTyped`]).
///
/// For conformance purposes only the *presence* of an error matters: every
/// `invalid_selector` case in the compliance test suite must yield some `Error`.
/// The distinct variants exist to give callers actionable diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The query string violates the RFC 9535 grammar.
    ///
    /// `position` is the zero-based byte offset into the query at which the parser
    /// gave up; `message` describes what was expected.
    Syntax {
        /// Byte offset into the query string where parsing failed.
        position: usize,
        /// Human-readable description of the syntax problem.
        message: String,
    },

    /// An array index or slice bound lies outside the I-JSON interoperable
    /// integer range `[-(2^53)+1, (2^53)-1]`.
    ///
    /// `repr` is the offending integer as it appeared in the query (kept as text
    /// because the value may not fit in an [`i64`]).
    IntegerOutOfRange {
        /// The offending integer literal, verbatim from the query.
        repr: String,
    },

    /// The query nests brackets or parentheses deeper than the supported limit
    /// (currently 128 levels).
    ///
    /// Parsing recurses once per nested filter, parenthesized expression, or
    /// function call, so nesting is capped to keep a hostile query from exhausting
    /// the stack (an abort, not a catchable panic). Only *nesting* is limited —
    /// sequential segments (`$[0][1]…`) are parsed iteratively, and a query of any
    /// length is accepted as long as it does not nest this deep. The deepest
    /// compliance-suite case nests 4 levels.
    NestingTooDeep {
        /// Byte offset of the bracket or parenthesis that exceeded the limit.
        position: usize,
    },

    /// A filter calls a function extension whose name is not registered.
    ///
    /// RFC 9535 defines exactly five: `length`, `count`, `match`, `search`,
    /// and `value`.
    UnknownFunction {
        /// The unrecognized function name.
        name: String,
    },

    /// A function extension was called with the wrong number of arguments.
    FunctionArity {
        /// The function name.
        name: String,
        /// The number of arguments the function requires.
        expected: usize,
        /// The number of arguments supplied in the query.
        found: usize,
    },

    /// A function expression is not well-typed under RFC 9535 §2.4.3.
    ///
    /// Examples: passing a non-singular query where a `ValueType` is required
    /// (`length(@.*)`), comparing a `LogicalType` result (`match(@.a, 'x') == true`),
    /// or using a `ValueType` result as a bare test (`value(@..c)`).
    IllTyped {
        /// Human-readable description of the type violation.
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { position, message } => {
                write!(f, "syntax error at position {position}: {message}")
            }
            Self::IntegerOutOfRange { repr } => write!(
                f,
                "integer {repr} is outside the safe range [{MIN_SAFE_INTEGER}, {MAX_SAFE_INTEGER}]"
            ),
            Self::NestingTooDeep { position } => write!(
                f,
                "query nesting exceeds the supported depth of {MAX_NESTING_DEPTH} at position {position}"
            ),
            Self::UnknownFunction { name } => write!(f, "unknown function extension `{name}`"),
            Self::FunctionArity {
                name,
                expected,
                found,
            } => write!(
                f,
                "function `{name}` expects {expected} argument(s) but was given {found}"
            ),
            Self::IllTyped { message } => write!(f, "ill-typed function expression: {message}"),
        }
    }
}

impl std::error::Error for Error {}
