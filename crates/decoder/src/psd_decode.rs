use super::*;

/// Upper bound for decoder-owned storage plus RGBA; downstream resizing has
/// its own budget. Include the largest PSD-v1 encoded row and zlib headroom.
pub(super) fn working_bytes(width: u32, height: u32, channels: usize, depth: usize) -> u64 {
    u64::from(width) * u64::from(height) * 4
        + (u64::from(width) * (depth / 8) as u64).max(raw::MAX_CHUNK_BYTES as u64)
        + u64::from(height) * channels as u64 * 2
        + u64::from(u16::MAX)
        + 128 * 1024
}

impl<R: Read + Seek> PsdDecoder<R> {
    pub(super) fn decode(mut self, buf: &mut [u8]) -> io::Result<()> {
        self.control.check()?;
        let width = self.width as usize;
        let height = self.height as usize;
        if buf.len() != width * height * 4 {
            return Err(invalid("PSD: incorrect output size"));
        }
        let compression = u16be(&mut self.reader)?;
        if compression > 3 {
            return Err(invalid(&format!(
                "PSD: unsupported composite compression={compression}"
            )));
        }
        if compression == 0 {
            return self.decode_raw(buf);
        }
        let lengths: Vec<u16> = if compression == 1 {
            let mut lengths = Vec::with_capacity(height * self.channels);
            for _ in 0..self.channels {
                self.control.check()?;
                for _ in 0..height {
                    lengths.push(u16be(&mut self.reader)?);
                }
            }
            lengths
        } else {
            Vec::new()
        };
        let mut input: Box<dyn Read + '_> = if compression >= 2 {
            Box::new(flate2::read::ZlibDecoder::new(&mut self.reader))
        } else {
            Box::new(&mut self.reader)
        };
        let mut row = vec![0; width * (self.depth / 8)];
        // Allocate once for the largest declared row; Vec growth would otherwise
        // reserve more than the encoded-row allowance in working_bytes().
        let mut encoded = vec![0; lengths.iter().copied().max().unwrap_or(0) as usize];
        for channel in 0..self.channels {
            for y in 0..height {
                self.control.check()?;
                let result = if compression == 1 {
                    encoded.resize(lengths[channel * height + y] as usize, 0);
                    input
                        .read_exact(&mut encoded)
                        .and_then(|()| crate::psd_rle::unpack(&encoded, &mut row))
                } else {
                    input.read_exact(&mut row)
                };
                result.map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "PSD: compression={compression}, channel={channel}, row={y}: {error}"
                        ),
                    )
                })?;
                if compression == 3 {
                    prediction::undo(&mut row, self.depth);
                }
                row_output::write(
                    &row,
                    &mut buf[y * width * 4..(y + 1) * width * 4],
                    channel,
                    self.depth,
                    self.mode == 3,
                );
            }
        }
        if compression >= 2 && input.read(&mut [0])? != 0 {
            return Err(invalid("PSD: ZIP output exceeds expected size"));
        }
        self.control.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_is_bounded_by_row_width_and_length_table() {
        let pixels = 10000u64 * 10000;
        let working = working_bytes(10000, 10000, 56, 16);
        assert!(working - pixels * 4 < 2 * 1024 * 1024);
        assert!(working < pixels * (56 * 2 + 4));
    }
}
