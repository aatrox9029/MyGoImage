use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "zh-TW")]
    TraditionalChinese,
    #[default]
    #[serde(rename = "en", other)]
    English,
}

impl Language {
    pub fn from_index(index: i32) -> Self {
        if index == 1 {
            Self::TraditionalChinese
        } else {
            Self::English
        }
    }

    pub fn index(self) -> i32 {
        i32::from(self == Self::TraditionalChinese)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn defaults_and_old_settings_use_english() {
        assert_eq!(Config::default().language, Language::English);
        let old: Config = serde_json::from_str(r#"{"preload_count":12}"#).unwrap();
        assert_eq!(old.language, Language::English);
        assert_eq!(old.preload_count, 12);
        let future: Config = serde_json::from_str(r#"{"language":"fr"}"#).unwrap();
        assert_eq!(future.language, Language::English);
    }

    #[test]
    fn both_languages_survive_save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        for language in [Language::English, Language::TraditionalChinese] {
            let config = Config {
                language,
                ..Config::default()
            };
            config.save(dir.path()).unwrap();
            assert_eq!(Config::load(dir.path()).unwrap().language, language);
            assert_eq!(Language::from_index(language.index()), language);
        }
    }
}
