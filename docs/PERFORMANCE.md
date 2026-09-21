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

Measured on 2026-09-21 with:

- AMD Ryzen 5 3600, 6 cores / 12 threads
- 15.5 GiB RAM
- Rust 1.92.0
- Git 2.55.0
- Linux, warm filesystem cache

| Tier | Analysis | Throughput | Import graph |
| --- | ---: | ---: | ---: |
| small | 11.86 ms | 202.22 KiB/s | 3.24 ms |
| medium | 22.40 ms | 924.40 KiB/s | 6.97 ms |
| large | 20.95 ms | 2.93 MiB/s | 12.77 ms |

Projecting an analysis into one answer runs on every request, including those served from a cached analysis, so it is the floor on query latency. Over the large tier's 360 function records:

| Projection | Median estimate |
| --- | ---: |
| `get_change_summary` | 303.7 us |
| `list_changed_functions` | 193.5 us |

The complete large-corpus CLI measured `23.0 ms +/- 0.5 ms` over 10 Hyperfine runs. Sampled peak RSS for the DiffScope process was 4,320 KiB. These numbers are local reference values, not cross-machine performance guarantees.

### A superseded baseline

An earlier table recorded 39.91 ms, 273.23 ms, and 778.72 ms for the three tiers, and 744.5 ms for the complete CLI. Those numbers do not reproduce. Re-measuring the commit they described, on the same machine, Rust, Git, and corpus, gives the figures above: the corpus is byte-for-byte identical, reporting the same 240 changed files, 360 functions, and 64,341 analyzed bytes.

The cause was not determined and is not guessed at here. It is recorded because a baseline nobody can reproduce is worse than no baseline: Criterion compares each run against whatever it last stored, so a stale figure silently turns an unchanged measurement into a reported 97% improvement. Comparisons that need to isolate one change measure both commits in the same session, as the section below does.

## Effect of identity scoping and churn measurement

Container-scoped identities, per-function churn, body hashing for ambiguous matches, and export collection each add work to the analysis. Measured by benchmarking the commit before that work and the commit after it, back to back in the same session:

| Tier | Before | After |
| --- | ---: | ---: |
| small | 12.59 ms | 11.96 ms |
| medium | 22.83 ms | 22.85 ms |
| large | 22.17 ms | 21.25 ms |

The difference is within run-to-run noise on every tier: the added work is not measurable against the cost of parsing.

The generated tiers are too small to show the cost, so the same two commits were measured against real repositories, where they do:

| Corpus | Before | After | Before RSS | After RSS |
| --- | ---: | ---: | ---: | ---: |
| vue-commit | 54.5 ms | 61.6 ms | 13,136 KiB | 13,736 KiB |
| vue-minor | 407.6 ms | 450.1 ms | 47,688 KiB | 57,544 KiB |
| vue-span | 554.0 ms | 606.0 ms | 55,788 KiB | 65,276 KiB |

The work costs roughly 10% in wall time and up to 21% in peak memory on the largest corpus. Most of the memory is identity: a container-scoped name such as `describe("Suspense").test("pending branch").<anonymous>#1` is many times the size of `<anonymous>#26`, and a large revision holds thousands of them. The rest is the body hash, churn counts, and export sets held per function and per file.

That cost buys a correct answer. The short names were not merely terser; they made a function's identity depend on how many anonymous functions preceded it in its file, so inserting one callback renumbered every later one and matched unrelated bodies to each other. Correctness before speed is the project's first rule, and this is what it costs here.

Both commits were measured in the same session, which is the only comparison that isolates a change. Criterion's own reported difference is against whatever it last stored, which may be another session's numbers, so it is not used here.

## Query cost and analysis reuse

An agent asks several questions about one comparison. Each is a projection of the same analysis, so the adapter keeps a small number of recent analyses keyed by the commits the revisions resolve to.

Measured on the Vue comparison below, driving `diffscope --jsonl` with a batch of requests and taking the median of five runs:

| Requests, one comparison | Median | Marginal cost per extra query |
| --- | ---: | ---: |
| 1 query | 114.3 ms | — |
| 8 queries | 131.9 ms | 2.53 ms |

The first query pays for the analysis; each later query of the same comparison costs about 2.5 ms, roughly 45 times less. Reuse is bounded by entry count and by the function records retained across entries, and it is not observable in results: a test asserts that a reused analysis answers identically to a fresh one.

## Import graph cost

The graph is built over a whole revision, so it is measured separately. On the Vue target revision below (702 files, 491 TypeScript, 4.2 MiB):

| Phase | Time |
| --- | ---: |
| `ls-tree` | 2.7 ms |
| `cat-file --batch`, 488 blobs | 21.7 ms |
| parse and scan imports | 381.5 ms |
| resolve specifiers | ~30 ms |
| **whole index** | **412 ms** |

Parsing dominates, which is why only each file's import region is read. Parsing the files whole costs 676 ms for the same 1,920 specifiers, so the bounded prefix saves 44% and loses no edge. Anchoring more tightly, on a quoted `from '`, would parse 20% of the bytes instead of 56%, but loses one file's imports; the index is built once per revision and cached, so the cheaper anchor is not worth an edge that silently does not exist.

Queries that use the graph therefore cost more on a comparison's first request:

| Requests, one comparison | Median | Marginal per extra query |
| --- | ---: | ---: |
| 1 query, with import graph | 539.7 ms | — |
| 5 queries, with import graph | 576.7 ms | 9.27 ms |

Response size is the reason the queries exist. For the same 49-file Vue comparison:

| Response | Bytes |
| --- | ---: |
| Complete analysis | 1,099,911 |
| `get_change_summary` | 9,155 |
| `list_changed_functions`, production source at high risk, limit 10 | 8,729 |
| `get_function_change` for one function | 1,221 |

## Real-world corpus

Generated tiers isolate per-file behavior but do not resemble real diffs: the large tier averages 268 bytes per changed file, while real TypeScript diffs average roughly 16 KiB. Real-repository measurements therefore accompany the generated tiers. These corpora are not committed; recreate them by cloning the repositories and using the pinned revisions.

| Corpus | Repository | Base | Target | Changed files | Analyzed bytes |
| --- | --- | --- | --- | ---: | ---: |
| vue-commit | `vuejs/core` | `897924c4f` | `4ab865a84` | 2 | 238,181 |
| vue-week | `vuejs/core` | `eeff32e51` | `4ab865a84` | 49 | 2,068,989 |
| vue-patches | `vuejs/core` | `v3.4.0` | `v3.4.15` | 104 | 2,973,234 |
| vue-minor | `vuejs/core` | `v3.4.0` | `v3.5.0` | 482 | 8,029,520 |
| vue-span | `vuejs/core` | `v3.0.0` | `v3.5.0` | 737 | 8,647,506 |
| ts-checker | `microsoft/TypeScript` | `ff7169214` | `fefa70aa1` | 12 | 6,138,304 |

`ts-checker` is one production pull request that edits eight lines of `src/compiler/checker.ts`, a single 2.9 MiB, 52,760-line file. It measures per-file parse cost, because both revisions of that file are parsed in full.

Measured on the machine and toolchain described above, warm filesystem cache, Hyperfine with one warm-up and five runs:

| Corpus | Wall time | Peak RSS |
| --- | ---: | ---: |
| vue-commit | 61.6 ms +/- 0.8 ms | 13,736 KiB |
| vue-week | 114.9 ms +/- 3.2 ms | 31,184 KiB |
| vue-patches | 175.3 ms +/- 4.0 ms | 37,576 KiB |
| vue-minor | 450.1 ms +/- 1.3 ms | 57,544 KiB |
| vue-span | 606.0 ms +/- 7.5 ms | 65,276 KiB |
| ts-checker | not re-measured | — |

`ts-checker` requires a `microsoft/TypeScript` clone that was not available when this table was recorded. Its row is left empty rather than carried over from an earlier session, for the reason given under the superseded baseline.

Summary line totals match `git diff --numstat --find-renames` exactly for every corpus.

## Measured optimization

The initial benchmark showed near-linear process overhead because each text blob was loaded with a separate `git cat-file -p` process. The Git adapter now sends all unique affected blob IDs through one `git cat-file --batch` process per analysis. Missing objects remain explicit `Missing` blob outcomes, and binary blobs remain unloaded.

Criterion measured the following wall-time changes with unchanged result and golden tests:

| Tier | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| small | 61.87 ms | 39.91 ms | 35.8% |
| medium | 467.58 ms | 273.23 ms | 41.6% |
| large | 1,396.5 ms | 778.72 ms | 44.2% |

### Whole-range diff collection

Profiling against the real-world corpus confirmed the candidate named above. Hunk collection and binary detection each ran one Git process per changed file, so an analysis spawned `2N + 5` processes: 969 for `vue-minor`. Replaying those 964 per-file calls in isolation took 2.083 s, while the two whole-range calls that carry the same information took 0.115 s together. Per-file process overhead was therefore 57% of that corpus's total runtime.

The Git adapter now reads all hunks from one `git diff --unified=0 --find-renames` over the range and all binary paths from one `git diff --numstat -z`, indexing both by path. Process count is constant at 7 regardless of changed-file count.

| Corpus | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| vue-commit | 107.2 ms | 104.4 ms | 2.6% |
| vue-week | 556.8 ms | 391.7 ms | 29.7% |
| vue-patches | 1.048 s | 679.3 ms | 35.2% |
| vue-minor | 3.638 s | 1.919 s | 47.3% |
| vue-span | 4.568 s | 2.020 s | 55.8% |
| ts-checker | 1.376 s | 1.222 s | 11.2% |

System time fell from 1.393 s to 0.075 s on `vue-minor` and from 2.135 s to 0.092 s on `vue-span`. Corpora dominated by parsing rather than by file count -- `vue-commit` and `ts-checker` -- improve least, as expected.

Holding the whole range's patch text in memory raised peak RSS on the largest corpora, from 22,720 KiB to 29,356 KiB on `vue-minor` and from 24,636 KiB to 32,768 KiB on `vue-span`. Result and golden tests are unchanged.

### Parallel per-file analysis

With Git access reduced to a constant number of processes, parsing both revisions of each changed file became the dominant cost, and it ran on one thread. Per-file mapping now runs on all available cores. Workers claim files from one shared cursor rather than taking a fixed slice each, so one very large file cannot leave the remaining workers idle.

Ordering is preserved as [`DEFINITIONS.md`](DEFINITIONS.md) requires: each result carries the index of its input and results are restored to input order, so file order, function order, and the sequential function identifiers are unaffected. A failing analysis still reports the error of the earliest file. Output was verified byte-identical to the sequential implementation on every corpus below, including an 8,309,154-byte result, and repeated runs of the same comparison produce identical output.

Measured on 12 logical cores:

| Corpus | Sequential | Parallel | Improvement |
| --- | ---: | ---: | ---: |
| vue-commit | 102.5 ms | 93.4 ms | 8.9% |
| vue-week | 394.0 ms | 148.7 ms | 62.3% |
| vue-patches | 691.8 ms | 194.6 ms | 71.9% |
| vue-minor | 1.914 s | 463.7 ms | 75.8% |
| vue-span | 2.005 s | 590.0 ms | 70.6% |
| ts-checker | 1.218 s | 1.240 s | -1.8% |

`ts-checker` regresses slightly because one 2.9 MiB file accounts for nearly all of its parse work: eleven workers finish immediately and only scheduling overhead remains. Diffs dominated by a single large file are the case per-file parallelism cannot improve; the next section addresses them.

Peak RSS rises because several files are held in flight at once, from 24,108 KiB to 43,708 KiB on `vue-minor` and from 31,680 KiB to 50,988 KiB on `vue-span`. `ts-checker` is unchanged at 69,384 KiB, since its memory is dominated by one file.

### Concurrent base and target parsing

A file's two revisions are independent, so parsing them at once is the only parallelism available to a diff dominated by one large file. Both blobs are analyzed on separate threads when each is at least 64 KiB; smaller blobs stay on the current thread, which keeps file-heavy diffs from starting a second thread per file and oversubscribing the machine.

| Corpus | Per-file only | With concurrent blobs | Improvement |
| --- | ---: | ---: | ---: |
| vue-commit | 95.0 ms | 58.7 ms | 38.2% |
| vue-week | 138.9 ms | 120.2 ms | 13.5% |
| vue-minor | 462.8 ms | 437.4 ms | 5.5% |
| vue-span | 579.7 ms | 578.8 ms | 0.2% |
| ts-checker | 1.237 s | 715.8 ms | 42.1% |

`ts-checker` gains most, as intended. `vue-commit` gains because both of its two files exceed the threshold. File-heavy corpora gain little, because few of their files are large enough to qualify, and none regress.

Holding two syntax trees of the same large file at once costs memory: `ts-checker` peak RSS rises from 69,492 KiB to 126,488 KiB. Corpora whose files are mostly below the threshold change little, from 44,100 KiB to 47,864 KiB on `vue-minor` and not at all on `vue-span`. Output remains byte-identical to a sequential analysis on every corpus.

### Analysis size limit

Profiling the real-world corpora showed that peak memory tracks the size of individual files, not the number of changed files: 4,000 changed files of a few KiB each peak at 95,824 KiB, while one changed 13.5 MiB file peaks at 1,535,628 KiB. A syntax tree costs roughly 20 to 57 times the source it describes, depending on syntax density, and both revisions of a file are analyzed at once.

Source blobs above [`MAX_ANALYZED_BLOB_BYTES`](../src/languages/mod.rs) (5 MiB) are therefore inventoried but not parsed, as [`DEFINITIONS.md`](DEFINITIONS.md) requires. The file keeps its status, hunks, and line counts, and reports an `oversized_file` diagnostic instead of function metrics.

| Corpus | Before the limit | With the limit |
| --- | ---: | ---: |
| one 13.5 MiB file, wall time | 7.038 s | 190.7 ms |
| one 13.5 MiB file, peak RSS | 1,535,628 KiB | 58,004 KiB |

Output for every corpus whose files are below the limit is byte-identical to output from before it, including `ts-checker`, whose 2.9 MiB `checker.ts` stays fully analyzed.

The limit bounds any single file, not the total of many. That is what the in-flight budget below addresses.

### In-flight byte budget

The size limit cannot see a diff of many moderately large files: the `codegen` corpus, 16 changed files of 1.36 MiB each and every one below the limit, peaked near 1.3 GiB because up to twelve files were analyzed at once and each held two syntax trees.

Workers now reserve the bytes they are about to parse against [`ANALYSIS_BYTES_IN_FLIGHT`](../src/application.rs) (8 MiB) and wait when the budget is full. A file larger than the whole budget is admitted whenever nothing else is in flight, so no file can deadlock the analysis, and blobs above the analysis size limit cost nothing because they are never parsed.

| Corpus | Before the budget | With the budget |
| --- | ---: | ---: |
| `codegen`, peak RSS | 1,314,608 KiB | 709,020 KiB |
| `codegen`, wall time | 14.799 s | 14.677 s |

Ordinary diffs never reach the budget -- the 482-file corpus has roughly 16 KiB per file, so twelve files in flight is under 0.4 MiB -- and their time and memory are unchanged. Output is byte-identical with and without the budget, and repeated runs of `codegen` produce identical output, so throttling does not affect ordering.

Roughly 288,000 KiB of what remains on `codegen` is glibc per-thread arena retention rather than live data: running the same comparison with `MALLOC_ARENA_MAX=1` peaks at 423,632 KiB, but costs 21% wall time from allocator contention, so no arena limit is imposed.

### Single complexity traversal

`complexity_points` computes cyclomatic and cognitive complexity in one walk of a function, but the two metrics were requested separately, so every function was traversed twice. Phase timings attributed roughly two thirds of the function collector's time to complexity, and the collector is about half of analysis CPU.

| Corpus | Two traversals | One traversal |
| --- | ---: | ---: |
| vue-commit | 58.6 ms | 54.4 ms |
| vue-week | 120.1 ms | 110.4 ms |
| vue-minor | 448.1 ms | 411.7 ms |
| vue-span | 589.9 ms | 558.7 ms |
| ts-checker | 708.4 ms | 633.7 ms |
| monorepo, 4,000 files | 791.8 ms | 718.3 ms |

Output is byte-identical on every corpus.

### Cumulative effect

Against the first real-world measurement, before whole-range diff collection and parallel analysis:

| Corpus | Original | Current | Speedup |
| --- | ---: | ---: | ---: |
| vue-commit | 107.2 ms | 58.7 ms | 1.83x |
| vue-week | 556.8 ms | 120.2 ms | 4.63x |
| vue-patches | 1.048 s | 194.6 ms | 5.39x |
| vue-minor | 3.638 s | 437.4 ms | 8.32x |
| vue-span | 4.568 s | 578.8 ms | 7.89x |
| ts-checker | 1.376 s | 715.8 ms | 1.92x |

The `Current` column above predates the identity, churn, export, and import-graph work, and is superseded by the real-world table earlier in this document. It is kept because it records what the listed optimizations achieved, which is still true of them.

Further optimization requires a new measurement. Phase timings collected on a 13.5 MiB file attribute 47% of analysis CPU to Tree-sitter parsing and 53% to the function collector, of which complexity accounts for roughly two thirds; Git access, grouping, matching, sorting, and rendering are each under 3%. On the `codegen` corpus the balance inverts and Git dominates at 83%, split evenly between the patch and numstat calls, which compute the same diff twice; requesting both from one invocation was measured and does not help, because Git recomputes internally.

Remaining candidates, in the order the current measurements justify them: deriving binary status from loaded blob content instead of a second Git diff, which costs about six seconds on `codegen`; and rendering output without buffering the entire result.
