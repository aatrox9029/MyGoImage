use mygo_core::{Cancel, Config};
use mygo_decoder::decode_profiled_backend;
use std::{path::PathBuf, time::Instant};

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 6,
        "profile IMAGE TARGET ITERATIONS WORKERS BACKEND"
    );
    let path = PathBuf::from(&args[1]);
    let target: u32 = args[2].parse()?;
    let iterations: usize = args[3].parse()?;
    let workers: usize = args[4].parse()?;
    anyhow::ensure!(
        (1..=2).contains(&workers) && iterations > 0,
        "workers must be 1 or 2; iterations > 0"
    );
    anyhow::ensure!(args[5] == "native" || args[5] == "image", "unknown backend");
    let image_jpeg = args[5] == "image";
    // No RAM/disk image cache. Inputs are pre-read by the orchestrator: OS warm.
    let started = Instant::now();
    let samples = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers).map(|worker| {
            let path = &path;
            scope.spawn(move || -> anyhow::Result<Vec<String>> {
                let mut rows = Vec::new();
                for index in (worker..iterations).step_by(workers) {
                    let config = Config { image_size_limit: 1, ..Config::default() };
                    let (frame, times) = decode_profiled_backend(path, target, &config, &Cancel::default(), image_jpeg)?;
                    std::hint::black_box(&frame);
                    let fields = times.stages().iter().map(|(key, us)| format!("\"{key}\":{us}")).collect::<Vec<_>>().join(",");
                    rows.push(format!("{{\"index\":{index},\"width\":{},\"height\":{},\"read_bytes\":{},\"stages_us\":{{{fields}}}}}", frame.width, frame.height, times.read_bytes));
                }
                Ok(rows)
            })
        }).collect();
        let mut rows = Vec::new();
        for handle in handles {
            rows.extend(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("profile worker panicked"))??,
            );
        }
        Ok::<_, anyhow::Error>(rows)
    })?;
    println!(
        "{{\"wall_us\":{},\"workers\":{workers},\"iterations\":{iterations},\"target\":{target},\"samples\":[{}]}}",
        started.elapsed().as_micros(),
        samples.join(",")
    );
    Ok(())
}
