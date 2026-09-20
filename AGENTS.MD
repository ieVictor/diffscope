# Development Rules

## Guiding rule

Correct, deterministic, safe, and simple—then measurably fast.

## Correctness and safety

- Use safe Rust. `unsafe` is forbidden by default. Any future exception must be isolated, document its invariants, be thoroughly tested, and have a measured performance justification.
- Do not panic on user-controlled input. Avoid `unwrap`, unchecked indexing, and assumptions about source code, paths, or Git data in production paths.
- Return typed errors with actionable context.
- Make invalid states unrepresentable with domain types and enums.
- Treat all analyzed repositories as untrusted. Handle malformed syntax, invalid UTF-8, binary and oversized files, and parser failures gracefully.
- Keep output deterministic. Identical inputs must produce identically ordered results.
- Document and version metric definitions and machine-readable schemas.

## Design

- Keep the analysis core independent from the CLI and future harness adapters.
- Keep Git, filesystem, terminal, and serialization concerns at system boundaries.
- Prefer small, focused modules, composition, and explicit data flow.
- Follow KISS and YAGNI. Add abstractions only for demonstrated needs.
- Keep dependencies minimal, maintained, and narrowly configured.
- Preserve compatibility within a machine-readable schema version. Breaking output changes require a schema-version change.

## Testing and performance

- Correctness comes before optimization; optimized code must pass the same behavior tests.
- Measure before optimizing. Performance claims require reproducible benchmarks.
- Profile first and optimize measured bottlenecks rather than assumptions.
- Add unit tests for metric rules, property tests for invariants, fixture tests for output, and integration tests for Git workflows.
- Add or update benchmarks when changing performance-sensitive paths.
- Never weaken correctness, safety, or diagnostics solely to improve a benchmark.

## Local quality gate

Before considering a change complete, run:

```sh
just check
```

Use `just coverage`, `just bench`, and `just bench-cli` when the corresponding behavior changes. No CI policy is required while the project remains local-only.
