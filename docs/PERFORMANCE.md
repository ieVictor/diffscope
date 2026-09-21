# Performance baselines

DiffScope performance is measured against deterministic generated Git repositories. Generated repositories and Criterion output live under `target/` and are not committed.

## Corpus

Run `just bench-corpus` to recreate all tiers at `target/benchmark-corpus`.

| Tier | Changed files | TypeScript | Unsupported Markdown | Binary | Reported functions | Analyzed bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| small | 10 | 7 | 2 | 1 | 14 | 2,455 |
| medium | 80 | 60 | 15 | 5 | 120 | 21,204 |
| large | 240 | 180 | 45 | 15 | 360 | 64,341 |

`Analyzed bytes` is the sum of available base and target text blobs loaded by the Git inventory. `Reported functions` is the number of before/after function-change records in the final result. The tiers satisfy the definitions in [`DEFINITIONS.md`](DEFINITIONS.md): fewer than 20, 20–200, and more than 200 changed files.

The generator fixes file contents, commit metadata, and revisions. It requires Git and a POSIX shell.

## Reproducing measurements

```sh
just bench         # Criterion wall time and analyzed-byte throughput for all tiers
just bench-cli     # complete large-tier process, Git access, analysis, and JSON rendering
just bench-memory  # sampled peak RSS of the large-tier CLI process
```

Criterion uses 1 second of warm-up, at least 10 samples, and a 3 second requested measurement window. Large runs may extend that window to collect 10 complete samples. `bench-memory` samples Linux `/proc/<pid>/status` every 10 ms and reports the DiffScope process's high-water RSS; it does not aggregate transient Git child-process memory.

## Baseline

Measured on 2026-09-20 with:

- AMD Ryzen 5 3600, 6 cores / 12 threads
- 15.5 GiB RAM
- Rust 1.92.0
- Git 2.55.0
- Linux, warm filesystem cache

| Tier | Median estimate | Throughput |
| --- | ---: | ---: |
| small | 39.91 ms | 60.07 KiB/s |
| medium | 273.23 ms | 75.79 KiB/s |
| large | 778.72 ms | 80.69 KiB/s |

The complete large-corpus CLI measured `744.5 ms ± 4.3 ms` over 10 Hyperfine runs. Sampled peak RSS for the DiffScope process was 5,076 KiB. These numbers are local reference values, not cross-machine performance guarantees.

## Measured optimization

The initial benchmark showed near-linear process overhead because each text blob was loaded with a separate `git cat-file -p` process. The Git adapter now sends all unique affected blob IDs through one `git cat-file --batch` process per analysis. Missing objects remain explicit `Missing` blob outcomes, and binary blobs remain unloaded.

Criterion measured the following wall-time changes with unchanged result and golden tests:

| Tier | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| small | 61.87 ms | 39.91 ms | 35.8% |
| medium | 467.58 ms | 273.23 ms | 41.6% |
| large | 1,396.5 ms | 778.72 ms | 44.2% |

Further optimization requires a new measurement. In particular, per-file diff and binary detection still invoke Git separately and are candidates only after profiling against this corpus.
