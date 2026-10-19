use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};

pub fn centered_position(
    origin: PhysicalPosition<i32>,
    screen: PhysicalSize<u32>,
    window: PhysicalSize<u32>,
) -> PhysicalPosition<i32> {
    PhysicalPosition::new(
        origin
            .x
            .saturating_add((screen.width.saturating_sub(window.width) / 2) as i32),
        origin
            .y
            .saturating_add((screen.height.saturating_sub(window.height) / 2) as i32),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centers_physical_window_including_frame_at_different_dpi() {
        for (screen, window, expected) in [
            ((1920, 1080), (1296, 839), (312, 120)),
            ((3840, 2160), (2592, 1678), (624, 241)),
            ((1920, 1080), (1920, 1080), (0, 0)),
            ((800, 600), (1296, 839), (0, 0)),
        ] {
            assert_eq!(
                centered_position((0, 0).into(), screen.into(), window.into()),
                expected.into()
            );
        }
    }

    #[test]
    fn uses_selected_monitor_origin_in_virtual_desktop_coordinates() {
        assert_eq!(
            centered_position(
                (-1920, -1080).into(),
                (1920, 1080).into(),
                (1280, 800).into()
            ),
            (-1600, -940).into()
        );
    }
}
