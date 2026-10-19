//! One bounded, in-flight source. No source pixels survive the worker operation.
use super::*;

pub struct SharedSource {
    source: Prepared,
    config: Config,
    timings: DecodeTimings,
}

impl SharedSource {
    /// Non-JPEG sources have target-independent codec pixels. JPEG is rejected
    /// by content before decoding, regardless of its extension.
    pub fn open(path: &Path, config: &Config, cancel: &Cancel) -> Result<Self> {
        let (source, timings) = prepare(path, 0, config, cancel, false, true)?;
        // Retained source + native output + RGBA, including 16-bit PNG.
        let bytes = source.image.as_bytes().len() as u64;
        let pixels = u64::from(source.image.width()) * u64::from(source.image.height());
        if config.image_limits().is_some_and(|(_, _, _, budget)| {
            bytes.saturating_mul(2).saturating_add(pixels * 4) > budget
        }) {
            bail!("共享來源與輸出超出記憶體預算");
        }
        Ok(Self {
            source,
            config: config.clone(),
            timings,
        })
    }

    pub fn is_native_size(&self, target: u32) -> bool {
        target == 0 || target >= self.source.image.width().max(self.source.image.height())
    }

    /// Consume the last source when no resampling is needed. This avoids copying
    /// full-size single outputs while preserving native depth through ICC.
    pub fn finish(self, target: u32, cancel: &Cancel) -> Result<(Frame, DecodeTimings)> {
        let Self {
            source,
            config,
            mut timings,
        } = self;
        let started = source.started;
        let frame = super::finish(
            source,
            target,
            &config,
            cancel,
            &mut |image, target| resize_cpu(image, target, &config, cancel),
            &mut timings,
        )?;
        timings.total_us = started.elapsed().as_micros() as u64;
        timings.other_us = timings
            .total_us
            .saturating_sub(timings.stages()[..7].iter().map(|(_, us)| us).sum());
        Ok((frame, timings))
    }

    pub fn render(&mut self, target: u32, cancel: &Cancel) -> Result<Frame> {
        render(
            &self.source,
            target,
            &self.config,
            cancel,
            &mut self.timings,
        )
    }

    pub fn timings(&self) -> DecodeTimings {
        let mut timings = self.timings.clone();
        timings.total_us = self.source.started.elapsed().as_micros() as u64;
        timings.other_us = timings
            .total_us
            .saturating_sub(timings.stages()[..7].iter().map(|(_, us)| us).sum());
        timings
    }
}

pub(super) fn render(
    source: &Prepared,
    target: u32,
    config: &Config,
    cancel: &Cancel,
    timings: &mut DecodeTimings,
) -> Result<Frame> {
    let stage = Instant::now();
    let image = resize::resize_borrowed(&source.image, target, config, cancel)?;
    timings.resize_us += stage.elapsed().as_micros() as u64;
    finish(
        Prepared {
            image,
            icc: source.icc.clone(),
            original_width: source.original_width,
            original_height: source.original_height,
            orientation: source.orientation,
            started: source.started,
        },
        0,
        config,
        cancel,
        &mut |image, _| Ok(image),
        timings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_single_output_transfers_rgba_without_a_full_pixel_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native.png");
        image::RgbaImage::from_pixel(100, 50, image::Rgba([40, 80, 120, 93]))
            .save(&path)
            .unwrap();
        let source = SharedSource::open(&path, &Config::default(), &Cancel::default()).unwrap();
        let pointer = source.source.image.as_rgba8().unwrap().as_ptr();
        assert!(source.is_native_size(0));
        assert!(source.is_native_size(100));
        assert!(!source.is_native_size(32));
        let (frame, timings) = source.finish(0, &Cancel::default()).unwrap();
        assert_eq!(frame.rgba.as_ptr(), pointer);
        assert_eq!((frame.width, frame.height), (100, 50));
        assert_eq!(timings.codec_calls, 1);
    }

    #[test]
    fn shared_source_enforces_budget_timeout_and_content_sniffing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.png");
        image::RgbaImage::new(100, 50).save(&path).unwrap();
        let tiny = Config {
            decode_mib: 0,
            ..Config::default()
        };
        assert!(SharedSource::open(&path, &tiny, &Cancel::default()).is_err());
        let config = Config {
            decode_timeout_ms: 1,
            ..Config::default()
        };
        let mut source = SharedSource::open(&path, &Config::default(), &Cancel::default()).unwrap();
        source.config = config;
        source.source.started = Instant::now() - Duration::from_secs(1);
        assert!(source.render(32, &Cancel::default()).is_err());
        image::RgbImage::new(100, 50)
            .save_with_format(&path, ImageFormat::Jpeg)
            .unwrap();
        assert!(SharedSource::open(&path, &Config::default(), &Cancel::default()).is_err());
    }
}
