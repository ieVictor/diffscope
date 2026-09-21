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
- Source blobs larger than 5 MiB are inventoried with their file status and diff statistics but are not parsed. They report an `oversized_file` diagnostic and no function metrics. Analyzed repositories are untrusted and commonly contain generated or vendored sources of arbitrary size; a syntax tree costs many times the source it describes, and both revisions of a file are analyzed, so an unbounded file size is an unbounded memory requirement.

## LOC definitions

- `physical_loc`: count of newline-delimited physical lines in a source range. A final non-empty line without a trailing newline counts as one line.
- `source_loc`: count of physical lines in a source range excluding blank-only lines and comment-only lines.
- Blank-only lines contain only Unicode whitespace.
- Comment-only lines are lines whose first non-whitespace token belongs entirely to a language comment and which contain no executable/source token outside comments.
- Mixed code-and-comment lines count as source LOC.
- For whole-file diff totals, line counts are textual diff lines, not `physical_loc` or `source_loc`.
- LOC is calculated only for textual source files in supported languages. Unsupported languages, binary files, and files above the analysis size limit report an unavailable metric reason.

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
- `qualified_name` uses language namespace/module/type nesting where available, joined with `.`.
- A function that declares no name of its own is named by the construct that contains it, then by an ordinal within that construct:
  - When the function is an argument to a call, the call contributes a segment. A call that leads with a string literal contributes that literal, so a test callback is identified as `describe("parser").test("rejects empty input").<anonymous>#1`.
  - Otherwise the segment is the callee and the argument's position, as `useEffect#0`.
  - The ordinal counts only the anonymous functions of the same containing construct. It is never counted across the file: a file-wide ordinal makes a function's identity depend on how many anonymous functions precede it, so inserting one callback renames every later one and matching then pairs unrelated bodies.
  - Call labels are collapsed to single spaces and truncated to 64 bytes, with `...` appended, so that an arbitrarily long string literal cannot produce an arbitrarily long identity.
- `symbol_id` addresses one function within its file as `<kind>:<qualified_name>`, where kind is `fn`, `method`, `ctor`, or `arrow`. Queries accept either a `symbol_id` or a bare `qualified_name`.
- Before-and-after matching first uses stable semantic identity: language, qualified name, and kind within matched files.
- For renamed files, matching uses the base path and target path from the rename pair as the same file identity.
- If exactly one base function and one target function share the stable semantic identity, they are matched even when their source ranges or signatures changed.
- If multiple candidates share the same semantic identity on either side, DiffScope reports an `ambiguous_function_match` diagnostic for that identity and then resolves the group rather than discarding it. Dropping the group removed real, parsed functions from the result with no way to ask for them.
- Every matched pair reports a `match_confidence`:

  | Value | Basis |
  | ---: | --- |
  | `1.0` | Exactly one base function and one target function share the identity. |
  | `0.9` | Resolved from a group of identical identities by identical source, compared with whitespace runs collapsed. |
  | `0.6` | Resolved from a group of identical identities by source order, after identical sources were paired. |

  Identical sources are paired before order is considered, because an unchanged function is the common case and its partner is unambiguous. A caller that will not act on a guess can require `1.0`; the diagnostic is reported either way.
- If no target match exists, the function is `removed`. If no base match exists, it is `added`. If a match exists and a diff hunk intersects the function on either side, or its metrics changed, it is `modified`; otherwise it is `unchanged`.
- A function whose source range only shifts because of an edit elsewhere in the file is `unchanged`. Absolute ranges are not compared: in a large file one edit shifts every function below it, and reporting those as modified hides the functions that actually changed. A function moved within a file still intersects a hunk at its old and its new position, so it remains `modified`.
- Moved functions within a file are matched by semantic identity, not by line number.

## Line churn

- A diff is read with `--unified=0`, so a hunk header's line span is exactly the span of changed lines on that side and carries no context lines. A function's churn is the intersection of its source range with those spans.
- `lines_removed` counts changed lines inside the function's base range; `lines_added` counts them inside its target range.
- `changed_hunks` counts the hunks that touch the function on either side. A pure insertion at a function's boundary touches it without changing a line inside it, so the two numbers can disagree.
- `hunk_overlap` is the changed lines inside the function over its physical length, measured against the revision the function still exists in, and rounded to two decimals. It separates a small edit inside a large function from a function that is wholly new.

## Export surface

- A module's export surface is the set of names an importer can write, collected from `export` declarations, export clauses including renames, `export default`, and `export *`.
- A whole-module re-export is recorded as `*`, because the names it forwards live in a file this analysis has not read. A renamed export records only the exported name, not the local one.
- `exports_added` and `exports_removed` are the differences between the two revisions' sets. A change to a function's signature or body is not a surface change: the name every importer writes is unchanged.

## Path classification

Every changed file is classified from its path alone. No filesystem access, no file contents, so the result is identical for the same path on every machine. Rules are evaluated in this order and the first match wins:

| Order | Classification | Matches |
| ---: | --- | --- |
| 1 | `lockfile` | Known lock file names, such as `pnpm-lock.yaml` or `Cargo.lock`. |
| 2 | `vendored` | A `node_modules`, `vendor`, `third_party`, or `thirdparty` directory. |
| 3 | `generated` | A `dist`, `.next`, `coverage`, `generated`, or `__snapshots__` directory; a `.snap` file; a `.min.`, `.generated.`, or `.g.` infix. |
| 4 | `test` | A `__tests__`, `__mocks__`, `test`, `tests`, `spec`, `e2e`, or `testing` directory; a `.spec.`, `.test.`, `_test.`, `-test.`, or `.test-d.` infix; a `test_` prefix. |
| 5 | `config` | A dotted file or directory name; known config file names; `tsconfig*.json`; a `.config.` infix; a `.toml`, `.yaml`, `.yml`, `.ini`, or `.cfg` extension. |
| 6 | `docs` | A `.md`, `.mdx`, `.rst`, `.adoc`, or `.txt` extension. |
| 7 | `source` | Everything else. |

Ordering resolves overlap deliberately: a snapshot under `__tests__` is `generated`, because it is machine-written and not meant to be read as a test.

`build`, `out`, and `target` are excluded from rule 3. They are common enough as ordinary source directory names that matching them anywhere in a path would misreport real source files.

These are heuristics over naming conventions, not facts about a project. Every result carries its classification, so a caller that disagrees can rank on the underlying numbers instead.

## Import graph

Everything above describes files a diff contains. What a change reaches is a question about files it does not contain, so it is answered from an index of the target revision's module graph.

- Every source file in the target tree is scanned for the specifiers it imports or re-exports. Dynamic `import(...)` is not followed: its argument need not be a literal, and guessing at one would invent edges.
- Only each file's import region is parsed. An import statement's specifier follows the last `import` or `from` token in the statement, so the file is read up to that token plus 512 bytes. On a real Vue revision this recovers every import of all 491 TypeScript files while parsing 56% of their bytes. A file is additionally capped at 64 KiB and reports that it was cut, so an edge is never silently missing.
- Specifiers resolve against the paths actually present in the revision, trying `.ts`, `.tsx`, `.mts`, `.cts`, `.d.ts`, `.js`, and `.jsx`, then `index` inside a directory.
- `compilerOptions.paths` from the revision's `tsconfig.json` is applied, longest pattern first. In a monorepo most cross-package imports are written through these aliases — 18.4% of a Vue revision's specifiers are `@vue/*` — and without them every cross-package edge disappears. The file is read with comments and trailing commas tolerated, because TypeScript accepts both. A missing or malformed config yields no aliases rather than an error.
- A specifier that resolves to nothing is external, almost always an installed package. It is counted, not guessed at.

### Related tests

A test is offered as related to a changed file when it **imports that file directly** (confidence `0.9`) or when its **name matches** the file's, after extensions and test suffixes are removed (confidence `0.8`). Each result states which rule found it.

Indirect imports are deliberately not offered. Through a package's barrel module almost every test reaches almost every file: on a real Vue revision one shared utility is reached by 166 modules within two hops, and the tests that surface are the compiler's, not the utility's. That reach is still reported, as `nearby_importers`, because a large number is itself a useful signal that a file is widely re-exported.

## Risk

Risk ranks changed functions by how much review attention they are likely to need. It is a ranking aid: it does not judge whether code is good, and it cannot know what a change was for.

Each rule that applies contributes points and one reason:

| Points | Rule |
| ---: | --- |
| 3 / 2 / 1 | Cognitive complexity increased by at least 10 / 5 / 2. |
| 2 / 1 | Cyclomatic complexity increased by at least 5 / 2. |
| 2 / 1 | Cognitive complexity is at least 30 / 15 after the change. |
| 1 | At least 30 lines changed inside the function. |
| 1 | An added function whose cognitive complexity is already at least 10. |
| 3 | The function's name is no longer exported. |
| 1 | The function's name is newly exported. |
| 2 / 1 | The file is imported directly by at least 20 / 5 modules. |
| 1 | The file is classified `source`. |

A score of 5 or more is `high`, 2 or more is `medium`, and anything else is `low`. A match confidence below `1.0` adds a reason but no points, because uncertainty about identity is not by itself a reason to review.

The rules are additive so that no single large but harmless number, such as a reformatted file's churn, can dominate the ranking. Every assessment reports its score and its reasons, so a caller who disagrees with the weighting can ignore the level entirely.

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
- Parallel execution must not affect result ordering, including when analysis is throttled to bound memory.

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

- `exports_added` / `exports_removed`: names this change adds to or removes from the module's public surface. Omitted when the surface is unchanged.

Function result fields:

- `id`: deterministic identifier scoped to one analysis result.
- `status`: `added`, `removed`, `modified`, or `unchanged`.
- `kind`: language-specific function kind.
- `qualified_name`: qualified function name or deterministic synthetic name.
- `base_range`: source range before change, absent for added functions.
- `target_range`: source range after change, absent for removed functions.
- `metrics_before`: LOC and complexity metrics before change, absent when no base function exists.
- `metrics_after`: LOC and complexity metrics after change, absent when no target function exists.
- `change`: `changed_hunks`, `lines_added`, `lines_removed`, and `hunk_overlap` for this function. Omitted for a function the diff does not touch.
- `match_confidence`: how firmly this pair is believed to be the same function.
- `diagnostics`: function-level diagnostics.

Metric values are either available numeric values or unavailable with a reason and diagnostic code.

## Queries

A whole analysis answers every question at once and is far larger than any one question needs. Queries project one analysis into the shape a caller asked for. They add no analysis of their own, so a query answer never disagrees with the analysis it came from.

| Query | Returns |
| --- | --- |
| `get_change_summary` | File, line, function, and diagnostic counts; changed files per classification; per-area line totals and complexity totals; a short ranked shortlist of review candidates. |
| `list_changed_files` | Changed files, ranked, with per-file complexity totals and export changes. Filters: `classification`, `minimum_risk`. |
| `list_changed_functions` | Changed functions, ranked, with metric deltas, churn, risk and its reasons, and match confidence. Filters: `file`, `status`, `classification`, `minimum_risk`, `min_complexity_delta`, `include_unchanged`. |
| `get_function_change` | One function in full, with the hunks that touch it and its diagnostics. |
| `get_analysis_diagnostics` | Diagnostics for the analysis, optionally scoped to one file. |

- **Unchanged functions are never returned by default.** They are the large majority of any analysis. `include_unchanged` retrieves them.
- Every list paginates. `limit` defaults to 50 and is capped at 200; `offset` skips rows. Each answer reports `returned`, `total`, `has_more`, and `next_offset`.
- Ranking is total and deterministic. Functions order by risk score, then cognitive delta, then churn, then file path, then symbol. Files order by risk, then total changed lines, then path. Equal rows keep the analysis order, so paging never drops or repeats a row.
- Change areas are derived from paths. Inside a directory that holds one project per child, such as `packages`, the area is that child; otherwise it is the top-level directory. **An area's name is a path.** DiffScope does not infer a semantic label such as "runtime rendering" for a directory, because nothing in a diff says what a directory is for.
- Complexity is aggregated per file and per area over every function, including untouched ones. Summing both revisions is what makes a refactor legible as a unit: extracting a helper moves complexity out of one function into a new one, and only the totals show whether the change reduced complexity or merely relocated it.

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
