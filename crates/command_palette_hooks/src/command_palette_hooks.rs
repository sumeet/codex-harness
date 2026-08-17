//! Provides hooks for customizing the behavior of the command palette.

#![deny(missing_docs)]

use std::{any::TypeId, rc::Rc};

use collections::{HashSet, TypeIdHashSet};
use derive_more::{Deref, DerefMut};
use gpui::{Action, App, BorrowAppContext, Global, Task};

/// Initializes the command palette hooks.
pub fn init(cx: &mut App) {
    cx.set_global(GlobalCommandPaletteFilter::default());
}

/// A filter for the command palette.
#[derive(Default)]
pub struct CommandPaletteFilter {
    hidden_namespaces: HashSet<&'static str>,
    hidden_action_types: TypeIdHashSet,
    /// Actions that have explicitly been shown. These should be shown even if
    /// they are in a hidden namespace.
    shown_action_types: TypeIdHashSet,
}

#[derive(Deref, DerefMut, Default)]
struct GlobalCommandPaletteFilter(CommandPaletteFilter);

impl Global for GlobalCommandPaletteFilter {}

impl CommandPaletteFilter {
    /// Returns the global [`CommandPaletteFilter`], if one is set.
    pub fn try_global(cx: &App) -> Option<&CommandPaletteFilter> {
        cx.try_global::<GlobalCommandPaletteFilter>()
            .map(|filter| &filter.0)
    }

    /// Returns a mutable reference to the global [`CommandPaletteFilter`].
    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<GlobalCommandPaletteFilter>()
    }

    /// Updates the global [`CommandPaletteFilter`] using the given closure.
    pub fn update_global<F>(cx: &mut App, update: F)
    where
        F: FnOnce(&mut Self, &mut App),
    {
        if cx.has_global::<GlobalCommandPaletteFilter>() {
            cx.update_global(|this: &mut GlobalCommandPaletteFilter, cx| update(&mut this.0, cx))
        }
    }

    /// Returns whether the given [`Action`] is hidden by the filter.
    pub fn is_hidden(&self, action: &dyn Action) -> bool {
        let name = action.name();
        let namespace = name.split("::").next().unwrap_or("malformed action name");

        // If this action has specifically been shown then it should be visible.
        if self.shown_action_types.contains(&action.type_id()) {
            return false;
        }

        self.hidden_namespaces.contains(namespace)
            || self.hidden_action_types.contains(&action.type_id())
    }

    /// Hides all actions in the given namespace.
    pub fn hide_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.insert(namespace);
    }

    /// Shows all actions in the given namespace.
    pub fn show_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.remove(namespace);
    }

    /// Hides all actions with the given types.
    pub fn hide_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.hidden_action_types.insert(*action_type);
            self.shown_action_types.remove(action_type);
        }
    }

    /// Shows all actions with the given types.
    pub fn show_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.shown_action_types.insert(*action_type);
            self.hidden_action_types.remove(action_type);
        }
    }
}

/// The result of intercepting a command palette command.
#[derive(Debug)]
pub struct CommandInterceptItem {
    /// The action produced as a result of the interception.
    pub action: Box<dyn Action>,
    /// The display string to show in the command palette for this result.
    pub string: String,
    /// The character positions in the string that match the query.
    /// Used for highlighting matched characters in the command palette UI.
    pub positions: Vec<usize>,
}

/// The result of intercepting a command palette command.
#[derive(Default, Debug)]
pub struct CommandInterceptResult {
    /// The items
    pub results: Vec<CommandInterceptItem>,
    /// Whether or not to continue to show the normal matches
    pub exclusive: bool,
}

/// Provides asynchronous filename completions for a command palette invocation.
#[derive(Clone)]
pub struct FilenameCompletionProvider(Rc<dyn Fn(&str, &mut App) -> Task<Vec<String>>>);

impl FilenameCompletionProvider {
    /// Creates a filename completion provider.
    pub fn new(provider: impl Fn(&str, &mut App) -> Task<Vec<String>> + 'static) -> Self {
        Self(Rc::new(provider))
    }

    fn complete(&self, query: &str, cx: &mut App) -> Task<Vec<String>> {
        (self.0)(query, cx)
    }
}

/// Host-provided services available while intercepting a command palette query.
///
/// The context is created for each command palette invocation so interceptors do
/// not need to depend on a particular application host, workspace, or project.
#[derive(Clone, Default)]
pub struct CommandPaletteInvocationContext {
    filename_completion_provider: Option<FilenameCompletionProvider>,
}

impl CommandPaletteInvocationContext {
    /// Adds a filename completion provider to this invocation.
    pub fn with_filename_completion_provider(
        mut self,
        provider: FilenameCompletionProvider,
    ) -> Self {
        self.filename_completion_provider = Some(provider);
        self
    }

    /// Returns asynchronous filename completions for the given query.
    ///
    /// Hosts that do not provide filename completion return no candidates.
    pub fn complete_filename(&self, query: &str, cx: &mut App) -> Task<Vec<String>> {
        self.filename_completion_provider
            .as_ref()
            .map(|provider| provider.complete(query, cx))
            .unwrap_or_else(|| Task::ready(Vec::new()))
    }
}

/// An interceptor for the command palette.
#[derive(Clone)]
pub struct GlobalCommandPaletteInterceptor(
    Rc<dyn Fn(&str, &CommandPaletteInvocationContext, &mut App) -> Task<CommandInterceptResult>>,
);

impl Global for GlobalCommandPaletteInterceptor {}

impl GlobalCommandPaletteInterceptor {
    /// Sets the global interceptor.
    ///
    /// This will override the previous interceptor, if it exists.
    pub fn set(
        cx: &mut App,
        interceptor: impl Fn(
            &str,
            &CommandPaletteInvocationContext,
            &mut App,
        ) -> Task<CommandInterceptResult>
        + 'static,
    ) {
        cx.set_global(Self(Rc::new(interceptor)));
    }

    /// Clears the global interceptor.
    pub fn clear(cx: &mut App) {
        if cx.has_global::<Self>() {
            cx.remove_global::<Self>();
        }
    }

    /// Intercepts the given query from the command palette.
    pub fn intercept(
        query: &str,
        invocation_context: &CommandPaletteInvocationContext,
        cx: &mut App,
    ) -> Option<Task<CommandInterceptResult>> {
        let interceptor = cx.try_global::<Self>()?;
        let handler = interceptor.0.clone();
        Some(handler(query, invocation_context, cx))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    async fn invocation_context_uses_host_filename_provider(cx: &mut TestAppContext) {
        let queries = Rc::new(RefCell::new(Vec::new()));
        let provider_queries = queries.clone();
        let context = CommandPaletteInvocationContext::default().with_filename_completion_provider(
            FilenameCompletionProvider::new(move |query, _| {
                provider_queries.borrow_mut().push(query.to_owned());
                Task::ready(vec![format!("{query}.rs")])
            }),
        );

        let completions = cx
            .update(|cx| context.complete_filename("src/main", cx))
            .await;

        assert_eq!(&*queries.borrow(), &["src/main".to_owned()]);
        assert_eq!(completions, ["src/main.rs".to_owned()]);
    }

    #[gpui::test]
    async fn invocation_context_without_provider_has_no_filename_completions(
        cx: &mut TestAppContext,
    ) {
        let context = CommandPaletteInvocationContext::default();
        let completions = cx
            .update(|cx| context.complete_filename("src/main", cx))
            .await;

        assert!(completions.is_empty());
    }
}
