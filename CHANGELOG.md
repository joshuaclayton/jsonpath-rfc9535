# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- RFC 9535 JSONPath queries over `serde_json::Value`: name, wildcard, index, slice, and
  filter selectors; child and descendant segments; and the `length`, `count`, `value`,
  `match`, and `search` functions (`match`/`search` behind the default `regex` feature).
- Normalized paths (RFC 9535 §2.7) for each selected node.
- Conformance verified against the JSONPath Compliance Test Suite.
- A `jp` command-line tool (installed via `cargo install jsonpath-rfc9535`) that queries JSON from a file or stdin.
- An experimental `scan` feature: `ScanQuery` evaluates queries over raw JSON text by
  byte-scanning the structural prefix (rsonpath) and finishing filters per extracted
  fragment — including filter pushdown, which decides pushable predicates from
  auxiliary scans and parses only passing candidates. `ScanMode` selects between
  adaptive (default), forced-scan, and forced-DOM strategies. Requires Rust 1.89.
