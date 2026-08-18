mod persistence;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use client::parse_zed_link;
use command_palette_core::{
    CommandPaletteSession, CommandPaletteUpdate, HistoryDirection, PaletteCommand, UpdateGeneration,
};
use command_palette_hooks::{
    CommandInterceptItem, CommandInterceptResult, CommandPaletteFilter,
    CommandPaletteInvocationContext, FilenameCompletionProvider, GlobalCommandPaletteInterceptor,
};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ParentElement, Render, Styled, Task, TaskExt, WeakEntity, Window,
};
use persistence::CommandPaletteDB;
use picker::Direction;
use picker::{Picker, PickerDelegate};
use postage::{sink::Sink, stream::Stream};
use settings::Settings;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use util::{
    paths::PathStyle,
    rel_path::{RelPath, RelPathBuf},
};
use workspace::{ModalView, Workspace, WorkspaceSettings};
use zed_actions::{OpenZedUrl, command_palette::Toggle};

pub use command_palette_core::{humanize_action_name, normalize_action_query};
pub use zed_actions::command_palette::OpenWithQuery;

pub fn init(cx: &mut App) {
    command_palette_hooks::init(cx);
    cx.observe_new(CommandPalette::register).detach();
}

impl ModalView for CommandPalette {}

pub struct CommandPalette {
    picker: Entity<Picker<CommandPaletteDelegate>>,
}

fn workspace_invocation_context(
    workspace: WeakEntity<Workspace>,
) -> CommandPaletteInvocationContext {
    CommandPaletteInvocationContext::default().with_filename_completion_provider(
        FilenameCompletionProvider::new(move |query, cx| {
            workspace_filename_completions(query, workspace.clone(), cx)
        }),
    )
}

fn workspace_filename_completions(
    query: &str,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> Task<Vec<String>> {
    let Some(workspace) = workspace.upgrade() else {
        return Task::ready(Vec::new());
    };

    let (task, query_path) = workspace.update(cx, |workspace, cx| {
        let prefix = workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .next()
            .or_else(std::env::home_dir)
            .unwrap_or_else(|| PathBuf::from(""));

        let rel_path = match RelPath::new(Path::new(query), PathStyle::local()) {
            Ok(path) => path.to_rel_path_buf(),
            Err(_) => {
                return (Task::ready(Ok(Vec::new())), RelPathBuf::new());
            }
        };

        let rel_path = if query.ends_with(PathStyle::local().primary_separator()) {
            rel_path
        } else {
            rel_path
                .parent()
                .map(|rel_path| rel_path.to_rel_path_buf())
                .unwrap_or(RelPathBuf::new())
        };

        let task = workspace.project().update(cx, |project, cx| {
            let path = prefix
                .join(rel_path.as_std_path())
                .to_string_lossy()
                .to_string();
            project.list_directory(path, cx)
        });

        (task, rel_path)
    });

    cx.background_spawn(async move {
        let directories = task.await.unwrap_or_default();
        directories
            .iter()
            .map(|dir| {
                let path = RelPath::new(dir.path.as_path(), PathStyle::local())
                    .map(|cow| cow.into_owned())
                    .unwrap_or(RelPathBuf::new());
                let mut path_string = query_path
                    .join(&path)
                    .display(PathStyle::local())
                    .to_string();
                if dir.is_dir {
                    path_string.push_str(PathStyle::local().primary_separator());
                }
                path_string
            })
            .collect()
    })
}

impl CommandPalette {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            Self::toggle(workspace, "", window, cx)
        });
        workspace.register_action(|workspace, action: &OpenWithQuery, window, cx| {
            Self::toggle(workspace, &action.query, window, cx)
        });
    }

    pub fn toggle(
        workspace: &mut Workspace,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace.active_modal::<CommandPalette>(cx).is_some() {
            workspace.hide_modal(window, cx);
            return;
        }

        if workspace.has_active_modal(window, cx) && !workspace.hide_modal(window, cx) {
            return;
        }

        let Some(previous_focus_handle) = window.focused(cx) else {
            return;
        };

        let entity = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            CommandPalette::new(previous_focus_handle, query, entity, window, cx)
        });
    }

    fn new(
        previous_focus_handle: FocusHandle,
        query: &str,
        entity: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = CommandPaletteFilter::try_global(cx);

        let commands = window
            .available_actions(cx)
            .into_iter()
            .filter_map(|action| {
                if filter.is_some_and(|filter| filter.is_hidden(&*action)) {
                    return None;
                }

                Some(PaletteCommand::new(
                    humanize_action_name(action.name()),
                    action,
                ))
            })
            .collect();

        let invocation_context = workspace_invocation_context(entity);
        let delegate = CommandPaletteDelegate::new(
            cx.entity().downgrade(),
            invocation_context,
            commands,
            previous_focus_handle,
        );

        let picker = cx.new(|cx| {
            // One-shot action; there's nothing to reopen.
            let picker = Picker::uniform_list(delegate, window, cx)
                .reopenable(false, cx)
                .show_scrollbar(true);
            picker.set_query(query, window, cx);
            picker
        });
        Self { picker }
    }

    pub fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.set_query(query, window, cx))
    }
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CommandPalette")
            .child(self.picker.clone())
    }
}

pub struct CommandPaletteDelegate {
    command_palette: WeakEntity<CommandPalette>,
    invocation_context: CommandPaletteInvocationContext,
    session: CommandPaletteSession,
    previous_focus_handle: FocusHandle,
    updating_matches: Option<(
        UpdateGeneration,
        Task<()>,
        postage::dispatch::Receiver<CommandPaletteUpdate>,
    )>,
}

impl CommandPaletteDelegate {
    fn new(
        command_palette: WeakEntity<CommandPalette>,
        invocation_context: CommandPaletteInvocationContext,
        commands: Vec<PaletteCommand>,
        previous_focus_handle: FocusHandle,
    ) -> Self {
        Self {
            command_palette,
            invocation_context,
            session: CommandPaletteSession::new(commands),
            previous_focus_handle,
            updating_matches: None,
        }
    }

    fn matches_updated(&mut self, update: CommandPaletteUpdate, _: &mut Context<Picker<Self>>) {
        let generation = update.generation();
        if self.session.apply_update(update)
            && self
                .updating_matches
                .as_ref()
                .is_some_and(|(pending_generation, _, _)| *pending_generation == generation)
        {
            self.updating_matches.take();
        }
    }

    /// Hit count for each command in the palette.
    /// We only account for commands triggered directly via command palette and not by e.g. keystrokes because
    /// if a user already knows a keystroke for a command, they are unlikely to use a command palette to look for it.
    fn hit_counts(&self, cx: &App) -> HashMap<String, u16> {
        if let Ok(commands) = CommandPaletteDB::global(cx).list_commands_used() {
            commands
                .into_iter()
                .map(|command| (command.command_name, command.invocations))
                .collect()
        } else {
            HashMap::new()
        }
    }

    fn selected_command(&self) -> Option<&PaletteCommand> {
        self.session.selected_command()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_history(&mut self, queries: &[&str]) {
        self.session
            .set_history(queries.iter().map(|query| (*query).to_owned()));
    }
}

impl PickerDelegate for CommandPaletteDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "command palette"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Execute a command...".into()
    }

    fn select_history(
        &mut self,
        direction: Direction,
        query: &str,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        let direction = match direction {
            Direction::Up => HistoryDirection::Previous,
            Direction::Down => HistoryDirection::Next,
        };
        self.session.select_history(direction, query, || {
            CommandPaletteDB::global(cx)
                .list_recent_queries()
                .unwrap_or_default()
        })
    }

    fn match_count(&self) -> usize {
        self.session.match_count()
    }

    fn selected_index(&self) -> usize {
        self.session.selected_index()
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.session.set_selected_index(ix);
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let settings = WorkspaceSettings::get_global(cx);
        let hit_counts = self.hit_counts(cx);
        let pending = self.session.begin_update(
            query,
            |query| {
                settings
                    .command_aliases
                    .get(query)
                    .map(|alias| alias.as_ref().to_owned())
            },
            hit_counts,
        );
        let resolved_query = pending.resolved_query();
        let intercept_task = if parse_zed_link(resolved_query, cx).is_some() {
            Some(Task::ready(CommandInterceptResult {
                results: vec![CommandInterceptItem {
                    action: OpenZedUrl {
                        url: resolved_query.to_owned().into(),
                    }
                    .boxed_clone(),
                    string: resolved_query.to_owned(),
                    positions: vec![],
                }],
                exclusive: false,
            }))
        } else {
            GlobalCommandPaletteInterceptor::intercept(resolved_query, &self.invocation_context, cx)
        };
        let pending = pending.with_interceptor(intercept_task);
        let generation = pending.generation();

        let (mut tx, mut rx) = postage::dispatch::channel(1);
        let executor = cx.background_executor().clone();
        let task = cx.background_spawn({
            async move {
                let update = pending.compute(executor).await;
                if tx.send(update).await.is_err() {
                    log::error!("command palette match receiver dropped before update delivery");
                }
            }
        });

        self.updating_matches = Some((generation, task, rx.clone()));

        cx.spawn_in(window, async move |picker, cx| {
            let Some(update) = rx.recv().await else {
                return;
            };

            picker
                .update(cx, |picker, cx| picker.delegate.matches_updated(update, cx))
                .ok();
        })
    }

    fn finalize_update_matches(
        &mut self,
        _query: String,
        duration: Duration,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        let Some((generation, task, rx)) = self.updating_matches.take() else {
            return true;
        };

        match cx
            .foreground_executor()
            .block_with_timeout(duration, rx.clone().recv())
        {
            Ok(Some(update)) => {
                debug_assert_eq!(generation, update.generation());
                self.matches_updated(update, cx);
                true
            }
            _ => {
                self.updating_matches = Some((generation, task, rx));
                false
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.command_palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if secondary {
            if self.session.matches().is_empty() {
                return;
            }
            let Some(selected_command) = self.selected_command() else {
                return;
            };
            let action_name = selected_command.action.name();
            let open_keymap = Box::new(zed_actions::ChangeKeybinding {
                action: action_name.to_string(),
            });
            window.dispatch_action(open_keymap, cx);
            self.dismissed(window, cx);
            return;
        }

        if self.session.matches().is_empty() {
            self.dismissed(window, cx);
            return;
        }

        let Some(command) = self.session.confirm_selected(|| {
            CommandPaletteDB::global(cx)
                .list_recent_queries()
                .unwrap_or_default()
        }) else {
            return;
        };
        telemetry::event!(
            "Action Invoked",
            source = "command palette",
            action = command.name
        );
        let command_name = command.name.clone();
        let latest_query = command.resolved_query;
        let db = CommandPaletteDB::global(cx);
        cx.background_spawn(async move {
            db.write_command_invocation(command_name, latest_query)
                .await
        })
        .detach_and_log_err(cx);
        let action = command.action;
        window.focus(&self.previous_focus_handle, cx);
        self.dismissed(window, cx);
        window.dispatch_action(action, cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matching_command = self.session.matches().get(ix)?;
        let command = self.session.commands().get(matching_command.candidate_id)?;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .py_px()
                        .justify_between()
                        .child(HighlightedLabel::new(
                            command.name.clone(),
                            matching_command.positions.clone(),
                        ))
                        .child(KeyBinding::for_action_in(
                            &*command.action,
                            &self.previous_focus_handle,
                            cx,
                        )),
                ),
        )
    }

    fn render_footer(
        &self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let selected_command = self.selected_command()?;
        let keybind =
            KeyBinding::for_action_in(&*selected_command.action, &self.previous_focus_handle, cx);

        let focus_handle = &self.previous_focus_handle;
        let keybinding_buttons = if keybind.has_binding(window) {
            Button::new("change", "Change Keybinding…")
                .key_binding(
                    KeyBinding::for_action_in(&menu::SecondaryConfirm, focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx);
                })
        } else {
            Button::new("add", "Add Keybinding…")
                .key_binding(
                    KeyBinding::for_action_in(&menu::SecondaryConfirm, focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx);
                })
        };

        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(keybinding_buttons)
                .child(
                    Button::new("run-action", "Run")
                        .key_binding(
                            KeyBinding::for_action_in(&menu::Confirm, &focus_handle, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                        }),
                )
                .into_any(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use editor::Editor;
    use go_to_line::GoToLine;
    use gpui::{TestAppContext, VisualTestContext};
    use language::Point;
    use project::Project;
    use settings::KeymapFile;
    use workspace::{AppState, MultiWorkspace, Workspace};

    #[test]
    fn test_humanize_action_name() {
        assert_eq!(
            humanize_action_name("editor::GoToDefinition"),
            "editor: go to definition"
        );
        assert_eq!(
            humanize_action_name("editor::Backspace"),
            "editor: backspace"
        );
        assert_eq!(
            humanize_action_name("go_to_line::Deploy"),
            "go to line: deploy"
        );
        assert_eq!(
            humanize_action_name("agent::OpenGlobalAGENTS.mdRules"),
            "agent: open global AGENTS.md rules"
        );
        assert_eq!(
            humanize_action_name("agent::OpenProjectAGENTS.mdRules"),
            "agent: open project AGENTS.md rules"
        );
        assert_eq!(humanize_action_name("editor::OpenURL"), "editor: open URL");
        assert_eq!(
            humanize_action_name("editor::OpenURLParser"),
            "editor: open URL parser"
        );
    }

    #[test]
    fn test_normalize_query() {
        assert_eq!(
            normalize_action_query("editor: backspace"),
            "editor: backspace"
        );
        assert_eq!(
            normalize_action_query("editor:  backspace"),
            "editor: backspace"
        );
        assert_eq!(
            normalize_action_query("editor:    backspace"),
            "editor: backspace"
        );
        assert_eq!(
            normalize_action_query("editor::GoToDefinition"),
            "editor:GoToDefinition"
        );
        assert_eq!(
            normalize_action_query("editor::::GoToDefinition"),
            "editor:GoToDefinition"
        );
        assert_eq!(
            normalize_action_query("editor: :GoToDefinition"),
            "editor: :GoToDefinition"
        );
        assert_eq!(
            normalize_action_query("terminal_panel::Toggle"),
            "terminal panel:Toggle"
        );
        assert_eq!(
            normalize_action_query("project_panel::ToggleFocus"),
            "project panel:ToggleFocus"
        );
    }

    #[gpui::test]
    async fn test_command_palette(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let db = cx.update(|cx| persistence::CommandPaletteDB::global(cx));
        db.clear_all().await.unwrap();
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let editor = cx.new_window_entity(|window, cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("abc", window, cx);
            editor
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        palette.read_with(cx, |palette, _| {
            assert!(palette.delegate.session.commands().len() > 5);
            let is_sorted = |actions: &[PaletteCommand]| {
                actions.windows(2).all(|pair| pair[0].name <= pair[1].name)
            };
            assert!(is_sorted(palette.delegate.session.commands()));
        });

        cx.simulate_input("bcksp");

        palette.read_with(cx, |palette, _| {
            assert_eq!(
                palette.delegate.session.matches()[0].string,
                "editor: backspace"
            );
        });

        cx.simulate_keystrokes("enter");

        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<CommandPalette>(cx).is_none());
            assert_eq!(editor.read(cx).text(cx), "ab")
        });

        // Add namespace filter, and redeploy the palette
        cx.update(|_window, cx| {
            CommandPaletteFilter::update_global(cx, |filter, _| {
                filter.hide_namespace("editor");
            });
        });

        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("bcksp");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });
        palette.read_with(cx, |palette, _| {
            assert!(palette.delegate.session.matches().is_empty())
        });
    }

    #[gpui::test]
    async fn test_selected_command_none_when_no_matches(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.simulate_keystrokes("cmd-shift-p");
        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        cx.simulate_input("definitely-no-command-should-match-this");
        cx.background_executor.run_until_parked();

        picker.read_with(cx, |picker, _cx| {
            assert!(picker.delegate.session.matches().is_empty());
            assert!(picker.delegate.selected_command().is_none());
        });
    }
    #[gpui::test]
    async fn test_normalized_matches(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let editor = cx.new_window_entity(|window, cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("abc", window, cx);
            editor
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        // Test normalize (trimming whitespace and double colons)
        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        cx.simulate_input("Editor::    Backspace");
        palette.read_with(cx, |palette, _| {
            assert_eq!(
                palette.delegate.session.matches()[0].string,
                "editor: backspace"
            );
        });
    }

    #[gpui::test]
    async fn test_go_to_line(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.simulate_keystrokes("cmd-n");

        let editor = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).unwrap()
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("1\n2\n3\n4\n5\n6\n", window, cx)
        });

        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("go to line: Toggle");
        cx.simulate_keystrokes("enter");

        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<GoToLine>(cx).is_some())
        });

        cx.simulate_keystrokes("3 enter");

        editor.update_in(cx, |editor, window, cx| {
            assert!(editor.focus_handle(cx).is_focused(window));
            assert_eq!(
                editor
                    .selections
                    .last::<Point>(&editor.display_snapshot(cx))
                    .range()
                    .start,
                Point::new(2, 0)
            );
        });
    }

    #[gpui::test]
    async fn test_reopen_command_palette_over_another_modal(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

        cx.simulate_keystrokes("cmd-n");

        for _ in 0..2 {
            cx.simulate_keystrokes("cmd-shift-p");
            cx.simulate_input("go to line: Toggle");
            cx.simulate_keystrokes("enter");

            workspace.update(cx, |workspace, cx| {
                assert!(workspace.active_modal::<GoToLine>(cx).is_some());
            });
        }
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            menu::init();
            go_to_line::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            cx.bind_keys(KeymapFile::load_panic_on_failure(
                r#"[
                    {
                        "bindings": {
                            "cmd-n": "workspace::NewFile",
                            "enter": "menu::Confirm",
                            "cmd-shift-p": "command_palette::Toggle",
                            "up": "menu::SelectPrevious",
                            "down": "menu::SelectNext"
                        }
                    }
                ]"#,
                cx,
            ));
            app_state
        })
    }

    fn open_palette_with_history(
        workspace: &Entity<Workspace>,
        history: &[&str],
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<CommandPaletteDelegate>> {
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        palette.update(cx, |palette, _cx| {
            palette.delegate.seed_history(history);
        });

        palette
    }

    #[gpui::test]
    async fn test_history_navigation_basic(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["backspace", "select all"], cx);

        // Query should be empty initially
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "");
        });

        // Press up - should load most recent query "select all"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press up again - should load "backspace"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "backspace");
        });

        // Press down - should go back to "select all"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press down again - should clear query (exit history mode)
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "");
        });
    }

    #[gpui::test]
    async fn test_history_mode_exit_on_typing(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["backspace"], cx);

        // Press up to enter history mode
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "backspace");
        });

        // Type something - should append to the history query
        cx.simulate_input("x");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "backspacex");
        });
    }

    #[gpui::test]
    async fn test_history_navigation_with_suggestions(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["editor: close", "editor: open"], cx);

        // Open palette with a query that has multiple matches
        cx.simulate_input("editor");
        cx.background_executor.run_until_parked();

        // Should have multiple matches, selected_ix should be 0
        palette.read_with(cx, |palette, _| {
            assert!(palette.delegate.session.matches().len() > 1);
            assert_eq!(palette.delegate.session.selected_index(), 0);
        });

        // Press down - should navigate to next suggestion (not history)
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, _| {
            assert_eq!(palette.delegate.session.selected_index(), 1);
        });

        // Press up - should go back to first suggestion
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, _| {
            assert_eq!(palette.delegate.session.selected_index(), 0);
        });

        // Press up again at top - should enter history mode and show previous query
        // that matches the "editor" prefix
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "editor: open");
        });
    }

    #[gpui::test]
    async fn test_history_prefix_search(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(
            &workspace,
            &["open file", "select all", "select line", "backspace"],
            cx,
        );

        // Type "sel" as a prefix
        cx.simulate_input("sel");
        cx.background_executor.run_until_parked();

        // Press up - should get "select line" (most recent matching "sel")
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select line");
        });

        // Press up again - should get "select all" (next matching "sel")
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press up again - should stay at "select all" (no more matches for "sel")
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press down - should go back to "select line"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select line");
        });

        // Press down again - should return to original prefix "sel"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "sel");
        });
    }

    #[gpui::test]
    async fn test_history_prefix_search_no_matches(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette =
            open_palette_with_history(&workspace, &["open file", "backspace", "select all"], cx);

        // Type "xyz" as a prefix that doesn't match anything
        cx.simulate_input("xyz");
        cx.background_executor.run_until_parked();

        // Press up - should stay at "xyz" (no matches)
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "xyz");
        });
    }

    #[gpui::test]
    async fn test_history_empty_prefix_searches_all(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["alpha", "beta", "gamma"], cx);

        // With empty query, press up - should get "gamma" (most recent)
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "gamma");
        });

        // Press up - should get "beta"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "beta");
        });

        // Press up - should get "alpha"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "alpha");
        });

        // Press down - should get "beta"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "beta");
        });

        // Press down - should get "gamma"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "gamma");
        });

        // Press down - should return to empty string (exit history mode)
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "");
        });
    }
}
