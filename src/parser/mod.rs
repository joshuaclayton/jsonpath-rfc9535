//! The JSONPath query parser (nom).
//!
//! This module turns a query string into a syntactic [`crate::ast::Query`]. It is
//! deliberately *only* concerned with syntax — well-typedness of function
//! extensions (RFC 9535 §2.4.3) is checked later, during lowering to the compiled
//! IR. The grammar implemented here is the collected ABNF of [RFC 9535 Appendix A].
//!
//! # Layout / conventions
//!
//! * Each grammar rule has a corresponding combinator, grouped by topic into the
//!   submodules ([`string`], [`number`], [`selector`], [`mod@slice`], [`segment`],
//!   [`filter`], [`function`], [`query`]). Every combinator is annotated with the
//!   verbatim ABNF rule it implements.
//! * Combinators take `&str` and return [`nom::IResult`] over `&str`, e.g.
//!   `fn name_selector(input: &str) -> IResult<&str, String>`.
//! * The ABNF whitespace rules are explicit: `B` is one blank byte
//!   (`%x20 / %x09 / %x0A / %x0D`) and `S = *B` is optional blank space. Apply `S`
//!   **only** where the grammar places it — JSONPath does *not* allow arbitrary
//!   whitespace (e.g. no leading space before `$`, none inside a shorthand name).
//! * Integer range checking: a parsed integer must be turned into a
//!   [`JsonInt`](crate::ast::JsonInt) via [`JsonInt::new`](crate::ast::JsonInt::new),
//!   which rejects values outside the I-JSON safe range. In a combinator, the
//!   idiomatic way is `nom::combinator::map_res(int_i64, JsonInt::new)`.
//!
//! [RFC 9535 Appendix A]: https://www.rfc-editor.org/rfc/rfc9535#appendix-A

use crate::Error;
use crate::ast::Query;
use nom::bytes::complete::take_while;
use nom::{IResult, Parser, combinator::all_consuming};

pub mod filter;
pub mod function;
pub mod number;
pub mod query;
pub mod segment;
pub mod selector;
pub mod slice;
pub mod string;

/// Parses a complete JSONPath query string into a syntactic [`Query`].
///
/// This consumes the *entire* input: trailing junk is a syntax error.
///
/// # Errors
///
/// Returns [`Error::Syntax`] (with the byte offset at which parsing failed) if the
/// string is not a grammatically valid JSONPath query, or [`Error::IntegerOutOfRange`]
/// if an array index or slice bound lies outside the I-JSON safe range.
pub fn parse(input: &str) -> Result<Query, Error> {
    match all_consuming(query::query).parse(input) {
        Ok((_rest, query)) => Ok(query),
        Err(err) => Err(map_nom_error(input, &err)),
    }
}

/// Converts a nom error into a crate [`Error::Syntax`], recovering the byte offset
/// of the failure from the unconsumed remainder of the input.
fn map_nom_error(full: &str, err: &nom::Err<nom::error::Error<&str>>) -> Error {
    let position = match err {
        nom::Err::Error(inner) | nom::Err::Failure(inner) => {
            full.len().saturating_sub(inner.input.len())
        }
        nom::Err::Incomplete(_) => full.len(),
    };
    Error::Syntax {
        position,
        message: "the input is not a valid JSONPath query".to_owned(),
    }
}

/// rule: `S = *B` where `B = %x20 / %x09 / %x0A / %x0D` — optional blank space.
///
/// Consumes zero or more blank bytes (space, tab, LF, CR — *not* the full Unicode
/// whitespace set) and returns the consumed span. Apply this only at the exact
/// points the grammar permits whitespace.
pub fn s(input: &str) -> IResult<&str, &str> {
    take_while(|c| matches!(c, ' ' | '\t' | '\n' | '\r')).parse(input)
}
