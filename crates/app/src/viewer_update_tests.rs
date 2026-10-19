#[test]
fn bidirectional_preview_budget_accounts_for_both_sides_and_obeys_cap() {
    let config = Config { preload_count: 10, ..Config::default() };
    assert_eq!(preview_cache_budget(&config, 1280), 18 * 1280 * 1280 * 4);
    assert_eq!(preview_cache_budget(&Config { main_mib: 1, ..config }, 4096), MIB);
}

#[test]
fn live_preload_applies_at_fiftieth_image_without_reselection() {
    let dir = tempfile::tempdir().unwrap();
    let photos = dir.path().join("photos");
    std::fs::create_dir(&photos).unwrap();
    for index in 0..65 {
        image::RgbImage::from_pixel(80, 40, image::Rgb([index, 20, 30]))
            .save(photos.join(format!("{index:02}.png"))).unwrap();
    }
    let base = dir.path().join("data");
    let config = Config { preload_enabled: false, ..Config::default() };
    config.save(&base).unwrap();
    let mut session = Session::new(base.clone(), BackgroundIo::default());
    session.send(Action::Open(photos, false));
    session.wait(|e| matches!(e, Event::ScanComplete(65)));
    session.send(Action::Select(49));
    session.wait(|e| matches!(e, Event::Main { frame, .. } if frame.rgba[0] == 49));
    let count = || std::fs::read_to_string(base.join("diagnostics.jsonl")).unwrap_or_default()
        .lines().filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .filter(|r| r["event"] == "main-decode" && r["ok"] == true).count();
    std::thread::sleep(Duration::from_millis(150));
    let baseline = count();
    assert!(baseline >= 1);
    session.send(Action::Settings(Config { preload_enabled: true, preload_count: 10, ..config.clone() }));
    session.probe(901);
    let deadline = Instant::now() + Duration::from_secs(10);
    while count() < baseline + 15 {
        assert!(Instant::now() < deadline, "live preloading did not start");
        if let Ok(event) = session.events.recv_timeout(Duration::from_millis(10)) {
            assert!(!matches!(event, Event::Selection { .. } | Event::Main { .. }));
        }
    }
    // Freeze preloading so navigation cannot populate a missing test image.
    session.send(Action::Settings(Config { preload_enabled: false, preload_count: 10, ..config }));
    session.probe(903);
    // Current image 50, preceding 45–49 and following 51–60 must all hit RAM.
    for index in 44..60 {
        session.send(Action::Select(index));
        session.wait(|e| matches!(e, Event::Main { frame, .. } if frame.rgba[0] as usize == index));
    }
    session.shutdown();
    let ram_hits = std::fs::read_to_string(base.join("diagnostics.jsonl")).unwrap()
        .lines().filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .filter(|r| r["event"] == "ram-dispatch").count();
    assert!(ram_hits >= 16, "preloaded images were not retained in RAM");
    assert_eq!(count(), baseline + 15, "navigation decoded an image outside the populated RAM window");
    let saved = Config::load(&base).unwrap();
    assert!(!saved.preload_enabled);
    assert_eq!(saved.preload_count, 10);
}

#[test]
fn copy_missing_folder_and_duplicate_are_not_persistent_errors() {
    let dir = tempfile::tempdir().unwrap();
    let photos = photo_folder(dir.path(), "photos");
    let base = dir.path().join("data");
    let destination = dir.path().join("edit");
    let mut session = Session::new(base.clone(), BackgroundIo::default());
    session.send(Action::Open(photos.clone(), false));
    session.wait(|e| matches!(e, Event::Main { .. }));
    session.send(Action::CopyCurrent);
    session.wait(|e| matches!(e, Event::Notice(s) if s.contains("選擇複製目的地")));
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("a.png"), b"existing").unwrap();
    session.send(Action::Settings(Config { copy_folder: destination.clone(), copy_shortcut: "Ctrl+Shift+C".into(), ..Config::default() }));
    session.send(Action::CopyCurrent);
    session.wait(|e| matches!(e, Event::Notice(s) if s.contains("已擁有")));
    session.shutdown();
    assert_eq!(std::fs::read(destination.join("a.png")).unwrap(), b"existing");
    assert_eq!(std::fs::read_dir(destination).unwrap().count(), 1);
    assert_eq!(Config::load(&base).unwrap().copy_shortcut, "Ctrl+Shift+C");
}

#[test]
fn details_follow_selection_and_are_optional() {
    let dir = tempfile::tempdir().unwrap();
    let photos = photo_folder(dir.path(), "photos");
    for name in ["b.png", "c.png"] {
        std::fs::copy(photos.join("a.png"), photos.join(name)).unwrap();
    }
    let mut session = Session::new(dir.path().join("data"), BackgroundIo::default());
    session.send(Action::Open(photos, false));
    session.wait(|e| matches!(e, Event::Main { .. }));
    session.send(Action::Details(true));
    session.wait(|e| matches!(e, Event::Details { fields, .. } if fields.iter().any(|f| f.value == "a.png")));
    session.send(Action::Select(1));
    session.wait(|e| matches!(e, Event::Details { fields, .. } if fields.iter().any(|f| f.value == "b.png")));
    session.send(Action::Details(false));
    session.send(Action::Select(2));
    session.wait(|e| matches!(e, Event::Main { .. }));
    session.probe(902);
    session.shutdown();
}
