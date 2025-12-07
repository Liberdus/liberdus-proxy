use std::sync::Arc;
use tokio::sync::watch;

pub struct SwapCell<T> {
    tx: watch::Sender<Arc<T>>,
    rx: watch::Receiver<Arc<T>>,
}

impl<T> SwapCell<T>
where
    T: Clone,
{
    pub fn new(initial_value: T) -> Self {
        let (tx, rx) = watch::channel(Arc::new(initial_value));
        Self { tx, rx }
    }

    pub fn publish(&self, new_value: T) {
        self.tx.send(Arc::new(new_value)).ok();
    }

    pub fn get_latest(&self) -> Arc<T> {
        self.rx.borrow().clone()
    }

    pub fn get_rx(&self) -> watch::Receiver<Arc<T>> {
        self.rx.clone()
    }
}
