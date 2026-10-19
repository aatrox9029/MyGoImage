use mygo_core::{Cancel, Entry, ImageFormats};
use std::{
    path::Path,
    sync::mpsc::SyncSender,
    time::{Duration, UNIX_EPOCH},
};

pub enum ScanEvent {
    Batch(Vec<Entry>),
    Done { errors: usize, complete: bool },
}
/// Bounded batches; symlinks/junctions are never followed. No image is opened.
pub fn scan(root: &Path, recursive: bool, cancel: &Cancel, tx: &SyncSender<ScanEvent>) {
    scan_excluding(root, recursive, cancel, tx, None);
}
pub fn scan_excluding(
    root: &Path,
    recursive: bool,
    cancel: &Cancel,
    tx: &SyncSender<ScanEvent>,
    excluded: Option<&Path>,
) {
    scan_excluding_notifying(root, recursive, cancel, tx, excluded, &|| {});
}
pub fn scan_excluding_notifying(
    root: &Path,
    recursive: bool,
    cancel: &Cancel,
    tx: &SyncSender<ScanEvent>,
    excluded: Option<&Path>,
    wake: &dyn Fn(),
) {
    scan_filtered(
        root,
        recursive,
        cancel,
        tx,
        excluded,
        wake,
        &ImageFormats::default(),
    );
}
pub fn scan_filtered(
    root: &Path,
    recursive: bool,
    cancel: &Cancel,
    tx: &SyncSender<ScanEvent>,
    excluded: Option<&Path>,
    wake: &dyn Fn(),
    formats: &ImageFormats,
) {
    let mut batch = Vec::with_capacity(256);
    let mut errors = 0;
    let mut first = true;
    let first_started = std::time::Instant::now();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_open(8)
        .max_depth(if recursive { usize::MAX } else { 1 });
    for result in walker
        .into_iter()
        .filter_entry(|e| excluded.is_none_or(|p| e.path() != p))
    {
        if cancel.cancelled() {
            return;
        }
        let item = match result {
            Ok(e) => e,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        if !item.file_type().is_file() || !formats.allows(item.path()) {
            continue;
        }
        let meta = match item.metadata() {
            Ok(m) => m,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let relative = item
            .path()
            .strip_prefix(root)
            .unwrap_or(item.path())
            .to_string_lossy()
            .to_string();
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        batch.push(Entry {
            id: 0,
            relative,
            size: meta.len(),
            modified_ns,
        });
        if batch.len() == 256
            || (first
                && (batch.len() >= 16 || first_started.elapsed() >= Duration::from_millis(20)))
        {
            first = false;
            if !send(
                tx,
                ScanEvent::Batch(std::mem::replace(&mut batch, Vec::with_capacity(256))),
                cancel,
                wake,
            ) {
                return;
            }
            // Give interactive reads a chance on removable media.
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    if !batch.is_empty() && !send(tx, ScanEvent::Batch(batch), cancel, wake) {
        return;
    }
    let _ = send(
        tx,
        ScanEvent::Done {
            errors,
            complete: errors == 0,
        },
        cancel,
        wake,
    );
}
fn send(
    tx: &SyncSender<ScanEvent>,
    mut event: ScanEvent,
    cancel: &Cancel,
    wake: &dyn Fn(),
) -> bool {
    loop {
        if cancel.cancelled() {
            return false;
        }
        match tx.try_send(event) {
            Ok(()) => {
                wake();
                return true;
            }
            Err(std::sync::mpsc::TrySendError::Full(e)) => {
                event = e;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return false,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_trash_is_excluded_from_recursive_browsing() {
        let d = tempfile::tempdir().unwrap();
        let trash = d.path().join("trash");
        std::fs::create_dir_all(trash.join("batch")).unwrap();
        std::fs::write(trash.join("batch/deleted.jpg"), b"deleted").unwrap();
        std::fs::write(d.path().join("kept.jpg"), b"kept").unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        scan_excluding(d.path(), true, &Cancel::default(), &tx, Some(&trash));
        drop(tx);
        let entries: Vec<_> = rx
            .into_iter()
            .filter_map(|e| {
                if let ScanEvent::Batch(b) = e {
                    Some(b)
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].relative, "kept.jpg");
    }
    #[test]
    fn incremental_and_no_decode() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..600 {
            std::fs::write(d.path().join(format!("{i}.jpg")), b"not a real image").unwrap();
        }
        std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::sync_channel(2);
            let p = d.path();
            s.spawn(move || scan(p, true, &Cancel::default(), &tx));
            let mut sizes = vec![];
            for e in rx {
                if let ScanEvent::Batch(b) = e {
                    sizes.push(b.len());
                }
            }
            assert!((1..=16).contains(&sizes[0]));
            assert_eq!(sizes.iter().sum::<usize>(), 600);
            assert!(sizes[1..sizes.len() - 1].iter().all(|n| *n == 256));
            assert!(sizes.last().is_some_and(|n| *n <= 256));
        });
    }
    #[test]
    fn cancelled_scan_does_not_wait_for_full_consumer() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..600 {
            std::fs::write(d.path().join(format!("{i}.jpg")), b"bad").unwrap();
        }
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let cancel = Cancel::default();
        let c = cancel.clone();
        let p = d.path().to_path_buf();
        let h = std::thread::spawn(move || scan(&p, true, &c, &tx));
        std::thread::sleep(Duration::from_millis(30));
        cancel.cancel();
        let start = std::time::Instant::now();
        h.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
