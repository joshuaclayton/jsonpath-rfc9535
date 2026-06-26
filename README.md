# jp-full

A complete, [RFC 9535](https://www.rfc-editor.org/rfc/rfc9535) compliant JSONPath
query engine for [`serde_json`](https://docs.rs/serde_json) values.

JSONPath selects a set of nodes — the *nodelist* — from a JSON document. `jp-full`
compiles a query string once and evaluates it against any number of documents,
returning the selected values together with their **normalized paths**.

## Features

- **Full RFC 9535 grammar** — name, wildcard, index, slice, and filter selectors;
  child and descendant segments; nested filter expressions.
- **Compile-time validation** — `JsonPath::parse` parses *and* runs the RFC 9535
  §2.4 function well-typedness check, so a compiled query is guaranteed valid and
  well-typed. Malformed or ill-typed queries are rejected up front with a clear error.
- **Normalized paths** — every selected node carries its canonical location
  (`$['store']['book'][0]`) per RFC 9535 §2.7.
- **Function extensions** — `length()`, `count()`, `value()`, plus `match()` and
  `search()` (I-Regexp, [RFC 9485](https://www.rfc-editor.org/rfc/rfc9485)).

## Usage

```rust
use jp_full::JsonPath;
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

For a one-off query, the top-level `jp_full::query` function compiles and evaluates
in a single call.

## Cargo features

- **`regex`** *(enabled by default)* — provides the `match()` and `search()` function
  extensions, backed by an I-Regexp engine. With the feature disabled the crate builds
  without the [`regex`](https://docs.rs/regex) dependency, and a query that uses those
  functions fails to compile with a clear error.

## Conformance

Correctness is pinned to the official
[JSONPath Compliance Test Suite](https://github.com/jsonpath-standard/jsonpath-compliance-test-suite):
the engine is exercised against every case — both the expected nodelists (values and
normalized paths, checked in lockstep) and the cases that must be *rejected* at compile
time.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions.
