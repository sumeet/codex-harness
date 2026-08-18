use std::{collections::HashMap, fs, path::PathBuf};

use anyhow::Context as _;
use command_palette_core::{
    CommandPaletteSession, ConfirmedCommand, HistoryDirection, PaletteCommand, humanize_action_name,
};
use command_palette_hooks::{
    CommandPaletteFilter, CommandPaletteInvocationContext, GlobalCommandPaletteInterceptor,
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyBinding as GpuiKeyBinding, Render, Window, actions, div, prelude::*, px,
};
use harness_editor::{LocalEditor, LocalEditorChanged};
use serde_json::{Value, json};
use ui::{
    Color, HighlightedLabel, Icon, IconName, IconSize, KeyBinding, Label, LabelCommon, LabelSize,
    ListItem, ListItemSpacing, Toggleable, prelude::ActiveTheme,
};

actions!(harness_palette, [MoveUp, MoveDown, Confirm, Dismiss]);

const MAX_VISIBLE_MATCHES: usize = 12;
const MAX_HISTORY_ENTRIES: usize = 100;

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct PaletteState {
    pub(crate) history: Vec<String>,
    pub(crate) usage: HashMap<String, u16>,
}

fn palette_state_path() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|directory| directory.join("codex-harness").join("palette.json"))
}

fn palette_state_from_value(value: &Value) -> PaletteState {
    let history = value
        .get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|query| !query.trim().is_empty())
        .map(ToOwned::to_owned)
        .rev()
        .take(MAX_HISTORY_ENTRIES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let usage = value
        .get("usage")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(command, count)| {
            let count = count.as_u64()?;
            (count > 0).then_some((command.clone(), count.min(u16::MAX as u64) as u16))
        })
        .collect();
    PaletteState { history, usage }
}

pub(crate) fn load_state() -> PaletteState {
    let Some(path) = palette_state_path() else {
        return PaletteState::default();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return PaletteState::default();
    };
    serde_json::from_str(&contents)
        .ok()
        .as_ref()
        .map(palette_state_from_value)
        .unwrap_or_default()
}

pub(crate) fn save_state(history: &[String], usage: &HashMap<String, u16>) -> anyhow::Result<()> {
    let Some(path) = palette_state_path() else {
        return Ok(());
    };
    let parent = path
        .parent()
        .context("palette state path has no parent directory")?;
    fs::create_dir_all(parent).context("create Harness state directory")?;
    let value = json!({ "history": history, "usage": usage });
    let contents = serde_json::to_vec_pretty(&value).context("encode palette state")?;
    fs::write(path, contents).context("write palette state")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PaletteEvent {
    Confirmed,
    Dismissed,
}

pub(crate) struct PaletteOverlay {
    input: Entity<LocalEditor>,
    session: CommandPaletteSession,
    previous_focus: FocusHandle,
    history: Vec<String>,
    usage: HashMap<String, u16>,
    confirmed: Option<ConfirmedCommand>,
    loading: bool,
}

impl EventEmitter<PaletteEvent> for PaletteOverlay {}

impl Focusable for PaletteOverlay {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        GpuiKeyBinding::new("up", MoveUp, Some("HarnessPalette")),
        GpuiKeyBinding::new("ctrl-p", MoveUp, Some("HarnessPalette")),
        GpuiKeyBinding::new("shift-tab", MoveUp, Some("HarnessPalette")),
        GpuiKeyBinding::new("down", MoveDown, Some("HarnessPalette")),
        GpuiKeyBinding::new("ctrl-n", MoveDown, Some("HarnessPalette")),
        GpuiKeyBinding::new("tab", MoveDown, Some("HarnessPalette")),
        GpuiKeyBinding::new("enter", Confirm, Some("HarnessPalette")),
        GpuiKeyBinding::new("escape", Dismiss, Some("HarnessPalette")),
        GpuiKeyBinding::new("ctrl-c", Dismiss, Some("HarnessPalette")),
    ]);
}

impl PaletteOverlay {
    pub(crate) fn new(
        initial_query: &str,
        previous_focus: FocusHandle,
        history: Vec<String>,
        usage: HashMap<String, u16>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = CommandPaletteFilter::try_global(cx);
        let commands = window
            .available_actions(cx)
            .into_iter()
            .filter(|action| {
                !matches!(
                    action.name(),
                    "command_palette::Toggle" | "command_palette::OpenWithQuery"
                ) && !filter.is_some_and(|filter| filter.is_hidden(&**action))
            })
            .map(|action| PaletteCommand::new(humanize_action_name(action.name()), action))
            .collect::<Vec<_>>();
        let input = cx.new(|cx| LocalEditor::plain_single_line("Type a command…", window, cx));
        cx.subscribe(&input, |this, _, _: &LocalEditorChanged, cx| {
            this.update_matches(cx);
        })
        .detach();

        let mut session = CommandPaletteSession::new(commands);
        session.set_history(history.clone());
        let mut this = Self {
            input,
            session,
            previous_focus,
            history,
            usage,
            confirmed: None,
            loading: false,
        };
        this.input
            .update(cx, |input, cx| input.set_text(initial_query, window, cx));
        this.input.focus_handle(cx).focus(window, cx);
        this.update_matches(cx);
        this
    }

    pub(crate) fn previous_focus(&self) -> FocusHandle {
        self.previous_focus.clone()
    }

    pub(crate) fn take_confirmed(&mut self) -> Option<ConfirmedCommand> {
        self.confirmed.take()
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).text(cx)
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let pending = self
            .session
            .begin_update(query, harness_alias, self.usage.clone());
        let invocation_context = CommandPaletteInvocationContext::default();
        let interceptor = GlobalCommandPaletteInterceptor::intercept(
            pending.resolved_query(),
            &invocation_context,
            cx,
        );
        let pending = pending.with_interceptor(interceptor);
        let executor = cx.background_executor().clone();
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let update = pending.compute(executor).await;
            this.update(cx, |this, cx| {
                if this.session.apply_update(update) {
                    this.loading = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn move_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let history = self.history.clone();
        if let Some(query) =
            self.session
                .select_history(HistoryDirection::Previous, &query, move || history)
        {
            self.input
                .update(cx, |input, cx| input.set_text(query, window, cx));
            return;
        }
        self.session
            .set_selected_index(self.session.selected_index().saturating_sub(1));
        cx.notify();
    }

    fn move_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let history = self.history.clone();
        if let Some(query) =
            self.session
                .select_history(HistoryDirection::Next, &query, move || history)
        {
            self.input
                .update(cx, |input, cx| input.set_text(query, window, cx));
            return;
        }
        if self.session.match_count() > 0 {
            self.session.set_selected_index(
                (self.session.selected_index() + 1).min(self.session.match_count() - 1),
            );
            cx.notify();
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let history = self.history.clone();
        let Some(command) = self.session.confirm_selected(move || history) else {
            return;
        };
        self.confirmed = Some(command);
        cx.emit(PaletteEvent::Confirmed);
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(PaletteEvent::Dismissed);
    }
}

impl Render for PaletteOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let selected = self.session.selected_index();
        let match_count = self.session.match_count();
        let start = selected
            .saturating_add(1)
            .saturating_sub(MAX_VISIBLE_MATCHES);
        let end = (start + MAX_VISIBLE_MATCHES).min(match_count);
        let previous_focus = self.previous_focus.clone();
        let rows = (start..end)
            .filter_map(|index| {
                let matching = self.session.matches().get(index)?;
                let command = self.session.commands().get(matching.candidate_id)?;
                let binding = KeyBinding::for_action_in(&*command.action, &previous_focus, cx);
                Some(
                    ListItem::new(index)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(index == selected)
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    HighlightedLabel::new(
                                        command.name.clone(),
                                        matching.positions.clone(),
                                    )
                                    .size(LabelSize::Small),
                                )
                                .child(binding),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.session.set_selected_index(index);
                            this.confirm(cx);
                        })),
                )
            })
            .collect::<Vec<_>>();

        div()
            .key_context("HarnessPalette")
            .w_full()
            .min_w_0()
            .rounded_lg()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.elevated_surface_background)
            .shadow_lg()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_action(cx.listener(|this, _: &MoveUp, window, cx| this.move_up(window, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, window, cx| this.move_down(window, cx)))
            .on_action(cx.listener(|this, _: &Confirm, _, cx| this.confirm(cx)))
            .on_action(cx.listener(|this, _: &Dismiss, _, cx| this.dismiss(cx)))
            .child(
                div()
                    .h(px(44.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().min_w_0().child(self.input.clone()))
                    .when(self.loading, |this| {
                        this.child(
                            Label::new("Matching…")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(
                div()
                    .max_h(px(440.))
                    .py_1()
                    .when(rows.is_empty(), |this| {
                        this.child(
                            div()
                                .px_4()
                                .py_4()
                                .text_sm()
                                .text_color(colors.text_muted)
                                .child("No matching commands"),
                        )
                    })
                    .children(rows),
            )
            .child(
                div()
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Label::new(format!("{match_count} commands"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("↑↓ navigate · Enter run · Esc close")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }
}

fn harness_alias(query: &str) -> Option<String> {
    let query = query.trim().trim_start_matches(':');
    match query {
        "new" | "enew" => Some("harness: new task".into()),
        "text" => Some("harness: show text transcript".into()),
        "rich" => Some("harness: show rich transcript".into()),
        "mono" => Some("harness: use buffer typography".into()),
        "reading" => Some("harness: use reading typography".into()),
        "compose" => Some("harness: focus composer".into()),
        "tasks" => Some("harness: toggle sidebar".into()),
        "stop" => Some("harness: stop".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_aliases_resolve_only_to_executable_harness_actions() {
        assert_eq!(harness_alias(":new").as_deref(), Some("harness: new task"));
        assert_eq!(
            harness_alias("text").as_deref(),
            Some("harness: show text transcript")
        );
        assert_eq!(
            harness_alias("rich").as_deref(),
            Some("harness: show rich transcript")
        );
        assert_eq!(
            harness_alias("mono").as_deref(),
            Some("harness: use buffer typography")
        );
        assert_eq!(
            harness_alias("reading").as_deref(),
            Some("harness: use reading typography")
        );
        assert_eq!(harness_alias("w"), None);
        assert_eq!(harness_alias("q"), None);
    }

    #[test]
    fn persisted_palette_state_is_bounded_and_rejects_invalid_usage() {
        let history = (0..105)
            .map(|index| Value::String(format!("query-{index}")))
            .collect::<Vec<_>>();
        let state = palette_state_from_value(&json!({
            "history": history,
            "usage": {
                "harness: show rich transcript": 7,
                "zero": 0,
                "too-large": 100000,
                "wrong-type": "4"
            }
        }));
        assert_eq!(state.history.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(state.history.first().map(String::as_str), Some("query-5"));
        assert_eq!(state.usage.get("harness: show rich transcript"), Some(&7));
        assert_eq!(state.usage.get("too-large"), Some(&u16::MAX));
        assert!(!state.usage.contains_key("zero"));
        assert!(!state.usage.contains_key("wrong-type"));
    }
}
