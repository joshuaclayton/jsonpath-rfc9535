# Default: run tests
default: test

# Lint with autofix: clippy fix + fmt
lint:
  cargo clippy --fix --allow-dirty --examples --test 
  cargo fmt

# Build
[group('dev')]
build:
  cargo build

# Install required tools for testing and coverage
[group('dev')]
setup: setup-nextest setup-coverage setup-audit setup-toml

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

# Run the criterion benchmarks. Pass criterion args after `--`, e.g.
# `just bench -- --baseline main` to compare against the saved baseline, or
# `just bench -- parse/child` to filter.
[group('dev')]
bench *ARGS:
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --bench queries {{ ARGS }}

# Cross-library comparison (jp-full vs jsonpath_lib / jsonpath-rust). Same `-- <args>` form.
[group('dev')]
bench-compare *ARGS:
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --features compare --bench comparison {{ ARGS }}

# Save the current numbers as the `main` baseline (the "where we started" reference).
[group('dev')]
bench-save-baseline:
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --bench queries -- --save-baseline main
  CRITERION_HOME={{ CRITERION_HOME }} cargo bench --features compare --bench comparison -- --save-baseline main

# Run the test suite
[group('test')]
test-suite: setup release
  cargo nextest run --workspace

# Verify code formatting
[group('test')]
test-fmt:
  cargo fmt --check

# Identify clippy warnings
[group('test')]
test-lint:
  cargo clippy --workspace --all-targets -- -D warnings

# Calculate code coverage and open in-browser
[group('test')]
test-coverage: setup
  cargo llvm-cov nextest --workspace --tests --html --open --ignore-filename-regex test_support

# Run doc tests
[group('test')]
test-doc:
  cargo test --doc

# Open docs
[group('docs')]
docs:
  cargo doc --open

# Verify examples compile
[group('test')]
test-examples:
  cargo check --examples

# Verify doc links are valid
[group('test')]
test-doc-links:
  RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --quiet

# Verify toml formatting
[group('test')]
test-toml: setup-toml
  taplo fmt -c .taplo.toml --check

# Run cargo audit
[group('test')]
test-audit: setup-audit
  cargo audit

# Run full test suite
[group('test')]
test: test-audit test-fmt test-lint test-suite test-coverage test-doc test-doc-links test-examples test-toml

[group('cargo')]
verify-publish:
  cargo publish --dry-run --allow-dirty
