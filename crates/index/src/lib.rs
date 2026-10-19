use anyhow::Result;
use mygo_core::{Entry, Sort};
use rusqlite::{Connection, params};
use std::{
    cell::{Cell, RefCell},
    path::Path,
};

struct Window {
    start: usize,
    sort: Sort,
    desc: bool,
    entries: Vec<Entry>,
}

/// Owned exclusively by the controller thread: exactly one database writer.
pub struct Index {
    conn: Connection,
    volume: i64,
    scan: i64,
    windows: RefCell<Vec<Window>>,
    count_cache: Cell<Option<usize>>,
    revision: Cell<u64>,
}
impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-8192;
            CREATE TABLE IF NOT EXISTS volumes(id INTEGER PRIMARY KEY, volume_key TEXT NOT NULL UNIQUE, last_seen_utc INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS images(id INTEGER PRIMARY KEY, volume_id INTEGER NOT NULL, relative_path TEXT NOT NULL, file_size INTEGER NOT NULL, modified_ns INTEGER, width INTEGER, height INTEGER, format TEXT, orientation INTEGER, last_seen_scan INTEGER NOT NULL, UNIQUE(volume_id, relative_path));
            CREATE INDEX IF NOT EXISTS idx_path ON images(volume_id,relative_path COLLATE NOCASE,id);
            CREATE INDEX IF NOT EXISTS idx_modified ON images(volume_id,modified_ns,id);
            CREATE INDEX IF NOT EXISTS idx_size ON images(volume_id,file_size,id);
            CREATE TABLE IF NOT EXISTS folder_selection(volume_id INTEGER PRIMARY KEY, relative_path TEXT NOT NULL);")?;
        Ok(Self {
            conn,
            volume: 0,
            scan: 0,
            windows: RefCell::new(Vec::new()),
            count_cache: Cell::new(None),
            revision: Cell::new(0),
        })
    }
    /// A separate WAL reader never mutates scope or schema on the writer.
    pub fn open_reader(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_millis(100))?;
        Ok(Self {
            conn,
            volume: 0,
            scan: 0,
            windows: RefCell::new(Vec::new()),
            count_cache: Cell::new(None),
            revision: Cell::new(0),
        })
    }
    pub fn read_scope(&mut self, key: &str) -> Result<()> {
        self.volume =
            self.conn
                .query_row("SELECT id FROM volumes WHERE volume_key=?1", [key], |r| {
                    r.get(0)
                })?;
        self.invalidate();
        Ok(())
    }
    pub fn revision(&self) -> u64 {
        self.revision.get()
    }
    pub fn scope(&mut self, key: &str) -> Result<()> {
        self.invalidate();
        self.scan = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_micros() as i64;
        self.conn.execute("INSERT INTO volumes(volume_key,last_seen_utc) VALUES(?1,?2) ON CONFLICT(volume_key) DO UPDATE SET last_seen_utc=excluded.last_seen_utc",params![key,self.scan])?;
        self.volume =
            self.conn
                .query_row("SELECT id FROM volumes WHERE volume_key=?1", [key], |r| {
                    r.get(0)
                })?;
        Ok(())
    }
    pub fn last_selection(&self) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT relative_path FROM folder_selection WHERE volume_id=?1",
                [self.volume],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub fn remember_selection(&self, path: &str) -> Result<()> {
        self.conn.execute("INSERT INTO folder_selection(volume_id,relative_path) VALUES(?1,?2) ON CONFLICT(volume_id) DO UPDATE SET relative_path=excluded.relative_path", params![self.volume, path])?;
        Ok(())
    }
    pub fn upsert(&mut self, entries: &[Entry]) -> Result<()> {
        let count = self.count_cache.get();
        self.invalidate();
        let tx = self.conn.transaction()?;
        let mut inserted = 0;
        {
            let mut s=tx.prepare_cached("INSERT INTO images(volume_id,relative_path,file_size,modified_ns,last_seen_scan) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(volume_id,relative_path) DO NOTHING")?;
            let mut update=tx.prepare_cached("UPDATE images SET file_size=?3,modified_ns=?4,last_seen_scan=?5 WHERE volume_id=?1 AND relative_path=?2")?;
            for e in entries {
                let values = params![
                    self.volume,
                    e.relative,
                    e.size as i64,
                    e.modified_ns,
                    self.scan
                ];
                if s.execute(values)? == 1 {
                    inserted += 1;
                } else {
                    update.execute(values)?;
                }
            }
        }
        tx.commit()?;
        // Avoid a full covering-index count after every incremental scan batch.
        // Failed transactions leave the cache invalid; duplicates count once.
        self.count_cache.set(count.map(|count| count + inserted));
        Ok(())
    }
    pub fn begin_scan(&mut self) -> Result<()> {
        self.scan = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_micros() as i64)
            .max(self.scan + 1);
        Ok(())
    }
    /// Prune only after a complete, error-free scan, never after hot removal/cancel.
    pub fn finish(&self) -> Result<()> {
        self.invalidate();
        self.conn.execute(
            "DELETE FROM images WHERE volume_id=?1 AND last_seen_scan<>?2",
            params![self.volume, self.scan],
        )?;
        Ok(())
    }
    fn invalidate(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        self.windows.borrow_mut().clear();
        self.count_cache.set(None);
    }
    pub fn count(&self) -> Result<usize> {
        if let Some(count) = self.count_cache.get() {
            return Ok(count);
        }
        let count = self.conn.query_row(
            "SELECT count(*) FROM images WHERE volume_id=?1",
            [self.volume],
            |r| r.get::<_, i64>(0),
        )? as usize;
        self.count_cache.set(Some(count));
        Ok(count)
    }
    pub fn remove_paths(&mut self, paths: &[String]) -> Result<()> {
        self.invalidate();
        let tx = self.conn.transaction()?;
        for path in paths {
            tx.execute(
                "DELETE FROM images WHERE volume_id=?1 AND relative_path=?2",
                params![self.volume, path],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Reconcile a completed file operation without changing the current scan scope.
    pub fn remove_paths_in_scope(&mut self, scope: &str, paths: &[String]) -> Result<()> {
        self.invalidate();
        let tx = self.conn.transaction()?;
        for path in paths {
            tx.execute("DELETE FROM images WHERE volume_id=(SELECT id FROM volumes WHERE volume_key=?1) AND relative_path=?2", params![scope,path])?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Two range seeks preserve NOCASE ties without the tuple-comparison scan.
    pub fn after(&self, anchor: &Entry, len: usize, sort: Sort, desc: bool) -> Result<Vec<Entry>> {
        let column = sort.column();
        let dir = if desc { "DESC" } else { "ASC" };
        let op = if desc { "<" } else { ">" };
        let size = anchor.size as i64;
        let value: &dyn rusqlite::ToSql = match sort {
            Sort::Name => &anchor.relative,
            Sort::Modified => &anchor.modified_ns,
            Sort::Size => &size,
        };
        let mut result = Vec::new();
        for predicate in [
            format!("{column}=?2 AND id {op} ?3"),
            format!("{column} {op} ?2 AND ?3 IS NOT NULL"),
        ] {
            if result.len() >= len {
                break;
            }
            let sql = format!(
                "SELECT id,relative_path,file_size,modified_ns FROM images WHERE volume_id=?1 AND {predicate} ORDER BY {column} {dir},id {dir} LIMIT ?4"
            );
            let mut statement = self.conn.prepare_cached(&sql)?;
            result.extend(
                statement
                    .query_map(
                        params![self.volume, value, anchor.id, (len - result.len()) as i64],
                        read_entry,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(result)
    }
    pub fn page(&self, start: usize, len: usize, sort: Sort, desc: bool) -> Result<Vec<Entry>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let count = self.count()?;
        if start >= count {
            return Ok(Vec::new());
        }
        let requested = len.min(count - start);
        let windows = self.windows.borrow();
        for window in windows
            .iter()
            .rev()
            .filter(|w| w.sort == sort && w.desc == desc)
        {
            if start >= window.start && start - window.start + requested <= window.entries.len() {
                return Ok(
                    window.entries[start - window.start..start - window.start + requested].to_vec(),
                );
            }
        }
        let anchor = windows
            .iter()
            .rev()
            .filter(|w| w.sort == sort && w.desc == desc)
            .find_map(|w| {
                if w.start + w.entries.len() == start {
                    w.entries.last().cloned().map(|e| (e, false))
                } else if start + requested == w.start {
                    w.entries.first().cloned().map(|e| (e, true))
                } else {
                    None
                }
            });
        drop(windows);
        let fetch = requested.max(32).min(count - start);
        let entries = if let Some((anchor, reverse)) = anchor {
            let mut entries = self.after(
                &anchor,
                if reverse { requested } else { fetch },
                sort,
                desc ^ reverse,
            )?;
            if reverse {
                entries.reverse();
            }
            entries
        } else {
            // Seek from the nearer end, so End never scans the entire index.
            let reverse = start > count - start - fetch;
            let direction = if desc ^ reverse { "DESC" } else { "ASC" };
            let offset = if reverse {
                count - start - fetch
            } else {
                start
            };
            let sql = format!(
                "SELECT id,relative_path,file_size,modified_ns FROM images WHERE volume_id=?1 ORDER BY {} {direction},id {direction} LIMIT ?2 OFFSET ?3",
                sort.column()
            );
            let mut statement = self.conn.prepare_cached(&sql)?;
            let mut entries = statement
                .query_map(
                    params![self.volume, fetch as i64, offset as i64],
                    read_entry,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if reverse {
                entries.reverse();
            }
            entries
        };
        // Only small navigation windows are retained; arbitrary callers stay bounded.
        if entries.len() <= 64 {
            let mut windows = self.windows.borrow_mut();
            if windows.len() == 4 {
                windows.remove(0);
            }
            windows.push(Window {
                start,
                sort,
                desc,
                entries: entries.clone(),
            });
        }
        Ok(entries.into_iter().take(requested).collect())
    }
    pub fn position(&self, id: i64, sort: Sort, desc: bool) -> Result<Option<usize>> {
        let op = if desc { ">" } else { "<" };
        let column = sort.column();
        let sql = format!(
            "SELECT (SELECT count(*) FROM images WHERE volume_id=?1 AND {column} {op} anchor.value) + (SELECT count(*) FROM images WHERE volume_id=?1 AND {column}=anchor.value AND id {op} ?2) FROM (SELECT {column} AS value FROM images WHERE volume_id=?1 AND id=?2) AS anchor"
        );
        // Separate strict-value and equal-value ranges let SQLite seek both
        // parts of the covering index, including NOCASE names and id ties.
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .prepare_cached(&sql)?
            .query_row(params![self.volume, id], |row| row.get::<_, i64>(0))
            .optional()?
            .map(|n| n as usize))
    }

    pub fn find_path(&self, path: &str) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM images WHERE volume_id=?1 AND relative_path=?2",
                params![self.volume, path],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn metadata(&self, id: i64, w: u32, h: u32, orientation: u8) -> Result<()> {
        self.conn.execute(
            "UPDATE images SET width=?1,height=?2,orientation=?3 WHERE id=?4 AND volume_id=?5 AND (width IS NOT ?1 OR height IS NOT ?2 OR orientation IS NOT ?3)",
            params![w, h, orientation, id, self.volume],
        )?;
        Ok(())
    }
}
fn read_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: r.get(0)?,
        relative: r.get(1)?,
        size: r.get::<_, i64>(2)? as u64,
        modified_ns: r.get(3)?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readonly_rank_tracks_commits_without_mutating_writer_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut writer = Index::open(&path).unwrap();
        writer.scope("first").unwrap();
        let entry = Entry {
            id: 0,
            relative: "z.png".into(),
            size: 20,
            modified_ns: 1,
        };
        writer.upsert(std::slice::from_ref(&entry)).unwrap();
        assert_eq!(writer.count().unwrap(), 1);
        writer.upsert(&[entry.clone(), entry.clone()]).unwrap();
        assert_eq!(writer.count().unwrap(), 1);
        let id = writer.find_path("z.png").unwrap().unwrap();
        let mut reader = Index::open_reader(&path).unwrap();
        reader.read_scope("first").unwrap();
        let revision = writer.revision();
        assert_eq!(reader.position(id, Sort::Name, false).unwrap(), Some(0));
        writer
            .upsert(&[Entry {
                relative: "a.png".into(),
                ..entry
            }])
            .unwrap();
        assert!(writer.revision() > revision);
        assert_eq!(writer.count().unwrap(), 2);
        assert_eq!(reader.position(id, Sort::Name, false).unwrap(), Some(1));
        writer.scope("second").unwrap();
        assert_eq!(writer.count().unwrap(), 0);
        assert_eq!(reader.position(id, Sort::Name, false).unwrap(), Some(1));
        reader.read_scope("second").unwrap();
        assert_eq!(reader.position(id, Sort::Name, false).unwrap(), None);
        assert!(reader.remember_selection("not-allowed.png").is_err());
    }
    #[test]
    fn selection_history_survives_reopen_and_is_scoped_to_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite3");
        let mut index = Index::open(&path).unwrap();
        index.scope("folder-a").unwrap();
        index.remember_selection("sub/last.jpg").unwrap();
        index.scope("folder-b").unwrap();
        assert!(index.last_selection().unwrap().is_none());
        index.remember_selection("different.png").unwrap();
        drop(index);
        let mut index = Index::open(&path).unwrap();
        index.scope("folder-a").unwrap();
        assert_eq!(
            index.last_selection().unwrap().as_deref(),
            Some("sub/last.jpg")
        );
        index.scope("folder-b").unwrap();
        assert_eq!(
            index.last_selection().unwrap().as_deref(),
            Some("different.png")
        );
    }
    #[test]
    fn cursor_windows_match_offset_with_ties_and_invalidate_on_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Index::open(&dir.path().join("cursor.sqlite3")).unwrap();
        db.scope("card").unwrap();
        let entries: Vec<_> = (0..180)
            .map(|n| Entry {
                id: 0,
                relative: format!(
                    "{}{:03}.jpg",
                    if n % 2 == 0 { "PHOTO" } else { "photo" },
                    n / 2
                ),
                size: n % 4,
                modified_ns: (n % 7) as i64,
            })
            .collect();
        db.upsert(&entries).unwrap();
        for sort in [Sort::Name, Sort::Modified, Sort::Size] {
            for desc in [false, true] {
                let expected = db.page(0, 1000, sort, desc).unwrap();
                for anchor in [0, 31, 70, 145, 178] {
                    let actual = db.after(&expected[anchor], 32, sort, desc).unwrap();
                    assert_eq!(
                        actual.iter().map(|e| e.id).collect::<Vec<_>>(),
                        expected[anchor + 1..(anchor + 33).min(180)]
                            .iter()
                            .map(|e| e.id)
                            .collect::<Vec<_>>()
                    );
                }
                for start in [0, 32, 64, 63, 62, 179, 178, 0, 80] {
                    assert_eq!(
                        db.page(start, 1, sort, desc).unwrap()[0].id,
                        expected[start].id
                    );
                }
            }
        }
        let old = db.page(0, 32, Sort::Name, false).unwrap();
        db.remove_paths(&[old[0].relative.clone()]).unwrap();
        assert_eq!(db.page(0, 1, Sort::Name, false).unwrap()[0].id, old[1].id);
        db.upsert(&[Entry {
            id: 0,
            relative: "000.jpg".into(),
            size: 1,
            modified_ns: 1,
        }])
        .unwrap();
        assert_eq!(
            db.page(0, 1, Sort::Name, false).unwrap()[0].relative,
            "000.jpg"
        );
        db.scope("other").unwrap();
        assert!(db.page(0, 32, Sort::Name, false).unwrap().is_empty());
    }
    #[test]
    fn restarted_scan_has_fresh_epoch_and_only_complete_scan_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Index::open(&dir.path().join("scan.sqlite3")).unwrap();
        db.scope("card").unwrap();
        let entries = vec![
            Entry {
                id: 0,
                relative: "a.jpg".into(),
                size: 1,
                modified_ns: 1,
            },
            Entry {
                id: 0,
                relative: "b.jpg".into(),
                size: 2,
                modified_ns: 2,
            },
        ];
        db.upsert(&entries).unwrap();
        db.finish().unwrap();
        db.begin_scan().unwrap();
        db.upsert(&entries[..1]).unwrap();
        assert_eq!(
            db.count().unwrap(),
            2,
            "incomplete scan must preserve prior index"
        );
        db.begin_scan().unwrap();
        db.upsert(&entries[1..]).unwrap();
        db.finish().unwrap();
        assert_eq!(db.count().unwrap(), 1);
        assert!(db.find_path("a.jpg").unwrap().is_none());
    }
    #[test]
    fn rank_matches_pages_with_ties_both_directions_and_old_scope_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Index::open(&dir.path().join("rank.sqlite3")).unwrap();
        db.scope("old").unwrap();
        let entries: Vec<_> = (0..90)
            .map(|i| Entry {
                id: 0,
                relative: format!(
                    "{}-{:03}.jpg",
                    if i % 2 == 0 { "PHOTO" } else { "photo" },
                    i / 2
                ),
                size: i % 5,
                modified_ns: (i % 7) as i64,
            })
            .collect();
        db.upsert(&entries).unwrap();
        for sort in [Sort::Name, Sort::Modified, Sort::Size] {
            for desc in [false, true] {
                for (rank, entry) in db.page(0, 100, sort, desc).unwrap().iter().enumerate() {
                    assert_eq!(db.position(entry.id, sort, desc).unwrap(), Some(rank));
                }
                assert_eq!(db.position(-1, sort, desc).unwrap(), None);
            }
        }
        let old_id = db.find_path(&entries[1].relative).unwrap().unwrap();
        db.scope("new").unwrap();
        db.upsert(&entries[..1]).unwrap();
        for sort in [Sort::Name, Sort::Modified, Sort::Size] {
            for desc in [false, true] {
                assert_eq!(db.position(old_id, sort, desc).unwrap(), None);
            }
        }
        db.remove_paths_in_scope("old", &[entries[0].relative.clone()])
            .unwrap();
        assert_eq!(db.count().unwrap(), 1);
        db.scope("old").unwrap();
        assert_eq!(db.count().unwrap(), 89);
    }
    #[test]
    fn hundred_thousand_sorted_paged_stable_ids() {
        let d = tempfile::tempdir().unwrap();
        let mut db = Index::open(&d.path().join("i.db")).unwrap();
        db.scope("card:photos").unwrap();
        for batch in 0..100 {
            let es = (0..1000)
                .map(|n| {
                    let n = batch * 1000 + n;
                    Entry {
                        id: 0,
                        relative: format!("{n:06}.jpg"),
                        size: n,
                        modified_ns: n as i64,
                    }
                })
                .collect::<Vec<_>>();
            db.upsert(&es).unwrap();
        }
        assert_eq!(db.count().unwrap(), 100000);
        let e = db.page(99999, 1, Sort::Size, false).unwrap().remove(0);
        assert_eq!(e.size, 99999);
        assert_eq!(db.position(e.id, Sort::Name, false).unwrap(), Some(99999));
        let mut e2 = e.clone();
        e2.size = 2;
        db.upsert(&[e2]).unwrap();
        assert_eq!(db.find_path(&e.relative).unwrap(), Some(e.id));
        db.scope("different-card").unwrap();
        assert_eq!(db.count().unwrap(), 0);
    }
}
