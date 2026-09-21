# Harness integrations

DiffScope answers coding-agent questions from one analysis core and one projection layer. Two transports are shipped, and both return the same answer objects for the same query:

- **MCP** — `diffscope mcp` serves the five queries as Model Context Protocol tools over standard input and output. This is what `diffscope setup` configures, and what a harness that speaks MCP should use.
- **JSONL** — `diffscope --jsonl` serves a versioned request/response protocol over standard input and output for adapters that manage the DiffScope process themselves. It additionally offers an `analyze` method that returns the complete analysis document.

[`DEFINITIONS.md`](DEFINITIONS.md) defines the query semantics both transports carry: filters, ranking, cursors, and the result fields.

## MCP server

Start the server with:

```sh
diffscope mcp
```

It speaks MCP over standard input and output: stdout carries one JSON-RPC message per line and nothing else, all diagnostics go to stderr, and the command takes no options. Closing standard input ends the session and exits `0`. `diffscope setup` writes a harness configuration that runs the absolute path of the installed binary with the single argument `mcp`, so the harness and the shell always use the same build. See [`../README.md`](../README.md) for installation, `diffscope setup`, `diffscope doctor`, and removal.

### Identity

The server advertises `serverInfo` name `diffscope`, the crate version of the binary as its version, the title `DiffScope`, and the `tools` capability, and marks every tool read-only, non-destructive, and idempotent. Its `instructions` describe the workflow rather than the implementation: every tool compares a committed base revision with a committed target revision and requires `repository`, `base`, and `target`; there is no default repository, so a call never analyzes a tree the caller did not name; start with `get_change_summary`, page through the change with `list_changed_files` or `list_changed_functions` (handing `page.next_cursor` back unchanged to continue), read one function in full with `get_function_change`, and check `get_analysis_diagnostics` when an analysis looks incomplete. Every tool reuses the analyses already made in the session.

### Tools

The five tools are exactly:

| Tool | Additional parameters | Answers |
| --- | --- | --- |
| `get_change_summary` | — | Counts, per-area totals, and a ranked shortlist of review candidates. |
| `list_changed_files` | `classification`, `minimum_risk`, `limit`, `cursor` | Ranked changed files, cursor-paginated. |
| `list_changed_functions` | `file`, `status`, `classification`, `minimum_risk`, `min_complexity_delta`, `include_unchanged`, `limit`, `cursor` | Ranked changed functions, cursor-paginated. |
| `get_function_change` | `function_id` | One function by `function_id`, with its hunks, its reach, and its diagnostics. |
| `get_analysis_diagnostics` | `file` | Diagnostics, optionally for one file. |

Every tool requires three inputs, and none of them has a default:

- `repository` — a path to the repository or to any path inside its work tree. Relative paths resolve against the working directory of the server process. Unlike the CLI's `--repository`, there is no `.` default, so a call can never silently analyze a tree the caller did not name.
- `base` and `target` — committed Git revisions, resolved by the repository's ref rules: a commit id, branch, tag, `HEAD`, or an expression such as `HEAD~1`. The working tree and the index are never inputs.

Filter values are the ones the query API defines: `classification` is one of `source`, `test`, `generated`, `vendored`, `lockfile`, `config`, `docs`; `minimum_risk` is one of `low`, `medium`, `high`; `status` is one of `added`, `removed`, `modified`, `unchanged`; `min_complexity_delta` is an integer compared against the larger of a function's cognitive and cyclomatic deltas; `include_unchanged` defaults to `false`; `limit` defaults to `50` and is clamped to 1–200; and `cursor` continues a list. Each tool's input schema is an explicit JSON Schema object with `additionalProperties: false`, so a misspelled parameter is rejected instead of ignored.

### Results

A successful call returns one text content block containing the answer as JSON, and `structuredContent` holding the same object. The object is byte-identical to the `result` object the JSONL protocol returns:

```json
{
  "analysis": {"id":"5a0f4e...","schema_version":2,"tool_version":"0.2.0",
               "base":{"id":"eeff32e...","display_name":"main"},
               "target":{"id":"4ab865a...","display_name":"HEAD"}},
  "query": {"file":null,"status":null,"classification":"source","minimum_risk":"high",
            "min_complexity_delta":null,"include_unchanged":false,"limit":10},
  "data": {"functions":[...]},
  "page": {"returned":10,"total":37,"has_more":true,"next_cursor":"..."}
}
```

`analysis` identifies the projection inputs — an opaque, deterministic analysis id, the query envelope schema version (`2`), the tool version, and the resolved base and target revisions. `query` echoes the canonical applied parameters and their defaults. `data` is the method's answer. `page` is present only for `list_changed_files` and `list_changed_functions`; `next_cursor` is `null` on the last page.

`analysis.id` is a digest of the resolved base commit, the resolved target commit, the tool version, and the query envelope schema version, so the same comparison produces the same id in every process while a different one never collides with it. Revision names are not inputs: `HEAD` and a branch that resolve to the same commit share one id.

The cursor is opaque and transport-independent. It binds the schema version, the analysis id, the tool, the normalized applied query, and the continuation position, so hand back `next_cursor` unchanged rather than constructing one. A cursor presented to the wrong tool, analysis, schema version, or query is rejected with `invalid_params` instead of answering from the wrong list, and a parameter that contradicts the query the cursor bound is rejected too. Because an analysis is immutable, a continuation stays valid for the life of that analysis.

### Errors

Errors after arguments parse are reported as tool results with `isError: true` and `structuredContent` of the form:

```json
{"error":{"code":"unknown_function","message":"no function `...` in this analysis; it contains ..."}}
```

The message text carries the same detail as the JSONL transport. Codes are `analysis_failed`, `invalid_params`, `unknown_function`, and `serialization_failed`. `unknown_function` names the closest known function ids, so a caller that guessed can correct itself in one step; a cursor that belongs to another analysis, or a parameter that contradicts the query it bound, is rejected as `invalid_params` rather than answered from the wrong list. A failing call leaves the server running; only that call is answered with an error.

Failures that happen before analysis — an unknown tool name, a missing `repository`, `base`, or `target`, an argument of the wrong type, or an unknown argument field — are JSON-RPC protocol errors instead, so no result object is produced; the message names what was wrong, and an unknown tool names the tools this server exposes.

### Relationship to the JSONL protocol

Both transports wrap one implementation. They share the analysis cache, the projection code, the query defaults, the ranking, the cursors, and the error codes, so a query answers identically whichever transport carries it.

They differ in framing and in surface:

- MCP relies on JSON-RPC for framing and request identity; the JSONL protocol carries its own `protocol_version` and `id` fields.
- MCP exposes the five query tools and no `analyze` method; the JSONL protocol also answers `analyze` with the complete analysis document (core output schema version 1).
- Neither transport re-analyzes a comparison it already holds: the process keeps a small number of recent analyses keyed by the commits the revisions resolve to, so a summary followed by a list followed by a detail costs one analysis.

## Agent workflow

A realistic question — "what on this branch should I review first, and why?" — becomes three calls. Every call names the same comparison, so all three answers come from one analysis.

**1. Summarize.** `get_change_summary` with `{"repository": ".", "base": "main", "target": "HEAD"}`. Measured on the Vue corpus in [`PERFORMANCE.md`](PERFORMANCE.md), the comparison is 49 changed files and 1,390 added against 548 removed lines — more than 1 MB of JSON as a complete analysis. The summary reports the totals, the per-classification and per-area breakdown, and a ranked shortlist of review candidates, here abbreviated to its first two entries:

```json
{"files":{"changed":49,"supported":22,"unsupported":27,
          "by_classification":{"source":10,"test":12,"config":24,"docs":1,"generated":1,"lockfile":1}},
 "lines":{"added":1390,"removed":548},
 "functions":{"added":72,"modified":46,"removed":5,"unchanged":1521},
 "review_candidates":[
   {"function_id":"packages/runtime-core/src/renderer.ts#arrow:patch@target:379:25",
    "symbol":"arrow:patch","status":"modified","risk":"high","review_priority":"high",
    "complexity_delta":{"cyclomatic":5,"cognitive":6}},
   {"function_id":"packages/compiler-sfc/src/style/cssVars.ts#fn:stripComments@target:117:0",
    "symbol":"fn:stripComments","status":"added","risk":"high","review_priority":"high",
    "complexity_delta":{"cyclomatic":13,"cognitive":34}}]}
```

**2. Narrow.** `list_changed_functions` with `{"classification": "source", "minimum_risk": "high", "limit": 10}`. The source filter removes the 24 config files, the 12 tests, and the rest without a second analysis, and the risk filter removes low-scoring rows. Each returned record carries the `function_id` that addresses it, its metrics, churn, risk and review priority with the reasons behind each score, and its match confidence:

```json
{"function_id":"packages/runtime-core/src/renderer.ts#arrow:patch@target:379:25",
 "symbol":"arrow:patch","qualified_name":"patch","kind":"arrow_function",
 "status":"modified","classification":"source","match_confidence":1.0,
 "metrics":{"cyclomatic_complexity":{"before":26,"after":31,"delta":5},
            "cognitive_complexity":{"before":39,"after":45,"delta":6},
            "physical_loc":{"before":118,"after":131,"delta":13},
            "source_loc":{"before":112,"after":123,"delta":11}},
 "change":{"changed_hunks":1,"lines_added":13,"lines_removed":0,"hunk_overlap":0.1},
 "range":{"before":{"start_line":379,"end_line":496},"after":{"start_line":379,"end_line":509}},
 "risk":{"model":"diffscope-risk-v1","score":6,"maximum_score":8,"level":"high",
         "reasons":[{"code":"cognitive_complexity_increased","message":"cognitive complexity increased by 6","value":6},
                    {"code":"cyclomatic_complexity_increased","message":"cyclomatic complexity increased by 5","value":5},
                    {"code":"cognitive_complexity_after","message":"cognitive complexity is 45 after the change","value":45}]},
 "review_priority":{"model":"diffscope-review-priority-v1","score":8,"maximum_score":14,"level":"high",
                    "reasons":[{"code":"cognitive_complexity_increased","message":"cognitive complexity increased by 6","value":6},
                               {"code":"cyclomatic_complexity_increased","message":"cyclomatic complexity increased by 5","value":5},
                               {"code":"cognitive_complexity_after","message":"cognitive complexity is 45 after the change","value":45},
                               {"code":"containing_module_direct_importers","message":"containing module is imported directly by 12 modules","value":12},
                               {"code":"production_source","message":"production source file"}]}}
```

Two source functions score `high`, so the page is `{"returned":2,"total":2,"has_more":false,"next_cursor":null}` — the second row is `packages/compiler-sfc/src/style/cssVars.ts#fn:stripComments@target:117:0`. A larger list would set `has_more` and return a `next_cursor` string that the caller hands back unchanged; a cursor presented with a different query is rejected rather than answered from another list.

**3. Open one.** `get_function_change` with `{"function_id": "packages/compiler-sfc/src/style/cssVars.ts#fn:stripComments@target:117:0"}`. The answer is the full function record plus the hunks that touch it, its reach, and its diagnostics:

```json
{"function":{"function_id":"packages/compiler-sfc/src/style/cssVars.ts#fn:stripComments@target:117:0",
             "symbol":"fn:stripComments","qualified_name":"stripComments","kind":"function",
             "status":"added","classification":"source","match_confidence":1.0,
             "metrics":{"cyclomatic_complexity":{"after":13,"delta":13},
                        "cognitive_complexity":{"after":34,"delta":34},
                        "physical_loc":{"after":36,"delta":36},
                        "source_loc":{"after":36,"delta":36}},
             "change":{"changed_hunks":1,"lines_added":36,"lines_removed":0,"hunk_overlap":1.0},
             "range":{"after":{"start_line":117,"end_line":152}},
             "risk":{"model":"diffscope-risk-v1","score":5,"maximum_score":8,"level":"high",
                     "reasons":[{"code":"added_function_cognitive_complexity","message":"new function has cognitive complexity 34","value":34},
                                {"code":"added_function_cyclomatic_complexity","message":"new function has cyclomatic complexity 13","value":13},
                                {"code":"high_churn","message":"36 lines changed inside the function","value":36}]},
             "review_priority":{"model":"diffscope-review-priority-v1","score":7,"maximum_score":14,"level":"high",
                                "reasons":[{"code":"added_function_cognitive_complexity","message":"new function has cognitive complexity 34","value":34},
                                           {"code":"added_function_cyclomatic_complexity","message":"new function has cyclomatic complexity 13","value":13},
                                           {"code":"high_churn","message":"36 lines changed inside the function","value":36},
                                           {"code":"containing_module_direct_importers","message":"containing module is imported directly by 5 modules","value":5},
                                           {"code":"production_source","message":"production source file"}]}},
 "hunks":[{"base_start":86,"base_count":0,"target_start":85,"target_count":139}],
 "impact":{"direct_importers":5,"nearby_importers":12,
           "related_tests":[{"file":"packages/compiler-sfc/__tests__/cssVars.spec.ts","reason":"name matches the changed file","confidence":0.8},
                            {"file":"packages/shared/__tests__/cssVars.spec.ts","reason":"name matches the changed file","confidence":0.8}]},
 "diagnostics":[]}
```

The observable outcome for the user is a ranking they can audit. `arrow:patch` is first because its cognitive complexity grew by 6 to 45, its cyclomatic complexity grew by 5 to 31, and 12 modules import its containing file directly. `fn:stripComments` is a newly added function with cognitive complexity 34 and 36 changed lines in one hunk; 5 modules import its containing file directly, and two test files share its file's name, which is why they are offered as related. Both rankings come with their reasons, so a reviewer who disagrees with the weighting can rank on the underlying numbers instead. If the caller had passed a `function_id` the analysis does not contain, the error would name the closest known ids and nothing else would change.

## JSONL protocol

The JSONL adapter is a long-lived process. Start it with:

```sh
diffscope --jsonl
```

The process reads one request per line from standard input and writes and flushes exactly one response per non-empty line to standard output. It continues after request-level errors. Protocol messages use `protocol_version: 2`.

### Request

```json
{"protocol_version":2,"id":"request-1","repository":"/path/to/repo","base":"main","target":"HEAD"}
```

- `id` is a caller-provided string copied to the response.
- `repository` is a path accepted by Git; it may point inside the work tree.
- `base` and `target` are explicit committed Git revisions.
- Unknown or missing fields produce a `malformed_request` response.
- `method` and `params` are optional. A request without a `method` is an `analyze` request and returns the complete analysis document.

### Methods

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

### Success response

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

#### Analysis identity

`analysis.id` is an opaque, deterministic identifier: a digest of the resolved base commit, the resolved target commit, the tool version, and the query envelope schema version. The same inputs produce the same id in every process; any change to those inputs produces a different id. Revision names are not inputs, so `HEAD` and a branch that resolve to the same commit share one id.

#### Pagination

`page` is `{"returned","total","has_more","next_cursor"}`. `next_cursor` is always present; it is `null` on the last page and a cursor string otherwise.

A cursor is opaque. It binds the schema version, the analysis id, the method, the normalized applied query, and the continuation position in the ranked list. It is accepted only by the two list methods; a cursor presented to any other method, or with a different analysis, method, schema version, or query, is rejected with `invalid_params` instead of silently answering from the wrong list. The cursor alone is enough to continue a list; a parameter that contradicts the query the cursor bound is rejected, and one that agrees with it is harmless. Because an analysis is immutable, a continuation offset recorded in a cursor remains valid for the life of that analysis.

#### Method data

- `analyze` returns the complete analysis document: the same object as `diffscope --format json BASE TARGET`, at core output schema version 1 (versioned independently of this query API).
- `get_change_summary` returns the summary body: resolved revisions, file and line counts, function counts, diagnostic counts, change areas, and the ranked review-candidate shortlist.
- `list_changed_files` returns `{"files":[...]}`: ranked file records with classification, area, line and function counts, aggregate complexity, risk, review priority, change shape, export changes, reach, and diagnostics.
- `list_changed_functions` returns `{"functions":[...]}`: ranked function records, each addressed by `function_id` and carrying its human-readable `symbol` and `qualified_name`, metrics, churn, risk and review priority, match confidence, and range.
- `get_function_change` returns `{"function":{...},"hunks":[...],"impact":{...},"diagnostics":[...]}` for the function named by `function_id`.
- `get_analysis_diagnostics` returns `{"diagnostics":[...],"counts":{"info":0,"warnings":2,"errors":0,"total":2}}`.

[`DEFINITIONS.md`](DEFINITIONS.md) defines every field, the scoring models, and the meaning of `change_shape`.

### Error response

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

### Reusing an analysis

The adapter is long-lived and keeps a small number of recent analyses, so the usual pattern of an overview, then a filtered page, then one function costs one analysis rather than several.

Entries are keyed by the commits the two revisions resolve to, never by the revision names. `HEAD` and a branch name point at different commits over time, so caching against a name would serve a stale analysis after the branch moved; a commit is immutable. Resolving the two names costs one `rev-parse` each, against an analysis that costs orders of magnitude more.

The import graph is cached separately and keyed by the target commit alone, because it describes one revision rather than a comparison: every comparison ending at the same commit shares one graph, however many bases they start from.

Reuse is not observable in results: a reused analysis answers identically to a fresh one, and it produces the same analysis id. Measured on the corpus in [`PERFORMANCE.md`](PERFORMANCE.md) with the version-1 protocol, a comparison's first query costs 114 ms without the import graph and 540 ms with it; later queries of the same comparison cost 2.5 ms and 9.3 ms.

### Rust API

Harness integrations that embed the crate use `HarnessRequest` and `HarnessResponse` through `diffscope::execute_harness_request`. These transport-neutral types call the same `analyze` application entry point as the CLI. Adapters are responsible only for translating their transport to and from those types.
