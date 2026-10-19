/// Prediction resets at each channel row. Preserve 16-bit carries before
/// reducing the recovered samples to their high bytes for RGBA8 display.
pub(super) fn undo(row: &mut [u8], depth: usize) {
    if depth == 8 {
        for i in 1..row.len() {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
    } else {
        let mut previous = 0u16;
        for sample in row.as_chunks_mut::<2>().0 {
            previous = previous.wrapping_add(u16::from_be_bytes(*sample));
            sample.copy_from_slice(&previous.to_be_bytes());
        }
    }
}
