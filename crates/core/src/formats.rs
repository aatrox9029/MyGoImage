use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageFormats {
    pub jpeg: bool,
    pub png: bool,
    pub webp: bool,
    pub bmp: bool,
    pub tiff: bool,
    pub gif: bool,
    pub psd: bool,
}
impl Default for ImageFormats {
    fn default() -> Self {
        Self {
            jpeg: true,
            png: true,
            webp: true,
            bmp: true,
            tiff: true,
            gif: true,
            psd: true,
        }
    }
}
impl ImageFormats {
    pub fn extensions(&self) -> Vec<&'static str> {
        [
            (self.jpeg, &["jpg", "jpeg"][..]),
            (self.png, &["png"][..]),
            (self.webp, &["webp"][..]),
            (self.bmp, &["bmp"][..]),
            (self.tiff, &["tif", "tiff"][..]),
            (self.gif, &["gif"][..]),
            (self.psd, &["psd"][..]),
        ]
        .into_iter()
        .filter(|(enabled, _)| *enabled)
        .flat_map(|(_, exts)| exts.iter().copied())
        .collect()
    }
    pub fn allows(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| {
                [
                    (self.jpeg, "jpg"),
                    (self.jpeg, "jpeg"),
                    (self.png, "png"),
                    (self.webp, "webp"),
                    (self.bmp, "bmp"),
                    (self.tiff, "tif"),
                    (self.tiff, "tiff"),
                    (self.gif, "gif"),
                    (self.psd, "psd"),
                ]
                .iter()
                .any(|(enabled, candidate)| *enabled && ext.eq_ignore_ascii_case(candidate))
            })
    }
    pub fn scope_key(&self) -> String {
        self.extensions().join(",")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    #[test]
    fn defaults_aliases_empty_and_round_trip() {
        let mut config: Config = serde_json::from_str("{}").unwrap();
        for ext in [
            "JpG", "JPEG", "PNG", "WEBP", "BMP", "TIF", "TIFF", "GIF", "PSD",
        ] {
            assert!(
                config
                    .image_formats
                    .allows(Path::new(&format!("image.{ext}")))
            );
        }
        config.image_formats.jpeg = false;
        assert!(!config.image_formats.allows(Path::new("image.JPEG")));
        assert!(config.image_formats.allows(Path::new("image.psd")));
        let dir = tempfile::tempdir().unwrap();
        config.save(dir.path()).unwrap();
        assert_eq!(
            Config::load(dir.path()).unwrap().image_formats,
            config.image_formats
        );
        let empty: ImageFormats = serde_json::from_str(r#"{"jpeg":false,"png":false,"webp":false,"bmp":false,"tiff":false,"gif":false,"psd":false}"#).unwrap();
        assert!(empty.extensions().is_empty());
        assert!(!empty.allows(Path::new("image.psd")));
    }
}
