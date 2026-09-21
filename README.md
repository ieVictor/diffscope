![DiffScope banner](docs/banner.png)

# DiffScope

DiffScope is a performance-driven tool for understanding the scope and impact of code changes.

It compares revisions and reports:

- Lines added, removed, and changed
- Files and functions touched by a diff
- Before-and-after function metrics, including LOC, cyclomatic complexity, and cognitive complexity
- Per-function diff churn, changes to a module's export surface, and how confidently each function was matched across revisions

DiffScope is designed around a reusable analysis core. It is available as a command-line application; adapters for coding-agent harnesses can use the same public analysis API.

The project prioritizes correctness, deterministic output, safe Rust, and measured performance. See [`AGENTS.MD`](AGENTS.MD) for its development rules.

## Command line

Compare two committed Git revisions from the repository containing the current directory:

```sh
diffscope <BASE> <TARGET>
```

Use `--repository <PATH>` to select another repository and `--format json` for the complete analysis document, a versioned schema (`schema_version: 1`). Human output is the default. Successful and partially supported analyses exit with status `0`, analysis failures with `1`, and invalid command-line usage with `2`.

The reusable Rust entry point is `diffscope::analyze(&AnalysisRequest)`. Renderers in `diffscope::output` consume the returned `AnalysisResult` and do not perform analysis.

Run `diffscope --jsonl` for the long-lived harness protocol over standard input and output. The transport protocol version is `2`, and its query envelope is a separately versioned schema (`schema_version: 2`). See [`docs/HARNESS.md`](docs/HARNESS.md) for its request and response contract.

## Queries for coding agents

A complete analysis answers every question at once. For a 49-file diff that is over 1 MB of JSON, four fifths of it functions the change did not touch, which is more than a coding agent should spend its context on.

The harness protocol therefore answers scoped questions: an overview, a filtered and ranked page of candidates, then one function in full. Every successful response carries the same envelope — the analysis it projected, the canonical query and defaults it applied, the answer, and, for list methods, a page — so an answer can be interpreted without the request that produced it. Lists paginate by opaque cursor, and one function is drilled into by the `function_id` a list reports.

Files are classified as source, test, generated, vendored, lockfile, config, or docs, so a caller can ask for production code alone. Ranking is a documented, deterministic score over measured quantities: each function carries an intrinsic-risk score and a review-priority score, and every score carries structured reasons with stable codes, so a caller can rank on the numbers or on the reasons behind them. See [`docs/DEFINITIONS.md`](DEFINITIONS.md) for the queries, the classification rules, and the scoring models.

```sh
echo '{"protocol_version":2,"id":"1","repository":".","base":"main","target":"HEAD",
       "method":"list_changed_functions",
       "params":{"classification":"source","minimum_risk":"high","limit":10}}' | diffscope --jsonl
```

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
