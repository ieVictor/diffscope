use std::{path::PathBuf, process::Command, time::Duration};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use diffscope::{AnalysisRequest, BlobContent, analyze, inventory_changes};

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
    targets = analysis
}
criterion_main!(benches);
