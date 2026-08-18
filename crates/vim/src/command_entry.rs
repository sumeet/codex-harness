use crate::Vim;
use editor::Editor;
use gpui::{Action, App, Context, Window, actions};
use zed_actions::command_palette::OpenWithQuery;

actions!(
    vim,
    [
        /// Opens command entry with the current visual range.
        VisualCommand,
        /// Opens command entry with the current count as a line range.
        CountCommand,
        /// Opens shell command entry with the current visual range.
        ShellCommand
    ]
);

fn count_command_query(count: usize) -> String {
    if count > 1 {
        format!(".,.+{}", count.saturating_sub(1))
    } else {
        ".".into()
    }
}

fn open_with_query(query: impl Into<String>, window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        OpenWithQuery {
            query: query.into(),
        }
        .boxed_clone(),
        cx,
    );
}

pub(crate) fn register(editor: &mut Editor, cx: &mut Context<Vim>) {
    Vim::action(editor, cx, |_, _: &VisualCommand, window, cx| {
        open_with_query("'<,'>", window, cx);
    });
    Vim::action(editor, cx, |_, _: &ShellCommand, window, cx| {
        open_with_query("'<,'>!", window, cx);
    });
    Vim::action(editor, cx, |_, _: &CountCommand, window, cx| {
        let count = Vim::take_count(cx).unwrap_or(1);
        Vim::take_forced_motion(cx);
        open_with_query(count_command_query(count), window, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_ranges_match_vim_command_entry() {
        assert_eq!(count_command_query(0), ".");
        assert_eq!(count_command_query(1), ".");
        assert_eq!(count_command_query(2), ".,.+1");
        assert_eq!(count_command_query(12), ".,.+11");
    }
}
