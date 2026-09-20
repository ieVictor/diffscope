use criterion::{Criterion, criterion_group, criterion_main};

// Add focused benchmarks alongside the first analysis behavior.
fn analysis(_criterion: &mut Criterion) {}

criterion_group!(benches, analysis);
criterion_main!(benches);
