# Default: run every check (`just ci`)
default: ci

# Auto-fix lints and formatting (clippy --fix, then rustfmt)
[group('dev')]
lint:
  cargo clippy --fix --allow-dirty --examples --test 
  cargo fmt

# Build
[group('dev')]
build:
  cargo build

# Install required tools for testing, coverage, and the docs.rs-style doc build
[group('dev')]
setup: setup-nextest setup-coverage setup-audit setup-toml setup-nightly

# Install nextest for running tests
[group('dev')]
setup-nextest:
  cargo install cargo-nextest --locked

# Install code coverage tools
[group('dev')]
setup-coverage:
  rustup component add llvm-tools-preview
  cargo install cargo-llvm-cov

# Install cargo-audit for auditing dependencies
[group('dev')]
setup-audit:
  cargo install cargo-audit --locked

# Install taplo for checking toml formatting
[group('dev')]
setup-toml:
  cargo install taplo-cli --locked

# Install the nightly toolchain (used for the docs.rs-style doc build)
[group('dev')]
setup-nightly:
  rustup toolchain install nightly

# Run the tests in watch mode, re-running on file changes
[group('test')]
local-test: setup release
  CARGO_TEST=1 cargo watch -x "nextest run --workspace"

# Generate a release build
[group('dev')]
release:
  cargo build --release

# Criterion baselines persist under .criterion/ (gitignored) so they survive `cargo clean`.
CRITERION_HOME := ".criterion"

# Generate the standard benchmark fixtures (idempotent; pass sizes to generate.py to scale up)
[group('bench')]
bench-fixtures:
  python3 benches/data/generate.py

# Run the criterion benchmarks (`-- --baseline main` to compare a baseline, `-- parse/child` to filter)
[group('bench')]
bench *ARGS: bench-fixtures
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --features rayon --bench queries {{ ARGS }}

# Cross-library comparison (jsonpath-rfc9535 vs jsonpath_lib / jsonpath-rust). Same `-- <args>` form.
[group('bench')]
bench-compare *ARGS: bench-fixtures
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --features compare --bench comparison {{ ARGS }}

# Save the current numbers as the `main` baseline for future comparisons.
[group('bench')]
bench-save-baseline: bench-fixtures
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --features rayon --bench queries -- --save-baseline main
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --features compare --bench comparison -- --save-baseline main

# Run the test suite
[group('test')]
test: setup-nextest release
  cargo nextest run --workspace

# Run the test suite with parallel evaluation on — the CTS harness cross-checks
# query_values (parallel) against query (serial) on every case, so this pins
# parallel result order
[group('test')]
test-rayon: setup-nextest
  cargo nextest run --workspace --features rayon
  cargo test --doc --features rayon

# Run the test suite with the `regex` feature off — verifies match()/search() are rejected
[group('test')]
test-no-default: setup-nextest
  cargo nextest run --workspace --no-default-features
  cargo test --doc --no-default-features

# Run the test suite with the `scan` feature on, in both regex configurations — the
# scan module has regex-gated behavior (pushdown of match()/search() predicates) that
# only one build or the other exercises
[group('test')]
test-scan: setup-nextest
  cargo nextest run --workspace --features scan
  cargo test --doc --features scan
  cargo nextest run --workspace --no-default-features --features scan
  cargo test --doc --no-default-features --features scan

# Verify code formatting
[group('test')]
test-fmt:
  cargo fmt --check

# Identify clippy warnings
[group('test')]
test-lint:
  cargo clippy --workspace --all-targets -- -D warnings

# Clippy with the regex feature off (catches dead-code/cfg breakage in the no-regex build)
[group('test')]
test-lint-no-default:
  cargo clippy --workspace --all-targets --no-default-features -- -D warnings

# Clippy with parallel evaluation on
[group('test')]
test-lint-rayon:
  cargo clippy --workspace --all-targets --features rayon -- -D warnings

# Clippy with the scan feature on, in both regex configurations (the scan module has
# regex-gated match arms that only one build or the other compiles)
[group('test')]
test-lint-scan:
  cargo clippy --workspace --all-targets --features scan -- -D warnings
  cargo clippy --workspace --all-targets --no-default-features --features scan -- -D warnings

# Generate the HTML coverage report and open it (on demand; not part of `just ci`)
[group('test')]
coverage: setup
  cargo llvm-cov nextest --workspace --tests --html --open --ignore-filename-regex test_support

# Differential-fuzz the scan pipelines against DOM evaluation (on demand; not part of
# `just ci`). Requires nightly + cargo-fuzz; runs until interrupted unless bounded,
# e.g. `just fuzz -- -max_total_time=300`.
[group('test')]
fuzz *ARGS: setup-nightly
  cargo install cargo-fuzz --locked
  cargo +nightly fuzz run scan_differential {{ARGS}}

# Run doc tests
[group('test')]
test-doc:
  cargo test --doc

# Build the API docs and open them in a browser
[group('docs')]
docs:
  cargo doc --open

# Build the docs and fail on any warning or broken link (public + private items)
[group('test')]
test-doc-links:
  RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --quiet

# Build docs the docs.rs way (nightly + --cfg docsrs, -D warnings) — catches doc_cfg breakage pre-publish
[group('test')]
test-doc-cfg: setup-nightly
  RUSTDOCFLAGS="--cfg docsrs -D warnings" cargo +nightly doc --no-deps --all-features --quiet

# Verify toml formatting
[group('test')]
test-toml: setup-toml
  taplo fmt -c .taplo.toml --check

# Run cargo audit
[group('test')]
test-audit: setup-audit
  cargo audit

# Run every check CI runs; if this passes, it's ready to merge (coverage is separate: `just coverage`)
[group('test')]
ci: test-audit test-fmt test-lint test-lint-no-default test-lint-rayon test-lint-scan test test-no-default test-rayon test-scan test-doc test-doc-links test-doc-cfg test-toml

# Dry-run `cargo publish` to catch packaging problems before a real release
[group('cargo')]
verify-publish:
  cargo publish --dry-run --allow-dirty
