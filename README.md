# DiffScope

DiffScope is a performance-driven tool for understanding the scope and impact of code changes.

It compares revisions and reports:

- Lines added, removed, and changed
- Files and functions touched by a diff
- Before-and-after function metrics, including LOC, cyclomatic complexity, and cognitive complexity

DiffScope is designed around a reusable analysis core. It will be available both as a command-line application and through adapters for multiple coding-agent harnesses.

The project prioritizes correctness, deterministic output, safe Rust, and measured performance. See [`AGENTS.MD`](AGENTS.MD) for its development rules.

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
```
