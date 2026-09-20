# Stable Definitions

This document defines DiffScope result semantics for schema version `1`. Later milestones must preserve these definitions unless a documented defect requires an additive clarification or a future schema version.

## Revision and diff semantics

- A comparison has exactly two endpoints: `base` and `target`.
- Revisions are resolved by Git using the repository's object database and ref rules. The working tree is not modified and is not an implicit input unless a future API explicitly names it.
- All file paths in results are repository-relative, UTF-8 strings when representable, and use `/` separators. Paths are ordered by their target path when present, otherwise by their base path.
- File status values are:
  - `added`: path exists only in `target`.
  - `deleted`: path exists only in `base`.
  - `modified`: same path exists in both revisions and content changed.
  - `renamed`: Git reports a rename from a base path to a target path. Content may also have changed.
  - `binary`: Git identifies either side as binary. Binary files are inventoried but not parsed for source metrics.
- Diff hunks use Git line numbering: one-based line numbers and line counts per side. Empty ranges are represented by count `0` at Git's reported insertion/deletion anchor.
- Added and removed line totals count physical diff lines from textual hunks only. Context lines are excluded. Binary deltas do not contribute line counts.
- Unavailable blobs, missing revisions, unsupported object types, and Git errors produce diagnostics and prevent affected downstream metrics rather than panics.

## LOC definitions

- `physical_loc`: count of newline-delimited physical lines in a source range. A final non-empty line without a trailing newline counts as one line.
- `source_loc`: count of physical lines in a source range excluding blank-only lines and comment-only lines.
- Blank-only lines contain only Unicode whitespace.
- Comment-only lines are lines whose first non-whitespace token belongs entirely to a language comment and which contain no executable/source token outside comments.
- Mixed code-and-comment lines count as source LOC.
- For whole-file diff totals, line counts are textual diff lines, not `physical_loc` or `source_loc`.
- LOC is calculated only for textual source files in supported languages. Unsupported languages and binary files report an unavailable metric reason.

## Complexity definitions

Complexity metrics are language-specific implementations of these shared concepts. Rule-focused fixtures define exact behavior for every supported language construct.

### Cyclomatic complexity

- Each function starts at `1`.
- Add `1` for each independent control-flow decision inside the function body.
- Decisions include conditional branches, loops, pattern-match arms/cases after the first branch, short-circuit boolean operators that create an additional path, conditional expressions, exception/catch handlers, and language-specific early-branch constructs documented by that language analyzer.
- Nested functions are excluded from the enclosing function's complexity once the language analyzer can identify them as separate functions.

### Cognitive complexity

- Each control-flow break in linear reading adds `1`.
- A control-flow break nested inside another break adds an additional nesting penalty equal to its nesting depth.
- Else-if chains are counted as one additional decision per else-if without increasing nesting for the chain itself.
- Short-circuit boolean operators in conditions add cognitive cost when they add a distinct mental branch.
- Recursion, labeled jumps, and other language-specific readability costs are documented per language before support is declared.
- Nested functions are excluded from the enclosing function's cognitive complexity once separately identified.

### TypeScript metric rules

For schema version `1`, TypeScript and TSX functions calculate metrics from Tree-sitter function, method, constructor, and arrow-function nodes.

- LOC is measured over the complete function node range.
- Source LOC removes `//` line comments and `/* ... */` block comments before counting non-blank lines. Comment markers inside string and template literals are treated as source text.
- Cyclomatic complexity starts at `1` and adds `1` for each `if`, `for`, `for...in/of`, `while`, `do...while`, `catch`, ternary expression, `case` clause, and short-circuit `&&`, `||`, or `??` binary expression.
- Cognitive complexity adds `1 + nesting depth` for each `if`, loop, `catch`, ternary expression, and `case` clause. Short-circuit `&&`, `||`, and `??` add `1` without a nesting penalty.
- Nested functions are not included in the enclosing function's cyclomatic or cognitive complexity.

## Function identity and matching

- A function identity is composed of:
  - repository-relative file path for the relevant side,
  - language,
  - qualified name,
  - function kind where the language distinguishes functions, methods, constructors, accessors, closures, etc.,
  - source range.
- `qualified_name` uses language namespace/module/type nesting where available. Anonymous functions receive a deterministic synthetic name scoped by their containing construct and ordinal position.
- Before-and-after matching first uses stable semantic identity: language, qualified name, and kind within matched files.
- For renamed files, matching uses the base path and target path from the rename pair as the same file identity.
- If exactly one base function and one target function share the stable semantic identity, they are matched even when their source ranges or signatures changed.
- If multiple candidates share the same semantic identity on either side, DiffScope reports an `ambiguous_function_match` diagnostic for those candidates and does not guess.
- If no target match exists, the function is `removed`. If no base match exists, it is `added`. If a match exists and any intersecting diff hunk or metric/source range changed, it is `modified`; otherwise it is `unchanged`.
- Moved functions within a file are matched by semantic identity, not by line number.

## Diagnostics

Diagnostics are structured, deterministic, and attached to the narrowest applicable scope: repository, file, or function.

Required diagnostic categories for schema version `1`:

- `unsupported_language`
- `binary_file`
- `malformed_source`
- `parse_error`
- `invalid_utf8`
- `oversized_file`
- `missing_blob`
- `git_error`
- `ambiguous_function_match`
- `metric_unavailable`

Diagnostics include a stable code, severity (`info`, `warning`, or `error`), human message, optional path, optional range, and optional related entity identifiers. Diagnostics are sorted by severity, code, path, range, and message.

## Deterministic ordering

- Files are ordered by target path for added/modified/renamed/binary files and by base path for deleted files. Rename ties use base path.
- Hunks are ordered by target start line when present, otherwise base start line.
- Functions are ordered by target range start for present target functions, otherwise base range start, then qualified name and kind.
- Diagnostics are ordered as defined above.
- JSON object field order is not semantically meaningful, but golden outputs may use a stable renderer order.
- Parallel execution must not affect result ordering.

## Schema version 1 result model

The machine-readable output has `schema_version: 1` and contains only data derived from explicit inputs.

Top-level fields:

- `schema_version`: integer, currently `1`.
- `tool_version`: DiffScope version string.
- `repository`: repository identifier or path as provided/resolved by the caller.
- `base`: resolved base revision id and display name.
- `target`: resolved target revision id and display name.
- `summary`: aggregate changed file count, added lines, removed lines, supported/unsupported file counts, and diagnostic counts.
- `files`: ordered list of file results.
- `diagnostics`: repository-level diagnostics.

File result fields:

- `base_path`: path before change, absent for added files.
- `target_path`: path after change, absent for deleted files.
- `status`: file status.
- `language`: detected language when known.
- `is_binary`: boolean.
- `added_lines`: textual diff added line count.
- `removed_lines`: textual diff removed line count.
- `hunks`: ordered hunk list.
- `functions`: ordered function change list.
- `diagnostics`: file-level diagnostics.

Function result fields:

- `id`: deterministic identifier scoped to one analysis result.
- `status`: `added`, `removed`, `modified`, or `unchanged`.
- `kind`: language-specific function kind.
- `qualified_name`: qualified function name or deterministic synthetic name.
- `base_range`: source range before change, absent for added functions.
- `target_range`: source range after change, absent for removed functions.
- `metrics_before`: LOC and complexity metrics before change, absent when no base function exists.
- `metrics_after`: LOC and complexity metrics after change, absent when no target function exists.
- `diagnostics`: function-level diagnostics.

Metric values are either available numeric values or unavailable with a reason and diagnostic code.

## Correctness fixtures and benchmark corpus

Milestone 1 defines the required fixture inventory; later milestones implement them as executable tests.

Correctness fixtures must cover:

- additions, deletions, modifications, renames, and binary files;
- text files with and without final newlines;
- invalid UTF-8 and oversized-file handling;
- unsupported languages;
- malformed source in a supported language;
- functions with body edits, signature edits, movement within a file, deletion, addition, and ambiguous duplicate identities;
- adjacent and overlapping hunks;
- nested functions or language-equivalent nested callable constructs;
- every LOC, cyclomatic, and cognitive complexity rule for the first supported language.

Benchmark corpus tiers:

- `small`: fewer than 20 changed files, suitable for fast local iteration.
- `medium`: 20-200 changed files with mixed supported and unsupported content.
- `large`: more than 200 changed files and enough source volume to expose parsing, allocation, and Git I/O costs.

Benchmarks report wall-clock time, peak memory when practical, changed file count, changed function count, and analyzed byte count. Generated benchmark output is not committed.
