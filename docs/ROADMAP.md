# Implementation Roadmap

## Delivery policy

DiffScope is built through forward-only, production-quality increments. An iteration may leave capabilities unsupported, but it must not add disposable implementations or behavior that a later phase is expected to replace.

- Do not merge placeholders, fake results, temporary adapters, or production `todo!` and `unimplemented!` paths.
- Introduce an abstraction only with its first real implementation.
- Represent incomplete capabilities explicitly as unsupported; never approximate silently.
- Define contracts before exposing them and evolve them additively within a schema version.
- Preserve completed behavior with tests before extending it.
- Fix defects with a regression test that fails without the fix.
- Optimize only against a benchmark while preserving observable behavior.
- Keep every merged revision formatted, lint-clean, tested, and documented.

Forward-only does not prohibit internal refactoring. Refactoring must preserve tested contracts and must not require consumers to change established behavior.

## Milestone 1: Stable definitions

Definitions are captured in [DEFINITIONS.md](DEFINITIONS.md). Define the behavior that later milestones must preserve:

- Revision and diff semantics, including additions, deletions, renames, and binary files
- Physical and source LOC definitions
- Cyclomatic and cognitive complexity rules
- Function identity and before-and-after matching rules
- Diagnostics for unsupported or malformed input
- Deterministic ordering rules
- Version 1 result schema
- Representative correctness fixtures and benchmark corpus

**Exit condition:** Every metric and public result field has an unambiguous written definition.

## Milestone 2: Core model and Git change inventory

Create the reusable library boundary and implement real Git-backed discovery of:

- Repository and revision resolution
- Changed files and statuses
- Renames and diff hunks
- Before-and-after blob loading
- Aggregate added and removed lines

The working tree must never be modified. Binary files and unavailable blobs must produce explicit outcomes.

**Exit condition:** Fixture repositories prove deterministic file-level results for the documented Git cases.

## Milestone 3: Language analysis foundation

Implement the language-analyzer contract together with the first supported language:

- Language detection
- Tree-sitter parsing
- Function and method discovery
- Stable source ranges and qualified identities
- Explicit unsupported-language results

The analyzer contract must be based on the needs of the real first implementation, not hypothetical future languages.

**Exit condition:** Valid, incomplete, and malformed fixture files produce tested function inventories without panics.

## Milestone 4: Changed-function mapping

Map Git hunks to functions in both revisions and classify functions as added, removed, modified, or unchanged. Match functions using documented identity rules and report ambiguity instead of guessing.

**Exit condition:** Tests cover edits to signatures and bodies, moved functions, adjacent hunks, nested functions, and file renames.

## Milestone 5: Function metrics

Implement the documented metrics for the first language:

- LOC
- Cyclomatic complexity
- Cognitive complexity

Calculate before-and-after values only from parsed source and preserve the reason when a value cannot be calculated.

**Exit condition:** Rule-focused tests and reviewed fixtures cover every documented metric construct.

## Milestone 6: Public outputs and CLI

Expose the complete analysis through:

- A stable Rust library API
- Concise human-readable output
- Versioned JSON output
- CLI arguments, diagnostics, and meaningful exit codes

Output renderers consume the result model and contain no analysis logic.

**Exit condition:** End-to-end tests cover successful, partially supported, and failed analyses; golden tests protect human and JSON output.

## Milestone 7: Harness integration

Add a transport-neutral request model and the first real harness adapter. Prefer a long-lived JSON/JSONL standard-input and standard-output protocol so harnesses can reuse the process without embedding Rust.

Additional adapters must translate the same request and result models rather than duplicate analysis behavior.

**Exit condition:** Contract tests prove that CLI and harness execution return equivalent analysis results.

## Milestone 8: Measured performance

Establish baselines for small, medium, and large fixture repositories, then improve only measured bottlenecks. Candidate optimizations include:

- Parallel file analysis
- Blob-identity analysis caching
- Reduced allocation and copying
- Reusing parsers in long-lived processes

Each optimization requires a benchmark result and must pass all correctness and output tests unchanged.

**Exit condition:** Documented latency, throughput, and memory baselines are reproducible locally, with no correctness regressions.

## Milestone 9: Language expansion

Add languages one at a time through the established analyzer contract. Each language must ship function discovery, all documented metrics, malformed-input behavior, fixtures, and benchmarks together.

**Exit condition per language:** It meets the same correctness, reliability, and performance evidence as the first language before being declared supported.

## Change acceptance checklist

A change is complete only when:

1. It implements real behavior needed by the current milestone.
2. Unsupported behavior remains explicit and safe.
3. Existing contract, fixture, and golden tests remain unchanged unless correcting a documented defect.
4. New behavior has tests at the appropriate level.
5. Performance-sensitive behavior has a representative benchmark.
6. `just check` passes.
7. Relevant documentation reflects the delivered behavior.
