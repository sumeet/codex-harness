use std::{ops::Range, path::Path, rc::Rc};

use chrono::Utc;
use codex_app_server_client::{Client, CodexThread};
use gpui::{
    AnyElement, Context, EventEmitter, IntoElement, Render, SharedString, Task,
    UniformListScrollHandle, Window, prelude::*, px, uniform_list,
};
use ui::{
    Button, ButtonSize, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip,
    WithScrollbar, prelude::*,
};
use util::ResultExt as _;

const THREAD_LIMIT: usize = 200;
const HISTORY_WIDTH: f32 = 272.;

#[derive(Clone, Debug)]
pub enum CodexHistoryEvent {
    NewThread,
    OpenThread {
        session_id: String,
        title: SharedString,
        cwd: String,
    },
}

pub struct CodexHistory {
    client: Option<Rc<Client>>,
    threads: Vec<CodexThread>,
    selected_thread_id: Option<String>,
    list: UniformListScrollHandle,
    loading: bool,
    error: Option<SharedString>,
    _list_task: Task<()>,
}

impl CodexHistory {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            client: None,
            threads: Vec::new(),
            selected_thread_id: None,
            list: UniformListScrollHandle::new(),
            loading: false,
            error: None,
            _list_task: Task::ready(()),
        };
        this.refresh(cx);
        this
    }

    pub fn select_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        self.selected_thread_id = session_id;
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        let existing_client = self.client.clone();

        self._list_task = cx.spawn(async move |this, cx| {
            let result = async move {
                let client = match existing_client {
                    Some(client) => client,
                    None => {
                        let client = Rc::new(Client::launch("codex")?);
                        client
                            .initialize("harness", "Harness", env!("CARGO_PKG_VERSION"))
                            .await?;
                        client
                    }
                };
                let response = client.list_threads(THREAD_LIMIT, None).await?;
                anyhow::Ok((client, response.data))
            }
            .await;

            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok((client, threads)) => {
                        this.client = Some(client);
                        this.threads = threads;
                        this.error = None;
                    }
                    Err(error) => {
                        this.error = Some(format!("Could not connect to Codex: {error}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn open_thread(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.get(index) else {
            return;
        };
        let session_id = thread.id.clone();
        let title = thread_title(thread);
        let cwd = thread.cwd.clone();
        self.selected_thread_id = Some(session_id.clone());
        cx.emit(CodexHistoryEvent::OpenThread {
            session_id,
            title,
            cwd,
        });
        cx.notify();
    }

    fn new_thread(&mut self, cx: &mut Context<Self>) {
        self.selected_thread_id = None;
        cx.emit(CodexHistoryEvent::NewThread);
        cx.notify();
    }

    fn render_threads(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|index| {
                let thread = self.threads.get(index)?;
                let is_selected = self.selected_thread_id.as_deref() == Some(thread.id.as_str());
                let title = thread_title(thread);
                let cwd = cwd_label(thread).unwrap_or_else(|| "Codex".into());
                let age = relative_time(thread.updated_at);

                Some(
                    h_flex()
                        .id(("codex-history-thread", index))
                        .group("codex-history-thread")
                        .relative()
                        .h(px(64.))
                        .w_full()
                        .px_2()
                        .gap_2()
                        .cursor_pointer()
                        .border_l_2()
                        .border_color(if is_selected {
                            cx.theme().colors().text_accent
                        } else {
                            gpui::transparent_black()
                        })
                        .when(is_selected, |this| {
                            this.bg(cx.theme().colors().element_selected)
                        })
                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_thread(index, cx);
                        }))
                        .child(div().flex_none().child(
                            Icon::new(IconName::Thread).size(IconSize::Small).color(
                                if is_selected {
                                    Color::Accent
                                } else {
                                    Color::Muted
                                },
                            ),
                        ))
                        .child(
                            v_flex()
                                .min_w_0()
                                .flex_1()
                                .gap_0p5()
                                .child(Label::new(title).size(LabelSize::Small).truncate())
                                .child(
                                    h_flex()
                                        .min_w_0()
                                        .gap_1()
                                        .child(
                                            Label::new(cwd)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .truncate(),
                                        )
                                        .child(
                                            Label::new("·")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Label::new(age)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                ),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.loading {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::Clock)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("Loading threads…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        if let Some(error) = self.error.clone() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .p_4()
                .child(Label::new(error).size(LabelSize::Small).color(Color::Muted))
                .child(
                    Button::new("retry-codex-history", "Retry")
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .into_any_element();
        }

        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::Thread)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("No Codex threads")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}

impl EventEmitter<CodexHistoryEvent> for CodexHistory {}

impl Render for CodexHistory {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.list.clone();
        let body = if self.threads.is_empty() || self.error.is_some() {
            self.render_empty_state(cx)
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_y_hidden()
                .child(
                    uniform_list(
                        "codex-history-threads",
                        self.threads.len(),
                        cx.processor(Self::render_threads),
                    )
                    .flex_1()
                    .track_scroll(&list),
                )
                .vertical_scrollbar_for(&list, window, cx)
                .into_any_element()
        };

        v_flex()
            .h_full()
            .w(px(HISTORY_WIDTH))
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .h(px(48.))
                    .flex_none()
                    .px_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(Label::new("Threads").size(LabelSize::Small))
                            .child(
                                Label::new(format!("{} recent", self.threads.len()))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                IconButton::new("refresh-codex-history", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh threads"))
                                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                            )
                            .child(
                                IconButton::new("new-codex-thread", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("New Codex thread"))
                                    .on_click(cx.listener(|this, _, _, cx| this.new_thread(cx))),
                            ),
                    ),
            )
            .child(body)
            .child(
                h_flex()
                    .h(px(28.))
                    .flex_none()
                    .px_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(cx.theme().colors().text_accent),
                    )
                    .child(
                        Label::new("Codex connected")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }
}

fn thread_title(thread: &CodexThread) -> SharedString {
    thread
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| thread.preview.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("Untitled thread")
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
