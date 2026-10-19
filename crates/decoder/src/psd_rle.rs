use std::io;

/// PackBits is bounded by both the encoded row and its declared output width.
pub(super) fn unpack(input: &[u8], output: &mut [u8]) -> io::Result<()> {
    let fail = || io::Error::new(io::ErrorKind::InvalidData, "PSD: malformed PackBits row");
    let (mut src, mut dst) = (0, 0);
    while src < input.len() {
        let control = input[src] as i8;
        src += 1;
        if control == -128 {
            continue;
        }
        let count = if control >= 0 {
            control as usize + 1
        } else {
            (1i16 - control as i16) as usize
        };
        let target = output.get_mut(dst..dst + count).ok_or_else(fail)?;
        if control >= 0 {
            target.copy_from_slice(input.get(src..src + count).ok_or_else(fail)?);
            src += count;
        } else {
            target.fill(*input.get(src).ok_or_else(fail)?);
            src += 1;
        }
        dst += count;
    }
    if dst != output.len() {
        return Err(fail());
    }
    Ok(())
}
