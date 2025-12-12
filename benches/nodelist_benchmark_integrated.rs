use arc_swap::ArcSwap;
use criterion::{criterion_group, criterion_main, Criterion};
use liberdus_proxy::{
    archivers::{self, ArchiverUtil},
    config::{self, Config},
    crypto::{self, ShardusCrypto},
    liberdus::{Consensor, Liberdus, SignedNodeListResp},
};
use rand::prelude::*;
use std::time::Duration;
use std::{
    cmp::Ordering,
    collections::HashMap,
    fs,
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc,
    },
};
use tokio::{runtime::Runtime, sync::RwLock};

fn benchmark_get_consensor(c: &mut Criterion) {
    let mut group = c.benchmark_group("Integrated Get Next Server");
    group.measurement_time(Duration::from_secs(50));

    let rt = Runtime::new().unwrap();

    let config = Config::load().unwrap();

    let crypto = Arc::new(ShardusCrypto::new(&config.crypto_seed));

    // Use the mock server address for the seed archiver
    let seed_archiver = {
        let blunt_string = fs::read_to_string(&config.archiver_seed_path)
            .map_err(|err| format!("Failed to read archiver seed file: {}", err))
            .unwrap();

        serde_json::from_str(&blunt_string).unwrap()
    };

    let arch_utils = Arc::new(ArchiverUtil::new(
        crypto.clone(),
        seed_archiver,
        config.clone(),
    ));
    let lbd = Arc::new(Liberdus::new(
        crypto.clone(),
        arch_utils.get_active_archivers(),
        config.clone(),
    ));

    // Background task
    let arch_utils_task = arch_utils.clone();
    let lbd_task = lbd.clone();

    let bg1 = rt.spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            ticker.tick().await;
            Arc::clone(&arch_utils_task).discover().await;
            lbd_task.update_active_nodelist().await;
        }
    });

    loop {
        if lbd.active_nodelist.load().len() > 0 {
            break;
        }
    }

    group.bench_function("Reads", |b| {
        b.to_async(&rt).iter(|| async {
            lbd.get_next_appropriate_consensor().await 
        });
    });

    bg1.abort();

}

criterion_group!(benches, benchmark_get_consensor);
criterion_main!(benches);

