use std::{path::PathBuf, process::Command, time::Duration};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use diffscope::{
    AnalysisRequest, BlobContent, analyze,
    git::Repository,
    imports::index_revision,
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
        eprintln!(
            "{tier}: changed_files={}, changed_functions={function_count}, analyzed_bytes={analyzed_bytes}",
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
    let status = Command::new(manifest.join("scripts/generate-benchmark-corpus.sh"))
        .arg(&root)
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
    targets = analysis, import_graph, query_projection
}
criterion_main!(benches);
