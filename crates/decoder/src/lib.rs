use anyhow::{Context, Result, bail};
mod icc;
use icc::apply_icc;
mod jpeg;
mod resize;
pub use resize::resize_cpu;

/// Includes resampling and alpha semantics; old disk pixels must not be reused.
pub const PIXEL_PIPELINE_VERSION: &str = "rgba-v3-simd-triangle-v1";
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
#[cfg(test)]
use mygo_core::MIB;
use mygo_core::{Cancel, Config};
use std::{
    fs::File,
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub orientation: u8,
    pub rgba: Arc<Vec<u8>>,
}
/// Exclusive wall-clock buckets. I/O is time inside open/metadata/read/seek,
/// including OS-cache hits; it is not physical disk latency or CPU usage.
#[derive(Clone, Debug, Default)]
pub struct DecodeTimings {
    pub io_us: u64,
    pub header_us: u64,
    pub codec_us: u64,
    pub orientation_us: u64,
    pub resize_us: u64,
    pub rgba_us: u64,
    pub icc_us: u64,
    pub other_us: u64,
    pub total_us: u64,
    pub read_bytes: u64,
    pub source_opens: u64,
    pub codec_calls: u64,
}
impl DecodeTimings {
    pub fn stages(&self) -> [(&'static str, u64); 9] {
        [
            ("image-io", self.io_us),
            ("image-header", self.header_us),
            ("image-codec", self.codec_us),
            ("image-orientation", self.orientation_us),
            ("image-resize", self.resize_us),
            ("image-rgba", self.rgba_us),
            ("image-icc", self.icc_us),
            ("image-other", self.other_us),
            ("image-total", self.total_us),
        ]
    }
}
#[derive(Default)]
struct IoStats {
    nanos: AtomicU64,
    bytes: AtomicU64,
    opens: AtomicU64,
}
impl IoStats {
    fn elapsed(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
    }
    fn add(&self, started: Instant) {
        self.nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}
impl Frame {
    pub fn bytes(&self) -> usize {
        self.rgba.len()
    }
}
struct Guarded {
    file: File,
    cancel: Cancel,
    deadline: Instant,
    stats: Arc<IoStats>,
}
impl Guarded {
    fn check(&self) -> io::Result<()> {
        if self.cancel.cancelled() {
            // Interrupted is retried by read_exact/read_to_end, causing an endless
            // retry loop once cancelled. A terminal error releases the worker.
            Err(io::Error::other("載入已取消"))
        } else if Instant::now() > self.deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "影像解碼超過時間上限",
            ))
        } else {
            Ok(())
        }
    }
}
impl Read for Guarded {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let started = Instant::now();
        let result = self.file.read(b);
        self.stats.add(started);
        if let Ok(n) = result {
            self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        result
    }
}
impl Seek for Guarded {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let started = Instant::now();
        let result = self.file.seek(p);
        self.stats.add(started);
        result
    }
}
fn reader(
    path: &Path,
    c: &Config,
    cancel: &Cancel,
    stats: Arc<IoStats>,
) -> Result<BufReader<Guarded>> {
    let started = Instant::now();
    let f = File::open(path).context("無法讀取圖片；來源可能已中斷或沒有權限")?;
    stats.opens.fetch_add(1, Ordering::Relaxed);
    let file_size = f.metadata()?.len();
    stats.add(started);
    if c.image_limits()
        .is_some_and(|(_, _, max_file, _)| file_size > max_file)
    {
        bail!("檔案超過設定的大小上限");
    }
    Ok(BufReader::with_capacity(
        65536,
        Guarded {
            file: f,
            cancel: cancel.clone(),
            deadline: Instant::now() + Duration::from_millis(c.decode_timeout_ms),
            stats,
        },
    ))
}
fn check_dimensions(w: u32, h: u32, c: &Config) -> Result<()> {
    if w == 0
        || h == 0
        || c.image_limits().is_some_and(|(pixels, dimension, _, _)| {
            w > dimension || h > dimension || u64::from(w) * u64::from(h) > pixels
        })
    {
        bail!("影像尺寸 {w} × {h} 超過安全上限");
    }
    Ok(())
}
pub fn decode(path: &Path, target: u32, c: &Config, cancel: &Cancel) -> Result<Frame> {
    decode_profiled(path, target, c, cancel).map(|(frame, _)| frame)
}
pub fn decode_profiled(
    path: &Path,
    target: u32,
    c: &Config,
    cancel: &Cancel,
) -> Result<(Frame, DecodeTimings)> {
    decode_profiled_backend(path, target, c, cancel, false)
}
/// `image_jpeg` compares the existing image-crate JPEG codec without native
/// reduced-resolution decoding. The production path keeps jpeg-decoder.
pub fn decode_profiled_backend(
    path: &Path,
    target: u32,
    c: &Config,
    cancel: &Cancel,
    image_jpeg: bool,
) -> Result<(Frame, DecodeTimings)> {
    decode_profiled_resize(path, target, c, cancel, image_jpeg, &mut |img, target| {
        resize_cpu(img, target, c, cancel)
    })
}
/// Benchmark hook: preserve codec, orientation, limits and ICC while comparing
/// complete resize paths, including their pixel conversions and transfers.
pub fn decode_profiled_resize(
    path: &Path,
    target: u32,
    c: &Config,
    cancel: &Cancel,
    image_jpeg: bool,
    resize: &mut impl FnMut(DynamicImage, u32) -> Result<DynamicImage>,
) -> Result<(Frame, DecodeTimings)> {
    let (source, mut timings) = prepare(path, target, c, cancel, image_jpeg, false)?;
    let started = Instant::now();
    let frame = finish(source, target, c, cancel, resize, &mut timings)?;
    timings.total_us += started.elapsed().as_micros() as u64;
    timings.other_us = timings
        .total_us
        .saturating_sub(timings.stages()[..7].iter().map(|(_, us)| us).sum());
    Ok((frame, timings))
}

struct Prepared {
    image: DynamicImage,
    icc: Option<Vec<u8>>,
    original_width: u32,
    original_height: u32,
    orientation: u8,
    started: Instant,
}

/// Experimental multi-target pipeline. JPEG uses the largest requested native
/// scale, so its pixels may differ from independent lower-resolution decodes.
/// Do not publish these frames under PIXEL_PIPELINE_VERSION cache keys.
pub const SHARED_PIXEL_PIPELINE_VERSION: &str = "rgba-v4-shared-source-v1";

/// Decode a source once and emit outputs in caller order (put Main first).
/// Each resize starts from oriented source pixels, before ICC. The callback can
/// release each output immediately; no full-resolution source survives the call.
/// `source_cancel` controls the common work; per-output tokens stay independent.
pub fn decode_shared(
    path: &Path,
    targets: &[(u32, Cancel)],
    c: &Config,
    source_cancel: &Cancel,
    mut emit: impl FnMut(usize, Frame) -> Result<()>,
) -> Result<DecodeTimings> {
    decode_shared_impl(path, targets, c, source_cancel, false, &mut emit)
}

/// Production sharing preserves independent-pipeline pixels and cache keys.
/// JPEG must use independent native scale selection; reject it before codec work.
pub fn decode_shared_exact(
    path: &Path,
    targets: &[(u32, Cancel)],
    c: &Config,
    source_cancel: &Cancel,
    mut emit: impl FnMut(usize, Frame) -> Result<()>,
) -> Result<DecodeTimings> {
    decode_shared_impl(path, targets, c, source_cancel, true, &mut emit)
}

fn decode_shared_impl(
    path: &Path,
    targets: &[(u32, Cancel)],
    c: &Config,
    source_cancel: &Cancel,
    exact: bool,
    emit: &mut impl FnMut(usize, Frame) -> Result<()>,
) -> Result<DecodeTimings> {
    let active: Vec<_> = targets
        .iter()
        .enumerate()
        .filter(|(_, (_, cancel))| !cancel.cancelled())
        .collect();
    if active.is_empty() {
        return Ok(DecodeTimings::default());
    }
    let largest = if active.iter().any(|(_, (target, _))| *target == 0) {
        0
    } else {
        active.iter().map(|(_, (target, _))| *target).max().unwrap()
    };
    let (source, mut timings) = prepare(path, largest, c, source_cancel, false, exact)?;
    let started = source.started;
    // Source + one clone + output/scratch must fit the same per-operation budget.
    if c.image_limits().is_some_and(|(_, _, _, budget)| {
        u64::from(source.image.width())
            * u64::from(source.image.height())
            * u64::from(source.image.color().bytes_per_pixel()).max(8)
            * 4
            > budget
    }) {
        bail!("共享來源與輸出超出記憶體預算");
    }
    for (index, (target, cancel)) in active {
        if source_cancel.cancelled() {
            bail!("載入已取消");
        }
        if cancel.cancelled() {
            continue;
        }
        let prepared = Prepared {
            image: source.image.clone(),
            icc: source.icc.clone(),
            original_width: source.original_width,
            original_height: source.original_height,
            orientation: source.orientation,
            started,
        };
        let result = finish(
            prepared,
            *target,
            c,
            cancel,
            &mut |img, target| resize_cpu(img, target, c, cancel),
            &mut timings,
        );
        if source_cancel.cancelled() {
            bail!("載入已取消");
        }
        if cancel.cancelled() {
            continue;
        }
        emit(index, result?)?;
    }
    timings.total_us = started.elapsed().as_micros() as u64;
    timings.other_us = timings
        .total_us
        .saturating_sub(timings.stages()[..7].iter().map(|(_, us)| us).sum());
    Ok(timings)
}

fn prepare(
    path: &Path,
    target: u32,
    c: &Config,
    cancel: &Cancel,
    image_jpeg: bool,
    reject_jpeg: bool,
) -> Result<(Prepared, DecodeTimings)> {
    let start = Instant::now();
    let stats = Arc::new(IoStats::default());
    let mut timings = DecodeTimings::default();
    let mut ir = ImageReader::new(reader(path, c, cancel, stats.clone())?).with_guessed_format()?;
    let format = ir
        .format()
        .context("不支援此圖片格式（HEIC / AVIF / RAW 未啟用）")?;
    if reject_jpeg && format == ImageFormat::Jpeg {
        bail!("JPEG 保留各尺寸獨立原生縮放");
    }
    let mut limits = image::Limits::default();
    let size_limits = c.image_limits();
    limits.max_image_width = size_limits.map(|(_, dimension, _, _)| dimension);
    limits.max_image_height = limits.max_image_width;
    limits.max_alloc = size_limits.map(|(_, _, _, budget)| budget);
    ir.limits(limits.clone());
    let mut header: Box<dyn ImageDecoder> = if format == ImageFormat::Jpeg && !image_jpeg {
        let mut source = ir.into_inner();
        let prefix = jpeg::read_header(&mut source)?;
        let mut header = image::codecs::jpeg::JpegDecoder::new(Cursor::new(prefix))?;
        header.set_limits(limits)?;
        Box::new(header)
    } else {
        Box::new(ir.into_decoder()?)
    };
    let (w, h) = header.dimensions();
    check_dimensions(w, h, c)?;
    let orientation = header
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let orientation_code = orientation.to_exif();
    let icc = header
        .icc_profile()
        .context("無法讀取圖片的 ICC 色彩描述")?;
    if icc
        .as_ref()
        .is_some_and(|profile| profile.len() > 4 * 1024 * 1024)
    {
        bail!("ICC 色彩描述超過 4 MiB 安全上限");
    }
    let header_io = stats.elapsed();
    timings.header_us = start.elapsed().saturating_sub(header_io).as_micros() as u64;
    let codec_started = Instant::now();
    let target = if target == 0 { w.max(h) } else { target };
    let mut img = if format == ImageFormat::Jpeg && !image_jpeg {
        drop(header);
        let mut d = jpeg_decoder::Decoder::new(reader(path, c, cancel, stats.clone())?);
        d.set_max_decoding_buffer_size(
            size_limits.map_or(usize::MAX, |(_, _, _, budget)| budget as usize),
        );
        d.read_info()?;
        let ratio = (target as f64 / w.max(h) as f64).min(1.0);
        let (mut dw, mut dh) = d.scale(
            (w as f64 * ratio).ceil().max(1.0) as u16,
            (h as f64 * ratio).ceil().max(1.0) as u16,
        )?;
        // Select the highest native JPEG scale that fits the working-buffer budget.
        let budget = size_limits.map_or(u64::MAX, |(_, _, _, budget)| budget);
        while u64::from(dw) * u64::from(dh) * 12 > budget {
            let next = d.scale((dw / 2).max(1), (dh / 2).max(1))?;
            if next == (dw, dh) {
                bail!("JPEG 最低解碼解析度仍超出記憶體預算");
            }
            (dw, dh) = next;
        }
        let pixels = d.decode()?;
        match d.info().context("JPEG 標頭不完整")?.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(dw as u32, dh as u32, pixels)
                    .context("JPEG 像素長度錯誤")?,
            ),
            jpeg_decoder::PixelFormat::L8 => DynamicImage::ImageLuma8(
                image::GrayImage::from_raw(dw as u32, dh as u32, pixels)
                    .context("JPEG 像素長度錯誤")?,
            ),
            _ => bail!("目前不支援 CMYK 或 16 位元 JPEG"),
        }
    } else {
        // Rust codecs require a full decode: reject before allocation if it cannot fit.
        if size_limits.is_some_and(|(_, _, _, budget)| u64::from(w) * u64::from(h) * 16 > budget) {
            bail!("PNG/TIFF 等完整解碼超出記憶體預算；此版本未提供分塊解碼");
        }
        DynamicImage::from_decoder(header)?
    };
    timings.codec_us = codec_started
        .elapsed()
        .saturating_sub(stats.elapsed().saturating_sub(header_io))
        .as_micros() as u64;
    if cancel.cancelled() {
        bail!("載入已取消");
    }
    let stage = Instant::now();
    img.apply_orientation(orientation);
    timings.orientation_us = stage.elapsed().as_micros() as u64;
    let (ow, oh) = if matches!(orientation_code, 5..=8) {
        (h, w)
    } else {
        (w, h)
    };
    timings.io_us = stats.elapsed().as_micros() as u64;
    timings.read_bytes = stats.bytes.load(Ordering::Relaxed);
    timings.source_opens = stats.opens.load(Ordering::Relaxed);
    timings.codec_calls = 1;
    timings.total_us = start.elapsed().as_micros() as u64;
    Ok((
        Prepared {
            image: img,
            icc,
            original_width: ow,
            original_height: oh,
            orientation: orientation_code,
            started: start,
        },
        timings,
    ))
}

fn finish(
    source: Prepared,
    target: u32,
    c: &Config,
    cancel: &Cancel,
    resize: &mut impl FnMut(DynamicImage, u32) -> Result<DynamicImage>,
    timings: &mut DecodeTimings,
) -> Result<Frame> {
    let Prepared {
        image: img,
        icc,
        original_width,
        original_height,
        orientation,
        started,
    } = source;
    let target = if target == 0 {
        img.width().max(img.height())
    } else {
        target
    };
    let stage = Instant::now();
    let img = if img.width() > target || img.height() > target {
        resize(img, target)?
    } else {
        img
    };
    timings.resize_us += stage.elapsed().as_micros() as u64;
    let stage = Instant::now();
    let mut rgba = img.into_rgba8();
    timings.rgba_us += stage.elapsed().as_micros() as u64;
    let stage = Instant::now();
    if let Some(profile) = icc {
        apply_icc(&mut rgba, &profile, cancel)?;
    }
    timings.icc_us += stage.elapsed().as_micros() as u64;
    if started.elapsed() > Duration::from_millis(c.decode_timeout_ms) {
        bail!("影像解碼超過時間上限");
    }
    if cancel.cancelled() {
        bail!("載入已取消");
    }
    Ok(Frame {
        width: rgba.width(),
        height: rgba.height(),
        original_width,
        original_height,
        orientation,
        rgba: Arc::new(rgba.into_raw()),
    })
}
pub fn encode_thumb(f: &Frame) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(24 + f.bytes());
    result.extend_from_slice(b"MGT2");
    for n in [
        f.width,
        f.height,
        f.original_width,
        f.original_height,
        f.orientation as u32,
    ] {
        result.extend_from_slice(&n.to_le_bytes());
    }
    result.extend_from_slice(&f.rgba);
    Ok(result)
}
pub fn decode_thumb(b: &[u8]) -> Result<Frame> {
    let (b, maximum) = if b.starts_with(b"MGT2") {
        (&b[4..], 1000)
    } else {
        (b, 320)
    };
    if b.len() < 20 {
        bail!("縮圖快取已損壞");
    }
    let mut c = Cursor::new(b);
    let mut n = [0u32; 5];
    for v in &mut n {
        let mut t = [0; 4];
        c.read_exact(&mut t)?;
        *v = u32::from_le_bytes(t);
    }
    if n[0] == 0
        || n[1] == 0
        || n[0] > maximum
        || n[1] > maximum
        || !(1..=8).contains(&n[4])
        || n[2] == 0
        || n[3] == 0
        || u64::from(n[0]) * u64::from(n[1]) * 4 + 20 != b.len() as u64
    {
        bail!("縮圖快取尺寸錯誤");
    }
    Ok(Frame {
        width: n[0],
        height: n[1],
        original_width: n[2],
        original_height: n[3],
        orientation: n[4] as u8,
        rgba: Arc::new(b[20..].to_vec()),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_source_preserves_png_pixels_metadata_and_independent_cancellation() {
        use image::ImageEncoder;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.png");
        let pixels = image::RgbaImage::from_fn(401, 251, |x, y| {
            image::Rgba([x as u8, y as u8, (x + y) as u8, (x * y) as u8])
        });
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder
            .set_icc_profile(moxcms::ColorProfile::new_display_p3().encode().unwrap())
            .unwrap();
        encoder
            .write_image(&pixels, 401, 251, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let targets: Vec<_> = [400, 320, 160, 0]
            .into_iter()
            .map(|n| (n, Cancel::default()))
            .collect();
        let config = Config::default();
        let mut emitted = Vec::new();
        let timing = decode_shared_exact(
            &path,
            &targets,
            &config,
            &Cancel::default(),
            |index, frame| {
                let expected =
                    decode(&path, targets[index].0, &config, &Cancel::default()).unwrap();
                assert_eq!(frame.rgba, expected.rgba);
                assert_eq!(
                    (
                        frame.original_width,
                        frame.original_height,
                        frame.orientation
                    ),
                    (401, 251, 1)
                );
                emitted.push(index);
                if index == 0 {
                    targets[1].1.cancel();
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(emitted, [0, 2, 3]);
        assert!(timing.read_bytes > 0);
        assert_eq!((timing.source_opens, timing.codec_calls), (1, 1));
        let source_cancel = Cancel::default();
        let mut delivered = 0;
        assert!(
            decode_shared(&path, &targets, &config, &source_cancel, |_, _| {
                delivered += 1;
                source_cancel.cancel();
                Ok(())
            })
            .is_err()
        );
        assert_eq!(delivered, 1);
        let cancelled = Cancel::default();
        cancelled.cancel();
        assert!(decode_shared(&path, &targets, &config, &cancelled, |_, _| panic!()).is_err());
        let tiny = Config {
            decode_mib: 2,
            ..config
        };
        assert!(
            decode_shared(&path, &targets, &tiny, &Cancel::default(), |_, _| panic!()).is_err()
        );
    }

    #[test]
    fn exact_sharing_rejects_jpeg_content_even_with_a_png_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disguised.png");
        image::RgbImage::from_pixel(100, 60, image::Rgb([40, 80, 120]))
            .save_with_format(&path, image::ImageFormat::Jpeg)
            .unwrap();
        let targets = [(80, Cancel::default()), (20, Cancel::default())];
        let error = decode_shared_exact(
            &path,
            &targets,
            &Config::default(),
            &Cancel::default(),
            |_, _| panic!("JPEG cannot use exact shared cache keys"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("JPEG"));
        assert!(decode(&path, 20, &Config::default(), &Cancel::default()).is_ok());
    }

    #[test]
    fn jpeg_streaming_header_preserves_late_split_metadata_and_skips_trailing_bytes() {
        fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
            let mut bytes = vec![0xff, marker];
            bytes.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            bytes.extend_from_slice(payload);
            bytes
        }
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.jpg");
        image::RgbImage::from_fn(81, 43, |x, y| {
            image::Rgb([(x * 3) as u8, (y * 5) as u8, ((x + y) * 2) as u8])
        })
        .save(&plain)
        .unwrap();
        let original = std::fs::read(&plain).unwrap();
        let mut position = 2;
        while original[position + 1] != 0xc0 {
            position += 2 + usize::from(u16::from_be_bytes([
                original[position + 2],
                original[position + 3],
            ]));
        }
        position += 2 + usize::from(u16::from_be_bytes([
            original[position + 2],
            original[position + 3],
        ]));
        let profile = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        let midpoint = profile.len() / 2;
        let config = Config::default();
        let cancel = Cancel::default();
        for orientation in 1..=8u8 {
            let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            exif.extend([orientation, 0, 0, 0, 0, 0, 0, 0]);
            let mut metadata = segment(0xe1, &exif);
            for (sequence, chunk) in [(2, &profile[midpoint..]), (1, &profile[..midpoint])] {
                let mut payload = b"ICC_PROFILE\0".to_vec();
                payload.extend([sequence, 2]);
                payload.extend_from_slice(chunk);
                metadata.extend(segment(0xe2, &payload));
            }
            let normal = dir.path().join("normal.jpg");
            let late = dir.path().join("late.jpg");
            let mut bytes = original[..2].to_vec();
            bytes.extend_from_slice(&metadata);
            bytes.extend_from_slice(&original[2..]);
            std::fs::write(&normal, bytes).unwrap();
            let mut bytes = original[..position].to_vec();
            bytes.extend_from_slice(&metadata);
            // Exercise headers larger than the source BufReader capacity.
            bytes.extend(segment(0xed, &vec![0; 60000]));
            bytes.extend(segment(0xed, &vec![0; 60000]));
            bytes.extend_from_slice(&original[position..]);
            std::fs::write(&late, bytes).unwrap();
            let expected = decode(&normal, 1000, &config, &cancel).unwrap();
            let actual = decode(&late, 1000, &config, &cancel).unwrap();
            assert_eq!(actual.orientation, orientation);
            assert_eq!(actual.rgba, expected.rgba);
            assert_eq!(
                (actual.width, actual.height),
                (expected.width, expected.height)
            );
            // Compare the retained metadata parser against its original full input.
            let mut full = ImageReader::open(&late).unwrap().into_decoder().unwrap();
            assert_eq!(full.orientation().unwrap().to_exif(), orientation);
            assert_eq!(full.icc_profile().unwrap().unwrap(), profile);
        }
        let expected = decode(&plain, 1000, &config, &cancel).unwrap();
        let mut trailing = original;
        trailing.resize(trailing.len() + 2 * MIB, 0);
        std::fs::write(&plain, trailing).unwrap();
        let (actual, timings) = decode_profiled(&plain, 1000, &config, &cancel).unwrap();
        assert_eq!(actual.rgba, expected.rgba);
        assert!(timings.read_bytes <= 3 * 65536, "{}", timings.read_bytes);
    }

    #[test]
    fn embedded_icc_is_applied_to_main_and_thumbnail_and_cache_versions_are_validated() {
        use image::ImageEncoder;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p3.png");
        let pixels = image::RgbaImage::from_pixel(1200, 800, image::Rgba([180, 90, 30, 123]));
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder
            .set_icc_profile(moxcms::ColorProfile::new_display_p3().encode().unwrap())
            .unwrap();
        encoder
            .write_image(&pixels, 1200, 800, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let config = Config::default();
        let cancel = Cancel::default();
        let main = decode(&path, 1000, &config, &cancel).unwrap();
        let thumb = decode(&path, 320, &config, &cancel).unwrap();
        let (profiled, times) = decode_profiled(&path, 320, &config, &cancel).unwrap();
        assert_eq!(profiled.rgba, thumb.rgba);
        assert_eq!(
            times.stages()[..8].iter().map(|(_, us)| us).sum::<u64>(),
            times.total_us
        );
        assert!(times.read_bytes > 0);
        assert!(times.codec_us > 0 && times.resize_us > 0 && times.icc_us > 0);
        // Premultiplied SIMD alpha introduces small integer rounding differences
        // from the retained <=320px RGBA path, before the common ICC transform.
        assert!(
            main.rgba[..3]
                .iter()
                .zip(&thumb.rgba[..3])
                .all(|(a, b)| a.abs_diff(*b) <= 2)
        );
        assert_eq!(main.rgba[3], thumb.rgba[3]);
        assert!(main.rgba[0].abs_diff(194) <= 3);
        let encoded = encode_thumb(&main).unwrap();
        assert_eq!(decode_thumb(&encoded).unwrap().rgba, main.rgba);
        assert!(decode_thumb(&encoded[4..]).is_err());
        let encoded = encode_thumb(&thumb).unwrap();
        assert_eq!(decode_thumb(&encoded[4..]).unwrap().rgba, thumb.rgba);
        let mut truncated = encoded;
        truncated.pop();
        assert!(decode_thumb(&truncated).is_err());
    }
    #[test]
    fn icc_srgb_identity_p3_conversion_and_invalid_profile() {
        let mut pixels = image::RgbaImage::from_pixel(8, 4, image::Rgba([180, 90, 30, 123]));
        let srgb = moxcms::ColorProfile::new_srgb().encode().unwrap();
        apply_icc(&mut pixels, &srgb, &Cancel::default()).unwrap();
        assert!(pixels.pixels().all(|pixel| {
            pixel
                .0
                .iter()
                .zip([180, 90, 30, 123])
                .all(|(a, b)| a.abs_diff(b) <= 1)
        }));
        let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        apply_icc(&mut pixels, &p3, &Cancel::default()).unwrap();
        let pixel = pixels.get_pixel(0, 0).0;
        // Independent P3 -> XYZ -> sRGB matrix reference, rounded to 8-bit.
        assert!(
            pixel[0].abs_diff(194) <= 3 && pixel[1].abs_diff(83) <= 3 && pixel[2] <= 4,
            "{pixel:?}"
        );
        assert_eq!(pixel[3], 123);
        let gray = moxcms::ColorProfile::new_gray_with_gamma(2.2)
            .encode()
            .unwrap();
        let mut grayscale = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 51]));
        apply_icc(&mut grayscale, &gray, &Cancel::default()).unwrap();
        assert!(
            grayscale
                .pixels()
                .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2] && pixel[3] == 51)
        );
        assert!(apply_icc(&mut pixels, b"broken", &Cancel::default()).is_err());
    }
    #[test]
    fn size_presets_override_legacy_limits_and_preserve_full_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.png");
        image::RgbImage::from_pixel(120, 80, image::Rgb([12, 50, 90]))
            .save(&path)
            .unwrap();
        let mut config = Config {
            max_pixels: 1,
            max_dimension: 1,
            max_file_mib: 0,
            decode_mib: 0,
            ..Config::default()
        };
        assert!(decode(&path, 0, &config, &Cancel::default()).is_err());
        for preset in 1..=4 {
            config.image_size_limit = preset;
            let frame = decode(&path, 0, &config, &Cancel::default()).unwrap();
            assert_eq!((frame.width, frame.height), (120, 80));
        }
        config.image_size_limit = 1;
        assert!(check_dimensions(6000, 4000, &config).is_ok());
        assert!(check_dimensions(6001, 4000, &config).is_err());
        config.image_size_limit = 4;
        assert!(check_dimensions(100_000, 100_000, &config).is_ok());
        assert!(check_dimensions(0, 1, &config).is_err());
    }
    #[test]
    fn jpeg_full_resolution_request_falls_back_within_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jpg");
        image::RgbImage::from_pixel(2001, 1001, image::Rgb([12, 50, 90]))
            .save(&path)
            .unwrap();
        let config = Config {
            decode_mib: 16,
            ..Config::default()
        };
        let frame = decode(&path, 0, &config, &Cancel::default()).unwrap();
        assert_eq!((frame.original_width, frame.original_height), (2001, 1001));
        assert_eq!((frame.width, frame.height), (1001, 501));
        assert!(u64::from(frame.width) * u64::from(frame.height) * 12 <= 16 * MIB as u64);
        let full = decode(&path, 0, &Config::default(), &Cancel::default()).unwrap();
        assert_eq!((full.width, full.height), (2001, 1001));
        let too_small = Config {
            decode_mib: 0,
            ..Config::default()
        };
        assert!(decode(&path, 0, &too_small, &Cancel::default()).is_err());
    }
    #[test]
    fn cancellation_is_a_terminal_read_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("read.bin");
        std::fs::write(&p, [1, 2, 3]).unwrap();
        let cancel = Cancel::default();
        let mut r = reader(
            &p,
            &Config::default(),
            &cancel,
            Arc::new(IoStats::default()),
        )
        .unwrap();
        cancel.cancel();
        let error = r.read_exact(&mut [0; 3]).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
    }
    #[test]
    fn formats_bad_file_and_limits() {
        let d = tempfile::tempdir().unwrap();
        let img = image::RgbImage::from_pixel(80, 40, image::Rgb([12, 50, 90]));
        for ext in ["jpg", "png", "webp", "bmp", "tiff", "gif"] {
            let p = d.path().join(format!("test.{ext}"));
            img.save(&p).unwrap();
            let f = decode(&p, 32, &Config::default(), &Cancel::default()).unwrap();
            assert_eq!((f.width, f.height), (32, 16));
            assert_eq!(
                decode_thumb(&encode_thumb(&f).unwrap()).unwrap().rgba,
                f.rgba
            );
        }
        let p = d.path().join("bad.jpg");
        std::fs::write(&p, b"bad").unwrap();
        assert!(decode(&p, 32, &Config::default(), &Cancel::default()).is_err());
        assert!(check_dimensions(65535, 65535, &Config::default()).is_err());
    }
    #[test]
    fn exif_orientation_is_applied_to_preview_and_thumbnail() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("oriented.jpg");
        image::RgbImage::from_pixel(80, 40, image::Rgb([10, 20, 30]))
            .save(&p)
            .unwrap();
        let jpeg = std::fs::read(&p).unwrap();
        for orientation in 1..=8u8 {
            let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            exif.extend([orientation, 0, 0, 0, 0, 0, 0, 0]);
            let mut data = vec![0xff, 0xd8, 0xff, 0xe1];
            data.extend(((exif.len() + 2) as u16).to_be_bytes());
            data.extend(exif);
            data.extend(&jpeg[2..]);
            std::fs::write(&p, data).unwrap();
            let f = decode(&p, 32, &Config::default(), &Cancel::default()).unwrap();
            assert_eq!(f.orientation, orientation);
            assert_eq!(
                (f.width, f.height),
                if orientation >= 5 { (16, 32) } else { (32, 16) }
            );
            let large = decode(&p, 1600, &Config::default(), &Cancel::default()).unwrap();
            let (alternative, timing) =
                decode_profiled_backend(&p, 1600, &Config::default(), &Cancel::default(), true)
                    .unwrap();
            assert_eq!(
                (
                    alternative.width,
                    alternative.height,
                    alternative.orientation
                ),
                (large.width, large.height, large.orientation)
            );
            assert!(
                alternative
                    .rgba
                    .iter()
                    .zip(large.rgba.iter())
                    .all(|(a, b)| a.abs_diff(*b) <= 3)
            );
            assert_eq!(
                timing.stages()[..8].iter().map(|(_, us)| us).sum::<u64>(),
                timing.total_us
            );
            assert_eq!(large.orientation, f.orientation);
            let requests = [(1600, Cancel::default()), (32, Cancel::default())];
            let shared = decode_shared(
                &p,
                &requests,
                &Config::default(),
                &Cancel::default(),
                |index, frame| {
                    assert_eq!(frame.orientation, orientation);
                    if index == 0 {
                        assert_eq!(frame.rgba, large.rgba);
                    } else {
                        assert_eq!((frame.width, frame.height), (f.width, f.height));
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!((shared.source_opens, shared.codec_calls), (2, 1));
            assert_eq!(
                (large.original_width, large.original_height),
                if orientation >= 5 { (40, 80) } else { (80, 40) }
            );
        }
    }
}
