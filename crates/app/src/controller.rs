use crate::{
    background::LatestWorker,
    pixel_budget::{PixelBudget, PixelPermit},
    transport::{CommandReceiver, EventSender},
};
use anyhow::{Context, Result};
use mygo_cache::{ByteLru, DiskCache};
use mygo_core::{Cancel, Config, Entry, MIB, Sort};
use mygo_decoder::Frame;
use mygo_index::Index;
use mygo_scanner::ScanEvent;
use mygo_scheduler::{Job, Priority, Scheduler};
type ImageInfoResult = (u64, Result<Vec<crate::image_info::Field>>);
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};
// Preferred 1280px client width leaves just over 1000px for the viewer. Start
// at the next bucket so first layout doesn't immediately restart the preview.
pub(crate) const INITIAL_PREVIEW: u32 = 1280;
pub enum Action {
    Picker(bool),
    Open(PathBuf, bool),
    Rescan(bool),
    Sort(i32, bool),
    Select(usize),
    Navigate(i32),
    Visible(usize),
    VisibleCount(usize),
    ThumbnailSize(u32),
    PreviewSize(u32),
    High,
    RevealFile,
    Cancel,
    Retry,
    Clear,
    Settings(Config),
    CopyCurrent,
    Details(bool),
    PickCopyFolder,
    ImageShown(u64, i64),
    Trash(i64),
    OpenTrash,
    Shutdown,
    Probe(u64),
}
#[derive(Debug)]
pub enum Event {
    Settings(Config, String),
    Reset {
        generation: u64,
        folder: String,
        wheel: bool,
    },
    Count(usize),
    Page {
        generation: u64,
        start: usize,
        entries: Vec<(i64, String)>,
    },
    Thumbnail {
        _permit: Option<PixelPermit>,
        generation: u64,
        index: usize,
        id: i64,
        frame: Option<Frame>,
    },
    Selection {
        generation: u64,
        id: i64,
        index: usize,
    },
    Relocate {
        id: i64,
        index: usize,
    },
    Main {
        _permit: Option<PixelPermit>,
        generation: u64,
        request: u64,
        key: String,
        enqueued: Instant,
        frame: Frame,
    },
    Details {
        generation: u64,
        fields: Vec<crate::image_info::Field>,
    },
    Notice(String),
    Status(String),
    Error(String),
    Idle,
    Probe(u64),
    ScanComplete(usize),
}
#[derive(Clone)]
struct DecodeTask {
    config: Config,
    enqueued: Instant,
    cache_epoch: u64,
    entry: Entry,
    path: PathBuf,
    scope: String,
    target: u32,
    index: usize,
    list: u64,
    main: bool,
}
impl DecodeTask {
    fn shareable(&self) -> bool {
        self.path.extension().is_some_and(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "png" | "webp" | "bmp" | "gif" | "tif" | "tiff"
            )
        })
    }
    fn same_source(&self, other: &Self) -> bool {
        self.path == other.path
            && self.scope == other.scope
            && self.entry.size == other.entry.size
            && self.entry.modified_ns == other.entry.modified_ns
            && self.cache_epoch == other.cache_epoch
            && self.config.image_limits() == other.config.image_limits()
            && self.config.decode_timeout_ms == other.config.decode_timeout_ms
    }
    fn key(&self) -> String {
        let scope = format!(
            "{}:{}:size-{}:{:?}",
            self.scope,
            mygo_decoder::PIXEL_PIPELINE_VERSION,
            self.config.image_size_limit,
            self.config.image_limits()
        );
        mygo_cache::key(&scope, &self.entry, self.target, 0)
    }
}
struct Subscriber {
    task: DecodeTask,
    generation: u64,
    cancel: Cancel,
}
struct Pending {
    request: u64,
    cancel: Cancel,
    subscribers: Vec<Subscriber>,
}
impl Pending {
    fn subscribe(&mut self, subscriber: Subscriber) -> bool {
        if self.cancel.cancelled() {
            return false;
        }
        self.subscribers.retain(|s| !s.cancel.cancelled());
        if let Some(old) = self.subscribers.iter_mut().find(|s| {
            s.generation == subscriber.generation
                && s.task.index == subscriber.task.index
                && s.task.main == subscriber.task.main
        }) {
            *old = subscriber;
        } else if self.subscribers.len() < 64 {
            self.subscribers.push(subscriber);
        } else {
            return false;
        }
        true
    }
    fn retain_live(&mut self) -> bool {
        if self.cancel.cancelled() {
            return false;
        }
        self.subscribers.retain(|s| !s.cancel.cancelled());
        if self.subscribers.is_empty() {
            self.cancel.cancel();
            false
        } else {
            true
        }
    }
}
struct Decoded {
    _permit: Option<PixelPermit>,
    request: u64,
    task: DecodeTask,
    generation: u64,
    result: Result<Frame>,
    elapsed: u128,
}
type Ram = Arc<Mutex<ByteLru<String, Frame>>>;
fn preview_cache_budget(config: &Config, target: u32) -> usize {
    let history = if config.preload_enabled {
        config.preload_count + config.preload_count / 2 + 3
    } else {
        3
    };
    config.main_mib.saturating_mul(MIB).min(
        (target as usize)
            .saturating_pow(2)
            .saturating_mul(4)
            .saturating_mul(history.max(8)),
    )
}
fn cached_frame(
    task: &DecodeTask,
    cache: &Ram,
    disk: &Mutex<DiskCache>,
) -> (Option<Frame>, &'static str) {
    let key = task.key();
    let mut hit = cache.lock().unwrap().get(&key);
    let mut origin = if hit.is_some() {
        "cache-ram-hit"
    } else {
        "cache-source-miss"
    };
    if hit.is_none() && !task.main && task.target <= 1000 {
        match disk.try_lock() {
            Ok(disk) => {
                if let Some(bytes) = disk.get(&key, u64::from(task.target).pow(2) * 4 + 24) {
                    hit = mygo_decoder::decode_thumb(&bytes).ok();
                    origin = if hit.is_some() {
                        "cache-disk-hit"
                    } else {
                        "cache-disk-invalid"
                    };
                }
            }
            Err(_) => origin = "cache-disk-busy",
        }
    }
    (hit, origin)
}
struct CacheStage {
    result_budget: PixelBudget,
    jobs: Scheduler<DecodeTask>,
    source: Scheduler<DecodeTask>,
    results: mpsc::SyncSender<Decoded>,
    thumb: Ram,
    disk: Arc<Mutex<DiskCache>>,
    epoch: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    ready: Arc<tokio::sync::Notify>,
    log: mygo_telemetry::Telemetry,
}
impl CacheStage {
    fn spawn(self) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("thumbnail-lookup".into())
            .spawn(move || {
                while let Some(job) = self.jobs.pop() {
                    let started = Instant::now();
                    let task = &job.payload;
                    let key = task.key();
                    let (hit, origin) = cached_frame(task, &self.thumb, &self.disk);
                    self.log.trace(
                        origin,
                        job.request_id,
                        job.generation_id,
                        &key,
                        started.elapsed().as_micros(),
                        true,
                    );
                    if job.cancel.cancelled() {
                        continue;
                    }
                    let Some(frame) = hit else {
                        let cancel = job.cancel.clone();
                        if !self.source.submit(job) {
                            cancel.cancel();
                        }
                        continue;
                    };
                    {
                        let mut ram = self.thumb.lock().unwrap();
                        if self.epoch.load(Ordering::Acquire) == task.cache_epoch {
                            ram.put(key.clone(), frame.clone(), frame.bytes());
                        }
                    }
                    self.log.trace(
                        "cache-stage-complete",
                        job.request_id,
                        job.generation_id,
                        &key,
                        task.enqueued.elapsed().as_micros(),
                        true,
                    );
                    let Some(permit) = self.result_budget.acquire(&frame, &job.cancel) else {
                        continue;
                    };
                    let bytes = frame.bytes() as u64;
                    self.bytes.fetch_add(bytes, Ordering::Relaxed);
                    if self
                        .results
                        .send(Decoded {
                            _permit: Some(permit),
                            request: job.request_id,
                            task: job.payload,
                            generation: job.generation_id,
                            result: Ok(frame),
                            elapsed: started.elapsed().as_millis(),
                        })
                        .is_err()
                    {
                        self.bytes.fetch_sub(bytes, Ordering::Relaxed);
                        break;
                    }
                    self.ready.notify_one();
                }
            })
    }
}
struct SourceStage {
    result_budget: PixelBudget,
    jobs: Scheduler<DecodeTask>,
    results: mpsc::SyncSender<Decoded>,
    thumb: Ram,
    main: Ram,
    disk: Arc<Mutex<DiskCache>>,
    epoch: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    ready: Arc<tokio::sync::Notify>,
    writes: mpsc::SyncSender<(String, Frame, Cancel, u64)>,
    log: mygo_telemetry::Telemetry,
}
impl SourceStage {
    fn spawn(self) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("source-decode".into())
            .spawn(move || {
                while let Some(first) = self.jobs.pop() {
                    let mut batch = vec![first];
                    // Never delay Main to gather work. Only drain compatible misses
                    // already admitted by the independent cache lookup stage.
                    if batch[0].payload.shareable() {
                        let first = &batch[0].payload;
                        let mut related =
                            self.jobs.take_matching(7, |task| first.same_source(task));
                        batch.append(&mut related);
                        batch.sort_by_key(|job| job.priority as usize);
                    }
                    let batch = batch
                        .into_iter()
                        .filter(|job| {
                            if job.cancel.cancelled() {
                                return false;
                            }
                            let started = Instant::now();
                            let task = &job.payload;
                            let key = task.key();
                            self.log.record(
                                "decode-queue-wait",
                                task.enqueued.elapsed().as_micros(),
                                0,
                                true,
                            );
                            let cache = if task.main { &self.main } else { &self.thumb };
                            let (hit, origin) = cached_frame(task, cache, &self.disk);
                            self.log.trace(
                                origin,
                                job.request_id,
                                job.generation_id,
                                &key,
                                started.elapsed().as_micros(),
                                true,
                            );
                            if let Some(frame) = hit {
                                self.complete(job, Ok(frame), true);
                                false
                            } else {
                                true
                            }
                        })
                        .collect::<Vec<_>>();
                    if batch.is_empty() {
                        continue;
                    }
                    let mut delivered = vec![false; batch.len()];
                    if batch.len() > 1 {
                        let source_cancel =
                            Cancel::when_all(batch.iter().map(|job| job.cancel.clone()).collect());
                        let targets = batch
                            .iter()
                            .map(|job| (job.payload.target, job.cancel.clone()))
                            .collect::<Vec<_>>();
                        for job in &batch {
                            self.started(job);
                        }
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            mygo_decoder::decode_shared_exact(
                                &batch[0].payload.path,
                                &targets,
                                &batch[0].payload.config,
                                &source_cancel,
                                |index, frame| {
                                    self.complete(&batch[index], Ok(frame), false);
                                    delivered[index] = true;
                                    Ok(())
                                },
                            )
                        }));
                        self.log.record(
                            "source-shared-batch",
                            0,
                            batch.len() as u64,
                            matches!(&result, Ok(Ok(_))),
                        );
                        if let Ok(Ok(timings)) = result {
                            self.log.record(
                                "source-shared-total",
                                timings.total_us as u128,
                                batch.len() as u64,
                                true,
                            );
                            self.log
                                .record("image-read-bytes", 0, timings.read_bytes, true);
                            self.log
                                .record("source-codec-calls", 0, timings.codec_calls, true);
                        }
                        // Shared work has a stricter memory guard. Unemitted targets
                        // fall back independently; completed targets never restart.
                    }
                    for (index, job) in batch.iter().enumerate() {
                        if delivered[index] || job.cancel.cancelled() {
                            continue;
                        }
                        self.started(job);
                        let started = Instant::now();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let (frame, timings) = mygo_decoder::decode_profiled(
                                &job.payload.path,
                                job.payload.target,
                                &job.payload.config,
                                &job.cancel,
                            )?;
                            self.timings(job, timings);
                            Ok(frame)
                        }))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("解碼器無法處理此圖片")));
                        self.log.record(
                            "source-decode",
                            started.elapsed().as_micros(),
                            1,
                            result.is_ok(),
                        );
                        self.complete(job, result, false);
                    }
                }
            })
    }
    fn started(&self, job: &Job<DecodeTask>) {
        self.log.trace(
            "decode-start",
            job.request_id,
            job.generation_id,
            &job.payload.key(),
            job.payload.enqueued.elapsed().as_micros(),
            true,
        );
    }
    fn timings(&self, job: &Job<DecodeTask>, timings: mygo_decoder::DecodeTimings) {
        for (event, micros) in timings.stages() {
            self.log.trace(
                event,
                job.request_id,
                job.generation_id,
                &job.payload.key(),
                u128::from(micros),
                true,
            );
        }
        self.log
            .record("image-read-bytes", 0, timings.read_bytes, true);
        self.log
            .record("source-codec-calls", 0, timings.codec_calls, true);
    }
    fn complete(&self, job: &Job<DecodeTask>, result: Result<Frame>, cached: bool) {
        if job.cancel.cancelled() {
            return;
        }
        let task = &job.payload;
        let key = task.key();
        self.log.trace(
            "decode-complete",
            job.request_id,
            job.generation_id,
            &key,
            task.enqueued.elapsed().as_micros(),
            result.is_ok(),
        );
        if let Ok(frame) = &result {
            let mut cache = (if task.main { &self.main } else { &self.thumb })
                .lock()
                .unwrap();
            if self.epoch.load(Ordering::Acquire) == task.cache_epoch {
                cache.put(key.clone(), frame.clone(), frame.bytes());
            }
        }
        self.log.record(
            if task.main {
                "main-decode"
            } else {
                "thumbnail"
            },
            task.enqueued.elapsed().as_micros(),
            u64::from(cached),
            result.is_ok(),
        );
        let write = result
            .as_ref()
            .ok()
            .filter(|_| !task.main && task.target <= 1000 && !cached)
            .map(|frame| (key, frame.clone(), job.cancel.clone(), task.cache_epoch));
        let permit = if let Ok(frame) = &result {
            let Some(permit) = self.result_budget.acquire(frame, &job.cancel) else {
                return;
            };
            Some(permit)
        } else {
            None
        };
        let bytes = result.as_ref().map_or(0, |frame| frame.bytes() as u64);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        if self
            .results
            .send(Decoded {
                _permit: permit,
                request: job.request_id,
                task: task.clone(),
                generation: job.generation_id,
                result,
                elapsed: task.enqueued.elapsed().as_millis(),
            })
            .is_err()
        {
            self.bytes.fetch_sub(bytes, Ordering::Relaxed);
            job.cancel.cancel();
            return;
        }
        self.ready.notify_one();
        if let Some(write) = write {
            self.log.record(
                "cache-write-enqueue",
                0,
                1,
                self.writes.try_send(write).is_ok(),
            );
        }
    }
}
struct FileResult {
    cleared: bool,
    copied: Option<usize>,
    id: u64,
    scope: String,
    root: PathBuf,
    list: u64,
    was_scanning: bool,
    report: crate::trash::Report,
}
struct FileOperation {
    id: u64,
    cancel: Cancel,
    progress: Arc<AtomicU64>,
    last_progress: Instant,
}
struct PreparedSource {
    root: PathBuf,
    scope: String,
    pending_file: Option<String>,
    recursive: bool,
}
impl PreparedSource {
    fn open(path: PathBuf, recursive: bool) -> Result<Self> {
        let path = path
            .canonicalize()
            .context("無法開啟來源；請確認 SD 卡仍連線且具有讀取權限")?;
        let metadata = path.metadata()?;
        let is_file = metadata.is_file();
        anyhow::ensure!(is_file || metadata.is_dir(), "來源不是圖片或資料夾");
        if is_file && !mygo_core::supported(&path) {
            anyhow::bail!("不支援此格式；HEIC / AVIF / RAW 解碼器未啟用");
        }
        let root = if is_file {
            path.parent().context("圖片沒有所在資料夾")?.to_path_buf()
        } else {
            path.clone()
        };
        Ok(Self {
            scope: format!("{}:recursive={recursive}", mygo_core::volume_scope(&root)?),
            root,
            pending_file: if is_file {
                path.file_name().map(|s| s.to_string_lossy().to_string())
            } else {
                None
            },
            recursive,
        })
    }
}
enum SourceTask {
    Open(PathBuf, bool),
    Probe(PathBuf, bool), // true for an explicit reconnect
}
enum SourceOutcome {
    Open(Result<PreparedSource>),
    Probe { exists: bool, retry: bool },
}
type PrepareSource = Box<dyn Fn(PathBuf, bool) -> Result<PreparedSource> + Send>;
type ProbeSource = Box<dyn Fn(&Path) -> bool + Send>;
type SaveSettings = Box<dyn Fn(&Config, &Path) -> Result<()> + Send>;
struct BackgroundIo {
    #[cfg(test)]
    before_rank: Box<dyn Fn() + Send>,
    prepare: PrepareSource,
    probe: ProbeSource,
    save: SaveSettings,
}
impl Default for BackgroundIo {
    fn default() -> Self {
        Self {
            #[cfg(test)]
            before_rank: Box::new(|| {}),
            prepare: Box::new(PreparedSource::open),
            probe: Box::new(Path::exists),
            save: Box::new(Config::save),
        }
    }
}
#[derive(Clone)]
struct RankRequest {
    ticket: u64,
    revision: u64,
    list: u64,
    scope: String,
    id: i64,
    sort: Sort,
    descending: bool,
    select: bool,
}
struct RankResult {
    request: RankRequest,
    position: Result<Option<usize>>,
}
struct Controller {
    result_budget: PixelBudget,
    rank_worker: LatestWorker<RankRequest, RankResult>,
    rank_ticket: u64,
    rank_pending: Option<RankRequest>,
    source_worker: LatestWorker<(u64, u64, SourceTask), (u64, u64, SourceOutcome)>,
    source_epoch: u64,
    source_ticket: u64,
    source_busy: bool,
    settings_worker: LatestWorker<(u64, Config), (u64, Result<()>)>,
    info_worker: LatestWorker<(u64, PathBuf), ImageInfoResult>,
    details_enabled: bool,
    settings_revision: u64,
    settings_completed: u64,
    #[cfg(test)]
    base: PathBuf,
    telemetry: mygo_telemetry::Telemetry,
    db: Index,
    config: Config,
    root: PathBuf,
    scope: String,
    recursive: bool,
    sort: Sort,
    descending: bool,
    list: u64,
    generation: u64,
    request: u64,
    scan_cancel: Cancel,
    main_cancel: Cancel,
    preload_cancel: Cancel,
    first_frame_ready: bool,
    visible_cancel: Cancel,
    scan: Option<Receiver<ScanEvent>>,
    decode: Receiver<Decoded>,
    decoded_bytes: Arc<AtomicU64>,
    ready: Arc<tokio::sync::Notify>,
    cache_writer: mpsc::SyncSender<(String, Frame, Cancel, u64)>,
    cache_epoch: Arc<AtomicU64>,
    pending: HashMap<String, Pending>,
    jobs: Scheduler<DecodeTask>,
    cache_jobs: Scheduler<DecodeTask>,
    out: EventSender,
    count: usize,
    selected: Option<Entry>,
    position: usize,
    visible: usize,
    sidebar_visible_count: usize,
    page_end: usize,
    thumbnail_target: u32,
    preview_target: u32,
    scanning: bool,
    scan_batches: usize,
    scan_remainder: VecDeque<Entry>,
    scan_write_size: usize,
    last_update: Instant,
    thumb: Ram,
    main: Ram,
    disk: Arc<Mutex<DiskCache>>,
    pending_file: Option<String>,
    page_key: String,
    pending_scan: bool,
    high_requested: bool,
    last_source_check: Instant,
    source_missing: bool,
    file_operation: Option<FileOperation>,
    file_results: Receiver<FileResult>,
    file_sender: mpsc::SyncSender<FileResult>,
    handles: Vec<std::thread::JoinHandle<()>>,
}
pub fn run(rx: CommandReceiver, out: EventSender, telemetry: mygo_telemetry::Telemetry) {
    run_logged(rx, out, mygo_core::data_dir(), telemetry, 8);
}
#[cfg(test)]
fn run_at(rx: CommandReceiver, out: EventSender, base: PathBuf) {
    let telemetry = mygo_telemetry::Telemetry::new(&base).unwrap();
    run_logged(rx, out, base, telemetry, 8);
}
fn run_logged(
    rx: CommandReceiver,
    out: EventSender,
    base: PathBuf,
    telemetry: mygo_telemetry::Telemetry,
    scan_batches: usize,
) {
    run_logged_with_io(
        rx,
        out,
        base,
        telemetry,
        scan_batches,
        BackgroundIo::default(),
    );
}
fn run_logged_with_io(
    mut rx: CommandReceiver,
    out: EventSender,
    base: PathBuf,
    telemetry: mygo_telemetry::Telemetry,
    scan_batches: usize,
    io_hooks: BackgroundIo,
) {
    let mut recovered_settings = None;
    let result = (|| -> Result<Controller> {
        let started = Instant::now();
        let (config, backup) = Config::load_with_recovery(&base)?;
        telemetry.record(
            "startup-settings-load",
            started.elapsed().as_micros(),
            0,
            true,
        );
        recovered_settings = backup;
        let started = Instant::now();
        let db = Index::open(&base.join("index.sqlite3"))?;
        telemetry.record("startup-index-open", started.elapsed().as_micros(), 0, true);
        let disk = Arc::new(Mutex::new(DiskCache::open_lazy(
            &base.join("thumbnails"),
            config.disk_mib * 1024 * 1024,
        )?));
        let thumb = Arc::new(Mutex::new(ByteLru::new(config.thumbnail_mib * MIB)));
        let main = Arc::new(Mutex::new(ByteLru::new(preview_cache_budget(
            &config,
            INITIAL_PREVIEW,
        ))));
        let jobs: Scheduler<DecodeTask> = Scheduler::default();
        let cache_jobs: Scheduler<DecodeTask> = Scheduler::default();
        let (dt, decode) = mpsc::sync_channel(8);
        let decoded_bytes = Arc::new(AtomicU64::new(0));
        let result_budget =
            PixelBudget::new((config.decode_mib * MIB / 4).clamp(4 * MIB, 32 * MIB));
        let ready = Arc::new(tokio::sync::Notify::new());
        let BackgroundIo {
            #[cfg(test)]
            before_rank,
            prepare,
            probe,
            save,
        } = io_hooks;
        let source_log = telemetry.clone();
        let info_log = telemetry.clone();
        let (info_worker, info_handle) = LatestWorker::spawn(
            "image-info",
            ready.clone(),
            move |(generation, path): (u64, PathBuf)| {
                let started = Instant::now();
                let result = crate::image_info::read(&path, Some(&info_log));
                info_log.record(
                    "image-info-read",
                    started.elapsed().as_micros(),
                    generation,
                    result.is_ok(),
                );
                (generation, result)
            },
        )?;
        let (source_worker, source_handle) = LatestWorker::spawn(
            "source-check",
            ready.clone(),
            move |(ticket, epoch, task)| {
                let started = Instant::now();
                let outcome = match task {
                    SourceTask::Open(path, recursive) => {
                        let result = prepare(path, recursive);
                        source_log.record(
                            "source-prepare",
                            started.elapsed().as_micros(),
                            ticket,
                            result.is_ok(),
                        );
                        SourceOutcome::Open(result)
                    }
                    SourceTask::Probe(root, retry) => {
                        let exists = probe(&root);
                        source_log.record(
                            "source-probe",
                            started.elapsed().as_micros(),
                            ticket,
                            exists,
                        );
                        SourceOutcome::Probe { exists, retry }
                    }
                };
                (ticket, epoch, outcome)
            },
        )?;
        let settings_base = base.clone();
        let settings_log = telemetry.clone();
        let (settings_worker, settings_handle) = LatestWorker::spawn(
            "settings-writer",
            ready.clone(),
            move |(revision, config)| {
                let started = Instant::now();
                let result = save(&config, &settings_base)
                    .context("無法儲存設定；本次變更僅套用於目前工作階段");
                settings_log.record(
                    "settings-save",
                    started.elapsed().as_micros(),
                    revision,
                    result.is_ok(),
                );
                (revision, result)
            },
        )?;
        let rank_path = base.join("index.sqlite3");
        let rank_log = telemetry.clone();
        let mut reader = None;
        let (rank_worker, rank_handle) =
            LatestWorker::spawn("index-rank", ready.clone(), move |request: RankRequest| {
                let started = Instant::now();
                #[cfg(test)]
                before_rank();
                let position = (|| -> Result<Option<usize>> {
                    if reader.is_none() {
                        reader = Some(Index::open_reader(&rank_path)?);
                    }
                    let db = reader.as_mut().unwrap();
                    db.read_scope(&request.scope)?;
                    db.position(request.id, request.sort, request.descending)
                })();
                rank_log.record(
                    "index-position-background",
                    started.elapsed().as_micros(),
                    request.revision,
                    position.is_ok(),
                );
                RankResult { request, position }
            })?;
        let (cache_writer, writes) = mpsc::sync_channel::<(String, Frame, Cancel, u64)>(2);
        let mut handles = vec![source_handle, settings_handle, rank_handle, info_handle];
        let cache_epoch = Arc::new(AtomicU64::new(0));
        handles.push(
            CacheStage {
                result_budget: result_budget.clone(),
                jobs: cache_jobs.clone(),
                source: jobs.clone(),
                results: dt.clone(),
                thumb: thumb.clone(),
                disk: disk.clone(),
                epoch: cache_epoch.clone(),
                bytes: decoded_bytes.clone(),
                ready: ready.clone(),
                log: telemetry.clone(),
            }
            .spawn()?,
        );
        let writer_epoch = cache_epoch.clone();
        let writer_disk = disk.clone();
        let writer_log = telemetry.clone();
        handles.push(
            std::thread::Builder::new()
                .name("thumbnail-writer".into())
                .spawn(move || {
                    let started = Instant::now();
                    let initialized = writer_disk.lock().unwrap().reconcile().is_ok();
                    writer_log.record(
                        "cache-reconcile",
                        started.elapsed().as_micros(),
                        0,
                        initialized,
                    );
                    while let Ok((key, frame, cancel, epoch)) = writes.recv() {
                        if !initialized {
                            continue;
                        }
                        if cancel.cancelled() {
                            continue;
                        }
                        let started = Instant::now();
                        let result = mygo_decoder::encode_thumb(&frame).and_then(|bytes| {
                            let disk = writer_disk.lock().unwrap();
                            if cancel.cancelled() || writer_epoch.load(Ordering::Acquire) != epoch {
                                return Ok(());
                            }
                            disk.put(&key, &bytes)
                        });
                        writer_log.record(
                            "cache-write",
                            started.elapsed().as_micros(),
                            frame.bytes() as u64,
                            result.is_ok(),
                        );
                    }
                })?,
        );
        handles.push(
            SourceStage {
                result_budget: result_budget.clone(),
                jobs: jobs.clone(),
                results: dt,
                thumb: thumb.clone(),
                main: main.clone(),
                disk: disk.clone(),
                epoch: cache_epoch.clone(),
                bytes: decoded_bytes.clone(),
                ready: ready.clone(),
                writes: cache_writer.clone(),
                log: telemetry.clone(),
            }
            .spawn()?,
        );
        let (file_sender, file_results) = mpsc::sync_channel(1);
        Ok(Controller {
            result_budget,
            rank_worker,
            rank_ticket: 0,
            rank_pending: None,
            source_worker,
            source_epoch: 0,
            source_ticket: 0,
            source_busy: false,
            settings_worker,
            info_worker,
            details_enabled: false,
            settings_revision: 0,
            settings_completed: 0,
            #[cfg(test)]
            base,
            telemetry: telemetry.clone(),
            db,
            config,
            root: PathBuf::new(),
            scope: String::new(),
            recursive: false,
            sort: Sort::Name,
            descending: false,
            list: 0,
            generation: 0,
            request: 0,
            scan_cancel: Cancel::default(),
            main_cancel: Cancel::default(),
            preload_cancel: Cancel::default(),
            first_frame_ready: false,
            visible_cancel: Cancel::default(),
            scan: None,
            decode,
            decoded_bytes,
            ready,
            cache_writer,
            cache_epoch,
            jobs,
            cache_jobs,
            pending: HashMap::new(),
            out: out.clone(),
            count: 0,
            selected: None,
            position: 0,
            visible: 0,
            sidebar_visible_count: 6,
            page_end: 0,
            thumbnail_target: 320,
            preview_target: INITIAL_PREVIEW,
            scanning: false,
            scan_batches,
            scan_remainder: VecDeque::new(),
            scan_write_size: 64,
            last_update: Instant::now(),
            thumb,
            main,
            disk,
            pending_file: None,
            page_key: String::new(),
            pending_scan: false,
            high_requested: false,
            last_source_check: Instant::now(),
            source_missing: false,
            file_operation: None,
            file_results,
            file_sender,
            handles,
        })
    })();
    let mut c = match result {
        Ok(c) => c,
        Err(e) => {
            let _ = out.send(Event::Error(format!("初始化失敗：{e:#}")));
            return;
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    c.emit(Event::Settings(
        c.config.clone(),
        crate::trash::directory()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    ));
    if let Some(backup) = recovered_settings {
        c.emit(Event::Error(format!(
            "設定檔已損壞，已恢復預設值。原檔備份：{}",
            backup.display()
        )));
    }
    runtime.block_on(async {
        let mut tick=tokio::time::interval(Duration::from_millis(12));
        let ready = c.ready.clone();
        loop {
            tokio::select! {
                biased;
                a=rx.recv()=>{match a {Some(Action::Shutdown)|None=>{
                    if let Some(settings)=rx.take_latest_settings()
                        && let Err(e)=c.action(settings) {c.emit(Event::Error(format!("{e:#}")));}
                    break;
                },Some(a)=>if let Err(e)=c.action(a){c.emit(Event::Error(format!("{e:#}")));}}},
                _=ready.notified()=>{if let Err(e)=c.poll(){c.emit(Event::Error(format!("{e:#}")));}},
                _=tick.tick()=>{if let Err(e)=c.poll(){c.emit(Event::Error(format!("{e:#}")));}}
            }
        }
    });
    c.scan_cancel.cancel();
    c.main_cancel.cancel();
    c.preload_cancel.cancel();
    c.visible_cancel.cancel();
    for pending in c.pending.values() {
        pending.cancel.cancel();
    }
    c.result_budget.close();
    c.jobs.close();
    c.cache_jobs.close();
    c.info_worker.close();
    c.settings_worker.close();
    c.source_worker.close();
    c.rank_worker.close();
    let settings_deadline = Instant::now() + Duration::from_secs(1);
    while c.settings_completed != c.settings_revision && Instant::now() < settings_deadline {
        c.poll_settings();
        if c.settings_completed != c.settings_revision {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    if c.settings_completed != c.settings_revision {
        c.telemetry
            .record("settings-shutdown-timeout", 0, c.settings_revision, false);
        c.emit(Event::Error("設定尚未完成儲存；關閉等待已逾時".into()));
    }
    if let Some(op) = &c.file_operation {
        op.cancel.cancel();
    }
    // Disconnect result consumers before joining, so a full channel cannot pin workers.
    drop(c.decode);
    drop(c.cache_writer);
    drop(c.scan);
    drop(c.file_results);
    drop(c.source_worker);
    drop(c.rank_worker);
    drop(c.info_worker);
    drop(c.settings_worker);
    let deadline = Instant::now() + Duration::from_millis(500);
    while c.handles.iter().any(|h| !h.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = c.telemetry.flush(Duration::from_millis(100));
    for handle in c.handles {
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}
impl Controller {
    fn trash_directory(&self) -> Result<PathBuf> {
        #[cfg(test)]
        {
            Ok(self.base.join("trash"))
        }
        #[cfg(not(test))]
        {
            crate::trash::directory()
        }
    }
    fn emit(&self, e: Event) {
        let _ = self.out.send(e);
    }
    fn action(&mut self, a: Action) -> Result<()> {
        match a {
            Action::Picker(file) => self.pick_directory(file, false)?,
            Action::PickCopyFolder => self.pick_directory(false, true)?,
            Action::CopyCurrent => self.copy_current()?,
            Action::Details(enabled) => {
                self.details_enabled = enabled;
                if enabled {
                    self.load_info();
                }
            }
            Action::ImageShown(list, id) => {
                if !self.first_frame_ready
                    && list == self.list
                    && self.selected.as_ref().is_some_and(|entry| entry.id == id)
                {
                    self.start_background_loading()?;
                }
            }
            Action::Open(p, r) => self.open(p, r)?,
            Action::Rescan(r) => {
                if !self.root.as_os_str().is_empty() {
                    self.open(self.root.clone(), r)?;
                }
            }
            Action::Sort(s, d) => {
                self.sort = match s {
                    1 => Sort::Modified,
                    2 => Sort::Size,
                    _ => Sort::Name,
                };
                self.descending = d;
                self.list += 1;
                self.visible_cancel.cancel();
                self.emit(Event::Reset {
                    generation: self.list,
                    folder: self.root.display().to_string(),
                    wheel: self.config.wheel_zoom,
                });
                self.emit(Event::Count(self.count));
                self.refresh_selection()?;
                self.page(self.position.saturating_sub(4))?;
                if let Some(entry) = self.selected.clone() {
                    self.emit(Event::Selection {
                        generation: self.generation,
                        id: entry.id,
                        index: self.position,
                    });
                    if !self.config.multi_view {
                        self.queue(
                            entry,
                            self.position,
                            self.preview_target,
                            Priority::Main,
                            true,
                        );
                    }
                }
            }
            Action::Select(i) => self.select(i)?,
            Action::Settings(mut config) => {
                config.wheel_zoom = true;
                config.images_per_view = config.images_per_view.clamp(1, 24);
                config.image_size_limit = config.image_size_limit.min(4);
                config.preload_count = config.preload_count.clamp(1, 24);
                self.settings_revision += 1;
                self.settings_worker
                    .submit((self.settings_revision, config.clone()));
                let changed_mode = self.config.multi_view != config.multi_view;
                let changed_size = self.config.image_size_limit != config.image_size_limit;
                let changed_preload = self.config.preload_enabled != config.preload_enabled
                    || self.config.preload_count != config.preload_count;
                let changed_layout = changed_size
                    || changed_mode
                    || changed_preload
                    || self.config.images_per_view != config.images_per_view;
                self.telemetry.record(
                    "settings-applied-live",
                    0,
                    config.preload_count as u64,
                    true,
                );
                self.config = config;
                self.main
                    .lock()
                    .unwrap()
                    .set_budget(preview_cache_budget(&self.config, self.preview_target));
                if self.config.resume_last_image
                    && let Some(entry) = &self.selected
                {
                    self.db.remember_selection(&entry.relative)?;
                }
                if changed_size {
                    self.thumb.lock().unwrap().clear();
                    self.main.lock().unwrap().clear();
                }
                if changed_preload {
                    self.preload_cancel.cancel();
                    self.preload_cancel = Cancel::default();
                    self.page_key.clear();
                }
                if changed_mode || changed_size {
                    self.select(self.position)?;
                }
                if changed_layout && !self.scope.is_empty() {
                    self.page(self.visible)?;
                }
                if changed_preload {
                    self.preload()?;
                }
            }
            Action::OpenTrash => {
                let trash = self.trash_directory()?;
                std::fs::create_dir_all(&trash)?;
                std::process::Command::new("explorer.exe")
                    .arg(trash)
                    .spawn()?;
            }
            Action::Trash(id) => {
                anyhow::ensure!(
                    self.file_operation.is_none(),
                    "檔案作業仍在進行中，請稍後重試"
                );
                if let Some(e) = self.selected.clone().filter(|e| e.id == id) {
                    let was_scanning = self.scanning;
                    self.scan_cancel.cancel();
                    self.scan = None;
                    self.scanning = false;
                    let trash = self.trash_directory()?;
                    let root = self.root.clone();
                    let scope = self.scope.clone();
                    let list = self.list;
                    let source = root.join(&e.relative);
                    let companions = self.config.trash_companions;
                    let cancel = Cancel::default();
                    let worker_cancel = cancel.clone();
                    let progress = Arc::new(AtomicU64::new(0));
                    let worker_progress = progress.clone();
                    self.request += 1;
                    let id = self.request;
                    let sender = self.file_sender.clone();
                    let handle = std::thread::Builder::new()
                        .name("file-operation".into())
                        .spawn(move || {
                            let report = crate::trash::move_report_progress(
                                &source,
                                companions,
                                &trash,
                                &worker_cancel,
                                &worker_progress,
                            );
                            let _ = sender.send(FileResult {
                                cleared: false,
                                copied: None,
                                id,
                                scope,
                                root,
                                list,
                                was_scanning,
                                report,
                            });
                        })?;
                    self.handles.push(handle);
                    self.file_operation = Some(FileOperation {
                        id,
                        cancel,
                        progress,
                        last_progress: Instant::now(),
                    });
                    self.emit(Event::Status("正在搬移至垃圾桶，可按 Esc 取消".into()));
                }
            }
            Action::Navigate(d) => {
                if self.count > 0 {
                    let i =
                        (self.position as i64 + d as i64).clamp(0, self.count as i64 - 1) as usize;
                    self.select(i)?;
                }
            }
            Action::PreviewSize(target) => {
                let target = target.clamp(640, 4096);
                if target != self.preview_target {
                    self.telemetry
                        .record("preview-size", 0, target as u64, true);
                    let growing = target > self.preview_target;
                    self.preview_target = target;
                    self.main
                        .lock()
                        .unwrap()
                        .set_budget(preview_cache_budget(&self.config, target));
                    // Geometry changes must not resume a user-cancelled request,
                    // reset zoom, or replace a requested full-resolution frame.
                    if !self.main_cancel.cancelled() {
                        self.preload_cancel.cancel();
                        self.preload_cancel = Cancel::default();
                        if growing
                            && !self.config.multi_view
                            && !self.high_requested
                            && let Some(entry) = self.selected.clone()
                        {
                            self.main_cancel.cancel();
                            self.main_cancel = Cancel::default();
                            self.queue(entry, self.position, target, Priority::Main, true);
                        }
                        self.preload()?;
                        self.prune_pending();
                    }
                }
            }
            Action::ThumbnailSize(target) => {
                if self.thumbnail_target != target {
                    self.thumbnail_target = target;
                    self.page_key.clear();
                    if !self.scope.is_empty() && self.config.multi_view {
                        self.page(self.visible)?;
                    }
                }
            }
            Action::Visible(start) => {
                if !self.scope.is_empty() {
                    self.page(start)?;
                    if !self.first_frame_ready
                        && self.config.multi_view
                        && self.pending_file.is_none()
                    {
                        self.select(start)?;
                    }
                }
            }
            Action::VisibleCount(count) => {
                let count = count.clamp(1, 32);
                if count != self.sidebar_visible_count {
                    self.sidebar_visible_count = count;
                    self.page_key.clear();
                    if !self.scope.is_empty() && !self.config.multi_view {
                        self.page(self.visible)?;
                    }
                }
            }
            Action::RevealFile => {
                if let Some(e) = &self.selected {
                    let path = self.root.join(&e.relative);
                    anyhow::ensure!(path.is_file(), "圖片已移動或來源已中斷，無法開啟圖片位置");
                    std::process::Command::new("explorer.exe")
                        .arg("/select,")
                        .arg(path)
                        .spawn()
                        .context("無法開啟檔案總管")?;
                }
            }
            Action::High => {
                if self.main_cancel.cancelled() {
                    self.main_cancel = Cancel::default();
                    self.generation += 1;
                    self.high_requested = false;
                }
                if !self.high_requested
                    && let Some(e) = self.selected.clone()
                {
                    self.high_requested = true;
                    self.queue(e, self.position, 0, Priority::Main, true);
                }
            }
            Action::Cancel => {
                self.rank_pending = None;
                self.source_epoch += 1;
                self.pending_scan = false;
                if let Some(op) = &self.file_operation {
                    op.cancel.cancel();
                }
                self.main_cancel.cancel();
                self.preload_cancel.cancel();
                self.scan_cancel.cancel();
                self.visible_cancel.cancel();
                self.generation += 1;
                self.scanning = false;
                self.scan = None;
                self.emit(Event::Idle);
                self.emit(Event::Status("已取消載入與掃描 · 可重新連線".into()));
            }
            Action::Retry => {
                if !self.root.as_os_str().is_empty() {
                    self.source_epoch += 1;
                    self.submit_source(SourceTask::Probe(self.root.clone(), true));
                }
            }
            Action::Clear => {
                self.cache_epoch.fetch_add(1, Ordering::AcqRel);
                anyhow::ensure!(
                    self.file_operation.is_none(),
                    "檔案作業仍在進行中，請稍後重試"
                );
                self.thumb.lock().unwrap().clear();
                self.main.lock().unwrap().clear();
                let disk = self.disk.clone();
                let sender = self.file_sender.clone();
                let scope = self.scope.clone();
                let root = self.root.clone();
                let list = self.list;
                self.request += 1;
                let id = self.request;
                let cancel = Cancel::default();
                let worker_cancel = cancel.clone();
                self.handles.push(
                    std::thread::Builder::new()
                        .name("cache-maintenance".into())
                        .spawn(move || {
                            let error = disk
                                .lock()
                                .unwrap()
                                .clear_cancelled(&worker_cancel)
                                .err()
                                .map(|e| format!("{e:#}"));
                            let _ = sender.send(FileResult {
                                cleared: true,
                                copied: None,
                                id,
                                scope,
                                root,
                                list,
                                was_scanning: false,
                                report: crate::trash::Report {
                                    removed: Vec::new(),
                                    error,
                                },
                            });
                        })?,
                );
                self.file_operation = Some(FileOperation {
                    id,
                    cancel,
                    progress: Arc::new(AtomicU64::new(0)),
                    last_progress: Instant::now(),
                });
            }
            Action::Probe(id) => self.emit(Event::Probe(id)),
            Action::Shutdown => {}
        }
        Ok(())
    }
    fn pick_directory(&mut self, file: bool, copy_folder: bool) -> Result<()> {
        anyhow::ensure!(
            PICKER.with(|picker| picker.borrow().is_none()),
            "選擇視窗已開啟，請先完成或取消"
        );
        // Native dialog on its own thread; controller and UI remain responsive.
        let out = self.out.clone();
        let root = if copy_folder && !self.config.copy_folder.as_os_str().is_empty() {
            self.config.copy_folder.clone()
        } else {
            self.root.clone()
        };
        // Dialog result is returned over a private bounded channel, polled as a scan-independent command.
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new().set_directory(root);
            let p = if file {
                dialog
                    .add_filter(
                        "圖片",
                        &[
                            "jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff", "gif", "heic",
                            "avif", "raw",
                        ],
                    )
                    .pick_file()
            } else {
                dialog.pick_folder()
            };
            let _ = tx.send(p);
            drop(out);
        });
        PICKER.with(|p| *p.borrow_mut() = Some((copy_folder, rx)));
        Ok(())
    }
    fn copy_current(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.file_operation.is_none(),
            "檔案作業仍在進行中，請稍後重試"
        );
        if self.config.copy_folder.as_os_str().is_empty() {
            self.telemetry.record("copy-folder-missing", 0, 1, false);
            self.emit(Event::Notice("請先在設定中選擇複製目的地".into()));
            return Ok(());
        }
        let entry = self.selected.as_ref().context("請先選取圖片")?;
        let source = self.root.join(&entry.relative);
        let destination = self.config.copy_folder.clone();
        let (root, scope, list) = (self.root.clone(), self.scope.clone(), self.list);
        let cancel = Cancel::default();
        let worker_cancel = cancel.clone();
        let progress = Arc::new(AtomicU64::new(0));
        let worker_progress = progress.clone();
        self.request += 1;
        let id = self.request;
        let sender = self.file_sender.clone();
        let out = self.out.clone();
        let log = self.telemetry.clone();
        self.handles.push(
            std::thread::Builder::new()
                .name("copy-files".into())
                .spawn(move || {
                    let result = crate::quick_copy::copy(
                        &source,
                        &destination,
                        &worker_cancel,
                        &worker_progress,
                    );
                    let copied = match &result {
                        Ok(crate::quick_copy::Outcome::Copied(paths)) => paths.len(),
                        Ok(crate::quick_copy::Outcome::AlreadyPresent) => {
                            log.record("copy-duplicate-skipped", 0, 1, true);
                            let _ = out.send(Event::Notice("指定資料夾已擁有此圖片".into()));
                            0
                        }
                        Err(_) => 0,
                    };
                    let _ = sender.send(FileResult {
                        cleared: false,
                        copied: Some(copied),
                        id,
                        scope,
                        root,
                        list,
                        was_scanning: false,
                        report: crate::trash::Report {
                            removed: Vec::new(),
                            error: result.err().map(|e| format!("{e:#}")),
                        },
                    });
                })?,
        );
        self.file_operation = Some(FileOperation {
            id,
            cancel,
            progress,
            last_progress: Instant::now(),
        });
        Ok(())
    }
    fn open(&mut self, path: PathBuf, recursive: bool) -> Result<()> {
        self.rank_pending = None;
        self.source_epoch += 1;
        self.submit_source(SourceTask::Open(path, recursive));
        self.emit(Event::Status("正在開啟來源…".into()));
        Ok(())
    }
    fn submit_source(&mut self, task: SourceTask) {
        self.source_ticket += 1;
        self.source_busy = true;
        self.source_worker
            .submit((self.source_ticket, self.source_epoch, task));
    }
    fn apply_source(&mut self, source: PreparedSource) -> Result<()> {
        if let Some(op) = &self.file_operation {
            op.cancel.cancel();
        }
        let PreparedSource {
            root,
            scope,
            pending_file,
            recursive,
        } = source;
        self.db.scope(&scope)?;
        self.scope = scope;
        self.scan_cancel.cancel();
        self.main_cancel.cancel();
        self.preload_cancel.cancel();
        self.visible_cancel.cancel();
        self.scan_cancel = Cancel::default();
        self.main_cancel = Cancel::default();
        self.preload_cancel = Cancel::default();
        self.first_frame_ready = false;
        self.visible_cancel = Cancel::default();
        self.root = root.clone();
        self.recursive = recursive;
        self.list += 1;
        self.generation += 1;
        self.selected = None;
        self.position = 0;
        self.visible = 0;
        self.high_requested = false;
        self.source_missing = false;
        self.last_source_check = Instant::now();
        self.pending_file = pending_file.or(if self.config.resume_last_image {
            self.db.last_selection()?
        } else {
            None
        });
        self.count = self.db.count()?;
        self.emit(Event::Reset {
            generation: self.list,
            folder: root.display().to_string(),
            wheel: self.config.wheel_zoom,
        });
        self.emit(Event::Count(self.count));
        self.page(0)?;
        if let Some(path) = &self.pending_file
            && let Some(id) = self.db.find_path(path)?
        {
            self.request_position(id, true);
        }
        if self.count > 0 && self.pending_file.is_none() && self.selected.is_none() {
            self.select(0)?;
        }
        self.start_scan()?;
        self.emit(Event::Status(format!(
            "掃描中 · {} 張已索引 · 結果仍在更新",
            self.count
        )));
        Ok(())
    }
    fn start_scan(&mut self) -> Result<()> {
        if self
            .handles
            .iter()
            .filter(|h| !h.is_finished() && h.thread().name() == Some("directory-scan"))
            .count()
            >= 2
        {
            self.pending_scan = true;
            self.scanning = true;
            return Ok(());
        }
        self.pending_scan = false;
        self.db.begin_scan()?;
        self.scan_cancel = Cancel::default();
        let root = self.root.clone();
        let recursive = self.recursive;
        let (tx, rx) = mpsc::sync_channel(2);
        let cancel = self.scan_cancel.clone();
        let trash = self.trash_directory()?;
        let ready = self.ready.clone();
        self.scan_remainder.clear();
        self.handles.push(
            std::thread::Builder::new()
                .name("directory-scan".into())
                .spawn(move || {
                    let trash = trash.canonicalize().ok();
                    mygo_scanner::scan_excluding_notifying(
                        &root,
                        recursive,
                        &cancel,
                        &tx,
                        trash.as_deref(),
                        &|| ready.notify_one(),
                    )
                })?,
        );
        self.scan = Some(rx);
        self.scanning = true;
        Ok(())
    }
    fn request_position(&mut self, id: i64, select: bool) {
        self.rank_ticket += 1;
        let request = RankRequest {
            ticket: self.rank_ticket,
            revision: self.db.revision(),
            list: self.list,
            scope: self.scope.clone(),
            id,
            sort: self.sort,
            descending: self.descending,
            select,
        };
        self.rank_worker.submit(request.clone());
        self.rank_pending = Some(request);
    }
    fn refresh_selection(&mut self) -> Result<()> {
        if let Some(entry) = &self.selected {
            self.request_position(entry.id, false);
        } else if let Some(path) = &self.pending_file
            && let Some(id) = self.db.find_path(path)?
        {
            self.request_position(id, true);
        }
        Ok(())
    }
    fn poll_rank(&mut self) -> Result<()> {
        while let Ok(RankResult { request, position }) = self.rank_worker.results.try_recv() {
            if self
                .rank_pending
                .as_ref()
                .is_none_or(|pending| pending.ticket != request.ticket)
            {
                continue;
            }
            self.rank_pending = None;
            if request.list != self.list
                || request.scope != self.scope
                || request.sort != self.sort
                || request.descending != self.descending
                || (!request.select
                    && self
                        .selected
                        .as_ref()
                        .is_none_or(|entry| entry.id != request.id))
            {
                continue;
            }
            if request.revision != self.db.revision() {
                self.request_position(request.id, request.select);
                continue;
            }
            if let Some(index) = position? {
                if request.select {
                    self.pending_file = None;
                    self.select(index)?;
                    self.page(index.saturating_sub(4))?;
                } else if self.position != index {
                    self.position = index;
                    if self.details_enabled {
                        self.load_info();
                    }
                    self.emit(Event::Relocate {
                        id: request.id,
                        index,
                    });
                }
                if self.first_frame_ready {
                    self.preload()?;
                }
            } else if request.select && !self.scanning {
                self.pending_file = None;
                if self.selected.is_none() && self.count > 0 {
                    self.select(0)?;
                }
            }
        }
        Ok(())
    }
    fn page(&mut self, start: usize) -> Result<()> {
        let columns = crate::grid_columns(self.config.images_per_view as i32) as usize;
        let lookahead = if self.config.preload_enabled {
            if self.config.multi_view {
                let pixel_bytes = (self.thumbnail_target as usize).pow(2) * 4;
                let slots = (self.config.thumbnail_mib * MIB / 2) / pixel_bytes.max(1);
                self.config
                    .preload_count
                    .min(slots.saturating_sub(self.config.images_per_view))
            } else {
                self.config.preload_count
            }
        } else {
            0
        };
        let page_size = if self.config.multi_view {
            // Keep the next viewport ready for PageDown, plus one row for a
            // partly visible viewport. Scale lookahead with the chosen layout.
            (self.config.images_per_view * 2 + columns)
                .max(self.config.images_per_view + lookahead)
                .min(64)
        } else {
            32.max(self.sidebar_visible_count + lookahead).min(64)
        };
        let entries = self.db.page(start, page_size, self.sort, self.descending)?;
        let signature = format!(
            "{}:{}:{}:{}:{}:{:?}",
            self.list,
            lookahead,
            start,
            self.config.multi_view,
            self.config.images_per_view,
            entries
                .iter()
                .map(|e| (e.id, e.size, e.modified_ns))
                .collect::<Vec<_>>()
        );
        if signature == self.page_key {
            return Ok(());
        }
        self.page_key = signature;
        self.visible = start;
        self.page_end = start.saturating_add(entries.len());
        self.visible_cancel.cancel();
        self.visible_cancel = Cancel::default();
        self.emit(Event::Page {
            generation: self.list,
            start,
            entries: entries.iter().map(|e| (e.id, e.relative.clone())).collect(),
        });
        if !self.first_frame_ready {
            return Ok(());
        }
        let visible_count = if self.config.multi_view {
            self.config.images_per_view
        } else {
            self.sidebar_visible_count
        };
        for (i, e) in entries.into_iter().enumerate() {
            if i >= visible_count + lookahead {
                break;
            }
            self.queue(
                e,
                start + i,
                if self.config.multi_view {
                    self.thumbnail_target
                } else {
                    320
                },
                if i < visible_count {
                    Priority::Visible
                } else {
                    Priority::Prefetch
                },
                false,
            );
        }
        // Reattach every overlapping image before cancelling obsolete page work.
        self.prune_pending();
        Ok(())
    }
    fn prune_pending(&mut self) {
        self.pending.retain(|_, pending| pending.retain_live());
    }
    fn queue(&mut self, e: Entry, index: usize, target: u32, p: Priority, main: bool) {
        // While a background rank is outstanding, the old numeric position may
        // point at the selected id in the new order. Never prefetch that id:
        // a RAM hit would recursively deliver Main and start preload again.
        if p == Priority::Prefetch
            && self
                .selected
                .as_ref()
                .is_some_and(|selected| selected.id == e.id)
        {
            return;
        }
        let cancel = if main && p == Priority::Prefetch {
            self.preload_cancel.clone()
        } else if main || (!self.first_frame_ready && self.config.multi_view) {
            self.main_cancel.clone()
        } else {
            self.visible_cancel.clone()
        };
        let generation = if main { self.generation } else { self.list };
        let task = DecodeTask {
            config: self.config.clone(),
            enqueued: Instant::now(),
            cache_epoch: self.cache_epoch.load(Ordering::Acquire),
            path: self.root.join(&e.relative),
            entry: e,
            scope: self.scope.clone(),
            target,
            index,
            list: self.list,
            main,
        };
        let key = task.key();
        let subscriber = Subscriber {
            task: task.clone(),
            generation,
            cancel,
        };
        // Hand the new subscription to a matching job before pruning the old
        // generation, so reselecting the same image does not restart its codec.
        if let Some(pending) = self.pending.get_mut(&key)
            && !pending.cancel.cancelled()
        {
            let accepted = pending.subscribe(subscriber);
            self.telemetry.record("decode-coalesced", 0, 1, accepted);
            // Promote a queued prefetch when it becomes the selected main image.
            if accepted && p == Priority::Main {
                self.jobs.promote(pending.request);
                self.cache_jobs.promote(pending.request);
            }
            return;
        }
        if self.pending.len() >= 128 {
            self.prune_pending();
        }
        if self.pending.len() >= 128 {
            self.telemetry.record("decode-backpressure", 0, 1, false);
            return;
        }
        self.request += 1;
        let request = self.request;
        let group_cancel = Cancel::default();
        self.telemetry.trace(
            "request-enqueue",
            request,
            generation,
            &key,
            task.enqueued.elapsed().as_micros(),
            true,
        );
        let hit = (if main { &self.main } else { &self.thumb })
            .lock()
            .unwrap()
            .get(&key);
        if let Some(frame) = hit
            && let Some(permit) = self.result_budget.try_acquire(&frame, &group_cancel)
        {
            self.telemetry.trace(
                "cache-ram-hit",
                request,
                generation,
                &key,
                task.enqueued.elapsed().as_micros(),
                true,
            );
            self.pending.insert(
                key,
                Pending {
                    request,
                    cancel: group_cancel,
                    subscribers: vec![subscriber],
                },
            );
            self.telemetry.record(
                "ram-dispatch",
                task.enqueued.elapsed().as_micros(),
                request,
                true,
            );
            if let Err(e) = self.deliver(Decoded {
                _permit: Some(permit),
                request,
                task,
                generation,
                result: Ok(frame),
                elapsed: 0,
            }) {
                self.emit(Event::Error(format!("{e:#}")));
            }
            return;
        }
        let scheduler = if !task.main && task.target <= 1000 {
            &self.cache_jobs
        } else {
            &self.jobs
        };
        if scheduler.submit(Job {
            request_id: request,
            generation_id: generation,
            file_id: task.entry.id,
            priority: p,
            cancel: group_cancel.clone(),
            payload: task,
        }) {
            self.pending.insert(
                key,
                Pending {
                    request,
                    cancel: group_cancel,
                    subscribers: vec![subscriber],
                },
            );
        }
    }
    fn load_info(&self) {
        if let Some(entry) = &self.selected {
            self.info_worker
                .submit((self.generation, self.root.join(&entry.relative)));
        }
    }
    fn select(&mut self, index: usize) -> Result<()> {
        self.rank_pending = None;
        if let Some(e) = self.db.page(index, 1, self.sort, self.descending)?.pop() {
            self.main_cancel.cancel();
            self.preload_cancel.cancel();
            self.preload_cancel = Cancel::default();
            self.main_cancel = Cancel::default();
            self.generation += 1;
            self.high_requested = false;
            self.selected = Some(e.clone());
            if self.config.resume_last_image {
                self.db.remember_selection(&e.relative)?;
            }
            self.position = index;
            if self.details_enabled {
                self.load_info();
            }
            self.emit(Event::Selection {
                generation: self.generation,
                id: e.id,
                index,
            });
            if self.config.multi_view {
                if !self.first_frame_ready {
                    if index < self.visible || index >= self.page_end {
                        self.page(index)?;
                    }
                    self.queue(
                        e.clone(),
                        index,
                        self.thumbnail_target,
                        Priority::Visible,
                        false,
                    );
                }
                self.emit(Event::Idle);
                self.emit(Event::Status(format!(
                    "{} / {} · {} · {:.2} MiB · {} · DEL 移至垃圾桶",
                    index + 1,
                    self.count,
                    e.relative,
                    e.size as f64 / MIB as f64,
                    crate::image_info::modified(e.modified_ns)
                )));
            } else {
                self.queue(e, index, self.preview_target, Priority::Main, true);
            }
            self.prune_pending();
        }
        Ok(())
    }
    fn preload(&mut self) -> Result<()> {
        if !self.first_frame_ready
            || self.rank_pending.is_some()
            || !self.config.preload_enabled
            || self.config.multi_view
            || self.selected.is_none()
        {
            return Ok(());
        }
        let ranges =
            crate::preload_window::ranges(self.position, self.count, self.config.preload_count);
        self.telemetry
            .record("preload-forward-count", 0, ranges[0].len() as u64, true);
        self.telemetry
            .record("preload-backward-count", 0, ranges[1].len() as u64, true);
        for range in ranges {
            if range.is_empty() {
                continue;
            }
            for (offset, entry) in self
                .db
                .page(range.start, range.len(), self.sort, self.descending)?
                .into_iter()
                .enumerate()
            {
                self.queue(
                    entry,
                    range.start + offset,
                    self.preview_target,
                    Priority::Prefetch,
                    true,
                );
            }
        }
        Ok(())
    }
    fn start_background_loading(&mut self) -> Result<()> {
        self.first_frame_ready = true;
        self.page_key.clear();
        self.page(self.visible)?;
        self.preload()
    }
    fn poll_settings(&mut self) {
        while let Ok((revision, result)) = self.settings_worker.results.try_recv() {
            self.settings_completed = revision;
            match result {
                Ok(()) if revision == self.settings_revision => {
                    self.emit(Event::Status("設定已儲存".into()))
                }
                Ok(()) => {}
                Err(error) => self.emit(Event::Error(format!("{error:#}"))),
            }
        }
    }
    fn poll_source(&mut self) -> Result<()> {
        while let Ok((ticket, epoch, outcome)) = self.source_worker.results.try_recv() {
            if ticket != self.source_ticket {
                continue;
            }
            self.source_busy = false;
            if epoch != self.source_epoch {
                continue;
            }
            self.last_source_check = Instant::now();
            match outcome {
                SourceOutcome::Open(result) => self.apply_source(result?)?,
                SourceOutcome::Probe {
                    exists: true,
                    retry: true,
                } => {
                    self.source_missing = false;
                    if self.selected.is_some() {
                        self.select(self.position)?;
                    } else {
                        self.open(self.root.clone(), self.recursive)?;
                    }
                }
                SourceOutcome::Probe {
                    exists: true,
                    retry: false,
                } => {}
                SourceOutcome::Probe {
                    exists: false,
                    retry,
                } => {
                    self.source_missing = true;
                    self.pending_scan = false;
                    self.scan_cancel.cancel();
                    self.main_cancel.cancel();
                    self.visible_cancel.cancel();
                    self.generation += 1;
                    self.scan = None;
                    self.scanning = false;
                    self.emit(Event::Error(
                        if retry {
                            "來源已中斷，請插回 SD 卡後按重新連線；磁碟代號變更時請重新選擇資料夾"
                        } else {
                            "來源已中斷，保留目前畫面；請插回 SD 卡並按重新連線"
                        }
                        .into(),
                    ));
                }
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<()> {
        while let Ok((generation, result)) = self.info_worker.results.try_recv() {
            if self.details_enabled && generation == self.generation {
                self.emit(Event::Details {
                    generation,
                    fields: result.unwrap_or_else(|e| {
                        vec![crate::image_info::Field::new(
                            "讀取提示",
                            format!("無法讀取圖片資訊：{e:#}"),
                        )]
                    }),
                });
            }
        }
        self.poll_settings();
        self.poll_source()?;
        self.poll_rank()?;
        self.prune_pending();
        let progress = self.file_operation.as_mut().and_then(|op| {
            if op.last_progress.elapsed() < Duration::from_millis(200) {
                return None;
            }
            op.last_progress = Instant::now();
            Some(op.progress.load(Ordering::Relaxed))
        });
        if let Some(bytes) = progress {
            self.emit(Event::Status(format!(
                "背景檔案作業 · 已複製 {} MiB",
                bytes / MIB as u64
            )));
        }
        self.telemetry.record(
            "decoded-unique-bytes",
            0,
            self.result_budget.used() as u64,
            true,
        );
        self.telemetry.record(
            "decoded-result-bytes",
            0,
            self.decoded_bytes.load(Ordering::Relaxed),
            true,
        );
        self.telemetry.record(
            "ram-cache-bytes",
            0,
            (self.thumb.lock().unwrap().used() + self.main.lock().unwrap().used()) as u64,
            true,
        );
        let mut running = Vec::new();
        for handle in self.handles.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                running.push(handle);
            }
        }
        self.handles = running;
        if self.pending_scan
            && self
                .handles
                .iter()
                .filter(|h| h.thread().name() == Some("directory-scan"))
                .count()
                < 2
        {
            self.start_scan()?;
        }
        if let Ok(done) = self.file_results.try_recv()
            && self
                .file_operation
                .as_ref()
                .is_some_and(|op| op.id == done.id)
        {
            self.file_operation = None;
            let removed: Vec<_> = done
                .report
                .removed
                .iter()
                .filter_map(|p| {
                    p.strip_prefix(&done.root)
                        .ok()
                        .map(|p| p.to_string_lossy().to_string())
                })
                .collect();
            self.db.remove_paths_in_scope(&done.scope, &removed)?;
            if done.scope == self.scope && done.root == self.root {
                if !removed.is_empty() {
                    self.main_cancel.cancel();
                    self.visible_cancel.cancel();
                    self.list += 1;
                    self.generation += 1;
                    self.selected = None;
                    self.count = self.db.count()?;
                    self.position = self.position.min(self.count.saturating_sub(1));
                    self.emit(Event::Reset {
                        generation: self.list,
                        folder: self.root.display().to_string(),
                        wheel: self.config.wheel_zoom,
                    });
                    self.emit(Event::Count(self.count));
                    self.select(self.position)?;
                    self.page(self.visible.min(self.count.saturating_sub(1)))?;
                }
                if done.was_scanning && done.list <= self.list && self.scan.is_none() {
                    self.start_scan()?;
                }
            }
            if let Some(error) = done.report.error {
                self.emit(Event::Error(error));
            } else if let Some(copied) = done.copied {
                if copied > 0 {
                    self.emit(Event::Status(format!("已複製 {copied} 個檔案至指定資料夾")));
                }
            } else if done.cleared {
                self.emit(Event::Status("已清除記憶體與磁碟縮圖快取".into()));
            } else {
                self.emit(Event::Status(format!(
                    "檔案作業完成 · 已搬移 {} 個檔案",
                    removed.len()
                )));
            }
        }

        if !self.root.as_os_str().is_empty()
            && !self.source_busy
            && !self.source_missing
            && self.last_source_check.elapsed() > Duration::from_secs(2)
        {
            self.last_source_check = Instant::now();
            self.submit_source(SourceTask::Probe(self.root.clone(), false));
        }
        let picked = PICKER.with(|picker| {
            let mut picker = picker.borrow_mut();
            match picker
                .as_ref()
                .map(|(copy, receiver)| (*copy, receiver.try_recv()))
            {
                Some((copy, Ok(path))) => {
                    *picker = None;
                    path.map(|path| (copy, path))
                }
                Some((_, Err(mpsc::TryRecvError::Disconnected))) => {
                    *picker = None;
                    None
                }
                _ => None,
            }
        });
        if let Some((copy, path)) = picked {
            if copy {
                let mut config = self.config.clone();
                config.copy_folder = path;
                self.action(Action::Settings(config))?;
                self.emit(Event::Settings(
                    self.config.clone(),
                    self.trash_directory()?.display().to_string(),
                ));
            } else {
                self.open(path, self.recursive)?;
            }
        }
        // A transaction may exceed the soft deadline; never start another after it.
        let scan_started = Instant::now();
        for _ in 0..self.scan_batches {
            if scan_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(scan) = self.scan.as_ref() else {
                self.scan_remainder.clear();
                break;
            };
            let event = if self.scan_remainder.is_empty() {
                scan.try_recv().ok()
            } else {
                Some(ScanEvent::Batch(
                    self.scan_remainder
                        .drain(..self.scan_write_size.min(self.scan_remainder.len()))
                        .collect(),
                ))
            };
            let Some(event) = event else {
                break;
            };
            match event {
                ScanEvent::Batch(mut b) => {
                    if b.len() > self.scan_write_size {
                        self.scan_remainder.extend(b.drain(self.scan_write_size..));
                    }
                    let sql_started = Instant::now();
                    self.db.upsert(&b)?;
                    let elapsed = sql_started.elapsed();
                    self.telemetry
                        .record("scan-upsert", elapsed.as_micros(), b.len() as u64, true);
                    if elapsed > Duration::from_millis(2) {
                        self.scan_write_size = (self.scan_write_size / 2).max(16);
                    } else if elapsed < Duration::from_micros(500) {
                        self.scan_write_size = (self.scan_write_size * 2).min(256);
                    }
                    self.ready.notify_one();
                    if self.last_update.elapsed() > Duration::from_millis(200) || self.count == 0 {
                        self.update_scan()?;
                        self.last_update = Instant::now();
                    }
                }
                ScanEvent::Done { errors, complete } => {
                    self.scanning = false;
                    self.scan = None;
                    if complete {
                        self.db.finish()?;
                    }
                    self.update_scan()?;
                    self.emit(Event::Status(format!(
                        "{} 張圖片 · 掃描完成 · {} 個無法存取項目 · DEL 移至垃圾桶",
                        self.count, errors
                    )));
                    self.emit(Event::ScanComplete(self.count));
                    if errors > 0 {
                        self.emit(Event::Error(
                            "部分來源無法存取或已中斷，保留既有索引；請重新連線".into(),
                        ));
                    }
                }
            }
        }
        for _ in 0..8 {
            let Ok(d) = self.decode.try_recv() else {
                break;
            };
            self.decoded_bytes.fetch_sub(
                d.result.as_ref().map_or(0, |frame| frame.bytes() as u64),
                Ordering::Relaxed,
            );
            self.deliver(d)?;
        }
        Ok(())
    }
    fn deliver(&mut self, d: Decoded) -> Result<()> {
        let key = d.task.key();
        self.telemetry.trace(
            "result-consume",
            d.request,
            d.generation,
            &key,
            d.task.enqueued.elapsed().as_micros(),
            d.result.is_ok(),
        );
        if !self
            .pending
            .get(&key)
            .is_some_and(|pending| pending.request == d.request)
        {
            return Ok(());
        }
        let selected_result = d.task.list == self.list
            && self
                .selected
                .as_ref()
                .is_some_and(|entry| entry.id == d.task.entry.id);
        let unlock_background = !self.first_frame_ready && selected_result && d.result.is_err();
        let mut preload_after = false;
        let pending = self.pending.remove(&key).unwrap();
        for subscriber in pending
            .subscribers
            .into_iter()
            .filter(|s| !s.cancel.cancelled())
        {
            let d = Decoded {
                _permit: d._permit.clone(),
                request: d.request,
                task: subscriber.task,
                generation: subscriber.generation,
                result: match &d.result {
                    Ok(frame) => Ok(frame.clone()),
                    Err(error) => Err(anyhow::anyhow!("{error:#}")),
                },
                elapsed: d.elapsed,
            };
            if d.task.main {
                if d.generation != self.generation
                    || d.task.list != self.list
                    || self
                        .selected
                        .as_ref()
                        .is_none_or(|e| e.id != d.task.entry.id)
                {
                    continue;
                }
                preload_after |= d.task.target != 0;
                match d.result {
                    Ok(f) => {
                        self.emit(Event::Status(format!(
                            "{} / {} · {} × {} · {:.2} MiB · {} · {} ms{}{}",
                            self.position + 1,
                            self.count,
                            f.original_width,
                            f.original_height,
                            d.task.entry.size as f64 / (MIB as f64),
                            crate::image_info::modified(d.task.entry.modified_ns),
                            d.elapsed,
                            if self.scanning { " · 掃描中" } else { "" },
                            if d.task.target == 0
                                && (f.width < f.original_width || f.height < f.original_height)
                            {
                                " · 已使用記憶體預算內最高解析度，可繼續放大檢視"
                            } else {
                                ""
                            }
                        )));
                        let metadata = (f.original_width, f.original_height, f.orientation);
                        self.emit(Event::Main {
                            _permit: d._permit,
                            generation: self.generation,
                            request: d.request,
                            key: key.clone(),
                            enqueued: d.task.enqueued,
                            frame: f,
                        });
                        self.db
                            .metadata(d.task.entry.id, metadata.0, metadata.1, metadata.2)?;
                    }
                    Err(e) => self.emit(Event::Error(format!("{e:#}"))),
                }
            } else if d.task.list == self.list
                && d.task.index >= self.visible
                && d.task.index < self.visible + 64
            {
                self.emit(Event::Thumbnail {
                    _permit: d._permit,
                    generation: self.list,
                    index: d.task.index,
                    id: d.task.entry.id,
                    frame: d.result.ok(),
                });
            }
        }
        if unlock_background {
            self.start_background_loading()?;
        }
        if preload_after && self.first_frame_ready {
            self.preload()?;
        }
        Ok(())
    }
    fn update_scan(&mut self) -> Result<()> {
        self.count = self.db.count()?;
        self.emit(Event::Count(self.count));
        self.page(self.visible)?;
        if let Some(path) = self.pending_file.clone()
            && let Some(id) = self.db.find_path(&path)?
            && self
                .rank_pending
                .as_ref()
                .is_none_or(|request| !request.select || request.id != id)
        {
            self.request_position(id, true);
        }
        if !self.scanning
            && self.selected.is_none()
            && self.pending_file.is_some()
            && self
                .rank_pending
                .as_ref()
                .is_none_or(|request| !request.select)
        {
            self.pending_file = None;
        }
        if !self.scanning
            && let Some(entry) = &self.selected
            && self.db.find_path(&entry.relative)?.is_none()
        {
            self.main_cancel.cancel();
            self.preload_cancel.cancel();
            self.selected = None;
        }
        if self.selected.is_none() && self.count > 0 && self.pending_file.is_none() {
            self.select(0)?;
        } else if self.pending_file.is_none() {
            self.refresh_selection()?;
        }
        if self.scanning {
            self.emit(Event::Status(format!(
                "掃描中 · {} 張已索引 · 結果仍在更新",
                self.count
            )));
        }
        Ok(())
    }
}
thread_local! { static PICKER:std::cell::RefCell<Option<(bool, Receiver<Option<PathBuf>>)>>=const{std::cell::RefCell::new(None)}; }

/// Metadata pipeline benchmark; private database, normal command/event path, no GUI claims.
pub fn benchmark_scan(root: PathBuf, output: PathBuf, legacy: bool) -> Result<()> {
    let data = tempfile::tempdir()?;
    let log = mygo_telemetry::Telemetry::new(data.path())?;
    let base = data.path().to_path_buf();
    let (tx, rx) = crate::transport::commands();
    let (et, er) = crate::transport::events();
    let handle =
        std::thread::spawn(move || run_logged(rx, et, base, log, if legacy { 1 } else { 8 }));
    let started = Instant::now();
    tx.try_send(Action::Open(root, false))
        .map_err(anyhow::Error::msg)?;
    let mut next_probe = Instant::now();
    let mut probe = 0;
    let mut pending = None;
    let mut latency = Vec::new();
    let mut updates = 0;
    let mut count = None;
    let deadline = started + Duration::from_secs(180);
    while Instant::now() < deadline {
        if pending.is_none() && Instant::now() >= next_probe {
            probe += 1;
            tx.try_send(Action::Probe(probe))
                .map_err(anyhow::Error::msg)?;
            pending = Some((probe, Instant::now()));
        }
        match er.try_recv() {
            Ok(Event::Probe(id)) => {
                if let Some((expected, sent)) = pending
                    && id == expected
                {
                    latency.push(sent.elapsed().as_secs_f64() * 1000.);
                    pending = None;
                    next_probe = Instant::now() + Duration::from_millis(20);
                }
            }
            Ok(Event::Count(_)) => updates += 1,
            Ok(Event::ScanComplete(n)) => {
                count = Some(n);
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    let elapsed = started.elapsed().as_secs_f64() * 1000.;
    tx.try_send(Action::Shutdown).map_err(anyhow::Error::msg)?;
    drop(er);
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("controller panic"))?;
    anyhow::ensure!(count.is_some(), "scan benchmark deadline exceeded");
    latency.sort_by(f64::total_cmp);
    let p95 = latency
        .get(((latency.len().saturating_sub(1)) as f64 * 0.95).ceil() as usize)
        .copied();
    std::fs::write(
        output,
        serde_json::to_vec_pretty(
            &serde_json::json!({"scope":"directory metadata -> SQLite -> UI event consumer; no pixel decode or GPU/frame-time claim","entries":count,"legacy_one_batch":legacy,"elapsed_ms":elapsed,"ui_count_updates":updates,"command_probe_p95_ms":p95,"probe_samples_ms":latency}),
        )?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("viewer_update_tests.rs");
    fn settings(
        wheel_zoom: bool,
        trash_companions: bool,
        multi_view: bool,
        images_per_view: usize,
        image_size_limit: usize,
    ) -> Action {
        Action::Settings(Config {
            wheel_zoom,
            trash_companions,
            multi_view,
            images_per_view,
            image_size_limit,
            ..Config::default()
        })
    }

    struct Session {
        tx: crate::transport::CommandSender,
        events: crate::transport::EventReceiver,
        handle: Option<std::thread::JoinHandle<()>>,
        list: std::cell::Cell<u64>,
        selected: std::cell::Cell<i64>,
    }
    impl Session {
        fn new(base: PathBuf, hooks: BackgroundIo) -> Self {
            let telemetry = mygo_telemetry::Telemetry::new(&base).unwrap();
            let (tx, rx) = crate::transport::commands();
            let (out, events) = crate::transport::events();
            let handle =
                std::thread::spawn(move || run_logged_with_io(rx, out, base, telemetry, 8, hooks));
            Self {
                tx,
                events,
                handle: Some(handle),
                list: std::cell::Cell::new(0),
                selected: std::cell::Cell::new(0),
            }
        }
        fn send(&self, action: Action) {
            self.tx.try_send(action).unwrap();
        }
        fn wait(&self, predicate: impl Fn(&Event) -> bool) -> Event {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(Instant::now() < deadline, "event deadline exceeded");
                if let Ok(event) = self.events.recv_timeout(Duration::from_millis(20)) {
                    match &event {
                        Event::Reset { generation, .. } => self.list.set(*generation),
                        Event::Selection { id, .. } => self.selected.set(*id),
                        Event::Main { .. } => {
                            self.send(Action::ImageShown(self.list.get(), self.selected.get()))
                        }
                        Event::Thumbnail {
                            generation,
                            id,
                            frame: Some(_),
                            ..
                        } => self.send(Action::ImageShown(*generation, *id)),
                        _ => {}
                    }
                    if predicate(&event) {
                        return event;
                    }
                    if let Event::Error(error) = event {
                        panic!("{error}");
                    }
                }
            }
        }
        fn probe(&self, id: u64) -> Duration {
            let started = Instant::now();
            self.send(Action::Probe(id));
            self.wait(|event| matches!(event, Event::Probe(actual) if *actual == id));
            started.elapsed()
        }
        fn shutdown(&mut self) {
            self.send(Action::Shutdown);
            self.handle.take().unwrap().join().unwrap();
        }
    }
    impl Drop for Session {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = self.tx.try_send(Action::Shutdown);
                let _ = handle.join();
            }
        }
    }
    fn photo_folder(base: &Path, name: &str) -> PathBuf {
        let photos = base.join(name);
        std::fs::create_dir(&photos).unwrap();
        image::RgbImage::from_pixel(80, 40, image::Rgb([90, 20, 10]))
            .save(photos.join("a.png"))
            .unwrap();
        photos
    }
    #[test]
    fn slow_rank_does_not_block_commands_or_override_a_new_selection() {
        let dir = tempfile::tempdir().unwrap();
        let photos = photo_folder(dir.path(), "photos");
        for name in ["b.png", "c.png"] {
            std::fs::copy(photos.join("a.png"), photos.join(name)).unwrap();
        }
        let (entered, active) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let first = std::sync::atomic::AtomicBool::new(true);
        let mut session = Session::new(
            dir.path().join("data"),
            BackgroundIo {
                before_rank: Box::new(move || {
                    if first.swap(false, Ordering::Relaxed) {
                        entered.send(()).unwrap();
                        gate.recv_timeout(Duration::from_secs(5)).unwrap();
                    }
                }),
                ..BackgroundIo::default()
            },
        );
        session.send(Action::Open(photos, false));
        session.wait(|event| matches!(event, Event::ScanComplete(3)));
        active.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut samples = Vec::new();
        for id in 0..20 {
            let elapsed = session.probe(id);
            assert!(
                elapsed < Duration::from_millis(150),
                "rank blocked commands: {elapsed:?}"
            );
            samples.push(elapsed.as_secs_f64() * 1000.);
        }
        session.send(Action::ImageShown(
            session.list.get(),
            session.selected.get(),
        ));
        session.probe(99);
        session.send(Action::Sort(0, true));
        // Sort keeps the selected id while its old numeric position is stale.
        // A synchronous RAM delivery must not prefetch that same id recursively.
        session.wait(|event| matches!(event, Event::Main { .. }));
        session.send(Action::Select(0));
        let selected =
            match session.wait(|event| matches!(event, Event::Selection { index: 0, .. })) {
                Event::Selection { id, .. } => id,
                _ => unreachable!(),
            };
        release.send(()).unwrap();
        // Let stale results and the replaced request drain. The old ascending
        // rank must not relocate the new descending selection.
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            if let Ok(event) = session.events.recv_timeout(Duration::from_millis(10)) {
                assert!(!matches!(event, Event::Relocate { id, .. } if id != selected));
                assert!(!matches!(event, Event::Error(_)), "{event:?}");
            }
        }
        session.shutdown();
        println!("rank_command_probe_ms={samples:?}");
    }

    #[test]
    fn cancelled_explicit_file_rank_cannot_restart_loading() {
        let dir = tempfile::tempdir().unwrap();
        let photos = photo_folder(dir.path(), "photos");
        let (entered, active) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut session = Session::new(
            dir.path().join("data"),
            BackgroundIo {
                before_rank: Box::new(move || {
                    entered.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(5)).unwrap();
                }),
                ..BackgroundIo::default()
            },
        );
        session.send(Action::Open(photos.join("a.png"), false));
        active.recv_timeout(Duration::from_secs(2)).unwrap();
        session.send(Action::Cancel);
        session.probe(7);
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            if let Ok(event) = session.events.recv_timeout(Duration::from_millis(10)) {
                assert!(!matches!(
                    event,
                    Event::Selection { .. } | Event::Main { .. }
                ));
            }
        }
        session.shutdown();
    }

    #[test]
    fn production_source_batch_decodes_once_and_delivers_main_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.png");
        image::RgbaImage::from_pixel(200, 100, image::Rgba([80, 40, 20, 127]))
            .save(&path)
            .unwrap();
        let jobs = Scheduler::default();
        let thumb = Arc::new(Mutex::new(ByteLru::new(4 * MIB)));
        let main = Arc::new(Mutex::new(ByteLru::new(4 * MIB)));
        let (results, rx) = mpsc::sync_channel(8);
        let (writes, _writer) = mpsc::sync_channel(2);
        let log = mygo_telemetry::Telemetry::new(dir.path()).unwrap();
        for (request, target, priority, is_main) in [
            (1, 40, Priority::Visible, false),
            (2, 160, Priority::Main, true),
        ] {
            let mut task = subscriber(1, Cancel::default()).task;
            task.path = path.clone();
            task.target = target;
            task.main = is_main;
            jobs.submit(Job {
                request_id: request,
                generation_id: 1,
                file_id: task.entry.id,
                priority,
                cancel: Cancel::default(),
                payload: task,
            });
        }
        let stage = SourceStage {
            result_budget: PixelBudget::new(MIB),
            jobs: jobs.clone(),
            results,
            thumb,
            main,
            disk: Arc::new(Mutex::new(
                DiskCache::new(&dir.path().join("cache"), MIB as u64).unwrap(),
            )),
            epoch: Arc::new(AtomicU64::new(0)),
            bytes: Arc::new(AtomicU64::new(0)),
            ready: Arc::new(tokio::sync::Notify::new()),
            writes,
            log: log.clone(),
        }
        .spawn()
        .unwrap();
        let main = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let thumb = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!((main.request, thumb.request), (2, 1));
        for result in [main, thumb] {
            let frame = result.result.unwrap();
            let expected = mygo_decoder::decode(
                &path,
                result.task.target,
                &result.task.config,
                &Cancel::default(),
            )
            .unwrap();
            assert_eq!(frame.rgba, expected.rgba);
        }
        jobs.close();
        stage.join().unwrap();
        assert!(log.flush(Duration::from_secs(1)));
        let records: Vec<serde_json::Value> =
            std::fs::read_to_string(dir.path().join("diagnostics.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        let codecs: u64 = records
            .iter()
            .filter(|record| record["event"] == "source-codec-calls")
            .map(|record| record["value"].as_u64().unwrap())
            .sum();
        assert_eq!(codecs, 1);
        assert!(
            records
                .iter()
                .any(|record| record["event"] == "source-shared-batch" && record["ok"] == true)
        );
    }

    #[test]
    fn preview_growth_keeps_generation_and_does_not_replace_high_or_resume_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        image::RgbImage::from_pixel(1800, 1200, image::Rgb([90, 20, 10]))
            .save(photos.join("a.png"))
            .unwrap();
        let mut session = Session::new(dir.path().join("data"), BackgroundIo::default());
        session.send(Action::Open(photos, false));
        let generation = match session
            .wait(|event| matches!(event, Event::Main { frame, .. } if frame.width == 1280))
        {
            Event::Main { generation, .. } => generation,
            _ => unreachable!(),
        };
        session.send(Action::PreviewSize(1600));
        let event =
            session.wait(|event| matches!(event, Event::Main { frame, .. } if frame.width == 1600));
        assert!(matches!(event, Event::Main { generation: actual, .. } if actual == generation));
        session.send(Action::High);
        session.wait(|event| matches!(event, Event::Main { frame, .. } if frame.width == 1800));
        session.send(Action::PreviewSize(2048));
        session.probe(1);
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            if let Ok(event) = session.events.recv_timeout(Duration::from_millis(10)) {
                assert!(!matches!(
                    event,
                    Event::Main { .. } | Event::Selection { .. }
                ));
            }
        }
        session.send(Action::Cancel);
        session.probe(2);
        session.send(Action::PreviewSize(4096));
        session.probe(3);
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            if let Ok(event) = session.events.recv_timeout(Duration::from_millis(10)) {
                assert!(!matches!(
                    event,
                    Event::Main { .. } | Event::Selection { .. }
                ));
            }
        }
        session.shutdown();
        let config = Config::default();
        assert!(preview_cache_budget(&config, 1000) < config.main_mib * MIB);
        assert_eq!(
            preview_cache_budget(
                &Config {
                    main_mib: 1,
                    ..config
                },
                4096
            ),
            MIB
        );
    }

    #[test]
    fn startup_waits_for_presentation_then_preloads_the_configured_count() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        for index in 0..12 {
            image::RgbImage::from_pixel(80, 40, image::Rgb([index * 10, 20, 30]))
                .save(photos.join(format!("{index:02}.png")))
                .unwrap();
        }
        let base = dir.path().join("data");
        Config {
            preload_count: 3,
            ..Config::default()
        }
        .save(&base)
        .unwrap();
        let mut session = Session::new(base.clone(), BackgroundIo::default());
        session.send(Action::Open(photos, false));
        let (mut list, mut selected) = (0, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline);
            match session.events.recv_timeout(Duration::from_millis(20)) {
                Ok(Event::Reset { generation, .. }) => list = generation,
                Ok(Event::Selection { id, .. }) => selected = id,
                Ok(Event::Thumbnail { .. }) => panic!("thumbnail decoded before first main image"),
                Ok(Event::Main { .. }) => break,
                Ok(Event::Error(error)) => panic!("{error}"),
                _ => {}
            }
        }
        session.probe(700);
        std::thread::sleep(Duration::from_millis(80));
        while let Ok(event) = session.events.try_recv() {
            assert!(
                !matches!(event, Event::Main { .. } | Event::Thumbnail { .. }),
                "pixel work started before presentation acknowledgement"
            );
        }
        session.send(Action::ImageShown(list, selected));
        let count_decodes = || {
            std::fs::read_to_string(base.join("diagnostics.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|row| row["event"] == "main-decode" && row["ok"] == true)
                .count()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while count_decodes() < 4 {
            assert!(Instant::now() < deadline, "preload count not reached");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(count_decodes(), 4);
        session.send(Action::Settings(Config {
            preload_enabled: false,
            ..Config::default()
        }));
        session.send(Action::Select(8));
        session.wait(|event| matches!(event, Event::Main { frame, .. } if frame.rgba[0] == 80));
        session.shutdown();
        assert_eq!(
            count_decodes(),
            5,
            "disabled preloading decoded more main images"
        );
    }
    #[test]
    fn resume_restarts_at_saved_image_but_explicit_file_and_disabled_option_take_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        for index in 0..4 {
            image::RgbImage::from_pixel(80, 40, image::Rgb([index * 40, 20, 30]))
                .save(photos.join(format!("{index}.png")))
                .unwrap();
        }
        let base = dir.path().join("data");
        let config = Config {
            resume_last_image: true,
            preload_enabled: false,
            ..Config::default()
        };
        config.save(&base).unwrap();
        let mut session = Session::new(base.clone(), BackgroundIo::default());
        session.send(Action::Open(photos.clone(), false));
        session.wait(|event| matches!(event, Event::Main { .. }));
        session.send(Action::Select(2));
        session.wait(|event| matches!(event, Event::Main { frame, .. } if frame.rgba[0] == 80));
        session.shutdown();
        let mut session = Session::new(base.clone(), BackgroundIo::default());
        session.send(Action::Open(photos.clone(), false));
        let event = session.wait(|event| matches!(event, Event::Main { .. }));
        assert!(matches!(event, Event::Main { frame, .. } if frame.rgba[0] == 80));
        session.send(Action::Open(photos.join("1.png"), false));
        let event = session.wait(|event| matches!(event, Event::Main { .. }));
        assert!(matches!(event, Event::Main { frame, .. } if frame.rgba[0] == 40));
        session.send(Action::Settings(Config {
            resume_last_image: false,
            ..config
        }));
        session.send(Action::Open(photos, false));
        let event = session.wait(|event| matches!(event, Event::Main { .. }));
        assert!(matches!(event, Event::Main { frame, .. } if frame.rgba[0] == 0));
        session.shutdown();
    }
    #[test]
    fn copy_command_keeps_selection_index_and_original_companions() {
        let dir = tempfile::tempdir().unwrap();
        let photos = photo_folder(dir.path(), "photos");
        std::fs::write(photos.join("a.raw"), b"raw").unwrap();
        std::fs::write(photos.join("a.xmp"), b"metadata").unwrap();
        let base = dir.path().join("data");
        let destination = dir.path().join("edit");
        Config {
            copy_folder: destination.clone(),
            ..Config::default()
        }
        .save(&base)
        .unwrap();
        let mut session = Session::new(base, BackgroundIo::default());
        session.send(Action::Open(photos.clone(), false));
        session.wait(|event| matches!(event, Event::Main { .. }));
        session.send(Action::CopyCurrent);
        session
            .wait(|event| matches!(event, Event::Status(text) if text.contains("已複製 3 個檔案")));
        for name in ["a.png", "a.raw", "a.xmp"] {
            assert_eq!(
                std::fs::read(photos.join(name)).unwrap(),
                std::fs::read(destination.join(name)).unwrap()
            );
        }
        session.send(Action::Select(0));
        session.wait(|event| matches!(event, Event::Main { .. }));
        session.shutdown();
    }
    #[test]
    fn multi_view_waits_for_first_tile_and_descending_sort_preserves_selection() {
        let dir = tempfile::tempdir().unwrap();
        let photos = photo_folder(dir.path(), "photos");
        for index in 1..12 {
            std::fs::copy(
                photos.join("a.png"),
                photos.join(format!("z{index:02}.png")),
            )
            .unwrap();
        }
        let base = dir.path().join("data");
        Config {
            multi_view: true,
            preload_enabled: false,
            ..Config::default()
        }
        .save(&base)
        .unwrap();
        let mut session = Session::new(base, BackgroundIo::default());
        session.send(Action::Open(photos, false));
        let deadline = Instant::now() + Duration::from_secs(10);
        let (list, id) = loop {
            assert!(Instant::now() < deadline);
            if let Ok(Event::Thumbnail {
                generation,
                index,
                id,
                frame: Some(_),
                ..
            }) = session.events.recv_timeout(Duration::from_millis(20))
            {
                assert_eq!(index, 0);
                break (generation, id);
            }
        };
        std::thread::sleep(Duration::from_millis(80));
        while let Ok(event) = session.events.try_recv() {
            assert!(!matches!(event, Event::Thumbnail { .. }));
        }
        session.send(Action::ImageShown(list, id));
        session.wait(|event| {
            matches!(
                event,
                Event::Thumbnail {
                    index: 5,
                    frame: Some(_),
                    ..
                }
            )
        });
        session.send(Action::Sort(0, true));
        session.send(Action::Visible(0));
        let page = session.wait(|event| matches!(event, Event::Page { entries, .. } if entries.first().is_some_and(|entry| entry.1 == "z11.png")));
        assert!(
            matches!(page, Event::Page { entries, .. } if entries.last().is_some_and(|entry| entry.1 == "a.png"))
        );
        session.shutdown();
    }
    #[test]
    fn multi_view_keeps_next_viewport_and_refreshes_thumbnail_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        image::RgbImage::from_pixel(900, 600, image::Rgb([90, 20, 10]))
            .save(photos.join("00.png"))
            .unwrap();
        for i in 1..48 {
            std::fs::copy(photos.join("00.png"), photos.join(format!("{i:02}.png"))).unwrap();
        }
        let base = dir.path().join("data");
        Config {
            multi_view: true,
            images_per_view: 24,
            ..Config::default()
        }
        .save(&base)
        .unwrap();
        let mut session = Session::new(base, BackgroundIo::default());
        session.send(Action::Open(photos, false));
        session.wait(|event| matches!(event, Event::ScanComplete(48)));
        for (start, expected) in [(6, 42), (0, 48), (42, 6)] {
            session.send(Action::Visible(start));
            let event =
                session.wait(|event| matches!(event, Event::Page { start: s, .. } if *s == start));
            let Event::Page { entries, .. } = event else {
                unreachable!()
            };
            assert_eq!(entries.len(), expected);
            assert_eq!(entries[0].1, format!("{start:02}.png"));
        }
        for target in [240, 800, 160] {
            session.send(Action::ThumbnailSize(target));
            session.wait(|event| matches!(event, Event::Thumbnail { index: 42, frame: Some(f), .. } if f.width == target));
        }
        session.shutdown();
    }
    #[test]
    fn slow_settings_coalesce_without_blocking_ram_navigation_and_flush_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let photos = photo_folder(dir.path(), "photos");
        let base = dir.path().join("data");
        let (started, writes) = mpsc::channel();
        let mut session = Session::new(
            base.clone(),
            BackgroundIo {
                save: Box::new(move |config, path| {
                    started.send(config.images_per_view).unwrap();
                    std::thread::sleep(Duration::from_millis(500));
                    config.save(path)
                }),
                ..BackgroundIo::default()
            },
        );
        session.send(Action::Open(photos.clone(), false));
        session.wait(|event| matches!(event, Event::Main { .. }));
        std::fs::remove_file(photos.join("a.png")).unwrap();
        session.send(settings(false, true, false, 7, 0));
        assert_eq!(writes.recv_timeout(Duration::from_secs(2)).unwrap(), 7);
        let started = Instant::now();
        session.send(Action::Select(0));
        session.wait(|event| matches!(event, Event::Main { .. }));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "RAM delivery waited for settings IO"
        );
        for (id, count) in [(1, 8), (2, 9), (3, 10)] {
            session.send(settings(false, true, false, count, 0));
            assert!(session.probe(id) < Duration::from_millis(200));
        }
        let samples: Vec<_> = (10..30)
            .map(|id| session.probe(id).as_secs_f64() * 1000.)
            .collect();
        println!(
            "slow-settings-command-probes-ms={}",
            serde_json::to_string(&samples).unwrap()
        );
        session.send(settings(false, false, false, 24, 0));
        session.shutdown(); // Shutdown may overtake the last queued Settings.
        assert_eq!(Config::load(&base).unwrap().images_per_view, 24);
        assert!(!Config::load(&base).unwrap().trash_companions);
        assert_eq!(writes.try_iter().collect::<Vec<_>>(), vec![24]);
    }
    #[test]
    fn failed_settings_are_reported_and_next_revision_can_recover() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("data");
        let mut session = Session::new(
            base.clone(),
            BackgroundIo {
                save: Box::new(|config, path| {
                    if config.images_per_view == 7 {
                        anyhow::bail!("injected write failure");
                    }
                    config.save(path)
                }),
                ..BackgroundIo::default()
            },
        );
        session.send(settings(false, true, false, 7, 0));
        let error = session.wait(|event| matches!(event, Event::Error(_)));
        assert!(
            matches!(error, Event::Error(message) if message.contains("injected write failure") && message.contains("目前工作階段"))
        );
        assert_ne!(Config::load(&base).unwrap().images_per_view, 7);
        session.send(settings(false, true, false, 12, 0));
        session.wait(|event| matches!(event, Event::Status(message) if message == "設定已儲存"));
        session.shutdown();
        assert_eq!(Config::load(&base).unwrap().images_per_view, 12);
    }
    #[test]
    fn slow_source_preparation_uses_latest_scope_and_cancel_discards_late_result() {
        let dir = tempfile::tempdir().unwrap();
        let first = photo_folder(dir.path(), "first");
        let skipped = photo_folder(dir.path(), "skipped");
        let last = photo_folder(dir.path(), "last");
        let (started, preparing) = mpsc::channel();
        let mut session = Session::new(
            dir.path().join("data"),
            BackgroundIo {
                prepare: Box::new(move |path, recursive| {
                    started.send(path.clone()).unwrap();
                    std::thread::sleep(Duration::from_millis(500));
                    PreparedSource::open(path, recursive)
                }),
                ..BackgroundIo::default()
            },
        );
        session.send(Action::Open(first.clone(), false));
        assert_eq!(
            preparing.recv_timeout(Duration::from_secs(2)).unwrap(),
            first
        );
        session.send(Action::Open(skipped, false));
        session.probe(1);
        session.send(Action::Open(last.clone(), false));
        assert!(session.probe(2) < Duration::from_millis(200));
        let reset = session.wait(|event| matches!(event, Event::Reset { .. }));
        assert!(
            matches!(reset, Event::Reset { folder, .. } if folder == last.canonicalize().unwrap().display().to_string())
        );
        session.wait(|event| matches!(event, Event::Main { .. }));
        assert_eq!(
            preparing.recv_timeout(Duration::from_secs(2)).unwrap(),
            last
        );
        session.send(Action::Open(first.clone(), false));
        assert_eq!(
            preparing.recv_timeout(Duration::from_secs(2)).unwrap(),
            first
        );
        session.send(Action::Cancel);
        session.wait(|event| matches!(event, Event::Idle));
        std::thread::sleep(Duration::from_millis(550));
        while let Ok(event) = session.events.try_recv() {
            assert!(
                !matches!(event, Event::Reset { .. } | Event::Main { .. }),
                "cancelled open applied late"
            );
        }
        session.shutdown();
        assert!(preparing.try_recv().is_err());
    }
    #[test]
    fn slow_offline_probe_cannot_override_a_new_source_or_block_commands() {
        let dir = tempfile::tempdir().unwrap();
        let first = photo_folder(dir.path(), "first");
        let last = photo_folder(dir.path(), "last");
        let (started, probes) = mpsc::channel();
        let mut session = Session::new(
            dir.path().join("data"),
            BackgroundIo {
                probe: Box::new(move |_| {
                    started.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(500));
                    false
                }),
                ..BackgroundIo::default()
            },
        );
        session.send(Action::Open(first, false));
        session.wait(|event| matches!(event, Event::Main { .. }));
        session.send(Action::Retry);
        probes.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(session.probe(1) < Duration::from_millis(200));
        let samples: Vec<_> = (10..30)
            .map(|id| session.probe(id).as_secs_f64() * 1000.)
            .collect();
        println!(
            "slow-source-command-probes-ms={}",
            serde_json::to_string(&samples).unwrap()
        );
        session.send(Action::Open(last.clone(), false));
        assert!(session.probe(2) < Duration::from_millis(200));
        let reset = session.wait(|event| matches!(event, Event::Reset { .. }));
        assert!(
            matches!(reset, Event::Reset { folder, .. } if folder == last.canonicalize().unwrap().display().to_string())
        );
        session.wait(|event| matches!(event, Event::Main { .. }));
        session.shutdown();
    }
    #[test]
    fn stuck_settings_shutdown_is_bounded_and_never_claims_saved() {
        let dir = tempfile::tempdir().unwrap();
        let (started, active) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let mut session = Session::new(
            dir.path().join("data"),
            BackgroundIo {
                save: Box::new(move |_, _| {
                    started.send(()).unwrap();
                    let _ = gate.recv_timeout(Duration::from_secs(5));
                    finished.send(()).unwrap();
                    anyhow::bail!("injected blocked write released")
                }),
                ..BackgroundIo::default()
            },
        );
        session.send(settings(false, true, false, 7, 0));
        active.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        session.shutdown();
        assert!(started.elapsed() < Duration::from_secs(2));
        let mut timeout = false;
        while let Ok(event) = session.events.try_recv() {
            match event {
                Event::Status(message) => assert_ne!(message, "設定已儲存"),
                Event::Error(message) => timeout |= message.contains("關閉等待已逾時"),
                _ => {}
            }
        }
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(timeout);
    }
    struct CacheFixture {
        jobs: Scheduler<DecodeTask>,
        source: Scheduler<DecodeTask>,
        results: Option<Receiver<Decoded>>,
        ram: Ram,
        disk: Arc<Mutex<DiskCache>>,
        epoch: Arc<AtomicU64>,
        bytes: Arc<AtomicU64>,
        handle: Option<std::thread::JoinHandle<()>>,
    }
    impl CacheFixture {
        fn new(base: &Path) -> Self {
            let jobs = Scheduler::default();
            let source = Scheduler::default();
            let (tx, rx) = mpsc::sync_channel(1);
            let ram = Arc::new(Mutex::new(ByteLru::new(MIB)));
            let disk = Arc::new(Mutex::new(
                DiskCache::new(&base.join("cache"), MIB as u64).unwrap(),
            ));
            let epoch = Arc::new(AtomicU64::new(0));
            let bytes = Arc::new(AtomicU64::new(0));
            let handle = CacheStage {
                result_budget: PixelBudget::new(4 * MIB),
                jobs: jobs.clone(),
                source: source.clone(),
                results: tx,
                thumb: ram.clone(),
                disk: disk.clone(),
                epoch: epoch.clone(),
                bytes: bytes.clone(),
                ready: Arc::new(tokio::sync::Notify::new()),
                log: mygo_telemetry::Telemetry::new(base).unwrap(),
            }
            .spawn()
            .unwrap();
            Self {
                jobs,
                source,
                results: Some(rx),
                ram,
                disk,
                epoch,
                bytes,
                handle: Some(handle),
            }
        }
        fn job(request: u64) -> Job<DecodeTask> {
            let mut task = subscriber(1, Cancel::default()).task;
            task.main = false;
            task.target = 320;
            task.path = PathBuf::from("missing-original-must-not-be-opened.png");
            Job {
                request_id: request,
                generation_id: 1,
                file_id: task.entry.id,
                priority: Priority::Visible,
                cancel: Cancel::default(),
                payload: task,
            }
        }
        fn store(&self, task: &DecodeTask) {
            let frame = Frame {
                width: 2,
                height: 2,
                original_width: 2,
                original_height: 2,
                orientation: 1,
                rgba: Arc::new(vec![200; 16]),
            };
            self.disk
                .lock()
                .unwrap()
                .put(&task.key(), &mygo_decoder::encode_thumb(&frame).unwrap())
                .unwrap();
        }
    }
    impl Drop for CacheFixture {
        fn drop(&mut self) {
            self.jobs.close();
            self.source.close();
            drop(self.results.take());
            self.handle.take().unwrap().join().unwrap();
        }
    }
    #[test]
    fn disk_hit_finishes_while_source_lane_is_occupied_without_original_file() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = CacheFixture::new(dir.path());
        let job = CacheFixture::job(1);
        fixture.store(&job.payload);
        assert_eq!(fixture.ram.lock().unwrap().used(), 0);
        let (started, active) = mpsc::channel();
        let source = fixture.source.clone();
        source.submit(CacheFixture::job(2));
        let worker = std::thread::spawn(move || {
            let _job = source.pop().unwrap();
            started.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        active.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        fixture.jobs.submit(job);
        let result = fixture
            .results
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_millis(200))
            .unwrap();
        let elapsed = started.elapsed();
        println!(
            "disk-hit-with-500ms-source-work-ms={}",
            elapsed.as_secs_f64() * 1000.
        );
        assert_eq!(result.request, 1);
        assert_eq!(result.result.unwrap().rgba.as_ref(), &vec![200; 16]);
        assert!(!worker.is_finished());
        assert_eq!(fixture.source.pending(), 0);
        assert_eq!(fixture.bytes.load(Ordering::Relaxed), 16);
        worker.join().unwrap();
    }
    #[test]
    fn corrupt_or_busy_disk_falls_back_with_original_priority_and_cancel_token() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = CacheFixture::new(dir.path());
        let job = CacheFixture::job(1);
        fixture
            .disk
            .lock()
            .unwrap()
            .put(&job.payload.key(), b"bad thumbnail")
            .unwrap();
        assert_eq!(
            cached_frame(&job.payload, &fixture.ram, &fixture.disk).1,
            "cache-disk-invalid"
        );
        let lock = fixture.disk.lock().unwrap();
        assert_eq!(
            cached_frame(&job.payload, &fixture.ram, &fixture.disk).1,
            "cache-disk-busy"
        );
        let cancel = job.cancel.clone();
        fixture.jobs.submit(job);
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.source.pending() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(fixture.source.pending(), 1);
        let forwarded = fixture.source.pop().unwrap();
        assert_eq!(forwarded.priority, Priority::Visible);
        assert_eq!(forwarded.request_id, 1);
        cancel.cancel();
        assert!(forwarded.cancel.cancelled());
        assert!(fixture.results.as_ref().unwrap().try_recv().is_err());
        drop(lock);
    }
    #[test]
    fn old_cache_epoch_cannot_repopulate_ram_and_full_result_channel_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = CacheFixture::new(dir.path());
        let first = CacheFixture::job(1);
        fixture.store(&first.payload);
        fixture.epoch.store(1, Ordering::Release);
        fixture.jobs.submit(first);
        fixture
            .results
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        fixture.bytes.fetch_sub(16, Ordering::Relaxed);
        assert_eq!(fixture.ram.lock().unwrap().used(), 0);
        fixture.jobs.submit(CacheFixture::job(2));
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.bytes.load(Ordering::Relaxed) < 16 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        fixture.jobs.submit(CacheFixture::job(3));
        while fixture.bytes.load(Ordering::Relaxed) < 32 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(fixture.bytes.load(Ordering::Relaxed), 32);
        let started = Instant::now();
        drop(fixture);
        assert!(started.elapsed() < Duration::from_millis(200));
    }
    #[test]
    fn saturated_source_queue_cancels_rejected_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = CacheFixture::new(dir.path());
        for id in 100..124 {
            let mut job = CacheFixture::job(id);
            job.priority = Priority::Prefetch;
            assert!(fixture.source.submit(job));
        }
        let mut job = CacheFixture::job(1);
        job.priority = Priority::Prefetch;
        let cancel = job.cancel.clone();
        fixture.jobs.submit(job);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cancel.cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(cancel.cancelled());
        assert_eq!(fixture.source.pending(), 24);
        assert!(fixture.results.as_ref().unwrap().try_recv().is_err());
    }
    fn subscriber(generation: u64, cancel: Cancel) -> Subscriber {
        Subscriber {
            generation,
            cancel,
            task: DecodeTask {
                config: Config::default(),
                enqueued: Instant::now(),
                cache_epoch: 0,
                entry: Entry {
                    id: 1,
                    relative: "photo.jpg".into(),
                    size: 123,
                    modified_ns: 10,
                },
                path: PathBuf::from("photo.jpg"),
                scope: "card".into(),
                target: 1600,
                index: 0,
                list: 1,
                main: true,
            },
        }
    }
    #[test]
    fn ram_reselection_delivers_without_reopening_source() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        let path = photos.join("a.png");
        image::RgbImage::from_pixel(80, 40, image::Rgb([90, 20, 10]))
            .save(&path)
            .unwrap();
        let base = dir.path().join("data");
        let (tx, rx) = crate::transport::commands();
        let (et, er) = crate::transport::events();
        let handle = std::thread::spawn(move || run_at(rx, et, base));
        tx.try_send(Action::Open(photos, false)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut loaded = false;
        let mut first_generation = 0;
        let mut scanned = false;
        while Instant::now() < deadline && !(loaded && scanned) {
            match er.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Main { generation, .. }) => {
                    loaded = true;
                    first_generation = generation;
                }
                Ok(Event::ScanComplete(_)) => scanned = true,
                _ => {}
            }
        }
        assert!(loaded && scanned);
        std::fs::remove_file(path).unwrap();
        tx.try_send(Action::Select(0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut cached = false;
        while Instant::now() < deadline {
            if let Ok(Event::Main { generation, .. }) = er.recv_timeout(Duration::from_millis(20))
                && generation > first_generation
            {
                cached = true;
                break;
            }
        }
        tx.try_send(Action::Shutdown).unwrap();
        drop(er);
        handle.join().unwrap();
        assert!(
            cached,
            "RAM hit reopened the removed source or missed delivery"
        );
    }
    #[test]
    fn same_image_handoff_and_independent_subscriber_cancellation() {
        let old = Cancel::default();
        let current = Cancel::default();
        let other = Cancel::default();
        let group = Cancel::default();
        let mut pending = Pending {
            request: 42,
            cancel: group.clone(),
            subscribers: vec![subscriber(1, old.clone())],
        };
        old.cancel();
        assert!(pending.subscribe(subscriber(2, current.clone())));
        assert!(pending.retain_live());
        assert!(!group.cancelled());
        assert_eq!(pending.request, 42);
        assert_eq!(pending.subscribers.len(), 1);
        assert_eq!(pending.subscribers[0].generation, 2);
        assert!(pending.subscribe(subscriber(3, other.clone())));
        current.cancel();
        assert!(pending.retain_live());
        assert!(!group.cancelled());
        other.cancel();
        assert!(!pending.retain_live());
        assert!(group.cancelled());
        assert!(
            !pending.subscribe(subscriber(4, Cancel::default())),
            "a cancelled codec cannot be revived"
        );
    }
    #[test]
    fn decode_key_tracks_content_and_limits_independently_of_subscriber_position() {
        let first = subscriber(1, Cancel::default()).task;
        let old_scope = format!(
            "{}:rgba-v2:size-{}:{:?}",
            first.scope,
            first.config.image_size_limit,
            first.config.image_limits()
        );
        assert_ne!(
            first.key(),
            mygo_cache::key(&old_scope, &first.entry, first.target, 0)
        );
        let mut next = first.clone();
        next.list = 99;
        next.index = 500;
        next.main = false;
        assert_eq!(first.key(), next.key());
        next.entry.modified_ns += 1;
        assert_ne!(first.key(), next.key());
        next = first.clone();
        next.target = 320;
        assert_ne!(first.key(), next.key());
        next = first.clone();
        next.config.image_size_limit = 2;
        assert_ne!(first.key(), next.key());
        next = first.clone();
        next.scope = "other card".into();
        assert_ne!(first.key(), next.key());
    }
    #[test]
    fn background_trash_allows_navigation_cancel_and_bounded_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        for i in 0..2 {
            image::RgbImage::from_pixel(80, 40, image::Rgb([40 + i * 80, 20, 30]))
                .save(photos.join(format!("{i}.png")))
                .unwrap();
        }
        std::fs::File::create(photos.join("0.raw"))
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let base = dir.path().join("data");
        let (tx, rx) = crate::transport::commands();
        let (et, er) = crate::transport::events();
        let handle = std::thread::spawn(move || run_at(rx, et, base));
        tx.try_send(Action::Open(photos.clone(), false)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut selected = None;
        let mut complete = false;
        while Instant::now() < deadline {
            match er.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Selection { id, index: 0, .. }) => selected = Some(id),
                Ok(Event::ScanComplete(_)) => complete = true,
                Ok(Event::Error(e)) => panic!("{e}"),
                _ => {}
            }
            if selected.is_some() && complete {
                break;
            }
        }
        let id = selected.expect("first selection");
        assert!(complete);
        tx.try_send(settings(true, true, false, 6, 0)).unwrap();
        tx.try_send(Action::Trash(id)).unwrap();
        tx.try_send(Action::Navigate(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut navigated = false;
        while Instant::now() < deadline {
            if matches!(
                er.recv_timeout(Duration::from_millis(20)),
                Ok(Event::Selection { index: 1, .. })
            ) {
                navigated = true;
                break;
            }
        }
        tx.try_send(Action::Cancel).unwrap();
        tx.try_send(Action::Shutdown).unwrap();
        drop(er);
        let started = Instant::now();
        handle.join().unwrap();
        assert!(navigated, "navigation was blocked by file IO");
        assert!(started.elapsed() < Duration::from_secs(2));
        for name in ["0.png", "0.raw"] {
            if !photos.join(name).exists() {
                assert!(dir.path().join("data/trash").join(name).is_file());
            }
        }
    }
    #[test]
    fn full_ui_mailbox_cannot_prevent_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let (tx, rx) = crate::transport::commands();
        let (et, _er) = crate::transport::events();
        let handle = std::thread::spawn(move || run_at(rx, et, base));
        for _ in 0..10000 {
            let _ = tx.try_send(Action::Sort(0, false));
        }
        tx.try_send(Action::Cancel).unwrap();
        tx.try_send(Action::Shutdown).unwrap();
        let started = Instant::now();
        handle.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn corrupt_settings_start_with_defaults_and_notify_ui() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), b"broken").unwrap();
        let base = dir.path().to_path_buf();
        let (tx, rx) = crate::transport::commands();
        let (et, er) = crate::transport::events();
        let h = std::thread::spawn(move || run_at(rx, et, base));
        let settings = er.recv_timeout(Duration::from_secs(10)).unwrap();
        let notice = er.recv_timeout(Duration::from_secs(10)).unwrap();
        tx.blocking_send(Action::Shutdown).unwrap();
        drop(er);
        h.join().unwrap();
        assert!(matches!(settings, Event::Settings(config, _) if config.workers == 2));
        assert!(
            matches!(notice, Event::Error(text) if text.contains("已恢復預設值") && text.contains("settings.corrupt-"))
        );
        assert!(Config::load(dir.path()).is_ok());
    }

    #[test]
    fn pipeline_navigation_stale_results_and_missing_source() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        for i in 0..3u8 {
            image::RgbImage::from_pixel(80, 40, image::Rgb([i * 80, 20, 30]))
                .save(photos.join(format!("{i}.png")))
                .unwrap();
        }
        let base = dir.path().join("data");
        let (tx, rx) = crate::transport::commands();
        let (et, er) = crate::transport::events();
        let h = std::thread::spawn(move || run_at(rx, et, base));
        tx.blocking_send(Action::Open(photos.clone(), false))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut complete = false;
        let mut first = false;
        while Instant::now() < deadline {
            match er.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Main { .. }) => first = true,
                Ok(Event::ScanComplete(_)) => complete = true,
                Ok(Event::Error(e)) => panic!("{e}"),
                _ => {}
            }
            if first && complete {
                break;
            }
        }
        assert!(first && complete);
        tx.blocking_send(Action::Select(1)).unwrap();
        tx.blocking_send(Action::Select(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut selected_two = false;
        let mut last = false;
        while Instant::now() < deadline {
            let event = er.recv_timeout(Duration::from_millis(100));
            match event {
                Ok(Event::Selection { index: 2, .. }) => selected_two = true,
                Ok(Event::Main { frame: f, .. }) if selected_two => {
                    assert_eq!(f.rgba[0], 160, "stale or prefetch image published");
                    last = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(last);
        std::fs::remove_file(photos.join("1.png")).unwrap();
        tx.blocking_send(Action::Select(1)).unwrap(); // May be cached: removal must remain recoverable.
        tx.blocking_send(Action::Open(dir.path().join("missing"), false))
            .unwrap();
        let mut error = false;
        for _ in 0..100 {
            if let Ok(Event::Error(_)) = er.recv_timeout(Duration::from_millis(50)) {
                error = true;
                break;
            }
        }
        assert!(error);
        let offline = dir.path().join("offline");
        // Windows may briefly deny a directory rename while a cancelled scan
        // or preload still holds an OS handle. Simulate unplug after it drains.
        let rename_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::rename(&photos, &offline) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        && Instant::now() < rename_deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("cannot simulate source removal: {error}"),
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut disconnected = false;
        while Instant::now() < deadline {
            if let Ok(Event::Error(e)) = er.recv_timeout(Duration::from_millis(100))
                && e.contains("保留目前畫面")
            {
                disconnected = true;
                break;
            }
        }
        assert!(disconnected, "source disappearance was not detected");
        std::fs::rename(offline, &photos).unwrap();
        tx.blocking_send(Action::Rescan(false)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reconnected = false;
        while Instant::now() < deadline {
            if let Ok(Event::Main { .. }) = er.recv_timeout(Duration::from_millis(100)) {
                reconnected = true;
                break;
            }
        }
        assert!(reconnected);
        tx.blocking_send(Action::Shutdown).unwrap();
        drop(er);
        h.join().unwrap();
    }
}
