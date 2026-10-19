use super::*;

#[test]
fn png_layouts_match_independent_pipeline_and_cancel_per_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/png");
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let config = Config::default();
        let mut source = SharedSource::open(&path, &config, &Cancel::default()).unwrap();
        for target in [400, 320, 160, 0] {
            let expected = decode(&path, target, &config, &Cancel::default()).unwrap();
            let actual = source.render(target, &Cancel::default()).unwrap();
            assert_eq!(
                (actual.width, actual.height),
                (expected.width, expected.height),
                "{path:?}"
            );
            assert_eq!(actual.rgba, expected.rgba, "{path:?}, target={target}");
            assert_eq!(actual.orientation, expected.orientation);
            assert_eq!(
                (actual.original_width, actual.original_height),
                (expected.original_width, expected.original_height)
            );
        }
        let cancelled = Cancel::default();
        cancelled.cancel();
        assert!(source.render(320, &cancelled).is_err());
        assert!(source.render(160, &Cancel::default()).is_ok());
        assert_eq!(source.timings().codec_calls, 1);
        assert_eq!(source.timings().source_opens, 1);
    }
}

#[test]
fn shared_png_icc_and_all_exif_orientations_match() {
    use image::ImageEncoder;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.png");
    for orientation in 1..=8u8 {
        let image = image::RgbaImage::from_fn(417, 259, |x, y| {
            image::Rgba([x as u8, y as u8, (x + y) as u8, (x * y) as u8])
        });
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder
            .set_icc_profile(moxcms::ColorProfile::new_display_p3().encode().unwrap())
            .unwrap();
        let mut exif = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
        exif.extend([orientation, 0, 0, 0, 0, 0, 0, 0]);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(&image, 417, 259, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let config = Config::default();
        let mut source = SharedSource::open(&path, &config, &Cancel::default()).unwrap();
        for target in [400, 320, 0] {
            let expected = decode(&path, target, &config, &Cancel::default()).unwrap();
            let actual = source.render(target, &Cancel::default()).unwrap();
            assert_eq!(actual.orientation, orientation);
            assert_eq!(actual.rgba, expected.rgba);
            assert_eq!(
                (actual.width, actual.height),
                (expected.width, expected.height)
            );
        }
    }
}
