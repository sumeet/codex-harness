use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::Path,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use agent::ThreadStore;
use agent_client_protocol::schema::v1 as acp;
use chrono::Utc;
use codex_app_server_client::{Client, CodexThread, Event};
use editor::EditorMode;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, FollowMode,
    IntoElement, KeyContext, ListAlignment, ListOffset, ListState, Render, SharedString,
    Subscription, Task, UniformListScrollHandle, WeakEntity, Window, list, prelude::*, px,
    uniform_list,
};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use parking_lot::RwLock;
use project::{AgentId, Project};
use serde_json::{Value, json};
use ui::{
    Button, ButtonSize, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip,
    WithScrollbar, prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    FocusCodexComposer, FocusCodexTranscript, ScrollOutputLineDown, ScrollOutputLineUp,
    ScrollOutputPageDown, ScrollOutputPageUp, ScrollOutputToBottom, ScrollOutputToTop,
    ToggleCodexSidebar, ToggleCodexTranscriptItem, YankCodexTranscriptItem,
    message_editor::{MessageEditor, MessageEditorEvent, SessionCapabilities},
};

const THREAD_LIMIT: usize = 300;
const HISTORY_WIDTH: f32 = 252.;
const STREAM_FRAME: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptKind {
    User,
    Agent,
    Reasoning,
    Plan,
    Command,
    FileChange,
    Tool,
    Diff,
    Image,
    Subagent,
    Web,
    Review,
    Turn,
    Trace,
    Error,
    Approval,
}

impl TranscriptKind {
    fn icon(self) -> IconName {
        match self {
            Self::User => IconName::Person,
            Self::Agent => IconName::AiOpenAi,
            Self::Reasoning => IconName::ThinkingMode,
            Self::Plan => IconName::ListTodo,
            Self::Command => IconName::ToolTerminal,
            Self::FileChange => IconName::FileDiff,
            Self::Tool => IconName::ToolHammer,
            Self::Diff => IconName::Diff,
            Self::Image => IconName::Image,
            Self::Subagent => IconName::UserGroup,
            Self::Web => IconName::ToolWeb,
            Self::Review => IconName::Eye,
            Self::Turn => IconName::Indicator,
            Self::Trace => IconName::Code,
            Self::Error => IconName::Warning,
            Self::Approval => IconName::Lock,
        }
    }

    fn is_markdown(self) -> bool {
        matches!(
            self,
            Self::User | Self::Agent | Self::Reasoning | Self::Plan | Self::Image
        )
    }

    fn is_structured(self) -> bool {
        matches!(
            self,
            Self::Command
                | Self::FileChange
                | Self::Tool
                | Self::Diff
                | Self::Subagent
                | Self::Web
                | Self::Review
                | Self::Trace
                | Self::Error
                | Self::Approval
        )
    }
}

#[derive(Clone, Debug)]
struct PendingRequest {
    id: Value,
    method: String,
    resolved: bool,
}

struct TranscriptItem {
    key: String,
    protocol_id: Option<String>,
    kind: TranscriptKind,
    title: SharedString,
    status: Option<SharedString>,
    content: String,
    raw: Value,
    event_count: usize,
    expanded: bool,
    markdown: Option<Entity<Markdown>>,
    pending_request: Option<PendingRequest>,
}

impl TranscriptItem {
    fn source(&self) -> SharedString {
        if self.kind.is_markdown() {
            return self.content.clone().into();
        }

        let language = if self.kind == TranscriptKind::Diff {
            "diff"
        } else if matches!(self.kind, TranscriptKind::Command) {
            "text"
        } else {
            "json"
        };
        let content = self.content.replace("```", "`\u{200b}`\u{200b}`");
        format!("```{language}\n{content}\n```").into()
    }
}

#[derive(Clone, Debug)]
struct RawEvent {
    method: String,
    payload: Value,
}

pub struct CodexHarness {
    language_registry: Option<Arc<LanguageRegistry>>,
    cwd: String,
    client: Option<Rc<Client>>,
    threads: Vec<CodexThread>,
    selected_thread_id: Option<String>,
    selected_title: SharedString,
    current_turn_id: Option<String>,
    items: Vec<TranscriptItem>,
    item_indices: HashMap<String, usize>,
    raw_events: Vec<RawEvent>,
    list_state: ListState,
    history_scroll: UniformListScrollHandle,
    selected_item: Option<usize>,
    sidebar_open: bool,
    transcript_mode: bool,
    connecting: bool,
    loading_thread: bool,
    error: Option<SharedString>,
    message_editor: Entity<MessageEditor>,
    focus_handle: FocusHandle,
    local_message_id: usize,
    _server_task: Task<()>,
    _request_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl CodexHarness {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: WeakEntity<Project>,
        thread_store: Entity<ThreadStore>,
        cwd: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project
            .upgrade()
            .map(|project| project.read(cx).languages().clone());
        let capabilities = Arc::new(RwLock::new(SessionCapabilities::new(
            acp::PromptCapabilities::new()
                .image(true)
                .embedded_context(true),
            Vec::new(),
            Vec::new(),
        )));
        let message_editor = cx.new(|cx| {
            MessageEditor::new(
                workspace.clone(),
                project,
                Some(thread_store),
                capabilities,
                AgentId::new("codex-app-server"),
                "Ask Codex…",
                EditorMode::AutoHeight {
                    min_lines: 3,
                    max_lines: Some(12),
                },
                window,
                cx,
            )
        });
        let list_state = ListState::new(0, ListAlignment::Top, px(2048.));
        list_state.set_follow_mode(FollowMode::Tail);

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &message_editor,
            window,
            |this, _, event, window, cx| match event {
                MessageEditorEvent::Send | MessageEditorEvent::SendImmediately => {
                    this.send_prompt(window, cx)
                }
                MessageEditorEvent::Cancel => this.focus_transcript(window, cx),
                _ => {}
            },
        ));

        let mut this = Self {
            language_registry,
            cwd,
            client: None,
            threads: Vec::new(),
            selected_thread_id: None,
            selected_title: "New task".into(),
            current_turn_id: None,
            items: Vec::new(),
            item_indices: HashMap::default(),
            raw_events: Vec::new(),
            list_state,
            history_scroll: UniformListScrollHandle::new(),
            selected_item: None,
            sidebar_open: true,
            transcript_mode: false,
            connecting: false,
            loading_thread: false,
            error: None,
            message_editor,
            focus_handle: cx.focus_handle(),
            local_message_id: 0,
            _server_task: Task::ready(()),
            _request_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.connect(cx);
        this
    }

    pub fn new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_thread_id = None;
        self.selected_title = "New task".into();
        self.current_turn_id = None;
        self.error = None;
        self.replace_items(Vec::new(), cx);
        self.message_editor.focus_handle(cx).focus(window, cx);
        self.transcript_mode = false;
        cx.notify();
    }

    pub(crate) fn composer_focus_handle(&self, cx: &App) -> FocusHandle {
        self.message_editor.focus_handle(cx)
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.connecting = true;
        self.error = None;
        self._server_task = cx.spawn(async move |this, cx| {
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
                    this.update(cx, |this, cx| {
                        this.client = Some(client.clone());
                        this.threads = threads;
                        this.connecting = false;
                        this.error = None;
                        cx.notify();
                    })
                    .ok();
                    client
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.connecting = false;
                        this.error = Some(format!("Could not connect to Codex: {error}").into());
                        cx.notify();
                    })
                    .ok();
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

    fn refresh_threads(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            self.connect(cx);
            return;
        };
        self.connecting = true;
        self._request_task = cx.spawn(async move |this, cx| {
            let result = client.list_threads(THREAD_LIMIT, None).await;
            this.update(cx, |this, cx| {
                this.connecting = false;
                match result {
                    Ok(response) => {
                        this.threads = response.data;
                        this.error = None;
                    }
                    Err(error) => this.error = Some(format!("Refresh failed: {error}").into()),
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn open_thread(&mut self, index: usize, cx: &mut Context<Self>) {
        let (thread_id, title) = match self.threads.get(index) {
            Some(thread) => (thread.id.clone(), thread_title(thread)),
            None => return,
        };
        let Some(client) = self.client.clone() else {
            return;
        };

        self.selected_thread_id = Some(thread_id.clone());
        self.selected_title = title;
        self.loading_thread = true;
        self.error = None;
        self.replace_items(Vec::new(), cx);
        self._request_task = cx.spawn(async move |this, cx| {
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

            this.update(cx, |this, cx| {
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
            .log_err();
        });
    }

    fn load_thread(&mut self, thread: CodexThread, cx: &mut Context<Self>) {
        self.selected_thread_id = Some(thread.id.clone());
        self.selected_title = thread_title(&thread);
        self.cwd = if thread.cwd.is_empty() {
            self.cwd.clone()
        } else {
            thread.cwd.clone()
        };
        let mut items = Vec::new();
        for turn in thread.turns {
            let turn_status = compact_json(&turn.status);
            items.push(self.make_item(
                format!("turn:{}", turn.id),
                None,
                TranscriptKind::Turn,
                "Turn".into(),
                Some(turn_status.into()),
                String::new(),
                json!({"id": turn.id, "status": turn.status}),
                false,
                None,
                cx,
            ));
            for protocol_item in turn.items {
                let mut raw = protocol_item.body.clone();
                raw.insert("id".into(), Value::String(protocol_item.id));
                raw.insert("type".into(), Value::String(protocol_item.kind));
                items.push(self.item_from_protocol(Value::Object(raw), true, cx));
            }
        }
        self.replace_items(items, cx);
        self.list_state.scroll_to_end();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocks = self
            .message_editor
            .read(cx)
            .draft_content_blocks_snapshot(cx);
        let input = app_server_inputs(blocks);
        if input.is_empty() {
            return;
        }
        let visible_text = prompt_preview(&input);
        self.message_editor.update(cx, |editor, cx| {
            editor.clear(window, cx);
        });

        self.local_message_id += 1;
        let key = format!("local-user:{}", self.local_message_id);
        let item = self.make_item(
            key.clone(),
            None,
            TranscriptKind::User,
            "You".into(),
            Some("sending".into()),
            visible_text,
            Value::Array(input.clone()),
            true,
            None,
            cx,
        );
        self.push_item(item);
        self.list_state.scroll_to_end();

        let Some(client) = self.client.clone() else {
            self.error = Some("Codex is not connected yet".into());
            cx.notify();
            return;
        };
        let existing_thread_id = self.selected_thread_id.clone();
        let cwd = self.cwd.clone();
        self._request_task = cx.spawn(async move |this, cx| {
            let result = async {
                let thread_id = match existing_thread_id {
                    Some(thread_id) => thread_id,
                    None => client.start_thread(&cwd).await?.id,
                };
                let response = client.start_turn(&thread_id, Value::Array(input)).await?;
                anyhow::Ok((thread_id, response))
            }
            .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok((thread_id, response)) => {
                        this.selected_thread_id = Some(thread_id);
                        this.current_turn_id = response
                            .pointer("/turn/id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        if let Some(ix) = this.item_indices.get(&key).copied() {
                            this.items[ix].status = Some("sent".into());
                            this.list_state.splice(ix..ix + 1, 1);
                        }
                        this.error = None;
                    }
                    Err(error) => {
                        this.error = Some(format!("Could not send: {error}").into());
                        if let Some(ix) = this.item_indices.get(&key).copied() {
                            this.items[ix].status = Some("failed".into());
                            this.list_state.splice(ix..ix + 1, 1);
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(thread_id), Some(turn_id)) = (
            self.client.clone(),
            self.selected_thread_id.clone(),
            self.current_turn_id.clone(),
        ) else {
            return;
        };
        self._request_task = cx.spawn(async move |this, cx| {
            let result = client.interrupt_turn(&thread_id, &turn_id).await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not stop turn: {error}").into());
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    fn apply_event_batch(&mut self, events: Vec<Event>, cx: &mut Context<Self>) {
        let old_len = self.items.len();
        let mut dirty = HashSet::default();
        for event in events {
            self.apply_event(event, &mut dirty, cx);
        }

        for index in &dirty {
            self.refresh_markdown(*index, cx);
        }
        if self.items.len() > old_len {
            self.list_state
                .splice(old_len..old_len, self.items.len() - old_len);
        }
        let changed_existing = dirty
            .into_iter()
            .filter(|index| *index < old_len)
            .collect::<HashSet<_>>();
        for index in changed_existing {
            self.list_state.splice(index..index + 1, 1);
        }
        cx.notify();
    }

    fn apply_event(&mut self, event: Event, dirty: &mut HashSet<usize>, cx: &mut Context<Self>) {
        match event {
            Event::Notification { method, params } => {
                self.raw_events.push(RawEvent {
                    method: method.clone(),
                    payload: params.clone(),
                });
                if !self.event_matches_selected_thread(&params) {
                    return;
                }
                match method.as_str() {
                    "item/started" | "item/completed" => {
                        if let Some(item) = params.get("item").cloned() {
                            let index =
                                self.upsert_protocol_item(item, method == "item/completed", cx);
                            dirty.insert(index);
                        }
                    }
                    "item/agentMessage/delta"
                    | "item/plan/delta"
                    | "item/reasoning/textDelta"
                    | "item/reasoning/summaryTextDelta"
                    | "item/commandExecution/outputDelta"
                    | "item/fileChange/outputDelta" => {
                        let fallback_kind = match method.as_str() {
                            "item/agentMessage/delta" => TranscriptKind::Agent,
                            "item/plan/delta" => TranscriptKind::Plan,
                            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                                TranscriptKind::Reasoning
                            }
                            "item/commandExecution/outputDelta" => TranscriptKind::Command,
                            _ => TranscriptKind::FileChange,
                        };
                        let index = self.append_delta(&method, &params, fallback_kind, cx);
                        dirty.insert(index);
                    }
                    "turn/diff/updated" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let index = self.upsert_generated(
                            format!("turn-diff:{turn_id}"),
                            TranscriptKind::Diff,
                            "Working tree diff",
                            string_at(&params, "/diff").unwrap_or_default().to_string(),
                            params,
                            cx,
                        );
                        dirty.insert(index);
                    }
                    "turn/plan/updated" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let content = render_plan(&params);
                        let index = self.upsert_generated(
                            format!("turn-plan:{turn_id}"),
                            TranscriptKind::Plan,
                            "Plan",
                            content,
                            params,
                            cx,
                        );
                        dirty.insert(index);
                    }
                    "turn/started" | "turn/completed" => {
                        let turn = params.get("turn").cloned().unwrap_or(Value::Null);
                        let turn_id = string_at(&turn, "/id").unwrap_or("unknown");
                        self.current_turn_id = if method == "turn/started" {
                            Some(turn_id.to_string())
                        } else {
                            None
                        };
                        let status = turn
                            .get("status")
                            .map(compact_json)
                            .unwrap_or_else(|| method.replace("turn/", ""));
                        let index = self.upsert_generated(
                            format!("turn:{turn_id}"),
                            TranscriptKind::Turn,
                            "Turn",
                            String::new(),
                            turn,
                            cx,
                        );
                        self.items[index].status = Some(status.into());
                        dirty.insert(index);
                        if method == "turn/completed" {
                            self.refresh_threads(cx);
                        }
                    }
                    "item/fileChange/patchUpdated" => {
                        let item_id = string_at(&params, "/itemId").unwrap_or("patch");
                        let content =
                            render_file_changes(params.get("changes").unwrap_or(&Value::Null));
                        let index = self.upsert_generated(
                            item_id.to_string(),
                            TranscriptKind::FileChange,
                            "File changes",
                            content,
                            params,
                            cx,
                        );
                        dirty.insert(index);
                    }
                    "item/mcpToolCall/progress" => {
                        let item_id = string_at(&params, "/itemId").unwrap_or("mcp");
                        let index = self.upsert_generated(
                            item_id.to_string(),
                            TranscriptKind::Tool,
                            "MCP tool",
                            string_at(&params, "/message")
                                .unwrap_or_default()
                                .to_string(),
                            params,
                            cx,
                        );
                        dirty.insert(index);
                    }
                    "thread/name/updated" => {
                        if let Some(name) = string_at(&params, "/name") {
                            self.selected_title = name.to_string().into();
                        }
                    }
                    "error" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let index = self.upsert_generated(
                            format!("error:{turn_id}"),
                            TranscriptKind::Error,
                            "Codex error",
                            pretty_json(&params),
                            params,
                            cx,
                        );
                        dirty.insert(index);
                    }
                    _ => {
                        let key = trace_key(&method, &params);
                        let index = self.upsert_generated(
                            key,
                            TranscriptKind::Trace,
                            method.clone(),
                            pretty_json(&params),
                            params,
                            cx,
                        );
                        dirty.insert(index);
                    }
                }
            }
            Event::ServerRequest { id, method, params } => {
                self.raw_events.push(RawEvent {
                    method: method.clone(),
                    payload: params.clone(),
                });
                let key = format!("request:{}", compact_json(&id));
                let index = self.upsert_generated(
                    key,
                    TranscriptKind::Approval,
                    friendly_method(&method),
                    pretty_json(&params),
                    params,
                    cx,
                );
                self.items[index].pending_request = Some(PendingRequest {
                    id,
                    method,
                    resolved: false,
                });
                dirty.insert(index);
            }
            Event::UnmatchedResponse { id, result, error } => {
                let payload =
                    json!({"id": id, "result": result, "error": error.map(|error| error.message)});
                self.raw_events.push(RawEvent {
                    method: "unmatchedResponse".into(),
                    payload: payload.clone(),
                });
                let index = self.upsert_generated(
                    format!("unmatched-response:{}", self.raw_events.len()),
                    TranscriptKind::Trace,
                    "Unmatched response",
                    pretty_json(&payload),
                    payload,
                    cx,
                );
                dirty.insert(index);
            }
            Event::Disconnected { reason } => {
                self.error = Some(reason.clone().into());
                let payload = json!({"reason": reason});
                self.raw_events.push(RawEvent {
                    method: "disconnected".into(),
                    payload: payload.clone(),
                });
                let index = self.upsert_generated(
                    "app-server-disconnected".into(),
                    TranscriptKind::Error,
                    "App Server disconnected",
                    pretty_json(&payload),
                    payload,
                    cx,
                );
                dirty.insert(index);
            }
        }
    }

    fn event_matches_selected_thread(&self, params: &Value) -> bool {
        match (
            self.selected_thread_id.as_deref(),
            string_at(params, "/threadId"),
        ) {
            (Some(selected), Some(event_thread)) => selected == event_thread,
            _ => true,
        }
    }

    fn append_delta(
        &mut self,
        method: &str,
        params: &Value,
        kind: TranscriptKind,
        cx: &mut Context<Self>,
    ) -> usize {
        let item_id = string_at(params, "/itemId").unwrap_or("streaming-item");
        let index = if let Some(index) = self.item_indices.get(item_id).copied() {
            index
        } else {
            let title = title_for_kind(kind);
            let item = self.make_item(
                item_id.to_string(),
                Some(item_id.to_string()),
                kind,
                title.into(),
                Some("streaming".into()),
                String::new(),
                params.clone(),
                true,
                None,
                cx,
            );
            self.push_item_without_splice(item)
        };
        let delta = string_at(params, "/delta").unwrap_or_default();
        if method == "item/reasoning/summaryTextDelta"
            && !self.items[index].content.is_empty()
            && !self.items[index].content.ends_with('\n')
        {
            self.items[index].content.push('\n');
        }
        self.items[index].content.push_str(delta);
        self.items[index].raw = params.clone();
        self.items[index].event_count += 1;
        index
    }

    fn upsert_protocol_item(
        &mut self,
        value: Value,
        completed: bool,
        cx: &mut Context<Self>,
    ) -> usize {
        let protocol_id = string_at(&value, "/id")
            .unwrap_or("unknown-item")
            .to_string();
        if let Some(index) = self.item_indices.get(&protocol_id).copied() {
            let old_events = self.items[index].event_count;
            let expanded = self.items[index].expanded;
            let pending_request = self.items[index].pending_request.clone();
            let mut item = self.item_from_protocol(value, completed, cx);
            item.event_count = old_events + 1;
            item.expanded = expanded;
            item.pending_request = pending_request;
            self.items[index] = item;
            return index;
        }

        let item = self.item_from_protocol(value, completed, cx);
        if item.kind == TranscriptKind::User
            && let Some(index) = self.items.iter().rposition(|candidate| {
                candidate.key.starts_with("local-user:") && candidate.content == item.content
            })
        {
            self.item_indices.remove(&self.items[index].key);
            self.item_indices.insert(protocol_id, index);
            self.items[index] = item;
            return index;
        }
        self.push_item_without_splice(item)
    }

    fn item_from_protocol(
        &self,
        raw: Value,
        completed: bool,
        cx: &mut Context<Self>,
    ) -> TranscriptItem {
        let protocol_id = string_at(&raw, "/id").unwrap_or("unknown-item").to_string();
        let protocol_kind = string_at(&raw, "/type").unwrap_or("unknown");
        let kind = kind_from_protocol(protocol_kind);
        let title = title_from_protocol(protocol_kind, &raw);
        let content = content_from_protocol(protocol_kind, &raw);
        let status = raw
            .get("status")
            .map(compact_json)
            .or_else(|| completed.then(|| "completed".into()))
            .or_else(|| Some("running".into()));
        self.make_item(
            protocol_id.clone(),
            Some(protocol_id),
            kind,
            title.into(),
            status.map(Into::into),
            content,
            raw,
            true,
            None,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn make_item(
        &self,
        key: String,
        protocol_id: Option<String>,
        kind: TranscriptKind,
        title: SharedString,
        status: Option<SharedString>,
        content: String,
        raw: Value,
        expanded: bool,
        pending_request: Option<PendingRequest>,
        cx: &mut Context<Self>,
    ) -> TranscriptItem {
        let mut item = TranscriptItem {
            key,
            protocol_id,
            kind,
            title,
            status,
            content,
            raw,
            event_count: 1,
            expanded,
            markdown: None,
            pending_request,
        };
        item.markdown = Some(
            cx.new(|cx| Markdown::new(item.source(), self.language_registry.clone(), None, cx)),
        );
        item
    }

    fn upsert_generated(
        &mut self,
        key: String,
        kind: TranscriptKind,
        title: impl Into<SharedString>,
        content: String,
        raw: Value,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(index) = self.item_indices.get(&key).copied() {
            let item = &mut self.items[index];
            item.kind = kind;
            item.title = title.into();
            item.content = content;
            item.raw = raw;
            item.event_count += 1;
            return index;
        }
        let item = self.make_item(
            key,
            None,
            kind,
            title.into(),
            None,
            content,
            raw,
            true,
            None,
            cx,
        );
        self.push_item_without_splice(item)
    }

    fn push_item_without_splice(&mut self, item: TranscriptItem) -> usize {
        let index = self.items.len();
        self.item_indices.insert(item.key.clone(), index);
        self.items.push(item);
        index
    }

    fn push_item(&mut self, item: TranscriptItem) -> usize {
        let index = self.push_item_without_splice(item);
        self.list_state.splice(index..index, 1);
        index
    }

    fn replace_items(&mut self, items: Vec<TranscriptItem>, cx: &mut Context<Self>) {
        let old_len = self.items.len();
        self.items = items;
        self.item_indices.clear();
        for (index, item) in self.items.iter().enumerate() {
            self.item_indices.insert(item.key.clone(), index);
        }
        self.selected_item = (!self.items.is_empty()).then(|| self.items.len() - 1);
        self.list_state.splice(0..old_len, self.items.len());
        cx.notify();
    }

    fn refresh_markdown(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        let source = item.source();
        if let Some(markdown) = item.markdown.clone() {
            markdown.update(cx, |markdown, cx| markdown.reset(source, cx));
        }
    }

    fn focus_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.transcript_mode = true;
        if self.selected_item.is_none() && !self.items.is_empty() {
            self.selected_item = Some(self.items.len() - 1);
        }
        self.focus_handle.focus(window, cx);
        if let Some(index) = self.selected_item {
            self.list_state.scroll_to_reveal_item(index);
        }
        cx.notify();
    }

    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.transcript_mode = false;
        self.message_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(next) = selection_after_delta(self.items.len(), self.selected_item, delta) else {
            return;
        };
        self.selected_item = Some(next);
        self.list_state.scroll_to_reveal_item(next);
        cx.notify();
    }

    fn toggle_selected(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.selected_item else {
            return;
        };
        self.items[index].expanded = !self.items[index].expanded;
        self.list_state.splice(index..index + 1, 1);
        cx.notify();
    }

    fn yank_selected(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.selected_item else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(self.items[index].content.clone()));
    }

    fn respond_to_request(&mut self, index: usize, decision: &'static str, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(request) = self
            .items
            .get(index)
            .and_then(|item| item.pending_request.clone())
        else {
            return;
        };
        if request.resolved {
            return;
        }

        if let Some(item) = self.items.get_mut(index) {
            item.status = Some("responding".into());
        }
        self.list_state.splice(index..index + 1, 1);
        self._request_task = cx.spawn(async move |this, cx| {
            let result = client
                .respond(request.id, json!({"decision": decision}))
                .await;
            this.update(cx, |this, cx| {
                if let Some(item) = this.items.get_mut(index) {
                    match result {
                        Ok(()) => {
                            item.status = Some(decision.into());
                            if let Some(request) = &mut item.pending_request {
                                request.resolved = true;
                            }
                        }
                        Err(error) => {
                            item.status = Some("response failed".into());
                            this.error = Some(format!("Approval response failed: {error}").into());
                        }
                    }
                }
                this.list_state.splice(index..index + 1, 1);
                cx.notify();
            })
            .log_err();
        });
    }

    fn key_context(&self) -> KeyContext {
        let mut context = KeyContext::new_with_defaults();
        context.add("CodexHarness");
        if self.transcript_mode {
            context.add("CodexTranscript");
        }
        context
    }

    fn render_history_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|index| {
                let thread = self.threads.get(index)?;
                let selected = self.selected_thread_id.as_deref() == Some(thread.id.as_str());
                let title = thread_title(thread);
                let meta = format!(
                    "{}  ·  {}",
                    cwd_label(thread).unwrap_or_else(|| "Codex".into()),
                    relative_time(thread.updated_at)
                );
                Some(
                    h_flex()
                        .id(("codex-direct-thread", index))
                        .group("codex-direct-thread")
                        .h(px(52.))
                        .w_full()
                        .px_2()
                        .gap_2()
                        .cursor_pointer()
                        .border_l_2()
                        .border_color(if selected {
                            cx.theme().colors().text_accent
                        } else {
                            gpui::transparent_black()
                        })
                        .when(selected, |this| {
                            this.bg(cx.theme().colors().element_selected)
                        })
                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_thread(index, cx)))
                        .child(Icon::new(IconName::Thread).size(IconSize::Small).color(
                            if selected {
                                Color::Accent
                            } else {
                                Color::Muted
                            },
                        ))
                        .child(
                            v_flex()
                                .min_w_0()
                                .flex_1()
                                .gap_0p5()
                                .child(Label::new(title).size(LabelSize::Small).truncate())
                                .child(
                                    Label::new(meta)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.sidebar_open {
            return div().into_any_element();
        }
        let scroll = self.history_scroll.clone();
        let body = if self.threads.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new(if self.connecting {
                        "Connecting…"
                    } else {
                        "No tasks"
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_y_hidden()
                .child(
                    uniform_list(
                        "codex-direct-history",
                        self.threads.len(),
                        cx.processor(Self::render_history_rows),
                    )
                    .flex_1()
                    .track_scroll(&scroll),
                )
                .vertical_scrollbar_for(&scroll, window, cx)
                .into_any_element()
        };

        v_flex()
            .h_full()
            .w(px(HISTORY_WIDTH))
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .h(px(44.))
                    .flex_none()
                    .px_2()
                    .justify_between()
                    .child(Label::new("Tasks").size(LabelSize::Small))
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                IconButton::new("refresh-direct-codex", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh tasks"))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.refresh_threads(cx)),
                                    ),
                            )
                            .child(
                                IconButton::new("new-direct-codex", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("New task"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_thread(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_transcript_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.items.get(index) else {
            return div().into_any_element();
        };
        let selected = self.transcript_mode && self.selected_item == Some(index);
        let kind = item.kind;
        let expanded = item.expanded;
        let markdown = item.markdown.clone();
        let title = item.title.clone();
        let status = item.status.clone();
        let event_count = item.event_count;
        let protocol_id = item.protocol_id.clone();
        let pending = item.pending_request.clone();
        let is_user = kind == TranscriptKind::User;
        let structured = kind.is_structured();

        let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        style.code_block_overflow_x_scroll = true;
        let body = markdown.map(|markdown| {
            div()
                .w_full()
                .min_w_0()
                .child(MarkdownElement::new(markdown, style).image_resolver({
                    let cwd = self.cwd.clone();
                    move |url, _cx| crate::resolve_agent_image(url, &[cwd.clone().into()])
                }))
                .into_any_element()
        });

        let header = h_flex()
            .w_full()
            .gap_2()
            .child(
                Icon::new(kind.icon())
                    .size(IconSize::Small)
                    .color(if selected {
                        Color::Accent
                    } else {
                        Color::Muted
                    }),
            )
            .child(Label::new(title).size(LabelSize::Small))
            .when_some(status, |this, status| {
                this.child(
                    Label::new(status)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(event_count > 1, |this| {
                this.child(
                    Label::new(format!("{event_count} events"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(selected, |this| {
                this.when_some(protocol_id, |this, protocol_id| {
                    this.child(
                        Label::new(protocol_id)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                })
            })
            .child(div().flex_1())
            .when(structured, |this| {
                this.child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
            });

        let request_controls = pending.and_then(|request| {
            (!request.resolved
                && matches!(
                    request.method.as_str(),
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                ))
            .then(|| {
                h_flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new(format!("allow-once-{index}"), "Allow once")
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond_to_request(index, "accept", cx)
                            })),
                    )
                    .child(
                        Button::new(format!("allow-session-{index}"), "Allow for task")
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond_to_request(index, "acceptForSession", cx)
                            })),
                    )
                    .child(
                        Button::new(format!("decline-{index}"), "Deny")
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond_to_request(index, "decline", cx)
                            })),
                    )
                    .into_any_element()
            })
        });

        let card = v_flex()
            .min_w_0()
            .when(is_user, |this| {
                this.max_w(gpui::relative(0.82))
                    .rounded_lg()
                    .bg(cx.theme().colors().element_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .p_3()
            })
            .when(!is_user && structured, |this| {
                this.rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .p_3()
            })
            .when(!is_user && !structured, |this| this.py_1())
            .child(header)
            .when(expanded, |this| {
                this.when_some(body, |this, body| this.pt_2().child(body))
                    .when_some(request_controls, |this, controls| this.child(controls))
            });

        h_flex()
            .id(("codex-transcript-item", index))
            .w_full()
            .min_w_0()
            .px_5()
            .py_2()
            .when(is_user, |this| this.justify_end())
            .border_l_2()
            .border_color(if selected {
                cx.theme().colors().text_accent
            } else {
                gpui::transparent_black()
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_item = Some(index);
                this.transcript_mode = true;
                this.focus_handle.focus(window, cx);
                cx.notify();
            }))
            .child(card)
            .into_any_element()
    }

    fn render_empty(&self, _cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::AiOpenAi)
                    .size(IconSize::Medium)
                    .color(Color::Muted),
            )
            .child(Label::new("What should we build?").size(LabelSize::Large))
            .child(
                Label::new(
                    "The transcript shows every App Server item, tool, diff, image, and event.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when(self.loading_thread, |this| {
                this.child(
                    Label::new("Loading task…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let list_state = self.list_state.clone();
        let transcript = if self.items.is_empty() {
            self.render_empty(cx)
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    list(
                        list_state.clone(),
                        cx.processor(|this, index, window, cx| {
                            this.render_transcript_item(index, window, cx)
                        }),
                    )
                    .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                    .flex_1(),
                )
                .vertical_scrollbar_for(&list_state, window, cx)
                .into_any_element()
        };
        let connected = self.client.is_some() && self.error.is_none();
        let raw_bytes = self
            .raw_events
            .iter()
            .map(|event| event.payload.to_string().len())
            .sum::<usize>();
        let latest_event = self
            .raw_events
            .last()
            .map(|event| event.method.as_str())
            .unwrap_or("ready");

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .h(px(44.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        IconButton::new(
                            "toggle-codex-sidebar",
                            if self.sidebar_open {
                                IconName::ThreadsSidebarLeftOpen
                            } else {
                                IconName::ThreadsSidebarLeftClosed
                            },
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Toggle task history"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_open = !this.sidebar_open;
                            cx.notify();
                        })),
                    )
                    .child(
                        Label::new(self.selected_title.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format!(
                            "{} events · {} KB · {}",
                            self.raw_events.len(),
                            raw_bytes / 1024,
                            latest_event
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(if self.transcript_mode {
                            "NORMAL"
                        } else {
                            "INSERT"
                        })
                        .size(LabelSize::XSmall)
                        .color(if self.transcript_mode {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                    )
                    .child(div().size(px(6.)).rounded_full().bg(if connected {
                        cx.theme().colors().text_accent
                    } else {
                        cx.theme().colors().text_muted
                    })),
            )
            .child(transcript)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .flex_none()
                        .px_4()
                        .py_1()
                        .gap_2()
                        .bg(cx.theme().colors().editor_background)
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
            })
            .child(
                v_flex()
                    .flex_none()
                    .mx_4()
                    .mb_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(if self.transcript_mode {
                        cx.theme().colors().border_variant
                    } else {
                        cx.theme().colors().text_accent.opacity(0.45)
                    })
                    .bg(cx.theme().colors().editor_background)
                    .shadow_sm()
                    .child(
                        div()
                            .min_h(px(76.))
                            .max_h(px(260.))
                            .p_3()
                            .child(self.message_editor.clone()),
                    )
                    .child(
                        h_flex()
                            .h(px(34.))
                            .px_3()
                            .gap_2()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Label::new("Ctrl+Enter send  ·  Ctrl+W K transcript")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .when(self.current_turn_id.is_some(), |this| {
                                this.child(
                                    Button::new("stop-direct-turn", "Stop")
                                        .size(ButtonSize::Compact)
                                        .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx))),
                                )
                            })
                            .child(
                                Button::new("send-direct-turn", "Send")
                                    .size(ButtonSize::Compact)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_prompt(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for CodexHarness {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CodexHarness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context(self.key_context())
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &FocusCodexTranscript, window, cx| {
                this.focus_transcript(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusCodexComposer, window, cx| {
                this.focus_composer(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollOutputLineUp, _, cx| {
                if this.transcript_mode {
                    this.move_selection(-1, cx);
                } else {
                    this.list_state.scroll_by(px(-72.));
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ScrollOutputLineDown, _, cx| {
                if this.transcript_mode {
                    this.move_selection(1, cx);
                } else {
                    this.list_state.scroll_by(px(72.));
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ScrollOutputPageUp, _, cx| {
                let height = this.list_state.viewport_bounds().size.height;
                this.list_state.scroll_by(-height * 0.9);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollOutputPageDown, _, cx| {
                let height = this.list_state.viewport_bounds().size.height;
                this.list_state.scroll_by(height * 0.9);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollOutputToTop, _, cx| {
                this.selected_item = (!this.items.is_empty()).then_some(0);
                this.list_state.scroll_to(ListOffset::default());
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollOutputToBottom, _, cx| {
                this.selected_item = this.items.len().checked_sub(1);
                this.list_state.scroll_to_end();
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &ToggleCodexTranscriptItem, _, cx| this.toggle_selected(cx)),
            )
            .on_action(
                cx.listener(|this, _: &YankCodexTranscriptItem, _, cx| this.yank_selected(cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleCodexSidebar, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .child(self.render_sidebar(window, cx))
            .child(self.render_main(window, cx))
    }
}

fn app_server_inputs(blocks: Vec<acp::ContentBlock>) -> Vec<Value> {
    blocks
        .into_iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(json!({"type": "text", "text": text.text})),
            acp::ContentBlock::Image(image) => {
                if let Some(uri) = image.uri
                    && let Some(path) = uri.strip_prefix("file://")
                {
                    Some(json!({"type": "localImage", "path": path}))
                } else {
                    Some(json!({
                        "type": "image",
                        "url": format!("data:{};base64,{}", image.mime_type, image.data)
                    }))
                }
            }
            acp::ContentBlock::Audio(audio) => Some(json!({
                "type": "audio",
                "url": format!("data:{};base64,{}", audio.mime_type, audio.data)
            })),
            acp::ContentBlock::ResourceLink(resource) => {
                let path = resource.uri.strip_prefix("file://").unwrap_or(&resource.uri);
                Some(json!({"type": "mention", "name": resource.name, "path": path}))
            }
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(resource) => Some(json!({
                    "type": "text",
                    "text": format!("<context uri=\"{}\">\n{}\n</context>", resource.uri, resource.text)
                })),
                acp::EmbeddedResourceResource::BlobResourceContents(resource) => Some(json!({
                    "type": "text",
                    "text": format!("Attached binary resource: {}", resource.uri)
                })),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn prompt_preview(input: &[Value]) -> String {
    input
        .iter()
        .filter_map(|input| match string_at(input, "/type") {
            Some("text") => string_at(input, "/text").map(ToOwned::to_owned),
            Some("localImage") => {
                string_at(input, "/path").map(|path| format!("![Attached image]({path})"))
            }
            Some("image") => Some("[Attached image]".into()),
            Some("localAudio") | Some("audio") => Some("[Attached audio]".into()),
            Some("mention") | Some("skill") => {
                string_at(input, "/path").map(|path| format!("`{path}`"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn kind_from_protocol(kind: &str) -> TranscriptKind {
    match kind {
        "userMessage" | "hookPrompt" => TranscriptKind::User,
        "agentMessage" => TranscriptKind::Agent,
        "reasoning" => TranscriptKind::Reasoning,
        "plan" => TranscriptKind::Plan,
        "commandExecution" | "sleep" => TranscriptKind::Command,
        "fileChange" => TranscriptKind::FileChange,
        "mcpToolCall" | "dynamicToolCall" => TranscriptKind::Tool,
        "collabAgentToolCall" | "subAgentActivity" => TranscriptKind::Subagent,
        "webSearch" => TranscriptKind::Web,
        "imageView" | "imageGeneration" => TranscriptKind::Image,
        "enteredReviewMode" | "exitedReviewMode" => TranscriptKind::Review,
        "contextCompaction" => TranscriptKind::Trace,
        _ => TranscriptKind::Trace,
    }
}

fn title_for_kind(kind: TranscriptKind) -> &'static str {
    match kind {
        TranscriptKind::User => "You",
        TranscriptKind::Agent => "Codex",
        TranscriptKind::Reasoning => "Reasoning",
        TranscriptKind::Plan => "Plan",
        TranscriptKind::Command => "Command",
        TranscriptKind::FileChange => "File changes",
        TranscriptKind::Tool => "Tool call",
        TranscriptKind::Diff => "Diff",
        TranscriptKind::Image => "Image",
        TranscriptKind::Subagent => "Subagent",
        TranscriptKind::Web => "Web search",
        TranscriptKind::Review => "Review",
        TranscriptKind::Turn => "Turn",
        TranscriptKind::Trace => "Protocol event",
        TranscriptKind::Error => "Error",
        TranscriptKind::Approval => "Approval",
    }
}

fn title_from_protocol(kind: &str, raw: &Value) -> String {
    match kind {
        "userMessage" | "hookPrompt" => "You".into(),
        "agentMessage" => "Codex".into(),
        "reasoning" => "Reasoning".into(),
        "plan" => "Plan".into(),
        "commandExecution" => string_at(raw, "/command")
            .map(|command| format!("Command · {}", one_line(command, 96)))
            .unwrap_or_else(|| "Command".into()),
        "fileChange" => "File changes".into(),
        "mcpToolCall" => format!(
            "MCP · {} / {}",
            string_at(raw, "/server").unwrap_or("server"),
            string_at(raw, "/tool").unwrap_or("tool")
        ),
        "dynamicToolCall" => format!(
            "Tool · {}",
            string_at(raw, "/tool").unwrap_or("dynamic tool")
        ),
        "collabAgentToolCall" => format!(
            "Subagents · {}",
            string_at(raw, "/tool").unwrap_or("activity")
        ),
        "subAgentActivity" => "Subagent activity".into(),
        "webSearch" => format!(
            "Web search · {}",
            string_at(raw, "/query").unwrap_or("search")
        ),
        "imageView" => "Viewed image".into(),
        "imageGeneration" => "Generated image".into(),
        "sleep" => "Wait".into(),
        "enteredReviewMode" => "Entered review".into(),
        "exitedReviewMode" => "Exited review".into(),
        "contextCompaction" => "Context compacted".into(),
        other => friendly_method(other),
    }
}

fn content_from_protocol(kind: &str, raw: &Value) -> String {
    match kind {
        "userMessage" => render_user_content(raw.get("content").unwrap_or(&Value::Null)),
        "hookPrompt" => pretty_json(raw.get("fragments").unwrap_or(&Value::Null)),
        "agentMessage" | "plan" => string_at(raw, "/text").unwrap_or_default().to_string(),
        "reasoning" => {
            let summary = string_array(raw.get("summary"));
            let content = string_array(raw.get("content"));
            [summary, content]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        "commandExecution" => {
            let command = string_at(raw, "/command").unwrap_or_default();
            let output = string_at(raw, "/aggregatedOutput").unwrap_or_default();
            if output.is_empty() {
                format!("$ {command}")
            } else {
                format!("$ {command}\n\n{output}")
            }
        }
        "fileChange" => render_file_changes(raw.get("changes").unwrap_or(&Value::Null)),
        "imageView" => string_at(raw, "/path")
            .map(|path| format!("![Viewed image]({path})\n\n`{path}`"))
            .unwrap_or_else(|| pretty_json(raw)),
        "imageGeneration" => {
            let path = string_at(raw, "/savedPath");
            let prompt = string_at(raw, "/revisedPrompt").unwrap_or_default();
            match path {
                Some(path) => format!("![Generated image]({path})\n\n{prompt}\n\n`{path}`"),
                None => pretty_json(raw),
            }
        }
        "webSearch" => pretty_json(raw),
        _ => pretty_json(raw),
    }
}

fn render_user_content(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match string_at(block, "/type") {
            Some("text") => string_at(block, "/text").map(ToOwned::to_owned),
            Some("localImage") => string_at(block, "/path")
                .map(|path| format!("![Attached image]({path})\n\n`{path}`")),
            Some("image") => {
                string_at(block, "/url").map(|url| format!("![Attached image]({url})"))
            }
            Some("localAudio") | Some("audio") => Some("[Attached audio]".into()),
            Some("mention") | Some("skill") => {
                string_at(block, "/path").map(|path| format!("`{path}`"))
            }
            _ => Some(pretty_json(block)),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_file_changes(changes: &Value) -> String {
    let Some(changes) = changes.as_array() else {
        return pretty_json(changes);
    };
    changes
        .iter()
        .map(|change| {
            let path = string_at(change, "/path").unwrap_or("unknown");
            let kind = change.get("kind").map(compact_json).unwrap_or_default();
            let diff = string_at(change, "/diff").unwrap_or_default();
            format!("{kind} {path}\n{diff}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_plan(params: &Value) -> String {
    let mut output = string_at(params, "/explanation")
        .filter(|text| !text.is_empty())
        .map(|text| format!("{text}\n\n"))
        .unwrap_or_default();
    if let Some(steps) = params.get("plan").and_then(Value::as_array) {
        for step in steps {
            let status = string_at(step, "/status").unwrap_or("pending");
            let text = string_at(step, "/step").unwrap_or_default();
            let marker = if status == "completed" { "x" } else { " " };
            output.push_str(&format!("- [{marker}] {text} _{status}_\n"));
        }
    }
    output
}

fn trace_key(method: &str, params: &Value) -> String {
    let identity = string_at(params, "/itemId")
        .or_else(|| string_at(params, "/turnId"))
        .or_else(|| string_at(params, "/threadId"))
        .unwrap_or("global");
    format!("trace:{method}:{identity}")
}

fn string_array(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn string_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn compact_json(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

fn one_line(value: &str, limit: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= limit {
        value
    } else {
        value.chars().take(limit).collect::<String>() + "…"
    }
}

fn friendly_method(method: &str) -> String {
    method
        .split(['/', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn thread_title(thread: &CodexThread) -> SharedString {
    thread
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| thread.preview.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("Untitled task")
        .trim()
        .replace('\n', " ")
        .into()
}

fn cwd_label(thread: &CodexThread) -> Option<SharedString> {
    Path::new(&thread.cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned().into())
}

fn relative_time(timestamp: i64) -> SharedString {
    let timestamp = if timestamp > 10_000_000_000 {
        timestamp / 1000
    } else {
        timestamp
    };
    let seconds = Utc::now().timestamp().saturating_sub(timestamp).max(0);
    match seconds {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m", seconds / 60).into(),
        3_600..=86_399 => format!("{}h", seconds / 3_600).into(),
        86_400..=604_799 => format!("{}d", seconds / 86_400).into(),
        _ => format!("{}w", seconds / 604_800).into(),
    }
}

fn selection_after_delta(
    item_count: usize,
    selected_item: Option<usize>,
    delta: isize,
) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    let current = selected_item.unwrap_or(item_count - 1).min(item_count - 1);
    Some(
        current
            .saturating_add_signed(delta)
            .min(item_count.saturating_sub(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_kinds_cover_every_app_server_thread_item() {
        let cases = [
            ("userMessage", TranscriptKind::User),
            ("hookPrompt", TranscriptKind::User),
            ("agentMessage", TranscriptKind::Agent),
            ("reasoning", TranscriptKind::Reasoning),
            ("plan", TranscriptKind::Plan),
            ("commandExecution", TranscriptKind::Command),
            ("sleep", TranscriptKind::Command),
            ("fileChange", TranscriptKind::FileChange),
            ("mcpToolCall", TranscriptKind::Tool),
            ("dynamicToolCall", TranscriptKind::Tool),
            ("collabAgentToolCall", TranscriptKind::Subagent),
            ("subAgentActivity", TranscriptKind::Subagent),
            ("webSearch", TranscriptKind::Web),
            ("imageView", TranscriptKind::Image),
            ("imageGeneration", TranscriptKind::Image),
            ("enteredReviewMode", TranscriptKind::Review),
            ("exitedReviewMode", TranscriptKind::Review),
            ("contextCompaction", TranscriptKind::Trace),
        ];

        for (protocol_kind, expected) in cases {
            assert_eq!(
                kind_from_protocol(protocol_kind),
                expected,
                "{protocol_kind}"
            );
        }
    }

    #[test]
    fn transcript_selection_clamps_like_vim_navigation() {
        assert_eq!(selection_after_delta(0, None, 1), None);
        assert_eq!(selection_after_delta(5, None, -1), Some(3));
        assert_eq!(selection_after_delta(5, Some(0), -100), Some(0));
        assert_eq!(selection_after_delta(5, Some(2), 1), Some(3));
        assert_eq!(selection_after_delta(5, Some(4), 100), Some(4));
        assert_eq!(selection_after_delta(5, Some(99), -1), Some(3));
    }

    #[test]
    fn rich_protocol_items_keep_their_visible_payloads() {
        let user = json!([
            {"type": "text", "text": "inspect this"},
            {"type": "localImage", "path": "/tmp/frame.png"},
            {"type": "mention", "path": "/tmp/lib.rs"}
        ]);
        let rendered_user = render_user_content(&user);
        assert!(rendered_user.contains("inspect this"));
        assert!(rendered_user.contains("![Attached image](/tmp/frame.png)"));
        assert!(rendered_user.contains("`/tmp/lib.rs`"));

        let command = json!({
            "command": "cargo test",
            "aggregatedOutput": "test result: ok"
        });
        assert_eq!(
            content_from_protocol("commandExecution", &command),
            "$ cargo test\n\ntest result: ok"
        );

        let changes = json!([{
            "path": "src/main.rs",
            "kind": "update",
            "diff": "@@ -1 +1 @@\n-old\n+new"
        }]);
        let rendered_changes = render_file_changes(&changes);
        assert!(rendered_changes.contains("src/main.rs"));
        assert!(rendered_changes.contains("+new"));

        let image = json!({"path": "/tmp/viewed.png"});
        assert!(
            content_from_protocol("imageView", &image)
                .starts_with("![Viewed image](/tmp/viewed.png)")
        );
    }

    #[test]
    fn plan_and_unknown_events_remain_maximally_visible() {
        let plan = json!({
            "explanation": "Ship the fast path",
            "plan": [
                {"step": "Parse events", "status": "completed"},
                {"step": "Render rows", "status": "in_progress"}
            ]
        });
        let rendered = render_plan(&plan);
        assert!(rendered.contains("- [x] Parse events _completed_"));
        assert!(rendered.contains("- [ ] Render rows _in_progress_"));

        let raw = json!({"threadId": "thread-1", "anything": {"new": true}});
        assert_eq!(
            trace_key("future/event", &raw),
            "trace:future/event:thread-1"
        );
        assert!(pretty_json(&raw).contains("\"anything\""));
    }
}
