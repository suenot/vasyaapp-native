//! Native translation state: scoped preferences, bounded viewport work and a
//! two-stage outgoing pipeline. Original messages and drafts stay authoritative.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct ChatPreferences {
    pub incoming_enabled: bool,
    pub outgoing_enabled: bool,
    pub incoming_target: String,
    pub outgoing_target: String,
}
impl Default for ChatPreferences {
    fn default() -> Self {
        Self {
            incoming_enabled: false,
            outgoing_enabled: false,
            incoming_target: "ru".into(),
            outgoing_target: "zh".into(),
        }
    }
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) struct IncomingKey {
    pub dialog: DialogKey,
    pub id: i32,
    pub text: String,
    pub target: String,
    pub generation: u64,
}
#[derive(Clone, Debug)]
pub(super) enum SendPayload {
    Text,
    File(PathBuf),
    Clipboard(Arc<Vec<u8>>, String),
}
#[derive(Clone, Debug)]
pub(super) struct SendIntent {
    pub key: DialogKey,
    pub id: i32,
    pub original: String,
    pub draft: String,
    pub revision: u64,
    pub generation: u64,
    pub payload: SendPayload,
    pub translating: bool,
    pub scope: String,
}
#[derive(Clone, Debug, Default)]
struct Draft {
    text: String,
    revision: u64,
    error: Option<String>,
}
pub(super) struct TranslationState {
    pub scope: String,
    pub generation: u64,
    pub visible_dialog: Option<DialogKey>,
    pub visible: Vec<i32>,
    active: HashSet<IncomingKey>,
    incoming_tasks: HashMap<IncomingKey, tokio::task::AbortHandle>,
    cache: HashMap<IncomingKey, std::result::Result<String, String>>,
    lru: VecDeque<IncomingKey>,
    originals: HashSet<IncomingKey>,
    drafts: HashMap<(String, DialogKey), Draft>,
    pub draft_revision: u64,
    pub(super) sending: HashMap<DialogKey, SendIntent>,
    failed: HashMap<(DialogKey, i32), SendIntent>,
    form_sequence: u64,
}
impl Default for TranslationState {
    fn default() -> Self {
        Self {
            scope: "embedded".into(),
            generation: 0,
            visible_dialog: None,
            visible: vec![],
            active: HashSet::new(),
            incoming_tasks: HashMap::new(),
            cache: HashMap::new(),
            lru: VecDeque::new(),
            originals: HashSet::new(),
            drafts: HashMap::new(),
            draft_revision: 0,
            sending: HashMap::new(),
            failed: HashMap::new(),
            form_sequence: 0,
        }
    }
}
impl Controller {
    fn translation_preference_key(&self, key: &DialogKey) -> String {
        // Topics inherit their parent chat's language settings.
        json!([self.translation.scope, key.account, key.chat]).to_string()
    }
    pub(super) fn translation_preferences(&self, key: &DialogKey) -> ChatPreferences {
        serde_json::from_value(
            self.settings["chat_translation"][self.translation_preference_key(key)].clone(),
        )
        .unwrap_or_default()
    }
    pub(super) fn save_translation_draft(&mut self) {
        if let Some(key) = self.key() {
            let entry = self
                .translation
                .drafts
                .entry((self.translation.scope.clone(), key))
                .or_default();
            if entry.text != self.vm.draft {
                entry.error = None;
            }
            entry.text = self.vm.draft.clone();
            entry.revision = self.translation.draft_revision;
        }
    }
    pub(super) fn restore_translation_draft(&mut self) {
        let draft = self
            .key()
            .and_then(|key| {
                self.translation
                    .drafts
                    .get(&(self.translation.scope.clone(), key))
            })
            .cloned()
            .unwrap_or_default();
        self.vm.draft = draft.text;
        self.vm.error = draft.error;
        // Revision belongs to this dialog, not to another chat edited meanwhile.
        self.translation.draft_revision = draft.revision;
    }
    pub(super) fn reset_translation(&mut self) {
        self.translation.generation += 1;
        for (_, task) in self.translation.incoming_tasks.drain() {
            task.abort();
        }
        self.translation.cache.clear();
        self.translation.lru.clear();
        self.translation.originals.clear();
        self.translation.active.clear();
        self.translation.visible.clear();
        self.translation.visible_dialog = None;
        self.translation.sending.clear();
        self.translation.failed.clear();
        self.vm.outgoing_translation_pending = false;
        self.vm.outgoing_translation_target = None;
    }
    fn incoming_key(&self, dialog: &DialogKey, message: &MessageView) -> Option<IncomingKey> {
        let preferences = self.translation_preferences(dialog);
        if !preferences.incoming_enabled
            || message.outgoing
            || message.pending
            || message.text.trim().is_empty()
        {
            return None;
        }
        Some(IncomingKey {
            dialog: dialog.clone(),
            id: message.id,
            text: message.text.clone(),
            target: preferences.incoming_target,
            generation: self.translation.generation,
        })
    }
    fn incoming_current(&self, key: &IncomingKey) -> bool {
        key.generation == self.translation.generation
            && !self.denied_accounts.contains(&key.dialog.account)
            && self
                .histories
                .get(&key.dialog)
                .and_then(|messages| messages.iter().find(|m| m.id == key.id))
                .and_then(|m| self.incoming_key(&key.dialog, m))
                .as_ref()
                == Some(key)
    }
    pub(super) fn pump_translations(&mut self) {
        let current = self.key();
        let visible: HashSet<_> = self.translation.visible.iter().copied().collect();
        let obsolete: Vec<_> = self
            .translation
            .active
            .iter()
            .filter(|key| {
                Some(&key.dialog) != current.as_ref()
                    || self.translation.visible_dialog.as_ref() != current.as_ref()
                    || !visible.contains(&key.id)
                    || !self.incoming_current(key)
            })
            .cloned()
            .collect();
        for key in obsolete {
            self.translation.active.remove(&key);
            if let Some(task) = self.translation.incoming_tasks.remove(&key) {
                task.abort();
            }
        }
        let Some(dialog) = current else {
            return;
        };
        if self.translation.visible_dialog.as_ref() != Some(&dialog) {
            return;
        }
        let preferences = self.translation_preferences(&dialog);
        if !preferences.incoming_enabled {
            return;
        }
        let candidates: Vec<_> = self
            .histories
            .get(&dialog)
            .into_iter()
            .flatten()
            .filter(|message| {
                visible.contains(&message.id)
                    && !message.outgoing
                    && !message.pending
                    && !message.text.trim().is_empty()
            })
            .map(|message| IncomingKey {
                dialog: dialog.clone(),
                id: message.id,
                text: message.text.clone(),
                target: preferences.incoming_target.clone(),
                generation: self.translation.generation,
            })
            .collect();
        for key in candidates {
            if self.translation.active.len() >= 2 {
                break;
            }
            if self.translation.cache.contains_key(&key) || self.translation.active.contains(&key) {
                continue;
            }
            self.translation.active.insert(key.clone());
            let backend = self.backend.clone();
            let tx = self.replies_tx.clone();
            let epoch = self.epoch;
            let request_key = key.clone();
            let task = self.spawn(async move {
                let result = backend
                    .request(
                        "POST",
                        "/api/v1/translation/translate",
                        json!({"text":request_key.text,"targetLanguage":request_key.target}),
                    )
                    .await;
                let _ = tx
                    .send(Reply {
                        epoch,
                        query: Query::TranslationIncoming(request_key),
                        result,
                        cached: false,
                    })
                    .await;
            });
            self.translation
                .incoming_tasks
                .insert(key, task.abort_handle());
        }
    }
    pub(super) fn decorate_translation(&mut self) {
        let Some(dialog) = self.key() else {
            self.vm.outgoing_translation_target = None;
            self.vm.outgoing_translation_pending = false;
            return;
        };
        let preferences = self.translation_preferences(&dialog);
        self.vm.outgoing_translation_target = preferences
            .outgoing_enabled
            .then_some(preferences.outgoing_target);
        self.vm.outgoing_translation_pending = self
            .translation
            .sending
            .get(&dialog)
            .is_some_and(|s| s.translating);
        // Cache indexes avoid cloning or reparsing every message on background ticks.
        let cached: HashMap<_, _> = self
            .translation
            .cache
            .iter()
            .filter(|(key, _)| {
                preferences.incoming_enabled
                    && key.dialog == dialog
                    && key.generation == self.translation.generation
                    && key.target == preferences.incoming_target
            })
            .map(|(key, result)| ((key.id, key.text.as_str()), (key, result)))
            .collect();
        let active: HashMap<_, _> = self
            .translation
            .active
            .iter()
            .filter(|key| {
                preferences.incoming_enabled
                    && key.dialog == dialog
                    && key.generation == self.translation.generation
            })
            .map(|key| (key.id, key))
            .collect();
        let presentation = |message: &MessageView| {
            let entry = cached
                .get(&(message.id, message.text.as_str()))
                .filter(|_| !message.outgoing);
            (
                entry.and_then(|(_, r)| r.as_ref().ok()).map(String::as_str),
                entry
                    .and_then(|(_, r)| r.as_ref().err())
                    .map(String::as_str),
                active
                    .get(&message.id)
                    .is_some_and(|key| key.text == message.text),
                entry.is_some_and(|(key, _)| self.translation.originals.contains(key)),
            )
        };
        if self.vm.messages.iter().any(|m| {
            let (text, error, pending, original) = presentation(m);
            m.translation.as_deref() != text
                || m.translation_error.as_deref() != error
                || m.translation_pending != pending
                || m.translation_show_original != original
        }) {
            for m in Arc::make_mut(&mut self.vm.messages).iter_mut() {
                let (text, error, pending, original) = presentation(m);
                let text = text.map(str::to_string);
                let error = error.map(str::to_string);
                m.translation = text;
                m.translation_error = error;
                m.translation_pending = pending;
                m.translation_show_original = original;
            }
        }
    }
    pub(super) fn forget_translation_account(&mut self, account: &str) {
        self.reset_translation();
        self.translation
            .drafts
            .retain(|(_, dialog), _| dialog.account != account);
    }
    pub(super) fn translation_action(&mut self, dialog: DialogKey, id: i32, retry: bool) {
        if self.key().as_ref() != Some(&dialog) {
            return;
        }
        let key = self
            .histories
            .get(&dialog)
            .and_then(|m| m.iter().find(|m| m.id == id))
            .and_then(|m| self.incoming_key(&dialog, m));
        if let Some(key) = key {
            if retry {
                self.translation.cache.remove(&key);
                self.translation.lru.retain(|k| k != &key);
            } else if !self.translation.originals.remove(&key) {
                self.translation.originals.insert(key);
            }
        }
    }
    pub(super) fn begin_send(&mut self, payload: SendPayload, retry: Option<i32>) -> Result<()> {
        let key = self.key().context("Choose a chat first")?;
        if self.translation.sending.contains_key(&key) {
            return Ok(());
        }
        if self.translation.sending.len() >= 4 {
            return Err(anyhow!("Sending is busy; wait for pending messages"));
        }
        self.save_translation_draft();
        let mut intent = if let Some(id) = retry {
            if let Some(failed) = self.translation.failed.get(&(key.clone(), id)) {
                failed.clone()
            } else {
                SendIntent {
                    key: key.clone(),
                    id,
                    original: self
                        .histories
                        .get(&key)
                        .and_then(|m| m.iter().find(|m| m.id == id))
                        .context("Message no longer available")?
                        .text
                        .clone(),
                    draft: String::new(),
                    revision: u64::MAX,
                    generation: 0,
                    payload: SendPayload::Text,
                    translating: false,
                    scope: self.translation.scope.clone(),
                }
            }
        } else {
            let id = self.pending_id;
            self.pending_id -= 1;
            SendIntent {
                key: key.clone(),
                id,
                original: self.vm.draft.trim().into(),
                draft: self.vm.draft.clone(),
                revision: self.translation.draft_revision,
                generation: 0,
                payload,
                translating: false,
                scope: self.translation.scope.clone(),
            }
        };
        if intent.original.is_empty() && matches!(intent.payload, SendPayload::Text) {
            return Ok(());
        }
        let preferences = self.translation_preferences(&key);
        intent.generation = self.translation.generation;
        intent.translating = preferences.outgoing_enabled && !intent.original.is_empty();
        self.vm.error = None;
        self.translation.sending.insert(key, intent.clone());
        if intent.translating {
            self.vm.status = "Translating before sending…".into();
            self.request(
                Query::TranslationOutgoing(Box::new(intent.clone())),
                "POST",
                "/api/v1/translation/translate".into(),
                json!({"text":intent.original,"targetLanguage":preferences.outgoing_target}),
                false,
            );
        } else {
            let original = intent.original.clone();
            self.deliver(intent, original);
        }
        Ok(())
    }
    fn deliver(&mut self, mut intent: SendIntent, text: String) {
        intent.translating = false;
        self.translation
            .sending
            .insert(intent.key.clone(), intent.clone());
        if matches!(intent.payload, SendPayload::Text) {
            let history = self.histories.entry(intent.key.clone()).or_default();
            history.retain(|m| m.id != intent.id);
            history.push(MessageView {
                id: intent.id,
                sender: "You".into(),
                text: text.clone(),
                time: chrono::Local::now().format("%H:%M").to_string(),
                outgoing: true,
                pending: true,
                ..Default::default()
            });
            if self.key().as_ref() == Some(&intent.key) {
                self.show_history();
            }
            self.request(
                Query::Delivery(Box::new(intent.clone())),
                "POST",
                format!("{}/messages", Self::dialog_path(&intent.key)),
                json!({"text":text,"topicId":intent.key.topic}),
                false,
            );
        } else {
            let backend = self.backend.clone();
            let tx = self.replies_tx.clone();
            let epoch = self.epoch;
            let directory = self.data_dir.join("captures");
            self.spawn(async move {
                let result = async {
                    let (path, temporary) = match &intent.payload {
                        SendPayload::File(path) => (path.clone(), false),
                        SendPayload::Clipboard(bytes, extension) => {
                            tokio::fs::create_dir_all(&directory).await?;
                            let path = directory.join(format!(
                                "clipboard-{}-{}.{}",
                                chrono::Utc::now().timestamp_millis(),
                                intent.id,
                                extension
                            ));
                            tokio::fs::write(&path, bytes.as_slice()).await?;
                            (path, true)
                        }
                        SendPayload::Text => unreachable!(),
                    };
                    let mut endpoint = format!(
                        "{}/media?caption={}",
                        Controller::dialog_path(&intent.key),
                        encode(&text)
                    );
                    if let Some(topic) = intent.key.topic {
                        endpoint.push_str(&format!("&topic_id={topic}"));
                    }
                    if path.starts_with(&directory)
                        && path.extension().and_then(|s| s.to_str()) == Some("m4a")
                    {
                        endpoint.push_str("&voice=true");
                    }
                    let result = backend.upload(&endpoint, path.clone()).await;
                    if temporary {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    result
                }
                .await;
                let _ = tx
                    .send(Reply {
                        epoch,
                        query: Query::Delivery(Box::new(intent)),
                        result,
                        cached: false,
                    })
                    .await;
            });
        }
    }
    fn send_failed(&mut self, intent: &SendIntent, error: String) {
        self.translation.sending.remove(&intent.key);
        let draft = self
            .translation
            .drafts
            .entry((intent.scope.clone(), intent.key.clone()))
            .or_default();
        draft.error = Some(error.clone());
        if let Some(history) = self.histories.get_mut(&intent.key) {
            if let Some(message) = history.iter_mut().find(|m| m.id == intent.id) {
                message.pending = false;
                message.failed = true;
                message.text = intent.original.clone();
                self.translation
                    .failed
                    .insert((intent.key.clone(), intent.id), intent.clone());
            }
        }
        if self.key().as_ref() == Some(&intent.key) {
            self.vm.error = Some(error);
            self.show_history();
        }
        self.vm.status = "Message was not sent".into();
    }
    pub(super) fn open_translation_form(&mut self, kind: FormKind) {
        let language = &self.vm.preferences.language;
        let (title, description, fields, submission) = if matches!(
            kind,
            FormKind::TranslationSettings
        ) {
            ("Translation settings","OpenAI-compatible API. Enabled chat directions send message text to this provider. Blank token keeps the saved key.",vec![field("base_url","API base URL","",false),field("model","Model","",false),field("api_key","API token (blank keeps existing)","",true),field("clear_key","Remove saved token","false",false)],Submission::TranslationSettings)
        } else {
            let Some(key) = self.key() else {
                self.vm.error = Some(tr(language, "Choose a chat first"));
                return;
            };
            let p = self.translation_preferences(&key);
            ("Chat translation","Incoming and outgoing translation are independent. Languages: ru, zh, en, or a language name. Configure the provider in Translation settings.",vec![field("incoming_enabled","Translate incoming messages",&p.incoming_enabled.to_string(),false),field("incoming_target","Incoming target language",&p.incoming_target,false),field("outgoing_enabled","Translate outgoing messages",&p.outgoing_enabled.to_string(),false),field("outgoing_target","Outgoing target language",&p.outgoing_target,false)],Submission::ChatTranslation(key))
        };
        self.vm.form = Some(Form {
            title: tr(language, title),
            description: tr(language, description),
            fields: fields
                .into_iter()
                .map(|mut f| {
                    f.label = tr(language, &f.label);
                    f
                })
                .collect(),
            submit_label: tr(language, "Save"),
        });
        self.form_kind = Some(submission);
        self.vm.error = None;
        if matches!(kind, FormKind::TranslationSettings) {
            self.translation.form_sequence += 1;
            self.vm.busy = true;
            self.get(
                Query::TranslationProvider(self.translation.form_sequence),
                "/api/v1/translation/settings".into(),
                false,
            );
        }
    }
    pub(super) fn submit_translation(
        &mut self,
        values: BTreeMap<String, String>,
        submission: Submission,
    ) -> Result<()> {
        let value = |name: &str| values.get(name).map(String::as_str).unwrap_or("").trim();
        match submission {
            Submission::TranslationSettings => {
                let mut body = json!({"base_url":value("base_url"),"model":value("model")});
                if value("clear_key") == "true" {
                    body["api_key"] = json!("");
                } else if !value("api_key").is_empty() {
                    body["api_key"] = json!(value("api_key"));
                }
                self.translation.generation += 1;
                self.request(
                    Query::Submitted(Submission::TranslationSettings),
                    "PUT",
                    "/api/v1/translation/settings".into(),
                    body,
                    false,
                );
            }
            Submission::ChatTranslation(key) => {
                if self.key().as_ref() != Some(&key) {
                    return Err(anyhow!("Chat changed; reopen translation settings"));
                }
                for name in ["incoming_target", "outgoing_target"] {
                    let target = value(name);
                    if target.is_empty()
                        || target.len() > 80
                        || target.chars().any(char::is_control)
                    {
                        return Err(anyhow!("Enter a target language (up to 80 characters)"));
                    }
                }
                let preferences = ChatPreferences {
                    incoming_enabled: value("incoming_enabled").parse()?,
                    outgoing_enabled: value("outgoing_enabled").parse()?,
                    incoming_target: value("incoming_target").into(),
                    outgoing_target: value("outgoing_target").into(),
                };
                let mut settings = self.settings.clone();
                if !settings["chat_translation"].is_object() {
                    settings["chat_translation"] = json!({});
                }
                settings["chat_translation"][self.translation_preference_key(&key)] =
                    serde_json::to_value(&preferences)?;
                self.request(
                    Query::TranslationPreferences(key, settings.clone()),
                    "PUT",
                    "/native/settings".into(),
                    settings,
                    false,
                );
            }
            _ => unreachable!(),
        }
        self.vm.busy = true;
        self.vm.error = None;
        Ok(())
    }
    pub(super) fn translation_reply(&mut self, reply: &Reply) -> bool {
        match &reply.query {
            Query::TranslationIncoming(key) => {
                self.translation.active.remove(key);
                self.translation.incoming_tasks.remove(key);
                if !self.incoming_current(key) {
                    return true;
                }
                let result = reply
                    .result
                    .as_ref()
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        v["text"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| "Translation returned empty text".into())
                    });
                self.translation.cache.insert(key.clone(), result);
                self.translation.lru.retain(|k| k != key);
                self.translation.lru.push_back(key.clone());
                while self.translation.cache.len() > 128
                    || self
                        .translation
                        .cache
                        .iter()
                        .map(|(k, v)| {
                            k.text.len() + v.as_ref().map_or_else(|e| e.len(), |s| s.len())
                        })
                        .sum::<usize>()
                        > 1024 * 1024
                {
                    if let Some(old) = self.translation.lru.pop_front() {
                        self.translation.cache.remove(&old);
                        self.translation.originals.remove(&old);
                    } else {
                        break;
                    }
                }
            }
            Query::TranslationOutgoing(intent) => {
                if intent.generation != self.translation.generation {
                    self.send_failed(
                        intent,
                        "Translation settings changed. Message was not sent; retry.".into(),
                    );
                    return true;
                }
                match reply
                    .result
                    .as_ref()
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        v["text"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| "Translation returned empty text".into())
                    }) {
                    Ok(text) => self.deliver(*intent.clone(), text),
                    Err(error) => self.send_failed(intent, error),
                }
            }
            Query::Delivery(intent) => match &reply.result {
                Err(error) => self.send_failed(intent, error.to_string()),
                Ok(value) => {
                    self.translation.sending.remove(&intent.key);
                    self.translation
                        .failed
                        .remove(&(intent.key.clone(), intent.id));
                    let history = self.histories.entry(intent.key.clone()).or_default();
                    history.retain(|m| m.id != intent.id);
                    if value["id"].is_number() {
                        merge_messages(history, vec![message_view(value)], false);
                    }
                    if let Some(draft) = self
                        .translation
                        .drafts
                        .get_mut(&(intent.scope.clone(), intent.key.clone()))
                    {
                        if draft.revision == intent.revision && draft.text == intent.draft {
                            draft.text.clear();
                            draft.error = None;
                        }
                    }
                    if self.key().as_ref() == Some(&intent.key) {
                        if self.translation.draft_revision == intent.revision
                            && self.vm.draft == intent.draft
                        {
                            self.vm.draft.clear();
                        }
                        self.show_history();
                    }
                    self.vm.status = "Message sent".into();
                }
            },
            Query::TranslationProvider(sequence) => {
                if *sequence != self.translation.form_sequence
                    || !matches!(self.form_kind, Some(Submission::TranslationSettings))
                {
                    return true;
                }
                self.vm.busy = false;
                match &reply.result {
                    Ok(value) => {
                        if let Some(form) = self.vm.form.as_mut() {
                            for f in &mut form.fields {
                                if matches!(f.key.as_str(), "base_url" | "model") {
                                    f.value = string(value, &f.key);
                                }
                            }
                            form.description.push_str(&format!(
                                "\n{}",
                                tr(
                                    &self.vm.preferences.language,
                                    if value["api_key_set"].as_bool() == Some(true) {
                                        "API token is configured"
                                    } else {
                                        "No API token configured (optional for local models)"
                                    }
                                )
                            ));
                        }
                    }
                    Err(error) => self.vm.error = Some(error.to_string()),
                }
            }
            Query::TranslationPreferences(key, settings) => {
                self.vm.busy = false;
                match &reply.result {
                    Ok(_) => {
                        self.settings = settings.clone();
                        self.translation.generation += 1;
                        self.translation.originals.clear();
                        self.translation.cache.clear();
                        self.translation.lru.clear();
                        if matches!(&self.form_kind,Some(Submission::ChatTranslation(open)) if open==key)
                        {
                            self.vm.form = None;
                            self.form_kind = None;
                        }
                        self.vm.status = "Chat translation saved".into();
                    }
                    Err(error) => self.vm.error = Some(error.to_string()),
                }
            }
            Query::Submitted(Submission::TranslationSettings) => {
                self.vm.busy = false;
                match &reply.result {
                    Ok(_) => {
                        self.vm.form = None;
                        self.form_kind = None;
                        self.translation.generation += 1;
                        self.translation.originals.clear();
                        self.translation.cache.clear();
                        self.translation.lru.clear();
                        self.vm.status = "Translation settings saved".into();
                    }
                    Err(error) => self.vm.error = Some(error.to_string()),
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
#[path = "translation_tests.rs"]
mod translation_tests;
