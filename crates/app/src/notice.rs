use slint::ComponentHandle;
use std::cell::Cell;

thread_local! { static SERIAL: Cell<u64> = const { Cell::new(0) }; }

pub fn show(window: &crate::MainWindow, message: &str) {
    let serial = SERIAL.with(|s| {
        s.set(s.get().wrapping_add(1));
        s.get()
    });
    window.set_notice(message.into());
    let weak = window.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
        if SERIAL.with(|s| s.get()) == serial
            && let Some(window) = weak.upgrade()
        {
            window.set_notice("".into());
        }
    });
}
