# Harness protocol

DiffScope's first harness adapter is a long-lived JSONL process. Start it with:

```sh
diffscope --jsonl
```

The process reads one request per line from standard input and writes and flushes exactly one response per non-empty line to standard output. It continues after request-level errors. Protocol messages use `protocol_version: 1`.

## Request

```json
{"protocol_version":1,"id":"request-1","repository":"/path/to/repo","base":"main","target":"HEAD"}
```

- `id` is a caller-provided string copied to the response.
- `repository` is a path accepted by Git; it may point inside the work tree.
- `base` and `target` are explicit committed Git revisions.
- Unknown or missing fields produce a `malformed_request` response.
- `method` and `params` are optional. **A request without a `method` returns the complete analysis**, which is what this protocol has always returned, so existing clients keep working unchanged.

## Methods

```json
{"protocol_version":1,"id":"request-2","repository":"/path/to/repo","base":"main","target":"HEAD","method":"list_changed_functions","params":{"classification":"source","minimum_risk":"high","limit":10}}
```

| `method` | Answers |
| --- | --- |
| absent, or `analyze` | The complete analysis. |
| `get_change_summary` | Counts, per-area totals, and a ranked shortlist. |
| `list_changed_files` | Ranked changed files. |
| `list_changed_functions` | Ranked changed functions. |
| `get_function_change` | One function, with its hunks and diagnostics. Requires `file` and `symbol`. |
| `get_analysis_diagnostics` | Diagnostics, optionally for one `file`. |

Every method but `get_analysis_diagnostics` and `analyze` also reports what each changed file reaches: how many modules import it, and the tests likely to cover it. That comes from an index of the target revision's module graph, which is built on first use and reused, so those methods cost more on a comparison's first query. [`DEFINITIONS.md`](DEFINITIONS.md) defines how the graph is built and what "related" means.

`params` accepts `file`, `symbol`, `status`, `classification`, `minimum_risk`, `min_complexity_delta`, `include_unchanged`, `limit`, and `offset`. Parameters that do not apply to the method are unused; an unknown parameter is rejected rather than ignored. [`DEFINITIONS.md`](DEFINITIONS.md) defines what each query returns and how results are ranked.

A whole analysis of a 49-file diff exceeds 1 MB, most of it functions the change did not touch. The same comparison answers `get_change_summary` in under 10 KB. Prefer a query, then narrow, rather than retrieving everything.

## Reusing an analysis

The adapter is long-lived and keeps a small number of recent analyses, so the usual pattern of an overview, then a filtered page, then one function costs one analysis rather than several.

Entries are keyed by the commits the two revisions resolve to, never by the revision names. `HEAD` and a branch name point at different commits over time, so caching against a name would serve a stale analysis after the branch moved; a commit is immutable. Resolving the two names costs one `rev-parse` each, against an analysis that costs orders of magnitude more.

The import graph is cached separately and keyed by the target commit alone, because it describes one revision rather than a comparison: every comparison ending at the same commit shares one graph, however many bases they start from.

Reuse is not observable in results: a reused analysis answers identically to a fresh one. Measured on the corpus in [`PERFORMANCE.md`](PERFORMANCE.md), a comparison's first query costs 114 ms without the import graph and 540 ms with it; later queries of the same comparison cost 2.5 ms and 9.3 ms.

## Success response

```json
{"protocol_version":1,"id":"request-1","result":{"schema_version":1}}
```

For a request without a `method`, `result` is the same complete schema-versioned object emitted by `diffscope --format json BASE TARGET`. The abbreviated object above is illustrative; the actual result includes every field defined in [`DEFINITIONS.md`](DEFINITIONS.md). For every other method, `result` is that method's answer.

## Error response

```json
{"protocol_version":1,"id":"request-1","error":{"code":"analysis_failed","message":"..."}}
```

Stable protocol error codes are:

- `malformed_request`: invalid JSON or a request that does not match the request shape.
- `unsupported_protocol_version`: the requested protocol version is not `1`.
- `analysis_failed`: repository, revision, Git, or analyzer failure.
- `unknown_method`: the requested `method` is not one of those listed above. The message names the accepted methods.
- `invalid_params`: a parameter is missing or is not one of its accepted values. The message names the field and what it accepts.
- `unknown_function`: `get_function_change` named a function the analysis does not contain. The message names the symbols that file does contain, so a caller that guessed can correct itself in one step.

The `id` field is omitted only when it cannot be recovered from a malformed request. Adapter transport failures terminate the process with exit status `1`; request-level errors do not.

## Rust API

Harness integrations that embed the crate use `HarnessRequest` and `HarnessResponse` through `diffscope::execute_harness_request`. These transport-neutral types call the same `analyze` application entry point as the CLI. Adapters are responsible only for translating their transport to and from those types.
