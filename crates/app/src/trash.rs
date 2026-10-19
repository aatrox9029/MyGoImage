use anyhow::{Context, Result};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub fn directory() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let parent = exe.parent().context("無法取得程式目錄")?;
    // Development binaries share the project's trash; portable builds use the executable folder.
    let project = parent
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "target"))
        .and_then(Path::parent);
    Ok(project.unwrap_or(parent).join("trash"))
}

/// Store files directly in trash. Reserve names without overwriting and verify the entire
/// group before removing any source. Companions share a suffix when a name is already used.
#[derive(Debug)]
pub struct Report {
    pub removed: Vec<PathBuf>,
    pub error: Option<String>,
}
#[cfg(test)]
pub fn move_to(source: &Path, companions: bool, trash: &Path) -> Result<Vec<PathBuf>> {
    let report = move_report(source, companions, trash, &mygo_core::Cancel::default());
    if let Some(error) = report.error {
        anyhow::bail!(error);
    }
    Ok(report.removed)
}
#[cfg(test)]
pub fn move_report(
    source: &Path,
    companions: bool,
    trash: &Path,
    cancel: &mygo_core::Cancel,
) -> Report {
    move_report_progress(source, companions, trash, cancel, &AtomicU64::new(0))
}
pub fn move_report_progress(
    source: &Path,
    companions: bool,
    trash: &Path,
    cancel: &mygo_core::Cancel,
    progress: &AtomicU64,
) -> Report {
    let mut removed = Vec::new();
    let result = move_inner(source, companions, trash, cancel, &mut removed, progress);
    Report {
        removed,
        error: result.err().map(|error| format!("{error:#}")),
    }
}
fn check_cancel(cancel: &mygo_core::Cancel) -> Result<()> {
    anyhow::ensure!(
        !cancel.cancelled(),
        "檔案作業已取消；已完成驗證的副本會保留"
    );
    Ok(())
}
/// Copy the selected file and same-stem regular files; never remove a source.
/// Reserve the whole group with a common suffix and clean up all new files if
/// any copy, verification, or cancellation fails.
pub fn copy_group_progress(
    source: &Path,
    destination: &Path,
    cancel: &mygo_core::Cancel,
    progress: &AtomicU64,
) -> Result<Vec<PathBuf>> {
    check_cancel(cancel)?;
    anyhow::ensure!(
        fs::symlink_metadata(source)?.file_type().is_file(),
        "來源不是一般檔案"
    );
    let source = source.canonicalize()?;
    let parent = source.parent().context("來源沒有資料夾")?;
    fs::create_dir_all(destination).context("無法建立複製目的地")?;
    let destination = destination.canonicalize()?;
    anyhow::ensure!(parent != destination, "目的地不可與來源資料夾相同");
    let mut sources = vec![source.clone()];
    for entry in fs::read_dir(parent)? {
        check_cancel(cancel)?;
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path != source
            && path
                .file_stem()
                .zip(source.file_stem())
                .is_some_and(|(a, b)| {
                    a.to_string_lossy()
                        .eq_ignore_ascii_case(&b.to_string_lossy())
                })
        {
            sources.push(path);
        }
    }
    let mut destinations = reserve(&sources, &destination)?;
    let result = (|| -> Result<()> {
        for (source, (path, output)) in sources.iter().zip(&mut destinations) {
            let mut input = fs::File::open(source)?;
            copy_cancellable(&mut input, output, cancel, progress)?;
            output.sync_all()?;
            verify(source, path, cancel)?;
        }
        check_cancel(cancel)?;
        Ok(())
    })();
    if let Err(error) = result {
        cleanup(destinations);
        return Err(error).context("複製未完成，來源檔案仍保留");
    }
    Ok(destinations.into_iter().map(|(path, _)| path).collect())
}
fn copy_cancellable(
    input: &mut fs::File,
    output: &mut fs::File,
    cancel: &mygo_core::Cancel,
    progress: &AtomicU64,
) -> std::io::Result<u64> {
    use std::io::Write;
    let mut buffer = [0u8; 65536];
    let mut total = 0;
    loop {
        if cancel.cancelled() {
            return Err(std::io::Error::other("檔案作業已取消"));
        }
        let bytes = input.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(total);
        }
        output.write_all(&buffer[..bytes])?;
        total += bytes as u64;
        progress.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}
fn move_inner(
    source: &Path,
    companions: bool,
    trash: &Path,
    cancel: &mygo_core::Cancel,
    removed: &mut Vec<PathBuf>,
    progress: &AtomicU64,
) -> Result<()> {
    anyhow::ensure!(
        fs::symlink_metadata(source)?.file_type().is_file(),
        "來源不是一般檔案"
    );
    let source = source.canonicalize()?;
    fs::create_dir_all(trash).context("無法建立垃圾桶；來源檔案未移除")?;
    let trash = trash.canonicalize()?;
    anyhow::ensure!(!source.starts_with(&trash), "垃圾桶內的檔案不可再次刪除");
    let mut sources = vec![source.clone()];
    if companions {
        for entry in fs::read_dir(source.parent().context("來源沒有資料夾")?)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_file()
                && path != source
                && path
                    .file_stem()
                    .zip(source.file_stem())
                    .is_some_and(|(a, b)| {
                        a.to_string_lossy()
                            .eq_ignore_ascii_case(&b.to_string_lossy())
                    })
            {
                sources.push(path);
            }
        }
    }
    let mut destinations = reserve(&sources, &trash)?;
    // Keep every source intact if any copy or verification fails (including full destination disks).
    let copied = (|| -> Result<()> {
        for (source, (destination, output)) in sources.iter().zip(&mut destinations) {
            let mut input = fs::File::open(source)?;
            copy_cancellable(&mut input, output, cancel, progress)
                .context("複製到垃圾桶失敗，來源仍保留")?;
            output.sync_all()?;
            check_cancel(cancel)?;
            verify(source, destination, cancel)?;
        }
        Ok(())
    })();
    if let Err(error) = copied {
        cleanup(destinations);
        return Err(error);
    }
    for (path, (destination, _)) in sources.iter().zip(&destinations) {
        check_cancel(cancel)?;
        verify(path, destination, cancel)?;
        check_cancel(cancel)?;
        fs::remove_file(path)
            .with_context(|| format!("垃圾桶副本已保留，但無法移除來源 {}", path.display()))?;
        removed.push(path.clone());
    }
    Ok(())
}

fn reserve(sources: &[PathBuf], trash: &Path) -> Result<Vec<(PathBuf, fs::File)>> {
    'candidates: for counter in 0u64.. {
        let mut reserved = Vec::new();
        for source in sources {
            let name = if counter == 0 {
                source.file_name().context("無效檔名")?.to_os_string()
            } else {
                let mut name = source.file_stem().context("無效檔名")?.to_os_string();
                name.push(format!(" ({counter})"));
                if let Some(extension) = source.extension() {
                    name.push(".");
                    name.push(extension);
                }
                name
            };
            let destination = trash.join(name);
            match fs::File::create_new(&destination) {
                Ok(file) => reserved.push((destination, file)),
                Err(error) => {
                    cleanup(reserved);
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                    continue 'candidates;
                }
            }
        }
        return Ok(reserved);
    }
    unreachable!()
}

fn cleanup(files: Vec<(PathBuf, fs::File)>) {
    for (path, file) in files {
        drop(file);
        let _ = fs::remove_file(path);
    }
}

fn verify(source: &Path, copy: &Path, cancel: &mygo_core::Cancel) -> Result<()> {
    let mut a = fs::File::open(source)?;
    let mut b = fs::File::open(copy)?;
    anyhow::ensure!(
        a.metadata()?.len() == b.metadata()?.len(),
        "副本大小不同，來源仍保留"
    );
    let mut left = [0u8; 65536];
    let mut right = [0u8; 65536];
    loop {
        check_cancel(cancel)?;
        let n = a.read(&mut left)?;
        if n == 0 {
            break;
        }
        b.read_exact(&mut right[..n])?;
        anyhow::ensure!(left[..n] == right[..n], "副本內容不同，來源仍保留");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_group_preserves_sources_and_uses_a_common_collision_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photos");
        let destination = dir.path().join("edit");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        for (name, bytes) in [
            ("photo.JPG", b"jpeg".as_slice()),
            ("photo.raw", b"raw"),
            ("PHOTO.xmp", b"metadata"),
            ("other.jpg", b"other"),
        ] {
            fs::write(source.join(name), bytes).unwrap();
        }
        fs::create_dir(source.join("photo.folder")).unwrap();
        fs::write(destination.join("photo.raw"), b"existing").unwrap();
        let paths = copy_group_progress(
            &source.join("photo.JPG"),
            &destination,
            &mygo_core::Cancel::default(),
            &AtomicU64::new(0),
        )
        .unwrap();
        assert_eq!(paths.len(), 3);
        for (original, copied) in [
            ("photo.JPG", "photo (1).JPG"),
            ("photo.raw", "photo (1).raw"),
            ("PHOTO.xmp", "PHOTO (1).xmp"),
        ] {
            assert_eq!(
                fs::read(source.join(original)).unwrap(),
                fs::read(destination.join(copied)).unwrap()
            );
        }
        assert_eq!(
            fs::read(destination.join("photo.raw")).unwrap(),
            b"existing"
        );
        assert!(!destination.join("other.jpg").exists());
        assert!(!destination.join("photo.folder").exists());
        assert!(
            copy_group_progress(
                &source.join("photo.JPG"),
                &source,
                &mygo_core::Cancel::default(),
                &AtomicU64::new(0)
            )
            .is_err()
        );
    }
    #[test]
    fn copy_cancellation_preserves_source_and_does_not_publish_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        fs::write(&source, b"original").unwrap();
        let destination = dir.path().join("destination");
        let cancel = mygo_core::Cancel::default();
        cancel.cancel();
        assert!(copy_group_progress(&source, &destination, &cancel, &AtomicU64::new(0)).is_err());
        assert_eq!(fs::read(source).unwrap(), b"original");
        assert!(!destination.exists());
    }
    #[cfg(windows)]
    #[test]
    fn copy_group_read_failure_cleans_every_reserved_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        let raw = dir.path().join("photo.raw");
        fs::write(&source, b"jpeg").unwrap();
        fs::write(&raw, b"raw").unwrap();
        let _lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&raw)
            .unwrap();
        let destination = dir.path().join("out");
        assert!(
            copy_group_progress(
                &source,
                &destination,
                &mygo_core::Cancel::default(),
                &AtomicU64::new(0)
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
        assert_eq!(fs::read(source).unwrap(), b"jpeg");
        assert!(raw.exists());
    }
    #[test]
    fn cancelled_copy_preserves_sources_and_cleans_reserved_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        fs::write(&source, b"original").unwrap();
        let cancel = mygo_core::Cancel::default();
        cancel.cancel();
        let report = move_report(&source, false, &dir.path().join("trash"), &cancel);
        assert!(report.error.is_some());
        assert!(report.removed.is_empty());
        assert_eq!(fs::read(&source).unwrap(), b"original");
        assert_eq!(fs::read_dir(dir.path().join("trash")).unwrap().count(), 0);
    }
    #[cfg(windows)]
    #[test]
    fn partial_group_failure_reports_only_removed_sources_with_verified_copies() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        fs::create_dir(&photos).unwrap();
        let jpg = photos.join("photo.jpg");
        let raw = photos.join("photo.raw");
        fs::write(&jpg, b"jpeg bytes").unwrap();
        fs::write(&raw, b"raw bytes").unwrap();
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&raw)
            .unwrap();
        let report = move_report(
            &jpg,
            true,
            &dir.path().join("trash"),
            &mygo_core::Cancel::default(),
        );
        drop(handle);
        assert!(report.error.is_some());
        assert_eq!(report.removed.len(), 1);
        assert!(!jpg.exists());
        assert!(raw.exists());
        assert_eq!(
            fs::read(dir.path().join("trash/photo.jpg")).unwrap(),
            b"jpeg bytes"
        );
        assert_eq!(
            fs::read(dir.path().join("trash/photo.raw")).unwrap(),
            b"raw bytes"
        );
    }
    #[test]
    fn flat_trash_companions_collisions_and_trash_guard() {
        let d = tempfile::tempdir().unwrap();
        let photos = d.path().join("photos");
        let trash = d.path().join("trash");
        fs::create_dir(&photos).unwrap();
        for name in ["IMG.JPG", "IMG.CR3", "IMG.png", "OTHER.NEF"] {
            fs::write(photos.join(name), name.as_bytes()).unwrap();
        }
        assert_eq!(
            move_to(&photos.join("IMG.JPG"), false, &trash)
                .unwrap()
                .len(),
            1
        );
        assert!(photos.join("IMG.CR3").exists());
        fs::write(photos.join("IMG.JPG"), b"second").unwrap();
        assert_eq!(
            move_to(&photos.join("IMG.JPG"), true, &trash)
                .unwrap()
                .len(),
            3
        );
        assert!(photos.join("OTHER.NEF").exists());
        let files: Vec<_> = fs::read_dir(&trash)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 4);
        assert_eq!(fs::read(trash.join("IMG.JPG")).unwrap(), b"IMG.JPG");
        assert_eq!(fs::read(trash.join("IMG (1).JPG")).unwrap(), b"second");
        assert_eq!(fs::read(trash.join("IMG (1).CR3")).unwrap(), b"IMG.CR3");
        assert!(trash.join("IMG (1).png").is_file());
        for file in files {
            assert!(file.is_file());
            assert!(move_to(&file, false, &trash).is_err());
            assert!(file.exists());
        }
    }
    #[test]
    fn failed_destination_preserves_source() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("photo.jpg");
        fs::write(&source, b"original").unwrap();
        let blocked = d.path().join("blocked");
        fs::write(&blocked, b"file").unwrap();
        assert!(move_to(&source, false, &blocked).is_err());
        assert_eq!(fs::read(source).unwrap(), b"original");
    }
}
