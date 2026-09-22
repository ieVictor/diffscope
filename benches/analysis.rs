use std::{path::PathBuf, process::Command, time::Duration};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use diffscope::{
    AnalysisRequest, BlobContent, analyze,
    git::Repository,
    graph::{
        self, Direction, Limits, Relation, View,
        build::{Request, Revisions},
        canonical_depth,
    },
    imports::{index_revision, index_symbols},
    inventory_changes,
    query::{self, FileFilter, FunctionFilter},
};

const TIERS: [&str; 3] = ["small", "medium", "large"];

fn analysis(criterion: &mut Criterion) {
    let corpus_root = prepare_corpus();
    let mut group = criterion.benchmark_group("git_analysis");

    for tier in TIERS {
        let request = AnalysisRequest {
            repository_path: corpus_root.join(tier),
            base_revision: "HEAD~1".to_owned(),
            target_revision: "HEAD".to_owned(),
        };
        let inventory = inventory_changes(&request).expect("benchmark inventory succeeds");
        let analyzed_bytes = inventory
            .files
            .iter()
            .map(|file| blob_bytes(&file.base_blob) + blob_bytes(&file.target_blob))
            .sum::<u64>();
        let baseline = analyze(&request).expect("benchmark analysis succeeds");
        let function_count = baseline
            .files
            .iter()
            .map(|file| file.functions.len())
            .sum::<usize>();
        // Call collection runs inside the collector's own traversal, so it is
        // part of the time measured below rather than a phase of its own. The
        // count is reported because the cost is proportional to it: without the
        // number, a tier's figure cannot be read as evidence about calls.
        let call_count = baseline
            .files
            .iter()
            .flat_map(|file| &file.functions)
            .map(|function| function.calls_before.len() + function.calls_after.len())
            .sum::<usize>();
        eprintln!(
            "{tier}: changed_files={}, changed_functions={function_count}, analyzed_bytes={analyzed_bytes}, collected_calls={call_count}",
            baseline.summary.changed_files
        );

        group.throughput(Throughput::Bytes(analyzed_bytes));
        group.bench_with_input(
            BenchmarkId::from_parameter(tier),
            &request,
            |bencher, request| {
                bencher.iter(|| analyze(std::hint::black_box(request)).expect("analysis succeeds"));
            },
        );
    }
    group.finish();
}

/// Building a revision's import graph reads every source file it contains, not
/// only the changed ones, so its cost scales with the repository rather than
/// with the diff. It is measured separately for that reason.
fn import_graph(criterion: &mut Criterion) {
    let corpus_root = prepare_corpus();
    let mut group = criterion.benchmark_group("import_graph");

    for tier in TIERS {
        let repository =
            Repository::open(&corpus_root.join(tier)).expect("benchmark repository opens");
        let commit = repository
            .resolve_revision("HEAD")
            .expect("benchmark revision resolves")
            .commit_id;
        let baseline = index_revision(&repository, &commit).expect("benchmark index succeeds");
        eprintln!(
            "{tier}: indexed_files={}, unresolved_specifiers={}",
            baseline.scanned_files(),
            baseline.unresolved_specifiers()
        );

        group.bench_with_input(
            BenchmarkId::from_parameter(tier),
            &commit,
            |bencher, commit| {
                bencher.iter(|| {
                    index_revision(&repository, std::hint::black_box(commit))
                        .expect("index succeeds")
                });
            },
        );
    }
    group.finish();
}

/// A delta graph is built from one analysis and the import indexes of both
/// revisions, so it costs one index beyond the target's plus the comparison
/// between the two. The comparison only follows edges the indexes already hold,
/// so it is measured against them rather than assumed cheap, and a rendering
/// runs on every request that asks for one, so each is measured to prove it is
/// negligible.
fn impact_graph(criterion: &mut Criterion) {
    let corpus_root = prepare_corpus();
    let mut group = criterion.benchmark_group("impact_graph");
    for tier in TIERS {
        impact_graph_tier(&mut group, &corpus_root, tier);
    }
    group.finish();
}

/// One corpus tier's graph costs: both indexes, the symbol index caller
/// discovery needs, the build, and each rendering.
fn impact_graph_tier(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    corpus_root: &std::path::Path,
    tier: &str,
) {
    {
        let analysis_request = AnalysisRequest {
            repository_path: corpus_root.join(tier),
            base_revision: "HEAD~1".to_owned(),
            target_revision: "HEAD".to_owned(),
        };
        let result = analyze(&analysis_request).expect("benchmark analysis succeeds");
        let repository =
            Repository::open(&corpus_root.join(tier)).expect("benchmark repository opens");
        let base_commit = repository
            .resolve_revision("HEAD~1")
            .expect("benchmark revision resolves")
            .commit_id;
        let target_commit = repository
            .resolve_revision("HEAD")
            .expect("benchmark revision resolves")
            .commit_id;
        let base = index_revision(&repository, &base_commit).expect("benchmark index succeeds");
        let target = index_revision(&repository, &target_commit).expect("benchmark index succeeds");
        // The files cross-file resolution is allowed to parse: the changed
        // files, their direct importers, and what they import. This is the
        // bound that makes function-level impact affordable, so its size is
        // reported per corpus rather than assumed small.
        let admitted = graph::build::admitted_files(&result, &base, &target);
        let base_symbols = index_symbols(&repository, &base_commit, &admitted)
            .expect("benchmark symbol index succeeds");
        let target_symbols = index_symbols(&repository, &target_commit, &admitted)
            .expect("benchmark symbol index succeeds");
        let revisions = Revisions {
            base: &base,
            target: &target,
            base_symbols: &base_symbols,
            target_symbols: &target_symbols,
        };
        // The request a default query makes: no named root (the changed set),
        // both directions, every supported relation, and canonical bounds.
        let request = Request {
            roots: &[],
            function_root: None,
            direction: Direction::Both,
            relations: Relation::SUPPORTED,
            depth: canonical_depth(None),
            view: View::Delta,
            limits: Limits::canonical(None, None),
        };
        let graph = graph::build::build(&result, &revisions, &request);
        eprintln!(
            "{tier}: base_indexed_files={}, target_indexed_files={}, changed_files={}, admitted_files={}, nodes={}, edges={}, truncated={}",
            base.scanned_files(),
            target.scanned_files(),
            result.files.len(),
            admitted.len(),
            graph.nodes().len(),
            graph.edges().len(),
            graph.truncated()
        );

        group.bench_with_input(
            BenchmarkId::new("base_index", tier),
            &base_commit,
            |bencher, commit| {
                bencher.iter(|| {
                    index_revision(&repository, std::hint::black_box(commit))
                        .expect("index succeeds")
                });
            },
        );
        // Caller discovery is the first thing DiffScope parses at full
        // fidelity outside the diff, so the cost of the files the bound admits
        // is measured as its own phase.
        group.bench_with_input(
            BenchmarkId::new("symbol_index", tier),
            &target_commit,
            |bencher, commit| {
                bencher.iter(|| {
                    index_symbols(
                        &repository,
                        std::hint::black_box(commit),
                        std::hint::black_box(&admitted),
                    )
                    .expect("symbol index succeeds")
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("build", tier),
            &request,
            |bencher, request| {
                bencher.iter(|| {
                    graph::build::build(
                        std::hint::black_box(&result),
                        std::hint::black_box(&revisions),
                        std::hint::black_box(request),
                    )
                });
            },
        );
        render_benches(group, tier, &graph);
    }
}

/// Every rendering runs on the request that asks for it, so each is measured
/// to prove it is negligible beside the build.
fn render_benches(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    tier: &str,
    graph: &diffscope::graph::Graph,
) {
    group.bench_with_input(
        BenchmarkId::new("render_dependency_diff", tier),
        graph,
        |bencher, graph| {
            bencher.iter(|| graph::render::dependency_diff(std::hint::black_box(graph)));
        },
    );
    group.bench_with_input(
        BenchmarkId::new("render_mermaid", tier),
        graph,
        |bencher, graph| {
            bencher.iter(|| graph::render::mermaid(std::hint::black_box(graph)));
        },
    );
}

/// Projecting an analysis into one answer runs on every request, including the
/// ones served from a cached analysis, so it is the floor on query latency.
fn query_projection(criterion: &mut Criterion) {
    let corpus_root = prepare_corpus();
    let mut group = criterion.benchmark_group("query");

    let request = AnalysisRequest {
        repository_path: corpus_root.join("large"),
        base_revision: "HEAD~1".to_owned(),
        target_revision: "HEAD".to_owned(),
    };
    let result = analyze(&request).expect("benchmark analysis succeeds");

    group.bench_function("change_summary", |bencher| {
        bencher.iter(|| query::change_summary(std::hint::black_box(&result), None));
    });
    group.bench_function("list_changed_files", |bencher| {
        let filter = FileFilter::default();
        bencher.iter(|| query::list_changed_files(std::hint::black_box(&result), &filter, None));
    });
    group.bench_function("list_changed_functions", |bencher| {
        let filter = FunctionFilter::default();
        bencher
            .iter(|| query::list_changed_functions(std::hint::black_box(&result), &filter, None));
    });
    group.finish();
}

fn prepare_corpus() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("target/benchmark-corpus");
    // Each commit the generator makes spawns detached Git auto-maintenance
    // (`git maintenance run --auto`, which repacks) that outlives the script and
    // writes into `.git/objects` while the next generation removes that
    // repository, failing the run on a non-empty directory. The benchmarks only
    // read the corpus, so packing it is pure noise; disable the detached
    // maintenance and the legacy auto-gc for the generator's own Git calls.
    let status = Command::new(manifest.join("scripts/generate-benchmark-corpus.sh"))
        .arg(&root)
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "gc.auto")
        .env("GIT_CONFIG_VALUE_1", "0")
        .status()
        .expect("run benchmark corpus generator");
    assert!(status.success(), "benchmark corpus generation failed");
    root
}

fn blob_bytes(blob: &BlobContent) -> u64 {
    match blob {
        BlobContent::Available(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        BlobContent::Missing | BlobContent::NotApplicable | BlobContent::Binary => 0,
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = analysis, import_graph, impact_graph, query_projection
}
criterion_main!(benches);
