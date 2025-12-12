use arc_swap::ArcSwap;
use criterion::{criterion_group, criterion_main, Criterion};
use liberdus_proxy::liberdus::Consensor;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;

const NODELIST_SIZE: usize = 20_000;

fn generate_test_nodelist(size: usize) -> Vec<Consensor> {
    (0..size)
        .map(|i| Consensor {
            foundationNode: Some(false),
            id: i.to_string(),
            ip: "127.0.0.1".to_string(),
            port: 8080 + i as u16,
            publicKey: format!("pk_{}", i),
            rng_bias: None,
        })
        .collect()
}

fn benchmark_read_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("Read Latency Under Write Contentions");

    group.measurement_time(Duration::from_secs(5));

    let runtime = Runtime::new().unwrap();

    let arcswap_nodelist = Arc::new(ArcSwap::from_pointee(generate_test_nodelist(NODELIST_SIZE)));
    let arcswap_clone = arcswap_nodelist.clone();
    let arcswap_handle = runtime.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_nanos(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            interval.tick().await;
            let new_list = generate_test_nodelist(NODELIST_SIZE);
            arcswap_clone.store(Arc::new(new_list));
        }
    });

    group.bench_function("ArcSwap load_full()", |b| {
        b.iter(|| {
            let _guard = arcswap_nodelist.load_full();
        });
    });

    group.bench_function("ArcSwap load()", |b| {
        b.iter(|| {
            let _guard = arcswap_nodelist.load();
        });
    });

    arcswap_handle.abort();

    group.finish();
}

criterion_group!(benches, benchmark_read_latency);
criterion_main!(benches);
