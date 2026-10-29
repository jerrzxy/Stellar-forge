use criterion::{black_box, criterion_group, criterion_main, Criterion};
use stellar_telemetry::ebpf::collector::{simulate_loss_workload, CollectorMode, EbpfCollector};
use std::net::{IpAddr, Ipv4Addr};

fn bench_ingest_50k(c: &mut Criterion) {
    c.bench_function("ebpf_ingest_50k_samples", |b| {
        b.iter(|| {
            let collector = EbpfCollector::new(CollectorMode::Ebpf);
            collector.watch_stellar_core(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1)));
            simulate_loss_workload(&collector, black_box(50_000), 0.01, 0.0);
        });
    });
}

criterion_group!(benches, bench_ingest_50k);
criterion_main!(benches);
