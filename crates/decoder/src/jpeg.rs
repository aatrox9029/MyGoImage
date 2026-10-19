use anyhow::{Result, ensure};
use std::io::Read;

/// Supply image's existing EXIF/ICC parser with only the JPEG marker stream,
/// through the first SOS header. Compressed scan data is left for jpeg-decoder.
/// Segment lengths, rather than byte searches, keep FF DA inside metadata safe.
pub(super) fn read_header(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut soi = [0; 2];
    reader.read_exact(&mut soi)?;
    ensure!(soi == [0xff, 0xd8], "JPEG 起始標記錯誤");
    let mut header = soi.to_vec();
    loop {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        ensure!(byte[0] == 0xff, "JPEG 標記前綴錯誤");
        header.push(0xff);
        // JPEG permits repeated FF fill bytes before a marker.
        loop {
            reader.read_exact(&mut byte)?;
            header.push(byte[0]);
            if byte[0] != 0xff {
                break;
            }
        }
        let marker = byte[0];
        ensure!(marker != 0, "JPEG 標頭包含無效標記");
        if marker == 0xd9 {
            // Let the existing decoder report a missing frame/scan.
            return Ok(header);
        }
        if matches!(marker, 0x01 | 0xd0..=0xd8) {
            continue;
        }
        let mut length = [0; 2];
        reader.read_exact(&mut length)?;
        let length_value = usize::from(u16::from_be_bytes(length));
        ensure!(length_value >= 2, "JPEG 區段長度錯誤");
        header.extend_from_slice(&length);
        let offset = header.len();
        header.resize(offset + length_value - 2, 0);
        reader.read_exact(&mut header[offset..])?;
        if marker == 0xda {
            return Ok(header);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn header_stops_before_scan_and_ignores_markers_inside_metadata() {
        let prefix = b"\xff\xd8\xff\xff\xe1\x00\x06\xff\xda\xff\xd9\xff\xda\x00\x02";
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(b"compressed scan data");
        let mut reader = Cursor::new(bytes);
        assert_eq!(read_header(&mut reader).unwrap(), prefix);
        assert_eq!(reader.position(), prefix.len() as u64);
        for bytes in [
            &b"bad"[..],
            &b"\xff\xd8\xff\xe1\x00\x01"[..],
            &b"\xff\xd8\xff\xe1\x00\x04\x00"[..],
            &b"\xff\xd8\xff\x00"[..],
            &b"\xff\xd8\x00"[..],
        ] {
            assert!(read_header(&mut Cursor::new(bytes)).is_err());
        }
    }
}
