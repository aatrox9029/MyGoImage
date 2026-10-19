//! Clipboard gets the original value directly, without labels or display formatting.
use slint::ComponentHandle;

pub fn install(window: &crate::MainWindow, telemetry: mygo_telemetry::Telemetry) {
    let weak = window.as_weak();
    window.on_copy_info(move |value| {
        let started = std::time::Instant::now();
        let result = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(value.to_string()));
        telemetry.record(
            "image-info-copy",
            started.elapsed().as_micros(),
            value.len() as u64,
            result.is_ok(),
        );
        if let Some(window) = weak.upgrade() {
            match result {
                Ok(()) => crate::notice::show(&window, "已複製欄位內容"),
                Err(error) => {
                    telemetry.trace("image-info-copy-error", 0, 0, &error.to_string(), 0, false);
                    crate::notice::show(&window, "複製失敗，請再試一次");
                }
            }
        }
    });
}
