use slint::winit_030::winit::keyboard::{Key, ModifiersState};

pub fn shortcut(key: &Key, modifiers: ModifiersState) -> Option<String> {
    let key = match key {
        Key::Character(text)
            if text.chars().count() == 1 && !text.chars().any(char::is_control) =>
        {
            text.to_uppercase()
        }
        Key::Named(name) => {
            let name = format!("{name:?}");
            if !name
                .strip_prefix('F')
                .is_some_and(|n| n.parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)))
            {
                return None;
            }
            name
        }
        _ => return None,
    };
    let mut parts = Vec::new();
    if modifiers.control_key() {
        parts.push("Ctrl");
    }
    if modifiers.alt_key() {
        parts.push("Alt");
    }
    if modifiers.shift_key() {
        parts.push("Shift");
    }
    if modifiers.super_key() {
        return None;
    }
    parts.push(&key);
    Some(parts.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::winit_030::winit::keyboard::NamedKey;
    #[test]
    fn shortcut_normalizes_case_and_distinguishes_modifiers() {
        let key = Key::Character("c".into());
        assert_eq!(
            shortcut(&key, ModifiersState::CONTROL | ModifiersState::SHIFT).as_deref(),
            Some("Ctrl+Shift+C")
        );
        assert_eq!(
            shortcut(&Key::Named(NamedKey::F8), ModifiersState::empty()).as_deref(),
            Some("F8")
        );
        assert!(shortcut(&Key::Named(NamedKey::Control), ModifiersState::CONTROL).is_none());
        assert!(shortcut(&key, ModifiersState::SUPER).is_none());
    }
}
