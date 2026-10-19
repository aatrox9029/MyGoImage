use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const LIMIT: u64 = 2 * 1024 * 1024;
struct Record {
    event: &'static str,
    monotonic_us: u128,
    trace: Option<(u64, u64, String)>,
    micros: u128,
    value: u64,
    ok: bool,
}
enum Packet {
    Record(Record),
    Flush(SyncSender<()>),
}
struct Inner {
    started: Instant,
    tx: SyncSender<Packet>,
    dropped: Arc<AtomicU64>,
    handle: Mutex<Option<JoinHandle<()>>>,
}
#[derive(Clone)]
pub struct Telemetry(Arc<Inner>);
impl Telemetry {
    pub fn new(base: &Path) -> std::io::Result<Self> {
        Self::with_limit(base, LIMIT)
    }
    fn with_limit(base: &Path, limit: u64) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(256);
        let dropped = Arc::new(AtomicU64::new(0));
        let losses = dropped.clone();
        let base = base.to_path_buf();
        let handle=std::thread::Builder::new().name("diagnostics".into()).spawn(move || {
            let mut writer=Writer::new(base,limit);
            while let Ok(mut packet)=rx.recv() {
                for batch in 0..32 {
                match packet {
                    Packet::Record(record)=>{
                        let line=serde_json::json!({"monotonic_us":record.monotonic_us,"trace":record.trace.map(|(request,generation,key)|serde_json::json!({"request":request,"generation":generation,"key":key})),"utc_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),"event":record.event,"duration_us":record.micros,"duration_ms":record.micros/1000,"value":record.value,"ok":record.ok,"dropped_total":losses.load(Ordering::Relaxed)}).to_string()+"\n";
                        if writer.append(line.as_bytes()).is_err(){losses.fetch_add(1,Ordering::Relaxed);}
                    }
                    Packet::Flush(ack)=>{let _=writer.flush();let _=ack.try_send(());}
                }
                if batch==31 {break;}
                match rx.try_recv(){Ok(next)=>packet=next,Err(_)=>break}
                }
                let _=writer.flush();
            }
            let _=writer.flush();
        })?;
        Ok(Self(Arc::new(Inner {
            started: Instant::now(),
            tx,
            dropped,
            handle: Mutex::new(Some(handle)),
        })))
    }
    pub fn record(&self, event: &'static str, micros: u128, value: u64, ok: bool) {
        self.send(event, micros, value, ok, None);
    }
    pub fn trace(
        &self,
        event: &'static str,
        request: u64,
        generation: u64,
        key: &str,
        elapsed: u128,
        ok: bool,
    ) {
        self.send(
            event,
            elapsed,
            0,
            ok,
            Some((request, generation, key.to_string())),
        );
    }
    fn send(
        &self,
        event: &'static str,
        micros: u128,
        value: u64,
        ok: bool,
        trace: Option<(u64, u64, String)>,
    ) {
        if self
            .0
            .tx
            .try_send(Packet::Record(Record {
                event,
                monotonic_us: self.0.started.elapsed().as_micros(),
                trace,
                micros,
                value,
                ok,
            }))
            .is_err()
        {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn dropped(&self) -> u64 {
        self.0.dropped.load(Ordering::Relaxed)
    }
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        self.0.tx.try_send(Packet::Flush(tx)).is_ok() && rx.recv_timeout(timeout).is_ok()
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        // Sending is never blocking. Detached writer owns at most 256 records and exits
        // when this final sender is dropped. Call flush for a bounded shutdown barrier.
        if let Some(handle) = self.handle.get_mut().unwrap().take()
            && handle.is_finished()
        {
            let _ = handle.join();
        }
    }
}
struct Writer {
    base: PathBuf,
    file: Option<BufWriter<File>>,
    size: u64,
    limit: u64,
    failed: bool,
    lock: Option<File>,
}
impl Writer {
    fn new(base: PathBuf, limit: u64) -> Self {
        Self {
            base,
            file: None,
            size: 0,
            limit,
            failed: false,
            lock: None,
        }
    }
    fn append(&mut self, line: &[u8]) -> std::io::Result<()> {
        let result = self.append_inner(line);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn append_inner(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.failed || line.len() as u64 > self.limit {
            return Err(std::io::Error::other("Diagnostics disabled"));
        }
        if self.lock.is_none() {
            std::fs::create_dir_all(&self.base)?;
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.base.join("diagnostics.lock"))?;
            lock.try_lock().map_err(std::io::Error::other)?;
            self.lock = Some(lock);
        }
        let path = self.base.join("diagnostics.jsonl");
        if self.file.is_none() {
            let file = OpenOptions::new().append(true).create(true).open(&path)?;
            self.size = file.metadata()?.len();
            self.file = Some(BufWriter::new(file));
        }
        if self.size + line.len() as u64 > self.limit {
            self.flush()?;
            self.file.take();
            let previous = self.base.join("diagnostics.previous.jsonl");
            match std::fs::remove_file(&previous) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            std::fs::rename(&path, previous)?;
            self.file = Some(BufWriter::new(
                OpenOptions::new().create_new(true).write(true).open(path)?,
            ));
            self.size = 0;
        }
        self.file.as_mut().unwrap().write_all(line)?;
        self.size += line.len() as u64;
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_replaces_previous_and_failure_never_extends_full_log() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("diagnostics.previous.jsonl"), b"old").unwrap();
        let mut writer = Writer::new(dir.path().into(), 64);
        writer.append(&[b'a'; 40]).unwrap();
        writer.append(&[b'b'; 40]).unwrap();
        writer.flush().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("diagnostics.previous.jsonl")).unwrap(),
            vec![b'a'; 40]
        );
        std::fs::remove_file(dir.path().join("diagnostics.previous.jsonl")).unwrap();
        std::fs::create_dir(dir.path().join("diagnostics.previous.jsonl")).unwrap();
        assert!(writer.append(&[b'c'; 40]).is_err());
        assert!(writer.append(b"x").is_err());
        assert_eq!(
            std::fs::metadata(dir.path().join("diagnostics.jsonl"))
                .unwrap()
                .len(),
            40
        );
    }
    #[test]
    fn parallel_events_are_bounded_and_flushable() {
        let dir = tempfile::tempdir().unwrap();
        let log = Telemetry::with_limit(dir.path(), 1024).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let log = &log;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        log.record("test", 5, 1, true);
                    }
                });
            }
        });
        // A full queue may reject a flush request; retry after allowing the writer to drain.
        for _ in 0..100 {
            if log.flush(Duration::from_millis(20)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(log.flush(Duration::from_secs(1)));
        for name in ["diagnostics.jsonl", "diagnostics.previous.jsonl"] {
            assert!(std::fs::metadata(dir.path().join(name)).unwrap().len() <= 1024);
        }
    }
}
