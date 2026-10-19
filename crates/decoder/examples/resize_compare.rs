#[path = "resize_support/gpu.rs"]
mod gpu;
use anyhow::{Context, Result};
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::DynamicImage;
use mygo_core::{Cancel, Config};
use mygo_decoder::decode_profiled_resize;
use serde_json::json;
use std::{path::PathBuf, time::Instant};

fn dimensions(image: &DynamicImage, target: u32) -> (u32, u32) {
    let ratio = f64::from(target) / f64::from(image.width().max(image.height()));
    (
        ((f64::from(image.width()) * ratio).round() as u32).max(1),
        ((f64::from(image.height()) * ratio).round() as u32).max(1),
    )
}
fn resize(
    image: DynamicImage,
    target: u32,
    method: &str,
    gpu: Option<&gpu::Gpu>,
) -> Result<DynamicImage> {
    let (w, h) = dimensions(&image, target);
    match method {
        "production" => mygo_decoder::resize_cpu(
            image,
            target,
            &Config {
                image_size_limit: 1,
                ..Config::default()
            },
            &Cancel::default(),
        ),
        "legacy" => Ok(image.thumbnail(target, target)),
        "image-triangle" => Ok(image.resize_exact(w, h, image::imageops::FilterType::Triangle)),
        "gpu-triangle" => gpu.context("GPU not initialized")?.resize(image, w, h),
        _ => {
            let filter = match method {
                "fir-box" => FilterType::Box,
                "fir-triangle" => FilterType::Bilinear,
                "fir-lanczos3" => FilterType::Lanczos3,
                _ => anyhow::bail!("unknown resize method"),
            };
            let mut destination = DynamicImage::new(w, h, image.color());
            // Keep native pixel depth; premultiply/unpremultiply alpha by default.
            // Fresh scratch buffers per frame, with runtime SIMD detection.
            Resizer::new().resize(
                &image,
                &mut destination,
                &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)),
            )?;
            Ok(destination)
        }
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 5,
        "resize_compare INPUT_DIR OUTPUT_DIR cpu|gpu|production ITERATIONS"
    );
    let input = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    let count: usize = args[4].parse()?;
    anyhow::ensure!(count > 0, "iterations > 0");
    std::fs::create_dir(&output)?;
    let methods = match args[3].as_str() {
        "production" => vec!["production"],
        "cpu" => vec![
            "legacy",
            "image-triangle",
            "fir-box",
            "fir-triangle",
            "fir-lanczos3",
        ],
        "gpu" => vec!["fir-triangle", "gpu-triangle"],
        _ => anyhow::bail!("unknown phase"),
    };
    let init = Instant::now();
    let gpu = if args[3] == "gpu" {
        Some(gpu::Gpu::new()?)
    } else {
        None
    };
    let init_us = init.elapsed().as_micros();
    let mut files: Vec<_> = std::fs::read_dir(input)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    files.retain(|p| p.is_file());
    files.sort();
    let config = Config {
        image_size_limit: 1,
        ..Config::default()
    };
    let mut cases = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .context("file name")?
            .to_string_lossy()
            .to_string();
        for target in [320, 1600] {
            let mut rows = Vec::new();
            let mut warmups = Vec::new();
            for method in &methods {
                let (frame, times) = decode_profiled_resize(
                    &path,
                    target,
                    &config,
                    &Cancel::default(),
                    false,
                    &mut |image, t| resize(image, t, method, gpu.as_ref()),
                )?;
                let save = output.join(format!("{name}-{target}-{method}.png"));
                image::save_buffer(
                    save,
                    &frame.rgba,
                    frame.width,
                    frame.height,
                    image::ExtendedColorType::Rgba8,
                )?;
                warmups.push(json!({"method":method, "total_us":times.total_us, "resize_us":times.resize_us, "output_dimensions":[frame.width,frame.height], "original_dimensions":[frame.original_width,frame.original_height], "orientation":frame.orientation}));
            }
            // Rotate algorithm order every repetition, in the same process.
            for iteration in 0..count {
                for offset in 0..methods.len() {
                    let method = methods[(iteration + offset) % methods.len()];
                    let (frame, times) = decode_profiled_resize(
                        &path,
                        target,
                        &config,
                        &Cancel::default(),
                        false,
                        &mut |image, t| resize(image, t, method, gpu.as_ref()),
                    )?;
                    std::hint::black_box(frame);
                    rows.push(json!({"iteration":iteration, "method":method, "stages_us":times.stages().into_iter().collect::<std::collections::BTreeMap<_,_>>() }));
                }
            }
            cases.push(json!({"name":name, "target":target, "warmups":warmups, "samples":rows}));
            eprintln!("completed {name} target={target}");
        }
    }
    let report = json!({"phase":args[3], "gpu_init_us":init_us, "adapter":gpu.as_ref().map(|g|&g.info), "iterations":count, "cases":cases, "scope":"Source file -> decode with native JPEG scale -> orientation -> resize including conversion/allocations/upload/compute/GPU wait/readback -> RGBA -> ICC -> CPU Frame. Warm GPU context, fresh frame buffers; setup and first calls recorded separately; no Slint presentation."});
    std::fs::write(output.join("raw.json"), serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}
