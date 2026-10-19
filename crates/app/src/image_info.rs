//! Metadata runs on a bounded worker; stale results are discarded by selection generation.
use std::{
    fs::File,
    io::{BufReader, Seek, SeekFrom},
    path::Path,
};

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub value: String,
}

impl Field {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

pub fn modified(ns: i64) -> String {
    chrono::DateTime::from_timestamp(
        ns.div_euclid(1_000_000_000),
        ns.rem_euclid(1_000_000_000) as u32,
    )
    .map(|date| {
        date.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    })
    .unwrap_or_else(|| "未知".into())
}

pub fn read(
    path: &Path,
    diagnostics: Option<&mygo_telemetry::Telemetry>,
) -> anyhow::Result<Vec<Field>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let mut fields = vec![
        Field::new(
            "檔名",
            path.file_name().unwrap_or_default().to_string_lossy(),
        ),
        Field::new(
            "大小",
            format!("{:.2} MiB", metadata.len() as f64 / mygo_core::MIB as f64),
        ),
    ];
    let mut reader = BufReader::with_capacity(65536, file);
    let png = crate::png_metadata::read_from(&mut reader, metadata.len())?;
    let dimensions = if let Some(png) = &png {
        png.dimensions
    } else {
        reader.seek(SeekFrom::Start(0))?;
        image::ImageReader::new(&mut reader)
            .with_guessed_format()?
            .into_dimensions()
            .ok()
    };
    if let Some((width, height)) = dimensions {
        fields.push(Field::new("尺寸", format!("{width} × {height}")));
    }
    if let Ok(date) = metadata.modified() {
        let date: chrono::DateTime<chrono::Local> = date.into();
        fields.push(Field::new(
            "修改日期",
            date.format("%Y-%m-%d %H:%M:%S").to_string(),
        ));
    }
    let mut warnings = Vec::new();
    let mut custom = Vec::new();
    let mut exif_reader = exif::Reader::new();
    exif_reader.continue_on_error(true);
    let exif_result = if let Some(png) = png {
        warnings.extend(png.warnings);
        custom = png.fields;
        png.exif.map_or_else(
            || Err(exif::Error::NotFound("PNG EXIF")),
            |bytes| exif_reader.read_raw(bytes),
        )
    } else {
        reader.seek(SeekFrom::Start(0))?;
        exif_reader.read_from_container(&mut reader)
    };
    match exif_result.or_else(|error| {
        error.distill_partial_result(|errors| {
            warnings.extend(errors.iter().map(ToString::to_string));
        })
    }) {
        Ok(exif) => {
            for field in exif.fields() {
                if field.ifd_num != exif::In::PRIMARY {
                    continue;
                }
                if let Some(name) = common_tag(field.tag) {
                    fields.push(Field::new(name, crate::exif_text::value(field, &exif)));
                }
            }
        }
        Err(exif::Error::NotFound(_)) => (),
        Err(error) => warnings.push(error.to_string()),
    }
    fields.extend(custom);
    // Detailed parser errors stay in diagnostics rather than crowding the panel.
    if !warnings.is_empty() {
        if let Some(log) = diagnostics {
            log.trace(
                "image-info-parse-warning",
                0,
                0,
                &warnings.join("; "),
                0,
                false,
            );
        }
        fields.push(Field::new("讀取提示", "部分圖片資訊無法讀取"));
    }
    Ok(fields)
}

fn common_tag(tag: exif::Tag) -> Option<&'static str> {
    use exif::Tag;
    Some(match tag {
        Tag::Make => "相機品牌",
        Tag::Model => "相機型號",
        Tag::DateTimeOriginal => "拍攝日期",
        Tag::LensModel => "鏡頭",
        Tag::ExposureTime => "快門",
        Tag::FNumber => "光圈",
        Tag::PhotographicSensitivity => "ISO",
        Tag::FocalLength => "焦距",
        Tag::ImageDescription => "描述",
        Tag::UserComment => "自訂註解",
        Tag::Artist => "作者",
        Tag::Copyright => "版權",
        Tag::Software => "製作軟體",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_png_combines_common_fields_and_original_custom_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.png");
        image::RgbImage::new(12, 8).save(&path).unwrap();
        let mut png = std::fs::read(&path).unwrap();
        let end = png.split_off(png.len() - 12);
        let data = b"workflow\0{\"nodes\":[],\"note\":\"a:b\"}";
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(b"tEXt");
        png.extend_from_slice(data);
        let mut hash = crc32fast::Hasher::new();
        hash.update(b"tEXt");
        hash.update(data);
        png.extend_from_slice(&hash.finalize().to_be_bytes());
        png.extend(end);
        std::fs::write(&path, png).unwrap();
        let fields = read(&path, None).unwrap();
        assert!(
            fields
                .iter()
                .any(|f| f.name == "尺寸" && f.value == "12 × 8")
        );
        assert!(
            fields
                .iter()
                .any(|f| f.name == "workflow" && f.value == "{\"nodes\":[],\"note\":\"a:b\"}")
        );
        assert!(!fields.iter().any(|f| f.name == "讀取提示"));
    }
    #[test]
    fn png_camera_exif_and_dimensions_survive_late_text_crc_failure() {
        use image::ImageEncoder;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera.png");
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder
            .set_exif_metadata(
                b"II*\0\x08\0\0\0\x01\0\x0f\x01\x02\0\x06\0\0\0\x1a\0\0\0\0\0\0\0Canon\0".to_vec(),
            )
            .unwrap();
        encoder
            .write_image(&[80; 12 * 8 * 3], 12, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let end = bytes.split_off(bytes.len() - 12);
        let text = b"prompt\0damaged";
        bytes.extend((text.len() as u32).to_be_bytes());
        bytes.extend(b"tEXt");
        bytes.extend(text);
        bytes.extend([0; 4]);
        bytes.extend(end);
        std::fs::write(&path, bytes).unwrap();
        let log = mygo_telemetry::Telemetry::new(dir.path()).unwrap();
        let fields = read(&path, Some(&log)).unwrap();
        assert!(
            fields
                .iter()
                .any(|f| f.name == "尺寸" && f.value == "12 × 8")
        );
        assert!(
            fields
                .iter()
                .any(|f| f.name == "相機品牌" && f.value.contains("Canon"))
        );
        assert!(fields.iter().any(|f| f.name == "讀取提示"));
        assert!(log.flush(std::time::Duration::from_secs(1)));
        let diagnostics = std::fs::read_to_string(dir.path().join("diagnostics.jsonl")).unwrap();
        assert!(diagnostics.contains("CRC"));
        assert!(diagnostics.contains("offset="));
        assert!(diagnostics.contains("tEXt"));
    }

    #[test]
    fn reads_camera_details_from_exif() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera.tif");
        std::fs::write(
            &path,
            b"II*\0\x08\0\0\0\x01\0\x0f\x01\x02\0\x06\0\0\0\x1a\0\0\0\0\0\0\0Canon\0",
        )
        .unwrap();
        let info = read(&path, None).unwrap();
        assert!(
            info.iter()
                .any(|f| f.name == "相機品牌" && f.value.contains("Canon")),
            "{info:?}"
        );
        assert_eq!(common_tag(exif::Tag::StripOffsets), None);
    }
    #[test]
    fn date_has_seconds_and_missing_exif_is_handled() {
        assert_eq!(modified(0).len(), 19);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"not an image").unwrap();
        let info = read(&path, None).unwrap();
        assert!(info.iter().any(|f| f.value == "photo.jpg"));
        assert!(info.iter().any(|f| f.name == "修改日期"));
        assert!(read(&dir.path().join("missing.jpg"), None).is_err());
    }
}
