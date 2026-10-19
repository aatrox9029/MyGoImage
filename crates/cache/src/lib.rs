use anyhow::Result;
use mygo_core::Entry;
use std::{
    collections::{BTreeMap, HashMap},
    hash::Hash,
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub struct ByteLru<K, V> {
    items: HashMap<K, (V, usize, u64)>,
    order: BTreeMap<u64, K>,
    budget: usize,
    used: usize,
    clock: u64,
}
impl<K: Eq + Hash + Clone, V: Clone> ByteLru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self {
            items: HashMap::new(),
            order: BTreeMap::new(),
            budget,
            used: 0,
            clock: 0,
        }
    }
    pub fn get(&mut self, k: &K) -> Option<V> {
        self.clock += 1;
        self.items.get_mut(k).map(|(v, _, t)| {
            self.order.remove(t);
            *t = self.clock;
            self.order.insert(*t, k.clone());
            v.clone()
        })
    }
    pub fn put(&mut self, k: K, v: V, bytes: usize) {
        if let Some((_, n, t)) = self.items.remove(&k) {
            self.order.remove(&t);
            self.used -= n;
        }
        if bytes > self.budget {
            return;
        }
        while self.used > self.budget - bytes {
            let Some((_, key)) = self.order.pop_first() else {
                break;
            };
            if let Some((_, n, _)) = self.items.remove(&key) {
                self.used -= n;
            }
        }
        self.clock += 1;
        self.used += bytes;
        self.order.insert(self.clock, k.clone());
        self.items.insert(k, (v, bytes, self.clock));
    }
    pub fn used(&self) -> usize {
        self.used
    }
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        while self.used > budget {
            let Some((_, key)) = self.order.pop_first() else {
                break;
            };
            if let Some((_, bytes, _)) = self.items.remove(&key) {
                self.used -= bytes;
            }
        }
    }
    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.used = 0;
    }
}
pub fn key(scope: &str, e: &Entry, target: u32, orientation: u8) -> String {
    // Metadata invalidates orientation changes as well; include orientation/version explicitly.
    let s = format!(
        "thumb-v1\0{scope}\0{}\0{}\0{}\0{target}\0{orientation}",
        e.relative.to_lowercase(),
        e.size,
        e.modified_ns
    );
    blake3::hash(s.as_bytes()).to_hex().to_string()
}
pub struct DiskCache {
    dir: PathBuf,
    budget: u64,
    size: std::cell::Cell<u64>,
    access: std::cell::RefCell<HashMap<String, u128>>,
    clock: std::cell::Cell<u128>,
    last_touch: std::cell::RefCell<HashMap<String, u128>>,
}
impl DiskCache {
    pub fn new(dir: &Path, budget: u64) -> Result<Self> {
        let s = Self::open_lazy(dir, budget)?;
        s.reconcile()?;
        Ok(s)
    }
    /// Reads are safe immediately; callers must reconcile before allowing writes.
    pub fn open_lazy(dir: &Path, budget: u64) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.into(),
            budget,
            size: std::cell::Cell::new(0),
            access: std::cell::RefCell::new(HashMap::new()),
            clock: std::cell::Cell::new(0),
            last_touch: std::cell::RefCell::new(HashMap::new()),
        })
    }
    pub fn reconcile(&self) -> Result<()> {
        self.prune()
    }
    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.thumb"))
    }
    fn touch(&self, key: &str) {
        let t = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .max(self.clock.get() + 1);
        self.clock.set(t);
        self.access.borrow_mut().insert(key.to_string(), t);
    }
    pub fn get(&self, key: &str, max: u64) -> Option<Vec<u8>> {
        if self.budget == 0 {
            return None;
        }
        let p = self.path(key);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p)
            .ok()?;
        if f.metadata().ok()?.len() > max {
            return None;
        }
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let needs_touch = self
            .last_touch
            .borrow()
            .get(key)
            .is_none_or(|previous| now.saturating_sub(*previous) >= 30_000_000_000);
        if needs_touch
            && f.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()))
                .is_ok()
        {
            self.last_touch.borrow_mut().insert(key.to_string(), now);
        }
        self.touch(key);
        use std::io::Read;
        let mut bytes = vec![];
        f.take(max + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > max {
            None
        } else {
            Some(bytes)
        }
    }
    pub fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        if data.len() as u64 > self.budget || self.budget == 0 {
            return Ok(());
        }
        let mut temp = tempfile::NamedTempFile::new_in(&self.dir)?;
        temp.write_all(data)?;
        temp.as_file().sync_all()?;
        // persist replaces atomically on Windows (MoveFileExW) and Unix (rename).
        let old = std::fs::metadata(self.path(key))
            .map(|m| m.len())
            .unwrap_or(0);
        temp.persist(self.path(key))?;
        self.touch(key);
        self.size
            .set(self.size.get().saturating_sub(old) + data.len() as u64);
        if self.size.get() > self.budget {
            self.prune()?;
        }
        Ok(())
    }
    pub fn clear(&self) -> Result<()> {
        self.clear_cancelled(&mygo_core::Cancel::default())
    }
    pub fn clear_cancelled(&self, cancel: &mygo_core::Cancel) -> Result<()> {
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            anyhow::ensure!(!cancel.cancelled(), "快取清除已取消");
            if e.path().extension().is_some_and(|x| x == "thumb") {
                std::fs::remove_file(e.path())?;
            }
        }
        self.size.set(0);
        self.access.borrow_mut().clear();
        self.last_touch.borrow_mut().clear();
        Ok(())
    }
    fn prune(&self) -> Result<()> {
        let mut entries = vec![];
        let mut total = 0;
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            if e.path().extension().is_some_and(|x| x == "thumb") {
                let m = e.metadata()?;
                total += m.len();
                entries.push((
                    self.access
                        .borrow()
                        .get(
                            &e.path()
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string(),
                        )
                        .copied()
                        .unwrap_or_else(|| {
                            m.modified()
                                .unwrap_or(SystemTime::UNIX_EPOCH)
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos()
                        }),
                    m.len(),
                    e.path(),
                ));
            }
        }
        let live: std::collections::HashSet<_> = entries
            .iter()
            .filter_map(|(_, _, p)| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .collect();
        self.access.borrow_mut().retain(|key, _| live.contains(key));
        self.last_touch
            .borrow_mut()
            .retain(|key, _| live.contains(key));
        let target = if total > self.budget {
            self.budget * 9 / 10
        } else {
            self.budget
        };
        if total > self.budget {
            entries.sort_unstable_by_key(|e| e.0);
        }
        for (_, n, p) in entries {
            if total <= target {
                break;
            }
            std::fs::remove_file(&p)?;
            self.access.borrow_mut().remove(
                &p.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            );
            total -= n;
        }
        self.size.set(total);
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shrinking_budget_evicts_old_history_and_keeps_recent_frames() {
        let mut cache = ByteLru::new(16);
        for id in 0..4 {
            cache.put(id, id, 4);
        }
        cache.get(&0);
        cache.set_budget(8);
        assert_eq!(cache.used(), 8);
        assert_eq!(cache.get(&0), Some(0));
        assert_eq!(cache.get(&3), Some(3));
        assert_eq!(cache.get(&1), None);
        cache.set_budget(0);
        assert_eq!(cache.used(), 0);
    }
    #[test]
    fn lazy_reads_precede_reconciliation_and_reconciled_writes_obey_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.thumb"), b"1234").unwrap();
        std::fs::write(dir.path().join("b.thumb"), b"5678").unwrap();
        let cache = DiskCache::open_lazy(dir.path(), 4).unwrap();
        assert_eq!(cache.get("a", 8).unwrap(), b"1234");
        cache.reconcile().unwrap();
        cache.put("c", b"data").unwrap();
        let total: u64 = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(total <= 4);
        cache.clear().unwrap();
        assert!(cache.get("c", 8).is_none());
    }
    #[test]
    fn byte_eviction_and_recency() {
        let mut l = ByteLru::new(8);
        l.put(1, 1, 4);
        l.put(2, 2, 4);
        l.get(&1);
        l.put(3, 3, 4);
        assert_eq!(l.get(&2), None);
        assert!(l.used() <= 8);
        l.put(4, 4, 9);
        assert_eq!(l.get(&4), None);
        l.put(1, 9, 2);
        assert_eq!(l.get(&1), Some(9));
        l.put(5, 5, 8);
        assert_eq!(l.used(), 8);
        assert_eq!(l.get(&1), None);
        l.clear();
        assert_eq!(l.used(), 0);
    }
    #[test]
    fn persists_replaces_and_bounds() {
        let d = tempfile::tempdir().unwrap();
        {
            let c = DiskCache::new(d.path(), 8).unwrap();
            c.put("a", b"1234").unwrap();
            c.put("a", b"abcd").unwrap();
            assert_eq!(c.get("a", 8).unwrap(), b"abcd");
        }
        let c = DiskCache::new(d.path(), 4).unwrap();
        assert_eq!(c.get("a", 8).unwrap(), b"abcd");
        c.put("b", b"5678").unwrap();
        assert!(c.get("a", 8).is_none());
    }
}
