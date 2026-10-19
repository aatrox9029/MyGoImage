//! Read the saved PSD composite, without allocating or rendering layers.
//! Layout: https://www.adobe.com/devnet-apps/photoshop/fileformatashtml/
use super::*;
use image::{ColorType, ImageError, ImageResult};
#[path = "psd_control.rs"]
mod control;
#[path = "psd_decode.rs"]
mod decode;
#[path = "psd_prediction.rs"]
mod prediction;
#[path = "psd_raw.rs"]
mod raw;
#[path = "psd_row.rs"]
mod row_output;

pub(super) struct PsdDecoder<R> {
    reader: R,
    width: u32,
    height: u32,
    channels: usize,
    depth: usize,
    mode: u16,
    icc: Option<Vec<u8>>,
    control: control::Control,
}
fn u16be(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn u32be(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
impl<R: Read + Seek> PsdDecoder<R> {
    pub fn new(mut reader: R, config: Config, cancel: &Cancel) -> Result<Self> {
        let control = control::Control::new(&config, cancel);
        control.check()?;
        let mut signature = [0; 4];
        reader.read_exact(&mut signature)?;
        anyhow::ensure!(
            &signature == b"8BPS" && u16be(&mut reader)? == 1,
            "PSD: invalid signature or unsupported PSB version"
        );
        let mut reserved = [0; 6];
        reader.read_exact(&mut reserved)?;
        let channels = u16be(&mut reader)? as usize;
        let height = u32be(&mut reader)?;
        let width = u32be(&mut reader)?;
        let depth = u16be(&mut reader)? as usize;
        let mode = u16be(&mut reader)?;
        super::check_dimensions(width, height, &config)?;
        anyhow::ensure!(
            width <= 30000 && height <= 30000 && (1..=56).contains(&channels),
            "PSD: invalid dimensions or channel count"
        );
        anyhow::ensure!(
            matches!(depth, 8 | 16) && matches!(mode, 1 | 3),
            "PSD: only 8/16-bit RGB and grayscale composites are supported (mode={mode}, depth={depth})"
        );
        anyhow::ensure!(
            channels >= if mode == 3 { 3 } else { 1 },
            "PSD: missing color channels"
        );
        let bytes = decode::working_bytes(width, height, channels, depth);
        anyhow::ensure!(
            config
                .image_limits()
                .is_none_or(|(_, _, _, budget)| bytes <= budget),
            "PSD: streaming composite exceeds decode memory budget (required={bytes})"
        );
        let color_len = u32be(&mut reader)?;
        reader.seek(SeekFrom::Current(i64::from(color_len)))?;
        let resources_len = u32be(&mut reader)?;
        let resources_end = reader
            .stream_position()?
            .checked_add(resources_len as u64)
            .context("PSD: resource offset overflow")?;
        let mut icc = None;
        while reader.stream_position()? < resources_end {
            control.check()?;
            reader.read_exact(&mut signature)?;
            anyhow::ensure!(&signature == b"8BIM", "PSD: invalid resource signature");
            let id = u16be(&mut reader)?;
            let mut name = [0];
            reader.read_exact(&mut name)?;
            reader.seek(SeekFrom::Current(((name[0] as i64 + 2) & !1) - 1))?;
            let len = u32be(&mut reader)? as u64;
            let end = reader
                .stream_position()?
                .checked_add(len + (len & 1))
                .context("PSD: resource overflow")?;
            anyhow::ensure!(end <= resources_end, "PSD: resource exceeds section");
            if id == 1039 {
                anyhow::ensure!(len <= 4 * 1024 * 1024, "PSD: ICC profile exceeds 4 MiB");
                let mut profile = vec![0; len as usize];
                reader.read_exact(&mut profile)?;
                icc = Some(profile);
            }
            reader.seek(SeekFrom::Start(end))?;
        }
        let layer_len = u32be(&mut reader)?;
        reader.seek(SeekFrom::Current(i64::from(layer_len)))?;
        Ok(Self {
            reader,
            width,
            height,
            channels,
            depth,
            mode,
            icc,
            control,
        })
    }
}
impl<R: Read + Seek> ImageDecoder for PsdDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }
    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.icc.take())
    }
    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let context = format!(
            "PSD composite {}x{}, {}-bit, mode={}, channels={}",
            self.width, self.height, self.depth, self.mode, self.channels
        );
        self.decode(buf).map_err(|error| {
            ImageError::IoError(io::Error::new(error.kind(), format!("{context}: {error}")))
        })
    }
    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}
