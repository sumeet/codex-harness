//! Host-neutral command palette matching and session state.

use std::{
    cmp::{self, Reverse},
    collections::{HashMap, VecDeque},
};

use command_palette_hooks::{CommandInterceptItem, CommandInterceptResult};
use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{Action, BackgroundExecutor, Task};

/// A command available to a command palette session.
pub struct PaletteCommand {
    /// The human-readable command name used for matching and rendering.
    pub name: String,
    /// The action dispatched when the command is confirmed.
    pub action: Box<dyn Action>,
}

impl PaletteCommand {
    /// Creates a palette command.
    pub fn new(name: String, action: Box<dyn Action>) -> Self {
        Self { name, action }
    }
}

impl Clone for PaletteCommand {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            action: self.action.boxed_clone(),
        }
    }
}

impl std::fmt::Debug for PaletteCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaletteCommand")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Identifies a particular asynchronous match update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateGeneration(u64);

/// The result of confirming the selected command.
pub struct ConfirmedCommand {
    /// The human-readable name of the confirmed command.
    pub name: String,
    /// The fully resolved query used to select the command.
    pub resolved_query: String,
    /// The action to dispatch.
    pub action: Box<dyn Action>,
}

/// The direction in which command history should be traversed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryDirection {
    /// Traverse toward older commands.
    Previous,
    /// Traverse toward newer commands.
    Next,
}

#[derive(Default)]
struct QueryHistory {
    history: Option<VecDeque<String>>,
    cursor: Option<usize>,
    prefix: Option<String>,
}

impl QueryHistory {
    fn ensure_loaded(&mut self, load: impl FnOnce() -> Vec<String>) {
        if self.history.is_none() {
            self.history = Some(load().into_iter().collect());
        }
    }

    fn add(&mut self, query: String) {
        let Some(history) = self.history.as_mut() else {
            self.reset_cursor();
            return;
        };
        if let Some(pos) = history.iter().position(|item| item == &query) {
            history.remove(pos);
        }
        history.push_back(query);
        self.reset_cursor();
    }

    fn validate_cursor(&mut self, current_query: &str) -> Option<usize> {
        if let Some(pos) = self.cursor
            && self
                .history
                .as_ref()
                .and_then(|history| history.get(pos))
                .map(String::as_str)
                != Some(current_query)
        {
            self.reset_cursor();
        }
        self.cursor
    }

    fn previous(&mut self, current_query: &str) -> Option<String> {
        if self.validate_cursor(current_query).is_none() {
            self.prefix = Some(current_query.to_owned());
        }

        let prefix = self.prefix.clone().unwrap_or_default();
        let history = self.history.as_ref()?;
        let start_index = self.cursor.unwrap_or(history.len());

        for index in (0..start_index).rev() {
            if history
                .get(index)
                .is_some_and(|entry| entry.starts_with(&prefix))
            {
                self.cursor = Some(index);
                return history.get(index).cloned();
            }
        }
        None
    }

    fn next(&mut self, current_query: &str) -> Option<String> {
        let selected = self.validate_cursor(current_query)?;
        let prefix = self.prefix.clone().unwrap_or_default();
        let history = self.history.as_ref()?;

        for index in (selected + 1)..history.len() {
            if history
                .get(index)
                .is_some_and(|entry| entry.starts_with(&prefix))
            {
                self.cursor = Some(index);
                return history.get(index).cloned();
            }
        }
        None
    }

    fn reset_cursor(&mut self) {
        self.cursor = None;
        self.prefix = None;
    }

    fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }
}

/// A pending asynchronous command palette update.
pub struct PendingCommandPaletteUpdate {
    generation: UpdateGeneration,
    resolved_query: String,
    commands: Vec<PaletteCommand>,
    hit_counts: HashMap<String, u16>,
    interceptor: Option<Task<CommandInterceptResult>>,
}

impl PendingCommandPaletteUpdate {
    /// Returns the generation assigned to this update.
    pub fn generation(&self) -> UpdateGeneration {
        self.generation
    }

    /// Returns the query after exact alias resolution.
    pub fn resolved_query(&self) -> &str {
        &self.resolved_query
    }

    /// Adds the host's interceptor task to this update.
    pub fn with_interceptor(mut self, interceptor: Option<Task<CommandInterceptResult>>) -> Self {
        self.interceptor = interceptor;
        self
    }

    /// Computes fuzzy and intercepted matches for this update.
    pub async fn compute(mut self, executor: BackgroundExecutor) -> CommandPaletteUpdate {
        self.commands.sort_by_key(|command| {
            (
                Reverse(self.hit_counts.get(&command.name).copied()),
                command.name.clone(),
            )
        });

        let candidates = self
            .commands
            .iter()
            .enumerate()
            .map(|(index, command)| StringMatchCandidate::new(index, &command.name))
            .collect::<Vec<_>>();
        let normalized_query = normalize_action_query(&self.resolved_query);
        let matches = fuzzy_nucleo::match_strings_async(
            &candidates,
            &normalized_query,
            fuzzy_nucleo::Case::Smart,
            fuzzy_nucleo::LengthPenalty::On,
            10_000,
            &Default::default(),
            executor,
        )
        .await;
        let intercept_result = if let Some(interceptor) = self.interceptor {
            interceptor.await
        } else {
            CommandInterceptResult::default()
        };

        CommandPaletteUpdate {
            generation: self.generation,
            resolved_query: self.resolved_query,
            commands: self.commands,
            matches,
            intercept_result,
        }
    }
}

/// The computed result of a pending command palette update.
pub struct CommandPaletteUpdate {
    generation: UpdateGeneration,
    resolved_query: String,
    commands: Vec<PaletteCommand>,
    matches: Vec<StringMatch>,
    intercept_result: CommandInterceptResult,
}

impl CommandPaletteUpdate {
    /// Returns the generation that produced this update.
    pub fn generation(&self) -> UpdateGeneration {
        self.generation
    }
}

/// Host-neutral state and behavior for a command palette session.
pub struct CommandPaletteSession {
    latest_query: String,
    all_commands: Vec<PaletteCommand>,
    commands: Vec<PaletteCommand>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    generation: UpdateGeneration,
    query_history: QueryHistory,
}

impl CommandPaletteSession {
    /// Creates a session containing the given commands.
    pub fn new(commands: Vec<PaletteCommand>) -> Self {
        Self {
            latest_query: String::new(),
            all_commands: commands.clone(),
            commands,
            matches: Vec::new(),
            selected_index: 0,
            generation: UpdateGeneration(0),
            query_history: QueryHistory::default(),
        }
    }

    /// Starts a new match update and resolves an exact query alias.
    pub fn begin_update(
        &mut self,
        query: String,
        resolve_alias: impl FnOnce(&str) -> Option<String>,
        hit_counts: HashMap<String, u16>,
    ) -> PendingCommandPaletteUpdate {
        self.generation = UpdateGeneration(self.generation.0.wrapping_add(1));
        let resolved_query = resolve_alias(&query).unwrap_or(query);

        PendingCommandPaletteUpdate {
            generation: self.generation,
            resolved_query,
            commands: self.all_commands.clone(),
            hit_counts,
            interceptor: None,
        }
    }

    /// Applies a computed update if it belongs to the current generation.
    ///
    /// Returns `false` without changing the session when the update is stale.
    pub fn apply_update(&mut self, update: CommandPaletteUpdate) -> bool {
        if update.generation != self.generation {
            return false;
        }

        let CommandPaletteUpdate {
            generation: _,
            resolved_query,
            mut commands,
            mut matches,
            intercept_result,
        } = update;
        self.latest_query = resolved_query;

        let mut merged_matches = Vec::new();
        for CommandInterceptItem {
            action,
            string,
            positions,
        } in intercept_result.results
        {
            if let Some(index) = matches.iter().position(|candidate| {
                commands
                    .get(candidate.candidate_id)
                    .is_some_and(|command| command.action.partial_eq(&*action))
            }) {
                matches.remove(index);
            }
            commands.push(PaletteCommand::new(string.clone(), action));
            merged_matches.push(StringMatch {
                candidate_id: commands.len() - 1,
                string: string.into(),
                positions,
                score: 0.0,
            });
        }
        if !intercept_result.exclusive {
            merged_matches.append(&mut matches);
        }

        self.commands = commands;
        self.matches = merged_matches;
        if self.matches.is_empty() {
            self.selected_index = 0;
        } else {
            self.selected_index = cmp::min(self.selected_index, self.matches.len() - 1);
        }
        true
    }

    /// Returns all currently computed commands.
    pub fn commands(&self) -> &[PaletteCommand] {
        &self.commands
    }

    /// Returns all currently computed matches.
    pub fn matches(&self) -> &[StringMatch] {
        &self.matches
    }

    /// Returns the number of current matches.
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Returns the selected match index.
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Sets the selected match index.
    pub fn set_selected_index(&mut self, index: usize) {
        self.selected_index = index;
    }

    /// Returns the selected command, if any.
    pub fn selected_command(&self) -> Option<&PaletteCommand> {
        if self.matches.is_empty() {
            return None;
        }
        let action_index = self
            .matches
            .get(self.selected_index)
            .map(|matched| matched.candidate_id)
            .unwrap_or(self.selected_index);
        self.commands.get(action_index)
    }

    /// Traverses query history with the command palette's prefix semantics.
    pub fn select_history(
        &mut self,
        direction: HistoryDirection,
        current_query: &str,
        load_history: impl FnOnce() -> Vec<String>,
    ) -> Option<String> {
        match direction {
            HistoryDirection::Previous => {
                let should_use_history =
                    self.selected_index == 0 || self.query_history.is_navigating();
                if should_use_history {
                    self.query_history.ensure_loaded(load_history);
                    self.query_history.previous(current_query)
                } else {
                    None
                }
            }
            HistoryDirection::Next => {
                if !self.query_history.is_navigating() {
                    return None;
                }
                if let Some(query) = self.query_history.next(current_query) {
                    Some(query)
                } else {
                    let prefix = self.query_history.prefix.take().unwrap_or_default();
                    self.query_history.reset_cursor();
                    Some(prefix)
                }
            }
        }
    }

    /// Replaces the session's loaded query history, from oldest to newest.
    pub fn set_history(&mut self, history: impl IntoIterator<Item = String>) {
        self.query_history.history = Some(history.into_iter().collect());
        self.query_history.reset_cursor();
    }

    /// Confirms and removes the selected command from the session.
    pub fn confirm_selected(
        &mut self,
        load_history: impl FnOnce() -> Vec<String>,
    ) -> Option<ConfirmedCommand> {
        if self.matches.is_empty() {
            return None;
        }

        if !self.latest_query.is_empty() {
            self.query_history.ensure_loaded(load_history);
            self.query_history.add(self.latest_query.clone());
            self.query_history.reset_cursor();
        }

        let action_index = self.matches.get(self.selected_index)?.candidate_id;
        if action_index >= self.commands.len() {
            return None;
        }
        let command = self.commands.swap_remove(action_index);
        self.matches.clear();
        self.commands.clear();

        Some(ConfirmedCommand {
            name: command.name,
            resolved_query: self.latest_query.clone(),
            action: command.action,
        })
    }
}

/// Removes repeated whitespace and colons and converts underscores to spaces.
pub fn normalize_action_query(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut last_char = None;

    for char in input.trim().chars() {
        let normalized_char = if char == '_' { ' ' } else { char };
        match (last_char, normalized_char) {
            (Some(':'), ':') => continue,
            (Some(last_char), current) if last_char.is_whitespace() && current.is_whitespace() => {
                continue;
            }
            _ => last_char = Some(normalized_char),
        }
        result.push(normalized_char);
    }

    result
}

/// Converts a GPUI action name into the human-readable label used by Zed's
/// command palette.
pub fn humanize_action_name(name: &str) -> String {
    let chars = name.chars().collect::<Vec<_>>();
    let capacity = name.len()
        + chars
            .iter()
            .filter(|character| character.is_uppercase())
            .count();
    let mut result = String::with_capacity(capacity);
    let mut index = 0;

    while index < chars.len() {
        let character = chars[index];
        if character == ':' {
            if result.ends_with(':') {
                result.push(' ');
            } else {
                result.push(':');
            }
            index += 1;
        } else if character == '_' {
            result.push(' ');
            index += 1;
        } else if character.is_uppercase() {
            let start = index;
            index += 1;
            while chars
                .get(index)
                .is_some_and(|next_character| next_character.is_uppercase())
            {
                index += 1;
            }

            let uppercase_run = &chars[start..index];
            if uppercase_run.len() > 1 {
                let split_before_last = chars
                    .get(index)
                    .is_some_and(|next_character| next_character.is_lowercase());
                let acronym_end = if split_before_last {
                    uppercase_run.len() - 1
                } else {
                    uppercase_run.len()
                };

                if acronym_end > 0 {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(&uppercase_run[..acronym_end]);
                }

                if split_before_last {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(uppercase_run[acronym_end].to_lowercase());
                }
            } else {
                if !result.ends_with(' ') {
                    result.push(' ');
                }
                result.extend(character.to_lowercase());
            }
        } else {
            result.push(character);
            index += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use gpui::{TestAppContext, actions};

    actions!(command_palette_core_test, [Alpha, Beta, Gamma]);

    fn commands() -> Vec<PaletteCommand> {
        vec![
            PaletteCommand::new("alpha".to_owned(), Alpha.boxed_clone()),
            PaletteCommand::new("beta".to_owned(), Beta.boxed_clone()),
            PaletteCommand::new("gamma".to_owned(), Gamma.boxed_clone()),
        ]
    }

    #[test]
    fn action_names_use_zeds_palette_labels() {
        assert_eq!(
            humanize_action_name("editor::GoToDefinition"),
            "editor: go to definition"
        );
        assert_eq!(
            humanize_action_name("go_to_line::Deploy"),
            "go to line: deploy"
        );
        assert_eq!(
            humanize_action_name("agent::OpenGlobalAGENTS.mdRules"),
            "agent: open global AGENTS.md rules"
        );
        assert_eq!(humanize_action_name("editor::OpenURL"), "editor: open URL");
        assert_eq!(
            humanize_action_name("editor::OpenURLParser"),
            "editor: open URL parser"
        );
    }

    fn history() -> Vec<String> {
        ["open file", "select all", "select line", "backspace"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn normalizes_action_queries_exactly() {
        assert_eq!(normalize_action_query(" editor::  Go_To "), "editor: Go To");
        assert_eq!(
            normalize_action_query("terminal_panel::Toggle"),
            "terminal panel:Toggle"
        );
    }

    #[test]
    fn history_traverses_matching_prefix_and_returns_to_original_query() {
        let mut session = CommandPaletteSession::new(Vec::new());

        assert_eq!(
            session.select_history(HistoryDirection::Previous, "sel", history),
            Some("select line".to_owned())
        );
        assert_eq!(
            session.select_history(HistoryDirection::Previous, "select line", Vec::new),
            Some("select all".to_owned())
        );
        assert_eq!(
            session.select_history(HistoryDirection::Previous, "select all", Vec::new),
            None
        );
        assert_eq!(
            session.select_history(HistoryDirection::Next, "select all", Vec::new),
            Some("select line".to_owned())
        );
        assert_eq!(
            session.select_history(HistoryDirection::Next, "select line", Vec::new),
            Some("sel".to_owned())
        );
    }

    #[test]
    fn history_is_not_entered_while_a_nonfirst_match_is_selected() {
        let mut session = CommandPaletteSession::new(Vec::new());
        session.set_selected_index(1);
        let loaded = Cell::new(false);

        assert_eq!(
            session.select_history(HistoryDirection::Previous, "", || {
                loaded.set(true);
                history()
            }),
            None
        );
        assert!(!loaded.get());
    }

    #[gpui::test]
    async fn aliases_usage_order_and_confirmation_use_resolved_query(cx: &mut TestAppContext) {
        let mut session = CommandPaletteSession::new(commands());
        let mut hit_counts = HashMap::new();
        hit_counts.insert("beta".to_owned(), 4);
        let pending = session.begin_update(
            "favorite".to_owned(),
            |query| (query == "favorite").then(|| "beta".to_owned()),
            hit_counts,
        );
        assert_eq!(pending.resolved_query(), "beta");

        let update = pending.compute(cx.background_executor.clone()).await;
        assert!(session.apply_update(update));
        assert_eq!(session.selected_command().unwrap().name, "beta");

        let confirmed = session.confirm_selected(Vec::new).unwrap();
        assert_eq!(confirmed.name, "beta");
        assert_eq!(confirmed.resolved_query, "beta");
        assert_eq!(
            session.select_history(HistoryDirection::Previous, "", Vec::new),
            Some("beta".to_owned())
        );
    }

    #[gpui::test]
    async fn usage_counts_order_empty_query_matches(cx: &mut TestAppContext) {
        let mut session = CommandPaletteSession::new(commands());
        let mut hit_counts = HashMap::new();
        hit_counts.insert("gamma".to_owned(), 2);
        hit_counts.insert("beta".to_owned(), 5);
        let pending = session.begin_update("".to_owned(), |_| None, hit_counts);

        let update = pending.compute(cx.background_executor.clone()).await;
        assert!(session.apply_update(update));
        let names = session
            .matches()
            .iter()
            .map(|matched| session.commands()[matched.candidate_id].name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["beta", "gamma", "alpha"]);
    }

    #[gpui::test]
    async fn interceptors_prepend_deduplicate_and_can_be_exclusive(cx: &mut TestAppContext) {
        let mut session = CommandPaletteSession::new(commands());
        let intercepted = CommandInterceptResult {
            results: vec![CommandInterceptItem {
                action: Beta.boxed_clone(),
                string: ":beta".to_owned(),
                positions: vec![1, 2],
            }],
            exclusive: false,
        };
        let pending = session
            .begin_update("".to_owned(), |_| None, HashMap::new())
            .with_interceptor(Some(Task::ready(intercepted)));
        let update = pending.compute(cx.background_executor.clone()).await;

        assert!(session.apply_update(update));
        let names = session
            .matches()
            .iter()
            .map(|matched| session.commands()[matched.candidate_id].name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, [":beta", "alpha", "gamma"]);
        assert_eq!(session.matches()[0].positions, [1, 2]);

        let exclusive = CommandInterceptResult {
            results: vec![CommandInterceptItem {
                action: Alpha.boxed_clone(),
                string: ":alpha".to_owned(),
                positions: Vec::new(),
            }],
            exclusive: true,
        };
        let pending = session
            .begin_update("".to_owned(), |_| None, HashMap::new())
            .with_interceptor(Some(Task::ready(exclusive)));
        let update = pending.compute(cx.background_executor.clone()).await;

        assert!(session.apply_update(update));
        assert_eq!(session.match_count(), 1);
        assert_eq!(session.selected_command().unwrap().name, ":alpha");
    }

    #[gpui::test]
    async fn stale_generations_are_ignored_and_selection_is_clamped(cx: &mut TestAppContext) {
        let mut session = CommandPaletteSession::new(commands());
        session.set_selected_index(99);
        let stale = session.begin_update("alpha".to_owned(), |_| None, HashMap::new());
        let current = session.begin_update("gamma".to_owned(), |_| None, HashMap::new());

        let stale = stale.compute(cx.background_executor.clone()).await;
        assert!(!session.apply_update(stale));
        assert_eq!(session.selected_index(), 99);

        let current = current.compute(cx.background_executor.clone()).await;
        assert!(session.apply_update(current));
        assert_eq!(session.match_count(), 1);
        assert_eq!(session.selected_index(), 0);
        assert_eq!(session.selected_command().unwrap().name, "gamma");
    }

    #[test]
    fn malformed_candidate_ids_are_rejected_without_panicking() {
        let mut session = CommandPaletteSession::new(commands());
        let pending = session.begin_update("alpha".to_owned(), |_| None, HashMap::new());
        let update = CommandPaletteUpdate {
            generation: pending.generation(),
            resolved_query: "alpha".to_owned(),
            commands: commands(),
            matches: vec![StringMatch {
                candidate_id: usize::MAX,
                score: 0.0,
                positions: Vec::new(),
                string: "malformed".into(),
            }],
            intercept_result: CommandInterceptResult::default(),
        };

        assert!(session.apply_update(update));
        assert!(session.selected_command().is_none());
        assert!(session.confirm_selected(Vec::new).is_none());
    }
}
