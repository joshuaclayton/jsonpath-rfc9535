# jsonpath-rfc9535

[![crates.io](https://img.shields.io/crates/v/jsonpath-rfc9535.svg)](https://crates.io/crates/jsonpath-rfc9535)
[![docs.rs](https://docs.rs/jsonpath-rfc9535/badge.svg)](https://docs.rs/jsonpath-rfc9535)

A complete [RFC 9535](https://www.rfc-editor.org/rfc/rfc9535) JSONPath query engine for
[`serde_json`](https://docs.rs/serde_json) values.

JSONPath selects a set of nodes — the *nodelist* — from a JSON document. `jsonpath-rfc9535`
parses and type-checks a query string once and evaluates it against any number of
documents, returning the selected values and, on request, their **normalized paths**.

## Features

- **Full RFC 9535 grammar** — name, wildcard, index, slice, and filter selectors;
  child and descendant segments; nested filter expressions.
- **Compile-time validation** — `JsonPath::parse` parses *and* runs the RFC 9535
  §2.4 function well-typedness check, so a compiled query is guaranteed valid and
  well-typed. Malformed or ill-typed queries are rejected at parse time, not at evaluation.
- **Normalized paths** — every selected node carries its canonical location
  (`$['store']['book'][0]`) per RFC 9535 §2.7.
- **Function extensions** — `length()`, `count()`, `value()`, plus `match()` and
  `search()` (I-Regexp, [RFC 9485](https://www.rfc-editor.org/rfc/rfc9485)).

## Install

```toml
[dependencies]
jsonpath-rfc9535 = "0.1"
```

## Command-line tool

The crate also ships a `jp` binary; `cargo install jsonpath-rfc9535` installs it. It reads
JSON from a file or stdin, applies a query, and prints each selected value as JSON:

```console
$ echo '{"a": [1, 2, 3]}' | jp '$.a[?@ > 1]'
[
  2,
  3
]
$ jp --paths '$.a[*]' data.json   # print normalized paths instead of values
```

## Usage

```rust
use jsonpath_rfc9535::JsonPath;
use serde_json::json;

let document = json!({
    "store": {
        "book": [
            { "title": "Sayings of the Century", "price": 8.95 },
            { "title": "Moby Dick", "price": 12.99 }
        ]
    }
});

// Compile once, evaluate against any number of documents.
let query = JsonPath::parse("$.store.book[?@.price < 10].title").expect("valid query");
let nodes = query.query(&document);

// Read back the selected values...
let titles: Vec<_> = nodes.values().collect();
assert_eq!(titles, [&json!("Sayings of the Century")]);

// ...and their normalized paths.
let paths: Vec<_> = nodes.paths().map(ToString::to_string).collect();
assert_eq!(paths, ["$['store']['book'][0]['title']"]);
```

### Values, or values and paths

`query` returns a `NodeList` pairing each value with its normalized path. When you
only need the values, `query_values` returns `Vec<&serde_json::Value>` and skips path
construction entirely — appreciably faster for wildcard- and descendant-heavy queries:

```rust
let document = serde_json::json!({ "a": [1, 2, 3] });
let query = jsonpath_rfc9535::JsonPath::parse("$.a[*]").expect("valid query");
let values = query.query_values(&document);
assert_eq!(values.len(), 3);
```

For a one-off query, the top-level `jsonpath_rfc9535::query` / `jsonpath_rfc9535::query_values` functions
compile and evaluate in a single call.

### Regex functions

With the default `regex` feature, filters may use `match()` (whole string, anchored) and
`search()` (substring), per I-Regexp:

```rust
let users = serde_json::json!([{ "name": "Ada" }, { "name": "linus" }]);
let capitalized =
    jsonpath_rfc9535::query_values(r#"$[?match(@.name, "[A-Z].*")]"#, &users)
        .expect("valid query");
assert_eq!(capitalized, [&serde_json::json!({ "name": "Ada" })]);
```

### Validating a query

`JsonPath::parse` rejects a malformed or ill-typed query, so it validates a query without
running it:

```rust
use jsonpath_rfc9535::JsonPath;

assert!(JsonPath::parse("$.store.book[0]").is_ok()); // valid
assert!(JsonPath::parse("$.store.book[").is_err()); // unclosed bracket
assert!(JsonPath::parse("$[?length(@.*) < 3]").is_err()); // ill-typed argument
```

## Performance

Results borrow from the input document — no JSON value is cloned. `query_values` is
path-free; `query` builds each node's `NormalizedPath` by borrowing the document's keys
(no per-step string allocation). Literal `match()` / `search()` patterns are compiled to
a regex once at `parse` time rather than per matched element. See [`benches/`](benches)
for the micro-benchmarks and cross-library comparisons (`just bench`, `just bench-compare`).

## Public API

The surface is deliberately small: `JsonPath`, the `query` / `query_values` free
functions, and the result types `NodeList`, `LocatedNode`, and `NormalizedPath` (plus
`Error`). The experimental `scan` feature adds `ScanQuery`, `ScanMode`, and `ScanError`.
The grammar AST, the compiled IR, the parser, and the evaluator are private
implementation details.

## Cargo features

- **`regex`** *(enabled by default)* — provides the `match()` and `search()` function
  extensions, backed by an I-Regexp engine. With the feature disabled the crate builds
  without the [`regex`](https://docs.rs/regex) dependency, and a query that uses those
  functions is rejected by `JsonPath::parse`.
- **`scan`** *(experimental, off by default)* — provides `ScanQuery`: evaluation over
  **raw JSON text** that byte-scans the query's structural prefix (via
  [rsonpath](https://docs.rs/rsonpath-lib)) and evaluates filters per extracted
  fragment — or, when a filter's predicate allows, decides it from auxiliary scans and
  parses only the values that pass. No whole-document DOM is ever built, so cost scales
  with what the query selects rather than document size: 2-25x faster than
  parse-then-query for one-shot text querying on selective queries. Adds the
  `rsonpath-lib`/`rsonpath-syntax` dependencies and raises the crate-wide MSRV to
  Rust 1.89 (scan enabled or not). See the `ScanQuery` docs for modes and caveats —
  in particular, scan-mode results over adversarial input have documented divergences
  from DOM evaluation and must not feed security decisions.

## Conformance

Correctness is pinned to the official
[JSONPath Compliance Test Suite](https://github.com/jsonpath-standard/jsonpath-compliance-test-suite):
the engine is exercised against every case — both the expected nodelists (values and
normalized paths, checked in lockstep) and the cases that must be *rejected* at compile
time.

## Development

This repo uses [`just`](https://github.com/casey/just) as its task runner; `just --list`
shows every recipe, grouped. Install the dev tooling once with `just setup`, then:

| Command | Purpose |
|---|---|
| `just ci` | Runs every check CI runs: fmt, clippy (both feature sets), the test suite (both feature sets), doc tests, doc generation + link check (incl. a docs.rs-style nightly build), audit, and toml. If it passes, it's ready to merge. |
| `just test` | Run just the test suite. |
| `just docs` | Build the API docs and open them in a browser. |
| `just coverage` | Generate the HTML coverage report and open it. |
| `just bench` | Run the criterion benchmarks (add `-- --baseline main` to compare against a saved baseline). |

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions.
