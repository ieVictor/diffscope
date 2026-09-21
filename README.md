# DiffScope

![DiffScope banner](docs/banner.png)

DiffScope is a performance-driven tool for understanding the scope and impact of code changes.

It compares revisions and reports:

- Lines added, removed, and changed
- Files and functions touched by a diff
- Before-and-after function metrics, including LOC, cyclomatic complexity, and cognitive complexity

DiffScope is designed around a reusable analysis core. It is available as a command-line application; adapters for coding-agent harnesses can use the same public analysis API.

The project prioritizes correctness, deterministic output, safe Rust, and measured performance. See [`AGENTS.MD`](AGENTS.MD) for its development rules.

## Command line

Compare two committed Git revisions from the repository containing the current directory:

```sh
diffscope <BASE> <TARGET>
```

Use `--repository <PATH>` to select another repository and `--format json` for schema-versioned JSON. Human output is the default. Successful and partially supported analyses exit with status `0`, analysis failures with `1`, and invalid command-line usage with `2`.

The reusable Rust entry point is `diffscope::analyze(&AnalysisRequest)`. Renderers in `diffscope::output` consume the returned `AnalysisResult` and do not perform analysis.

Run `diffscope --jsonl` for the long-lived harness protocol over standard input and output. See [`docs/HARNESS.md`](docs/HARNESS.md) for its versioned request and response contract.

## Local development

Install the pinned development tools and run the local quality gate:

```sh
./scripts/bootstrap.sh
just check
```

Run performance benchmarks with:

```sh
just bench
just bench-cli
just bench-memory
```

See [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) for the generated corpus, methodology, and current baseline.
