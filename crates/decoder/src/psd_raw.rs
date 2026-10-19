use super::*;

pub(super) const MAX_CHUNK_BYTES: usize = 256 * 1024;

impl<R: Read + Seek> PsdDecoder<R> {
    /// Visit small horizontal bands across the planar channels. The output band
    /// stays cache-local, while reads remain large enough to avoid tiny I/O.
    pub(super) fn decode_raw(mut self, buf: &mut [u8]) -> io::Result<()> {
        let start = self.reader.stream_position()?;
        let row_bytes = self.width as usize * (self.depth / 8);
        let height = self.height as usize;
        let output_row = self.width as usize * 4;
        let band_rows = (MAX_CHUNK_BYTES / row_bytes).clamp(1, 64).min(height);
        let mut band = vec![0; band_rows * row_bytes];
        for y in (0..height).step_by(band_rows) {
            let rows = band_rows.min(height - y);
            for channel in 0..self.channels {
                self.control.check()?;
                let offset = start + ((channel * height + y) * row_bytes) as u64;
                self.reader.seek(SeekFrom::Start(offset))?;
                self.reader
                    .read_exact(&mut band[..rows * row_bytes])
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("PSD: compression=0, channel={channel}, row={y}: {error}"),
                        )
                    })?;
                for (index, row) in band[..rows * row_bytes].chunks_exact(row_bytes).enumerate() {
                    self.control.check()?;
                    let output_y = y + index;
                    row_output::write(
                        row,
                        &mut buf[output_y * output_row..(output_y + 1) * output_row],
                        channel,
                        self.depth,
                        self.mode == 3,
                    );
                }
            }
        }
        self.control.check()
    }
}
