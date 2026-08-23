use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    path::Path,
    rc::Rc,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use assets::Assets;
use codex_app_server_client::{Client, CodexThread, Event as AppServerEvent};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, FollowMode,
    IntoElement, KeyBinding, KeyContext, Keystroke, ListAlignment, ListSizingBehavior, ListState,
    Render, ScrollHandle, SharedString, StyledText, Task, UpdateGlobal, WeakEntity, Window,
    WindowBounds, WindowOptions, actions, canvas, deferred, div, list, point, prelude::*, px,
    relative, size,
};
use gpui_platform::application;
use harness_editor::{
    LocalEditor, LocalEditorChanged, ModeIndicator, TranscriptEditor, TranscriptReplacement,
    TranscriptSelectionChanged, TranscriptSelectionSnapshot, TranscriptSupplement,
    TranscriptTypographyProfile, VimNextMatch, VimPreviousMatch, VimSearch, VimWordNext,
    VimWordPrevious, shell_capture_priority, shell_capture_ranges,
};
use harness_protocol as model;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use model::{TranscriptItem, TranscriptModel, minimal_text_edit};
use serde_json::{Value, json};
use settings::SettingsStore;
use ui::prelude::{ActiveTheme, StyledTypography};
use ui::{
    AgentThreadStatus, Button, ButtonCommon, ButtonSize, ButtonStyle, Clickable, Color,
    ContextMenu, ContextMenuEntry, DiffStat, Disableable, Disclosure, Icon, IconButton,
    IconButtonShape, IconName, IconSize, Label, LabelCommon, LabelSize, ListItem, ListItemSpacing,
    ScrollAxes, Scrollbars, SelectableButton, ThreadItem, TintColor, Toggleable, WithScrollbar,
    right_click_menu,
};

mod image_surface;
mod palette;
mod performance;
mod request_surface;

use image_surface::{
    ImageSurface, SurfaceSyncDecision as ImageSurfaceSyncDecision,
    keys_to_sync as image_surface_keys_to_sync, supplement_key as image_supplement_key,
    surface_sync_decision as image_surface_sync_decision,
};
use palette::{PaletteEvent, PaletteOverlay};
use performance::PerformanceReporter;
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
        ToggleOutput,
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
        UseBufferTypography,
        UseReadingTypography,
        CopyPerformanceReport,
        RunPerformanceBenchmark,
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
const READ_ONLY_ACTIVE_REFRESH: Duration = Duration::from_millis(900);
const READ_ONLY_IDLE_REFRESH: Duration = Duration::from_secs(5);
const MAX_RECONNECT_ATTEMPTS: u8 = 3;
const STRUCTURED_OUTPUT_PREVIEW_LINES: usize = 10;
const STRUCTURED_OUTPUT_PREVIEW_BYTES: usize = 1_200;
const COMMAND_PREVIEW_LINES: usize = 4;
const COMMAND_PREVIEW_BYTES: usize = 800;
#[cfg(test)]
const WEB_RESULT_PREVIEW_COUNT: usize = 3;
const PROGRESSIVE_OUTPUT_MEDIUM_LINES: usize = 100;
const PROGRESSIVE_OUTPUT_MEDIUM_BYTES: usize = 16 * 1_024;
const PROGRESSIVE_OUTPUT_LARGE_LINES: usize = 500;
const PROGRESSIVE_OUTPUT_LARGE_BYTES: usize = 64 * 1_024;
const RICH_SEARCH_HIGHLIGHT_LIMIT: usize = 128;
const RICH_NESTED_COMMAND_MAX_HEIGHT: f32 = 140.;
const RICH_NESTED_OUTPUT_MAX_HEIGHT: f32 = 280.;
const RICH_COMMAND_ROW_HEIGHT_HINT: f32 = 20.;
const PERFORMANCE_J_STEPS: u16 = 240;
const PERFORMANCE_STATUS_DURATION: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PerformanceJPhase {
    Prepare,
    Baseline,
    Dispatch { remaining: u16 },
    Report,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PerformanceJStep {
    Prepare,
    Baseline,
    Dispatch { down: bool },
    Report,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PerformanceJRunState {
    generation: u64,
    phase: PerformanceJPhase,
}

impl PerformanceJRunState {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            phase: PerformanceJPhase::Prepare,
        }
    }

    fn next_step(&mut self, generation: u64) -> Option<PerformanceJStep> {
        if self.generation != generation {
            return None;
        }
        match self.phase {
            PerformanceJPhase::Prepare => {
                self.phase = PerformanceJPhase::Baseline;
                Some(PerformanceJStep::Prepare)
            }
            PerformanceJPhase::Baseline => {
                self.phase = PerformanceJPhase::Dispatch {
                    remaining: PERFORMANCE_J_STEPS,
                };
                Some(PerformanceJStep::Baseline)
            }
            PerformanceJPhase::Dispatch {
                remaining: remaining @ 2..=u16::MAX,
            } => {
                self.phase = PerformanceJPhase::Dispatch {
                    remaining: remaining - 1,
                };
                Some(PerformanceJStep::Dispatch {
                    down: remaining % 2 == 0,
                })
            }
            PerformanceJPhase::Dispatch { remaining: 1 } => {
                self.phase = PerformanceJPhase::Report;
                Some(PerformanceJStep::Dispatch { down: false })
            }
            PerformanceJPhase::Dispatch { remaining: 0 } | PerformanceJPhase::Report => {
                self.phase = PerformanceJPhase::Complete;
                Some(PerformanceJStep::Report)
            }
            PerformanceJPhase::Complete => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PerformanceJDriver {
    state: PerformanceJRunState,
    j: Keystroke,
    k: Keystroke,
    pending_motion_origin: Option<usize>,
}

impl PerformanceJDriver {
    fn new(generation: u64, j: Keystroke, k: Keystroke) -> Self {
        Self {
            state: PerformanceJRunState::new(generation),
            j,
            k,
            pending_motion_origin: None,
        }
    }
}

fn performance_j_has_room(logical_lines: usize) -> bool {
    logical_lines > 1
}

fn performance_j_candidate(model: &TranscriptModel) -> Option<usize> {
    model.items.iter().enumerate().find_map(|(index, item)| {
        (matches!(
            item.kind,
            model::TranscriptKind::User
                | model::TranscriptKind::Agent
                | model::TranscriptKind::Plan
        ) && item.expanded
            && item.status.as_deref() != Some("streaming")
            && item.pending_request.is_none())
        .then(|| model.rich_navigation_item_projection(index))
        .flatten()
        .filter(|projection| performance_j_has_room(projection.body_text().lines().count()))
        .map(|_| index)
    })
}

#[derive(Debug, Eq, PartialEq)]
struct StructuredOutputPreview {
    content: String,
    footer: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(dead_code)]
enum OutputExpansion {
    #[default]
    Preview,
    Medium,
    Large,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutputLimits {
    lines: usize,
    bytes: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct SearchContextSnippet {
    text: String,
    match_range: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchTextRanges {
    ranges: Vec<Range<usize>>,
    active: Option<usize>,
}

#[derive(Clone)]
struct RichSearchPaint {
    query: SharedString,
    active_item: bool,
    active_claimed: Rc<Cell<bool>>,
    remaining_ranges: Rc<Cell<usize>>,
}

/// Item-local projection of the real Editor/Vim selection used by Rich
/// renderers. `body_text` is the exact logical text on which Vim operated;
/// renderers only translate its byte ranges into their visual fragments.
#[derive(Clone, Debug)]
struct RichNavigationPaint {
    body_text: Arc<str>,
    ranges: Vec<Range<usize>>,
    head: Option<usize>,
    visual: bool,
    cursor_claimed: Rc<Cell<bool>>,
}

#[derive(Clone)]
struct RichNestedScrollBinding {
    handle: ScrollHandle,
    reveal_cursor: bool,
}

#[derive(Default)]
struct RichNestedScrollState {
    handle: ScrollHandle,
    last_cursor: Option<usize>,
    command: Option<RichCommandSurface>,
}

#[derive(Clone, Copy)]
enum RichCommandSource {
    Command,
    Output,
}

#[derive(Clone)]
struct RichCommandRow {
    source: RichCommandSource,
    source_range: Range<usize>,
    line_index: usize,
}

#[derive(Clone)]
struct RichCommandData {
    command: Arc<str>,
    output: Arc<str>,
    rows: Arc<[RichCommandRow]>,
    command_row_count: usize,
}

struct RichCommandSurface {
    event_count: usize,
    content_len: usize,
    data: RichCommandData,
    command_list_state: ListState,
    output_list_state: ListState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RichMarkdownNavigationPaint {
    selections: Vec<Range<usize>>,
    cursor: Option<usize>,
}

impl RichNavigationPaint {
    fn cursor_range(&self) -> Option<Range<usize>> {
        let mut head = self.head?.min(self.body_text.len());
        while !self.body_text.is_char_boundary(head) {
            head = head.saturating_sub(1);
        }
        if head < self.body_text.len() {
            let character = self.body_text[head..].chars().next()?;
            if matches!(character, '\r' | '\n') {
                if let Some((offset, character)) = self.body_text[head..]
                    .char_indices()
                    .find(|(_, character)| !matches!(character, '\r' | '\n'))
                {
                    let start = head + offset;
                    return Some(start..start + character.len_utf8());
                }
                let (start, character) = self.body_text[..head]
                    .char_indices()
                    .rev()
                    .find(|(_, character)| !matches!(character, '\r' | '\n'))?;
                return Some(start..start + character.len_utf8());
            }
            let end = self.body_text[head..]
                .char_indices()
                .nth(1)
                .map_or(self.body_text.len(), |(offset, _)| head + offset);
            Some(head..end)
        } else if head > 0 {
            let (start, character) = self.body_text[..head]
                .char_indices()
                .rev()
                .find(|(_, character)| !matches!(character, '\r' | '\n'))?;
            Some(start..start + character.len_utf8())
        } else {
            None
        }
    }

    fn markdown_source_navigation(&self, source: &str) -> RichMarkdownNavigationPaint {
        let selections = self
            .visual
            .then(|| self.ranges.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|range| {
                markdown_source_offset_for_logical(&self.body_text, source, range.start)
                    ..markdown_source_offset_for_logical(&self.body_text, source, range.end)
            })
            .filter(|range| range.start < range.end)
            .collect();
        let cursor = (!self.visual)
            .then(|| {
                self.cursor_range()
                    .map(|range| range.start)
                    .unwrap_or_default()
            })
            .map(|logical_offset| {
                markdown_source_offset_for_logical(&self.body_text, source, logical_offset)
            });
        if cursor.is_some() {
            self.cursor_claimed.set(true);
        }
        RichMarkdownNavigationPaint { selections, cursor }
    }
}

/// Give hidden Rich navigation a stable proxy in the card header. Collapsed
/// folds and non-text request/image surfaces show their full title for a
/// Visual selection; a Normal cursor owns exactly its first glyph.
fn rich_header_navigation_range(
    title: &str,
    navigation: Option<&RichNavigationPaint>,
    proxy_body: bool,
) -> Option<Range<usize>> {
    let navigation = navigation?;
    if navigation.visual {
        return (proxy_body && !navigation.ranges.is_empty()).then(|| 0..title.len());
    }
    if !proxy_body || navigation.head.is_none() || navigation.cursor_claimed.get() {
        return None;
    }
    let end = title.chars().next().map(char::len_utf8)?;
    navigation.cursor_claimed.set(true);
    Some(0..end)
}

/// Locate a visible Rich fragment in the logical transcript body, preserving
/// order when the same text occurs more than once. Renderers use this small
/// adapter instead of knowing anything about Editor-global offsets.
fn rich_navigation_fragment_range(
    navigation: Option<&RichNavigationPaint>,
    fragment: &str,
    logical_cursor: &mut usize,
) -> Range<usize> {
    let start = if let Some(navigation) = navigation {
        let search_start = (*logical_cursor).min(navigation.body_text.len());
        let Some(offset) = navigation.body_text[search_start..].find(fragment) else {
            // The Rich renderer may show ornaments synthesized from structured
            // protocol data. If they are absent from the logical document,
            // leave them deliberately unselectable instead of shifting every
            // subsequent fragment onto the wrong Vim bytes.
            return search_start..search_start;
        };
        search_start + offset
    } else {
        *logical_cursor
    };
    let end = start + fragment.len();
    *logical_cursor = end;
    start..end
}

fn logical_line_fragments(text: &str, logical_start: usize) -> Vec<(String, Range<usize>)> {
    let mut offset = logical_start;
    text.split('\n')
        .map(|line| {
            let range = offset..offset + line.len();
            offset = range.end.saturating_add(1);
            (line.to_owned(), range)
        })
        .collect()
}

fn highlights_for_local_fragment(
    highlights: &[(Range<usize>, gpui::HighlightStyle)],
    fragment: Range<usize>,
) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
    highlights
        .iter()
        .filter_map(|(range, style)| {
            let start = range.start.max(fragment.start);
            let end = range.end.min(fragment.end);
            (start < end).then(|| (start - fragment.start..end - fragment.start, style.clone()))
        })
        .collect()
}

fn reveal_rich_nested_cursor(
    binding: Option<&RichNestedScrollBinding>,
    navigation: Option<&RichNavigationPaint>,
    row_ranges: &[Option<Range<usize>>],
) {
    let Some(binding) = binding.filter(|binding| binding.reveal_cursor) else {
        return;
    };
    let Some(cursor) = navigation
        .and_then(RichNavigationPaint::cursor_range)
        .map(|range| range.start)
    else {
        return;
    };
    if let Some(row) = rich_nested_cursor_row(cursor, row_ranges) {
        binding.handle.scroll_to_item(row);
    }
}

fn rich_nested_cursor_row(cursor: usize, row_ranges: &[Option<Range<usize>>]) -> Option<usize> {
    // Structured renderers can omit logical separators and protocol
    // furniture. Match navigation_ranges_for_fragment: prefer the exact row,
    // then reveal the next real glyph when the cursor lands in such a gap.
    let exact = row_ranges.iter().position(|range| {
        range
            .as_ref()
            .is_some_and(|range| range.start <= cursor && cursor < range.end)
    });
    let next = row_ranges.iter().position(|range| {
        range
            .as_ref()
            .is_some_and(|range| !range.is_empty() && cursor <= range.start)
    });
    exact.or(next)
}

/// Map visible Markdown text back to its source by treating formatting marks
/// as skippable source bytes. This deliberately uses the same plain-text
/// projection that Vim sees. It handles ordinary emphasis, links, and inline
/// code without allowing the cursor to disappear inside hidden delimiters.
fn markdown_source_offset_for_logical(logical: &str, source: &str, requested: usize) -> usize {
    let requested = requested.min(logical.len());
    let mut source_cursor = 0;
    for (logical_offset, character) in logical.char_indices() {
        let found = source[source_cursor..]
            .char_indices()
            .find(|(_, source_character)| *source_character == character)
            .map(|(offset, source_character)| {
                source_cursor + offset..source_cursor + offset + source_character.len_utf8()
            });
        if let Some(found) = found {
            if logical_offset >= requested || requested < logical_offset + character.len_utf8() {
                return found.start;
            }
            source_cursor = found.end;
        } else if logical_offset >= requested {
            return source_cursor;
        }
    }
    source_cursor.min(source.len())
}

/// Reverse the paint mapping for mouse placement. A click on a visible glyph
/// becomes an offset in the delimiter-free text owned by the navigation
/// Editor; clicks on hidden Markdown punctuation snap to the next visible
/// glyph instead of placing Vim on a byte the Rich renderer cannot show.
fn markdown_logical_offset_for_source(logical: &str, source: &str, requested: usize) -> usize {
    let requested = requested.min(source.len());
    let mut source_cursor = 0;
    for (logical_offset, character) in logical.char_indices() {
        let Some((offset, source_character)) = source[source_cursor..]
            .char_indices()
            .find(|(_, source_character)| *source_character == character)
        else {
            continue;
        };
        let found_start = source_cursor + offset;
        let found_end = found_start + source_character.len_utf8();
        if requested <= found_start || requested < found_end {
            return logical_offset;
        }
        source_cursor = found_end;
    }
    logical.len()
}

impl RichSearchPaint {
    fn new(query: impl Into<SharedString>, active_item: bool) -> Self {
        Self {
            query: query.into(),
            active_item,
            active_claimed: Rc::new(Cell::new(false)),
            remaining_ranges: Rc::new(Cell::new(RICH_SEARCH_HIGHLIGHT_LIMIT)),
        }
    }

    fn ranges_for(&self, text: &str) -> SearchTextRanges {
        let ranges = folded_match_byte_ranges(text, &self.query, self.remaining_ranges.get());
        self.decorate_ranges(ranges)
    }

    fn decorate_ranges(&self, mut ranges: Vec<Range<usize>>) -> SearchTextRanges {
        let remaining = self.remaining_ranges.get();
        ranges.truncate(remaining);
        self.remaining_ranges
            .set(remaining.saturating_sub(ranges.len()));
        let active =
            (self.active_item && !ranges.is_empty() && !self.active_claimed.get()).then(|| {
                self.active_claimed.set(true);
                0
            });
        SearchTextRanges { ranges, active }
    }
}

fn folded_match_byte_ranges(text: &str, query: &str, limit: usize) -> Vec<Range<usize>> {
    let folded_query = query
        .trim()
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    if folded_query.is_empty() || limit == 0 {
        return Vec::new();
    }

    // Stream folded characters through KMP instead of materializing a second
    // copy of a potentially very large transcript fragment. The short source
    // window maps folded matches back to valid UTF-8 byte boundaries, including
    // lowercase expansions such as `İ` -> `i` + dot.
    let mut prefix = vec![0; folded_query.len()];
    for index in 1..folded_query.len() {
        let mut candidate = prefix[index - 1];
        while candidate > 0 && folded_query[index] != folded_query[candidate] {
            candidate = prefix[candidate - 1];
        }
        if folded_query[index] == folded_query[candidate] {
            candidate += 1;
        }
        prefix[index] = candidate;
    }

    let mut source_window = VecDeque::with_capacity(folded_query.len());
    let mut matched = 0;
    let mut ranges = Vec::with_capacity(limit.min(RICH_SEARCH_HIGHLIGHT_LIMIT));
    'source: for (source_start, character) in text.char_indices() {
        let source_range = source_start..source_start + character.len_utf8();
        for folded_character in character.to_lowercase() {
            while matched > 0 && folded_query[matched] != folded_character {
                matched = prefix[matched - 1];
            }
            if folded_query[matched] == folded_character {
                matched += 1;
            }

            source_window.push_back(source_range.clone());
            if source_window.len() > folded_query.len() {
                source_window.pop_front();
            }
            if matched == folded_query.len() {
                if let (Some(first), Some(last)) = (source_window.front(), source_window.back()) {
                    let range = first.start..last.end;
                    if ranges.last() != Some(&range) {
                        ranges.push(range);
                    }
                }
                // Preserve the existing non-overlapping substring behavior.
                matched = 0;
                source_window.clear();
                if ranges.len() == limit {
                    break 'source;
                }
            }
        }
    }

    ranges
}

fn compose_search_highlights(
    base: Vec<(Range<usize>, gpui::HighlightStyle)>,
    search: &SearchTextRanges,
    passive_background: gpui::Hsla,
    active_background: gpui::Hsla,
) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
    let search_highlights = search.ranges.iter().enumerate().map(|(index, range)| {
        (
            range.clone(),
            gpui::HighlightStyle {
                background_color: Some(if search.active == Some(index) {
                    active_background
                } else {
                    passive_background
                }),
                ..Default::default()
            },
        )
    });
    gpui::combine_highlights(base, search_highlights).collect()
}

fn markdown_search_autoscroll(
    search: &SearchTextRanges,
    generation: Option<u64>,
    last_generation: Option<u64>,
) -> Option<(u64, usize)> {
    generation
        .filter(|generation| last_generation != Some(*generation))
        .and_then(|generation| {
            search
                .active
                .and_then(|active| search.ranges.get(active))
                .map(|range| (generation, range.start))
        })
}

fn searchable_styled_text(
    text: String,
    base: Vec<(Range<usize>, gpui::HighlightStyle)>,
    search: Option<&RichSearchPaint>,
    cx: &App,
) -> StyledText {
    let highlights = if let Some(search) = search {
        compose_search_highlights(
            base,
            &search.ranges_for(&text),
            cx.theme().colors().search_match_background,
            cx.theme().colors().search_active_match_background,
        )
    } else {
        base
    };
    StyledText::new(text).with_highlights(highlights)
}

fn navigation_highlights_for_fragment(
    navigation: Option<&RichNavigationPaint>,
    fragment: Range<usize>,
    cx: &App,
) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
    let Some(navigation) = navigation else {
        return Vec::new();
    };
    let background_color = rich_navigation_text_highlight_background(navigation, cx);
    navigation_ranges_for_fragment(navigation, fragment)
        .into_iter()
        .map(|range| {
            (
                range,
                gpui::HighlightStyle {
                    background_color: Some(background_color),
                    ..Default::default()
                },
            )
        })
        .collect()
}

fn navigation_ranges_for_fragment(
    navigation: &RichNavigationPaint,
    fragment: Range<usize>,
) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut append_intersection = |selection: &Range<usize>| {
        let start = selection.start.max(fragment.start);
        let end = selection.end.min(fragment.end);
        if start < end {
            ranges.push(start - fragment.start..end - fragment.start);
        }
    };
    if navigation.visual {
        for selection in &navigation.ranges {
            append_intersection(selection);
        }
    } else if !navigation.cursor_claimed.get()
        && let Some(cursor) = navigation.cursor_range()
    {
        append_intersection(&cursor);
        if !ranges.is_empty() {
            navigation.cursor_claimed.set(true);
        } else if !fragment.is_empty() && cursor.end <= fragment.start {
            // Structured renderers can omit logical separators or replace
            // protocol furniture with native controls. Snap a cursor in such a
            // gap to the next visible glyph, and claim it exactly once.
            let first_character_end = navigation
                .body_text
                .get(fragment.clone())
                .and_then(|text| text.chars().next())
                .map_or(0, char::len_utf8);
            if first_character_end > 0 {
                ranges.push(0..first_character_end);
                navigation.cursor_claimed.set(true);
            }
        }
    }
    ranges
}

/// Markdown paints its external selection after its glyphs, so it needs a
/// translucent overlay. StyledText paints backgrounds below glyphs and can
/// use the stronger native Vim player color without obscuring selected text.
fn rich_navigation_overlay_selection_background(cx: &App) -> gpui::Hsla {
    // Markdown paints its selection as an overlay after its glyphs. The
    // theme's element-selection color is already tuned for that compositing
    // order; the read-only player color is opaque and grayscale, so merely
    // lowering its alpha makes selected Rich text look disabled.
    cx.theme().colors().element_selection_background.alpha(0.42)
}

fn rich_navigation_text_highlight_background(
    navigation: &RichNavigationPaint,
    cx: &App,
) -> gpui::Hsla {
    if navigation.visual {
        // StyledText backgrounds are below their glyphs, so the stronger Vim
        // player color stays crisp over syntax-colored diff and tool output.
        cx.theme().players().local().selection
    } else {
        cx.theme().players().local().cursor.opacity(0.62)
    }
}

fn rich_navigation_markdown_highlight_background(
    navigation: &RichNavigationPaint,
    cx: &App,
) -> gpui::Hsla {
    if navigation.visual {
        rich_navigation_overlay_selection_background(cx)
    } else {
        cx.theme().players().local().cursor.opacity(0.62)
    }
}

fn navigation_searchable_styled_text(
    text: String,
    base: Vec<(Range<usize>, gpui::HighlightStyle)>,
    search: Option<&RichSearchPaint>,
    navigation: Option<&RichNavigationPaint>,
    logical_fragment: Range<usize>,
    cx: &App,
) -> StyledText {
    let navigation = navigation_highlights_for_fragment(navigation, logical_fragment, cx);
    let base = gpui::combine_highlights(base, navigation).collect::<Vec<_>>();
    searchable_styled_text(text, base, search, cx)
}

fn logical_offset_for_rendered_index(
    logical_fragment: &Range<usize>,
    rendered_index: usize,
) -> usize {
    logical_fragment.start + rendered_index.min(logical_fragment.len())
}

fn rich_cursor_index_for_fragment(
    navigation: Option<&RichNavigationPaint>,
    logical_fragment: &Range<usize>,
) -> Option<usize> {
    let cursor = navigation?.cursor_range()?;
    let start = cursor.start.max(logical_fragment.start);
    let end = cursor.end.min(logical_fragment.end);
    (start < end).then_some(start - logical_fragment.start)
}

/// Ask every containing List to reveal the exact painted cursor line. Nested
/// lists consume the request to reveal the row locally, then forward it to the
/// transcript list once it is inside their own viewport.
fn rich_cursor_autoscroll_marker(
    layout: gpui::TextLayout,
    rendered_index: usize,
    color: gpui::Hsla,
) -> AnyElement {
    canvas(
        move |bounds, window, _| {
            let cursor_position = layout
                .position_for_index(rendered_index)
                .unwrap_or(bounds.origin);
            let cursor_y = cursor_position
                .y
                .max(bounds.top())
                .min(bounds.bottom() - px(1.));
            window.request_autoscroll(Bounds::from_corners(
                point(bounds.left(), cursor_y),
                point(
                    bounds.right(),
                    (cursor_y + layout.line_height()).min(bounds.bottom()),
                ),
            ));

            let text = layout.text();
            let next_index = text
                .get(rendered_index..)
                .and_then(|text| text.chars().next())
                .map_or(rendered_index, |character| {
                    rendered_index + character.len_utf8()
                });
            let cursor_width = layout
                .position_for_index(next_index)
                .filter(|end| end.y == cursor_position.y)
                .map_or(px(8.), |end| (end.x - cursor_position.x).max(px(2.)));
            gpui::fill(
                Bounds::new(cursor_position, size(cursor_width, layout.line_height())),
                color,
            )
        },
        |_, cursor, window, _| window.paint_quad(cursor),
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

fn rich_transcript_entry_placement(
    cursor_initialized: bool,
    current_item: Option<usize>,
    target_item: Option<usize>,
) -> Option<usize> {
    target_item.filter(|target| !cursor_initialized || current_item != Some(*target))
}

/// Make a visible structured-text fragment place the persistent Editor/Vim
/// cursor at the clicked glyph. Without this adapter, the card-level click
/// handler can only jump to the first row of the item.
fn rich_clickable_styled_text(
    id: String,
    styled: StyledText,
    item_index: usize,
    logical_fragment: Range<usize>,
    owner: Option<WeakEntity<HarnessApp>>,
) -> AnyElement {
    let Some(owner) = owner.filter(|_| !logical_fragment.is_empty()) else {
        return styled.into_any_element();
    };
    let layout = styled.layout().clone();
    div()
        .id(id)
        // A transcript row is a placement surface, not merely a run of
        // glyphs. Clicking after short output or within wrapped-line padding
        // should still place the cursor at the nearest byte on that row.
        .w_full()
        .min_h(px(20.))
        .min_w_0()
        .on_click(move |event, window, cx| {
            let rendered_index = match layout.index_for_position(event.position()) {
                Ok(index) | Err(index) => index,
            };
            let body_offset = logical_offset_for_rendered_index(&logical_fragment, rendered_index);
            cx.stop_propagation();
            window.prevent_default();
            owner
                .update(cx, |this, cx| {
                    this.selected_item = item_index;
                    this.visual_anchor = None;
                    this.focus_mode = FocusMode::Buffer;
                    this.transcript_cursor_initialized = true;
                    this.list_state.pause_following_tail();
                    // The clicked glyph is already visible. A command row can
                    // wrap to many viewport heights, so revealing the whole
                    // virtual row here would immediately scroll the exact
                    // click target away. Preserve this cursor as the nested
                    // surface's already-revealed position; keyboard motions
                    // still take the ordinary row/line reveal path.
                    if let Some(item_key) = this
                        .model
                        .items
                        .get(item_index)
                        .filter(|item| item.kind == model::TranscriptKind::Command)
                        .map(|item| item.key.clone())
                        && let Some(state) = this.rich_nested_scrolls.get_mut(&item_key)
                    {
                        state.last_cursor = Some(body_offset);
                    }
                    this.transcript_editor.update(cx, |editor, cx| {
                        editor.set_cursor_in_item(item_index, body_offset, window, cx);
                    });
                    this.transcript_editor.focus_handle(cx).focus(window, cx);
                    this.transcript_editor.update(cx, |editor, cx| {
                        editor.enter_normal_mode(window, cx);
                    });
                    cx.notify();
                })
                .ok();
        })
        .child(styled)
        .into_any_element()
}

fn search_context_snippet(
    content: &str,
    query: &str,
    max_chars: usize,
) -> Option<SearchContextSnippet> {
    let folded_query = query.trim().to_lowercase();
    if folded_query.is_empty() || max_chars == 0 {
        return None;
    }

    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Some(source_range) = folded_match_byte_ranges(line, &folded_query, 1).pop() else {
            continue;
        };
        let matched = &line[source_range.clone()];
        let match_chars = matched.chars().count();
        let context_budget = max_chars.saturating_sub(match_chars);
        let context_before = context_budget / 3;
        let mut prefix_chars = line[..source_range.start]
            .chars()
            .rev()
            .take(context_before)
            .collect::<Vec<_>>();
        prefix_chars.reverse();
        let prefix_truncated = line[..source_range.start].chars().count() > prefix_chars.len();
        let prefix = prefix_chars.into_iter().collect::<String>();
        let context_after = context_budget.saturating_sub(prefix.chars().count());
        let suffix = line[source_range.end..]
            .chars()
            .take(context_after)
            .collect::<String>();
        let suffix_truncated = line[source_range.end..].chars().count() > suffix.chars().count();

        let mut text = String::new();
        if prefix_truncated {
            text.push_str("… ");
        }
        let prefix_bytes = text.len();
        text.push_str(&prefix);
        let match_start = prefix_bytes + prefix.len();
        text.push_str(matched);
        let match_range = match_start..match_start + matched.len();
        text.push_str(&suffix);
        if suffix_truncated {
            text.push_str(" …");
        }
        return Some(SearchContextSnippet { text, match_range });
    }

    None
}

fn item_search_context_snippet(
    item: &TranscriptItem,
    query: &str,
    max_chars: usize,
) -> Option<SearchContextSnippet> {
    if item.kind == model::TranscriptKind::Web {
        let semantic_results = web_search_presentation(&item.raw)
            .results
            .into_iter()
            .flat_map(|result| {
                [Some(result.title), result.url, result.snippet]
                    .into_iter()
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join("\n");
        search_context_snippet(&semantic_results, query, max_chars)
            .or_else(|| search_context_snippet(&item.content, query, max_chars))
    } else {
        search_context_snippet(&item.content, query, max_chars)
    }
}

fn transcript_item_shows_header(item: &TranscriptItem) -> bool {
    !matches!(
        (item.kind, item.title.as_str()),
        (model::TranscriptKind::Agent, "Codex") | (model::TranscriptKind::User, "You")
    )
}

fn transcript_item_header_title(item: &TranscriptItem) -> &str {
    item.pending_request
        .as_ref()
        .and_then(|request| request_header_title(&request.method))
        .unwrap_or(&item.title)
}

fn image_protocol_path(raw: &Value) -> Option<&str> {
    raw.get("path")
        .or_else(|| raw.get("savedPath"))
        .and_then(Value::as_str)
}

fn image_caption_for_display(item: &TranscriptItem) -> Option<&str> {
    let caption = item.content.trim();
    (!caption.is_empty()
        && image_protocol_path(&item.raw).is_none_or(|path| caption != path.trim()))
    .then_some(caption)
}

fn transcript_item_searchable_body(item: &TranscriptItem) -> &str {
    if item.kind == model::TranscriptKind::Image {
        image_caption_for_display(item).unwrap_or_default()
    } else {
        &item.content
    }
}

fn rich_item_body_paints_navigation(item: &TranscriptItem) -> bool {
    if item.pending_request.is_some() || !item.expanded {
        return false;
    }
    match item.kind {
        model::TranscriptKind::Image => image_caption_for_display(item).is_some(),
        _ => !item.content.is_empty(),
    }
}

/// Virtual lists render their row closures during layout, after the card header
/// has already been constructed. Their cursor claim is therefore deferred even
/// though every logical command/output glyph has a visible owner.
fn rich_item_defers_navigation_claim(item: &TranscriptItem) -> bool {
    item.expanded
        && item.kind == model::TranscriptKind::Command
        && item.command_transcript().is_some()
}

fn item_matches_folded_query(item: &TranscriptItem, folded_query: &str) -> bool {
    (transcript_item_shows_header(item)
        && transcript_item_header_title(item)
            .to_lowercase()
            .contains(folded_query))
        || transcript_item_searchable_body(item)
            .to_lowercase()
            .contains(folded_query)
        || item
            .display_status()
            .is_some_and(|status| status.to_lowercase().contains(folded_query))
        || (item.kind == model::TranscriptKind::Web
            && web_search_presentation(&item.raw)
                .results
                .iter()
                .any(|result| {
                    result.title.to_lowercase().contains(folded_query)
                        || result
                            .url
                            .as_deref()
                            .is_some_and(|url| url.to_lowercase().contains(folded_query))
                        || result
                            .snippet
                            .as_deref()
                            .is_some_and(|snippet| snippet.to_lowercase().contains(folded_query))
                }))
}

fn reconcile_sorted_search_match(matches: &mut Vec<usize>, index: usize, is_match: bool) {
    match (matches.binary_search(&index), is_match) {
        (Err(position), true) => matches.insert(position, index),
        (Ok(position), false) => {
            matches.remove(position);
        }
        _ => {}
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ActivityTextSection {
    heading: Option<String>,
    body: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonTokenKind {
    Key,
    String,
    Number,
    Literal,
    Punctuation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JsonToken {
    range: Range<usize>,
    kind: JsonTokenKind,
}

fn activity_text_sections(content: &str) -> Vec<ActivityTextSection> {
    const HEADINGS: &[&str] = &[
        "Arguments",
        "Result",
        "Structured result",
        "Error",
        "Prompt",
        "Agents",
        "Query",
        "Page",
        "Find",
    ];

    let mut sections: Vec<ActivityTextSection> = Vec::new();
    for block in content.split("\n\n").filter(|block| !block.is_empty()) {
        let (first_line, remainder) = block.split_once('\n').unwrap_or((block, ""));
        if HEADINGS.contains(&first_line) {
            sections.push(ActivityTextSection {
                heading: Some(first_line.into()),
                body: remainder.into(),
            });
        } else if let Some(section) = sections.last_mut() {
            if !section.body.is_empty() {
                section.body.push_str("\n\n");
            }
            section.body.push_str(block);
        } else {
            sections.push(ActivityTextSection {
                heading: None,
                body: block.into(),
            });
        }
    }
    sections
}

fn is_valid_json(content: &str) -> bool {
    serde_json::from_str::<Box<serde_json::value::RawValue>>(content).is_ok()
}

#[cfg(test)]
fn json_tokens(content: &str) -> Option<Vec<JsonToken>> {
    is_valid_json(content).then_some(())?;
    Some(json_tokens_unchecked(content))
}

fn json_tokens_unchecked(content: &str) -> Vec<JsonToken> {
    let bytes = content.as_bytes();
    let mut tokens = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match bytes[offset] {
            b'"' => {
                let start = offset;
                offset += 1;
                while offset < bytes.len() {
                    match bytes[offset] {
                        b'\\' => offset = (offset + 2).min(bytes.len()),
                        b'"' => {
                            offset += 1;
                            break;
                        }
                        _ => offset += 1,
                    }
                }
                let mut next = offset;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                tokens.push(JsonToken {
                    range: start..offset,
                    kind: if bytes.get(next) == Some(&b':') {
                        JsonTokenKind::Key
                    } else {
                        JsonTokenKind::String
                    },
                });
            }
            b'-' | b'0'..=b'9' => {
                let start = offset;
                offset += 1;
                while offset < bytes.len()
                    && matches!(
                        bytes[offset],
                        b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
                    )
                {
                    offset += 1;
                }
                tokens.push(JsonToken {
                    range: start..offset,
                    kind: JsonTokenKind::Number,
                });
            }
            b't' | b'f' | b'n' => {
                let start = offset;
                offset += 1;
                while offset < bytes.len() && bytes[offset].is_ascii_alphabetic() {
                    offset += 1;
                }
                tokens.push(JsonToken {
                    range: start..offset,
                    kind: JsonTokenKind::Literal,
                });
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                tokens.push(JsonToken {
                    range: offset..offset + 1,
                    kind: JsonTokenKind::Punctuation,
                });
                offset += 1;
            }
            _ => offset += 1,
        }
    }
    tokens
}

#[cfg(test)]
fn structured_output_preview(content: &str, noun: &str) -> StructuredOutputPreview {
    structured_output_preview_with_limits(
        content,
        noun,
        STRUCTURED_OUTPUT_PREVIEW_LINES,
        STRUCTURED_OUTPUT_PREVIEW_BYTES,
    )
}

fn structured_output_preview_with_limits(
    content: &str,
    noun: &str,
    line_limit: usize,
    byte_limit: usize,
) -> StructuredOutputPreview {
    let total_lines = content.lines().count();
    let mut preview = if total_lines > line_limit {
        content
            .lines()
            .take(line_limit)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        content.to_string()
    };
    let hidden_lines = total_lines.saturating_sub(line_limit);
    let truncated_bytes = if preview.len() > byte_limit {
        let end = preview
            .char_indices()
            .map(|(offset, _)| offset)
            .take_while(|offset| *offset <= byte_limit)
            .last()
            .unwrap_or(0);
        preview.truncate(end);
        true
    } else {
        false
    };
    let footer = if hidden_lines > 0 {
        Some(format!("Show {hidden_lines} more {noun} lines"))
    } else if truncated_bytes {
        Some(format!("Show more {noun}"))
    } else {
        None
    };
    StructuredOutputPreview {
        content: preview,
        footer,
    }
}

fn output_limits(
    expansion: OutputExpansion,
    preview_lines: usize,
    preview_bytes: usize,
) -> OutputLimits {
    match expansion {
        OutputExpansion::Preview => OutputLimits {
            lines: preview_lines,
            bytes: preview_bytes,
        },
        OutputExpansion::Medium => OutputLimits {
            lines: PROGRESSIVE_OUTPUT_MEDIUM_LINES,
            bytes: PROGRESSIVE_OUTPUT_MEDIUM_BYTES,
        },
        OutputExpansion::Large => OutputLimits {
            lines: PROGRESSIVE_OUTPUT_LARGE_LINES,
            bytes: PROGRESSIVE_OUTPUT_LARGE_BYTES,
        },
        OutputExpansion::All => OutputLimits {
            lines: usize::MAX,
            bytes: usize::MAX,
        },
    }
}

fn command_output_for_display(output: &str) -> &str {
    output.trim_end_matches(['\r', '\n'])
}

fn normalize_command_line_endings(mut text: String) -> String {
    if text.contains('\r') {
        text = text.replace("\r\n", "\n").replace('\r', "\n");
    }
    text
}

fn command_line_ranges(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut offset = 0;
    text.split('\n').map(move |line| {
        let range = offset..offset + line.len();
        offset = range.end.saturating_add(1);
        range
    })
}

fn rich_command_data(item: &TranscriptItem) -> Option<RichCommandData> {
    let transcript = item.command_transcript()?;
    let command: Arc<str> = Arc::from(normalize_command_line_endings(
        transcript.command.trim_end_matches(['\r', '\n']).to_owned(),
    ));
    let output: Arc<str> = Arc::from(normalize_command_line_endings(
        command_output_for_display(&transcript.output).to_owned(),
    ));
    let mut rows = command_line_ranges(&command)
        .enumerate()
        .map(|(line_index, source_range)| RichCommandRow {
            source: RichCommandSource::Command,
            source_range,
            line_index,
        })
        .collect::<Vec<_>>();
    let command_row_count = rows.len();
    if !output.is_empty() {
        rows.extend(
            command_line_ranges(&output)
                .enumerate()
                .map(|(line_index, source_range)| RichCommandRow {
                    source: RichCommandSource::Output,
                    source_range,
                    line_index,
                }),
        );
    }
    Some(RichCommandData {
        command,
        output,
        rows: rows.into(),
        command_row_count,
    })
}

fn rich_command_row_logical_range(data: &RichCommandData, row: &RichCommandRow) -> Range<usize> {
    let base = match row.source {
        RichCommandSource::Command => 0,
        RichCommandSource::Output => data.command.len() + usize::from(!data.command.is_empty()),
    };
    base + row.source_range.start..base + row.source_range.end
}

fn progressive_line_limit(expansion: OutputExpansion, preview_limit: usize) -> usize {
    match expansion {
        OutputExpansion::Preview => preview_limit,
        OutputExpansion::Medium => PROGRESSIVE_OUTPUT_MEDIUM_LINES,
        OutputExpansion::Large => PROGRESSIVE_OUTPUT_LARGE_LINES,
        OutputExpansion::All => usize::MAX,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct FileChangePresentation {
    operation: String,
    path: String,
    content: String,
}

#[derive(Debug, Eq, PartialEq)]
struct DiffFilePresentation {
    path: String,
    content: String,
}

fn normalized_diff_path(path: &str) -> Option<String> {
    let path = path
        .trim()
        .trim_matches('"')
        .split_once('\t')
        .map_or(path.trim().trim_matches('"'), |(path, _)| path)
        .trim_matches('"');
    if path.is_empty() || path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_string(),
    )
}

fn diff_header_path(line: &str) -> Option<String> {
    let header = line.strip_prefix("diff --git ")?;
    header
        .rsplit_once(" b/")
        .map(|(_, path)| path)
        .or_else(|| header.rsplit_once(" \"b/").map(|(_, path)| path))
        .and_then(normalized_diff_path)
}

fn push_diff_file_presentation(
    sections: &mut Vec<DiffFilePresentation>,
    path: Option<String>,
    lines: &mut Vec<&str>,
) {
    if lines.is_empty() && path.is_none() {
        return;
    }
    let inferred_path = lines
        .iter()
        .find_map(|line| line.strip_prefix("+++ ").and_then(normalized_diff_path))
        .or_else(|| {
            lines
                .iter()
                .find_map(|line| line.strip_prefix("--- ").and_then(normalized_diff_path))
        });
    sections.push(DiffFilePresentation {
        path: path.or(inferred_path).unwrap_or_else(|| "Diff".into()),
        content: lines.join("\n").trim_end().to_string(),
    });
    lines.clear();
}

fn diff_file_presentations(content: &str) -> Vec<DiffFilePresentation> {
    let mut sections = Vec::new();
    let mut path = None;
    let mut lines = Vec::new();

    for line in content.lines() {
        if line.starts_with("diff --git ") {
            push_diff_file_presentation(&mut sections, path.take(), &mut lines);
            path = diff_header_path(line);
        } else {
            lines.push(line);
        }
    }
    push_diff_file_presentation(&mut sections, path, &mut lines);

    if sections.is_empty() && !content.trim().is_empty() {
        sections.push(DiffFilePresentation {
            path: "Diff".into(),
            content: content.trim_end().into(),
        });
    }
    sections
}

fn diff_content_counts(content: &str) -> (usize, usize) {
    let mut in_hunk = false;
    content
        .lines()
        .map(|line| diff_line_tone(line, &mut in_hunk))
        .fold((0, 0), |(additions, deletions), tone| match tone {
            DiffLineTone::Addition => (additions + 1, deletions),
            DiffLineTone::Deletion => (additions, deletions + 1),
            _ => (additions, deletions),
        })
}

fn aggregate_diff_counts<'a>(contents: impl IntoIterator<Item = &'a str>) -> (usize, usize) {
    contents.into_iter().map(diff_content_counts).fold(
        (0, 0),
        |(total_additions, total_deletions), (additions, deletions)| {
            (total_additions + additions, total_deletions + deletions)
        },
    )
}

fn fair_line_allocations(line_counts: &[usize], budget: usize) -> Vec<usize> {
    if budget == usize::MAX {
        return line_counts.to_vec();
    }
    if line_counts.is_empty() || budget == 0 {
        return vec![0; line_counts.len()];
    }
    let mut allocations = vec![0; line_counts.len()];
    let mut remaining_budget = budget;
    let share = (budget / line_counts.len()).max(1);
    for (index, line_count) in line_counts.iter().copied().enumerate() {
        if remaining_budget == 0 {
            break;
        }
        let added = line_count.min(share).min(remaining_budget);
        allocations[index] = added;
        remaining_budget -= added;
    }

    // Extra detail can safely go to the final visible file: its header is
    // already ahead of those lines, so this never pushes a later filename out
    // of the preview the way refilling an earlier huge patch would.
    if let (Some(allocation), Some(line_count)) =
        (allocations.last_mut(), line_counts.last().copied())
    {
        let added = (line_count - *allocation).min(remaining_budget);
        *allocation += added;
    }
    allocations
}

fn progressive_file_line_allocations(
    line_counts: &[usize],
    expansion: OutputExpansion,
) -> Vec<usize> {
    let limit = progressive_line_limit(expansion, STRUCTURED_OUTPUT_PREVIEW_LINES);
    fair_line_allocations(
        &line_counts.iter().copied().take(limit).collect::<Vec<_>>(),
        limit,
    )
}

fn file_change_summary(line: &str) -> Option<(&str, &str)> {
    ["Added", "Modified", "Deleted", "Moved"]
        .into_iter()
        .find_map(|operation| {
            line.strip_prefix(operation)
                .and_then(|rest| rest.strip_prefix(" · "))
                .map(|path| (operation, path))
        })
}

fn file_change_presentations(content: &str) -> Vec<FileChangePresentation> {
    let mut presentations = Vec::new();
    let mut current: Option<FileChangePresentation> = None;

    for line in content.lines() {
        if let Some((operation, path)) = file_change_summary(line) {
            if let Some(mut presentation) = current.take() {
                presentation.content = presentation.content.trim_end().to_string();
                presentations.push(presentation);
            }
            current = Some(FileChangePresentation {
                operation: operation.into(),
                path: path.into(),
                content: String::new(),
            });
        } else if let Some(presentation) = current.as_mut() {
            if !presentation.content.is_empty() {
                presentation.content.push('\n');
            }
            presentation.content.push_str(line);
        }
    }

    if let Some(mut presentation) = current {
        presentation.content = presentation.content.trim_end().to_string();
        presentations.push(presentation);
    }

    if presentations.is_empty() && !content.trim().is_empty() {
        presentations.push(FileChangePresentation {
            operation: "Changed".into(),
            path: "File details unavailable".into(),
            content: content.trim_end().into(),
        });
    }

    presentations
}

fn file_change_counts(presentation: &FileChangePresentation) -> (usize, usize) {
    let unified = presentation
        .content
        .lines()
        .any(|line| line.starts_with("@@"));
    if !unified {
        let line_count = presentation.content.lines().count();
        return match presentation.operation.as_str() {
            "Added" => (line_count, 0),
            "Deleted" => (0, line_count),
            _ => (0, 0),
        };
    }

    diff_content_counts(&presentation.content)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiffLineTone {
    Normal,
    Hunk,
    Addition,
    Deletion,
}

fn diff_line_tone(line: &str, in_hunk: &mut bool) -> DiffLineTone {
    if line.starts_with("--- ") || line.starts_with("diff --git ") {
        *in_hunk = false;
    }
    if line.starts_with("@@") {
        *in_hunk = true;
        return DiffLineTone::Hunk;
    }
    if *in_hunk && line.starts_with('+') && !line.starts_with("+++") {
        DiffLineTone::Addition
    } else if *in_hunk && line.starts_with('-') && !line.starts_with("---") {
        DiffLineTone::Deletion
    } else {
        DiffLineTone::Normal
    }
}

fn diff_hunk_starts(line: &str) -> Option<(usize, usize)> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "@@" {
        return None;
    }
    let old_line = fields
        .next()?
        .strip_prefix('-')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    let new_line = fields
        .next()?
        .strip_prefix('+')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    Some((old_line, new_line))
}

fn diff_line_numbers(
    line: &str,
    tone: DiffLineTone,
    unified: bool,
    in_hunk: bool,
    old_line: &mut Option<usize>,
    new_line: &mut Option<usize>,
    fallback_line: usize,
) -> (Option<usize>, Option<usize>) {
    if tone == DiffLineTone::Hunk {
        if let Some((old_start, new_start)) = diff_hunk_starts(line) {
            *old_line = Some(old_start);
            *new_line = Some(new_start);
        }
        return (None, None);
    }
    if !unified {
        return (None, Some(fallback_line));
    }
    if !in_hunk || line.starts_with("\\ No newline") {
        return (None, None);
    }

    match tone {
        DiffLineTone::Addition => {
            let displayed = *new_line;
            *new_line = new_line.map(|line| line + 1);
            (None, displayed)
        }
        DiffLineTone::Deletion => {
            let displayed = *old_line;
            *old_line = old_line.map(|line| line + 1);
            (displayed, None)
        }
        DiffLineTone::Normal => {
            let displayed = (*old_line, *new_line);
            *old_line = old_line.map(|line| line + 1);
            *new_line = new_line.map(|line| line + 1);
            displayed
        }
        DiffLineTone::Hunk => unreachable!(),
    }
}

fn rich_search_match_needs_context(item: &TranscriptItem, expansion: OutputExpansion) -> bool {
    let _ = expansion;
    !item.expanded
}

fn folded_contains(text: &str, query: &str) -> bool {
    !folded_match_byte_ranges(text, query, 1).is_empty()
}

/// Whether the active Rich card already paints the matching text. Search
/// context is a fallback for collapsed/truncated content, not a second copy of
/// metadata that the card always exposes (notably file paths and commands).
fn rich_search_query_is_visible(
    item: &TranscriptItem,
    _expansion: OutputExpansion,
    query: &str,
) -> bool {
    if (transcript_item_shows_header(item)
        && folded_contains(transcript_item_header_title(item), query))
        || item
            .display_status()
            .is_some_and(|status| folded_contains(status, query))
    {
        return true;
    }
    if !item.expanded {
        return item.kind == model::TranscriptKind::Reasoning
            && folded_contains(&compact_reasoning_preview(&item.content), query);
    }

    match item.kind {
        model::TranscriptKind::Command => item.command_transcript().is_some_and(|transcript| {
            let command_text = transcript.command.trim_end_matches(['\r', '\n']);
            let output_text = command_output_for_display(&transcript.output);
            folded_contains(command_text, query) || folded_contains(output_text, query)
        }),
        model::TranscriptKind::FileChange => file_change_presentations(&item.content)
            .into_iter()
            .any(|presentation| {
                let (additions, deletions) = file_change_counts(&presentation);
                ((additions == 0 && deletions == 0)
                    && folded_contains(&presentation.operation, query))
                    || folded_contains(&presentation.path, query)
                    || folded_contains(&presentation.content, query)
            }),
        model::TranscriptKind::Diff => {
            diff_file_presentations(&item.content)
                .into_iter()
                .any(|presentation| {
                    folded_contains(&presentation.path, query)
                        || folded_contains(&presentation.content, query)
                })
        }
        model::TranscriptKind::Web => {
            web_search_presentation(&item.raw)
                .results
                .iter()
                .any(|result| {
                    folded_contains(&result.title, query)
                        || result
                            .url
                            .as_deref()
                            .is_some_and(|url| folded_contains(url, query))
                        || result
                            .snippet
                            .as_deref()
                            .is_some_and(|snippet| folded_contains(snippet, query))
                })
        }
        model::TranscriptKind::Tool
        | model::TranscriptKind::Subagent
        | model::TranscriptKind::Review => folded_contains(&item.content, query),
        kind if kind.is_structured() => folded_contains(&item.content, query),
        _ => folded_contains(transcript_item_searchable_body(item), query),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WebResultPresentation {
    title: String,
    url: Option<String>,
    snippet: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct WebSearchPresentation {
    related_queries: usize,
    results: Vec<WebResultPresentation>,
}

fn compact_web_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
fn web_search_has_hidden_content(presentation: &WebSearchPresentation) -> bool {
    presentation.results.len() > WEB_RESULT_PREVIEW_COUNT
        || presentation.results.iter().any(|result| {
            result.title.chars().count() > 100
                || result
                    .url
                    .as_ref()
                    .is_some_and(|url| url.chars().count() > 140)
                || result
                    .snippet
                    .as_ref()
                    .is_some_and(|snippet| snippet.chars().count() > 120)
        })
}

fn web_search_presentation(raw: &Value) -> WebSearchPresentation {
    let action = raw.get("action").unwrap_or(&Value::Null);
    let query_count = action
        .get("queries")
        .and_then(Value::as_array)
        .map(|queries| queries.iter().filter(|query| query.is_string()).count())
        .unwrap_or_else(|| {
            usize::from(
                action.get("query").and_then(Value::as_str).is_some()
                    || raw.get("query").and_then(Value::as_str).is_some(),
            )
        });
    let results = raw
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|result| {
            if let Some(text) = result.as_str() {
                return Some(WebResultPresentation {
                    title: compact_web_text(text),
                    url: None,
                    snippet: None,
                });
            }
            let title = result
                .get("title")
                .or_else(|| result.get("name"))
                .or_else(|| result.get("text"))
                .and_then(Value::as_str);
            let url = result
                .get("url")
                .or_else(|| result.get("link"))
                .and_then(Value::as_str);
            let snippet = result
                .get("snippet")
                .or_else(|| result.get("description"))
                .or_else(|| result.get("content"))
                .and_then(Value::as_str);
            (title.is_some() || url.is_some() || snippet.is_some()).then(|| WebResultPresentation {
                title: compact_web_text(title.unwrap_or("Result")),
                url: url.map(compact_web_text),
                snippet: snippet.map(compact_web_text),
            })
        })
        .collect();
    WebSearchPresentation {
        related_queries: query_count.saturating_sub(1),
        results,
    }
}

fn reasoning_steps(content: &str) -> Vec<String> {
    content
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
        .collect()
}

fn append_rich_navigation_fragment(output: &mut String, fragment: &str) {
    let fragment = fragment.trim_matches(['\r', '\n']);
    if fragment.is_empty() {
        return;
    }
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(fragment);
}

/// Build the text native Vim navigates from the same semantic presentations
/// the Rich card renderer consumes. Every logical row must correspond to
/// visible card text; protocol-only labels and presentation-only blank rows do
/// not belong in this document.
fn rich_navigation_body_for_item(item: &TranscriptItem, fallback: &str) -> String {
    let mut output = String::new();
    match item.kind {
        model::TranscriptKind::User
        | model::TranscriptKind::Agent
        | model::TranscriptKind::Plan
            if !item.status.as_deref().is_some_and(|status| {
                matches!(
                    status,
                    "streaming" | "running" | "inProgress" | "in_progress"
                )
            }) =>
        {
            return model::rich_markdown_navigation_text(&item.content);
        }
        model::TranscriptKind::Reasoning => {
            for step in reasoning_steps(&item.content) {
                append_rich_navigation_fragment(&mut output, &step);
            }
        }
        model::TranscriptKind::Diff => {
            for presentation in diff_file_presentations(&item.content) {
                append_rich_navigation_fragment(&mut output, &presentation.path);
                append_rich_navigation_fragment(&mut output, &presentation.content);
            }
        }
        model::TranscriptKind::FileChange => {
            for presentation in file_change_presentations(&item.content) {
                append_rich_navigation_fragment(&mut output, &presentation.path);
                append_rich_navigation_fragment(&mut output, &presentation.content);
            }
        }
        model::TranscriptKind::Web => {
            let presentation = web_search_presentation(&item.raw);
            if presentation.results.is_empty() {
                return fallback.to_owned();
            }
            for result in presentation.results {
                append_rich_navigation_fragment(&mut output, &result.title);
                if let Some(url) = result.url {
                    append_rich_navigation_fragment(&mut output, &url);
                }
                if let Some(snippet) = result.snippet {
                    append_rich_navigation_fragment(&mut output, &snippet);
                }
            }
        }
        model::TranscriptKind::Tool
        | model::TranscriptKind::Subagent
        | model::TranscriptKind::Review => {
            let sections = activity_text_sections(&item.content);
            if !sections.iter().any(|section| section.heading.is_some()) {
                return fallback.to_owned();
            }
            for section in sections {
                if let Some(heading) = section.heading {
                    append_rich_navigation_fragment(&mut output, &heading);
                }
                append_rich_navigation_fragment(&mut output, &section.body);
            }
        }
        model::TranscriptKind::Image => {
            if let Some(caption) = image_caption_for_display(item) {
                append_rich_navigation_fragment(&mut output, caption);
            } else {
                return fallback.to_owned();
            }
        }
        _ => return fallback.to_owned(),
    }
    output
}

fn rich_navigation_item_projection(
    model: &TranscriptModel,
    item_index: usize,
) -> Option<model::TranscriptItemProjection> {
    let projection = model.rich_navigation_item_projection(item_index)?;
    let item = model.items.get(item_index)?;
    let body = rich_navigation_body_for_item(item, projection.body_text());
    let projection = if body == projection.body_text() {
        projection
    } else {
        projection.with_body_text(body)
    };
    let has_following_visible_item = model.items[item_index + 1..]
        .iter()
        .any(TranscriptItem::is_presentationally_visible);
    Some(if has_following_visible_item {
        projection
    } else {
        projection.without_terminal_separator()
    })
}

fn rich_navigation_document(model: &TranscriptModel) -> model::TranscriptDocument {
    model::TranscriptDocument::from_item_projections(
        model.items.len(),
        (0..model.items.len()).filter_map(|index| rich_navigation_item_projection(model, index)),
    )
}

fn shell_highlights(command: &str, cx: &App) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
    let mut byte_styles = vec![(0_u8, None); command.len()];
    for (range, capture_name) in shell_capture_ranges(command) {
        let Some(style) = cx.theme().syntax().style_for_name(&capture_name) else {
            continue;
        };
        let priority = shell_capture_priority(&capture_name);
        for byte_style in &mut byte_styles[range] {
            if priority >= byte_style.0 {
                *byte_style = (priority, Some(style));
            }
        }
    }

    let mut highlights = Vec::new();
    let mut active_style = None;
    let mut active_start = 0;
    for offset in command
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(command.len()))
    {
        let style = byte_styles
            .get(offset)
            .and_then(|(_, style)| style.as_ref())
            .copied();
        if style != active_style {
            if let Some(style) = active_style {
                highlights.push((active_start..offset, style));
            }
            active_style = style;
            active_start = offset;
        }
    }
    highlights
}

fn json_highlights(content: &str, cx: &App) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
    let syntax = cx.theme().syntax();
    let colors = cx.theme().colors();
    json_tokens_unchecked(content)
        .into_iter()
        .map(|token| {
            let style = match token.kind {
                JsonTokenKind::Key => syntax
                    .style_for_name("property")
                    .or_else(|| syntax.style_for_name("variable"))
                    .unwrap_or(gpui::HighlightStyle {
                        color: Some(colors.text_accent),
                        ..Default::default()
                    }),
                JsonTokenKind::String => syntax.style_for_name("string").unwrap_or_default(),
                JsonTokenKind::Number => syntax.style_for_name("number").unwrap_or_default(),
                JsonTokenKind::Literal => syntax
                    .style_for_name("boolean")
                    .or_else(|| syntax.style_for_name("constant"))
                    .unwrap_or_default(),
                JsonTokenKind::Punctuation => gpui::HighlightStyle {
                    color: Some(colors.text_muted),
                    ..Default::default()
                },
            };
            (token.range, style)
        })
        .collect()
}

fn reconnect_delay(attempt: u8) -> Option<Duration> {
    (attempt < MAX_RECONNECT_ATTEMPTS).then(|| Duration::from_secs(1 << attempt))
}

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
    search_query: Option<String>,
    search_ranges: Vec<Range<usize>>,
    navigation: Option<RichMarkdownNavigationPaint>,
    last_autoscroll_generation: Option<u64>,
}

struct LiveRequestSurface {
    request_id: Value,
    entity: Entity<RequestSurface>,
}

fn legacy_request_controls_active(
    request_is_live: bool,
    uses_shared_surface: bool,
    request_is_pending: bool,
) -> bool {
    request_is_live && !uses_shared_surface && request_is_pending
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ComposerRenderMetrics {
    empty: bool,
    height: f32,
}

fn composer_available_width(
    viewport_width: f32,
    sidebar_open: bool,
    sidebar_user_override: bool,
) -> f32 {
    let compact = viewport_width < COMPACT_SIDEBAR_THRESHOLD;
    let sidebar_visible = sidebar_open && (!compact || sidebar_user_override);
    viewport_width - if sidebar_visible { SIDEBAR_WIDTH } else { 0. }
}

fn estimated_composer_columns(line: &str, profile: TranscriptTypographyProfile) -> f32 {
    line.chars()
        .map(|character| {
            if character == '\t' {
                return 4.;
            }
            if !character.is_ascii() {
                return 2.;
            }
            if profile == TranscriptTypographyProfile::Buffer {
                return 1.;
            }
            match character {
                // A small proportional-width model keeps the fixed-height host
                // conservative for genuinely wide prose without making lines
                // full of narrow glyphs grow as early as monospace content.
                'i' | 'l' | 'I' | '!' | '|' | '.' | ',' | ':' | ';' | '\'' | '`' | ' ' => 0.55,
                'M' | 'W' | '@' | '#' | '%' | '&' => 1.5,
                _ => 1.,
            }
        })
        .sum()
}

fn composer_height(text: &str, available_width: f32, profile: TranscriptTypographyProfile) -> f32 {
    let columns = ((available_width - 48.).max(0.) / 8.).floor().max(24.);
    let visual_rows = if text.is_empty() {
        1
    } else {
        text.split('\n')
            .map(|line| {
                ((estimated_composer_columns(line, profile) / columns).ceil() as usize).max(1)
            })
            .sum::<usize>()
            .clamp(1, 8)
    };
    78. + 20. * visual_rows.saturating_sub(1) as f32
}

fn composer_render_metrics(
    text: &str,
    available_width: f32,
    profile: TranscriptTypographyProfile,
) -> ComposerRenderMetrics {
    ComposerRenderMetrics {
        empty: text.trim().is_empty(),
        height: composer_height(text, available_width, profile),
    }
}

fn composer_edit_requires_root_invalidation(
    previous: ComposerRenderMetrics,
    next: ComposerRenderMetrics,
) -> bool {
    previous != next
}

fn composer_send_blocked(
    composer_empty: bool,
    loading_thread: bool,
    read_only: bool,
    transport_available: bool,
) -> bool {
    composer_empty || loading_thread || read_only || !transport_available
}

fn request_should_take_focus(
    newly_mounted: bool,
    is_live: bool,
    unresolved: bool,
    composer_empty: bool,
    focus_mode: FocusMode,
) -> bool {
    newly_mounted && is_live && unresolved && composer_empty && focus_mode == FocusMode::Composer
}

fn request_header_title(method: &str) -> Option<&'static str> {
    match method {
        "item/commandExecution/requestApproval" | "execCommandApproval" => Some("Command approval"),
        "item/fileChange/requestApproval" | "applyPatchApproval" => Some("File change approval"),
        "item/permissions/requestApproval" => Some("Permission request"),
        "item/tool/requestUserInput" => Some("Input requested"),
        "mcpServer/elicitation/request" => Some("MCP request"),
        _ => None,
    }
}

fn mark_unbacked_requests_inactive(
    model: &mut TranscriptModel,
    live_request_keys: &HashSet<String>,
) {
    for item in &mut model.items {
        if item
            .pending_request
            .as_ref()
            .is_some_and(|request| !request.resolved)
            && !live_request_keys.contains(&item.key)
        {
            item.status = Some("inactive".into());
        }
    }
}

const HYBRID_REPLACEMENT_PREFIX: &str = "hybrid-rich:";

fn hybrid_replacement_key(item_key: &str) -> String {
    format!("{HYBRID_REPLACEMENT_PREFIX}{item_key}")
}

fn selectable_rich_command_experiment() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var("HARNESS_SELECTABLE_RICH_COMMAND")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
    });
    *ENABLED
}

fn selectable_rich_diff_experiment() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var("HARNESS_SELECTABLE_RICH_DIFF")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
    });
    *ENABLED
}

fn rich_vim_experiment() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        // Rich rendering backed by the persistent native Editor is now the
        // standalone default. Keep an explicit escape hatch while the Text
        // projection remains useful for diagnosing selection/layout parity.
        std::env::var("HARNESS_RICH_VIM")
            .ok()
            .is_none_or(|value| !matches!(value.as_str(), "0" | "false" | "no"))
    });
    *ENABLED
}

fn slow_list_diagnostics() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("GPUI_SLOW_LIST_DIAGNOSTICS")
            .is_some_and(|value| !value.is_empty() && value != std::ffi::OsStr::new("0"))
    });
    *ENABLED
}

fn item_uses_hybrid_surface(item: &TranscriptItem) -> bool {
    if selectable_rich_command_experiment() && item.kind == model::TranscriptKind::Command {
        return false;
    }
    if selectable_rich_diff_experiment() && item.kind == model::TranscriptKind::Diff {
        return false;
    }
    match item.kind {
        model::TranscriptKind::Diff => true,
        model::TranscriptKind::Command => item.command_transcript().is_some(),
        _ => false,
    }
}

struct HybridStructuredSurface {
    item: TranscriptItem,
    item_index: usize,
    owner: WeakEntity<HarnessApp>,
}

impl HybridStructuredSurface {
    fn new(item: TranscriptItem, item_index: usize, owner: WeakEntity<HarnessApp>) -> Self {
        Self {
            item,
            item_index,
            owner,
        }
    }

    fn update(&mut self, item: TranscriptItem, item_index: usize, cx: &mut Context<Self>) {
        if self.item.key == item.key
            && self.item.event_count == item.event_count
            && self.item.expanded == item.expanded
            && self.item.content == item.content
            && self.item.title == item.title
            && self.item.status == item.status
            && self.item_index == item_index
        {
            return;
        }
        self.item = item;
        self.item_index = item_index;
        cx.notify();
    }
}

impl Render for HybridStructuredSurface {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let body = self
            .item
            .expanded
            .then(|| match self.item.kind {
                model::TranscriptKind::Diff => Some(HarnessApp::render_diff_content(
                    &self.item,
                    self.item_index,
                    None,
                    None,
                    OutputExpansion::Preview,
                    None,
                    Some(self.owner.clone()),
                    cx,
                )),
                model::TranscriptKind::Command => HarnessApp::render_command_content(
                    &self.item,
                    self.item_index,
                    None,
                    None,
                    OutputExpansion::Preview,
                    None,
                    Some(self.owner.clone()),
                    cx,
                ),
                _ => None,
            })
            .flatten();
        let item_key = self.item.key.clone();
        let owner = self.owner.clone();
        let header = div()
            .id(format!("hybrid-structured-header:{}", self.item.key))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                owner
                    .update(cx, |app, cx| {
                        if let Some(item) =
                            app.model.items.iter_mut().find(|item| item.key == item_key)
                        {
                            item.expanded = !item.expanded;
                            app.transcript_editor.update(cx, |editor, _| {
                                editor.pause_tail_follow();
                            });
                            cx.notify();
                        }
                    })
                    .ok();
            })
            .child(
                Icon::new(icon_for_kind(self.item.kind))
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_ui_sm(cx)
                    .text_color(colors.text_muted)
                    .child(transcript_item_header_title(&self.item).to_owned()),
            )
            .child(Disclosure::new(
                format!("hybrid-structured-disclosure:{}", self.item.key),
                self.item.expanded,
            ));

        div().size_full().min_w_0().py_1().child(
            div()
                .size_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .rounded_sm()
                .border_1()
                .border_color(colors.border_variant)
                .px_2()
                .py_1()
                .child(header)
                .when_some(body, |this, body| this.child(body)),
        )
    }
}

fn hybrid_structured_rows(item: &TranscriptItem) -> u32 {
    if !item.expanded {
        return 2;
    }
    let rows = match item.kind {
        model::TranscriptKind::Diff => {
            let presentations = diff_file_presentations(&item.content);
            let visible_lines = progressive_file_line_allocations(
                &presentations
                    .iter()
                    .map(|presentation| presentation.content.lines().count())
                    .collect::<Vec<_>>(),
                OutputExpansion::Preview,
            )
            .into_iter()
            .sum::<usize>();
            let structural_rows = presentations.len() + usize::from(presentations.len() > 1) + 3;
            visible_lines + structural_rows
        }
        model::TranscriptKind::Command => item.command_transcript().map_or(4, |command| {
            let command_limits = output_limits(
                OutputExpansion::Preview,
                COMMAND_PREVIEW_LINES,
                COMMAND_PREVIEW_BYTES,
            );
            let output_limits = output_limits(
                OutputExpansion::Preview,
                STRUCTURED_OUTPUT_PREVIEW_LINES,
                STRUCTURED_OUTPUT_PREVIEW_BYTES,
            );
            let command_lines = structured_output_preview_with_limits(
                command.command.trim_end_matches(['\r', '\n']),
                "command",
                command_limits.lines,
                command_limits.bytes,
            )
            .content
            .lines()
            .count()
            .max(1);
            let output_lines = structured_output_preview_with_limits(
                command_output_for_display(&command.output),
                "output",
                output_limits.lines,
                output_limits.bytes,
            )
            .content
            .lines()
            .count();
            command_lines + output_lines + usize::from(output_lines > 0) * 2 + 3
        }),
        _ => 2,
    };
    u32::try_from(rows).unwrap_or(18).clamp(4, 18)
}

struct HarnessApp {
    cwd: String,
    replay_count: Option<usize>,
    client: Option<Rc<Client>>,
    threads: Vec<CodexThread>,
    selected_thread_id: Option<String>,
    loaded_thread_updated_at: Option<i64>,
    connecting: bool,
    loading_thread: bool,
    thread_read_only_reason: Option<SharedString>,
    error: Option<SharedString>,
    model: TranscriptModel,
    composer: Entity<LocalEditor>,
    composer_metrics: ComposerRenderMetrics,
    search_editor: Entity<LocalEditor>,
    transcript_editor: Entity<TranscriptEditor>,
    rich_navigation_selection: Option<TranscriptSelectionSnapshot>,
    mode_indicator: Entity<ModeIndicator>,
    buffer_view: bool,
    transcript_focus: FocusHandle,
    focus_mode: FocusMode,
    transcript_cursor_initialized: bool,
    selected_item: usize,
    selected_task: usize,
    visual_anchor: Option<usize>,
    raw_visible: HashSet<String>,
    markdown_cache: HashMap<String, CachedMarkdown>,
    search_visible: bool,
    search_query: String,
    search_matches: Vec<usize>,
    active_search_match: usize,
    search_navigation_generation: u64,
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
    performance_reporter: PerformanceReporter,
    performance_j_generation: u64,
    performance_j_run: Option<PerformanceJDriver>,
    performance_status: Option<SharedString>,
    performance_status_generation: u64,
    dirty_image_surfaces: HashSet<String>,
    image_surfaces: HashMap<String, Entity<ImageSurface>>,
    hybrid_surfaces: HashMap<String, Entity<HybridStructuredSurface>>,
    rich_nested_scrolls: HashMap<String, RichNestedScrollState>,
    list_state: ListState,
    task_list_state: ListState,
    sidebar_open: bool,
    sidebar_user_override: bool,
    server_task: Task<()>,
    request_task: Task<()>,
    reconnect_task: Task<()>,
    read_only_refresh_task: Task<()>,
    reconnect_attempts: u8,
}

impl HarnessApp {
    fn rich_nested_scroll_binding(
        &mut self,
        item_key: &str,
        navigation: Option<&RichNavigationPaint>,
    ) -> RichNestedScrollBinding {
        let cursor = navigation
            .and_then(RichNavigationPaint::cursor_range)
            .map(|range| range.start);
        let state = self
            .rich_nested_scrolls
            .entry(item_key.to_owned())
            .or_default();
        let reveal_cursor = cursor.is_some() && cursor != state.last_cursor;
        state.last_cursor = cursor;
        RichNestedScrollBinding {
            handle: state.handle.clone(),
            reveal_cursor,
        }
    }

    fn rich_command_surface(
        &mut self,
        item: &TranscriptItem,
        navigation: Option<&RichNavigationPaint>,
    ) -> Option<(RichCommandData, ListState, ListState)> {
        let needs_rebuild = self
            .rich_nested_scrolls
            .get(&item.key)
            .and_then(|state| state.command.as_ref())
            .is_none_or(|surface| {
                surface.event_count != item.event_count || surface.content_len != item.content.len()
            });

        if needs_rebuild {
            let data = rich_command_data(item)?;
            let command_row_count = data.command_row_count;
            let output_row_count = data.rows.len().saturating_sub(command_row_count);
            let state = self
                .rich_nested_scrolls
                .entry(item.key.clone())
                .or_default();
            let command_list_state = state.command.as_ref().map_or_else(
                || {
                    // Commands are normally only a handful of logical lines. Measure
                    // all of them once so wrapped lines contribute their exact height
                    // to the bottom boundary before the user starts scrolling.
                    ListState::new(command_row_count, ListAlignment::Top, px(120.)).measure_all()
                },
                |surface| surface.command_list_state.clone(),
            );
            let output_list_state = state.command.as_ref().map_or_else(
                || {
                    // Output can contain hundreds of thousands of lines, so eagerly
                    // measuring it would defeat virtualization. A one-line height
                    // estimate makes every unseen row scroll-reachable; the list
                    // replaces estimates with exact wrapped heights as rows appear.
                    ListState::new(output_row_count, ListAlignment::Top, px(240.))
                        .with_uniform_item_height(px(RICH_COMMAND_ROW_HEIGHT_HINT))
                },
                |surface| surface.output_list_state.clone(),
            );
            command_list_state.set_diagnostics_name(format!("command-input:{}", item.key));
            output_list_state.set_diagnostics_name(format!("command-output:{}", item.key));
            if command_list_state.item_count() != command_row_count {
                command_list_state.splice(0..command_list_state.item_count(), command_row_count);
            }
            if output_list_state.item_count() != output_row_count {
                output_list_state.splice(0..output_list_state.item_count(), output_row_count);
                output_list_state
                    .clone()
                    .with_uniform_item_height(px(RICH_COMMAND_ROW_HEIGHT_HINT));
            }
            state.command = Some(RichCommandSurface {
                event_count: item.event_count,
                content_len: item.content.len(),
                data,
                command_list_state,
                output_list_state,
            });
        }

        let state = self.rich_nested_scrolls.get_mut(&item.key)?;
        let surface = state.command.as_ref()?;
        let cursor = navigation
            .and_then(RichNavigationPaint::cursor_range)
            .map(|range| range.start);
        if cursor.is_some() && cursor != state.last_cursor {
            let cursor = cursor.unwrap();
            let row = surface
                .data
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| (index, rich_command_row_logical_range(&surface.data, row)))
                .find(|(_, range)| range.contains(&cursor) || range.end == cursor)
                .map(|(index, _)| index);
            if let Some(row) = row {
                if row < surface.data.command_row_count {
                    surface.command_list_state.scroll_to_reveal_item(row);
                } else {
                    surface
                        .output_list_state
                        .scroll_to_reveal_item(row - surface.data.command_row_count);
                }
            }
        }
        state.last_cursor = cursor;
        Some((
            surface.data.clone(),
            surface.command_list_state.clone(),
            surface.output_list_state.clone(),
        ))
    }

    fn rich_navigation_for_item(&self, item_index: usize) -> Option<RichNavigationPaint> {
        // The navigation Editor retains its selection while focus moves to the
        // composer, search, approvals, or sidebar. Only paint that cached
        // selection while the transcript itself owns keyboard focus; otherwise
        // the window presents two independent Vim cursors.
        if self.focus_mode != FocusMode::Buffer {
            return None;
        }
        let snapshot = self.rich_navigation_selection.as_ref()?;
        let item = snapshot
            .items
            .iter()
            .find(|item| item.item_index == item_index)?;
        Some(RichNavigationPaint {
            body_text: item.body_text.clone(),
            ranges: item.ranges.clone(),
            head: item.head,
            visual: snapshot.visual,
            cursor_claimed: Rc::new(Cell::new(false)),
        })
    }

    fn reveal_rich_navigation_item(&mut self, item_index: usize, body_offset: usize) {
        let Some(item) = self.model.items.get(item_index) else {
            return;
        };
        if item.kind != model::TranscriptKind::Command {
            self.list_state.scroll_to_reveal_item(item_index);
            return;
        }

        let viewport = self.list_state.viewport_bounds();
        let item_is_visible = self
            .list_state
            .bounds_for_item(item_index)
            .is_some_and(|bounds| bounds.intersects(&viewport));
        if item_is_visible {
            // The cursor-line marker in the selected nested row will perform
            // exact inner-then-outer autoscroll during prepaint. Revealing the
            // entire tall card here would instead align its bottom and can move
            // a command cursor out of view.
            return;
        }

        let cursor_is_in_command = rich_command_data(item).is_none_or(|data| {
            body_offset < data.command.len() + usize::from(!data.command.is_empty())
        });
        if cursor_is_in_command {
            self.list_state.scroll_to(gpui::ListOffset {
                item_ix: item_index,
                offset_in_item: px(0.),
            });
        } else {
            self.list_state.scroll_to_reveal_item(item_index);
        }
    }

    /// Update a mounted Markdown message without invalidating HarnessApp.
    ///
    /// Most ordinary `j`/`k` motions remain within one user or agent message.
    /// Markdown is already its own GPUI entity, so its external selection can
    /// repaint independently; rebuilding the sidebar, composer, list, and all
    /// visible cards for that motion is both unnecessary and the dominant
    /// Rich+Vim latency cost.
    fn update_cached_markdown_navigation(
        &mut self,
        item_index: usize,
        snapshot: &TranscriptSelectionSnapshot,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(item) = self.model.items.get(item_index) else {
            return false;
        };
        if !matches!(
            item.kind,
            model::TranscriptKind::User
                | model::TranscriptKind::Agent
                | model::TranscriptKind::Plan
        ) || !item.expanded
            || item.status.as_deref() == Some("streaming")
            || item.content.is_empty()
            || item.pending_request.is_some()
        {
            return false;
        }
        let Some(selection) = snapshot
            .items
            .iter()
            .find(|selection| selection.item_index == item_index)
        else {
            return false;
        };

        let key = item.key.clone();
        let source = item.content.clone();
        let navigation = RichNavigationPaint {
            body_text: selection.body_text.clone(),
            ranges: selection.ranges.clone(),
            head: selection.head,
            visual: snapshot.visual,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        let next_navigation = Some(navigation.markdown_source_navigation(&source));
        let Some(cached) = self.markdown_cache.get_mut(&key) else {
            return false;
        };
        if cached.source != source {
            return false;
        }
        if cached.navigation != next_navigation {
            cached.navigation = next_navigation.clone();
            cached.entity.update(cx, |markdown, cx| {
                let navigation = next_navigation.as_ref();
                markdown.set_external_navigation(
                    navigation.map(|navigation| navigation.selections.clone()),
                    navigation.and_then(|navigation| navigation.cursor),
                    cx,
                )
            });
        }
        true
    }

    fn new(
        cwd: String,
        replay_count: Option<usize>,
        start_in_text_view: bool,
        initial_thread_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mode_indicator = cx.new(|cx| ModeIndicator::new(window, cx));
        let composer = cx.new(|cx| LocalEditor::modal_composer(window, cx));
        let composer_metrics = composer_render_metrics(
            "",
            composer_available_width(f32::from(window.viewport_size().width), true, false),
            composer.read(cx).typography_profile(),
        );
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
        cx.subscribe_in(
            &composer,
            window,
            |this, composer, _: &LocalEditorChanged, window, cx| {
                let text = composer.read(cx).text(cx);
                let available_width = composer_available_width(
                    f32::from(window.viewport_size().width),
                    this.sidebar_open,
                    this.sidebar_user_override,
                );
                let next = composer_render_metrics(
                    &text,
                    available_width,
                    composer.read(cx).typography_profile(),
                );
                if composer_edit_requires_root_invalidation(this.composer_metrics, next) {
                    this.composer_metrics = next;
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe(
            &transcript_editor,
            |this, editor, _: &TranscriptSelectionChanged, cx| {
                // Snapshot the native selection once at the event boundary. Rich
                // rendering can then consume the cached logical ranges without
                // rebuilding an Editor display snapshot and copying selected
                // Buffer bodies again on every unrelated root render.
                let snapshot = editor.update(cx, |editor, cx| editor.selection_snapshot(cx));
                let item_index = snapshot
                    .items
                    .iter()
                    .find_map(|item| item.head.map(|_| item.item_index));
                let body_offset = snapshot.items.iter().find_map(|item| item.head);
                let rich_selection_changed = !this.buffer_view
                    && rich_vim_experiment()
                    && this.rich_navigation_selection.as_ref() != Some(&snapshot);
                let local_markdown_repaint = rich_selection_changed
                    && item_index.is_some_and(|item_index| {
                        this.rich_navigation_selection
                            .as_ref()
                            .is_some_and(|previous| {
                                previous.items.len() == 1
                                    && snapshot.items.len() == 1
                                    && previous.items[0].item_index == item_index
                                    && snapshot.items[0].item_index == item_index
                            })
                            && this.update_cached_markdown_navigation(item_index, &snapshot, cx)
                    });
                if !this.buffer_view && rich_vim_experiment() {
                    this.rich_navigation_selection = Some(snapshot);
                }
                if let Some(item_index) = item_index {
                    if this.focus_mode == FocusMode::Buffer {
                        this.transcript_cursor_initialized = true;
                    }
                    let changed = this.selected_item != item_index;
                    this.selected_item = item_index;
                    if !this.buffer_view && rich_vim_experiment() {
                        this.list_state.pause_following_tail();
                        this.reveal_rich_navigation_item(
                            item_index,
                            body_offset.unwrap_or_default(),
                        );
                    }
                    if changed || (rich_selection_changed && !local_markdown_repaint) {
                        cx.notify();
                    }
                } else if rich_selection_changed && !local_markdown_repaint {
                    cx.notify();
                }
            },
        )
        .detach();
        let mut model = replay_count
            .map(TranscriptModel::replay)
            .unwrap_or_default();
        mark_unbacked_requests_inactive(&mut model, &HashSet::default());
        let dirty_image_surfaces = model
            .items
            .iter()
            .filter(|item| item.kind == model::TranscriptKind::Image)
            .map(|item| item.key.clone())
            .collect();
        let list_state = ListState::new(model.items.len(), ListAlignment::Top, px(1600.));
        list_state.set_diagnostics_name("transcript");
        list_state.set_follow_mode(FollowMode::Tail);
        let task_list_state = ListState::new(0, ListAlignment::Top, px(54.));
        task_list_state.set_diagnostics_name("tasks");
        let transcript_focus = cx.focus_handle();
        if start_in_text_view {
            transcript_editor.focus_handle(cx).focus(window, cx);
        } else {
            composer.focus_handle(cx).focus(window, cx);
        }
        let palette_state = palette::load_state();

        let mut this = Self {
            cwd,
            replay_count,
            client: None,
            threads: Vec::new(),
            selected_thread_id: initial_thread_id,
            loaded_thread_updated_at: None,
            connecting: false,
            loading_thread: false,
            thread_read_only_reason: None,
            error: None,
            selected_item: model.items.len().saturating_sub(1),
            model,
            composer,
            composer_metrics,
            search_editor,
            transcript_editor,
            rich_navigation_selection: None,
            mode_indicator,
            buffer_view: start_in_text_view,
            transcript_focus,
            focus_mode: if start_in_text_view {
                FocusMode::Buffer
            } else {
                FocusMode::Composer
            },
            transcript_cursor_initialized: false,
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
            search_navigation_generation: 0,
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
            command_palette_history: palette_state.history,
            command_palette_usage: palette_state.usage,
            performance_reporter: PerformanceReporter::default(),
            performance_j_generation: 0,
            performance_j_run: None,
            performance_status: None,
            performance_status_generation: 0,
            dirty_image_surfaces,
            image_surfaces: HashMap::default(),
            hybrid_surfaces: HashMap::default(),
            rich_nested_scrolls: HashMap::default(),
            sidebar_open: true,
            sidebar_user_override: false,
            server_task: Task::ready(()),
            request_task: Task::ready(()),
            reconnect_task: Task::ready(()),
            read_only_refresh_task: Task::ready(()),
            reconnect_attempts: 0,
        };
        if rich_vim_experiment() && !start_in_text_view {
            this.transcript_editor
                .update(cx, |editor, cx| editor.set_input_only(true, cx));
        }
        if start_in_text_view || rich_vim_experiment() {
            drop(this.sync_transcript_document(cx));
        }
        if start_in_text_view {
            let selected_item = this.selected_item;
            this.transcript_editor.update(cx, |editor, cx| {
                editor.set_cursor_at_item_last_line(selected_item, window, cx);
                editor.reveal_tail(cx);
                editor.enter_normal_mode(window, cx);
            });
            this.transcript_cursor_initialized = true;
        }
        match automatic_performance_capture() {
            Some(AutomaticPerformanceCapture::Timed(capture_duration)) => {
                this.set_performance_status("Performance capture warming up…", None, cx);
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    if this
                        .update_in(cx, |this, window, cx| {
                            this.performance_reporter.mark_baseline(window);
                            this.set_performance_status(
                                format!(
                                    "Recording scroll performance for {:.0}s…",
                                    capture_duration.as_secs_f64()
                                ),
                                None,
                                cx,
                            );
                        })
                        .is_err()
                    {
                        return;
                    }

                    cx.background_executor().timer(capture_duration).await;
                    _ = this.update_in(cx, |this, window, cx| {
                        match this.performance_reporter.snapshot_report(window) {
                            Ok(report) => {
                                eprintln!(
                                    "completed automatic Harness performance capture\n{report}"
                                );
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(report));
                                this.set_performance_status(
                                    "Performance capture complete · report copied",
                                    Some(Duration::from_secs(4)),
                                    cx,
                                );
                            }
                            Err(error) => {
                                eprintln!(
                                    "failed automatic Harness performance capture: {error:#}"
                                );
                                this.set_performance_status(
                                    "Performance capture failed",
                                    Some(Duration::from_secs(4)),
                                    cx,
                                );
                            }
                        }
                    });
                })
                .detach();
            }
            Some(AutomaticPerformanceCapture::UntilClose) => {
                this.performance_reporter.mark_baseline(window);
                this.set_performance_status(
                    "Performance capture armed · close Harness when done",
                    Some(Duration::from_secs(4)),
                    cx,
                );
                let owner = cx.weak_entity();
                window.on_window_should_close(cx, move |window, cx| {
                    _ = owner.update(cx, |this, _| {
                        match this.performance_reporter.snapshot_report(window) {
                            Ok(report) => eprintln!(
                                "completed close-triggered Harness performance capture\n{report}"
                            ),
                            Err(error) => eprintln!(
                                "failed close-triggered Harness performance capture: {error:#}"
                            ),
                        }
                    });
                    true
                });
            }
            None => {}
        }
        if replay_count.is_none() {
            this.connect(cx);
        }
        this
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.connecting || self.client.is_some() || self.replay_count.is_some() {
            return;
        }
        self.connecting = true;
        self.error = None;
        let initial_thread_query = self
            .selected_thread_id
            .clone()
            .or_else(|| std::env::var("HARNESS_OPEN_THREAD").ok());
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
                            this.reconnect_attempts = 0;
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
                            this.error = Some(
                                if this.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                                    format!(
                                        "Could not reconnect to Codex: {error}. Refresh the task list to try again."
                                    )
                                } else {
                                    format!("Could not connect to Codex: {error}")
                                }
                                .into(),
                            );
                            this.schedule_reconnect(cx);
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
            _ = this.update(cx, |this, cx| this.handle_server_disconnect(&client, cx));
        });
    }

    fn schedule_reconnect(&mut self, cx: &mut Context<Self>) {
        if self.client.is_some() || self.connecting || self.replay_count.is_some() {
            return;
        }
        let Some(delay) = reconnect_delay(self.reconnect_attempts) else {
            return;
        };
        self.reconnect_attempts += 1;
        let attempt = self.reconnect_attempts;
        self.error = Some(
            format!(
                "Codex app server disconnected. Reconnecting in {}s ({attempt}/{MAX_RECONNECT_ATTEMPTS})…",
                delay.as_secs()
            )
            .into(),
        );
        self.reconnect_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            _ = this.update(cx, |this, cx| {
                if this.client.is_none() && !this.connecting {
                    this.connect(cx);
                }
            });
        });
    }

    fn handle_server_disconnect(
        &mut self,
        disconnected_client: &Rc<Client>,
        cx: &mut Context<Self>,
    ) {
        let is_current_client = self
            .client
            .as_ref()
            .is_some_and(|client| Rc::ptr_eq(client, disconnected_client));
        if !is_current_client {
            return;
        }

        let dirty_requests = self
            .model
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                (item
                    .pending_request
                    .as_ref()
                    .is_some_and(|request| !request.resolved)
                    && self.live_request_keys.contains(&item.key))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        self.client = None;
        self.connecting = false;
        self.read_only_refresh_task = Task::ready(());
        self.model.current_turn_id = None;
        self.thread_read_only_reason = None;
        self.error = Some("Codex app server disconnected.".into());
        self.retire_all_request_surfaces();
        mark_unbacked_requests_inactive(&mut self.model, &self.live_request_keys);
        for index in &dirty_requests {
            self.list_state.splice(*index..*index + 1, 1);
        }
        if (self.buffer_view || rich_vim_experiment()) && !dirty_requests.is_empty() {
            let item_count = self.model.items.len();
            if !self.sync_transcript_item_updates(item_count, &dirty_requests, cx) {
                drop(self.sync_transcript_document(cx));
            }
        }
        self.schedule_reconnect(cx);
        cx.notify();
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
            self.update_search_matches_for_changes(old_len, &dirty_items);
        }
        if document_changed && (self.buffer_view || rich_vim_experiment()) {
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

    fn sync_hybrid_surfaces(&mut self, cx: &mut Context<Self>) {
        let candidates = self
            .model
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| self.buffer_view && item_uses_hybrid_surface(item))
            .map(|(index, item)| (index, item.clone()))
            .collect::<Vec<_>>();
        let desired_keys = candidates
            .iter()
            .map(|(_, item)| item.key.clone())
            .collect::<HashSet<_>>();
        let stale_keys = self
            .hybrid_surfaces
            .keys()
            .filter(|key| !desired_keys.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for item_key in stale_keys {
            self.hybrid_surfaces.remove(&item_key);
            self.transcript_editor.update(cx, |editor, cx| {
                editor.remove_replacement(&hybrid_replacement_key(&item_key), cx);
            });
        }

        for (index, item) in candidates {
            let surface = if let Some(surface) = self.hybrid_surfaces.get(&item.key) {
                surface.update(cx, |surface, cx| surface.update(item.clone(), index, cx));
                surface.clone()
            } else {
                let owner = cx.weak_entity();
                let surface = cx.new(|_| HybridStructuredSurface::new(item.clone(), index, owner));
                self.hybrid_surfaces
                    .insert(item.key.clone(), surface.clone());
                surface
            };
            let rows = hybrid_structured_rows(&item);
            self.transcript_editor.update(cx, |editor, cx| {
                editor.upsert_replacement(
                    TranscriptReplacement::new(
                        hybrid_replacement_key(&item.key),
                        item.key,
                        rows,
                        surface.into(),
                    ),
                    cx,
                );
            });
        }
    }

    fn sync_request_surfaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = std::mem::take(&mut self.dirty_request_surfaces);
        let composer_empty = self.composer.read(cx).text(cx).trim().is_empty();
        let focus_mode = self.focus_mode;
        let mut auto_focus_request: Option<(usize, String)> = None;
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

                    let newly_mounted = !self.request_surfaces.contains_key(&item_key);
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
                    if request_should_take_focus(
                        newly_mounted,
                        is_live,
                        unresolved,
                        composer_empty,
                        focus_mode,
                    ) && let Some(index) = self
                        .model
                        .items
                        .iter()
                        .position(|item| item.key == item_key)
                        && auto_focus_request
                            .as_ref()
                            .is_none_or(|(current_index, _)| index > *current_index)
                    {
                        auto_focus_request = Some((index, item_key.clone()));
                    }
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
        if let Some((_, item_key)) = auto_focus_request {
            cx.defer_in(window, move |this, window, cx| {
                let composer_empty = this.composer.read(cx).text(cx).trim().is_empty();
                let Some(index) = this
                    .model
                    .items
                    .iter()
                    .position(|item| item.key == item_key)
                else {
                    return;
                };
                let unresolved = this.model.items[index]
                    .pending_request
                    .as_ref()
                    .is_some_and(|request| !request.resolved);
                if !request_should_take_focus(
                    true,
                    this.live_request_keys.contains(&item_key),
                    unresolved,
                    composer_empty,
                    this.focus_mode,
                ) {
                    return;
                }
                this.selected_item = index;
                this.focus_selected_request_surface(window, cx);
            });
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
        let mut existing_updates = dirty_items
            .iter()
            .copied()
            .filter(|item_index| *item_index < old_model_item_count)
            .map(|item_index| {
                let projection = if self.buffer_view {
                    self.model.item_projection(item_index)
                } else {
                    rich_navigation_item_projection(&self.model, item_index)
                };
                (item_index, projection)
            })
            .collect::<Vec<_>>();
        let appended = (old_model_item_count..new_model_item_count)
            .map(|item_index| {
                if self.buffer_view {
                    self.model.item_projection(item_index)
                } else {
                    rich_navigation_item_projection(&self.model, item_index)
                }
            })
            .collect::<Vec<_>>();

        // Rich projections deliberately omit the final separator so `G`
        // cannot enter an unpainted EOF row. When a visible item is appended,
        // the formerly-final item must gain that separator before the new
        // projection is appended. Make that boundary transfer an ordinary
        // incremental item update instead of forcing a full document rebuild.
        if !self.buffer_view
            && appended.iter().any(Option::is_some)
            && let Some(previous_last) = (0..old_model_item_count)
                .rev()
                .find(|index| self.model.items[*index].is_presentationally_visible())
            && !existing_updates
                .iter()
                .any(|(item_index, _)| *item_index == previous_last)
        {
            existing_updates.push((
                previous_last,
                rich_navigation_item_projection(&self.model, previous_last),
            ));
        }

        let applied = self.transcript_editor.update(cx, |editor, cx| {
            editor.apply_item_projections(old_model_item_count, &existing_updates, &appended, cx)
        });
        if applied && !self.buffer_view && rich_vim_experiment() {
            self.rich_navigation_selection = None;
        }
        applied
    }

    fn sync_transcript_document(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<model::TranscriptDocument> {
        if !self.buffer_view && !rich_vim_experiment() {
            return None;
        }
        let document = if self.buffer_view {
            self.model.full_document()
        } else {
            rich_navigation_document(&self.model)
        };
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
        if !self.buffer_view && rich_vim_experiment() {
            self.rich_navigation_selection = None;
        }
        Some(document)
    }

    fn refresh_threads(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            if self.replay_count.is_none() {
                self.reconnect_attempts = 0;
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

    fn schedule_read_only_refresh(&mut self, mut active: bool, cx: &mut Context<Self>) {
        let (Some(client), Some(thread_id)) =
            (self.client.clone(), self.selected_thread_id.clone())
        else {
            return;
        };
        self.read_only_refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(if active {
                        READ_ONLY_ACTIVE_REFRESH
                    } else {
                        READ_ONLY_IDLE_REFRESH
                    })
                    .await;

                let Ok((still_selected, loaded_updated_at)) = this.update(cx, |this, _| {
                    (
                        this.selected_thread_id.as_deref() == Some(thread_id.as_str())
                            && this.thread_read_only_reason.is_some()
                            && this
                                .client
                                .as_ref()
                                .is_some_and(|current| Rc::ptr_eq(current, &client)),
                        this.loaded_thread_updated_at,
                    )
                }) else {
                    return;
                };
                if !still_selected {
                    return;
                }

                if !active {
                    let response = match client.list_threads(THREAD_LIMIT, None).await {
                        Ok(response) => response,
                        Err(error) => {
                            log::debug!("could not check read-only task freshness: {error}");
                            continue;
                        }
                    };
                    let selected_updated_at = response
                        .data
                        .iter()
                        .find(|thread| thread.id == thread_id)
                        .map(|thread| thread.updated_at);
                    let should_read = selected_updated_at
                        .is_some_and(|updated_at| Some(updated_at) != loaded_updated_at);
                    let Ok(still_selected) = this.update(cx, |this, cx| {
                        if this.selected_thread_id.as_deref() != Some(thread_id.as_str())
                            || this.thread_read_only_reason.is_none()
                        {
                            return false;
                        }
                        if this.threads != response.data {
                            let selected_task_id = this
                                .threads
                                .get(this.selected_task)
                                .map(|thread| thread.id.clone());
                            let old_len = this.threads.len();
                            this.threads = response.data;
                            this.task_list_state.splice(0..old_len, this.threads.len());
                            this.selected_task = selected_task_id
                                .as_deref()
                                .and_then(|selected_id| {
                                    this.threads
                                        .iter()
                                        .position(|thread| thread.id == selected_id)
                                })
                                .unwrap_or_else(|| {
                                    this.selected_task.min(this.threads.len().saturating_sub(1))
                                });
                            cx.notify();
                        }
                        true
                    }) else {
                        return;
                    };
                    if !still_selected {
                        return;
                    }
                    if !should_read {
                        continue;
                    }
                }

                let thread = match client.read_thread(&thread_id).await {
                    Ok(thread) => thread,
                    Err(error) => {
                        log::debug!("could not refresh read-only task {thread_id}: {error}");
                        continue;
                    }
                };
                active = thread_has_active_turn(&thread);
                if this
                    .update(cx, |this, cx| {
                        this.apply_read_only_thread_refresh(thread, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    fn apply_read_only_thread_refresh(&mut self, thread: CodexThread, cx: &mut Context<Self>) {
        if self.selected_thread_id.as_deref() != Some(thread.id.as_str())
            || self.thread_read_only_reason.is_none()
        {
            return;
        }

        let was_following_tail = if self.buffer_view {
            self.transcript_editor.read(cx).is_following_tail()
        } else {
            self.list_state.is_following_tail()
        };
        let selected_key = self
            .model
            .items
            .get(self.selected_item)
            .map(|item| item.key.clone());
        self.loaded_thread_updated_at = Some(thread.updated_at);
        if !thread.cwd.is_empty() {
            self.cwd = thread.cwd.clone();
        }

        let outcome = self.model.refresh_thread(&thread);
        let mut dirty_items = outcome.dirty.into_iter().collect::<Vec<_>>();
        dirty_items.sort_unstable();
        if !outcome.reset && outcome.old_len == outcome.new_len && dirty_items.is_empty() {
            return;
        }

        if outcome.reset {
            self.list_state.splice(0..outcome.old_len, outcome.new_len);
        } else {
            if outcome.new_len > outcome.old_len {
                self.list_state.splice(
                    outcome.old_len..outcome.old_len,
                    outcome.new_len - outcome.old_len,
                );
            }
            for index in &dirty_items {
                if *index < outcome.old_len {
                    self.list_state.splice(*index..*index + 1, 1);
                }
            }
        }

        for index in &dirty_items {
            if let Some(item) = self.model.items.get(*index) {
                self.markdown_cache.remove(&item.key);
            }
        }
        self.track_image_surface_updates(outcome.old_len, outcome.new_len, &dirty_items);
        mark_unbacked_requests_inactive(&mut self.model, &self.live_request_keys);

        self.selected_item = if was_following_tail {
            self.model.items.len().saturating_sub(1)
        } else {
            selected_key
                .as_deref()
                .and_then(|key| self.model.items.iter().position(|item| item.key == key))
                .unwrap_or_else(|| {
                    self.selected_item
                        .min(self.model.items.len().saturating_sub(1))
                })
        };
        if !self.search_query.is_empty() {
            if outcome.reset {
                self.rebuild_search_matches();
            } else {
                self.update_search_matches_for_changes(outcome.old_len, &dirty_items);
            }
        }

        if self.buffer_view || rich_vim_experiment() {
            let incrementally_applied = !outcome.reset
                && self.sync_transcript_item_updates(outcome.old_len, &dirty_items, cx);
            if !incrementally_applied {
                drop(self.sync_transcript_document(cx));
            }
        } else if was_following_tail {
            self.list_state.scroll_to_end();
        }
        cx.notify();
    }

    fn open_thread(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.get(index) else {
            return;
        };
        let thread_id = thread.id.clone();
        let Some(client) = self.client.clone() else {
            return;
        };

        self.reject_pending_requests(cx);
        self.read_only_refresh_task = Task::ready(());
        self.selected_thread_id = Some(thread_id.clone());
        self.loaded_thread_updated_at = None;
        self.loading_thread = true;
        self.thread_read_only_reason = None;
        self.error = None;
        let old_len = self.model.items.len();
        self.model.clear();
        self.mark_all_image_surfaces_dirty();
        self.markdown_cache.clear();
        self.rich_nested_scrolls.clear();
        self.raw_visible.clear();
        self.request_answers.clear();
        self.request_editors.clear();
        self.request_question_cursor.clear();
        self.request_option_cursor.clear();
        self.retire_all_request_surfaces();
        self.list_state.splice(0..old_len, 0);
        self.selected_item = 0;
        self.transcript_cursor_initialized = false;
        drop(self.sync_transcript_document(cx));
        cx.notify();

        self.request_task = cx.spawn(async move |this, cx| {
            let read = client.read_thread(&thread_id).await;
            let result = match read {
                Ok(read_thread) => match client.resume_thread(&thread_id).await {
                    Ok(resumed) => Ok((resumed, None)),
                    Err(error) => {
                        log::warn!("could not resume task {thread_id}; opening read-only: {error}");
                        Ok((
                            read_thread,
                            Some(
                                "Another Codex client owns this task. History is available read-only."
                                    .to_string(),
                            ),
                        ))
                    }
                },
                Err(error) => Err(error),
            };
            if this
                .update(cx, |this, cx| {
                    this.loading_thread = false;
                    match result {
                        Ok((thread, warning)) => {
                            let read_only = warning.is_some();
                            let active = thread_has_active_turn(&thread);
                            this.load_thread(thread, cx);
                            this.thread_read_only_reason = warning.map(Into::into);
                            if read_only {
                                this.schedule_read_only_refresh(active, cx);
                            }
                            this.error = None;
                        }
                        Err(error) => {
                            this.thread_read_only_reason = Some(
                                "This task could not be loaded. Choose another task or start a new one."
                                    .into(),
                            );
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
        self.loaded_thread_updated_at = Some(thread.updated_at);
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
        mark_unbacked_requests_inactive(&mut self.model, &self.live_request_keys);
        if slow_list_diagnostics_enabled() {
            for (index, item) in self.model.items.iter().enumerate() {
                eprintln!(
                    "transcript-item item={index} kind={:?} content_bytes={} events={}",
                    item.kind,
                    item.content.len(),
                    item.event_count,
                );
            }
        }
        self.markdown_cache.clear();
        self.rich_nested_scrolls.clear();
        self.raw_visible.clear();
        self.request_answers.clear();
        self.request_editors.clear();
        self.request_question_cursor.clear();
        self.request_option_cursor.clear();
        self.retire_all_request_surfaces();
        self.mark_all_image_surfaces_dirty();
        self.list_state.splice(0..old_len, self.model.items.len());
        self.selected_item = self.model.items.len().saturating_sub(1);
        self.transcript_cursor_initialized = false;
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
        self.read_only_refresh_task = Task::ready(());
        let old_len = self.model.items.len();
        self.model.clear();
        self.mark_all_image_surfaces_dirty();
        self.markdown_cache.clear();
        self.rich_nested_scrolls.clear();
        self.raw_visible.clear();
        self.request_answers.clear();
        self.request_editors.clear();
        self.request_question_cursor.clear();
        self.request_option_cursor.clear();
        self.retire_all_request_surfaces();
        self.list_state.splice(0..old_len, 0);
        self.selected_thread_id = None;
        self.loaded_thread_updated_at = None;
        self.selected_item = 0;
        self.transcript_cursor_initialized = false;
        self.thread_read_only_reason = None;
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
        if composer_send_blocked(
            text.is_empty(),
            self.loading_thread,
            self.thread_read_only_reason.is_some(),
            self.client.is_some() || self.replay_count.is_some(),
        ) {
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
        if request.resolved || !self.live_request_keys.contains(&request_key) {
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
        if self.buffer_view || rich_vim_experiment() {
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
                        if this.buffer_view || rich_vim_experiment() {
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
        let live_request_keys = self.live_request_keys.clone();
        let pending = self
            .model
            .items
            .iter_mut()
            .filter_map(|item| {
                if !live_request_keys.contains(&item.key) {
                    return None;
                }
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
        if self.buffer_view || rich_vim_experiment() {
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
        if rich_vim_experiment() {
            let document = self.sync_transcript_document(cx);
            let target_item = document.as_ref().and_then(|document| {
                document
                    .segments
                    .iter()
                    .rev()
                    .find(|segment| segment.item_index <= self.selected_item)
                    .map(|segment| segment.item_index)
            });
            let current_item = self
                .transcript_editor
                .update(cx, |editor, cx| editor.selected_item(cx));
            let entry_placement = rich_transcript_entry_placement(
                self.transcript_cursor_initialized,
                current_item,
                target_item,
            );
            self.focus_mode = FocusMode::Buffer;
            if let Some(target_item) = entry_placement {
                self.selected_item = target_item;
                self.transcript_editor.update(cx, |editor, cx| {
                    editor.set_cursor_at_item_last_line(target_item, window, cx);
                });
            }
            self.transcript_cursor_initialized = true;
            self.transcript_editor.focus_handle(cx).focus(window, cx);
            self.list_state.scroll_to_reveal_item(self.selected_item);
            cx.defer_in(window, |this, window, cx| {
                if !this.buffer_view && this.focus_mode == FocusMode::Buffer {
                    this.transcript_editor.update(cx, |editor, cx| {
                        editor.enter_normal_mode(window, cx);
                    });
                    cx.notify();
                }
            });
            cx.notify();
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
        let rich_cursor = (rich_vim_experiment() && self.buffer_view)
            .then(|| {
                self.transcript_editor
                    .update(cx, |editor, cx| editor.selection_snapshot(cx))
            })
            .and_then(|snapshot| {
                snapshot
                    .items
                    .into_iter()
                    .find_map(|item| item.head.map(|head| (item.item_index, head)))
            });
        let was_following_tail =
            self.buffer_view && self.transcript_editor.read(cx).is_following_tail();
        let top_visible_item = self
            .buffer_view
            .then(|| {
                self.transcript_editor
                    .update(cx, |editor, cx| editor.top_visible_item(cx))
            })
            .flatten();
        let preserved_viewport = top_visible_item.is_some();
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
        if rich_vim_experiment() {
            self.transcript_editor
                .update(cx, |editor, cx| editor.set_input_only(true, cx));
        }
        self.focus_mode = if rich_vim_experiment() {
            FocusMode::Buffer
        } else {
            FocusMode::Transcript
        };
        if rich_vim_experiment() {
            drop(self.sync_transcript_document(cx));
            if let Some((item_index, body_offset)) = rich_cursor {
                self.transcript_editor.update(cx, |editor, cx| {
                    editor.set_cursor_in_item(item_index, body_offset, window, cx);
                });
            }
            self.transcript_editor.focus_handle(cx).focus(window, cx);
        } else {
            self.transcript_focus.focus(window, cx);
        }
        if was_following_tail {
            self.list_state.set_follow_mode(FollowMode::Tail);
        } else {
            self.list_state.pause_following_tail();
            self.list_state.scroll_to(gpui::ListOffset {
                item_ix: top_visible_item.unwrap_or(self.selected_item),
                offset_in_item: px(0.),
            });
        }
        cx.defer_in(window, move |this, _, cx| {
            if !this.buffer_view
                && matches!(this.focus_mode, FocusMode::Transcript | FocusMode::Buffer)
            {
                if this.list_state.is_following_tail() {
                    this.list_state.scroll_to_end();
                } else if !preserved_viewport {
                    let selected_visible = this
                        .list_state
                        .bounds_for_item(this.selected_item)
                        .is_some_and(|bounds| {
                            bounds.intersects(&this.list_state.viewport_bounds())
                        });
                    if !selected_visible {
                        this.list_state.scroll_to_reveal_item(this.selected_item);
                    }
                }
                cx.notify();
            }
        });
        if rich_vim_experiment() {
            cx.defer_in(window, |this, window, cx| {
                if !this.buffer_view && this.focus_mode == FocusMode::Buffer {
                    this.transcript_editor.update(cx, |editor, cx| {
                        editor.enter_normal_mode(window, cx);
                    });
                }
            });
        }
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
        let top_visible_item = (!should_follow_tail).then(|| {
            self.list_state
                .logical_scroll_top()
                .item_ix
                .min(self.model.items.len().saturating_sub(1))
        });
        self.buffer_view = true;
        self.transcript_editor
            .update(cx, |editor, cx| editor.set_input_only(false, cx));
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
        let top_row = top_visible_item.and_then(|item_index| {
            document
                .item_rows
                .get(item_index)
                .and_then(|row| *row)
                .or_else(|| {
                    document.item_rows[..item_index.min(document.item_rows.len())]
                        .iter()
                        .rev()
                        .find_map(|row| *row)
                })
        });
        self.focus_mode = FocusMode::Buffer;
        self.transcript_editor.update(cx, |editor, cx| {
            editor.set_cursor_row(row, window, cx);
            if should_follow_tail {
                editor.reveal_tail(cx);
            } else {
                if let Some(top_row) = top_row {
                    editor.reveal_row_at_top(top_row, cx);
                } else {
                    editor.pause_tail_follow();
                }
            }
        });
        self.transcript_editor.focus_handle(cx).focus(window, cx);
        cx.defer_in(window, |this, window, cx| {
            if this.buffer_view {
                this.transcript_editor.update(cx, |editor, cx| {
                    editor.enter_normal_mode(window, cx);
                    editor.refresh_after_becoming_visible(cx);
                });
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

    fn use_transcript_typography(
        &mut self,
        profile: TranscriptTypographyProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.transcript_editor.update(cx, |editor, cx| {
            editor.set_typography_profile(profile, window, cx);
        });
        self.composer.update(cx, |composer, cx| {
            composer.set_typography_profile(profile, window, cx);
        });
        self.show_text_transcript(window, cx);
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
                    if let Err(error) = palette::save_state(
                        &this.command_palette_history,
                        &this.command_palette_usage,
                    ) {
                        log::warn!("could not persist command palette state: {error}");
                    }
                    // Palette events are delivered while `HarnessApp` is already being
                    // updated. Dispatching a root action synchronously here attempts to
                    // update the same entity again and panics in GPUI's entity map.
                    // Return the current update first, then route the confirmed action
                    // through the normal window action path.
                    window.defer(cx, move |window, cx| {
                        window.dispatch_action(command.action, cx);
                    });
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

    fn copy_performance_report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.performance_reporter.snapshot_report(window) {
            Ok(report) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(report));
                log::info!("copied Harness performance report to the clipboard");
            }
            Err(error) => {
                log::error!("failed to build Harness performance report: {error:#}");
            }
        }
    }

    fn set_performance_status(
        &mut self,
        message: impl Into<SharedString>,
        clear_after: Option<Duration>,
        cx: &mut Context<Self>,
    ) {
        self.performance_status_generation = self.performance_status_generation.wrapping_add(1);
        let generation = self.performance_status_generation;
        self.performance_status = Some(message.into());
        cx.notify();

        let Some(delay) = clear_after else {
            return;
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            _ = this.update(cx, |this, cx| {
                if this.performance_status_generation == generation {
                    this.performance_status = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn performance_j_ready(&self, window: &Window, cx: &App) -> bool {
        (self.buffer_view || rich_vim_experiment())
            && self.focus_mode == FocusMode::Buffer
            && self.transcript_editor.focus_handle(cx).is_focused(window)
    }

    fn schedule_performance_j_step(generation: u64, window: &mut Window, cx: &mut Context<Self>) {
        cx.on_next_frame(window, move |this, window, cx| {
            this.performance_j_step(generation, window, cx);
        });
    }

    fn dispatch_performance_j_keys(
        generation: u64,
        keys: Vec<Keystroke>,
        failure_reason: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `dispatch_keystroke` may synchronously draw the window. A frame callback
        // entered through `Context<Self>::on_next_frame` still holds HarnessApp's
        // entity lease, so dispatching there would make the draw try to render an
        // already-updating root entity. Defer only the actual input dispatch; once
        // it returns, reacquire HarnessApp to advance the frame-paced state machine.
        let this = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            let handled = keys
                .into_iter()
                .all(|keystroke| window.dispatch_keystroke(keystroke, cx));
            this.update(cx, |this, cx| {
                if handled {
                    Self::schedule_performance_j_step(generation, window, cx);
                } else {
                    this.cancel_performance_j(generation, failure_reason, cx);
                }
            })
            .ok();
        });
    }

    fn cancel_performance_j(&mut self, generation: u64, reason: &str, cx: &mut Context<Self>) {
        if self
            .performance_j_run
            .as_ref()
            .is_some_and(|driver| driver.state.generation == generation)
        {
            self.performance_j_run = None;
            self.set_performance_status(
                format!("Performance run cancelled: {reason}"),
                Some(PERFORMANCE_STATUS_DURATION),
                cx,
            );
            eprintln!("cancelled Harness :perf-j run: {reason}");
            log::warn!("cancelled Harness :perf-j run: {reason}");
        }
    }

    fn performance_j_step(&mut self, generation: u64, window: &mut Window, cx: &mut Context<Self>) {
        let pending_motion_origin = self
            .performance_j_run
            .as_mut()
            .filter(|driver| driver.state.generation == generation)
            .and_then(|driver| driver.pending_motion_origin.take());
        if let Some(origin) = pending_motion_origin {
            let current = self
                .transcript_editor
                .update(cx, |editor, cx| editor.cursor_offset(cx));
            if current == origin {
                self.cancel_performance_j(
                    generation,
                    "Vim motion was handled without moving the native cursor",
                    cx,
                );
                return;
            }
        }

        let (step, key) = {
            let Some(driver) = self.performance_j_run.as_mut() else {
                return;
            };
            let Some(step) = driver.state.next_step(generation) else {
                return;
            };
            let key = match step {
                PerformanceJStep::Dispatch { down: true } => Some(driver.j.clone()),
                PerformanceJStep::Dispatch { down: false } => Some(driver.k.clone()),
                PerformanceJStep::Prepare
                | PerformanceJStep::Baseline
                | PerformanceJStep::Report => None,
            };
            (step, key)
        };

        if step != PerformanceJStep::Report && !self.performance_j_ready(window, cx) {
            self.cancel_performance_j(generation, "Vim transcript navigation lost focus", cx);
            return;
        }

        match step {
            PerformanceJStep::Prepare => {
                self.transcript_editor.update(cx, |editor, cx| {
                    editor.enter_normal_mode(window, cx);
                });
                self.set_performance_status(
                    format!("Running {PERFORMANCE_J_STEPS}-frame Vim j/k performance sample…"),
                    None,
                    cx,
                );
                Self::schedule_performance_j_step(generation, window, cx);
            }
            PerformanceJStep::Baseline => {
                self.performance_reporter.mark_baseline(window);
                Self::schedule_performance_j_step(generation, window, cx);
            }
            PerformanceJStep::Dispatch { down } => {
                let Some(motion) = key else {
                    self.cancel_performance_j(generation, "Vim motion key was unavailable", cx);
                    return;
                };
                let origin = self
                    .transcript_editor
                    .update(cx, |editor, cx| editor.cursor_offset(cx));
                if let Some(driver) = self.performance_j_run.as_mut() {
                    driver.pending_motion_origin = Some(origin);
                }
                Self::dispatch_performance_j_keys(
                    generation,
                    vec![motion],
                    if down {
                        "Vim j was not handled"
                    } else {
                        "Vim k was not handled"
                    },
                    window,
                    cx,
                );
            }
            PerformanceJStep::Report => {
                self.performance_j_run = None;
                match self.performance_reporter.snapshot_benchmark_report(window) {
                    Ok(report) => {
                        // Keep the interactive clipboard handoff, but also put
                        // the benchmark body in the application log. On
                        // Wayland the clipboard is owned by this process, so a
                        // headless validation run otherwise loses its only
                        // report as soon as the harness exits.
                        eprintln!("completed Harness :perf-j run\n{report}");
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(report));
                        self.set_performance_status(
                            format!(
                                "{PERFORMANCE_J_STEPS}-frame performance report copied to clipboard"
                            ),
                            Some(PERFORMANCE_STATUS_DURATION),
                            cx,
                        );
                    }
                    Err(error) => {
                        self.set_performance_status(
                            "Performance run completed, but its report failed",
                            Some(PERFORMANCE_STATUS_DURATION),
                            cx,
                        );
                        log::error!("failed to build Harness :perf-j report: {error:#}");
                    }
                }
            }
        }
    }

    fn run_performance_j(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.performance_j_generation = self.performance_j_generation.wrapping_add(1);
        let generation = self.performance_j_generation;
        self.performance_j_run = None;
        let Some(candidate) = performance_j_candidate(&self.model) else {
            self.set_performance_status(
                ":perf-j needs a completed multi-line Markdown message",
                Some(PERFORMANCE_STATUS_DURATION),
                cx,
            );
            log::warn!("Harness :perf-j could not find a multi-line Markdown message");
            return;
        };
        let j = match Keystroke::parse("j") {
            Ok(keystroke) => keystroke,
            Err(error) => {
                self.set_performance_status(
                    "Performance run could not prepare its Vim key",
                    Some(PERFORMANCE_STATUS_DURATION),
                    cx,
                );
                log::error!("failed to parse Harness :perf-j measurement key: {error}");
                return;
            }
        };
        let k = match Keystroke::parse("k") {
            Ok(keystroke) => keystroke,
            Err(error) => {
                self.set_performance_status(
                    "Performance run could not prepare its Vim key",
                    Some(PERFORMANCE_STATUS_DURATION),
                    cx,
                );
                log::error!("failed to parse Harness :perf-j measurement key: {error}");
                return;
            }
        };
        self.selected_item = candidate;
        if rich_vim_experiment() && !self.buffer_view {
            self.focus_transcript(window, cx);
        } else {
            self.show_text_transcript(window, cx);
        }
        self.transcript_editor.update(cx, |editor, cx| {
            editor.set_cursor_in_item(candidate, 0, window, cx);
        });

        self.performance_j_run = Some(PerformanceJDriver::new(generation, j, k));
        self.set_performance_status("Preparing Vim j/k performance sample…", None, cx);
        log::info!("starting Harness :perf-j run ({PERFORMANCE_J_STEPS} frames)");
        Self::schedule_performance_j_step(generation, window, cx);
    }

    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_mode = FocusMode::Composer;
        self.composer.focus_handle(cx).focus(window, cx);
        cx.defer_in(window, |this, window, cx| {
            if this.focus_mode == FocusMode::Composer {
                this.composer
                    .update(cx, |composer, cx| composer.enter_insert_mode(window, cx));
            }
        });
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
                self.focus_mode = if self.buffer_view {
                    FocusMode::Buffer
                } else {
                    FocusMode::Transcript
                };
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
        if self.selected_item + 1 >= self.model.items.len() && delta > 0 {
            self.list_state.set_follow_mode(FollowMode::Tail);
        } else {
            self.list_state.pause_following_tail();
            self.list_state.scroll_to_reveal_item(self.selected_item);
        }
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
        if self
            .model
            .items
            .get(self.selected_item)
            .is_some_and(|item| self.request_surfaces.contains_key(&item.key))
        {
            self.focus_selected_request_surface(window, cx);
            return;
        }
        if let Some(method) = self
            .model
            .items
            .get(self.selected_item)
            .filter(|item| self.live_request_keys.contains(&item.key))
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
            if item.content.trim().is_empty() {
                return;
            }
            item.expanded = !item.expanded;
            let item_key = item.key.clone();
            let collapsed = !item.expanded;
            self.list_state
                .splice(self.selected_item..self.selected_item + 1, 1);
            if rich_vim_experiment() {
                self.transcript_editor.update(cx, |editor, cx| {
                    editor.set_item_collapsed(&item_key, collapsed, window, cx);
                });
            }
            cx.notify();
        }
    }

    fn toggle_selected_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_selected(window, cx);
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
                    item_matches_folded_query(item, &query).then_some(index)
                })
                .collect()
        };
        self.active_search_match = self
            .active_search_match
            .min(self.search_matches.len().saturating_sub(1));
    }

    fn update_search_matches_for_changes(&mut self, old_len: usize, dirty_items: &[usize]) {
        let query = self.search_query.to_lowercase();
        if query.is_empty() {
            self.search_matches.clear();
            self.active_search_match = 0;
            return;
        }

        let active_item = self.search_matches.get(self.active_search_match).copied();
        let item_count = self.model.items.len();
        self.search_matches.retain(|index| *index < item_count);

        let mut changed = dirty_items.to_vec();
        changed.extend(old_len.min(item_count)..item_count);
        changed.sort_unstable();
        changed.dedup();
        for index in changed {
            let Some(item) = self.model.items.get(index) else {
                continue;
            };
            reconcile_sorted_search_match(
                &mut self.search_matches,
                index,
                item_matches_folded_query(item, &query),
            );
        }

        self.active_search_match = active_item
            .and_then(|active_item| self.search_matches.binary_search(&active_item).ok())
            .unwrap_or_else(|| {
                self.search_matches
                    .partition_point(|index| *index < self.selected_item)
                    .min(self.search_matches.len().saturating_sub(1))
            });
    }

    fn jump_to_search_match(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.search_matches.get(self.active_search_match).copied() {
            self.selected_item = index;
            self.search_navigation_generation = self.search_navigation_generation.wrapping_add(1);
            self.list_state.pause_following_tail();
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
        let thread_id = thread.id.clone();
        let thread_cwd = if thread.cwd.is_empty() {
            self.cwd.clone()
        } else {
            thread.cwd.clone()
        };
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
        let thread_item = ThreadItem::new(("task", index), title)
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
            .into_any_element();

        right_click_menu(format!("thread-context-menu-{index}"))
            .trigger(move |_, _, _| thread_item)
            .menu(move |window, cx| {
                let thread_id = thread_id.clone();
                let thread_cwd = thread_cwd.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(ContextMenuEntry::new("Open in New Window").handler({
                        let thread_id = thread_id.clone();
                        let thread_cwd = thread_cwd.clone();
                        move |_, cx| {
                            open_harness_window(
                                thread_cwd.clone(),
                                None,
                                false,
                                Some(thread_id.clone()),
                                cx,
                            );
                        }
                    }))
                })
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
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        autoscroll_generation: Option<u64>,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        let cached = self
            .markdown_cache
            .entry(key.to_string())
            .or_insert_with(|| {
                let entity = cx.new(|cx| Markdown::new(source.to_string().into(), None, None, cx));
                CachedMarkdown {
                    source: source.to_string(),
                    entity,
                    search_query: None,
                    search_ranges: Vec::new(),
                    navigation: None,
                    last_autoscroll_generation: None,
                }
            });
        if cached.source != source {
            cached.source = source.to_string();
            cached.search_query = None;
            cached.search_ranges.clear();
            cached.navigation = None;
            cached.last_autoscroll_generation = None;
            cached.entity.update(cx, |markdown, cx| {
                markdown.reset(source.to_string().into(), cx)
            });
        }

        let markdown_navigation =
            navigation.map(|navigation| navigation.markdown_source_navigation(source));
        if cached.navigation != markdown_navigation {
            cached.navigation = markdown_navigation.clone();
            cached.entity.update(cx, |markdown, cx| {
                let navigation = markdown_navigation.as_ref();
                markdown.set_external_navigation(
                    navigation.map(|navigation| navigation.selections.clone()),
                    navigation.and_then(|navigation| navigation.cursor),
                    cx,
                )
            });
        }

        let desired = if let Some(search) = search {
            if cached.search_query.as_deref() != Some(search.query.as_ref()) {
                cached.search_query = Some(search.query.to_string());
                cached.search_ranges =
                    folded_match_byte_ranges(source, &search.query, RICH_SEARCH_HIGHLIGHT_LIMIT);
            }
            search.decorate_ranges(cached.search_ranges.clone())
        } else {
            cached.search_query = None;
            cached.search_ranges.clear();
            SearchTextRanges {
                ranges: Vec::new(),
                active: None,
            }
        };
        let changed = {
            let markdown = cached.entity.read(cx);
            markdown.search_highlights() != desired.ranges
                || markdown.active_search_highlight() != desired.active
        };
        let autoscroll_source = markdown_search_autoscroll(
            &desired,
            autoscroll_generation,
            cached.last_autoscroll_generation,
        );
        if changed || autoscroll_source.is_some() {
            cached.entity.update(cx, |markdown, cx| {
                if changed {
                    markdown.set_search_highlights(desired.ranges, desired.active, cx);
                }
                if let Some((_, source_index)) = autoscroll_source {
                    markdown.request_autoscroll_to_source_index(source_index, cx);
                }
            });
        }
        if let Some((generation, _)) = autoscroll_source {
            cached.last_autoscroll_generation = Some(generation);
        }
        cached.entity.clone()
    }

    fn render_diff_lines(
        content: &str,
        visible_line_count: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        logical_body_start: usize,
        item_index: usize,
        owner: Option<WeakEntity<HarnessApp>>,
        cx: &App,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors().clone();
        let unified = content.lines().any(|line| line.starts_with("@@"));
        let mut in_hunk = false;
        let mut old_line = None;
        let mut new_line = None;
        let mut logical_line_offset = logical_body_start;
        content
            .lines()
            .take(visible_line_count)
            .enumerate()
            .map(|(index, line)| {
                let logical_line_range = logical_line_offset..logical_line_offset + line.len();
                logical_line_offset += line.len() + 1;
                let tone = diff_line_tone(line, &mut in_hunk);
                let (displayed_old_line, displayed_new_line) = diff_line_numbers(
                    line,
                    tone,
                    unified,
                    in_hunk,
                    &mut old_line,
                    &mut new_line,
                    index + 1,
                );
                let highlighted_line = navigation_searchable_styled_text(
                    line.to_string(),
                    Vec::new(),
                    search,
                    navigation,
                    logical_line_range.clone(),
                    cx,
                );
                let clickable_line = rich_clickable_styled_text(
                    format!("rich-diff-line:{item_index}:{logical_line_offset}"),
                    highlighted_line,
                    item_index,
                    logical_line_range,
                    owner.clone(),
                );
                div()
                    .w_full()
                    .min_h(px(22.))
                    .px_2()
                    .py_0p5()
                    .flex()
                    .gap_2()
                    .font_buffer(cx)
                    .text_ui_sm(cx)
                    .bg(if tone == DiffLineTone::Addition {
                        colors.version_control_added.opacity(0.12)
                    } else if tone == DiffLineTone::Deletion {
                        colors.version_control_deleted.opacity(0.12)
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if tone == DiffLineTone::Addition {
                        colors.version_control_added
                    } else if tone == DiffLineTone::Deletion {
                        colors.version_control_deleted
                    } else if tone == DiffLineTone::Hunk {
                        colors.text_accent
                    } else {
                        colors.text
                    })
                    .child(
                        div()
                            .w(if unified { px(54.) } else { px(28.) })
                            .flex_none()
                            .flex()
                            .gap_1()
                            .text_color(colors.text_muted)
                            .when(unified, |this| {
                                this.child(
                                    div().w(px(24.)).flex().justify_end().child(
                                        displayed_old_line
                                            .map(|line| line.to_string())
                                            .unwrap_or_default(),
                                    ),
                                )
                            })
                            .child(
                                div().w(px(24.)).flex().justify_end().child(
                                    displayed_new_line
                                        .map(|line| line.to_string())
                                        .unwrap_or_default(),
                                ),
                            ),
                    )
                    .child(div().min_w_0().whitespace_nowrap().child(clickable_line))
                    .into_any_element()
            })
            .collect()
    }

    fn render_diff(
        &mut self,
        item: &TranscriptItem,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let presentations = diff_file_presentations(&item.content);
        let file_count = presentations.len();
        let (total_additions, total_deletions) = aggregate_diff_counts(
            presentations
                .iter()
                .map(|presentation| presentation.content.as_str()),
        );
        let owner = cx.weak_entity();
        let mut logical_cursor = 0;
        let mut row_ranges = Vec::new();
        let mut rows = Vec::new();

        for (section_index, presentation) in presentations.into_iter().enumerate() {
            if section_index > 0 {
                row_ranges.push(None);
                rows.push(
                    div()
                        .w_full()
                        .h(px(9.))
                        .mt_1()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .into_any_element(),
                );
            }
            let path_range =
                rich_navigation_fragment_range(navigation, &presentation.path, &mut logical_cursor);
            let content_range = rich_navigation_fragment_range(
                navigation,
                &presentation.content,
                &mut logical_cursor,
            );
            let (additions, deletions) = diff_content_counts(&presentation.content);
            let highlighted_path = navigation_searchable_styled_text(
                presentation.path,
                Vec::new(),
                search,
                navigation,
                path_range.clone(),
                cx,
            );
            let clickable_path = rich_clickable_styled_text(
                format!("rich-diff-path:{index}:{section_index}"),
                highlighted_path,
                index,
                path_range.clone(),
                Some(owner.clone()),
            );
            row_ranges.push(Some(path_range));
            rows.push(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_subheader_background.opacity(0.72))
                    .child(
                        Icon::new(IconName::File)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .font_buffer(cx)
                            .text_ui_sm(cx)
                            .truncate()
                            .child(clickable_path),
                    )
                    .when(additions > 0 || deletions > 0, |this| {
                        this.child(
                            DiffStat::new(
                                format!("rich-diff-file-stat:{index}:{section_index}"),
                                additions,
                                deletions,
                            )
                            .label_size(LabelSize::XSmall)
                            .tooltip(format!(
                                "{additions} lines added, {deletions} lines removed"
                            )),
                        )
                    })
                    .into_any_element(),
            );

            if !presentation.content.is_empty() {
                let line_count = presentation.content.lines().count();
                row_ranges.extend(
                    logical_line_fragments(&presentation.content, content_range.start)
                        .into_iter()
                        .take(line_count)
                        .map(|(_, range)| Some(range)),
                );
                rows.extend(Self::render_diff_lines(
                    &presentation.content,
                    usize::MAX,
                    search,
                    navigation,
                    content_range.start,
                    index,
                    Some(owner.clone()),
                    cx,
                ));
            }
        }

        let binding = self.rich_nested_scroll_binding(&item.key, navigation);
        reveal_rich_nested_cursor(Some(&binding), navigation, &row_ranges);
        div()
            .id(("rich-diff-output", index))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .when(file_count > 1, |this| {
                this.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .pb_1()
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(format!("{file_count} files"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .when(total_additions > 0 || total_deletions > 0, |this| {
                            this.child(
                                DiffStat::new(
                                    format!("rich-diff-total-stat:{index}"),
                                    total_additions,
                                    total_deletions,
                                )
                                .label_size(LabelSize::XSmall)
                                .tooltip(format!(
                                    "{total_additions} total lines added, {total_deletions} total lines removed"
                                )),
                            )
                        }),
                )
            })
            .child(
                div()
                    .id(("rich-diff-scroll", index))
                    .w_full()
                    .min_w_0()
                    .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
                    .overflow_x_scroll()
                    .overflow_y_scroll()
                    .track_scroll(&binding.handle)
                    .children(rows)
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
                            .id(("rich-diff-scrollbar", index))
                            .with_thumb_color(colors.text_muted.opacity(0.5))
                            .tracked_scroll_handle(&binding.handle),
                        window,
                        cx,
                    ),
            )
            .into_any_element()
    }

    fn render_diff_content(
        item: &TranscriptItem,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        expansion: OutputExpansion,
        toggle: Option<AnyElement>,
        owner: Option<WeakEntity<HarnessApp>>,
        cx: &App,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let presentations = diff_file_presentations(&item.content);
        let file_count = presentations.len();
        let (total_additions, total_deletions) = aggregate_diff_counts(
            presentations
                .iter()
                .map(|presentation| presentation.content.as_str()),
        );
        let allocations = progressive_file_line_allocations(
            &presentations
                .iter()
                .map(|presentation| presentation.content.lines().count())
                .collect::<Vec<_>>(),
            expansion,
        );
        let mut sections = Vec::new();
        let mut logical_search_start = 0;

        for (section_index, (presentation, visible_lines)) in
            presentations.into_iter().zip(allocations).enumerate()
        {
            let path_range = rich_navigation_fragment_range(
                navigation,
                &presentation.path,
                &mut logical_search_start,
            );
            let content_range = rich_navigation_fragment_range(
                navigation,
                &presentation.content,
                &mut logical_search_start,
            );
            let (additions, deletions) = diff_content_counts(&presentation.content);
            let highlighted_path = navigation_searchable_styled_text(
                presentation.path,
                Vec::new(),
                search,
                navigation,
                path_range.clone(),
                cx,
            );
            let clickable_path = rich_clickable_styled_text(
                format!("rich-diff-path:{index}:{section_index}"),
                highlighted_path,
                index,
                path_range,
                owner.clone(),
            );
            sections.push(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .when(section_index > 0, |this| {
                        this.mt_1()
                            .pt_2()
                            .border_t_1()
                            .border_color(colors.border_variant)
                    })
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .border_b_1()
                            .border_color(colors.border_variant)
                            .bg(colors.editor_subheader_background.opacity(0.72))
                            .child(
                                Icon::new(IconName::File)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .font_buffer(cx)
                                    .text_ui_sm(cx)
                                    .truncate()
                                    .child(clickable_path),
                            )
                            .when(additions > 0 || deletions > 0, |this| {
                                this.child(
                                    DiffStat::new(
                                        format!("diff-file-stat:{index}:{section_index}"),
                                        additions,
                                        deletions,
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .tooltip(format!(
                                        "{additions} lines added, {deletions} lines removed"
                                    )),
                                )
                            }),
                    )
                    .when(
                        !presentation.content.is_empty() && visible_lines > 0,
                        |this| {
                            this.child(
                                div()
                                    .id(format!("diff-file-lines:{index}:{section_index}"))
                                    .w_full()
                                    .min_w_0()
                                    .overflow_x_scroll()
                                    .children(Self::render_diff_lines(
                                        &presentation.content,
                                        visible_lines,
                                        search,
                                        navigation,
                                        content_range.start,
                                        index,
                                        owner.clone(),
                                        cx,
                                    )),
                            )
                        },
                    ),
            );
        }

        div()
            .id(("diff-output", index))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .when(file_count > 1, |this| {
                this.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .pb_1()
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(format!("{file_count} files"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .when(total_additions > 0 || total_deletions > 0, |this| {
                            this.child(
                                DiffStat::new(
                                    format!("diff-total-stat:{index}"),
                                    total_additions,
                                    total_deletions,
                                )
                                .label_size(LabelSize::XSmall)
                                .tooltip(format!(
                                    "{total_additions} total lines added, {total_deletions} total lines removed"
                                )),
                            )
                        }),
                )
            })
            .children(sections)
            .when_some(toggle, |this, toggle| {
                this.child(
                    div()
                        .pt_1()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .child(toggle),
                )
            })
            .into_any_element()
    }

    fn render_file_change(
        &mut self,
        item: &TranscriptItem,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let presentations = file_change_presentations(&item.content);
        let file_count = presentations.len();
        let (total_additions, total_deletions) = presentations.iter().map(file_change_counts).fold(
            (0, 0),
            |(total_additions, total_deletions), (additions, deletions)| {
                (total_additions + additions, total_deletions + deletions)
            },
        );
        let owner = cx.weak_entity();
        let mut logical_cursor = 0;
        let mut row_ranges = Vec::new();
        let mut rows = Vec::new();

        for (section_index, presentation) in presentations.into_iter().enumerate() {
            if section_index > 0 {
                row_ranges.push(None);
                rows.push(
                    div()
                        .w_full()
                        .h(px(9.))
                        .mt_1()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .into_any_element(),
                );
            }
            let (additions, deletions) = file_change_counts(&presentation);
            let path_range =
                rich_navigation_fragment_range(navigation, &presentation.path, &mut logical_cursor);
            let highlighted_path = navigation_searchable_styled_text(
                presentation.path.clone(),
                Vec::new(),
                search,
                navigation,
                path_range.clone(),
                cx,
            );
            let clickable_path = rich_clickable_styled_text(
                format!("rich-file-change-path:{index}:{section_index}"),
                highlighted_path,
                index,
                path_range.clone(),
                Some(owner.clone()),
            );
            let content_range = rich_navigation_fragment_range(
                navigation,
                &presentation.content,
                &mut logical_cursor,
            );
            let operation_color = match presentation.operation.as_str() {
                "Added" => Color::Success,
                "Deleted" => Color::Error,
                "Modified" | "Moved" => Color::Accent,
                _ => Color::Muted,
            };
            let operation_text_color = match operation_color {
                Color::Success => cx.theme().status().success,
                Color::Error => cx.theme().status().error,
                Color::Accent => colors.text_accent,
                _ => colors.text_muted,
            };
            row_ranges.push(Some(path_range));
            rows.push(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_subheader_background.opacity(0.72))
                    .child(
                        Icon::new(IconName::File)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .font_buffer(cx)
                            .text_ui_sm(cx)
                            .truncate()
                            .child(clickable_path),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(additions > 0 || deletions > 0, |this| {
                                this.child(
                                    DiffStat::new(
                                        format!("file-change-stat:{index}:{section_index}"),
                                        additions,
                                        deletions,
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .tooltip(format!(
                                        "{additions} lines added, {deletions} lines removed"
                                    )),
                                )
                            })
                            .when(additions == 0 && deletions == 0, |this| {
                                let operation = searchable_styled_text(
                                    presentation.operation,
                                    Vec::new(),
                                    search,
                                    cx,
                                );
                                this.child(
                                    div()
                                        .text_ui_xs(cx)
                                        .text_color(operation_text_color)
                                        .child(operation),
                                )
                            }),
                    )
                    .into_any_element(),
            );

            if !presentation.content.is_empty() {
                let line_count = presentation.content.lines().count();
                row_ranges.extend(
                    logical_line_fragments(&presentation.content, content_range.start)
                        .into_iter()
                        .take(line_count)
                        .map(|(_, range)| Some(range)),
                );
                rows.extend(Self::render_diff_lines(
                    &presentation.content,
                    usize::MAX,
                    search,
                    navigation,
                    content_range.start,
                    index,
                    Some(owner.clone()),
                    cx,
                ));
            }
        }

        let binding = self.rich_nested_scroll_binding(&item.key, navigation);
        reveal_rich_nested_cursor(Some(&binding), navigation, &row_ranges);
        div()
            .id(("file-change-output", index))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .when(file_count > 1, |this| {
                this.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .pb_1()
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(format!("{file_count} files"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .when(total_additions > 0 || total_deletions > 0, |this| {
                            this.child(
                                DiffStat::new(
                                    format!("file-change-total-stat:{index}"),
                                    total_additions,
                                    total_deletions,
                                )
                                .label_size(LabelSize::XSmall)
                                .tooltip(format!(
                                    "{total_additions} total lines added, {total_deletions} total lines removed"
                                )),
                            )
                        }),
                )
            })
            .child(
                div()
                    .id(("file-change-scroll", index))
                    .w_full()
                    .min_w_0()
                    .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
                    .overflow_x_scroll()
                    .overflow_y_scroll()
                    .track_scroll(&binding.handle)
                    .children(rows)
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
                            .id(("file-change-scrollbar", index))
                            .with_thumb_color(colors.text_muted.opacity(0.5))
                            .tracked_scroll_handle(&binding.handle),
                        window,
                        cx,
                    ),
            )
            .into_any_element()
    }

    fn render_reasoning(
        content: &str,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let mut logical_search_start = 0;
        let steps = reasoning_steps(content)
            .into_iter()
            .map(|line| {
                let start = navigation
                    .and_then(|navigation| {
                        navigation.body_text[logical_search_start.min(navigation.body_text.len())..]
                            .find(&line)
                            .map(|offset| logical_search_start + offset)
                    })
                    .unwrap_or(logical_search_start);
                logical_search_start = start + line.len();
                (start, line)
            })
            .collect::<Vec<_>>();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .children(steps.into_iter().map(|(start, step)| {
                let end = start + step.len();
                let highlighted_step = navigation_searchable_styled_text(
                    step,
                    Vec::new(),
                    search,
                    navigation,
                    start..end,
                    cx,
                );
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
                    .child(div().min_w_0().flex_1().child(highlighted_step))
            }))
            .into_any_element()
    }

    fn render_terminal(
        &mut self,
        content: String,
        item_key: &str,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let mut logical_cursor = 0;
        let body_range = rich_navigation_fragment_range(navigation, &content, &mut logical_cursor);
        let lines = logical_line_fragments(&content, body_range.start);
        let row_ranges = lines
            .iter()
            .map(|(_, range)| Some(range.clone()))
            .collect::<Vec<_>>();
        let binding = self.rich_nested_scroll_binding(item_key, navigation);
        reveal_rich_nested_cursor(Some(&binding), navigation, &row_ranges);
        let owner = cx.weak_entity();
        let rows = lines
            .into_iter()
            .enumerate()
            .map(|(line_index, (line, range))| {
                let highlighted = navigation_searchable_styled_text(
                    line,
                    Vec::new(),
                    search,
                    navigation,
                    range.clone(),
                    cx,
                );
                let clickable = rich_clickable_styled_text(
                    format!("rich-terminal:{index}:{line_index}"),
                    highlighted,
                    index,
                    range,
                    Some(owner.clone()),
                );
                div()
                    .w_full()
                    .min_w_0()
                    .min_h(px(20.))
                    .whitespace_normal()
                    .child(clickable)
            })
            .collect::<Vec<_>>();
        div()
            .id(("terminal-scroll", index))
            .w_full()
            .min_w_0()
            .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&binding.handle)
            .font_buffer(cx)
            .text_ui_sm(cx)
            .line_height(relative(1.45))
            .text_color(colors.text)
            .children(rows)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical)
                    .id(("terminal-scrollbar", index))
                    .with_thumb_color(colors.text_muted.opacity(0.5))
                    .tracked_scroll_handle(&binding.handle),
                window,
                cx,
            )
            .into_any_element()
    }

    fn render_activity_sections(
        &mut self,
        content: String,
        item_key: &str,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sections = activity_text_sections(&content);
        if !sections.iter().any(|section| section.heading.is_some()) {
            return self.render_terminal(content, item_key, index, search, navigation, window, cx);
        }

        let colors = cx.theme().colors().clone();
        let error_color = cx.theme().status().error;
        let mut logical_cursor = 0;
        let owner = cx.weak_entity();
        let mut row_ranges = Vec::new();
        let mut rows = Vec::new();

        for (section_index, section) in sections.into_iter().enumerate() {
            let error = section.heading.as_deref() == Some("Error");
            let primary = matches!(
                section.heading.as_deref(),
                Some("Result" | "Structured result" | "Error")
            );
            if section_index > 0 {
                row_ranges.push(None);
                rows.push(
                    div()
                        .w_full()
                        .h(px(9.))
                        .my_1()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .into_any_element(),
                );
            }
            if let Some(heading) = section.heading {
                let range =
                    rich_navigation_fragment_range(navigation, &heading, &mut logical_cursor);
                let highlighted = navigation_searchable_styled_text(
                    heading,
                    Vec::new(),
                    search,
                    navigation,
                    range.clone(),
                    cx,
                );
                let clickable = rich_clickable_styled_text(
                    format!("rich-activity-heading:{index}:{section_index}"),
                    highlighted,
                    index,
                    range.clone(),
                    Some(owner.clone()),
                );
                row_ranges.push(Some(range));
                rows.push(
                    div()
                        .w_full()
                        .min_h(px(18.))
                        .text_ui_xs(cx)
                        .text_color(if error {
                            error_color
                        } else if primary {
                            colors.text
                        } else {
                            colors.text_muted
                        })
                        .child(clickable)
                        .into_any_element(),
                );
            }

            if !section.body.is_empty() {
                let body_range =
                    rich_navigation_fragment_range(navigation, &section.body, &mut logical_cursor);
                let json = is_valid_json(&section.body)
                    .then(|| json_highlights(&section.body, cx))
                    .unwrap_or_default();
                for (line_index, (line, range)) in
                    logical_line_fragments(&section.body, body_range.start)
                        .into_iter()
                        .enumerate()
                {
                    let local = range.start - body_range.start..range.end - body_range.start;
                    let base = highlights_for_local_fragment(&json, local);
                    let highlighted = navigation_searchable_styled_text(
                        line,
                        base,
                        search,
                        navigation,
                        range.clone(),
                        cx,
                    );
                    let clickable = rich_clickable_styled_text(
                        format!("rich-activity-body:{index}:{section_index}:{line_index}"),
                        highlighted,
                        index,
                        range.clone(),
                        Some(owner.clone()),
                    );
                    row_ranges.push(Some(range));
                    rows.push(
                        div()
                            .w_full()
                            .min_w_0()
                            .min_h(px(20.))
                            .font_buffer(cx)
                            .text_ui_sm(cx)
                            .line_height(relative(1.45))
                            .text_color(if error { error_color } else { colors.text })
                            .whitespace_normal()
                            .child(clickable)
                            .into_any_element(),
                    );
                }
            }
        }

        let binding = self.rich_nested_scroll_binding(item_key, navigation);
        reveal_rich_nested_cursor(Some(&binding), navigation, &row_ranges);
        div()
            .id(("activity-sections", index))
            .w_full()
            .min_w_0()
            .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&binding.handle)
            .children(rows)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical)
                    .id(("activity-scrollbar", index))
                    .with_thumb_color(colors.text_muted.opacity(0.5))
                    .tracked_scroll_handle(&binding.handle),
                window,
                cx,
            )
            .into_any_element()
    }

    fn render_command(
        &mut self,
        item: &TranscriptItem,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_rich_command_content(item, index, search, navigation, window, cx)
            .unwrap_or_else(|| {
                self.render_terminal(
                    item.content.clone(),
                    &item.key,
                    index,
                    search,
                    navigation,
                    window,
                    cx,
                )
            })
    }

    fn render_rich_command_content(
        &mut self,
        item: &TranscriptItem,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (data, command_list_state, output_list_state) =
            self.rich_command_surface(item, navigation)?;
        let colors = cx.theme().colors().clone();
        let owner = cx.weak_entity();
        let search = search.cloned();
        let navigation = navigation.cloned();
        let command_data = data.clone();
        let command_search = search.clone();
        let command_navigation = navigation.clone();
        let command_owner = owner.clone();
        let command_rows = list(command_list_state.clone(), move |row_index, _, cx| {
            let row = &command_data.rows[row_index];
            let line = &command_data.command[row.source_range.clone()];
            let logical_range = rich_command_row_logical_range(&command_data, row);
            let highlighted = navigation_searchable_styled_text(
                line.to_owned(),
                shell_highlights(line, cx),
                command_search.as_ref(),
                command_navigation.as_ref(),
                logical_range.clone(),
                cx,
            );
            let cursor_marker =
                rich_cursor_index_for_fragment(command_navigation.as_ref(), &logical_range).map(
                    |rendered_index| {
                        rich_cursor_autoscroll_marker(
                            highlighted.layout().clone(),
                            rendered_index,
                            cx.theme().players().local().cursor.opacity(0.55),
                        )
                    },
                );
            let clickable = rich_clickable_styled_text(
                format!("rich-command-text:{index}:{}", row.line_index),
                highlighted,
                index,
                logical_range,
                Some(command_owner.clone()),
            );
            div()
                .w_full()
                .min_w_0()
                .min_h(px(20.))
                .relative()
                .whitespace_normal()
                .child(clickable)
                .when_some(cursor_marker, |this, marker| this.child(marker))
                .into_any_element()
        })
        .with_sizing_behavior(ListSizingBehavior::Infer)
        .max_h(px(RICH_NESTED_COMMAND_MAX_HEIGHT));

        let output_data = data.clone();
        let output_search = search.clone();
        let output_navigation = navigation.clone();
        let output_owner = owner.clone();
        let output_row_offset = data.command_row_count;
        let output_rows = list(output_list_state.clone(), move |row_index, _, cx| {
            let row = &output_data.rows[output_row_offset + row_index];
            let line = &output_data.output[row.source_range.clone()];
            let logical_range = rich_command_row_logical_range(&output_data, row);
            let highlighted = navigation_searchable_styled_text(
                line.to_owned(),
                Vec::new(),
                output_search.as_ref(),
                output_navigation.as_ref(),
                logical_range.clone(),
                cx,
            );
            let cursor_marker =
                rich_cursor_index_for_fragment(output_navigation.as_ref(), &logical_range).map(
                    |rendered_index| {
                        rich_cursor_autoscroll_marker(
                            highlighted.layout().clone(),
                            rendered_index,
                            cx.theme().players().local().cursor.opacity(0.55),
                        )
                    },
                );
            let clickable = rich_clickable_styled_text(
                format!("rich-command-output:{index}:{}", row.line_index),
                highlighted,
                index,
                logical_range,
                Some(output_owner.clone()),
            );
            div()
                .w_full()
                .min_w_0()
                .min_h(px(20.))
                .relative()
                .whitespace_normal()
                .child(clickable)
                .when_some(cursor_marker, |this, marker| this.child(marker))
                .into_any_element()
        })
        .with_sizing_behavior(ListSizingBehavior::Infer)
        .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT));

        let command_region = div()
            .id(("command-input-scroll", index))
            .w_full()
            .min_w_0()
            .relative()
            .max_h(px(RICH_NESTED_COMMAND_MAX_HEIGHT))
            .child(command_rows)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical)
                    .id(("command-input-scrollbar", index))
                    .with_thumb_color(colors.text_muted.opacity(0.5))
                    .tracked_scroll_handle(&command_list_state),
                window,
                cx,
            );

        let output_region = (!data.output.is_empty()).then(|| {
            div()
                .id(("command-output-scroll", index))
                .w_full()
                .min_w_0()
                .relative()
                .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
                .border_t_1()
                .border_color(colors.border_variant)
                .mt_2()
                .pt_2()
                .child(output_rows)
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical)
                        .id(("command-output-scrollbar", index))
                        .with_thumb_color(colors.text_muted.opacity(0.5))
                        .tracked_scroll_handle(&output_list_state),
                    window,
                    cx,
                )
        });

        Some(
            div()
                .id(("command-output", index))
                .w_full()
                .min_w_0()
                .font_buffer(cx)
                .text_ui_sm(cx)
                .line_height(relative(1.45))
                .text_color(colors.text)
                .child(command_region)
                .when_some(output_region, |this, output| this.child(output))
                .into_any_element(),
        )
    }

    fn render_command_content(
        item: &TranscriptItem,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        expansion: OutputExpansion,
        toggle: Option<AnyElement>,
        owner: Option<WeakEntity<HarnessApp>>,
        cx: &App,
    ) -> Option<AnyElement> {
        let command = item.command_transcript()?;
        let colors = cx.theme().colors().clone();
        let command_text = command.command.trim_end_matches(['\r', '\n']);
        let output = command_output_for_display(&command.output).to_string();
        let command_limits = output_limits(expansion, COMMAND_PREVIEW_LINES, COMMAND_PREVIEW_BYTES);
        let output_limits = output_limits(
            expansion,
            STRUCTURED_OUTPUT_PREVIEW_LINES,
            STRUCTURED_OUTPUT_PREVIEW_BYTES,
        );
        let command_preview = structured_output_preview_with_limits(
            command_text,
            "command",
            command_limits.lines,
            command_limits.bytes,
        );
        let visible_command_source = if command_preview.footer.is_some() {
            command_preview.content.trim_end()
        } else {
            command_preview.content.as_str()
        };
        let visible_command_source_len = visible_command_source.len();
        let displayed_command = if command_preview.footer.is_some() {
            format!("{visible_command_source} …")
        } else {
            visible_command_source.to_owned()
        };
        let displayed_output = structured_output_preview_with_limits(
            &output,
            "output",
            output_limits.lines,
            output_limits.bytes,
        )
        .content;
        let command_start = navigation
            .and_then(|navigation| navigation.body_text.find(command_text))
            .unwrap_or(0);
        // The ellipsis is presentation chrome, not a Vim byte. Keep its
        // clickable/highlight range clamped to the actual command prefix so a
        // long preview cannot spill into the output's logical row.
        let command_end = command_start + visible_command_source_len;
        let output_start = navigation
            .and_then(|navigation| {
                navigation.body_text[command_start.min(navigation.body_text.len())..]
                    .find(&displayed_output)
                    .map(|offset| command_start + offset)
            })
            .unwrap_or(command_end);
        let highlighted_command = navigation_searchable_styled_text(
            displayed_command.clone(),
            shell_highlights(&displayed_command, cx),
            search,
            navigation,
            command_start..command_end,
            cx,
        );
        let highlighted_output = navigation_searchable_styled_text(
            displayed_output.clone(),
            Vec::new(),
            search,
            navigation,
            output_start..output_start + displayed_output.len(),
            cx,
        );
        let clickable_command = rich_clickable_styled_text(
            format!("rich-command-text:{index}"),
            highlighted_command,
            index,
            command_start..command_end,
            owner.clone(),
        );
        let clickable_output = rich_clickable_styled_text(
            format!("rich-command-output:{index}"),
            highlighted_output,
            index,
            output_start..output_start + displayed_output.len(),
            owner,
        );

        Some(
            div()
                .id(("command-output", index))
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .font_buffer(cx)
                        .text_ui_sm(cx)
                        .line_height(relative(1.45))
                        .whitespace_normal()
                        .child(clickable_command),
                )
                .when(!displayed_output.is_empty(), |this| {
                    this.child(
                        div()
                            .id(("command-output-scroll", index))
                            .w_full()
                            .min_w_0()
                            .border_t_1()
                            .border_color(colors.border_variant)
                            .mt_2()
                            .pt_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .font_buffer(cx)
                            .text_ui_sm(cx)
                            .line_height(relative(1.45))
                            .text_color(colors.text)
                            .whitespace_normal()
                            .child(clickable_output),
                    )
                })
                .when_some(toggle, |this, toggle| {
                    this.child(
                        div()
                            .mt_1()
                            .pt_1()
                            .border_t_1()
                            .border_color(colors.border_variant)
                            .child(toggle),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_web_search(
        &mut self,
        item: &TranscriptItem,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let presentation = web_search_presentation(&item.raw);
        let total_results = presentation.results.len();
        let item_key = item.key.clone();
        let mut logical_cursor = 0;
        let mut row_ranges = Vec::new();
        let visible_results = presentation
            .results
            .into_iter()
            .enumerate()
            .map(|(result_index, result)| {
                let title_range =
                    rich_navigation_fragment_range(navigation, &result.title, &mut logical_cursor);
                let result_start = title_range.start;
                let highlighted_title = navigation_searchable_styled_text(
                    result.title,
                    Vec::new(),
                    search,
                    navigation,
                    title_range.clone(),
                    cx,
                );
                let url = result.url.map(|url| {
                    let range =
                        rich_navigation_fragment_range(navigation, &url, &mut logical_cursor);
                    let highlighted = navigation_searchable_styled_text(
                        url.clone(),
                        Vec::new(),
                        search,
                        navigation,
                        range.clone(),
                        cx,
                    );
                    (url, highlighted, range)
                });
                let snippet = result.snippet.map(|snippet| {
                    let range =
                        rich_navigation_fragment_range(navigation, &snippet, &mut logical_cursor);
                    let highlighted = navigation_searchable_styled_text(
                        snippet,
                        Vec::new(),
                        search,
                        navigation,
                        range.clone(),
                        cx,
                    );
                    (highlighted, range)
                });
                let result_end = snippet
                    .as_ref()
                    .map(|(_, range)| range.end)
                    .or_else(|| url.as_ref().map(|(_, _, range)| range.end))
                    .unwrap_or(title_range.end);
                row_ranges.push(Some(result_start..result_end));
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_start()
                    .gap_2()
                    .py_1()
                    .when(result_index > 0, |this| {
                        this.border_t_1().border_color(colors.border_variant)
                    })
                    .child(
                        div()
                            .w(px(18.))
                            .flex_none()
                            .text_ui_xs(cx)
                            .text_color(colors.text_muted)
                            .child(format!("{}.", result_index + 1)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(div().min_w_0().text_ui_sm(cx).child(highlighted_title))
                            .when_some(url, |this, (url, highlighted_url, _)| {
                                let open_url = url.clone();
                                this.child(
                                    div()
                                        .id(format!("web-result-url:{item_key}:{result_index}"))
                                        .min_w_0()
                                        .cursor_pointer()
                                        .text_ui_xs(cx)
                                        .text_color(colors.text_accent)
                                        .hover(|this| this.underline())
                                        .on_click(move |_, _, cx| cx.open_url(&open_url))
                                        .child(highlighted_url),
                                )
                            })
                            .when_some(snippet, |this, (highlighted_snippet, _)| {
                                this.child(
                                    div()
                                        .min_w_0()
                                        .text_ui_xs(cx)
                                        .text_color(colors.text_muted)
                                        .child(highlighted_snippet),
                                )
                            }),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let results_scroll = (total_results > 0).then(|| {
            let binding = self.rich_nested_scroll_binding(&item.key, navigation);
            reveal_rich_nested_cursor(Some(&binding), navigation, &row_ranges);
            div()
                .id(("web-results-scroll", index))
                .w_full()
                .min_w_0()
                .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(&binding.handle)
                .children(visible_results)
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical)
                        .id(("web-results-scrollbar", index))
                        .with_thumb_color(colors.text_muted.opacity(0.5))
                        .tracked_scroll_handle(&binding.handle),
                    window,
                    cx,
                )
        });

        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .when(presentation.related_queries > 0, |this| {
                this.child(
                    Label::new(format!(
                        "+{} related {}",
                        presentation.related_queries,
                        if presentation.related_queries == 1 {
                            "query"
                        } else {
                            "queries"
                        }
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
            })
            .when_some(results_scroll, |this, results| this.child(results))
            .when(total_results == 0, |this| {
                this.child(self.render_terminal(
                    item.content.clone(),
                    &item.key,
                    index,
                    search,
                    navigation,
                    window,
                    cx,
                ))
            })
            .into_any_element()
    }

    fn render_plain_prose(
        content: &str,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut paragraph_offset = 0;
        let paragraphs = content
            .split("\n\n")
            .map(|paragraph| {
                let paragraph_start = paragraph_offset;
                paragraph_offset = (paragraph_offset + paragraph.len() + 2).min(content.len());
                let mut line_offset = paragraph_start;
                paragraph
                    .lines()
                    .map(|line| {
                        let start = line_offset;
                        line_offset += line.len() + 1;
                        (start, line.to_string())
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .text_ui(cx)
            .line_height(relative(1.55))
            .children(paragraphs.into_iter().map(|paragraph| {
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .children(paragraph.into_iter().map(|(start, line)| {
                        let end = start + line.len();
                        let highlighted_line = navigation_searchable_styled_text(
                            line,
                            Vec::new(),
                            search,
                            navigation,
                            start..end,
                            cx,
                        );
                        div()
                            .min_h(px(20.))
                            .whitespace_normal()
                            .child(highlighted_line)
                    }))
            }))
            .into_any_element()
    }

    fn render_image(
        item: &TranscriptItem,
        surface: Option<Entity<ImageSurface>>,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let caption = image_caption_for_display(item).map(ToOwned::to_owned);
        let mut logical_cursor = 0;
        let highlighted_caption = caption.as_ref().map(|caption| {
            let range = rich_navigation_fragment_range(navigation, caption, &mut logical_cursor);
            navigation_searchable_styled_text(
                caption.clone(),
                Vec::new(),
                search,
                navigation,
                range,
                cx,
            )
        });
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
            .when_some(highlighted_caption, |this, highlighted_caption| {
                this.child(
                    div()
                        .text_ui(cx)
                        .line_height(relative(1.45))
                        .text_color(colors.text_muted)
                        .child(highlighted_caption),
                )
            })
            .into_any_element()
    }

    fn render_pending_request_summary(
        item: &TranscriptItem,
        request_is_live: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        let semantic_content = matches!(
            method,
            "item/permissions/requestApproval"
                | "item/tool/requestUserInput"
                | "mcpServer/elicitation/request"
        )
        .then(|| item.content.trim())
        .filter(|content| !content.is_empty())
        .map(ToOwned::to_owned);
        let inactive_label = (!request_is_live).then(|| {
            if item
                .pending_request
                .as_ref()
                .is_some_and(|request| request.resolved)
            {
                "Request completed"
            } else {
                "Request is no longer active"
            }
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
            .when_some(semantic_content, |this, content| {
                this.child(
                    div()
                        .text_ui(cx)
                        .line_height(relative(1.45))
                        .text_color(colors.text_muted)
                        .child(content),
                )
            })
            .when_some(inactive_label, |this, label| {
                this.child(
                    div()
                        .mt_1()
                        .text_ui_xs(cx)
                        .text_color(colors.text_disabled)
                        .child(label),
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
        let render_started_at = slow_list_diagnostics().then(std::time::Instant::now);
        let clone_started_at = slow_list_diagnostics().then(std::time::Instant::now);
        let item = self.model.items[index].clone();
        if let Some(started_at) = clone_started_at {
            let elapsed = started_at.elapsed();
            if elapsed >= Duration::from_millis(4) {
                eprintln!(
                    "slow-transcript phase=clone item={index} kind={:?} content_bytes={} elapsed_ms={:.2}",
                    item.kind,
                    item.content.len(),
                    elapsed.as_secs_f64() * 1_000.
                );
            }
        }
        if !item.is_presentationally_visible() {
            return div().into_any_element();
        }
        let rich_navigation = self.rich_navigation_for_item(index);
        let cursor = matches!(
            self.focus_mode,
            FocusMode::Transcript | FocusMode::Request | FocusMode::Approval | FocusMode::Buffer
        ) && index == self.selected_item;
        let visual = !rich_vim_experiment()
            && self.visual_anchor.is_some_and(|anchor| {
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
        let request_is_live = self.live_request_keys.contains(&item.key);
        let legacy_request_controls = legacy_request_controls_active(
            request_is_live,
            uses_shared_request_surface,
            pending_method.is_some(),
        );
        let has_approval = legacy_request_controls && !response_choices.is_empty();
        let approval_focused =
            self.focus_mode == FocusMode::Approval && index == self.selected_item;
        let approval_cursor = self.approval_cursor;
        let colors = cx.theme().colors().clone();
        let narrow = window.viewport_size().width < px(720.);
        let narrative = matches!(
            item.kind,
            model::TranscriptKind::User
                | model::TranscriptKind::Agent
                | model::TranscriptKind::Reasoning
                | model::TranscriptKind::Plan
        );
        let streaming = item.status.as_deref() == Some("streaming");
        let search_item_position = self.search_matches.binary_search(&index).ok();
        let active_search_item = search_item_position == Some(self.active_search_match);
        let rich_search = (self.search_visible
            && !self.search_query.trim().is_empty()
            && search_item_position.is_some())
        .then(|| RichSearchPaint::new(self.search_query.clone(), active_search_item));
        let icon = icon_for_kind(item.kind);
        let user_input = (legacy_request_controls && has_user_input)
            .then(|| self.render_user_input_request(index, &item, window, cx));
        let mcp_elicitation = (legacy_request_controls && has_elicitation)
            .then(|| self.render_mcp_elicitation(index, &item, window, cx));
        let pending_summary = request_method
            .filter(|_| !uses_shared_request_surface)
            .filter(|method| {
                !legacy_request_controls
                    || !matches!(
                        *method,
                        "item/tool/requestUserInput" | "mcpServer/elicitation/request"
                    )
            })
            .map(|_| Self::render_pending_request_summary(&item, request_is_live, cx));
        let choice_buttons = response_choices
            .into_iter()
            .filter(|_| legacy_request_controls)
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
        let header_title = transcript_item_header_title(&item).to_owned();
        let has_collapsible_content = !item.content.trim().is_empty();
        let show_header = transcript_item_shows_header(&item);
        let disclosure_weak = cx.weak_entity();
        let is_disclosure = has_collapsible_content
            && (item.kind.is_structured() || item.kind == model::TranscriptKind::Reasoning);
        let raw_search_visible = raw_visible
            && self.search_visible
            && folded_contains(&item.raw.to_string(), &self.search_query);
        let search_context = (self.search_visible
            && active_search_item
            && is_disclosure
            && rich_search_match_needs_context(&item, OutputExpansion::Preview)
            && !raw_search_visible
            && !rich_search_query_is_visible(&item, OutputExpansion::Preview, &self.search_query))
        .then(|| item_search_context_snippet(&item, &self.search_query, 180))
        .flatten()
        .map(|snippet| {
            let styled = StyledText::new(snippet.text).with_highlights(vec![(
                snippet.match_range,
                gpui::HighlightStyle {
                    color: Some(colors.text),
                    background_color: Some(colors.search_active_match_background),
                    ..Default::default()
                },
            )]);
            div()
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .border_l_2()
                .border_color(colors.search_active_match_background)
                .pl_2()
                .py_0p5()
                .font_buffer(cx)
                .text_ui_xs(cx)
                .text_color(colors.text_muted)
                .child(styled)
                .into_any_element()
        });
        let markdown = (narrative
            && item.kind != model::TranscriptKind::Reasoning
            && item.expanded
            && !streaming
            && !item.content.is_empty())
        .then(|| {
            self.markdown_for(
                &item.key,
                &item.content,
                rich_search.as_ref(),
                rich_navigation.as_ref(),
                (active_search_item && self.search_visible)
                    .then_some(self.search_navigation_generation),
                cx,
            )
        });

        let body = if request_method.is_some() || !item.expanded || item.content.is_empty() {
            None
        } else if let Some(markdown) = markdown {
            let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
            style.code_block_overflow_x_scroll = true;
            if let Some(navigation) = rich_navigation.as_ref() {
                style.selection_background_color =
                    rich_navigation_markdown_highlight_background(navigation, cx);
            }
            let mut element = MarkdownElement::new(markdown, style);
            if rich_vim_experiment() {
                let source = item.content.clone();
                let logical = rich_navigation_item_projection(&self.model, index)
                    .map(|projection| projection.body_text().to_owned())
                    .unwrap_or_else(|| source.clone());
                let owner = cx.weak_entity();
                element = element.on_source_click(move |source_offset, _, window, cx| {
                    let body_offset =
                        markdown_logical_offset_for_source(&logical, &source, source_offset);
                    owner
                        .update(cx, |this, cx| {
                            this.selected_item = index;
                            this.visual_anchor = None;
                            this.focus_mode = FocusMode::Buffer;
                            this.transcript_cursor_initialized = true;
                            this.list_state.pause_following_tail();
                            this.transcript_editor.update(cx, |editor, cx| {
                                editor.set_cursor_in_item(index, body_offset, window, cx);
                            });
                            this.transcript_editor.focus_handle(cx).focus(window, cx);
                            this.transcript_editor.update(cx, |editor, cx| {
                                editor.enter_normal_mode(window, cx);
                            });
                            cx.notify();
                        })
                        .ok();
                    true
                });
            }
            Some(div().w_full().min_w_0().child(element).into_any_element())
        } else {
            Some(match item.kind {
                model::TranscriptKind::User
                | model::TranscriptKind::Agent
                | model::TranscriptKind::Plan => Self::render_plain_prose(
                    &item.content,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    cx,
                ),
                model::TranscriptKind::Reasoning => Self::render_reasoning(
                    &item.content,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    cx,
                ),
                model::TranscriptKind::Command => self.render_command(
                    &item,
                    index,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    window,
                    cx,
                ),
                model::TranscriptKind::Web => self.render_web_search(
                    &item,
                    index,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    window,
                    cx,
                ),
                model::TranscriptKind::Diff => self.render_diff(
                    &item,
                    index,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    window,
                    cx,
                ),
                model::TranscriptKind::FileChange => self.render_file_change(
                    &item,
                    index,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    window,
                    cx,
                ),
                model::TranscriptKind::Image => Self::render_image(
                    &item,
                    self.image_surfaces.get(&item.key).cloned(),
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    cx,
                ),
                model::TranscriptKind::Tool
                | model::TranscriptKind::Subagent
                | model::TranscriptKind::Review => self.render_activity_sections(
                    item.content.clone(),
                    &item.key,
                    index,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    window,
                    cx,
                ),
                _ => self.render_terminal(
                    item.content.clone(),
                    &item.key,
                    index,
                    rich_search.as_ref(),
                    rich_navigation.as_ref(),
                    window,
                    cx,
                ),
            })
        };

        let header_search = show_header.then_some(rich_search.as_ref()).flatten();
        // Every expanded Rich structured body is complete and scrollable. If
        // a renderer deliberately has no glyph for a protocol-only offset,
        // keep Vim visible on the header instead of mounting a second,
        // progressively expanded copy of the body.
        let body_left_navigation_unclaimed = !rich_item_defers_navigation_claim(&item)
            && rich_navigation
                .as_ref()
                .is_some_and(|navigation| !navigation.cursor_claimed.get());
        let header_cursor_range = rich_header_navigation_range(
            &header_title,
            rich_navigation.as_ref(),
            !rich_item_body_paints_navigation(&item) || body_left_navigation_unclaimed,
        );
        let header_highlights = header_cursor_range
            .map(|range| {
                vec![(
                    range,
                    gpui::HighlightStyle {
                        background_color: rich_navigation.as_ref().map(|navigation| {
                            rich_navigation_text_highlight_background(navigation, cx)
                        }),
                        ..Default::default()
                    },
                )]
            })
            .unwrap_or_default();
        let highlighted_header_title =
            searchable_styled_text(header_title, header_highlights, header_search, cx);
        let highlighted_status = visible_status
            .as_ref()
            .map(|status| searchable_styled_text(status.clone(), Vec::new(), header_search, cx));

        let header = div()
            .id(("item-header", index))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Icon::new(icon)
                    .size(IconSize::Small)
                    .color(transcript_icon_color(item.kind, cursor)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_ui_sm(cx)
                    .text_color(if cursor {
                        colors.text
                    } else {
                        colors.text_muted
                    })
                    .child(highlighted_header_title),
            )
            .when_some(
                visible_status.zip(highlighted_status),
                |this, (status, highlighted)| {
                    this.child(
                        div()
                            .flex_none()
                            .text_ui_xs(cx)
                            .text_color(match transcript_status_color(&status) {
                                Color::Error => cx.theme().status().error,
                                Color::Warning => cx.theme().status().warning,
                                Color::Accent => colors.text_accent,
                                _ => colors.text_muted,
                            })
                            .child(highlighted),
                    )
                },
            )
            .when(is_disclosure, |this| {
                this.cursor_pointer()
                    .on_click(move |_, window, cx| {
                        disclosure_weak
                            .update(cx, |this, cx| {
                                if let Some(item) = this.model.items.get_mut(index) {
                                    item.expanded = !item.expanded;
                                    let item_key = item.key.clone();
                                    let collapsed = !item.expanded;
                                    this.list_state.splice(index..index + 1, 1);
                                    if rich_vim_experiment() {
                                        this.transcript_editor.update(cx, |editor, cx| {
                                            editor.set_item_collapsed(
                                                &item_key, collapsed, window, cx,
                                            );
                                        });
                                    }
                                    cx.notify();
                                }
                            })
                            .ok();
                    })
                    .child(Disclosure::new(("item-disclosure", index), item.expanded))
            });

        let raw = raw_visible.then(|| {
            let content =
                serde_json::to_string_pretty(&item.raw).unwrap_or_else(|_| item.raw.to_string());
            let highlighted = searchable_styled_text(content, Vec::new(), rich_search.as_ref(), cx);
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
                .child(highlighted)
                .into_any_element()
        });

        let content = if narrative {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .when(item.kind == model::TranscriptKind::User, |this| {
                    this.rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.element_background)
                        .px_2()
                        .py_1()
                })
                .when(
                    matches!(
                        item.kind,
                        model::TranscriptKind::Reasoning | model::TranscriptKind::Plan
                    ),
                    |this| this.py_1(),
                )
                .when(show_header, |this| this.child(header))
                .when_some(reasoning_preview, |this, preview| {
                    let highlighted =
                        searchable_styled_text(preview, Vec::new(), rich_search.as_ref(), cx);
                    this.child(
                        div()
                            .pl_6()
                            .text_sm()
                            .text_color(colors.text_muted)
                            .child(highlighted),
                    )
                })
                .when_some(search_context, |this, context| this.child(context))
                .when_some(body, |this, body| this.child(body))
                .when_some(raw, |this, raw| this.child(raw))
                .into_any_element()
        } else {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .when(!compact_trace, |this| {
                    this.rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .px_2()
                        .py_1()
                })
                .when(compact_trace, |this| this.px_1().py_1())
                .child(header)
                .when_some(search_context, |this, context| this.child(context))
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

        let element = div()
            .id(("transcript-item", index))
            .w_full()
            .px(if narrow { px(10.) } else { px(18.) })
            .py(if compact_trace {
                px(3.)
            } else if narrow && !narrative {
                px(6.)
            } else {
                px(8.)
            })
            .when(visual, |this| {
                this.bg(colors.element_selection_background.opacity(0.45))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_item = index;
                this.visual_anchor = None;
                if index + 1 < this.model.items.len() {
                    this.list_state.pause_following_tail();
                }
                this.focus_transcript(window, cx);
            }))
            .child(content)
            .into_any_element();
        if let Some(started_at) = render_started_at {
            let elapsed = started_at.elapsed();
            if elapsed >= Duration::from_millis(4) {
                eprintln!(
                    "slow-transcript phase=construct item={index} kind={:?} content_bytes={} elapsed_ms={:.2}",
                    item.kind,
                    item.content.len(),
                    elapsed.as_secs_f64() * 1_000.
                );
            }
        }
        element
    }
}

impl Render for HarnessApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let transcript_input_only = rich_vim_experiment() && !self.buffer_view;
        self.transcript_editor.update(cx, |editor, cx| {
            editor.set_input_only(transcript_input_only, cx)
        });
        self.sync_hybrid_surfaces(cx);
        if rich_vim_experiment() && !self.buffer_view {
            if self.rich_navigation_selection.is_none() {
                self.rich_navigation_selection = Some(
                    self.transcript_editor
                        .update(cx, |editor, cx| editor.selection_snapshot(cx)),
                );
            }
        } else {
            self.rich_navigation_selection = None;
        }
        self.sync_image_surfaces(window, cx);
        self.sync_request_surfaces(window, cx);
        let colors = cx.theme().colors().clone();
        let compact = window.viewport_size().width < px(COMPACT_SIDEBAR_THRESHOLD);
        let sidebar_visible = self.sidebar_open && (!compact || self.sidebar_user_override);
        let viewport_width = f32::from(window.viewport_size().width);
        let composer_text = self.composer.read(cx).text(cx);
        let composer_metrics = composer_render_metrics(
            &composer_text,
            composer_available_width(
                viewport_width,
                self.sidebar_open,
                self.sidebar_user_override,
            ),
            self.composer.read(cx).typography_profile(),
        );
        self.composer_metrics = composer_metrics;
        let composer_empty = composer_metrics.empty;
        let send_blocked = composer_send_blocked(
            composer_empty,
            self.loading_thread,
            self.thread_read_only_reason.is_some(),
            self.client.is_some() || self.replay_count.is_some(),
        );
        let composer_height = composer_metrics.height;
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
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .flex()
                .flex_col()
                .child(
                    list(
                        task_list_state.clone(),
                        cx.processor(|this, index, _, cx| this.render_task(index, cx)),
                    )
                    .flex_1()
                    .min_h_0(),
                )
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical)
                        .id("task-list-scrollbar")
                        .with_thumb_color(colors.text_muted.opacity(0.5))
                        .tracked_scroll_handle(&task_list_state),
                    window,
                    cx,
                )
                .into_any_element()
        };
        let transcript_body = if self.buffer_view {
            div()
                .flex_1()
                .min_h_0()
                // Give buffer-native cards a real outer gutter. This also
                // narrows soft wrapping without inserting transcript bytes.
                .px_4()
                .overflow_hidden()
                .bg(colors.editor_background)
                .child(self.transcript_editor.clone())
                .into_any_element()
        } else {
            let rich_list = div()
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(
                    list(
                        list_state.clone(),
                        cx.processor(|this, index, window, cx| this.render_item(index, window, cx)),
                    )
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden(),
                )
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical)
                        .id("rich-transcript-scrollbar")
                        .with_thumb_color(colors.text_muted.opacity(0.5))
                        .tracked_scroll_handle(&list_state),
                    window,
                    cx,
                );
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(rich_list)
                .when(rich_vim_experiment(), |this| {
                    // Keep the native Editor fully laid out so its Vim action
                    // handlers and motion state remain real, but position it
                    // outside the clipped Rich viewport. It contributes no
                    // pixels and cannot intercept pointer input.
                    this.child(
                        div()
                            .absolute()
                            .left(px(-4096.))
                            .top_0()
                            // Vim needs the real Rich-column width so wrapped
                            // motions agree with what the user sees. It does
                            // not need a second viewport-height Editor painted
                            // offscreen on every cursor move. A small five-row
                            // viewport gives native vertical motions enough
                            // context to resolve and autoscroll while keeping
                            // layout local to the cursor neighborhood.
                            .w_full()
                            .h(px(120.))
                            .opacity(0.)
                            .child(self.transcript_editor.clone()),
                    )
                })
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
        let composer_status: Option<(SharedString, Color)> =
            if let Some(status) = self.performance_status.clone() {
                Some((status, Color::Muted))
            } else if self.loading_thread {
                Some(("Loading task history…".into(), Color::Muted))
            } else if self.thread_read_only_reason.is_some() {
                Some(("Read-only · Ctrl-N for a new thread".into(), Color::Warning))
            } else if self.connecting {
                Some(("Connecting…".into(), Color::Muted))
            } else if self.client.is_none() && self.replay_count.is_none() {
                Some(("Offline · refresh to reconnect".into(), Color::Warning))
            } else {
                None
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
            .on_action(cx.listener(|this, _: &UseBufferTypography, window, cx| {
                this.use_transcript_typography(TranscriptTypographyProfile::Buffer, window, cx)
            }))
            .on_action(cx.listener(|this, _: &UseReadingTypography, window, cx| {
                this.use_transcript_typography(TranscriptTypographyProfile::Reading, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyPerformanceReport, window, cx| {
                this.copy_performance_report(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &RunPerformanceBenchmark, window, cx| {
                    this.run_performance_j(window, cx)
                }),
            )
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
                    this.list_state.set_follow_mode(FollowMode::Tail);
                }
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &ToggleItem, window, cx| this.toggle_selected(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleOutput, window, cx| {
                this.toggle_selected_output(window, cx)
            }))
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
            .on_action(cx.listener(|this, action: &VimWordNext, window, cx| {
                if !this.buffer_view {
                    cx.propagate();
                    return;
                }
                this.transcript_editor.update(cx, |editor, cx| {
                    editor.search_word_under_cursor(
                        false,
                        action.partial_word(),
                        action.case_sensitive(),
                        window,
                        cx,
                    );
                });
                cx.notify();
            }))
            .on_action(cx.listener(|this, action: &VimWordPrevious, window, cx| {
                if !this.buffer_view {
                    cx.propagate();
                    return;
                }
                this.transcript_editor.update(cx, |editor, cx| {
                    editor.search_word_under_cursor(
                        true,
                        action.partial_word(),
                        action.case_sensitive(),
                        window,
                        cx,
                    );
                });
                cx.notify();
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
                                .px_1()
                                .flex()
                                .items_center()
                                .gap_1()
                                .border_b_1()
                                .border_color(colors.border)
                                .child(
                                    IconButton::new(
                                        "hide-sidebar",
                                        IconName::ThreadsSidebarLeftOpen,
                                    )
                                    .shape(IconButtonShape::Square)
                                    .size(ButtonSize::Default)
                                    .style(ButtonStyle::Subtle)
                                    .aria_label("Hide thread list")
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.toggle_sidebar(window, cx),
                                    )),
                                )
                                .child(div().flex_1())
                                .child(
                                    IconButton::new("transcript-view", IconName::Code)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Default)
                                        .style(if self.buffer_view {
                                            ButtonStyle::Tinted(TintColor::Accent)
                                        } else {
                                            ButtonStyle::Subtle
                                        })
                                        .aria_label(if self.buffer_view {
                                            "Show rich transcript"
                                        } else {
                                            "Show Vim text view"
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.toggle_buffer_view(window, cx)
                                        })),
                                )
                                .child(
                                    IconButton::new("refresh-tasks", IconName::RotateCw)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Default)
                                        .style(ButtonStyle::Subtle)
                                        .aria_label("Refresh threads")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.refresh_threads(cx)),
                                        ),
                                )
                                .child(
                                    IconButton::new("new-task", IconName::Plus)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Default)
                                        .style(ButtonStyle::Subtle)
                                        .aria_label("New thread")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.new_task(window, cx)
                                        })),
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
                    .relative()
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
                            .h(px(composer_height))
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
                                    .child(self.mode_indicator.clone())
                                    .when_some(composer_status, |this, (status, color)| {
                                        this.child(
                                            Label::new(status).size(LabelSize::XSmall).color(color),
                                        )
                                    })
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
                                                .disabled(send_blocked)
                                                .icon_color(if send_blocked {
                                                    Color::Muted
                                                } else {
                                                    Color::Accent
                                                })
                                                .aria_label(if self.loading_thread {
                                                    "Wait for task history to finish loading"
                                                } else if self.thread_read_only_reason.is_some() {
                                                    "This task is open read-only"
                                                } else if self.client.is_none()
                                                    && self.replay_count.is_none()
                                                {
                                                    "Reconnect to Codex before sending"
                                                } else if composer_empty {
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
                    .when(!sidebar_visible, |this| {
                        this.child(
                            deferred(
                                div()
                                    .absolute()
                                    .top(px(6.))
                                    .left(px(6.))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border)
                                    .bg(colors.panel_background.opacity(0.92))
                                    .child(
                                        IconButton::new(
                                            "show-sidebar",
                                            IconName::ThreadsSidebarLeftClosed,
                                        )
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Default)
                                        .style(ButtonStyle::Subtle)
                                        .aria_label("Show thread list")
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.toggle_sidebar(window, cx)
                                            }),
                                        ),
                                    ),
                            )
                            .with_priority(1),
                        )
                    }),
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

fn transcript_icon_color(kind: model::TranscriptKind, selected: bool) -> Color {
    if selected {
        return Color::Accent;
    }
    match kind {
        model::TranscriptKind::Error => Color::Error,
        model::TranscriptKind::Approval => Color::Warning,
        model::TranscriptKind::FileChange | model::TranscriptKind::Diff => Color::Modified,
        _ => Color::Muted,
    }
}

fn transcript_status_color(status: &str) -> Color {
    let normalized = status.to_ascii_lowercase();
    if normalized.contains("error")
        || normalized.contains("fail")
        || normalized.contains("declin")
        || normalized.contains("denied")
    {
        Color::Error
    } else if normalized.contains("waiting")
        || normalized.contains("approval")
        || normalized.contains("interrupt")
    {
        Color::Warning
    } else if normalized.contains("stream")
        || normalized.contains("running")
        || normalized.contains("progress")
    {
        Color::Accent
    } else {
        Color::Muted
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

fn thread_has_active_turn(thread: &CodexThread) -> bool {
    let Some(turn) = thread.turns.last() else {
        return false;
    };
    let status = turn
        .status
        .as_str()
        .or_else(|| turn.status.get("type").and_then(Value::as_str))
        .or_else(|| turn.status.get("status").and_then(Value::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        status.as_str(),
        "active" | "inprogress" | "in_progress" | "running" | "streaming"
    )
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

enum AutomaticPerformanceCapture {
    Timed(Duration),
    UntilClose,
}

fn automatic_performance_capture() -> Option<AutomaticPerformanceCapture> {
    let value = std::env::var("HARNESS_PERF_CAPTURE_SECONDS").ok()?;
    if value.eq_ignore_ascii_case("close") {
        return Some(AutomaticPerformanceCapture::UntilClose);
    }
    let seconds = value.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds > 0.)
        .then(|| AutomaticPerformanceCapture::Timed(Duration::from_secs_f64(seconds)))
}

fn slow_list_diagnostics_enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("GPUI_SLOW_LIST_DIAGNOSTICS")
            .is_some_and(|value| !value.is_empty() && value != std::ffi::OsStr::new("0"))
    });
    *ENABLED
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
        KeyBinding::new("enter", ToggleOutput, Some("HarnessTranscript")),
        KeyBinding::new("space", ToggleItem, Some("HarnessTranscript")),
        KeyBinding::new("z a", ToggleItem, Some("HarnessTranscript")),
        KeyBinding::new("r", ToggleRaw, Some("HarnessTranscript")),
        KeyBinding::new("v", ToggleVisual, Some("HarnessTranscript")),
        KeyBinding::new("y", YankItem, Some("HarnessTranscript")),
        KeyBinding::new("/", OpenSearch, Some("HarnessTranscript")),
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
            "i",
            FocusComposer,
            Some("HarnessBuffer && Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "a",
            FocusComposer,
            Some("HarnessBuffer && Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "o",
            FocusComposer,
            Some("HarnessBuffer && Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "z a",
            ToggleItem,
            Some("HarnessBuffer && Editor && VimControl && vim_mode == normal"),
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

    #[test]
    fn rich_vim_markdown_offsets_skip_formatting_marks_in_both_directions() {
        let logical = "Build a real Vim composer with café.";
        let source = "Build a **real Vim composer** with [café](https://example.com).";

        let real = logical.find("real").unwrap();
        let source_real = source.find("real").unwrap();
        assert_eq!(
            markdown_source_offset_for_logical(logical, source, real),
            source_real
        );
        assert_eq!(
            markdown_logical_offset_for_source(logical, source, source_real),
            real
        );

        let cafe = logical.find("café").unwrap();
        let source_cafe = source.find("café").unwrap();
        assert_eq!(
            markdown_source_offset_for_logical(logical, source, cafe),
            source_cafe
        );
        assert_eq!(
            markdown_logical_offset_for_source(logical, source, source_cafe),
            cafe
        );
    }

    #[test]
    fn rich_vim_markdown_offsets_remain_aligned_after_link_and_fenced_code() {
        let source = "I updated [discord-canary.desktop](/home/smt/.local/share/applications/discord-canary.desktop) to inject:\n\n```text\nXDG_SESSION_TYPE=wayland\n```\n\nOne more restart.";
        let logical = model::rich_markdown_navigation_text(source);

        for needle in ["XDG_SESSION_TYPE", "wayland", "One more", "restart"] {
            let source_offset = source.find(needle).unwrap();
            let logical_offset = logical.find(needle).unwrap();
            assert_eq!(
                markdown_logical_offset_for_source(&logical, source, source_offset),
                logical_offset,
                "source to logical mapping diverged at {needle}"
            );
            assert_eq!(
                markdown_source_offset_for_logical(&logical, source, logical_offset),
                source_offset,
                "logical to source mapping diverged at {needle}"
            );
        }
    }

    #[test]
    fn rich_command_cursor_marker_uses_the_exact_utf8_glyph_in_each_surface() {
        let command = "/usr/bin/bash -lc 'printf café'";
        let output = "ok\nfinished";
        let body = format!("{command}\n{output}");
        let cursor = body.find("é").unwrap();
        let navigation = RichNavigationPaint {
            body_text: body.clone().into(),
            ranges: Vec::new(),
            head: Some(cursor),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };

        assert_eq!(
            rich_cursor_index_for_fragment(Some(&navigation), &(0..command.len())),
            Some(cursor),
            "the explicit painted cursor must retain its UTF-8 byte position in command text"
        );
        assert_eq!(
            rich_cursor_index_for_fragment(Some(&navigation), &(command.len() + 1..body.len())),
            None,
            "only the surface containing the cursor may paint it"
        );

        let output_cursor = body.find("finished").unwrap() + "fin".len();
        let output_navigation = RichNavigationPaint {
            body_text: body.into(),
            ranges: Vec::new(),
            head: Some(output_cursor),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            rich_cursor_index_for_fragment(
                Some(&output_navigation),
                &(command.len() + 1..command.len() + 1 + output.len())
            ),
            Some(output_cursor - command.len() - 1),
            "output cursor paint must be relative to the output row"
        );
    }

    #[test]
    fn rich_navigation_fragments_preserve_order_and_skip_ornaments() {
        let navigation = RichNavigationPaint {
            body_text: "same\nvisible body\nsame".into(),
            ranges: Vec::new(),
            head: Some(0),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        let mut cursor = 0;

        assert_eq!(
            rich_navigation_fragment_range(Some(&navigation), "same", &mut cursor),
            0..4
        );
        assert_eq!(
            rich_navigation_fragment_range(Some(&navigation), "ornamental label", &mut cursor),
            4..4
        );
        assert_eq!(
            rich_navigation_fragment_range(Some(&navigation), "same", &mut cursor),
            18..22
        );
    }

    #[test]
    fn rich_navigation_bodies_contain_only_rows_painted_by_structured_renderers() {
        let replay = TranscriptModel::replay(6);
        let diff = rich_navigation_item_projection(&replay, 4).unwrap();
        assert!(
            diff.body_text()
                .starts_with("crates/harness_app/src/main.rs\n@@ -83,7 +83,10 @@")
        );
        assert!(!diff.body_text().contains("diff --git"));

        let command = rich_navigation_item_projection(&replay, 5).unwrap();
        assert_eq!(
            command.body_text(),
            "cargo check -p harness_app\nFinished replay frame 5 without blocking paint"
        );
        assert!(
            rich_item_defers_navigation_claim(&replay.items[5]),
            "virtual command rows must own the cursor after header construction"
        );
        assert!(!rich_item_defers_navigation_claim(&replay.items[4]));

        let project = |kind, content: &str, raw: Value| {
            let mut model = TranscriptModel::default();
            model.items.push(TranscriptItem {
                key: "canonical-fixture".into(),
                protocol_id: Some("canonical-fixture".into()),
                kind,
                title: "Fixture".into(),
                status: None,
                content: content.into(),
                raw,
                event_count: 1,
                expanded: true,
                pending_request: None,
            });
            rich_navigation_item_projection(&model, 0)
                .unwrap()
                .body_text()
                .to_owned()
        };

        assert_eq!(
            project(
                model::TranscriptKind::Tool,
                "Arguments\n{\"query\":\"x\"}\n\nResult\nok",
                Value::Null,
            ),
            "Arguments\n{\"query\":\"x\"}\nResult\nok"
        );
        assert_eq!(
            project(
                model::TranscriptKind::FileChange,
                "Modified · /tmp/a\n@@ -1 +1 @@\n-old\n+new",
                Value::Null,
            ),
            "/tmp/a\n@@ -1 +1 @@\n-old\n+new"
        );
        assert_eq!(
            project(
                model::TranscriptKind::Reasoning,
                "**One**\n\n- Two",
                Value::Null,
            ),
            "One\nTwo"
        );
        assert_eq!(
            project(
                model::TranscriptKind::Web,
                "Query\nold protocol text\n\nResults\nold protocol result",
                json!({
                    "action": {"query": "zed"},
                    "results": [{
                        "title": "Zed Docs",
                        "url": "https://zed.dev/docs",
                        "snippet": "Fast editor"
                    }]
                }),
            ),
            "Zed Docs\nhttps://zed.dev/docs\nFast editor"
        );
    }

    #[test]
    fn virtual_command_rows_preserve_the_navigation_document() {
        let replay = TranscriptModel::replay(6);
        let item = &replay.items[5];
        let data = rich_command_data(item).expect("replay command should be structured");
        let projection = rich_navigation_item_projection(&replay, 5)
            .expect("command should participate in rich navigation");
        let body = projection.body_text();

        assert_eq!(&*data.command, "cargo check -p harness_app");
        assert!(data.output.starts_with("Finished replay frame 5"));
        assert!(!data.output.ends_with('\n'));
        assert_eq!(data.command_row_count, 1);
        assert_eq!(data.rows.len(), 2);
        assert!(matches!(data.rows[0].source, RichCommandSource::Command));
        assert!(matches!(data.rows[1].source, RichCommandSource::Output));

        for row in data.rows.iter() {
            let source_text = match row.source {
                RichCommandSource::Command => &data.command,
                RichCommandSource::Output => &data.output,
            };
            let logical_range = rich_command_row_logical_range(&data, row);
            assert_eq!(&body[logical_range], &source_text[row.source_range.clone()]);
        }
    }

    #[test]
    fn command_row_ranges_normalize_line_endings_and_drop_only_terminal_newlines() {
        let normalized = normalize_command_line_endings("one\r\ntwo\rthree\r\n".into());
        assert_eq!(normalized, "one\ntwo\nthree\n");
        assert_eq!(command_output_for_display(&normalized), "one\ntwo\nthree");
        assert_eq!(
            command_line_ranges(command_output_for_display(&normalized)).collect::<Vec<_>>(),
            vec![0..3, 4..7, 8..13]
        );
    }

    #[test]
    fn rich_navigation_document_has_no_unpainted_terminal_row() {
        let replay = TranscriptModel::replay(6);
        let document = rich_navigation_document(&replay);
        let last = document.segments.last().unwrap();

        assert!(!document.text.ends_with('\n'));
        assert_eq!(last.whole_range.end, document.text.len());
        assert_eq!(last.body_range.end, document.text.len());
        assert_eq!(
            &document.text[last.body_range.clone()],
            "cargo check -p harness_app\nFinished replay frame 5 without blocking paint"
        );

        let previous = &document.segments[document.segments.len() - 2];
        assert_eq!(
            &document.text[previous.whole_range.end - 1..previous.whole_range.end],
            "\n"
        );
        assert_eq!(previous.whole_range.end, last.whole_range.start);
    }

    #[test]
    fn appending_visible_item_transfers_the_terminal_separator() {
        let before_model = TranscriptModel::replay(5);
        let before = rich_navigation_document(&before_model);
        assert!(!before.text.ends_with('\n'));

        let after_model = TranscriptModel::replay(6);
        let previous_last = rich_navigation_item_projection(&after_model, 4).unwrap();
        let appended = rich_navigation_item_projection(&after_model, 5).unwrap();
        assert!(previous_last.text.ends_with('\n'));
        assert!(!appended.text.ends_with('\n'));

        let mut incrementally_appended = before.text;
        incrementally_appended.push('\n');
        incrementally_appended.push_str(&appended.text);
        assert_eq!(
            incrementally_appended,
            rich_navigation_document(&after_model).text
        );
    }

    #[test]
    fn rich_navigation_preserves_each_visual_block_row() {
        let navigation = RichNavigationPaint {
            body_text: "abcdefghijkl".into(),
            ranges: vec![2..4, 8..10],
            head: Some(9),
            visual: true,
            cursor_claimed: Rc::new(Cell::new(false)),
        };

        assert_eq!(
            navigation_ranges_for_fragment(&navigation, 3..9),
            [0..1, 5..6]
        );
        assert_eq!(
            navigation_ranges_for_fragment(&navigation, 10..12),
            Vec::<Range<usize>>::new()
        );
    }

    #[test]
    fn rich_navigation_cursor_crosses_newlines_and_hidden_furniture_once() {
        let navigation = RichNavigationPaint {
            body_text: "alpha\n<hidden>beta\ngamma".into(),
            ranges: Vec::new(),
            head: Some(7),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };

        assert!(navigation_ranges_for_fragment(&navigation, 0..5).is_empty());
        assert_eq!(navigation_ranges_for_fragment(&navigation, 14..18), [0..1]);
        assert!(navigation_ranges_for_fragment(&navigation, 19..24).is_empty());

        let navigation = RichNavigationPaint {
            body_text: "alpha\nbeta".into(),
            ranges: Vec::new(),
            head: Some(5),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        assert!(navigation_ranges_for_fragment(&navigation, 0..5).is_empty());
        assert_eq!(navigation_ranges_for_fragment(&navigation, 6..10), [0..1]);
    }

    #[test]
    fn rich_nested_cursor_reveals_exact_rows_and_skips_unpainted_furniture() {
        let rows = [Some(0..5), None, Some(14..18), Some(19..24)];

        assert_eq!(rich_nested_cursor_row(2, &rows), Some(0));
        assert_eq!(rich_nested_cursor_row(15, &rows), Some(2));
        assert_eq!(rich_nested_cursor_row(20, &rows), Some(3));
        assert_eq!(
            rich_nested_cursor_row(7, &rows),
            Some(2),
            "a logical cursor in protocol-only furniture must reveal the next painted glyph"
        );
        assert_eq!(rich_nested_cursor_row(25, &rows), None);
    }

    #[test]
    fn rich_navigation_cursor_has_one_visible_structured_owner_at_every_offset() {
        let body = "alpha\n<hidden>beta\ngamma\n";
        let visible_fragments = [0..5, 14..18, 19..24];

        for head in (0..=body.len()).filter(|offset| body.is_char_boundary(*offset)) {
            let navigation = RichNavigationPaint {
                body_text: body.into(),
                ranges: Vec::new(),
                head: Some(head),
                visual: false,
                cursor_claimed: Rc::new(Cell::new(false)),
            };
            let owners = visible_fragments
                .iter()
                .filter(|fragment| {
                    !navigation_ranges_for_fragment(&navigation, (*fragment).clone()).is_empty()
                })
                .count();
            assert_eq!(owners, 1, "cursor owner count at body byte {head}");
        }
    }

    #[test]
    fn rich_header_navigation_is_a_single_fallback_owner() {
        let navigation = RichNavigationPaint {
            body_text: "hidden body".into(),
            ranges: Vec::new(),
            head: Some(3),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };

        assert_eq!(
            rich_header_navigation_range("Command", Some(&navigation), true),
            Some(0..1)
        );
        assert!(navigation.cursor_claimed.get());
        assert_eq!(
            rich_header_navigation_range("Command", Some(&navigation), true),
            None
        );

        let visible_body = RichNavigationPaint {
            body_text: "visible body".into(),
            ranges: Vec::new(),
            head: Some(3),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            rich_header_navigation_range("Command", Some(&visible_body), false),
            None,
            "a visible virtual body claims its cursor later during list layout"
        );
        assert!(!visible_body.cursor_claimed.get());

        let unicode = RichNavigationPaint {
            body_text: "hidden body".into(),
            ranges: Vec::new(),
            head: Some(3),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            rich_header_navigation_range("🧭 Tool", Some(&unicode), true),
            Some(0.."🧭".len())
        );

        let visual = RichNavigationPaint {
            body_text: "hidden body".into(),
            ranges: vec![2..7],
            head: Some(6),
            visual: true,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            rich_header_navigation_range("Command", Some(&visual), true),
            Some(0.."Command".len()),
            "a collapsed card should visibly represent its hidden selection"
        );
        assert_eq!(
            rich_header_navigation_range("Command", Some(&visual), false),
            None,
            "an expanded card paints Visual mode on its body fragments"
        );
    }

    #[test]
    fn rich_structured_text_hit_testing_stays_inside_its_logical_fragment() {
        let fragment = 10..15;

        assert_eq!(logical_offset_for_rendered_index(&fragment, 0), 10);
        assert_eq!(logical_offset_for_rendered_index(&fragment, 3), 13);
        assert_eq!(logical_offset_for_rendered_index(&fragment, 5), 15);
        assert_eq!(logical_offset_for_rendered_index(&fragment, usize::MAX), 15);
    }

    #[test]
    fn composer_focus_enters_the_selected_item_once_then_preserves_vim_position() {
        assert_eq!(
            rich_transcript_entry_placement(false, Some(7), Some(7)),
            Some(7),
            "the Editor's default cursor is not a meaningful saved position"
        );
        assert_eq!(
            rich_transcript_entry_placement(true, Some(7), Some(7)),
            None,
            "returning from the composer must preserve an established Vim cursor"
        );
        assert_eq!(
            rich_transcript_entry_placement(true, Some(6), Some(7)),
            Some(7),
            "a newly selected streaming item should receive spatial focus"
        );
    }

    #[test]
    fn rich_diff_navigation_starts_on_the_visible_file_path() {
        let mut model = TranscriptModel::default();
        model.items.push(TranscriptItem {
            key: "diff-fixture".into(),
            protocol_id: Some("diff-fixture".into()),
            kind: model::TranscriptKind::Diff,
            title: "Working tree diff".into(),
            status: None,
            content: "diff --git a/src/main.rs b/src/main.rs\n@@ -1 +1 @@\n-old\n+new".into(),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        });
        let projection = rich_navigation_item_projection(&model, 0).unwrap();
        let body = projection.body_text();
        let presentation = diff_file_presentations(&model.items[0].content)
            .pop()
            .unwrap();
        assert_eq!(body, "src/main.rs\n@@ -1 +1 @@\n-old\n+new");

        let normal = RichNavigationPaint {
            body_text: body.into(),
            ranges: Vec::new(),
            head: Some(0),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        let mut logical_cursor = 0;
        let path_range =
            rich_navigation_fragment_range(Some(&normal), &presentation.path, &mut logical_cursor);
        let content_range = rich_navigation_fragment_range(
            Some(&normal),
            &presentation.content,
            &mut logical_cursor,
        );
        assert_eq!(path_range, 0.."src/main.rs".len());
        assert_eq!(&body[path_range.clone()], "src/main.rs");
        assert_eq!(
            navigation_ranges_for_fragment(&normal, path_range.clone()),
            [0..1]
        );
        assert!(navigation_ranges_for_fragment(&normal, content_range.clone()).is_empty());

        let visual = RichNavigationPaint {
            body_text: body.into(),
            ranges: vec![0..content_range.start + 2],
            head: Some(content_range.start + 1),
            visual: true,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            navigation_ranges_for_fragment(&visual, path_range.clone()),
            [0..path_range.len()]
        );
        assert_eq!(
            navigation_ranges_for_fragment(&visual, content_range),
            [0..2]
        );
    }

    #[test]
    fn rich_vim_markdown_preserves_each_visual_block_row() {
        let logical = "abcde\nabcde";
        let source = "**abcde**\n*abcde*";
        let navigation = RichNavigationPaint {
            body_text: logical.into(),
            ranges: vec![1..3, 7..9],
            head: Some(8),
            visual: true,
            cursor_claimed: Rc::new(Cell::new(false)),
        };

        let paint = navigation.markdown_source_navigation(source);
        assert_eq!(paint.selections.len(), 2);
        assert_eq!(&source[paint.selections[0].clone()], "bc");
        assert_eq!(&source[paint.selections[1].clone()], "bc");
        assert!(paint.selections[0].end < paint.selections[1].start);
        assert_eq!(paint.cursor, None);
    }

    #[test]
    fn rich_vim_markdown_keeps_cursor_separate_from_selection() {
        let logical = "Build a real Vim composer.";
        let source = "Build a **real Vim composer**.";
        let head = logical.find("real").unwrap();
        let navigation = RichNavigationPaint {
            body_text: logical.into(),
            ranges: Vec::new(),
            head: Some(head),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };

        let paint = navigation.markdown_source_navigation(source);
        assert!(paint.selections.is_empty());
        assert_eq!(paint.cursor, source.find("real"));
        assert!(navigation.cursor_claimed.get());

        let navigation = RichNavigationPaint {
            body_text: logical.into(),
            ranges: Vec::new(),
            head: Some(logical.len()),
            visual: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        let paint = navigation.markdown_source_navigation(source);
        assert_eq!(paint.cursor, source.rfind('.'));
    }

    #[test]
    fn performance_j_run_has_one_preparation_one_baseline_and_exact_move_count() {
        let mut run = PerformanceJRunState::new(7);
        let mut steps = Vec::new();
        while let Some(step) = run.next_step(7) {
            steps.push(step);
        }

        assert_eq!(steps.first(), Some(&PerformanceJStep::Prepare));
        assert_eq!(steps.get(1), Some(&PerformanceJStep::Baseline));
        assert_eq!(steps.last(), Some(&PerformanceJStep::Report));
        assert_eq!(
            steps
                .iter()
                .filter(|step| matches!(step, PerformanceJStep::Dispatch { .. }))
                .count(),
            usize::from(PERFORMANCE_J_STEPS)
        );
        assert!(steps.windows(2).all(|pair| {
            !matches!(
                pair,
                [
                    PerformanceJStep::Dispatch { down: first },
                    PerformanceJStep::Dispatch { down: second }
                ] if first == second
            )
        }));
        assert_eq!(run.phase, PerformanceJPhase::Complete);
    }

    #[test]
    fn stale_performance_j_generation_does_not_advance_the_active_run() {
        let mut run = PerformanceJRunState::new(9);

        assert_eq!(run.next_step(8), None);
        assert_eq!(run.phase, PerformanceJPhase::Prepare);
        assert_eq!(run.next_step(9), Some(PerformanceJStep::Prepare));
    }

    #[test]
    fn performance_jk_requires_two_lines_for_repeated_motion() {
        assert!(!performance_j_has_room(1));
        assert!(performance_j_has_room(2));
    }

    #[test]
    fn performance_j_uses_one_multiline_markdown_surface() {
        let model = TranscriptModel::replay(3);

        assert_eq!(performance_j_candidate(&model), Some(2));
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(reconnect_delay(0), Some(Duration::from_secs(1)));
        assert_eq!(reconnect_delay(1), Some(Duration::from_secs(2)));
        assert_eq!(reconnect_delay(2), Some(Duration::from_secs(4)));
        assert_eq!(reconnect_delay(3), None);
        assert_eq!(reconnect_delay(u8::MAX), None);
    }

    #[test]
    fn read_only_refresh_rate_follows_the_latest_turn_state() {
        use codex_app_server_client::CodexTurn;

        let thread = |status: Value| CodexThread {
            id: "thread-1".into(),
            name: None,
            preview: String::new(),
            cwd: String::new(),
            updated_at: 1,
            turns: vec![CodexTurn {
                id: "turn-1".into(),
                status,
                items: Vec::new(),
            }],
        };
        assert!(thread_has_active_turn(&thread(json!("inProgress"))));
        assert!(thread_has_active_turn(&thread(
            json!({"status": "running"})
        )));
        assert!(!thread_has_active_turn(&thread(json!("completed"))));
        assert!(!thread_has_active_turn(&thread(json!("interrupted"))));
    }

    #[test]
    fn transcript_chrome_uses_semantic_tones_only_when_they_add_information() {
        assert_eq!(
            transcript_icon_color(model::TranscriptKind::FileChange, false),
            Color::Modified
        );
        assert_eq!(
            transcript_icon_color(model::TranscriptKind::Error, false),
            Color::Error
        );
        assert_eq!(
            transcript_icon_color(model::TranscriptKind::Command, false),
            Color::Muted
        );
        assert_eq!(
            transcript_icon_color(model::TranscriptKind::Error, true),
            Color::Accent
        );
        assert_eq!(transcript_status_color("failed"), Color::Error);
        assert_eq!(transcript_status_color("waiting"), Color::Warning);
        assert_eq!(transcript_status_color("in progress"), Color::Accent);
        assert_eq!(transcript_status_color("custom status"), Color::Muted);
    }

    #[test]
    fn large_structured_output_gets_a_stable_non_scrolling_preview() {
        assert_eq!(
            structured_output_preview("one\ntwo\nthree", "output"),
            StructuredOutputPreview {
                content: "one\ntwo\nthree".into(),
                footer: None,
            }
        );
        let content = std::iter::repeat_n("line", STRUCTURED_OUTPUT_PREVIEW_LINES + 2)
            .collect::<Vec<_>>()
            .join("\n");
        let preview = structured_output_preview(&content, "output");
        assert_eq!(
            preview.content.lines().count(),
            STRUCTURED_OUTPUT_PREVIEW_LINES
        );
        assert_eq!(preview.footer.as_deref(), Some("Show 2 more output lines"));

        let preview =
            structured_output_preview(&"é".repeat(STRUCTURED_OUTPUT_PREVIEW_BYTES), "output");
        assert!(preview.content.is_char_boundary(preview.content.len()));
        assert_eq!(preview.footer.as_deref(), Some("Show more output"));

        let command = std::iter::repeat_n("echo 'hello'", COMMAND_PREVIEW_LINES + 3)
            .collect::<Vec<_>>()
            .join("\n");
        let command_preview = structured_output_preview_with_limits(
            &command,
            "command",
            COMMAND_PREVIEW_LINES,
            COMMAND_PREVIEW_BYTES,
        );
        assert_eq!(
            command_preview.content.lines().count(),
            COMMAND_PREVIEW_LINES
        );
        assert_eq!(
            command_preview.footer.as_deref(),
            Some("Show 3 more command lines")
        );
    }

    #[test]
    fn aggregate_diff_parser_recovers_files_and_per_file_stats() {
        let presentations = diff_file_presentations(
            "diff --git a/src/first.rs b/src/first.rs\n\
             index 111..222 100644\n\
             --- a/src/first.rs\n\
             +++ b/src/first.rs\n\
             @@ -1,2 +1,3 @@\n\
              context\n\
             -old\n\
             +new\n\
             +another\n\
             diff --git \"a/docs/name with spaces.md\" \"b/docs/name with spaces.md\"\n\
             --- \"a/docs/name with spaces.md\"\n\
             +++ \"b/docs/name with spaces.md\"\n\
             @@ -4 +4 @@\n\
             -before\n\
             +after",
        );

        assert_eq!(presentations.len(), 2);
        assert_eq!(presentations[0].path, "src/first.rs");
        assert_eq!(presentations[1].path, "docs/name with spaces.md");
        assert_eq!(diff_content_counts(&presentations[0].content), (2, 1));
        assert_eq!(diff_content_counts(&presentations[1].content), (1, 1));
        assert_eq!(
            aggregate_diff_counts(
                presentations
                    .iter()
                    .map(|presentation| presentation.content.as_str())
            ),
            (3, 2)
        );
    }

    #[test]
    fn progressive_file_lines_cannot_let_first_huge_patch_starve_later_headers() {
        let allocations = fair_line_allocations(&[10_000, 2, 10_000], 18);
        assert_eq!(allocations, vec![6, 2, 10]);
        assert!(allocations.iter().sum::<usize>() <= 18);

        let metadata_after_huge_patch = fair_line_allocations(&[10_000, 0, 0], 18);
        assert_eq!(metadata_after_huge_patch, vec![6, 0, 0]);
        assert_eq!(
            fair_line_allocations(&[10_000, 2, 10_000], usize::MAX),
            vec![10_000, 2, 10_000]
        );
    }

    #[test]
    fn aggregate_diff_search_sees_every_nested_scroll_row() {
        let first_lines = std::iter::once("@@ -1 +1 @@".to_string())
            .chain((0..40).map(|index| format!(" context {index}")))
            .chain(std::iter::once("+hidden-tail-needle".into()))
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!(
            "diff --git a/src/first.rs b/src/first.rs\n{first_lines}\n\
             diff --git a/src/later.rs b/src/later.rs\n\
             @@ -1 +1 @@\n\
             +visible-later-needle"
        );
        let item = TranscriptItem {
            key: "aggregate-diff".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Diff,
            title: "Working tree diff".into(),
            status: None,
            content,
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "later.rs"
        ));
        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "visible-later-needle"
        ));
        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "hidden-tail-needle"
        ));
        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::All,
            "hidden-tail-needle"
        ));
    }

    #[test]
    fn hybrid_diff_block_resizes_to_a_compact_collapsed_header() {
        let mut item = TranscriptItem {
            key: "hybrid-diff".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Diff,
            title: "Working tree diff".into(),
            status: None,
            content: "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n-old\n+new".into(),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(hybrid_structured_rows(&item) >= 6);
        item.expanded = false;
        assert_eq!(hybrid_structured_rows(&item), 2);
    }

    #[test]
    fn hybrid_command_requires_a_parseable_command_and_has_bounded_rows() {
        let mut item = TranscriptItem {
            key: "hybrid-command".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Command,
            title: "Command".into(),
            status: None,
            content: "$ cargo check -p harness_app\n\nFinished successfully".into(),
            raw: json!({"command":"cargo check -p harness_app"}),
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(item_uses_hybrid_surface(&item));
        assert!((4..=18).contains(&hybrid_structured_rows(&item)));

        item.content = "unstructured output".into();
        item.raw = Value::Null;
        assert!(!item_uses_hybrid_surface(&item));
    }

    #[test]
    fn multi_file_change_search_sees_every_nested_scroll_row() {
        let first_lines = std::iter::once("@@ -1 +1 @@".to_string())
            .chain((0..40).map(|index| format!(" context {index}")))
            .chain(std::iter::once("+hidden-first-tail".into()))
            .collect::<Vec<_>>()
            .join("\n");
        let item = TranscriptItem {
            key: "multi-file-change".into(),
            protocol_id: None,
            kind: model::TranscriptKind::FileChange,
            title: "File changes · 2 files".into(),
            status: None,
            content: format!(
                "Modified · /tmp/first.rs\n{first_lines}\n\n\
                 Modified · /tmp/later.rs\n\
                 @@ -1 +1 @@\n\
                 +visible-later-change"
            ),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "later.rs"
        ));
        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "visible-later-change"
        ));
        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "hidden-first-tail"
        ));
    }

    #[test]
    fn exact_image_path_body_is_suppressed_without_hiding_real_caption_text() {
        let mut item = TranscriptItem {
            key: "image".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Image,
            title: "Viewed image · preview.png".into(),
            status: None,
            content: "/tmp/preview.png".into(),
            raw: json!({"path": "/tmp/preview.png"}),
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert_eq!(image_caption_for_display(&item), None);
        assert!(!item_matches_folded_query(&item, "/tmp/preview.png"));

        item.content = "/tmp/preview.png\n\nRevised prompt\nA detailed scene".into();
        assert_eq!(
            image_caption_for_display(&item),
            Some("/tmp/preview.png\n\nRevised prompt\nA detailed scene")
        );
        assert!(item_matches_folded_query(&item, "detailed scene"));
    }

    #[test]
    fn rich_search_context_is_reserved_for_collapsed_cards() {
        let mut item = TranscriptItem {
            key: "tool-1".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Tool,
            title: "Tool".into(),
            status: None,
            content: std::iter::repeat_n("output", STRUCTURED_OUTPUT_PREVIEW_LINES + 1)
                .collect::<Vec<_>>()
                .join("\n"),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(!rich_search_match_needs_context(
            &item,
            OutputExpansion::Preview
        ));
        assert!(!rich_search_match_needs_context(
            &item,
            OutputExpansion::All
        ));
        item.expanded = false;
        assert!(rich_search_match_needs_context(&item, OutputExpansion::All));
    }

    #[test]
    fn rich_search_does_not_duplicate_visible_file_metadata() {
        let mut lines = vec!["@@ -1 +1 @@".to_string()];
        lines.extend((0..STRUCTURED_OUTPUT_PREVIEW_LINES).map(|index| format!(" context {index}")));
        lines.push("+hidden needle".into());
        let item = TranscriptItem {
            key: "file-1".into(),
            protocol_id: None,
            kind: model::TranscriptKind::FileChange,
            title: "File change · 1 file".into(),
            status: None,
            content: format!(
                "Modified · /tmp/REVERSE_ENGINEERING.md\n{}",
                lines.join("\n")
            ),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "reverse_engineering.md"
        ));
        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::Preview,
            "hidden needle"
        ));
        assert!(rich_search_query_is_visible(
            &item,
            OutputExpansion::All,
            "hidden needle"
        ));
    }

    #[test]
    fn tool_activity_sections_preserve_result_paragraphs() {
        assert_eq!(
            activity_text_sections(
                "Arguments\n{\"path\":\"README.md\"}\n\nResult\nFirst paragraph\n\nSecond paragraph"
            ),
            vec![
                ActivityTextSection {
                    heading: Some("Arguments".into()),
                    body: "{\"path\":\"README.md\"}".into(),
                },
                ActivityTextSection {
                    heading: Some("Result".into()),
                    body: "First paragraph\n\nSecond paragraph".into(),
                },
            ]
        );
    }

    #[test]
    fn json_activity_tokens_distinguish_keys_and_value_kinds() {
        let content = r#"{"path":"README.md","count":2,"ok":true,"none":null}"#;
        let tokens = json_tokens(content).unwrap();
        let token = |text: &str| {
            tokens
                .iter()
                .find(|token| &content[token.range.clone()] == text)
                .map(|token| token.kind)
        };

        assert_eq!(token("\"path\""), Some(JsonTokenKind::Key));
        assert_eq!(token("\"README.md\""), Some(JsonTokenKind::String));
        assert_eq!(token("2"), Some(JsonTokenKind::Number));
        assert_eq!(token("true"), Some(JsonTokenKind::Literal));
        assert_eq!(token("{"), Some(JsonTokenKind::Punctuation));
    }

    #[test]
    fn command_output_only_trims_trailing_line_breaks() {
        assert_eq!(
            command_output_for_display("first\n\nsecond  \r\n\n"),
            "first\n\nsecond  "
        );
        assert_eq!(command_output_for_display("\n\r\n"), "");
    }

    #[test]
    fn rich_search_ranges_map_lowercase_expansion_to_utf8_source_bytes() {
        let text = "İSTANBUL · café · İ";
        let dotted_i = folded_match_byte_ranges(text, "i", 8);
        assert_eq!(dotted_i.len(), 2);
        assert_eq!(&text[dotted_i[0].clone()], "İ");
        assert_eq!(&text[dotted_i[1].clone()], "İ");
        assert!(dotted_i.iter().all(|range| {
            text.is_char_boundary(range.start) && text.is_char_boundary(range.end)
        }));

        let accented = folded_match_byte_ranges(text, "CAFÉ", 8);
        assert_eq!(accented.len(), 1);
        assert_eq!(&text[accented[0].clone()], "café");

        let fallback = folded_match_byte_ranges("aaab", "aab", 8);
        assert_eq!(fallback, vec![1..4]);
    }

    #[test]
    fn rich_search_range_budget_is_bounded_across_card_fragments() {
        let paint = RichSearchPaint::new("needle", true);
        let first = paint.ranges_for(&"needle ".repeat(100));
        let second = paint.ranges_for(&"needle ".repeat(100));

        assert_eq!(first.ranges.len(), 100);
        assert_eq!(first.active, Some(0));
        assert_eq!(second.ranges.len(), RICH_SEARCH_HIGHLIGHT_LIMIT - 100);
        assert_eq!(second.active, None);
        assert!(paint.ranges_for("needle").ranges.is_empty());
        assert_eq!(
            folded_match_byte_ranges(&"needle ".repeat(100), "needle", 7).len(),
            7
        );
    }

    #[test]
    fn search_background_composes_with_existing_syntax_color() {
        let syntax_color = gpui::red();
        let passive = gpui::yellow();
        let active = gpui::blue();
        let composed = compose_search_highlights(
            vec![(
                0..6,
                gpui::HighlightStyle {
                    color: Some(syntax_color),
                    ..Default::default()
                },
            )],
            &SearchTextRanges {
                ranges: vec![1..4, 4..5],
                active: Some(0),
            },
            passive,
            active,
        );

        let active_overlap = composed
            .iter()
            .find(|(range, _)| range == &(1..4))
            .map(|(_, style)| *style)
            .unwrap();
        assert_eq!(active_overlap.color, Some(syntax_color));
        assert_eq!(active_overlap.background_color, Some(active));
        let passive_overlap = composed
            .iter()
            .find(|(range, _)| range == &(4..5))
            .map(|(_, style)| *style)
            .unwrap();
        assert_eq!(passive_overlap.color, Some(syntax_color));
        assert_eq!(passive_overlap.background_color, Some(passive));
    }

    #[test]
    fn markdown_search_autoscroll_is_one_shot_per_navigation_generation() {
        let search = SearchTextRanges {
            ranges: vec![120..128, 240..248],
            active: Some(0),
        };
        assert_eq!(
            markdown_search_autoscroll(&search, Some(7), None),
            Some((7, 120))
        );
        assert_eq!(markdown_search_autoscroll(&search, Some(7), Some(7)), None);
        assert_eq!(
            markdown_search_autoscroll(&search, Some(8), Some(7)),
            Some((8, 120))
        );
        assert_eq!(
            markdown_search_autoscroll(
                &SearchTextRanges {
                    ranges: search.ranges,
                    active: None,
                },
                Some(9),
                Some(8),
            ),
            None
        );
    }

    #[test]
    fn rich_search_index_and_context_include_semantic_web_fields_and_status() {
        let item = TranscriptItem {
            key: "web-search".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Web,
            title: "Web search".into(),
            status: Some("waiting for results".into()),
            content: "Query\nunrelated".into(),
            raw: json!({
                "results": [{
                    "title": "Semantic Needle",
                    "url": "https://example.com/semantic-needle",
                    "snippet": "Only projected from raw protocol data"
                }]
            }),
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(item_matches_folded_query(&item, "semantic needle"));
        assert!(item_matches_folded_query(&item, "waiting for results"));
        let snippet = item_search_context_snippet(&item, "semantic needle", 72).unwrap();
        assert_eq!(&snippet.text[snippet.match_range], "Semantic Needle");
    }

    #[test]
    fn rich_search_index_excludes_default_hidden_speaker_titles() {
        let item = TranscriptItem {
            key: "agent-message".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Agent,
            title: "Codex".into(),
            status: None,
            content: "An unrelated answer".into(),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert!(!item_matches_folded_query(&item, "codex"));
        assert!(item_matches_folded_query(&item, "unrelated"));
    }

    #[test]
    fn collapsed_search_context_centers_and_exposes_the_actual_match() {
        let content = format!(
            "unrelated first line\n{} ReVeRsE_EnGiNeErInG.md trailing details",
            "prefix ".repeat(30)
        );
        let snippet = search_context_snippet(&content, "reverse_engineering.md", 72).unwrap();
        assert_eq!(&snippet.text[snippet.match_range], "ReVeRsE_EnGiNeErInG.md");
        assert!(snippet.text.starts_with("… "));
        assert!(snippet.text.chars().count() <= 76);
    }

    #[test]
    fn collapsed_search_context_maps_expanding_unicode_case_folds_safely() {
        let content = format!("{} İSTANBUL suffix", "界".repeat(60));
        let snippet = search_context_snippet(&content, "i\u{307}stanbul", 36).unwrap();
        assert_eq!(&snippet.text[snippet.match_range], "İSTANBUL");
        assert!(snippet.text.starts_with("… "));
    }

    #[test]
    fn streaming_search_reconciliation_only_changes_affected_sorted_indices() {
        let mut matches = vec![1, 4, 9];
        reconcile_sorted_search_match(&mut matches, 3, true);
        reconcile_sorted_search_match(&mut matches, 4, false);
        reconcile_sorted_search_match(&mut matches, 9, true);
        reconcile_sorted_search_match(&mut matches, 12, false);
        assert_eq!(matches, vec![1, 3, 9]);
    }

    #[test]
    fn rich_search_pauses_tail_follow_before_revealing_a_match() {
        let source = include_str!("main.rs");
        let jump = source
            .split_once("fn jump_to_search_match")
            .and_then(|(_, after)| after.split_once("fn move_search_match"))
            .map(|(jump, _)| jump)
            .expect("search jump must precede match movement");
        let pause = jump
            .find("pause_following_tail")
            .expect("search jumps must pause tail-follow");
        let reveal = jump
            .find("scroll_to_reveal_item")
            .expect("search jumps must reveal their item");

        assert!(pause < reveal);
    }

    #[test]
    fn file_change_metadata_is_not_numbered_or_misread_as_diff_content() {
        let presentations = file_change_presentations(
            "Added · /tmp/helium-browser-flags.conf\n# Native Wayland\n--ozone-platform=wayland",
        );
        assert_eq!(
            presentations,
            vec![FileChangePresentation {
                operation: "Added".into(),
                path: "/tmp/helium-browser-flags.conf".into(),
                content: "# Native Wayland\n--ozone-platform=wayland".into(),
            }]
        );
        assert_eq!(file_change_counts(&presentations[0]), (2, 0));

        assert_eq!(
            file_change_counts(&FileChangePresentation {
                operation: "Modified".into(),
                path: "/tmp/example".into(),
                content: "@@ -3,2 +3,3 @@\n context\n-removed\n+added\n+another".into(),
            }),
            (2, 1)
        );

        let mut in_hunk = false;
        assert_eq!(
            diff_line_tone("--ozone-platform=wayland", &mut in_hunk),
            DiffLineTone::Normal
        );
        assert_eq!(
            diff_line_tone("@@ -1 +1 @@", &mut in_hunk),
            DiffLineTone::Hunk
        );
        assert_eq!(
            diff_line_tone("-old value", &mut in_hunk),
            DiffLineTone::Deletion
        );
        assert_eq!(
            diff_line_tone("+new value", &mut in_hunk),
            DiffLineTone::Addition
        );

        let mut old_line = None;
        let mut new_line = None;
        assert_eq!(
            diff_line_numbers(
                "@@ -29,2 +29,4 @@",
                DiffLineTone::Hunk,
                true,
                true,
                &mut old_line,
                &mut new_line,
                1,
            ),
            (None, None)
        );
        assert_eq!((old_line, new_line), (Some(29), Some(29)));
        assert_eq!(
            diff_line_numbers(
                " context",
                DiffLineTone::Normal,
                true,
                true,
                &mut old_line,
                &mut new_line,
                2,
            ),
            (Some(29), Some(29))
        );
        assert_eq!(
            diff_line_numbers(
                "+added",
                DiffLineTone::Addition,
                true,
                true,
                &mut old_line,
                &mut new_line,
                3,
            ),
            (None, Some(30))
        );
        assert_eq!(
            diff_line_numbers(
                "-removed",
                DiffLineTone::Deletion,
                true,
                true,
                &mut old_line,
                &mut new_line,
                4,
            ),
            (Some(30), None)
        );
    }

    #[test]
    fn web_search_projection_uses_semantic_results_instead_of_raw_terminal_text() {
        let presentation = web_search_presentation(&json!({
            "query": "GPUI",
            "action": {"type": "search", "queries": ["GPUI", "Zed GPUI"]},
            "results": [
                {
                    "title": " GPUI   framework ",
                    "url": "https://example.test/gpui",
                    "snippet": "A   native UI framework",
                },
                "A plain result",
            ],
        }));
        assert_eq!(presentation.related_queries, 1);
        assert_eq!(presentation.results.len(), 2);
        assert_eq!(presentation.results[0].title, "GPUI framework");
        assert_eq!(
            presentation.results[0].snippet.as_deref(),
            Some("A native UI framework")
        );
        assert_eq!(presentation.results[1].title, "A plain result");
        assert!(!web_search_has_hidden_content(&presentation));

        let presentation = web_search_presentation(&json!({
            "results": [{
                "title": "Long result",
                "snippet": "x".repeat(121),
            }],
        }));
        assert!(web_search_has_hidden_content(&presentation));
        assert_eq!(
            presentation.results[0].snippet.as_ref().map(String::len),
            Some(121)
        );
    }

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
    fn legacy_request_controls_require_an_exact_live_request() {
        assert!(legacy_request_controls_active(true, false, true));
        assert!(
            !legacy_request_controls_active(false, false, true),
            "replay and persisted requests must stay inert"
        );
        assert!(
            !legacy_request_controls_active(true, true, true),
            "the shared surface is the sole live control owner"
        );
        assert!(
            !legacy_request_controls_active(true, false, false),
            "resolved requests cannot regain controls"
        );
    }

    #[test]
    fn loaded_request_status_matches_the_live_request_registry() {
        let mut model = TranscriptModel::replay(24);
        let pending_keys = model
            .items
            .iter()
            .filter(|item| {
                item.pending_request
                    .as_ref()
                    .is_some_and(|request| !request.resolved)
            })
            .map(|item| item.key.clone())
            .collect::<Vec<_>>();
        assert!(pending_keys.len() >= 2);
        let live = HashSet::from([pending_keys[0].clone()]);

        mark_unbacked_requests_inactive(&mut model, &live);

        for item in model.items.iter().filter(|item| {
            item.pending_request
                .as_ref()
                .is_some_and(|request| !request.resolved)
        }) {
            if item.key == pending_keys[0] {
                assert_ne!(item.status.as_deref(), Some("inactive"));
            } else {
                assert_eq!(item.status.as_deref(), Some("inactive"));
            }
        }
    }

    #[test]
    fn composer_height_is_compact_grows_with_content_and_stays_bounded() {
        let reading = TranscriptTypographyProfile::Reading;
        assert_eq!(composer_height("", 900., reading), 78.);
        assert_eq!(composer_height("one line", 900., reading), 78.);
        assert!(composer_height("first\nsecond\nthird", 900., reading) > 78.);
        assert!(
            composer_height(&"wrapped ".repeat(200), 320., reading)
                > composer_height(&"wrapped ".repeat(20), 900., reading),
            "narrow, wrapped prompts should receive more editing room"
        );
        assert_eq!(composer_height(&"line\n".repeat(100), 320., reading), 218.);
    }

    #[test]
    fn composer_height_accounts_for_reading_and_monospace_glyph_widths() {
        let narrow = "i".repeat(40);
        let wide = "W".repeat(40);
        let unicode = "界".repeat(40);

        assert!(
            estimated_composer_columns(&wide, TranscriptTypographyProfile::Reading)
                > estimated_composer_columns(&narrow, TranscriptTypographyProfile::Reading)
        );
        assert_eq!(
            estimated_composer_columns(&wide, TranscriptTypographyProfile::Buffer),
            estimated_composer_columns(&narrow, TranscriptTypographyProfile::Buffer)
        );
        assert!(
            composer_height(&wide, 320., TranscriptTypographyProfile::Reading)
                > composer_height(&narrow, 320., TranscriptTypographyProfile::Reading)
        );
        assert!(
            composer_height(&unicode, 320., TranscriptTypographyProfile::Reading)
                >= composer_height(&wide, 320., TranscriptTypographyProfile::Reading)
        );
    }

    #[test]
    fn one_typography_control_updates_both_persistent_editor_surfaces() {
        let source = include_str!("main.rs");
        let method = source
            .split_once("fn use_transcript_typography(")
            .and_then(|(_, after)| after.split_once("fn open_command_palette("))
            .map(|(method, _)| method)
            .expect("typography orchestration must remain independently auditable");

        assert!(method.contains("self.transcript_editor.update("));
        assert!(method.contains("self.composer.update("));
        assert_eq!(method.matches("set_typography_profile(profile").count(), 2);
        assert!(!method.contains("LocalEditor::modal_composer"));
        assert!(!method.contains("TranscriptEditor::read_only"));
    }

    #[test]
    fn composer_root_invalidation_only_tracks_empty_and_height_bucket_changes() {
        let profile = TranscriptTypographyProfile::Reading;
        let empty = composer_render_metrics("", 320., profile);
        let first_character = composer_render_metrics("a", 320., profile);
        let same_row_edit = composer_render_metrics("a longer same-row prompt", 320., profile);
        let wrapped = composer_render_metrics(&"wrapped ".repeat(200), 320., profile);
        let cleared = composer_render_metrics("   ", 320., profile);

        assert!(composer_edit_requires_root_invalidation(
            empty,
            first_character
        ));
        assert!(!composer_edit_requires_root_invalidation(
            first_character,
            same_row_edit
        ));
        assert!(composer_edit_requires_root_invalidation(
            same_row_edit,
            wrapped
        ));
        assert!(composer_edit_requires_root_invalidation(
            first_character,
            cleared
        ));
        assert!(!composer_edit_requires_root_invalidation(empty, cleared));
    }

    #[test]
    fn shell_command_highlighting_identifies_commands_strings_and_operators() {
        let command = "printf '%s' \"$USER\" && cargo test --offline";
        let captures = shell_capture_ranges(command);
        let captured = |name: &str, text: &str| {
            captures
                .iter()
                .any(|(range, capture)| capture == name && command.get(range.clone()) == Some(text))
        };

        assert!(captured("function", "printf"));
        assert!(captured("string", "'%s'"));
        assert!(captured("operator", "&&"));
        assert!(captured("function", "cargo"));
        assert!(shell_capture_priority("function") > shell_capture_priority("string"));
        assert!(shell_capture_priority("operator") > shell_capture_priority("string"));
        assert!(shell_capture_priority("constant") > shell_capture_priority("string"));
    }

    #[test]
    fn composer_send_is_blocked_while_loading_or_read_only() {
        assert!(composer_send_blocked(true, false, false, true));
        assert!(composer_send_blocked(false, true, false, true));
        assert!(composer_send_blocked(false, false, true, true));
        assert!(composer_send_blocked(false, false, false, false));
        assert!(!composer_send_blocked(false, false, false, true));
    }

    #[test]
    fn a_new_blocking_request_takes_an_idle_composer_but_never_steals_a_draft() {
        assert!(request_should_take_focus(
            true,
            true,
            true,
            true,
            FocusMode::Composer,
        ));
        assert!(!request_should_take_focus(
            true,
            true,
            true,
            false,
            FocusMode::Composer,
        ));
        assert!(!request_should_take_focus(
            true,
            true,
            true,
            true,
            FocusMode::Buffer,
        ));
        assert!(!request_should_take_focus(
            false,
            true,
            true,
            true,
            FocusMode::Composer,
        ));
        assert!(!request_should_take_focus(
            true,
            false,
            true,
            true,
            FocusMode::Composer,
        ));
    }

    #[test]
    fn request_headers_name_the_interaction_without_repeating_its_payload() {
        assert_eq!(
            request_header_title("item/commandExecution/requestApproval"),
            Some("Command approval")
        );
        assert_eq!(
            request_header_title("item/fileChange/requestApproval"),
            Some("File change approval")
        );
        assert_eq!(
            request_header_title("item/permissions/requestApproval"),
            Some("Permission request")
        );
        assert_eq!(
            request_header_title("item/tool/requestUserInput"),
            Some("Input requested")
        );
        assert_eq!(
            request_header_title("mcpServer/elicitation/request"),
            Some("MCP request")
        );
        assert_eq!(request_header_title("future/request"), None);
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

fn open_harness_window(
    cwd: String,
    replay_count: Option<usize>,
    start_in_text_view: bool,
    initial_thread_id: Option<String>,
    cx: &mut App,
) {
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
            cx.new(|cx| {
                HarnessApp::new(
                    cwd,
                    replay_count,
                    start_in_text_view,
                    initial_thread_id,
                    window,
                    cx,
                )
            })
        },
    ) {
        log::error!("failed to open Harness window: {error}");
    }
}

fn main() {
    let scroll_diagnostics = std::env::var_os("GPUI_SCROLL_DIAGNOSTICS")
        .is_some_and(|value| !value.is_empty() && value != std::ffi::OsStr::new("0"));
    let mut logger = env_logger::builder();
    logger
        .filter_level(log::LevelFilter::Warn)
        .parse_default_env();
    if scroll_diagnostics {
        logger.filter_module("gpui_scroll", log::LevelFilter::Info);
    }
    logger.init();
    let replay_count = replay_count();
    let start_in_text_view = std::env::args().any(|argument| argument == "--text");
    let initial_thread_id = std::env::var("HARNESS_OPEN_THREAD")
        .ok()
        .filter(|thread_id| !thread_id.trim().is_empty());
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

        open_harness_window(cwd, replay_count, start_in_text_view, initial_thread_id, cx);
        cx.activate(true);
    });
}
