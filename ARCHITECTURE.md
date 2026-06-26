# Architecture

This document is for contributors. It maps the (private) internals behind the small
public API described in [`README.md`](README.md) and records the design decisions that
shaped them. For per-item detail, build the docs with internals visible:

```sh
cargo doc --document-private-items --open
```

## Pipeline: query string → nodelist

A query travels through three stages. Only the first two can fail; evaluation is
infallible.

```
&str ──parse──▶ ast::Query ──lower──▶ compiled::Query ──evaluate──▶ NodeList / Vec<&Value>
       (nom)      (syntactic)   (type-checked IR)         (borrows the document)
```

1. **Parse** (`src/parser/`, nom 8) → `ast::Query`. Pure syntax: one combinator per
   RFC 9535 Appendix A grammar rule. Produces the syntactic tree or `Error::Syntax` /
   `Error::IntegerOutOfRange`.
2. **Lower** (`src/compiled.rs`, `lower`) → `compiled::Query`. This pass *is* the
   RFC 9535 §2.4.3 function well-typedness check — it resolves each function call to a
   typed `Function`, enforces arity and `ValueType`/`NodesType`/`LogicalType` slot
   rules, and rejects ill-typed queries with `Error::UnknownFunction` /
   `Error::FunctionArity` / `Error::IllTyped`. It also precomputes the optimizations
   below (singular form, literal/regex compilation, existence form).
3. **Evaluate** (`src/eval.rs`) → `NodeList` (with paths) or `Vec<&Value>` (values
   only). Never fails; borrows from the input `serde_json::Value`.

`JsonPath::parse` runs stages 1–2 and stores the `compiled::Query`; `query` /
`query_values` run stage 3.

## Module map

| Module | Visibility | Role |
|---|---|---|
| `parser/` | private | nom combinators, grouped one submodule per topic (string, number, selector, slice, segment, filter, function, query). Each is annotated with its verbatim ABNF rule. |
| `ast` | private | The untyped syntactic tree — a faithful, desugared model of the grammar. Parser output; lowering input. |
| `compiled` | private | The typed IR plus `lower()` (the well-typedness checker). Mirrors the AST shape but with functions resolved and the eval-time optimizations baked in. |
| `eval` | private | The evaluator: worklist traversal, slice algorithm, comparison semantics, function extensions. |
| `iregexp` | private (feature `regex`) | I-Regexp (RFC 9485) → `regex`-crate pattern translation for `match()`/`search()`. |
| `normalized_path` | `NormalizedPath` public (opaque) | A node's canonical location as a shared `Rc` parent-chain; rendered via `Display`. |
| `node` | `NodeList`, `LocatedNode` public | Query results. |
| `jsonpath` | `JsonPath` public | The compile-once facade. |
| `error` | `Error` re-exported; module private | The single error type (compilation is the only fallible operation). |

### Why two parallel trees (`ast` vs `compiled`)

The AST is grammar-faithful (what the parser produced); the compiled IR is the
type-checked, lowered form. Keeping them separate means the type system enforces, at the
boundary, that the evaluator only ever sees well-typed queries — and lets the IR carry
representations the grammar can't (a resolved `Function` with exact argument slots, a
pre-built `regex::Regex`, the precomputed singular/existence forms). `lower()` is the
single conversion point. The leaf types that contain no function (`JsonInt`, `Slice`,
`ComparisonOp`, `Literal`, `QueryRoot`, `SingularQuery`) are reused from `ast` directly.

## Evaluation model

Evaluation is a **level/worklist** traversal: each segment maps the current nodelist to
the next. `walk()` threads a `Vec<(P, &Value)>`, where `P` is the position type:

- **`P = NormalizedPath`** builds real paths (for `query`).
- **`P = ()`** is a zero-sized no-op (for `query_values`): the same generic traversal,
  with all path work compiled away. This is why values-only evaluation pays no path cost.

The `Position<'a>` trait abstracts the two. Descendant segments use a fused pre-order
`descend` (the full descendant set is never materialized); the common `$..name` shape
has a specialized tight recursion (`descend_name`). Filters evaluate a `LogicalExpr` per
element.

## Performance decisions

Each was validated by A/B benchmark before landing; see [`benches/BASELINE.md`](benches/BASELINE.md)
for the measured impact and the reasoning (including approaches that were tried and
reverted, e.g. CPS for wildcard chains). In brief:

- **Path-free value path** — `Position = ()` makes `query_values` skip all path
  construction.
- **Borrowed path names** — `NormalizedPath<'a>` stores `&'a str` member names borrowed
  from the document's own map keys (via `Map::get_key_value`), so a name step costs only
  the parent-chain `Rc`, never a string copy.
- **Descendant leaf-prune** — `descend`/`descend_name` don't recurse into scalars
  (`is_container`); a leaf can't contain a deeper match, so building the path step to
  descend into it would be wasted.
- **Singular fast path** — a query of only single name/index child steps
  (`compiled::Query::singular`) is resolved by threading one `&Value` down the document,
  with no worklist and no per-segment `Vec`.
- **Filter literal precompute** — comparison literals are lowered to a
  `Box<serde_json::Value>` once and borrowed during comparison, not rebuilt per element.
- **Existence short-circuit** — a singular existence test (`?@.a.b`,
  `ExistenceTest::Singular`) is a path-free `eval_singular(...).is_some()` check, not a
  full nodelist build.
- **Regex precompile** — a literal `match()`/`search()` pattern compiles to a
  `regex::Regex` once at `parse` time (`compiled::Pattern::Literal`); only a
  document-derived pattern (`Pattern::Dynamic`) compiles per evaluation.

All query results **borrow** from the queried document: `NodeList<'a>`,
`LocatedNode<'a>`, and `NormalizedPath<'a>` cannot outlive the `Value` they came from.

## Conformance & tests

- **`tests/cts.rs`** — the official [JSONPath Compliance Test Suite] (`tests/data/cts.json`).
  Every case is checked for selected **values and normalized paths in lockstep**, plus
  invalid-query rejection, plus a `query_values == query().values()` cross-check.
- **`tests/properties.rs`** — proptests: parse-never-panics, normalized-path round-trip,
  and "a path re-selects its node."
- **`benches/`** — criterion micro-benchmarks (`queries.rs`) and cross-library
  comparison (`comparison.rs`, feature `compare`: `jsonpath_lib`, `jsonpath-rust`,
  `rsonpath`). Run with `just bench` / `just bench-compare`.

## Conventions & invariants

- **`JsonInt`** enforces the I-JSON safe-integer range `[-(2^53)+1, (2^53)-1]` at
  construction, so the parser and evaluator never re-check index/slice bounds.
- **Strict lints** — `pedantic` + `nursery` are `deny`, `#[allow]`/`#[expect]` are
  forbidden, and non-test code uses no `unwrap`/`expect`/`panic!`/indexing. The only
  relaxation is `expect` in tests (`clippy.toml`).
- **`regex` feature** gates `match()`/`search()`: with it off, the type checker rejects
  those functions at `parse` time, and the `regex` dependency is not built.
- **Parser** — nom 8, one combinator per ABNF rule, explicit whitespace (JSONPath does
  not allow arbitrary whitespace). Note the nom 8 `recognize`/`consumed` span gotcha
  worked around in `number.rs` (reconstruct the matched slice by length, not pointer
  offset).

[JSONPath Compliance Test Suite]: https://github.com/jsonpath-standard/jsonpath-compliance-test-suite
