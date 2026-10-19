//! Bounded, nonblocking UI/controller mailboxes. Never spawn a thread per command.
use crate::controller::{Action, Event};
#[cfg(test)]
use std::time::{Duration, Instant};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};

struct Commands {
    queue: VecDeque<Action>,
    shutdown: bool,
    cancel: bool,
    closed: bool,
}
#[derive(Clone)]
pub struct CommandSender(Arc<(Mutex<Commands>, tokio::sync::Notify)>);
pub struct CommandReceiver(Arc<(Mutex<Commands>, tokio::sync::Notify)>);
pub fn commands() -> (CommandSender, CommandReceiver) {
    let state = Arc::new((
        Mutex::new(Commands {
            queue: VecDeque::new(),
            shutdown: false,
            cancel: false,
            closed: false,
        }),
        tokio::sync::Notify::new(),
    ));
    (CommandSender(state.clone()), CommandReceiver(state))
}
impl CommandSender {
    pub fn try_send(&self, action: Action) -> Result<(), &'static str> {
        let mut state = self.0.0.lock().unwrap();
        if state.closed {
            return Err("控制器已關閉");
        }
        match action {
            Action::Shutdown => {
                state.shutdown = true;
                state.closed = true;
            }
            Action::Cancel => {
                state.cancel = true;
                state.queue.retain(|a| {
                    !matches!(
                        a,
                        Action::Navigate(_)
                            | Action::Select(_)
                            | Action::Visible(_)
                            | Action::High
                            | Action::Open(..)
                            | Action::Rescan(_)
                            | Action::Retry
                    )
                });
            }
            action => {
                if let (Some(Action::Navigate(old)), Action::Navigate(new)) =
                    (state.queue.back_mut(), &action)
                    && old.signum() == new.signum()
                {
                    *old = old.saturating_add(*new);
                    drop(state);
                    self.0.1.notify_one();
                    return Ok(());
                }
                let replace = matches!(
                    (state.queue.back(), &action),
                    (Some(Action::Visible(_)), Action::Visible(_))
                        | (Some(Action::ThumbnailSize(_)), Action::ThumbnailSize(_))
                        | (Some(Action::PreviewSize(_)), Action::PreviewSize(_))
                        | (Some(Action::VisibleCount(_)), Action::VisibleCount(_))
                        | (Some(Action::Select(_)), Action::Select(_))
                        | (Some(Action::Sort(..)), Action::Sort(..))
                        | (Some(Action::Settings(..)), Action::Settings(..))
                );
                if replace {
                    state.queue.pop_back();
                }
                if state.queue.len() == 64 {
                    return Err("操作佇列已滿，請稍後重試");
                }
                state.queue.push_back(action);
            }
        }
        drop(state);
        self.0.1.notify_one();
        Ok(())
    }
    #[cfg(test)]
    pub fn blocking_send(&self, action: Action) -> Result<(), &'static str> {
        self.try_send(action)
    }
}
impl CommandReceiver {
    /// Preserve the final accepted settings snapshot when shutdown overtakes
    /// ordinary commands. Other queued work is intentionally abandoned.
    pub fn take_latest_settings(&mut self) -> Option<Action> {
        let mut state = self.0.0.lock().unwrap();
        let index = state
            .queue
            .iter()
            .rposition(|action| matches!(action, Action::Settings(..)))?;
        state.queue.remove(index)
    }
    pub async fn recv(&mut self) -> Option<Action> {
        loop {
            let notified = self.0.1.notified();
            {
                let mut s = self.0.0.lock().unwrap();
                if s.shutdown {
                    s.shutdown = false;
                    return Some(Action::Shutdown);
                }
                if s.cancel {
                    s.cancel = false;
                    return Some(Action::Cancel);
                }
                if let Some(a) = s.queue.pop_front() {
                    return Some(a);
                }
                if s.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}
impl Drop for CommandReceiver {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().closed = true;
    }
}

struct Events {
    queue: VecDeque<Event>,
    closed: bool,
    suppressed: u64,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}
#[derive(Clone)]
pub struct EventSender(Arc<(Mutex<Events>, Condvar)>);
pub struct EventReceiver(Arc<(Mutex<Events>, Condvar)>);
pub fn events() -> (EventSender, EventReceiver) {
    let state = Arc::new((
        Mutex::new(Events {
            queue: VecDeque::new(),
            closed: false,
            suppressed: 0,
            wake: None,
        }),
        Condvar::new(),
    ));
    (EventSender(state.clone()), EventReceiver(state))
}
fn same_kind(a: &Event, b: &Event) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}
impl EventSender {
    pub fn send(&self, event: Event) -> Result<(), ()> {
        let mut s = self.0.0.lock().unwrap();
        if s.closed {
            return Err(());
        }
        match &event {
            Event::Reset{..} => s.queue.retain(|e| matches!(e,Event::Error(_) | Event::Settings(..))),
            Event::Selection{..} => s.queue.retain(|e| !matches!(e,Event::Selection{..}|Event::Main{..}|Event::Relocate{..})),
            Event::Thumbnail{generation,index,id,..} => s.queue.retain(|e| !matches!(e,Event::Thumbnail{generation:g,index:i,id:f,..} if g==generation && (i==index || f==id))),
            Event::Error(_) => {
                if s.queue.iter().filter(|e| matches!(e,Event::Error(_))).count() >= 16 {
                    let index=s.queue.iter().position(|e| matches!(e,Event::Error(_))).unwrap();
                    s.queue.remove(index); s.suppressed+=1;
                }
            }
            _ => s.queue.retain(|e| !same_kind(e,&event)),
        }
        if let Event::Page {
            generation,
            start,
            entries,
        } = &event
        {
            s.queue.retain(|e| !matches!(e,Event::Thumbnail{generation:g,index,..} if g!=generation || *index<*start || *index>=*start+entries.len()));
        }
        if s.queue.len() >= 96 {
            if let Some(index) = s
                .queue
                .iter()
                .position(|e| matches!(e, Event::Thumbnail { .. }))
            {
                s.queue.remove(index);
            } else {
                s.suppressed += 1;
                return Err(());
            }
        }
        s.queue.push_back(event);
        let wake = s.wake.clone();
        drop(s);
        if let Some(wake) = wake {
            wake();
        }
        self.0.1.notify_one();
        Ok(())
    }
}
impl EventReceiver {
    pub fn set_waker(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.0.0.lock().unwrap().wake = Some(wake);
        self.wake_pending();
    }
    pub fn wake_pending(&self) {
        let s = self.0.0.lock().unwrap();
        let wake = if !s.queue.is_empty() || s.suppressed > 0 {
            s.wake.clone()
        } else {
            None
        };
        drop(s);
        if let Some(wake) = wake {
            wake();
        }
    }

    pub fn try_recv(&self) -> Result<Event, ()> {
        let mut s = self.0.0.lock().unwrap();
        if s.suppressed > 0 {
            let count = std::mem::take(&mut s.suppressed);
            return Ok(Event::Error(format!("另有 {count} 筆重複或過量事件已合併")));
        }
        // Present the current main image before bulk thumbnail uploads, but first
        // apply the structural state and errors which precede that image.
        let index = if let Some(main) = s
            .queue
            .iter()
            .position(|event| matches!(event, Event::Main { .. }))
        {
            s.queue
                .iter()
                .take(main)
                .position(|event| {
                    matches!(
                        event,
                        Event::Reset { .. }
                            | Event::Selection { .. }
                            | Event::Settings(..)
                            | Event::Error(_)
                    )
                })
                .unwrap_or(main)
        } else {
            0
        };
        s.queue.remove(index).ok_or(())
    }
    #[cfg(test)]
    pub fn recv_timeout(&self, duration: Duration) -> Result<Event, ()> {
        let deadline = Instant::now() + duration;
        loop {
            if let Ok(e) = self.try_recv() {
                return Ok(e);
            }
            let s = self.0.0.lock().unwrap();
            if !s.queue.is_empty() {
                continue;
            }
            if Instant::now() >= deadline {
                return Err(());
            }
            drop(
                self.0
                    .1
                    .wait_timeout(s, deadline.saturating_duration_since(Instant::now()))
                    .unwrap(),
            );
        }
    }
}
impl Drop for EventReceiver {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().closed = true;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cancel_preserves_queued_geometry_for_the_next_explicit_selection() {
        let (tx, mut rx) = commands();
        tx.try_send(Action::PreviewSize(2048)).unwrap();
        tx.try_send(Action::Cancel).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(matches!(rx.recv().await, Some(Action::Cancel)));
            assert!(matches!(rx.recv().await, Some(Action::PreviewSize(2048))));
        });
        // PreviewSize updates geometry after cancellation without decoding; the
        // controller test separately verifies that automatic resizing stays idle.
    }
    #[test]
    fn cancel_discards_queued_source_work_and_shutdown_keeps_last_settings() {
        let (tx, mut rx) = commands();
        tx.try_send(Action::Open("first".into(), false)).unwrap();
        tx.try_send(Action::Retry).unwrap();
        tx.try_send(Action::Rescan(true)).unwrap();
        tx.try_send(Action::Settings(mygo_core::Config {
            wheel_zoom: false,
            trash_companions: true,
            multi_view: false,
            images_per_view: 7,
            image_size_limit: 0,
            ..mygo_core::Config::default()
        }))
        .unwrap();
        tx.try_send(Action::Cancel).unwrap();
        assert_eq!(rx.0.0.lock().unwrap().queue.len(), 1);
        tx.try_send(Action::Settings(mygo_core::Config {
            wheel_zoom: false,
            trash_companions: true,
            multi_view: false,
            images_per_view: 24,
            image_size_limit: 0,
            ..mygo_core::Config::default()
        }))
        .unwrap();
        tx.try_send(Action::Shutdown).unwrap();
        assert!(
            tx.try_send(Action::Settings(mygo_core::Config {
                wheel_zoom: false,
                trash_companions: true,
                multi_view: false,
                images_per_view: 6,
                image_size_limit: 0,
                ..mygo_core::Config::default()
            }))
            .is_err()
        );
        assert!(matches!(
            rx.take_latest_settings(),
            Some(Action::Settings(config)) if config.images_per_view == 24
        ));
    }
    use super::*;
    #[test]
    fn waker_rearms_remaining_events_and_stops_when_drained() {
        let (tx, rx) = events();
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let count = calls.clone();
        tx.send(Event::Count(1)).unwrap();
        rx.set_waker(Arc::new(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        tx.send(Event::Status("ready".into())).unwrap();
        rx.wake_pending();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        rx.wake_pending();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
    }
    #[test]
    fn main_upload_precedes_thumbnails_after_selection_is_applied() {
        let (tx, rx) = events();
        tx.send(Event::Thumbnail {
            _permit: None,
            generation: 1,
            index: 0,
            id: 10,
            frame: None,
        })
        .unwrap();
        tx.send(Event::Selection {
            generation: 2,
            id: 20,
            index: 1,
        })
        .unwrap();
        tx.send(Event::Main {
            _permit: None,
            generation: 2,
            request: 1,
            key: "test".into(),
            enqueued: std::time::Instant::now(),
            frame: mygo_decoder::Frame {
                width: 1,
                height: 1,
                original_width: 1,
                original_height: 1,
                orientation: 1,
                rgba: Arc::new(vec![1, 2, 3, 255]),
            },
        })
        .unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::Selection { generation: 2, .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::Main { generation: 2, .. })
        ));
        assert!(matches!(rx.try_recv(), Ok(Event::Thumbnail { .. })));
    }
    #[test]
    fn saturation_keeps_control_and_accumulates_relative_navigation() {
        let (tx, mut rx) = commands();
        for _ in 0..10000 {
            tx.try_send(Action::Navigate(1)).unwrap();
        }
        for _ in 0..63 {
            tx.try_send(Action::Retry).unwrap();
        }
        assert!(tx.try_send(Action::Trash(42)).is_err());
        tx.try_send(Action::Cancel).unwrap();
        tx.try_send(Action::Shutdown).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(matches!(rx.recv().await, Some(Action::Shutdown)));
            assert!(matches!(rx.recv().await, Some(Action::Cancel)));
        });
        let (tx, mut rx) = commands();
        tx.try_send(Action::Navigate(5)).unwrap();
        tx.try_send(Action::Navigate(7)).unwrap();
        tx.try_send(Action::Navigate(-1)).unwrap();
        rt.block_on(async {
            assert!(matches!(rx.recv().await, Some(Action::Navigate(12))));
            assert!(matches!(rx.recv().await, Some(Action::Navigate(-1))));
        });
    }
    #[test]
    fn slow_ui_does_not_block_controller_or_grow_queue() {
        let (tx, rx) = events();
        for i in 0..10000 {
            tx.send(Event::Status(i.to_string())).unwrap();
            tx.send(Event::Error(i.to_string())).unwrap();
        }
        assert!(tx.0.0.lock().unwrap().queue.len() <= 17);
        assert!(matches!(rx.try_recv(),Ok(Event::Error(s)) if s.contains("已合併")));
    }
}
