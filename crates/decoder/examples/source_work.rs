//! Isolated R05/R07 experiment; production scheduling remains unchanged.
use anyhow::{Result, ensure};
use mygo_core::{Cancel, Config, MIB};
use mygo_decoder::{decode_profiled_backend, decode_shared};
use serde_json::json;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    time::Instant,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() >= 6,
        "source_work serial|shared|parallel|image-jpeg WORKERS GLOBAL_MIB ROUNDS FILE..."
    );
    let mode = &args[1];
    let workers: usize = args[2].parse()?;
    let budget: usize = args[3].parse()?;
    let rounds: usize = args[4].parse()?;
    ensure!(
        (1..=2).contains(&workers) && budget >= workers && rounds > 0,
        "invalid limits"
    );
    ensure!(
        ["serial", "shared", "parallel", "image-jpeg"].contains(&mode.as_str()),
        "invalid mode"
    );
    let files: Vec<PathBuf> = args[5..].iter().map(PathBuf::from).collect();
    let config = Config {
        decode_mib: budget / workers,
        ..Config::default()
    };
    let mut samples = Vec::new();
    for round in 0..rounds {
        let start = Instant::now();
        let mut outputs = Vec::new();
        if mode == "parallel" {
            // Claim worst-case per-job budget before dispatch. Work completes in
            // bounded waves; this measures throughput, not production Main priority.
            for wave in files.chunks(workers) {
                let results = std::thread::scope(|scope| {
                    let handles: Vec<_> = wave
                        .iter()
                        .map(|path| {
                            let config = &config;
                            scope.spawn(move || run(path, config, false, false))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().unwrap())
                        .collect::<Result<Vec<_>>>()
                })?;
                outputs.extend(results);
            }
        } else {
            for path in &files {
                outputs.push(run(path, &config, mode == "shared", mode == "image-jpeg")?);
            }
        }
        samples.push(
            json!({"round": round, "wall_us": start.elapsed().as_micros(), "files": outputs}),
        );
    }
    println!(
        "{}",
        json!({"mode": mode, "workers": workers, "global_working_bytes": budget * MIB,
        "per_job_bytes": config.decode_mib * MIB, "samples": samples})
    );
    Ok(())
}
fn run(
    path: &std::path::Path,
    config: &Config,
    shared: bool,
    image_jpeg: bool,
) -> Result<serde_json::Value> {
    let targets: Vec<_> = [1600, 1000, 640, 320]
        .into_iter()
        .map(|n| (n, Cancel::default()))
        .collect();
    let mut hashes = Vec::new();
    let started = Instant::now();
    let mut emit = |index: usize, frame: mygo_decoder::Frame| {
        let ready_us = started.elapsed().as_micros();
        let mut hash = DefaultHasher::new();
        frame.rgba.hash(&mut hash);
        hashes.push(json!({"target": targets[index].0, "width": frame.width,
            "height": frame.height, "hash": hash.finish(), "ready_us": ready_us}));
        Ok(())
    };
    let (mut read_bytes, mut codec_us, mut source_opens, mut codec_calls) = (0, 0, 0, 0);
    if shared {
        let timing = decode_shared(path, &targets, config, &Cancel::default(), &mut emit)?;
        read_bytes += timing.read_bytes;
        codec_us += timing.codec_us;
        source_opens += timing.source_opens;
        codec_calls += timing.codec_calls;
    } else {
        for (index, (target, cancel)) in targets.iter().enumerate() {
            let (frame, timing) =
                decode_profiled_backend(path, *target, config, cancel, image_jpeg)?;
            read_bytes += timing.read_bytes;
            codec_us += timing.codec_us;
            source_opens += timing.source_opens;
            codec_calls += timing.codec_calls;
            emit(index, frame)?;
        }
    }
    Ok(
        json!({"path": path, "read_bytes": read_bytes, "codec_us": codec_us,
        "codec_calls": codec_calls, "source_opens": source_opens, "outputs": hashes}),
    )
}
