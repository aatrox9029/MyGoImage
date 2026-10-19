use std::ops::Range;

/// Zero-based ranges, excluding the displayed image. Load forward first and
/// retain half as many preceding images; never wrap or compensate at an edge.
pub fn ranges(position: usize, total: usize, ahead: usize) -> [Range<usize>; 2] {
    if position >= total {
        return [total..total, total..total];
    }
    let current = position.min(total);
    [
        position.saturating_add(1).min(total)
            ..position.saturating_add(ahead).saturating_add(1).min(total),
        current.saturating_sub(ahead / 2)..current,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenth_setting_covers_images_45_through_60_at_image_50() {
        assert_eq!(ranges(49, 100, 10), [50..60, 44..49]);
    }

    #[test]
    fn edges_and_odd_counts_are_clamped_without_wrapping() {
        assert_eq!(ranges(0, 100, 10), [1..11, 0..0]);
        assert_eq!(ranges(99, 100, 10), [100..100, 94..99]);
        assert_eq!(ranges(49, 100, 3), [50..53, 48..49]);
        assert_eq!(ranges(0, 0, 10), [0..0, 0..0]);
        assert_eq!(ranges(0, 1, 10), [1..1, 0..0]);
        assert_eq!(ranges(usize::MAX, 10, 10), [10..10, 10..10]);
    }
}
