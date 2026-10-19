//! Preserve actual text rather than EXIF's quoted/escaped display representation.
pub fn value(field: &exif::Field, exif: &exif::Exif) -> String {
    if let exif::Value::Ascii(values) = &field.value {
        return values
            .iter()
            .map(|v| {
                String::from_utf8_lossy(v)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if field.tag == exif::Tag::UserComment
        && let exif::Value::Undefined(bytes, _) = &field.value
        && let Some(text) = comment(bytes, exif.little_endian())
    {
        return text;
    }
    field.display_value().with_unit(exif).to_string()
}

fn comment(bytes: &[u8], little_endian: bool) -> Option<String> {
    let (header, text) = if bytes.len() >= 8 {
        bytes.split_at(8)
    } else {
        return String::from_utf8(bytes.to_vec()).ok();
    };
    let text = if header == b"UNICODE\0" {
        let (little, text) = match text.get(..2) {
            Some([0xff, 0xfe]) => (true, &text[2..]),
            Some([0xfe, 0xff]) => (false, &text[2..]),
            _ => (little_endian, text),
        };
        if text.len() % 2 != 0 {
            return None;
        }
        let words: Vec<_> = text
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        String::from_utf16(&words).ok()?
    } else if header == b"ASCII\0\0\0" || header == [0; 8] {
        String::from_utf8(text.to_vec()).ok()?
    } else {
        return String::from_utf8(bytes.to_vec()).ok();
    };
    Some(text.trim_end_matches('\0').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_comments_keep_multiline_text_without_encoding_prefix() {
        assert_eq!(
            comment(b"ASCII\0\0\0prompt: cat\nsteps: 20\0", false).as_deref(),
            Some("prompt: cat\nsteps: 20")
        );
        for little in [true, false] {
            let mut data = b"UNICODE\0".to_vec();
            for word in "自訂提示\nworkflow".encode_utf16() {
                data.extend(if little {
                    word.to_le_bytes()
                } else {
                    word.to_be_bytes()
                });
            }
            assert_eq!(
                comment(&data, little).as_deref(),
                Some("自訂提示\nworkflow")
            );
        }
        assert!(comment(b"UNICODE\0\xff", false).is_none());
    }
}
