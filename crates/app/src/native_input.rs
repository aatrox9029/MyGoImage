//! One native event filter owns wheel routing, file drops and shortcut capture.
use crate::shortcuts::shortcut;
use crate::{MainWindow, UiCommands, controller::Action, notice};
use slint::{
    ComponentHandle,
    winit_030::{
        EventResult, WinitWindowAccessor,
        winit::{
            event::{ElementState, MouseScrollDelta, WindowEvent},
            keyboard::{Key, ModifiersState, NamedKey},
        },
    },
};

pub fn install(window: &MainWindow, commands: UiCommands) {
    let weak = window.as_weak();
    let mut modifiers = ModifiersState::empty();
    window.window().on_winit_window_event(move |_, event| {
        let Some(window) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        match event {
            WindowEvent::ModifiersChanged(state) => modifiers = state.state(),
            WindowEvent::Focused(false) => modifiers = ModifiersState::empty(),
            WindowEvent::MouseWheel { delta, .. } if window.get_settings_open() => {
                let delta = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y * 40.0,
                    MouseScrollDelta::PixelDelta(position) => {
                        position.y as f32 / window.window().scale_factor()
                    }
                };
                window.invoke_scroll_settings(delta);
                commands
                    .telemetry
                    .record("settings-wheel-routed", 0, 1, true);
                return EventResult::PreventDefault;
            }
            WindowEvent::DroppedFile(path) => {
                if mygo_core::supported(path) {
                    window.invoke_save_settings();
                    window.set_settings_open(false);
                    window.set_capture_shortcut(false);
                    let _ = commands.try_send(Action::Open(path.clone(), false));
                    commands.telemetry.record("image-drop-open", 0, 1, true);
                } else {
                    notice::show(&window, "請拖入支援的圖片格式");
                }
                return EventResult::PreventDefault;
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if window.get_capture_shortcut() {
                    if event.logical_key == Key::Named(NamedKey::Escape) {
                        window.set_capture_shortcut(false);
                    } else if !event.repeat
                        && let Some(key) = shortcut(&event.logical_key, modifiers)
                    {
                        // Preserve the system/application shortcuts with irreversible or essential behavior.
                        if matches!(key.as_str(), "Alt+F4" | "Ctrl+O") {
                            notice::show(&window, "此快捷鍵已保留，請選擇其他組合");
                        } else {
                            window.set_copy_shortcut(key.into());
                            window.set_capture_shortcut(false);
                            window.invoke_save_settings();
                            commands
                                .telemetry
                                .record("copy-shortcut-captured", 0, 1, true);
                        }
                    }
                    return EventResult::PreventDefault;
                }
                if !window.get_settings_open()
                    && !window.get_copy_shortcut().is_empty()
                    && shortcut(&event.logical_key, modifiers).as_deref()
                        == Some(window.get_copy_shortcut().as_str())
                {
                    if !event.repeat {
                        window.invoke_copy_current();
                    }
                    return EventResult::PreventDefault;
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}
