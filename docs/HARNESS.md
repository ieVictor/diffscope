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

## Success response

```json
{"protocol_version":1,"id":"request-1","result":{"schema_version":1}}
```

`result` is the same complete schema-versioned object emitted by `diffscope --format json BASE TARGET`. The abbreviated object above is illustrative; the actual result includes every field defined in [`DEFINITIONS.md`](DEFINITIONS.md).

## Error response

```json
{"protocol_version":1,"id":"request-1","error":{"code":"analysis_failed","message":"..."}}
```

Stable protocol error codes are:

- `malformed_request`: invalid JSON or a request that does not match the request shape.
- `unsupported_protocol_version`: the requested protocol version is not `1`.
- `analysis_failed`: repository, revision, Git, or analyzer failure.

The `id` field is omitted only when it cannot be recovered from a malformed request. Adapter transport failures terminate the process with exit status `1`; request-level errors do not.

## Rust API

Harness integrations that embed the crate use `HarnessRequest` and `HarnessResponse` through `diffscope::execute_harness_request`. These transport-neutral types call the same `analyze` application entry point as the CLI. Adapters are responsible only for translating their transport to and from those types.
