use crate::centered_position::centered_position;
use slint::winit_030::WinitWindowAccessor;

pub fn center_on_primary(window: &slint::Window) -> bool {
    window
        .with_winit_window(|native| {
            let Some(monitor) = native.primary_monitor() else {
                return false;
            };
            native.set_outer_position(centered_position(
                monitor.position(),
                monitor.size(),
                native.outer_size(),
            ));
            true
        })
        .unwrap_or(false)
}
