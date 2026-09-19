use std::{collections::HashMap, sync::OnceLock};
/// Translate application labels, retaining external/backend text verbatim.
pub fn tr(language: &str, text: &str) -> String {
    static RU: OnceLock<HashMap<String, String>> = OnceLock::new();
    if language == "ru" {
        RU.get_or_init(|| {
            serde_json::from_str(include_str!("../assets/ru.json")).expect("bundled translations")
        })
        .get(text)
        .cloned()
        .unwrap_or_else(|| text.into())
    } else {
        text.into()
    }
}
