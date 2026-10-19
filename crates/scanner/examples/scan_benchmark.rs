use mygo_core::Cancel;
use mygo_scanner::ScanEvent;
use std::time::Instant;
fn main() {
    let p = std::path::PathBuf::from(std::env::args_os().nth(1).expect("directory"));
    let (tx, rx) = std::sync::mpsc::sync_channel(2);
    let t = Instant::now();
    std::thread::spawn(move || mygo_scanner::scan(&p, true, &Cancel::default(), &tx));
    let mut count = 0;
    let mut first = None;
    let mut errors = 0;
    for event in rx {
        match event {
            ScanEvent::Batch(es) => {
                if first.is_none() {
                    first = Some(t.elapsed().as_secs_f64() * 1000.);
                }
                count += es.len();
            }
            ScanEvent::Done { errors: e, .. } => errors = e,
        }
    }
    println!(
        "{{\"entries\":{count},\"first_batch_ms\":{:.3},\"total_ms\":{:.3},\"errors\":{errors}}}",
        first.unwrap_or(0.),
        t.elapsed().as_secs_f64() * 1000.
    );
}
