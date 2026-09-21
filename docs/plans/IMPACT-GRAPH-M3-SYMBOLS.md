# M3 — Cross-file named symbols

Stage three of [Milestone 10](IMPACT-GRAPH.md#milestones). It depends on
[M2](IMPACT-GRAPH-M2-CALLS.md), which delivers call sites and local resolution.

## What becomes answerable

> What calls this changed function from other files, and what does it call in theirs?

This is the question the whole feature exists for. A changed function's own file rarely tells a
reviewer whether the change is safe; the callers do. It is also the expensive question, because
callers live in files the diff does not contain.

## The cost constraint, and the bound that makes it affordable

Reading every file of a revision whole is not affordable. Extracting imports from Vue's 491
TypeScript files costs 381.5 ms when only each file's import region is parsed and 676 ms when the
files are parsed whole ([`PERFORMANCE.md`](../PERFORMANCE.md)) — and that 676 ms is parsing
alone, before the function collector that accounts for roughly half of analysis time. A
whole-revision call graph is out of the question at this project's performance standard.

It is also unnecessary. **A file can only call into a changed module if it imports that module,
and the import index already names those files.** Full parsing is therefore restricted to the
direct importers of changed files.

On the `vue-week` corpus — 49 changed files — that set is small: the risk model already reports
a changed module's direct importers, and the documented thresholds treat 20 direct importers as
the high band. The bound holds because it is a property of how code is written, not of how the
tool is configured.

Two consequences are part of the contract:

- **Only direct importers are parsed.** Indirect importers are not, for the same reason
  `related_tests` offers only direct importers: through a package's barrel module almost every
  file reaches almost every other, and on a real Vue revision one shared utility is reached by
  166 modules within two hops. A caller list built that way is not a caller list.
- **Reach is still reported as a number.** `nearby_importers` already says a file is widely
  re-exported without pretending to enumerate what that means.

## Scope

Added in this stage:

| Aspect | Added |
| --- | --- |
| Relations | `re_exports` |
| Resolutions | `imported_symbol` (1.0), `export_clause` (1.0), `re_exported_symbol` (0.9) |
| Node kinds | none |
| Roots | none |

## What must be recorded that is not today

### Import bindings

`ImportScan` (`src/languages/mod.rs:99`) carries `specifiers: BTreeSet<String>` — the module
paths, and nothing about the names imported from them. `import { a } from "x"` and
`import "x"` are indistinguishable, which is exactly the information a call resolver needs.

It gains the bindings behind each specifier: the local name a file uses, the exported name it
came from, and the specifier it came through. That covers named imports, renamed imports
(`import { a as b }`), default imports, and namespace imports, each distinguishable, because
resolving `b()` requires knowing it means `a` from `"x"`.

The scan window does not change. An import statement's bindings sit in the same statement as its
specifier, inside the region already read, so this records more from bytes already parsed rather
than parsing more bytes. That is measured rather than asserted.

### Exported symbols

`SourceAnalysis.exports` (`src/languages/mod.rs:54`) is a flat `BTreeSet<String>` of names, with
no kind, no position, and no link to a definition. It is sufficient for the export-surface delta
it was built for and insufficient for resolution: knowing `"stripComments"` is exported does not
say which function it is.

A parallel record gains, per exported name, the local name behind it and the definition's range
when the export names a function. `exports` itself does not change shape, so the export-surface
delta and its goldens are untouched.

### A symbol index

A per-commit index mapping `(path, exported_name)` to a definition, built for the files the
resolution bound admits and cached beside the import index and keyed the same way — by the
commit it describes, because it describes one revision rather than a comparison.

## Resolving a cross-file call

For a call site whose callee did not resolve locally in [M2](IMPACT-GRAPH-M2-CALLS.md):

1. Look the callee name up in the containing file's import bindings. No binding, no edge.
2. Resolve the binding's specifier to a path, using `resolve` (`src/imports.rs:210`) unchanged —
   relative paths, candidate extensions, directory index entries, and root `tsconfig.json`
   `compilerOptions.paths` aliases.
3. Look the binding's exported name up in that path's exported symbols.
4. A hit that is a function definition is a `calls` edge at `imported_symbol`, confidence 1.0,
   with the call site as evidence.
5. A name re-exported from another module is followed, up to a bounded number of hops. A
   definition found through a re-export chain resolves at `re_exported_symbol`, confidence 0.9.
   The value is lower than a direct import for a stated reason: each hop is a separate resolution
   that could be wrong, and a `export *` forwards names from a file the scan may not have read.
6. A whole-module re-export recorded as `*` is not followed. `DEFINITIONS.md` already records
   `*` precisely because the names it forwards live in a file the analysis has not read;
   guessing which name came through it would invent an edge.
7. A namespace import (`import * as ns`) whose call is `ns.name()` resolves when `name` is in the
   module's exported symbols, at `imported_symbol`. This is a member expression, but the receiver
   is a namespace with known contents, so the resolution is exact.

The `re_exports` relation records the forwarding itself: an edge from the re-exporting module to
the module it forwards from, at `export_clause`. A re-export is a real structural relationship
and one that changes — a barrel module that stops forwarding a name breaks every importer of
that name, which is the case `export_removed` already scores 3 points for.

Unresolved calls still produce no edge. [M4](IMPACT-GRAPH-M4-READABILITY.md) adds heuristic
resolution with its own relation and confidence; this stage stays exact.

## Discovering callers

1. Take the changed files.
2. Ask the import index for their direct importers.
3. Parse those files fully, in both revisions, reusing the parallel file mapping
   `src/application.rs` already uses and the byte budget that bounds it.
4. Resolve their call sites by the rules above.
5. Keep the edges that point at a function in the graph.

Both revisions are parsed because a caller removed by the change exists only in the base, and a
caller graph that cannot show a removed caller is not a delta. The base import index M1 added is
what names that set.

## Files touched

| File | Change |
| --- | --- |
| `src/languages/mod.rs` | `ImportScan` carries bindings; exported symbols carry kind and range. |
| `src/languages/typescript.rs` | Collect bindings; collect exported symbols beside `collect_exports`. |
| `src/imports.rs` | Bindings survive into the index; the symbol index is built and cached. |
| `src/harness/mod.rs` | The symbol index joins the per-commit caches. |
| `src/graph/build.rs` | Cross-file resolution, caller discovery, `re_exports` edges. |
| `src/graph/mod.rs` | The `re_exports` relation and its resolutions. |
| `benches/analysis.rs` | Caller discovery measured against a real corpus. |

## Commits

```text
docs: define cross-file symbol resolution in the impact graph
feat(typescript): record import bindings and exported definitions
test(typescript): cover bindings, aliases, and export forms
feat(imports): index a revision's exported symbols
test(imports): cover symbol resolution and re-export chains
feat(graph): resolve calls across files
test(graph): cover imported, aliased, and re-exported callees
feat(graph): discover callers from direct importers
test(graph): cover caller discovery and its bound
bench: measure caller discovery
docs: record the cost of cross-file resolution
```

## Tests

- A named import called directly resolves at confidence 1.0.
- A renamed import (`import { a as b }`) called as `b()` resolves to `a`.
- A default import resolves to the default export.
- A namespace import called as `ns.name()` resolves; called as `ns[key]()` it does not.
- A re-export chain of two hops resolves at confidence 0.9.
- A name reached only through `export *` produces no edge.
- A specifier resolving to no file in the revision produces no edge and is counted as
  unresolved, not guessed at.
- A `tsconfig.json` alias is applied, so a cross-package call in a monorepo resolves. Without
  aliases every cross-package edge disappears — 18.4% of a Vue revision's specifiers are
  `@vue/*`.
- A caller present in the base and absent in the target produces a `removed` edge.
- Only direct importers are parsed: a fixture where an indirect importer contains a same-named
  call asserts that file was not read.
- A changed file with no importers produces a graph with no callers and does not parse anything
  beyond the diff.

## Benchmarks

Caller discovery is the first thing DiffScope does that parses files the diff does not contain
at full fidelity, so it is measured as its own phase, as the import graph is:

- the number of files the bound admits, per corpus;
- the cost of parsing them, against the cost of the analysis itself;
- the cost of building and caching the symbol index;
- peak RSS, because full parses of unchanged files are a new memory cost.

Measured on `vue-commit`, `vue-week`, `vue-minor`, and `vue-span`, so the relationship between
changed-file count and admitted-caller count is visible rather than assumed. If a corpus shows
the bound failing — a changed barrel module with hundreds of direct importers — that is recorded
and bounded explicitly, not left to be discovered in use.

## Exit condition

A changed function's callers and callees across files are resolved exactly or not at all; the
files parsed beyond the diff are bounded by the import graph and the bound is measured; and an
unresolved call is visibly unresolved rather than absent.
