use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicU64,
};

pub enum Outcome {
    Copied(Vec<PathBuf>),
    AlreadyPresent,
}

pub fn copy(
    source: &Path,
    destination: &Path,
    cancel: &mygo_core::Cancel,
    progress: &AtomicU64,
) -> Result<Outcome> {
    let name = source
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("無效檔名"))?;
    // Run on the existing file worker: even checking removable/network storage can block.
    if destination.try_exists()? {
        for entry in std::fs::read_dir(destination)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(&name.to_string_lossy())
            {
                return Ok(Outcome::AlreadyPresent);
            }
        }
    }
    crate::trash::copy_group_progress(source, destination, cancel, progress).map(Outcome::Copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_image_is_skipped_without_touching_either_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        let destination = dir.path().join("out");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(destination.join("PHOTO.JPG"), b"existing").unwrap();
        assert!(matches!(
            copy(
                &source,
                &destination,
                &mygo_core::Cancel::default(),
                &AtomicU64::new(0)
            )
            .unwrap(),
            Outcome::AlreadyPresent
        ));
        assert_eq!(std::fs::read(source).unwrap(), b"new");
        assert_eq!(
            std::fs::read(destination.join("PHOTO.JPG")).unwrap(),
            b"existing"
        );
        assert_eq!(std::fs::read_dir(destination).unwrap().count(), 1);
    }
}
