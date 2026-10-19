mod formats;
mod language;
use anyhow::Context;
pub use formats::ImageFormats;
pub use language::Language;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const MIB: usize = 1024 * 1024;
#[derive(Default, Debug)]
struct CancelState {
    cancelled: AtomicBool,
    children: Vec<Cancel>,
}
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<CancelState>);
impl Cancel {
    /// Common source work ends only when every independent subscriber ends.
    pub fn when_all(children: Vec<Cancel>) -> Self {
        Self(Arc::new(CancelState {
            cancelled: AtomicBool::new(false),
            children,
        }))
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
    pub fn cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
            || (!self.0.children.is_empty() && self.0.children.iter().all(Cancel::cancelled))
    }
}
#[cfg(test)]
mod source_cancel_tests {
    use super::Cancel;
    #[test]
    fn shared_work_survives_one_cancel_and_stops_after_the_last() {
        let main = Cancel::default();
        let thumb = Cancel::default();
        let group = Cancel::when_all(vec![main.clone(), thumb.clone()]);
        main.cancel();
        assert!(!group.cancelled());
        thumb.cancel();
        assert!(group.cancelled());
        let explicit = Cancel::when_all(vec![Cancel::default()]);
        explicit.cancel();
        assert!(explicit.cancelled());
    }
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: i64,
    pub relative: String,
    pub size: u64,
    pub modified_ns: i64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Name,
    Modified,
    Size,
}
impl Sort {
    pub fn column(self) -> &'static str {
        match self {
            Self::Name => "relative_path COLLATE NOCASE",
            Self::Modified => "modified_ns",
            Self::Size => "file_size",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub language: Language,
    pub image_formats: ImageFormats,
    pub image_size_limit: usize,
    pub thumbnail_mib: usize,
    pub main_mib: usize,
    pub disk_mib: u64,
    pub max_pixels: u64,
    pub max_file_mib: u64,
    pub decode_mib: usize,
    pub max_dimension: u32,
    pub decode_timeout_ms: u64,
    pub workers: usize,
    pub wheel_zoom: bool,
    pub trash_companions: bool,
    // Session-only: always start in single-image mode, including with old settings.
    #[serde(skip)]
    pub multi_view: bool,
    pub images_per_view: usize,
    pub preload_enabled: bool,
    pub preload_count: usize,
    pub resume_last_image: bool,
    pub copy_folder: PathBuf,
    pub copy_shortcut: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            language: Language::default(),
            image_formats: ImageFormats::default(),
            image_size_limit: 0,
            thumbnail_mib: 128,
            main_mib: 384,
            disk_mib: 2048,
            max_pixels: 160_000_000,
            max_file_mib: 512,
            decode_mib: 128,
            max_dimension: 65535,
            decode_timeout_ms: 15000,
            workers: 2,
            wheel_zoom: true,
            trash_companions: false,
            multi_view: false,
            images_per_view: 6,
            preload_enabled: true,
            preload_count: 2,
            resume_last_image: false,
            copy_folder: PathBuf::new(),
            copy_shortcut: String::new(),
        }
    }
}
impl Config {
    /// None means no application-imposed size or working-memory limit.
    pub fn image_limits(&self) -> Option<(u64, u32, u64, u64)> {
        let (pixels, file_mib) = match self.image_size_limit {
            1 => (24_000_000, 512),
            2 => (50_000_000, 1024),
            3 => (100_000_000, 2048),
            4 => return None,
            _ => {
                return Some((
                    self.max_pixels,
                    self.max_dimension,
                    self.max_file_mib * MIB as u64,
                    (self.decode_mib * MIB) as u64,
                ));
            }
        };
        Some((pixels, 65535, file_mib * MIB as u64, pixels * 16))
    }

    pub fn load(base: &Path) -> anyhow::Result<Self> {
        Self::load_with_recovery(base).map(|(config, _)| config)
    }

    /// Returns the preserved corrupt file, if defaults were restored.
    pub fn load_with_recovery(base: &Path) -> anyhow::Result<(Self, Option<PathBuf>)> {
        let _lock = settings_lock(base)?;
        let path = base.join("settings.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                config.save_locked(base)?;
                return Ok((config, None));
            }
            Err(e) => return Err(e).context("無法讀取設定檔"),
        };
        let mut c: Self = match serde_json::from_slice(&bytes) {
            Ok(config) => config,
            Err(_) => {
                // Preserve the exact bytes before replacing the damaged file.
                let mut backup = tempfile::Builder::new()
                    .prefix("settings.corrupt-")
                    .suffix(".json")
                    .tempfile_in(base)
                    .context("無法備份損壞的設定檔，已保留原檔")?;
                backup.write_all(&bytes)?;
                backup.as_file().sync_all()?;
                let (_, backup_path) = backup.keep()?;
                let config = Self::default();
                config.save_locked(base)?;
                return Ok((config, Some(backup_path)));
            }
        };
        c.normalize();
        Ok((c, None))
    }

    fn normalize(&mut self) {
        let c = self;
        c.workers = c.workers.clamp(1, 4);
        c.image_size_limit = c.image_size_limit.min(4);
        c.disk_mib = c.disk_mib.min(20 * 1024);
        c.decode_mib = c.decode_mib.clamp(16, 128);
        c.main_mib = c.main_mib.clamp(16, 384);
        c.thumbnail_mib = c.thumbnail_mib.clamp(1, 128);
        c.max_dimension = c.max_dimension.clamp(1, 65535);
        c.max_pixels = c.max_pixels.clamp(1, 1_000_000_000);
        c.max_file_mib = c.max_file_mib.clamp(1, 4096);
        c.decode_timeout_ms = c.decode_timeout_ms.clamp(100, 60000);
        c.images_per_view = c.images_per_view.clamp(1, 24);
        c.preload_count = c.preload_count.clamp(1, 24);
    }
    pub fn save(&self, base: &Path) -> anyhow::Result<()> {
        let _lock = settings_lock(base)?;
        self.save_locked(base)
    }

    fn save_locked(&self, base: &Path) -> anyhow::Result<()> {
        let mut config = self.clone();
        config.normalize();
        let mut pending = tempfile::Builder::new()
            .prefix("settings.pending-")
            .tempfile_in(base)?;
        pending.write_all(&serde_json::to_vec_pretty(&config)?)?;
        pending.as_file().sync_all()?;
        // Same-directory atomic replacement; failure leaves the old settings intact.
        pending
            .persist(base.join("settings.json"))
            .map_err(|error| error.error)
            .context("無法替換設定檔，已保留原設定")?;
        Ok(())
    }
}

fn settings_lock(base: &Path) -> anyhow::Result<File> {
    std::fs::create_dir_all(base)?;
    // Keep the lock file: unlinking it could let two instances lock different files.
    // The OS releases the lock on close or process exit, including a crash.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(base.join("settings.lock"))?;
    lock.try_lock()
        .context("設定檔正由另一個實例使用或無法鎖定，請稍後重試")?;
    Ok(lock)
}
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("MyGoImage")
}
pub fn supported(path: &Path) -> bool {
    ImageFormats::default().allows(path)
}
/// Volume serial + volume-relative scope: independent of Windows drive letter.
pub fn volume_scope(root: &Path) -> anyhow::Result<String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut mount = vec![0u16; 32768];
        let mut serial = 0u32;
        // SAFETY: nul-terminated input, writable buffers sized by their supplied lengths.
        unsafe {
            if GetVolumePathNameW(wide.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if GetVolumeInformationW(
                mount.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let mount =
            String::from_utf16_lossy(&mount[..mount.iter().position(|x| *x == 0).unwrap_or(0)]);
        let s = root.to_string_lossy();
        let relative = s.strip_prefix(&mount).unwrap_or(&s);
        Ok(format!("{serial:08x}:{}", relative.to_lowercase()))
    }
    #[cfg(not(windows))]
    {
        Ok(root.to_string_lossy().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browsing_preferences_round_trip_and_old_settings_keep_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            preload_enabled: false,
            preload_count: usize::MAX,
            resume_last_image: true,
            copy_folder: PathBuf::from("D:/照片編輯"),
            ..Config::default()
        };
        config.save(dir.path()).unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert!(!loaded.preload_enabled);
        assert_eq!(loaded.preload_count, 24);
        assert!(loaded.resume_last_image);
        assert_eq!(loaded.copy_folder, config.copy_folder);
        let old: Config = serde_json::from_str("{}").unwrap();
        assert!(old.preload_enabled);
        assert_eq!(old.preload_count, 2);
        assert!(!old.resume_last_image);
        assert!(old.copy_folder.as_os_str().is_empty());
    }
    #[test]
    fn corrupt_settings_are_backed_up_before_defaults_are_saved() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [b"".as_slice(), b"{\"workers\":", b"{\"workers\":null}"] {
            std::fs::write(dir.path().join("settings.json"), bytes).unwrap();
            let (config, backup) = Config::load_with_recovery(dir.path()).unwrap();
            assert_eq!(config.workers, Config::default().workers);
            assert_eq!(std::fs::read(backup.unwrap()).unwrap(), bytes);
            let (_, backup) = Config::load_with_recovery(dir.path()).unwrap();
            assert!(backup.is_none());
        }
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .filter(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.corrupt-"))
                .count(),
            3
        );
    }

    #[test]
    fn settings_creation_compatibility_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("data");
        assert_eq!(Config::load(&base).unwrap().workers, 2);
        std::fs::write(base.join("settings.json"), br#"{"wheel_zoom":false}"#).unwrap();
        let config = Config::load(&base).unwrap();
        assert!(!config.wheel_zoom);
        assert_eq!(config.images_per_view, 6);
        let config = Config {
            workers: usize::MAX,
            thumbnail_mib: 0,
            main_mib: usize::MAX,
            disk_mib: u64::MAX,
            decode_mib: 0,
            max_pixels: u64::MAX,
            max_file_mib: u64::MAX,
            max_dimension: 0,
            decode_timeout_ms: u64::MAX,
            image_size_limit: usize::MAX,
            images_per_view: 0,
            ..Config::default()
        };
        // Loading and saving both normalize out-of-range, valid JSON numbers.
        std::fs::write(
            base.join("settings.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let loaded = Config::load(&base).unwrap();
        config.save(&base).unwrap();
        let saved: Config =
            serde_json::from_slice(&std::fs::read(base.join("settings.json")).unwrap()).unwrap();
        for c in [loaded, saved] {
            assert_eq!(
                (c.workers, c.images_per_view, c.image_size_limit),
                (4, 1, 4)
            );
            assert_eq!((c.thumbnail_mib, c.main_mib, c.decode_mib), (1, 384, 16));
            assert_eq!(
                (c.disk_mib, c.max_pixels, c.max_file_mib),
                (20480, 1_000_000_000, 4096)
            );
            assert_eq!((c.max_dimension, c.decode_timeout_ms), (1, 60000));
        }
    }

    #[test]
    fn settings_contention_is_reported_without_modifying_files() {
        const CHILD_BASE: &str = "MYGO_SETTINGS_LOCK_TEST_BASE";
        if let Some(base) = std::env::var_os(CHILD_BASE) {
            let base = Path::new(&base);
            assert!(Config::load_with_recovery(base).is_err());
            assert!(Config::default().save(base).is_err());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"broken").unwrap();
        let lock = settings_lock(dir.path()).unwrap();
        assert!(Config::load_with_recovery(dir.path()).is_err());
        assert!(Config::default().save(dir.path()).is_err());
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::settings_contention_is_reported_without_modifying_files",
            ])
            .env(CHILD_BASE, dir.path())
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"broken");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        drop(lock);
        assert!(Config::load_with_recovery(dir.path()).unwrap().1.is_some());
    }

    #[test]
    fn concurrent_settings_writes_remain_complete_and_clean_up_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        Config::default().save(dir.path()).unwrap();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=4)
                .map(|workers| {
                    let base = dir.path();
                    scope.spawn(move || {
                        let config = Config {
                            workers,
                            images_per_view: workers * 2,
                            ..Config::default()
                        };
                        (0..20).filter(|_| config.save(base).is_ok()).count()
                    })
                })
                .collect();
            assert!(
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .sum::<usize>()
                    > 0
            );
        });
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.images_per_view, config.workers * 2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn failed_settings_replacement_preserves_previous_valid_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        Config::default().save(dir.path()).unwrap();
        let path = dir.path().join("settings.json");
        let before = std::fs::read(&path).unwrap();
        // Simulate a destination which Windows will not allow us to replace.
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let result = Config {
            workers: 4,
            ..Config::default()
        }
        .save(dir.path());
        drop(handle);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(Config::load(dir.path()).unwrap().workers, 2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);

        // Failed recovery must preserve both the damaged original and its backup.
        std::fs::write(&path, b"broken").unwrap();
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let result = Config::load_with_recovery(dir.path());
        drop(handle);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken");
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), b"broken");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[test]
    fn cancellation_is_shared() {
        let a = Cancel::default();
        let b = a.clone();
        a.cancel();
        assert!(b.cancelled());
    }
    #[test]
    fn extension_filter() {
        assert!(supported(Path::new("相片.JpG")));
        assert!(!supported(Path::new("x.raw")));
    }
    #[test]
    fn older_settings_keep_new_options_safe_by_default() {
        let config: Config = serde_json::from_str(r#"{"wheel_zoom":false,"workers":2}"#).unwrap();
        assert!(!config.trash_companions);
        assert!(!config.multi_view);
        assert_eq!(config.images_per_view, 6);
        assert!(!config.wheel_zoom);
        assert_eq!(config.image_size_limit, 0);
    }
    #[test]
    fn image_size_presets_round_trip_and_have_sufficient_decode_budget() {
        for preset in 0..=4 {
            let config = Config {
                image_size_limit: preset,
                ..Config::default()
            };
            let restored: Config =
                serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
            assert_eq!(restored.image_size_limit, preset);
            if preset == 4 {
                assert!(restored.image_limits().is_none());
            } else if preset > 0 {
                let (pixels, _, _, budget) = restored.image_limits().unwrap();
                assert_eq!(pixels * 16, budget);
            }
        }
    }
}
