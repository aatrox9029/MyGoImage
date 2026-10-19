use mygo_core::{Entry, Sort};
use mygo_index::Index;
use std::time::Instant;
fn percentile(samples: &mut [f64], q: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() - 1) as f64 * q).ceil() as usize]
}
fn main() -> anyhow::Result<()> {
    let arg = std::env::args_os().nth(1).expect("database path argument");
    let path = std::path::Path::new(&arg);
    let mut db = Index::open(path)?;
    let requested: usize = std::env::args()
        .nth(2)
        .map(|s| s.parse().expect("entry count"))
        .unwrap_or(100000);
    assert!(requested >= 1000);
    db.scope("synthetic-benchmark")?;
    let now = Instant::now();
    if db.count()? == 0 {
        for chunk in 0..requested.div_ceil(1000) {
            let batch = (0..1000.min(requested - chunk * 1000))
                .map(|i| {
                    let n = chunk * 1000 + i;
                    Entry {
                        id: 0,
                        relative: format!("相片/{n:06}.jpg"),
                        size: (10_000_000 + n) as u64,
                        modified_ns: n as i64,
                    }
                })
                .collect::<Vec<_>>();
            db.upsert(&batch)?;
        }
    }
    let index_ms = now.elapsed().as_secs_f64() * 1000.;
    let mut times = Vec::new();
    let mut positions = Vec::new();
    for i in 0..500 {
        let n = (i * 7919) % (db.count()? - 32);
        let t = Instant::now();
        let p = db.page(
            n,
            32,
            if i % 3 == 0 {
                Sort::Size
            } else if i % 3 == 1 {
                Sort::Name
            } else {
                Sort::Modified
            },
            i % 2 == 0,
        )?;
        assert_eq!(p.len(), 32);
        times.push(t.elapsed().as_secs_f64() * 1000.);
        if i < 60 {
            let t = Instant::now();
            assert_eq!(
                db.position(
                    p[0].id,
                    if i % 3 == 0 {
                        Sort::Size
                    } else if i % 3 == 1 {
                        Sort::Name
                    } else {
                        Sort::Modified
                    },
                    i % 2 == 0
                )?,
                Some(n)
            );
            positions.push(t.elapsed().as_secs_f64() * 1000.);
        }
    }
    let mut seeks = Vec::new();
    for sort in [Sort::Name, Sort::Modified, Sort::Size] {
        for desc in [false, true] {
            let anchor = db.page(requested - 64, 1, sort, desc)?.remove(0);
            let expected = db.page(requested - 63, 32, sort, desc)?;
            for _ in 0..31 {
                let started = Instant::now();
                let actual = db.after(&anchor, 32, sort, desc)?;
                seeks.push(started.elapsed().as_secs_f64() * 1000.);
                assert_eq!(
                    actual.iter().map(|e| e.id).collect::<Vec<_>>(),
                    expected.iter().map(|e| e.id).collect::<Vec<_>>()
                );
            }
        }
    }
    let seek_p95 = percentile(&mut seeks, 0.95);
    let (p50, p95, p99) = (
        percentile(&mut times, 0.5),
        percentile(&mut times, 0.95),
        percentile(&mut times, 0.99),
    );
    let position_p95 = percentile(&mut positions, 0.95);
    println!(
        "{{\"entries\":{},\"index_ms\":{index_ms:.3},\"cursor_p95_ms\":{seek_p95:.3},\"position_p95_ms\":{position_p95:.3},\"page_p50_ms\":{p50:.3},\"page_p95_ms\":{p95:.3},\"page_p99_ms\":{p99:.3}}}",
        db.count()?
    );
    Ok(())
}
