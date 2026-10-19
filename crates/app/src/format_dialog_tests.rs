//! Exercise the real settings UI without creating an operating-system window.
use super::*;
use slint::platform::{
    Platform, PlatformError, WindowAdapter, WindowEvent,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};

struct HeadlessPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn format_dialog_renders_and_escape_preserves_settings() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(HeadlessPlatform(adapter.clone()))).unwrap();
    let ui = MainWindow::new().unwrap();
    localization::install(&ui);
    let saves = Rc::new(std::cell::Cell::new(0));
    let saved = saves.clone();
    ui.on_save_settings(move || saved.set(saved.get() + 1));
    ui.set_settings_open(true);
    ui.show().unwrap();
    let output =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-output/format-dialog-ui");
    std::fs::create_dir_all(&output).unwrap();
    for (width, height, english) in [(900, 600, false), (900, 600, true), (1280, 800, true)] {
        adapter.set_size(slint::PhysicalSize::new(width, height));
        ui.global::<I18n>().set_english(english);
        ui.set_formats_open(true);
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
        adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        });
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        image::save_buffer(
            output.join(format!("{width}-{height}-{english}.png")),
            &bytes,
            width,
            height,
            image::ColorType::Rgb8,
        )
        .unwrap();
        let click = |x, y| {
            let position = slint::LogicalPosition::new(x, y);
            adapter.dispatch_event(WindowEvent::PointerMoved { position });
            adapter.dispatch_event(WindowEvent::PointerPressed {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
            adapter.dispatch_event(WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        };
        let left = (width as f32 - 520.0) / 2.0;
        let top = (height as f32 - 460.0) / 2.0;
        let before = saves.get();
        click(left + 33.0, top + 90.0);
        assert!(!ui.get_format_jpeg());
        assert_eq!(saves.get(), before + 1);
        click(left + 148.0, top + 420.0);
        assert!(!ui.get_format_png() && !ui.get_format_psd());
        click(left + 60.0, top + 420.0);
        assert!(ui.get_format_jpeg() && ui.get_format_png() && ui.get_format_psd());
        assert_eq!(saves.get(), before + 3);
        click(left + 465.0, top + 420.0);
        assert!(!ui.get_formats_open());
        assert!(ui.get_settings_open());
        click(100.0, 164.0);
        assert!(
            ui.get_formats_open(),
            "Settings button must reopen the dialog"
        );
        adapter.dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Escape.into(),
        });
        adapter.dispatch_event(WindowEvent::KeyReleased {
            text: slint::platform::Key::Escape.into(),
        });
        assert!(
            !ui.get_formats_open(),
            "Escape must close the format dialog"
        );
        assert!(
            ui.get_settings_open(),
            "Escape must keep the settings page open"
        );
        assert!(ui.get_format_jpeg() && ui.get_format_psd());
    }
    ui.hide().unwrap();
}
