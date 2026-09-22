# Stable Definitions

This document defines DiffScope result semantics. Two schemas are versioned separately:

- The **core output schema**, version `1`, is the complete analysis document emitted by `--format json` and by the JSONL `analyze` method. It is versioned independently of the query API.
- The **query envelope schema**, version `2`, is the shape of the JSONL query API: a common envelope that carries the projection inputs, the applied query, the method answer, and a page. The query layer projects one analysis into the answer a caller asked for; it adds no analysis of its own, so a query answer never disagrees with the analysis it came from.

Unless a section names a schema, its definitions apply to both.

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

TypeScript and TSX functions calculate metrics from Tree-sitter function, method, constructor, and arrow-function nodes.

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
- The query layer addresses one function by `function_id`, a deterministic identifier unique within an analysis. It is built from the function's path, its symbol identity, the revision side the definition sits on, and its position there: `<path>#<symbol>@<base|target>:<line>:<column>`. A present target function is addressed at its target range, because that is what a caller reads and reviews; a removed function is addressed at its base range. A definition with neither range — no such case is produced today — would be addressed as `none` at line and column `0`. Two functions that share a name in one file — an addition and a removal, or one name declared on both sides — stay distinct.
- `symbol` is the human-readable identity `<kind>:<qualified_name>`, where kind is `fn`, `method`, `ctor`, or `arrow`. Query function records carry `symbol` and `qualified_name` next to `function_id`; only `function_id` is accepted by a detail query.
- The complete analysis document (core output schema version 1) keeps its own per-function `id`, a deterministic sequential identifier scoped to one analysis result. It is not a query handle.
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

Everything above describes files a diff contains. What a change reaches is a question about files it does not contain, so it is answered from an index of a revision's module graph.

- Every source file in that revision's tree is scanned for the specifiers it imports or re-exports. Dynamic `import(...)` is not followed: its argument need not be a literal, and guessing at one would invent edges.
- Only each file's import region is parsed. An import statement's specifier follows the last `import` or `from` token in the statement, so the file is read up to that token plus 512 bytes. On a real Vue revision this recovers every import of all 491 TypeScript files while parsing 56% of their bytes. A file is additionally capped at 64 KiB and reports that it was cut, so an edge is never silently missing.
- Specifiers resolve against the paths actually present in the revision, trying `.ts`, `.tsx`, `.mts`, `.cts`, `.d.ts`, `.js`, and `.jsx`, then `index` inside a directory.
- `compilerOptions.paths` from the revision's `tsconfig.json` is applied, longest pattern first. In a monorepo most cross-package imports are written through these aliases — 18.4% of a Vue revision's specifiers are `@vue/*` — and without them every cross-package edge disappears. The file is read with comments and trailing commas tolerated, because TypeScript accepts both. A missing or malformed config yields no aliases rather than an error.
- A specifier that resolves to nothing is external, almost always an installed package. It is counted, not guessed at.

### Related tests

A test is offered as related to a changed file when it **imports that file directly** (confidence `0.9`) or when its **name matches** the file's, after extensions and test suffixes are removed (confidence `0.8`). Each result states which rule found it.

Indirect imports are deliberately not offered. Through a package's barrel module almost every test reaches almost every file: on a real Vue revision one shared utility is reached by 166 modules within two hops, and the tests that surface are the compiler's, not the utility's. That reach is still reported, as `nearby_importers`, because a large number is itself a useful signal that a file is widely re-exported.

## Change-impact graph

The import graph answers how many modules reach a changed file. The change-impact graph answers **which** modules import it, which tests cover it, which modules re-export it, and — when the request names a function — which functions it calls and which functions call it, in its own file and in others, together with which of those relationships the comparison added or removed. It is built from one comparison's analysis, the import index of each revision, and a symbol index of the files cross-file resolution is allowed to read, and adds identity, status, a bounded walk, and presentation over them.

The graph is anchored to one comparison. A module map of a project, or of an unchanged revision, is not produced: nothing in a diff says what a directory means, so the graph reports relationships it resolved and never a judgment about structure.

### Relations

| Relation | Joins | Basis |
| --- | --- | --- |
| `imports` | An importer to a module it imports. | A specifier in the importer's import region resolved to a path in that revision. |
| `tested_by` | A test file to the module it covers. | The test imports the module directly, or its name matches the module's. |
| `calls` | A function to a function it calls. | Exact local, imported, namespace-import, or re-exported-symbol resolution. |
| `contains` | A module to a function it declares. | The function's definition is in that module, on one revision side or both. |
| `re_exports` | A module to a module it forwards names from. | An `export ... from` clause whose specifier resolved to a path in that revision. |
| `possible_call` | A function to one possible callee. | A property or computed call whose property name matches exactly one function visible to the caller. |

The supported relations are `imports`, `tested_by`, `calls`, `contains`, `re_exports`, and `possible_call`; a relation outside that set is rejected as `invalid_params` naming the accepted set rather than answered with an empty edge set. A request that omits `relations` walks every supported relation except `possible_call`. A caller includes that heuristic only by naming `possible_call` explicitly.

A `calls` edge is resolved exactly or not at all. Three shapes resolve: a callee the caller's own file declares; a callee bound by a named or default import whose specifier resolves to a module exporting a function under that name; and `ns.name()` where `ns` is a namespace import, because the receiver then names a module whose exported names are known. A re-export chain is followed for a bounded number of hops and has lower confidence. `possible_call` is not a `calls` edge: it is a `property_name_match` at confidence `0.5`, and only when one visible candidate matches exactly. Callbacks, dependency injection, dynamically selected properties with non-literal keys, runtime `import()`, and overloads distinguishable only by types remain unresolved; so do ambiguous names and calls outside the bounded symbol-resolution scope.

Callers in other files are discovered from the **direct importers** of the changed file, in both revisions. A file can only call into a module it imports, and the import index already names those files, so that set bounds what is parsed beyond the diff. Indirect importers are not read, for the same reason `related_tests` offers only direct importers: through a package's barrel module almost every file reaches almost every other, and a caller list built that way is not a caller list. A caller that reaches a changed function only through a barrel module is therefore not reported, and the reach that hides it is still reported as a number by `nearby_importers`.

There is no `imported_by`; it is an `imports` edge read backwards, and storing the reverse as its own relation makes two facts out of one and invites them to disagree. Direction of travel is a property of the walk, not of the edge. There is no generic `depends_on` either: a caller cannot tell whether such an edge came from an import or from a test-name match, so it cannot decide whether to trust it.

### Node and edge status

An edge's status is its membership in the two revisions, never an estimate:

| Present in base | Present in target | Status |
| --- | --- | --- |
| no | yes | `added` |
| yes | no | `removed` |
| yes | yes | `unchanged` |

An edge has no `modified`. A relationship whose target changed is one removed edge and one added edge, which is exactly what a reader needs to see.

A node's status is stronger than membership, because the analysis already computed it. A module takes the status of the file it names: `added` for a file the change creates, `removed` for one it deletes, `modified` for one the diff otherwise contains (an in-place edit, a rename, or a binary delta), and `unchanged` for a module the diff does not contain. A node is addressed at the path a reader would open: the target path when the file has one, otherwise the base path.

A function node takes the status the comparison gave the function, so `added`, `removed`, `modified`, and `unchanged` mean here what they mean everywhere else. Its identity is `function:<function_id>`, built from the same `function_id` the listing and detail queries publish, its label is the function's qualified name, and its path is the path of the file that declares it. Function nodes exist only when a request names a function root: a graph rooted at a file or at the changed set is the module graph exactly as before, with no function nodes and no `calls` or `contains` edges.

A function the diff does not contain — a caller or callee in an unchanged file — has no record to take a status from, so it takes the status its membership decides: `unchanged` when both revisions hold the definition, `added` when only the target does, `removed` when only the base does. Its identity has the same shape, naming the side the definition still exists on, so a function both revisions share is one node rather than two. A `contains` edge is attached only for the root module's own functions: the module that declares a caller in another file is the file-rooted question, not this one.

A `group` node is a synthetic node that stands for several collapsed module nodes. It carries `group: {"size": <count>, "role": <role>}` in addition to the ordinary node fields; its label is the count and role, followed by the shared change area when there is one. Grouping happens before delivery truncation, in this deterministic order: related tests; generated, vendored, and lockfile nodes; then remaining over-budget callers and dependencies separately. A group node ranks ahead of a node it replaces, but `truncated`, `omitted`, and `reasons` still report any delivered-graph budget loss.

That is why both revisions are indexed. A graph built from the target alone can show what exists now; it cannot prove that anything was removed, and a delta that cannot show a removal is not a delta.

### Resolutions and confidence

Confidence is fixed by how an edge was resolved, never a free-form number, so two runs cannot disagree and a reader can look up what `0.9` meant:

| Relation | Resolution | Confidence | Basis |
| --- | --- | ---: | --- |
| `imports` | `resolved_specifier` | 1.0 | An import statement whose specifier resolved to a path in the revision. |
| `tested_by` | `test_imports_module` | 0.9 | A test file imports the module directly. |
| `tested_by` | `test_name_matches_module` | 0.8 | A test file's name matches the module's, after extensions and test suffixes are removed. |
| `calls` | `direct_local_symbol` | 1.0 | The callee is a plain identifier with exactly one declaration in the same file. |
| `contains` | `declaration` | 1.0 | The function's definition is in that module, on one revision side or both. |
| `calls` | `imported_symbol` | 1.0 | The callee is a named, default, or namespace import resolving to a function the imported module exports itself. |
| `re_exports` | `export_clause` | 1.0 | An export clause forwards names from a module the specifier resolved to. |
| `calls` | `re_exported_symbol` | 0.9 | The callee resolves through one or more re-export hops. |
| `possible_call` | `property_name_match` | 0.5 | A property or computed call's name matches exactly one function visible to the caller. |

The two `tested_by` values are the ones [Related tests](#related-tests) publishes, so a test reported at `0.9` by `get_function_change` cannot appear in a graph at another number. A re-exported symbol is the one exact call resolution below certainty, and for a stated reason: each hop is a separate specifier resolution that could be wrong. `property_name_match` proves only that the name is unique in scope, not that the receiver contains that function, so it is distinct from `calls`.

An `imports`, `calls`, `possible_call`, or `re_exports` edge carries `evidence`: the source file and line in the revision that has the edge. A `tested_by` edge carries none: a name match has no site to point at, and the site of a direct import is the test file's own `imports` edge. A `contains` edge carries none either: it restates which module a function node came from rather than pointing at a relationship site.

### Traversal

- A walk starts at a root: one changed file, one function, or the changed set. With no root, every changed file becomes a root and the walk proceeds from all of them. `file` and `function_id` are mutually exclusive — a graph has one center — so naming both is rejected as `invalid_params`, and so is either root that the analysis does not contain. A function root is placed at the node of the function it names, and the walk follows `calls` in both directions and `contains` upward to the module that declares the function, so a function graph still shows which module the change sits in.
- `direction` decides which way edges are followed: `upstream` follows them backwards — what reaches the root — `downstream` follows them forwards, and `both` (the default) does both.
- `depth` is hops from the root: `1` by default, clamped to 1–3. One hop of importers and one of imports is what a reviewer reads; a deeper walk is available by request, because depth grows a graph far faster than it grows what the graph says.
- The walk is breadth-first and records the fewest hops by which each node was reached, matching `ImportIndex::reachable_importers`. A node already seen is not re-queued, but the edge that closed a cycle is kept. Strongly connected node sets are reported as cycles; Mermaid places a cycle of two or more nodes in a `subgraph` labelled, for example, `cycle · 3 nodes`. A cycle is `cycle_introduced` when that exact node set is cyclic in the target view but not the base view.
- Tests are attached to the modules the walk reached rather than reached by it: every test either revision offers for a module becomes a `tested_by` edge, and the test node sits one hop beyond the module it covers.
- `view` decides which relationships are shown: `delta` (the default) shows all of them, `base` shows only those present in the base revision, and `target` only those present in the target. A view narrows what is shown; it never rewrites what is true, so an edge shown under `target` still reads `added`.
- A `file` that is not a changed file in this analysis is rejected as `invalid_params`; the message names the closest known changed paths, the way a detail query names the closest known function ids.

### Truncation

Every walk is bounded and every omission is reported. Budgets default to 30 nodes and 60 edges and are request parameters: `max_nodes` is clamped to 3–100 and `max_edges` to 3–200.

Grouping happens before the node budget is applied. Tests collapse first, then generated, vendored, and lockfile nodes, then any remaining overflow becomes one callers group and/or one dependencies group. Groups retain the collapsed set's size and role; they do not make the graph complete or suppress truncation accounting.

After grouping, nodes are kept in this order:

1. the root or roots;
2. group nodes;
3. changed nodes — `added`, `removed`, or `modified`;
4. remaining nodes by hop distance, nearest first;
5. the graph's node ordering, as a total tiebreak.

Edges are kept when both endpoints were kept, then by the edge ordering. Any remaining loss is reported:

```json
{"truncated": true, "omitted": {"nodes": 47, "edges": 83}, "reasons": ["max_nodes"]}
```

`completeness` is always present beside `graph`, including when all counts are zero:

```json
{"scan_truncated_files":0,"unresolved_specifiers":0,"unresolved_calls":0,
 "relations_supported":["imports","tested_by","calls","contains","re_exports","possible_call"]}
```

Its scope is captured before view filtering, grouping, and delivery budgets: module graphs count reached module paths; function graphs count the root file and its direct importers. `delta` reports both revision sides, while `base` and `target` report only their selected side. `scan_truncated_files` counts files whose 64 KiB import scan cap was reached, `unresolved_specifiers` counts specifiers naming no file, and `unresolved_calls` counts examined call sites no exact or requested heuristic rule resolved. `relations_supported` is the running version's full supported set, not the request's selected relations.

### Determinism

Identical inputs produce byte-identical output, including the rendered strings:

- node identities come from stable identities — a repository-relative path — never from iteration order, and a node reached twice keeps the fewest hops it was reached by;
- nodes are ordered by kind, then path, then source position, then label, so functions of one file order by where they sit in it — the order `list_changed_functions` returns them in;
- edges are ordered by source node, then relation, then target node, then resolution, so a removal and the addition that replaced it sit together;
- render keys `n0`…`nN` are assigned after ordering and truncation, so they are dense and are a function of the delivered graph rather than of the walk that produced it;
- defaults, traversal, and the truncation preference order are fixed, and labels are sanitized identically every time.

The Mermaid **source** is deterministic; that is what DiffScope controls. Rendered geometry belongs to the Mermaid version and layout engine that draw it, so identical pixels require pinning those.

### Renderings

The dependency diff is one line per edge, ordered by the edge ordering: a removed relationship's line opens with `-`, an added one's with `+`, and an unchanged one's with a space, so every path starts at the same column. Endpoints are paths, not labels: a diff line is read as a place in the repository, and two files sharing a basename are two different files. A function endpoint is written `<path>::<qualified_name>`, so two functions of one file are two distinct lines rather than one path. A relation other than `imports` is named, so a `tested_by` line cannot be read as an import:

```text
- packages/compiler-sfc/src/style/cssVars.ts -> packages/compiler-sfc/src/legacyParser.ts
+ packages/compiler-sfc/src/style/cssVars.ts -> packages/compiler-sfc/src/parse.ts
  packages/compiler-sfc/src/compileStyle.ts -> packages/compiler-sfc/src/style/cssVars.ts
```

The Mermaid rendering is a `flowchart LR` document with one fixed `classDef` per node status, so a status never looks like two different things in two answers. A node's box shows its basename, with its status appended unless it is `unchanged`; group boxes show their deterministic count-and-role label. Cycles of two or more nodes appear in `subgraph` blocks labelled `cycle · <n> nodes`. Labels are quoted, quotes are dropped, control characters and whitespace runs collapse to single spaces, and the result is truncated to 64 bytes with `...` appended, so a hostile or merely long path cannot break the syntax or produce an unbounded diagram line. A removed edge renders as `-. "removed" .->`, and an edge below full confidence as `-. "~0.5" .->`: one dashed style for both would make "this relationship is gone" and "this relationship may exist" look alike when they are opposites.

### Visualization recommendation

A graph is produced when asked for, and the answer states whether a diagram is worth rendering and why. Each criterion is a fixed threshold over a measured quantity and contributes one `{code, message, value}` reason in the shape the risk models use; there is no score, because a "diagram score" would be an opaque number where an explainable list works.

A diagram is recommended when at least one positive signal applies and no negative signal does:

| Signal | Trigger |
| --- | --- |
| `many_callers` | Three or more relationships point at the root. |
| `many_dependencies` | The root points at three or more. |
| `converges_and_branches` | Two or more relationships point at the root and two or more leave it. |
| `crosses_areas` | The graph spans two or more change areas. |
| `cycle` | The graph contains a cycle: a node lies on a walk that leaves it and returns. |
| `cycle_introduced` | A cycle exists in the target and not the base. |
| `changed_on_both_sides` | At least one relationship changed on each side of the root. |
| `multiple_levels` | The relationships the comparison changed appear at two or more distinct hop levels from the root. |
| `linear_and_small` (negative) | Fewer than four nodes and no branching — no node has more than one relationship entering or leaving it. |

`multiple_levels` counts the distinct levels at which changed relationships sit — a level being the farther of an edge's two endpoints from the root — so a change that reaches a level or two below the root, which call edges make possible, is stated as a shape a list of lines reads poorly. `linear_and_small` is the straight line that should always be a dependency diff, and it overrides every positive signal. The criteria are evaluated over the truncated graph — the graph the reader will actually see — so a diagram is never recommended for a topology the answer does not carry. Change areas reuse the derivation under [Queries](#queries), so "spans two areas" means the same thing here as in a change summary.

## Risk and review priority

Two additive, deterministic scores rank changed functions. Both are ranking aids: they do not judge whether code is good, and they cannot know what a change was for.

- **Intrinsic risk** scores what a function's own code became: complexity and churn. It deliberately excludes the module's reach and the file's classification.
- **Review priority** starts from the intrinsic score and adds what the function's position exposes: its name in the module's public surface, how many modules import the file containing it, and whether that file is production source.

Each score is an assessment object with `model`, `maximum_score`, `score`, `level`, and `reasons`. Every reason is `{code, message, value}`; `value` carries the measured integer when the rule has one and is omitted otherwise. The models are `diffscope-risk-v1` (maximum score 8) and `diffscope-review-priority-v1` (maximum score 14).

Each rule that applies contributes points and one reason.

Intrinsic risk:

| Points | Code | Trigger | Message |
| ---: | --- | --- | --- |
| 3 / 2 / 1 | `cognitive_complexity_increased` | Cognitive complexity increased by at least 10 / 5 / 2. | `cognitive complexity increased by {n}` |
| 2 / 1 | `cyclomatic_complexity_increased` | Cyclomatic complexity increased by at least 5 / 2. | `cyclomatic complexity increased by {n}` |
| 2 / 1 | `cognitive_complexity_after` | Cognitive complexity is at least 30 / 15 after the change. | `cognitive complexity is {n} after the change` |
| 1 | `high_churn` | At least 30 lines changed inside the function. | `{n} lines changed inside the function` |
| 0 | `match_confidence` | Match confidence below `1.0`. | `matched with {f} confidence; verify it is the same function` |

An added function has no before, so its complexity is scored absolutely. These rules replace both the increase rule and the after rule, and never use "increased" wording:

| Points | Code | Trigger | Message |
| ---: | --- | --- | --- |
| 3 / 2 / 1 | `added_function_cognitive_complexity` | Cognitive complexity is at least 30 / 15 / 10. | `new function has cognitive complexity {n}` |
| 2 / 1 | `added_function_cyclomatic_complexity` | Cyclomatic complexity is at least 20 / 10. | `new function has cyclomatic complexity {n}` |

Review priority adds, after the intrinsic reasons (the match-confidence caveat stays last, as it does in intrinsic risk):

| Points | Code | Trigger | Message |
| ---: | --- | --- | --- |
| 3 | `export_removed` | The function's name is no longer exported. | `no longer exported; every importer of this name breaks` |
| 1 | `export_added` | The function's name is newly exported. | `newly part of the module's public surface` |
| 2 / 1 | `containing_module_direct_importers` | The containing module is imported directly by at least 20 / 5 modules. | `containing module is imported directly by {n} modules` |
| 1 | `production_source` | The containing file is classified `source`. | `production source file` |

Both models bucket the same way: a score of 5 or more is `high`, 2 or more is `medium`, and anything else is `low` (levels serialize as those lowercase strings). `review_priority.score` is at least `risk.score`, so its level is never below intrinsic risk's.

The rules are additive so that no single large but harmless number, such as a reformatted file's churn, can dominate the ranking. A match confidence below `1.0` adds a reason but no points, because uncertainty about identity is not by itself a reason to review. Every assessment reports its score and its reasons, so a caller who disagrees with the weighting can ignore the level entirely and rank on the underlying numbers.

## Diagnostics

Diagnostics are structured, deterministic, and attached to the narrowest applicable scope: repository, file, or function.

Required diagnostic categories:

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

Diagnostics include a stable code, severity (`info`, `warning`, or `error`), human message, optional path, optional range, and optional related entity identifiers. Diagnostics are sorted by severity, code, path, range, and message. Query answers report diagnostic counts as `info`, `warnings`, `errors`, and `total`, so a caller can size the whole list before reading it.

## Deterministic ordering

- Files are ordered by target path for added/modified/renamed/binary files and by base path for deleted files. Rename ties use base path.
- Hunks are ordered by target start line when present, otherwise base start line.
- Functions are ordered by target range start for present target functions, otherwise base range start, then qualified name and kind.
- Diagnostics are ordered as defined above.
- JSON object field order is not semantically meaningful, but golden outputs may use a stable renderer order.
- Parallel execution must not affect result ordering, including when analysis is throttled to bound memory.

## Core output schema version 1 result model

The complete analysis document has `schema_version: 1` and contains only data derived from explicit inputs. It is emitted by `--format json` and by the JSONL `analyze` method.

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

- `id`: deterministic sequential identifier scoped to one analysis result, such as `function-1`. The query layer addresses functions by `function_id` instead.
- `status`: `added`, `removed`, `modified`, or `unchanged`.
- `kind`: language-specific function kind.
- `qualified_name`: qualified function name or deterministic synthetic name.
- `base_range`: source range before change, absent for added functions.
- `target_range`: source range after change, absent for removed functions.
- `metrics_before`: LOC and complexity metrics before change, absent when no base function exists.
- `metrics_after`: LOC and complexity metrics after the change, absent when no target function exists.
- `change`: `changed_hunks`, `lines_added`, `lines_removed`, and `hunk_overlap` for this function. Omitted for a function the diff does not touch.
- `match_confidence`: how firmly this pair is believed to be the same function.
- `diagnostics`: function-level diagnostics.

Metric values are either available numeric values or unavailable with a reason and diagnostic code.

## Queries

A whole analysis answers every question at once and is far larger than any one question needs. Queries project one analysis into the shape a caller asked for. They add no analysis of their own, so a query answer never disagrees with the analysis it came from. The JSONL harness protocol carries them; [`HARNESS.md`](HARNESS.md) defines the transport, and the sections here define the semantics.

| Query | Returns |
| --- | --- |
| `get_change_summary` | File, line, function, and diagnostic counts; changed files per classification; per-area line totals and complexity totals; a short ranked shortlist of review candidates. |
| `list_changed_files` | Changed files, ranked, with per-file complexity totals, risk and review priority, change shape, and export changes. Filters: `classification`, `minimum_risk`. |
| `list_changed_functions` | Changed functions, ranked, with metric deltas, churn, risk and review priority, and match confidence. Filters: `file`, `status`, `classification`, `minimum_risk`, `min_complexity_delta`, `include_unchanged`. |
| `get_function_change` | One function in full, with the hunks that touch it, its reach, and its diagnostics. Addressed by `function_id` only. |
| `get_impact_graph` | The relationships the comparison added, removed, or left in place, walked from one changed `file`, one `function_id`, or the changed set and bounded by `direction`, `relations`, `depth`, `view`, `max_nodes`, and `max_edges`; `render` asks for the dependency-diff and Mermaid renderings. |
| `get_analysis_diagnostics` | Diagnostics for the analysis, optionally scoped to one file. |

Every query answer is wrapped in the common envelope:

- `analysis`: the projection inputs — an opaque, deterministic analysis id, the query envelope schema version (`2`), the tool version, and the resolved base and target revisions. The id is a digest of the resolved base commit, the resolved target commit, the tool version, and the envelope schema version, so it is identical for the same inputs in every process and changes when any input changes.
- `query`: the canonical applied parameters and defaults, so a response can be interpreted without its request.
- `data`: the method's answer, defined below.
- `page`: present for the two list methods, absent otherwise.

- **Unchanged functions are never returned by default.** They are the large majority of any analysis. `include_unchanged` retrieves them. `minimum_risk` compares a row's intrinsic-risk level; `min_complexity_delta` compares the larger of a function's cognitive and cyclomatic deltas.
- Every list paginates by opaque cursor. `limit` defaults to 50 and is clamped to 1–200. A cursor binds the schema version, analysis id, method, normalized applied query, and continuation position; a cursor presented with any of those changed is rejected as `invalid_params` rather than answered from the wrong list. Because an analysis is immutable, a continuation recorded in a cursor stays valid for the life of that analysis.
- Every list answer reports `returned`, `total`, `has_more`, and `next_cursor`. `next_cursor` is always present: `null` on the last page, a cursor string otherwise.
- Ranking is total and deterministic. Functions order by review-priority score, then intrinsic-risk score, then cognitive delta, then churn, then file path, then symbol. Files order by review-priority level, then intrinsic-risk level, then total changed lines, then path. Change areas order by review-priority level, then intrinsic-risk level, then file count, then name. Equal rows keep the analysis order, so paging never drops or repeats a row.
- Change areas are derived from paths. Inside a directory that holds one project per child, such as `packages`, the area is that child; otherwise it is the top-level directory. **An area's name is a path.** DiffScope does not infer a semantic label such as "runtime rendering" for a directory, because nothing in a diff says what a directory is for.
- Complexity is aggregated per file and per area over every function, including untouched ones. Summing both revisions is what makes a refactor legible as a unit: extracting a helper moves complexity out of one function into a new one, and only the totals show whether the change reduced complexity or merely relocated it.
- `change_shape`, reported on `list_changed_files` file records, is a statement about the shape of the change: `complexity_extraction` when a substantial added helper — an added function whose cognitive complexity after the change is at least 5 — coincides with no net file cognitive-complexity increase and with a modified or removed function that loses cognitive complexity. A pure extraction conserves the file total (the helper takes exactly what the function gave up), and conservation counts; a change that merely rearranged the same complexity without shrinking any function is `other`, which is also the value for every change that does not match all conditions.

### Query result fields

Shared shapes:

- `lines`: `added`, `removed`.
- `complexity`: `cyclomatic_before`, `cyclomatic_after`, `cyclomatic_delta`, `cognitive_before`, `cognitive_after`, `cognitive_delta`, `source_loc_before`, `source_loc_after`.
- `metrics`: `physical_loc`, `source_loc`, `cyclomatic_complexity`, `cognitive_complexity`, each an object of `before`, `after`, and `delta`. `before` is absent for an added function and `after` for a removed one.
- `change`: `changed_hunks`, `lines_added`, `lines_removed`, `hunk_overlap`. Absent for a function the diff does not touch.
- `risk`: the intrinsic assessment: `model`, `maximum_score`, `score`, `level`, `reasons`.
- `review_priority`: the review-priority assessment, in the same shape.
- `reason`: `code`, `message`, and `value` when the rule measured an integer.
- `impact`: `direct_importers`, `nearby_importers`, `related_tests`, each related test carrying `file`, `reason`, and `confidence`. The whole object is absent when the import graph was not built, which is not the same as nothing reaching the file.
- `page`: `returned`, `total`, `has_more`, and `next_cursor`.

`get_change_summary` data: `base` and `target` (each `id`, `display_name`); `files` (`changed`, `supported`, `unsupported`, `by_classification`); `lines`; `functions` (`added`, `removed`, `modified`, `unchanged`); `diagnostics` (`info`, `warnings`, `errors`, `total`); `change_areas`, each with `name`, `files`, `lines`, `risk`, `review_priority` (each a level, not the full assessment), and `complexity`; and up to five `review_candidates`, each with `function_id`, `file`, `symbol`, `status`, `complexity_delta` (`cyclomatic`, `cognitive`), `risk`, `review_priority` (each a level), and `match_confidence`.

`list_changed_files` data: `files` and `page`. Each file carries `path`, `renamed_from` when the path changed, `status`, `classification`, `language`, `area`, `lines`, `functions`, `complexity`, `risk`, `review_priority`, `change_shape`, `exports` (`added`, `removed`), `impact`, and `diagnostics`.

`list_changed_functions` data: `functions` and `page`. Each function carries `function_id`, `file`, `symbol`, `qualified_name`, `kind`, `status`, `classification`, `metrics`, `change`, `risk`, `review_priority`, `match_confidence`, and `range` (`before` and `after`, each `start_line` and `end_line`).

`get_function_change` data: `function` (the same record the list returns), `hunks` (`base_start`, `base_count`, `target_start`, `target_count`) for the hunks touching it, `impact`, and `diagnostics`. Naming a `function_id` the analysis does not contain is an error whose message names the closest known function ids.

`get_impact_graph` data: `root`, `graph`, and `visualization`, plus `dependency_diff` and `mermaid` when `render` asks for them. The answer is not paginated: it is bounded by `max_nodes` and `max_edges`, and a `cursor` is rejected as `invalid_params`.

- `root`: the root the request named, as `kind`, `id`, and `path`; `null` when the request named none and the graph is centered on the changed set. A file root is `kind: "module"` with `id: "module:<path>"` and `path` naming that file; a function root is `kind: "function"` with `id: "function:<function_id>"` and `path` naming the file that declares the function.
- `graph`: `nodes`, `edges`, `truncated`, `omitted`, and `reasons`.
  - A node carries `id`, `key` (`n0`…`nN`), `label`, `kind`, `path`, and `status`. A module node's id is `module:<path>`, its label is the path's basename, and its kind is `module`; a function node's id is `function:<function_id>`, its label is the function's qualified name, and its kind is `function`. Function nodes are present only in a graph rooted at a function.
  - An edge carries `from` and `to` node ids, `relation`, `status`, `resolution`, `confidence`, and `evidence` (`file`, `line`) when one site produced it.
  - `omitted` reports the dropped `nodes` and `edges` counts; `reasons` names the budgets that were reached — `max_nodes`, `max_edges`, or both — and is empty when nothing was dropped.
- `dependency_diff`: the dependency diff, one line per edge, as described under [Change-impact graph](#change-impact-graph).
- `mermaid`: the Mermaid `flowchart LR` document.
- `visualization`: `recommended` and `reasons`, each reason `{code, message, value}`.

`get_analysis_diagnostics` data: `diagnostics`, each with `code`, `severity`, `message`, `path`, and `related_entity_ids`; plus `counts` (`info`, `warnings`, `errors`, `total`).

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
