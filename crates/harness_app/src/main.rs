use std::{
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use assets::Assets;
use codex_app_server_client::{Client, CodexThread, Event as AppServerEvent};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, FollowMode,
    FontWeight, IntoElement, KeyBinding, KeyContext, ListAlignment, ListState, Render,
    SharedString, Task, UpdateGlobal, Window, WindowBounds, WindowOptions, actions, deferred, div,
    list, prelude::*, px, relative, size,
};
use gpui_platform::application;
use harness_editor::{
    LocalEditor, ModeIndicator, TranscriptEditor, TranscriptSelectionChanged, TranscriptSupplement,
    VimNextMatch, VimPreviousMatch, VimSearch,
};
use harness_protocol as model;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use model::{TranscriptItem, TranscriptModel, minimal_text_edit};
use serde_json::{Value, json};
use settings::SettingsStore;
use ui::prelude::{ActiveTheme, StyledTypography};
use ui::{
    AgentThreadStatus, Button, ButtonCommon, ButtonSize, ButtonStyle, Clickable, Color,
    Disableable, Disclosure, Icon, IconButton, IconButtonShape, IconName, IconSize, Label,
    LabelCommon, LabelSize, ListItem, ListItemSpacing, SelectableButton, ThreadItem, TintColor,
    Toggleable,
};

mod image_surface;
mod palette;
mod request_surface;

use image_surface::{
    ImageSurface, SurfaceSyncDecision as ImageSurfaceSyncDecision,
    keys_to_sync as image_surface_keys_to_sync, supplement_key as image_supplement_key,
    surface_sync_decision as image_surface_sync_decision,
};
use palette::{PaletteEvent, PaletteOverlay};
use request_surface::{
    RequestSurface, Respond as RequestSurfaceRespond, ReturnToTranscript, SurfaceSyncDecision,
    surface_sync_decision,
};
use zed_actions::command_palette::{OpenWithQuery, Toggle as ToggleCommandPalette};

actions!(
    harness,
    [
        Send,
        Stop,
        FocusTranscript,
        FocusTasks,
        FocusComposer,
        MoveUp,
        MoveDown,
        GoTop,
        GoBottom,
        ToggleItem,
        ToggleRaw,
        ToggleVisual,
        YankItem,
        PageUp,
        PageDown,
        OpenTask,
        NewTask,
        RefreshTasks,
        ToggleSidebar,
        OpenSearch,
        CommitSearch,
        CloseSearch,
        NextMatch,
        PreviousMatch,
        MoveLeft,
        MoveRight,
        ChooseRequest,
        SubmitRequest,
        EditRequest,
        ToggleBufferView,
        ShowRichTranscript,
        ShowTextTranscript,
        NormalEscape,
        ChooseApproval,
        OpenRequestSurface,
        ReturnFromRequest,
    ]
);

const SIDEBAR_WIDTH: f32 = 252.;
const COMPACT_SIDEBAR_THRESHOLD: f32 = 1100.;
const THREAD_LIMIT: usize = 300;
const STREAM_FRAME: Duration = Duration::from_millis(32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusMode {
    Tasks,
    Transcript,
    Composer,
    Search,
    Request,
    Approval,
    Buffer,
}

#[derive(Clone, Debug, PartialEq)]
enum RequestReply {
    Result(Value),
    Error { code: i64, message: String },
}

#[derive(Clone, Debug, PartialEq)]
enum RequestRoute {
    Interactive,
    Immediate(RequestReply),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestChoiceTone {
    Allow,
    Deny,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
struct RequestChoice {
    label: String,
    response: Value,
    completed_status: String,
    tone: RequestChoiceTone,
}

struct CachedMarkdown {
    source: String,
    entity: Entity<Markdown>,
}

struct LiveRequestSurface {
    request_id: Value,
    entity: Entity<RequestSurface>,
}

struct HarnessApp {
    cwd: String,
    replay_count: Option<usize>,
    client: Option<Rc<Client>>,
    threads: Vec<CodexThread>,
    selected_thread_id: Option<String>,
    selected_title: SharedString,
    connecting: bool,
    loading_thread: bool,
    error: Option<SharedString>,
    model: TranscriptModel,
    composer: Entity<LocalEditor>,
    search_editor: Entity<LocalEditor>,
    transcript_editor: Entity<TranscriptEditor>,
    mode_indicator: Entity<ModeIndicator>,
    buffer_view: bool,
    transcript_focus: FocusHandle,
    focus_mode: FocusMode,
    selected_item: usize,
    selected_task: usize,
    visual_anchor: Option<usize>,
    raw_visible: HashSet<String>,
    markdown_cache: HashMap<String, CachedMarkdown>,
    search_visible: bool,
    search_query: String,
    search_matches: Vec<usize>,
    active_search_match: usize,
    search_returns_to_buffer: bool,
    buffer_search_backwards: bool,
    request_answers: HashMap<String, HashMap<String, Vec<String>>>,
    request_editors: HashMap<String, Entity<LocalEditor>>,
    request_question_cursor: HashMap<String, usize>,
    request_option_cursor: HashMap<String, usize>,
    approval_cursor: usize,
    live_request_keys: HashSet<String>,
    dirty_request_surfaces: HashSet<String>,
    request_surfaces: HashMap<String, LiveRequestSurface>,
    command_palette: Option<Entity<PaletteOverlay>>,
    command_palette_history: Vec<String>,
    command_palette_usage: HashMap<String, u16>,
    dirty_image_surfaces: HashSet<String>,
    image_surfaces: HashMap<String, Entity<ImageSurface>>,
    list_state: ListState,
    task_list_state: ListState,
    sidebar_open: bool,
    sidebar_user_override: bool,
    server_task: Task<()>,
    request_task: Task<()>,
}

impl HarnessApp {
    fn new(
        cwd: String,
        replay_count: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mode_indicator = cx.new(|cx| ModeIndicator::new(window, cx));
        let composer = cx.new(|cx| LocalEditor::modal_composer(window, cx));
        let search_editor =
            cx.new(|cx| LocalEditor::plain_single_line("Search transcript…", window, cx));
        let transcript_editor = cx.new(|cx| TranscriptEditor::read_only(window, cx));
        cx.on_focus_in(
            &transcript_editor.focus_handle(cx),
            window,
            |this, _window, cx| {
                if this.focus_mode != FocusMode::Buffer {
                    this.focus_mode = FocusMode::Buffer;
                    cx.notify();
                }
            },
        )
        .detach();
        cx.on_focus_in(&composer.focus_handle(cx), window, |this, _window, cx| {
            if this.focus_mode != FocusMode::Composer {
                this.focus_mode = FocusMode::Composer;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(
            &transcript_editor,
            |this, editor, _: &TranscriptSelectionChanged, cx| {
                let item_index = editor.update(cx, |editor, cx| editor.selected_item(cx));
                if this.buffer_view
                    && let Some(item_index) = item_index
                    && this.selected_item != item_index
                {
                    this.selected_item = item_index;
                    cx.notify();
                }
            },
        )
        .detach();
        let model = replay_count
            .map(TranscriptModel::replay)
            .unwrap_or_default();
        let dirty_image_surfaces = model
            .items
            .iter()
            .filter(|item| item.kind == model::TranscriptKind::Image)
            .map(|item| item.key.clone())
            .collect();
        let list_state = ListState::new(model.items.len(), ListAlignment::Top, px(1600.));
        list_state.set_follow_mode(FollowMode::Tail);
        let task_list_state = ListState::new(0, ListAlignment::Top, px(54.));
        let transcript_focus = cx.focus_handle();
        composer.focus_handle(cx).focus(window, cx);

        let mut this = Self {
            cwd,
            replay_count,
            client: None,
            threads: Vec::new(),
            selected_thread_id: None,
            selected_title: if let Some(count) = replay_count {
                format!("Replay · {count} items").into()
            } else {
                "New task".into()
            },
            connecting: false,
            loading_thread: false,
            error: None,
            selected_item: model.items.len().saturating_sub(1),
            model,
            composer,
            search_editor,
            transcript_editor,
            mode_indicator,
            buffer_view: false,
            transcript_focus,
            focus_mode: FocusMode::Composer,
            list_state,
            task_list_state,
            selected_task: 0,
            visual_anchor: None,
            raw_visible: HashSet::default(),
            markdown_cache: HashMap::default(),
            search_visible: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            active_search_match: 0,
            search_returns_to_buffer: false,
            buffer_search_backwards: false,
            request_answers: HashMap::default(),
            request_editors: HashMap::default(),
            request_question_cursor: HashMap::default(),
            request_option_cursor: HashMap::default(),
            approval_cursor: 0,
            live_request_keys: HashSet::default(),
            dirty_request_surfaces: HashSet::default(),
            request_surfaces: HashMap::default(),
            command_palette: None,
            command_palette_history: Vec::new(),
            command_palette_usage: HashMap::default(),
            dirty_image_surfaces,
            image_surfaces: HashMap::default(),
            sidebar_open: true,
            sidebar_user_override: false,
            server_task: Task::ready(()),
            request_task: Task::ready(()),
        };
        if replay_count.is_none() {
            this.connect(cx);
        }
        this
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.connecting = true;
        self.error = None;
        let initial_thread_query = std::env::var("HARNESS_OPEN_THREAD").ok();
        self.server_task = cx.spawn(async move |this, cx| {
            let result = async {
                let client = Rc::new(Client::launch("codex")?);
                client
                    .initialize("harness", "Harness", env!("CARGO_PKG_VERSION"))
                    .await?;
                let threads = client.list_threads(THREAD_LIMIT, None).await?.data;
                anyhow::Ok((client, threads))
            }
            .await;

            let client = match result {
                Ok((client, threads)) => {
                    if this
                        .update(cx, |this, cx| {
                            let old_len = this.threads.len();
                            this.client = Some(client.clone());
                            this.threads = threads;
                            this.task_list_state.splice(0..old_len, this.threads.len());
                            this.connecting = false;
                            this.error = None;
                            if let Some(query) = initial_thread_query.as_deref() {
                                let query = query.to_lowercase();
                                if let Some(index) = this.threads.iter().position(|thread| {
                                    thread_title(thread).to_lowercase().contains(&query)
                                        || thread.id == query
                                }) {
                                    this.open_thread(index, cx);
                                }
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                    client
                }
                Err(error) => {
                    if this
                        .update(cx, |this, cx| {
                            this.connecting = false;
                            this.error =
                                Some(format!("Could not connect to Codex: {error}").into());
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                    return;
                }
            };

            let events = client.events();
            while let Ok(first) = events.recv().await {
                cx.background_executor().timer(STREAM_FRAME).await;
                let mut batch = vec![first];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                if this
                    .update(cx, |this, cx| this.apply_event_batch(batch, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    fn apply_event_batch(&mut self, events: Vec<AppServerEvent>, cx: &mut Context<Self>) {
        let (events, live_request_ids) = self.dispatch_server_requests(events, cx);
        let was_following_tail = if self.buffer_view {
            self.transcript_editor.read(cx).is_following_tail()
        } else {
            self.list_state.is_following_tail()
        };
        let old_len = self.model.items.len();
        let outcome = self
            .model
            .apply_batch(events, self.selected_thread_id.as_deref());
        let new_len = self.model.items.len();
        let document_changed = new_len != old_len || !outcome.dirty.is_empty();
        let mut dirty_items = outcome.dirty.into_iter().collect::<Vec<_>>();
        dirty_items.sort_unstable();
        if new_len > old_len {
            self.list_state.splice(old_len..old_len, new_len - old_len);
            if was_following_tail {
                self.selected_item = self.model.items.len().saturating_sub(1);
            }
        }
        for index in &dirty_items {
            if *index < old_len {
                self.list_state.splice(*index..*index + 1, 1);
            }
        }
        if let Some(name) = outcome.renamed_thread {
            self.selected_title = name.into();
        }
        if let Some(error) = outcome.transport_error {
            self.error = Some(error.into());
        }
        self.track_live_request_updates(&live_request_ids, old_len, new_len, &dirty_items);
        self.track_image_surface_updates(old_len, new_len, &dirty_items);
        if outcome.refresh_threads {
            if let Some(thread_id) = self.selected_thread_id.as_deref()
                && let Err(error) = self.model.persist_transcript(thread_id)
            {
                log::warn!("could not persist transcript history: {error}");
            }
            self.refresh_threads(cx);
        }
        if !self.search_query.is_empty() {
            self.rebuild_search_matches();
        }
        if document_changed && self.buffer_view {
            let incrementally_applied =
                self.sync_transcript_item_updates(old_len, &dirty_items, cx);
            if !incrementally_applied {
                drop(self.sync_transcript_document(cx));
            }
        }
        cx.notify();
    }

    fn dispatch_server_requests(
        &self,
        events: Vec<AppServerEvent>,
        cx: &mut Context<Self>,
    ) -> (Vec<AppServerEvent>, Vec<Value>) {
        let mut forwarded = Vec::with_capacity(events.len());
        let mut live_request_ids = Vec::new();
        for event in events {
            let AppServerEvent::ServerRequest { id, method, params } = event else {
                forwarded.push(event);
                continue;
            };
            match route_server_request(&method, &params, self.selected_thread_id.as_deref()) {
                RequestRoute::Interactive => {
                    live_request_ids.push(id.clone());
                    forwarded.push(AppServerEvent::ServerRequest { id, method, params })
                }
                RequestRoute::Immediate(reply) => {
                    self.send_immediate_request_reply(id, method, reply, cx);
                }
            }
        }
        (forwarded, live_request_ids)
    }

    fn send_immediate_request_reply(
        &self,
        id: Value,
        method: String,
        reply: RequestReply,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            log::warn!("could not answer app-server request {method}: client is unavailable");
            return;
        };
        cx.spawn(async move |_, _| {
            let result = match reply {
                RequestReply::Result(value) => client.respond(id, value).await,
                RequestReply::Error { code, message } => {
                    client.respond_error(id, code, message).await
                }
            };
            match result {
                Ok(()) => log::debug!("safely resolved app-server request {method}"),
                Err(error) => log::warn!("could not resolve app-server request {method}: {error}"),
            }
        })
        .detach();
    }

    fn track_live_request_updates(
        &mut self,
        live_request_ids: &[Value],
        old_len: usize,
        new_len: usize,
        dirty_items: &[usize],
    ) {
        let mut candidates = dirty_items.iter().copied().collect::<HashSet<_>>();
        candidates.extend(old_len..new_len);
        for index in candidates {
            let Some(item) = self.model.items.get(index) else {
                continue;
            };
            if item.pending_request.as_ref().is_some_and(|request| {
                !request.resolved && live_request_ids.iter().any(|id| *id == request.id)
            }) {
                self.live_request_keys.insert(item.key.clone());
            }
            if self.live_request_keys.contains(&item.key)
                || self.request_surfaces.contains_key(&item.key)
            {
                self.dirty_request_surfaces.insert(item.key.clone());
            }
        }
    }

    fn track_image_surface_updates(
        &mut self,
        old_len: usize,
        new_len: usize,
        dirty_items: &[usize],
    ) {
        let mut candidates = dirty_items.iter().copied().collect::<HashSet<_>>();
        candidates.extend(old_len..new_len);
        for index in candidates {
            let Some(item) = self.model.items.get(index) else {
                continue;
            };
            if item.kind == model::TranscriptKind::Image
                || self.image_surfaces.contains_key(&item.key)
            {
                self.dirty_image_surfaces.insert(item.key.clone());
            }
        }
    }

    fn mark_all_image_surfaces_dirty(&mut self) {
        let keys = image_surface_keys_to_sync(
            self.image_surfaces.keys().cloned(),
            self.model
                .items
                .iter()
                .filter(|item| item.kind == model::TranscriptKind::Image)
                .map(|item| item.key.clone()),
        );
        self.dirty_image_surfaces.extend(keys);
    }

    fn sync_image_surfaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = std::mem::take(&mut self.dirty_image_surfaces);
        for item_key in dirty {
            let item = self.model.items.iter().find(|item| item.key == item_key);
            let item_is_image = item.is_some_and(|item| item.kind == model::TranscriptKind::Image);
            let surface_exists = self.image_surfaces.contains_key(&item_key);

            match image_surface_sync_decision(item_is_image, surface_exists) {
                ImageSurfaceSyncDecision::Ignore => {}
                ImageSurfaceSyncDecision::Remove => {
                    self.image_surfaces.remove(&item_key);
                    self.transcript_editor.update(cx, |editor, cx| {
                        editor.remove_supplement(&image_supplement_key(&item_key), window, cx);
                    });
                }
                ImageSurfaceSyncDecision::Upsert => {
                    let Some(raw) = item.map(|item| item.raw.clone()) else {
                        continue;
                    };
                    let surface = if let Some(surface) = self.image_surfaces.get(&item_key) {
                        surface.clone()
                    } else {
                        let surface = cx.new(|_| ImageSurface::new(&raw));
                        self.image_surfaces
                            .insert(item_key.clone(), surface.clone());
                        surface
                    };
                    surface.update(cx, |surface, cx| surface.update(&raw, cx));
                    let rows = surface.read(cx).rows();
                    self.transcript_editor.update(cx, |editor, cx| {
                        editor.upsert_supplement(
                            TranscriptSupplement::new(
                                image_supplement_key(&item_key),
                                item_key.clone(),
                                rows,
                                surface.clone().into(),
                            ),
                            cx,
                        );
                    });
                }
            }
        }
    }

    fn sync_request_surfaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = std::mem::take(&mut self.dirty_request_surfaces);
        for item_key in dirty {
            let item = self
                .model
                .items
                .iter()
                .find(|item| item.key == item_key)
                .cloned();
            let request = item
                .as_ref()
                .and_then(|item| item.pending_request.as_ref())
                .cloned();
            let is_live = self.live_request_keys.contains(&item_key);
            let unresolved = request.as_ref().is_some_and(|request| !request.resolved);
            let exists = self.request_surfaces.contains_key(&item_key);
            let responding = self
                .request_surfaces
                .get(&item_key)
                .is_some_and(|entry| entry.entity.read(cx).is_responding());

            match surface_sync_decision(is_live, unresolved, responding, exists) {
                SurfaceSyncDecision::Ignore | SurfaceSyncDecision::KeepResponding => {}
                SurfaceSyncDecision::Remove => {
                    self.remove_request_surface(&item_key, true, window, cx);
                }
                SurfaceSyncDecision::Upsert => {
                    let (Some(item), Some(request)) = (item, request) else {
                        continue;
                    };
                    let replace_entity = self
                        .request_surfaces
                        .get(&item_key)
                        .is_some_and(|entry| entry.request_id != request.id);
                    if replace_entity {
                        self.remove_request_surface(&item_key, false, window, cx);
                    }

                    let surface = if let Some(entry) = self.request_surfaces.get(&item_key) {
                        entry.entity.clone()
                    } else {
                        let surface = cx.new(|cx| {
                            RequestSurface::new(
                                item_key.clone(),
                                request.method.clone(),
                                item.raw.clone(),
                                window,
                                cx,
                            )
                        });
                        let surface_focus = surface.focus_handle(cx);
                        let focus_item_key = item_key.clone();
                        cx.on_focus_in(&surface_focus, window, move |this, _window, cx| {
                            if let Some(index) = this
                                .model
                                .items
                                .iter()
                                .position(|item| item.key == focus_item_key)
                            {
                                this.selected_item = index;
                            }
                            if let Some(entry) = this.request_surfaces.get(&focus_item_key) {
                                this.focus_mode = if entry.entity.read(cx).is_approval() {
                                    FocusMode::Approval
                                } else {
                                    FocusMode::Request
                                };
                            }
                            cx.notify();
                        })
                        .detach();
                        cx.subscribe(&surface, |this, _, event: &RequestSurfaceRespond, cx| {
                            this.handle_request_surface_response(event.clone(), cx);
                        })
                        .detach();
                        cx.subscribe_in(
                            &surface,
                            window,
                            |this, _, event: &ReturnToTranscript, window, cx| {
                                this.handle_return_to_transcript(event, window, cx);
                            },
                        )
                        .detach();
                        self.request_surfaces.insert(
                            item_key.clone(),
                            LiveRequestSurface {
                                request_id: request.id.clone(),
                                entity: surface.clone(),
                            },
                        );
                        surface
                    };
                    surface.update(cx, |surface, cx| {
                        surface.update_request(request.method.clone(), item.raw.clone(), window, cx)
                    });
                    let rows = surface.read(cx).rows();
                    self.transcript_editor.update(cx, |editor, cx| {
                        editor.upsert_supplement(
                            TranscriptSupplement::new(
                                request_supplement_key(&item_key),
                                item_key.clone(),
                                rows,
                                surface.clone().into(),
                            ),
                            cx,
                        )
                    });
                }
            }
        }
    }

    fn remove_request_surface(
        &mut self,
        item_key: &str,
        retire_live_request: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.request_surfaces.remove(item_key) {
            if entry.entity.read(cx).contains_focus(window, cx) {
                self.focus_transcript(window, cx);
            }
            self.transcript_editor.update(cx, |editor, cx| {
                editor.remove_supplement(&request_supplement_key(item_key), window, cx);
            });
        }
        if retire_live_request {
            self.live_request_keys.remove(item_key);
        }
    }

    fn retire_all_request_surfaces(&mut self) {
        self.live_request_keys.clear();
        self.dirty_request_surfaces
            .extend(self.request_surfaces.keys().cloned());
    }

    fn handle_request_surface_response(
        &mut self,
        event: RequestSurfaceRespond,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .model
            .items
            .iter()
            .position(|item| item.key == event.item_key)
        else {
            return;
        };
        let unresolved = self.model.items[index]
            .pending_request
            .as_ref()
            .is_some_and(|request| !request.resolved);
        if !unresolved || self.client.is_none() {
            return;
        }
        if let Some(entry) = self.request_surfaces.get(&event.item_key) {
            entry
                .entity
                .update(cx, |surface, cx| surface.set_responding(true, cx));
        }
        self.respond_with_value(index, event.response, event.completed_status, cx);
    }

    fn handle_return_to_transcript(
        &mut self,
        event: &ReturnToTranscript,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.request_surfaces.contains_key(&event.item_key) {
            self.focus_transcript(window, cx);
        }
    }

    fn focus_buffer_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.buffer_view {
            return;
        }
        self.focus_mode = FocusMode::Buffer;
        self.transcript_editor.focus_handle(cx).focus(window, cx);
        cx.defer_in(window, |this, window, cx| {
            if this.buffer_view && this.focus_mode == FocusMode::Buffer {
                this.transcript_editor
                    .update(cx, |editor, cx| editor.enter_normal_mode(window, cx));
            }
        });
        cx.notify();
    }

    fn focus_selected_request_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer_view {
            let item_index = self
                .transcript_editor
                .update(cx, |editor, cx| editor.selected_item(cx));
            if let Some(item_index) = item_index {
                self.selected_item = item_index;
            }
        }
        let Some(item_key) = self
            .model
            .items
            .get(self.selected_item)
            .map(|item| item.key.clone())
        else {
            return;
        };
        let Some(entry) = self.request_surfaces.get(&item_key) else {
            return;
        };
        let surface = entry.entity.clone();
        self.focus_mode = if surface.read(cx).is_approval() {
            FocusMode::Approval
        } else {
            FocusMode::Request
        };
        if self.buffer_view {
            self.transcript_editor.update(cx, |editor, cx| {
                editor.reveal_supplement(&request_supplement_key(&item_key), window, cx);
            });
        } else {
            self.list_state.scroll_to_reveal_item(self.selected_item);
        }
        surface.update(cx, |surface, cx| surface.focus(window, cx));
        cx.notify();
    }

    fn return_from_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry
                .entity
                .update(cx, |surface, cx| surface.return_to_transcript(cx));
            return;
        }
        self.focus_transcript(window, cx);
    }

    fn sync_transcript_item_updates(
        &mut self,
        old_model_item_count: usize,
        dirty_items: &[usize],
        cx: &mut Context<Self>,
    ) -> bool {
        let new_model_item_count = self.model.items.len();
        if new_model_item_count < old_model_item_count {
            return false;
        }
        let existing_updates = dirty_items
            .iter()
            .copied()
            .filter(|item_index| *item_index < old_model_item_count)
            .map(|item_index| (item_index, self.model.item_projection(item_index)))
            .collect::<Vec<_>>();
        let appended = (old_model_item_count..new_model_item_count)
            .map(|item_index| self.model.item_projection(item_index))
            .collect::<Vec<_>>();

        self.transcript_editor.update(cx, |editor, cx| {
            editor.apply_item_projections(old_model_item_count, &existing_updates, &appended, cx)
        })
    }

    fn sync_transcript_document(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<model::TranscriptDocument> {
        if !self.buffer_view {
            return None;
        }
        let document = self.model.full_document();
        let old_text = self.transcript_editor.read(cx).text(cx);
        if old_text == document.text {
            self.transcript_editor
                .update(cx, |editor, cx| editor.decorate(&document, cx));
            return Some(document);
        }
        let (old_range, replacement) = minimal_text_edit(&old_text, &document.text);
        self.transcript_editor.update(cx, |editor, cx| {
            editor.edit(old_range, replacement, cx);
            editor.decorate(&document, cx);
        });
        Some(document)
    }

    fn refresh_threads(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            if self.replay_count.is_none() {
                self.connect(cx);
            }
            return;
        };
        self.connecting = true;
        self.request_task = cx.spawn(async move |this, cx| {
            let result = client.list_threads(THREAD_LIMIT, None).await;
            if this
                .update(cx, |this, cx| {
                    this.connecting = false;
                    match result {
                        Ok(response) => {
                            let old_len = this.threads.len();
                            this.threads = response.data;
                            this.task_list_state.splice(0..old_len, this.threads.len());
                            this.error = None;
                        }
                        Err(error) => {
                            this.error = Some(format!("Refresh failed: {error}").into());
                        }
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        });
    }

    fn open_thread(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.get(index) else {
            return;
        };
        let thread_id = thread.id.clone();
        let title = thread_title(thread);
        let Some(client) = self.client.clone() else {
            return;
        };

        self.reject_pending_requests(cx);
        self.selected_thread_id = Some(thread_id.clone());
        self.selected_title = title.into();
        self.loading_thread = true;
        self.error = None;
        let old_len = self.model.items.len();
        self.model.clear();
        self.mark_all_image_surfaces_dirty();
        self.markdown_cache.clear();
        self.raw_visible.clear();
        self.request_answers.clear();
        self.request_editors.clear();
        self.request_question_cursor.clear();
        self.request_option_cursor.clear();
        self.retire_all_request_surfaces();
        self.list_state.splice(0..old_len, 0);
        self.selected_item = 0;
        drop(self.sync_transcript_document(cx));
        cx.notify();

        self.request_task = cx.spawn(async move |this, cx| {
            let read = client.read_thread(&thread_id).await;
            let result = match read {
                Ok(read_thread) => match client.resume_thread(&thread_id).await {
                    Ok(resumed) => Ok((resumed, None)),
                    Err(error) => Ok((
                        read_thread,
                        Some(format!(
                            "Read-only while another Codex window owns this task: {error}"
                        )),
                    )),
                },
                Err(error) => Err(error),
            };
            if this
                .update(cx, |this, cx| {
                    this.loading_thread = false;
                    match result {
                        Ok((thread, warning)) => {
                            this.load_thread(thread, cx);
                            this.error = warning.map(Into::into);
                        }
                        Err(error) => {
                            this.error = Some(format!("Could not open task: {error}").into());
                        }
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        });
    }

    fn load_thread(&mut self, thread: CodexThread, cx: &mut Context<Self>) {
        let old_len = self.model.items.len();
        self.selected_thread_id = Some(thread.id.clone());
        self.selected_title = thread_title(&thread).into();
        if !thread.cwd.is_empty() {
            self.cwd = thread.cwd.clone();
        }
        self.model.load_thread(&thread);
        match self.model.merge_persisted_transcript(&thread.id) {
            Ok(restored) if restored > 0 => {
                log::info!("restored {restored} live-only transcript items")
            }
            Ok(_) => {}
            Err(error) => log::warn!("could not restore transcript history: {error}"),
        }
        self.markdown_cache.clear();
        self.raw_visible.clear();
        self.request_answers.clear();
        self.request_editors.clear();
        self.request_question_cursor.clear();
        self.request_option_cursor.clear();
        self.retire_all_request_surfaces();
        self.mark_all_image_surfaces_dirty();
        self.list_state.splice(0..old_len, self.model.items.len());
        self.selected_item = self.model.items.len().saturating_sub(1);
        self.list_state.set_follow_mode(FollowMode::Tail);
        drop(self.sync_transcript_document(cx));
        if self.buffer_view {
            self.transcript_editor
                .update(cx, |editor, cx| editor.reveal_tail(cx));
        }
        cx.notify();
    }

    fn new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reject_pending_requests(cx);
        let old_len = self.model.items.len();
        self.model.clear();
        self.mark_all_image_surfaces_dirty();
        self.markdown_cache.clear();
        self.raw_visible.clear();
        self.request_answers.clear();
        self.request_editors.clear();
        self.request_question_cursor.clear();
        self.request_option_cursor.clear();
        self.retire_all_request_surfaces();
        self.list_state.splice(0..old_len, 0);
        self.selected_thread_id = None;
        self.selected_title = "New task".into();
        self.selected_item = 0;
        self.error = None;
        self.list_state.set_follow_mode(FollowMode::Tail);
        drop(self.sync_transcript_document(cx));
        if self.buffer_view {
            self.transcript_editor
                .update(cx, |editor, cx| editor.reveal_tail(cx));
        }
        self.focus_composer(window, cx);
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text(cx);
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let (index, key) = self.model.push_local_user(text.clone());
        self.list_state.splice(index..index, 1);
        self.selected_item = index;
        self.list_state.set_follow_mode(FollowMode::Tail);
        let document = self.sync_transcript_document(cx);
        if self.buffer_view {
            let row = document
                .as_ref()
                .and_then(|document| document.item_rows.get(index))
                .and_then(|row| *row)
                .unwrap_or(0);
            self.transcript_editor.update(cx, |editor, cx| {
                editor.set_cursor_row(row, window, cx);
                editor.reveal_tail(cx);
            });
        }
        self.composer
            .update(cx, |editor, cx| editor.set_text("", window, cx));

        if self.replay_count.is_some() {
            self.model.set_status_for_key(&key, "replay");
            self.list_state.splice(index..index + 1, 1);
            cx.notify();
            return;
        }
        let Some(client) = self.client.clone() else {
            self.model.set_status_for_key(&key, "not connected");
            self.error = Some("Codex is not connected yet".into());
            self.list_state.splice(index..index + 1, 1);
            cx.notify();
            return;
        };
        let existing_thread_id = self.selected_thread_id.clone();
        let cwd = self.cwd.clone();
        let input = vec![json!({"type": "text", "text": text})];
        self.request_task = cx.spawn(async move |this, cx| {
            let result = async {
                let thread_id = match existing_thread_id {
                    Some(thread_id) => thread_id,
                    None => client.start_thread(&cwd).await?.id,
                };
                let response = client.start_turn(&thread_id, Value::Array(input)).await?;
                anyhow::Ok((thread_id, response))
            }
            .await;
            if this
                .update(cx, |this, cx| {
                    match result {
                        Ok((thread_id, response)) => {
                            this.selected_thread_id = Some(thread_id);
                            this.model.current_turn_id = response
                                .pointer("/turn/id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            if let Some(index) = this.model.set_status_for_key(&key, "sent") {
                                this.list_state.splice(index..index + 1, 1);
                            }
                            this.error = None;
                        }
                        Err(error) => {
                            if let Some(index) = this.model.set_status_for_key(&key, "failed") {
                                this.list_state.splice(index..index + 1, 1);
                            }
                            this.error = Some(format!("Could not send: {error}").into());
                        }
                    }
                    drop(this.sync_transcript_document(cx));
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        });
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(thread_id), Some(turn_id)) = (
            self.client.clone(),
            self.selected_thread_id.clone(),
            self.model.current_turn_id.clone(),
        ) else {
            return;
        };
        self.request_task = cx.spawn(async move |this, cx| {
            let result = client.interrupt_turn(&thread_id, &turn_id).await;
            if let Err(error) = result {
                if this
                    .update(cx, |this, cx| {
                        this.error = Some(format!("Could not stop turn: {error}").into());
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    fn respond_with_choice(&mut self, index: usize, choice: RequestChoice, cx: &mut Context<Self>) {
        self.respond_with_value(index, choice.response, choice.completed_status, cx);
    }

    fn respond_with_value(
        &mut self,
        index: usize,
        response: Value,
        completed_status: String,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some((request_key, request)) = self.model.items.get(index).and_then(|item| {
            item.pending_request
                .clone()
                .map(|request| (item.key.clone(), request))
        }) else {
            return;
        };
        if request.resolved {
            return;
        }
        if let Some(entry) = self.request_surfaces.get(&request_key) {
            entry
                .entity
                .update(cx, |surface, cx| surface.set_responding(true, cx));
        }
        self.model.items[index].status = Some("responding".into());
        if let Some(request) = &mut self.model.items[index].pending_request {
            request.resolved = true;
        }
        self.list_state.splice(index..index + 1, 1);
        if self.buffer_view {
            let item_count = self.model.items.len();
            if !self.sync_transcript_item_updates(item_count, &[index], cx) {
                drop(self.sync_transcript_document(cx));
            }
        }
        cx.spawn(async move |this, cx| {
            let result = client.respond(request.id, response).await;
            if this
                .update(cx, |this, cx| {
                    if let Some(index) = this
                        .model
                        .items
                        .iter()
                        .position(|item| item.key == request_key)
                    {
                        let item = &mut this.model.items[index];
                        match result {
                            Ok(()) => {
                                item.status = Some(completed_status);
                                if let Some(request) = &mut item.pending_request {
                                    request.resolved = true;
                                }
                            }
                            Err(error) => {
                                item.status = Some("response failed".into());
                                if let Some(request) = &mut item.pending_request {
                                    request.resolved = false;
                                }
                                this.error =
                                    Some(format!("Approval response failed: {error}").into());
                            }
                        }
                        if let Some(entry) = this.request_surfaces.get(&request_key) {
                            entry
                                .entity
                                .update(cx, |surface, cx| surface.set_responding(false, cx));
                        }
                        this.dirty_request_surfaces.insert(request_key.clone());
                        this.list_state.splice(index..index + 1, 1);
                        if this.buffer_view {
                            let item_count = this.model.items.len();
                            if !this.sync_transcript_item_updates(item_count, &[index], cx) {
                                drop(this.sync_transcript_document(cx));
                            }
                        }
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        })
        .detach();
    }

    fn reject_pending_requests(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .model
            .items
            .iter_mut()
            .filter_map(|item| {
                let (id, method) = {
                    let request = item
                        .pending_request
                        .as_mut()
                        .filter(|request| !request.resolved)?;
                    request.resolved = true;
                    (request.id.clone(), request.method.clone())
                };
                item.status = Some("declined when task closed".into());
                let reply = safe_request_rejection(&method, &item.raw);
                Some((item.key.clone(), id, method, reply))
            })
            .collect::<Vec<_>>();
        for (item_key, id, method, reply) in pending {
            self.dirty_request_surfaces.insert(item_key);
            self.send_immediate_request_reply(id, method, reply, cx);
        }
    }

    fn choose_request_option(
        &mut self,
        index: usize,
        request_key: String,
        question_id: String,
        answer: String,
        cx: &mut Context<Self>,
    ) {
        self.request_answers
            .entry(request_key)
            .or_default()
            .insert(question_id, vec![answer]);
        self.list_state.splice(index..index + 1, 1);
        cx.notify();
    }

    fn move_request_question(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry
                .entity
                .update(cx, |surface, cx| surface.move_vertical(delta, cx));
            return;
        }
        let Some(item) = self.model.items.get(self.selected_item) else {
            return;
        };
        let question_count = item
            .raw
            .get("questions")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if question_count == 0 {
            return;
        }
        let cursor = self
            .request_question_cursor
            .entry(item.key.clone())
            .or_insert(0);
        *cursor = cursor.saturating_add_signed(delta).min(question_count - 1);
        self.list_state
            .splice(self.selected_item..self.selected_item + 1, 1);
        cx.notify();
    }

    fn move_request_option(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry
                .entity
                .update(cx, |surface, cx| surface.move_horizontal(delta, cx));
            return;
        }
        let Some(item) = self.model.items.get(self.selected_item) else {
            return;
        };
        let Some(questions) = item.raw.get("questions").and_then(Value::as_array) else {
            return;
        };
        let question_index = self
            .request_question_cursor
            .get(&item.key)
            .copied()
            .unwrap_or(0)
            .min(questions.len().saturating_sub(1));
        let Some(question) = questions.get(question_index) else {
            return;
        };
        let option_count = question
            .get("options")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if option_count == 0 {
            return;
        }
        let question_id = question
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("question");
        let cursor = self
            .request_option_cursor
            .entry(format!("{}:{question_id}", item.key))
            .or_insert(0);
        *cursor = cursor.saturating_add_signed(delta).min(option_count - 1);
        self.list_state
            .splice(self.selected_item..self.selected_item + 1, 1);
        cx.notify();
    }

    fn choose_current_request_option(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry.entity.update(cx, |surface, cx| surface.choose(cx));
            return;
        }
        let Some(item) = self.model.items.get(self.selected_item) else {
            return;
        };
        let Some(questions) = item.raw.get("questions").and_then(Value::as_array) else {
            return;
        };
        let question_index = self
            .request_question_cursor
            .get(&item.key)
            .copied()
            .unwrap_or(0)
            .min(questions.len().saturating_sub(1));
        let Some(question) = questions.get(question_index) else {
            return;
        };
        let question_id = question
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("question")
            .to_string();
        let option_index = self
            .request_option_cursor
            .get(&format!("{}:{question_id}", item.key))
            .copied()
            .unwrap_or(0);
        let Some(answer) = question
            .get("options")
            .and_then(Value::as_array)
            .and_then(|options| options.get(option_index))
            .and_then(|option| option.get("label"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            return;
        };
        let request_key = item.key.clone();
        self.choose_request_option(self.selected_item, request_key, question_id, answer, cx);
    }

    fn move_approval_option(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry
                .entity
                .update(cx, |surface, cx| surface.move_horizontal(delta, cx));
            return;
        }
        let Some(item) = self.model.items.get(self.selected_item) else {
            return;
        };
        let Some(request) = item
            .pending_request
            .as_ref()
            .filter(|request| !request.resolved)
        else {
            return;
        };
        let choice_count = request_choices(&request.method, &item.raw).len();
        if choice_count == 0 {
            return;
        }
        self.approval_cursor = self
            .approval_cursor
            .saturating_add_signed(delta)
            .min(choice_count - 1);
        self.list_state
            .splice(self.selected_item..self.selected_item + 1, 1);
        cx.notify();
    }

    fn choose_approval(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry.entity.update(cx, |surface, cx| surface.choose(cx));
            return;
        }
        let Some(item) = self.model.items.get(self.selected_item) else {
            return;
        };
        let Some(request) = item
            .pending_request
            .as_ref()
            .filter(|request| !request.resolved)
        else {
            return;
        };
        let choices = request_choices(&request.method, &item.raw);
        let Some(choice) = choices
            .get(self.approval_cursor.min(choices.len().saturating_sub(1)))
            .cloned()
        else {
            return;
        };
        self.respond_with_choice(self.selected_item, choice, cx);
        self.focus_mode = FocusMode::Transcript;
        self.transcript_focus.focus(window, cx);
        cx.notify();
    }

    fn edit_current_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry
                .entity
                .update(cx, |surface, cx| surface.edit_current(window, cx));
            return;
        }
        let Some(item) = self.model.items.get(self.selected_item) else {
            return;
        };
        let Some(questions) = item.raw.get("questions").and_then(Value::as_array) else {
            return;
        };
        let question_index = self
            .request_question_cursor
            .get(&item.key)
            .copied()
            .unwrap_or(0)
            .min(questions.len().saturating_sub(1));
        let Some(question_id) = questions
            .get(question_index)
            .and_then(|question| question.get("id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let editor_key = format!("{}:{question_id}", item.key);
        if let Some(editor) = self.request_editors.get(&editor_key) {
            self.focus_mode = FocusMode::Request;
            editor.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn request_editor(
        &mut self,
        editor_key: &str,
        secret: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LocalEditor> {
        if let Some(editor) = self.request_editors.get(editor_key) {
            return editor.clone();
        }
        let editor = cx.new(|cx| LocalEditor::plain_single_line("Type an answer…", window, cx));
        editor.update(cx, |editor, cx| editor.set_masked(secret, cx));
        self.request_editors
            .insert(editor_key.to_string(), editor.clone());
        editor
    }

    fn submit_user_input(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.model.items.get(index).cloned() else {
            return;
        };
        let Some(questions) = item.raw.get("questions").and_then(Value::as_array) else {
            return;
        };
        let mut typed_answers = HashMap::new();
        for question in questions {
            let Some(question_id) = question.get("id").and_then(Value::as_str) else {
                self.set_request_validation_error(index, "question is missing an id".into(), cx);
                return;
            };
            let editor_key = format!("{}:{question_id}", item.key);
            if let Some(editor) = self.request_editors.get(&editor_key) {
                let text = editor.read(cx).text(cx).trim().to_string();
                if !text.is_empty() {
                    typed_answers.insert(question_id.to_string(), text);
                }
            }
        }
        let response = match build_user_input_response(
            questions,
            self.request_answers.get(&item.key),
            &typed_answers,
        ) {
            Ok(response) => response,
            Err(error) => {
                self.set_request_validation_error(index, error, cx);
                return;
            }
        };
        self.respond_with_value(index, response, "answered".into(), cx);
    }

    fn submit_mcp_form(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.model.items.get(index).cloned() else {
            return;
        };
        let Some(properties) = item
            .raw
            .pointer("/requestedSchema/properties")
            .and_then(Value::as_object)
        else {
            self.respond_with_value(
                index,
                json!({"action": "decline", "content": Value::Null}),
                "declined unsupported form".into(),
                cx,
            );
            return;
        };
        let mut field_text = HashMap::new();
        for name in properties.keys() {
            let editor_key = format!("{}:mcp:{name}", item.key);
            let text = self
                .request_editors
                .get(&editor_key)
                .map(|editor| editor.read(cx).text(cx).trim().to_string())
                .unwrap_or_default();
            if !text.is_empty() {
                field_text.insert(name.clone(), text);
            }
        }
        let response = match build_mcp_form_response(
            item.raw.pointer("/requestedSchema").unwrap_or(&Value::Null),
            &field_text,
        ) {
            Ok(response) => response,
            Err(error) => {
                self.set_request_validation_error(index, error, cx);
                return;
            }
        };
        self.respond_with_value(index, response, "submitted".into(), cx);
    }

    fn submit_active_request(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| self.request_surfaces.get(&item.key))
        {
            entry.entity.update(cx, |surface, cx| surface.submit(cx));
            return;
        }
        let method = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| item.pending_request.as_ref())
            .filter(|request| !request.resolved)
            .map(|request| request.method.as_str());
        if method == Some("mcpServer/elicitation/request") {
            self.submit_mcp_form(self.selected_item, cx);
        } else {
            self.submit_user_input(self.selected_item, cx);
        }
    }

    fn set_request_validation_error(
        &mut self,
        index: usize,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = self.model.items.get_mut(index) {
            item.status = Some(message);
            self.list_state.splice(index..index + 1, 1);
        }
        if self.buffer_view {
            let item_count = self.model.items.len();
            if !self.sync_transcript_item_updates(item_count, &[index], cx) {
                drop(self.sync_transcript_document(cx));
            }
        }
        cx.notify();
    }

    fn focus_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer_view {
            self.focus_buffer_transcript(window, cx);
            return;
        }
        self.focus_mode = FocusMode::Transcript;
        self.transcript_focus.focus(window, cx);
        self.list_state.scroll_to_reveal_item(self.selected_item);
        cx.defer_in(window, |this, _, cx| {
            if !this.buffer_view && this.focus_mode == FocusMode::Transcript {
                this.list_state.scroll_to_reveal_item(this.selected_item);
                cx.notify();
            }
        });
        cx.notify();
    }

    fn show_rich_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_following_tail =
            self.buffer_view && self.transcript_editor.read(cx).is_following_tail();
        if self.buffer_view {
            let item_index = self
                .transcript_editor
                .update(cx, |editor, cx| editor.selected_item(cx));
            if let Some(item_index) = item_index {
                self.selected_item = item_index;
            }
        }
        self.buffer_view = false;
        self.search_returns_to_buffer = false;
        self.focus_mode = FocusMode::Transcript;
        self.transcript_focus.focus(window, cx);
        if was_following_tail {
            self.list_state.set_follow_mode(FollowMode::Tail);
        } else {
            self.list_state.pause_following_tail();
            self.list_state.scroll_to_reveal_item(self.selected_item);
        }
        cx.defer_in(window, |this, _, cx| {
            if !this.buffer_view && this.focus_mode == FocusMode::Transcript {
                if this.list_state.is_following_tail() {
                    this.list_state.scroll_to_end();
                } else {
                    this.list_state.scroll_to_reveal_item(this.selected_item);
                }
                cx.notify();
            }
        });
        cx.notify();
    }

    fn toggle_buffer_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer_view {
            self.show_rich_transcript(window, cx);
            return;
        }

        self.search_visible = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.active_search_match = 0;
        self.search_returns_to_buffer = false;
        self.buffer_search_backwards = false;
        let should_follow_tail = self.list_state.is_following_tail()
            && (self.model.items.is_empty() || self.selected_item + 1 >= self.model.items.len());
        self.buffer_view = true;
        let Some(document) = self.sync_transcript_document(cx) else {
            return;
        };
        let row = document
            .item_rows
            .get(self.selected_item)
            .and_then(|row| *row)
            .or_else(|| {
                document.item_rows[..self.selected_item.min(document.item_rows.len())]
                    .iter()
                    .rev()
                    .find_map(|row| *row)
            })
            .unwrap_or(0);
        self.focus_mode = FocusMode::Buffer;
        self.transcript_editor.update(cx, |editor, cx| {
            editor.set_cursor_row(row, window, cx);
            if should_follow_tail {
                editor.reveal_tail(cx);
            } else {
                editor.pause_tail_follow();
            }
        });
        self.transcript_editor.focus_handle(cx).focus(window, cx);
        cx.defer_in(window, |this, window, cx| {
            if this.buffer_view {
                this.transcript_editor
                    .update(cx, |editor, cx| editor.enter_normal_mode(window, cx));
            }
        });
        cx.notify();
    }

    fn show_text_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.buffer_view {
            self.toggle_buffer_view(window, cx);
        } else {
            self.focus_buffer_transcript(window, cx);
        }
    }

    fn open_command_palette(
        &mut self,
        initial_query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_palette.is_some() {
            self.close_command_palette(window, cx);
            return;
        }
        let previous_focus = window
            .focused(cx)
            .unwrap_or_else(|| self.transcript_focus.clone());
        let palette = cx.new(|cx| {
            PaletteOverlay::new(
                initial_query,
                previous_focus,
                self.command_palette_history.clone(),
                self.command_palette_usage.clone(),
                window,
                cx,
            )
        });
        cx.subscribe_in(
            &palette,
            window,
            |this, palette, event: &PaletteEvent, window, cx| {
                let previous_focus = palette.read(cx).previous_focus();
                let confirmed = (*event == PaletteEvent::Confirmed)
                    .then(|| palette.update(cx, |palette, _| palette.take_confirmed()))
                    .flatten();
                this.command_palette = None;
                window.focus(&previous_focus, cx);
                if let Some(command) = confirmed {
                    if !command.resolved_query.is_empty() {
                        this.command_palette_history
                            .retain(|query| query != &command.resolved_query);
                        this.command_palette_history.push(command.resolved_query);
                        if this.command_palette_history.len() > 100 {
                            this.command_palette_history.remove(0);
                        }
                    }
                    let next_usage = this
                        .command_palette_usage
                        .get(&command.name)
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(1);
                    this.command_palette_usage.insert(command.name, next_usage);
                    window.dispatch_action(command.action, cx);
                }
                cx.notify();
            },
        )
        .detach();
        self.command_palette = Some(palette.clone());
        palette.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous_focus = self
            .command_palette
            .as_ref()
            .map(|palette| palette.read(cx).previous_focus());
        self.command_palette = None;
        if let Some(previous_focus) = previous_focus {
            window.focus(&previous_focus, cx);
        }
        cx.notify();
    }

    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_mode = FocusMode::Composer;
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn focus_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = true;
        if window.viewport_size().width < px(COMPACT_SIDEBAR_THRESHOLD) {
            self.sidebar_user_override = true;
        }
        self.focus_mode = FocusMode::Tasks;
        if let Some(index) = self
            .threads
            .iter()
            .position(|thread| self.selected_thread_id.as_deref() == Some(thread.id.as_str()))
        {
            self.selected_task = index;
        }
        self.transcript_focus.focus(window, cx);
        if !self.threads.is_empty() {
            self.selected_task = self.selected_task.min(self.threads.len() - 1);
            self.task_list_state
                .scroll_to_reveal_item(self.selected_task);
        }
        cx.notify();
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = window.viewport_size().width < px(COMPACT_SIDEBAR_THRESHOLD);
        let currently_visible = self.sidebar_open && (!compact || self.sidebar_user_override);
        if currently_visible {
            self.sidebar_open = false;
            self.sidebar_user_override = false;
            if self.focus_mode == FocusMode::Tasks {
                self.focus_mode = FocusMode::Transcript;
            }
        } else {
            self.sidebar_open = true;
            self.sidebar_user_override = compact;
        }
        match self.focus_mode {
            FocusMode::Composer => self.composer.focus_handle(cx).focus(window, cx),
            FocusMode::Buffer => self.transcript_editor.focus_handle(cx).focus(window, cx),
            FocusMode::Search => self.search_editor.focus_handle(cx).focus(window, cx),
            FocusMode::Request => {
                let editor_key = self.model.items.get(self.selected_item).and_then(|item| {
                    let questions = item.raw.get("questions")?.as_array()?;
                    let question_index = self
                        .request_question_cursor
                        .get(&item.key)
                        .copied()
                        .unwrap_or(0)
                        .min(questions.len().saturating_sub(1));
                    let question_id = questions.get(question_index)?.get("id")?.as_str()?;
                    Some(format!("{}:{question_id}", item.key))
                });
                if let Some(editor) = editor_key
                    .as_ref()
                    .and_then(|key| self.request_editors.get(key))
                {
                    editor.focus_handle(cx).focus(window, cx);
                } else {
                    self.transcript_focus.focus(window, cx);
                }
            }
            FocusMode::Tasks | FocusMode::Transcript | FocusMode::Approval => {
                self.transcript_focus.focus(window, cx);
            }
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.model.items.is_empty() {
            return;
        }
        self.selected_item = self
            .selected_item
            .saturating_add_signed(delta)
            .min(self.model.items.len() - 1);
        self.list_state.scroll_to_reveal_item(self.selected_item);
        cx.notify();
    }

    fn move_task_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.threads.is_empty() {
            return;
        }
        self.selected_task = self
            .selected_task
            .saturating_add_signed(delta)
            .min(self.threads.len() - 1);
        self.task_list_state
            .scroll_to_reveal_item(self.selected_task);
        cx.notify();
    }

    fn move_active_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        match self.focus_mode {
            FocusMode::Tasks => self.move_task_selection(delta, cx),
            FocusMode::Transcript => self.move_selection(delta, cx),
            FocusMode::Request => self.move_request_question(delta, cx),
            FocusMode::Composer | FocusMode::Search | FocusMode::Approval | FocusMode::Buffer => {}
        }
    }

    fn toggle_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(method) = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| item.pending_request.as_ref())
            .filter(|request| !request.resolved)
            .map(|request| request.method.clone())
        {
            if method == "item/tool/requestUserInput" {
                self.focus_mode = FocusMode::Request;
            } else if method == "mcpServer/elicitation/request"
                && self.model.items[self.selected_item]
                    .raw
                    .get("mode")
                    .and_then(Value::as_str)
                    == Some("form")
            {
                self.focus_mode = FocusMode::Request;
                let editor_key = self.model.items[self.selected_item]
                    .raw
                    .pointer("/requestedSchema/properties")
                    .and_then(Value::as_object)
                    .and_then(|properties| properties.keys().next())
                    .map(|name| format!("{}:mcp:{name}", self.model.items[self.selected_item].key));
                if let Some(editor_key) = editor_key {
                    let editor = self.request_editor(&editor_key, false, window, cx);
                    editor.focus_handle(cx).focus(window, cx);
                    cx.notify();
                    return;
                }
            } else if method.contains("requestApproval")
                || matches!(
                    method.as_str(),
                    "execCommandApproval" | "applyPatchApproval" | "mcpServer/elicitation/request"
                )
            {
                self.focus_mode = FocusMode::Approval;
                self.approval_cursor = 0;
            } else {
                return;
            }
            self.transcript_focus.focus(window, cx);
            cx.notify();
            return;
        }
        if let Some(item) = self.model.items.get_mut(self.selected_item) {
            if !item.kind.is_structured() && item.kind != model::TranscriptKind::Reasoning {
                return;
            }
            item.expanded = !item.expanded;
            self.list_state
                .splice(self.selected_item..self.selected_item + 1, 1);
            cx.notify();
        }
    }

    fn toggle_raw(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.model.items.get(self.selected_item) else {
            return;
        };
        if !self.raw_visible.remove(&item.key) {
            self.raw_visible.insert(item.key.clone());
        }
        self.list_state
            .splice(self.selected_item..self.selected_item + 1, 1);
        cx.notify();
    }

    fn toggle_visual(&mut self, cx: &mut Context<Self>) {
        self.visual_anchor = if self.visual_anchor.is_some() {
            None
        } else {
            Some(self.selected_item)
        };
        cx.notify();
    }

    fn yank_selected(&mut self, cx: &mut Context<Self>) {
        if self.model.items.is_empty() {
            return;
        }
        let anchor = self.visual_anchor.unwrap_or(self.selected_item);
        let start = anchor.min(self.selected_item);
        let end = anchor.max(self.selected_item);
        let content = self.model.items[start..=end]
            .iter()
            .map(|item| item.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(content));
        self.visual_anchor = None;
        cx.notify();
    }

    fn open_selected_task(&mut self, cx: &mut Context<Self>) {
        self.open_thread(self.selected_task, cx);
    }

    fn scroll_page(&mut self, direction: f32, cx: &mut Context<Self>) {
        let height = self.list_state.viewport_bounds().size.height;
        self.list_state.scroll_by(height * direction * 0.88);
        if !self.model.items.is_empty() {
            self.selected_item = self
                .list_state
                .logical_scroll_top()
                .item_ix
                .min(self.model.items.len() - 1);
        }
        cx.notify();
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_returns_to_buffer = self.focus_mode == FocusMode::Buffer;
        if !self.search_returns_to_buffer {
            self.buffer_search_backwards = false;
        }
        self.search_visible = true;
        self.focus_mode = FocusMode::Search;
        self.search_editor.update(cx, |editor, cx| {
            editor.set_text(self.search_query.clone(), window, cx)
        });
        self.search_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn open_buffer_search(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.buffer_view {
            return;
        }
        self.buffer_search_backwards = backwards;
        self.open_search(window, cx);
    }

    fn commit_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_query = self.search_editor.read(cx).text(cx).trim().to_string();
        if self.search_returns_to_buffer {
            self.search_visible = false;
            self.focus_mode = FocusMode::Buffer;
            self.transcript_editor.update(cx, |editor, cx| {
                editor.search(&self.search_query, self.buffer_search_backwards, window, cx);
            });
            self.transcript_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        }
        self.rebuild_search_matches();
        if !self.search_matches.is_empty() {
            self.active_search_match = self
                .search_matches
                .iter()
                .position(|index| *index >= self.selected_item)
                .unwrap_or(0);
            self.jump_to_search_match(cx);
        }
        self.focus_mode = FocusMode::Transcript;
        self.transcript_focus.focus(window, cx);
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_visible = false;
        if self.search_returns_to_buffer {
            self.transcript_editor
                .update(cx, |editor, cx| editor.clear_search(cx));
            self.focus_mode = FocusMode::Buffer;
            self.transcript_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        }
        self.search_query.clear();
        self.search_matches.clear();
        self.active_search_match = 0;
        self.focus_transcript(window, cx);
    }

    fn rebuild_search_matches(&mut self) {
        let query = self.search_query.to_lowercase();
        self.search_matches = if query.is_empty() {
            Vec::new()
        } else {
            self.model
                .items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    (item.title.to_lowercase().contains(&query)
                        || item.content.to_lowercase().contains(&query))
                    .then_some(index)
                })
                .collect()
        };
        self.active_search_match = self
            .active_search_match
            .min(self.search_matches.len().saturating_sub(1));
    }

    fn jump_to_search_match(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.search_matches.get(self.active_search_match).copied() {
            self.selected_item = index;
            self.list_state.scroll_to_reveal_item(index);
            cx.notify();
        }
    }

    fn move_search_match(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_returns_to_buffer {
            self.transcript_editor.update(cx, |editor, cx| {
                editor.repeat_search(delta < 0, window, cx);
            });
            cx.notify();
            return;
        }
        if self.search_matches.is_empty() {
            return;
        }
        let len = self.search_matches.len();
        self.active_search_match = if delta < 0 {
            self.active_search_match.checked_sub(1).unwrap_or(len - 1)
        } else {
            (self.active_search_match + 1) % len
        };
        self.jump_to_search_match(cx);
    }

    fn key_context(&self) -> KeyContext {
        let mut context = KeyContext::new_with_defaults();
        context.add("Harness");
        if self.search_visible {
            context.add("HarnessSearchVisible");
        }
        if self.buffer_view
            && self
                .model
                .items
                .get(self.selected_item)
                .is_some_and(|item| self.request_surfaces.contains_key(&item.key))
        {
            context.add("HarnessPendingRequest");
        }
        match self.focus_mode {
            FocusMode::Tasks => context.add("HarnessTasks"),
            FocusMode::Transcript => context.add("HarnessTranscript"),
            FocusMode::Composer => context.add("HarnessComposer"),
            FocusMode::Search => context.add("HarnessSearch"),
            FocusMode::Request => context.add("HarnessRequest"),
            FocusMode::Approval => context.add("HarnessApproval"),
            FocusMode::Buffer => context.add("HarnessBuffer"),
        }
        context
    }

    fn render_task(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let thread = &self.threads[index];
        let selected = self.selected_thread_id.as_deref() == Some(thread.id.as_str());
        let cursor = self.focus_mode == FocusMode::Tasks && self.selected_task == index;
        let title = thread_title(thread);
        let project = Path::new(&thread.cwd)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Codex".into());
        let status = if selected
            && self.model.items.iter().any(|item| {
                item.pending_request
                    .as_ref()
                    .is_some_and(|request| !request.resolved)
            }) {
            AgentThreadStatus::WaitingForConfirmation
        } else if selected && self.model.current_turn_id.is_some() {
            AgentThreadStatus::Running
        } else if selected && self.error.is_some() {
            AgentThreadStatus::Error
        } else {
            AgentThreadStatus::Completed
        };
        let weak = cx.weak_entity();
        ThreadItem::new(("task", index), title)
            .icon(IconName::AiOpenAi)
            .project_name(project)
            .timestamp(relative_time(thread.updated_at))
            .status(status)
            .selected(selected)
            .focused(cursor)
            .base_bg(colors.panel_background)
            .on_click(move |_, _, cx| {
                weak.update(cx, |this, cx| {
                    this.selected_task = index;
                    this.open_thread(index, cx);
                })
                .ok();
            })
            .into_any_element()
    }

    fn render_replay_task(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        ThreadItem::new("replay-task", "Performance replay")
            .icon(IconName::AiOpenAi)
            .project_name(format!(
                "{} virtual rows",
                self.replay_count.unwrap_or_default()
            ))
            .timestamp("now")
            .status(AgentThreadStatus::WaitingForConfirmation)
            .selected(true)
            .focused(self.focus_mode == FocusMode::Tasks)
            .base_bg(colors.panel_background)
            .into_any_element()
    }

    fn markdown_for(
        &mut self,
        key: &str,
        source: &str,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        if let Some(cached) = self.markdown_cache.get_mut(key) {
            if cached.source != source {
                cached.source = source.to_string();
                cached.entity.update(cx, |markdown, cx| {
                    markdown.reset(source.to_string().into(), cx)
                });
            }
            return cached.entity.clone();
        }

        let entity = cx.new(|cx| Markdown::new(source.to_string().into(), None, None, cx));
        self.markdown_cache.insert(
            key.to_string(),
            CachedMarkdown {
                source: source.to_string(),
                entity: entity.clone(),
            },
        );
        entity
    }

    fn render_diff(content: &str, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let line_count = content.lines().count();
        let lines = content
            .lines()
            .take(1_200)
            .enumerate()
            .map(|(index, line)| {
                let addition = line.starts_with('+') && !line.starts_with("+++");
                let deletion = line.starts_with('-') && !line.starts_with("---");
                let hunk = line.starts_with("@@");
                div()
                    .w_full()
                    .min_h(px(22.))
                    .px_3()
                    .py_0p5()
                    .flex()
                    .gap_3()
                    .font_buffer(cx)
                    .text_ui_sm(cx)
                    .bg(if addition {
                        colors.version_control_added.opacity(0.12)
                    } else if deletion {
                        colors.version_control_deleted.opacity(0.12)
                    } else {
                        colors.editor_background
                    })
                    .text_color(if addition {
                        colors.version_control_added
                    } else if deletion {
                        colors.version_control_deleted
                    } else if hunk {
                        colors.text_accent
                    } else {
                        colors.text
                    })
                    .child(
                        div()
                            .w(px(34.))
                            .flex_none()
                            .text_color(colors.text_muted)
                            .child((index + 1).to_string()),
                    )
                    .child(div().min_w_0().whitespace_nowrap().child(line.to_string()))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .id(("diff-scroll", index))
            .w_full()
            .overflow_x_scroll()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
            .children(lines)
            .when(line_count > 1_200, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_ui_sm(cx)
                        .text_color(colors.text_muted)
                        .child(format!(
                            "{} more lines · switch to TEXT for the complete selectable diff",
                            line_count - 1_200
                        )),
                )
            })
            .into_any_element()
    }

    fn render_reasoning(content: &str, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let steps = content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.trim_matches('*')
                    .trim_start_matches('#')
                    .trim_start_matches("- ")
                    .trim()
                    .to_string()
            })
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        let hidden_count = steps.len().saturating_sub(24);
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .children(steps.into_iter().skip(hidden_count).map(|step| {
                div()
                    .w_full()
                    .flex()
                    .items_start()
                    .gap_2()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(
                        div()
                            .mt(px(7.))
                            .size(px(5.))
                            .flex_none()
                            .rounded_full()
                            .bg(colors.text_accent.opacity(0.7)),
                    )
                    .child(div().min_w_0().flex_1().child(step))
            }))
            .when(hidden_count > 0, |this| {
                this.child(
                    div()
                        .pl_4()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(format!(
                            "{hidden_count} earlier steps · Shift-V opens text-buffer view"
                        )),
                )
            })
            .into_any_element()
    }

    fn render_terminal(content: String, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        div()
            .id(("terminal-scroll", index))
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
            .p_3()
            .font_buffer(cx)
            .text_ui_sm(cx)
            .line_height(relative(1.45))
            .text_color(colors.text)
            .whitespace_normal()
            .child(content)
            .into_any_element()
    }

    fn render_plain_prose(content: &str, cx: &mut Context<Self>) -> AnyElement {
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .text_ui(cx)
            .line_height(relative(1.55))
            .children(content.split("\n\n").map(|paragraph| {
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .children(paragraph.lines().map(|line| {
                        div()
                            .min_h(px(20.))
                            .whitespace_normal()
                            .child(line.to_string())
                    }))
            }))
            .into_any_element()
    }

    fn render_image(
        item: &TranscriptItem,
        surface: Option<Entity<ImageSurface>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let surface_height = surface
            .as_ref()
            .map(|surface| px(surface.read(cx).rows() as f32 * 20.));
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .when_some(surface.zip(surface_height), |this, (surface, height)| {
                this.child(
                    div()
                        .w_full()
                        .h(height)
                        .min_h(px(56.))
                        .overflow_hidden()
                        .child(surface),
                )
            })
            .when(!item.content.is_empty(), |this| {
                this.child(
                    div()
                        .text_ui(cx)
                        .line_height(relative(1.45))
                        .text_color(colors.text_muted)
                        .child(item.content.clone()),
                )
            })
            .into_any_element()
    }

    fn render_pending_request_summary(item: &TranscriptItem, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let method = item
            .pending_request
            .as_ref()
            .map(|request| request.method.as_str())
            .unwrap_or_default();
        let reason = item
            .raw
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .map(ToOwned::to_owned);
        let primary = match method {
            "item/commandExecution/requestApproval" => item
                .raw
                .get("command")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            "execCommandApproval" => {
                item.raw
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            }
            "item/fileChange/requestApproval" => item
                .raw
                .get("grantRoot")
                .and_then(Value::as_str)
                .map(|root| format!("Write access under {root}")),
            "applyPatchApproval" => {
                item.raw
                    .get("fileChanges")
                    .and_then(Value::as_object)
                    .map(|changes| {
                        let files = changes.keys().cloned().collect::<Vec<_>>();
                        match files.as_slice() {
                            [] => "Apply requested patch".into(),
                            [file] => format!("Change {file}"),
                            _ => format!("Change {} files", files.len()),
                        }
                    })
            }
            "item/permissions/requestApproval" => Some("Additional permissions requested".into()),
            _ => None,
        };
        let cwd = item
            .raw
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(ToOwned::to_owned);
        let permissions = (method == "item/permissions/requestApproval")
            .then(|| item.raw.get("permissions"))
            .flatten()
            .map(|permissions| {
                serde_json::to_string_pretty(permissions)
                    .unwrap_or_else(|_| permissions.to_string())
            });

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .when_some(primary, |this, primary| {
                this.child(
                    div()
                        .rounded_md()
                        .bg(colors.editor_background)
                        .px_3()
                        .py_2()
                        .font_buffer(cx)
                        .text_ui_sm(cx)
                        .text_color(colors.text)
                        .child(primary),
                )
            })
            .when_some(cwd, |this, cwd| {
                this.child(Label::new(cwd).size(LabelSize::XSmall).color(Color::Muted))
            })
            .when_some(reason, |this, reason| {
                this.child(
                    Label::new(reason)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(permissions, |this, permissions| {
                this.child(
                    div()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.editor_background)
                        .px_3()
                        .py_2()
                        .font_buffer(cx)
                        .text_ui_xs(cx)
                        .text_color(colors.text_muted)
                        .child(permissions),
                )
            })
            .into_any_element()
    }

    fn render_user_input_request(
        &mut self,
        index: usize,
        item: &TranscriptItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let questions = item
            .raw
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let request_key = item.key.clone();
        let mut rendered_questions = Vec::with_capacity(questions.len());

        for (question_index, question) in questions.into_iter().enumerate() {
            let question_id = question
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("question")
                .to_string();
            let header = question
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("Input")
                .to_string();
            let prompt = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let selected = self
                .request_answers
                .get(&request_key)
                .and_then(|answers| answers.get(&question_id))
                .cloned()
                .unwrap_or_default();
            let active_question = self.focus_mode == FocusMode::Request
                && self.selected_item == index
                && self
                    .request_question_cursor
                    .get(&request_key)
                    .copied()
                    .unwrap_or(0)
                    == question_index;
            let option_cursor = self
                .request_option_cursor
                .get(&format!("{request_key}:{question_id}"))
                .copied()
                .unwrap_or(0);
            let mut option_rows = Vec::with_capacity(options.len());
            for (option_index, option) in options.iter().enumerate() {
                let label = option
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("Option")
                    .to_string();
                let description = option
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let is_selected = selected.contains(&label);
                let option_request_key = request_key.clone();
                let option_question_id = question_id.clone();
                let answer = label.clone();
                let option_weak = cx.weak_entity();
                let selected_icon: Option<Icon> = is_selected.then(|| {
                    Icon::new(IconName::Check)
                        .size(IconSize::XSmall)
                        .color(Color::Accent)
                });
                option_rows.push(
                    ListItem::new(format!("request-option-{index}-{question_id}-{label}"))
                        .spacing(ListItemSpacing::Sparse)
                        .rounded()
                        .toggle_state(is_selected)
                        .focused(active_question && option_index == option_cursor)
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(Label::new(label).size(LabelSize::Small))
                                .when(!description.is_empty(), |this| {
                                    this.child(
                                        Label::new(description)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                }),
                        )
                        .end_slot::<Icon>(selected_icon)
                        .on_click(move |_, _, cx| {
                            option_weak
                                .update(cx, |this, cx| {
                                    this.choose_request_option(
                                        index,
                                        option_request_key.clone(),
                                        option_question_id.clone(),
                                        answer.clone(),
                                        cx,
                                    )
                                })
                                .ok();
                        })
                        .into_any_element(),
                );
            }

            let needs_text = options.is_empty()
                || question
                    .get("isOther")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let editor = needs_text.then(|| {
                let editor_key = format!("{request_key}:{question_id}");
                let secret = question
                    .get("isSecret")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.request_editor(&editor_key, secret, window, cx)
            });
            rendered_questions.push(
                div()
                    .w_full()
                    .pl_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Label::new(header).size(LabelSize::Small).color(
                                if active_question {
                                    Color::Accent
                                } else {
                                    Color::Default
                                },
                            ))
                            .child(
                                Label::new(prompt)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .children(option_rows)
                    .when_some(editor, |this, editor| {
                        this.child(
                            div()
                                .h(px(38.))
                                .rounded_md()
                                .border_1()
                                .border_color(colors.border_variant)
                                .bg(colors.editor_background)
                                .px_2()
                                .child(editor)
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.focus_mode = FocusMode::Request;
                                        cx.stop_propagation();
                                        cx.notify();
                                    }),
                                ),
                        )
                    })
                    .into_any_element(),
            );
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .children(rendered_questions)
            .child(
                div().flex().justify_end().child(
                    action_button("Submit answers", Some(TintColor::Accent), false).on_click(
                        cx.listener(move |this, _, _, cx| this.submit_user_input(index, cx)),
                    ),
                ),
            )
            .into_any_element()
    }

    fn render_mcp_elicitation(
        &mut self,
        index: usize,
        item: &TranscriptItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let mode = item
            .raw
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = item
            .raw
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("An MCP server is requesting input.")
            .to_string();
        let server = item
            .raw
            .get("serverName")
            .and_then(Value::as_str)
            .unwrap_or("MCP server")
            .to_string();
        let mut content = div().w_full().flex().flex_col().gap_3().child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    Label::new(server)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(message).size(LabelSize::Small)),
        );

        match mode {
            "form" => {
                let properties = item
                    .raw
                    .pointer("/requestedSchema/properties")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let required = item
                    .raw
                    .pointer("/requestedSchema/required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<HashSet<_>>();
                let mut fields = Vec::with_capacity(properties.len());
                for (name, schema) in properties {
                    let title = schema.get("title").and_then(Value::as_str).unwrap_or(&name);
                    let required_suffix = if required.contains(name.as_str()) {
                        " · required"
                    } else {
                        ""
                    };
                    let description = schema
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let hint = mcp_form_field_hint(&schema);
                    let editor_key = format!("{}:mcp:{name}", item.key);
                    let editor = self.request_editor(&editor_key, false, window, cx);
                    fields.push(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                Label::new(format!("{title}{required_suffix}"))
                                    .size(LabelSize::Small),
                            )
                            .when(!description.is_empty(), |this| {
                                this.child(
                                    Label::new(description.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .when(!hint.is_empty(), |this| {
                                this.child(
                                    Label::new(hint).size(LabelSize::XSmall).color(Color::Muted),
                                )
                            })
                            .child(
                                div()
                                    .w_full()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border_variant)
                                    .bg(colors.editor_background)
                                    .px_2()
                                    .py_1()
                                    .child(editor)
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.focus_mode = FocusMode::Request;
                                            cx.stop_propagation();
                                            cx.notify();
                                        }),
                                    ),
                            )
                            .into_any_element(),
                    );
                }
                content = content
                    .children(fields)
                    .child(div().flex().justify_end().child(
                        action_button("Submit form", Some(TintColor::Accent), false).on_click(
                            cx.listener(move |this, _, _, cx| this.submit_mcp_form(index, cx)),
                        ),
                    ));
            }
            "url" => {
                let url = item
                    .raw
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let open_url = url.clone();
                content = content
                    .child(
                        div()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border_variant)
                            .bg(colors.editor_background)
                            .px_3()
                            .py_2()
                            .font_buffer(cx)
                            .text_ui_xs(cx)
                            .text_color(colors.text_muted)
                            .child(url),
                    )
                    .child(
                        div().flex().child(
                            action_button("Open link", Some(TintColor::Accent), false)
                                .on_click(move |_, _, cx| cx.open_url(&open_url)),
                        ),
                    );
            }
            "openai/form" => {
                content = content.child(
                    Label::new(
                        "This server requested an extended form that Harness cannot safely render. Decline or cancel to continue.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                );
            }
            _ => {}
        }
        content.into_any_element()
    }

    fn render_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let item = self.model.items[index].clone();
        let cursor = matches!(
            self.focus_mode,
            FocusMode::Transcript | FocusMode::Request | FocusMode::Approval
        ) && index == self.selected_item;
        let visual = self.visual_anchor.is_some_and(|anchor| {
            (anchor.min(self.selected_item)..=anchor.max(self.selected_item)).contains(&index)
        });
        let raw_visible = self.raw_visible.contains(&item.key);
        let compact_trace = item.kind == model::TranscriptKind::Trace && !item.expanded;
        let request_method = item
            .pending_request
            .as_ref()
            .map(|request| request.method.as_str());
        let pending_method = item
            .pending_request
            .as_ref()
            .filter(|request| !request.resolved)
            .map(|request| request.method.as_str());
        let response_choices = item
            .pending_request
            .as_ref()
            .filter(|request| !request.resolved)
            .map(|request| request_choices(&request.method, &item.raw))
            .unwrap_or_default();
        let has_elicitation = pending_method == Some("mcpServer/elicitation/request");
        let has_user_input = pending_method == Some("item/tool/requestUserInput");
        let request_surface = self
            .request_surfaces
            .get(&item.key)
            .map(|entry| entry.entity.clone());
        let uses_shared_request_surface = request_surface.is_some();
        let has_approval = !uses_shared_request_surface && !response_choices.is_empty();
        let approval_focused =
            self.focus_mode == FocusMode::Approval && index == self.selected_item;
        let approval_cursor = self.approval_cursor;
        let colors = cx.theme().colors().clone();
        let narrative = matches!(
            item.kind,
            model::TranscriptKind::User
                | model::TranscriptKind::Agent
                | model::TranscriptKind::Reasoning
                | model::TranscriptKind::Plan
        );
        let streaming = item.status.as_deref() == Some("streaming");
        let markdown = (narrative
            && item.kind != model::TranscriptKind::Reasoning
            && item.expanded
            && !streaming
            && !item.content.is_empty())
        .then(|| self.markdown_for(&item.key, &item.content, cx));
        let icon = icon_for_kind(item.kind);
        let user_input = (!uses_shared_request_surface && has_user_input)
            .then(|| self.render_user_input_request(index, &item, window, cx));
        let mcp_elicitation = (!uses_shared_request_surface && has_elicitation)
            .then(|| self.render_mcp_elicitation(index, &item, window, cx));
        let pending_summary = request_method
            .filter(|_| !uses_shared_request_surface)
            .filter(|method| {
                !matches!(
                    *method,
                    "item/tool/requestUserInput" | "mcpServer/elicitation/request"
                )
            })
            .map(|_| Self::render_pending_request_summary(&item, cx));
        let choice_buttons = response_choices
            .into_iter()
            .filter(|_| !uses_shared_request_surface)
            .enumerate()
            .map(|(choice_index, choice)| {
                let (icon, color) = request_choice_visual(choice.tone);
                let label = choice.label.clone();
                decision_button(
                    label,
                    icon,
                    color,
                    approval_focused && approval_cursor == choice_index,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.respond_with_choice(index, choice.clone(), cx)
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();
        let reasoning_preview = (item.kind == model::TranscriptKind::Reasoning
            && !item.expanded
            && !item.content.is_empty())
        .then(|| compact_reasoning_preview(&item.content));
        let visible_status = item.display_status().map(ToOwned::to_owned);
        let disclosure_weak = cx.weak_entity();

        let header = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(Icon::new(icon).size(IconSize::Small).color(if cursor {
                Color::Accent
            } else {
                Color::Muted
            }))
            .child(
                Label::new(item.title.clone())
                    .size(LabelSize::Small)
                    .color(if cursor { Color::Default } else { Color::Muted }),
            )
            .when_some(visible_status, |this, status| {
                this.child(
                    Label::new(status)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(div().flex_1())
            .when(
                item.kind.is_structured() || item.kind == model::TranscriptKind::Reasoning,
                |this| {
                    this.child(
                        Disclosure::new(("item-disclosure", index), item.expanded).on_click(
                            move |_, _, cx| {
                                disclosure_weak
                                    .update(cx, |this, cx| {
                                        if let Some(item) = this.model.items.get_mut(index) {
                                            item.expanded = !item.expanded;
                                            this.list_state.splice(index..index + 1, 1);
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                            },
                        ),
                    )
                },
            );

        let body = if request_method.is_some() || !item.expanded || item.content.is_empty() {
            None
        } else if let Some(markdown) = markdown {
            let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
            style.code_block_overflow_x_scroll = true;
            Some(
                div()
                    .w_full()
                    .min_w_0()
                    .child(MarkdownElement::new(markdown, style))
                    .into_any_element(),
            )
        } else {
            Some(match item.kind {
                model::TranscriptKind::User
                | model::TranscriptKind::Agent
                | model::TranscriptKind::Plan => Self::render_plain_prose(&item.content, cx),
                model::TranscriptKind::Reasoning => Self::render_reasoning(&item.content, cx),
                model::TranscriptKind::Command => {
                    Self::render_terminal(item.content.clone(), index, cx)
                }
                model::TranscriptKind::Diff | model::TranscriptKind::FileChange => {
                    Self::render_diff(&item.content, index, cx)
                }
                model::TranscriptKind::Image => {
                    Self::render_image(&item, self.image_surfaces.get(&item.key).cloned(), cx)
                }
                _ => Self::render_terminal(item.content.clone(), index, cx),
            })
        };

        let raw = raw_visible.then(|| {
            let content =
                serde_json::to_string_pretty(&item.raw).unwrap_or_else(|_| item.raw.to_string());
            div()
                .mt_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.editor_background)
                .p_3()
                .font_buffer(cx)
                .text_ui_xs(cx)
                .text_color(colors.text_muted)
                .child(content)
                .into_any_element()
        });

        let content = if narrative {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .when(item.kind == model::TranscriptKind::User, |this| {
                    this.rounded_lg()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.element_background)
                        .p_4()
                })
                .when(
                    matches!(
                        item.kind,
                        model::TranscriptKind::Reasoning | model::TranscriptKind::Plan
                    ),
                    |this| {
                        this.border_l_2()
                            .border_color(colors.text_accent.opacity(0.55))
                            .pl_4()
                            .py_1()
                    },
                )
                .child(header)
                .when_some(reasoning_preview, |this, preview| {
                    this.child(
                        div()
                            .pl_6()
                            .text_sm()
                            .text_color(colors.text_muted)
                            .child(preview),
                    )
                })
                .when_some(body, |this, body| this.child(body))
                .when_some(raw, |this, raw| this.child(raw))
                .into_any_element()
        } else {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .when(!compact_trace, |this| {
                    this.rounded_lg()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.element_background.opacity(0.72))
                        .p_3()
                })
                .when(compact_trace, |this| this.px_1().py_1())
                .child(header)
                .when_some(request_surface, |this, surface| this.child(surface))
                .when_some(body, |this, body| this.child(body))
                .when_some(raw, |this, raw| this.child(raw))
                .when_some(pending_summary, |this, summary| this.child(summary))
                .when_some(user_input, |this, input| this.child(input))
                .when_some(mcp_elicitation, |this, elicitation| this.child(elicitation))
                .when(has_approval, |this| {
                    this.child(
                        div()
                            .mt_1()
                            .pt_1()
                            .border_t_1()
                            .border_color(colors.border_variant)
                            .flex()
                            .flex_wrap()
                            .gap_0p5()
                            .children(choice_buttons),
                    )
                })
                .into_any_element()
        };

        div()
            .id(("transcript-item", index))
            .w_full()
            .px_6()
            .py(if compact_trace { px(3.) } else { px(12.) })
            .cursor_pointer()
            .border_l_2()
            .border_color(if cursor {
                colors.text_accent
            } else if visual {
                colors.text_accent.opacity(0.45)
            } else {
                gpui::transparent_black()
            })
            .when(visual, |this| {
                this.bg(colors.element_selection_background.opacity(0.45))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_item = index;
                this.visual_anchor = None;
                this.focus_transcript(window, cx);
            }))
            .child(content)
            .into_any_element()
    }

    fn connection_label(&self) -> SharedString {
        if let Some(count) = self.replay_count {
            return format!("REPLAY {count}").into();
        }
        if self.connecting {
            "CONNECTING".into()
        } else if self.client.is_some() {
            "APP SERVER".into()
        } else {
            "OFFLINE".into()
        }
    }
}

impl Render for HarnessApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_image_surfaces(window, cx);
        self.sync_request_surfaces(window, cx);
        let colors = cx.theme().colors().clone();
        let compact = window.viewport_size().width < px(COMPACT_SIDEBAR_THRESHOLD);
        let sidebar_visible = self.sidebar_open && (!compact || self.sidebar_user_override);
        let composer_empty = self.composer.read(cx).text(cx).trim().is_empty();
        let turn_active = self.model.current_turn_id.is_some();
        let list_state = self.list_state.clone();
        let task_list_state = self.task_list_state.clone();
        let command_palette = self.command_palette.clone();
        let task_body = if self.replay_count.is_some() {
            div()
                .flex_1()
                .min_h_0()
                .child(self.render_replay_task(cx))
                .into_any_element()
        } else if self.threads.is_empty() {
            div()
                .flex_1()
                .min_h_0()
                .p_4()
                .text_sm()
                .text_color(colors.text_muted)
                .child(if self.connecting {
                    "Connecting to Codex…"
                } else {
                    "No tasks"
                })
                .into_any_element()
        } else {
            list(
                task_list_state,
                cx.processor(|this, index, _, cx| this.render_task(index, cx)),
            )
            .flex_1()
            .min_h_0()
            .into_any_element()
        };
        let transcript_body = if self.buffer_view {
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .bg(colors.editor_background)
                .child(self.transcript_editor.clone())
                .into_any_element()
        } else {
            list(
                list_state,
                cx.processor(|this, index, window, cx| this.render_item(index, window, cx)),
            )
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .into_any_element()
        };
        let search_status = if self.search_returns_to_buffer {
            "Enter to jump".to_string()
        } else if self.search_query.is_empty() {
            "Enter to search".to_string()
        } else if self.search_matches.is_empty() {
            "No matches".to_string()
        } else {
            format!(
                "{} / {}",
                self.active_search_match + 1,
                self.search_matches.len()
            )
        };
        let pending_request = self
            .model
            .items
            .get(self.selected_item)
            .and_then(|item| item.pending_request.as_ref())
            .filter(|request| !request.resolved);
        let approval_count = pending_request
            .map(|request| {
                request_choices(&request.method, &self.model.items[self.selected_item].raw).len()
            })
            .unwrap_or_default()
            .max(1);
        let has_any_pending_request = !self.request_surfaces.is_empty();
        let active_region: SharedString = match self.focus_mode {
            FocusMode::Tasks => format!(
                "Task {}/{}",
                self.selected_task.saturating_add(1),
                if self.replay_count.is_some() {
                    1
                } else {
                    self.threads.len().max(1)
                }
            )
            .into(),
            FocusMode::Transcript => format!(
                "Block {}/{}",
                self.selected_item.saturating_add(1),
                self.model.items.len().max(1)
            )
            .into(),
            FocusMode::Composer => "Composer".into(),
            FocusMode::Search => "Search".into(),
            FocusMode::Request => "Request".into(),
            FocusMode::Approval => format!(
                "Approval {}/{}",
                self.approval_cursor.min(approval_count - 1) + 1,
                approval_count
            )
            .into(),
            FocusMode::Buffer => {
                let semantic_total = self
                    .model
                    .items
                    .iter()
                    .filter(|item| item.kind != model::TranscriptKind::Trace)
                    .count()
                    .max(1);
                let semantic_position = self
                    .model
                    .items
                    .iter()
                    .take(self.selected_item.saturating_add(1))
                    .filter(|item| item.kind != model::TranscriptKind::Trace)
                    .count()
                    .clamp(1, semantic_total);
                format!("Text {semantic_position}/{semantic_total}").into()
            }
        };
        let navigation_hint = match self.focus_mode {
            FocusMode::Tasks => "j/k move · Enter open · l transcript",
            FocusMode::Transcript if self.visual_anchor.is_some() => {
                "j/k extend · y copy · v cancel"
            }
            FocusMode::Transcript => "j/k blocks · Shift-V text view · / find",
            FocusMode::Composer => "Ctrl-W K transcript",
            FocusMode::Search if self.search_returns_to_buffer => "Enter jump · Esc cancel",
            FocusMode::Search => "Enter jump · Esc close",
            FocusMode::Request
                if pending_request
                    .is_some_and(|request| request.method == "mcpServer/elicitation/request") =>
            {
                "Edit field · Ctrl-Enter submit · Esc return"
            }
            FocusMode::Request => "j/k question · h/l option · Enter choose · i type",
            FocusMode::Approval => "h/l choose · Enter confirm · Esc cancel",
            FocusMode::Buffer if has_any_pending_request => {
                "Enter on a request to answer · Zed Vim motions"
            }
            FocusMode::Buffer => "Zed Vim motions · / find · Ctrl-W J composer",
        };

        div()
            .key_context(self.key_context())
            .track_focus(&self.transcript_focus)
            .size_full()
            .flex()
            .bg(colors.background)
            .text_color(colors.text)
            .font_ui(cx)
            .on_action(cx.listener(|this, _: &Send, window, cx| this.send(window, cx)))
            .on_action(cx.listener(|this, _: &Stop, _, cx| this.stop(cx)))
            .on_action(cx.listener(|this, _: &FocusTranscript, window, cx| {
                this.focus_transcript(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusTasks, window, cx| this.focus_tasks(window, cx)))
            .on_action(
                cx.listener(|this, _: &FocusComposer, window, cx| this.focus_composer(window, cx)),
            )
            .on_action(cx.listener(|_this, _: &NormalEscape, window, cx| {
                if let Ok(action) = cx.build_action("vim::ClearOperators", None) {
                    window.dispatch_action(action, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_active_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_active_selection(1, cx)))
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                if this.focus_mode == FocusMode::Approval {
                    this.move_approval_option(-1, cx)
                } else {
                    this.move_request_option(-1, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                if this.focus_mode == FocusMode::Approval {
                    this.move_approval_option(1, cx)
                } else {
                    this.move_request_option(1, cx)
                }
            }))
            .on_action(
                cx.listener(|this, _: &ChooseRequest, _, cx| {
                    this.choose_current_request_option(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ChooseApproval, window, cx| {
                    this.choose_approval(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &OpenRequestSurface, window, cx| {
                this.focus_selected_request_surface(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ReturnFromRequest, window, cx| {
                this.return_from_request(window, cx)
            }))
            .on_action(cx.listener(|this, _: &SubmitRequest, _, cx| this.submit_active_request(cx)))
            .on_action(cx.listener(|this, _: &EditRequest, window, cx| {
                this.edit_current_request(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleBufferView, window, cx| {
                this.toggle_buffer_view(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowRichTranscript, window, cx| {
                if this.buffer_view {
                    this.show_rich_transcript(window, cx);
                } else {
                    this.focus_transcript(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ShowTextTranscript, window, cx| {
                this.show_text_transcript(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
                this.open_command_palette("", window, cx)
            }))
            .on_action(cx.listener(|this, action: &OpenWithQuery, window, cx| {
                this.open_command_palette(&action.query, window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoTop, _, cx| {
                if this.focus_mode == FocusMode::Tasks {
                    this.selected_task = 0;
                    this.task_list_state.scroll_to(gpui::ListOffset::default());
                } else {
                    this.selected_item = 0;
                    this.list_state.scroll_to(gpui::ListOffset::default());
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &GoBottom, _, cx| {
                if this.focus_mode == FocusMode::Tasks {
                    this.selected_task = this.threads.len().saturating_sub(1);
                    this.task_list_state.scroll_to_end();
                } else {
                    this.selected_item = this.model.items.len().saturating_sub(1);
                    this.list_state.scroll_to_end();
                }
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &ToggleItem, window, cx| this.toggle_selected(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleRaw, _, cx| this.toggle_raw(cx)))
            .on_action(cx.listener(|this, _: &ToggleVisual, _, cx| this.toggle_visual(cx)))
            .on_action(cx.listener(|this, _: &YankItem, _, cx| this.yank_selected(cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.scroll_page(-1., cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.scroll_page(1., cx)))
            .on_action(cx.listener(|this, _: &OpenTask, _, cx| this.open_selected_task(cx)))
            .on_action(cx.listener(|this, _: &OpenSearch, window, cx| this.open_search(window, cx)))
            .on_action(cx.listener(|this, action: &VimSearch, window, cx| {
                this.open_buffer_search(action.backwards, window, cx)
            }))
            .on_action(cx.listener(|this, _: &VimNextMatch, window, cx| {
                this.move_search_match(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &VimPreviousMatch, window, cx| {
                this.move_search_match(-1, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &CommitSearch, window, cx| this.commit_search(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseSearch, window, cx| this.close_search(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &NextMatch, window, cx| {
                    this.move_search_match(1, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &PreviousMatch, window, cx| {
                this.move_search_match(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewTask, window, cx| this.new_task(window, cx)))
            .on_action(cx.listener(|this, _: &RefreshTasks, _, cx| this.refresh_threads(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)),
            )
            .when(sidebar_visible, |this| {
                this.child(
                    div()
                        .w(px(SIDEBAR_WIDTH))
                        .h_full()
                        .flex_none()
                        .border_r_1()
                        .border_color(colors.border)
                        .bg(colors.panel_background)
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .h(px(32.))
                                .px_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .border_b_1()
                                .border_color(colors.border)
                                .child(
                                    Label::new("Tasks")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            IconButton::new("refresh-tasks", IconName::RotateCw)
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Default)
                                                .style(ButtonStyle::Subtle)
                                                .aria_label("Refresh tasks")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.refresh_threads(cx)
                                                })),
                                        )
                                        .child(
                                            IconButton::new("new-task", IconName::Plus)
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Default)
                                                .style(ButtonStyle::Subtle)
                                                .aria_label("New task")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.new_task(window, cx)
                                                })),
                                        ),
                                ),
                        )
                        .child(task_body),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(32.))
                            .flex_none()
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_3()
                            .border_b_1()
                            .border_color(colors.border)
                            .child(
                                IconButton::new(
                                    "toggle-sidebar",
                                    if sidebar_visible {
                                        IconName::ThreadsSidebarLeftOpen
                                    } else {
                                        IconName::ThreadsSidebarLeftClosed
                                    },
                                )
                                .shape(IconButtonShape::Square)
                                .size(ButtonSize::Default)
                                .style(ButtonStyle::Subtle)
                                .aria_label("Toggle task rail")
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.toggle_sidebar(window, cx),
                                )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .truncate()
                                    .child(self.selected_title.clone()),
                            ),
                    )
                    .when(self.search_visible, |this| {
                        this.child(
                            div()
                                .key_context("HarnessSearch")
                                .h(px(42.))
                                .flex_none()
                                .px_4()
                                .flex()
                                .items_center()
                                .gap_2()
                                .border_b_1()
                                .border_color(colors.border)
                                .bg(colors.toolbar_background)
                                .child(
                                    Icon::new(IconName::MagnifyingGlass)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(div().flex_1().min_w_0().child(self.search_editor.clone()))
                                .child(
                                    Label::new(search_status)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    IconButton::new("previous-search-match", IconName::ArrowUp)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Default)
                                        .style(ButtonStyle::Subtle)
                                        .aria_label("Previous match")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.move_search_match(-1, window, cx)
                                        })),
                                )
                                .child(
                                    IconButton::new("next-search-match", IconName::ArrowDown)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Default)
                                        .style(ButtonStyle::Subtle)
                                        .aria_label("Next match")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.move_search_match(1, window, cx)
                                        })),
                                )
                                .child(
                                    IconButton::new("close-search", IconName::Close)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Default)
                                        .style(ButtonStyle::Subtle)
                                        .aria_label("Close search")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_search(window, cx)
                                        })),
                                ),
                        )
                    })
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .flex_none()
                                .px_4()
                                .py_2()
                                .border_b_1()
                                .border_color(colors.border)
                                .bg(colors.surface_background)
                                .text_xs()
                                .text_color(cx.theme().status().error)
                                .truncate()
                                .child(error),
                        )
                    })
                    .child(div().flex_1().min_h_0().flex().child(transcript_body))
                    .when(self.model.items.is_empty(), |this| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(32.))
                                .left(px(0.))
                                .right(px(0.))
                                .bottom(px(150.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(colors.text_muted)
                                .child(if self.loading_thread {
                                    "Loading task history…"
                                } else {
                                    "What should we build?"
                                }),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .min_h(px(72.))
                            .max_h(px(280.))
                            .border_t_1()
                            .border_color(if self.focus_mode == FocusMode::Composer {
                                colors.border_focused
                            } else {
                                colors.border
                            })
                            .bg(colors.editor_background)
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .min_h(px(48.))
                                    .px_3()
                                    .pt_2()
                                    .child(self.composer.clone()),
                            )
                            .child(
                                div()
                                    .h(px(30.))
                                    .flex_none()
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Label::new("Ctrl-Enter send")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(div().flex_1())
                                    .when(turn_active, |this| {
                                        this.child(
                                            IconButton::new("stop-turn", IconName::Stop)
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Default)
                                                .icon_color(Color::Error)
                                                .style(ButtonStyle::Tinted(TintColor::Error))
                                                .aria_label("Stop turn")
                                                .on_click(
                                                    cx.listener(|this, _, _, cx| this.stop(cx)),
                                                ),
                                        )
                                    })
                                    .when(!turn_active, |this| {
                                        this.child(
                                            IconButton::new("send-turn", IconName::Send)
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Default)
                                                .style(ButtonStyle::Filled)
                                                .disabled(composer_empty)
                                                .icon_color(if composer_empty {
                                                    Color::Muted
                                                } else {
                                                    Color::Accent
                                                })
                                                .aria_label(if composer_empty {
                                                    "Type a prompt to send"
                                                } else {
                                                    "Send prompt"
                                                })
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.send(window, cx)
                                                })),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .h(px(30.))
                            .flex_none()
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_t_1()
                            .border_color(colors.border)
                            .bg(colors.status_bar_background)
                            .child(self.mode_indicator.clone())
                            .child(
                                Label::new(active_region)
                                    .size(LabelSize::XSmall)
                                    .weight(FontWeight::MEDIUM),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .child(navigation_hint),
                            )
                            .when(!compact, |this| {
                                this.child(
                                    Label::new(self.connection_label())
                                        .size(LabelSize::XSmall)
                                        .color(
                                            if self.client.is_some() || self.replay_count.is_some()
                                            {
                                                Color::Success
                                            } else {
                                                Color::Muted
                                            },
                                        ),
                                )
                            })
                            .child(
                                Button::new(
                                    "transcript-view",
                                    if self.buffer_view { "TEXT" } else { "RICH" },
                                )
                                .size(ButtonSize::Compact)
                                .style(ButtonStyle::Subtle)
                                .aria_label(if self.buffer_view {
                                    "Show rich transcript"
                                } else {
                                    "Show Vim text view"
                                })
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.toggle_buffer_view(window, cx),
                                )),
                            ),
                    ),
            )
            .when_some(command_palette, |this, command_palette| {
                this.child(deferred(
                    div()
                        .absolute()
                        .inset_0()
                        .pt(px(56.))
                        .flex()
                        .items_start()
                        .justify_center()
                        .bg(gpui::black().opacity(0.16))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.close_command_palette(window, cx)
                            }),
                        )
                        .child(
                            div()
                                .w(relative(0.64))
                                .min_w(px(420.))
                                .max_w(px(780.))
                                .child(command_palette),
                        ),
                ))
            })
    }
}

fn action_button(
    label: impl Into<SharedString>,
    tint: Option<TintColor>,
    selected: bool,
) -> Button {
    let label = label.into();
    Button::new(label.clone(), label)
        .size(ButtonSize::Default)
        .style(tint.map_or(ButtonStyle::Subtle, ButtonStyle::Tinted))
        .toggle_state(selected)
        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
}

fn request_supplement_key(item_key: &str) -> String {
    format!("request-surface:{item_key}")
}

fn route_server_request(
    method: &str,
    params: &Value,
    selected_thread_id: Option<&str>,
) -> RequestRoute {
    if !request_matches_thread(params, selected_thread_id) {
        return RequestRoute::Immediate(safe_request_rejection(method, params));
    }

    match method {
        "item/commandExecution/requestApproval" => {
            if command_approval_decisions(params).is_empty() {
                RequestRoute::Immediate(RequestReply::Error {
                    code: -32602,
                    message: "command approval contained no supported decisions".into(),
                })
            } else {
                RequestRoute::Interactive
            }
        }
        "item/fileChange/requestApproval" | "applyPatchApproval" | "execCommandApproval" => {
            RequestRoute::Interactive
        }
        "item/tool/requestUserInput" => {
            let questions_are_valid = params
                .get("questions")
                .and_then(Value::as_array)
                .is_some_and(|questions| valid_user_input_questions(questions));
            if questions_are_valid {
                RequestRoute::Interactive
            } else {
                RequestRoute::Immediate(RequestReply::Error {
                    code: -32602,
                    message: "request_user_input contained no answerable questions".into(),
                })
            }
        }
        "mcpServer/elicitation/request" => match params.get("mode").and_then(Value::as_str) {
            Some("form")
                if params
                    .pointer("/requestedSchema/type")
                    .and_then(Value::as_str)
                    == Some("object")
                    && params
                        .pointer("/requestedSchema/required")
                        .is_none_or(|required| required.is_null() || required.is_array())
                    && params
                        .pointer("/requestedSchema/properties")
                        .and_then(Value::as_object)
                        .is_some() =>
            {
                RequestRoute::Interactive
            }
            Some("openai/form") => RequestRoute::Interactive,
            Some("url")
                if params
                    .get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| {
                        url.starts_with("https://") || url.starts_with("http://")
                    }) =>
            {
                RequestRoute::Interactive
            }
            _ => RequestRoute::Immediate(RequestReply::Result(json!({
                "action": "decline",
                "content": Value::Null,
            }))),
        },
        "item/permissions/requestApproval" => {
            if params
                .get("permissions")
                .and_then(Value::as_object)
                .is_some()
            {
                RequestRoute::Interactive
            } else {
                RequestRoute::Immediate(RequestReply::Result(json!({
                    "permissions": {},
                    "scope": "turn",
                })))
            }
        }
        _ => RequestRoute::Immediate(RequestReply::Error {
            code: -32601,
            message: format!("unsupported app-server request method: {method}"),
        }),
    }
}

fn request_matches_thread(params: &Value, selected_thread_id: Option<&str>) -> bool {
    let request_thread_id = params
        .get("threadId")
        .or_else(|| params.get("conversationId"))
        .and_then(Value::as_str);
    match (selected_thread_id, request_thread_id) {
        (Some(selected), Some(request_thread_id)) => selected == request_thread_id,
        (None, Some(_)) => false,
        (_, None) => true,
    }
}

fn valid_user_input_questions(questions: &[Value]) -> bool {
    if questions.is_empty() {
        return false;
    }
    let mut ids = HashSet::new();
    questions.iter().all(|question| {
        let Some(id) = question
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return false;
        };
        if !ids.insert(id) {
            return false;
        }
        if question.get("header").and_then(Value::as_str).is_none()
            || question
                .get("question")
                .and_then(Value::as_str)
                .filter(|question| !question.is_empty())
                .is_none()
        {
            return false;
        }
        question.get("options").is_none_or(|options| {
            options.is_null()
                || options.as_array().is_some_and(|options| {
                    options.iter().all(|option| {
                        option.get("label").and_then(Value::as_str).is_some()
                            && option.get("description").and_then(Value::as_str).is_some()
                    })
                })
        })
    })
}

fn safe_request_rejection(method: &str, params: &Value) -> RequestReply {
    match method {
        "item/commandExecution/requestApproval" => {
            let decision = command_approval_decisions(params)
                .into_iter()
                .find(|decision| decision.as_str() == Some("decline"))
                .or_else(|| {
                    command_approval_decisions(params)
                        .into_iter()
                        .find(|decision| decision.as_str() == Some("cancel"))
                });
            decision.map_or_else(
                || RequestReply::Error {
                    code: -32000,
                    message: "background command approval offered no safe rejection".into(),
                },
                |decision| RequestReply::Result(json!({"decision": decision})),
            )
        }
        "item/fileChange/requestApproval" => RequestReply::Result(json!({"decision": "decline"})),
        "item/tool/requestUserInput" => RequestReply::Error {
            code: -32000,
            message: "request_user_input belongs to a task that is not open".into(),
        },
        "mcpServer/elicitation/request" => RequestReply::Result(json!({
            "action": "decline",
            "content": Value::Null,
        })),
        "item/permissions/requestApproval" => RequestReply::Result(json!({
            "permissions": {},
            "scope": "turn",
        })),
        "applyPatchApproval" | "execCommandApproval" => RequestReply::Result(json!({
            "decision": {
                "denied": {
                    "rejection": "Approval belongs to a task that is not open",
                },
            },
        })),
        _ => RequestReply::Error {
            code: -32601,
            message: format!("unsupported app-server request method: {method}"),
        },
    }
}

fn request_choices(method: &str, params: &Value) -> Vec<RequestChoice> {
    match method {
        "item/commandExecution/requestApproval" => command_approval_decisions(params)
            .into_iter()
            .filter_map(command_decision_choice)
            .collect(),
        "item/fileChange/requestApproval" => [
            ("Allow once", "accept", RequestChoiceTone::Allow),
            (
                "Allow for task",
                "acceptForSession",
                RequestChoiceTone::Allow,
            ),
            ("Deny", "decline", RequestChoiceTone::Deny),
            ("Cancel turn", "cancel", RequestChoiceTone::Cancel),
        ]
        .into_iter()
        .map(|(label, decision, tone)| RequestChoice {
            label: label.into(),
            response: json!({"decision": decision}),
            completed_status: decision.into(),
            tone,
        })
        .collect(),
        "item/permissions/requestApproval" => permission_choices(params),
        "applyPatchApproval" | "execCommandApproval" => [
            ("Allow once", json!("approved"), RequestChoiceTone::Allow),
            (
                "Allow for task",
                json!("approved_for_session"),
                RequestChoiceTone::Allow,
            ),
            (
                "Deny",
                json!({"denied": {"rejection": "Denied by user"}}),
                RequestChoiceTone::Deny,
            ),
            ("Cancel turn", json!("abort"), RequestChoiceTone::Cancel),
        ]
        .into_iter()
        .map(|(label, decision, tone)| RequestChoice {
            label: label.into(),
            response: json!({"decision": decision}),
            completed_status: label.to_lowercase(),
            tone,
        })
        .collect(),
        "mcpServer/elicitation/request" => {
            let mut choices = Vec::new();
            if params.get("mode").and_then(Value::as_str) == Some("url") {
                choices.push(RequestChoice {
                    label: "I completed this".into(),
                    response: json!({"action": "accept", "content": Value::Null}),
                    completed_status: "accepted".into(),
                    tone: RequestChoiceTone::Allow,
                });
            }
            choices.extend([
                RequestChoice {
                    label: "Decline".into(),
                    response: json!({"action": "decline", "content": Value::Null}),
                    completed_status: "declined".into(),
                    tone: RequestChoiceTone::Deny,
                },
                RequestChoice {
                    label: "Cancel".into(),
                    response: json!({"action": "cancel", "content": Value::Null}),
                    completed_status: "cancelled".into(),
                    tone: RequestChoiceTone::Cancel,
                },
            ]);
            choices
        }
        _ => Vec::new(),
    }
}

fn command_approval_decisions(params: &Value) -> Vec<Value> {
    let defaults = || {
        vec![
            json!("accept"),
            json!("acceptForSession"),
            json!("decline"),
            json!("cancel"),
        ]
    };
    match params.get("availableDecisions") {
        None | Some(Value::Null) => defaults(),
        Some(Value::Array(decisions)) => decisions
            .iter()
            .filter(|decision| is_supported_command_decision(decision))
            .cloned()
            .collect(),
        Some(_) => Vec::new(),
    }
}

fn is_supported_command_decision(decision: &Value) -> bool {
    if matches!(
        decision.as_str(),
        Some("accept" | "acceptForSession" | "decline" | "cancel")
    ) {
        return true;
    }
    let Some(decision_object) = decision.as_object().filter(|object| object.len() == 1) else {
        return false;
    };
    if let Some(amendment) = decision.pointer("/acceptWithExecpolicyAmendment/execpolicy_amendment")
    {
        return decision_object.contains_key("acceptWithExecpolicyAmendment")
            && decision
                .get("acceptWithExecpolicyAmendment")
                .and_then(Value::as_object)
                .is_some_and(|object| object.len() == 1)
            && amendment
                .as_array()
                .is_some_and(|parts| parts.iter().all(Value::is_string));
    }
    let Some(amendment) = decision.pointer("/applyNetworkPolicyAmendment/network_policy_amendment")
    else {
        return false;
    };
    decision_object.contains_key("applyNetworkPolicyAmendment")
        && decision
            .get("applyNetworkPolicyAmendment")
            .and_then(Value::as_object)
            .is_some_and(|object| object.len() == 1)
        && amendment.get("host").and_then(Value::as_str).is_some()
        && matches!(
            amendment.get("action").and_then(Value::as_str),
            Some("allow" | "deny")
        )
}

fn command_decision_choice(decision: Value) -> Option<RequestChoice> {
    let (label, status, tone) = match decision.as_str() {
        Some("accept") => (
            "Allow once".into(),
            "accepted".into(),
            RequestChoiceTone::Allow,
        ),
        Some("acceptForSession") => (
            "Allow for task".into(),
            "accepted for task".into(),
            RequestChoiceTone::Allow,
        ),
        Some("decline") => ("Deny".into(), "declined".into(), RequestChoiceTone::Deny),
        Some("cancel") => (
            "Cancel turn".into(),
            "cancelled".into(),
            RequestChoiceTone::Cancel,
        ),
        _ if decision.get("acceptWithExecpolicyAmendment").is_some() => (
            "Allow & remember command".into(),
            "policy amended".into(),
            RequestChoiceTone::Allow,
        ),
        _ if decision.get("applyNetworkPolicyAmendment").is_some() => {
            let amendment =
                decision.pointer("/applyNetworkPolicyAmendment/network_policy_amendment")?;
            let action = amendment.get("action")?.as_str()?;
            let host = amendment.get("host")?.as_str()?;
            (
                format!("{action} {host}"),
                "network policy amended".into(),
                if action == "allow" {
                    RequestChoiceTone::Allow
                } else {
                    RequestChoiceTone::Deny
                },
            )
        }
        _ => return None,
    };
    Some(RequestChoice {
        label,
        response: json!({"decision": decision}),
        completed_status: status,
        tone,
    })
}

fn permission_choices(params: &Value) -> Vec<RequestChoice> {
    let permissions = params
        .get("permissions")
        .filter(|permissions| permissions.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut choices = vec![RequestChoice {
        label: "Allow once".into(),
        response: json!({"permissions": permissions, "scope": "turn"}),
        completed_status: "accepted".into(),
        tone: RequestChoiceTone::Allow,
    }];

    let file_system = params.pointer("/permissions/fileSystem").cloned();
    let network = params.pointer("/permissions/network").cloned();
    if file_system.as_ref().is_some_and(|value| !value.is_null())
        && network.as_ref().is_some_and(|value| !value.is_null())
    {
        choices.push(RequestChoice {
            label: "Files only".into(),
            response: json!({
                "permissions": {"fileSystem": file_system},
                "scope": "turn",
            }),
            completed_status: "file access accepted".into(),
            tone: RequestChoiceTone::Allow,
        });
        choices.push(RequestChoice {
            label: "Network only".into(),
            response: json!({
                "permissions": {"network": network},
                "scope": "turn",
            }),
            completed_status: "network access accepted".into(),
            tone: RequestChoiceTone::Allow,
        });
    }

    choices.extend([
        RequestChoice {
            label: "Allow for task".into(),
            response: json!({"permissions": permissions, "scope": "session"}),
            completed_status: "accepted for task".into(),
            tone: RequestChoiceTone::Allow,
        },
        RequestChoice {
            label: "Deny".into(),
            response: json!({"permissions": {}, "scope": "turn"}),
            completed_status: "declined".into(),
            tone: RequestChoiceTone::Deny,
        },
    ]);
    choices
}

fn request_choice_visual(tone: RequestChoiceTone) -> (IconName, Color) {
    match tone {
        RequestChoiceTone::Allow => (IconName::Check, Color::Success),
        RequestChoiceTone::Deny => (IconName::Close, Color::Error),
        RequestChoiceTone::Cancel => (IconName::Close, Color::Muted),
    }
}

fn build_user_input_response(
    questions: &[Value],
    selected_answers: Option<&HashMap<String, Vec<String>>>,
    typed_answers: &HashMap<String, String>,
) -> Result<Value, String> {
    if questions.is_empty() {
        return Err("request contains no questions".into());
    }
    let mut answers = serde_json::Map::new();
    for question in questions {
        let question_id = question
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "question is missing an id".to_string())?;
        if answers.contains_key(question_id) {
            return Err(format!("question id {question_id} is duplicated"));
        }
        let mut values = selected_answers
            .and_then(|answers| answers.get(question_id))
            .cloned()
            .unwrap_or_default();
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| option.get("label").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if !options.is_empty()
            && values
                .iter()
                .any(|value| !options.iter().any(|option| *option == value.as_str()))
        {
            return Err(format!("{question_id} contains an unavailable option"));
        }
        if let Some(text) = typed_answers
            .get(question_id)
            .map(|text| text.trim())
            .filter(|text| !text.is_empty())
        {
            if !options.is_empty()
                && !question
                    .get("isOther")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            {
                return Err(format!("{question_id} does not accept a custom answer"));
            }
            values.push(text.to_string());
        }
        values.dedup();
        if values.is_empty() {
            return Err("answer every question".into());
        }
        answers.insert(question_id.into(), json!({"answers": values}));
    }
    Ok(json!({"answers": answers}))
}

fn build_mcp_form_response(
    requested_schema: &Value,
    field_text: &HashMap<String, String>,
) -> Result<Value, String> {
    if requested_schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("unsupported MCP form schema".into());
    }
    let properties = requested_schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "unsupported MCP form schema".to_string())?;
    let required = requested_schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let mut content = serde_json::Map::new();
    for (name, schema) in properties {
        let text = field_text
            .get(name)
            .map(|text| text.trim())
            .unwrap_or_default();
        let value = if text.is_empty() {
            if let Some(default) = schema.get("default").filter(|value| !value.is_null()) {
                Some(default.clone())
            } else if required.contains(name.as_str()) {
                return Err(format!("{name} is required"));
            } else {
                None
            }
        } else {
            Some(parse_mcp_form_value(schema, text).map_err(|error| format!("{name}: {error}"))?)
        };
        if let Some(value) = value {
            content.insert(name.clone(), value);
        }
    }
    Ok(json!({"action": "accept", "content": content}))
}

fn parse_mcp_form_value(schema: &Value, text: &str) -> Result<Value, String> {
    let value = match schema.get("type").and_then(Value::as_str) {
        Some("boolean") => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Value::Bool(true),
            "false" | "no" | "0" => Value::Bool(false),
            _ => return Err("enter true or false".into()),
        },
        Some("integer") => text
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "enter a whole number".to_string())?,
        Some("number") => {
            let number = text
                .parse::<f64>()
                .map_err(|_| "enter a number".to_string())?;
            serde_json::Number::from_f64(number)
                .map(Value::Number)
                .ok_or_else(|| "enter a finite number".to_string())?
        }
        Some("array") => Value::Array(
            text.split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| Value::String(part.into()))
                .collect(),
        ),
        Some("string") | None => Value::String(text.into()),
        Some(kind) => return Err(format!("unsupported field type {kind}")),
    };

    if let Value::String(value) = &value {
        let allowed = string_choices_from_schema(schema);
        if !allowed.is_empty() && !allowed.iter().any(|allowed| allowed == value) {
            return Err(format!("choose one of {}", allowed.join(", ")));
        }
        let character_count = value.chars().count() as u64;
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| character_count < minimum)
        {
            return Err("value is too short".into());
        }
        if schema
            .get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| character_count > maximum)
        {
            return Err("value is too long".into());
        }
    }
    if let Some(number) = value.as_f64() {
        if schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|minimum| number < minimum)
        {
            return Err("value is below the minimum".into());
        }
        if schema
            .get("maximum")
            .and_then(Value::as_f64)
            .is_some_and(|maximum| number > maximum)
        {
            return Err("value is above the maximum".into());
        }
    }
    if let Some(values) = value.as_array() {
        let allowed = string_choices_from_schema(schema.get("items").unwrap_or(&Value::Null));
        if !allowed.is_empty()
            && values.iter().any(|value| {
                value
                    .as_str()
                    .is_none_or(|value| !allowed.iter().any(|allowed| allowed == value))
            })
        {
            return Err(format!("choose only from {}", allowed.join(", ")));
        }
        if schema
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| values.len() < minimum as usize)
        {
            return Err("select more values".into());
        }
        if schema
            .get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| values.len() > maximum as usize)
        {
            return Err("select fewer values".into());
        }
    }
    Ok(value)
}

fn string_choices_from_schema(schema: &Value) -> Vec<String> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(values) = schema.get(key).and_then(Value::as_array) {
            return values
                .iter()
                .filter_map(|value| value.get("const").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect();
        }
    }
    Vec::new()
}

fn mcp_form_field_hint(schema: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        parts.push(match kind {
            "array" => "comma-separated values".into(),
            "boolean" => "true or false".into(),
            other => other.to_string(),
        });
    }
    let choices = if schema.get("type").and_then(Value::as_str) == Some("array") {
        string_choices_from_schema(schema.get("items").unwrap_or(&Value::Null))
    } else {
        string_choices_from_schema(schema)
    };
    if !choices.is_empty() {
        parts.push(format!("choices: {}", choices.join(", ")));
    }
    if let Some(default) = schema.get("default").filter(|value| !value.is_null()) {
        parts.push(format!("default: {default}"));
    }
    parts.join(" · ")
}

fn decision_button(
    label: impl Into<SharedString>,
    icon: IconName,
    icon_color: Color,
    selected: bool,
) -> Button {
    let label = label.into();
    Button::new(label.clone(), label)
        .size(ButtonSize::Default)
        .style(ButtonStyle::Subtle)
        .start_icon(Icon::new(icon).size(IconSize::XSmall).color(icon_color))
        .toggle_state(selected)
        .selected_style(ButtonStyle::Filled)
}

fn icon_for_kind(kind: model::TranscriptKind) -> IconName {
    match kind {
        model::TranscriptKind::User => IconName::Person,
        model::TranscriptKind::Agent => IconName::AiOpenAi,
        model::TranscriptKind::Reasoning => IconName::ToolThink,
        model::TranscriptKind::Plan => IconName::ListTodo,
        model::TranscriptKind::Command => IconName::ToolTerminal,
        model::TranscriptKind::FileChange => IconName::FileDiff,
        model::TranscriptKind::Tool => IconName::ToolHammer,
        model::TranscriptKind::Diff => IconName::Diff,
        model::TranscriptKind::Image => IconName::Image,
        model::TranscriptKind::Subagent => IconName::UserGroup,
        model::TranscriptKind::Web => IconName::ToolWeb,
        model::TranscriptKind::Review => IconName::Eye,
        model::TranscriptKind::Trace => IconName::Code,
        model::TranscriptKind::Error => IconName::Warning,
        model::TranscriptKind::Approval => IconName::Lock,
    }
}

fn thread_title(thread: &CodexThread) -> String {
    thread
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| thread.preview.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("Untitled task")
        .trim()
        .replace('\n', " ")
}

fn relative_time(timestamp: i64) -> String {
    let timestamp = if timestamp > 10_000_000_000 {
        timestamp / 1000
    } else {
        timestamp
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let seconds = now.saturating_sub(timestamp).max(0);
    match seconds {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        86_400..=604_799 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

fn compact_reasoning_preview(content: &str) -> String {
    let latest = content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Thinking")
        .trim()
        .trim_matches('*')
        .trim();
    let mut preview = latest.chars().take(140).collect::<String>();
    if latest.chars().count() > 140 {
        preview.push('…');
    }
    preview
}

fn replay_count() -> Option<usize> {
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--replay" {
            return Some(
                arguments
                    .next()
                    .and_then(|count| count.parse().ok())
                    .unwrap_or(10_000),
            );
        }
        if let Some(count) = argument.strip_prefix("--replay=") {
            return count.parse().ok();
        }
    }
    std::env::var("HARNESS_REPLAY_ITEMS")
        .ok()
        .and_then(|count| count.parse().ok())
}

fn load_harness_keymaps(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-enter", Send, Some("HarnessComposer && Editor")),
        KeyBinding::new("ctrl-c", Stop, Some("Harness && !Editor")),
        KeyBinding::new(
            "ctrl-w k",
            FocusTranscript,
            Some("Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "ctrl-w h",
            FocusTasks,
            Some("Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "escape",
            NormalEscape,
            Some("Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new("j", MoveDown, Some("HarnessTranscript || HarnessTasks")),
        KeyBinding::new("k", MoveUp, Some("HarnessTranscript || HarnessTasks")),
        KeyBinding::new("g g", GoTop, Some("HarnessTranscript || HarnessTasks")),
        KeyBinding::new(
            "shift-g",
            GoBottom,
            Some("HarnessTranscript || HarnessTasks"),
        ),
        KeyBinding::new("ctrl-u", PageUp, Some("HarnessTranscript")),
        KeyBinding::new("ctrl-d", PageDown, Some("HarnessTranscript")),
        KeyBinding::new("enter", ToggleItem, Some("HarnessTranscript")),
        KeyBinding::new("space", ToggleItem, Some("HarnessTranscript")),
        KeyBinding::new("z a", ToggleItem, Some("HarnessTranscript")),
        KeyBinding::new("r", ToggleRaw, Some("HarnessTranscript")),
        KeyBinding::new("v", ToggleVisual, Some("HarnessTranscript")),
        KeyBinding::new("y", YankItem, Some("HarnessTranscript")),
        KeyBinding::new("/", OpenSearch, Some("HarnessTranscript")),
        KeyBinding::new("shift-v", ToggleBufferView, Some("HarnessTranscript")),
        KeyBinding::new("n", NextMatch, Some("HarnessTranscript")),
        KeyBinding::new("shift-n", PreviousMatch, Some("HarnessTranscript")),
        KeyBinding::new("i", FocusComposer, Some("HarnessTranscript")),
        KeyBinding::new("a", FocusComposer, Some("HarnessTranscript")),
        KeyBinding::new("o", FocusComposer, Some("HarnessTranscript")),
        KeyBinding::new("h", FocusTasks, Some("HarnessTranscript")),
        KeyBinding::new("ctrl-w h", FocusTasks, Some("HarnessTranscript")),
        KeyBinding::new(
            "ctrl-j",
            FocusComposer,
            Some("HarnessTranscript || HarnessTasks"),
        ),
        KeyBinding::new(
            "ctrl-w j",
            FocusComposer,
            Some("HarnessTranscript || HarnessTasks"),
        ),
        KeyBinding::new(
            "ctrl-w j",
            FocusComposer,
            Some("Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "enter",
            OpenRequestSurface,
            Some(
                "HarnessBuffer && HarnessPendingRequest && Editor && VimControl && vim_mode == normal",
            ),
        ),
        KeyBinding::new("enter", OpenTask, Some("HarnessTasks")),
        KeyBinding::new("l", FocusTranscript, Some("HarnessTasks")),
        KeyBinding::new("ctrl-l", FocusTranscript, Some("HarnessTasks")),
        KeyBinding::new("ctrl-w l", FocusTranscript, Some("HarnessTasks")),
        KeyBinding::new("enter", CommitSearch, Some("HarnessSearch")),
        KeyBinding::new("escape", CloseSearch, Some("HarnessSearch")),
        KeyBinding::new(
            "escape",
            CloseSearch,
            Some("HarnessTranscript && HarnessSearchVisible"),
        ),
        KeyBinding::new("j", MoveDown, Some("HarnessRequest && !Editor")),
        KeyBinding::new("k", MoveUp, Some("HarnessRequest && !Editor")),
        KeyBinding::new("h", MoveLeft, Some("HarnessRequest && !Editor")),
        KeyBinding::new("l", MoveRight, Some("HarnessRequest && !Editor")),
        KeyBinding::new("enter", ChooseRequest, Some("HarnessRequest && !Editor")),
        KeyBinding::new("i", EditRequest, Some("HarnessRequest && !Editor")),
        KeyBinding::new("ctrl-enter", SubmitRequest, Some("HarnessRequest")),
        KeyBinding::new("escape", ReturnFromRequest, Some("HarnessRequest")),
        KeyBinding::new("h", MoveLeft, Some("HarnessApproval")),
        KeyBinding::new("l", MoveRight, Some("HarnessApproval")),
        KeyBinding::new("enter", ChooseApproval, Some("HarnessApproval")),
        KeyBinding::new("escape", ReturnFromRequest, Some("HarnessApproval")),
        KeyBinding::new("ctrl-n", NewTask, Some("Harness")),
        KeyBinding::new("ctrl-r", RefreshTasks, Some("HarnessTranscript")),
        KeyBinding::new("ctrl-b", ToggleSidebar, Some("Harness")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_interactive(method: &str, params: Value) {
        assert_eq!(
            route_server_request(method, &params, Some("thread-1")),
            RequestRoute::Interactive,
            "{method} should remain interactive"
        );
    }

    #[test]
    fn routes_all_interactive_request_methods_and_rejects_unknown_methods() {
        assert_interactive(
            "item/commandExecution/requestApproval",
            json!({
                "threadId": "thread-1",
                "availableDecisions": ["decline", "accept"],
            }),
        );
        assert_interactive(
            "item/fileChange/requestApproval",
            json!({"threadId": "thread-1"}),
        );
        assert_interactive(
            "item/tool/requestUserInput",
            json!({
                "threadId": "thread-1",
                "questions": [{
                    "id": "choice",
                    "header": "Choice",
                    "question": "Which one?",
                    "options": null,
                }],
            }),
        );
        assert_interactive(
            "mcpServer/elicitation/request",
            json!({
                "threadId": "thread-1",
                "mode": "form",
                "requestedSchema": {"type": "object", "properties": {}},
            }),
        );
        assert_interactive(
            "item/permissions/requestApproval",
            json!({"threadId": "thread-1", "permissions": {}}),
        );
        assert_interactive("applyPatchApproval", json!({"conversationId": "thread-1"}));
        assert_interactive("execCommandApproval", json!({"conversationId": "thread-1"}));

        assert_eq!(
            route_server_request("future/request", &json!({}), Some("thread-1")),
            RequestRoute::Immediate(RequestReply::Error {
                code: -32601,
                message: "unsupported app-server request method: future/request".into(),
            })
        );
    }

    #[test]
    fn cross_task_requests_receive_safe_method_specific_responses() {
        let cases = [
            (
                "item/commandExecution/requestApproval",
                json!({"threadId": "thread-2", "availableDecisions": ["accept", "decline"]}),
                RequestReply::Result(json!({"decision": "decline"})),
            ),
            (
                "item/fileChange/requestApproval",
                json!({"threadId": "thread-2"}),
                RequestReply::Result(json!({"decision": "decline"})),
            ),
            (
                "item/tool/requestUserInput",
                json!({"threadId": "thread-2"}),
                RequestReply::Error {
                    code: -32000,
                    message: "request_user_input belongs to a task that is not open".into(),
                },
            ),
            (
                "mcpServer/elicitation/request",
                json!({"threadId": "thread-2"}),
                RequestReply::Result(json!({"action": "decline", "content": null})),
            ),
            (
                "item/permissions/requestApproval",
                json!({"threadId": "thread-2"}),
                RequestReply::Result(json!({"permissions": {}, "scope": "turn"})),
            ),
            (
                "applyPatchApproval",
                json!({"conversationId": "thread-2"}),
                RequestReply::Result(json!({
                    "decision": {"denied": {"rejection": "Approval belongs to a task that is not open"}},
                })),
            ),
            (
                "execCommandApproval",
                json!({"conversationId": "thread-2"}),
                RequestReply::Result(json!({
                    "decision": {"denied": {"rejection": "Approval belongs to a task that is not open"}},
                })),
            ),
        ];

        for (method, params, expected) in cases {
            assert_eq!(
                route_server_request(method, &params, Some("thread-1")),
                RequestRoute::Immediate(expected),
                "{method} was not safely resolved"
            );
        }
        assert_eq!(
            route_server_request(
                "item/fileChange/requestApproval",
                &json!({"threadId": "thread-2"}),
                None,
            ),
            RequestRoute::Immediate(RequestReply::Result(json!({"decision": "decline"})))
        );
    }

    #[test]
    fn malformed_requests_are_resolved_without_entering_the_transcript() {
        assert!(matches!(
            route_server_request(
                "item/commandExecution/requestApproval",
                &json!({"availableDecisions": []}),
                None,
            ),
            RequestRoute::Immediate(RequestReply::Error { code: -32602, .. })
        ));
        assert!(matches!(
            route_server_request(
                "item/tool/requestUserInput",
                &json!({
                    "questions": [
                        {"id": "same", "header": "A", "question": "A?"},
                        {"id": "same", "header": "B", "question": "B?"},
                    ],
                }),
                None,
            ),
            RequestRoute::Immediate(RequestReply::Error { code: -32602, .. })
        ));
        assert_eq!(
            route_server_request(
                "mcpServer/elicitation/request",
                &json!({"mode": "form", "requestedSchema": {"properties": {}}}),
                None,
            ),
            RequestRoute::Immediate(RequestReply::Result(
                json!({"action": "decline", "content": null}),
            ))
        );
        assert_eq!(
            route_server_request(
                "mcpServer/elicitation/request",
                &json!({"mode": "url", "url": "javascript:alert(1)"}),
                None,
            ),
            RequestRoute::Immediate(RequestReply::Result(
                json!({"action": "decline", "content": null}),
            ))
        );
        assert_eq!(
            route_server_request(
                "item/permissions/requestApproval",
                &json!({"permissions": "everything"}),
                None,
            ),
            RequestRoute::Immediate(RequestReply::Result(
                json!({"permissions": {}, "scope": "turn"}),
            ))
        );
    }

    #[test]
    fn command_choices_preserve_the_server_order_and_exact_decisions() {
        let amendment = json!({
            "applyNetworkPolicyAmendment": {
                "network_policy_amendment": {"host": "example.com", "action": "allow"},
            },
        });
        let params = json!({
            "availableDecisions": ["cancel", amendment, "accept", "not-a-decision"],
        });
        let choices = request_choices("item/commandExecution/requestApproval", &params);
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[0].response, json!({"decision": "cancel"}));
        assert_eq!(choices[1].response, json!({"decision": amendment}));
        assert_eq!(choices[2].response, json!({"decision": "accept"}));
    }

    #[test]
    fn file_and_legacy_choices_match_their_response_schemas() {
        let file = request_choices("item/fileChange/requestApproval", &json!({}));
        assert_eq!(
            file.iter()
                .map(|choice| choice.response.clone())
                .collect::<Vec<_>>(),
            vec![
                json!({"decision": "accept"}),
                json!({"decision": "acceptForSession"}),
                json!({"decision": "decline"}),
                json!({"decision": "cancel"}),
            ]
        );

        for method in ["applyPatchApproval", "execCommandApproval"] {
            let legacy = request_choices(method, &json!({}));
            assert_eq!(
                legacy
                    .iter()
                    .map(|choice| choice.response.clone())
                    .collect::<Vec<_>>(),
                vec![
                    json!({"decision": "approved"}),
                    json!({"decision": "approved_for_session"}),
                    json!({"decision": {"denied": {"rejection": "Denied by user"}}}),
                    json!({"decision": "abort"}),
                ]
            );
        }
    }

    #[test]
    fn permission_choices_never_grant_more_than_the_request() {
        let file_system = json!({"read": ["/project"], "write": ["/project/out"]});
        let network = json!({"enabled": true});
        let requested = json!({"fileSystem": file_system, "network": network});
        let choices = permission_choices(&json!({"permissions": requested}));
        assert_eq!(choices.len(), 5);
        assert_eq!(
            choices[0].response,
            json!({"permissions": requested, "scope": "turn"})
        );
        assert_eq!(
            choices[1].response,
            json!({"permissions": {"fileSystem": file_system}, "scope": "turn"})
        );
        assert_eq!(
            choices[2].response,
            json!({"permissions": {"network": network}, "scope": "turn"})
        );
        assert_eq!(
            choices[3].response,
            json!({"permissions": requested, "scope": "session"})
        );
        assert_eq!(
            choices[4].response,
            json!({"permissions": {}, "scope": "turn"})
        );
    }

    #[test]
    fn user_input_response_has_the_exact_nested_answer_shape() {
        let questions = vec![
            json!({
                "id": "pick",
                "header": "Pick",
                "question": "Pick one",
                "options": [{"label": "First", "description": "The first option"}],
            }),
            json!({"id": "note", "header": "Note", "question": "Say more"}),
        ];
        let selected = HashMap::from([("pick".into(), vec!["First".into()])]);
        let typed = HashMap::from([("note".into(), "details".into())]);
        assert_eq!(
            build_user_input_response(&questions, Some(&selected), &typed),
            Ok(json!({
                "answers": {
                    "pick": {"answers": ["First"]},
                    "note": {"answers": ["details"]},
                },
            }))
        );
        assert_eq!(
            build_user_input_response(&questions, None, &HashMap::new()),
            Err("answer every question".into())
        );
    }

    #[test]
    fn mcp_form_response_parses_types_defaults_and_constraints() {
        let schema = json!({
            "type": "object",
            "properties": {
                "active": {"type": "boolean"},
                "count": {"type": "integer", "minimum": 1, "maximum": 10},
                "name": {"type": "string", "minLength": 2},
                "region": {"type": "string", "enum": ["us", "eu"], "default": "us"},
                "tags": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["active", "count", "name"],
        });
        let fields = HashMap::from([
            ("active".into(), "yes".into()),
            ("count".into(), "3".into()),
            ("name".into(), "zed".into()),
            ("tags".into(), "fast, native".into()),
        ]);
        assert_eq!(
            build_mcp_form_response(&schema, &fields),
            Ok(json!({
                "action": "accept",
                "content": {
                    "active": true,
                    "count": 3,
                    "name": "zed",
                    "region": "us",
                    "tags": ["fast", "native"],
                },
            }))
        );

        let invalid = HashMap::from([
            ("active".into(), "yes".into()),
            ("count".into(), "30".into()),
            ("name".into(), "z".into()),
        ]);
        assert_eq!(
            build_mcp_form_response(&schema, &invalid),
            Err("count: value is above the maximum".into())
        );
    }

    #[test]
    fn mcp_modes_only_offer_schema_valid_actions() {
        let form = request_choices("mcpServer/elicitation/request", &json!({"mode": "form"}));
        assert_eq!(
            form.iter()
                .map(|choice| choice.response.clone())
                .collect::<Vec<_>>(),
            vec![
                json!({"action": "decline", "content": null}),
                json!({"action": "cancel", "content": null}),
            ]
        );
        let url = request_choices("mcpServer/elicitation/request", &json!({"mode": "url"}));
        assert_eq!(
            url.iter()
                .map(|choice| choice.response.clone())
                .collect::<Vec<_>>(),
            vec![
                json!({"action": "accept", "content": null}),
                json!({"action": "decline", "content": null}),
                json!({"action": "cancel", "content": null}),
            ]
        );
    }

    #[test]
    fn request_surface_lifetime_never_revives_persisted_or_expired_requests() {
        assert_eq!(
            surface_sync_decision(false, true, false, false),
            SurfaceSyncDecision::Ignore,
            "an unresolved request loaded from persistence is not live"
        );
        assert_eq!(
            surface_sync_decision(true, true, false, false),
            SurfaceSyncDecision::Upsert
        );
        assert_eq!(
            surface_sync_decision(true, false, true, true),
            SurfaceSyncDecision::KeepResponding,
            "the entity and its drafts survive until the response result arrives"
        );
        assert_eq!(
            surface_sync_decision(true, true, false, true),
            SurfaceSyncDecision::Upsert,
            "a failed response reuses the existing entity"
        );
        assert_eq!(
            surface_sync_decision(true, false, false, true),
            SurfaceSyncDecision::Remove
        );
        assert_eq!(
            surface_sync_decision(false, false, false, true),
            SurfaceSyncDecision::Remove
        );
    }

    #[test]
    fn request_surface_response_event_preserves_exact_choice_payload() {
        let mut choices = request_choices(
            "item/permissions/requestApproval",
            &json!({"permissions": {"network": {"enabled": true}}}),
        );
        let choice = choices.remove(0);
        assert_eq!(
            RequestSurfaceRespond::from_choice("request:42", &choice),
            RequestSurfaceRespond {
                item_key: "request:42".into(),
                response: json!({
                    "permissions": {"network": {"enabled": true}},
                    "scope": "turn",
                }),
                completed_status: "accepted".into(),
            }
        );
    }
}

fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .init();
    let replay_count = replay_count();
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .into_owned();

    application().with_assets(Assets).run(move |cx| {
        cx.set_app_identity("dev.harness.app", "Harness");
        release_channel::init_test(
            semver::Version::new(0, 1, 0),
            release_channel::ReleaseChannel::Dev,
            cx,
        );
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        if let Err(error) = Assets.load_fonts(cx) {
            log::error!("failed to load fonts: {error}");
            return;
        }
        SettingsStore::update_global(cx, |store, cx| {
            _ = store.set_user_settings(r#"{"vim_mode": true}"#, cx);
        });
        if let Err(error) = harness_editor::init(cx) {
            log::error!("failed to load editor keymaps: {error}");
            return;
        }
        command_palette_hooks::init(cx);
        palette::init(cx);
        load_harness_keymaps(cx);

        let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(520.))),
                titlebar: None,
                app_id: Some("dev.harness.app".into()),
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title("Harness");
                theme_settings::setup_ui_font(window, cx);
                cx.new(|cx| HarnessApp::new(cwd, replay_count, window, cx))
            },
        ) {
            log::error!("failed to open Harness window: {error}");
            return;
        }
        cx.activate(true);
    });
}
