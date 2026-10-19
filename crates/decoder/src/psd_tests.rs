use super::*;
use std::io::Write;

fn fixture(depth: u16, mode: u16, channels: u16, compression: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = b"8BPS".to_vec();
    out.extend(1u16.to_be_bytes());
    out.extend([0; 6]);
    out.extend(channels.to_be_bytes());
    out.extend(1u32.to_be_bytes());
    out.extend(2u32.to_be_bytes());
    out.extend(depth.to_be_bytes());
    out.extend(mode.to_be_bytes());
    out.extend([0; 12]);
    out.extend(compression.to_be_bytes());
    out.extend(payload);
    out
}
fn decode_fixture(bytes: &[u8], config: &Config) -> Result<Frame> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("content-detection.PSD");
    std::fs::write(&path, bytes)?;
    decode(&path, 0, config, &Cancel::default())
}
#[test]
fn raw_rle_zip_and_prediction_have_identical_rgba() {
    let planes = [10, 20, 30, 40, 50, 60, 70, 80];
    let expected = [10, 30, 50, 70, 20, 40, 60, 80];
    for compression in 0..=3 {
        let payload = match compression {
            0 => planes.to_vec(),
            1 => {
                let mut b = vec![0, 3, 0, 3, 0, 3, 0, 3];
                for row in planes.chunks(2) {
                    b.push(1);
                    b.extend(row);
                }
                b
            }
            _ => {
                let raw = if compression == 3 {
                    vec![10, 10, 30, 10, 50, 10, 70, 10]
                } else {
                    planes.to_vec()
                };
                let mut z =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                z.write_all(&raw).unwrap();
                z.finish().unwrap()
            }
        };
        let frame =
            decode_fixture(&fixture(8, 3, 4, compression, &payload), &Config::default()).unwrap();
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.rgba.as_slice(), expected);
    }
}
#[test]
fn grayscale_16bit_and_zip_prediction() {
    let raw = [0x12, 0x34, 0xab, 0xcd];
    for compression in [0, 3] {
        let payload = if compression == 0 {
            raw.to_vec()
        } else {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(&[0x12, 0x34, 0x99, 0x99]).unwrap();
            z.finish().unwrap()
        };
        let frame = decode_fixture(
            &fixture(16, 1, 1, compression, &payload),
            &Config::default(),
        )
        .unwrap();
        assert_eq!(
            frame.rgba.as_slice(),
            [0x12, 0x12, 0x12, 255, 0xab, 0xab, 0xab, 255]
        );
    }
}
#[test]
fn malformed_unsupported_and_over_budget_fail_without_panicking() {
    let valid = fixture(8, 3, 3, 0, &[1; 6]);
    for length in 0..valid.len() {
        assert!(decode_fixture(&valid[..length], &Config::default()).is_err());
    }
    assert!(decode_fixture(&fixture(8, 4, 4, 0, &[0; 8]), &Config::default()).is_err());
    assert!(
        decode_fixture(
            &fixture(8, 3, 3, 1, &[0, 1, 0, 1, 0, 1, 127, 127, 127]),
            &Config::default()
        )
        .is_err()
    );
    let mut oversized = valid.clone();
    oversized[14..18].copy_from_slice(&30000u32.to_be_bytes());
    oversized[18..22].copy_from_slice(&30000u32.to_be_bytes());
    assert!(decode_fixture(&oversized, &Config::default()).is_err());
    let mut out = [0; 3];
    super::psd_rle::unpack(&[254, 42], &mut out).unwrap();
    assert_eq!(out, [42; 3]);
    assert!(super::psd_rle::unpack(&[253, 42], &mut out).is_err());
}

fn multirow_fixture(depth: u16, mode: u16, channels: u16, compression: u16) -> (Vec<u8>, Vec<u8>) {
    let (width, height) = (131usize, 3usize);
    let mut planes = Vec::new();
    let mut predicted = Vec::new();
    let mut packed = Vec::new();
    let mut lengths = Vec::new();
    let value = |c: usize, y: usize, x: usize| ((c * 19991 + y * 32761 + x * 997) & 65535) as u16;
    for c in 0..channels as usize {
        for y in 0..height {
            let mut row = Vec::new();
            let mut previous = 0u16;
            for x in 0..width {
                let v = value(c, y, x);
                if depth == 16 {
                    row.extend(v.to_be_bytes());
                    predicted.extend(v.wrapping_sub(previous).to_be_bytes());
                } else {
                    row.push(v as u8);
                    predicted.push((v as u8).wrapping_sub(previous as u8));
                }
                previous = v;
            }
            planes.extend(&row);
            let start = packed.len();
            for chunk in row.chunks(128) {
                packed.push(chunk.len() as u8 - 1);
                packed.extend(chunk);
            }
            lengths.extend(((packed.len() - start) as u16).to_be_bytes());
        }
    }
    let payload = match compression {
        0 => planes,
        1 => {
            lengths.extend(packed);
            lengths
        }
        _ => {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(if compression == 3 {
                &predicted
            } else {
                &planes
            })
            .unwrap();
            z.finish().unwrap()
        }
    };
    let mut bytes = fixture(depth, mode, channels, compression, &payload);
    bytes[14..18].copy_from_slice(&(height as u32).to_be_bytes());
    bytes[18..22].copy_from_slice(&(width as u32).to_be_bytes());
    let mut expected = Vec::new();
    let colors = if mode == 3 { 3 } else { 1 };
    for y in 0..height {
        for x in 0..width {
            let sample = |c| {
                if depth == 16 {
                    (value(c, y, x) >> 8) as u8
                } else {
                    value(c, y, x) as u8
                }
            };
            expected.extend([
                sample(0),
                sample(if mode == 3 { 1 } else { 0 }),
                sample(if mode == 3 { 2 } else { 0 }),
                if channels as usize > colors {
                    sample(colors)
                } else {
                    255
                },
            ]);
        }
    }
    (bytes, expected)
}

#[test]
fn streaming_rows_preserve_all_layouts_prediction_carries_and_extra_channels() {
    for depth in [8, 16] {
        for (mode, channels) in [(3, 3), (3, 4), (3, 6), (1, 1), (1, 2), (1, 5)] {
            for compression in 0..=3 {
                let (bytes, expected) = multirow_fixture(depth, mode, channels, compression);
                let frame = decode_fixture(&bytes, &Config::default()).unwrap();
                assert_eq!(
                    *frame.rgba, expected,
                    "depth={depth}, mode={mode}, channels={channels}, compression={compression}"
                );
            }
        }
    }
}

#[test]
fn streaming_validates_discarded_channels_and_zip_output_length() {
    for compression in 0..=3 {
        let (mut bytes, _) = multirow_fixture(16, 3, 6, compression);
        // Remove a substantial tail, including pixels in ignored extra channels.
        bytes.truncate(bytes.len() - 8);
        assert!(
            decode_fixture(&bytes, &Config::default()).is_err(),
            "compression={compression}"
        );
    }
    for compression in [2, 3] {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&[1; 7]).unwrap(); // RGB 2x1 expects exactly six bytes.
        let error = decode_fixture(
            &fixture(8, 3, 3, compression, &z.finish().unwrap()),
            &Config::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("ZIP output exceeds"));
    }
    let error = decode_fixture(
        &fixture(8, 3, 3, 1, &[0, 1, 0, 1, 0, 1, 127, 127, 127]),
        &Config::default(),
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("compression=1, channel=0, row=0"), "{error}");
}

#[test]
fn streaming_checks_cancellation_even_with_buffered_compressed_data() {
    struct CancelReader {
        input: Cursor<Vec<u8>>,
        cancel: Cancel,
    }
    impl Read for CancelReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.input.position() >= 40 {
                self.cancel.cancel();
            }
            self.input.read(out)
        }
    }
    impl Seek for CancelReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.input.seek(pos)
        }
    }
    for compression in 0..=3 {
        let (bytes, _) = multirow_fixture(8, 3, 4, compression);
        let cancel = Cancel::default();
        let reader = CancelReader {
            input: Cursor::new(bytes),
            cancel: cancel.clone(),
        };
        let decoder = psd::PsdDecoder::new(reader, Config::default(), &cancel).unwrap();
        let error = decoder.read_image(&mut vec![0; 131 * 3 * 4]).unwrap_err();
        assert!(error.to_string().contains("decode cancelled"), "{error}");
    }
}

#[test]
fn streaming_psd_main_thumbnail_and_shared_outputs_match_reference_png() {
    let dir = tempfile::tempdir().unwrap();
    let psd_path = dir.path().join("multirow.psd");
    let png_path = dir.path().join("reference.png");
    let (bytes, expected) = multirow_fixture(16, 3, 4, 3);
    std::fs::write(&psd_path, bytes).unwrap();
    image::save_buffer(&png_path, &expected, 131, 3, image::ColorType::Rgba8).unwrap();
    let config = Config::default();
    let targets = [
        (0, Cancel::default()),
        (67, Cancel::default()),
        (17, Cancel::default()),
    ];
    let mut shared = Vec::new();
    decode_shared_exact(
        &psd_path,
        &targets,
        &config,
        &Cancel::default(),
        |_, frame| {
            shared.push(frame);
            Ok(())
        },
    )
    .unwrap();
    for ((target, cancel), shared_frame) in targets.iter().zip(shared) {
        let frame = decode(&psd_path, *target, &config, cancel).unwrap();
        let reference = decode(&png_path, *target, &config, cancel).unwrap();
        assert_eq!(
            (frame.width, frame.height),
            (reference.width, reference.height)
        );
        assert_eq!(frame.rgba, reference.rgba);
        assert_eq!(shared_frame.rgba, reference.rgba);
        assert_eq!(
            (shared_frame.original_width, shared_frame.original_height),
            (131, 3)
        );
    }
}

#[test]
fn raw_bands_cover_partial_final_band_and_validate_extra_channel_tail() {
    let height = 137;
    let mut planes = Vec::new();
    let mut expected = Vec::new();
    let value = |c, y, x| (c * 31 + y + x) as u8;
    for c in 0..6 {
        for y in 0..height {
            for x in 0..2 {
                planes.push(value(c, y, x));
            }
        }
    }
    for y in 0..height {
        for x in 0..2 {
            expected.extend([
                value(0, y, x),
                value(1, y, x),
                value(2, y, x),
                value(3, y, x),
            ]);
        }
    }
    let mut bytes = fixture(8, 3, 6, 0, &planes);
    bytes[14..18].copy_from_slice(&(height as u32).to_be_bytes());
    let frame = decode_fixture(&bytes, &Config::default()).unwrap();
    assert_eq!(*frame.rgba, expected);
    bytes.pop();
    let error = decode_fixture(&bytes, &Config::default()).unwrap_err();
    assert!(format!("{error:#}").contains("compression=0, channel=5, row=128"));
}
