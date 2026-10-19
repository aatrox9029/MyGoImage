//! 10k/100k filesystem maintenance benchmark. Uses an isolated temporary cache.
use anyhow::{Result, ensure};
use mygo_cache::DiskCache;
use std::{io::Write, time::Instant};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 3, "maintenance COUNT ROUNDS");
    let count: u64 = args[1].parse()?;
    let rounds: usize = args[2].parse()?;
    ensure!(
        count > 0 && rounds > 0,
        "positive count and rounds required"
    );
    let dir = tempfile::tempdir()?;
    let start = Instant::now();
    for index in 0..count {
        let mut file = std::fs::File::create(dir.path().join(format!("{index:064x}.thumb")))?;
        file.write_all(&[0u8; 64])?;
    }
    println!("stage,round,count,elapsed_us");
    println!("seed,0,{count},{}", start.elapsed().as_micros());
    for round in 0..rounds {
        let cache = DiskCache::open_lazy(dir.path(), count * 64)?;
        let start = Instant::now();
        cache.reconcile()?;
        println!("reconcile,{round},{count},{}", start.elapsed().as_micros());
    }
    let cache = DiskCache::new(dir.path(), count * 64)?;
    let start = Instant::now();
    cache.put("overflow", &[1; 64])?;
    println!("prune,0,{count},{}", start.elapsed().as_micros());
    let total: u64 = std::fs::read_dir(dir.path())?
        .map(|e| e.and_then(|e| e.metadata()).map(|m| m.len()))
        .sum::<std::io::Result<u64>>()?;
    ensure!(total <= count * 64 * 9 / 10, "prune exceeded low watermark");
    Ok(())
}
