use criterion::{criterion_group, criterion_main, Criterion};
use liberdus_proxy::liberdus::Consensor;
use liberdus_proxy::swap_cell::SwapCell;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

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

    // --- SharedData Benchmark ---
    let swapcell_nodelist = Arc::new(SwapCell::new(generate_test_nodelist(NODELIST_SIZE)));
    let swapcell_clone = swapcell_nodelist.clone();
    let swapcell_handle = runtime.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_nanos(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            interval.tick().await;
            let new_list = generate_test_nodelist(NODELIST_SIZE);
            swapcell_clone.publish(new_list);
        }
    });

    group.bench_function("SwapCell Read", |b| {
        b.iter(|| {
            let _guard = swapcell_nodelist.get_latest();
        });
    });

    swapcell_handle.abort();

    group.finish();
}

criterion_group!(benches, benchmark_read_latency);
criterion_main!(benches);
