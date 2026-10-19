use std::sync::{
    Arc, Condvar, Mutex,
    mpsc::{self, Receiver},
};

struct Slot<T> {
    pending: Option<T>,
    closed: bool,
}

/// One running operation, one replaceable pending operation and one result.
/// An uninterruptible OS call never causes replacement threads to be spawned.
pub struct LatestWorker<I, O> {
    slot: Arc<(Mutex<Slot<I>>, Condvar)>,
    pub results: Receiver<O>,
}

impl<I: Send + 'static, O: Send + 'static> LatestWorker<I, O> {
    pub fn spawn(
        name: &str,
        ready: Arc<tokio::sync::Notify>,
        mut work: impl FnMut(I) -> O + Send + 'static,
    ) -> std::io::Result<(Self, std::thread::JoinHandle<()>)> {
        let slot = Arc::new((
            Mutex::new(Slot {
                pending: None,
                closed: false,
            }),
            Condvar::new(),
        ));
        let shared = slot.clone();
        let (tx, results) = mpsc::sync_channel(1);
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                loop {
                    let input = {
                        let (lock, wake) = &*shared;
                        let mut state = lock.lock().unwrap();
                        while state.pending.is_none() && !state.closed {
                            state = wake.wait(state).unwrap();
                        }
                        match state.pending.take() {
                            Some(input) => input,
                            None => break,
                        }
                    };
                    if tx.send(work(input)).is_err() {
                        break;
                    }
                    ready.notify_one();
                }
            })?;
        Ok((Self { slot, results }, handle))
    }

    pub fn submit(&self, input: I) {
        let mut state = self.slot.0.lock().unwrap();
        assert!(!state.closed);
        state.pending = Some(input);
        self.slot.1.notify_one();
    }

    /// Finish the current operation and the last submitted value, then exit.
    pub fn close(&self) {
        self.slot.0.lock().unwrap().closed = true;
        self.slot.1.notify_one();
    }
}

impl<I, O> Drop for LatestWorker<I, O> {
    fn drop(&mut self) {
        let mut state = self.slot.0.lock().unwrap();
        state.pending = None;
        state.closed = true;
        self.slot.1.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn only_latest_pending_value_is_drained_on_close() {
        let (entered, active) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (worker, handle) = LatestWorker::spawn(
            "bounded-test",
            Arc::new(tokio::sync::Notify::new()),
            move |value| {
                if value == 0 {
                    entered.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                value
            },
        )
        .unwrap();
        worker.submit(0);
        active.recv_timeout(Duration::from_secs(2)).unwrap();
        for value in 1..=10000 {
            worker.submit(value);
        }
        worker.close();
        release.send(()).unwrap();
        assert_eq!(
            worker.results.recv_timeout(Duration::from_secs(2)).unwrap(),
            0
        );
        assert_eq!(
            worker.results.recv_timeout(Duration::from_secs(2)).unwrap(),
            10000
        );
        handle.join().unwrap();
        assert!(worker.results.try_recv().is_err());
    }
}
