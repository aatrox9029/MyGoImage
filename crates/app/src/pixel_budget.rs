//! Backpressure for decoded results. Arc aliases count once while queued.
//! This bounds the result stage, not the allocator, UI textures or process RSS.
use mygo_core::Cancel;
use mygo_decoder::Frame;
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

#[derive(Default, Debug)]
struct State {
    allocations: HashMap<usize, (usize, usize)>,
    used: usize,
    closed: bool,
}
#[derive(Clone, Debug)]
pub struct PixelBudget {
    inner: Arc<(Mutex<State>, Condvar)>,
    limit: usize,
}
#[derive(Debug)]
pub struct PixelPermit {
    budget: PixelBudget,
    identity: usize,
    // Keep the allocation alive even if a consumer separates frame and permit;
    // allocator pointer reuse must never alias a different image's accounting.
    pixels: Arc<Vec<u8>>,
}
impl PixelBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new((Mutex::new(State::default()), Condvar::new())),
            limit,
        }
    }
    pub fn acquire(&self, frame: &Frame, cancel: &Cancel) -> Option<PixelPermit> {
        self.reserve(frame, cancel, true)
    }
    pub fn try_acquire(&self, frame: &Frame, cancel: &Cancel) -> Option<PixelPermit> {
        self.reserve(frame, cancel, false)
    }
    fn reserve(&self, frame: &Frame, cancel: &Cancel, wait: bool) -> Option<PixelPermit> {
        let identity = Arc::as_ptr(&frame.rgba) as usize;
        let bytes = frame.bytes();
        let (lock, wake) = &*self.inner;
        let mut state = lock.lock().unwrap();
        loop {
            if state.closed || cancel.cancelled() {
                return None;
            }
            if let Some((_, references)) = state.allocations.get_mut(&identity) {
                *references += 1;
                break;
            }
            // One oversized frame may pass alone, so high resolution requests
            // cannot deadlock on a queue limit intended for small thumbnails.
            if state.used == 0 || bytes <= self.limit.saturating_sub(state.used) {
                state.used += bytes;
                state.allocations.insert(identity, (bytes, 1));
                break;
            }
            if !wait {
                return None;
            }
            state = wake
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        Some(PixelPermit {
            budget: self.clone(),
            identity,
            pixels: frame.rgba.clone(),
        })
    }
    pub fn used(&self) -> usize {
        self.inner.0.lock().unwrap().used
    }
    pub fn close(&self) {
        self.inner.0.lock().unwrap().closed = true;
        self.inner.1.notify_all();
    }
}
impl Clone for PixelPermit {
    fn clone(&self) -> Self {
        self.budget
            .inner
            .0
            .lock()
            .unwrap()
            .allocations
            .get_mut(&self.identity)
            .unwrap()
            .1 += 1;
        Self {
            budget: self.budget.clone(),
            identity: self.identity,
            pixels: self.pixels.clone(),
        }
    }
}
impl Drop for PixelPermit {
    fn drop(&mut self) {
        let mut state = self.budget.inner.0.lock().unwrap();
        let (bytes, references) = state.allocations.get_mut(&self.identity).unwrap();
        *references -= 1;
        if *references == 0 {
            let bytes = *bytes;
            state.allocations.remove(&self.identity);
            state.used -= bytes;
        }
        self.budget.inner.1.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(bytes: usize) -> Frame {
        Frame {
            width: (bytes / 4) as u32,
            height: 1,
            original_width: (bytes / 4) as u32,
            original_height: 1,
            orientation: 1,
            rgba: Arc::new(vec![0; bytes]),
        }
    }
    #[test]
    fn aliases_count_once_and_dropping_results_releases_capacity() {
        let budget = PixelBudget::new(8);
        let image = frame(8);
        let first = budget.acquire(&image, &Cancel::default()).unwrap();
        let alias = budget.acquire(&image.clone(), &Cancel::default()).unwrap();
        assert_eq!(budget.used(), 8);
        drop(first);
        assert_eq!(budget.used(), 8);
        drop(alias);
        assert_eq!(budget.used(), 0);
        let large = budget.acquire(&frame(16), &Cancel::default()).unwrap();
        assert_eq!(budget.used(), 16);
        drop(large);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn saturated_results_stop_on_cancel_and_shutdown() {
        let budget = PixelBudget::new(4);
        let _held = budget.acquire(&frame(4), &Cancel::default()).unwrap();
        let cancel = Cancel::default();
        let waiting = budget.clone();
        let token = cancel.clone();
        let handle = std::thread::spawn(move || waiting.acquire(&frame(4), &token).is_none());
        cancel.cancel();
        assert!(handle.join().unwrap());
        budget.close();
        assert!(budget.acquire(&frame(4), &Cancel::default()).is_none());
    }
}
