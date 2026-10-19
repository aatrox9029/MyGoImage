//! Translate display text only. File paths and metadata values stay untouched.
use slint::ComponentHandle;
use std::{collections::BTreeMap, sync::LazyLock};

static TEXT: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("translations.json")).expect("valid translation catalog")
});

pub fn install(window: &crate::MainWindow) {
    window
        .global::<crate::I18n>()
        .on_translate(|message, english| {
            if english {
                translate(&message).into()
            } else {
                message
            }
        });
}

fn translate(message: &str) -> String {
    if let Some(text) = TEXT.get(message) {
        return text.clone();
    }
    // Error chains contain separately generated contexts. Translate each context.
    if let Some((context, cause)) = message.split_once(": ")
        && TEXT.contains_key(context)
    {
        return format!("{}: {}", translate(context), translate(cause));
    }
    for (source, target) in [
        (
            " · 已使用記憶體預算內最高解析度，可繼續放大檢視",
            " · Highest resolution within memory budget; zoom remains available",
        ),
        (" · 掃描中", " · Scanning"),
    ] {
        if let Some(prefix) = message.strip_suffix(source) {
            return format!("{}{target}", translate(prefix));
        }
    }
    for (source, target) in TEXT.iter().filter(|(source, _)| source.contains('{')) {
        if let Some(result) = template(message, source, target) {
            return result;
        }
    }
    message.to_owned()
}

// Match literal segments around format placeholders; captured data is copied verbatim.
fn template(message: &str, source: &str, target: &str) -> Option<String> {
    fn segments(text: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let Some(end) = rest[start..].find('}') else {
                break;
            };
            parts.push(&rest[..start]);
            rest = &rest[start + end + 1..];
        }
        parts.push(rest);
        parts
    }
    let source = segments(source);
    let target = segments(target);
    if source.len() != target.len() {
        return None;
    }
    let mut rest = message.strip_prefix(source[0])?;
    let mut values = Vec::new();
    for (index, literal) in source.iter().enumerate().skip(1) {
        let position = if index == source.len() - 1 {
            rest.strip_suffix(literal)?.len()
        } else {
            rest.find(literal)?
        };
        values.push(&rest[..position]);
        rest = &rest[position + literal.len()..];
    }
    if !rest.is_empty() {
        return None;
    }
    let mut result = target[0].to_owned();
    for (value, literal) in values.iter().zip(target.iter().skip(1)) {
        result.push_str(value);
        result.push_str(literal);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ui_literal_has_an_english_translation() {
        for source in [
            include_str!("../../../ui/main.slint"),
            include_str!("../../../ui/components/format_settings_dialog.slint"),
            include_str!("../../../ui/components/thumbnail_sidebar.slint"),
        ] {
            for part in source.split("I18n.t(\"").skip(1) {
                let text = part.split("\")").next().unwrap();
                let key: String = serde_json::from_str(&format!("\"{text}\"")).unwrap();
                assert!(TEXT.contains_key(&key), "missing translation: {key}");
            }
        }
    }

    #[test]
    fn formatted_messages_preserve_user_data() {
        assert_eq!(
            translate("已複製 3 個檔案至指定資料夾"),
            "Copied 3 files to the destination folder"
        );
        assert_eq!(
            translate("1 / 2 · 中文照片.png · 1.25 MiB · 2026-10-19 · DEL 移至垃圾桶"),
            "1 / 2 · 中文照片.png · 1.25 MiB · 2026-10-19 · DEL to move to trash"
        );
        assert_eq!(translate("原始 workflow 中文"), "原始 workflow 中文");
    }
}
