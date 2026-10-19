use anyhow::{Context, Result, bail};
use moxcms::{ColorProfile, DataColorSpace, Layout, Transform8BitExecutor, TransformOptions};
use mygo_core::Cancel;
use std::{cell::RefCell, sync::Arc};

// One entry per decoding thread. Compare all profile bytes: layout and options
// are fixed here, and a hash collision must never change displayed colors.
const MAX_PROFILE_BYTES: usize = 64 * 1024;
const MAX_SCRATCH_WIDTH: usize = 1000;

struct Prepared {
    gray: bool,
    transform: Arc<Transform8BitExecutor>,
}

impl Prepared {
    fn new(profile: &[u8]) -> Result<Self> {
        let source =
            ColorProfile::new_from_slice(profile).context("ICC 色彩描述已損壞或不受支援")?;
        let gray = source.color_space == DataColorSpace::Gray;
        anyhow::ensure!(
            gray || source.color_space == DataColorSpace::Rgb,
            "目前僅支援 RGB 與灰階 ICC 色彩描述"
        );
        let transform = source
            .create_transform_8bit(
                if gray { Layout::Gray } else { Layout::Rgba },
                &ColorProfile::new_srgb(),
                Layout::Rgba,
                TransformOptions::default(),
            )
            .context("無法將此 ICC 色彩描述轉換為 sRGB")?;
        Ok(Self { gray, transform })
    }
}

#[derive(Default)]
struct IccCache {
    entry: Option<(Vec<u8>, Prepared)>,
    line: Vec<u8>,
    gray_line: Vec<u8>,
}

thread_local! {
    static CACHE: RefCell<IccCache> = RefCell::default();
}

pub(super) fn apply_icc(
    rgba: &mut image::RgbaImage,
    profile: &[u8],
    cancel: &Cancel,
) -> Result<()> {
    CACHE.with_borrow_mut(|cache| cache.apply(rgba, profile, cancel))
}

impl IccCache {
    fn apply(
        &mut self,
        rgba: &mut image::RgbaImage,
        profile: &[u8],
        cancel: &Cancel,
    ) -> Result<()> {
        if cancel.cancelled() {
            bail!("載入已取消");
        }
        let cached = profile.len() <= MAX_PROFILE_BYTES;
        let temporary;
        let prepared = if cached {
            if !self.entry.as_ref().is_some_and(|(key, _)| key == profile) {
                // Release the previous transform before building its replacement.
                self.entry = None;
                self.entry = Some((profile.to_vec(), Prepared::new(profile)?));
            }
            &self.entry.as_ref().unwrap().1
        } else {
            // Larger/LUT-heavy profiles remain supported, without retaining them.
            self.entry = None;
            temporary = Prepared::new(profile)?;
            &temporary
        };
        let width = rgba.width() as usize;
        if width <= MAX_SCRATCH_WIDTH {
            convert(rgba, prepared, &mut self.line, &mut self.gray_line, cancel)
        } else {
            // Full-resolution requests must not leave large scratch on workers.
            convert(rgba, prepared, &mut Vec::new(), &mut Vec::new(), cancel)
        }
    }
}

fn convert(
    rgba: &mut image::RgbaImage,
    prepared: &Prepared,
    line: &mut Vec<u8>,
    gray_line: &mut Vec<u8>,
    cancel: &Cancel,
) -> Result<()> {
    let width = rgba.width() as usize;
    if width == 0 {
        return Ok(());
    }
    // Avoid Vec's geometric growth retaining more than the scratch policy.
    if line.capacity() < width * 4 {
        line.reserve_exact(width * 4 - line.len());
    }
    line.resize(width * 4, 0);
    if prepared.gray {
        if gray_line.capacity() < width {
            gray_line.reserve_exact(width - gray_line.len());
        }
        gray_line.resize(width, 0);
    }
    for row in rgba.as_mut().chunks_exact_mut(line.len()) {
        if cancel.cancelled() {
            bail!("載入已取消");
        }
        if prepared.gray {
            for (luma, pixel) in gray_line.iter_mut().zip(row.as_chunks::<4>().0.iter()) {
                *luma = pixel[0];
            }
        }
        prepared
            .transform
            .transform(if prepared.gray { gray_line } else { row }, line)
            .context("ICC 色彩轉換失敗")?;
        for (pixel, converted) in row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(line.as_chunks::<4>().0.iter())
        {
            pixel[..3].copy_from_slice(&converted[..3]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(width: u32) -> image::RgbaImage {
        image::RgbaImage::from_fn(width, 37, |x, y| {
            image::Rgba([(x % 256) as u8, (y * 5) as u8, 90, (x * 7 % 256) as u8])
        })
    }

    fn fresh(rgba: &mut image::RgbaImage, profile: &[u8], cancel: &Cancel) -> Result<()> {
        // Original path: parse/build every time and allocate fresh scanlines.
        convert(
            rgba,
            &Prepared::new(profile)?,
            &mut Vec::new(),
            &mut Vec::new(),
            cancel,
        )
    }

    #[test]
    fn reuse_matches_fresh_rgb_gray_alpha_and_profile_switches() {
        let p3 = ColorProfile::new_display_p3().encode().unwrap();
        let srgb = ColorProfile::new_srgb().encode().unwrap();
        let gray = ColorProfile::new_gray_with_gamma(2.2).encode().unwrap();
        let mut cache = IccCache::default();
        let cancel = Cancel::default();
        for profile in [&p3, &p3, &srgb, &gray, &gray, &p3] {
            for width in [1, 320, 160, 640, 800, 1000, 1201] {
                let source = pixels(width);
                let mut expected = source.clone();
                fresh(&mut expected, profile, &cancel).unwrap();
                let mut actual = source.clone();
                cache.apply(&mut actual, profile, &cancel).unwrap();
                assert_eq!(actual, expected);
                assert!(
                    actual
                        .pixels()
                        .zip(source.pixels())
                        .all(|(a, b)| a[3] == b[3])
                );
                assert!(cache.line.capacity() <= MAX_SCRATCH_WIDTH * 4);
                assert!(cache.gray_line.capacity() <= MAX_SCRATCH_WIDTH);
            }
        }
        let transform = cache.entry.as_ref().unwrap().1.transform.clone();
        cache.apply(&mut pixels(320), &p3, &cancel).unwrap();
        assert!(Arc::ptr_eq(
            &transform,
            &cache.entry.as_ref().unwrap().1.transform
        ));

        let mut actual = pixels(320);
        let original = actual.clone();
        assert!(cache.apply(&mut actual, b"broken", &cancel).is_err());
        assert_eq!(actual, original);
        assert!(cache.entry.is_none());
        cache.apply(&mut actual, &p3, &cancel).unwrap();
        cancel.cancel();
        let original = actual.clone();
        assert!(cache.apply(&mut actual, &p3, &cancel).is_err());
        assert_eq!(actual, original);
    }

    #[test]
    fn large_profiles_and_full_resolution_scratch_are_not_retained() {
        let p3 = ColorProfile::new_display_p3().encode().unwrap();
        // Valid ICC with trailing padding and an updated declared profile size.
        let mut padded = p3.clone();
        padded.resize(MAX_PROFILE_BYTES + 4, 0);
        let len = padded.len() as u32;
        padded[..4].copy_from_slice(&len.to_be_bytes());
        let cancel = Cancel::default();
        let mut cache = IccCache::default();
        cache.apply(&mut pixels(160), &p3, &cancel).unwrap();
        let capacities = (cache.line.capacity(), cache.gray_line.capacity());
        let mut actual = pixels(1600);
        let mut expected = actual.clone();
        fresh(&mut expected, &padded, &cancel).unwrap();
        cache.apply(&mut actual, &padded, &cancel).unwrap();
        assert_eq!(actual, expected);
        assert!(cache.entry.is_none());
        assert_eq!(
            capacities,
            (cache.line.capacity(), cache.gray_line.capacity())
        );
        cache.apply(&mut pixels(160), &p3, &cancel).unwrap();
        assert_eq!(cache.entry.as_ref().unwrap().0, p3);
    }

    #[test]
    #[ignore = "release-only ICC stage benchmark; run with --ignored --nocapture"]
    fn benchmark_icc_reuse() {
        use std::time::Instant;
        let cancel = Cancel::default();
        let mut rows = Vec::new();
        for (name, profile) in [
            ("p3", ColorProfile::new_display_p3().encode().unwrap()),
            ("srgb", ColorProfile::new_srgb().encode().unwrap()),
            (
                "gray",
                ColorProfile::new_gray_with_gamma(2.2).encode().unwrap(),
            ),
        ] {
            for target in [160, 320, 1000] {
                let source = image::RgbaImage::from_fn(target, target * 2 / 3, |x, y| {
                    image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, (x * 7 % 256) as u8])
                });
                // First call is separately measured; repeated calls use the real TLS path.
                CACHE.with_borrow_mut(|cache| *cache = IccCache::default());
                let mut initial = source.clone();
                let started = Instant::now();
                apply_icc(&mut initial, &profile, &cancel).unwrap();
                let cold_us = started.elapsed().as_micros() as u64;
                let mut before = Vec::new();
                let mut after = Vec::new();
                for pair in 0..30 {
                    let mut a = source.clone();
                    let mut b = source.clone();
                    for cached in if pair % 2 == 0 {
                        [false, true]
                    } else {
                        [true, false]
                    } {
                        let started = Instant::now();
                        if cached {
                            apply_icc(&mut b, &profile, &cancel).unwrap();
                            after.push(started.elapsed().as_micros() as u64);
                        } else {
                            fresh(&mut a, &profile, &cancel).unwrap();
                            before.push(started.elapsed().as_micros() as u64);
                        }
                    }
                    assert_eq!(a, b);
                    std::hint::black_box((&a, &b));
                }
                rows.push(serde_json::json!({
                    "profile": name, "profile_bytes": profile.len(), "target": target,
                    "cold_us": cold_us, "before_us": before, "after_us": after,
                }));
            }
        }
        let result =
            serde_json::json!({"benchmark": "ICC stage only", "pairs_per_case": 30, "rows": rows});
        println!("{result}");
        if let Ok(path) = std::env::var("MYGO_ICC_BENCHMARK_OUTPUT") {
            std::fs::write(path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        }
    }
}
