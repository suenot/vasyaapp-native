use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

#[derive(Clone, Debug, Default)]
pub struct AccountView {
    pub id: String,
    pub title: String,
}
#[derive(Clone, Debug, Default)]
pub struct ChatView {
    pub id: i64,
    pub title: String,
    pub preview: String,
    pub unread: i32,
    pub is_forum: bool,
    pub chat_type: String,
    pub username: String,
}
#[derive(Clone, Debug, Default)]
pub struct MessageView {
    pub id: i32,
    pub sender: String,
    pub text: String,
    pub time: String,
    pub outgoing: bool,
    pub pending: bool,
    pub failed: bool,
    pub media_kind: Option<String>,
    pub image_path: Option<PathBuf>,
    pub transcription: Option<String>,
    pub translation: Option<String>,
    pub translation_pending: bool,
    pub translation_error: Option<String>,
    pub translation_show_original: bool,
}
impl MessageView {
    pub fn display_text(&self) -> &str {
        if self.translation_show_original {
            &self.text
        } else {
            self.translation.as_deref().unwrap_or(&self.text)
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct FolderView {
    pub id: String,
    pub title: String,
}
#[derive(Clone, Debug, Default)]
pub struct TopicView {
    pub id: i32,
    pub title: String,
}
#[derive(Clone, Debug)]
pub struct FormField {
    pub key: String,
    pub label: String,
    pub value: String,
    pub secret: bool,
    pub multiline: bool,
}
#[derive(Clone, Debug)]
pub struct Form {
    pub title: String,
    pub description: String,
    pub fields: Vec<FormField>,
    pub submit_label: String,
}
#[derive(Clone, Debug)]
pub struct SearchHitView {
    pub chat_id: i64,
    pub message_id: i32,
    pub title: String,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct Preferences {
    pub hotkeys: BTreeMap<String, String>,
    pub folder_layout: String,
    pub language: String,
    pub density: String,
    pub text_size: f32,
    pub markdown: bool,
    pub merge_messages: bool,
    pub notification_sound: bool,
    pub message_preview: bool,
    pub auto_photos: bool,
    pub auto_stickers: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            hotkeys: crate::default_hotkeys(),
            folder_layout: "horizontal".into(),
            language: "en".into(),
            density: "normal".into(),
            text_size: 14.,
            markdown: false,
            merge_messages: true,
            notification_sound: true,
            message_preview: true,
            auto_photos: true,
            auto_stickers: true,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ViewModel {
    pub preferences: Preferences,
    pub revision: u64,
    pub version: &'static str,
    pub ready: bool,
    pub accounts: Vec<AccountView>,
    pub selected_account: Option<String>,
    pub chats: Arc<Vec<ChatView>>,
    pub selected_chat: Option<i64>,
    pub chat_title: String,
    pub messages: Arc<Vec<MessageView>>,
    pub folders: Vec<FolderView>,
    pub topics: Vec<TopicView>,
    pub selected_topic: Option<i32>,
    pub selected_folder: Option<String>,
    pub draft: String,
    pub search: String,
    pub status: String,
    pub error: Option<String>,
    pub form: Option<Form>,
    pub busy: bool,
    pub has_older: bool,
    pub dark: bool,
    pub scale: f64,
    pub detail: Option<(String, String)>,
    pub search_hits: Arc<Vec<SearchHitView>>,
    pub jump_to: Option<i32>,
    pub remote: bool,
    pub outgoing_translation_target: Option<String>,
    pub outgoing_translation_pending: bool,
}
impl Default for ViewModel {
    fn default() -> Self {
        Self {
            preferences: Preferences::default(),
            revision: 0,
            version: env!("CARGO_PKG_VERSION"),
            ready: false,
            accounts: vec![],
            selected_account: None,
            chats: Arc::new(vec![]),
            selected_chat: None,
            chat_title: "Vasya".into(),
            messages: Arc::new(vec![]),
            folders: vec![],
            topics: vec![],
            selected_topic: None,
            selected_folder: None,
            draft: String::new(),
            search: String::new(),
            status: "Starting Telegram engine…".into(),
            error: None,
            form: None,
            busy: false,
            has_older: false,
            dark: true,
            scale: 1.0,
            detail: None,
            search_hits: Arc::new(vec![]),
            jump_to: None,
            remote: false,
            outgoing_translation_target: None,
            outgoing_translation_pending: false,
        }
    }
}
#[derive(Clone, Debug)]
pub enum FormKind {
    TranslationSettings,
    ChatTranslation,
    Hotkeys,
    Storage,
    Tabs,
    Credentials,
    Login,
    Remote,
    Settings,
    Stt,
    LocalApi,
    CreateGroup,
    CreateChannel,
    Folder,
    Forward(i32),
    SendFile,
    SearchMessages,
}
#[derive(Clone, Debug)]
pub enum UiCommand {
    Shortcut(String),
    Shutdown(std::sync::mpsc::Sender<()>),
    SelectAccount(String),
    SelectChat(i64),
    SelectTopic(Option<i32>),
    SelectFolder(Option<String>),
    Search(String),
    Draft(String),
    Send,
    LoadOlder,
    Refresh,
    OpenForm(FormKind),
    SubmitForm(BTreeMap<String, String>),
    CloseOverlay,
    Download(i32),
    Transcribe(i32),
    Forward(i32),
    ForwardMany(Vec<i32>),
    Retry(i32),
    SendFile(PathBuf),
    SendClipboardImage {
        bytes: Vec<u8>,
        extension: String,
    },
    Logout,
    LeaveChat,
    ChatInfo,
    DeleteFolder,
    Downloads,
    ToggleFavorite,
    JumpToMessage(i64, i32),
    Calls,
    ToggleTheme,
    VisibleMessages {
        account: String,
        chat: i64,
        topic: Option<i32>,
        ids: Vec<i32>,
    },
    ToggleMessageTranslation {
        account: String,
        chat: i64,
        topic: Option<i32>,
        id: i32,
    },
    RetryTranslation {
        account: String,
        chat: i64,
        topic: Option<i32>,
        id: i32,
    },
    EmbeddedMode,
    DismissError,
    OpenDownloaded(i32),
    RecordVoice,
    Camera,
}
