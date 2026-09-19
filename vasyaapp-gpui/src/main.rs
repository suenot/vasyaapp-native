mod preview;
use gpui_kit::component::input::InputEvent;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    button::*, input::*, ActiveTheme, Disableable, Root, Sizable, StyledExt, Theme, ThemeMode,
};
use gpui_kit::component::{
    checkbox::Checkbox,
    searchable_list::SearchableVec,
    select::{Select, SelectState},
    IndexPath,
};
use gpui_kit::{prelude::*, *};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use vasya_native::{FormKind, NativeApp, UiCommand, ViewModel};

#[derive(Default)]
struct StressMetrics {
    renders: Vec<f64>,
    input_latency: Vec<f64>,
    pending: Option<(String, Instant)>,
    message_rows: usize,
}
fn percentile(samples: &[f64], fraction: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted
        .get(((sorted.len().saturating_sub(1)) as f64 * fraction) as usize)
        .copied()
        .unwrap_or(0.)
}
fn chat_height(vm: &ViewModel) -> Pixels {
    px(match vm.preferences.density.as_str() {
        "very-compact" => 48.,
        "compact" => 60.,
        _ => 76.,
    })
}
struct Messenger {
    stress: Option<StressMetrics>,
    backend: NativeApp,
    vm: Arc<ViewModel>,
    search: Entity<InputState>,
    composer: Entity<TextareaState>,
    fields: Vec<(String, String, FieldInput)>,
    form_key: String,
    search_results: ListState,
    jumped_to: Option<(Option<String>, Option<i64>, i32)>,
    chat_list: ListState,
    previews: HashMap<PathBuf, preview::Entry>,
    preview_generation: u64,
    preview_active: usize,
    preview_clock: u64,
    selected_messages: HashSet<i32>,
    messages: ListState,
    menu: bool,
    _subscriptions: Vec<Subscription>,
}
enum FieldInput {
    Bool(bool),
    Choice(Entity<SelectState<SearchableVec<String>>>),
    Line(Entity<InputState>),
    Multi(Entity<TextareaState>),
}
impl Messenger {
    fn new(backend: NativeApp, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let vm = backend.snapshot();
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(vasya_native::tr(
                &vm.preferences.language,
                "Search conversations",
            ))
        });
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(vasya_native::tr(
                    &vm.preferences.language,
                    "Message · Enter to send, Shift+Enter for a new line",
                ))
                .auto_grow(2, 6)
                .submit_on_enter(true)
        });
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, input, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.backend
                        .send(UiCommand::Search(input.read(cx).value().to_string()));
                }
            }),
            cx.subscribe_in(
                &composer,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::Change => this
                        .backend
                        .send(UiCommand::Draft(input.read(cx).value().to_string())),
                    InputEvent::PressEnter { shift: false, .. } => {
                        this.backend
                            .send(UiCommand::Draft(input.read(cx).value().to_string()));
                        this.backend.send(UiCommand::Send);
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                        this.messages.scroll_to_end();
                    }
                    _ => {}
                },
            ),
        ];
        let messages = ListState::new(vm.messages.len(), ListAlignment::Bottom, px(300.));
        messages.set_follow_mode(FollowMode::Tail);
        let mut watch = backend.subscribe();
        cx.spawn_in(window, async move |weak, cx| {
            while watch.changed().await.is_ok() {
                let next = watch.borrow_and_update().clone();
                if cx
                    .update(|window, cx| weak.update(cx, |this, cx| this.apply(next, window, cx)))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let stress = std::env::args().any(|arg| arg == "--stress-test");
        if stress {
            cx.spawn_in(window, async move |weak, cx| {
                for tick in 0..300 {
                    cx.background_executor().timer(Duration::from_millis(100)).await;
                    if cx.update(|window, cx| weak.update(cx, |this, cx| {
                        if !this.vm.ready { return; }
                        if this.vm.selected_chat.is_some() {
                            this.messages.scroll_by(px(if tick % 60 < 45 { -240. } else { 240. }));
                            let draft = format!("Native input stress {tick} Кириллица");
                            if let Some(metrics) = &mut this.stress { metrics.pending = Some((draft.clone(), Instant::now())); }
                            this.composer.update(cx, |input, cx| input.set_value(draft.clone(), window, cx));
                            this.backend.send(UiCommand::Draft(draft));
                            cx.notify();
                        }
                    })).is_err() { break; }
                }
                let _ = cx.update(|_, cx| {
                    let _ = weak.update(cx, |this, _| {
                        if let Some(m) = &this.stress {
                            eprintln!("VASYA_GPUI_STRESS {}", serde_json::json!({
                                "render_element_construction_p95_ms": percentile(&m.renders, 0.95),
                                "render_element_construction_max_ms": percentile(&m.renders, 1.),
                                "input_widget_to_snapshot_p95_ms": percentile(&m.input_latency, 0.95),
                                "input_samples": m.input_latency.len(), "render_samples": m.renders.len(),
                                "message_row_constructions": m.message_rows,
                                "loaded_messages": this.vm.messages.len(), "chats": this.vm.chats.len(),
                                "gpu_frame_presentation_measured": false,
                            }));
                        }
                    });
                    cx.quit();
                });
            }).detach();
        }
        cx.on_release(|this, cx| {
            for (_, entry) in this.previews.drain() {
                if let preview::Preview::Ready(image) = entry.image {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        let mut this = Self {
            stress: stress.then(StressMetrics::default),
            backend,
            search_results: ListState::new(vm.search_hits.len(), ListAlignment::Top, px(100.))
                .with_uniform_item_height(px(92.)),
            jumped_to: None,
            chat_list: ListState::new(vm.chats.len(), ListAlignment::Top, px(150.))
                .with_uniform_item_height(chat_height(&vm)),
            vm,
            search,
            composer,
            fields: vec![],
            form_key: String::new(),
            previews: HashMap::new(),
            preview_generation: 0,
            preview_active: 0,
            preview_clock: 0,
            selected_messages: HashSet::new(),
            messages,
            menu: false,
            _subscriptions: subscriptions,
        };
        this.sync_form(window, cx);
        this
    }
    fn apply(&mut self, vm: Arc<ViewModel>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(metrics) = &mut self.stress {
            if let Some((draft, started)) = &metrics.pending {
                if draft == &vm.draft {
                    metrics
                        .input_latency
                        .push(started.elapsed().as_secs_f64() * 1000.);
                    metrics.pending = None;
                }
            }
        }
        let changed_chat = (
            vm.selected_account.as_ref(),
            vm.selected_chat,
            vm.selected_topic,
        ) != (
            self.vm.selected_account.as_ref(),
            self.vm.selected_chat,
            self.vm.selected_topic,
        );
        if changed_chat {
            self.preview_generation = self.preview_generation.wrapping_add(1);
            for (_, entry) in self.previews.drain() {
                if let preview::Preview::Ready(image) = entry.image {
                    cx.drop_image(image, Some(window));
                }
            }
            self.selected_messages.clear();
            self.messages.reset(vm.messages.len());
            self.messages.scroll_to_end();
        } else if !Arc::ptr_eq(&vm.messages, &self.vm.messages) {
            let at_end = self.messages.is_scrolled_to_end().unwrap_or(true);
            let anchor = self.messages.logical_scroll_top();
            let anchor_id = self.vm.messages.get(anchor.item_ix).map(|m| m.id);
            // Keep measured heights of unchanged rows when history grows.
            let old = &self.vm.messages;
            let new = &vm.messages;
            let prefix = old
                .iter()
                .zip(new.iter())
                .take_while(|(a, b)| a.id == b.id)
                .count();
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(new[prefix..].iter().rev())
                .take_while(|(a, b)| a.id == b.id)
                .count();
            self.messages
                .splice(prefix..old.len() - suffix, new.len() - prefix - suffix);
            for index in 0..new.len() {
                let previous = if index < prefix {
                    old.get(index)
                } else if index >= new.len() - suffix {
                    old.get(old.len() - (new.len() - index))
                } else {
                    None
                };
                if let Some(previous) = previous {
                    let current = &new[index];
                    if previous.text != current.text
                        || previous.image_path != current.image_path
                        || previous.transcription != current.transcription
                        || previous.failed != current.failed
                    {
                        self.messages.remeasure_items(index..index + 1);
                    }
                }
            }
            if at_end {
                self.messages.scroll_to_end();
            } else if let Some(index) =
                anchor_id.and_then(|id| vm.messages.iter().position(|m| m.id == id))
            {
                self.messages.scroll_to(ListOffset {
                    item_ix: index,
                    offset_in_item: anchor.offset_in_item,
                });
            }
        }
        if vm.chats.len() != self.vm.chats.len()
            || vm.preferences.density != self.vm.preferences.density
        {
            self.chat_list
                .reset_with_uniform_height(vm.chats.len(), chat_height(&vm));
        }
        if changed_chat
            || vm.draft != self.vm.draft && self.vm.draft == self.composer.read(cx).value().as_ref()
        {
            self.composer.update(cx, |input, cx| {
                input.set_value(vm.draft.clone(), window, cx)
            });
        }
        if vm.scale != self.vm.scale
            || vm.preferences.density != self.vm.preferences.density
            || vm.preferences.text_size != self.vm.preferences.text_size
            || vm.preferences.markdown != self.vm.preferences.markdown
            || vm.preferences.merge_messages != self.vm.preferences.merge_messages
        {
            self.messages.remeasure();
        }
        if vm.dark != self.vm.dark {
            Theme::change(
                if vm.dark {
                    ThemeMode::Dark
                } else {
                    ThemeMode::Light
                },
                Some(window),
                cx,
            );
        }
        if vm.preferences.language != self.vm.preferences.language {
            self.search.update(cx, |input, cx| {
                input.set_placeholder(
                    vasya_native::tr(&vm.preferences.language, "Search conversations"),
                    window,
                    cx,
                )
            });
            self.composer.update(cx, |input, cx| {
                input.set_placeholder(
                    vasya_native::tr(
                        &vm.preferences.language,
                        "Message · Enter to send, Shift+Enter for a new line",
                    ),
                    window,
                    cx,
                )
            });
        }
        if !Arc::ptr_eq(&vm.search_hits, &self.vm.search_hits) {
            self.search_results
                .reset_with_uniform_height(vm.search_hits.len(), px(92.));
        }
        if let Some(message_id) = vm.jump_to {
            let key = (vm.selected_account.clone(), vm.selected_chat, message_id);
            if self.jumped_to.as_ref() != Some(&key) {
                if let Some(index) = vm
                    .messages
                    .iter()
                    .position(|message| message.id == message_id)
                {
                    self.messages.scroll_to_reveal_item(index);
                    self.jumped_to = Some(key);
                }
            }
        } else {
            self.jumped_to = None;
        }
        self.vm = vm;
        self.sync_form(window, cx);
        cx.notify();
    }
    fn sync_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self
            .vm
            .form
            .as_ref()
            .map(|f| {
                format!(
                    "{}:{:?}",
                    f.title,
                    f.fields
                        .iter()
                        .map(|f| (&f.key, &f.value))
                        .collect::<Vec<_>>()
                )
            })
            .unwrap_or_default();
        if key == self.form_key {
            return;
        }
        self.form_key = key;
        self.fields = self
            .vm
            .form
            .as_ref()
            .map(|form| {
                form.fields
                    .iter()
                    .map(|f| {
                        let field = if !f.secret && matches!(f.value.as_str(), "true" | "false") {
                            FieldInput::Bool(f.value == "true")
                        } else if matches!(
                            f.key.as_str(),
                            "provider"
                                | "whisper_model"
                                | "ui_language"
                                | "density"
                                | "folder_layout"
                        ) {
                            let options: Vec<String> = match f.key.as_str() {
                                "provider" => vec!["deepgram", "local_whisper"],
                                "ui_language" => vec!["en", "ru"],
                                "density" => vec!["normal", "compact", "very-compact"],
                                "folder_layout" => vec!["horizontal", "vertical"],
                                _ => vec!["tiny", "base", "small", "medium"],
                            }
                            .into_iter()
                            .map(str::to_string)
                            .collect();
                            let selected = options
                                .iter()
                                .position(|v| v == &f.value)
                                .map(IndexPath::new);
                            FieldInput::Choice(cx.new(|cx| {
                                SelectState::new(SearchableVec::new(options), selected, window, cx)
                            }))
                        } else if f.multiline && !f.secret {
                            FieldInput::Multi(cx.new(|cx| {
                                TextareaState::new(window, cx)
                                    .default_value(f.value.clone())
                                    .auto_grow(3, 8)
                            }))
                        } else {
                            FieldInput::Line(cx.new(|cx| {
                                InputState::new(window, cx)
                                    .default_value(f.value.clone())
                                    .masked(f.secret)
                            }))
                        };
                        (f.key.clone(), f.label.clone(), field)
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    fn tr(&self, text: &str) -> String {
        vasya_native::tr(&self.vm.preferences.language, text)
    }
    fn button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        command: UiCommand,
    ) -> Button {
        let backend = self.backend.clone();
        let label: SharedString = label.into();
        Button::new(id)
            .label(self.tr(label.as_ref()))
            .small()
            .on_click(move |_, _, _| backend.send(command.clone()))
    }
    fn preview(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.preview_clock = self.preview_clock.wrapping_add(1);
        if !self.previews.contains_key(&path) && self.preview_active < preview::MAX_WORKERS {
            if self.previews.len() >= preview::MAX_ENTRIES {
                let oldest = self
                    .previews
                    .iter()
                    .filter(|(_, entry)| !matches!(entry.image, preview::Preview::Loading))
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(path, _)| path.clone());
                if let Some(oldest) = oldest {
                    if let Some(preview::Entry {
                        image: preview::Preview::Ready(image),
                        ..
                    }) = self.previews.remove(&oldest)
                    {
                        cx.drop_image(image, Some(window));
                    }
                }
            }
            if self.previews.len() < preview::MAX_ENTRIES {
                self.previews.insert(
                    path.clone(),
                    preview::Entry {
                        image: preview::Preview::Loading,
                        last_used: self.preview_clock,
                    },
                );
                self.preview_active += 1;
                let generation = self.preview_generation;
                let load_path = path.clone();
                let decode = cx.background_executor().spawn(async move {
                    preview::decode(&load_path).map_err(|error| error.to_string())
                });
                let result_path = path.clone();
                cx.spawn_in(window, async move |weak, cx| {
                    let result = decode.await;
                    let _ = cx.update(|_, cx| {
                        weak.update(cx, |this, cx| {
                            this.preview_active = this.preview_active.saturating_sub(1);
                            if generation == this.preview_generation {
                                if let Some(entry) = this.previews.get_mut(&result_path) {
                                    entry.image = match result {
                                        Ok(image) => preview::Preview::Ready(image),
                                        Err(error) => preview::Preview::Failed(error),
                                    };
                                }
                            }
                            cx.notify();
                        })
                    });
                })
                .detach();
            }
        }
        let content = match self.previews.get_mut(&path) {
            Some(entry) => {
                entry.last_used = self.preview_clock;
                match &entry.image {
                    preview::Preview::Ready(image) => img(image.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .into_any_element(),
                    preview::Preview::Failed(error) => div()
                        .text_xs()
                        .p_2()
                        .child(format!("Preview unavailable: {error}"))
                        .into_any_element(),
                    preview::Preview::Loading => div()
                        .text_sm()
                        .child(self.tr("Loading preview…"))
                        .into_any_element(),
                }
            }
            None => div()
                .text_sm()
                .child(self.tr("Loading preview…"))
                .into_any_element(),
        };
        // Reserve the same height for pending, ready and failed previews, so
        // completion never moves the user's message anchor.
        div()
            .w(px(360.))
            .h(px(180.))
            .rounded_md()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }
    fn message(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(metrics) = &mut self.stress {
            metrics.message_rows += 1;
        }
        let vm = self.vm.clone();
        let Some(m) = vm.messages.get(ix) else {
            return div().into_any_element();
        };
        let message_padding = match self.vm.preferences.density.as_str() {
            "very-compact" => 4.,
            "compact" => 8.,
            _ => 12.,
        };
        let mut body = div()
            .v_flex()
            .gap(px(message_padding / 2.))
            .p(px(message_padding))
            .rounded_lg()
            .max_w(px(680.))
            .bg(if m.outgoing {
                cx.theme().primary.opacity(0.15)
            } else {
                cx.theme().muted
            });
        let merged = self.vm.preferences.merge_messages
            && ix > 0
            && self.vm.messages.get(ix - 1).is_some_and(|previous| {
                previous.sender == m.sender && previous.outgoing == m.outgoing
            });
        body = body.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "{}  {}{}",
                    if merged { "" } else { &m.sender },
                    m.time,
                    if m.pending { " · Sending…" } else { "" }
                )),
        );
        if let Some(path) = &m.image_path {
            body = body.child(self.preview(path.clone(), window, cx));
        }
        if !m.text.is_empty() {
            body = body.child(
                TextView::markdown(
                    ("text", m.id as usize),
                    if self.vm.preferences.markdown {
                        m.text.clone()
                    } else {
                        let mut escaped = String::new();
                        for c in m.text.chars() {
                            if "\\`*_{}[]<>()#+-.!|~".contains(c) {
                                escaped.push('\\');
                            }
                            if c == '\n' {
                                escaped.push_str("  ");
                            }
                            escaped.push(c);
                        }
                        escaped
                    },
                )
                .plugin(preview::MarkdownImageLinks { block: false })
                .plugin(preview::MarkdownImageLinks { block: true })
                .selectable(true)
                .on_link_click(|url, _, _, cx| cx.open_url(url)),
            );
        }
        if let Some(text) = &m.transcription {
            body = body.child(div().text_sm().child(text.clone()));
        }
        let mut actions = div().h_flex().gap_1();
        if m.id > 0 {
            let id = m.id;
            actions = actions.child(
                Checkbox::new(("select-message", ix))
                    .label(self.tr("Select"))
                    .checked(self.selected_messages.contains(&id))
                    .on_click(cx.listener(move |this, checked, _, cx| {
                        if *checked {
                            this.selected_messages.insert(id);
                        } else {
                            this.selected_messages.remove(&id);
                        }
                        cx.notify();
                    })),
            );
        }
        if let Some(kind) = &m.media_kind {
            actions = actions
                .child(self.button(
                    ("download", ix),
                    format!("Download {kind}"),
                    UiCommand::Download(m.id),
                ))
                .child(self.button(("open", ix), "Open", UiCommand::OpenDownloaded(m.id)));
            if kind.contains("voice") || kind.contains("audio") {
                actions = actions.child(self.button(
                    ("stt", ix),
                    "Transcribe",
                    UiCommand::Transcribe(m.id),
                ));
            }
        }
        actions = actions.child(self.button(("forward", ix), "Forward", UiCommand::Forward(m.id)));
        if m.failed {
            actions = actions.child(self.button(("retry", ix), "Retry", UiCommand::Retry(m.id)));
        }
        div()
            .w_full()
            .px_4()
            .py(px(message_padding / 2.))
            .h_flex()
            .when(m.outgoing, |d| d.justify_end())
            .child(body.child(actions))
            .into_any_element()
    }
}
impl Render for Messenger {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_started = Instant::now();
        // Bind viewport IDs and provenance to the same rendered snapshot. An
        // event from an old list cannot request media in a newly opened chat.
        let rendered = self.vm.clone();
        let weak = cx.weak_entity();
        self.messages.set_scroll_handler(move |event, _, cx| {
            let _ = weak.update(cx, |this, _| {
                if let (Some(account), Some(chat)) =
                    (&rendered.selected_account, rendered.selected_chat)
                {
                    let ids = rendered
                        .messages
                        .iter()
                        .skip(event.visible_range.start)
                        .take(event.visible_range.len())
                        .map(|m| m.id)
                        .collect();
                    this.backend.send(UiCommand::VisibleMessages {
                        account: account.clone(),
                        chat,
                        topic: rendered.selected_topic,
                        ids,
                    });
                    if this.vm.selected_account.as_ref() == Some(account)
                        && this.vm.selected_chat == Some(chat)
                        && this.vm.selected_topic == rendered.selected_topic
                        && event.visible_range.start < 3
                        && this.vm.has_older
                        && !this.vm.busy
                    {
                        this.backend.send(UiCommand::LoadOlder);
                    }
                }
            });
        });
        window.set_rem_size(px(16. * self.vm.scale as f32));
        let mut accounts = div().h_flex().flex_wrap().gap_1();
        for account in &self.vm.accounts {
            accounts = accounts.child(
                self.button(
                    SharedString::from(format!("account-{}", account.id)),
                    account.title.clone(),
                    UiCommand::SelectAccount(account.id.clone()),
                )
                .when(
                    self.vm.selected_account.as_ref() == Some(&account.id),
                    |button| button.primary(),
                ),
            );
        }
        accounts =
            accounts.child(self.button("login", "+ Account", UiCommand::OpenForm(FormKind::Login)));
        let mut folders = div()
            .id("folders")
            .max_h(px(180.))
            .overflow_y_scroll()
            .when(self.vm.preferences.folder_layout == "vertical", |d| {
                d.v_flex()
            })
            .when(self.vm.preferences.folder_layout != "vertical", |d| {
                d.h_flex().flex_wrap()
            })
            .gap_1()
            .child(self.button("all", "All", UiCommand::SelectFolder(None)));
        for folder in &self.vm.folders {
            folders = folders.child(
                self.button(
                    SharedString::from(format!("folder-{}", folder.id)),
                    folder.title.clone(),
                    UiCommand::SelectFolder(Some(folder.id.clone())),
                )
                .when(
                    self.vm.selected_folder.as_ref() == Some(&folder.id),
                    |button| button.primary(),
                ),
            );
        }
        let weak = cx.weak_entity();
        let chats = list(self.chat_list.clone(), move |ix, _, cx| {
            weak.update(cx, |this, cx| {
                let Some(chat) = this.vm.chats.get(ix) else {
                    return div().into_any_element();
                };
                let id = chat.id;
                let backend = this.backend.clone();
                div()
                    .id(("chat", ix))
                    .h(chat_height(&this.vm))
                    .px_3()
                    .py_2()
                    .v_flex()
                    .gap_1()
                    .cursor_pointer()
                    .bg(if this.vm.selected_chat == Some(id) {
                        cx.theme().primary.opacity(0.15)
                    } else {
                        cx.theme().transparent
                    })
                    .hover(|d| d.bg(cx.theme().muted))
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(chat.title.clone()),
                            )
                            .child(if chat.unread > 0 {
                                chat.unread.to_string()
                            } else {
                                String::new()
                            }),
                    )
                    .when(this.vm.preferences.density != "very-compact", |row| {
                        row.child(
                            div()
                                .text_sm()
                                .truncate()
                                .text_color(cx.theme().muted_foreground)
                                .child(chat.preview.clone()),
                        )
                    })
                    .on_click(move |_, _, _| backend.send(UiCommand::SelectChat(id)))
                    .into_any_element()
            })
            .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .w_full();
        let sidebar = div()
            .v_flex()
            .w(px(310.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .p_4()
                    .v_flex()
                    .gap_3()
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .child(div().text_xl().font_weight(FontWeight::BOLD).child("Vasya"))
                            .child(Button::new("menu").label("☰").small().on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.menu = !this.menu;
                                    cx.notify();
                                },
                            ))),
                    )
                    .child(accounts)
                    .child(Input::new(&self.search))
                    .child(folders),
            )
            .child(chats)
            .child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "GPUI Kit · {}  {}",
                        self.vm.version,
                        if self.vm.remote { "Remote" } else { "Native" }
                    )),
            );
        let mut header = div()
            .h_flex()
            .p_4()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.vm.chat_title.clone()),
            );
        if !self.selected_messages.is_empty() {
            let mut selected: Vec<i32> = self.selected_messages.iter().copied().collect();
            selected.sort_unstable();
            header = header
                .child(format!("{}: {}", self.tr("Selected"), selected.len()))
                .child(self.button(
                    "forward-selected",
                    "Forward selected",
                    UiCommand::ForwardMany(selected),
                ))
                .child(
                    Button::new("clear-selection")
                        .label(self.tr("Clear"))
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.selected_messages.clear();
                            cx.notify();
                        })),
                );
        } else if self.vm.selected_chat.is_some() {
            header = header
                .child(self.button("info", "Info", UiCommand::ChatInfo))
                .child(self.button(
                    "find",
                    "Find",
                    UiCommand::OpenForm(FormKind::SearchMessages),
                ))
                .child(self.button("calls", "Calls unavailable", UiCommand::Calls));
        }
        let mut topics = div().h_flex().flex_wrap().gap_1().px_3();
        if !self.vm.topics.is_empty() {
            topics = topics.child(self.button("general", "General", UiCommand::SelectTopic(None)));
        }
        for topic in &self.vm.topics {
            topics = topics.child(self.button(
                ("topic", topic.id as usize),
                topic.title.clone(),
                UiCommand::SelectTopic(Some(topic.id)),
            ));
        }
        let weak = cx.weak_entity();
        let history = list(self.messages.clone(), move |ix, window, cx| {
            weak.update(cx, |this, cx| this.message(ix, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .w_full();
        let mut content = div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .child(topics);
        if self.vm.selected_chat.is_some() {
            if self.vm.has_older {
                content = content.child(self.button(
                    "older",
                    "Load earlier messages",
                    UiCommand::LoadOlder,
                ));
            }
            let backend = self.backend.clone();
            let clipboard_backend = self.backend.clone();
            content = content.child(history).child(
                div()
                    .p_3()
                    .v_flex()
                    .gap_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(Textarea::new(&self.composer).on_paste(move |item, _, _| {
                        let mut handled = false;
                        for entry in item.entries() {
                            match entry {
                                ClipboardEntry::Image(image) => {
                                    clipboard_backend.send(UiCommand::SendClipboardImage {
                                        bytes: image.bytes().to_vec(),
                                        extension: image.format().extension().to_string(),
                                    });
                                    handled = true;
                                }
                                ClipboardEntry::ExternalPaths(paths) => {
                                    for path in paths.paths() {
                                        clipboard_backend.send(UiCommand::SendFile(path.clone()));
                                    }
                                    handled = true;
                                }
                                _ => {}
                            }
                        }
                        handled
                    }))
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                Button::new("attach")
                                    .label(self.tr("Attach"))
                                    .small()
                                    .on_click(move |_, _, cx| {
                                        let backend = backend.clone();
                                        cx.spawn(async move |_| {
                                            if let Some(file) =
                                                rfd::AsyncFileDialog::new().pick_file().await
                                            {
                                                backend.send(UiCommand::SendFile(
                                                    file.path().to_path_buf(),
                                                ));
                                            }
                                        })
                                        .detach();
                                    }),
                            )
                            .child(self.button("voice", "Voice", UiCommand::RecordVoice))
                            .child(self.button("camera", "Camera", UiCommand::Camera))
                            .child(div().flex_1())
                            .child(self.button("send", "Send", UiCommand::Send).primary()),
                    ),
            );
        } else {
            content = content.child(
                div()
                    .v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(
                        div()
                            .text_2xl()
                            .child(self.tr("Your conversations, native.")),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.tr("Select a chat, or connect your Telegram account.")),
                    )
                    .child(
                        self.button(
                            "connect",
                            "Connect Telegram",
                            UiCommand::OpenForm(FormKind::Credentials),
                        )
                        .primary(),
                    ),
            );
        }
        let drop_backend = self.backend.clone();
        let mut root = div()
            .on_drop(move |paths: &ExternalPaths, _, _| {
                for path in paths.paths() {
                    drop_backend.send(UiCommand::SendFile(path.clone()));
                }
            })
            .size_full()
            .v_flex()
            .relative()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(self.vm.preferences.text_size * self.vm.scale as f32))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.to_lowercase();
                let modifiers = event.keystroke.modifiers;
                if key == "escape"
                    && (this.menu || this.vm.form.is_some() || this.vm.detail.is_some())
                {
                    this.menu = false;
                    this.backend.send(UiCommand::CloseOverlay);
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                // Keep the text editor's send/newline and native clipboard bindings.
                if key == "enter" || (key == "v" && (modifiers.platform || modifiers.control)) {
                    return;
                }
                let mut chord = String::new();
                for (active, name) in [
                    (modifiers.platform, "meta"),
                    (modifiers.control, "ctrl"),
                    (modifiers.alt, "alt"),
                    (modifiers.shift, "shift"),
                ] {
                    if active {
                        chord.push_str(name);
                        chord.push('+');
                    }
                }
                chord.push_str(&key);
                if let Some(action) = vasya_native::shortcut_action(&this.vm.preferences, &chord) {
                    if action == "focus_search" {
                        this.search.update(cx, |input, cx| input.focus(window, cx));
                    } else {
                        this.backend.send(UiCommand::Shortcut(action));
                    }
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(content),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(self.vm.status.clone()),
            );
        if let Some(error) = &self.vm.error {
            root = root.child(
                div()
                    .p_2()
                    .h_flex()
                    .gap_2()
                    .bg(cx.theme().danger.opacity(0.15))
                    .child(div().flex_1().child(error.clone()))
                    .child(self.button("dismiss", "Dismiss", UiCommand::DismissError)),
            );
        }
        if self.menu {
            let mut menu = div()
                .id("main-menu")
                .max_h(relative(0.85))
                .overflow_y_scroll()
                .absolute()
                .top(px(58.))
                .left(px(20.))
                .w(px(260.))
                .p_3()
                .v_flex()
                .gap_1()
                .rounded_lg()
                .shadow_lg()
                .bg(cx.theme().background)
                .border_1()
                .border_color(cx.theme().border);
            for (i, (label, command)) in [
                ("Settings", UiCommand::OpenForm(FormKind::Settings)),
                ("Keyboard shortcuts", UiCommand::OpenForm(FormKind::Hotkeys)),
                (
                    "Telegram credentials",
                    UiCommand::OpenForm(FormKind::Credentials),
                ),
                ("Add account", UiCommand::OpenForm(FormKind::Login)),
                ("Remote server", UiCommand::OpenForm(FormKind::Remote)),
                ("Use embedded engine", UiCommand::EmbeddedMode),
                ("Local API", UiCommand::OpenForm(FormKind::LocalApi)),
                ("Speech recognition", UiCommand::OpenForm(FormKind::Stt)),
                ("Create group", UiCommand::OpenForm(FormKind::CreateGroup)),
                (
                    "Create channel",
                    UiCommand::OpenForm(FormKind::CreateChannel),
                ),
                ("Manage folders", UiCommand::OpenForm(FormKind::Folder)),
                ("Delete folder", UiCommand::DeleteFolder),
                ("Tabs", UiCommand::OpenForm(FormKind::Tabs)),
                ("Storage", UiCommand::OpenForm(FormKind::Storage)),
                ("Downloads", UiCommand::Downloads),
                ("Switch theme", UiCommand::ToggleTheme),
                ("Refresh", UiCommand::Refresh),
                ("Favorite / unfavorite", UiCommand::ToggleFavorite),
                ("Leave current chat", UiCommand::LeaveChat),
                ("Log out", UiCommand::Logout),
            ]
            .into_iter()
            .enumerate()
            {
                let backend = self.backend.clone();
                menu = menu.child(Button::new(("menu-item", i)).label(label).small().on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.menu = false;
                        backend.send(command.clone());
                        cx.notify();
                    }),
                ));
            }
            root = root.child(menu);
        }
        if let Some(form) = &self.vm.form {
            let mut panel = div()
                .w(px(520.))
                .max_h(relative(0.9))
                .id("form")
                .overflow_y_scroll()
                .p_6()
                .v_flex()
                .gap_3()
                .rounded_lg()
                .shadow_lg()
                .bg(cx.theme().background)
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(form.title.clone()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(form.description.clone()),
                );
            for (index, (_, label, input)) in self.fields.iter().enumerate() {
                panel = panel.child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(label.clone())
                        .child(match input {
                            FieldInput::Bool(value) => Checkbox::new(("boolean", index))
                                .checked(*value)
                                .on_click(cx.listener(move |this, value, _, cx| {
                                    if let Some((_, _, FieldInput::Bool(current))) =
                                        this.fields.get_mut(index)
                                    {
                                        *current = *value;
                                    }
                                    cx.notify();
                                }))
                                .into_any_element(),
                            FieldInput::Choice(input) => Select::new(input).into_any_element(),
                            FieldInput::Line(input) => Input::new(input).into_any_element(),
                            FieldInput::Multi(input) => Textarea::new(input).into_any_element(),
                        }),
                );
            }
            panel = panel.child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(self.button("cancel", "Cancel", UiCommand::CloseOverlay))
                    .child(
                        Button::new("submit")
                            .primary()
                            .label(form.submit_label.clone())
                            .disabled(self.vm.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let values: BTreeMap<_, _> = this
                                    .fields
                                    .iter()
                                    .map(|(key, _, input)| {
                                        (
                                            key.clone(),
                                            match input {
                                                FieldInput::Bool(value) => value.to_string(),
                                                FieldInput::Choice(input) => input
                                                    .read(cx)
                                                    .selected_value()
                                                    .cloned()
                                                    .unwrap_or_default(),
                                                FieldInput::Line(input) => {
                                                    input.read(cx).value().to_string()
                                                }
                                                FieldInput::Multi(input) => {
                                                    input.read(cx).value().to_string()
                                                }
                                            },
                                        )
                                    })
                                    .collect();
                                this.backend.send(UiCommand::SubmitForm(values));
                            })),
                    ),
            );
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(black().opacity(0.45))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(panel),
            );
        } else if let Some((title, text)) = &self.vm.detail {
            let weak = cx.weak_entity();
            let search_results = list(self.search_results.clone(), move |ix, _, cx| {
                weak.update(cx, |this, cx| {
                    let Some(hit) = this.vm.search_hits.get(ix) else {
                        return div().into_any_element();
                    };
                    div()
                        .h(px(92.))
                        .v_flex()
                        .gap_2()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(this.button(
                            ("search-result", ix),
                            hit.title.clone(),
                            UiCommand::JumpToMessage(hit.chat_id, hit.message_id),
                        ))
                        .child(div().truncate().text_sm().child(hit.text.clone()))
                        .into_any_element()
                })
                .unwrap_or_else(|_| div().into_any_element())
            })
            .h(px(360.))
            .w_full();
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(black().opacity(0.45))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("detail")
                            .overflow_y_scroll()
                            .w(px(560.))
                            .max_h(relative(0.85))
                            .p_6()
                            .v_flex()
                            .gap_4()
                            .rounded_lg()
                            .bg(cx.theme().background)
                            .child(div().text_xl().child(title.clone()))
                            .child(text.clone())
                            .when(!self.vm.search_hits.is_empty(), |panel| {
                                panel.child(search_results)
                            })
                            .child(self.button("close", "Close", UiCommand::CloseOverlay)),
                    ),
            );
        }
        if let Some(metrics) = &mut self.stress {
            metrics
                .renders
                .push(render_started.elapsed().as_secs_f64() * 1000.);
        }
        root
    }
}
fn main() -> anyhow::Result<()> {
    let smoke = std::env::args().any(|arg| arg == "--smoke-test");
    let backend = NativeApp::new("gpui")?;
    let shutdown = backend.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // macOS may terminate inside NSApplication rather than return from run().
            let quit_backend = backend.clone();
            cx.on_app_quit(move |_| {
                quit_backend.shutdown();
                async {}
            })
            .detach();
            Theme::change(
                if backend.snapshot().dark {
                    ThemeMode::Dark
                } else {
                    ThemeMode::Light
                },
                None,
                cx,
            );
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            cx.spawn(async move |cx| {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(880.), px(600.))),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Vasya · GPUI".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = cx.new(|cx| Messenger::new(backend, window, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("Open native GPUI window");
                cx.update(|cx| cx.activate(true));
                eprintln!("VASYA_GPUI_WINDOW_READY");
                if smoke {
                    cx.background_executor().timer(Duration::from_secs(3)).await;
                    cx.update(|cx| cx.quit());
                }
            })
            .detach();
        });
    shutdown.shutdown();
    Ok(())
}
