# M1 — Module impact graph

Stage one of [Milestone 10](IMPACT-GRAPH.md#milestones). Read
[`IMPACT-GRAPH.md`](IMPACT-GRAPH.md) first: it defines the model, the vocabulary, and the
contract this stage implements a subset of.

## What becomes answerable

> Which modules import the changed file, which tests cover it, and which of those relationships
> did this change add or remove?

Every relationship in this stage is one DiffScope already resolves. Nothing new is parsed. What
is new is that relationships are resolved for **both** revisions, compared, given identity and
status, traversed with bounds, and rendered.

This is deliberately the whole of the first increment. The import graph is already built,
already cached, and already correct; the questions it can answer are the ones reviewers ask
first; and building the model on top of it proves the model before the hard resolution work
begins.

## Scope

Supported in this stage:

| Aspect | This stage |
| --- | --- |
| Node kinds | `module` |
| Relations | `imports`, `tested_by` |
| Resolutions | `resolved_specifier` (1.0), `test_imports_module` (0.9), `test_name_matches_module` (0.8) |
| Roots | `file`, or none (the changed set) |
| Views | `delta`, `base`, `target` |
| Directions | `upstream`, `downstream`, `both` |
| Renderings | dependency diff, Mermaid |
| Truncation | budgets and omitted counts |

Explicitly unsupported, and rejected rather than approximated:

- `function_id` as a root — `invalid_params`, with a message saying function roots need call
  resolution and naming `file` as what this version accepts.
- Any relation outside `imports` and `tested_by` — `invalid_params` naming the supported set.
- `cursor` — this answer is bounded by `max_nodes`, not paginated. `project()` already rejects a
  cursor on a non-paginated method.

`group` nodes and collapsing arrive in [M4](IMPACT-GRAPH-M4-READABILITY.md). This stage reports
omitted counts without collapsing.

## Contract delta

### Both revisions are indexed

`ImportIndex` is built for the target revision only today, and `HarnessSession::import_index`
(`src/harness/mod.rs:202`) resolves the target internally and keys the cache on that commit
alone. A delta needs the base as well.

- `Method::needs_import_graph` (`src/harness/mod.rs:333`) becomes a three-valued requirement:
  `None`, `Target`, `Both`. Existing methods return `Target` or `None` exactly as they do now,
  so nothing about their behavior changes.
- `HarnessSession` gains a per-commit lookup that either side can use. The existing
  `IndexEntry { repository_root, commit, index }` cache already keys on a commit, so it serves
  both sides unchanged; only the resolution of *which* commit moves out of the lookup.
- `answer` (`src/harness/mod.rs:403`) resolves what the method asked for and passes both
  indexes to `project`.
- `MAX_CACHED_INDEXES` (`src/harness/mod.rs:122`) rises from 4. At 4, one comparison holding a
  base and a target index leaves room for exactly one more, so two alternating comparisons
  evict each other's indexes on every query. 8 restores the four-comparison headroom the
  constant was chosen for.

The analysis cache already resolves both revisions to build its key
(`CacheKey { repository_root, base_commit, target_commit }`), so no additional `rev-parse` is
needed.

### New parameters

`QueryParams` (`src/harness/mod.rs:689`) gains `direction`, `relations`, `depth`, `view`,
`max_nodes`, `max_edges`, and `render`. The struct is `deny_unknown_fields`, so every transport
rejects a parameter that is not declared there — which is why the additions are deliberate and
why the older methods keep rejecting these names.

Parsing follows the existing convention: `parse_enum` (`src/harness/mod.rs:666`) with a
hand-written accepted-value string, and a `parse` associated function on each domain enum
beside `FileClassification::parse` and `RiskLevel::parse`. `depth`, `max_nodes`, and `max_edges`
clamp the way `canonical_limit` (`src/query/mod.rs:56`) clamps `limit`, and the clamped value is
what the `query` echo reports.

### New method

`Method::GetImpactGraph`, wire name `get_impact_graph`. Touch points, in order:

| Place | Change |
| --- | --- |
| `src/harness/mod.rs:321` | `Method` variant. |
| `src/harness/mod.rs:349` | `Method::parse` arm, and the inline accepted-methods string in its error. |
| `src/harness/mod.rs:333` | Import-graph requirement: `Both`. |
| `src/harness/mod.rs:344` | `paginated`: false. |
| `src/harness/mod.rs:456` | `project` arm, and a private `fn impact_graph` beside `summary` and `function_detail`. |
| `src/harness/mod.rs:689` | The seven new `QueryParams` fields. |
| `src/harness/mcp.rs:225` | One `ToolSpec`, plus an argument schema function per new parameter. |
| `src/harness/mcp.rs:30` | `INSTRUCTIONS` names the new tool in the workflow it describes. |
| `src/harness/jsonl.rs` | Nothing. It routes through `Method::parse` and `answer` with no per-method code. |
| `src/main.rs:47` | `Command::Graph(GraphArguments)`, an arm in `run_command`, and a handler. |

The echoed `query` object reports every parameter the method understands, with the defaults that
were applied and the clamped values that were used — the rule `HARNESS.md` already states.

### New modules

```text
src/graph/mod.rs        # model, identities, ordering, traversal, truncation
src/graph/build.rs      # build from an AnalysisResult and two ImportIndexes
src/graph/render.rs     # dependency-diff and Mermaid renderers
src/graph/recommend.rs  # visualization criteria
src/query/graph.rs      # request validation, serde views, the projection
```

`src/graph/` takes no dependency on `serde`, on the harness, or on the CLI.

## Building the graph

1. **Collect roots.** A named `file` must be a changed file in this analysis; naming an unchanged
   or unknown path is `invalid_params` with the closest known changed paths, mirroring
   `describe_unknown` (`src/harness/mod.rs:627`). With no root, every changed file is a root.
2. **Walk.** Breadth-first from each root to `depth` hops, over the union of the base and target
   indexes, following `imports` edges backwards for `upstream` and forwards for `downstream`.
   Record the fewest hops by which each node was reached. A node already seen is not re-queued,
   but the edge that reached it is kept, so cycles survive.
3. **Attach tests.** For every module node in the graph, add `tested_by` edges from the tests
   `src/query/impact.rs` would offer for it. The two resolutions and their confidences come from
   `TestLink` unchanged, so the graph and `get_function_change` cannot disagree about a test.
4. **Status every node and edge.** An edge's status is its membership in the two indexes. A
   module node's status is the file status the analysis reported, or `unchanged` for a module
   the diff does not contain.
5. **Apply the view.** `base` and `target` drop the edges that revision does not have. Statuses
   are not rewritten.
6. **Order, truncate, then key.** Order nodes and edges by the rules in
   [`IMPACT-GRAPH.md`](IMPACT-GRAPH.md#determinism); truncate to the budgets by the documented
   preference order; assign `n0`…`nN` after truncation, so keys are dense and are a function of
   the delivered graph.
7. **Recommend.** Evaluate the criteria over the truncated graph — the graph the reader will
   actually see — and emit one reason per signal that applied.

Evidence on an `imports` edge is the importing file and the line its import statement sits on.
The import scanner reports specifiers, not their positions, so this stage either extends
`ImportScan` to carry the specifier's line or omits `evidence` on `imports` edges. Extending the
scan is preferred and is cheap — the node is already in hand when the specifier is read — but it
touches a structure built over a whole revision, so it is measured before it is kept.

## Files touched

| File | Change |
| --- | --- |
| `src/graph/mod.rs` | New: model, ordering, traversal, truncation. |
| `src/graph/build.rs` | New: build from analysis and indexes. |
| `src/graph/render.rs` | New: dependency-diff and Mermaid renderers. |
| `src/graph/recommend.rs` | New: visualization criteria. |
| `src/query/graph.rs` | New: validation, serde views, projection. |
| `src/query/mod.rs` | Declare `graph` beside `classify`, `impact`, `risk`. |
| `src/lib.rs` | Declare `graph`; export what the CLI needs. |
| `src/imports.rs` | Specifier line numbers, if measurement keeps them. |
| `src/languages/mod.rs` | `ImportScan` carries specifier positions, if kept. |
| `src/harness/mod.rs` | Method, params, index requirement, cache size, projection. |
| `src/harness/mcp.rs` | Tool spec, argument schemas, instructions. |
| `src/main.rs` | `graph` subcommand, its arguments, its format enum, its handler. |
| `benches/analysis.rs` | `impact_graph` group. |

## Commits

The repository's order is contract, then implementation, then tests, in separate commits.

```text
docs: plan the change-impact graph                     # docs/plans/*, STRUCTURE.MD
docs: define the impact graph contract                 # DEFINITIONS.md, HARNESS.md, ARCHITECTURE.md, ROADMAP.md
feat(imports): index the base revision's module graph
test(imports): cover base and target index reuse
feat(graph): model a revision-delta impact graph
test(graph): cover statuses, traversal, and truncation
feat(graph): render dependency diffs and Mermaid
test(graph): cover deterministic rendering
feat(graph): recommend a diagram from measured signals
test(graph): cover the recommendation criteria
feat(query): answer impact graph queries
test(query): cover roots, views, and rejected relations
feat(mcp): expose the impact graph tool
feat(cli): add the graph subcommand
test(cli): cover graph formats and harness equivalence
bench: measure impact graph construction
docs: record the base index cost                       # PERFORMANCE.md
docs: describe the graph command and tool              # README.md
```

## Tests

### Unit, beside the modules

- Edge status from membership: an edge only in target is `added`, only in base is `removed`, in
  both is `unchanged`.
- Node status is taken from the analysis, not from membership: a modified file is `modified`
  even though it is in both revisions.
- Traversal bounds: `upstream` reaches importers and not dependencies, `downstream` the reverse,
  `both` reaches both; `depth` 1 stops at one hop.
- A cycle is retained: the closing edge appears, and the walk terminates.
- Truncation: the root survives a budget of 3; changed nodes outrank unchanged ones; `omitted`
  counts what was dropped and `reasons` names which budget was reached.
- Ordering is independent of insertion order: building the same graph from edges supplied in
  reverse produces identical nodes, edges, and keys. `ImportIndex::from_edges` already exists for
  exactly this kind of test.
- Render keys are dense and assigned after truncation.
- Mermaid label sanitization: a label with quotes, newlines, and 200 bytes produces the same
  fixed-width, quoted, `...`-terminated label every time.
- A removed edge and a low-confidence edge render as distinguishable dashed forms.
- Recommendation: each positive signal fires on its threshold and not below it;
  `linear_and_small` suppresses a three-node chain even when another signal applies.

### `tests/harness_jsonl.rs`

- `successful_answers_share_one_v2_envelope` gains the method: keys are exactly
  `["analysis", "data", "query"]`, with no `page`.
- `canonical_query_echoes_applied_parameters_and_defaults` gains an exact expected `query`
  object for `get_impact_graph`, including every default and the clamped values.
- New `invalid_params` cases: an unsupported relation, a `function_id` root, a `cursor`, a
  `depth` outside 1–3, and both `file` and `function_id` together. Each message names the field
  and what it accepts, and the stream survives every one.
- `repeated_queries_are_byte_identical` covers the method, including its rendered Mermaid.
- A delta test over a fixture repository: an import removed and another added produces one
  `removed` edge and one `added` edge, and the dependency diff shows them adjacent.

### `tests/mcp.rs`

`TOOLS` is `[&str; 5]` and its contents and order are asserted in
`handshake_negotiates_and_lists_exactly_the_five_tools`, with the count asserted again in
`client_disconnect_ends_the_session_cleanly`. Those assertions and the test's name change
together with the sixth tool — the breakage is the mechanism working, not an obstacle. The new
tool's schema is asserted like the others: `type: object`, `additionalProperties: false`,
`repository`/`base`/`target` required, and the read-only annotations present.

### `tests/output_golden.rs`

A golden dependency diff and a golden Mermaid document, built from an in-memory fixture as the
existing goldens are. `samples/queries/` is gitignored and read by nothing, so a captured answer
there is a local aid and not a fixture.

### `tests/cli.rs`

`diffscope graph` in each of `text`, `diff`, `mermaid`, and `json`, and a test asserting the
`json` output is identical to the JSONL answer for the same comparison.

### Fixture repositories

Covering an added edge, a removed edge, an import redirected from one module to another, a
cycle, a change spanning two areas, and a graph that exceeds both budgets.

## Benchmarks

`benches/analysis.rs` gains an `impact_graph` group beside the existing `import_graph` group,
which is already measured separately because its cost scales with the repository rather than
with the diff. It records:

- building the base index in addition to the target index;
- building the graph from two indexes, which should be far below either index;
- rendering, which should be negligible and is measured to prove it.

[`PERFORMANCE.md`](../PERFORMANCE.md) gains the measured base-index cost. The estimate — roughly
double the 412 ms Vue figure on a first query, near zero afterwards — is replaced by a
measurement, following the rule that a baseline nobody can reproduce is worse than none.

## Documentation

| Document | Change |
| --- | --- |
| `DEFINITIONS.md` | An impact-graph section: relations, statuses, resolutions and their confidences, traversal, truncation, determinism, and the recommendation criteria as a signal/trigger table. The query table and "Query result fields" gain the method. |
| `HARNESS.md` | The method in all five of its enumerations: the MCP tool table, the JSONL methods table, the import-graph sentence, the `query` echo list, and "Method data". |
| `ARCHITECTURE.md` | The graph component, its place in the component diagram, and the note that the import index is now built for both revisions. |
| `ROADMAP.md` | Milestone 10 and its exit condition. |
| `PERFORMANCE.md` | The measured base-index and graph-construction cost. |
| `README.md` | Five MCP tools becomes six; the `graph` subcommand in the command-line section; a worked example. |
| `STRUCTURE.MD` | `docs/plans/`, `src/graph/`, and the missing `src/query/`. |

## Exit condition

A comparison's module-level relationship changes are answerable from the CLI, the JSONL
protocol, and the MCP server; the three agree byte for byte; repeated queries are byte
identical; every unsupported input is rejected with a message naming what is accepted; and the
base index cost is measured rather than estimated.
