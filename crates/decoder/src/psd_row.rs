/// Scatter a single planar row directly into its final RGBA row. Extra
/// channels are still consumed and validated by the caller, but need no storage.
pub(super) fn write(row: &[u8], output: &mut [u8], channel: usize, depth: usize, rgb: bool) {
    let colors = if rgb { 3 } else { 1 };
    if channel > colors {
        return;
    }
    if depth == 8 {
        write_samples::<1>(row, output, channel, rgb);
    } else {
        write_samples::<2>(row, output, channel, rgb);
    }
}

fn write_samples<const STEP: usize>(row: &[u8], output: &mut [u8], channel: usize, rgb: bool) {
    let samples = row.as_chunks::<STEP>().0.iter();
    let pixels = output.as_chunks_mut::<4>().0;
    if channel == 0 {
        if rgb {
            for (sample, pixel) in samples.zip(pixels) {
                *pixel = [sample[0], 0, 0, 255];
            }
        } else {
            for (sample, pixel) in samples.zip(pixels) {
                *pixel = [sample[0], sample[0], sample[0], 255];
            }
        }
    } else {
        let offset = if rgb { channel } else { 3 };
        for (sample, pixel) in samples.zip(pixels) {
            pixel[offset] = sample[0];
        }
    }
}
