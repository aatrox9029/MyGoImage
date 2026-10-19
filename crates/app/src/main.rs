#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod background;
mod controller;
mod exif_text;
#[cfg(test)]
mod format_dialog_tests;
mod image_info;
mod info_clipboard;
mod localization;
mod native_input;
mod notice;
mod pixel_budget;
mod png_metadata;
mod preload_window;
mod quick_copy;
mod shortcuts;
mod transport;
mod trash;
use controller::{Action, Event};
use slint::{ComponentHandle, Model, ModelNotify, ModelRc};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};
slint::include_modules!();
type UiPump = Rc<RefCell<dyn FnMut()>>;
struct RenderTrace {
    request: u64,
    generation: u64,
    key: String,
    applied: Instant,
    enqueued: Instant,
}
thread_local! { static UI_PUMP: RefCell<Option<UiPump>> = RefCell::new(None); }

#[derive(Default)]
struct PagedModel {
    count: std::cell::Cell<usize>,
    start: std::cell::Cell<usize>,
    rows: RefCell<BTreeMap<usize, ThumbnailRow>>,
    notify: ModelNotify,
    pixel_bytes: RefCell<BTreeMap<usize, usize>>,
}
impl Model for PagedModel {
    type Data = ThumbnailRow;
    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }
    fn row_data(&self, row: usize) -> Option<ThumbnailRow> {
        if row >= self.row_count() {
            None
        } else {
            Some(
                self.rows
                    .borrow()
                    .get(&(self.start.get() + row))
                    .cloned()
                    .unwrap_or_else(|| ThumbnailRow {
                        name: "…".into(),
                        ..Default::default()
                    }),
            )
        }
    }
    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        &self.notify
    }
}
impl PagedModel {
    fn count(&self, n: usize) {
        self.count.set(n);
    }
    fn page(&self, start: usize, entries: Vec<(i64, String)>) {
        let mut rows = self.rows.borrow_mut();
        self.start.set(start);
        rows.retain(|k, _| *k >= start && *k < start + entries.len());
        for (i, (id, name)) in entries.into_iter().enumerate() {
            let r = rows.entry(start + i).or_default();
            if r.id != id.to_string().as_str() {
                self.pixel_bytes.borrow_mut().remove(&(start + i));
                *r = ThumbnailRow {
                    id: id.to_string().into(),
                    name: name.into(),
                    ..Default::default()
                };
            }
        }
        self.pixel_bytes
            .borrow_mut()
            .retain(|index, _| rows.contains_key(index));
        drop(rows);
        // Only this small page has delegates; the ScrollView owns total geometry.
        self.notify.reset();
    }
    fn thumbnail(&self, index: usize, id: i64, frame: Option<mygo_decoder::Frame>) {
        let mut rows = self.rows.borrow_mut();
        if let Some(r) = rows.get_mut(&index) {
            if r.id != id.to_string().as_str() {
                return;
            }
            r.failed = frame.is_none();
            if let Some(f) = frame {
                self.pixel_bytes.borrow_mut().insert(index, f.bytes());
                r.thumbnail = to_image(&f);
            }
            drop(rows);
            self.notify.row_changed(index - self.start.get());
        }
    }
    fn clear(&self) {
        self.rows.borrow_mut().clear();
        self.pixel_bytes.borrow_mut().clear();
        self.count.set(0);
        self.start.set(0);
        self.notify.reset();
    }
    /// Refuse distant prefetch copies before allocating Slint pixels. Visible
    /// rows always win; closer lookahead may displace more distant copies.
    fn admit_thumbnail(
        &self,
        index: usize,
        id: i64,
        bytes: usize,
        first: usize,
        count: usize,
        budget: usize,
    ) -> bool {
        if self
            .rows
            .borrow()
            .get(&index)
            .is_none_or(|row| row.id != id.to_string().as_str())
        {
            return false;
        }
        self.trim_images(first, count, budget);
        let end = first.saturating_add(count);
        if index >= first && index < end {
            return true;
        }
        if bytes > budget {
            return false;
        }
        let distance = |i: usize| if i < first { first - i } else { i - end + 1 };
        let pixels = self.pixel_bytes.borrow();
        let mut extra: Vec<_> = pixels
            .iter()
            .filter(|(i, _)| **i != index && (**i < first || **i >= end))
            .map(|(&i, &size)| (i, size))
            .collect();
        let mut needed = bytes + extra.iter().map(|(_, size)| size).sum::<usize>();
        extra.sort_by_key(|(i, _)| std::cmp::Reverse(distance(*i)));
        let mut victims = Vec::new();
        for (i, size) in extra {
            if needed <= budget {
                break;
            }
            if distance(i) < distance(index) {
                break;
            }
            victims.push(i);
            needed -= size;
        }
        drop(pixels);
        if needed > budget {
            return false;
        }
        let mut rows = self.rows.borrow_mut();
        let mut pixels = self.pixel_bytes.borrow_mut();
        for &i in &victims {
            if let Some(row) = rows.get_mut(&i) {
                row.thumbnail = Default::default();
                pixels.remove(&i);
            }
        }
        drop(pixels);
        drop(rows);
        for i in victims {
            self.notify.row_changed(i - self.start.get());
        }
        true
    }
    /// Keep visible and partially visible rows intact; bound extra UI copies.
    /// Metadata stays in the page so normal RAM delivery can refill evicted rows.
    fn trim_images(&self, first: usize, count: usize, extra_budget: usize) -> usize {
        let end = first.saturating_add(count);
        let mut rows = self.rows.borrow_mut();
        let mut bytes = self.pixel_bytes.borrow_mut();
        let mut extra: Vec<_> = bytes
            .iter()
            .filter(|(index, _)| **index < first || **index >= end)
            .map(|(&index, &size)| (index, size))
            .collect();
        let mut used: usize = extra.iter().map(|(_, size)| size).sum();
        extra.sort_by_key(|(index, _)| {
            std::cmp::Reverse(if *index < first {
                first - index
            } else {
                index - end + 1
            })
        });
        let mut changed = Vec::new();
        for (index, size) in extra {
            if used <= extra_budget {
                break;
            }
            if let Some(row) = rows.get_mut(&index) {
                row.thumbnail = Default::default();
                bytes.remove(&index);
                used -= size;
                changed.push(index - self.start.get());
            }
        }
        drop(bytes);
        drop(rows);
        for row in changed {
            self.notify.row_changed(row);
        }
        used
    }
}
type CachedImage = (std::sync::Weak<Vec<u8>>, slint::Image);
const UI_REUSE_BYTES: usize = 8 * mygo_core::MIB;
thread_local! { static IMAGES: RefCell<mygo_cache::ByteLru<(usize,u32,u32),CachedImage>> = RefCell::new(mygo_cache::ByteLru::new(UI_REUSE_BYTES)); }
fn thumbnail_bucket(pixels: f32) -> u32 {
    [160, 240, 320, 480, 640, 800, 1000]
        .into_iter()
        .find(|&size| pixels <= size as f32)
        .unwrap_or(1000)
}
/// Grow as needed; require 10% spare room before choosing a smaller bucket.
/// This prevents repeated decodes when window layout oscillates at a boundary.
fn thumbnail_target(pixels: f32, current: u32) -> u32 {
    let next = thumbnail_bucket(pixels);
    if next < current && pixels > next as f32 * 0.9 {
        current
    } else {
        next
    }
}
fn preview_target(pixels: f32, current: u32) -> u32 {
    let next = [640, 800, 1000, 1280, 1600, 2048, 2560, 3200, 4096]
        .into_iter()
        .find(|&size| pixels <= size as f32)
        .unwrap_or(4096);
    if next < current && pixels > next as f32 * 0.9 {
        current
    } else {
        next
    }
}
fn visible_thumbnail_range(window: &MainWindow) -> (usize, usize) {
    let first = window.get_first_visible().max(0) as usize;
    let count = if window.get_multi_view() {
        (window.get_images_per_view() + window.get_grid_columns()).max(1) as usize
    } else {
        window.get_sidebar_visible_count().max(1) as usize
    };
    (first, count)
}
fn to_image(f: &mygo_decoder::Frame) -> slint::Image {
    let key = (std::sync::Arc::as_ptr(&f.rgba) as usize, f.width, f.height);
    if let Some((source, image)) = IMAGES.with(|cache| cache.borrow_mut().get(&key))
        && source.upgrade().is_some()
    {
        return image;
    }
    let image = make_image(f);
    // Weak identity prevents stale pointer hits without pinning an evicted RAM
    // frame. The cache owns only the Slint pixel copy, not the source pixels.
    IMAGES.with(|cache| {
        cache.borrow_mut().put(
            key,
            (std::sync::Arc::downgrade(&f.rgba), image.clone()),
            f.bytes(),
        )
    });
    image
}
fn make_image(f: &mygo_decoder::Frame) -> slint::Image {
    slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            f.rgba.as_slice(),
            f.width,
            f.height,
        ),
    )
}
#[derive(Clone)]
struct UiCommands {
    inner: transport::CommandSender,
    window: slint::Weak<MainWindow>,
    telemetry: mygo_telemetry::Telemetry,
}
impl UiCommands {
    fn try_send(&self, action: Action) -> Result<(), &'static str> {
        let started = Instant::now();
        let result = self.inner.try_send(action);
        self.telemetry.record(
            "command-enqueue",
            started.elapsed().as_micros(),
            1,
            result.is_ok(),
        );
        if let Err(error) = result
            && let Some(window) = self.window.upgrade()
        {
            window.set_error(error.into());
        }
        result
    }
}
fn main() -> anyhow::Result<()> {
    let startup = Instant::now();
    let entered_utc_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_micros() as u64;
    let args: Vec<_> = std::env::args_os().collect();
    if args
        .get(1)
        .is_some_and(|a| a == "--benchmark-scan" || a == "--benchmark-scan-legacy")
    {
        anyhow::ensure!(args.len() == 4, "Usage: --benchmark-scan ROOT OUTPUT.json");
        return controller::benchmark_scan(
            args[2].clone().into(),
            args[3].clone().into(),
            args[1] == "--benchmark-scan-legacy",
        );
    }
    let telemetry = mygo_telemetry::Telemetry::new(&mygo_core::data_dir())?;
    let controller_log = telemetry.clone();
    telemetry.record(
        "startup-main-to-telemetry",
        startup.elapsed().as_micros(),
        0,
        true,
    );
    telemetry.record("startup-main-entry-utc-us", 0, entered_utc_us, true);
    // Settings, indexing and first-image decoding can overlap backend creation.
    // No UI objects are accessed by the controller; its bounded transport holds
    // early results until the UI pump is installed below.
    let (commands, rx) = transport::commands();
    let (events, out) = transport::events();
    let controller_handle = std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || controller::run(rx, events, controller_log))?;
    if let Some(path) = args.get(1) {
        let _ = commands.try_send(Action::Open(path.into(), false));
    }
    let ui_started = Instant::now();
    let app = MainWindow::new()?;
    localization::install(&app);
    telemetry.record(
        "startup-ui-create",
        ui_started.elapsed().as_micros(),
        0,
        true,
    );
    let first_present: Rc<RefCell<Option<(u64, i64)>>> = Rc::default();
    let presented = first_present.clone();
    let render_commands = commands.clone();
    let pending_render: Rc<RefCell<Option<RenderTrace>>> = Rc::default();
    let render_log = telemetry.clone();
    let render_pending = pending_render.clone();
    let mut render_started = None;
    let mut first_render = true;
    let notifier = app
        .window()
        .set_rendering_notifier(move |state, _| match state {
            slint::RenderingState::RenderingSetup => {
                render_log.record(
                    "startup-renderer-ready",
                    startup.elapsed().as_micros(),
                    0,
                    true,
                );
            }
            slint::RenderingState::BeforeRendering => render_started = Some(Instant::now()),
            slint::RenderingState::AfterRendering => {
                if first_render {
                    first_render = false;
                    render_log.record(
                        "startup-first-after-render",
                        startup.elapsed().as_micros(),
                        0,
                        true,
                    );
                }
                if let Some((list, id)) = presented.borrow_mut().take() {
                    let _ = render_commands.try_send(Action::ImageShown(list, id));
                }
                if let Some(started) = render_started.take() {
                    render_log.record("render-callback", started.elapsed().as_micros(), 0, true);
                }
                if let Some(trace) = render_pending.borrow_mut().take() {
                    render_log.trace(
                        "ui-apply-to-after-render",
                        trace.request,
                        trace.generation,
                        &trace.key,
                        trace.applied.elapsed().as_micros(),
                        true,
                    );
                    render_log.trace(
                        "request-to-after-render",
                        trace.request,
                        trace.generation,
                        &trace.key,
                        trace.enqueued.elapsed().as_micros(),
                        true,
                    );
                }
            }
            _ => {}
        });
    telemetry.record("render-notifier-supported", 0, 0, notifier.is_ok());
    let render_notifier_supported = notifier.is_ok();
    let model = Rc::new(PagedModel::default());
    app.set_rows(ModelRc::from(model.clone()));
    let tx = UiCommands {
        inner: commands,
        window: app.as_weak(),
        telemetry: telemetry.clone(),
    };
    native_input::install(&app, tx.clone());
    {
        let t = tx.clone();
        let w = app.as_weak();
        app.on_load_info(move || {
            if let Some(w) = w.upgrade() {
                let _ = t.try_send(Action::Details(w.get_info_visible()));
            }
        });
    }
    info_clipboard::install(&app, telemetry.clone());
    {
        let t = tx.clone();
        app.on_open_folder(move || {
            let _ = t.try_send(Action::Picker(false));
        });
    }
    {
        let t = tx.clone();
        app.on_open_file(move || {
            let _ = t.try_send(Action::Picker(true));
        });
    }
    {
        let t = tx.clone();
        let w = app.as_weak();
        app.on_rescan(move || {
            if let Some(w) = w.upgrade() {
                let _ = t.try_send(Action::Rescan(w.get_recursive()));
            }
        });
    }
    {
        let t = tx.clone();
        app.on_sorting(move |s, d| {
            let _ = t.try_send(Action::Sort(s, d));
        });
    }
    {
        let t = tx.clone();
        app.on_pick(move |i| {
            let _ = t.try_send(Action::Select(i as usize));
        });
    }
    {
        let t = tx.clone();
        let w = app.as_weak();
        let mut previous: Option<(i32, Instant)> = None;
        app.on_grid_pick(move |index| {
            let double = previous
                .is_some_and(|(i, time)| i == index && time.elapsed() < Duration::from_millis(500));
            previous = if double {
                None
            } else {
                Some((index, Instant::now()))
            };
            let _ = t.try_send(Action::Select(index as usize));
            if double && let Some(w) = w.upgrade() {
                w.set_multi_view(false);
                w.invoke_save_settings();
            }
        });
    }
    {
        let t = tx.clone();
        app.on_navigate(move |d| {
            let _ = t.try_send(Action::Navigate(d));
        });
    }
    {
        let t = tx.clone();
        let w = app.as_weak();
        app.on_turn_page(move |direction| {
            if let Some(w) = w.upgrade()
                && let Some(index) = next_page(
                    w.get_grid_offset(),
                    w.get_grid_view_height(),
                    w.get_images_per_view(),
                    w.get_total_count(),
                    direction,
                )
            {
                w.invoke_show_grid_page(index);
                let _ = t.try_send(Action::Select(index as usize));
                // A discrete page turn already has a final viewport. Request
                // it now instead of waiting for the 60ms scroll debounce.
                let _ = t.try_send(Action::Visible(index as usize));
            }
        });
    }
    {
        let t = tx.clone();
        app.on_high_resolution(move || {
            let _ = t.try_send(Action::High);
        });
    }
    {
        let t = tx.clone();
        app.on_cancel_load(move || {
            let _ = t.try_send(Action::Cancel);
        });
    }
    {
        let t = tx.clone();
        app.on_retry(move || {
            let _ = t.try_send(Action::Retry);
        });
    }
    {
        let t = tx.clone();
        app.on_clear_cache(move || {
            let _ = t.try_send(Action::Clear);
        });
    }
    {
        let t = tx.clone();
        app.on_reveal_file(move || {
            let _ = t.try_send(Action::RevealFile);
        });
    }
    {
        let t = tx.clone();
        let w = app.as_weak();
        app.on_trash_current(move || {
            if let Some(w) = w.upgrade()
                && let Ok(id) = w.get_selected_id().parse()
            {
                let _ = t.try_send(Action::Trash(id));
            }
        });
    }
    {
        let t = tx.clone();
        app.on_open_trash(move || {
            let _ = t.try_send(Action::OpenTrash);
        });
    }
    let settings_snapshot = Rc::new(RefCell::new(mygo_core::Config::default()));
    {
        let t = tx.clone();
        app.on_copy_current(move || {
            let _ = t.try_send(Action::CopyCurrent);
        });
    }
    {
        let t = tx.clone();
        app.on_choose_copy_folder(move || {
            let _ = t.try_send(Action::PickCopyFolder);
        });
    }
    {
        let t = tx.clone();
        let w = app.as_weak();
        let snapshot = settings_snapshot.clone();
        app.on_save_settings(move || {
            if let Some(w) = w.upgrade() {
                w.set_grid_columns(grid_columns(w.get_images_per_view()));
                let mut config = snapshot.borrow_mut();
                config.language = mygo_core::Language::from_index(w.get_language_index());
                config.image_formats.jpeg = w.get_format_jpeg();
                config.image_formats.png = w.get_format_png();
                config.image_formats.webp = w.get_format_webp();
                config.image_formats.bmp = w.get_format_bmp();
                config.image_formats.tiff = w.get_format_tiff();
                config.image_formats.gif = w.get_format_gif();
                config.image_formats.psd = w.get_format_psd();
                config.wheel_zoom = true;
                config.copy_shortcut = w.get_copy_shortcut().to_string();
                config.trash_companions = w.get_trash_companions();
                config.multi_view = w.get_multi_view();
                config.images_per_view = w.get_images_per_view().max(1) as usize;
                config.image_size_limit = w.get_image_size_limit().max(0) as usize;
                config.preload_enabled = w.get_preload_enabled();
                config.preload_count = w.get_preload_count().clamp(1, 24) as usize;
                config.resume_last_image = w.get_resume_last_image();
                config.copy_folder = std::path::PathBuf::from(w.get_copy_folder().as_str());
                let _ = t.try_send(Action::Settings(config.clone()));
                w.invoke_reveal(w.get_selected_index());
            }
        });
    }
    let timer = slint::Timer::default();
    let w = app.as_weak();
    let t = tx.clone();
    let mut last_page = usize::MAX;
    let mut last_visible_count = 0;
    let mut last_target = 320;
    let mut last_preview_target = controller::INITIAL_PREVIEW;
    let mut last_offset = 0usize;
    let mut last_move = Instant::now();
    let mut list_gen = 0;
    let mut main_gen = 0;
    let mut main_bytes = 0;
    let mut previous_tick = Instant::now();
    let mut first_pump = true;
    let mut last_scale = 0;
    let timer_log = telemetry.clone();
    let mut previous_timer = Instant::now();
    let out = Rc::new(out);
    let receiver = out.clone();
    let pump: UiPump = Rc::new(RefCell::new(move || {
        let Some(w) = w.upgrade() else {
            return;
        };
        let started = Instant::now();
        if first_pump {
            first_pump = false;
            telemetry.record(
                "startup-first-ui-pump",
                startup.elapsed().as_micros(),
                0,
                true,
            );
        }
        let scale = (w.window().scale_factor() * 1000.0).round() as u64;
        if scale != last_scale {
            last_scale = scale;
            telemetry.record("ui-scale-factor-milli", 0, scale, true);
        }
        telemetry.record(
            "ui-pump-interval",
            previous_tick.elapsed().as_micros(),
            0,
            true,
        );
        previous_tick = started;
        let mut uploaded = 0usize;
        for _ in 0..32 {
            if started.elapsed() >= Duration::from_millis(4) || uploaded >= 16 * mygo_core::MIB {
                break;
            }
            let Ok(e) = out.try_recv() else {
                break;
            };
            match e {
                Event::Settings(c, trash) => {
                    *settings_snapshot.borrow_mut() = c.clone();
                    w.set_format_jpeg(c.image_formats.jpeg);
                    w.set_format_png(c.image_formats.png);
                    w.set_format_webp(c.image_formats.webp);
                    w.set_format_bmp(c.image_formats.bmp);
                    w.set_format_tiff(c.image_formats.tiff);
                    w.set_format_gif(c.image_formats.gif);
                    w.set_format_psd(c.image_formats.psd);
                    w.set_language_index(c.language.index());
                    w.global::<I18n>()
                        .set_english(c.language == mygo_core::Language::English);
                    w.set_copy_shortcut(c.copy_shortcut.clone().into());
                    w.set_preload_enabled(c.preload_enabled);
                    w.set_preload_count(c.preload_count as i32);
                    w.set_resume_last_image(c.resume_last_image);
                    w.set_copy_folder(c.copy_folder.to_string_lossy().to_string().into());
                    w.set_image_size_limit(c.image_size_limit as i32);
                    w.set_wheel_zoom(true);
                    w.set_trash_companions(c.trash_companions);
                    w.set_multi_view(c.multi_view);
                    w.set_images_per_view(c.images_per_view as i32);
                    w.set_grid_columns(grid_columns(c.images_per_view as i32));
                    w.set_trash_path(trash.into());
                }
                Event::Reset {
                    generation,
                    folder,
                    wheel,
                } => {
                    list_gen = generation;
                    pending_render.borrow_mut().take();
                    first_present.borrow_mut().take();
                    model.clear();
                    main_bytes = 0;
                    w.set_folder(folder.into());
                    w.set_picture(Default::default());
                    w.set_error("".into());
                    w.set_selected_id("".into());
                    w.set_loading(false);
                    w.invoke_reset_view();
                    w.invoke_reveal(0);
                    let _ = wheel;
                    w.set_wheel_zoom(true);
                    w.set_image_info(Default::default());
                    last_page = usize::MAX;
                }
                Event::Count(n) => {
                    model.count(n);
                    w.set_total_count(n as i32);
                }
                Event::Page {
                    generation,
                    start,
                    entries,
                } => {
                    if generation == list_gen {
                        w.set_page_start(start as i32);
                        model.page(start, entries);
                    }
                }
                Event::Thumbnail {
                    generation,
                    index,
                    id,
                    frame,
                    _permit,
                } => {
                    if generation == list_gen {
                        let (first, count) = visible_thumbnail_range(&w);
                        if let Some(frame) = &frame
                            && !model.admit_thumbnail(
                                index,
                                id,
                                frame.bytes(),
                                first,
                                count,
                                UI_REUSE_BYTES,
                            )
                        {
                            continue;
                        }
                        uploaded += frame.as_ref().map_or(0, |f| f.bytes());
                        let ready =
                            frame.is_some() && w.get_selected_id().as_str() == id.to_string();
                        model.thumbnail(index, id, frame);
                        if ready {
                            if render_notifier_supported {
                                *first_present.borrow_mut() = Some((list_gen, id));
                            } else {
                                let _ = t.try_send(Action::ImageShown(list_gen, id));
                            }
                        }
                    }
                }
                Event::Selection {
                    generation,
                    id,
                    index,
                } => {
                    main_gen = generation;
                    pending_render.borrow_mut().take();
                    w.set_selected_index(index as i32);
                    w.set_selected_id(id.to_string().into());
                    w.set_image_info(Default::default());
                    w.set_loading(true);
                    w.set_error("".into());
                    w.invoke_reset_view();
                    w.invoke_reveal(index as i32);
                }
                Event::Relocate { id, index } => {
                    w.set_selected_index(index as i32);
                    w.set_selected_id(id.to_string().into());
                    w.invoke_reveal(index as i32);
                }
                Event::Main {
                    generation,
                    request,
                    key,
                    enqueued,
                    frame: f,
                    _permit,
                } => {
                    if generation != main_gen {
                        continue;
                    }
                    uploaded += f.bytes();
                    main_bytes = f.bytes();
                    let apply_started = Instant::now();
                    w.set_picture(to_image(&f));
                    if let Ok(id) = w.get_selected_id().parse::<i64>() {
                        if render_notifier_supported {
                            *first_present.borrow_mut() = Some((list_gen, id));
                        } else {
                            let _ = t.try_send(Action::ImageShown(list_gen, id));
                        }
                    }
                    telemetry.trace(
                        "ui-main-buffer-apply",
                        request,
                        generation,
                        &key,
                        apply_started.elapsed().as_micros(),
                        true,
                    );
                    *pending_render.borrow_mut() = Some(RenderTrace {
                        request,
                        generation,
                        key: key.clone(),
                        applied: Instant::now(),
                        enqueued,
                    });
                    telemetry.trace(
                        "ui-main-apply",
                        request,
                        generation,
                        &key,
                        enqueued.elapsed().as_micros(),
                        true,
                    );
                    w.set_original_width(f.original_width as i32);
                    w.set_original_height(f.original_height as i32);
                    w.set_loading(false);
                    w.set_error("".into());
                }
                Event::Details { generation, fields } => {
                    if generation == main_gen {
                        w.set_image_info(slint::ModelRc::new(slint::VecModel::from(
                            fields
                                .into_iter()
                                .map(|f| InfoField {
                                    name: f.name.into(),
                                    value: f.value.into(),
                                })
                                .collect::<Vec<_>>(),
                        )));
                    }
                }
                Event::Notice(s) => notice::show(&w, &s),
                Event::Status(s) => {
                    // Saving preferences must not hide the current image's size/date.
                    if s != "設定已儲存" {
                        w.set_status(s.into());
                    }
                }
                Event::Error(s) => {
                    w.set_error(s.into());
                    w.set_loading(false);
                }
                Event::Idle => w.set_loading(false),
                Event::Probe(_) | Event::ScanComplete(_) => {}
            }
        }
        let (first, visible_rows) = visible_thumbnail_range(&w);
        let extra_bytes = model.trim_images(first, visible_rows, UI_REUSE_BYTES);
        telemetry.record("ui-offscreen-pixel-bytes", 0, extra_bytes as u64, true);
        telemetry.record(
            "ui-reuse-buffer-bytes",
            0,
            IMAGES.with(|cache| cache.borrow().used()) as u64,
            true,
        );
        telemetry.record(
            "ui-pixel-buffer-bytes",
            0,
            (main_bytes + model.pixel_bytes.borrow().values().sum::<usize>()) as u64,
            true,
        );
        telemetry.record(
            "ui-event-pump",
            started.elapsed().as_micros(),
            uploaded as u64,
            true,
        );
        let offset = w.get_first_visible().max(0) as usize;
        let visible_count = w.get_sidebar_visible_count().max(1) as usize;
        if visible_count != last_visible_count {
            last_visible_count = visible_count;
            let _ = t.try_send(Action::VisibleCount(visible_count));
        }
        w.set_current_grid_page(
            (w.get_grid_offset().max(0.0) / w.get_grid_view_height().max(1.0) + 0.000001).floor()
                as i32
                + 1,
        );
        if offset != last_offset {
            last_offset = offset;
            last_move = Instant::now();
        }
        let columns = if w.get_multi_view() {
            w.get_grid_columns() as usize
        } else {
            1
        };
        let target = thumbnail_target(
            w.get_grid_card_pixels() * w.window().scale_factor(),
            last_target,
        );
        let preview_pixels = w.get_preview_pixels() * w.window().scale_factor();
        if !w.get_multi_view() && preview_pixels > 0.0 {
            let preview = preview_target(preview_pixels, last_preview_target);
            if preview != last_preview_target && t.try_send(Action::PreviewSize(preview)).is_ok() {
                last_preview_target = preview;
            }
        }
        if w.get_multi_view()
            && target != last_target
            && t.try_send(Action::ThumbnailSize(target)).is_ok()
        {
            last_target = target;
        }
        let start = (offset / columns) * columns;
        if start != last_page
            && last_move.elapsed() >= Duration::from_millis(60)
            && t.try_send(Action::Visible(start)).is_ok()
        {
            last_page = start;
        }
        out.wake_pending();
    }));
    UI_PUMP.with(|slot| *slot.borrow_mut() = Some(pump.clone()));
    let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    receiver.set_waker(std::sync::Arc::new(move || {
        use std::sync::atomic::Ordering;
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = pending.clone();
        if slint::invoke_from_event_loop(move || {
            flag.store(false, Ordering::Release);
            UI_PUMP.with(|slot| {
                let pump = slot.borrow().clone();
                if let Some(pump) = pump {
                    (pump.borrow_mut())();
                }
            });
        })
        .is_err()
        {
            pending.store(false, Ordering::Release);
        }
    }));
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            timer_log.record(
                "ui-timer-interval",
                previous_timer.elapsed().as_micros(),
                0,
                true,
            );
            previous_timer = Instant::now();
            (pump.borrow_mut())();
        },
    );
    // Apply any already decoded first image before run() initializes rendering,
    // instead of leaving it behind the first (potentially slow) empty draw.
    UI_PUMP.with(|slot| {
        if let Some(pump) = slot.borrow().clone() {
            (pump.borrow_mut())();
        }
    });
    let result = app.run();
    UI_PUMP.with(|slot| slot.borrow_mut().take());
    let _ = tx.try_send(Action::Shutdown);
    finish_controller(controller_handle);
    result?;
    Ok(())
}

fn finish_controller(controller_handle: std::thread::JoinHandle<()>) {
    let deadline = Instant::now() + Duration::from_millis(800);
    while !controller_handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    if controller_handle.is_finished() {
        let _ = controller_handle.join();
    }
}

fn grid_columns(count: i32) -> i32 {
    let count = count.clamp(1, 24);
    (1..=count)
        .filter(|n| n * n >= count && count % n == 0)
        .min()
        .unwrap_or(1)
}

fn next_page(offset: f32, height: f32, count: i32, total: i32, direction: i32) -> Option<i32> {
    if count <= 0 || total <= 0 || height <= 0.0 {
        return None;
    }
    let position = offset.max(0.0) / height;
    let page = if direction > 0 {
        (position + 0.000001).floor() as i32 + 1
    } else {
        (position - 0.000001).ceil() as i32 - 1
    };
    Some(page.clamp(0, (total - 1) / count) * count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_pixels_cover_viewport_and_keep_resize_hysteresis() {
        assert_eq!(
            preview_target(1002.0, controller::INITIAL_PREVIEW),
            controller::INITIAL_PREVIEW
        );
        assert_eq!(preview_target(950.0, 1000), 1000);
        assert_eq!(preview_target(1001.0, 1000), 1280);
        assert_eq!(preview_target(1800.0, 1000), 2048);
        assert_eq!(preview_target(999.0, 1280), 1280);
        assert_eq!(preview_target(880.0, 1280), 1000);
        assert_eq!(preview_target(8000.0, 1000), 4096);
    }
    #[test]
    fn ui_admission_skips_distant_copies_and_visible_rows_displace_prefetch() {
        IMAGES.with(|cache| cache.borrow_mut().clear());
        let model = PagedModel::default();
        model.page(0, (0..10).map(|id| (id, format!("{id}.png"))).collect());
        let frame = || mygo_decoder::Frame {
            width: 2,
            height: 2,
            original_width: 2,
            original_height: 2,
            orientation: 1,
            rgba: std::sync::Arc::new(vec![255; 16]),
        };
        for index in [2, 3, 4] {
            assert!(model.admit_thumbnail(index, index as i64, 16, 2, 2, 16));
            model.thumbnail(index, index as i64, Some(frame()));
        }
        let copied = IMAGES.with(|cache| cache.borrow().used());
        if model.admit_thumbnail(9, 9, 16, 2, 2, 16) {
            model.thumbnail(9, 9, Some(frame()));
        }
        assert!(!model.pixel_bytes.borrow().contains_key(&9));
        assert_eq!(IMAGES.with(|cache| cache.borrow().used()), copied);
        assert!(model.admit_thumbnail(1, 1, 16, 2, 2, 16));
        model.thumbnail(1, 1, Some(frame()));
        assert!(!model.pixel_bytes.borrow().contains_key(&4));
        assert!(!model.admit_thumbnail(7, 99, 16, 7, 1, 0));
        assert!(model.admit_thumbnail(7, 7, 16, 7, 1, 0));
        model.thumbnail(7, 7, Some(frame()));
        assert_eq!(model.pixel_bytes.borrow().len(), 1);
        assert_eq!(model.row_data(7).unwrap().thumbnail.size().width, 2);
        IMAGES.with(|cache| cache.borrow_mut().clear());
    }
    #[test]
    fn ui_trim_keeps_visible_pixels_and_row_identity_without_pinning_source() {
        IMAGES.with(|cache| cache.borrow_mut().clear());
        let model = PagedModel::default();
        model.page(0, (0..8).map(|id| (id, format!("{id}.png"))).collect());
        let pixels = std::sync::Arc::new(vec![255; 16]);
        let weak = std::sync::Arc::downgrade(&pixels);
        for index in 0..8 {
            model.thumbnail(
                index,
                index as i64,
                Some(mygo_decoder::Frame {
                    width: 2,
                    height: 2,
                    original_width: 2,
                    original_height: 2,
                    orientation: 1,
                    rgba: pixels.clone(),
                }),
            );
        }
        assert_eq!(model.trim_images(2, 2, 16), 16);
        assert_eq!(model.pixel_bytes.borrow().values().sum::<usize>(), 48);
        for index in [2, 3] {
            assert_eq!(model.row_data(index).unwrap().thumbnail.size().width, 2);
        }
        assert_eq!(model.row_count(), 8);
        assert_eq!(model.row_data(0).unwrap().id.as_str(), "0");
        drop(pixels);
        assert!(
            weak.upgrade().is_none(),
            "UI reuse cache pinned the decoder allocation"
        );
        IMAGES.with(|cache| cache.borrow_mut().clear());
    }
    #[test]
    fn thumbnail_sizes_cover_physical_pixels_and_resist_boundary_jitter() {
        for (pixels, size) in [
            (128.0, 160),
            (224.0, 240),
            (300.0, 320),
            (400.0, 480),
            (600.0, 640),
            (700.0, 800),
            (900.0, 1000),
            (2000.0, 1000),
        ] {
            assert_eq!(thumbnail_bucket(pixels), size);
        }
        assert_eq!(thumbnail_target(239.0, 320), 320);
        assert_eq!(thumbnail_target(215.0, 320), 240);
        assert_eq!(thumbnail_target(241.0, 240), 320);
        assert_eq!(thumbnail_target(600.0, 320), 640); // 300 logical px at 2x DPI
    }
    #[test]
    fn paging_uses_viewport_top_without_skipping_a_partly_visible_page() {
        for offset in [0.0, 1.0, 120.0, 599.0] {
            assert_eq!(next_page(offset, 600.0, 3, 20, 1), Some(3));
        }
        assert_eq!(next_page(600.0, 600.0, 3, 20, 1), Some(6));
        assert_eq!(next_page(601.0, 600.0, 3, 20, 1), Some(6));
        assert_eq!(next_page(3600.0, 600.0, 3, 20, 1), Some(18));
        assert_eq!(next_page(601.0, 600.0, 3, 20, -1), Some(3));
        assert_eq!(next_page(600.0, 600.0, 3, 20, -1), Some(0));
        assert_eq!(next_page(0.0, 600.0, 3, 0, 1), None);
    }
    #[test]
    fn hundred_thousand_ui_rows_store_only_a_page() {
        let m = PagedModel::default();
        m.count(100000);
        for start in [0, 50000, 99968] {
            m.page(
                start,
                (start..start + 32)
                    .map(|i| (i as i64, i.to_string()))
                    .collect(),
            );
            assert_eq!(m.rows.borrow().len(), 32);
            assert_eq!(m.count.get(), 100000);
            assert_eq!(m.row_count(), 32);
            assert!(m.row_data(31).is_some());
        }
        assert!(m.row_data(32).is_none());
    }
}
