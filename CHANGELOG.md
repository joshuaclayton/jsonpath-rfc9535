# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `JsonPath::singular_steps()` — a query's member-name and array-index steps when
  it selects at most one node, or `None` when it has a wildcard, slice, filter,
  descendant segment, or several selectors in one segment. For callers that hand a
  path to another engine (such as SQLite's `json_extract`) and need to refuse
  queries that engine cannot evaluate. `SingularSegment` and `JsonInt` are now
  public. Member names are decoded. `SingularSegment` is exhaustive: RFC 9535
  §2.3.5.1 defines a singular query's segments as exactly names and indexes.

### Changed

- `NodeList::exactly_one()` is now a `const fn`.

## [0.1.1] - 2026-08-04

### Added

- `NormalizedPath::elements()` — the path's steps as structural `Element::Name` /
  `Element::Index` data, root → leaf, for callers that walk a document along the
  path (e.g. to reach a node's parent container for removal) instead of parsing
  the `Display` form. Member names are verbatim; §2.7 escaping stays a
  rendering-only concern.

## [0.1.0] - 2026-07-10

### Added

- RFC 9535 JSONPath queries over `serde_json::Value`: name, wildcard, index, slice, and
  filter selectors; child and descendant segments; and the `length`, `count`, `value`,
  `match`, and `search` functions (`match`/`search` behind the default `regex` feature).
- Normalized paths (RFC 9535 §2.7) for each selected node.
- Out-of-range array indexes and slice bounds are rejected with
  `Error::IntegerOutOfRange`, carrying the offending integer text verbatim, rather
  than a generic syntax error.
- Queries nesting brackets or parentheses deeper than 128 levels are rejected with
  `Error::NestingTooDeep`. Previously a hostile query of ~10 kB could exhaust the
  parser's stack and abort the process; only nesting is limited — flat queries of
  any length still parse.
- Conformance verified against the JSONPath Compliance Test Suite.
- A `jp` command-line tool (installed via `cargo install jsonpath-rfc9535`) that queries JSON from a file or stdin.
- An experimental `scan` feature: `ScanQuery` evaluates queries over raw JSON text by
  byte-scanning the structural prefix (rsonpath) and finishing filters per extracted
  fragment — including filter pushdown, which decides pushable predicates from
  auxiliary scans and parses only passing candidates. `ScanMode` selects between
  adaptive (default), always-scan, and never-scan strategies. Raises the crate's MSRV
  to Rust 1.89 (`rsonpath-lib` pins it; the bump applies to every build, scan enabled
  or not).
