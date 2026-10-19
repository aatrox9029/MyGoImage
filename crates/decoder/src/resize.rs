use anyhow::{Result, bail};
use fast_image_resize::{FilterType, IntoImageView, ResizeAlg, ResizeOptions, Resizer};
use image::{ColorType, DynamicImage};
use mygo_core::{Cancel, Config};
use std::cell::RefCell;

// Keep small intermediates only. This is per decoding thread, not an RSS limit.
const MAX_SCRATCH_BYTES: usize = 2 * 1024 * 1024;

thread_local! {
    static RESIZER: RefCell<Resizer> = RefCell::new(Resizer::new());
}

fn resize_fir(
    image: &DynamicImage,
    width: u32,
    height: u32,
    budget: u64,
) -> std::result::Result<DynamicImage, fast_image_resize::ResizeError> {
    RESIZER.with_borrow_mut(|resizer| {
        // Account for previously retained scratch before allocating a new working
        // set, including calls with a newly reduced decode budget.
        let required = working_bytes(image, width, height);
        if resizer.size_of_internal_buffers() as u64 > budget.saturating_sub(required) {
            resizer.reset_internal_buffers();
        }
        let mut destination = DynamicImage::new(width, height, image.color());
        let result = resizer.resize(image, &mut destination, &resize_options());
        let retain = (budget / 16).min(MAX_SCRATCH_BYTES as u64);
        if result.is_err() || resizer.size_of_internal_buffers() as u64 > retain {
            resizer.reset_internal_buffers();
        }
        result.map(|_| destination)
    })
}

fn resize_options() -> ResizeOptions {
    ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
}

fn dimensions(image: &DynamicImage, target: u32) -> (u32, u32) {
    let ratio = f64::from(target) / f64::from(image.width().max(image.height()));
    (
        ((f64::from(image.width()) * ratio).round() as u32).max(1),
        ((f64::from(image.height()) * ratio).round() as u32).max(1),
    )
}

/// Conservative working-set estimate for pinned FIR 6.1 Triangle: source,
/// native destination, RGBA output, the larger possible separable intermediate,
/// an alpha copy, and coefficient/bounds/alignment headroom. Not an RSS limit.
fn working_bytes(image: &DynamicImage, width: u32, height: u32) -> u64 {
    let (sw, sh, dw, dh) = (
        u64::from(image.width()),
        u64::from(image.height()),
        u64::from(width),
        u64::from(height),
    );
    let bpp = u64::from(image.color().bytes_per_pixel());
    let source = sw.saturating_mul(sh).saturating_mul(bpp);
    let destination = dw.saturating_mul(dh);
    let intermediate = sw
        .saturating_mul(dh)
        .max(dw.saturating_mul(sh))
        .saturating_mul(bpp);
    let alpha = if image.color().has_alpha() { source } else { 0 };
    let coefficients = sw
        .saturating_add(sh)
        .saturating_add(dw)
        .saturating_add(dh)
        .saturating_mul(128)
        .saturating_add(65536);
    source
        .saturating_add(destination.saturating_mul(bpp.saturating_add(4)))
        .saturating_add(intermediate)
        .saturating_add(alpha)
        .saturating_add(coefficients)
}

/// Aspect-preserving, non-upscaling CPU resize with runtime SIMD detection.
/// Keep native depth until the normal decoder RGBA/ICC stages. Per-thread FIR
/// scratch retains at most 2 MiB (and at most 1/16 of the decode budget).
pub fn resize_cpu(
    image: DynamicImage,
    target: u32,
    config: &Config,
    cancel: &Cancel,
) -> Result<DynamicImage> {
    if !cancel.cancelled() && (target == 0 || image.width().max(image.height()) <= target) {
        anyhow::ensure!(image.width() > 0 && image.height() > 0, "影像尺寸不得為零");
        RESIZER.with_borrow_mut(Resizer::reset_internal_buffers);
        return Ok(image);
    }
    resize_borrowed(&image, target, config, cancel)
}

/// Resample directly from shared pixels. Only unchanged-size outputs need a copy.
pub(crate) fn resize_borrowed(
    image: &DynamicImage,
    target: u32,
    config: &Config,
    cancel: &Cancel,
) -> Result<DynamicImage> {
    if cancel.cancelled() {
        RESIZER.with_borrow_mut(Resizer::reset_internal_buffers);
        bail!("載入已取消");
    }
    anyhow::ensure!(image.width() > 0 && image.height() > 0, "影像尺寸不得為零");
    if target == 0 || image.width().max(image.height()) <= target {
        RESIZER.with_borrow_mut(Resizer::reset_internal_buffers);
        return Ok(image.clone());
    }
    let (width, height) = dimensions(image, target);
    // Measured exception: SIMD alpha premultiplication is slower for RGBA8
    // thumbnails <=320px. Retain their existing algorithm/appearance.
    let small_rgba = image.color() == ColorType::Rgba8 && target <= 320;
    let budget = config.image_limits().map_or(u64::MAX, |limits| limits.3);
    let use_simd = !small_rgba
        && image.pixel_type().is_some()
        && working_bytes(image, width, height) <= budget;
    let resized = if use_simd {
        // Drop failed destination and scratch before any legacy fallback.
        resize_fir(image, width, height, budget).ok()
    } else {
        RESIZER.with_borrow_mut(Resizer::reset_internal_buffers);
        None
    };
    let result = resized.unwrap_or_else(|| image.thumbnail(target, target));
    if cancel.cancelled() {
        RESIZER.with_borrow_mut(Resizer::reset_internal_buffers);
        bail!("載入已取消");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn fresh(image: &DynamicImage, target: u32) -> DynamicImage {
        let (w, h) = dimensions(image, target);
        let mut destination = DynamicImage::new(w, h, image.color());
        Resizer::new()
            .resize(image, &mut destination, &resize_options())
            .unwrap();
        destination
    }

    fn retained() -> usize {
        RESIZER.with_borrow(|r| r.size_of_internal_buffers())
    }

    fn fixture(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 239) as u8, ((x + y) % 233) as u8])
        }))
    }

    #[test]
    fn reused_scratch_matches_fresh_across_layouts_and_releases_high_water() {
        let config = Config::default();
        let cancel = Cancel::default();
        let rgb = fixture(900, 600);
        let rgba = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(900, 600, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 239) as u8, 71, (x % 255) as u8])
        }));
        let gray = DynamicImage::ImageLuma16(image::ImageBuffer::from_fn(900, 600, |x, y| {
            image::Luma([((x * 31 + y * 7) % 65536) as u16])
        }));
        for source in [&rgb, &gray, &rgb, &rgba, &rgb] {
            for target in [640, 400, 640] {
                let actual = resize_cpu(source.clone(), target, &config, &cancel).unwrap();
                let expected = fresh(source, target);
                assert_eq!(actual.color(), expected.color());
                assert_eq!(actual.as_bytes(), expected.as_bytes());
                assert!(retained() <= MAX_SCRATCH_BYTES);
            }
        }
        assert!(retained() > 0);
        resize_cpu(fixture(2400, 1600), 1000, &config, &cancel).unwrap();
        assert_eq!(retained(), 0);
        resize_cpu(rgb.clone(), 320, &config, &cancel).unwrap();
        assert!(retained() > 0);
        let tight = Config {
            decode_mib: 4,
            ..config.clone()
        };
        resize_cpu(rgb.clone(), 640, &tight, &cancel).unwrap();
        assert_eq!(retained(), 0);
        resize_cpu(rgb.clone(), 320, &config, &cancel).unwrap();
        cancel.cancel();
        assert!(resize_cpu(rgb, 320, &config, &cancel).is_err());
        assert_eq!(retained(), 0);
    }

    #[test]
    #[ignore = "release-only paired resize benchmark"]
    fn benchmark_resizer_reuse() {
        use std::time::Instant;
        let mut rows = Vec::new();
        for (name, source) in [
            ("rgb-photo", fixture(1800, 1200)),
            (
                "rgba",
                DynamicImage::ImageRgba8(fixture(600, 400).into_rgba8()),
            ),
            (
                "gray16",
                DynamicImage::ImageLuma16(fixture(1200, 800).into_luma16()),
            ),
        ] {
            for target in [160, 320, 480, 1000] {
                if target >= source.width().max(source.height()) {
                    continue;
                }
                let (w, h) = dimensions(&source, target);
                RESIZER.with_borrow_mut(Resizer::reset_internal_buffers);
                let started = Instant::now();
                let initial = resize_fir(&source, w, h, u64::MAX).unwrap();
                let cold_us = started.elapsed().as_micros() as u64;
                assert_eq!(initial.as_bytes(), fresh(&source, target).as_bytes());
                let mut before = Vec::new();
                let mut after = Vec::new();
                for pair in 0..60 {
                    let mut outputs = Vec::new();
                    for reuse in if pair % 2 == 0 {
                        [false, true]
                    } else {
                        [true, false]
                    } {
                        let started = Instant::now();
                        let output = if reuse {
                            resize_fir(&source, w, h, u64::MAX).unwrap()
                        } else {
                            fresh(&source, target)
                        };
                        let us = started.elapsed().as_micros() as u64;
                        if reuse {
                            after.push(us);
                        } else {
                            before.push(us);
                        }
                        outputs.push(output);
                    }
                    assert_eq!(outputs[0].as_bytes(), outputs[1].as_bytes());
                    std::hint::black_box(&outputs);
                }
                rows.push(serde_json::json!({"fixture":name, "width":source.width(), "height":source.height(), "target":target, "cold_us":cold_us, "before_us":before, "after_us":after, "retained_bytes":retained()}));
            }
        }
        let result = serde_json::json!({"benchmark":"FIR resize stage only; destination allocation included; synthetic inputs; RGBA <=320 production fallback excluded", "pairs_per_case":60, "rows":rows});
        if let Ok(path) = std::env::var("MYGO_RESIZER_BENCHMARK_OUTPUT") {
            std::fs::write(path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        }
        println!("{result}");
    }

    #[test]
    fn alpha_edges_keep_hidden_color_out_and_16bit_data_is_preserved() {
        let mut pixels = image::RgbaImage::from_pixel(1200, 800, image::Rgba([255, 0, 255, 0]));
        for y in 0..800 {
            for x in 601..1200 {
                pixels.put_pixel(x, y, image::Rgba([0, 200, 0, 255]));
            }
        }
        let resized = resize_cpu(
            DynamicImage::ImageRgba8(pixels),
            640,
            &Config::default(),
            &Cancel::default(),
        )
        .unwrap()
        .into_rgba8();
        assert_eq!(resized.dimensions(), (640, 427));
        let edge: Vec<_> = resized
            .pixels()
            .filter(|p| p[3] > 0 && p[3] < 255)
            .collect();
        assert!(!edge.is_empty());
        assert!(
            edge.iter()
                .all(|p| p[0] == 0 && p[2] == 0 && p[1].abs_diff(200) <= 2)
        );
        let gray = image::ImageBuffer::from_pixel(1200, 800, image::Luma([12345u16]));
        let resized = resize_cpu(
            DynamicImage::ImageLuma16(gray),
            640,
            &Config::default(),
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(resized.color(), ColorType::L16);
        assert!(resized.as_luma16().unwrap().pixels().all(|p| p[0] == 12345));
    }

    #[test]
    fn small_rgba_and_tight_budget_keep_legacy_pixels_without_upscaling() {
        let pixels = image::RgbaImage::from_fn(900, 600, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 251) as u8, 87, (x % 255) as u8])
        });
        let image = DynamicImage::ImageRgba8(pixels);
        let cancel = Cancel::default();
        let small = resize_cpu(image.clone(), 320, &Config::default(), &cancel).unwrap();
        assert_eq!(small.as_bytes(), image.thumbnail(320, 320).as_bytes());
        let tight = Config {
            decode_mib: 4,
            ..Config::default()
        };
        let fallback = resize_cpu(image.clone(), 640, &tight, &cancel).unwrap();
        assert_eq!(fallback.as_bytes(), image.thumbnail(640, 640).as_bytes());
        let original = resize_cpu(image.clone(), 1600, &Config::default(), &cancel).unwrap();
        assert_eq!(original.as_bytes(), image.as_bytes());
        assert_eq!(original.dimensions(), image.dimensions());
        cancel.cancel();
        assert!(resize_cpu(image, 640, &Config::default(), &cancel).is_err());
    }
}
