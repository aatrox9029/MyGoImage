//! Bounded in-flight sharing; source memory is released before the next job.
use super::*;

pub(super) struct SourceStage {
    pub(super) result_budget: PixelBudget,
    pub(super) jobs: Scheduler<DecodeTask>,
    pub(super) results: mpsc::SyncSender<Decoded>,
    pub(super) thumb: Ram,
    pub(super) main: Ram,
    pub(super) disk: Arc<Mutex<DiskCache>>,
    pub(super) epoch: Arc<AtomicU64>,
    pub(super) bytes: Arc<AtomicU64>,
    pub(super) ready: Arc<tokio::sync::Notify>,
    pub(super) writes: mpsc::SyncSender<(String, Frame, Cancel, u64)>,
    pub(super) log: mygo_telemetry::Telemetry,
}
impl SourceStage {
    pub(super) fn spawn(self) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("source-decode".into())
            .spawn(move || {
                while let Some(first) = self.jobs.pop() {
                    let mut batch = vec![first];
                    let mut delivered = Vec::new();
                    if batch[0].payload.shareable() {
                        self.gather(&mut batch);
                        batch.sort_by_key(|job| job.priority as usize);
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            self.shared(&mut batch, &mut delivered)
                        }))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("共享解碼器無法處理此圖片")));
                        if let Err(error) = &result {
                            self.log.trace(
                                "source-shared-fallback",
                                batch[0].request_id,
                                batch[0].generation_id,
                                &format!("{}: {error:#}", batch[0].payload.key()),
                                0,
                                false,
                            );
                        }
                        self.log.record(
                            "source-shared-batch",
                            0,
                            batch.len() as u64,
                            result.is_ok(),
                        );
                    }
                    batch.sort_by_key(|job| job.priority as usize);
                    for job in &batch {
                        if delivered.contains(&job.request_id) || job.cancel.cancelled() {
                            continue;
                        }
                        if self.cached(job) {
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

    fn gather(&self, batch: &mut Vec<Job<DecodeTask>>) {
        let related = self
            .jobs
            .take_matching(8 - batch.len(), |task| batch[0].payload.same_source(task));
        for job in &related {
            self.log.trace(
                "source-join",
                job.request_id,
                job.generation_id,
                &job.payload.key(),
                job.payload.enqueued.elapsed().as_micros(),
                true,
            );
        }
        batch.extend(related);
    }

    fn cached(&self, job: &Job<DecodeTask>) -> bool {
        if job.cancel.cancelled() {
            return true;
        }
        let task = &job.payload;
        self.log.record(
            "decode-queue-wait",
            task.enqueued.elapsed().as_micros(),
            0,
            true,
        );
        let cache = if task.main { &self.main } else { &self.thumb };
        let started = Instant::now();
        let (hit, origin) = cached_frame(task, cache, &self.disk);
        self.log.trace(
            origin,
            job.request_id,
            job.generation_id,
            &task.key(),
            started.elapsed().as_micros(),
            true,
        );
        if let Some(frame) = hit {
            self.complete(job, Ok(frame), true);
            true
        } else {
            false
        }
    }

    fn shared(&self, batch: &mut Vec<Job<DecodeTask>>, delivered: &mut Vec<u64>) -> Result<()> {
        // Resolve cache hits before any codec work. Keep every drained job in batch
        // so a decoder error or panic cannot lose a subscriber.
        for job in batch.iter() {
            if self.cached(job) {
                delivered.push(job.request_id);
            }
        }
        if delivered.len() == batch.len() {
            return Ok(());
        }
        let cancel = Cancel::when_all(batch.iter().map(|job| job.cancel.clone()).collect());
        let first = &batch[0];
        self.started(first);
        let mut source =
            mygo_decoder::SharedSource::open(&first.payload.path, &first.payload.config, &cancel)?;
        self.gather(batch);
        let pending = batch
            .iter()
            .filter(|job| !delivered.contains(&job.request_id))
            .collect::<Vec<_>>();
        if pending.len() == 1 && source.is_native_size(pending[0].payload.target) {
            let job = pending[0];
            let result = source.finish(job.payload.target, &job.cancel);
            if !job.cancel.cancelled() {
                let (frame, timings) = result?;
                self.timings(job, timings);
                self.complete(job, Ok(frame), false);
            }
            delivered.push(job.request_id);
            return Ok(());
        }
        self.finish_shared(&mut source, batch, delivered)
    }

    pub(super) fn finish_shared(
        &self,
        source: &mut mygo_decoder::SharedSource,
        batch: &mut Vec<Job<DecodeTask>>,
        delivered: &mut Vec<u64>,
    ) -> Result<()> {
        loop {
            // Admit arrivals during codec/resize, without waiting or retaining a
            // source across operations. At most eight subscribers per source.
            self.gather(batch);
            let next = batch
                .iter()
                .filter(|job| !delivered.contains(&job.request_id))
                .min_by_key(|job| job.priority as usize);
            let Some(job) = next else {
                break;
            };
            if !self.cached(job) {
                self.started(job);
                let result = source.render(job.payload.target, &job.cancel);
                if !job.cancel.cancelled() {
                    self.complete(job, Ok(result?), false);
                }
            }
            delivered.push(job.request_id);
        }
        let timings = source.timings();
        self.log.record(
            "source-shared-total",
            timings.total_us as u128,
            batch.len() as u64,
            true,
        );
        self.timings(&batch[0], timings);
        Ok(())
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
        self.log.trace(
            "source-codec-summary",
            job.request_id,
            job.generation_id,
            &format!(
                "{}; codec_calls={}; source_opens={}; read_bytes={}",
                job.payload.key(),
                timings.codec_calls,
                timings.source_opens,
                timings.read_bytes
            ),
            timings.total_us as u128,
            true,
        );
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
