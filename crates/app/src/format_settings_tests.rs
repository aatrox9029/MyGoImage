#[test]
fn format_changes_rescan_cached_folders_and_psd_opens_without_ui() {
    let dir = tempfile::tempdir().unwrap();
    let photos = photo_folder(dir.path(), "photos");
    image::RgbImage::from_pixel(2, 1, image::Rgb([10, 20, 30]))
        .save(photos.join("b.JPEG")).unwrap();
    let mut psd = b"8BPS".to_vec();
    psd.extend(1u16.to_be_bytes()); psd.extend([0; 6]); psd.extend(3u16.to_be_bytes());
    psd.extend(1u32.to_be_bytes()); psd.extend(2u32.to_be_bytes());
    psd.extend(8u16.to_be_bytes()); psd.extend(3u16.to_be_bytes());
    psd.extend([0; 14]); psd.extend([10, 20, 30, 40, 50, 60]);
    std::fs::write(photos.join("c.PSD"), psd).unwrap();
    let base = dir.path().join("data");
    let mut session = Session::new(base.clone(), BackgroundIo::default());
    session.send(Action::Open(photos.clone(), false));
    session.wait(|e| matches!(e, Event::ScanComplete(3)));
    let mut config = Config::default();
    config.image_formats.jpeg = false;
    config.image_formats.png = false;
    session.send(Action::Settings(config.clone()));
    session.wait(|e| matches!(e, Event::ScanComplete(1)));
    session.wait(|e| matches!(e, Event::Main { frame, .. } if frame.original_width == 2 && frame.original_height == 1));
    config.image_formats.psd = false;
    session.send(Action::Settings(config));
    session.wait(|e| matches!(e, Event::ScanComplete(0)));
    session.send(Action::Open(photos.join("c.PSD"), false));
    session.wait(|e| matches!(e, Event::Error(message) if message.contains("格式未啟用")));
    session.send(Action::Settings(Config::default()));
    session.wait(|e| matches!(e, Event::ScanComplete(3)));
    session.send(Action::Open(photos.join("c.PSD"), false));
    session.wait(|e| matches!(e, Event::Main { frame, .. } if frame.original_width == 2));
    session.shutdown();
    assert_eq!(Config::load(&base).unwrap().image_formats, mygo_core::ImageFormats::default());
}
