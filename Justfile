set shell := ["bash", "-euo", "pipefail", "-c"]

# Show available project commands.
default:
    @just --list

# Install the pinned local-development tools.
bootstrap:
    ./scripts/bootstrap.sh

# Run the complete local quality gate.
check: format-check lint test deny unused-dependencies spelling toml-check

# Format Rust and TOML files.
format:
    cargo fmt --all
    taplo format

# Verify Rust formatting.
format-check:
    cargo fmt --all --check

# Run Clippy for every target and feature.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run tests with the next-generation test runner.
test:
    cargo nextest run --all-features --no-tests pass

# Check advisories, licenses, dependency bans, and sources.
deny:
    cargo deny check

# Detect dependencies that are no longer used.
unused-dependencies:
    cargo machete

# Detect spelling mistakes.
spelling:
    typos

# Validate formatting and syntax of TOML files.
toml-check:
    taplo format --check
    taplo lint

# Produce an HTML coverage report under target/llvm-cov/html.
coverage:
    cargo llvm-cov --all-features --html

# Generate deterministic small, medium, and large benchmark repositories.
bench-corpus:
    ./scripts/generate-benchmark-corpus.sh target/benchmark-corpus

# Run focused in-process benchmarks.
bench:
    cargo bench --bench analysis

# Measure the complete CLI, including process startup and JSON rendering.
bench-cli: bench-corpus
    cargo build --release
    hyperfine --warmup 3 './target/release/diffscope --repository target/benchmark-corpus/large --format json HEAD~1 HEAD > /dev/null'

# Report peak resident memory for the CLI process during a large-corpus analysis.
bench-memory: bench-corpus
    cargo build --release
    ./scripts/measure-peak-memory.sh ./target/release/diffscope --repository target/benchmark-corpus/large --format json HEAD~1 HEAD
