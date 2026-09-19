//! GUI-independent state machine. All I/O lives on a dedicated Tokio runtime.
mod benchmark;
mod hotkeys;
mod i18n;
mod platform;
mod translation;
mod types;
use anyhow::{anyhow, Context, Result};
pub use hotkeys::{default_hotkeys, shortcut_action};
pub use i18n::tr;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use translation::{IncomingKey, SendIntent, SendPayload, TranslationState};
pub use types::*;
use vasya_backend::Backend;
use vasya_core::events::Event;

#[derive(Clone)]
pub struct NativeApp {
    commands: mpsc::Sender<UiCommand>,
    state: watch::Receiver<Arc<ViewModel>>,
}
impl NativeApp {
    pub fn new(flavor: &str) -> Result<Self> {
        let data = profile_dir(flavor)?;
        let (commands, rx) = mpsc::channel(512);
        let (tx, state) = watch::channel(Arc::new(ViewModel::default()));
        std::thread::Builder::new()
            .name(format!("vasya-{flavor}-runtime"))
            .spawn(move || {
                let run = || -> Result<()> {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(4)
                        .enable_all()
                        .build()?;
                    runtime.block_on(async {
                        let backend = Arc::new(Backend::new(data.clone()).await?);
                        Controller::new(backend, tx.clone(), data).run(rx).await;
                        Ok(())
                    })
                };
                if let Err(error) = run() {
                    tx.send_modify(|state| {
                        let state = Arc::make_mut(state);
                        state.error = Some(error.to_string());
                        state.status = "Engine initialization failed".into();
                        state.revision += 1;
                    });
                }
            })?;
        Ok(Self { commands, state })
    }
    /// Flush encrypted sessions and close transports after the GUI event loop exits.
    pub fn shutdown(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.commands.try_send(UiCommand::Shutdown(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }
    pub fn snapshot(&self) -> Arc<ViewModel> {
        self.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<ViewModel>> {
        self.state.clone()
    }
    pub fn send(&self, command: UiCommand) {
        if let Err(error) = self.commands.try_send(command) {
            tracing::warn!(
                "UI command queue unavailable: {}",
                if matches!(error, mpsc::error::TrySendError::Full(_)) {
                    "busy"
                } else {
                    "closed"
                }
            );
        }
    }
}
fn profile_dir(flavor: &str) -> Result<PathBuf> {
    if benchmark::enabled() {
        return Ok(std::env::temp_dir()
            .join(format!("vasya-benchmark-{}", std::process::id()))
            .join(flavor));
    }
    if !matches!(flavor, "gpui" | "iced" | "test") {
        return Err(anyhow!("Unknown GUI profile"));
    }
    if let Some(base) = std::env::var_os("VASYA_NATIVE_DATA_DIR") {
        return Ok(PathBuf::from(base).join(flavor));
    }
    let home = std::env::var_os("HOME").context("HOME is missing")?;
    #[cfg(target_os = "macos")]
    let base = PathBuf::from(home).join("Library/Application Support");
    #[cfg(not(target_os = "macos"))]
    let base = PathBuf::from(home).join(".local/share");
    Ok(base.join(format!("cc.marketmaker.vasya.{flavor}")))
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct DialogKey {
    account: String,
    chat: i64,
    topic: Option<i32>,
}
#[derive(Clone, Debug)]
enum Query {
    TranslationProvider(u64),
    TranslationPreferences(DialogKey, Value),
    TranslationIncoming(IncomingKey),
    TranslationOutgoing(Box<SendIntent>),
    Delivery(Box<SendIntent>),
    Accounts,
    LoggedOut(String),
    Credentials,
    Settings,
    Connection,
    Chats(String, u64),
    Folders(String),
    Tabs(String),
    Topics(DialogKey),
    Messages(DialogKey, bool, u64),
    Jump(DialogKey, i32),
    Form(FormKind),
    Submitted(Submission),
    Downloaded(DialogKey, i32, bool),
    Transcribed(DialogKey, i32),
    Details(String),
    GlobalSearch(String, u64),
    Capture,
    Ignore,
}
#[derive(Clone, Debug)]
enum Submission {
    TranslationSettings,
    ChatTranslation(DialogKey),
    Hotkeys,
    Storage,
    Tabs,
    Credentials,
    Phone,
    Code,
    Password,
    Remote,
    Settings,
    Stt,
    LocalApi,
    Group,
    Channel,
    Folder,
    Forward,
    File,
    Search,
}
struct Reply {
    epoch: u64,
    query: Query,
    result: Result<Value>,
    cached: bool,
}
struct Controller {
    backend: Arc<Backend>,
    tx: watch::Sender<Arc<ViewModel>>,
    vm: ViewModel,
    replies_tx: mpsc::Sender<Reply>,
    replies: mpsc::Receiver<Reply>,
    epoch: u64,
    chats: HashMap<String, Vec<ChatView>>,
    folders: Vec<Value>,
    tabs: Vec<Value>,
    denied_accounts: HashSet<String>,
    transport_changing: bool,
    resubscribe: bool,
    histories: HashMap<DialogKey, Vec<MessageView>>,
    history_lru: VecDeque<DialogKey>,
    generation: u64,
    event_sequence: u64,
    message_events: HashMap<(String, i64, i32), u64>,
    chat_events: HashMap<(String, i64), u64>,
    loading_chats: HashSet<String>,
    search_task: Option<tokio::task::JoinHandle<()>>,
    transcribing: HashSet<(DialogKey, i32)>,
    transcription_slots: Arc<tokio::sync::Semaphore>,
    loading: HashSet<DialogKey>,
    cached_pages: HashMap<(DialogKey, u64), HashSet<i32>>,
    refresh_needed: HashSet<DialogKey>,
    chats_dirty: bool,
    history_dirty: bool,
    search_results: Vec<ChatView>,
    form_kind: Option<Submission>,
    forward_id: Option<i32>,
    forward_ids: Vec<i32>,
    login_account: Option<String>,
    pending_id: i32,
    downloads: HashMap<(DialogKey, i32), PathBuf>,
    downloading: HashSet<(DialogKey, i32)>,
    media_slots: Arc<tokio::sync::Semaphore>,
    settings: Value,
    data_dir: PathBuf,
    voice_stop: Option<watch::Sender<bool>>,
    translation: TranslationState,
    tasks: std::sync::Mutex<Vec<tokio::task::AbortHandle>>,
}
impl Controller {
    fn new(backend: Arc<Backend>, tx: watch::Sender<Arc<ViewModel>>, data_dir: PathBuf) -> Self {
        let (replies_tx, replies) = mpsc::channel(512);
        Self {
            backend,
            tx,
            vm: ViewModel::default(),
            replies_tx,
            replies,
            epoch: 0,
            chats: HashMap::new(),
            folders: vec![],
            tabs: vec![],
            denied_accounts: HashSet::new(),
            transport_changing: false,
            resubscribe: false,
            histories: HashMap::new(),
            history_lru: VecDeque::new(),
            generation: 0,
            event_sequence: 0,
            message_events: HashMap::new(),
            chat_events: HashMap::new(),
            loading_chats: HashSet::new(),
            search_task: None,
            transcribing: HashSet::new(),
            transcription_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            loading: HashSet::new(),
            cached_pages: HashMap::new(),
            refresh_needed: HashSet::new(),
            chats_dirty: false,
            history_dirty: false,
            search_results: vec![],
            form_kind: None,
            forward_id: None,
            forward_ids: vec![],
            login_account: None,
            pending_id: -1,
            downloads: HashMap::new(),
            downloading: HashSet::new(),
            media_slots: Arc::new(tokio::sync::Semaphore::new(3)),
            settings: json!({}),
            data_dir,
            voice_stop: None,
            translation: TranslationState::default(),
            tasks: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn spawn(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let handle = tokio::spawn(task);
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle.abort_handle());
        handle
    }
    fn abort_requests(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
    fn publish(&mut self) {
        self.pump_translations();
        if self.chats_dirty {
            self.rebuild_chats();
            self.chats_dirty = false;
        }
        if self.history_dirty {
            self.rebuild_history();
            self.history_dirty = false;
        }
        self.decorate_translation();
        self.vm.revision += 1;
        self.tx.send_replace(Arc::new(self.vm.clone()));
    }
    fn request(&self, query: Query, method: &str, path: String, body: Value, cache: bool) {
        let backend = self.backend.clone();
        let tx = self.replies_tx.clone();
        let epoch = self.epoch;
        let method = method.to_string();
        self.spawn(async move {
            if cache && !benchmark::enabled() {
                let cp = format!("/native/cache?path={}", encode(&path));
                if let Ok(value) = backend.request("GET", &cp, Value::Null).await {
                    if !value.is_null() {
                        let _ = tx
                            .send(Reply {
                                epoch,
                                query: query.clone(),
                                result: Ok(value),
                                cached: true,
                            })
                            .await;
                    }
                }
            }
            let result = if benchmark::enabled() {
                benchmark::request(&method, &path, &body)
            } else {
                backend.request(&method, &path, body).await
            };
            let _ = tx
                .send(Reply {
                    epoch,
                    query,
                    result,
                    cached: false,
                })
                .await;
        });
    }
    fn get(&self, query: Query, path: String, cache: bool) {
        self.request(query, "GET", path, Value::Null, cache);
    }
    fn account_path(&self) -> Result<String> {
        Ok(format!(
            "/api/v1/accounts/{}",
            encode(
                self.vm
                    .selected_account
                    .as_deref()
                    .context("Choose an account first")?
            )
        ))
    }
    fn key(&self) -> Option<DialogKey> {
        Some(DialogKey {
            account: self.vm.selected_account.clone()?,
            chat: self.vm.selected_chat?,
            topic: self.vm.selected_topic,
        })
    }
    fn dialog_path(key: &DialogKey) -> String {
        format!(
            "/api/v1/accounts/{}/chats/{}",
            encode(&key.account),
            key.chat
        )
    }
    fn bootstrap(&self) {
        self.get(Query::Connection, "/native/connection".into(), false);
        self.get(Query::Settings, "/native/settings".into(), false);
        self.get(Query::Accounts, "/api/v1/accounts".into(), false);
        self.get(
            Query::Credentials,
            "/api/v1/telegram/credentials".into(),
            false,
        );
    }
    async fn run(mut self, mut commands: mpsc::Receiver<UiCommand>) {
        let mut events = self.backend.subscribe();
        self.bootstrap();
        let mut stress_tick = tokio::time::interval(Duration::from_millis(10));
        stress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stress_sequence = 0;
        let mut shutdown_ack = None;
        let mut dirty = false;
        let mut last_publish = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _=stress_tick.tick(), if benchmark::enabled()=>{stress_sequence+=1;self.event(benchmark::event(stress_sequence));dirty=true;},
                command = commands.recv() => match command {
                    Some(UiCommand::Shutdown(ack)) => {shutdown_ack=Some(ack);break;},
                    Some(command) => { if let Err(e) = self.command(command) { self.vm.error = Some(e.to_string()); } dirty=true; }
                    None => break,
                },
                reply = self.replies.recv() => if let Some(reply) = reply {
                    if reply.epoch == self.epoch { self.reply(reply); if self.resubscribe {events=self.backend.subscribe();self.resubscribe=false;} dirty=true; }
                },
                event = events.recv() => match event {
                    Ok(event) => { self.event(event); dirty=true; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { self.refresh(); dirty=true; }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tokio::time::sleep_until(last_publish + Duration::from_millis(16)), if dirty => {
                    self.publish(); dirty=false; last_publish=tokio::time::Instant::now();
                }
            }
        }
        if let Some(stop) = self.voice_stop.take() {
            let _ = stop.send(true);
        }
        self.abort_requests();
        self.backend.shutdown().await;
        if let Some(ack) = shutdown_ack {
            let _ = ack.send(());
        }
    }
    fn refresh(&mut self) {
        self.get(Query::Accounts, "/api/v1/accounts".into(), false);
        if let Some(account) = self.vm.selected_account.clone() {
            self.load_chats(account);
        }
        if let Some(key) = self.key() {
            self.load_messages(key, false);
        }
    }
    fn load_chats(&mut self, account: String) {
        if !self.loading_chats.insert(account.clone()) {
            return;
        }
        self.get(
            Query::Chats(account.clone(), self.event_sequence),
            format!("/api/v1/accounts/{}/chats?source=live", encode(&account)),
            true,
        );
        self.get(
            Query::Folders(account.clone()),
            format!("/api/v1/accounts/{}/folders", encode(&account)),
            false,
        );
    }
    fn load_messages(&mut self, key: DialogKey, older: bool) {
        if !self.loading.insert(key.clone()) {
            if !older {
                self.refresh_needed.insert(key);
            }
            return;
        }
        let offset = if older {
            self.histories
                .get(&key)
                .and_then(|v| v.iter().filter(|m| m.id > 0).map(|m| m.id).min())
                .unwrap_or(0)
        } else {
            0
        };
        let mut path = format!(
            "{}/messages?limit=50&offset_id={offset}",
            Self::dialog_path(&key)
        );
        if let Some(topic) = key.topic {
            path.push_str(&format!("&topic_id={topic}"));
        }
        self.get(Query::Messages(key, older, self.event_sequence), path, true);
    }
    fn visible_chats(&mut self) {
        self.chats_dirty = true;
    }
    fn rebuild_chats(&mut self) {
        let account = self.vm.selected_account.as_deref().unwrap_or("");
        let search = self.vm.search.to_lowercase();
        let folder = self
            .vm
            .selected_folder
            .as_ref()
            .and_then(|id| self.folders.iter().find(|f| f["id"].as_str() == Some(id)));
        let items = self
            .chats
            .get(account)
            .into_iter()
            .flatten()
            .filter(|c| {
                if !search.is_empty()
                    && !c.title.to_lowercase().contains(&search)
                    && !c.preview.to_lowercase().contains(&search)
                {
                    return false;
                }
                match self.vm.selected_folder.as_deref() {
                    Some("contacts") => {
                        return c.chat_type == "user" && !c.username.to_lowercase().contains("bot")
                    }
                    Some("chats") => return c.chat_type == "group" || c.chat_type == "channel",
                    Some("favorites") => {
                        return arr(&self.settings["favorites"][account])
                            .iter()
                            .any(|v| v.as_i64() == Some(c.id))
                    }
                    _ => {}
                }
                if let Some(f) = folder {
                    let includes = arr(&f["included_chat_ids"]);
                    let excludes = arr(&f["excluded_chat_ids"]);
                    if excludes.iter().any(|v| v.as_i64() == Some(c.id)) {
                        return false;
                    }
                    if includes.iter().any(|v| v.as_i64() == Some(c.id)) {
                        return true;
                    }
                    let types = arr(&f["included_chat_types"]);
                    let category = match c.chat_type.as_str() {
                        "user" if c.username.to_lowercase().contains("bot") => "bots",
                        "user" => "contacts",
                        "group" => "groups",
                        "channel" => "channels",
                        _ => "non_contacts",
                    };
                    if arr(&f["excluded_chat_types"])
                        .iter()
                        .any(|v| v.as_str() == Some(category))
                    {
                        return false;
                    }
                    return types
                        .iter()
                        .any(|v| v.as_str() == Some(category) || v.as_str() == Some(&c.chat_type));
                }
                true
            })
            .cloned()
            .collect();
        let mut items: Vec<ChatView> = items;
        if !search.is_empty() {
            for c in &self.search_results {
                if !items.iter().any(|i| i.id == c.id) {
                    items.push(c.clone());
                }
            }
        }
        self.vm.chats = Arc::new(items);
    }
    fn show_history(&mut self) {
        self.history_dirty = true;
    }
    fn rebuild_history(&mut self) {
        self.vm.messages = Arc::new(
            self.key()
                .and_then(|key| self.histories.get(&key).cloned())
                .unwrap_or_default(),
        );
    }
    fn command(&mut self, command: UiCommand) -> Result<()> {
        match command {
            UiCommand::Shutdown(_) => {},
            UiCommand::Shortcut(action)=>{
                match action.as_str(){
                    "open_settings"=>self.open_form(FormKind::Settings),
                    "search_in_chat"=>self.open_form(FormKind::SearchMessages),
                    "close_chat"|"close_panel"=>{self.vm.selected_chat=None;self.vm.selected_topic=None;self.vm.messages=Arc::new(vec![]);},
                    "next_chat"|"next_chat_tab"|"prev_chat"|"prev_chat_tab"|"next_unread_chat"|"prev_unread_chat"=>{
                        let chats:Vec<_>=self.vm.chats.iter().filter(|c|!action.contains("unread")||c.unread>0).map(|c|c.id).collect();
                        if !chats.is_empty(){let index=chats.iter().position(|id|Some(*id)==self.vm.selected_chat).unwrap_or(0);let next=if action.starts_with("prev"){(index+chats.len()-1)%chats.len()}else{(index+1)%chats.len()};self.command(UiCommand::SelectChat(chats[next]))?;}
                    },
                    _=>{if let Some(index)=action.strip_prefix("folder_").and_then(|n|n.parse::<usize>().ok()){if let Some(folder)=self.vm.folders.get(index.saturating_sub(1)){self.command(UiCommand::SelectFolder(Some(folder.id.clone())))?;}}}
                }
            },
            UiCommand::SelectAccount(account) => {
                self.save_translation_draft();
                self.vm.selected_account=Some(account.clone()); self.vm.selected_chat=None; self.vm.selected_topic=None;
                self.vm.messages=Arc::new(vec![]);self.vm.topics.clear();self.vm.folders.clear();self.tabs.clear();self.folders.clear();self.vm.selected_folder=None;self.vm.search.clear();self.vm.draft.clear();
                self.visible_chats();self.load_chats(account);
            }
            UiCommand::JumpToMessage(chat,id)=>{self.command(UiCommand::SelectChat(chat))?;self.vm.search_hits=Arc::new(vec![]);self.vm.detail=None;let key=self.key().context("Choose a chat")?;self.get(Query::Jump(key.clone(),id),format!("{}/messages?limit=50&offset_id={}",Self::dialog_path(&key),id.saturating_add(1)),false);},
            UiCommand::SelectChat(chat) => {
                self.save_translation_draft();
                self.vm.selected_chat=Some(chat); self.vm.selected_topic=None;self.vm.jump_to=None;self.vm.topics.clear();self.vm.draft.clear();self.vm.has_older=true;
                self.vm.chat_title=self.chats.get(self.vm.selected_account.as_deref().unwrap_or("")).into_iter().flatten().find(|c|c.id==chat).map(|c|c.title.clone()).unwrap_or_else(||chat.to_string());
                self.restore_translation_draft();
                self.show_history();
                if let Some(key)=self.key() {
                    self.load_messages(key.clone(),false);
                    if self.vm.chats.iter().any(|c|c.id==chat && c.is_forum) { self.get(Query::Topics(key.clone()),format!("{}/topics",Self::dialog_path(&key)),false); }
                }
            }
            UiCommand::SelectTopic(topic) => { self.save_translation_draft();self.vm.selected_topic=topic;self.restore_translation_draft();self.show_history(); if let Some(key)=self.key(){self.load_messages(key,false);} }
            UiCommand::SelectFolder(folder) => {self.vm.selected_folder=folder;self.visible_chats();}
            UiCommand::Search(search) => {
                self.vm.search=search.clone();self.search_results.clear();self.visible_chats();self.generation+=1;
                if let Some(task)=self.search_task.take(){task.abort();}
                if search.chars().count()>=3 {
                    let generation=self.generation;let account=self.vm.selected_account.clone().context("Choose an account")?;
                    let path=format!("/api/v1/accounts/{}/search?q={}&limit=50",encode(&account),encode(&search));
                    let backend=self.backend.clone();let tx=self.replies_tx.clone();let epoch=self.epoch;
                    self.search_task=Some(self.spawn(async move {tokio::time::sleep(Duration::from_millis(300)).await;let result=backend.request("GET",&path,Value::Null).await;let _=tx.send(Reply{epoch,query:Query::GlobalSearch(account,generation),result,cached:false}).await;}));
                }
            }
            UiCommand::Draft(text)=>{self.vm.draft=text;self.translation.draft_revision+=1;self.save_translation_draft();},
            UiCommand::ToggleMessageTranslation{account,chat,topic,id}=>self.translation_action(DialogKey{account,chat,topic},id,false),
            UiCommand::RetryTranslation{account,chat,topic,id}=>self.translation_action(DialogKey{account,chat,topic},id,true),
            UiCommand::Send=>self.send_message(None)?,
            UiCommand::Retry(id)=>self.send_message(Some(id))?,
            UiCommand::LoadOlder=>if let Some(key)=self.key(){if self.vm.has_older {self.load_messages(key,true);}},
            UiCommand::Refresh=>self.refresh(),
            UiCommand::OpenForm(kind)=>self.open_form(kind),
            UiCommand::SubmitForm(values)=>self.submit(values)?,
            UiCommand::CloseOverlay=>{self.vm.busy=false;self.vm.form=None;self.vm.detail=None;self.vm.search_hits=Arc::new(vec![]);self.form_kind=None;},
            UiCommand::DismissError=>self.vm.error=None,
            UiCommand::ToggleTheme=>{self.vm.dark=!self.vm.dark;self.settings["dark"]=json!(self.vm.dark);self.request(Query::Ignore,"PUT","/native/settings".into(),self.settings.clone(),false);},
            UiCommand::Forward(id)=>{self.forward_ids=vec![id];self.open_form(FormKind::Forward(id));},
            UiCommand::ForwardMany(ids)=>{let ids:Vec<_>=ids.into_iter().filter(|id|*id>0).collect();if ids.is_empty(){return Err(anyhow!("Select messages to forward"));}self.open_form(FormKind::Forward(ids[0]));self.forward_ids=ids;},
            UiCommand::Download(id)=>self.download(id,false)?,
            UiCommand::OpenDownloaded(id)=>self.download(id,true)?,
            UiCommand::Transcribe(id)=>self.transcribe(id)?,
            UiCommand::SendFile(path)=>self.upload(path)?,
            UiCommand::SendClipboardImage { bytes, extension } => {
                if !matches!(extension.to_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif" | "tiff") || bytes.len() > 32 * 1024 * 1024 { return Err(anyhow!("Clipboard image format or size unsupported")); }
                self.begin_send(SendPayload::Clipboard(Arc::new(bytes),extension),None)?;
            },
            UiCommand::VisibleMessages { account, chat, topic, ids } => {
                // A queued viewport update belongs to the snapshot that produced it,
                // even when another dialog contains the same numeric message IDs.
                if self.key() != Some(DialogKey { account, chat, topic }) { return Ok(()); }
                self.translation.visible_dialog=self.key();
                self.translation.visible=ids.iter().copied().take(80).collect();
                for id in ids.into_iter().take(80) {
                    if let Some(message)=self.vm.messages.iter().find(|m|m.id==id) {
                        if (message.media_kind.as_deref()==Some("photo")&&self.vm.preferences.auto_photos)||(message.media_kind.as_deref()==Some("sticker")&&self.vm.preferences.auto_stickers) {let _=self.download(id,false);}
                        else if matches!(message.media_kind.as_deref(),Some("voice")) && self.settings["auto_transcribe"].as_bool()==Some(true) && message.transcription.is_none() {let _=self.transcribe(id);}
                    }
                }
            },
            UiCommand::Logout=>{
                let account=self.vm.selected_account.clone().context("Choose an account")?;let path=self.account_path()?;
                self.epoch+=1;self.abort_requests();self.loading.clear();self.cached_pages.clear();self.loading_chats.clear();self.refresh_needed.clear();
                self.forget_translation_account(&account);self.denied_accounts.insert(account.clone());self.chats.remove(&account);self.histories.retain(|k,_|k.account!=account);self.downloads.retain(|(k,_),_|k.account!=account);self.downloading.retain(|(k,_)|k.account!=account);self.transcribing.retain(|(k,_)|k.account!=account);
                self.vm.accounts.retain(|a|a.id!=account);self.vm.selected_account=None;self.vm.selected_chat=None;self.vm.selected_topic=None;self.vm.folders.clear();self.vm.topics.clear();self.vm.chats=Arc::new(vec![]);self.vm.messages=Arc::new(vec![]);self.vm.draft.clear();self.vm.search_hits=Arc::new(vec![]);self.search_results.clear();
                self.request(Query::LoggedOut(account),"DELETE",path,Value::Null,false);
            },
            UiCommand::LeaveChat=>{let key=self.key().context("Choose a chat")?;self.request(Query::Submitted(Submission::Group),"DELETE",Self::dialog_path(&key),Value::Null,false);self.vm.selected_chat=None;self.vm.messages=Arc::new(vec![]);},
            UiCommand::DeleteFolder=>{let id=self.vm.selected_folder.clone().filter(|id|self.folders.iter().any(|f|f["id"].as_str()==Some(id))).context("Choose a custom folder first")?;self.request(Query::Submitted(Submission::Folder),"DELETE",format!("{}/folders/{}",self.account_path()?,encode(&id)),Value::Null,false);self.vm.selected_folder=None;},
            UiCommand::ToggleFavorite=>{let key=self.key().context("Choose a chat")?;let mut ids:Vec<i64>=arr(&self.settings["favorites"][&key.account]).iter().filter_map(Value::as_i64).collect();if ids.contains(&key.chat){ids.retain(|id|*id!=key.chat);}else{ids.push(key.chat);}if !self.settings["favorites"].is_object(){self.settings["favorites"]=json!({});}self.settings["favorites"][&key.account]=json!(ids);self.request(Query::Ignore,"PUT","/native/settings".into(),self.settings.clone(),false);self.visible_chats();},
            UiCommand::Downloads=>{self.vm.detail=Some((tr(&self.vm.preferences.language,"Downloads"),format!("Active or queued: {}\nCached files: {}\n\n{}",self.downloading.len(),self.downloads.len(),self.downloads.values().map(|p|p.display().to_string()).collect::<Vec<_>>().join("\n"))));},
            UiCommand::ChatInfo=>{let key=self.key().context("Choose a chat")?;self.vm.detail=Some((self.vm.chat_title.clone(),format!("Chat ID: {}\nAccount: {}\nTopic: {:?}",key.chat,key.account,key.topic)));},
            UiCommand::Calls=>self.vm.detail=Some(("Calls".into(),"Audio calls are unavailable: the original project's WebRTC transport and encryption are unfinished. Signaling APIs are retained; no connected state is simulated.".into())),
            UiCommand::EmbeddedMode=>{
                self.epoch+=1;self.clear_transport();let backend=self.backend.clone();let tx=self.replies_tx.clone();let epoch=self.epoch;
                self.spawn(async move{let result=backend.set_embedded().await.map(|_|json!({"remote":false}));let _=tx.send(Reply{epoch,query:Query::Submitted(Submission::Remote),result,cached:false}).await;});
            }
            UiCommand::RecordVoice=>self.capture(true)?,
            UiCommand::Camera=>self.capture(false)?,
        }
        Ok(())
    }
    fn clear_transport(&mut self) {
        self.save_translation_draft();
        self.reset_translation();
        self.transport_changing = true;
        self.denied_accounts.clear();
        self.vm.form = None;
        self.vm.search_hits = Arc::new(vec![]);
        self.vm.detail = None;
        self.search_results.clear();
        self.vm.search.clear();
        self.vm.draft.clear();
        self.abort_requests();
        self.loading_chats.clear();
        self.message_events.clear();
        self.chat_events.clear();
        self.transcribing.clear();
        self.chats.clear();
        self.histories.clear();
        self.cached_pages.clear();
        self.loading.clear();
        self.downloads.clear();
        self.downloading.clear();
        self.vm.accounts.clear();
        self.vm.selected_account = None;
        self.vm.selected_chat = None;
        self.vm.chats = Arc::new(vec![]);
        self.vm.messages = Arc::new(vec![]);
    }
    fn send_message(&mut self, retry: Option<i32>) -> Result<()> {
        self.begin_send(SendPayload::Text, retry)
    }
    fn upload(&mut self, path: PathBuf) -> Result<()> {
        self.begin_send(SendPayload::File(path), None)
    }
    fn download(&mut self, id: i32, open: bool) -> Result<()> {
        let key = self.key().context("Choose a chat")?;
        if let Some(path) = self.downloads.get(&(key.clone(), id)) {
            if open {
                platform::open_file(path.clone());
            }
            return Ok(());
        }
        if !self.downloading.insert((key.clone(), id)) {
            return Ok(());
        }
        let path = format!("{}/messages/{id}/media", Self::dialog_path(&key));
        let backend = self.backend.clone();
        let tx = self.replies_tx.clone();
        let epoch = self.epoch;
        let slots = self.media_slots.clone();
        self.spawn(async move {
            let _permit = slots.acquire().await;
            let result = backend.download(&path).await.map(|p| json!(p));
            let _ = tx
                .send(Reply {
                    epoch,
                    query: Query::Downloaded(key, id, open),
                    result,
                    cached: false,
                })
                .await;
        });
        Ok(())
    }
    fn transcribe(&mut self, id: i32) -> Result<()> {
        let key = self.key().context("Choose a chat")?;
        if !self.transcribing.insert((key.clone(), id)) {
            return Ok(());
        }
        let slots = self.transcription_slots.clone();
        let local = self.settings["stt_provider"].as_str() == Some("local_whisper");
        let backend = self.backend.clone();
        let tx = self.replies_tx.clone();
        let epoch = self.epoch;
        let model = self.settings["whisper_model"]
            .as_str()
            .unwrap_or("small")
            .to_string();
        let language = self.settings["language"]
            .as_str()
            .unwrap_or("auto")
            .to_string();
        self.spawn(async move {
            let _permit = slots.acquire().await;
            let result = async {
                if local {
                    let path = backend
                        .download(&format!(
                            "{}/messages/{id}/media",
                            Controller::dialog_path(&key)
                        ))
                        .await?;
                    backend
                        .request(
                            "POST",
                            "/native/stt/transcribe",
                            json!({"filePath":path,"model":model,"language":language}),
                        )
                        .await
                } else {
                    backend
                        .request(
                            "POST",
                            "/api/v1/stt/transcribe",
                            json!({"accountId":key.account,"chatId":key.chat,"messageId":id}),
                        )
                        .await
                }
            }
            .await;
            let _ = tx
                .send(Reply {
                    epoch,
                    query: Query::Transcribed(key, id),
                    result,
                    cached: false,
                })
                .await;
        });
        Ok(())
    }
    fn capture(&mut self, voice: bool) -> Result<()> {
        if voice {
            if let Some(stop) = self.voice_stop.take() {
                let _ = stop.send(true);
                self.vm.status = "Finishing recording…".into();
                return Ok(());
            }
        }
        let (stop, rx) = watch::channel(false);
        if voice {
            self.voice_stop = Some(stop);
            self.vm.status = "Recording — press microphone again to stop".into();
        }
        let tx = self.replies_tx.clone();
        let epoch = self.epoch;
        let dir = self.data_dir.join("captures");
        self.spawn(async move {
            let result = platform::capture(voice, dir, rx).await.map(|p| json!(p));
            let _ = tx
                .send(Reply {
                    epoch,
                    query: Query::Capture,
                    result,
                    cached: false,
                })
                .await;
        });
        Ok(())
    }
    fn open_form(&mut self, kind: FormKind) {
        if matches!(
            kind,
            FormKind::TranslationSettings | FormKind::ChatTranslation
        ) {
            self.open_translation_form(kind);
            return;
        }
        let (title, description, mut fields, submission) = match kind.clone() {
            FormKind::TranslationSettings | FormKind::ChatTranslation => unreachable!(),
            FormKind::Hotkeys => ("Keyboard shortcuts","Use lowercase keys with meta+ctrl+alt+shift modifiers in that order. Escape and Enter remain reserved.",self.vm.preferences.hotkeys.iter().map(|(k,v)|field(k,&k.replace('_'," "),v,false)).chain(std::iter::once(field("reset_defaults","Reset to defaults","false",false))).collect(),Submission::Hotkeys),
            FormKind::Storage => (
                "Metadata storage",
                "Sync chat metadata, folders and tabs independently of the Telegram engine.",
                vec![
                    field("mode", "Storage mode", "local", false),
                    field("url", "Sync service URL", "https://", false),
                    field("apiKey", "API key (blank keeps existing)", "", true),
                ],
                Submission::Storage,
            ),
            FormKind::Tabs => (
                "Manage tabs",
                "Toggle visibility and set display order of folders.",
                vec![],
                Submission::Tabs,
            ),
            FormKind::Credentials => (
                "Telegram API",
                "Enter your API credentials from my.telegram.org.",
                vec![
                    field("api_id", "API ID", "", false),
                    field("api_hash", "API hash", "", true),
                ],
                Submission::Credentials,
            ),
            FormKind::Login => (
                "Add account",
                "Telegram will send a login code to your existing session or phone.",
                vec![field("phone", "Phone number (+country code)", "", false)],
                Submission::Phone,
            ),
            FormKind::Remote => (
                "Remote server",
                "Connect to your existing vasya-server using a bearer token.",
                vec![
                    field("url", "Server URL", "https://", false),
                    field("token", "Access token", "", true),
                ],
                Submission::Remote,
            ),
            FormKind::Settings => (
                "Appearance and behavior",
                "Changes apply to this application's profile.",
                vec![
                    field(
                        "dark",
                        "Dark theme (true/false)",
                        &self.vm.dark.to_string(),
                        false,
                    ),
                    field(
                        "scale",
                        "Interface scale (0.75–2)",
                        &self.vm.scale.to_string(),
                        false,
                    ),
                    field(
                        "notifications",
                        "Notifications (true/false)",
                        bool_setting(&self.settings, "notifications", true),
                        false,
                    ),
                    field(
                        "auto_transcribe",
                        "Auto-transcribe visible voice messages (true/false)",
                        bool_setting(&self.settings, "auto_transcribe", false),
                        false,
                    ),
                ],
                Submission::Settings,
            ),
            FormKind::Stt => (
                "Voice transcription",
                "Deepgram uses your key. Local Whisper requires an installed model.",
                vec![
                    field(
                        "provider",
                        "Provider: deepgram / local_whisper",
                        self.settings["stt_provider"].as_str().unwrap_or("deepgram"),
                        false,
                    ),
                    field(
                        "deepgram_api_key",
                        "Deepgram key (blank keeps existing)",
                        "",
                        true,
                    ),
                    field(
                        "whisper_model",
                        "Whisper model: tiny / base / small / medium",
                        "small",
                        false,
                    ),
                    field("language", "Language code or auto", "auto", false),
                    field(
                        "install_model",
                        "Download selected model (true/false)",
                        "false",
                        false,
                    ),
                ],
                Submission::Stt,
            ),
            FormKind::LocalApi => (
                "Local API",
                "Expose this embedded engine on localhost for agents. Token is generated locally.",
                vec![
                    field("enabled", "Enable (true/false)", "true", false),
                    field("port", "Port (0 chooses available)", "8787", false),
                ],
                Submission::LocalApi,
            ),
            FormKind::CreateGroup => (
                "Create group",
                "Select members using Telegram user IDs.",
                vec![
                    field("title", "Group name", "", false),
                    field("user_ids", "Member IDs (comma separated)", "", false),
                ],
                Submission::Group,
            ),
            FormKind::CreateChannel => (
                "Create channel",
                "Create a Telegram channel or supergroup.",
                vec![
                    field("title", "Name", "", false),
                    field("about", "Description", "", false),
                    field("is_megagroup", "Supergroup (true/false)", "false", false),
                ],
                Submission::Channel,
            ),
            FormKind::Folder => (
                "Save folder",
                "Use the same folder ID to update a folder. Empty chat types include all chats.",
                vec![
                    field("id", "Folder ID", "", false),
                    field("name", "Folder name", "", false),
                    field(
                        "included_chat_types",
                        "Types (contacts,groups,channels,bots,non_contacts)",
                        "",
                        false,
                    ),
                    field("excluded_chat_types","Excluded types (contacts,groups,channels,bots)","",false),
                    field("icon","Folder icon name","folder",false),
                    field("included_chat_ids", "Always include chat IDs", "", false),
                    field("excluded_chat_ids", "Exclude chat IDs", "", false),
                ],
                Submission::Folder,
            ),
            FormKind::Forward(id) => {
                self.forward_id = Some(id);
                (
                    "Forward message",
                    "Send a copy to a destination chat.",
                    vec![field("to_chat_id", "Destination chat ID", "", false)],
                    Submission::Forward,
                )
            }
            FormKind::SendFile => (
                "Send attachment",
                "Choose a file with Attach, or enter its full path. Review before sending.",
                vec![field("path", "File path", "", false)],
                Submission::File,
            ),
            FormKind::SearchMessages => (
                "Search messages",
                "Search the selected chat or all chats in this account.",
                vec![
                    field("query", "Search text", "", false),
                    field("all_chats", "Search all chats (true/false)", "false", false),
                ],
                Submission::Search,
            ),
        };
        if matches!(kind, FormKind::Settings) {
            let prefs = &self.vm.preferences;
            fields.extend([
                field(
                    "folder_layout",
                    "Folder layout",
                    &prefs.folder_layout,
                    false,
                ),
                field("ui_language", "Language", &prefs.language, false),
                field("density", "Chat density", &prefs.density, false),
                field(
                    "text_size",
                    "Message text size",
                    &prefs.text_size.to_string(),
                    false,
                ),
                field(
                    "markdown",
                    "Render Markdown",
                    &prefs.markdown.to_string(),
                    false,
                ),
                field(
                    "merge_messages",
                    "Group adjacent messages",
                    &prefs.merge_messages.to_string(),
                    false,
                ),
                field(
                    "notification_sound",
                    "Notification sound",
                    &prefs.notification_sound.to_string(),
                    false,
                ),
                field(
                    "message_preview",
                    "Message preview in notifications",
                    &prefs.message_preview.to_string(),
                    false,
                ),
                field(
                    "auto_photos",
                    "Download visible photos",
                    &prefs.auto_photos.to_string(),
                    false,
                ),
                field(
                    "auto_stickers",
                    "Download visible stickers",
                    &prefs.auto_stickers.to_string(),
                    false,
                ),
            ]);
        }
        if matches!(kind, FormKind::Folder) {
            if let Some(folder) = self
                .vm
                .selected_folder
                .as_ref()
                .and_then(|id| self.folders.iter().find(|f| f["id"].as_str() == Some(id)))
            {
                for f in &mut fields {
                    if let Some(value) = folder.get(&f.key) {
                        f.value = if let Some(v) = value.as_str() {
                            v.into()
                        } else {
                            arr(value)
                                .iter()
                                .map(|v| {
                                    v.as_str()
                                        .map(str::to_string)
                                        .unwrap_or_else(|| v.to_string())
                                })
                                .collect::<Vec<_>>()
                                .join(",")
                        };
                    }
                }
            }
        }
        if matches!(kind, FormKind::Stt) {
            for f in &mut fields {
                let key = match f.key.as_str() {
                    "whisper_model" => "whisper_model",
                    "language" => "language",
                    _ => continue,
                };
                if let Some(value) = self.settings[key].as_str() {
                    f.value = value.into();
                }
            }
        }
        for f in &mut fields {
            f.label = tr(&self.vm.preferences.language, &f.label);
        }
        self.vm.form = Some(Form {
            title: tr(&self.vm.preferences.language, title),
            description: tr(&self.vm.preferences.language, description),
            fields,
            submit_label: tr(&self.vm.preferences.language, "Continue"),
        });
        self.form_kind = Some(submission);
        self.vm.error = None;
        if matches!(kind, FormKind::LocalApi) {
            self.get(Query::Form(kind.clone()), "/native/local-api".into(), false);
        }
        if matches!(kind, FormKind::Storage) {
            self.get(Query::Form(kind.clone()), "/native/storage".into(), false);
        }
        if matches!(kind, FormKind::Tabs) {
            if let Ok(path) = self.account_path() {
                self.get(Query::Form(kind.clone()), format!("{path}/tabs"), false);
            }
        }
        if matches!(kind, FormKind::Stt) {
            self.get(Query::Form(kind), "/api/v1/stt/settings".into(), false);
        }
        if matches!(self.form_kind, Some(Submission::Group)) {
            if let Ok(path) = self.account_path() {
                self.get(
                    Query::Details("Contacts".into()),
                    format!("{path}/contacts"),
                    false,
                );
            }
        }
    }
    fn submit(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        if self.vm.busy {
            return Ok(());
        }
        let get = |name: &str| values.get(name).map(String::as_str).unwrap_or("").trim();
        let submission = self.form_kind.clone().context("No form is open")?;
        let query = Query::Submitted(submission.clone());
        match submission {
            Submission::TranslationSettings | Submission::ChatTranslation(_) => return self.submit_translation(values,submission),
            Submission::Hotkeys=>{let keys=if get("reset_defaults")=="true"{default_hotkeys()}else{let mut result=BTreeMap::new();let mut seen=HashSet::new();for action in default_hotkeys().keys(){let chord=get(action);if chord.is_empty()||chord.contains(' ')||chord=="enter"||chord=="escape"||!seen.insert(chord.to_string()){return Err(anyhow!("Shortcuts must be unique, nonempty and cannot use Enter or Escape"));}result.insert(action.clone(),chord.to_string());}result};self.settings["hotkeys"]=serde_json::to_value(keys)?;self.request(query,"PUT","/native/settings".into(),self.settings.clone(),false);},
            Submission::Storage=>{if !matches!(get("mode"),"local"|"remote"){return Err(anyhow!("Storage mode must be local or remote"));}let mut body=json!({"mode":get("mode"),"url":get("url")});if !get("apiKey").is_empty(){body["apiKey"]=json!(get("apiKey"));}self.request(query,"PUT","/native/storage".into(),body,false);},
            Submission::Tabs=>{let mut tabs=vec![];for folder in self.all_folders(){let id=&folder.id;tabs.push(json!({"id":id,"account_id":self.vm.selected_account,"visible":get(&format!("visible_{id}")).parse::<bool>()?,"sort_order":get(&format!("order_{id}")).parse::<i32>()?}));}self.request(query,"PUT",format!("{}/tabs",self.account_path()?),json!(tabs),false);},
            Submission::Credentials=>{let id:i32=get("api_id").parse().context("API ID must be a number")?;if id<=0||get("api_hash").len()!=32{return Err(anyhow!("Enter a positive API ID and a 32-character API hash"));}self.request(query,"PUT","/api/v1/telegram/credentials".into(),json!({"apiId":id,"apiHash":get("api_hash")}),false);},
            Submission::Phone=>{if !get("phone").starts_with('+'){return Err(anyhow!("Use international format: +country code and phone number"));}self.request(query,"POST","/api/v1/telegram/login/code".into(),json!({"phone":get("phone")}),false);},
            Submission::Code=>self.request(query,"POST","/api/v1/telegram/login/verify".into(),json!({"accountId":self.login_account,"code":get("code")}),false),
            Submission::Password=>self.request(query,"POST","/api/v1/telegram/login/password".into(),json!({"accountId":self.login_account,"password":values.get("password").cloned().unwrap_or_default()}),false),
            Submission::Remote=>{
                let url=get("url").to_string();let token=get("token").to_string();if token.is_empty(){return Err(anyhow!("Access token is required"));}
                self.epoch+=1;self.clear_transport();let backend=self.backend.clone();let tx=self.replies_tx.clone();let epoch=self.epoch;
                self.spawn(async move{let result=backend.set_remote(url,token).await.map(|_|json!({"remote":true}));let _=tx.send(Reply{epoch,query,result,cached:false}).await;});
            },
            Submission::Settings=>{let scale:f64=get("scale").parse().context("Scale must be a number")?;if !(0.75..=2.).contains(&scale){return Err(anyhow!("Scale must be between 0.75 and 2"));}let dark:bool=get("dark").parse()?;let notifications:bool=get("notifications").parse()?;let auto:bool=get("auto_transcribe").parse()?;self.settings["dark"]=json!(dark);self.settings["scale"]=json!(scale);self.settings["notifications"]=json!(notifications);self.settings["auto_transcribe"]=json!(auto);
                if !matches!(get("ui_language"),"en"|"ru")||!matches!(get("density"),"normal"|"compact"|"very-compact"){return Err(anyhow!("Choose a supported language and density"));}
                let size:f32=get("text_size").parse()?;if !(11.0..=24.0).contains(&size){return Err(anyhow!("Text size must be 11–24"));}
                self.settings["folder_layout"]=json!(get("folder_layout"));self.settings["ui_language"]=json!(get("ui_language"));self.settings["density"]=json!(get("density"));self.settings["text_size"]=json!(size);
                for name in ["markdown","merge_messages","notification_sound","message_preview","auto_photos","auto_stickers"]{self.settings[name]=json!(get(name).parse::<bool>()?);}
                self.request(query,"PUT","/native/settings".into(),self.settings.clone(),false);},
            Submission::Stt=>{
                let provider=get("provider");if !matches!(provider,"deepgram"|"local_whisper"){return Err(anyhow!("Provider must be deepgram or local_whisper"));}
                self.settings["stt_provider"]=json!(provider);self.settings["whisper_model"]=json!(get("whisper_model"));self.settings["language"]=json!(get("language"));
                self.request(Query::Ignore,"PUT","/native/settings".into(),self.settings.clone(),false);
                if provider=="deepgram" {let mut body=json!({"provider":"deepgram","whisperModel":get("whisper_model"),"language":get("language")});if !get("deepgram_api_key").is_empty(){body["deepgramApiKey"]=json!(get("deepgram_api_key"));}self.request(query,"PUT","/api/v1/stt/settings".into(),body,false);}
                else if get("install_model")=="true"{self.request(query,"POST","/native/stt/models/install".into(),json!({"model":get("whisper_model")}),false);}
                else{self.get(query,"/native/stt/models".into(),false);}
            },
            Submission::LocalApi=>{let port:u16=get("port").parse()?;let enabled:bool=get("enabled").parse()?;self.request(query,if enabled{"POST"}else{"DELETE"},"/native/local-api".into(),json!({"port":port}),false);},
            Submission::Group=>{let ids=parse_ids(get("user_ids"))?;if ids.is_empty(){return Err(anyhow!("Add at least one member ID"));}self.request(query,"POST",format!("{}/groups",self.account_path()?),json!({"title":get("title"),"userIds":ids}),false);},
            Submission::Channel=>self.request(query,"POST",format!("{}/channels",self.account_path()?),json!({"title":get("title"),"about":get("about"),"isMegagroup":get("is_megagroup").parse::<bool>()?}),false),
            Submission::Folder=>{let id=if get("id").is_empty(){format!("folder-{}",chrono::Utc::now().timestamp_millis())}else{get("id").to_string()};self.request(query,"POST",format!("{}/folders",self.account_path()?),json!({"id":id,"account_id":self.vm.selected_account,"name":get("name"),"icon":get("icon"),"included_chat_types":get("included_chat_types").split(',').map(str::trim).filter(|s|!s.is_empty()).collect::<Vec<_>>(),"excluded_chat_types":get("excluded_chat_types").split(',').map(str::trim).filter(|s|!s.is_empty()).collect::<Vec<_>>(),"included_chat_ids":parse_ids(get("included_chat_ids"))?,"excluded_chat_ids":parse_ids(get("excluded_chat_ids"))?,"sort_order":self.vm.folders.len()}),false);},
            Submission::Forward=>{let key=self.key().context("Choose a chat")?;let destination:i64=get("to_chat_id").parse()?;self.request(query,"POST",format!("{}/messages/forward",self.account_path()?),json!({"fromChatId":key.chat,"toChatId":destination,"messageIds":if self.forward_ids.is_empty(){vec![self.forward_id.context("Choose a message")?]}else{self.forward_ids.clone()}}),false);},
            Submission::File=>{self.upload(PathBuf::from(get("path")))?;self.vm.form=None;self.form_kind=None;return Ok(());},
            Submission::Search=>{let all:bool=get("all_chats").parse()?;let base=if all{self.account_path()?}else{Self::dialog_path(&self.key().context("Choose a chat")?)};self.get(query,format!("{base}/messages/search?q={}&limit=100",encode(get("query"))),false);},
        }
        self.vm.busy = true;
        self.vm.error = None;
        Ok(())
    }
    fn reply(&mut self, reply: Reply) {
        if reply.epoch != self.epoch {
            return;
        }
        if self.translation_reply(&reply) {
            return;
        }
        let query = reply.query;
        // Consume cache bookkeeping for every terminal response, including errors.
        let cached_page = match &query {
            Query::Messages(key, _, started) if !reply.cached => {
                self.cached_pages.remove(&(key.clone(), *started))
            }
            _ => None,
        };
        if let Query::Messages(key, _, _) = &query {
            if !reply.cached {
                self.loading.remove(key);
                if self.refresh_needed.remove(key) {
                    self.load_messages(key.clone(), false);
                }
            }
        }
        if !reply.cached {
            if let Query::Chats(account, _) = &query {
                self.loading_chats.remove(account);
            }
        }
        if let Query::Transcribed(key, id) = &query {
            self.transcribing.remove(&(key.clone(), *id));
        }
        let value = match reply.result {
            Ok(value) => value,
            Err(error) => {
                if let Query::LoggedOut(account) = &query {
                    self.denied_accounts.remove(account);
                    self.get(Query::Accounts, "/api/v1/accounts".into(), false);
                }
                if matches!(&query, Query::Submitted(Submission::Remote)) {
                    self.transport_changing = false;
                    self.resubscribe = true;
                    self.bootstrap();
                }
                if let Query::Downloaded(key, id, _) = &query {
                    self.downloading.remove(&(key.clone(), *id));
                }
                if matches!(&query, Query::Submitted(_)) {
                    self.vm.busy = false;
                }
                if matches!(&query, Query::Capture) {
                    self.voice_stop = None;
                }
                if !matches!(&query, Query::GlobalSearch(..) | Query::Ignore) {
                    self.vm.error = Some(error.to_string());
                }
                return;
            }
        };
        match query {
            Query::TranslationIncoming(_)
            | Query::TranslationOutgoing(_)
            | Query::Delivery(_)
            | Query::TranslationProvider(_)
            | Query::TranslationPreferences(_, _) => unreachable!(),
            Query::LoggedOut(_) => {
                self.get(Query::Accounts, "/api/v1/accounts".into(), false);
            }
            Query::Accounts => {
                if value.is_array() {
                    self.vm.accounts = arr(&value)
                        .iter()
                        .filter(|a| !self.denied_accounts.contains(&string(a, "accountId")))
                        .map(|a| AccountView {
                            id: string(a, "accountId"),
                            title: nonempty(string(a, "phone"), string(a, "accountId")),
                        })
                        .collect();
                } else {
                    self.get(Query::Accounts, "/api/v1/accounts".into(), false);
                }
                self.vm.ready = true;
                self.vm.status = if self.vm.accounts.is_empty() {
                    "Add a Telegram account to get started".into()
                } else {
                    "Connected".into()
                };
                if self.vm.selected_account.is_none() {
                    if let Some(account) = self.vm.accounts.first() {
                        let _ = self.command(UiCommand::SelectAccount(account.id.clone()));
                    }
                }
            }
            Query::Credentials => {
                if value["configured"].as_bool() == Some(false) && self.vm.form.is_none() {
                    self.open_form(FormKind::Credentials);
                }
            }
            Query::Settings => {
                self.settings = value;
                self.apply_settings();
            }
            Query::Connection => {
                self.vm.remote = value["remote"].as_bool().unwrap_or(false);
                self.translation.scope = if self.vm.remote {
                    format!("remote:{}", string(&value, "baseUrl").trim_end_matches('/'))
                } else {
                    "embedded".into()
                };
            }
            Query::Chats(account, started) => {
                let mut chats = arr(&value).iter().map(chat_view).collect::<Vec<_>>();
                if reply.cached && self.chats.contains_key(&account) {
                    return;
                }
                if let Some(existing) = self.chats.get(&account) {
                    for c in existing {
                        if self
                            .chat_events
                            .get(&(account.clone(), c.id))
                            .copied()
                            .unwrap_or(0)
                            > started
                        {
                            chats.retain(|v| v.id != c.id);
                            chats.insert(0, c.clone());
                        }
                    }
                }
                self.chats.insert(account.clone(), chats);
                if benchmark::enabled() && self.vm.selected_chat.is_none() {
                    let _ = self.command(UiCommand::SelectChat(1));
                }
                if self.vm.selected_account.as_ref() == Some(&account) {
                    self.visible_chats();
                    self.vm.status = if reply.cached {
                        "Cached chats — refreshing…".into()
                    } else {
                        "Connected".into()
                    };
                }
            }
            Query::Tabs(account) => {
                if self.vm.selected_account.as_ref() == Some(&account) {
                    self.tabs = arr(&value).to_vec();
                    self.rebuild_folders();
                }
            }
            Query::Folders(account) => {
                if self.vm.selected_account.as_ref() == Some(&account) {
                    self.folders = arr(&value).to_vec();
                    self.rebuild_folders();
                    self.get(
                        Query::Tabs(account.clone()),
                        format!("/api/v1/accounts/{}/tabs", encode(&account)),
                        false,
                    );
                    self.visible_chats();
                }
            }
            Query::Topics(key) => {
                if self.key().as_ref() == Some(&key) {
                    self.vm.topics = arr(&value)
                        .iter()
                        .map(|v| TopicView {
                            id: v["id"].as_i64().unwrap_or_default() as i32,
                            title: string(v, "title"),
                        })
                        .collect();
                }
            }
            Query::Jump(key, id) => {
                if self.key().as_ref() == Some(&key) {
                    let history = self.histories.entry(key).or_default();
                    merge_messages(
                        history,
                        arr(&value).iter().map(message_view).collect(),
                        false,
                    );
                    self.show_history();
                    self.vm.jump_to = Some(id);
                }
            }
            Query::Messages(key, older, started) => {
                let count = arr(&value).len();
                let incoming = arr(&value)
                    .iter()
                    .map(message_view)
                    .filter(|m| {
                        self.message_events
                            .get(&(key.account.clone(), key.chat, m.id))
                            .copied()
                            .unwrap_or(0)
                            <= if reply.cached { 0 } else { started }
                            && self
                                .message_events
                                .get(&(key.account.clone(), 0, m.id))
                                .copied()
                                .unwrap_or(0)
                                <= if reply.cached { 0 } else { started }
                    })
                    .collect::<Vec<_>>();
                if reply.cached {
                    self.cached_pages.insert(
                        (key.clone(), started),
                        incoming.iter().filter(|m| m.id > 0).map(|m| m.id).collect(),
                    );
                }
                let history = self.histories.entry(key.clone()).or_default();
                if let Some(cached_ids) = cached_page {
                    let live_ids: HashSet<i32> = arr(&value)
                        .iter()
                        .filter_map(|m| m["id"].as_i64().map(|id| id as i32))
                        .collect();
                    history.retain(|message| {
                        !cached_ids.contains(&message.id)
                            || live_ids.contains(&message.id)
                            || self
                                .message_events
                                .get(&(key.account.clone(), key.chat, message.id))
                                .copied()
                                .unwrap_or(0)
                                > started
                            || self
                                .message_events
                                .get(&(key.account.clone(), 0, message.id))
                                .copied()
                                .unwrap_or(0)
                                > started
                    });
                }
                // Older pagination advances the window toward older IDs. Latest
                // refreshes advance toward newer IDs; neither evicts pending sends.
                merge_messages_window(history, incoming, reply.cached, older);
                self.touch_history(&key);
                if self.key().as_ref() == Some(&key) {
                    self.vm.has_older = count >= 50;
                    self.show_history();
                    if !reply.cached && !older {
                        if let Some(id) = self
                            .histories
                            .get(&key)
                            .into_iter()
                            .flatten()
                            .filter(|m| m.id > 0)
                            .map(|m| m.id)
                            .max()
                        {
                            self.request(
                                Query::Ignore,
                                "POST",
                                format!("{}/read", Self::dialog_path(&key)),
                                json!({"maxId":id}),
                                false,
                            );
                        }
                    }
                }
            }
            Query::Form(FormKind::LocalApi) => {
                if matches!(self.form_kind, Some(Submission::LocalApi)) {
                    if let Some(form) = self.vm.form.as_mut() {
                        for field in &mut form.fields {
                            match field.key.as_str() {
                                "enabled" => {
                                    field.value =
                                        value["running"].as_bool().unwrap_or(false).to_string()
                                }
                                "port" => {
                                    if let Some(address) = value["address"].as_str() {
                                        if let Some(port) = address.rsplit(':').next() {
                                            field.value = port.to_string();
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let Some(error) = value["error"].as_str() {
                            self.vm.error = Some(error.into());
                        }
                    }
                }
            }
            Query::Form(FormKind::Storage) => {
                if matches!(self.form_kind, Some(Submission::Storage)) {
                    if let Some(form) = self.vm.form.as_mut() {
                        for field in &mut form.fields {
                            if field.key != "apiKey" {
                                if let Some(v) = value[&field.key].as_str() {
                                    field.value = v.into();
                                }
                            }
                        }
                    }
                }
            }
            Query::Form(FormKind::Tabs) => {
                let all_folders = self.all_folders();
                if matches!(self.form_kind, Some(Submission::Tabs)) {
                    if let Some(form) = self.vm.form.as_mut() {
                        form.fields.clear();
                        for (index, folder) in all_folders.iter().enumerate() {
                            let tab = arr(&value)
                                .iter()
                                .find(|v| v["id"].as_str() == Some(&folder.id));
                            form.fields.push(field(
                                &format!("visible_{}", folder.id),
                                &folder.title,
                                if tab.and_then(|t| t["visible"].as_bool()).unwrap_or(true) {
                                    "true"
                                } else {
                                    "false"
                                },
                                false,
                            ));
                            form.fields.push(field(
                                &format!("order_{}", folder.id),
                                &format!("{} · order", folder.title),
                                &tab.and_then(|t| t["sort_order"].as_i64())
                                    .unwrap_or(index as i64)
                                    .to_string(),
                                false,
                            ));
                        }
                    }
                }
            }
            Query::Form(kind) => {
                if matches!(kind, FormKind::Stt) && matches!(self.form_kind, Some(Submission::Stt))
                {
                    if let Some(form) = self.vm.form.as_mut() {
                        for field in &mut form.fields {
                            let key = match field.key.as_str() {
                                "provider" => "provider",
                                "whisper_model" => "whisperModel",
                                "language" => "language",
                                _ => continue,
                            };
                            if let Some(v) = value[key].as_str() {
                                field.value = v.into();
                            }
                        }
                    }
                }
            }
            Query::Submitted(submission) => {
                self.vm.busy = false;
                match submission {
                    Submission::Phone => {
                        self.login_account = Some(string(&value, "accountId"));
                        self.vm.form = Some(Form {
                            title: "Login code".into(),
                            description: "Enter the code Telegram sent to your session or phone."
                                .into(),
                            fields: vec![field("code", "Code", "", false)],
                            submit_label: "Verify".into(),
                        });
                        self.form_kind = Some(Submission::Code);
                    }
                    Submission::Code if value["status"].as_str() == Some("password_required") => {
                        self.vm.form = Some(Form {
                            title: "Two-step verification".into(),
                            description: "Enter your Telegram password.".into(),
                            fields: vec![field("password", "Password", "", true)],
                            submit_label: "Sign in".into(),
                        });
                        self.form_kind = Some(Submission::Password);
                    }
                    Submission::Credentials => {
                        self.vm.form = None;
                        self.open_form(FormKind::Login);
                    }
                    Submission::Search => {
                        self.vm.form = None;
                        self.vm.search_hits = Arc::new(
                            arr(&value)
                                .iter()
                                .map(|v| SearchHitView {
                                    chat_id: v["chatId"]
                                        .as_i64()
                                        .or(v["chat_id"].as_i64())
                                        .or(self.vm.selected_chat)
                                        .unwrap_or_default(),
                                    message_id: v["messageId"]
                                        .as_i64()
                                        .or(v["id"].as_i64())
                                        .unwrap_or_default()
                                        as i32,
                                    title: nonempty(
                                        string(v, "chatTitle"),
                                        self.vm.chat_title.clone(),
                                    ),
                                    text: string(v, "text"),
                                })
                                .collect(),
                        );
                        self.vm.detail = Some((
                            tr(&self.vm.preferences.language, "Search results"),
                            if self.vm.search_hits.is_empty() {
                                "No messages found".into()
                            } else {
                                "Select a result to open the message".into()
                            },
                        ));
                    }
                    Submission::LocalApi => {
                        self.vm.form = None;
                        self.vm.detail = Some((
                            "Local API".into(),
                            serde_json::to_string_pretty(&value).unwrap_or_default(),
                        ));
                    }
                    Submission::Remote => {
                        self.transport_changing = false;
                        self.resubscribe = true;
                        self.vm.remote = value["remote"].as_bool().unwrap_or(false);
                        self.vm.form = None;
                        self.bootstrap();
                    }
                    Submission::Settings | Submission::Hotkeys => {
                        self.vm.form = None;
                        self.apply_settings();
                    }
                    Submission::Stt => {
                        self.vm.form = None;
                        self.vm.status = "Transcription settings saved".into();
                    }
                    _ => {
                        self.vm.form = None;
                        self.form_kind = None;
                        self.refresh();
                    }
                }
            }
            Query::Downloaded(key, id, open) => {
                self.downloading.remove(&(key.clone(), id));
                if let Some(path) = value.as_str() {
                    let path = PathBuf::from(path);
                    self.downloads.insert((key.clone(), id), path.clone());
                    if open {
                        platform::open_file(path.clone());
                    }
                    if let Some(history) = self.histories.get_mut(&key) {
                        if let Some(m) = history.iter_mut().find(|m| m.id == id) {
                            if matches!(m.media_kind.as_deref(), Some("photo" | "sticker")) {
                                m.image_path = Some(path);
                            }
                        }
                    }
                    if self.key().as_ref() == Some(&key) {
                        self.show_history();
                    }
                    while self.downloads.len() > 512 {
                        if let Some(k) = self
                            .downloads
                            .keys()
                            .find(|(k, _)| Some(k) != self.key().as_ref())
                            .cloned()
                        {
                            self.downloads.remove(&k);
                        } else {
                            break;
                        }
                    }
                }
            }
            Query::Transcribed(key, id) => {
                if let Some(history) = self.histories.get_mut(&key) {
                    if let Some(m) = history.iter_mut().find(|m| m.id == id) {
                        m.transcription = Some(string(&value, "text"));
                    }
                }
                if self.key().as_ref() == Some(&key) {
                    self.show_history();
                }
            }
            Query::Details(title) => {
                self.vm.detail = Some((
                    title,
                    serde_json::to_string_pretty(&value).unwrap_or_default(),
                ))
            }
            Query::GlobalSearch(account, generation) => {
                if generation == self.generation
                    && self.vm.selected_account.as_ref() == Some(&account)
                {
                    self.search_results = arr(&value).iter().map(chat_view).collect();
                    self.visible_chats();
                }
            }
            Query::Capture => {
                self.voice_stop = None;
                self.vm.status = "Capture ready — review attachment before sending".into();
                self.open_form(FormKind::SendFile);
                if let Some(form) = self.vm.form.as_mut() {
                    form.fields[0].value = value.as_str().unwrap_or_default().into();
                }
            }
            Query::Ignore => {}
        }
    }
    fn all_folders(&self) -> Vec<FolderView> {
        let mut folders = vec![
            FolderView {
                id: "contacts".into(),
                title: tr(&self.vm.preferences.language, "Contacts"),
            },
            FolderView {
                id: "chats".into(),
                title: tr(&self.vm.preferences.language, "Groups and channels"),
            },
            FolderView {
                id: "favorites".into(),
                title: tr(&self.vm.preferences.language, "Favorites"),
            },
        ];
        folders.extend(self.folders.iter().map(|f| FolderView {
            id: string(f, "id"),
            title: string(f, "name"),
        }));
        folders
    }
    fn rebuild_folders(&mut self) {
        let mut folders: Vec<_> = self
            .all_folders()
            .into_iter()
            .enumerate()
            .filter_map(|(index, f)| {
                let tab = self.tabs.iter().find(|t| t["id"].as_str() == Some(&f.id));
                if !tab
                    .and_then(|t| t["visible"].as_bool())
                    .unwrap_or(f.id != "chats")
                {
                    return None;
                }
                Some((
                    tab.and_then(|t| t["sort_order"].as_i64())
                        .unwrap_or(index as i64),
                    f,
                ))
            })
            .collect();
        folders.sort_by_key(|(order, _)| *order);
        self.vm.folders = folders.into_iter().map(|(_, f)| f).collect();
        self.visible_chats();
    }
    fn apply_settings(&mut self) {
        let p = &mut self.vm.preferences;
        p.folder_layout = self.settings["folder_layout"]
            .as_str()
            .unwrap_or("horizontal")
            .into();
        p.hotkeys = serde_json::from_value(self.settings["hotkeys"].clone())
            .unwrap_or_else(|_| default_hotkeys());
        p.language = self.settings["ui_language"].as_str().unwrap_or("en").into();
        p.density = self.settings["density"].as_str().unwrap_or("normal").into();
        p.text_size = self.settings["text_size"].as_f64().unwrap_or(14.) as f32;
        p.markdown = self.settings["markdown"].as_bool().unwrap_or(false);
        p.merge_messages = self.settings["merge_messages"].as_bool().unwrap_or(true);
        p.notification_sound = self.settings["notification_sound"]
            .as_bool()
            .unwrap_or(true);
        p.message_preview = self.settings["message_preview"].as_bool().unwrap_or(true);
        p.auto_photos = self.settings["auto_photos"].as_bool().unwrap_or(true);
        p.auto_stickers = self.settings["auto_stickers"].as_bool().unwrap_or(true);

        self.vm.dark = self.settings["dark"].as_bool().unwrap_or(true);
        self.vm.scale = self.settings["scale"]
            .as_f64()
            .filter(|v| (0.75..=2.).contains(v))
            .unwrap_or(1.);
    }
    fn touch_history(&mut self, key: &DialogKey) {
        self.history_lru.retain(|k| k != key);
        self.history_lru.push_back(key.clone());
        while self.history_lru.len() > 16 {
            if let Some(old) = self.history_lru.pop_front() {
                if self.key().as_ref() != Some(&old) {
                    self.histories.remove(&old);
                }
            }
        }
    }
    fn event(&mut self, event: Event) {
        if self.transport_changing {
            return;
        }
        let v = event.payload;
        if event.name == "native:resync" {
            self.refresh();
            return;
        }
        let account = string(&v, "accountId");
        if self.transport_changing || self.denied_accounts.contains(&account) {
            return;
        }
        if account.is_empty() {
            return;
        }
        self.event_sequence += 1;
        let event_chat = v["chatId"].as_i64().unwrap_or_default();
        if event.name == "telegram:new-message" || event.name == "telegram:message-edited" {
            self.message_events.insert(
                (
                    account.clone(),
                    event_chat,
                    v["id"].as_i64().unwrap_or_default() as i32,
                ),
                self.event_sequence,
            );
            self.chat_events
                .insert((account.clone(), event_chat), self.event_sequence);
        }
        if event.name == "telegram:message-deleted" {
            for id in arr(&v["messageIds"]) {
                if let Some(id) = id.as_i64() {
                    self.message_events.insert(
                        (account.clone(), event_chat, id as i32),
                        self.event_sequence,
                    );
                }
            }
        }
        if self.message_events.len() > 20000 {
            let minimum = self.event_sequence.saturating_sub(10000);
            self.message_events
                .retain(|_, sequence| *sequence >= minimum);
        }
        match event.name.as_str(){
            "chat-loaded"=>{let chat=chat_view(&v);let items=self.chats.entry(account.clone()).or_default();if let Some(old)=items.iter_mut().find(|c|c.id==chat.id){*old=chat;}else{items.push(chat);}if self.vm.selected_account.as_ref()==Some(&account){self.visible_chats();}},
            "telegram:new-message"=>{
                let chat=v["chatId"].as_i64().unwrap_or_default();let message=message_view(&v);
                let selected=self.vm.selected_account.as_ref()==Some(&account)&&self.vm.selected_chat==Some(chat);
                // Events lack reliable topic IDs: refresh the selected topic instead of mixing replies.
                if selected&&self.vm.selected_topic.is_some(){if let Some(key)=self.key(){self.load_messages(key,false);}}
                let key=DialogKey{account:account.clone(),chat,topic:None};
                if let Some(history)=self.histories.get_mut(&key){merge_messages(history,vec![message.clone()],false);}
                let items=self.chats.entry(account.clone()).or_default();
                if let Some(index)=items.iter().position(|c|c.id==chat){let mut item=items.remove(index);item.preview=message.text.clone();if !selected&&!message.outgoing{item.unread+=1;}items.insert(0,item);}else{self.load_chats(account.clone());}
                if self.vm.selected_account.as_ref()==Some(&account){self.visible_chats();if selected{self.show_history();}}
                if !selected&&!message.outgoing&&self.settings["notifications"].as_bool().unwrap_or(true){platform::notify(message.sender,message.text,self.vm.preferences.notification_sound,self.vm.preferences.message_preview);}
            }
            "telegram:message-edited"=>{let chat=v["chatId"].as_i64().unwrap_or_default();let id=v["id"].as_i64().unwrap_or_default()as i32;for(k,items)in &mut self.histories{if k.account==account&&k.chat==chat{if let Some(m)=items.iter_mut().find(|m|m.id==id){m.text=string(&v,"newText");}}}self.show_history();},
            "telegram:message-deleted"=>{let chat=v["chatId"].as_i64().unwrap_or_default();let ids:HashSet<i32>=arr(&v["messageIds"]).iter().filter_map(|v|v.as_i64().map(|i|i as i32)).collect();for(k,items)in &mut self.histories{if k.account==account&&(chat==0||k.chat==chat){items.retain(|m|!ids.contains(&m.id));}}self.show_history();},
            "connection-status"|"telegram:connection-status"=>if self.vm.selected_account.as_ref()==Some(&account){self.vm.status=string(&v,"status");},
            "telegram:incoming-call"=>self.vm.detail=Some(("Incoming call".into(),"Audio calls are not supported by this engine yet. Use the official Telegram client to answer.".into())),
            _=>{},
        }
    }
}
fn field(key: &str, label: &str, value: &str, secret: bool) -> FormField {
    FormField {
        key: key.into(),
        label: label.into(),
        value: value.into(),
        secret,
        multiline: false,
    }
}
fn bool_setting<'a>(v: &Value, key: &str, default: bool) -> &'a str {
    if v[key].as_bool().unwrap_or(default) {
        "true"
    } else {
        "false"
    }
}
fn parse_ids(input: &str) -> Result<Vec<i64>> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(Into::into))
        .collect()
}
fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn string(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}
fn nonempty(first: String, second: String) -> String {
    if first.is_empty() {
        second
    } else {
        first
    }
}
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn chat_view(v: &Value) -> ChatView {
    ChatView {
        id: v["id"].as_i64().unwrap_or_default(),
        title: string(v, "title"),
        preview: string(v, "lastMessage"),
        unread: v["unreadCount"].as_i64().unwrap_or_default() as i32,
        is_forum: v["isForum"].as_bool().unwrap_or(false),
        chat_type: nonempty(string(v, "chatType"), string(v, "resultType")),
        username: string(v, "username"),
    }
}
fn message_view(v: &Value) -> MessageView {
    let media = arr(&v["media"]).first();
    let kind = media
        .and_then(|m| m["media_type"].as_str().or(m["mediaType"].as_str()))
        .or(v["mediaType"].as_str())
        .map(str::to_string);
    let mut text = string(v, "text");
    if let Some(m) = media {
        if kind.as_deref() == Some("webpage") {
            for key in ["webpage_title", "webpage_description", "webpage_url"] {
                if let Some(s) = m[key].as_str() {
                    text.push_str(&format!("\n{s}"));
                }
            }
        }
    }
    MessageView {
        id: v["id"].as_i64().unwrap_or_default() as i32,
        sender: nonempty(string(v, "sender_name"), string(v, "senderName")),
        text,
        time: chrono::DateTime::from_timestamp(v["date"].as_i64().unwrap_or_default(), 0)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%d %b · %H:%M")
                    .to_string()
            })
            .unwrap_or_default(),
        outgoing: v["is_outgoing"]
            .as_bool()
            .or(v["isOutgoing"].as_bool())
            .unwrap_or(false),
        media_kind: kind,
        ..Default::default()
    }
}
fn merge_messages(history: &mut Vec<MessageView>, incoming: Vec<MessageView>, cached: bool) {
    merge_messages_window(history, incoming, cached, false);
}

fn merge_messages_window(
    history: &mut Vec<MessageView>,
    incoming: Vec<MessageView>,
    cached: bool,
    older: bool,
) {
    for mut message in incoming {
        if let Some(index) = history.iter().position(|v| v.id == message.id) {
            if cached {
                continue;
            }
            message.image_path = history[index].image_path.clone();
            message.transcription = history[index].transcription.clone();
            history[index] = message;
        } else {
            history.push(message);
        }
    }
    history.sort_by_key(|m| if m.id > 0 { (0, m.id) } else { (1, -m.id) });
    const CONFIRMED_HISTORY_LIMIT: usize = 5000;
    let confirmed = history.iter().filter(|m| m.id > 0).count();
    if confirmed > CONFIRMED_HISTORY_LIMIT {
        let excess = confirmed - CONFIRMED_HISTORY_LIMIT;
        let mut index = 0;
        history.retain(|message| {
            if message.id <= 0 {
                return true;
            }
            let retain = if older {
                index < CONFIRMED_HISTORY_LIMIT
            } else {
                index >= excess
            };
            index += 1;
            retain
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_cannot_overwrite_live_message() {
        let mut items = vec![MessageView {
            id: 1,
            text: "live".into(),
            ..Default::default()
        }];
        merge_messages(
            &mut items,
            vec![MessageView {
                id: 1,
                text: "stale".into(),
                ..Default::default()
            }],
            true,
        );
        assert_eq!(items[0].text, "live");
    }
    #[test]
    fn merge_deduplicates_and_retains_media() {
        let mut items = vec![MessageView {
            id: 2,
            image_path: Some("image.png".into()),
            ..Default::default()
        }];
        merge_messages(
            &mut items,
            vec![
                MessageView {
                    id: 2,
                    text: "edit".into(),
                    ..Default::default()
                },
                MessageView {
                    id: 1,
                    ..Default::default()
                },
            ],
            false,
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 1);
        assert_eq!(items[1].image_path, Some("image.png".into()));
    }
    #[test]
    fn account_topic_keys_are_isolated() {
        let a = DialogKey {
            account: "a".into(),
            chat: 5,
            topic: None,
        };
        let b = DialogKey {
            account: "b".into(),
            ..a.clone()
        };
        let c = DialogKey {
            topic: Some(4),
            ..a.clone()
        };
        let map = HashMap::from([(a, 1), (b, 2), (c, 3)]);
        assert_eq!(map.len(), 3);
    }
    #[test]
    fn url_encoding_preserves_unicode_and_separators() {
        assert_eq!(encode("a/b?c d"), "a%2Fb%3Fc%20d");
        assert_eq!(encode("т"), "%D1%82");
    }
    #[test]
    fn event_and_rest_messages_map_identically() {
        let a = message_view(
            &json!({"id":1,"sender_name":"A","is_outgoing":true,"media":[{"media_type":"voice"}]}),
        );
        let b = message_view(
            &json!({"id":1,"senderName":"A","isOutgoing":true,"media":[{"mediaType":"voice"}]}),
        );
        assert_eq!(a.sender, b.sender);
        assert_eq!(a.outgoing, b.outgoing);
        assert_eq!(a.media_kind, b.media_kind);
    }
}

#[cfg(test)]
mod controller_tests;
