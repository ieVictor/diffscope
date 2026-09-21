# M2 — Local function calls

Stage two of [Milestone 10](IMPACT-GRAPH.md#milestones). It depends on
[M1](IMPACT-GRAPH-M1-MODULES.md), which delivers the model this stage adds node and edge kinds
to.

## What becomes answerable

> What does the changed function call inside its own file, and what calls it there?

This is the smallest step from modules to functions, and it is the one where resolution is
exact. A call to an identifier declared in the same file needs no import resolution, no
`package.json`, and no type information: the declaration is in the same tree the analyzer
already parsed.

It is also the step that makes the most common real change legible. A function extracted from
another function — the `complexity_extraction` shape `list_changed_files` already reports — is
one new node and one new `calls` edge, and the graph shows where the complexity went.

## Scope

Added in this stage:

| Aspect | Added |
| --- | --- |
| Node kinds | `function` |
| Relations | `calls`, `contains` |
| Resolutions | `direct_local_symbol` (1.0), `declaration` (1.0) |
| Roots | `function_id` |
| Recommendation | `multiple_levels` |

A call whose callee is not a plain identifier declared in the same file produces **no edge** in
this stage. It is not guessed at, not recorded at a lower confidence, and not counted. Heuristic
resolution arrives in [M4](IMPACT-GRAPH-M4-READABILITY.md) with its own relation and confidence;
until then, absence means "this stage did not resolve it", which the response says by reporting
which relations it supports.

## Collecting call sites

`FunctionDefinition` (`src/languages/mod.rs:32`) carries a range, metrics, and a body hash.
It gains the call sites inside the function.

The collection is a new pass in the existing style. There are no Tree-sitter queries anywhere in
the codebase — every traversal is a hand-written recursive walk — and this stage does not
introduce the first one. `complexity_points` (`src/languages/typescript.rs:460`) already
demonstrates the exact shape needed:

```rust
// recurse, but stop at a nested function's boundary
if child.start_byte() != node.start_byte() && function_kind(child, "").is_some() {
    continue;
}
```

A call inside a nested arrow function belongs to that arrow function, not to its enclosing
function, for the same reason complexity does: attributing it upward would make the enclosing
function appear to call things it does not, and the nested function is already its own node.

For each `call_expression` and `new_expression` reached by that walk, the collector records the
callee text and its position. Only a plain identifier callee is recorded in this stage; a member
expression, a computed access, or a call on a call is left for M3 and M4.

Two properties matter and are tested:

- The walk is folded into the traversal `function_metrics` already performs.
  `function_metrics` exists as one traversal precisely because "walking the function twice, once
  per metric, doubles the most expensive step of the analyzer", and
  [`PERFORMANCE.md`](../PERFORMANCE.md) attributes 53% of analysis time to the function
  collector. A separate walk for calls would repeat that mistake.
- Call sites are held per function, and a large revision holds thousands of functions.
  [`PERFORMANCE.md`](../PERFORMANCE.md) records that container-scoped identities, churn, body
  hashes, and export sets together cost up to 21% in peak RSS on the largest corpus. Call sites
  are the same kind of cost and are measured the same way, against `vue-span`, before they are
  kept.

## Resolving a local call

Within one file's analysis:

1. Build a map from declared name to function definition, using the leaf segment of each
   `qualified_name`.
2. For each call site, look its callee text up in that map.
3. A unique hit is a `calls` edge at `direct_local_symbol`, confidence 1.0, with the call site
   as evidence.
4. A name that resolves to more than one definition in the file produces **no edge**. Two
   same-named functions in one file are exactly the case `ambiguous_function_match` already
   exists for, and a call graph that picks one arbitrarily is worse than one that says nothing.
5. A name that resolves to nothing is a call to an import, a local variable, a global, or a
   parameter. M3 resolves the first; the rest are out of scope permanently.

Shadowing is not modelled. A local binding that shadows a module-level function of the same name
would make this resolution wrong, so the map is built from declarations at any depth in the file
and a name declared more than once falls into rule 4 — ambiguity, no edge. That is conservative
and it is checkable; a scope tree is not justified by the evidence yet.

## Graph changes

- A module node gains a `contains` edge to every function node the graph includes from it, at
  confidence 1.0. `contains` is what lets a reader see which module a function came from without
  reading the path on every node.
- Function nodes take their status from `FunctionChangeStatus`, so `added`, `removed`,
  `modified`, and `unchanged` mean what they mean everywhere else.
- `function_id` becomes a valid root, and the `invalid_params` rejection M1 emits for it is
  removed.
- A function root walks `calls` edges in both directions and `contains` upward to its module, so
  a function graph still shows which module the change sits in.
- Node ordering already sorts by path then range start, which orders functions within a file by
  position — the same order `list_changed_functions` returns them in.

The `multiple_levels` recommendation signal becomes available: with call edges, a changed
relationship can sit more than one hop from the root, which is a shape a dependency diff reads
poorly.

## Files touched

| File | Change |
| --- | --- |
| `src/languages/mod.rs` | `FunctionDefinition` carries call sites; the call-site type. |
| `src/languages/typescript.rs` | Call collection inside the existing collector walk. |
| `src/analysis.rs` | Call sites survive into `FunctionChange` for both revisions. |
| `src/result.rs` | Call sites reach the result model, if the graph reads them from there. |
| `src/graph/build.rs` | Function nodes, local call resolution, `contains` edges. |
| `src/graph/mod.rs` | `function` node kind; `calls` and `contains` relations. |
| `src/graph/recommend.rs` | `multiple_levels`. |
| `src/query/graph.rs` | `function_id` roots accepted. |
| `src/harness/mod.rs` | The `function_id` rejection is removed. |
| `benches/analysis.rs` | Call collection measured in the analysis group. |

A decision this stage must make explicitly: whether call sites travel through
`AnalysisResult` (core output schema version 1) or stay inside the analysis and are read by the
graph builder directly. Adding them to the result document is an additive schema change and
makes them visible to `--format json`; keeping them internal avoids growing a document
[`PERFORMANCE.md`](../PERFORMANCE.md) already measures at 1.1 MB for a 49-file diff. The default
is to keep them internal until a caller asks for them, following YAGNI, and the milestone
document is where that is recorded if it changes.

## Commits

```text
docs: define function call edges in the impact graph
feat(typescript): collect call sites inside each function
test(typescript): cover call collection and nesting boundaries
feat(graph): resolve calls within one file
test(graph): cover local resolution, ambiguity, and contains edges
feat(query): accept function roots for the impact graph
test(query): cover function roots
bench: measure call collection
docs: record the cost of call collection
```

## Tests

- A call to a function declared in the same file produces one `calls` edge at confidence 1.0
  with the call site as evidence.
- A call inside a nested arrow function is attributed to the arrow function, not its enclosing
  function — the mirror of `excludes_nested_functions_from_complexity_metrics`.
- A callee name declared twice in one file produces no edge, and the ambiguity is visible rather
  than silently resolved.
- A call to an imported name produces no edge in this stage.
- A method call, a computed call, and a call on a call produce no edges in this stage.
- An extracted helper produces one added function node and one added `calls` edge, on the same
  fixture `extraction_is_distinguished_from_other_changes` already uses.
- A `function_id` root that the analysis does not contain is rejected with the closest known
  ids, as `get_function_change` already does.
- A recursive function produces a self-edge and the walk terminates.
- Call sites do not change any existing metric, identity, or golden output.

## Benchmarks

Call collection runs on every parsed function of every changed file, inside the traversal that
[`PERFORMANCE.md`](../PERFORMANCE.md) attributes 53% of analysis time to. It is measured on the
generated tiers and on `vue-span` and `ts-checker` — the latter because it parses one 2.9 MiB,
52,760-line file in full and is the corpus that exposes per-file parse cost.

Both wall time and peak RSS are recorded, and both commits are measured back to back in one
session, which is the only comparison that isolates a change.

## Exit condition

Intra-file call relationships are resolved exactly or not at all, they cost a measured and
accepted amount, `function_id` roots are answerable, and every existing metric, identity, and
golden output is unchanged.
