# Harness protocol

DiffScope's first harness adapter is a long-lived JSONL process. Start it with:

```sh
diffscope --jsonl
```

The process reads one request per line from standard input and writes and flushes exactly one response per non-empty line to standard output. It continues after request-level errors. Protocol messages use `protocol_version: 2`.

## Request

```json
{"protocol_version":2,"id":"request-1","repository":"/path/to/repo","base":"main","target":"HEAD"}
```

- `id` is a caller-provided string copied to the response.
- `repository` is a path accepted by Git; it may point inside the work tree.
- `base` and `target` are explicit committed Git revisions.
- Unknown or missing fields produce a `malformed_request` response.
- `method` and `params` are optional. A request without a `method` is an `analyze` request and returns the complete analysis document.

## Methods

```json
{"protocol_version":2,"id":"request-2","repository":"/path/to/repo","base":"main","target":"HEAD","method":"list_changed_functions","params":{"classification":"source","minimum_risk":"high","limit":10}}
```

| `method` | Answers |
| --- | --- |
| absent, or `analyze` | The complete analysis document (core output schema version 1). |
| `get_change_summary` | Counts, per-area totals, and a ranked shortlist. |
| `list_changed_files` | Ranked changed files, cursor-paginated. |
| `list_changed_functions` | Ranked changed functions, cursor-paginated. |
| `get_function_change` | One function by `function_id`, with the hunks that touch it, its reach, and its diagnostics. |
| `get_analysis_diagnostics` | Diagnostics, optionally for one `file`. |

Every method but `get_analysis_diagnostics` and `analyze` consults an index of the target revision's module graph: the summary uses it to rank candidates by review priority, and the list and detail methods additionally report what each changed file reaches — how many modules import it, and the tests likely to cover it. The graph is built on first use and reused, so those methods cost more on a comparison's first query. [`DEFINITIONS.md`](DEFINITIONS.md) defines how the graph is built and what "related" means.

`params` accepts `function_id`, `file`, `status`, `classification`, `minimum_risk`, `min_complexity_delta`, `include_unchanged`, `limit`, and `cursor`. Parameters that do not apply to the method are unused; an unknown parameter is rejected rather than ignored. `limit` defaults to 50 and is clamped to 1–200. Lists paginate by `cursor`; there is no offset parameter. [`DEFINITIONS.md`](DEFINITIONS.md) defines what each query returns and how results are ranked.

A whole analysis of a 49-file diff exceeds 1 MB, most of it functions the change did not touch. The same comparison's summary is a small fraction of that. Prefer a query, then narrow, rather than retrieving everything.

## Success response

Every successful response has the same envelope. Transport fields stay at the top; the result carries the projection inputs, the query that was applied, the answer, and — for list methods — a page:

```json
{"protocol_version":2,"id":"request-1","result":{
  "analysis":{"id":"6f3c1a...","schema_version":2,"tool_version":"0.2.0",
              "base":{"id":"eeff32e...","display_name":"main"},
              "target":{"id":"4ab865a...","display_name":"HEAD"}},
  "query":{"file":null,"status":null,"classification":"source","minimum_risk":"high",
           "min_complexity_delta":null,"include_unchanged":false,"limit":10},
  "data":{"functions":[...]},
  "page":{"returned":10,"total":37,"has_more":true,"next_cursor":"..."}}}
```

- `analysis` identifies the projection inputs: an opaque analysis id, the query envelope schema version (`2`), the tool version, and the resolved base and target revisions.
- `query` echoes the canonical applied parameters and their defaults, so a response can be interpreted without the request that produced it. Every parameter the method understands is present; a parameter that was not applied is `null`. The cursor itself is a paging token and is never echoed. The echoed sets are:
  - `analyze`, `get_change_summary`: `{}`.
  - `list_changed_files`: `classification`, `minimum_risk`, `limit`.
  - `list_changed_functions`: `file`, `status`, `classification`, `minimum_risk`, `min_complexity_delta`, `include_unchanged`, `limit`.
  - `get_function_change`: `function_id`.
  - `get_analysis_diagnostics`: `file`.
- `data` is the method's answer, shaped as described below.
- `page` is present for `list_changed_files` and `list_changed_functions` and absent otherwise.

### Analysis identity

`analysis.id` is an opaque, deterministic identifier: a digest of the resolved base commit, the resolved target commit, the tool version, and the query envelope schema version. The same inputs produce the same id in every process; any change to those inputs produces a different id. Revision names are not inputs, so `HEAD` and a branch that resolve to the same commit share one id.

### Pagination

`page` is `{"returned","total","has_more","next_cursor"}`. `next_cursor` is always present; it is `null` on the last page and a cursor string otherwise.

A cursor is opaque. It binds the schema version, the analysis id, the method, the normalized applied query, and the continuation position in the ranked list. It is accepted only by the two list methods; a cursor presented to any other method, or with a different analysis, method, schema version, or query, is rejected with `invalid_params` instead of silently answering from the wrong list. The cursor alone is enough to continue a list; a parameter that contradicts the query the cursor bound is rejected, and one that agrees with it is harmless. Because an analysis is immutable, a continuation offset recorded in a cursor remains valid for the life of that analysis.

### Method data

- `analyze` returns the complete analysis document: the same object as `diffscope --format json BASE TARGET`, at core output schema version 1 (versioned independently of this query API).
- `get_change_summary` returns the summary body: resolved revisions, file and line counts, function counts, diagnostic counts, change areas, and the ranked review-candidate shortlist.
- `list_changed_files` returns `{"files":[...]}`: ranked file records with classification, area, line and function counts, aggregate complexity, risk, review priority, change shape, export changes, reach, and diagnostics.
- `list_changed_functions` returns `{"functions":[...]}`: ranked function records, each addressed by `function_id` and carrying its human-readable `symbol` and `qualified_name`, metrics, churn, risk and review priority, match confidence, and range.
- `get_function_change` returns `{"function":{...},"hunks":[...],"impact":{...},"diagnostics":[...]}` for the function named by `function_id`.
- `get_analysis_diagnostics` returns `{"diagnostics":[...],"counts":{"info":0,"warnings":2,"errors":0,"total":2}}`.

[`DEFINITIONS.md`](DEFINITIONS.md) defines every field, the scoring models, and the meaning of `change_shape`.

## Error response

```json
{"protocol_version":2,"id":"request-1","error":{"code":"analysis_failed","message":"..."}}
```

Stable protocol error codes are:

- `malformed_request`: invalid JSON or a request that does not match the request shape.
- `unsupported_protocol_version`: the requested protocol version is not `2`.
- `analysis_failed`: repository, revision, Git, or analyzer failure.
- `unknown_method`: the requested `method` is not one of those listed above. The message names the accepted methods.
- `invalid_params`: a parameter is missing, is not one of its accepted values, or is a cursor that does not match this request's analysis, method, schema version, or query. The message names the field and what it accepts.
- `unknown_function`: `get_function_change` named a `function_id` the analysis does not contain. The message names the closest known function ids, so a caller that guessed can correct itself in one step.

The `id` field is omitted only when it cannot be recovered from a malformed request. Adapter transport failures terminate the process with exit status `1`; request-level errors do not.

## Reusing an analysis

The adapter is long-lived and keeps a small number of recent analyses, so the usual pattern of an overview, then a filtered page, then one function costs one analysis rather than several.

Entries are keyed by the commits the two revisions resolve to, never by the revision names. `HEAD` and a branch name point at different commits over time, so caching against a name would serve a stale analysis after the branch moved; a commit is immutable. Resolving the two names costs one `rev-parse` each, against an analysis that costs orders of magnitude more.

The import graph is cached separately and keyed by the target commit alone, because it describes one revision rather than a comparison: every comparison ending at the same commit shares one graph, however many bases they start from.

Reuse is not observable in results: a reused analysis answers identically to a fresh one, and it produces the same analysis id. Measured on the corpus in [`PERFORMANCE.md`](PERFORMANCE.md) with the version-1 protocol, a comparison's first query costs 114 ms without the import graph and 540 ms with it; later queries of the same comparison cost 2.5 ms and 9.3 ms.

## Rust API

Harness integrations that embed the crate use `HarnessRequest` and `HarnessResponse` through `diffscope::execute_harness_request`. These transport-neutral types call the same `analyze` application entry point as the CLI. Adapters are responsible only for translating their transport to and from those types.
