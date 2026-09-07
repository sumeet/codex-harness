use std::collections::HashMap;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, SharedString, StyledText, Window, div, prelude::*, px, relative,
};
use harness_editor::{LocalEditor, LocalEditorChanged};
use serde_json::Value;
use ui::{
    Clickable, Color, Disableable, Icon, IconName, IconSize, Label, LabelCommon, LabelSize,
    ListItem, ListItemSpacing, TintColor, Toggleable,
    prelude::{ActiveTheme, StyledTypography},
};

use super::{
    RequestChoice, action_button, build_mcp_form_response, build_user_input_response,
    decision_button, mcp_form_field_hint, request_choice_visual, request_choices, shell_highlights,
};

gpui::actions!(
    harness_prompt,
    [
        ConfirmPrompt,
        CancelPrompt,
        NextPromptAction,
        PreviousPromptAction
    ]
);

pub(crate) fn init_prompts(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("enter", ConfirmPrompt, Some("HarnessPrompt")),
        gpui::KeyBinding::new("escape", CancelPrompt, Some("HarnessPrompt")),
        gpui::KeyBinding::new("tab", NextPromptAction, Some("HarnessPrompt")),
        gpui::KeyBinding::new("down", NextPromptAction, Some("HarnessPrompt")),
        gpui::KeyBinding::new("shift-tab", PreviousPromptAction, Some("HarnessPrompt")),
        gpui::KeyBinding::new("up", PreviousPromptAction, Some("HarnessPrompt")),
    ]);
    cx.set_prompt_builder(|_, message, detail, actions, handle, window, cx| {
        let view = cx.new(|cx| ConfirmationPrompt {
            message: message.to_owned().into(),
            detail: detail.map(|text| SharedString::from(text.to_owned())),
            actions: actions.to_vec(),
            selected: actions
                .iter()
                .position(gpui::PromptButton::is_cancel)
                .unwrap_or(0),
            focus: cx.focus_handle(),
        });
        handle.with_view(view, window, cx)
    });
}

struct ConfirmationPrompt {
    message: SharedString,
    detail: Option<SharedString>,
    actions: Vec<gpui::PromptButton>,
    selected: usize,
    focus: FocusHandle,
}

impl EventEmitter<gpui::PromptResponse> for ConfirmationPrompt {}

impl Focusable for ConfirmationPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ConfirmationPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = (window.viewport_size().width - px(32.)).min(px(480.));
        div()
            .size_full()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.35))
            .child(
                div()
                    .key_context("HarnessPrompt")
                    .whitespace_normal()
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &ConfirmPrompt, _, cx| {
                        if this.actions.get(this.selected).is_some() {
                            cx.emit(gpui::PromptResponse(this.selected));
                        }
                    }))
                    .on_action(cx.listener(|this, _: &CancelPrompt, _, cx| {
                        if let Some(index) =
                            this.actions.iter().position(gpui::PromptButton::is_cancel)
                        {
                            cx.emit(gpui::PromptResponse(index));
                        }
                    }))
                    .on_action(cx.listener(|this, _: &NextPromptAction, _, cx| {
                        if !this.actions.is_empty() {
                            this.selected = (this.selected + 1) % this.actions.len();
                            cx.notify();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &PreviousPromptAction, _, cx| {
                        this.selected = this
                            .selected
                            .checked_sub(1)
                            .unwrap_or_else(|| this.actions.len().saturating_sub(1));
                        cx.notify();
                    }))
                    .child(
                        ui::AlertModal::new("confirmation-prompt")
                            .width(width)
                            .title(self.message.clone())
                            .children(self.detail.clone())
                            .footer(div().flex().flex_col().p_3().gap_1().children(
                                self.actions.iter().enumerate().map(|(index, action)| {
                                    use ui::{ButtonCommon as _, FixedWidth as _};
                                    ui::Button::new(index, action.label().clone())
                                        .full_width()
                                        .style(ui::ButtonStyle::Outlined)
                                        .when(index == self.selected, |button| {
                                            button.style(ui::ButtonStyle::Tinted(TintColor::Accent))
                                        })
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(gpui::PromptResponse(index));
                                        }))
                                }),
                            )),
                    ),
            )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Respond {
    pub item_key: String,
    pub response: Value,
    pub completed_status: String,
}

impl Respond {
    pub(crate) fn from_choice(item_key: &str, choice: &RequestChoice) -> Self {
        Self {
            item_key: item_key.to_string(),
            response: choice.response.clone(),
            completed_status: choice.completed_status.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReturnToTranscript {
    pub item_key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Approval,
    UserInput,
    McpForm,
    McpUrl,
    McpUnsupported,
}

pub(crate) struct RequestSurface {
    item_key: String,
    method: String,
    raw: Value,
    kind: SurfaceKind,
    focus_handle: FocusHandle,
    editors: HashMap<String, Entity<LocalEditor>>,
    selected_answers: HashMap<String, Vec<String>>,
    question_cursor: usize,
    option_cursors: HashMap<String, usize>,
    choice_cursor: usize,
    validation_error: Option<SharedString>,
    responding: bool,
}

impl EventEmitter<Respond> for RequestSurface {}
impl EventEmitter<ReturnToTranscript> for RequestSurface {}

impl RequestSurface {
    pub(crate) fn new(
        item_key: String,
        method: String,
        raw: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            item_key,
            kind: surface_kind(&method, &raw),
            method,
            raw,
            focus_handle: cx.focus_handle(),
            editors: HashMap::new(),
            selected_answers: HashMap::new(),
            question_cursor: 0,
            option_cursors: HashMap::new(),
            choice_cursor: 0,
            validation_error: None,
            responding: false,
        };
        this.ensure_editors(window, cx);
        this
    }

    pub(crate) fn update_request(
        &mut self,
        method: String,
        raw: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.method == "claude/questions" && self.raw["native"] != raw["native"] {
            self.selected_answers.clear();
            self.editors.clear();
            self.option_cursors.clear();
            self.validation_error =
                Some("Claude changed this request. Review the new questions.".into());
        }
        self.kind = surface_kind(&method, &raw);
        self.method = method;
        self.raw = raw;
        self.ensure_editors(window, cx);
        self.clamp_cursors();
        cx.notify();
    }

    pub(crate) fn is_approval(&self) -> bool {
        self.kind == SurfaceKind::Approval
    }

    pub(crate) fn is_responding(&self) -> bool {
        self.responding
    }

    pub(crate) fn set_responding(&mut self, responding: bool, cx: &mut Context<Self>) {
        if self.responding != responding {
            self.responding = responding;
            cx.notify();
        }
    }

    pub(crate) fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
            || self
                .editors
                .values()
                .any(|editor| editor.focus_handle(cx).contains_focused(window, cx))
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
    }

    pub(crate) fn move_vertical(&mut self, delta: isize, cx: &mut Context<Self>) {
        match self.kind {
            SurfaceKind::UserInput => {
                let count = self.questions().len();
                if count > 0 {
                    self.question_cursor = self
                        .question_cursor
                        .saturating_add_signed(delta)
                        .min(count - 1);
                }
            }
            SurfaceKind::McpForm => {
                let count = self.mcp_field_names().len();
                if count > 0 {
                    self.question_cursor = self
                        .question_cursor
                        .saturating_add_signed(delta)
                        .min(count - 1);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    pub(crate) fn move_horizontal(&mut self, delta: isize, cx: &mut Context<Self>) {
        match self.kind {
            SurfaceKind::UserInput => {
                let questions = self.questions();
                let Some(question) = questions.get(self.question_cursor) else {
                    return;
                };
                let question_id = question_id(question);
                let option_count = question
                    .get("options")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                if option_count > 0 {
                    let cursor = self.option_cursors.entry(question_id).or_insert(0);
                    *cursor = cursor.saturating_add_signed(delta).min(option_count - 1);
                }
            }
            SurfaceKind::Approval
            | SurfaceKind::McpForm
            | SurfaceKind::McpUrl
            | SurfaceKind::McpUnsupported => {
                let choice_count = request_choices(&self.method, &self.raw).len()
                    + usize::from(self.kind == SurfaceKind::McpForm);
                if choice_count > 0 {
                    self.choice_cursor = self
                        .choice_cursor
                        .saturating_add_signed(delta)
                        .min(choice_count - 1);
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.responding {
            return;
        }
        match self.kind {
            SurfaceKind::UserInput => {
                let questions = self.questions();
                let Some(question) = questions.get(self.question_cursor) else {
                    return;
                };
                let question_id = question_id(question);
                let option_index = self.option_cursors.get(&question_id).copied().unwrap_or(0);
                let answer = question
                    .get("options")
                    .and_then(Value::as_array)
                    .and_then(|options| options.get(option_index))
                    .and_then(|option| option.get("label"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                if let Some(answer) = answer {
                    self.select_answer(
                        &question_id,
                        answer,
                        question["multiSelect"] == true,
                        window,
                        cx,
                    );
                }
            }
            SurfaceKind::Approval | SurfaceKind::McpUrl | SurfaceKind::McpUnsupported => {
                let choices = request_choices(&self.method, &self.raw);
                if let Some(choice) =
                    choices.get(self.choice_cursor.min(choices.len().saturating_sub(1)))
                {
                    cx.emit(Respond::from_choice(&self.item_key, choice));
                }
            }
            SurfaceKind::McpForm if self.choice_cursor == 0 => self.submit(cx),
            SurfaceKind::McpForm => {
                let choices = request_choices(&self.method, &self.raw);
                if let Some(choice) = choices.get(self.choice_cursor - 1) {
                    cx.emit(Respond::from_choice(&self.item_key, choice));
                }
            }
        }
    }

    fn select_answer(
        &mut self,
        identifier: &str,
        answer: String,
        multiple: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.responding {
            return;
        }
        if self.method == "claude/questions"
            && !multiple
            && let Some(editor) = self.editors.get(identifier)
        {
            editor.update(cx, |editor, cx| editor.set_text("", window, cx));
        }
        toggle_answer(
            self.selected_answers
                .entry(identifier.to_owned())
                .or_default(),
            answer,
            multiple,
        );
        self.validation_error = None;
        cx.notify();
    }

    pub(crate) fn edit_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor_key = match self.kind {
            SurfaceKind::UserInput => {
                let questions = self.questions();
                questions.get(self.question_cursor).map(question_id)
            }
            SurfaceKind::McpForm => self.mcp_field_names().get(self.question_cursor).cloned(),
            SurfaceKind::McpUrl => {
                if let Some(url) = self.raw.get("url").and_then(Value::as_str) {
                    cx.open_url(url);
                }
                None
            }
            _ => None,
        };
        if let Some(editor) = editor_key.and_then(|key| self.editors.get(&key)) {
            editor.focus_handle(cx).focus(window, cx);
        }
    }

    pub(crate) fn submit(&mut self, cx: &mut Context<Self>) {
        if self.responding {
            return;
        }
        let response = match self.kind {
            SurfaceKind::UserInput => self.build_user_input_response(cx),
            SurfaceKind::McpForm => self.build_mcp_form_response(cx),
            _ => return,
        };
        match response {
            Ok((response, completed_status)) => {
                self.validation_error = None;
                cx.emit(Respond {
                    item_key: self.item_key.clone(),
                    response,
                    completed_status,
                });
            }
            Err(error) => {
                self.validation_error = Some(error.into());
                cx.notify();
            }
        }
    }

    pub(crate) fn return_to_transcript(&self, cx: &mut Context<Self>) {
        cx.emit(ReturnToTranscript {
            item_key: self.item_key.clone(),
        });
    }

    fn build_user_input_response(&self, cx: &App) -> Result<(Value, String), String> {
        let questions = self.questions();
        let typed = questions
            .iter()
            .filter_map(|question| {
                let id = question_id(question);
                let text = self.editors.get(&id)?.read(cx).text(cx).trim().to_string();
                (!text.is_empty()).then_some((id, text))
            })
            .collect::<HashMap<_, _>>();
        build_user_input_response(&questions, Some(&self.selected_answers), &typed).map(
            |mut response| {
                if self.method == "claude/questions" {
                    response["nativeDialog"] = self.raw["native"].clone();
                }
                (response, "answered".into())
            },
        )
    }

    fn build_mcp_form_response(&self, cx: &App) -> Result<(Value, String), String> {
        let fields = self
            .mcp_field_names()
            .into_iter()
            .filter_map(|name| {
                let text = self
                    .editors
                    .get(&name)?
                    .read(cx)
                    .text(cx)
                    .trim()
                    .to_string();
                (!text.is_empty()).then_some((name, text))
            })
            .collect::<HashMap<_, _>>();
        build_mcp_form_response(
            self.raw.pointer("/requestedSchema").unwrap_or(&Value::Null),
            &fields,
        )
        .map(|response| (response, "submitted".into()))
    }

    fn ensure_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = match self.kind {
            SurfaceKind::UserInput => self
                .questions()
                .into_iter()
                .filter(|question| {
                    question
                        .get("options")
                        .and_then(Value::as_array)
                        .is_none_or(Vec::is_empty)
                        || question
                            .get("isOther")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                })
                .map(|question| {
                    (
                        question_id(&question),
                        question
                            .get("isSecret")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                })
                .collect::<Vec<_>>(),
            SurfaceKind::McpForm => self
                .mcp_field_names()
                .into_iter()
                .map(|name| (name, false))
                .collect(),
            _ => Vec::new(),
        };
        for (key, secret) in fields {
            if !self.editors.contains_key(&key) {
                let editor =
                    cx.new(|cx| LocalEditor::plain_single_line("Type an answer…", window, cx));
                editor.update(cx, |editor, cx| editor.set_masked(secret, cx));
                let identifier = key.clone();
                cx.subscribe(&editor, move |this, editor, _: &LocalEditorChanged, cx| {
                    if this.method == "claude/questions"
                        && !editor.read(cx).text(cx).trim().is_empty()
                        && this.questions().iter().any(|question| {
                            question_id(question) == identifier && question["multiSelect"] != true
                        })
                    {
                        this.selected_answers.remove(&identifier);
                    }
                    this.validation_error = None;
                    cx.notify();
                })
                .detach();
                self.editors.insert(key, editor);
            }
        }
    }

    fn clamp_cursors(&mut self) {
        let count = match self.kind {
            SurfaceKind::UserInput => self.questions().len(),
            SurfaceKind::McpForm => self.mcp_field_names().len(),
            _ => 0,
        };
        self.question_cursor = self.question_cursor.min(count.saturating_sub(1));
        let choice_count = request_choices(&self.method, &self.raw).len()
            + usize::from(self.kind == SurfaceKind::McpForm);
        self.choice_cursor = self.choice_cursor.min(choice_count.saturating_sub(1));
    }

    fn questions(&self) -> Vec<Value> {
        self.raw
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn mcp_field_names(&self) -> Vec<String> {
        self.raw
            .pointer("/requestedSchema/properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn emit_choice(&mut self, choice: RequestChoice, cx: &mut Context<Self>) {
        if !self.responding {
            cx.emit(Respond::from_choice(&self.item_key, &choice));
        }
    }

    fn render_summary(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let reason = self
            .raw
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .map(ToOwned::to_owned);
        let (primary, primary_is_command) = match self.method.as_str() {
            "claude/permission" => (
                Some(
                    serde_json::to_string_pretty(&self.raw["input"])
                        .unwrap_or_else(|error| format!("Could not display tool input: {error}")),
                ),
                false,
            ),
            "item/commandExecution/requestApproval" => (
                self.raw
                    .get("command")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                true,
            ),
            "execCommandApproval" => (
                self.raw
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    }),
                true,
            ),
            "item/fileChange/requestApproval" => (
                self.raw
                    .get("grantRoot")
                    .and_then(Value::as_str)
                    .map(|root| format!("Write access under {root}")),
                false,
            ),
            "applyPatchApproval" => (
                self.raw
                    .get("fileChanges")
                    .and_then(Value::as_object)
                    .map(|changes| match changes.keys().next() {
                        Some(file) if changes.len() == 1 => format!("Change {file}"),
                        None => "Apply requested patch".into(),
                        Some(_) => format!("Change {} files", changes.len()),
                    }),
                false,
            ),
            "item/permissions/requestApproval" => {
                (Some("Additional permissions requested".into()), false)
            }
            _ => (None, false),
        };
        let cwd = self
            .raw
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(ToOwned::to_owned);
        let permissions = if self.method == "item/permissions/requestApproval" {
            semantic_permission_rows(&self.raw)
        } else {
            Vec::new()
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .when_some(primary, |this, primary| {
                let highlights = primary_is_command
                    .then(|| shell_highlights(&primary, cx))
                    .unwrap_or_default();
                this.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .font_buffer(cx)
                        .text_ui_sm(cx)
                        .line_height(relative(1.4))
                        .text_color(colors.text)
                        .whitespace_normal()
                        .child(StyledText::new(primary).with_highlights(highlights)),
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
            .when(!permissions.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .font_buffer(cx)
                        .text_ui_xs(cx)
                        .text_color(colors.text_muted)
                        .children(permissions.into_iter().map(|permission| {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Check)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(permission)
                        })),
                )
            })
            .into_any_element()
    }

    fn render_choices(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let choice_offset = usize::from(self.kind == SurfaceKind::McpForm);
        request_choices(&self.method, &self.raw)
            .into_iter()
            .enumerate()
            .map(|(index, choice)| {
                let (icon, color) = request_choice_visual(choice.tone);
                let selected = index + choice_offset == self.choice_cursor;
                let choice_for_click = choice.clone();
                decision_button(choice.label, icon, color, selected)
                    .disabled(self.responding)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choice_cursor = index + choice_offset;
                        this.emit_choice(choice_for_click.clone(), cx);
                    }))
                    .into_any_element()
            })
            .collect()
    }

    fn render_user_input(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let mut rows = Vec::new();
        for (index, question) in self.questions().into_iter().enumerate() {
            let id = question_id(&question);
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
            let active = index == self.question_cursor;
            let multiple = question["multiSelect"] == true;
            let selected = self.selected_answers.get(&id).cloned().unwrap_or_default();
            let option_cursor = self.option_cursors.get(&id).copied().unwrap_or(0);
            let mut option_rows = Vec::new();
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
                let chosen = selected.contains(&label);
                let id_for_click = id.clone();
                let label_for_click = label.clone();
                option_rows.push(
                    ListItem::new(format!(
                        "surface-option-{}-{id}-{option_index}",
                        self.item_key
                    ))
                    .spacing(ListItemSpacing::Sparse)
                    .rounded()
                    .toggle_state(chosen)
                    .focused(active && option_index == option_cursor)
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
                    .end_slot::<Icon>(chosen.then(|| {
                        Icon::new(IconName::Check)
                            .size(IconSize::XSmall)
                            .color(Color::Accent)
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.question_cursor = index;
                        this.option_cursors
                            .insert(id_for_click.clone(), option_index);
                        this.select_answer(
                            &id_for_click,
                            label_for_click.clone(),
                            multiple,
                            window,
                            cx,
                        );
                    }))
                    .into_any_element(),
                );
            }
            let editor = self.editors.get(&id).cloned();
            rows.push(
                div()
                    .w_full()
                    .pl_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .border_l_2()
                    .border_color(if active {
                        colors.text_accent
                    } else {
                        colors.border_variant
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Label::new(header).size(LabelSize::Small))
                            .child(
                                Label::new(prompt)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .when(multiple, |this| {
                        this.child(
                            Label::new("Select all that apply")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .children(option_rows)
                    .when_some(editor, |this, editor| {
                        this.child(
                            div()
                                .h(px(34.))
                                .rounded_md()
                                .border_1()
                                .border_color(colors.border_variant)
                                .bg(colors.editor_background)
                                .px_2()
                                .child(editor),
                        )
                    })
                    .into_any_element(),
            );
        }
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .children(rows)
            .into_any_element()
    }

    fn render_mcp(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let server = self
            .raw
            .get("serverName")
            .and_then(Value::as_str)
            .unwrap_or("MCP server")
            .to_string();
        let message = self
            .raw
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("An MCP server is requesting input.")
            .to_string();
        let mut content = div().w_full().flex().flex_col().gap_2().child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    Label::new(server)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(message).size(LabelSize::Small)),
        );
        match self.kind {
            SurfaceKind::McpForm => {
                let properties = self
                    .raw
                    .pointer("/requestedSchema/properties")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let required = self
                    .raw
                    .pointer("/requestedSchema/required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                let mut fields = Vec::new();
                for (index, (name, schema)) in properties.into_iter().enumerate() {
                    let title = schema.get("title").and_then(Value::as_str).unwrap_or(&name);
                    let suffix = if required.contains(&name.as_str()) {
                        " · required"
                    } else {
                        ""
                    };
                    let description = schema
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let hint = mcp_form_field_hint(&schema);
                    let editor = self.editors.get(&name).cloned();
                    fields.push(
                        div()
                            .w_full()
                            .pl_3()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .border_l_2()
                            .border_color(if index == self.question_cursor {
                                colors.text_accent
                            } else {
                                colors.border_variant
                            })
                            .child(Label::new(format!("{title}{suffix}")).size(LabelSize::Small))
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
                            .when_some(editor, |this, editor| {
                                this.child(
                                    div()
                                        .h(px(34.))
                                        .rounded_md()
                                        .border_1()
                                        .border_color(colors.border_variant)
                                        .bg(colors.editor_background)
                                        .px_2()
                                        .child(editor),
                                )
                            })
                            .into_any_element(),
                    );
                }
                content = content.children(fields);
            }
            SurfaceKind::McpUrl => {
                let url = self
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
                        action_button("Open link", Some(TintColor::Accent), false)
                            .on_click(move |_, _, cx| cx.open_url(&open_url)),
                    );
            }
            SurfaceKind::McpUnsupported => {
                content = content.child(
                    Label::new(
                        "This server requested a form Harness cannot safely render. Decline or cancel to continue.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                );
            }
            _ => {}
        }
        content.into_any_element()
    }
}

impl Focusable for RequestSurface {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RequestSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let body = match self.kind {
            SurfaceKind::Approval => self.render_summary(cx),
            SurfaceKind::UserInput => self.render_user_input(cx),
            SurfaceKind::McpForm | SurfaceKind::McpUrl | SurfaceKind::McpUnsupported => {
                self.render_mcp(cx)
            }
        };
        let choices = self.render_choices(cx);
        let can_submit = matches!(self.kind, SurfaceKind::UserInput | SurfaceKind::McpForm);
        let submit_label = if self.kind == SurfaceKind::McpForm {
            "Submit form"
        } else {
            "Submit answers"
        };
        let responding = self.responding;
        let key_hint = match self.kind {
            SurfaceKind::Approval => "h/l choose · Enter confirm · Esc return",
            SurfaceKind::UserInput => {
                "j/k question · h/l option · i type · Ctrl-Enter submit · Esc return"
            }
            SurfaceKind::McpForm => {
                "j/k field · i edit · h/l action · Ctrl-Enter submit · Esc return"
            }
            SurfaceKind::McpUrl => "i open link · h/l choose · Enter confirm · Esc return",
            SurfaceKind::McpUnsupported => "h/l choose · Enter confirm · Esc return",
        };

        div()
            .id(format!("request-surface-{}", self.item_key))
            .key_context("RequestSurface")
            .track_focus(&self.focus_handle)
            .w_full()
            .min_w_0()
            .pl_2()
            .pr_1()
            .py_1()
            .flex()
            .flex_col()
            .gap_1()
            .border_l_2()
            .border_color(if self.focus_handle.contains_focused(window, cx) {
                colors.text_accent
            } else {
                colors.border_variant
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus_handle.focus(window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(body)
            .when_some(self.validation_error.clone(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        Label::new(if responding {
                            "Sending response…"
                        } else {
                            key_hint
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_end()
                            .gap_1()
                            .when(can_submit, |this| {
                                this.child(
                                    action_button(submit_label, Some(TintColor::Accent), false)
                                        .toggle_state(
                                            self.kind != SurfaceKind::McpForm
                                                || self.choice_cursor == 0,
                                        )
                                        .disabled(responding)
                                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                                )
                            })
                            .children(choices),
                    ),
            )
    }
}

fn surface_kind(method: &str, raw: &Value) -> SurfaceKind {
    match method {
        "item/tool/requestUserInput" | "claude/questions" => SurfaceKind::UserInput,
        "mcpServer/elicitation/request" => match raw.get("mode").and_then(Value::as_str) {
            Some("form") => SurfaceKind::McpForm,
            Some("url") => SurfaceKind::McpUrl,
            _ => SurfaceKind::McpUnsupported,
        },
        _ => SurfaceKind::Approval,
    }
}

fn toggle_answer(selected: &mut Vec<String>, answer: String, multiple: bool) {
    if !multiple {
        *selected = vec![answer];
    } else if selected.contains(&answer) {
        selected.retain(|value| value != &answer);
    } else {
        selected.push(answer);
    }
}

fn question_id(question: &Value) -> String {
    question
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("question")
        .to_string()
}

fn semantic_permission_rows(raw: &Value) -> Vec<String> {
    let Some(permissions) = raw.get("permissions").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    if let Some(file_system) = permissions.get("fileSystem") {
        if let Some(file_system) = file_system.as_object() {
            for (key, label) in [("read", "Read"), ("write", "Write")] {
                let values = concise_string_values(file_system.get(key));
                if !values.is_empty() {
                    rows.push(format!("{label} · {}", values.join(", ")));
                }
            }
            if !file_system.is_empty()
                && !file_system.contains_key("read")
                && !file_system.contains_key("write")
            {
                rows.push("File system access".into());
            }
        } else {
            rows.push("File system access".into());
        }
    }
    if let Some(network) = permissions.get("network") {
        let details = network
            .as_object()
            .map(|network| {
                ["host", "hosts", "domains"]
                    .into_iter()
                    .flat_map(|key| concise_string_values(network.get(key)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| concise_string_values(Some(network)));
        if details.is_empty() {
            let enabled = network
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            rows.push(if enabled {
                "Network access".into()
            } else {
                "Network access disabled".into()
            });
        } else {
            rows.push(format!("Network · {}", details.join(", ")));
        }
    }
    for key in permissions.keys() {
        if !matches!(key.as_str(), "fileSystem" | "network") {
            rows.push(format!("{} access", humanize_permission_key(key)));
        }
    }
    rows
}

fn concise_string_values(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn humanize_permission_key(key: &str) -> String {
    let mut result = String::new();
    for character in key.chars() {
        if character.is_uppercase() && !result.is_empty() {
            result.push(' ');
        }
        result.extend(character.to_lowercase());
    }
    let mut characters = result.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => "Permission".into(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceSyncDecision {
    Ignore,
    Upsert,
    KeepResponding,
    Remove,
}

pub(crate) fn surface_sync_decision(
    is_live: bool,
    unresolved: bool,
    responding: bool,
    exists: bool,
) -> SurfaceSyncDecision {
    if !is_live {
        return if exists {
            SurfaceSyncDecision::Remove
        } else {
            SurfaceSyncDecision::Ignore
        };
    }
    if unresolved {
        SurfaceSyncDecision::Upsert
    } else if responding && exists {
        SurfaceSyncDecision::KeepResponding
    } else if exists {
        SurfaceSyncDecision::Remove
    } else {
        SurfaceSyncDecision::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn questions_share_a_surface_but_keep_single_and_multiple_selection_distinct() {
        assert_eq!(
            surface_kind("claude/questions", &Value::Null),
            SurfaceKind::UserInput
        );
        assert_eq!(
            surface_kind("item/tool/requestUserInput", &Value::Null),
            SurfaceKind::UserInput
        );
        let mut selected = Vec::new();
        toggle_answer(&mut selected, "Blue".into(), false);
        toggle_answer(&mut selected, "Amber".into(), false);
        assert_eq!(selected, ["Amber"]);
        toggle_answer(&mut selected, "Blue".into(), true);
        assert_eq!(selected, ["Amber", "Blue"]);
        toggle_answer(&mut selected, "Amber".into(), true);
        assert_eq!(selected, ["Blue"]);
        toggle_answer(&mut selected, "Blue".into(), true);
        assert!(selected.is_empty());
    }

    #[test]
    fn semantic_permissions_are_compact_and_never_dump_json() {
        let rows = semantic_permission_rows(&json!({
            "permissions": {
                "fileSystem": {
                    "read": ["/workspace"],
                    "write": ["/workspace/out"],
                },
                "network": {"hosts": ["api.example.com"]},
                "cameraCapture": true,
            },
        }));

        assert_eq!(
            rows,
            vec![
                "Read · /workspace",
                "Write · /workspace/out",
                "Network · api.example.com",
                "Camera capture access",
            ]
        );
        assert!(
            rows.iter()
                .all(|row| !row.contains('{') && !row.contains('}'))
        );
    }
}
