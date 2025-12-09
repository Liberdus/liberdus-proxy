use arc_swap::ArcSwap;
use std::sync::Arc;
use tokio::runtime::Runtime;

#[test]
fn test_arc_swap_concurrency() {
    let rt = Runtime::new().unwrap();
    let data = Arc::new(ArcSwap::from_pointee(vec![1, 2, 3]));
    let data_clone = data.clone();

    let handle = rt.spawn(async move {
        for i in 0..1000 {
            let new_data = vec![i, i + 1, i + 2];
            data_clone.store(Arc::new(new_data));
            tokio::task::yield_now().await;
        }
    });

    rt.block_on(async {
        for _ in 0..1000 {
            let current = data.load_full();
            assert_eq!(current.len(), 3);
            tokio::task::yield_now().await;
        }
        handle.await.unwrap();
        let final_data = data.load_full();
        assert_eq!(final_data.len(), 3);
    });
}
