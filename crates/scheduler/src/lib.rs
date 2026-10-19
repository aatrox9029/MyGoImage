use mygo_core::Cancel;
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Main = 0,
    Visible = 1,
    Prefetch = 2,
}
pub struct Job<T> {
    pub request_id: u64,
    pub generation_id: u64,
    pub file_id: i64,
    pub priority: Priority,
    pub cancel: Cancel,
    pub payload: T,
}
struct State<T> {
    queues: [VecDeque<Job<T>>; 3],
    closed: bool,
}
pub struct Scheduler<T> {
    inner: Arc<(Mutex<State<T>>, Condvar)>,
}
impl<T> Clone for Scheduler<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl<T> Default for Scheduler<T> {
    fn default() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(State {
                    queues: std::array::from_fn(|_| VecDeque::new()),
                    closed: false,
                }),
                Condvar::new(),
            )),
        }
    }
}
impl<T> Scheduler<T> {
    pub fn promote(&self, request: u64) {
        let mut s = self.inner.0.lock().unwrap();
        s.queues[0].retain(|job| !job.cancel.cancelled());
        for priority in 1..3 {
            if let Some(index) = s.queues[priority]
                .iter()
                .position(|j| j.request_id == request)
            {
                let mut job = s.queues[priority].remove(index).unwrap();
                job.priority = Priority::Main;
                if s.queues[0].len() < 4 {
                    s.queues[0].push_back(job);
                } else {
                    job.priority = if priority == 1 {
                        Priority::Visible
                    } else {
                        Priority::Prefetch
                    };
                    s.queues[priority].push_front(job);
                }
                self.inner.1.notify_one();
                break;
            }
        }
    }
    pub fn submit(&self, job: Job<T>) -> bool {
        if job.cancel.cancelled() {
            return false;
        }
        let (m, c) = &*self.inner;
        let mut s = m.lock().unwrap();
        if s.closed {
            return false;
        }
        let q = &mut s.queues[job.priority as usize];
        q.retain(|j| !j.cancel.cancelled());
        if q.iter().any(|j| {
            j.file_id == job.file_id
                && j.generation_id == job.generation_id
                && j.request_id == job.request_id
        }) {
            return true;
        }
        let cap = match job.priority {
            Priority::Main => 4,
            Priority::Visible => 96,
            Priority::Prefetch => 24,
        };
        if q.len() >= cap {
            if job.priority == Priority::Prefetch {
                return false;
            }
            if let Some(evicted) = q.pop_front() {
                evicted.cancel.cancel();
            }
        }
        q.push_back(job);
        c.notify_one();
        true
    }
    pub fn pop(&self) -> Option<Job<T>> {
        let (m, c) = &*self.inner;
        let mut s = m.lock().unwrap();
        loop {
            if s.closed {
                return None;
            }
            for q in &mut s.queues {
                while let Some(j) = q.pop_front() {
                    if !j.cancel.cancelled() {
                        return Some(j);
                    }
                }
            }
            s = c.wait(s).unwrap();
        }
    }
    pub fn close(&self) {
        let (m, c) = &*self.inner;
        let mut s = m.lock().unwrap();
        s.closed = true;
        for q in &mut s.queues {
            q.clear();
        }
        c.notify_all();
    }
    /// Drain only already queued compatible work; never wait to form a batch.
    /// Preserve Main/Visible/Prefetch ordering and independent cancellation.
    pub fn take_matching(&self, limit: usize, matches: impl Fn(&T) -> bool) -> Vec<Job<T>> {
        let mut state = self.inner.0.lock().unwrap();
        let mut jobs = Vec::new();
        if state.closed {
            return jobs;
        }
        for queue in &mut state.queues {
            queue.retain(|job| !job.cancel.cancelled());
            let mut index = 0;
            while index < queue.len() && jobs.len() < limit {
                if matches(&queue[index].payload) {
                    jobs.push(queue.remove(index).unwrap());
                } else {
                    index += 1;
                }
            }
        }
        jobs
    }
    pub fn pending(&self) -> usize {
        self.inner
            .0
            .lock()
            .unwrap()
            .queues
            .iter()
            .map(VecDeque::len)
            .sum()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compatible_batches_preserve_priority_and_leave_other_work_queued() {
        let scheduler = Scheduler::default();
        scheduler.submit(job(Priority::Prefetch, 4, Cancel::default()));
        scheduler.submit(job(Priority::Visible, 2, Cancel::default()));
        scheduler.submit(job(Priority::Main, 1, Cancel::default()));
        scheduler.submit(job(Priority::Visible, 3, Cancel::default()));
        // Request identity can be checked by the predicate's payload in real use.
        let batch = scheduler.take_matching(2, |_| true);
        assert_eq!(
            batch.iter().map(|j| j.request_id).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(scheduler.pop().unwrap().request_id, 3);
        assert_eq!(scheduler.pop().unwrap().request_id, 4);
        scheduler.close();
    }
    fn job(p: Priority, id: u64, c: Cancel) -> Job<()> {
        Job {
            request_id: id,
            generation_id: 1,
            file_id: id as i64,
            priority: p,
            cancel: c,
            payload: (),
        }
    }
    #[test]
    fn promoted_prefetch_wins_next_execution_boundary_and_eviction_cancels_work() {
        let scheduler = Scheduler::default();
        scheduler.submit(job(Priority::Visible, 1, Cancel::default()));
        scheduler.submit(job(Priority::Prefetch, 2, Cancel::default()));
        scheduler.promote(2);
        assert_eq!(scheduler.pop().unwrap().file_id, 2);
        let evicted = Cancel::default();
        scheduler.submit(job(Priority::Main, 3, evicted.clone()));
        for id in 4..8 {
            scheduler.submit(job(Priority::Main, id, Cancel::default()));
        }
        assert!(evicted.cancelled());
        scheduler.close();
    }
    #[test]
    fn bounded_priority_cancel() {
        let s = Scheduler::default();
        for i in 0..10000 {
            s.submit(job(Priority::Prefetch, i, Cancel::default()));
        }
        assert_eq!(s.pending(), 24);
        let c = Cancel::default();
        s.submit(job(Priority::Main, 999, c.clone()));
        c.cancel();
        s.submit(job(Priority::Visible, 123, Cancel::default()));
        s.submit(job(Priority::Main, 124, Cancel::default()));
        assert_eq!(s.pop().unwrap().file_id, 124);
        assert_eq!(s.pop().unwrap().file_id, 123);
        s.close();
        assert!(s.pop().is_none());
    }
}
