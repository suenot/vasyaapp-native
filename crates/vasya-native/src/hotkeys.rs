use crate::Preferences;
use std::collections::BTreeMap;
pub fn default_hotkeys() -> BTreeMap<String, String> {
    let mut keys: BTreeMap<_, _> = [
        ("focus_search", "meta+k"),
        ("search_in_chat", "meta+f"),
        ("next_chat", "alt+down"),
        ("prev_chat", "alt+up"),
        ("next_chat_tab", "ctrl+tab"),
        ("prev_chat_tab", "ctrl+shift+tab"),
        ("next_unread_chat", "alt+shift+down"),
        ("prev_unread_chat", "alt+shift+up"),
        ("open_settings", "meta+,"),
        ("close_chat", "ctrl+w"),
    ]
    .into_iter()
    .map(|(a, b)| (a.into(), b.into()))
    .collect();
    for n in 1..=9 {
        keys.insert(format!("folder_{n}"), format!("ctrl+{n}"));
    }
    keys
}
pub fn shortcut_action(preferences: &Preferences, chord: &str) -> Option<String> {
    preferences
        .hotkeys
        .iter()
        .find(|(_, v)| v.as_str() == chord)
        .map(|(k, _)| k.clone())
}
