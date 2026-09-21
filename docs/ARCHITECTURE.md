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
- **Import index:** Resolves each revision's module graph, so a change can be related to the files and tests that reach it.
- **Query layer:** Projects one analysis into the answer a caller asked for. It filters, ranks, and scores; it performs no analysis of its own.
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

- Analyze only affected files and functions. The import graph is the one
  deliberate exception: "what breaks if this changes?" is a question about the
  files a diff does not contain, so that index is built over a whole revision.
  It parses only each file's import region, is keyed by the commit it
  describes, and is built only for the queries that use it.
- Read blobs directly from Git; do not create temporary checkouts.
- Process independent files in parallel.
- Cache analysis by blob identity, language, and analyzer version when measurement justifies it.
- Keep parsing and metric calculation allocation-conscious.
- Benchmark representative repositories and optimize only measured bottlenecks.

## Reliability boundaries

Repositories and source files are untrusted input. Parsing failures, malformed data, unsupported languages, binary files, and resource limits produce explicit diagnostics rather than panics. Metric definitions and machine-readable output are versioned so results remain reproducible.
