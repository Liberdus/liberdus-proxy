use criterion::{criterion_group, criterion_main, Criterion};
use liberdus_proxy::liberdus::Consensor;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use arc_swap::ArcSwap;

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
    let mut group = c.benchmark_group("Read Latency With and Without Write Contentions");

    group.measurement_time(Duration::from_secs(20));

    let runtime = Runtime::new().unwrap();

    // --- RwLock Benchmark ---
    let rwlock_nodelist = Arc::new(RwLock::new(generate_test_nodelist(NODELIST_SIZE)));
    let rwlock_clone = rwlock_nodelist.clone();
    let rwlock_handle = runtime.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_nanos(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            interval.tick().await;
            let new_list = generate_test_nodelist(NODELIST_SIZE);
            {
                let mut guard = rwlock_clone.write().await;
                *guard = new_list;
            }
        }
    });

    group.bench_function("RwLock Read", |b| {
        b.to_async(&runtime).iter(|| async {
            let _guard = rwlock_nodelist.read().await;
        });
    });

    rwlock_handle.abort();

    // --- ArcSwap Benchmark ---
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

    group.bench_function("ArcSwap Read", |b| {
        b.iter(|| {
            let _guard = arcswap_nodelist.load_full();
        });
    });

    arcswap_handle.abort();

    group.finish();
}

criterion_group!(benches, benchmark_read_latency);
criterion_main!(benches);
