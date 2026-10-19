use mygo_cache::DiskCache;
use std::time::Instant;
fn percentile(values: &mut [f64], q: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * q).ceil() as usize]
}
fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let budget = 48 * 1024 * 1024;
    let cache = DiskCache::new(dir.path(), budget)?;
    let bytes = vec![42; 4096];
    let start = Instant::now();
    for index in 0..10000 {
        cache.put(&format!("{index:06}"), &bytes)?;
    }
    let initial_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut hits = Vec::new();
    for index in 0..1000 {
        let start = Instant::now();
        assert_eq!(cache.get(&format!("{index:06}"), 4096).unwrap(), bytes);
        hits.push(start.elapsed().as_secs_f64() * 1000.);
    }
    let mut writes = Vec::new();
    for index in 10000..13000 {
        let start = Instant::now();
        cache.put(&format!("{index:06}"), &bytes)?;
        writes.push(start.elapsed().as_secs_f64() * 1000.);
    }
    drop(cache);
    let start = Instant::now();
    let _cache = DiskCache::new(dir.path(), budget)?;
    let reopen_ms = start.elapsed().as_secs_f64() * 1000.;
    let total: u64 = std::fs::read_dir(dir.path())?
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= budget);
    println!(
        "{{\"written_files\":13000,\"budget_bytes\":{budget},\"remaining_bytes\":{total},\"initial_write_ms\":{initial_ms},\"hit_p95_ms\":{},\"put_p95_ms\":{},\"put_max_ms\":{},\"reopen_ms\":{reopen_ms}}}",
        percentile(&mut hits, 0.95),
        percentile(&mut writes, 0.95),
        writes.iter().copied().fold(0., f64::max)
    );
    Ok(())
}
