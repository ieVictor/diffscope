# Architecture

## Purpose

DiffScope measures the scope and impact of changes between Git revisions. It reports changed lines, touched files and functions, and before-and-after function metrics. The same analysis engine supports the CLI and multiple harness adapters.

## Components

```text
CLI / harness adapters
         │
         ▼
  Application layer
         │
         ▼
   Analysis engine
    ├── Git diff and blob loading
    ├── Language detection and parsing
    ├── Changed-function matching
    └── LOC and complexity metrics
         │
         ▼
 Complete analysis (core output schema v1)
    ├── Human-readable output
    ├── JSON output
    └── Query projections (query envelope schema v2)
         ├── Deterministic analysis identity
         ├── Risk and review-priority scoring
         ├── Cursor pagination
         └── Change-impact graph with dependency-diff and Mermaid renderings
```

- **Core analysis:** A library independent of terminal and harness concerns.
- **Application layer:** Coordinates revision resolution, analysis, and result generation.
- **Git adapter:** Reads revisions, diffs, renames, hunks, and blobs without modifying the working tree.
- **Language analyzers:** Use Tree-sitter grammars and language-specific rules to identify functions and calculate metrics.
- **Import index:** Resolves one revision's module graph, so a change can be related to the files and tests that reach it. It is keyed by the commit it describes and shared by every comparison that touches that commit; a delta query builds it for both compared revisions.
- **Query layer:** Projects one analysis into the answer a caller asked for. It filters, ranks, and scores; it performs no analysis of its own. A projection is deterministic and its answer never disagrees with the analysis it came from.
- **Change-impact graph:** Projects one comparison and both revisions' import indexes into an ordered, bounded graph of module relationships with their statuses, plus dependency-diff and Mermaid renderings. Every edge is a relationship an index already resolved; the graph adds identity, status, traversal, bounds, and presentation.
- **Risk and review-priority models:** Additive, documented scores over measured signals, each rule contributing a structured reason. Intrinsic risk covers complexity and churn; review priority adds public-surface, blast-radius, and source signals.
- **Interfaces:** The CLI and harness adapters translate inputs and outputs without implementing analysis logic.

Dependencies point inward: interfaces and infrastructure depend on the core, never the reverse.

## Analysis pipeline

1. Resolve base and target revisions.
2. Find changed files, renames, and diff hunks.
3. Load only affected blobs before and after the change.
4. Detect each file's language and parse supported files.
5. Map changed ranges to functions in both revisions.
6. Match functions across revisions.
7. Calculate LOC, cyclomatic complexity, and cognitive complexity.
8. Produce deterministic, schema-versioned results.

## Query protocol

The JSONL harness protocol answers scoped questions instead of returning the whole analysis:

- A successful response carries a common envelope: `analysis`, `query`, `data`, and — for list methods — `page`.
- `analysis` identifies the projection inputs: an opaque, deterministic analysis id, the query envelope schema version, the tool version, and the resolved base and target revisions. The id is a function of the resolved commits, the tool version, and the schema version, so it changes when any of them does.
- `query` echoes the canonical parameters and defaults that were applied, so a response can be interpreted without the request.
- Lists paginate by opaque cursor. A cursor binds the schema version, analysis id, method, normalized query, and continuation position; a mismatched cursor is rejected as `invalid_params` rather than answered from the wrong list.
- Function drill-down is addressed by `function_id`, an identifier that is collision-safe within one analysis and stable for the same inputs.

Two schemas are versioned separately:

- The **core output schema** (`schema_version: 1`) is the complete analysis document emitted by `--format json` and by the JSONL `analyze` method. It is versioned independently of the query API.
- The **query envelope schema** (`schema_version: 2`) is the shape of the JSONL query API described above, carried by transport protocol version 2.

## Performance model

- Analyze only affected files and functions. The import graph is the one
  deliberate exception: "what breaks if this changes?" is a question about the
  files a diff does not contain, so that index is built over a whole revision.
  It parses only each file's import region, is keyed by the commit it
  describes, and is built only for the queries that use it; a query that
  compares two revisions' relationships builds both, and each index is reused
  by every comparison that touches its commit.
- Read blobs directly from Git; do not create temporary checkouts.
- Process independent files in parallel.
- Cache analysis by blob identity, language, and analyzer version when measurement justifies it.
- Keep parsing and metric calculation allocation-conscious.
- Benchmark representative repositories and optimize only measured bottlenecks.

## Reliability boundaries

Repositories and source files are untrusted input. Parsing failures, malformed data, unsupported languages, binary files, and resource limits produce explicit diagnostics rather than panics. Metric definitions and machine-readable output are versioned so results remain reproducible.
