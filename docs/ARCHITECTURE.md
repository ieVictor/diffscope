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
 Versioned result model
    ├── Human-readable output
    └── JSON / JSONL output
```

- **Core analysis:** A library independent of terminal and harness concerns.
- **Application layer:** Coordinates revision resolution, analysis, and result generation.
- **Git adapter:** Reads revisions, diffs, renames, hunks, and blobs without modifying the working tree.
- **Language analyzers:** Use Tree-sitter grammars and language-specific rules to identify functions and calculate metrics.
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

## Performance model

- Analyze only affected files and functions.
- Read blobs directly from Git; do not create temporary checkouts.
- Process independent files in parallel.
- Cache analysis by blob identity, language, and analyzer version when measurement justifies it.
- Keep parsing and metric calculation allocation-conscious.
- Benchmark representative repositories and optimize only measured bottlenecks.

## Reliability boundaries

Repositories and source files are untrusted input. Parsing failures, malformed data, unsupported languages, binary files, and resource limits produce explicit diagnostics rather than panics. Metric definitions and machine-readable output are versioned so results remain reproducible.
