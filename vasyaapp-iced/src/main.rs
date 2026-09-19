use iced::{
    widget::{
        self, button, column, container, image, markdown, row, scrollable, space, text,
        text_editor, text_input,
    },
    Element, Fill, Subscription, Task, Theme,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use vasya_native::{FormKind, MessageView, NativeApp, UiCommand, ViewModel};

fn main() -> iced::Result {
    let app = NativeApp::new("iced").expect("Unable to initialize Vasya Iced profile");
    let shutdown = app.clone();
    let result = iced::application(move || App::new(app.clone()), App::update, App::view)
        .title("Vasya · Iced")
        .theme(|state: &App| {
            if state.vm.dark {
                Theme::Dark
            } else {
                Theme::Light
            }
        })
        .scale_factor(|state: &App| state.vm.scale as f32)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1180., 800.),
            min_size: Some(iced::Size::new(920., 640.)),
            ..Default::default()
        })
        .run();
    shutdown.shutdown();
    result
}

#[derive(Debug, Clone)]
enum Event {
    Snapshot(Arc<ViewModel>, Instant),
    Key(iced::keyboard::Event),
    StressTick(Instant),
    Command(UiCommand),
    FormValue(String, String),
    Submit,
    Edit(text_editor::Action),
    PickFile,
    Copy(String),
    Markdown(String),
    Inspect(String),
    SelectText(text_editor::Action),
    OpenLink(String),
    PasteImage,
    ClipboardImage(Result<Vec<u8>, String>),
    File(Option<PathBuf>),
    Menu,
    ChatsScrolled(f32),
    ResultsScrolled(f32),
    SelectMessage(i32, bool),
    ClearSelection,
    ImageReady(u64, PathBuf, Result<DecodedPreview, String>),
    HistoryScrolled(f32, f32, f32),
    Resized(f32),
    Exit,
    LayoutReady(
        Arc<Vec<MessageView>>,
        f32,
        f32,
        Vec<f32>,
        HashMap<i32, (u64, f32)>,
    ),
}

struct App {
    app: NativeApp,
    vm: Arc<ViewModel>,
    form: BTreeMap<String, String>,
    editor: text_editor::Content,
    sent_draft: Option<String>,
    menu: bool,
    chat_offset: f32,
    search_offset: f32,
    selected_messages: HashSet<i32>,
    image_cache: PreviewCache,
    image_pending: HashSet<PathBuf>,
    image_epoch: u64,
    visible: Vec<i32>,
    history_offset: f32,
    viewport_height: f32,
    width: f32,
    bottom: bool,
    height_cache: HashMap<i32, (u64, f32)>,
    heights: Vec<f32>,
    prefix: Vec<f32>,
    layout_busy: bool,
    layout_pending: bool,
    clipboard_error: Option<String>,
    markdown: Option<Vec<markdown::Item>>,
    inspector: Option<text_editor::Content>,
    rendered: HashMap<i32, (u64, Vec<markdown::Item>)>,
    stress: bool,
    metrics: RefCell<Metrics>,
    stress_ticks: u64,
    stress_started: Instant,
    pending_draft: Option<(String, Instant)>,
    layout_started: Option<Instant>,
}

impl App {
    fn new(app: NativeApp) -> (Self, Task<Event>) {
        let receiver = app.subscribe();
        let vm = receiver.borrow().clone();
        let updates = Task::run(
            iced::futures::stream::unfold(receiver, |mut rx| async move {
                if rx.changed().await.is_err() {
                    return None;
                }
                let snapshot = rx.borrow_and_update().clone();
                Some((snapshot, rx))
            }),
            |vm| Event::Snapshot(vm, Instant::now()),
        );
        let stress = std::env::args().any(|v| v == "--stress-test");
        let startup = if stress || std::env::args().any(|v| v == "--smoke-test") {
            Task::perform(
                async move {
                    tokio::time::sleep(Duration::from_secs(if stress { 30 } else { 3 })).await
                },
                |_| Event::Exit,
            )
        } else {
            Task::none()
        };
        let mut state = Self {
            app,
            vm,
            form: BTreeMap::new(),
            editor: text_editor::Content::new(),
            sent_draft: None,
            menu: false,
            chat_offset: 0.,
            search_offset: 0.,
            selected_messages: HashSet::new(),
            image_cache: PreviewCache::default(),
            image_pending: HashSet::new(),
            image_epoch: 0,
            visible: vec![],
            history_offset: 0.,
            viewport_height: 560.,
            width: 1180.,
            bottom: true,
            height_cache: HashMap::new(),
            heights: vec![],
            prefix: vec![0.],
            layout_busy: false,
            layout_pending: false,
            clipboard_error: None,
            markdown: None,
            inspector: None,
            rendered: HashMap::new(),
            stress,
            metrics: RefCell::new(Metrics::default()),
            stress_ticks: 0,
            stress_started: Instant::now(),
            pending_draft: None,
            layout_started: None,
        };
        state.form = state
            .vm
            .form
            .as_ref()
            .map(|form| {
                form.fields
                    .iter()
                    .map(|field| (field.key.clone(), field.value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        state.editor = text_editor::Content::with_text(&state.vm.draft);
        state.rebuild_heights();
        let layout = state.schedule_layout();
        (state, Task::batch([updates, startup, layout]))
    }
    fn subscription(&self) -> Subscription<Event> {
        Subscription::batch([
            if self.stress {
                iced::time::every(Duration::from_millis(100)).map(Event::StressTick)
            } else {
                Subscription::none()
            },
            iced::event::listen_with(|event, status, _| match event {
                iced::Event::Keyboard(event @ iced::keyboard::Event::KeyPressed { .. }) => {
                    if let iced::keyboard::Event::KeyPressed { modifiers, key, .. } = &event {
                        if status == iced::event::Status::Captured
                            && !modifiers.logo()
                            && !modifiers.control()
                            && !modifiers.alt()
                            && *key
                                != iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
                        {
                            return None;
                        }
                    }
                    Some(Event::Key(event))
                }
                _ => None,
            }),
            iced::window::resize_events().map(|(_, size)| Event::Resized(size.width)),
        ])
    }
    fn rebuild_heights(&mut self) {
        let width = (self.width - 450.).max(80.);
        self.heights = self
            .vm
            .messages
            .iter()
            .map(|message| {
                let key = height_key(message, width, self.vm.preferences.text_size);
                if let Some((cached, height)) = self.height_cache.get(&message.id) {
                    if *cached == key {
                        return *height;
                    }
                }
                estimate_height(message, width, self.vm.preferences.text_size)
            })
            .collect();
        if self.height_cache.len() > self.vm.messages.len() + 1000 {
            let ids: std::collections::HashSet<_> = self.vm.messages.iter().map(|m| m.id).collect();
            self.height_cache.retain(|id, _| ids.contains(id));
        }
        self.layout_pending = true;
        self.rebuild_prefix();
    }
    fn rebuild_prefix(&mut self) {
        self.prefix.clear();
        self.prefix.push(0.);
        for height in &self.heights {
            self.prefix
                .push(self.prefix.last().copied().unwrap_or(0.) + height);
        }
    }
    fn schedule_layout(&mut self) -> Task<Event> {
        if self.layout_busy || !self.layout_pending {
            return Task::none();
        }
        self.layout_busy = true;
        self.layout_started = Some(Instant::now());
        self.layout_pending = false;
        let messages = self.vm.messages.clone();
        let width = (self.width - 450.).max(80.);
        let mut cache = self.height_cache.clone();
        let size = self.vm.preferences.text_size;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let heights = messages
                        .iter()
                        .map(|message| {
                            let key = height_key(message, width, size);
                            if let Some((cached, height)) = cache.get(&message.id) {
                                if *cached == key {
                                    return *height;
                                }
                            }
                            let height = message_height(message, width, size);
                            cache.insert(message.id, (key, height));
                            height
                        })
                        .collect();
                    (messages, width, size, heights, cache)
                })
                .await
                .expect("text layout worker")
            },
            |(messages, width, size, heights, cache)| {
                Event::LayoutReady(messages, width, size, heights, cache)
            },
        )
    }
    fn update(&mut self, event: Event) -> Task<Event> {
        let start = Instant::now();
        let task = self.update_inner(event);
        let images = self.schedule_images();
        if self.stress {
            self.metrics
                .borrow_mut()
                .updates
                .push(start.elapsed().as_micros() as u64);
        }
        Task::batch([task, images])
    }
    fn history_is_visible(&self) -> bool {
        self.vm.selected_chat.is_some()
            && !self.menu
            && self.vm.form.is_none()
            && self.vm.detail.is_none()
            && self.markdown.is_none()
            && self.inspector.is_none()
            && self.vm.search_hits.is_empty()
    }
    fn schedule_images(&mut self) -> Task<Event> {
        if !self.history_is_visible() {
            return Task::none();
        }
        let start = self
            .prefix
            .partition_point(|y| *y < self.history_offset)
            .saturating_sub(1)
            .min(self.vm.messages.len());
        let end = self
            .prefix
            .partition_point(|y| *y < self.history_offset + self.viewport_height)
            .min(self.vm.messages.len());
        let mut tasks = Vec::new();
        for message in &self.vm.messages[start..end] {
            let Some(path) = &message.image_path else {
                continue;
            };
            if self.image_cache.touch(path) || self.image_pending.contains(path) {
                continue;
            }
            if self.image_pending.len() >= 2 {
                continue;
            }
            self.image_pending.insert(path.clone());
            let source = path.clone();
            let epoch = self.image_epoch;
            tasks.push(Task::perform(
                async move {
                    let path = source.clone();
                    let result = tokio::task::spawn_blocking(move || decode_preview(&path))
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                    (epoch, source, result)
                },
                |(epoch, path, result)| Event::ImageReady(epoch, path, result),
            ));
        }
        Task::batch(tasks)
    }
    fn update_inner(&mut self, event: Event) -> Task<Event> {
        match event {
            Event::ImageReady(epoch, path, result) => {
                self.image_pending.remove(&path);
                if epoch == self.image_epoch {
                    self.image_cache.insert(path, result);
                }
            }
            Event::Key(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                let chord = normalized_chord(&key, modifiers);
                if let Some(action) = vasya_native::shortcut_action(&self.vm.preferences, &chord) {
                    if action == "focus_search" {
                        return widget::operation::focus(widget::Id::new("chat-search"));
                    }
                    if matches!(
                        action.as_str(),
                        "close_panel" | "close_chat" | "open_settings" | "search_in_chat"
                    ) {
                        self.menu = false;
                        self.markdown = None;
                        self.inspector = None;
                    }
                    self.app.send(UiCommand::Shortcut(action));
                } else if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) {
                    self.menu = false;
                    self.markdown = None;
                    self.inspector = None;
                    self.app.send(UiCommand::CloseOverlay);
                }
            }
            Event::Key(_) => {}

            Event::StressTick(sent) => {
                self.metrics
                    .borrow_mut()
                    .timer_dispatch
                    .push(sent.elapsed().as_micros() as u64);
                self.stress_ticks += 1;
                if self.pending_draft.is_none() {
                    let draft = format!("Stress input {} — native UI", self.stress_ticks);
                    self.editor = text_editor::Content::with_text(&draft);
                    self.pending_draft = Some((draft.clone(), Instant::now()));
                    self.app.send(UiCommand::Draft(draft));
                }
                if self.stress_ticks % 25 == 0 {
                    self.app.send(UiCommand::LoadOlder);
                }
                let y = if self.stress_ticks % 20 < 10 {
                    0.25
                } else {
                    0.8
                };
                return Task::batch([
                    widget::operation::snap_to(
                        history_id(),
                        scrollable::RelativeOffset { x: 0., y },
                    ),
                    widget::operation::snap_to(
                        widget::Id::new("chat-list"),
                        scrollable::RelativeOffset { x: 0., y },
                    ),
                ]);
            }
            Event::Snapshot(vm, delivered) => {
                if self.stress {
                    let mut metrics = self.metrics.borrow_mut();
                    metrics
                        .watch_dispatch
                        .push(delivered.elapsed().as_micros() as u64);
                    metrics.snapshots += 1;
                    if let Some((draft, started)) = &self.pending_draft {
                        if vm.draft == *draft {
                            metrics.draft_ack.push(started.elapsed().as_micros() as u64);
                            self.pending_draft = None;
                        }
                    }
                }
                let switched = self.vm.selected_account != vm.selected_account
                    || self.vm.selected_chat != vm.selected_chat
                    || self.vm.selected_topic != vm.selected_topic;
                let jump = vm
                    .jump_to
                    .filter(|id| switched || self.vm.jump_to != Some(*id));
                if !Arc::ptr_eq(&self.vm.search_hits, &vm.search_hits) {
                    self.search_offset = 0.;
                }
                let anchor_index = self
                    .prefix
                    .partition_point(|y| *y <= self.history_offset)
                    .saturating_sub(1);
                let anchor = self
                    .vm
                    .messages
                    .get(anchor_index)
                    .map(|msg| (msg.id, self.history_offset - self.prefix[anchor_index]));
                let form_changed = self.vm.form.as_ref().map(|f| (&f.title, &f.description))
                    != vm.form.as_ref().map(|f| (&f.title, &f.description));
                if form_changed {
                    self.form = vm
                        .form
                        .as_ref()
                        .map(|f| {
                            f.fields
                                .iter()
                                .map(|v| (v.key.clone(), v.value.clone()))
                                .collect()
                        })
                        .unwrap_or_default();
                }
                if switched
                    || (vm.draft.is_empty()
                        && self.sent_draft.as_ref() == Some(&self.editor.text()))
                {
                    self.editor = text_editor::Content::with_text(&vm.draft);
                    self.sent_draft = None;
                }
                if switched {
                    self.selected_messages.clear();
                    self.visible.clear();
                    self.image_epoch = self.image_epoch.wrapping_add(1);
                    self.image_cache = PreviewCache::default();
                    self.height_cache.clear();
                }
                let history_changed = !Arc::ptr_eq(&self.vm.messages, &vm.messages)
                    || self.vm.preferences.text_size != vm.preferences.text_size;
                self.vm = vm;
                if history_changed {
                    self.rebuild_heights();
                }
                if let Some(id) = jump {
                    if let Some(index) =
                        self.vm.messages.iter().position(|message| message.id == id)
                    {
                        self.bottom = false;
                        self.history_offset =
                            (self.prefix[index] - self.viewport_height * 0.25).max(0.);
                        self.report_visible();
                        return Task::batch([
                            widget::operation::scroll_to(
                                history_id(),
                                scrollable::AbsoluteOffset {
                                    x: 0.,
                                    y: self.history_offset,
                                },
                            ),
                            self.schedule_layout(),
                        ]);
                    }
                }
                if switched || (self.bottom && history_changed) {
                    self.bottom = true;
                    self.history_offset =
                        (self.prefix.last().copied().unwrap_or(0.) - self.viewport_height).max(0.);
                    self.report_visible();
                    return Task::batch([
                        widget::operation::snap_to(history_id(), scrollable::RelativeOffset::END),
                        self.schedule_layout(),
                    ]);
                }
                if let Some((id, delta)) = anchor {
                    if let Some(index) = self.vm.messages.iter().position(|m| m.id == id) {
                        let offset = self.prefix[index] + delta;
                        if (offset - self.history_offset).abs() > 1. {
                            self.history_offset = offset;
                            return Task::batch([
                                widget::operation::scroll_to(
                                    history_id(),
                                    scrollable::AbsoluteOffset { x: 0., y: offset },
                                ),
                                self.schedule_layout(),
                            ]);
                        }
                    }
                }
            }
            Event::Command(command) => {
                if matches!(command, UiCommand::OpenForm(_)) {
                    self.menu = false;
                    self.markdown = None;
                    self.inspector = None;
                }
                if matches!(command, UiCommand::CloseOverlay) {
                    self.menu = false;
                    self.markdown = None;
                    self.inspector = None;
                }
                if matches!(command, UiCommand::Send) {
                    self.bottom = true;
                    self.sent_draft = Some(self.editor.text());
                }
                self.app.send(command);
            }
            Event::FormValue(key, value) => {
                self.form.insert(key, value);
            }
            Event::Submit => self.app.send(UiCommand::SubmitForm(self.form.clone())),
            Event::Edit(action) => {
                self.editor.perform(action);
                self.app.send(UiCommand::Draft(self.editor.text()));
            }
            Event::Inspect(value) => self.inspector = Some(text_editor::Content::with_text(&value)),
            Event::SelectText(action) => {
                if !action.is_edit() {
                    if let Some(content) = self.inspector.as_mut() {
                        content.perform(action);
                    }
                }
            }
            Event::Markdown(value) => {
                self.markdown = Some(markdown::parse(&value).collect());
            }
            Event::OpenLink(uri) => {
                if uri.starts_with("https://") || uri.starts_with("http://") {
                    #[cfg(target_os = "macos")]
                    {
                        let _ = std::process::Command::new("/usr/bin/open").arg(uri).spawn();
                    }
                    #[cfg(target_os = "linux")]
                    {
                        let _ = std::process::Command::new("xdg-open").arg(uri).spawn();
                    }
                    #[cfg(target_os = "windows")]
                    {
                        let _ = std::process::Command::new("explorer").arg(uri).spawn();
                    }
                }
            }
            Event::Copy(value) => return iced::clipboard::write(value),
            Event::PasteImage => {
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(|| {
                            let mut clipboard =
                                arboard::Clipboard::new().map_err(|e| e.to_string())?;
                            let data = clipboard.get_image().map_err(|_| {
                                "The clipboard does not contain an image.".to_owned()
                            })?;
                            let image = ::image::RgbaImage::from_raw(
                                data.width as u32,
                                data.height as u32,
                                data.bytes.into_owned(),
                            )
                            .ok_or("Invalid clipboard image")?;
                            let mut png = std::io::Cursor::new(Vec::new());
                            ::image::DynamicImage::ImageRgba8(image)
                                .write_to(&mut png, ::image::ImageFormat::Png)
                                .map_err(|e| e.to_string())?;
                            Ok(png.into_inner())
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Event::ClipboardImage,
                )
            }
            Event::ClipboardImage(result) => match result {
                Ok(bytes) => {
                    self.clipboard_error = None;
                    self.app.send(UiCommand::SendClipboardImage {
                        bytes,
                        extension: "png".into(),
                    });
                }
                Err(error) => self.clipboard_error = Some(error),
            },
            Event::PickFile => {
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .pick_file()
                            .await
                            .map(|file| file.path().to_path_buf())
                    },
                    Event::File,
                )
            }
            Event::File(Some(path)) => self.app.send(UiCommand::SendFile(path)),
            Event::File(None) => {}
            Event::Menu => self.menu = !self.menu,
            Event::ChatsScrolled(y) => self.chat_offset = y,
            Event::ResultsScrolled(y) => self.search_offset = y,
            Event::SelectMessage(id, selected) => {
                if id > 0 {
                    if selected {
                        self.selected_messages.insert(id);
                    } else {
                        self.selected_messages.remove(&id);
                    }
                }
            }
            Event::ClearSelection => self.selected_messages.clear(),
            Event::HistoryScrolled(y, height, maximum) => {
                self.history_offset = y;
                self.viewport_height = height;
                self.bottom = maximum - y < 48.;
            }
            Event::Resized(width) => {
                self.width = width;
                self.rebuild_heights();
            }
            Event::LayoutReady(messages, width, size, heights, cache) => {
                self.layout_busy = false;
                if let Some(started) = self.layout_started.take() {
                    if self.stress {
                        self.metrics
                            .borrow_mut()
                            .layout_completion
                            .push(started.elapsed().as_micros() as u64);
                    }
                }
                self.height_cache = cache;
                if Arc::ptr_eq(&messages, &self.vm.messages)
                    && width == (self.width - 450.).max(80.)
                    && size == self.vm.preferences.text_size
                {
                    let index = self
                        .prefix
                        .partition_point(|y| *y <= self.history_offset)
                        .saturating_sub(1)
                        .min(self.heights.len());
                    let delta = self.history_offset - self.prefix[index];
                    self.heights = heights;
                    self.rebuild_prefix();
                    self.layout_pending = false;
                    self.history_offset = if self.bottom {
                        (self.prefix.last().copied().unwrap_or(0.) - self.viewport_height).max(0.)
                    } else {
                        self.prefix[index] + delta
                    };
                    self.report_visible();
                    return widget::operation::scroll_to(
                        history_id(),
                        scrollable::AbsoluteOffset {
                            x: 0.,
                            y: self.history_offset,
                        },
                    );
                }
                self.layout_pending = true;
            }
            Event::Exit => {
                if self.stress {
                    println!(
                        "VASYA_STRESS_METRICS {}",
                        self.metrics.borrow().json(
                            self.vm.chats.len(),
                            self.vm.messages.len(),
                            self.stress_started.elapsed().as_secs_f64()
                        )
                    );
                } else {
                    println!("Vasya Iced native window startup: OK");
                }
                return iced::exit();
            }
        }
        self.report_visible();
        self.schedule_layout()
    }
    fn report_visible(&mut self) {
        if !self.history_is_visible() {
            if !self.visible.is_empty() {
                self.visible.clear();
                if let (Some(account), Some(chat)) =
                    (&self.vm.selected_account, self.vm.selected_chat)
                {
                    self.app.send(UiCommand::VisibleMessages {
                        account: account.clone(),
                        chat,
                        topic: self.vm.selected_topic,
                        ids: Vec::new(),
                    });
                }
            }
            return;
        }
        let start = self
            .prefix
            .partition_point(|y| *y < self.history_offset)
            .saturating_sub(1)
            .min(self.vm.messages.len());
        let end = self
            .prefix
            .partition_point(|y| *y < self.history_offset + self.viewport_height)
            .min(self.vm.messages.len());
        let ids: Vec<_> = self.vm.messages[start..end].iter().map(|m| m.id).collect();
        if self.vm.preferences.markdown {
            for message in &self.vm.messages[start..end] {
                let key = height_key(message, 0., self.vm.preferences.text_size);
                if self.rendered.get(&message.id).map(|(cached, _)| *cached) != Some(key) {
                    self.rendered
                        .insert(message.id, (key, markdown::parse(&message.text).collect()));
                }
            }
            self.rendered.retain(|id, _| ids.contains(id));
        } else {
            self.rendered.clear();
        }
        if ids != self.visible {
            self.visible = ids.clone();
            if let (Some(account), Some(chat)) = (&self.vm.selected_account, self.vm.selected_chat)
            {
                self.app.send(UiCommand::VisibleMessages {
                    account: account.clone(),
                    chat,
                    topic: self.vm.selected_topic,
                    ids,
                });
            }
        }
    }
    fn view(&self) -> Element<'_, Event> {
        if !self.stress {
            return self.view_inner();
        }
        let start = Instant::now();
        let view = self.view_inner();
        self.metrics
            .borrow_mut()
            .views
            .push(start.elapsed().as_micros() as u64);
        view
    }
    fn view_inner(&self) -> Element<'_, Event> {
        let vm = &self.vm;
        let mut accounts = row![].spacing(5);
        for account in &vm.accounts {
            accounts = accounts.push(
                button(text(&account.title).size(12))
                    .on_press(Event::Command(UiCommand::SelectAccount(account.id.clone())))
                    .style(if vm.selected_account.as_ref() == Some(&account.id) {
                        button::primary
                    } else {
                        button::secondary
                    }),
            );
        }
        let mut folder_buttons: Vec<Element<'_, Event>> = vec![button(text(self.tr("All")))
            .on_press(Event::Command(UiCommand::SelectFolder(None)))
            .style(button::text)
            .into()];
        for folder in &vm.folders {
            folder_buttons.push(
                button(text(&folder.title).size(12))
                    .on_press(Event::Command(UiCommand::SelectFolder(Some(
                        folder.id.clone(),
                    ))))
                    .style(if vm.selected_folder.as_ref() == Some(&folder.id) {
                        button::primary
                    } else {
                        button::text
                    })
                    .into(),
            );
        }
        let folders: Element<'_, Event> = if vm.preferences.folder_layout == "vertical" {
            scrollable(widget::Column::with_children(folder_buttons).spacing(3))
                .height((vm.folders.len() as f32 * 36. + 36.).min(180.))
                .into()
        } else {
            scrollable(widget::Row::with_children(folder_buttons).spacing(3))
                .direction(scrollable::Direction::Horizontal(Default::default()))
                .into()
        };
        let chat_height = match vm.preferences.density.as_str() {
            "compact" => 64.,
            "very-compact" => 52.,
            _ => 76.,
        };
        let start = ((self.chat_offset / chat_height) as usize)
            .saturating_sub(3)
            .min(vm.chats.len());
        let end = (start + 30).min(vm.chats.len());
        let mut chats = column![space().height(start as f32 * chat_height)];
        for chat in &vm.chats[start..end] {
            let title = row![
                text(&chat.title).size(15).width(Fill),
                text(if chat.unread > 0 {
                    chat.unread.to_string()
                } else {
                    String::new()
                })
                .size(12)
            ]
            .spacing(8);
            chats = chats.push(
                button(column![title, text(truncate(&chat.preview, 42)).size(12)].spacing(7))
                    .width(Fill)
                    .height(chat_height)
                    .padding(if chat_height < 60. {
                        4
                    } else if chat_height < 70. {
                        8
                    } else {
                        12
                    })
                    .style(if vm.selected_chat == Some(chat.id) {
                        button::primary
                    } else {
                        button::text
                    })
                    .on_press(Event::Command(UiCommand::SelectChat(chat.id))),
            );
        }
        chats = chats.push(space().height((vm.chats.len() - end) as f32 * chat_height));
        let sidebar = container(
            column![
                row![
                    text("Vasya").size(26).width(Fill),
                    button("☰").on_press(Event::Menu)
                ]
                .align_y(iced::Center),
                scrollable(accounts)
                    .direction(scrollable::Direction::Horizontal(Default::default())),
                text_input(&self.tr("Search conversations"), &vm.search)
                    .id(widget::Id::new("chat-search"))
                    .on_input(|v| Event::Command(UiCommand::Search(v)))
                    .padding(11),
                folders,
                scrollable(chats)
                    .id(widget::Id::new("chat-list"))
                    .on_scroll(|viewport| Event::ChatsScrolled(viewport.absolute_offset().y))
                    .height(Fill),
                text(format!(
                    "{} · {}",
                    if vm.remote { "Remote" } else { "Telegram" },
                    vm.version
                ))
                .size(11),
            ]
            .spacing(12),
        )
        .padding(16)
        .width(310)
        .height(Fill)
        .style(container::rounded_box);
        let content: Element<'_, Event> = if self.menu {
            self.menu_view()
        } else if let Some(content) = &self.inspector {
            container(
                column![
                    row![
                        text(self.tr("Message text")).size(24).width(Fill),
                        self.action("Close", UiCommand::CloseOverlay)
                    ],
                    text_editor(content)
                        .on_action(Event::SelectText)
                        .height(Fill)
                ]
                .spacing(20),
            )
            .padding(24)
            .width(Fill)
            .into()
        } else if let Some(items) = &self.markdown {
            container(
                column![
                    row![
                        text(self.tr("Formatted message")).size(24).width(Fill),
                        self.action("Close", UiCommand::CloseOverlay)
                    ],
                    scrollable(
                        markdown::view(items, if vm.dark { Theme::Dark } else { Theme::Light })
                            .map(Event::OpenLink)
                    )
                    .height(Fill)
                ]
                .spacing(20),
            )
            .padding(24)
            .width(Fill)
            .into()
        } else if let Some(form) = &vm.form {
            let mut fields =
                column![text(&form.title).size(26), text(&form.description).size(14)].spacing(16);
            for field in &form.fields {
                let key = field.key.clone();
                let value = self
                    .form
                    .get(&key)
                    .map(String::as_str)
                    .unwrap_or(&field.value);
                if matches!(value, "true" | "false") {
                    fields = fields.push(
                        widget::checkbox(value == "true")
                            .label(field.label.trim_end_matches(" (true/false)"))
                            .on_toggle(move |checked| {
                                Event::FormValue(key.clone(), checked.to_string())
                            }),
                    );
                    continue;
                }
                let choices: Option<&'static [&'static str]> = match field.key.as_str() {
                    "provider" => Some(&["deepgram", "local_whisper"]),
                    "ui_language" => Some(&["en", "ru"]),
                    "density" => Some(&["normal", "compact", "very-compact"]),
                    "folder_layout" => Some(&["horizontal", "vertical"]),
                    "whisper_model" => Some(&["tiny", "base", "small", "medium"]),
                    _ => None,
                };
                if let Some(choices) = choices {
                    let selected = choices.iter().copied().find(|choice| *choice == value);
                    fields = fields.push(
                        column![
                            text(&field.label).size(13),
                            widget::pick_list(choices, selected, move |selected| Event::FormValue(
                                key.clone(),
                                selected.to_owned()
                            ))
                            .padding(12)
                        ]
                        .spacing(6),
                    );
                    continue;
                }
                fields = fields.push(
                    column![
                        text(&field.label).size(13),
                        text_input(&field.label, value)
                            .secure(field.secret)
                            .on_input(move |value| Event::FormValue(key.clone(), value))
                            .on_submit(Event::Submit)
                            .padding(12)
                    ]
                    .spacing(6),
                );
            }
            fields = fields.push(
                row![
                    button(text(&form.submit_label))
                        .on_press(Event::Submit)
                        .padding(12),
                    self.action("Cancel", UiCommand::CloseOverlay)
                ]
                .spacing(10),
            );
            container(scrollable(fields).height(Fill))
                .padding(32)
                .width(Fill)
                .into()
        } else if !vm.search_hits.is_empty() {
            let start = ((self.search_offset / 104.) as usize)
                .saturating_sub(3)
                .min(vm.search_hits.len());
            let end = (start + 24).min(vm.search_hits.len());
            let mut results = column![space().height(start as f32 * 104.)];
            for hit in &vm.search_hits[start..end] {
                results = results.push(
                    button(
                        column![
                            text(&hit.title).size(15),
                            text(truncate(&hit.text, 140)).size(13)
                        ]
                        .spacing(8),
                    )
                    .width(Fill)
                    .height(104)
                    .padding(12)
                    .style(button::secondary)
                    .on_press(Event::Command(UiCommand::JumpToMessage(
                        hit.chat_id,
                        hit.message_id,
                    ))),
                );
            }
            results = results.push(space().height((vm.search_hits.len() - end) as f32 * 104.));
            container(
                column![
                    row![
                        text(self.tr("Search results")).size(24).width(Fill),
                        self.action("Close", UiCommand::CloseOverlay)
                    ],
                    scrollable(results)
                        .on_scroll(|viewport| Event::ResultsScrolled(viewport.absolute_offset().y))
                        .height(Fill)
                ]
                .spacing(16),
            )
            .padding(24)
            .width(Fill)
            .into()
        } else if let Some((title, detail)) = &vm.detail {
            container(
                column![
                    row![
                        text(title).size(26).width(Fill),
                        self.action("Close", UiCommand::CloseOverlay)
                    ],
                    scrollable(text(detail)).height(Fill)
                ]
                .spacing(20),
            )
            .padding(28)
            .width(Fill)
            .into()
        } else if vm.selected_chat.is_none() {
            container(
                column![
                    text(self.tr("Your conversations, in focus.")).size(30),
                    text(self.tr("Native Rust. Background updates. A quieter place to talk."))
                        .size(16),
                    row![
                        self.action("Sign in", UiCommand::OpenForm(FormKind::Login)),
                        self.action("Connect server", UiCommand::OpenForm(FormKind::Remote))
                    ]
                    .spacing(10),
                    self.action(
                        "Telegram API credentials",
                        UiCommand::OpenForm(FormKind::Credentials)
                    )
                ]
                .spacing(22),
            )
            .center_x(Fill)
            .center_y(Fill)
            .padding(32)
            .into()
        } else {
            self.conversation()
        };
        let mut layout = column![row![sidebar, content].spacing(1).height(Fill)];
        if let Some(error) = &vm.error {
            layout = layout.push(
                container(
                    row![
                        text(error).size(13).width(Fill),
                        self.action("Dismiss", UiCommand::DismissError)
                    ]
                    .spacing(10),
                )
                .padding(10)
                .style(container::rounded_box),
            );
        }
        layout = layout.push(
            container(row![
                text(self.clipboard_error.as_deref().unwrap_or(&vm.status))
                    .size(12)
                    .width(Fill),
                text(if vm.busy { "Working…" } else { "" }).size(12)
            ])
            .padding([6, 16]),
        );
        container(layout).width(Fill).height(Fill).into()
    }
    fn menu_view(&self) -> Element<'_, Event> {
        let mut menu = column![row![
            text(self.tr("Vasya settings")).size(28).width(Fill),
            button(text(self.tr("Done"))).on_press(Event::Menu)
        ]]
        .spacing(12);
        for (label, kind) in [
            ("Telegram API credentials", FormKind::Credentials),
            ("Add account", FormKind::Login),
            ("Remote server", FormKind::Remote),
            ("Preferences", FormKind::Settings),
            ("Storage", FormKind::Storage),
            ("Tabs", FormKind::Tabs),
            ("Keyboard shortcuts", FormKind::Hotkeys),
            ("Transcription", FormKind::Stt),
            ("Local API", FormKind::LocalApi),
            ("Create group", FormKind::CreateGroup),
            ("Create channel", FormKind::CreateChannel),
            ("Create folder", FormKind::Folder),
        ] {
            menu = menu.push(self.action(label, UiCommand::OpenForm(kind)));
        }
        menu = menu.push(
            row![
                self.action("Embedded mode", UiCommand::EmbeddedMode),
                self.action("Light / dark", UiCommand::ToggleTheme),
                self.action("Refresh", UiCommand::Refresh)
            ]
            .spacing(8),
        );
        menu = menu.push(self.action("Downloads", UiCommand::Downloads));
        if self.vm.selected_chat.is_some() {
            menu = menu.push(self.action("Favorite / unfavorite", UiCommand::ToggleFavorite));
        }
        if self.vm.selected_folder.is_some() {
            menu = menu.push(self.action("Delete folder", UiCommand::DeleteFolder));
        }
        menu = menu.push(self.action("Sign out of selected account", UiCommand::Logout));
        container(scrollable(menu)).padding(32).width(Fill).into()
    }
    fn conversation(&self) -> Element<'_, Event> {
        let vm = &self.vm;
        let header = row![
            text(&vm.chat_title).size(22).width(Fill),
            self.action("Search", UiCommand::OpenForm(FormKind::SearchMessages)),
            self.action("Info", UiCommand::ChatInfo),
            self.action("Calls unavailable", UiCommand::Calls),
            self.action("Leave", UiCommand::LeaveChat)
        ]
        .spacing(6)
        .align_y(iced::Center);
        let mut topics = row![].spacing(5);
        if !vm.topics.is_empty() {
            topics = topics.push(self.action("All topics", UiCommand::SelectTopic(None)));
        }
        for topic in &vm.topics {
            topics = topics.push(
                button(text(&topic.title).size(12))
                    .on_press(Event::Command(UiCommand::SelectTopic(Some(topic.id)))),
            );
        }
        let start = self
            .prefix
            .partition_point(|y| *y < (self.history_offset - 400.).max(0.))
            .saturating_sub(1)
            .min(vm.messages.len());
        let end = self
            .prefix
            .partition_point(|y| *y < self.history_offset + self.viewport_height + 600.)
            .min(vm.messages.len());
        let mut history =
            column![space().height(self.prefix.get(start).copied().unwrap_or(0.))].spacing(0);
        for (index, message) in vm.messages.iter().enumerate().take(end).skip(start) {
            let show_sender = !vm.preferences.merge_messages
                || index == 0
                || vm.messages[index - 1].sender != message.sender
                || vm.messages[index - 1].outgoing != message.outgoing;
            history = history.push(self.message(message, self.heights[index], show_sender));
        }
        history = history.push(
            space().height(
                (self.prefix.last().copied().unwrap_or(0.)
                    - self.prefix.get(end).copied().unwrap_or(0.))
                .max(0.),
            ),
        );
        let history = scrollable(history)
            .id(history_id())
            .on_scroll(|v| {
                Event::HistoryScrolled(
                    v.absolute_offset().y,
                    v.bounds().height,
                    (v.content_bounds().height - v.bounds().height).max(0.),
                )
            })
            .height(Fill);
        let editor = text_editor(&self.editor)
            .placeholder(if self.vm.preferences.language == "ru" {
                "Написать сообщение…"
            } else {
                "Write a message…"
            })
            .on_action(Event::Edit)
            .key_binding(|press| {
                if press.key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter)
                    && !press.modifiers.shift()
                {
                    Some(text_editor::Binding::Custom(Event::Command(
                        UiCommand::Send,
                    )))
                } else {
                    text_editor::Binding::from_key_press(press)
                }
            })
            .height(74)
            .padding(12);
        let mut body = column![
            header,
            scrollable(topics).direction(scrollable::Direction::Horizontal(Default::default()))
        ]
        .spacing(12);
        if !self.selected_messages.is_empty() {
            let mut ids: Vec<_> = self.selected_messages.iter().copied().collect();
            ids.sort_unstable();
            body = body.push(
                row![
                    text(format!("{} {}", ids.len(), self.tr("selected"))).width(Fill),
                    self.action("Forward selected", UiCommand::ForwardMany(ids)),
                    button(text(self.tr("Clear"))).on_press(Event::ClearSelection)
                ]
                .spacing(8),
            );
        }
        if vm.has_older {
            body = body.push(self.action("Load earlier messages", UiCommand::LoadOlder));
        }
        body = body.push(history).push(editor).push(
            row![
                button(text(self.tr("Attach file"))).on_press(Event::PickFile),
                button(text(self.tr("Paste image"))).on_press(Event::PasteImage),
                self.action("Voice", UiCommand::RecordVoice),
                self.action("Camera", UiCommand::Camera),
                space().width(Fill),
                self.action("Send", UiCommand::Send)
            ]
            .spacing(8),
        );
        container(body).padding(20).width(Fill).height(Fill).into()
    }
    fn message<'a>(
        &'a self,
        msg: &'a MessageView,
        height: f32,
        show_sender: bool,
    ) -> Element<'a, Event> {
        let body: Element<'a, Event> = if let Some((_, items)) = self
            .rendered
            .get(&msg.id)
            .filter(|_| self.vm.preferences.markdown)
        {
            let extra = if msg.image_path.is_some() { 187. } else { 0. }
                + if msg.media_kind.is_some() { 38. } else { 0. };
            scrollable(
                markdown::view(
                    items,
                    markdown::Settings::with_text_size(
                        self.vm.preferences.text_size,
                        if self.vm.dark {
                            Theme::Dark
                        } else {
                            Theme::Light
                        },
                    ),
                )
                .map(Event::OpenLink),
            )
            .height((height - 100. - extra).max(30.))
            .into()
        } else {
            text(&msg.text)
                .size(self.vm.preferences.text_size)
                .line_height(self.vm.preferences.text_size * 1.5)
                .shaping(iced::advanced::text::Shaping::Advanced)
                .into()
        };
        let selector: Element<'a, Event> = if msg.id > 0 {
            widget::checkbox(self.selected_messages.contains(&msg.id))
                .on_toggle(move |selected| Event::SelectMessage(msg.id, selected))
                .into()
        } else {
            space().width(20).into()
        };
        let mut content = column![
            row![
                selector,
                text(if show_sender { msg.sender.as_str() } else { "" })
                    .size(12)
                    .width(Fill),
                text(format!(
                    "{}{}",
                    msg.time,
                    if msg.failed {
                        " · failed"
                    } else if msg.pending {
                        " · sending"
                    } else {
                        ""
                    }
                ))
                .size(11)
            ]
            .spacing(10),
            body
        ]
        .spacing(7);
        if let Some(path) = &msg.image_path {
            let preview: Element<'a, Event> = match self
                .image_cache
                .entries
                .get(path)
                .map(|entry| &entry.result)
            {
                Some(Ok(decoded)) => image(decoded.handle.clone())
                    .height(180)
                    .content_fit(iced::ContentFit::Contain)
                    .into(),
                Some(Err(error)) => container(
                    text(format!("{}: {}", self.tr("Preview unavailable"), error)).size(12),
                )
                .height(180)
                .center_y(180)
                .into(),
                None => container(text(self.tr("Loading preview…")).size(12))
                    .height(180)
                    .center_y(180)
                    .into(),
            };
            content = content.push(preview);
        }
        if let Some(kind) = &msg.media_kind {
            content = content.push(
                row![
                    text(kind).size(12),
                    self.action("Download", UiCommand::Download(msg.id)),
                    self.action("Open", UiCommand::OpenDownloaded(msg.id)),
                    self.action("Transcribe", UiCommand::Transcribe(msg.id))
                ]
                .spacing(5),
            );
        }
        if let Some(value) = &msg.transcription {
            content = content.push(
                text(value)
                    .size(self.vm.preferences.text_size)
                    .line_height(self.vm.preferences.text_size * 1.5)
                    .shaping(iced::advanced::text::Shaping::Advanced),
            );
        }
        let mut actions = row![
            self.action("Forward", UiCommand::Forward(msg.id)),
            button(text("Select").size(12))
                .padding([7, 10])
                .style(button::secondary)
                .on_press(Event::Inspect(msg.text.clone())),
            button(text("Format").size(12))
                .padding([7, 10])
                .style(button::secondary)
                .on_press(Event::Markdown(msg.text.clone())),
            button(text("Copy").size(12))
                .padding([7, 10])
                .style(button::secondary)
                .on_press(Event::Copy(msg.text.clone()))
        ]
        .spacing(5);
        if msg.failed {
            actions = actions.push(self.action("Retry", UiCommand::Retry(msg.id)));
        }
        content = content.push(actions);
        let bubble = container(content)
            .padding(12)
            .width(Fill)
            .style(if msg.outgoing {
                container::bordered_box
            } else {
                container::rounded_box
            });
        container(bubble)
            .padding(iced::Padding {
                top: 5.,
                bottom: 5.,
                left: if msg.outgoing { 56. } else { 8. },
                right: if msg.outgoing { 8. } else { 56. },
            })
            .height(height)
            .width(Fill)
            .into()
    }
    fn tr(&self, label: &str) -> String {
        vasya_native::tr(&self.vm.preferences.language, label)
    }

    fn action<'a>(&self, label: &str, command: UiCommand) -> Element<'a, Event> {
        button(text(self.tr(label)).size(12))
            .padding([7, 10])
            .on_press(Event::Command(command))
            .style(button::secondary)
            .into()
    }
}
fn history_id() -> widget::Id {
    widget::Id::new("message-history")
}
fn truncate(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}
fn measure_text(value: &str, width: f32, size: f32) -> f32 {
    use iced::advanced::text::{LineHeight, Paragraph, Shaping, Text, Wrapping};
    let paragraph = iced::advanced::graphics::text::Paragraph::with_text(Text {
        content: value,
        bounds: iced::Size::new(width, f32::INFINITY),
        size: size.into(),
        line_height: LineHeight::Absolute((size * 1.5).into()),
        font: iced::Font::DEFAULT,
        align_x: Default::default(),
        align_y: iced::alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: Wrapping::WordOrGlyph,
    });
    paragraph.min_bounds().height.max(size * 1.5)
}
fn message_height(message: &MessageView, width: f32, size: f32) -> f32 {
    100. + measure_text(&message.text, width, size)
        + if message.image_path.is_some() {
            187.
        } else {
            0.
        }
        + if message.media_kind.is_some() {
            38.
        } else {
            0.
        }
        + message
            .transcription
            .as_ref()
            .map(|text| measure_text(text, width, size) + 7.)
            .unwrap_or(0.)
}

fn height_key(message: &MessageView, width: f32, size: f32) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    message.text.hash(&mut hash);
    message.transcription.hash(&mut hash);
    message.image_path.hash(&mut hash);
    message.media_kind.hash(&mut hash);
    width.to_bits().hash(&mut hash);
    size.to_bits().hash(&mut hash);
    hash.finish()
}
fn estimate_height(message: &MessageView, width: f32, size: f32) -> f32 {
    let line = |value: &str| {
        value
            .lines()
            .map(|line| ((line.len() as f32 * size * 0.55 / width).ceil()).max(1.) * size * 1.5)
            .sum::<f32>()
            .max(22.)
    };
    100. + line(&message.text)
        + if message.image_path.is_some() {
            187.
        } else {
            0.
        }
        + if message.media_kind.is_some() {
            38.
        } else {
            0.
        }
        + message
            .transcription
            .as_ref()
            .map(|v| line(v) + 7.)
            .unwrap_or(0.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_accounts_for_wrapping_and_media() {
        let short = MessageView {
            text: "Hello".into(),
            ..Default::default()
        };
        let long = MessageView {
            text: "Привет, мир! A message that wraps. ".repeat(20),
            ..Default::default()
        };
        assert!(message_height(&long, 180., 15.) > message_height(&long, 600., 15.));
        assert!(message_height(&long, 600., 15.) > message_height(&short, 600., 15.));
        let media = MessageView {
            image_path: Some("preview.png".into()),
            ..short.clone()
        };
        assert_eq!(
            message_height(&media, 600., 15.) - message_height(&short, 600., 15.),
            187.
        );
    }

    #[test]
    fn layout_cache_invalidates_for_content_and_width() {
        let message = MessageView {
            text: "Original".into(),
            ..Default::default()
        };
        let edited = MessageView {
            text: "Edited".into(),
            ..message.clone()
        };
        assert_ne!(
            height_key(&message, 300., 15.),
            height_key(&edited, 300., 15.)
        );
        assert_ne!(
            height_key(&message, 300., 15.),
            height_key(&message, 500., 15.)
        );
    }
}

#[derive(Default)]
struct Metrics {
    updates: Vec<u64>,
    views: Vec<u64>,
    watch_dispatch: Vec<u64>,
    layout_completion: Vec<u64>,
    draft_ack: Vec<u64>,
    timer_dispatch: Vec<u64>,
    snapshots: u64,
}
impl Metrics {
    fn json(&self, chats: usize, loaded_messages: usize, elapsed: f64) -> serde_json::Value {
        fn stats(values: &[u64]) -> serde_json::Value {
            let mut sorted = values.to_vec();
            sorted.sort_unstable();
            let p95 = sorted
                .get(((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1))
                .copied()
                .unwrap_or(0);
            serde_json::json!({"samples": sorted.len(), "p95_ms": p95 as f64 / 1000., "max_ms": sorted.last().copied().unwrap_or(0) as f64 / 1000.})
        }
        serde_json::json!({"gui":"iced", "target_duration_seconds":30, "duration_seconds":elapsed, "chats":chats, "loaded_messages":loaded_messages, "snapshots":self.snapshots,
            "update_handler":stats(&self.updates), "view_construction":stats(&self.views),
            "watch_delivery_to_handler":stats(&self.watch_dispatch), "background_layout_completion":stats(&self.layout_completion),
            "scripted_draft_to_snapshot":stats(&self.draft_ack), "timer_to_handler":stats(&self.timer_dispatch),
            "limitations":"Scripted application events, not physical input latency. View construction excludes widget layout, GPU submission, presentation and frame time."})
    }
}

fn normalized_chord(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> String {
    use iced::keyboard::{key::Named, Key};
    let name = match key {
        Key::Character(value) => value.to_lowercase(),
        Key::Named(Named::ArrowDown) => "down".into(),
        Key::Named(Named::ArrowUp) => "up".into(),
        Key::Named(Named::ArrowLeft) => "left".into(),
        Key::Named(Named::ArrowRight) => "right".into(),
        Key::Named(value) => format!("{value:?}").to_lowercase(),
        _ => return String::new(),
    };
    let mut parts = Vec::new();
    if modifiers.logo() {
        parts.push("meta".to_owned());
    }
    if modifiers.control() {
        parts.push("ctrl".to_owned());
    }
    if modifiers.alt() {
        parts.push("alt".to_owned());
    }
    if modifiers.shift() {
        parts.push("shift".to_owned());
    }
    parts.push(name);
    parts.join("+")
}

const PREVIEW_CACHE_BYTES: usize = 32 * 1024 * 1024;
const PREVIEW_CACHE_ENTRIES: usize = 32;
const PREVIEW_DECODE_BYTES: u64 = 64 * 1024 * 1024;
const PREVIEW_EDGE: u32 = 512;
#[derive(Clone, Debug)]
struct DecodedPreview {
    handle: image::Handle,
    bytes: usize,
}
struct PreviewEntry {
    result: Result<DecodedPreview, String>,
    stamp: u64,
}
#[derive(Default)]
struct PreviewCache {
    entries: HashMap<PathBuf, PreviewEntry>,
    bytes: usize,
    stamp: u64,
}
impl PreviewCache {
    fn touch(&mut self, path: &std::path::Path) -> bool {
        if let Some(entry) = self.entries.get_mut(path) {
            self.stamp += 1;
            entry.stamp = self.stamp;
            true
        } else {
            false
        }
    }
    fn insert(&mut self, path: PathBuf, result: Result<DecodedPreview, String>) {
        if let Some(old) = self.entries.remove(&path) {
            self.bytes -= old.result.as_ref().map(|image| image.bytes).unwrap_or(0);
        }
        self.stamp += 1;
        self.bytes += result.as_ref().map(|image| image.bytes).unwrap_or(0);
        self.entries.insert(
            path,
            PreviewEntry {
                result,
                stamp: self.stamp,
            },
        );
        while self.bytes > PREVIEW_CACHE_BYTES || self.entries.len() > PREVIEW_CACHE_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stamp)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes -= old.result.as_ref().map(|image| image.bytes).unwrap_or(0);
            }
        }
    }
}
fn decode_preview(path: &std::path::Path) -> Result<DecodedPreview, String> {
    use ::image::ImageDecoder;
    if std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len()
        > PREVIEW_DECODE_BYTES
    {
        return Err("Image file exceeds the 64 MiB preview limit".into());
    }
    let mut reader = ::image::ImageReader::open(path)
        .map_err(|error| error.to_string())?
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = ::image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(PREVIEW_DECODE_BYTES);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    if decoder.total_bytes() > PREVIEW_DECODE_BYTES {
        return Err("Decoded image exceeds the 64 MiB preview limit".into());
    }
    let preview = ::image::DynamicImage::from_decoder(decoder)
        .map_err(|error| error.to_string())?
        .thumbnail(PREVIEW_EDGE, PREVIEW_EDGE)
        .to_rgba8();
    let (width, height) = preview.dimensions();
    let pixels = preview.into_raw();
    let bytes = pixels.len();
    Ok(DecodedPreview {
        handle: image::Handle::from_rgba(width, height, pixels),
        bytes,
    })
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    struct File(PathBuf);
    impl File {
        fn png(width: u32, height: u32) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vasya-iced-preview-{}-{}.png",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            ::image::RgbaImage::new(width, height).save(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for File {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[test]
    fn preview_decoder_downsamples_and_rejects_oversized_dimensions() {
        let source = File::png(2048, 512);
        let decoded = decode_preview(&source.0).unwrap();
        match decoded.handle {
            image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            } => {
                assert_eq!((width, height), (512, 128));
                assert_eq!(pixels.len(), 512 * 128 * 4);
            }
            _ => panic!("Renderer must only receive decoded RGBA"),
        }
        let oversized = File::png(8193, 1);
        assert!(decode_preview(&oversized.0).is_err());
    }
    #[test]
    fn preview_cache_is_bounded_and_remembers_failures() {
        let mut cache = PreviewCache::default();
        let failure = PathBuf::from("broken.png");
        cache.insert(failure.clone(), Err("invalid image".into()));
        assert!(cache.touch(&failure));
        let pixels = vec![0; PREVIEW_EDGE as usize * PREVIEW_EDGE as usize * 4];
        for index in 0..40 {
            cache.insert(
                format!("{index}.png").into(),
                Ok(DecodedPreview {
                    handle: image::Handle::from_rgba(PREVIEW_EDGE, PREVIEW_EDGE, pixels.clone()),
                    bytes: pixels.len(),
                }),
            );
            assert!(cache.entries.len() <= PREVIEW_CACHE_ENTRIES);
            assert!(cache.bytes <= PREVIEW_CACHE_BYTES);
        }
        assert!(!cache.entries.contains_key(&PathBuf::from("0.png")));
        assert!(cache.entries.contains_key(&PathBuf::from("39.png")));
    }
}
