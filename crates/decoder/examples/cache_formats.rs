use image::ImageEncoder;
use mygo_core::{Cancel, Config};
use mygo_decoder::{decode, decode_thumb, encode_thumb};
use std::{io::Cursor, path::PathBuf, time::Instant};
fn main() -> anyhow::Result<()> {
    let path: PathBuf = std::env::args_os().nth(1).expect("image path").into();
    let frame = decode(&path, 320, &Config::default(), &Cancel::default())?;
    let raw = encode_thumb(&frame)?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        &frame.rgba,
        frame.width,
        frame.height,
        image::ExtendedColorType::Rgba8,
    )?;
    let mut raw_times = Vec::new();
    let mut png_times = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        let restored = decode_thumb(&raw)?;
        std::hint::black_box(restored);
        raw_times.push(start.elapsed().as_secs_f64() * 1000.);
        let start = Instant::now();
        let restored = image::ImageReader::new(Cursor::new(&png))
            .with_guessed_format()?
            .decode()?
            .into_rgba8();
        assert_eq!(restored.as_raw(), &*frame.rgba);
        png_times.push(start.elapsed().as_secs_f64() * 1000.);
    }
    raw_times.sort_by(f64::total_cmp);
    png_times.sort_by(f64::total_cmp);
    println!(
        "{{\"width\":{},\"height\":{},\"raw_bytes\":{},\"png_bytes\":{},\"raw_decode_p95_ms\":{},\"png_decode_p95_ms\":{}}}",
        frame.width,
        frame.height,
        raw.len(),
        png.len(),
        raw_times[94],
        png_times[94]
    );
    Ok(())
}
