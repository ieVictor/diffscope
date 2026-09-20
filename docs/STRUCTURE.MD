# Project Structure

DiffScope begins as one Rust package. It should become a workspace only when independently versioned harness adapters or language packages require it.

```text
diffScope/
├── src/
│   ├── main.rs             # Thin CLI entry point
│   ├── lib.rs              # Public analysis API
│   ├── application/        # Analysis workflow orchestration
│   ├── analysis/           # Diff mapping and function matching
│   ├── git/                # Git repository adapter
│   ├── languages/          # Language detection and analyzers
│   ├── metrics/            # LOC and complexity calculations
│   └── output/             # Human and versioned machine output
├── tests/
│   ├── fixtures/           # Small repositories and source samples
│   └── integration/        # End-to-end Git workflow tests
├── benches/                # Criterion benchmarks
├── docs/
│   ├── ARCHITECTURE.md     # System design and boundaries
│   ├── DEFINITIONS.md      # Versioned metric and result semantics
│   ├── ROADMAP.md          # Forward-only implementation plan
│   └── STRUCTURE.MD        # Directory ownership and conventions
├── scripts/                # Local setup and maintenance scripts
├── AGENTS.MD               # Development rules
├── CONTRIBUTING.md         # Local development workflow
├── Justfile                # Quality, coverage, and benchmark commands
└── Cargo.toml              # Package, lint, and benchmark configuration
```

## Placement rules

- Keep `main.rs` limited to argument handling, application invocation, and exit behavior.
- Expose reusable behavior through `lib.rs`; harnesses must not call CLI internals.
- Keep domain data and analysis rules free of filesystem, terminal, and serialization dependencies.
- Put language-specific syntax knowledge under `languages/` and shared metric concepts under `metrics/`.
- Keep Git access behind the `git/` boundary.
- Place unit tests beside their modules and cross-module behavior in `tests/`.
- Store only compact, intentional fixtures. Do not commit generated benchmark or coverage output.
- Add directories only when they contain a real responsibility; avoid empty architectural scaffolding.
