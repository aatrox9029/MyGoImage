//! Read text chunks without decoding pixels, including chunks after IDAT.
use crate::image_info::Field;
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const LIMIT: usize = 8 * 1024 * 1024;

pub fn read(path: &Path) -> Result<Vec<Field>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut signature = [0; 8];
    if file.read_exact(&mut signature).is_err() || signature != *b"\x89PNG\r\n\x1a\n" {
        return Ok(Vec::new());
    }
    let mut fields = Vec::new();
    let mut used = 0;
    for _ in 0..100_000 {
        let mut header = [0; 8];
        file.read_exact(&mut header).context("截斷的 PNG 標頭")?;
        let length = u32::from_be_bytes(header[..4].try_into()?) as usize;
        ensure!(
            file.stream_position()? + length as u64 + 4 <= size,
            "截斷的 PNG 區塊"
        );
        let kind = &header[4..];
        if matches!(kind, b"tEXt" | b"zTXt" | b"iTXt") {
            ensure!(length <= LIMIT - used, "自訂資訊超過 8 MiB 限制");
            let mut data = vec![0; length];
            file.read_exact(&mut data)?;
            let mut crc = [0; 4];
            file.read_exact(&mut crc)?;
            let mut hash = crc32fast::Hasher::new();
            hash.update(kind);
            hash.update(&data);
            ensure!(
                hash.finalize() == u32::from_be_bytes(crc),
                "自訂資訊 CRC 錯誤"
            );
            let field = parse(kind, &data, LIMIT - used)?;
            used += length.max(field.name.len() + field.value.len());
            ensure!(used <= LIMIT, "自訂資訊超過 8 MiB 限制");
            if !field.value.trim().is_empty() {
                fields.push(field);
            }
        } else {
            file.seek(SeekFrom::Current(length as i64 + 4))?;
        }
        if kind == b"IEND" {
            return Ok(fields);
        }
    }
    anyhow::bail!("PNG 區塊數量超過限制")
}

fn split(data: &[u8]) -> Result<(&[u8], &[u8])> {
    let pos = data
        .iter()
        .position(|&b| b == 0)
        .context("缺少文字分隔符號")?;
    Ok((&data[..pos], &data[pos + 1..]))
}

fn latin1(data: &[u8]) -> String {
    data.iter().map(|&b| char::from(b)).collect()
}

fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(limit as u64 + 1)
        .read_to_end(&mut output)?;
    ensure!(output.len() <= limit, "解壓縮資訊超過限制");
    Ok(output)
}

fn parse(kind: &[u8], data: &[u8], limit: usize) -> Result<Field> {
    let (key, rest) = split(data)?;
    ensure!(!key.is_empty() && key.len() <= 79, "無效的自訂欄位名稱");
    let value = match kind {
        b"tEXt" => latin1(rest),
        b"zTXt" => {
            ensure!(rest.first() == Some(&0), "無效的壓縮方式");
            latin1(&inflate(&rest[1..], limit)?)
        }
        b"iTXt" => {
            ensure!(
                rest.len() >= 2 && rest[0] <= 1 && rest[1] == 0,
                "無效的 UTF-8 文字標頭"
            );
            let (_, rest_text) = split(&rest[2..])?;
            let (_, text) = split(rest_text)?;
            let bytes = if rest[0] == 1 {
                inflate(text, limit)?
            } else {
                text.to_vec()
            };
            String::from_utf8(bytes).context("自訂資訊不是有效 UTF-8")?
        }
        _ => unreachable!(),
    };
    Ok(Field::new(latin1(key), value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn compressed(text: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text).unwrap();
        encoder.finish().unwrap()
    }

    fn chunk(png: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        png.extend_from_slice(&crc.finalize().to_be_bytes());
    }

    #[test]
    fn reads_comfyui_and_all_text_encodings_including_after_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("comfy.png");
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"tEXt", b"prompt\0{\"text\":\"a: b\\nc\"}");
        chunk(&mut png, b"IDAT", b"pixels skipped without decoding");
        let mut ztext = b"workflow\0\0".to_vec();
        ztext.extend(compressed(b"{\"nodes\":[]}"));
        chunk(&mut png, b"zTXt", &ztext);
        for flag in [0, 1] {
            let mut itext = b"custom\0".to_vec();
            itext.extend([flag, 0, 0, 0]);
            let value = "自訂內容\n第二行".as_bytes();
            itext.extend(if flag == 1 {
                compressed(value)
            } else {
                value.to_vec()
            });
            chunk(&mut png, b"iTXt", &itext);
        }
        chunk(&mut png, b"IEND", b"");
        std::fs::write(&path, &png).unwrap();
        let fields = read(&path).unwrap();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0].name, "prompt");
        assert_eq!(fields[0].value, "{\"text\":\"a: b\\nc\"}");
        assert_eq!(fields[1].value, "{\"nodes\":[]}");
        assert_eq!(fields[2].value, "自訂內容\n第二行");
        assert_eq!(fields[3].value, fields[2].value);
        png[20] ^= 1;
        std::fs::write(&path, png).unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn rejects_truncation_bad_headers_and_decompression_bombs() {
        assert!(parse(b"iTXt", b"key\0", LIMIT).is_err());
        assert!(parse(b"zTXt", b"key\0\x01bad", LIMIT).is_err());
        assert!(inflate(&compressed(&[b'a'; 1024]), 16).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n\x7f\xff\xff\xfftEXt").unwrap();
        assert!(read(&path).is_err());
    }
}
