# M4 — Readability, heuristics, and recommendation

Stage four of [Milestone 10](IMPACT-GRAPH.md#milestones). It depends on
[M3](IMPACT-GRAPH-M3-SYMBOLS.md), and it is the stage that makes the earlier three usable on
real changes rather than on fixtures.

## What becomes answerable

> The same answers, readable when the graph is large, honest about what could not be resolved.

Three problems appear once real repositories are involved, and none of them is solved by a
bigger budget.

- A changed module with 40 importers truncates to a number. A reader needs to know the shape —
  "12 direct callers" as one node — not that 37 nodes were dropped.
- JavaScript and TypeScript resolve many calls only at runtime. Reporting nothing for
  `handlers[type](source)` is honest but unhelpful when exactly one function in the revision has
  that name.
- A missing edge is indistinguishable from an absent relationship. A file whose import scan hit
  the 64 KiB cap, or a specifier that resolved to nothing, silently removes edges from the graph.

## Scope

Added in this stage:

| Aspect | Added |
| --- | --- |
| Node kinds | `group` |
| Relations | `possible_call` |
| Resolutions | `property_name_match` (0.5) |
| Response | completeness reporting |
| Recommendation | `cycle` grouping; signals that need call edges |

## Collapsing

When a budget would drop nodes, the dropped ones are replaced by one `group` node rather than by
a count in a footer. A picture that says `["12 direct callers"]` communicates the shape; a
picture missing 12 boxes with a note underneath does not.

Grouping rules, applied before truncation:

- **Tests are grouped separately.** A module with eight related tests contributes one
  `tested_by` edge to one group node. Tests are context, not topology, and eight boxes of
  context crowd out the structure the reader came for.
- **Generated, vendored, and lockfile nodes collapse.** The classification is already computed
  per path by `classify` (`src/query/classify.rs:134`), from the path alone with no filesystem
  access, so the same graph collapses identically on every machine.
- **External dependencies collapse.** A specifier that resolved to nothing is an installed
  package. It is already counted by `unresolved_specifiers` and it is not a node worth drawing
  individually.
- **Remaining overflow collapses by direction.** Importers that did not fit become one group,
  dependencies that did not fit become another, so the two sides of the root stay
  distinguishable.

A group node carries the count it replaces and, when its members share one classification or one
change area, that name. It is identified by what it collapses, so the same collapse yields the
same id and the rendered source stays deterministic.

`omitted` and `reasons` remain, because a group is not the same as the nodes it replaces and a
caller that wants them can raise the budget.

## Cycles

A cycle is retained by the traversal from M1 — a seen node is not re-queued, but the edge that
closed the cycle is kept. This stage makes it visible:

- Cycles among the graph's nodes are detected and reported, so `cycle` becomes a measured
  recommendation signal rather than an assertion.
- In Mermaid, a cycle's members render inside a `subgraph`, so the loop reads as a unit instead
  of as an arrow crossing the diagram.
- A cycle **introduced** by the change — present in the target and not the base — is its own
  recommendation reason. That is one of the few structural facts a reviewer would always want
  surfaced, and it is provable from the two indexes M1 added.

## Heuristic edges

Everything before this stage resolves exactly or emits nothing. That rule holds for `calls`; it
is why `calls` can be trusted. This stage adds a **different relation** for what cannot be
resolved exactly, so nothing about `calls` weakens.

`possible_call` at `property_name_match`, confidence 0.5: a call whose callee is a property or a
computed access, whose property name matches exactly one function name known in the graph's
resolution scope.

Deliberate limits, because a heuristic that fires often is noise:

- It applies only when the name matches **exactly one** candidate. A name matching two or more
  produces nothing — the same rule ambiguity already gets everywhere else in DiffScope.
- It never appears as a `calls` edge, is never counted where `calls` is counted, and is excluded
  from `relations` by default. A caller opts into it by name.
- It renders dashed with its confidence in the label, distinct from the `removed` label, so a
  reader can see at a glance which edges are guesses.

The cases that stay unresolved permanently are worth naming, so nobody plans for them: values
passed as callbacks, functions supplied by dependency injection, dynamically selected properties
with non-literal keys, runtime `import()`, and overloaded methods distinguishable only by types.
A Tree-sitter analyzer cannot resolve these, and this plan does not pretend otherwise.

## Completeness reporting

An incomplete graph currently looks like a complete small one. This stage reports what was not
seen.

`data` gains a `completeness` object:

| Field | Meaning |
| --- | --- |
| `scan_truncated_files` | Files in the graph's scope whose import region hit the 64 KiB cap, so later imports are not represented. |
| `unresolved_specifiers` | Specifiers in the graph's scope that named no file in the revision. |
| `unresolved_calls` | Call sites in the graph's scope that no resolution rule matched. |
| `relations_supported` | The relations this version can resolve. |

`ImportIndex::truncated_files` (`src/imports.rs:86`) exists and is read by nothing today. This
is what it was for: a file that reports it was cut is the difference between a missing edge and
a silently missing edge.

`relations_supported` is not redundant with the `query` echo. The echo says which relations were
requested; this says which the running version can resolve at all, which is what tells a reader
whether an empty caller list means "nothing calls it" or "this version cannot tell".

## Recommendation

With call edges and cycle detection available, the criteria complete:

| Signal | Trigger | New here |
| --- | --- | --- |
| `many_callers` | Three or more relationships point at the root. | |
| `many_dependencies` | The root points at three or more. | |
| `converges_and_branches` | Callers converge and dependencies branch. | |
| `crosses_areas` | The graph spans two or more change areas. | |
| `cycle` | The graph contains a cycle. | measured, not asserted |
| `cycle_introduced` | A cycle exists in the target and not the base. | yes |
| `changed_on_both_sides` | Relationships changed inbound and outbound. | |
| `multiple_levels` | Changed relationships more than one hop from the root. | |
| `linear_and_small` | Fewer than four nodes, no branching. Suppresses all of the above. | |

A candidate deferred from M1 becomes reasonable here: `get_function_change` could carry the
`visualization` object, so an agent reading one function learns a diagram would help without
asking for the graph first. It costs one traversal at depth 1 on a method that already builds
the import graph. This is a decision for the stage, not a commitment: it adds work to an answer
that does not need it, and the measurement decides.

## Files touched

| File | Change |
| --- | --- |
| `src/graph/mod.rs` | `group` node kind; `possible_call`; cycle detection; completeness. |
| `src/graph/build.rs` | Grouping before truncation; heuristic resolution; completeness collection. |
| `src/graph/render.rs` | Group nodes; cycle subgraphs; the heuristic edge style. |
| `src/graph/recommend.rs` | `cycle_introduced`; cycle and level signals measured. |
| `src/query/graph.rs` | The `completeness` view; `possible_call` opted into by name. |
| `src/query/mod.rs` | `visualization` on `get_function_change`, if measurement keeps it. |

## Commits

```text
docs: define grouping, heuristic edges, and completeness
feat(graph): collapse overflow into grouped nodes
test(graph): cover grouping, classification collapse, and determinism
feat(graph): detect and group cycles
test(graph): cover cycles and introduced cycles
feat(graph): offer property-name call matches at low confidence
test(graph): cover heuristic matching and its ambiguity rule
feat(graph): report what the graph could not resolve
test(graph): cover completeness reporting
docs: describe reading a large impact graph
```

## Tests

- A budget of 10 against 40 importers yields 10 nodes including one group node carrying 31, and
  the group is identified identically across runs.
- Tests collapse into one group even when the budget would have admitted them individually.
- A generated file and a vendored file collapse; a source file with the same name does not.
- A cycle renders inside a subgraph and the walk still terminates.
- A cycle present in the target and absent in the base produces `cycle_introduced`; one present
  in both produces only `cycle`.
- A property call whose name matches exactly one function produces a `possible_call` at 0.5; a
  name matching two produces nothing.
- `possible_call` is absent unless requested by name, and never appears as `calls`.
- A file cut by the import scan cap is counted in `scan_truncated_files` and the count survives
  into the answer.
- A graph over a revision with no truncation and no unresolved specifiers reports zeros rather
  than omitting `completeness`, so a caller can tell zero from absent.
- Grouping happens before truncation, so a group node is never itself dropped for a node it
  replaced.

## Exit condition

A graph over a real repository's widely-imported module is readable at the default budgets;
every collapsed set is visible as one node with its size; uncertain edges are a separate
relation that must be asked for by name; and an incomplete graph says so.
