# Contributing

The project is currently developed and checked locally. Read [`AGENTS.MD`](AGENTS.MD) before making changes.

## Setup

Install the pinned Rust toolchain automatically by entering the repository, then install development tools:

```sh
./scripts/bootstrap.sh
```

## Local checks

Run the full local quality gate:

```sh
just check
```

Performance-sensitive changes should also run the relevant Criterion and end-to-end benchmarks:

```sh
just bench
just bench-cli
just bench-memory
```

See [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) for corpus definitions, measurement methodology, and the current local baseline.
