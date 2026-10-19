use mygo_cache::ByteLru;
use std::time::Instant;
fn main() {
    let mut samples = Vec::new();
    for _ in 0..30 {
        let mut cache = ByteLru::new(5000 * 4096);
        for key in 0..5000 {
            cache.put(key, (), 4096);
        }
        let start = Instant::now();
        cache.put(6000, (), 5000 * 4096);
        samples.push(start.elapsed().as_secs_f64() * 1000.);
        assert_eq!(cache.used(), 5000 * 4096);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{{\"items\":5000,\"eviction_p50_ms\":{},\"eviction_p95_ms\":{}}}",
        samples[15], samples[28]
    );
}
