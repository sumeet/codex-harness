use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LazyLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use assets::Assets;
use base64::Engine as _;
use codex_app_server_client::{
    Client, CodexSessionSource, CodexSubagentSource, CodexThread, CodexThreadStatus,
    Event as AppServerEvent, ThreadOpenResponse,
};
use file_icons::FileIcons;
use gpui::{
    AnimationExt, AnyElement, App, AppContext as _, Bounds, ClipboardEntry, Context, Entity,
    FocusHandle, Focusable, FollowMode, Image, ImageFormat, ImageSource, IntoElement, KeyBinding,
    KeyContext, Keystroke, ListAlignment, ListSizingBehavior, ListState, Modifiers, ObjectFit,
    PlatformInput, Render, ScrollDelta, ScrollHandle, ScrollStrategy, ScrollWheelEvent,
    SharedString, StyledImage, StyledText, Task, TouchPhase, UniformListScrollHandle, UpdateGlobal,
    WeakEntity, Window, WindowBounds, WindowOptions, actions, canvas, deferred, div, list, point,
    prelude::*, px, relative, size, uniform_list,
};
use gpui_platform::application;
use harness_editor::{
    LocalEditor, LocalEditorChanged, LocalEditorImageClicked, LocalEditorSteered,
    LocalEditorSubmitted, ModeIndicator, TranscriptEditor, TranscriptReplacement,
    TranscriptSelectionChanged, TranscriptSelectionSnapshot, TranscriptSupplement,
    TranscriptTypographyProfile, VimNextMatch, VimPreviousMatch, VimSearch, VimWordNext,
    VimWordPrevious, shell_capture_priority, shell_capture_ranges, syntax_highlights_for_path,
};
use harness_protocol as model;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use model::{TranscriptItem, TranscriptModel, minimal_text_edit};
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use theme_settings::ThemeSettings;
use ui::prelude::{ActiveTheme, StyledTypography};
use ui::{
    AgentThreadStatus, Button, ButtonCommon, ButtonSize, ButtonStyle, CircularProgress, Clickable,
    Color, CommonAnimationExt, ContextMenu, ContextMenuEntry, DiffStat, Disableable, Disclosure,
    DocumentationSide, Icon, IconButton, IconButtonShape, IconName, IconPosition, IconSize, Label,
    LabelCommon, LabelSize, ListItem, ListItemSpacing, PopoverMenu, PopoverMenuHandle, ScrollAxes,
    Scrollbars, SelectableButton, SpinnerLabel, ThreadItem, TintColor, Toggleable, Tooltip,
    WithScrollbar, right_click_menu,
};
use uuid::Uuid;

mod image_surface;
mod palette;
mod performance;
mod request_surface;
mod theme_sources;
mod visual_theme;

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
use visual_theme::{
    HarnessPreferences, HarnessVisualTheme, MAX_HARNESS_FONT_SIZE, MAX_HARNESS_FONT_WEIGHT,
    MIN_HARNESS_FONT_SIZE, MIN_HARNESS_FONT_WEIGHT, preferred_preferences, remember_preferences,
};
use zed_actions::command_palette::{OpenWithQuery, Toggle as ToggleCommandPalette};

actions!(
    harness,
    [
        Send,
        Steer,
        PasteComposer,
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
        OpenActionPalette,
        CommitSearch,
        CloseSearch,
        ClearSearchHighlights,
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
const CHILD_HIERARCHY_REFRESH_DEBOUNCE: Duration = Duration::from_millis(80);
const MAX_RECONNECT_ATTEMPTS: u8 = 3;
const STRUCTURED_OUTPUT_PREVIEW_LINES: usize = 10;
const STRUCTURED_OUTPUT_PREVIEW_BYTES: usize = 1_200;
const COMMAND_PREVIEW_LINES: usize = 4;
const COMMAND_PREVIEW_BYTES: usize = 800;
const MAX_COMPOSER_IMAGES: usize = 8;
const MAX_COMPOSER_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const QUEUED_PREVIEW_IMAGE_LIMIT: usize = 2;
#[cfg(test)]
const WEB_RESULT_PREVIEW_COUNT: usize = 3;
const PROGRESSIVE_OUTPUT_MEDIUM_LINES: usize = 100;
const PROGRESSIVE_OUTPUT_MEDIUM_BYTES: usize = 16 * 1_024;
const PROGRESSIVE_OUTPUT_LARGE_LINES: usize = 500;
const PROGRESSIVE_OUTPUT_LARGE_BYTES: usize = 64 * 1_024;
const RICH_SEARCH_HIGHLIGHT_LIMIT: usize = 128;
const RICH_NESTED_COMMAND_MAX_HEIGHT: f32 = 98.;
const RICH_NESTED_COMMAND_OUTPUT_MAX_HEIGHT: f32 = 112.;
const RICH_NESTED_OUTPUT_MAX_HEIGHT: f32 = 196.;
const RICH_MIN_CODE_ROW_HEIGHT: f32 = 20.;
const RICH_MIN_CARD_IDENTITY_ROW_HEIGHT: f32 = 20.;
const RICH_CARD_LEADING_WIDTH: f32 = 16.;
const PERFORMANCE_J_STEPS: u16 = 240;
const PERFORMANCE_SCROLL_STEPS: u16 = 360;
const PERFORMANCE_SCROLL_INTERVAL: Duration = Duration::from_nanos(8_333_333);
const PERFORMANCE_SCROLL_SETTLE_DURATION: Duration = Duration::from_millis(1_600);
const PERFORMANCE_STATUS_DURATION: Duration = Duration::from_secs(5);
const THEME_CATALOG_PAGE_SIZE: usize = 100;
// Keep the most recently visited tasks warm. Two entries made ordinary
// back-and-forth navigation fall through to a full app-server history request
// far too often, while eight still keeps the cache deliberately bounded.
const THREAD_SNAPSHOT_CACHE_LIMIT: usize = 8;

fn harness_code_font_size(cx: &App) -> gpui::Pixels {
    ThemeSettings::get_global(cx).agent_buffer_font_size(cx)
}

fn harness_code_row_height(cx: &App) -> gpui::Pixels {
    px((harness_code_font_size(cx).as_f32() * 1.35).max(RICH_MIN_CODE_ROW_HEIGHT))
}

fn harness_reading_row_height(cx: &App) -> gpui::Pixels {
    let size = ThemeSettings::get_global(cx).agent_ui_font_size(cx);
    px((size.as_f32() * 1.25).max(RICH_MIN_CARD_IDENTITY_ROW_HEIGHT))
}

/// Apply Harness's semantic code role as one indivisible family/size choice.
///
/// Zed's `font_buffer` and `text_ui_sm` helpers intentionally represent two
/// unrelated roles. Combining them made tool cards adopt the configured code
/// family and then silently overwrite its size with a fixed UI token.
trait HarnessStyledTypography: gpui::Styled + Sized {
    fn font_harness_reading(self, cx: &App) -> Self {
        let settings = ThemeSettings::get_global(cx);
        self.font_family(settings.agent_ui_font_family().clone())
            .font_weight(settings.ui_font.weight)
            .text_size(settings.agent_ui_font_size(cx))
    }

    fn font_harness_code(self, cx: &App) -> Self {
        let settings = ThemeSettings::get_global(cx);
        self.font_family(settings.agent_buffer_font_family().clone())
            .font_weight(settings.buffer_font.weight)
            .text_size(settings.agent_buffer_font_size(cx))
    }
}

impl<E: gpui::Styled> HarnessStyledTypography for E {}

fn rich_card_identity_row(cx: &App) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .h(harness_reading_row_height(cx))
        .flex()
        .items_center()
        .gap_1()
}

fn rich_card_identity_icon(icon: IconName, size: IconSize, color: Color) -> gpui::Div {
    div()
        .w(px(RICH_CARD_LEADING_WIDTH))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).size(size).color(color))
}

/// Use the same icon-theme lookup as Zed's agent edit cards. A semantic
/// fallback keeps replay fixtures and icon themes without a matching suffix
/// deterministic.
fn rich_file_identity_icon(path: &str, cx: &App) -> gpui::Div {
    let icon = FileIcons::get_icon(Path::new(path), cx)
        .map(|path| {
            Icon::from_path(path)
                .size(IconSize::Small)
                .color(Color::Muted)
        })
        .unwrap_or_else(|| {
            Icon::new(IconName::File)
                .size(IconSize::Small)
                .color(Color::Muted)
        });
    div()
        .w(px(RICH_CARD_LEADING_WIDTH))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(icon)
}

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

fn turn_tail_list_splice(
    current_count: usize,
    transcript_count: usize,
    turn_active: bool,
) -> Option<(Range<usize>, usize)> {
    let desired_count = transcript_count + usize::from(turn_active);
    if current_count == desired_count {
        return None;
    }
    if turn_active && current_count == transcript_count {
        return Some((transcript_count..transcript_count, 1));
    }
    if !turn_active && current_count == transcript_count + 1 {
        return Some((transcript_count..transcript_count + 1, 0));
    }

    // Thread replacement can race the visual turn transition by one render.
    // Reconcile an unexpected count in one operation rather than leaving a
    // stale generating row addressable as a transcript item.
    Some((0..current_count, desired_count))
}

/// Resolve the final selectable Rich/Editor position, deliberately ignoring
/// protocol-only and header-only transcript items. The document owns the
/// canonical projection, so tail navigation never guesses from the visible
/// list index or lands in the activity sentinel after the final real glyph.
fn transcript_tail_target(document: &model::TranscriptDocument) -> Option<(usize, usize)> {
    document.segments.last().map(|segment| {
        (
            segment.item_index,
            segment.body_range.end - segment.body_range.start,
        )
    })
}

fn transcript_has_inline_activity(transcript: &TranscriptModel) -> bool {
    transcript.items.iter().rev().any(|item| {
        item.kind == model::TranscriptKind::Agent
            && item.expanded
            && item.status.as_deref() == Some("streaming")
            && !item.content.trim().is_empty()
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
    active_ordinal: Option<usize>,
    seen_ranges: Rc<Cell<usize>>,
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
    linewise: bool,
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
    file_change: Option<RichFileChangeSurface>,
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
    command_scroll_handle: ScrollHandle,
    output_list_state: ListState,
    output_horizontal_handle: ScrollHandle,
    row_height: gpui::Pixels,
}

#[derive(Clone)]
enum RichFileChangeRow {
    Header {
        section_index: usize,
        logical_range: Range<usize>,
    },
    Line {
        section_index: usize,
        line_index: usize,
        text: Arc<str>,
        logical_range: Range<usize>,
        tone: DiffLineTone,
    },
}

impl RichFileChangeRow {
    fn logical_range(&self) -> Option<&Range<usize>> {
        match self {
            Self::Header { logical_range, .. } | Self::Line { logical_range, .. } => {
                Some(logical_range)
            }
        }
    }
}

#[derive(Clone)]
struct RichFileChangeData {
    presentations: Arc<[FileChangePresentation]>,
    rows: Arc<[RichFileChangeRow]>,
}

struct RichFileChangeSurface {
    event_count: usize,
    content_len: usize,
    data: RichFileChangeData,
    list_state: ListState,
    horizontal_handle: ScrollHandle,
    row_height: gpui::Pixels,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RichMarkdownNavigationPaint {
    selections: Vec<Range<usize>>,
    cursor: Option<usize>,
}

fn changed_markdown_cursor(
    previous: Option<&RichMarkdownNavigationPaint>,
    next: Option<&RichMarkdownNavigationPaint>,
) -> Option<usize> {
    let next_cursor = next.and_then(|navigation| navigation.cursor)?;
    (previous.and_then(|navigation| navigation.cursor) != Some(next_cursor)).then_some(next_cursor)
}

impl RichNavigationPaint {
    fn cursor_range(&self) -> Option<Range<usize>> {
        let raw_head = self.head?.min(self.body_text.len());
        let mut head = if self.visual && self.linewise {
            self.ranges
                .iter()
                .find(|range| range.start <= raw_head && raw_head <= range.end)
                .and_then(|range| {
                    if raw_head <= range.start {
                        Some(range.start)
                    } else if raw_head >= range.end
                        || self.body_text[raw_head..]
                            .chars()
                            .next()
                            .is_some_and(|character| matches!(character, '\r' | '\n'))
                    {
                        self.body_text[..raw_head.min(range.end).min(self.body_text.len())]
                            .char_indices()
                            .rev()
                            .find(|(_, character)| !matches!(character, '\r' | '\n'))
                            .map(|(start, _)| start)
                    } else {
                        Some(raw_head)
                    }
                })
                .unwrap_or(raw_head)
        } else {
            raw_head
        };
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

fn rich_navigation_slice(
    navigation: &RichNavigationPaint,
    source_range: Range<usize>,
    text: &str,
) -> RichNavigationPaint {
    let ranges = navigation
        .ranges
        .iter()
        .filter_map(|range| {
            let start = range.start.max(source_range.start);
            let end = range.end.min(source_range.end);
            (start < end).then(|| start - source_range.start..end - source_range.start)
        })
        .collect();
    let head = navigation.head.and_then(|head| {
        (source_range.start <= head && head <= source_range.end)
            .then(|| (head - source_range.start).min(text.len()))
    });
    RichNavigationPaint {
        body_text: Arc::from(text),
        ranges,
        head,
        visual: navigation.visual,
        linewise: navigation.linewise,
        cursor_claimed: navigation.cursor_claimed.clone(),
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
    fn new(query: impl Into<SharedString>, active_ordinal: Option<usize>) -> Self {
        Self {
            query: query.into(),
            active_ordinal,
            seen_ranges: Rc::new(Cell::new(0)),
            remaining_ranges: Rc::new(Cell::new(RICH_SEARCH_HIGHLIGHT_LIMIT)),
        }
    }

    fn ranges_for(&self, text: &str) -> SearchTextRanges {
        let ranges = search_match_byte_ranges(text, &self.query, self.remaining_ranges.get());
        self.decorate_ranges(ranges)
    }

    fn decorate_ranges(&self, mut ranges: Vec<Range<usize>>) -> SearchTextRanges {
        let remaining = self.remaining_ranges.get();
        ranges.truncate(remaining);
        self.remaining_ranges
            .set(remaining.saturating_sub(ranges.len()));
        let first_ordinal = self.seen_ranges.get();
        let active = self.active_ordinal.and_then(|active| {
            (first_ordinal <= active && active < first_ordinal + ranges.len())
                .then(|| active - first_ordinal)
        });
        self.seen_ranges.set(first_ordinal + ranges.len());
        SearchTextRanges { ranges, active }
    }
}

fn search_query_is_case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

fn search_match_byte_ranges(text: &str, query: &str, limit: usize) -> Vec<Range<usize>> {
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }
    if search_query_is_case_sensitive(query) {
        return text
            .match_indices(query)
            .take(limit)
            .map(|(start, matched)| start..start + matched.len())
            .collect();
    }
    folded_match_byte_ranges(text, query, limit)
}

fn folded_match_byte_ranges(text: &str, query: &str, limit: usize) -> Vec<Range<usize>> {
    let folded_query = query
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
        let colors = cx.theme().colors();
        compose_search_highlights(
            base,
            &search.ranges_for(&text),
            // Keep committed matches legible after the command line closes
            // (`hlsearch`) without introducing a Harness-only palette.
            colors
                .search_match_background
                .blend(colors.text_accent.opacity(0.12)),
            colors
                .search_active_match_background
                .blend(colors.text_accent.opacity(0.18)),
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
    HarnessVisualTheme::from_zed(cx.theme().colors())
        .selection_surface
        .alpha(0.42)
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

/// Apply Zed's active syntax theme to the code painted by the transcript diff.
/// The component fork removes unified-diff syntax before this point, matching
/// the source text Zed's native BufferDiff Editor sends to Tree-sitter.
fn diff_line_syntax_highlights(
    path: &str,
    line: &str,
    tone: DiffLineTone,
    _unified: bool,
    cx: &App,
) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
    if tone == DiffLineTone::Hunk {
        return Vec::new();
    }
    syntax_highlights_for_path(Path::new(path), line, cx)
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
                    this.place_rich_cursor_in_item(item_index, body_offset, window, cx);
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
    let query = query.trim();
    if query.is_empty() || max_chars == 0 {
        return None;
    }

    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Some(source_range) = search_match_byte_ranges(line, query, 1).pop() else {
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
        let presentation = web_search_presentation(&item.raw);
        let semantic_results = presentation
            .queries
            .into_iter()
            .chain(presentation.results.into_iter().flat_map(|result| {
                [Some(result.title), result.url, result.snippet]
                    .into_iter()
                    .flatten()
            }))
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

fn transcript_item_is_compact_activity(item: &TranscriptItem) -> bool {
    !item.expanded
        && !matches!(
            item.kind,
            model::TranscriptKind::User
                | model::TranscriptKind::Agent
                | model::TranscriptKind::Reasoning
                | model::TranscriptKind::Plan
        )
}

fn plan_progress(raw: &Value) -> Option<(usize, usize)> {
    let steps = raw.get("plan")?.as_array()?;
    (!steps.is_empty()).then(|| {
        let completed = steps
            .iter()
            .filter(|step| {
                matches!(
                    step.get("status").and_then(Value::as_str),
                    Some("completed" | "complete")
                )
            })
            .count();
        (completed, steps.len())
    })
}

fn expanded_item_uses_content_as_header(item: &TranscriptItem) -> bool {
    item.expanded
        && item.pending_request.is_none()
        && (matches!(
            item.kind,
            model::TranscriptKind::Diff | model::TranscriptKind::FileChange
        ) || command_uses_raw_identity(item))
}

fn transcript_item_header_title(item: &TranscriptItem) -> String {
    if item.kind == model::TranscriptKind::Reasoning {
        return "Thinking".into();
    }
    let title = item
        .pending_request
        .as_ref()
        .and_then(|request| request_header_title(&request.method))
        .unwrap_or(&item.title);
    if command_uses_raw_identity(item) {
        item.command_transcript()
            .and_then(|command| {
                command
                    .command
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .map(str::trim)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| "Shell command".to_owned())
    } else if item.kind == model::TranscriptKind::Web {
        web_search_presentation(&item.raw)
            .queries
            .into_iter()
            .next()
            .unwrap_or_else(|| title.replace(" · ", " — "))
    } else {
        title.replace(" · ", " — ")
    }
}

fn command_uses_raw_identity(item: &TranscriptItem) -> bool {
    item.kind == model::TranscriptKind::Command
}

fn toggle_model_item_expansion_at(
    model: &mut TranscriptModel,
    index: usize,
) -> Option<(String, bool)> {
    let item = model.items.get_mut(index)?;
    if (!item.kind.is_structured()
        && !matches!(
            item.kind,
            model::TranscriptKind::Reasoning | model::TranscriptKind::Plan
        ))
        || item.content.trim().is_empty()
    {
        return None;
    }

    item.expanded = !item.expanded;
    Some((item.key.clone(), !item.expanded))
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

fn item_matches_search_query(item: &TranscriptItem, query: &str) -> bool {
    (transcript_item_shows_header(item)
        && !expanded_item_uses_content_as_header(item)
        && search_contains(&transcript_item_header_title(item), query))
        || search_contains(transcript_item_searchable_body(item), query)
        || item
            .display_status()
            .is_some_and(|status| search_contains(status, query))
        || (item.kind == model::TranscriptKind::Web
            && web_search_presentation(&item.raw)
                .results
                .iter()
                .any(|result| {
                    search_contains(&result.title, query)
                        || result
                            .url
                            .as_deref()
                            .is_some_and(|url| search_contains(url, query))
                        || result
                            .snippet
                            .as_deref()
                            .is_some_and(|snippet| search_contains(snippet, query))
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
    // A lone `Result` label merely restates that the tool card has a body.
    // Preserve headings when they distinguish multiple semantic sections,
    // but let the common one-result case render its payload directly.
    if sections.len() == 1 && sections[0].heading.as_deref() == Some("Result") {
        sections[0].heading = None;
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

fn render_command_visual_status(
    status: model::CommandExecutionStatus,
    cx: &App,
) -> Option<(f32, AnyElement)> {
    match status {
        model::CommandExecutionStatus::Running => {
            return Some((
                44.,
                div()
                    .size(px(20.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::LoadCircle)
                            .size(IconSize::Small)
                            .color(Color::Accent)
                            .with_rotate_animation(2),
                    )
                    .into_any_element(),
            ));
        }
        // A successful command settles back into the ordinary card state.
        // Besides matching Zed's terminal tool treatment, rendering nothing
        // here lets the command reclaim the status gutter completely.
        model::CommandExecutionStatus::Succeeded => return None,
        model::CommandExecutionStatus::Failed(exit_code) => {
            let label = exit_code
                .map(|code| format!("exit {code}"))
                .unwrap_or_else(|| "failed".to_owned());
            let reserved_width = if exit_code.is_some() { 78. } else { 68. };
            Some((
                reserved_width,
                div()
                    .h(px(20.))
                    .px_1()
                    .flex_none()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .font_harness_code(cx)
                    .text_ui_xs(cx)
                    .text_color(cx.theme().status().error)
                    .bg(cx.theme().status().error_background.opacity(0.42))
                    .child(label)
                    .into_any_element(),
            ))
        }
    }
}

fn rich_command_row_logical_range(data: &RichCommandData, row: &RichCommandRow) -> Range<usize> {
    let base = match row.source {
        RichCommandSource::Command => 0,
        RichCommandSource::Output => data.command.len() + usize::from(!data.command.is_empty()),
    };
    base + row.source_range.start..base + row.source_range.end
}

fn rich_command_row_navigation_range(data: &RichCommandData, row: &RichCommandRow) -> Range<usize> {
    rich_command_row_logical_range(data, row)
}

fn progressive_line_limit(expansion: OutputExpansion, preview_limit: usize) -> usize {
    match expansion {
        OutputExpansion::Preview => preview_limit,
        OutputExpansion::Medium => PROGRESSIVE_OUTPUT_MEDIUM_LINES,
        OutputExpansion::Large => PROGRESSIVE_OUTPUT_LARGE_LINES,
        OutputExpansion::All => usize::MAX,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

/// Keep file headers informative without letting machine-specific absolute
/// prefixes dominate the card. This transformation happens in the shared
/// presentation model so paint, Vim navigation, search geometry, and copying
/// all see the same string.
fn compact_file_panel_path(path: &str) -> String {
    let path = path.trim();
    if !Path::new(path).is_absolute() {
        return path.to_owned();
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(relative) = Path::new(path).strip_prefix(&cwd)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().into_owned();
    }
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(relative) = Path::new(path).strip_prefix(home)
    {
        return format!("~/{}", relative.to_string_lossy());
    }
    path.to_owned()
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

/// Remove indentation shared by every non-empty code row in each unified-diff
/// hunk. This keeps relative indentation while avoiding large, uninformative
/// left margins in compact transcript cards. The presentation text itself is
/// transformed so rendering, Vim navigation, hit testing, search, and copying
/// all operate on the same bytes.
fn compact_diff_indentation(content: &str) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    let mut trims = vec![0; lines.len()];
    let mut change_rows = vec![false; lines.len()];
    let mut hunk_rows = Vec::<(usize, usize)>::new();
    let mut minimum_indent = None::<usize>;

    let flush_hunk =
        |rows: &mut Vec<(usize, usize)>, minimum: &mut Option<usize>, trims: &mut [usize]| {
            let Some(common_indent) = minimum.take() else {
                rows.clear();
                return;
            };
            if common_indent > 0 {
                for (line_index, available_indent) in rows.drain(..) {
                    trims[line_index] = common_indent.min(available_indent);
                }
            } else {
                rows.clear();
            }
        };

    let mut in_hunk = false;
    for (line_index, line) in lines.iter().copied().enumerate() {
        if line.starts_with("@@") {
            flush_hunk(&mut hunk_rows, &mut minimum_indent, &mut trims);
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with("\\ No newline") {
            continue;
        }
        let Some(marker) = line.as_bytes().first().copied() else {
            continue;
        };
        if !matches!(marker, b'+' | b'-' | b' ') {
            continue;
        }
        change_rows[line_index] = matches!(marker, b'+' | b'-');

        let body = &line[1..];
        let indent = body
            .as_bytes()
            .iter()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        hunk_rows.push((line_index, indent));
        if !body[indent..].is_empty() {
            minimum_indent = Some(minimum_indent.map_or(indent, |minimum| minimum.min(indent)));
        }
    }
    flush_hunk(&mut hunk_rows, &mut minimum_indent, &mut trims);

    lines
        .into_iter()
        .enumerate()
        .map(|(line_index, line)| {
            let trim = trims[line_index];
            if change_rows[line_index] {
                format!("{}{}", &line[..1], &line[1 + trim..])
            } else if trim == 0 {
                line.to_owned()
            } else {
                format!("{}{}", &line[..1], &line[1 + trim..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ZedDiffLine {
    text: Arc<str>,
    tone: DiffLineTone,
}

/// Project unified-diff transport text into the code rows painted by Zed's
/// agent BufferDiff editor. File headers, hunk coordinates, change markers,
/// and `No newline` notices are protocol furniture rather than editor glyphs.
/// Keeping this as a shared presentation transform makes Rich paint, Vim,
/// search, clicking, and copying agree on one visible coordinate space.
fn zed_diff_lines(content: &str, operation: &str) -> Vec<ZedDiffLine> {
    let unified = content.lines().any(|line| line.starts_with("@@"));
    let fallback_tone = match operation {
        "Added" => DiffLineTone::Addition,
        "Deleted" => DiffLineTone::Deletion,
        _ => DiffLineTone::Normal,
    };
    let mut in_hunk = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        let tone = diff_line_tone(line, &mut in_hunk);
        if tone == DiffLineTone::Hunk || line.starts_with("\\ No newline") {
            continue;
        }
        if unified {
            if !in_hunk {
                continue;
            }
            let Some(marker) = line.as_bytes().first().copied() else {
                lines.push(ZedDiffLine {
                    text: Arc::from(""),
                    tone: DiffLineTone::Normal,
                });
                continue;
            };
            if !matches!(marker, b'+' | b'-' | b' ') {
                continue;
            }
            lines.push(ZedDiffLine {
                text: Arc::from(&line[1..]),
                tone,
            });
        } else {
            lines.push(ZedDiffLine {
                text: Arc::from(line),
                tone: fallback_tone,
            });
        }
    }
    lines
}

fn zed_diff_visible_text(content: &str, operation: &str) -> String {
    zed_diff_lines(content, operation)
        .into_iter()
        .map(|line| line.text.to_string())
        .collect::<Vec<_>>()
        .join("\n")
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
    let content = lines.join("\n").trim_end().to_string();
    sections.push(DiffFilePresentation {
        path: path.or(inferred_path).unwrap_or_else(|| "Diff".into()),
        content: compact_diff_indentation(&content),
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
                presentation.content = compact_diff_indentation(&presentation.content);
                presentations.push(presentation);
            }
            current = Some(FileChangePresentation {
                operation: operation.into(),
                path: compact_file_panel_path(path),
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
        presentation.content = compact_diff_indentation(&presentation.content);
        presentations.push(presentation);
    }

    if presentations.is_empty() && !content.trim().is_empty() {
        let content = content.trim_end();
        presentations.push(FileChangePresentation {
            operation: "Changed".into(),
            path: "File details unavailable".into(),
            content: compact_diff_indentation(content),
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

fn rich_file_change_data(item: &TranscriptItem) -> RichFileChangeData {
    let presentations = if item.kind == model::TranscriptKind::Diff {
        diff_file_presentations(&item.content)
            .into_iter()
            .map(|presentation| FileChangePresentation {
                operation: "Modified".into(),
                path: presentation.path,
                content: presentation.content,
            })
            .collect()
    } else {
        file_change_presentations(&item.content)
    };
    let mut rows = Vec::new();
    let mut logical_cursor = 0;

    for (section_index, presentation) in presentations.iter().enumerate() {
        if section_index > 0 {
            logical_cursor += 1;
        }

        let path_range = logical_cursor..logical_cursor + presentation.path.len();
        logical_cursor = path_range.end;
        rows.push(RichFileChangeRow::Header {
            section_index,
            logical_range: path_range,
        });

        if presentation.content.is_empty() {
            continue;
        }

        logical_cursor += 1;
        for (line_index, line) in zed_diff_lines(&presentation.content, &presentation.operation)
            .into_iter()
            .enumerate()
        {
            let logical_range = logical_cursor..logical_cursor + line.text.len();
            let logical_end = logical_range.end;
            rows.push(RichFileChangeRow::Line {
                section_index,
                line_index,
                text: line.text,
                logical_range,
                tone: line.tone,
            });
            logical_cursor = logical_end + 1;
        }
        logical_cursor = logical_cursor.saturating_sub(1);
    }

    RichFileChangeData {
        presentations: presentations.into(),
        rows: rows.into(),
    }
}

fn rich_search_match_needs_context(item: &TranscriptItem, expansion: OutputExpansion) -> bool {
    let _ = expansion;
    !item.expanded
}

fn search_contains(text: &str, query: &str) -> bool {
    !search_match_byte_ranges(text, query, 1).is_empty()
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
        && search_contains(&transcript_item_header_title(item), query))
        || item
            .display_status()
            .is_some_and(|status| search_contains(status, query))
    {
        return true;
    }
    if !item.expanded {
        return item.kind == model::TranscriptKind::Reasoning
            && search_contains(&compact_reasoning_preview(&item.content), query);
    }

    match item.kind {
        model::TranscriptKind::Command => item.command_transcript().is_some_and(|transcript| {
            let command_text = transcript.command.trim_end_matches(['\r', '\n']);
            let output_text = command_output_for_display(&transcript.output);
            search_contains(command_text, query) || search_contains(output_text, query)
        }),
        model::TranscriptKind::FileChange => file_change_presentations(&item.content)
            .into_iter()
            .any(|presentation| {
                let (additions, deletions) = file_change_counts(&presentation);
                ((additions == 0 && deletions == 0)
                    && search_contains(&presentation.operation, query))
                    || search_contains(&presentation.path, query)
                    || search_contains(&presentation.content, query)
            }),
        model::TranscriptKind::Diff => {
            diff_file_presentations(&item.content)
                .into_iter()
                .any(|presentation| {
                    search_contains(&presentation.path, query)
                        || search_contains(&presentation.content, query)
                })
        }
        model::TranscriptKind::Web => {
            let presentation = web_search_presentation(&item.raw);
            presentation
                .queries
                .iter()
                .any(|candidate| search_contains(candidate, query))
                || presentation.results.iter().any(|result| {
                    search_contains(&result.title, query)
                        || result
                            .url
                            .as_deref()
                            .is_some_and(|url| search_contains(url, query))
                        || result
                            .snippet
                            .as_deref()
                            .is_some_and(|snippet| search_contains(snippet, query))
                })
        }
        model::TranscriptKind::Tool
        | model::TranscriptKind::Subagent
        | model::TranscriptKind::Review => search_contains(&item.content, query),
        kind if kind.is_structured() => search_contains(&item.content, query),
        _ => search_contains(transcript_item_searchable_body(item), query),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WebResultPresentation {
    title: String,
    url: Option<String>,
    domain: Option<String>,
    snippet: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct WebSearchPresentation {
    queries: Vec<String>,
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
    let mut queries = action
        .get("queries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(compact_web_text)
        .filter(|query| !query.is_empty())
        .collect::<Vec<_>>();
    if queries.is_empty()
        && let Some(query) = action
            .get("query")
            .or_else(|| raw.get("query"))
            .and_then(Value::as_str)
            .map(compact_web_text)
            .filter(|query| !query.is_empty())
    {
        queries.push(query);
    }
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
                    domain: None,
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
            let domain = result
                .get("domain")
                .and_then(Value::as_str)
                .map(compact_web_text)
                .or_else(|| url.and_then(web_result_domain));
            let snippet = result
                .get("snippet")
                .or_else(|| result.get("description"))
                .or_else(|| result.get("content"))
                .and_then(Value::as_str);
            (title.is_some() || url.is_some() || snippet.is_some()).then(|| WebResultPresentation {
                title: compact_web_text(title.unwrap_or("Result")),
                url: url.map(compact_web_text),
                domain,
                snippet: snippet.map(compact_web_text),
            })
        })
        .collect();
    WebSearchPresentation { queries, results }
}

fn web_result_domain(url: &str) -> Option<String> {
    let authority = url.split_once("://").map_or(url, |(_, rest)| rest);
    authority
        .split(['/', '?', '#'])
        .next()
        .filter(|domain| !domain.is_empty())
        .map(|domain| domain.trim_start_matches("www.").to_owned())
}

fn reasoning_summary_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.trim_matches('*')
                .trim_start_matches('#')
                .trim_start_matches("- ")
                .trim()
                .to_owned()
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
            for summary in reasoning_summary_lines(&item.content) {
                append_rich_navigation_fragment(&mut output, &summary);
            }
        }
        model::TranscriptKind::Diff => {
            for presentation in diff_file_presentations(&item.content) {
                append_rich_navigation_fragment(&mut output, &presentation.path);
                append_rich_navigation_fragment(
                    &mut output,
                    &zed_diff_visible_text(&presentation.content, "Modified"),
                );
            }
        }
        model::TranscriptKind::FileChange => {
            for presentation in file_change_presentations(&item.content) {
                append_rich_navigation_fragment(&mut output, &presentation.path);
                append_rich_navigation_fragment(
                    &mut output,
                    &zed_diff_visible_text(&presentation.content, &presentation.operation),
                );
            }
        }
        model::TranscriptKind::Command => {
            let Some(command) = item.command_transcript() else {
                return fallback.to_owned();
            };
            append_rich_navigation_fragment(&mut output, &command.command);
            append_rich_navigation_fragment(
                &mut output,
                command_output_for_display(&command.output),
            );
        }
        model::TranscriptKind::Web => {
            let presentation = web_search_presentation(&item.raw);
            if presentation.queries.is_empty() && presentation.results.is_empty() {
                return fallback.to_owned();
            }
            for query in presentation.queries {
                append_rich_navigation_fragment(&mut output, &query);
            }
            for result in presentation.results {
                append_rich_navigation_fragment(&mut output, &result.title);
                if let Some(domain) = result.domain {
                    append_rich_navigation_fragment(&mut output, &domain);
                }
            }
        }
        model::TranscriptKind::Tool
        | model::TranscriptKind::Subagent
        | model::TranscriptKind::Review => {
            let sections = activity_text_sections(&item.content);
            if !sections.iter().any(|section| section.heading.is_some()) {
                return sections
                    .into_iter()
                    .map(|section| section.body)
                    .collect::<Vec<_>>()
                    .join("\n");
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
    let body = if item.expanded {
        rich_navigation_body_for_item(item, projection.body_text())
    } else {
        transcript_item_header_title(item)
    };
    let projection = if body == projection.body_text() {
        projection
    } else {
        projection.with_body_text(body)
    };
    if projection.text.is_empty() {
        return None;
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VimCommandLine {
    Search { backwards: bool },
    Ex,
}

impl VimCommandLine {
    fn prompt(self) -> &'static str {
        match self {
            Self::Search { backwards: false } => "/",
            Self::Search { backwards: true } => "?",
            Self::Ex => ":",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum RequestReply {
    Result(Value),
    Error { code: i64, message: String },
}

#[derive(Clone, Debug, PartialEq)]
enum RequestRoute {
    Interactive,
    ReturnToThread(String),
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

#[derive(Clone)]
struct ComposerImageAttachment {
    id: u64,
    image: Arc<Image>,
}

struct ComposerSubmission {
    key: Option<String>,
    client_user_message_id: String,
    input: Value,
}

#[derive(Clone)]
struct QueuedTurnSubmission {
    id: Option<String>,
    client_user_message_id: String,
    input: Value,
    preview_images: Vec<Arc<Image>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueOperation {
    Removing,
    Editing,
    AddingToResponse,
    Interrupting,
}

impl QueueOperation {
    fn progress_label(self) -> &'static str {
        match self {
            Self::Removing => "Removing…",
            Self::Editing => "Opening draft…",
            Self::AddingToResponse => "Adding to response…",
            Self::Interrupting => "Interrupting…",
        }
    }
}

#[derive(Clone)]
struct UserImagePreview {
    semantic_source: model::UserImageSource,
    source: ImageSource,
    dimensions: Option<(u32, u32)>,
}

fn composer_is_empty(text: &str, image_count: usize) -> bool {
    text.trim().is_empty() && image_count == 0
}

fn composer_image_token(id: u64) -> String {
    format!("[Image #{id}]")
}

fn composer_prompt_preview(input: &[Value]) -> String {
    input
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text" | "inputText") => block.get("text").and_then(Value::as_str),
            _ => None,
        })
        .collect::<String>()
}

fn show_submission_optimistically_in_transcript(turn_active: bool) -> bool {
    !turn_active
}

fn queued_submission_from_value(value: &Value) -> Option<QueuedTurnSubmission> {
    let input = value.get("input").cloned().unwrap_or_else(|| json!([]));
    Some(QueuedTurnSubmission {
        id: Some(value.get("id")?.as_str()?.to_owned()),
        client_user_message_id: value
            .get("clientUserMessageId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        preview_images: queued_submission_preview_images(&input),
        input,
    })
}

fn queued_submissions_from_response(response: &Value) -> VecDeque<QueuedTurnSubmission> {
    response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(queued_submission_from_value)
        .collect()
}

fn queued_submission_text(input: &Value) -> String {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text" | "inputText") => block.get("text").and_then(Value::as_str),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn queued_submission_preview(input: &Value) -> String {
    queued_submission_text(input)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn queued_submission_image_count(input: &Value) -> usize {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| {
            matches!(
                block.get("type").and_then(Value::as_str),
                Some("image" | "inputImage")
            )
        })
        .count()
}

fn queued_submission_image(block: &Value) -> Option<Arc<Image>> {
    if !matches!(
        block.get("type").and_then(Value::as_str),
        Some("image" | "inputImage")
    ) {
        return None;
    }
    let url = block
        .get("url")
        .or_else(|| block.get("imageUrl"))
        .and_then(Value::as_str)?;
    let data_url = url.strip_prefix("data:")?;
    let (mime_info, encoded) = data_url.split_once(',')?;
    let (mime_type, encoding) = mime_info.split_once(';')?;
    if encoding != "base64" {
        return None;
    }
    let max_encoded_len = MAX_COMPOSER_IMAGE_BYTES.div_ceil(3) * 4;
    if encoded.len() > max_encoded_len {
        return None;
    }
    let format = ImageFormat::from_mime_type(mime_type)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() > MAX_COMPOSER_IMAGE_BYTES {
        return None;
    }
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

fn queued_submission_preview_images(input: &Value) -> Vec<Arc<Image>> {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(queued_submission_image)
        .take(QUEUED_PREVIEW_IMAGE_LIMIT)
        .collect()
}

fn composer_app_server_input(text: &str, images: &[ComposerImageAttachment]) -> Vec<Value> {
    let image_value = |attachment: &ComposerImageAttachment| {
        let mime_type = attachment.image.format().mime_type();
        let encoded = base64::engine::general_purpose::STANDARD.encode(attachment.image.bytes());
        json!({
            "type": "image",
            "url": format!("data:{mime_type};base64,{encoded}")
        })
    };
    let mut input = Vec::with_capacity(images.len().saturating_mul(2).saturating_add(1));
    let mut cursor = 0;
    let mut used = HashSet::new();
    while cursor < text.len() {
        let next = images
            .iter()
            .filter_map(|attachment| {
                let token = composer_image_token(attachment.id);
                text[cursor..]
                    .find(&token)
                    .map(|relative| (cursor + relative, token, attachment))
            })
            .min_by_key(|(offset, _, _)| *offset);
        let Some((offset, token, attachment)) = next else {
            break;
        };
        if offset > cursor {
            input.push(json!({"type": "text", "text": &text[cursor..offset]}));
        }
        input.push(image_value(attachment));
        used.insert(attachment.id);
        cursor = offset + token.len();
    }
    if cursor < text.len() {
        input.push(json!({"type": "text", "text": &text[cursor..]}));
    }
    // Preserve old drafts and queued submissions that predate inline markers.
    input.extend(
        images
            .iter()
            .filter(|attachment| !used.contains(&attachment.id))
            .map(image_value),
    );
    input
}

fn image_dimensions(bytes: &[u8], format: ImageFormat) -> Option<(u32, u32)> {
    let format = match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Webp => image::ImageFormat::WebP,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Svg => return None,
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        ImageFormat::Ico => image::ImageFormat::Ico,
        ImageFormat::Pnm => image::ImageFormat::Pnm,
    };
    image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .ok()
}

fn transcript_user_image_source(source: &model::UserImageSource) -> Option<UserImagePreview> {
    match source {
        model::UserImageSource::LocalPath(path) => Some(UserImagePreview {
            semantic_source: source.clone(),
            source: PathBuf::from(path).into(),
            dimensions: image::image_dimensions(path).ok(),
        }),
        model::UserImageSource::Url(url) => {
            if !url.starts_with("data:") {
                return Some(UserImagePreview {
                    semantic_source: source.clone(),
                    source: url.clone().into(),
                    dimensions: None,
                });
            }
            let data_url = url.strip_prefix("data:")?;
            let (mime_info, encoded) = data_url.split_once(',')?;
            let (mime_type, encoding) = mime_info.split_once(';')?;
            if encoding != "base64" {
                return None;
            }
            let format = ImageFormat::from_mime_type(mime_type)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?;
            let dimensions = image_dimensions(&bytes, format);
            Some(UserImagePreview {
                semantic_source: source.clone(),
                source: Arc::new(Image::from_bytes(format, bytes)).into(),
                dimensions,
            })
        }
    }
}

fn user_image_preview_size(dimensions: Option<(u32, u32)>) -> (f32, f32) {
    const MAX_WIDTH: f32 = 384.;
    const MAX_HEIGHT: f32 = 220.;
    const FALLBACK_WIDTH: f32 = 320.;
    const FALLBACK_HEIGHT: f32 = 180.;

    let Some((width, height)) = dimensions.filter(|(width, height)| *width > 0 && *height > 0)
    else {
        return (FALLBACK_WIDTH, FALLBACK_HEIGHT);
    };
    let scale = (MAX_WIDTH / width as f32)
        .min(MAX_HEIGHT / height as f32)
        .min(1.);
    (width as f32 * scale, height as f32 * scale)
}

fn legacy_request_controls_active(
    request_is_live: bool,
    uses_shared_surface: bool,
    request_is_pending: bool,
) -> bool {
    request_is_live && !uses_shared_surface && request_is_pending
}

fn composer_send_blocked(
    composer_empty: bool,
    loading_thread: bool,
    attaching_thread: bool,
    settings_update_pending: bool,
    read_only: bool,
    transport_available: bool,
) -> bool {
    composer_empty
        || loading_thread
        || attaching_thread
        || settings_update_pending
        || read_only
        || !transport_available
}

fn child_inspection_blocked(read_only_child: bool, has_unresolved_live_request: bool) -> bool {
    read_only_child && has_unresolved_live_request
}

fn callback_origin_is_visible(origin_thread_id: &str, selected_thread_id: Option<&str>) -> bool {
    selected_thread_id == Some(origin_thread_id)
}

fn queue_state_belongs_to_thread(
    origin_thread_id: &str,
    selected_thread_id: Option<&str>,
    preserved_work_thread_id: Option<&str>,
) -> bool {
    preserved_work_thread_id == Some(origin_thread_id)
        || (preserved_work_thread_id.is_none() && selected_thread_id == Some(origin_thread_id))
}

fn queue_state_is_visible(
    selected_thread_id: Option<&str>,
    preserved_work_thread_id: Option<&str>,
) -> bool {
    selected_thread_id.is_some()
        && preserved_work_thread_id.is_none_or(|parent| selected_thread_id == Some(parent))
}

fn reject_pending_requests_on_switch(
    preserve_background_work: bool,
    leaving_child_with_live_request: bool,
) -> bool {
    !preserve_background_work || leaving_child_with_live_request
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

fn slow_list_item_threshold() -> Duration {
    static THRESHOLD: LazyLock<Duration> = LazyLock::new(|| {
        std::env::var("GPUI_SLOW_LIST_THRESHOLD_MS")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.)
            .map(|milliseconds| Duration::from_secs_f64(milliseconds / 1_000.))
            .unwrap_or(Duration::from_millis(4))
    });
    *THRESHOLD
}

fn vim_search_available(buffer_view: bool, rich_vim_enabled: bool) -> bool {
    buffer_view || rich_vim_enabled
}

fn search_uses_native_editor(
    buffer_view: bool,
    focus_mode: FocusMode,
    rich_vim_enabled: bool,
) -> bool {
    (buffer_view || rich_vim_enabled)
        && matches!(focus_mode, FocusMode::Buffer | FocusMode::Transcript)
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
        let visuals = HarnessVisualTheme::from_zed(&colors);
        let command_fallback_title = command_uses_raw_identity(&self.item);
        let command_status = self
            .item
            .command_execution_status()
            .and_then(|status| render_command_visual_status(status, cx));
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
        let header = rich_card_identity_row(cx)
            .id(format!("hybrid-structured-header:{}", self.item.key))
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
            .child(rich_card_identity_icon(
                icon_for_item(&self.item),
                IconSize::Small,
                Color::Muted,
            ))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_ui_sm(cx)
                    .when(command_fallback_title, |this| this.font_harness_code(cx))
                    .when(!command_fallback_title, |this| {
                        this.font_harness_reading(cx)
                    })
                    .text_color(colors.text_muted)
                    .child(transcript_item_header_title(&self.item)),
            )
            .when_some(command_status, |this, (_, status)| this.child(status))
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
                .border_color(visuals.divider)
                .bg(visuals.raised_surface)
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
                    .map(|presentation| zed_diff_lines(&presentation.content, "Modified").len())
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
    child_threads: ChildThreadRegistry,
    sidebar_threads: Vec<SidebarThreadRow>,
    available_models: Vec<ModelChoice>,
    permission_profiles: Vec<PermissionProfileChoice>,
    model_menu_handle: PopoverMenuHandle<ContextMenu>,
    permission_menu_handle: PopoverMenuHandle<ContextMenu>,
    settings_update_pending: bool,
    appearance_settings_open: bool,
    appearance_settings_section: AppearanceSettingsSection,
    appearance_font_role: AppearanceFontRole,
    appearance_scroll_handle: ScrollHandle,
    theme_catalog_filter: Entity<ui_input::InputField>,
    theme_catalog: Vec<theme_sources::ThemeCatalogEntry>,
    theme_catalog_loading: bool,
    theme_catalog_error: Option<SharedString>,
    theme_catalog_visible: usize,
    theme_packs_installing: HashSet<String>,
    installed_theme_packs: HashSet<String>,
    thread_snapshots: ThreadSnapshotCache,
    selected_thread_id: Option<String>,
    loaded_thread_updated_at: Option<i64>,
    connecting: bool,
    loading_thread: bool,
    attaching_thread: bool,
    thread_read_only_reason: Option<SharedString>,
    error: Option<SharedString>,
    transient_turn_status: Option<SharedString>,
    model: TranscriptModel,
    composer: Entity<LocalEditor>,
    composer_images: Vec<ComposerImageAttachment>,
    next_composer_image_id: u64,
    composer_attachment_error: Option<SharedString>,
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
    vim_command_line: Option<VimCommandLine>,
    command_line_error: Option<SharedString>,
    search_highlights_visible: bool,
    search_query: String,
    search_matches: Vec<usize>,
    active_search_match: usize,
    search_match_count: usize,
    active_search_item: Option<usize>,
    active_search_body_offset: Option<usize>,
    search_navigation_generation: u64,
    search_returns_to_buffer: bool,
    search_return_focus: FocusMode,
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
    user_image_previews: HashMap<String, Vec<UserImagePreview>>,
    expanded_user_image: Option<ImageSource>,
    hybrid_surfaces: HashMap<String, Entity<HybridStructuredSurface>>,
    rich_nested_scrolls: HashMap<String, RichNestedScrollState>,
    list_state: ListState,
    task_list_state: UniformListScrollHandle,
    sidebar_open: bool,
    sidebar_user_override: bool,
    turn_start_pending: bool,
    queue_start_pending: bool,
    queue_refresh_pending: bool,
    turn_start_generation: u64,
    queue_start_generation: u64,
    queue_refresh_generation: u64,
    queue_operations: HashMap<String, QueueOperation>,
    queued_turns: VecDeque<QueuedTurnSubmission>,
    server_task: Task<()>,
    turn_task: Task<()>,
    thread_list_task: Task<()>,
    thread_open_task: Task<()>,
    child_hierarchy_task: Task<()>,
    child_hierarchy_generation: u64,
    deferred_server_requests: Vec<AppServerEvent>,
    background_parent_thread_id: Option<String>,
    preserved_work_thread_id: Option<String>,
    reconnect_task: Task<()>,
    read_only_refresh_task: Task<()>,
    reconnect_attempts: u8,
}

#[derive(Default)]
struct ThreadSnapshotCache {
    entries: VecDeque<CodexThread>,
}

#[derive(Default)]
struct ChildThreadRegistry {
    by_id: HashMap<String, CodexThread>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SidebarThreadRow {
    thread_id: String,
    depth: usize,
    root_index: Option<usize>,
}

#[derive(Debug, Eq, PartialEq)]
struct SubagentActivityPresentation {
    title: String,
    content: String,
}

#[derive(Clone, Debug)]
struct ModelChoice {
    id: String,
    model: String,
    display_name: SharedString,
    default_effort: String,
    efforts: Vec<String>,
    is_default: bool,
}

#[derive(Clone, Debug)]
struct PermissionProfileChoice {
    id: String,
    description: Option<SharedString>,
    allowed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComposerActionState {
    Send,
    Queue,
    Stop,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AppearanceSettingsSection {
    #[default]
    Themes,
    Explore,
    Typography,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AppearanceFontRole {
    #[default]
    Reading,
    Code,
}

fn composer_action_state(turn_active: bool, composer_empty: bool) -> ComposerActionState {
    match (turn_active, composer_empty) {
        (true, true) => ComposerActionState::Stop,
        (true, false) => ComposerActionState::Queue,
        (false, _) => ComposerActionState::Send,
    }
}

fn model_choices_from_response(response: &Value) -> Vec<ModelChoice> {
    response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            !entry
                .get("hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            let model = entry.get("model")?.as_str()?.to_owned();
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&model)
                .to_owned();
            let display_name = entry
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or(&model)
                .to_owned()
                .into();
            let efforts = entry
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|effort| {
                    effort
                        .get("reasoningEffort")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect::<Vec<_>>();
            let default_effort = entry
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| efforts.first().cloned())
                .unwrap_or_default();
            Some(ModelChoice {
                id,
                model,
                display_name,
                default_effort,
                efforts,
                is_default: entry
                    .get("isDefault")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn effective_model_choice<'a>(
    choices: &'a [ModelChoice],
    selected_model: Option<&str>,
) -> Option<&'a ModelChoice> {
    match selected_model {
        Some(selected) => choices
            .iter()
            .find(|choice| choice.model == selected || choice.id == selected),
        None => choices
            .iter()
            .find(|choice| choice.is_default)
            .or_else(|| choices.first()),
    }
}

fn effective_reasoning_effort(
    selected_effort: Option<&str>,
    choice: Option<&ModelChoice>,
) -> Option<String> {
    selected_effort
        .map(ToOwned::to_owned)
        .or_else(|| choice.map(|choice| choice.default_effort.clone()))
        .filter(|effort| !effort.is_empty())
}

fn reasoning_effort_label(effort: &str) -> SharedString {
    match effort {
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "X high".into(),
        "ultra" => "Ultra".into(),
        other => other.to_owned().into(),
    }
}

fn apply_harness_preferences(preferences: &HarnessPreferences, cx: &mut App) -> bool {
    if let Err(error) = theme::ThemeRegistry::global(cx).get(&preferences.theme) {
        log::error!("cannot select Harness theme: {error}");
        return false;
    }

    let settings = preferences.settings_json();
    let mut applied = true;
    SettingsStore::update_global(cx, |store, cx| {
        if let Err(error) = store.set_user_settings(&settings, cx).result() {
            applied = false;
            log::error!("could not apply Harness appearance settings: {error}");
        }
    });
    if !applied {
        return false;
    }
    theme_settings::reload_theme(cx);
    if let Err(error) = remember_preferences(preferences) {
        log::warn!("could not remember Harness appearance settings: {error}");
    }
    true
}

fn permission_profile_choices_from_response(response: &Value) -> Vec<PermissionProfileChoice> {
    response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(PermissionProfileChoice {
                id: entry.get("id")?.as_str()?.to_owned(),
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|description| description.to_owned().into()),
                allowed: entry
                    .get("allowed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn permission_profile_label(id: &str) -> SharedString {
    match id.trim_start_matches(':') {
        "danger-full-access" | "full-access" => "Full access".into(),
        "workspace" | "workspace-write" => "Workspace".into(),
        "read-only" => "Read only".into(),
        other => other.replace(['-', '_'], " ").into(),
    }
}

fn context_window_usage(token_usage: Option<&Value>) -> Option<(i64, i64)> {
    let token_usage = token_usage?.get("tokenUsage").unwrap_or(token_usage?);
    let used = token_usage
        .pointer("/last/totalTokens")
        .and_then(Value::as_i64)?;
    let capacity = token_usage
        .get("modelContextWindow")
        .and_then(Value::as_i64)?;
    (capacity > 0).then_some((used.max(0), capacity))
}

fn compact_token_count(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1_000.)
    } else {
        tokens.to_string()
    }
}

impl ChildThreadRegistry {
    fn reconcile(&mut self, threads: Vec<CodexThread>) -> bool {
        let next = threads
            .into_iter()
            .filter(|thread| !thread.id.is_empty() && thread.effective_parent_thread_id().is_some())
            .map(|thread| (thread.id.clone(), thread))
            .collect();
        if self.by_id == next {
            return false;
        }
        self.by_id = next;
        true
    }

    fn get(&self, thread_id: &str) -> Option<&CodexThread> {
        self.by_id.get(thread_id)
    }
}

fn sidebar_thread_rows(
    roots: &[CodexThread],
    children: &ChildThreadRegistry,
) -> Vec<SidebarThreadRow> {
    fn append_children(
        parent_id: &str,
        depth: usize,
        children_by_parent: &HashMap<String, Vec<&CodexThread>>,
        visited: &mut HashSet<String>,
        rows: &mut Vec<SidebarThreadRow>,
    ) {
        let Some(children) = children_by_parent.get(parent_id) else {
            return;
        };
        for child in children {
            if !visited.insert(child.id.clone()) {
                continue;
            }
            rows.push(SidebarThreadRow {
                thread_id: child.id.clone(),
                depth,
                root_index: None,
            });
            append_children(&child.id, depth + 1, children_by_parent, visited, rows);
        }
    }

    let mut children_by_parent: HashMap<String, Vec<&CodexThread>> = HashMap::new();
    for child in children.by_id.values() {
        if let Some(parent_id) = child.effective_parent_thread_id() {
            children_by_parent
                .entry(parent_id.to_owned())
                .or_default()
                .push(child);
        }
    }
    for siblings in children_by_parent.values_mut() {
        siblings.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.id.cmp(&right.id))
        });
    }

    let mut rows = Vec::with_capacity(roots.len() + children.by_id.len());
    let mut visited = HashSet::new();
    for (root_index, root) in roots.iter().enumerate() {
        visited.insert(root.id.clone());
        rows.push(SidebarThreadRow {
            thread_id: root.id.clone(),
            depth: 0,
            root_index: Some(root_index),
        });
        append_children(&root.id, 1, &children_by_parent, &mut visited, &mut rows);
    }
    rows
}

fn sidebar_selection_index(
    rows: &[SidebarThreadRow],
    selected_thread_id: Option<&str>,
    fallback: usize,
) -> usize {
    selected_thread_id
        .and_then(|selected_id| {
            rows.iter()
                .position(|row| row.thread_id.as_str() == selected_id)
        })
        .unwrap_or_else(|| fallback.min(rows.len().saturating_sub(1)))
}

fn child_agent_path(thread: &CodexThread) -> Option<&str> {
    let CodexSessionSource::SubAgent(CodexSubagentSource::ThreadSpawn(spawn)) = &thread.source
    else {
        return None;
    };
    spawn
        .agent_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn subagent_thread_ids(raw: &Value) -> Vec<String> {
    fn push_unique(ids: &mut Vec<String>, thread_id: &str) {
        if !thread_id.is_empty() && !ids.iter().any(|existing| existing == thread_id) {
            ids.push(thread_id.to_owned());
        }
    }

    let mut ids = Vec::new();
    for value in [raw, raw.get("result").unwrap_or(&Value::Null)] {
        if let Some(states) = value.get("agentsStates").and_then(Value::as_object) {
            for thread_id in states.keys() {
                push_unique(&mut ids, thread_id);
            }
        }
        for field in ["receiverThreadIds", "threadIds"] {
            if let Some(thread_ids) = value.get(field).and_then(Value::as_array) {
                for thread_id in thread_ids.iter().filter_map(Value::as_str) {
                    push_unique(&mut ids, thread_id);
                }
            }
        }
        for field in ["agentThreadId", "childThreadId"] {
            if let Some(thread_id) = value.get(field).and_then(Value::as_str) {
                push_unique(&mut ids, thread_id);
            }
        }
    }
    ids
}

fn subagent_identity(thread: &CodexThread) -> String {
    [
        thread.agent_nickname.as_deref(),
        thread.agent_role.as_deref(),
        child_agent_path(thread),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|identity| !identity.is_empty())
    .unwrap_or(&thread.id)
    .to_owned()
}

fn subagent_activity_presentation(
    item: &TranscriptItem,
    children: &ChildThreadRegistry,
) -> Option<SubagentActivityPresentation> {
    if item.kind != model::TranscriptKind::Subagent {
        return None;
    }

    let thread_ids = subagent_thread_ids(&item.raw);
    if thread_ids.is_empty() {
        let action = item
            .raw
            .get("tool")
            .or_else(|| item.raw.get("kind"))
            .and_then(Value::as_str);
        let is_idless_wait = action.is_some_and(|action| action.eq_ignore_ascii_case("wait"))
            && item.content.trim() == "Subagent state unavailable";
        return is_idless_wait.then(|| SubagentActivityPresentation {
            title: "Subagent coordination · Wait".into(),
            content: String::new(),
        });
    }

    let identities = thread_ids
        .iter()
        .filter_map(|thread_id| {
            children
                .get(thread_id)
                .map(|thread| (thread_id, subagent_identity(thread)))
        })
        .collect::<Vec<_>>();
    if identities.is_empty() {
        return None;
    }

    // Keep the event-local action, status, message, prompt, kind, path, and
    // body exactly as the protocol rendered them. The registry contributes a
    // compact title identity only; its current status must not rewrite history
    // or change the selectable body text.
    let mut title = item.title.clone();
    let mut identity_names = identities
        .iter()
        .map(|(_, identity)| identity.as_str())
        .collect::<Vec<_>>();
    identity_names.dedup();
    let identity = if identity_names.len() > 3 {
        format!(
            "{} +{}",
            identity_names[..3].join(", "),
            identity_names.len() - 3
        )
    } else {
        identity_names.join(", ")
    };
    if !identity.is_empty() && !title.contains(&identity) {
        title.push_str(" · ");
        title.push_str(&identity);
    }
    (title != item.title).then_some(SubagentActivityPresentation {
        title,
        content: item.content.clone(),
    })
}

fn event_refreshes_child_hierarchy(event: &AppServerEvent) -> bool {
    let AppServerEvent::Notification { method, params } = event else {
        return false;
    };
    let item = params.get("item").unwrap_or(params);
    let kind = item
        .get("type")
        .or_else(|| item.get("kind"))
        .and_then(Value::as_str);
    let method = method.to_ascii_lowercase();
    matches!(kind, Some("collabAgentToolCall" | "subAgentActivity"))
        || method.contains("subagent")
        || method.contains("collab")
}

impl ThreadSnapshotCache {
    fn take(&mut self, thread_id: &str) -> Option<CodexThread> {
        let index = self
            .entries
            .iter()
            .position(|thread| thread.id == thread_id)?;
        self.entries.remove(index)
    }

    fn insert(&mut self, thread: CodexThread) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|cached| cached.id == thread.id)
        {
            self.entries.remove(index);
        }
        self.entries.push_front(thread);
        self.entries.truncate(THREAD_SNAPSHOT_CACHE_LIMIT);
    }
}

impl HarnessApp {
    fn refresh_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        theme_settings::setup_ui_font(window, cx);
        self.transcript_editor
            .update(cx, |editor, cx| editor.refresh_typography(window, cx));
        self.composer
            .update(cx, |editor, cx| editor.refresh_typography(window, cx));
        cx.notify();
    }

    fn update_appearance(
        owner: &WeakEntity<Self>,
        window: &mut Window,
        cx: &mut App,
        update: impl FnOnce(&mut HarnessPreferences),
    ) {
        let mut preferences = preferred_preferences();
        // Preserve the theme currently visible in this window even when it was
        // selected through a replay-only environment override.
        preferences.theme = cx.theme().name.to_string();
        update(&mut preferences);
        if apply_harness_preferences(&preferences, cx) {
            _ = owner.update(cx, |this, cx| this.refresh_appearance(window, cx));
        }
    }

    fn ensure_theme_catalog(&mut self, cx: &mut Context<Self>) {
        if self.theme_catalog_loading || !self.theme_catalog.is_empty() {
            return;
        }
        self.theme_catalog_loading = true;
        self.theme_catalog_error = None;
        let client = cx.http_client();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = theme_sources::fetch_theme_catalog(client).await;
            _ = this.update(cx, |this, cx| {
                this.theme_catalog_loading = false;
                match result {
                    Ok(catalog) => {
                        this.theme_catalog = catalog;
                        this.theme_catalog_error = None;
                    }
                    Err(error) => {
                        this.theme_catalog_error =
                            Some(format!("Could not load Zed themes: {error:#}").into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn install_catalog_theme(&mut self, extension_id: String, cx: &mut Context<Self>) {
        if !self.theme_packs_installing.insert(extension_id.clone()) {
            return;
        }
        self.theme_catalog_error = None;
        let client = cx.http_client();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = theme_sources::install_theme_pack(client, &extension_id).await;
            _ = this.update(cx, |this, cx| {
                this.theme_packs_installing.remove(&extension_id);
                match result {
                    Ok(installed) => {
                        let report =
                            theme_sources::load_external_themes(&theme::ThemeRegistry::global(cx));
                        for error in report.errors {
                            log::warn!("could not load installed theme: {error}");
                        }
                        this.installed_theme_packs.insert(extension_id.clone());
                        this.theme_catalog_error = None;
                        log::info!(
                            "installed Zed theme pack {extension_id} ({} files, {} new themes)",
                            installed.theme_files,
                            report.themes_added
                        );
                    }
                    Err(error) => {
                        this.theme_catalog_error = Some(
                            format!("Could not install theme pack {extension_id}: {error:#}")
                                .into(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_appearance_selector(&self, cx: &Context<Self>) -> AnyElement {
        let typography = ThemeSettings::get_global(cx);
        IconButton::new("appearance-selector-trigger", IconName::Settings)
            .shape(IconButtonShape::Square)
            .size(ButtonSize::Default)
            .style(if self.appearance_settings_open {
                ButtonStyle::Tinted(TintColor::Accent)
            } else {
                ButtonStyle::Subtle
            })
            .aria_label(format!(
                "Appearance settings: {}; reading {} {:.0}px; code {} {:.0}px",
                cx.theme().name,
                typography.agent_ui_font_family(),
                typography.agent_ui_font_size(cx).as_f32(),
                typography.agent_buffer_font_family(),
                typography.agent_buffer_font_size(cx).as_f32(),
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.appearance_settings_open = !this.appearance_settings_open;
                this.appearance_scroll_handle = ScrollHandle::new();
                if this.appearance_settings_open {
                    this.ensure_theme_catalog(cx);
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_appearance_settings(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let visuals = HarnessVisualTheme::from_zed(&colors);
        let owner = cx.entity().downgrade();
        let typography = ThemeSettings::get_global(cx);
        let reading_font = typography.agent_ui_font_family().clone();
        let reading_size = typography.agent_ui_font_size(cx).as_f32();
        let code_font = typography.agent_buffer_font_family().clone();
        let code_size = typography.agent_buffer_font_size(cx).as_f32();
        let scroll_handle = self.appearance_scroll_handle.clone();

        let theme_tab = Button::new("appearance-themes-tab", "Themes")
            .size(ButtonSize::Default)
            .style(ButtonStyle::Subtle)
            .toggle_state(self.appearance_settings_section == AppearanceSettingsSection::Themes)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .on_click({
                let owner = owner.clone();
                move |_, _, cx| {
                    _ = owner.update(cx, |this, cx| {
                        this.appearance_settings_section = AppearanceSettingsSection::Themes;
                        this.appearance_scroll_handle = ScrollHandle::new();
                        cx.notify();
                    });
                }
            });
        let explore_tab = Button::new("appearance-explore-tab", "Explore")
            .size(ButtonSize::Default)
            .style(ButtonStyle::Subtle)
            .toggle_state(self.appearance_settings_section == AppearanceSettingsSection::Explore)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .on_click({
                let owner = owner.clone();
                move |_, _, cx| {
                    _ = owner.update(cx, |this, cx| {
                        this.appearance_settings_section = AppearanceSettingsSection::Explore;
                        this.appearance_scroll_handle = ScrollHandle::new();
                        this.ensure_theme_catalog(cx);
                        cx.notify();
                    });
                }
            });
        let typography_tab = Button::new("appearance-typography-tab", "Typography")
            .size(ButtonSize::Default)
            .style(ButtonStyle::Subtle)
            .toggle_state(self.appearance_settings_section == AppearanceSettingsSection::Typography)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .on_click({
                let owner = owner.clone();
                move |_, _, cx| {
                    _ = owner.update(cx, |this, cx| {
                        this.appearance_settings_section = AppearanceSettingsSection::Typography;
                        this.appearance_scroll_handle = ScrollHandle::new();
                        cx.notify();
                    });
                }
            });

        let body = match self.appearance_settings_section {
            AppearanceSettingsSection::Themes => {
                let current_theme = cx.theme().name.clone();
                let mut themes = theme::ThemeRegistry::global(cx).list();
                themes.sort_unstable_by(|left, right| {
                    left.appearance
                        .is_light()
                        .cmp(&right.appearance.is_light())
                        .then(left.name.cmp(&right.name))
                });
                let registry = theme::ThemeRegistry::global(cx);
                let mut rows = Vec::with_capacity(themes.len() + 2);
                let mut rendering_light_themes = false;
                rows.push(
                    div()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .text_ui_sm(cx)
                        .text_color(colors.text_muted)
                        .child("Dark")
                        .into_any_element(),
                );
                for (index, theme) in themes.into_iter().enumerate() {
                    if theme.appearance.is_light() && !rendering_light_themes {
                        rendering_light_themes = true;
                        rows.push(
                            div()
                                .px_3()
                                .pt_3()
                                .pb_1()
                                .text_ui_sm(cx)
                                .text_color(colors.text_muted)
                                .child("Light")
                                .into_any_element(),
                        );
                    }
                    let selected = theme.name == current_theme;
                    let theme_name = theme.name.clone();
                    let theme_colors = registry
                        .get(&theme.name)
                        .map(|theme| theme.colors().clone())
                        .unwrap_or_else(|_| colors.clone());
                    let owner = owner.clone();
                    rows.push(
                        div()
                            .id(("appearance-theme", index))
                            .mx_2()
                            .mb_1()
                            .min_h(px(38.))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(if selected {
                                colors.border_selected
                            } else {
                                gpui::transparent_black()
                            })
                            .bg(if selected {
                                colors.element_selected
                            } else {
                                visuals.raised_surface
                            })
                            .hover(|style| style.bg(colors.element_hover))
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                let theme_name = theme_name.clone();
                                Self::update_appearance(&owner, window, cx, move |preferences| {
                                    preferences.theme = theme_name.to_string();
                                });
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .gap_0p5()
                                    .rounded_xs()
                                    .overflow_hidden()
                                    .children(
                                        [
                                            theme_colors.editor_background,
                                            theme_colors.text,
                                            theme_colors.text_accent,
                                            theme_colors.version_control_added,
                                            theme_colors.version_control_deleted,
                                        ]
                                        .map(|color| div().w(px(10.)).h(px(20.)).bg(color)),
                                    ),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_ui(cx)
                                    .text_color(colors.text)
                                    .child(theme.name),
                            )
                            .into_any_element(),
                    );
                }
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .children(rows)
                    .into_any_element()
            }
            AppearanceSettingsSection::Explore => {
                let mut rows = Vec::new();
                rows.push(
                    div()
                        .px_3()
                        .pt_3()
                        .pb_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().text_ui(cx).text_color(colors.text).child(
                            if self.theme_catalog.is_empty() {
                                "Zed theme packs".to_owned()
                            } else {
                                format!("{} Zed theme packs", self.theme_catalog.len())
                            },
                        ))
                        .when(self.theme_catalog_loading, |this| {
                            this.child(SpinnerLabel::new().size(LabelSize::Small))
                        })
                        .into_any_element(),
                );
                rows.push(
                    div()
                        .px_3()
                        .pb_2()
                        .child(self.theme_catalog_filter.clone())
                        .into_any_element(),
                );

                if let Some(error) = self.theme_catalog_error.clone() {
                    let retry_owner = owner.clone();
                    rows.push(
                        div()
                            .mx_2()
                            .mb_2()
                            .p_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().status().error.opacity(0.45))
                            .bg(cx.theme().status().error_background)
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_ui_sm(cx)
                                    .text_color(colors.text)
                                    .child(error),
                            )
                            .child(
                                Button::new("retry-theme-catalog", "Retry")
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Subtle)
                                    .on_click(move |_, _, cx| {
                                        _ = retry_owner.update(cx, |this, cx| {
                                            this.theme_catalog_error = None;
                                            this.ensure_theme_catalog(cx);
                                        });
                                    }),
                            )
                            .into_any_element(),
                    );
                }

                let query = self
                    .theme_catalog_filter
                    .read(cx)
                    .text(cx)
                    .trim()
                    .to_lowercase();
                let matching = self
                    .theme_catalog
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        query.is_empty()
                            || entry.name.to_lowercase().contains(&query)
                            || entry.description.to_lowercase().contains(&query)
                            || entry.id.to_lowercase().contains(&query)
                    })
                    .collect::<Vec<_>>();
                let visible = self.theme_catalog_visible.min(matching.len());
                for (index, entry) in matching.iter().take(visible).copied() {
                    let installed = self.installed_theme_packs.contains(&entry.id);
                    let installing = self.theme_packs_installing.contains(&entry.id);
                    let extension_id = entry.id.clone();
                    let install_owner = owner.clone();
                    let action = if installing {
                        div()
                            .h(px(28.))
                            .px_2()
                            .flex()
                            .items_center()
                            .child(SpinnerLabel::new().size(LabelSize::Small))
                            .into_any_element()
                    } else {
                        Button::new(
                            ("install-theme-pack", index),
                            if installed { "Update" } else { "Get" },
                        )
                        .size(ButtonSize::Compact)
                        .style(if installed {
                            ButtonStyle::Subtle
                        } else {
                            ButtonStyle::Tinted(TintColor::Accent)
                        })
                        .on_click(move |_, _, cx| {
                            let extension_id = extension_id.clone();
                            _ = install_owner.update(cx, |this, cx| {
                                this.install_catalog_theme(extension_id, cx);
                            });
                        })
                        .into_any_element()
                    };
                    let metadata = if entry.download_count >= 1_000_000 {
                        format!(
                            "v{} · {:.1}m downloads",
                            entry.version,
                            entry.download_count as f64 / 1_000_000.
                        )
                    } else if entry.download_count >= 1_000 {
                        format!(
                            "v{} · {:.0}k downloads",
                            entry.version,
                            entry.download_count as f64 / 1_000.
                        )
                    } else {
                        format!("v{} · {} downloads", entry.version, entry.download_count)
                    };
                    rows.push(
                        div()
                            .id(("theme-catalog-entry", index))
                            .mx_2()
                            .mb_1()
                            .min_h(px(56.))
                            .px_3()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(visuals.divider)
                            .bg(visuals.raised_surface)
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .flex()
                                            .items_baseline()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_ui(cx)
                                                    .text_color(colors.text)
                                                    .child(entry.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_ui_sm(cx)
                                                    .text_color(colors.text_muted)
                                                    .child(metadata),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_ui_sm(cx)
                                            .text_color(colors.text_muted)
                                            .child(entry.description.clone()),
                                    ),
                            )
                            .child(action)
                            .into_any_element(),
                    );
                }
                if visible < matching.len() {
                    let more_owner = owner.clone();
                    let remaining = matching.len() - visible;
                    rows.push(
                        div()
                            .px_3()
                            .py_3()
                            .flex()
                            .justify_center()
                            .child(
                                Button::new(
                                    "show-more-theme-packs",
                                    format!("Show {} more", remaining.min(THEME_CATALOG_PAGE_SIZE)),
                                )
                                .size(ButtonSize::Default)
                                .style(ButtonStyle::Subtle)
                                .on_click(move |_, _, cx| {
                                    _ = more_owner.update(cx, |this, cx| {
                                        this.theme_catalog_visible = this
                                            .theme_catalog_visible
                                            .saturating_add(THEME_CATALOG_PAGE_SIZE);
                                        cx.notify();
                                    });
                                }),
                            )
                            .into_any_element(),
                    );
                }

                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .children(rows)
                    .into_any_element()
            }
            AppearanceSettingsSection::Typography => {
                let role = self.appearance_font_role;
                let (current_font, current_size, current_weight, sample): (
                    SharedString,
                    f32,
                    f32,
                    &'static str,
                ) = match role {
                    AppearanceFontRole::Reading => (
                        reading_font.clone(),
                        reading_size,
                        typography.ui_font.weight.0,
                        "Clear prose should feel effortless to read.",
                    ),
                    AppearanceFontRole::Code => (
                        code_font.clone(),
                        code_size,
                        typography.buffer_font.weight.0,
                        "let transcript = render(events);",
                    ),
                };
                let mut fonts = theme::FontFamilyCache::global(cx)
                    .try_list_font_families()
                    .unwrap_or_default();
                fonts.extend([reading_font.clone(), code_font.clone()]);
                fonts.sort_unstable_by_key(|font| font.to_lowercase());
                fonts.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

                let reading_role = Button::new("appearance-reading-role", "Reading")
                    .size(ButtonSize::Default)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(role == AppearanceFontRole::Reading)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .on_click({
                        let owner = owner.clone();
                        move |_, _, cx| {
                            _ = owner.update(cx, |this, cx| {
                                this.appearance_font_role = AppearanceFontRole::Reading;
                                cx.notify();
                            });
                        }
                    });
                let code_role = Button::new("appearance-code-role", "Code")
                    .size(ButtonSize::Default)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(role == AppearanceFontRole::Code)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .on_click({
                        let owner = owner.clone();
                        move |_, _, cx| {
                            _ = owner.update(cx, |this, cx| {
                                this.appearance_font_role = AppearanceFontRole::Code;
                                cx.notify();
                            });
                        }
                    });

                let smaller_owner = owner.clone();
                let larger_owner = owner.clone();
                let size_controls = div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        IconButton::new("appearance-font-smaller", IconName::Dash)
                            .shape(IconButtonShape::Square)
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Subtle)
                            .aria_label("Decrease font size")
                            .on_click(move |_, window, cx| {
                                Self::update_appearance(&smaller_owner, window, cx, |preferences| {
                                    match role {
                                        AppearanceFontRole::Reading => {
                                            preferences.reading_font_size =
                                                Some((current_size - 1.).clamp(
                                                    MIN_HARNESS_FONT_SIZE,
                                                    MAX_HARNESS_FONT_SIZE,
                                                ))
                                        }
                                        AppearanceFontRole::Code => {
                                            preferences.code_font_size =
                                                Some((current_size - 1.).clamp(
                                                    MIN_HARNESS_FONT_SIZE,
                                                    MAX_HARNESS_FONT_SIZE,
                                                ))
                                        }
                                    }
                                })
                            }),
                    )
                    .child(
                        div()
                            .w(px(48.))
                            .text_center()
                            .text_ui_sm(cx)
                            .text_color(colors.text)
                            .child(format!("{current_size:.0} px")),
                    )
                    .child(
                        IconButton::new("appearance-font-larger", IconName::Plus)
                            .shape(IconButtonShape::Square)
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Subtle)
                            .aria_label("Increase font size")
                            .on_click(move |_, window, cx| {
                                Self::update_appearance(&larger_owner, window, cx, |preferences| {
                                    match role {
                                        AppearanceFontRole::Reading => {
                                            preferences.reading_font_size =
                                                Some((current_size + 1.).clamp(
                                                    MIN_HARNESS_FONT_SIZE,
                                                    MAX_HARNESS_FONT_SIZE,
                                                ))
                                        }
                                        AppearanceFontRole::Code => {
                                            preferences.code_font_size =
                                                Some((current_size + 1.).clamp(
                                                    MIN_HARNESS_FONT_SIZE,
                                                    MAX_HARNESS_FONT_SIZE,
                                                ))
                                        }
                                    }
                                })
                            }),
                    );

                let weight_choices = [
                    (100., "Thin"),
                    (200., "Extra light"),
                    (300., "Light"),
                    (400., "Regular"),
                    (500., "Medium"),
                    (600., "Semibold"),
                    (700., "Bold"),
                    (800., "Extra bold"),
                    (900., "Black"),
                ];
                let current_weight_name = weight_choices
                    .iter()
                    .min_by(|(left, _), (right, _)| {
                        (current_weight - *left)
                            .abs()
                            .total_cmp(&(current_weight - *right).abs())
                    })
                    .map(|(_, label)| *label)
                    .unwrap_or("Custom");
                let mut weight_buttons = Vec::with_capacity(weight_choices.len());
                for (index, (weight, label)) in weight_choices.into_iter().enumerate() {
                    let selected = (current_weight - weight).abs() < 1.;
                    let owner = owner.clone();
                    weight_buttons.push(
                        Button::new(("appearance-font-weight", index), label)
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Subtle)
                            .toggle_state(selected)
                            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                            .on_click(move |_, window, cx| {
                                Self::update_appearance(&owner, window, cx, |preferences| {
                                    let weight = weight
                                        .clamp(MIN_HARNESS_FONT_WEIGHT, MAX_HARNESS_FONT_WEIGHT);
                                    match role {
                                        AppearanceFontRole::Reading => {
                                            preferences.reading_font_weight = Some(weight)
                                        }
                                        AppearanceFontRole::Code => {
                                            preferences.code_font_weight = Some(weight)
                                        }
                                    }
                                })
                            })
                            .into_any_element(),
                    );
                }

                let has_custom_typography = {
                    let preferences = preferred_preferences();
                    preferences.reading_font_family.is_some()
                        || preferences.reading_font_size.is_some()
                        || preferences.reading_font_weight.is_some()
                        || preferences.code_font_family.is_some()
                        || preferences.code_font_size.is_some()
                        || preferences.code_font_weight.is_some()
                };
                let reset_owner = owner.clone();
                let mut font_rows = Vec::with_capacity(fonts.len());
                for (index, font) in fonts.into_iter().enumerate() {
                    let selected = font == current_font;
                    let font_name = font.to_string();
                    let owner = owner.clone();
                    font_rows.push(
                        div()
                            .id(("appearance-font", index))
                            .mx_2()
                            .mb_1()
                            .min_h(px(38.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_sm()
                            .border_1()
                            .border_color(if selected {
                                colors.border_selected
                            } else {
                                gpui::transparent_black()
                            })
                            .bg(if selected {
                                colors.element_selected
                            } else {
                                visuals.raised_surface
                            })
                            .hover(|style| style.bg(colors.element_hover))
                            .cursor_pointer()
                            .font_family(font.clone())
                            .font_weight(gpui::FontWeight(current_weight))
                            .text_size(px(current_size.clamp(12., 20.)))
                            .text_color(colors.text)
                            .on_click(move |_, window, cx| {
                                let font_name = font_name.clone();
                                Self::update_appearance(&owner, window, cx, move |preferences| {
                                    match role {
                                        AppearanceFontRole::Reading => {
                                            preferences.reading_font_family = Some(font_name)
                                        }
                                        AppearanceFontRole::Code => {
                                            preferences.code_font_family = Some(font_name)
                                        }
                                    }
                                });
                            })
                            .child(font)
                            .into_any_element(),
                    );
                }

                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_3()
                            .pt_3()
                            .pb_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(reading_role)
                            .child(code_role)
                            .child(div().flex_1())
                            .child(size_controls),
                    )
                    .child(
                        div()
                            .px_3()
                            .pb_2()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .mr_1()
                                    .text_ui_sm(cx)
                                    .text_color(colors.text_muted)
                                    .child(format!(
                                        "Weight · {current_weight_name} ({current_weight:.0})"
                                    )),
                            )
                            .children(weight_buttons),
                    )
                    .child(
                        div()
                            .mx_2()
                            .mb_2()
                            .min_h(px(66.))
                            .px_3()
                            .py_2()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .rounded_sm()
                            .border_1()
                            .border_color(visuals.divider)
                            .bg(colors.editor_background)
                            .font_family(current_font.clone())
                            .font_weight(gpui::FontWeight(current_weight))
                            .text_size(px(current_size))
                            .text_color(colors.text)
                            .child(sample),
                    )
                    .child(
                        div()
                            .px_3()
                            .pb_1()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_ui_sm(cx)
                                    .text_color(colors.text_muted)
                                    .child(format!("Font family · {current_font}")),
                            )
                            .child(
                                Button::new("appearance-reset-typography", "Reset")
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Subtle)
                                    .disabled(!has_custom_typography)
                                    .on_click(move |_, window, cx| {
                                        Self::update_appearance(
                                            &reset_owner,
                                            window,
                                            cx,
                                            HarnessPreferences::reset_typography,
                                        )
                                    }),
                            ),
                    )
                    .children(font_rows)
                    .into_any_element()
            }
        };

        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_end()
            .bg(gpui::black().opacity(0.12))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.appearance_settings_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(relative(0.38))
                    .min_w(px(420.))
                    .max_w(px(560.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .border_l_1()
                    .border_color(visuals.strong_divider)
                    .bg(visuals.rail)
                    .shadow_lg()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(px(44.))
                            .flex_none()
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .border_b_1()
                            .border_color(visuals.divider)
                            .child(
                                div()
                                    .px_1()
                                    .flex_1()
                                    .text_ui(cx)
                                    .text_color(colors.text)
                                    .child("Appearance"),
                            )
                            .child(theme_tab)
                            .child(explore_tab)
                            .child(typography_tab)
                            .child(
                                IconButton::new("close-appearance-settings", IconName::Close)
                                    .shape(IconButtonShape::Square)
                                    .size(ButtonSize::Default)
                                    .style(ButtonStyle::Subtle)
                                    .aria_label("Close appearance settings")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.appearance_settings_open = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id("appearance-settings-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&scroll_handle)
                            .child(body),
                    )
                    .vertical_scrollbar_for(&scroll_handle, window, cx),
            )
            .into_any_element()
    }

    fn apply_thread_open_settings(&mut self, response: &ThreadOpenResponse) {
        self.model.telemetry.set_thread_settings(
            model::ThreadSettingsSnapshot::from_open_response(
                response.model.clone(),
                response.model_provider.clone(),
                response.reasoning_effort.clone(),
                response.approval_policy.clone(),
                response.sandbox.clone(),
                response.active_permission_profile.clone(),
                response.service_tier.clone(),
                response.cwd.clone(),
            ),
        );
    }

    fn load_server_options(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let cwd = self.cwd.clone();
        let requested_cwd = cwd.clone();
        cx.spawn(async move |this, cx| {
            let models = client.request("model/list", json!({ "limit": 100 })).await;
            let permissions = client
                .request(
                    "permissionProfile/list",
                    json!({ "cwd": cwd, "limit": 100 }),
                )
                .await;
            _ = this.update(cx, |this, cx| {
                match models {
                    Ok(response) => this.available_models = model_choices_from_response(&response),
                    Err(error) => log::warn!("could not load Codex model catalog: {error}"),
                }
                match permissions {
                    Ok(response) if this.cwd == requested_cwd => {
                        this.permission_profiles =
                            permission_profile_choices_from_response(&response)
                    }
                    Ok(_) => {}
                    Err(error) => log::warn!("could not load Codex permission profiles: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_queued_turns(&mut self, cx: &mut Context<Self>) {
        if self.queue_refresh_pending
            || !queue_state_is_visible(
                self.selected_thread_id.as_deref(),
                self.preserved_work_thread_id.as_deref(),
            )
        {
            return;
        }
        let (Some(client), Some(thread_id)) =
            (self.client.clone(), self.selected_thread_id.clone())
        else {
            return;
        };
        self.queue_refresh_pending = true;
        self.queue_refresh_generation = self.queue_refresh_generation.wrapping_add(1);
        let generation = self.queue_refresh_generation;
        cx.spawn(async move |this, cx| {
            let result = client.list_queued_turns(&thread_id).await;
            _ = this.update(cx, |this, cx| {
                if this.queue_refresh_generation != generation {
                    return;
                }
                this.queue_refresh_pending = false;
                if !queue_state_belongs_to_thread(
                    &thread_id,
                    this.selected_thread_id.as_deref(),
                    this.preserved_work_thread_id.as_deref(),
                ) {
                    return;
                }
                let origin_is_visible =
                    callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref());
                match result {
                    Ok(response) => {
                        let pending = this
                            .queued_turns
                            .iter()
                            .filter(|entry| entry.id.is_none())
                            .cloned()
                            .collect::<Vec<_>>();
                        let mut queued = queued_submissions_from_response(&response);
                        for entry in pending {
                            if !queued.iter().any(|candidate| {
                                candidate.client_user_message_id == entry.client_user_message_id
                            }) {
                                queued.push_back(entry);
                            }
                        }
                        this.queued_turns = queued;
                        if origin_is_visible {
                            this.error = None;
                        }
                    }
                    Err(error) => log::warn!("could not refresh queued prompts: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn update_thread_settings(&mut self, fields: Value, cx: &mut Context<Self>) {
        let (Some(client), Some(thread_id)) =
            (self.client.clone(), self.selected_thread_id.clone())
        else {
            return;
        };
        let requested_thread_id = thread_id.clone();
        let mut params = fields.as_object().cloned().unwrap_or_default();
        params.insert("threadId".into(), thread_id.into());
        self.settings_update_pending = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .request("thread/settings/update", Value::Object(params))
                .await;
            _ = this.update(cx, |this, cx| {
                if this.selected_thread_id.as_deref() != Some(requested_thread_id.as_str()) {
                    return;
                }
                this.settings_update_pending = false;
                if let Err(error) = result {
                    this.error = Some(format!("Could not update task settings: {error}").into());
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn persist_transcript_in_background(&self, thread_id: &str, cx: &mut Context<Self>) {
        let thread_id = thread_id.to_owned();
        cx.spawn(async move |this, cx| {
            // Let the newly loaded transcript reach the compositor before
            // serializing its cache. The write and fsync already run in the
            // background; this small deferral also keeps JSON preparation out
            // of the first visible history frame.
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            _ = this.update(cx, |this, cx| {
                if this.selected_thread_id.as_deref() != Some(thread_id.as_str()) {
                    return;
                }
                let snapshot = match this.model.prepare_transcript_snapshot(&thread_id) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        log::warn!("could not prepare transcript cache for {thread_id}: {error}");
                        return;
                    }
                };
                let snapshot_thread_id = thread_id.clone();
                cx.background_spawn(async move {
                    if let Err(error) = snapshot.write() {
                        log::warn!("could not cache task {snapshot_thread_id}: {error}");
                    }
                })
                .detach();
            });
        })
        .detach();
    }

    fn render_model_effort_selector(&self, cx: &Context<Self>) -> AnyElement {
        let settings = self.model.telemetry.thread_settings.as_ref();
        let selected_model = settings.and_then(|settings| settings.model.as_deref());
        let selected_effort = settings.and_then(|settings| settings.effort.as_deref());
        let selected_choice = effective_model_choice(&self.available_models, selected_model);
        let effective_effort = effective_reasoning_effort(selected_effort, selected_choice);
        let model_label = selected_choice
            .map(|choice| choice.display_name.clone())
            .or_else(|| selected_model.map(SharedString::from))
            .unwrap_or_else(|| "Loading models…".into());
        // Preserve Codex's important at-a-glance effort state while using the
        // same provider/model trigger hierarchy as Zed's agent composer.
        let trigger_label: SharedString = effective_effort
            .as_deref()
            .map(reasoning_effort_label)
            .map(|effort| format!("{model_label} · {effort}").into())
            .unwrap_or(model_label);
        let choices = self.available_models.clone();
        let current_model = selected_choice
            .map(|choice| choice.model.clone())
            .or_else(|| selected_model.map(ToOwned::to_owned));
        let current_effort = effective_effort;
        let effort_choices = selected_choice
            .map(|choice| choice.efforts.clone())
            .unwrap_or_default();
        let menu_deployed = self.model_menu_handle.is_deployed();
        let trigger_color = if menu_deployed {
            Color::Accent
        } else {
            Color::Muted
        };
        let weak = cx.weak_entity();
        let trigger = Button::new("model-effort-selector-trigger", trigger_label)
            .label_size(LabelSize::Small)
            .color(trigger_color)
            .start_icon(
                Icon::new(IconName::AiOpenAi)
                    .size(IconSize::XSmall)
                    .color(trigger_color),
            )
            .end_icon(
                Icon::new(if menu_deployed {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .disabled(
                (self.selected_thread_id.is_none() && self.replay_count.is_none())
                    || self.settings_update_pending,
            )
            .aria_label("Change model or thinking effort");

        PopoverMenu::new("model-effort-selector")
            .trigger(trigger)
            .anchor(gpui::Anchor::BottomRight)
            .offset(gpui::Point {
                x: px(0.),
                y: px(-2.),
            })
            .menu(move |window, cx| {
                let choices = choices.clone();
                let current_model = current_model.clone();
                let current_effort = current_effort.clone();
                let effort_choices = effort_choices.clone();
                let weak = weak.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("Model");
                    if choices.is_empty() {
                        menu = menu
                            .item(ContextMenuEntry::new("Loading model catalog…").disabled(true));
                    } else {
                        for choice in choices {
                            let model = choice.model.clone();
                            let is_selected =
                                current_model.as_deref() == Some(choice.model.as_str());
                            let effort = current_effort
                                .as_ref()
                                .filter(|effort| choice.efforts.contains(effort))
                                .cloned()
                                .unwrap_or_else(|| choice.default_effort.clone());
                            let entry = ContextMenuEntry::new(choice.display_name)
                                .toggleable(IconPosition::End, is_selected)
                                .handler({
                                    let weak = weak.clone();
                                    move |_, cx| {
                                        weak.update(cx, |this, cx| {
                                            this.update_thread_settings(
                                                json!({ "model": model, "effort": effort }),
                                                cx,
                                            )
                                        })
                                        .ok();
                                    }
                                });
                            menu = menu.item(entry);
                        }
                    }
                    menu = menu.separator().header("Thinking effort");
                    if effort_choices.is_empty() {
                        menu =
                            menu.item(ContextMenuEntry::new("No thinking controls").disabled(true));
                    } else {
                        for effort in effort_choices {
                            let selected = current_effort.as_deref() == Some(effort.as_str());
                            menu = menu.item(
                                ContextMenuEntry::new(reasoning_effort_label(&effort))
                                    .toggleable(IconPosition::End, selected)
                                    .handler({
                                        let weak = weak.clone();
                                        move |_, cx| {
                                            weak.update(cx, |this, cx| {
                                                this.update_thread_settings(
                                                    json!({ "effort": effort }),
                                                    cx,
                                                )
                                            })
                                            .ok();
                                        }
                                    }),
                            );
                        }
                    }
                    menu
                }))
            })
            .with_handle(self.model_menu_handle.clone())
            .into_any_element()
    }

    fn render_permission_selector(&self, cx: &Context<Self>) -> AnyElement {
        let active_profile = self
            .model
            .telemetry
            .thread_settings
            .as_ref()
            .and_then(|settings| settings.active_permission_profile.as_ref())
            .and_then(|profile| profile.id.as_deref());
        let label = active_profile
            .map(permission_profile_label)
            .unwrap_or_else(|| "Permissions".into());
        let current_profile = active_profile.map(ToOwned::to_owned);
        let profiles = self.permission_profiles.clone();
        let menu_deployed = self.permission_menu_handle.is_deployed();
        let trigger_color = if menu_deployed {
            Color::Accent
        } else {
            Color::Muted
        };
        let weak = cx.weak_entity();
        let trigger = Button::new("permission-selector-trigger", label)
            .label_size(LabelSize::Small)
            .color(trigger_color)
            .end_icon(
                Icon::new(if menu_deployed {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .disabled(
                (self.selected_thread_id.is_none() && self.replay_count.is_none())
                    || self.settings_update_pending,
            )
            .aria_label("Change task permissions");

        PopoverMenu::new("permission-selector")
            .trigger(trigger)
            .anchor(gpui::Anchor::BottomRight)
            .offset(gpui::Point {
                x: px(0.),
                y: px(-2.),
            })
            .menu(move |window, cx| {
                let profiles = profiles.clone();
                let current_profile = current_profile.clone();
                let weak = weak.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("Permissions");
                    if profiles.is_empty() {
                        return menu.item(
                            ContextMenuEntry::new("Loading permission profiles…").disabled(true),
                        );
                    }
                    for profile in profiles {
                        let id = profile.id.clone();
                        let mut entry =
                            ContextMenuEntry::new(permission_profile_label(&profile.id))
                                .toggleable(
                                    IconPosition::End,
                                    current_profile.as_deref() == Some(id.as_str()),
                                )
                                .disabled(!profile.allowed);
                        if let Some(description) = profile.description {
                            entry = entry
                                .documentation_aside(DocumentationSide::Right, move |_| {
                                    Label::new(description.clone()).into_any_element()
                                });
                        }
                        menu = menu.item(entry.handler({
                            let weak = weak.clone();
                            move |_, cx| {
                                weak.update(cx, |this, cx| {
                                    this.update_thread_settings(json!({ "permissions": id }), cx)
                                })
                                .ok();
                            }
                        }));
                    }
                    menu
                }))
            })
            .with_handle(self.permission_menu_handle.clone())
            .into_any_element()
    }

    fn render_context_usage(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (used, capacity) = context_window_usage(self.model.telemetry.token_usage.as_ref())?;
        let ratio = used as f32 / capacity as f32;
        let progress_color = if ratio >= 0.85 {
            cx.theme().status().warning
        } else {
            cx.theme().colors().text_muted
        };
        let tooltip: SharedString = format!(
            "Context: {} / {} tokens ({:.0}%)",
            compact_token_count(used),
            compact_token_count(capacity),
            ratio * 100.
        )
        .into();
        Some(
            div()
                .id("context-window-usage")
                .size(px(18.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    CircularProgress::new(used as f32, capacity as f32, px(16.), cx)
                        .stroke_width(px(2.))
                        .progress_color(progress_color),
                )
                .hoverable_tooltip(Tooltip::text(tooltip))
                .into_any_element(),
        )
    }

    /// Match Zed's composer terminal action: one stable, normally sized control
    /// changes meaning with the turn and draft state instead of growing into a
    /// row of textual Queue/Stop buttons. Stopping remains the primary action
    /// while the draft is empty; as soon as there is a draft, the same location
    /// queues it.
    fn render_composer_action(
        &self,
        turn_active: bool,
        composer_empty: bool,
        send_blocked: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let state = composer_action_state(turn_active, composer_empty);
        if state == ComposerActionState::Stop {
            return IconButton::new("stop-turn", IconName::Stop)
                .style(ButtonStyle::Subtle)
                .icon_color(Color::Error)
                .aria_label("Stop the current response")
                .tooltip(Tooltip::text("Stop response"))
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                .into_any_element();
        }

        let (id, icon, label): (&'static str, IconName, SharedString) = if state
            == ComposerActionState::Queue
        {
            (
                "queue-turn",
                IconName::QueueMessage,
                "Queue after this response · Ctrl-Shift-Enter adds to the active response".into(),
            )
        } else {
            (
                "send-turn",
                IconName::Send,
                if self.loading_thread {
                    "Wait for task history to finish loading".into()
                } else if self.attaching_thread {
                    "Wait for the live task connection".into()
                } else if self.settings_update_pending {
                    "Wait for task settings to finish updating".into()
                } else if self.thread_read_only_reason.is_some() {
                    "This task is open read-only".into()
                } else if self.client.is_none() && self.replay_count.is_none() {
                    "Reconnect to Codex before sending".into()
                } else if composer_empty {
                    "Type a prompt to send".into()
                } else {
                    "Send prompt".into()
                },
            )
        };
        let tooltip = label.clone();

        IconButton::new(id, icon)
            .style(ButtonStyle::Subtle)
            .disabled(send_blocked)
            .icon_color(if send_blocked {
                Color::Muted
            } else {
                Color::Accent
            })
            .aria_label(label)
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
            .into_any_element()
    }

    fn render_queued_turns(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.queued_turns.is_empty()
            || !queue_state_is_visible(
                self.selected_thread_id.as_deref(),
                self.preserved_work_thread_id.as_deref(),
            )
        {
            return None;
        }
        let colors = cx.theme().colors().clone();
        let visuals = HarnessVisualTheme::from_zed(&colors);
        let queue_count = self.queued_turns.len();
        let active_turn = self.model.current_turn_id.is_some();
        let operations = self.queue_operations.clone();
        let weak = cx.weak_entity();

        Some(
            div()
                .id("queued-prompts")
                .flex_none()
                .max_h(px(160.))
                .border_t_1()
                .border_color(visuals.divider)
                .bg(visuals.pending_surface)
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("queued-prompt-list")
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(self.queued_turns.iter().enumerate().map(|(index, entry)| {
                            let client_id = entry.client_user_message_id.clone();
                            let queue_ready = entry.id.is_some();
                            let operation = operations.get(&client_id).copied();
                            let operation_pending = operation.is_some();
                            let preview = queued_submission_preview(&entry.input);
                            let image_count = queued_submission_image_count(&entry.input);
                            let preview_image_count = entry.preview_images.len();
                            let remaining_image_count =
                                image_count.saturating_sub(preview_image_count);
                            let position_tooltip = format!(
                                "Queued prompt {} of {queue_count}",
                                index + 1
                            );
                            let weak_edit = weak.clone();
                            let weak_steer = weak.clone();
                            let weak_send = weak.clone();
                            let weak_remove = weak.clone();
                            div()
                                .id(("queued-prompt", index))
                                .group("queued-prompt")
                                .h(px(32.))
                                .flex_none()
                                .px_2p5()
                                .flex()
                                .items_center()
                                .gap_1()
                                .when(index + 1 < queue_count, |this| {
                                    this.border_b_1().border_color(colors.border_variant)
                                })
                                .child(
                                    div()
                                        .id(("queued-prompt-position", index))
                                        .flex_none()
                                        .tooltip(Tooltip::text(position_tooltip))
                                        .child(
                                            Icon::new(IconName::QueueMessage)
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                )
                                .children(entry.preview_images.iter().map(|image| {
                                    div()
                                        .size(px(22.))
                                        .flex_none()
                                        .overflow_hidden()
                                        .rounded_xs()
                                        .border_1()
                                        .border_color(colors.border_variant)
                                        .bg(colors.editor_background)
                                        .child(
                                            gpui::img(image.clone())
                                                .size_full()
                                                .object_fit(ObjectFit::Cover),
                                        )
                                }))
                                .when(remaining_image_count > 0, |this| {
                                    this.child(
                                        Label::new(if preview_image_count == 0 {
                                            if remaining_image_count == 1 {
                                                "1 image".into()
                                            } else {
                                                format!("{remaining_image_count} images")
                                            }
                                        } else {
                                            format!("+{remaining_image_count}")
                                        })
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .text_color(colors.text)
                                        .child(if preview.is_empty() {
                                            "Image prompt".to_owned()
                                        } else {
                                            preview
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap_0p5()
                                        .when(!queue_ready && operation.is_none(), |this| {
                                            this.child(SpinnerLabel::new().size(LabelSize::Small))
                                                .child(
                                                    Label::new("Saving…")
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                        })
                                        .when_some(operation, |this, operation| {
                                            this.child(SpinnerLabel::new().size(LabelSize::Small))
                                                .child(
                                                    Label::new(operation.progress_label())
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                        })
                                        .when(queue_ready && !operation_pending, |this| {
                                            this.when(active_turn, |this| {
                                                let client_id =
                                                    entry.client_user_message_id.clone();
                                                this.child(
                                                    Button::new(
                                                        ("steer-queued-prompt", index),
                                                        "Add",
                                                    )
                                                    .start_icon(
                                                        Icon::new(IconName::SteeringWheel)
                                                            .size(IconSize::XSmall),
                                                    )
                                                    .size(ButtonSize::Compact)
                                                    .style(ButtonStyle::Subtle)
                                                    .aria_label("Add queued prompt to response")
                                                    .tooltip(Tooltip::text(
                                                        "Add to the active response at its next input boundary · Ctrl-Shift-Enter for a draft",
                                                    ))
                                                    .on_click(move |_, _, cx| {
                                                        weak_steer
                                                            .update(cx, |this, cx| {
                                                                this.steer_queued_turn(
                                                                    client_id.clone(),
                                                                    cx,
                                                                )
                                                            })
                                                            .ok();
                                                    }),
                                                )
                                            })
                                            .child(
                                                Button::new(
                                                    ("send-queued-prompt", index),
                                                    if active_turn { "Run now" } else { "Run" },
                                                )
                                                .start_icon(
                                                    Icon::new(IconName::InterruptAndRun)
                                                        .size(IconSize::XSmall),
                                                )
                                                .size(ButtonSize::Compact)
                                                .style(if index == 0 {
                                                    ButtonStyle::Outlined
                                                } else {
                                                    ButtonStyle::Subtle
                                                })
                                                .aria_label(if active_turn {
                                                    "Interrupt response and run queued prompt"
                                                } else {
                                                    "Run queued prompt now"
                                                })
                                                .tooltip(Tooltip::text(if active_turn {
                                                    "Stop the active response and start this prompt as a new turn"
                                                } else {
                                                    "Start this queued prompt now"
                                                }))
                                                .on_click({
                                                    let client_id =
                                                        entry.client_user_message_id.clone();
                                                    move |_, _, cx| {
                                                        weak_send
                                                            .update(cx, |this, cx| {
                                                                this.send_queued_turn_now(
                                                                    client_id.clone(),
                                                                    cx,
                                                                )
                                                            })
                                                            .ok();
                                                    }
                                                }),
                                            )
                                            .child(
                                                IconButton::new(
                                                    ("edit-queued-prompt", index),
                                                    IconName::Pencil,
                                                )
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Compact)
                                                .style(ButtonStyle::Subtle)
                                                .aria_label("Edit queued prompt")
                                                .tooltip(Tooltip::text("Edit queued prompt"))
                                                .on_click(move |_, window, cx| {
                                                    let client_id = client_id.clone();
                                                    weak_edit
                                                        .update(cx, |this, cx| {
                                                            this.edit_queued_turn(
                                                                client_id, window, cx,
                                                            )
                                                        })
                                                        .ok();
                                                }),
                                            )
                                            .child(
                                                IconButton::new(
                                                    ("remove-queued-prompt", index),
                                                    IconName::Trash,
                                                )
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Compact)
                                                .style(ButtonStyle::Subtle)
                                                .aria_label("Remove queued prompt")
                                                .tooltip(Tooltip::text("Remove queued prompt"))
                                                .on_click({
                                                    let client_id =
                                                        entry.client_user_message_id.clone();
                                                    move |_, _, cx| {
                                                        weak_remove
                                                            .update(cx, |this, cx| {
                                                                this.cancel_queued_turn(
                                                                    client_id.clone(),
                                                                    cx,
                                                                )
                                                            })
                                                            .ok();
                                                    }
                                                }),
                                            )
                                        }),
                                )
                        })),
                )
                .into_any_element(),
        )
    }

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
        cx: &App,
    ) -> Option<(RichCommandData, ScrollHandle, ListState, ScrollHandle)> {
        let row_height = harness_code_row_height(cx);
        let needs_rebuild = self
            .rich_nested_scrolls
            .get(&item.key)
            .and_then(|state| state.command.as_ref())
            .is_none_or(|surface| {
                surface.event_count != item.event_count
                    || surface.content_len != item.content.len()
                    || surface.row_height != row_height
            });

        if needs_rebuild {
            let data = rich_command_data(item)?;
            let command_row_count = data.command_row_count;
            let output_row_count = data.rows.len().saturating_sub(command_row_count);
            let state = self
                .rich_nested_scrolls
                .entry(item.key.clone())
                .or_default();
            let output_list_state = state.command.as_ref().map_or_else(
                || {
                    // Completed and streaming terminal output opens at its tail. Each
                    // logical output row is one unwrapped terminal row; allowing it
                    // to wrap while retaining a fixed virtual-row estimate made text
                    // overlap and made click-to-cursor geometry fundamentally wrong.
                    ListState::new(output_row_count, ListAlignment::Bottom, px(160.))
                        .with_uniform_item_height(row_height)
                },
                |surface| surface.output_list_state.clone(),
            );
            output_list_state.set_diagnostics_name(format!("command-output:{}", item.key));
            let row_height_changed = state
                .command
                .as_ref()
                .is_some_and(|surface| surface.row_height != row_height);
            if row_height_changed {
                output_list_state.reset_with_uniform_height(output_row_count, row_height);
            } else if output_list_state.item_count() != output_row_count {
                output_list_state.splice(0..output_list_state.item_count(), output_row_count);
                output_list_state
                    .clone()
                    .with_uniform_item_height(row_height);
            }
            state.command = Some(RichCommandSurface {
                event_count: item.event_count,
                content_len: item.content.len(),
                data,
                command_scroll_handle: state
                    .command
                    .as_ref()
                    .map(|surface| surface.command_scroll_handle.clone())
                    .unwrap_or_default(),
                output_list_state,
                output_horizontal_handle: state
                    .command
                    .as_ref()
                    .map(|surface| surface.output_horizontal_handle.clone())
                    .unwrap_or_default(),
                row_height,
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
                .map(|(index, row)| (index, rich_command_row_navigation_range(&surface.data, row)))
                .find(|(_, range)| range.contains(&cursor) || range.end == cursor)
                .map(|(index, _)| index);
            if let Some(row) = row {
                if row < surface.data.command_row_count {
                    surface.command_scroll_handle.scroll_to_item(row);
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
            surface.command_scroll_handle.clone(),
            surface.output_list_state.clone(),
            surface.output_horizontal_handle.clone(),
        ))
    }

    fn rich_file_change_surface(
        &mut self,
        item: &TranscriptItem,
        navigation: Option<&RichNavigationPaint>,
        cx: &App,
    ) -> (RichFileChangeData, ListState, ScrollHandle) {
        let row_height = harness_code_row_height(cx);
        let needs_rebuild = self
            .rich_nested_scrolls
            .get(&item.key)
            .and_then(|state| state.file_change.as_ref())
            .is_none_or(|surface| {
                surface.event_count != item.event_count
                    || surface.content_len != item.content.len()
                    || surface.row_height != row_height
            });

        if needs_rebuild {
            let data = rich_file_change_data(item);
            let state = self
                .rich_nested_scrolls
                .entry(item.key.clone())
                .or_default();
            let previous = state.file_change.take();
            let list_state = previous.as_ref().map_or_else(
                || {
                    ListState::new(data.rows.len(), ListAlignment::Top, px(80.))
                        .with_uniform_item_height(row_height)
                },
                |surface| surface.list_state.clone(),
            );
            let row_height_changed = previous
                .as_ref()
                .is_some_and(|surface| surface.row_height != row_height);
            if row_height_changed {
                list_state.reset_with_uniform_height(data.rows.len(), row_height);
            } else if list_state.item_count() != data.rows.len() {
                list_state.splice(0..list_state.item_count(), data.rows.len());
            }
            list_state.set_diagnostics_name(format!("file-change:{}", item.key));
            state.file_change = Some(RichFileChangeSurface {
                event_count: item.event_count,
                content_len: item.content.len(),
                data,
                list_state,
                horizontal_handle: previous
                    .map(|surface| surface.horizontal_handle)
                    .unwrap_or_default(),
                row_height,
            });
        }

        let state = self
            .rich_nested_scrolls
            .get_mut(&item.key)
            .expect("file-change scroll state should exist");
        let surface = state
            .file_change
            .as_ref()
            .expect("file-change surface should exist");
        let cursor = navigation
            .and_then(RichNavigationPaint::cursor_range)
            .map(|range| range.start);
        if cursor.is_some() && cursor != state.last_cursor {
            let cursor = cursor.unwrap();
            let exact = surface.data.rows.iter().position(|row| {
                row.logical_range()
                    .is_some_and(|range| range.start <= cursor && cursor < range.end)
            });
            let next = surface.data.rows.iter().position(|row| {
                row.logical_range()
                    .is_some_and(|range| !range.is_empty() && cursor <= range.start)
            });
            if let Some(row) = exact.or(next) {
                surface.list_state.scroll_to_reveal_item(row);
            }
        }
        state.last_cursor = cursor;
        (
            surface.data.clone(),
            surface.list_state.clone(),
            surface.horizontal_handle.clone(),
        )
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
            linewise: snapshot.linewise,
            cursor_claimed: Rc::new(Cell::new(false)),
        })
    }

    fn reveal_rich_navigation_item(&mut self, item_index: usize, body_offset: usize) {
        let Some(item) = self.model.items.get(item_index) else {
            return;
        };
        let viewport = self.list_state.viewport_bounds();
        let item_is_visible = self
            .list_state
            .bounds_for_item(item_index)
            .is_some_and(|bounds| bounds.intersects(&viewport));
        if item.kind != model::TranscriptKind::Command {
            if !item_is_visible {
                self.list_state.scroll_to_reveal_item(item_index);
            }
            return;
        }

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
            linewise: snapshot.linewise,
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
            let autoscroll_cursor =
                changed_markdown_cursor(cached.navigation.as_ref(), next_navigation.as_ref());
            cached.navigation = next_navigation.clone();
            cached.entity.update(cx, |markdown, cx| {
                let navigation = next_navigation.as_ref();
                markdown.set_external_navigation(
                    navigation.map(|navigation| navigation.selections.clone()),
                    navigation.and_then(|navigation| navigation.cursor),
                    cx,
                );
                if let Some(source_index) = autoscroll_cursor {
                    markdown.request_autoscroll_to_source_index(source_index, cx);
                }
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
        let font_family_cache = theme::FontFamilyCache::global(cx);
        cx.spawn(async move |this, cx| {
            font_family_cache.prefetch(cx).await;
            this.update(cx, |_, cx| cx.notify())
        })
        .detach();
        let mode_indicator = cx.new(|cx| ModeIndicator::compact(window, cx));
        let composer = cx.new(|cx| LocalEditor::modal_composer(window, cx));
        let search_editor = cx.new(|cx| LocalEditor::plain_single_line("", window, cx));
        let theme_catalog_filter = cx.new(|cx| {
            ui_input::InputField::new(window, cx, "Search 600+ Zed theme packs…")
                .start_icon(IconName::MagnifyingGlass)
        });
        let transcript_editor = cx.new(|cx| TranscriptEditor::read_only(window, cx));
        let theme_filter_owner = cx.entity().downgrade();
        let theme_filter_for_subscription = theme_catalog_filter.clone();
        let erased_theme_filter = theme_catalog_filter.read(cx).editor().clone();
        erased_theme_filter
            .subscribe(
                Box::new(move |event, _, cx| {
                    if event == ui_input::ErasedEditorEvent::BufferEdited {
                        _ = theme_filter_owner.update(cx, |this, cx| {
                            this.theme_catalog_visible = THEME_CATALOG_PAGE_SIZE;
                            // Reading the field here makes the subscription's
                            // dependency explicit and ensures its newest text
                            // has reached the erased editor before repaint.
                            _ = theme_filter_for_subscription.read(cx).text(cx);
                            cx.notify();
                        });
                    }
                }),
                window,
                cx,
            )
            .detach();
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
            |this, editor, _: &LocalEditorChanged, window, cx| {
                // The native auto-height Editor owns wrapping and intrinsic
                // row measurement. Re-render controls when text changes, but
                // never guess its visual height from character counts.
                let text = editor.read(cx).text(cx);
                this.composer_images
                    .retain(|attachment| text.contains(&composer_image_token(attachment.id)));
                let tokens = this
                    .composer_images
                    .iter()
                    .map(|attachment| {
                        (
                            composer_image_token(attachment.id),
                            attachment.image.clone(),
                        )
                    })
                    .collect();
                editor.update(cx, |editor, cx| {
                    editor.set_inline_image_tokens(tokens, window, cx)
                });
                cx.notify();
            },
        )
        .detach();
        cx.subscribe_in(
            &composer,
            window,
            |this, _, _: &LocalEditorSubmitted, window, cx| this.send(window, cx),
        )
        .detach();
        cx.subscribe_in(
            &composer,
            window,
            |this, _, _: &LocalEditorSteered, window, cx| this.steer(window, cx),
        )
        .detach();
        cx.subscribe(&composer, |this, _, event: &LocalEditorImageClicked, cx| {
            this.expanded_user_image = Some(event.image.clone().into());
            cx.notify();
        })
        .detach();
        cx.subscribe_in(
            &search_editor,
            window,
            |this, editor, _: &LocalEditorChanged, window, cx| match this.vim_command_line {
                Some(VimCommandLine::Search { backwards })
                    if this.search_visible && this.search_returns_to_buffer =>
                {
                    let query = editor.read(cx).text(cx);
                    this.transcript_editor.update(cx, |editor, cx| {
                        editor.preview_search(&query, backwards, window, cx);
                    });
                    this.sync_native_search_state(cx);
                    cx.notify();
                }
                Some(VimCommandLine::Ex) if this.command_line_error.take().is_some() => {
                    cx.notify();
                }
                _ => {}
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
        if replay_count.is_some() && replay_streaming() {
            if let Some(agent) = model
                .items
                .iter_mut()
                .rev()
                .find(|item| item.kind == model::TranscriptKind::Agent)
            {
                agent.status = Some("streaming".into());
            }
            model.current_turn_id = Some("fixture-streaming-turn".into());
        }
        let (available_models, permission_profiles) = if replay_count.is_some() {
            model
                .telemetry
                .set_thread_settings(model::ThreadSettingsSnapshot {
                    model: Some("gpt-5.6-sol".into()),
                    effort: Some("xhigh".into()),
                    active_permission_profile: Some(model::ActivePermissionProfileSnapshot {
                        id: Some("danger-full-access".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            model.telemetry.token_usage = Some(json!({
                "tokenUsage": {
                    "last": { "totalTokens": 91_200 },
                    "modelContextWindow": 256_000
                }
            }));
            (
                vec![ModelChoice {
                    id: "gpt-5.6-sol".into(),
                    model: "gpt-5.6-sol".into(),
                    display_name: "GPT-5.6-Sol".into(),
                    default_effort: "xhigh".into(),
                    efforts: vec!["medium".into(), "high".into(), "xhigh".into()],
                    is_default: true,
                }],
                vec![PermissionProfileChoice {
                    id: "danger-full-access".into(),
                    description: Some("Read and write anywhere without approval prompts".into()),
                    allowed: true,
                }],
            )
        } else {
            (Vec::new(), Vec::new())
        };
        mark_unbacked_requests_inactive(&mut model, &HashSet::default());
        let dirty_image_surfaces = model
            .items
            .iter()
            .filter(|item| {
                item.kind == model::TranscriptKind::Image
                    || (item.kind == model::TranscriptKind::User
                        && !model.user_image_sources(&item.key).is_empty())
            })
            .map(|item| item.key.clone())
            .collect();
        // The list measures one boundary sentinel just outside the viewport,
        // so variable-height rows remain discoverable when leaving a followed
        // tail without laying out an additional partial viewport every frame.
        let list_state = ListState::new(model.items.len(), ListAlignment::Top, px(0.));
        list_state.set_diagnostics_name("transcript");
        list_state.set_follow_mode(FollowMode::Tail);
        let task_list_state = UniformListScrollHandle::new();
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
            child_threads: ChildThreadRegistry::default(),
            sidebar_threads: Vec::new(),
            available_models,
            permission_profiles,
            model_menu_handle: PopoverMenuHandle::default(),
            permission_menu_handle: PopoverMenuHandle::default(),
            settings_update_pending: false,
            appearance_settings_open: false,
            appearance_settings_section: AppearanceSettingsSection::Themes,
            appearance_font_role: AppearanceFontRole::Reading,
            appearance_scroll_handle: ScrollHandle::new(),
            theme_catalog_filter,
            theme_catalog: Vec::new(),
            theme_catalog_loading: false,
            theme_catalog_error: None,
            theme_catalog_visible: THEME_CATALOG_PAGE_SIZE,
            theme_packs_installing: HashSet::new(),
            installed_theme_packs: theme_sources::installed_harness_theme_packs(),
            thread_snapshots: ThreadSnapshotCache::default(),
            selected_thread_id: initial_thread_id,
            loaded_thread_updated_at: None,
            connecting: false,
            loading_thread: false,
            attaching_thread: false,
            thread_read_only_reason: None,
            error: None,
            transient_turn_status: None,
            selected_item: model.items.len().saturating_sub(1),
            model,
            composer,
            composer_images: Vec::new(),
            next_composer_image_id: 0,
            composer_attachment_error: None,
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
            vim_command_line: None,
            command_line_error: None,
            search_highlights_visible: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            active_search_match: 0,
            search_match_count: 0,
            active_search_item: None,
            active_search_body_offset: None,
            search_navigation_generation: 0,
            search_returns_to_buffer: false,
            search_return_focus: FocusMode::Transcript,
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
            user_image_previews: HashMap::default(),
            expanded_user_image: None,
            hybrid_surfaces: HashMap::default(),
            rich_nested_scrolls: HashMap::default(),
            sidebar_open: true,
            sidebar_user_override: false,
            turn_start_pending: false,
            queue_start_pending: false,
            queue_refresh_pending: false,
            turn_start_generation: 0,
            queue_start_generation: 0,
            queue_refresh_generation: 0,
            queue_operations: HashMap::new(),
            queued_turns: VecDeque::new(),
            server_task: Task::ready(()),
            turn_task: Task::ready(()),
            thread_list_task: Task::ready(()),
            thread_open_task: Task::ready(()),
            child_hierarchy_task: Task::ready(()),
            child_hierarchy_generation: 0,
            deferred_server_requests: Vec::new(),
            background_parent_thread_id: None,
            preserved_work_thread_id: None,
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
        if automatic_performance_j() {
            cx.spawn_in(window, async move |this, cx| {
                // The compositor's keyboard-enter event can arrive after the
                // first application defer. Wait for it so the
                // benchmark is not accidentally measured through GPUI's
                // deliberate 30 Hz inactive-window throttle.
                let mut active = false;
                for _ in 0..40 {
                    active = this
                        .update_in(cx, |_, window, _| window.is_window_active())
                        .unwrap_or(false);
                    if active {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                }
                if !active {
                    eprintln!("cancelled automatic Harness :perf-j run: window stayed inactive");
                    return;
                }
                _ = this.update_in(cx, |this, window, cx| {
                    this.run_performance_j(window, cx);
                });
            })
            .detach();
        }
        if automatic_performance_scroll() {
            cx.spawn_in(window, async move |this, cx| {
                let mut active = false;
                for _ in 0..40 {
                    active = this
                        .update_in(cx, |_, window, _| window.is_window_active())
                        .unwrap_or(false);
                    if active {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                }
                if !active {
                    eprintln!(
                        "cancelled automatic Harness :perf-scroll run: window stayed inactive"
                    );
                    return;
                }

                if this
                    .update_in(cx, |this, _, cx| {
                        this.list_state.pause_following_tail();
                        this.list_state.scroll_to(gpui::ListOffset {
                            item_ix: 0,
                            offset_in_item: px(0.),
                        });
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor()
                    // Let startup scrollbar reveal/hide animation finish before
                    // measuring. Otherwise an unrelated sidebar fade adds
                    // roughly 48 full-window draws to a three-second scroll.
                    .timer(PERFORMANCE_SCROLL_SETTLE_DURATION)
                    .await;

                let position = match this.update_in(cx, |this, window, _| {
                    this.performance_reporter
                        .mark_baseline_as(window, ":perf-scroll baseline");
                    let viewport = this.list_state.viewport_bounds();
                    point(viewport.origin.x + px(2.), viewport.center().y)
                }) {
                    Ok(position) => position,
                    Err(_) => return,
                };

                let started_at = Instant::now();
                for step in 0..PERFORMANCE_SCROLL_STEPS {
                    let phase = if step == 0 {
                        TouchPhase::Started
                    } else {
                        TouchPhase::Moved
                    };
                    if this
                        .update_in(cx, |_, window, _| {
                            window.enqueue_platform_input_for_diagnostics(
                                PlatformInput::ScrollWheel(ScrollWheelEvent {
                                    position,
                                    event_time: Some(Instant::now()),
                                    delta: ScrollDelta::Pixels(point(px(0.), px(-8.))),
                                    modifiers: Modifiers::default(),
                                    touch_phase: phase,
                                    synthesize_momentum: false,
                                }),
                            );
                        })
                        .is_err()
                    {
                        return;
                    }
                    let deadline = started_at + PERFORMANCE_SCROLL_INTERVAL * u32::from(step + 1);
                    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                        cx.background_executor().timer(remaining).await;
                    }
                }
                _ = this.update_in(cx, |_, window, _| {
                    window.enqueue_platform_input_for_diagnostics(PlatformInput::ScrollWheel(
                        ScrollWheelEvent {
                            position,
                            event_time: Some(Instant::now()),
                            delta: ScrollDelta::Pixels(point(px(0.), px(0.))),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Ended,
                            synthesize_momentum: false,
                        },
                    ));
                });
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                _ = this.update_in(cx, |this, window, _| {
                    match this.performance_reporter.snapshot_benchmark_report(window) {
                        Ok(report) => {
                            eprintln!("completed Harness :perf-scroll run\n{report}")
                        }
                        Err(error) => {
                            eprintln!("failed automatic Harness :perf-scroll report: {error:#}")
                        }
                    }
                });
            })
            .detach();
        }
        if replay_count.is_none() {
            this.connect(cx);
        }
        this
    }

    fn sidebar_thread(&self, row: &SidebarThreadRow) -> Option<&CodexThread> {
        row.root_index
            .and_then(|index| self.threads.get(index))
            .or_else(|| self.child_threads.get(&row.thread_id))
    }

    fn sidebar_thread_by_id(&self, thread_id: &str) -> Option<&CodexThread> {
        self.threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .or_else(|| self.child_threads.get(thread_id))
    }

    fn rebuild_sidebar_threads(&mut self, preferred_thread_id: Option<&str>) {
        self.sidebar_threads = sidebar_thread_rows(&self.threads, &self.child_threads);
        self.selected_task = sidebar_selection_index(
            &self.sidebar_threads,
            preferred_thread_id.or(self.selected_thread_id.as_deref()),
            self.selected_task,
        );
    }

    fn selected_sidebar_thread_id(&self) -> Option<String> {
        self.sidebar_threads
            .get(self.selected_task)
            .map(|row| row.thread_id.clone())
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
                let child_threads = match client
                    .list_spawned_subagent_threads(THREAD_LIMIT, None)
                    .await
                {
                    Ok(response) => response.data,
                    Err(error) => {
                        log::debug!("could not list spawned child threads: {error}");
                        Vec::new()
                    }
                };
                anyhow::Ok((client, threads, child_threads))
            }
            .await;

            let client = match result {
                Ok((client, threads, child_threads)) => {
                    if this
                        .update(cx, |this, cx| {
                            this.client = Some(client.clone());
                            this.threads = threads;
                            this.child_threads.reconcile(child_threads);
                            this.rebuild_sidebar_threads(None);
                            this.connecting = false;
                            this.reconnect_attempts = 0;
                            this.error = None;
                            this.load_server_options(cx);
                            if let Some(query) = initial_thread_query.as_deref() {
                                let query = query.to_lowercase();
                                let thread_id = this
                                    .sidebar_threads
                                    .iter()
                                    .filter_map(|row| this.sidebar_thread(row))
                                    .find(|thread| {
                                        thread_title(thread).to_lowercase().contains(&query)
                                            || thread.id.eq_ignore_ascii_case(&query)
                                    })
                                    .map(|thread| thread.id.clone());
                                if let Some(thread_id) = thread_id {
                                    this.open_thread_by_id(&thread_id, cx);
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
        self.thread_list_task = Task::ready(());
        self.thread_open_task = Task::ready(());
        self.child_hierarchy_task = Task::ready(());
        self.child_hierarchy_generation = self.child_hierarchy_generation.wrapping_add(1);
        self.read_only_refresh_task = Task::ready(());
        self.turn_task = Task::ready(());
        self.turn_start_pending = false;
        self.queue_start_pending = false;
        self.queue_refresh_pending = false;
        self.turn_start_generation = self.turn_start_generation.wrapping_add(1);
        self.queue_start_generation = self.queue_start_generation.wrapping_add(1);
        self.queue_refresh_generation = self.queue_refresh_generation.wrapping_add(1);
        self.queue_operations.clear();
        self.settings_update_pending = false;
        self.queued_turns.clear();
        self.model.current_turn_id = None;
        self.transient_turn_status = None;
        self.thread_read_only_reason = None;
        self.deferred_server_requests.clear();
        self.background_parent_thread_id = None;
        self.preserved_work_thread_id = None;
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
        let refresh_child_hierarchy = events.iter().any(event_refreshes_child_hierarchy);
        let queue_changed = events.iter().any(|event| {
            matches!(
                event,
                AppServerEvent::Notification { method, params }
                    if method == "thread/queue/changed"
                        && params.get("threadId").and_then(Value::as_str)
                            == self.selected_thread_id.as_deref()
            )
        });
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
        if self.model.current_turn_id.is_some()
            && queue_state_is_visible(
                self.selected_thread_id.as_deref(),
                self.preserved_work_thread_id.as_deref(),
            )
        {
            self.turn_start_pending = false;
        }
        let completed_turn = lifecycle_ended_active_turn(&outcome.turn_lifecycle);
        let new_len = self.model.items.len();
        let mut dirty_items = outcome.dirty.into_iter().collect::<Vec<_>>();
        dirty_items.sort_unstable();
        dirty_items.dedup();
        let document_changed = new_len != old_len || !dirty_items.is_empty();
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
        if let Some(update) = outcome.transient_turn_status {
            self.transient_turn_status = match update {
                model::TransientTurnStatusUpdate::Set(status) => Some(status.into()),
                model::TransientTurnStatusUpdate::Clear => None,
            };
        }
        self.track_live_request_updates(&live_request_ids, old_len, new_len, &dirty_items);
        self.track_image_surface_updates(old_len, new_len, &dirty_items);
        if outcome.refresh_threads {
            if let Some(thread_id) = self.selected_thread_id.clone() {
                self.persist_transcript_in_background(&thread_id, cx);
            }
            self.refresh_threads(cx);
        } else if refresh_child_hierarchy {
            self.schedule_child_hierarchy_refresh(cx);
        }
        if !self.search_query.is_empty() && !self.search_returns_to_buffer {
            self.update_search_matches_for_changes(old_len, &dirty_items);
        }
        if document_changed && (self.buffer_view || rich_vim_experiment()) {
            let incrementally_applied =
                self.sync_transcript_item_updates(old_len, &dirty_items, cx);
            if !incrementally_applied {
                drop(self.sync_transcript_document(cx));
            }
        }
        if completed_turn {
            self.start_next_queued_turn(cx);
        }
        if queue_changed {
            self.refresh_queued_turns(cx);
        }
        cx.notify();
    }

    fn dispatch_server_requests(
        &mut self,
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
            match route_server_request_with_background(
                &method,
                &params,
                self.selected_thread_id.as_deref(),
                self.background_parent_thread_id.as_deref(),
            ) {
                RequestRoute::Interactive => {
                    let request = AppServerEvent::ServerRequest { id, method, params };
                    if self.loading_thread {
                        self.deferred_server_requests.push(request);
                    } else {
                        let AppServerEvent::ServerRequest { id, .. } = &request else {
                            unreachable!();
                        };
                        live_request_ids.push(id.clone());
                        forwarded.push(request);
                    }
                }
                RequestRoute::ReturnToThread(thread_id) => {
                    self.deferred_server_requests
                        .push(AppServerEvent::ServerRequest { id, method, params });
                    self.open_thread_by_id(&thread_id, cx);
                }
                RequestRoute::Immediate(reply) => {
                    self.send_immediate_request_reply(id, method, reply, cx);
                }
            }
        }
        (forwarded, live_request_ids)
    }

    fn replay_deferred_requests_for_selected(&mut self, cx: &mut Context<Self>) {
        let Some(selected_thread_id) = self.selected_thread_id.clone() else {
            return;
        };
        let mut matching = Vec::new();
        let mut remaining = Vec::new();
        for event in std::mem::take(&mut self.deferred_server_requests) {
            let matches_selected = matches!(
                &event,
                AppServerEvent::ServerRequest { params, .. }
                    if request_matches_thread(params, Some(&selected_thread_id))
            );
            if matches_selected {
                matching.push(event);
            } else {
                remaining.push(event);
            }
        }
        self.deferred_server_requests = remaining;
        if !matching.is_empty() {
            self.apply_event_batch(matching, cx);
        }
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
                || (item.kind == model::TranscriptKind::User
                    && !self.model.user_image_sources(&item.key).is_empty())
                || self.image_surfaces.contains_key(&item.key)
                || self.user_image_previews.contains_key(&item.key)
            {
                self.dirty_image_surfaces.insert(item.key.clone());
            }
        }
    }

    fn mark_all_image_surfaces_dirty(&mut self) {
        let keys = image_surface_keys_to_sync(
            self.image_surfaces
                .keys()
                .chain(self.user_image_previews.keys())
                .cloned(),
            self.model
                .items
                .iter()
                .filter(|item| {
                    item.kind == model::TranscriptKind::Image
                        || (item.kind == model::TranscriptKind::User
                            && !self.model.user_image_sources(&item.key).is_empty())
                })
                .map(|item| item.key.clone()),
        );
        self.dirty_image_surfaces.extend(keys);
    }

    fn sync_image_surfaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_user_image_keys = self
            .model
            .items
            .iter()
            .filter(|item| {
                item.kind == model::TranscriptKind::User
                    && !self.model.user_image_sources(&item.key).is_empty()
            })
            .map(|item| item.key.as_str())
            .collect::<HashSet<_>>();
        self.user_image_previews
            .retain(|key, _| active_user_image_keys.contains(key.as_str()));

        let dirty = std::mem::take(&mut self.dirty_image_surfaces);
        for item_key in dirty {
            let item = self.model.items.iter().find(|item| item.key == item_key);
            let item_is_user = item.is_some_and(|item| item.kind == model::TranscriptKind::User);
            if item_is_user {
                let previews = self
                    .model
                    .user_image_sources(&item_key)
                    .iter()
                    .filter_map(transcript_user_image_source)
                    .collect::<Vec<_>>();
                if previews.is_empty() {
                    self.user_image_previews.remove(&item_key);
                } else {
                    self.user_image_previews.insert(item_key.clone(), previews);
                }
            } else {
                self.user_image_previews.remove(&item_key);
            }

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
        // Rich mode does not use editor replacement surfaces. Its normal
        // scroll-frame render path should not scan the entire transcript just
        // to rediscover that there is nothing to synchronize.
        if !self.buffer_view && self.hybrid_surfaces.is_empty() {
            return;
        }
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
        if dirty.is_empty() {
            return;
        }
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
        self.thread_list_task = cx.spawn(async move |this, cx| {
            let roots = client.list_threads(THREAD_LIMIT, None).await;
            if this
                .update(cx, |this, cx| {
                    this.connecting = false;
                    let preferred_thread_id = this.selected_sidebar_thread_id();
                    match roots {
                        Ok(response) => {
                            this.threads = response.data;
                            this.error = None;
                            this.rebuild_sidebar_threads(preferred_thread_id.as_deref());
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
        self.schedule_child_hierarchy_refresh(cx);
    }

    fn schedule_child_hierarchy_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.child_hierarchy_generation = self.child_hierarchy_generation.wrapping_add(1);
        let generation = self.child_hierarchy_generation;
        self.child_hierarchy_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(CHILD_HIERARCHY_REFRESH_DEBOUNCE)
                .await;
            let Ok(still_current) = this.update(cx, |this, _| {
                this.child_hierarchy_generation == generation
                    && this
                        .client
                        .as_ref()
                        .is_some_and(|current| Rc::ptr_eq(current, &client))
            }) else {
                return;
            };
            if !still_current {
                return;
            }
            let children = client
                .list_spawned_subagent_threads(THREAD_LIMIT, None)
                .await;
            _ = this.update(cx, |this, cx| {
                if this.child_hierarchy_generation != generation
                    || !this
                        .client
                        .as_ref()
                        .is_some_and(|current| Rc::ptr_eq(current, &client))
                {
                    return;
                }
                match children {
                    Ok(response) => {
                        let preferred_thread_id = this.selected_sidebar_thread_id();
                        if this.child_threads.reconcile(response.data) {
                            this.rebuild_sidebar_threads(preferred_thread_id.as_deref());
                            cx.notify();
                        }
                    }
                    Err(error) => {
                        log::debug!("could not refresh spawned child threads: {error}");
                    }
                }
            });
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

                let Ok((still_selected, loaded_updated_at, selected_is_child)) =
                    this.update(cx, |this, _| {
                        (
                            this.selected_thread_id.as_deref() == Some(thread_id.as_str())
                                && this.thread_read_only_reason.is_some()
                                && this
                                    .client
                                    .as_ref()
                                    .is_some_and(|current| Rc::ptr_eq(current, &client)),
                            this.loaded_thread_updated_at,
                            this.child_threads.get(&thread_id).is_some(),
                        )
                    })
                else {
                    return;
                };
                if !still_selected {
                    return;
                }

                if !active && !selected_is_child {
                    let root_response = match client.list_threads(THREAD_LIMIT, None).await {
                        Ok(response) => response,
                        Err(error) => {
                            log::debug!("could not check read-only task freshness: {error}");
                            continue;
                        }
                    };
                    let selected_updated_at = root_response
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
                        let selected_task_id = this.selected_sidebar_thread_id();
                        let roots_changed = this.threads != root_response.data;
                        if roots_changed {
                            this.threads = root_response.data;
                        }
                        if roots_changed {
                            this.rebuild_sidebar_threads(selected_task_id.as_deref());
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
                        this.apply_read_only_thread_refresh(&thread, cx);
                        this.thread_snapshots.insert(thread);
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    fn apply_read_only_thread_refresh(&mut self, thread: &CodexThread, cx: &mut Context<Self>) {
        if self.selected_thread_id.as_deref() != Some(thread.id.as_str())
            || self.thread_read_only_reason.is_none()
        {
            return;
        }

        self.apply_loaded_thread_refresh(thread, cx);
    }

    fn apply_loaded_thread_refresh(&mut self, thread: &CodexThread, cx: &mut Context<Self>) {
        if self.selected_thread_id.as_deref() != Some(thread.id.as_str()) {
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
        self.model.current_turn_id = active_thread_turn_id(thread).map(ToOwned::to_owned);
        let mut dirty_items = outcome.dirty.into_iter().collect::<Vec<_>>();
        dirty_items.sort_unstable();
        dirty_items.dedup();
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
        if !self.search_query.is_empty() && !self.search_returns_to_buffer {
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
        let Some(thread_id) = self
            .sidebar_threads
            .get(index)
            .map(|row| row.thread_id.clone())
        else {
            return;
        };
        self.open_thread_by_id(&thread_id, cx);
    }

    fn has_unresolved_live_request(&self) -> bool {
        self.model.items.iter().any(|item| {
            self.live_request_keys.contains(&item.key)
                && item
                    .pending_request
                    .as_ref()
                    .is_some_and(|request| !request.resolved)
        })
    }

    fn complete_thread_open_request_routing(
        &mut self,
        authoritative_turn_state_loaded: bool,
        cx: &mut Context<Self>,
    ) {
        if self.background_parent_thread_id.as_deref() == self.selected_thread_id.as_deref() {
            self.background_parent_thread_id = None;
        }
        if self.preserved_work_thread_id.as_deref() == self.selected_thread_id.as_deref() {
            self.preserved_work_thread_id = None;
        }
        self.replay_deferred_requests_for_selected(cx);
        if authoritative_turn_state_loaded {
            self.start_next_queued_turn(cx);
        }
    }

    fn open_thread_by_id(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(thread) = self.sidebar_thread_by_id(thread_id).cloned() else {
            return;
        };
        let thread_id = thread.id.clone();
        let is_child = self.child_threads.get(&thread_id).is_some();
        let can_accept_direct_input = thread.can_accept_direct_input.unwrap_or(true);
        let observational_child = is_child && !can_accept_direct_input;
        if child_inspection_blocked(observational_child, self.has_unresolved_live_request()) {
            self.error =
                Some("Answer the open request before inspecting this read-only child task.".into());
            cx.notify();
            return;
        }
        if self.selected_thread_id.as_deref() == Some(thread_id.as_str())
            && (self.loading_thread || !self.model.items.is_empty())
        {
            return;
        }
        let returning_to_background =
            self.background_parent_thread_id.as_deref() == Some(thread_id.as_str());
        let returning_to_preserved_work =
            self.preserved_work_thread_id.as_deref() == Some(thread_id.as_str());
        let leaving_child_with_live_request = returning_to_background
            && self
                .selected_thread_id
                .as_deref()
                .is_some_and(|selected| self.child_threads.get(selected).is_some())
            && self.has_unresolved_live_request();
        if is_child {
            let selected_root = self.selected_thread_id.as_ref().filter(|selected| {
                selected.as_str() != thread_id && self.child_threads.get(selected).is_none()
            });
            self.background_parent_thread_id = selected_root
                .cloned()
                .or_else(|| self.background_parent_thread_id.clone())
                .or_else(|| thread.effective_parent_thread_id().map(ToOwned::to_owned));
            if observational_child {
                self.preserved_work_thread_id = self.background_parent_thread_id.clone();
            } else {
                self.preserved_work_thread_id = None;
            }
        } else if !returning_to_background {
            self.background_parent_thread_id = None;
            self.preserved_work_thread_id = None;
        }
        let preserve_background_work = observational_child || returning_to_preserved_work;
        let cached_thread = self.thread_snapshots.take(&thread_id);
        let load_started_at = thread_load_diagnostics_enabled().then(Instant::now);
        let Some(client) = self.client.clone() else {
            return;
        };

        if reject_pending_requests_on_switch(
            preserve_background_work,
            leaving_child_with_live_request,
        ) {
            self.reject_pending_requests(cx);
        }
        self.read_only_refresh_task = Task::ready(());
        if !preserve_background_work {
            self.turn_task = Task::ready(());
            self.turn_start_pending = false;
            self.queue_start_pending = false;
            self.queue_refresh_pending = false;
            self.turn_start_generation = self.turn_start_generation.wrapping_add(1);
            self.queue_start_generation = self.queue_start_generation.wrapping_add(1);
            self.queue_refresh_generation = self.queue_refresh_generation.wrapping_add(1);
            self.queue_operations.clear();
            self.queued_turns.clear();
        }
        self.selected_thread_id = Some(thread_id.clone());
        self.loaded_thread_updated_at = None;
        self.loading_thread = true;
        self.attaching_thread = can_accept_direct_input;
        self.settings_update_pending = false;
        self.thread_read_only_reason = (!can_accept_direct_input)
            .then(|| "This child task does not accept direct input.".into());
        self.transient_turn_status = None;
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
        let mut had_local_content = false;
        if let Some(cached_thread) = cached_thread {
            self.load_thread(&cached_thread, cx);
            self.thread_snapshots.insert(cached_thread);
            had_local_content = true;
            if thread_load_diagnostics_enabled() {
                eprintln!("thread-load cache-hit thread={thread_id}");
            }
        } else {
            match self.model.restore_persisted_transcript(&thread_id) {
                Ok(restored) if restored > 0 => {
                    mark_unbacked_requests_inactive(&mut self.model, &self.live_request_keys);
                    self.mark_all_image_surfaces_dirty();
                    self.list_state.splice(0..0, restored);
                    self.selected_item = restored.saturating_sub(1);
                    self.list_state.set_follow_mode(FollowMode::Tail);
                    drop(self.sync_transcript_document(cx));
                    had_local_content = true;
                    if thread_load_diagnostics_enabled() {
                        eprintln!(
                            "thread-load disk-cache-hit thread={thread_id} items={restored} total_ms={:.1}",
                            load_started_at
                                .map(|started_at| started_at.elapsed().as_secs_f64() * 1_000.)
                                .unwrap_or_default(),
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("could not restore cached task {thread_id}: {error}"),
            }
        }
        cx.notify();
        if !observational_child {
            self.refresh_queued_turns(cx);
        }

        self.thread_open_task = cx.spawn(async move |this, cx| {
            if !can_accept_direct_input {
                match client.read_thread(&thread_id).await {
                    Ok(thread) => {
                        let active = thread_has_active_turn(&thread);
                        this.update(cx, |this, cx| {
                            if this.selected_thread_id.as_deref() != Some(thread_id.as_str()) {
                                return;
                            }
                            if had_local_content {
                                this.apply_loaded_thread_refresh(&thread, cx);
                            } else {
                                this.load_thread(&thread, cx);
                            }
                            this.thread_snapshots.insert(thread);
                            this.loading_thread = false;
                            this.attaching_thread = false;
                            this.thread_read_only_reason = Some(
                                "This child task does not accept direct input.".into(),
                            );
                            this.schedule_read_only_refresh(active, cx);
                            this.error = None;
                            this.complete_thread_open_request_routing(true, cx);
                            cx.notify();
                        })
                        .ok();
                    }
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            if this.selected_thread_id.as_deref() != Some(thread_id.as_str()) {
                                return;
                            }
                            this.loading_thread = false;
                            this.attaching_thread = false;
                            this.thread_read_only_reason = Some(
                                "This child task does not accept direct input.".into(),
                            );
                            this.error = Some(
                                format!("Could not open child task history: {error}").into(),
                            );
                            this.complete_thread_open_request_routing(false, cx);
                            cx.notify();
                        })
                        .ok();
                    }
                }
                return;
            }

            // `thread/resume` already returns every turn as well as attaching
            // this connection to future events. Reading first duplicated the
            // complete history transfer on every successful open.
            let resume_started_at = Instant::now();
            let resume = client.resume_thread_with_settings(&thread_id).await;
            let resume_elapsed = resume_started_at.elapsed();
            if resume_elapsed >= Duration::from_millis(250) {
                log::info!(
                    "slow task history response thread={thread_id} elapsed_ms={:.1} local_history_visible={had_local_content}",
                    resume_elapsed.as_secs_f64() * 1_000.,
                );
            }
            if thread_load_diagnostics_enabled() {
                eprintln!(
                    "thread-load resume thread={thread_id} elapsed_ms={:.1} success={}",
                    resume_elapsed.as_secs_f64() * 1_000.,
                    resume.is_ok(),
                );
            }
            let resumed = match resume {
                Ok(response) => response,
                Err(resume_error) => {
                    log::warn!(
                        "could not resume task {thread_id}; trying read-only: {resume_error}"
                    );
                    let read_started_at = Instant::now();
                    let read = client.read_thread(&thread_id).await;
                    if thread_load_diagnostics_enabled() {
                        eprintln!(
                            "thread-load read-fallback thread={thread_id} elapsed_ms={:.1} success={}",
                            read_started_at.elapsed().as_secs_f64() * 1_000.,
                            read.is_ok(),
                        );
                    }
                    match read {
                        Ok(thread) => {
                            let active = thread_has_active_turn(&thread);
                            this.update(cx, |this, cx| {
                                if this.selected_thread_id.as_deref() != Some(thread_id.as_str()) {
                                    return;
                                }
                                if had_local_content {
                                    this.apply_loaded_thread_refresh(&thread, cx);
                                } else {
                                    this.load_thread(&thread, cx);
                                }
                                this.thread_snapshots.insert(thread);
                                this.loading_thread = false;
                                this.attaching_thread = false;
                                this.thread_read_only_reason = Some(
                                    "Could not attach live. History is available read-only."
                                        .into(),
                                );
                                this.schedule_read_only_refresh(active, cx);
                                this.error = None;
                                this.complete_thread_open_request_routing(true, cx);
                                cx.notify();
                            })
                            .ok();
                        }
                        Err(read_error) => {
                            this.update(cx, |this, cx| {
                                if this.selected_thread_id.as_deref() != Some(thread_id.as_str()) {
                                    return;
                                }
                                this.loading_thread = false;
                                this.attaching_thread = false;
                                this.thread_read_only_reason = Some(if had_local_content {
                                    "Could not refresh or attach live. Cached history is read-only."
                                        .into()
                                } else {
                                    "This task could not be loaded. Choose another task or start a new one."
                                        .into()
                                });
                                this.error = Some(
                                    format!(
                                        "Could not open task: {read_error} (resume failed: {resume_error})"
                                    )
                                    .into(),
                                );
                                this.complete_thread_open_request_routing(false, cx);
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                    return;
                }
            };

            let resumed_thread = resumed.thread.clone();
            this.update(cx, |this, cx| {
                let update_started_at = Instant::now();
                if this.selected_thread_id.as_deref() != Some(thread_id.as_str()) {
                    return;
                }
                if had_local_content {
                    this.apply_loaded_thread_refresh(&resumed_thread, cx);
                } else {
                    this.load_thread(&resumed_thread, cx);
                }
                this.apply_thread_open_settings(&resumed);
                this.load_server_options(cx);
                this.thread_snapshots.insert(resumed_thread);
                this.loading_thread = false;
                this.attaching_thread = false;
                this.thread_read_only_reason = None;
                this.error = None;
                this.complete_thread_open_request_routing(true, cx);
                cx.notify();
                if thread_load_diagnostics_enabled() {
                    eprintln!(
                        "thread-load foreground thread={thread_id} elapsed_ms={:.1} total_ms={:.1}",
                        update_started_at.elapsed().as_secs_f64() * 1_000.,
                        load_started_at
                            .map(|started_at| started_at.elapsed().as_secs_f64() * 1_000.)
                            .unwrap_or_default(),
                    );
                }
            })
            .ok();
        });
    }

    fn load_thread(&mut self, thread: &CodexThread, cx: &mut Context<Self>) {
        let load_started_at = Instant::now();
        let old_len = self.model.items.len();
        self.selected_thread_id = Some(thread.id.clone());
        self.loaded_thread_updated_at = Some(thread.updated_at);
        if !thread.cwd.is_empty() {
            self.cwd = thread.cwd.clone();
        }
        let model_started_at = Instant::now();
        self.model.load_thread(thread);
        self.model.current_turn_id = active_thread_turn_id(thread).map(ToOwned::to_owned);
        let model_elapsed = model_started_at.elapsed();
        let persisted_started_at = Instant::now();
        match self.model.merge_persisted_transcript(&thread.id) {
            Ok(restored) if restored > 0 => {
                log::info!("restored {restored} live-only transcript items")
            }
            Ok(_) => {}
            Err(error) => log::warn!("could not restore transcript history: {error}"),
        }
        let persisted_elapsed = persisted_started_at.elapsed();
        mark_unbacked_requests_inactive(&mut self.model, &self.live_request_keys);
        if thread_load_diagnostics_enabled() {
            let content_bytes = self
                .model
                .items
                .iter()
                .map(|item| item.content.len())
                .sum::<usize>();
            let largest_item_bytes = self
                .model
                .items
                .iter()
                .map(|item| item.content.len())
                .max()
                .unwrap_or_default();
            eprintln!(
                "transcript-items count={} content_bytes={content_bytes} largest_item_bytes={largest_item_bytes}",
                self.model.items.len(),
            );
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
        let document_started_at = Instant::now();
        drop(self.sync_transcript_document(cx));
        let document_elapsed = document_started_at.elapsed();
        if self.buffer_view {
            self.transcript_editor
                .update(cx, |editor, cx| editor.reveal_tail(cx));
        }
        cx.notify();
        let cache_started_at = Instant::now();
        self.persist_transcript_in_background(&thread.id, cx);
        let cache_elapsed = cache_started_at.elapsed();
        let total_elapsed = load_started_at.elapsed();
        if total_elapsed >= Duration::from_millis(100) {
            log::info!(
                "slow task history projection thread={} items={} model_ms={:.1} merge_ms={:.1} document_ms={:.1} cache_prepare_ms={:.1} total_ms={:.1}",
                thread.id,
                self.model.items.len(),
                model_elapsed.as_secs_f64() * 1_000.,
                persisted_elapsed.as_secs_f64() * 1_000.,
                document_elapsed.as_secs_f64() * 1_000.,
                cache_elapsed.as_secs_f64() * 1_000.,
                total_elapsed.as_secs_f64() * 1_000.,
            );
        }
        if thread_load_diagnostics_enabled() {
            eprintln!(
                "thread-load model thread={} items={} model_ms={:.1} merge_ms={:.1} document_ms={:.1} cache_prepare_ms={:.1} total_ms={:.1}",
                thread.id,
                self.model.items.len(),
                model_elapsed.as_secs_f64() * 1_000.,
                persisted_elapsed.as_secs_f64() * 1_000.,
                document_elapsed.as_secs_f64() * 1_000.,
                cache_elapsed.as_secs_f64() * 1_000.,
                total_elapsed.as_secs_f64() * 1_000.,
            );
        }
    }

    fn new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reject_pending_requests(cx);
        self.thread_open_task = Task::ready(());
        self.read_only_refresh_task = Task::ready(());
        self.turn_task = Task::ready(());
        self.turn_start_pending = false;
        self.queue_start_pending = false;
        self.queue_refresh_pending = false;
        self.turn_start_generation = self.turn_start_generation.wrapping_add(1);
        self.queue_start_generation = self.queue_start_generation.wrapping_add(1);
        self.queue_refresh_generation = self.queue_refresh_generation.wrapping_add(1);
        self.queue_operations.clear();
        self.queued_turns.clear();
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
        self.loading_thread = false;
        self.attaching_thread = false;
        self.settings_update_pending = false;
        self.selected_item = 0;
        self.transcript_cursor_initialized = false;
        self.thread_read_only_reason = None;
        self.deferred_server_requests.clear();
        self.background_parent_thread_id = None;
        self.preserved_work_thread_id = None;
        self.error = None;
        self.list_state.set_follow_mode(FollowMode::Tail);
        drop(self.sync_transcript_document(cx));
        if self.buffer_view {
            self.transcript_editor
                .update(cx, |editor, cx| editor.reveal_tail(cx));
        }
        self.focus_composer(window, cx);
    }

    fn paste_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_mode != FocusMode::Composer {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        let mut added = 0;
        let mut error = None;
        let mut saw_image = false;
        let mut inserted_tokens = Vec::new();
        for entry in clipboard.entries() {
            let ClipboardEntry::Image(image) = entry else {
                continue;
            };
            saw_image = true;
            if image.bytes().is_empty() {
                continue;
            }
            if image.bytes().len() > MAX_COMPOSER_IMAGE_BYTES {
                error = Some("Pasted image is larger than 20 MB".into());
                continue;
            }
            if self.composer_images.len() >= MAX_COMPOSER_IMAGES {
                error = Some("A prompt can contain up to 8 pasted images".into());
                break;
            }

            self.next_composer_image_id = self.next_composer_image_id.wrapping_add(1);
            self.composer_images.push(ComposerImageAttachment {
                id: self.next_composer_image_id,
                image: Arc::new(image.clone()),
            });
            inserted_tokens.push(composer_image_token(self.next_composer_image_id));
            added += 1;
        }

        if saw_image {
            self.composer_attachment_error = error;
            if !inserted_tokens.is_empty() {
                let marker = gpui::ClipboardItem::new_string(inserted_tokens.join(" "));
                self.composer
                    .update(cx, |editor, cx| editor.paste_item(&marker, window, cx));
            }
            if added > 0 || self.composer_attachment_error.is_some() {
                cx.notify();
            }
        } else {
            self.composer
                .update(cx, |editor, cx| editor.paste_item(&clipboard, window, cx));
        }
    }

    fn take_composer_submission(
        &mut self,
        show_optimistically_in_transcript: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ComposerSubmission> {
        let text = self.composer.read(cx).text(cx);
        let text = text.trim().to_string();
        if composer_send_blocked(
            composer_is_empty(&text, self.composer_images.len()),
            self.loading_thread,
            self.attaching_thread,
            self.settings_update_pending,
            self.thread_read_only_reason.is_some(),
            self.client.is_some() || self.replay_count.is_some(),
        ) {
            return None;
        }
        let input = composer_app_server_input(&text, &self.composer_images);
        let preview = composer_prompt_preview(&input);
        let client_user_message_id = Uuid::new_v4().to_string();
        let key = show_optimistically_in_transcript.then(|| {
            let (index, key) = self
                .model
                .push_local_user(&client_user_message_id, preview, &input);
            self.dirty_image_surfaces.insert(key.clone());
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
            key
        });
        self.composer
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.composer_images.clear();
        self.composer_attachment_error = None;

        Some(ComposerSubmission {
            key,
            client_user_message_id,
            input: Value::Array(input),
        })
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Starting a brand-new task is the only moment at which no stable
        // thread id exists for the server-owned queue. Leave the composer
        // untouched until thread/start returns instead of losing a fast second
        // submission into a Harness-only queue.
        if self.turn_start_pending && self.selected_thread_id.is_none() {
            self.error = Some("Starting the task; send again when it is ready.".into());
            cx.notify();
            return;
        }
        let turn_active = self.turn_active();
        let Some(submission) = self.take_composer_submission(
            show_submission_optimistically_in_transcript(turn_active),
            window,
            cx,
        ) else {
            return;
        };

        if self.replay_count.is_some() {
            if let Some(key) = submission.key.as_deref() {
                self.model.set_status_for_key(key, "replay");
            }
            let index = self.model.items.len().saturating_sub(1);
            self.list_state.splice(index..index + 1, 1);
            cx.notify();
            return;
        }

        if turn_active {
            self.queue_submission(submission, cx);
        } else {
            self.start_submission(submission, cx);
        }
    }

    fn start_submission(&mut self, submission: ComposerSubmission, cx: &mut Context<Self>) {
        let Some(key) = submission.key.clone() else {
            self.error = Some("Could not prepare the prompt for sending".into());
            cx.notify();
            return;
        };
        let Some(client) = self.client.clone() else {
            self.model.set_status_for_key(&key, "not connected");
            self.error = Some("Codex is not connected yet".into());
            if let Some(index) = self.model.items.iter().position(|item| item.key == key) {
                self.list_state.splice(index..index + 1, 1);
            }
            cx.notify();
            return;
        };
        let ComposerSubmission {
            key: _,
            client_user_message_id,
            input,
        } = submission;
        let existing_thread_id = self.selected_thread_id.clone();
        let origin_thread_id = existing_thread_id.clone();
        let cwd = self.cwd.clone();
        self.transient_turn_status = None;
        self.turn_start_pending = true;
        self.turn_start_generation = self.turn_start_generation.wrapping_add(1);
        let generation = self.turn_start_generation;
        self.turn_task = cx.spawn(async move |this, cx| {
            let result = async {
                let (thread_id, opened_thread) = match existing_thread_id {
                    Some(thread_id) => (thread_id, None),
                    None => {
                        let opened = client.start_thread_with_settings(&cwd).await?;
                        (opened.thread.id.clone(), Some(opened))
                    }
                };
                let response = client
                    .start_turn_with_client_user_message_id(
                        &thread_id,
                        input,
                        Some(&client_user_message_id),
                    )
                    .await?;
                anyhow::Ok((thread_id, opened_thread, response))
            }
            .await;
            if this
                .update(cx, |this, cx| {
                    if this.turn_start_generation != generation {
                        return;
                    }
                    this.turn_start_pending = false;
                    let origin_is_visible = origin_thread_id.as_deref().map_or_else(
                        || this.selected_thread_id.is_none(),
                        |origin| {
                            callback_origin_is_visible(origin, this.selected_thread_id.as_deref())
                        },
                    );
                    if !origin_is_visible {
                        if result.is_ok() {
                            this.refresh_threads(cx);
                        }
                        return;
                    }
                    match result {
                        Ok((thread_id, opened_thread, response)) => {
                            this.selected_thread_id = Some(thread_id);
                            if let Some(opened_thread) = opened_thread.as_ref() {
                                this.apply_thread_open_settings(opened_thread);
                                this.load_server_options(cx);
                            }
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

    fn queue_submission(&mut self, submission: ComposerSubmission, cx: &mut Context<Self>) {
        let (Some(client), Some(thread_id)) =
            (self.client.clone(), self.selected_thread_id.clone())
        else {
            self.show_failed_queued_submission(&submission, cx);
            self.error = Some("The task is not ready to queue another prompt yet".into());
            cx.notify();
            return;
        };
        let ComposerSubmission {
            key: _,
            client_user_message_id,
            input,
        } = submission;
        self.queued_turns.push_back(QueuedTurnSubmission {
            id: None,
            client_user_message_id: client_user_message_id.clone(),
            preview_images: queued_submission_preview_images(&input),
            input: input.clone(),
        });
        cx.spawn(async move |this, cx| {
            let result = client
                .queue_turn(&thread_id, input.clone(), &client_user_message_id)
                .await;
            _ = this.update(cx, |this, cx| {
                if !queue_state_belongs_to_thread(
                    &thread_id,
                    this.selected_thread_id.as_deref(),
                    this.preserved_work_thread_id.as_deref(),
                ) {
                    return;
                }
                let origin_is_visible =
                    callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref());
                match result {
                    Ok(response) => {
                        if let Some(queued) = response
                            .get("queuedSubmission")
                            .and_then(queued_submission_from_value)
                        {
                            if let Some(index) = this.queued_turns.iter().position(|entry| {
                                entry.client_user_message_id == client_user_message_id
                            }) {
                                this.queued_turns[index] = queued;
                            } else {
                                this.queued_turns.push_back(queued);
                            }
                        } else if origin_is_visible {
                            this.refresh_queued_turns(cx);
                        }
                        if origin_is_visible {
                            this.error = None;
                        }
                        if origin_is_visible
                            && this.model.current_turn_id.is_none()
                            && !this.turn_start_pending
                        {
                            this.start_next_queued_turn(cx);
                        }
                    }
                    Err(error) => {
                        this.queued_turns
                            .retain(|entry| entry.client_user_message_id != client_user_message_id);
                        if origin_is_visible {
                            this.show_failed_queued_submission(
                                &ComposerSubmission {
                                    key: None,
                                    client_user_message_id: client_user_message_id.clone(),
                                    input: input.clone(),
                                },
                                cx,
                            );
                            this.error = Some(format!("Could not queue prompt: {error}").into());
                        } else {
                            log::warn!("could not queue background prompt: {error}");
                        }
                    }
                }
                if origin_is_visible {
                    drop(this.sync_transcript_document(cx));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn show_failed_queued_submission(
        &mut self,
        submission: &ComposerSubmission,
        cx: &mut Context<Self>,
    ) {
        let input = submission.input.as_array().cloned().unwrap_or_default();
        let preview = queued_submission_text(&submission.input);
        let (index, key) =
            self.model
                .push_local_user(&submission.client_user_message_id, preview, &input);
        self.model.set_status_for_key(&key, "failed");
        self.dirty_image_surfaces.insert(key);
        self.list_state.splice(index..index, 1);
        self.selected_item = index;
        self.list_state.set_follow_mode(FollowMode::Tail);
        drop(self.sync_transcript_document(cx));
    }

    fn show_started_queued_submission(
        &mut self,
        submission: &QueuedTurnSubmission,
        delivery_status: &str,
        cx: &mut Context<Self>,
    ) {
        let input = submission.input.as_array().cloned().unwrap_or_default();
        let preview = queued_submission_text(&submission.input);
        let was_following_tail = if self.buffer_view {
            self.transcript_editor.read(cx).is_following_tail()
        } else {
            self.list_state.is_following_tail()
        };
        let Some((index, key)) =
            self.model
                .ensure_local_user(&submission.client_user_message_id, preview, &input)
        else {
            return;
        };

        self.model.set_status_for_key(&key, delivery_status);
        self.dirty_image_surfaces.insert(key);
        self.list_state.splice(index..index, 1);
        if was_following_tail {
            self.selected_item = index;
            self.list_state.set_follow_mode(FollowMode::Tail);
        }
        drop(self.sync_transcript_document(cx));
    }

    fn start_next_queued_turn(&mut self, cx: &mut Context<Self>) {
        if self.queue_start_pending
            || self.turn_start_pending
            || self.model.current_turn_id.is_some()
            || self.has_unresolved_live_request()
            || self.queued_turns.is_empty()
            || !queue_state_is_visible(
                self.selected_thread_id.as_deref(),
                self.preserved_work_thread_id.as_deref(),
            )
        {
            return;
        }
        let (Some(client), Some(thread_id)) =
            (self.client.clone(), self.selected_thread_id.clone())
        else {
            return;
        };
        let queued_entry = self.queued_turns.front().cloned();
        self.queue_start_pending = true;
        self.queue_start_generation = self.queue_start_generation.wrapping_add(1);
        let generation = self.queue_start_generation;
        self.turn_task = cx.spawn(async move |this, cx| {
            let result = client.start_next_queued_turn(&thread_id).await;
            _ = this.update(cx, |this, cx| {
                if this.queue_start_generation != generation {
                    return;
                }
                this.queue_start_pending = false;
                if !queue_state_belongs_to_thread(
                    &thread_id,
                    this.selected_thread_id.as_deref(),
                    this.preserved_work_thread_id.as_deref(),
                ) {
                    return;
                }
                let origin_is_visible =
                    callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref());
                match result {
                    Ok(response) => {
                        if let Some(queued_entry) = queued_entry.as_ref() {
                            this.remove_queued_entry_locally(&queued_entry.client_user_message_id);
                            if origin_is_visible {
                                this.show_started_queued_submission(queued_entry, "sent", cx);
                            }
                        }
                        if origin_is_visible {
                            this.model.current_turn_id = response
                                .pointer("/turn/id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            this.error = None;
                        }
                    }
                    Err(error) => {
                        let already_started = error
                            .to_string()
                            .to_ascii_lowercase()
                            .contains("active or pending turn");
                        if origin_is_visible {
                            this.refresh_queued_turns(cx);
                        }
                        if origin_is_visible
                            && this.model.current_turn_id.is_none()
                            && !already_started
                        {
                            this.error =
                                Some(format!("Could not start queued prompt: {error}").into());
                        }
                    }
                }
                if origin_is_visible {
                    drop(this.sync_transcript_document(cx));
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn remove_queued_entry_locally(&mut self, client_user_message_id: &str) {
        if let Some(index) = self
            .queued_turns
            .iter()
            .position(|entry| entry.client_user_message_id == client_user_message_id)
        {
            self.queued_turns.remove(index);
        }
    }

    fn cancel_queued_turn(&mut self, client_user_message_id: String, cx: &mut Context<Self>) {
        let Some(entry) = self
            .queued_turns
            .iter()
            .find(|entry| entry.client_user_message_id == client_user_message_id)
            .cloned()
        else {
            return;
        };
        let (Some(client), Some(thread_id), Some(queued_submission_id)) = (
            self.client.clone(),
            self.selected_thread_id.clone(),
            entry.id.clone(),
        ) else {
            return;
        };
        if !queue_state_is_visible(
            self.selected_thread_id.as_deref(),
            self.preserved_work_thread_id.as_deref(),
        ) {
            return;
        }
        if self.queue_operations.contains_key(&client_user_message_id) {
            return;
        }
        self.queue_operations
            .insert(client_user_message_id.clone(), QueueOperation::Removing);
        cx.spawn(async move |this, cx| {
            let result = client
                .delete_queued_turn(&thread_id, &queued_submission_id)
                .await;
            _ = this.update(cx, |this, cx| {
                if !queue_state_belongs_to_thread(
                    &thread_id,
                    this.selected_thread_id.as_deref(),
                    this.preserved_work_thread_id.as_deref(),
                ) {
                    return;
                }
                this.queue_operations.remove(&client_user_message_id);
                let origin_is_visible =
                    callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref());
                match result {
                    Ok(_) => {
                        this.remove_queued_entry_locally(&client_user_message_id);
                        if origin_is_visible {
                            this.error = None;
                        }
                    }
                    Err(error) => {
                        if origin_is_visible {
                            this.error =
                                Some(format!("Could not remove queued prompt: {error}").into())
                        } else {
                            log::warn!("could not remove background queued prompt: {error}");
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn place_queued_turn_in_composer(
        &mut self,
        entry: &QueuedTurnSubmission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.composer.read(cx).text(cx);
        let mut text = existing;
        if !text.trim().is_empty() {
            text.push_str("\n\n");
        }
        for block in entry.input.as_array().into_iter().flatten() {
            match block.get("type").and_then(Value::as_str) {
                Some("text" | "inputText") => {
                    text.push_str(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
                Some("image" | "inputImage")
                    if self.composer_images.len() < MAX_COMPOSER_IMAGES =>
                {
                    let Some(image) = queued_submission_image(block) else {
                        continue;
                    };
                    self.next_composer_image_id = self.next_composer_image_id.wrapping_add(1);
                    let id = self.next_composer_image_id;
                    self.composer_images
                        .push(ComposerImageAttachment { id, image });
                    text.push_str(&composer_image_token(id));
                }
                _ => {}
            }
        }
        self.composer
            .update(cx, |editor, cx| editor.set_text(&text, window, cx));
        self.focus_composer(window, cx);
    }

    fn edit_queued_turn(
        &mut self,
        client_user_message_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .queued_turns
            .iter()
            .find(|entry| entry.client_user_message_id == client_user_message_id)
            .cloned()
        else {
            return;
        };
        let (Some(client), Some(thread_id), Some(queued_submission_id)) = (
            self.client.clone(),
            self.selected_thread_id.clone(),
            entry.id.clone(),
        ) else {
            return;
        };
        if !queue_state_is_visible(
            self.selected_thread_id.as_deref(),
            self.preserved_work_thread_id.as_deref(),
        ) {
            return;
        }
        if self.queue_operations.contains_key(&client_user_message_id) {
            return;
        }
        self.queue_operations
            .insert(client_user_message_id.clone(), QueueOperation::Editing);
        cx.spawn_in(window, async move |this, cx| {
            let result = client
                .delete_queued_turn(&thread_id, &queued_submission_id)
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                if !queue_state_belongs_to_thread(
                    &thread_id,
                    this.selected_thread_id.as_deref(),
                    this.preserved_work_thread_id.as_deref(),
                ) {
                    return;
                }
                this.queue_operations.remove(&client_user_message_id);
                let origin_is_visible =
                    callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref());
                match result {
                    Ok(_) => {
                        this.remove_queued_entry_locally(&client_user_message_id);
                        if origin_is_visible {
                            this.place_queued_turn_in_composer(&entry, window, cx);
                            this.error = None;
                        }
                    }
                    Err(error) => {
                        if origin_is_visible {
                            this.error =
                                Some(format!("Could not edit queued prompt: {error}").into())
                        } else {
                            log::warn!("could not edit background queued prompt: {error}");
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn steer_queued_turn(&mut self, client_user_message_id: String, cx: &mut Context<Self>) {
        let Some(entry) = self
            .queued_turns
            .iter()
            .find(|entry| entry.client_user_message_id == client_user_message_id)
            .cloned()
        else {
            return;
        };
        let (Some(client), Some(thread_id), Some(turn_id), Some(queued_submission_id)) = (
            self.client.clone(),
            self.selected_thread_id.clone(),
            self.model.current_turn_id.clone(),
            entry.id.clone(),
        ) else {
            return;
        };
        if !queue_state_is_visible(
            self.selected_thread_id.as_deref(),
            self.preserved_work_thread_id.as_deref(),
        ) {
            return;
        }
        if self.queue_operations.contains_key(&client_user_message_id) {
            return;
        }
        self.queue_operations.insert(
            client_user_message_id.clone(),
            QueueOperation::AddingToResponse,
        );
        cx.spawn(async move |this, cx| {
            let deleted = client
                .delete_queued_turn(&thread_id, &queued_submission_id)
                .await;
            let result = match deleted {
                Ok(_) => {
                    client
                        .steer_turn(
                            &thread_id,
                            &turn_id,
                            entry.input.clone(),
                            &entry.client_user_message_id,
                        )
                        .await
                }
                Err(error) => Err(error),
            };
            if result.is_err() {
                _ = client
                    .queue_turn(
                        &thread_id,
                        entry.input.clone(),
                        &entry.client_user_message_id,
                    )
                    .await;
            }
            _ = this.update(cx, |this, cx| {
                if !queue_state_belongs_to_thread(
                    &thread_id,
                    this.selected_thread_id.as_deref(),
                    this.preserved_work_thread_id.as_deref(),
                ) {
                    return;
                }
                this.queue_operations.remove(&client_user_message_id);
                let origin_is_visible =
                    callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref());
                match result {
                    Ok(_) => {
                        this.remove_queued_entry_locally(&client_user_message_id);
                        // The steer RPC only confirms that the active turn accepted
                        // this input. Keep the optimistic user message visibly pending
                        // until the authoritative userMessage item with the same
                        // clientUserMessageId replaces it at an input boundary.
                        if origin_is_visible {
                            this.show_started_queued_submission(
                                &entry,
                                "awaiting incorporation",
                                cx,
                            );
                            this.error = None;
                        }
                    }
                    Err(error) => {
                        if origin_is_visible {
                            this.refresh_queued_turns(cx);
                            this.error =
                                Some(format!("Could not steer queued prompt: {error}").into());
                        } else {
                            log::warn!("could not steer background queued prompt: {error}");
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn send_queued_turn_now(&mut self, client_user_message_id: String, cx: &mut Context<Self>) {
        let Some(entry) = self
            .queued_turns
            .iter()
            .find(|entry| entry.client_user_message_id == client_user_message_id)
            .cloned()
        else {
            return;
        };
        let (Some(client), Some(thread_id), Some(queued_submission_id)) = (
            self.client.clone(),
            self.selected_thread_id.clone(),
            entry.id.clone(),
        ) else {
            return;
        };
        if !queue_state_is_visible(
            self.selected_thread_id.as_deref(),
            self.preserved_work_thread_id.as_deref(),
        ) {
            return;
        }
        if self.queue_operations.contains_key(&client_user_message_id) {
            return;
        }
        self.queue_operations
            .insert(client_user_message_id.clone(), QueueOperation::Interrupting);
        // A turn/completed notification may arrive between interrupting the
        // current turn and starting this particular queued submission. Keep
        // the ordinary queue drain from racing the explicit "Send now" path.
        self.queue_start_pending = true;
        self.queue_start_generation = self.queue_start_generation.wrapping_add(1);
        let generation = self.queue_start_generation;
        let active_turn_id = self.model.current_turn_id.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                if let Some(turn_id) = active_turn_id {
                    client.interrupt_turn(&thread_id, &turn_id).await?;
                }
                client
                    .start_queued_turn(&thread_id, Some(&queued_submission_id))
                    .await
            }
            .await;
            _ = this.update(cx, |this, cx| {
                if this.queue_start_generation != generation {
                    return;
                }
                this.queue_start_pending = false;
                if !queue_state_belongs_to_thread(
                    &thread_id,
                    this.selected_thread_id.as_deref(),
                    this.preserved_work_thread_id.as_deref(),
                ) {
                    return;
                }
                this.queue_operations.remove(&client_user_message_id);
                let origin_is_visible =
                    callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref());
                match result {
                    Ok(response) => {
                        this.remove_queued_entry_locally(&client_user_message_id);
                        if origin_is_visible {
                            this.show_started_queued_submission(&entry, "sent", cx);
                            this.model.current_turn_id = response
                                .pointer("/turn/id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            this.error = None;
                        }
                    }
                    Err(error) => {
                        if origin_is_visible {
                            this.refresh_queued_turns(cx);
                            this.error =
                                Some(format!("Could not send queued prompt now: {error}").into());
                        } else {
                            log::warn!("could not send background queued prompt now: {error}");
                        }
                    }
                }
                if origin_is_visible {
                    drop(this.sync_transcript_document(cx));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn steer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn_id) = self.model.current_turn_id.clone() else {
            self.send(window, cx);
            return;
        };
        let draft = self.composer.read(cx).text(cx);
        if composer_is_empty(&draft, self.composer_images.len()) {
            // Ctrl-Shift-Enter immediately after queueing is a useful, stable
            // promotion gesture: add the newest acknowledged queued prompt to
            // the active response. Never guess while its queue write is still
            // pending—the row's Saving state makes that visible.
            if let Some(entry) = self
                .queued_turns
                .iter()
                .rev()
                .find(|entry| {
                    entry.id.is_some()
                        && !self
                            .queue_operations
                            .contains_key(&entry.client_user_message_id)
                })
                .cloned()
            {
                self.steer_queued_turn(entry.client_user_message_id, cx);
            }
            return;
        }
        let Some(submission) = self.take_composer_submission(true, window, cx) else {
            return;
        };
        let (Some(client), Some(thread_id)) =
            (self.client.clone(), self.selected_thread_id.clone())
        else {
            return;
        };
        let ComposerSubmission {
            key: Some(key),
            client_user_message_id,
            input,
        } = submission
        else {
            return;
        };
        if let Some(index) = self.model.set_status_for_key(&key, "adding to response") {
            self.list_state.splice(index..index + 1, 1);
        }
        cx.spawn(async move |this, cx| {
            let result = client
                .steer_turn(&thread_id, &turn_id, input, &client_user_message_id)
                .await;
            _ = this.update(cx, |this, cx| {
                if !callback_origin_is_visible(&thread_id, this.selected_thread_id.as_deref()) {
                    if let Err(error) = &result {
                        log::warn!("could not steer background turn: {error}");
                    }
                    return;
                }
                match result {
                    Ok(_) => {
                        // Success means app-server accepted the steer, not that the
                        // active model turn has incorporated it yet. item/started or
                        // item/completed will echo clientUserMessageId and replace
                        // this optimistic item once that boundary is actually crossed.
                        if let Some(index) = this
                            .model
                            .set_status_for_key(&key, "awaiting incorporation")
                        {
                            this.list_state.splice(index..index + 1, 1);
                        }
                        this.error = None;
                    }
                    Err(error) => {
                        if let Some(index) = this.model.set_status_for_key(&key, "failed") {
                            this.list_state.splice(index..index + 1, 1);
                        }
                        this.error = Some(format!("Could not steer turn: {error}").into());
                    }
                }
                drop(this.sync_transcript_document(cx));
                cx.notify();
            });
        })
        .detach();
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
        self.turn_task = cx.spawn(async move |this, cx| {
            let result = client.interrupt_turn(&thread_id, &turn_id).await;
            if let Err(error) = result {
                if this
                    .update(cx, |this, cx| {
                        if callback_origin_is_visible(
                            &thread_id,
                            this.selected_thread_id.as_deref(),
                        ) {
                            this.error = Some(format!("Could not stop turn: {error}").into());
                            cx.notify();
                        } else {
                            log::warn!("could not stop background turn: {error}");
                        }
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
                self.place_rich_cursor_at_item_last_line(target_item, window, cx);
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

    /// Host-driven Rich cursor placement does not produce a local native
    /// Editor selection event. Snapshot it immediately so the painted Rich
    /// transcript and its hidden Vim input surface agree in the same frame.
    /// Ordinary Vim motions continue through `TranscriptSelectionChanged`.
    fn place_rich_cursor_in_item(
        &mut self,
        item_index: usize,
        body_offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let placed = self.transcript_editor.update(cx, |editor, cx| {
            editor.set_cursor_in_item(item_index, body_offset, window, cx)
        });
        if placed && rich_vim_experiment() && !self.buffer_view {
            self.rich_navigation_selection = Some(
                self.transcript_editor
                    .update(cx, |editor, cx| editor.selection_snapshot(cx)),
            );
        }
        placed
    }

    fn place_rich_cursor_at_item_last_line(
        &mut self,
        item_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let placed = self.transcript_editor.update(cx, |editor, cx| {
            editor.set_cursor_at_item_last_line(item_index, window, cx)
        });
        if placed && rich_vim_experiment() && !self.buffer_view {
            self.rich_navigation_selection = Some(
                self.transcript_editor
                    .update(cx, |editor, cx| editor.selection_snapshot(cx)),
            );
        }
        placed
    }

    /// Make every representation of the transcript agree that the user is at
    /// the live edge. Rich mode has a native Editor for Vim state and a
    /// virtualized List for presentation; updating just one is what made `G`
    /// appear to do nothing (or immediately snap back) while a response grew.
    fn go_to_transcript_tail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let document = self.sync_transcript_document(cx);
        if let Some((item_index, body_offset)) = document.as_ref().and_then(transcript_tail_target)
        {
            self.selected_item = item_index;
            self.visual_anchor = None;
            self.place_rich_cursor_in_item(item_index, body_offset, window, cx);
            self.transcript_cursor_initialized = true;
        } else {
            self.selected_item = self.model.items.len().saturating_sub(1);
        }

        // Keep the native text projection ready for streaming and pin the
        // visible Rich list past its final real item, including the transient
        // activity row. List Tail mode will disengage on an upward gesture and
        // re-engage when a manual scroll reaches the bottom again.
        self.transcript_editor
            .update(cx, |editor, cx| editor.reveal_tail(cx));
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();

        // A streaming row can change height in the same frame. Reassert the
        // item-count anchor after that layout rather than revealing only the
        // last measured line from the preceding frame.
        cx.defer_in(window, |this, _, cx| {
            if this.list_state.is_following_tail() {
                this.list_state.scroll_to_end();
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
        self.search_returns_to_buffer = rich_vim_experiment() && !self.search_query.is_empty();
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
                self.place_rich_cursor_in_item(item_index, body_offset, window, cx);
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
        self.vim_command_line = None;
        self.command_line_error = None;
        self.search_highlights_visible = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.active_search_match = 0;
        self.search_match_count = 0;
        self.active_search_item = None;
        self.active_search_body_offset = None;
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
                self.performance_reporter
                    .mark_baseline_as(window, ":perf-j baseline");
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
        self.place_rich_cursor_in_item(candidate, 0, window, cx);

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
        self.selected_task = sidebar_selection_index(
            &self.sidebar_threads,
            self.selected_thread_id.as_deref(),
            self.selected_task,
        );
        self.transcript_focus.focus(window, cx);
        if !self.sidebar_threads.is_empty() {
            self.selected_task = self.selected_task.min(self.sidebar_threads.len() - 1);
            self.task_list_state
                .scroll_to_item(self.selected_task, ScrollStrategy::Nearest);
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
        if self.sidebar_threads.is_empty() {
            return;
        }
        self.selected_task = self
            .selected_task
            .saturating_add_signed(delta)
            .min(self.sidebar_threads.len() - 1);
        self.task_list_state
            .scroll_to_item(self.selected_task, ScrollStrategy::Nearest);
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
        self.toggle_item_at(self.selected_item, window, cx);
    }

    fn toggle_item_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((item_key, collapsed)) = toggle_model_item_expansion_at(&mut self.model, index)
        else {
            return;
        };
        self.list_state.splice(index..index + 1, 1);
        if self.buffer_view {
            self.transcript_editor.update(cx, |editor, cx| {
                editor.set_item_collapsed(&item_key, collapsed, window, cx);
            });
        } else if rich_vim_experiment() {
            let item_count = self.model.items.len();
            if !self.sync_transcript_item_updates(item_count, &[index], cx) {
                drop(self.sync_transcript_document(cx));
            }
        }
        cx.notify();
    }

    fn toggle_item_by_key(&mut self, item_key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .model
            .items
            .iter()
            .position(|item| item.key == item_key)
        else {
            return;
        };
        self.toggle_item_at(index, window, cx);
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
        self.open_vim_search(false, window, cx);
    }

    fn open_vim_search(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.search_return_focus = self.focus_mode;
        self.search_returns_to_buffer =
            search_uses_native_editor(self.buffer_view, self.focus_mode, rich_vim_experiment());
        self.buffer_search_backwards = backwards;
        self.vim_command_line = Some(VimCommandLine::Search { backwards });
        self.command_line_error = None;
        self.search_visible = true;
        self.search_highlights_visible = !self.search_query.is_empty();
        self.focus_mode = FocusMode::Search;
        if self.search_returns_to_buffer {
            self.transcript_editor
                .update(cx, |editor, cx| editor.begin_search_preview(cx));
        }
        self.search_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.search_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn open_buffer_search(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !vim_search_available(self.buffer_view, rich_vim_experiment()) {
            return;
        }
        self.open_vim_search(backwards, window, cx);
    }

    fn open_ex_command(
        &mut self,
        initial_query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_return_focus = self.focus_mode;
        self.vim_command_line = Some(VimCommandLine::Ex);
        self.command_line_error = None;
        self.search_visible = true;
        self.focus_mode = FocusMode::Search;
        self.search_editor.update(cx, |editor, cx| {
            editor.set_text(initial_query.trim_start_matches(':'), window, cx)
        });
        self.search_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn restore_search_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_return_focus == FocusMode::Buffer {
            self.focus_mode = FocusMode::Buffer;
            self.transcript_editor.focus_handle(cx).focus(window, cx);
        } else {
            self.focus_mode = FocusMode::Transcript;
            self.transcript_focus.focus(window, cx);
        }
    }

    fn commit_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim_command_line == Some(VimCommandLine::Ex) {
            self.commit_ex_command(window, cx);
            return;
        }
        let next_query = self.search_editor.read(cx).text(cx);
        let repeat_previous = next_query.is_empty();
        if !next_query.is_empty() {
            self.search_query.clone_from(&next_query);
        }
        self.search_highlights_visible = !self.search_query.is_empty();
        if self.search_returns_to_buffer {
            self.search_visible = false;
            self.vim_command_line = None;
            self.command_line_error = None;
            self.focus_mode = FocusMode::Buffer;
            let query = self.search_query.clone();
            let backwards = self.buffer_search_backwards;
            self.transcript_editor.update(cx, |editor, cx| {
                if repeat_previous {
                    editor.commit_search_preview();
                    editor.search(&query, backwards, window, cx);
                } else {
                    editor.preview_search(&query, backwards, window, cx);
                    editor.commit_search_preview();
                }
            });
            self.sync_native_search_state(cx);
            self.transcript_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        }
        self.rebuild_search_matches();
        if !self.search_matches.is_empty() {
            self.active_search_match = if self.buffer_search_backwards {
                self.search_matches
                    .iter()
                    .rposition(|index| *index <= self.selected_item)
                    .unwrap_or(self.search_matches.len() - 1)
            } else {
                self.search_matches
                    .iter()
                    .position(|index| *index >= self.selected_item)
                    .unwrap_or(0)
            };
            self.jump_to_search_match(cx);
        }
        self.search_visible = false;
        self.vim_command_line = None;
        self.command_line_error = None;
        self.restore_search_focus(window, cx);
        cx.notify();
    }

    fn commit_ex_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search_editor.read(cx).text(cx);
        let command = query.trim().trim_start_matches(':');
        let supported = matches!(
            command,
            "new"
                | "enew"
                | "text"
                | "rich"
                | "mono"
                | "reading"
                | "perf"
                | "perf-j"
                | "compose"
                | "tasks"
                | "stop"
                | "noh"
                | "nohl"
                | "nohlsearch"
        );
        if !supported {
            self.command_line_error = Some(format!("Not an editor command: {command}").into());
            cx.notify();
            return;
        }

        self.search_visible = false;
        self.vim_command_line = None;
        self.command_line_error = None;
        self.restore_search_focus(window, cx);
        match command {
            "new" | "enew" => self.new_task(window, cx),
            "text" => self.show_text_transcript(window, cx),
            "rich" => self.show_rich_transcript(window, cx),
            "mono" => {
                self.use_transcript_typography(TranscriptTypographyProfile::Buffer, window, cx)
            }
            "reading" => {
                self.use_transcript_typography(TranscriptTypographyProfile::Reading, window, cx)
            }
            "perf" => self.copy_performance_report(window, cx),
            "perf-j" => self.run_performance_j(window, cx),
            "compose" => self.focus_composer(window, cx),
            "tasks" => self.toggle_sidebar(window, cx),
            "stop" => self.stop(cx),
            "noh" | "nohl" | "nohlsearch" => self.clear_search_highlights(cx),
            _ => unreachable!("supported Ex command was not dispatched"),
        }
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_command_line, Some(VimCommandLine::Search { .. }))
            && self.search_returns_to_buffer
        {
            self.transcript_editor
                .update(cx, |editor, cx| editor.cancel_search_preview(window, cx));
            self.sync_native_search_state(cx);
        }
        self.search_visible = false;
        self.vim_command_line = None;
        self.command_line_error = None;
        self.restore_search_focus(window, cx);
        cx.notify();
    }

    fn clear_search_highlights(&mut self, cx: &mut Context<Self>) {
        self.search_visible = false;
        self.vim_command_line = None;
        self.command_line_error = None;
        self.search_highlights_visible = false;
        if self.search_returns_to_buffer {
            self.transcript_editor
                .update(cx, |editor, cx| editor.hide_search_highlights(cx));
        }
        cx.notify();
    }

    fn sync_native_search_state(&mut self, cx: &mut Context<Self>) {
        let snapshot = self
            .transcript_editor
            .update(cx, |editor, cx| editor.search_snapshot(cx));
        let Some(snapshot) = snapshot else {
            self.search_matches.clear();
            self.search_match_count = 0;
            self.active_search_match = 0;
            self.active_search_item = None;
            self.active_search_body_offset = None;
            return;
        };
        self.search_query = snapshot.query;
        self.search_highlights_visible = snapshot.highlights_visible;
        self.search_matches = snapshot.matching_item_indices;
        self.search_match_count = snapshot.match_count;
        self.active_search_match = snapshot.active_match_index.unwrap_or(0);
        self.active_search_item = snapshot.active_item_index;
        self.active_search_body_offset = snapshot.active_body_offset;
        self.search_navigation_generation = self.search_navigation_generation.wrapping_add(1);
    }

    fn rebuild_search_matches(&mut self) {
        let query = self.search_query.trim();
        self.search_matches = if query.is_empty() {
            Vec::new()
        } else {
            self.model
                .items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| item_matches_search_query(item, query).then_some(index))
                .collect()
        };
        self.active_search_match = self
            .active_search_match
            .min(self.search_matches.len().saturating_sub(1));
        self.search_match_count = self.search_matches.len();
        self.active_search_item = self.search_matches.get(self.active_search_match).copied();
        self.active_search_body_offset = None;
    }

    fn update_search_matches_for_changes(&mut self, old_len: usize, dirty_items: &[usize]) {
        let query = self.search_query.trim();
        if query.is_empty() {
            self.search_matches.clear();
            self.active_search_match = 0;
            self.search_match_count = 0;
            self.active_search_item = None;
            self.active_search_body_offset = None;
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
                item_matches_search_query(item, query),
            );
        }

        self.active_search_match = active_item
            .and_then(|active_item| self.search_matches.binary_search(&active_item).ok())
            .unwrap_or_else(|| {
                self.search_matches
                    .partition_point(|index| *index < self.selected_item)
                    .min(self.search_matches.len().saturating_sub(1))
            });
        self.search_match_count = self.search_matches.len();
        self.active_search_item = self.search_matches.get(self.active_search_match).copied();
        self.active_search_body_offset = None;
    }

    fn jump_to_search_match(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.search_matches.get(self.active_search_match).copied() {
            self.selected_item = index;
            self.active_search_item = Some(index);
            self.active_search_body_offset = None;
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
            self.sync_native_search_state(cx);
            cx.notify();
            return;
        }
        if self.search_matches.is_empty() {
            return;
        }
        self.search_highlights_visible = true;
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
        if self.turn_active() {
            context.add("HarnessTurnActive");
        }
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

    fn turn_active(&self) -> bool {
        self.model.current_turn_id.is_some() || self.turn_start_pending || self.queue_start_pending
    }

    fn sync_turn_tail_indicator(&self, turn_active: bool) {
        if let Some((range, replacement_count)) = turn_tail_list_splice(
            self.list_state.item_count(),
            self.model.items.len(),
            turn_active,
        ) {
            self.list_state.splice(range, replacement_count);
        }
    }

    fn render_task(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let Some(row) = self.sidebar_threads.get(index).cloned() else {
            return div().into_any_element();
        };
        let Some(thread) = self.sidebar_thread(&row).cloned() else {
            return div().into_any_element();
        };
        let thread_id = thread.id.clone();
        let thread_cwd = if thread.cwd.is_empty() {
            self.cwd.clone()
        } else {
            thread.cwd.clone()
        };
        let selected = self.selected_thread_id.as_deref() == Some(thread.id.as_str());
        let cursor = self.focus_mode == FocusMode::Tasks && self.selected_task == index;
        let title = thread_title(&thread);
        let project = if row.depth == 0 {
            Path::new(&thread.cwd)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Codex".into())
        } else {
            child_thread_identity(&thread)
        };
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
        } else if row.depth > 0 {
            listed_thread_status(&thread)
        } else {
            AgentThreadStatus::Completed
        };
        let weak = cx.weak_entity();
        let open_thread_id = thread_id.clone();
        let thread_item = ThreadItem::new(format!("task-{thread_id}"), title)
            .icon(IconName::AiOpenAi)
            .project_name(project)
            .timestamp(relative_time(thread.updated_at))
            .status(status)
            .selected(selected)
            .focused(cursor)
            .base_bg(colors.panel_background)
            .on_click(move |_, _, cx| {
                weak.update(cx, |this, cx| {
                    this.selected_task = sidebar_selection_index(
                        &this.sidebar_threads,
                        Some(&open_thread_id),
                        index,
                    );
                    this.open_thread_by_id(&open_thread_id, cx);
                })
                .ok();
            })
            .into_any_element();

        let thread_item = right_click_menu(format!("thread-context-menu-{thread_id}"))
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
            .into_any_element();

        div()
            .w_full()
            .pl(px((row.depth.min(6) as f32) * 12.))
            .child(thread_item)
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
                let entity = cx.new(|cx| {
                    Markdown::new_with_options(
                        source.to_string().into(),
                        None,
                        None,
                        markdown::MarkdownOptions::default(),
                        cx,
                    )
                });
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
            let appended_suffix = source
                .strip_prefix(cached.source.as_str())
                .filter(|suffix| !suffix.is_empty())
                .map(ToOwned::to_owned);
            cached.source = source.to_string();
            cached.search_query = None;
            cached.search_ranges.clear();
            cached.navigation = None;
            cached.last_autoscroll_generation = None;
            cached.entity.update(cx, |markdown, cx| {
                if let Some(suffix) = appended_suffix.as_deref() {
                    markdown.append(suffix, cx);
                } else {
                    markdown.reset(source.to_string().into(), cx);
                }
            });
        }

        let markdown_navigation =
            navigation.map(|navigation| navigation.markdown_source_navigation(source));
        if cached.navigation != markdown_navigation {
            let autoscroll_cursor =
                changed_markdown_cursor(cached.navigation.as_ref(), markdown_navigation.as_ref());
            cached.navigation = markdown_navigation.clone();
            cached.entity.update(cx, |markdown, cx| {
                let navigation = markdown_navigation.as_ref();
                markdown.set_external_navigation(
                    navigation.map(|navigation| navigation.selections.clone()),
                    navigation.and_then(|navigation| navigation.cursor),
                    cx,
                );
                if let Some(source_index) = autoscroll_cursor {
                    markdown.request_autoscroll_to_source_index(source_index, cx);
                }
            });
        }

        let desired = if let Some(search) = search {
            if cached.search_query.as_deref() != Some(search.query.as_ref()) {
                cached.search_query = Some(search.query.to_string());
                cached.search_ranges =
                    search_match_byte_ranges(source, &search.query, RICH_SEARCH_HIGHLIGHT_LIMIT);
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
        path: &str,
        operation: &str,
        visible_line_count: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        logical_body_start: usize,
        item_index: usize,
        owner: Option<WeakEntity<HarnessApp>>,
        cx: &App,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors().clone();
        let visuals = HarnessVisualTheme::from_zed(&colors);
        let mut logical_line_offset = logical_body_start;
        zed_diff_lines(content, operation)
            .into_iter()
            .take(visible_line_count)
            .map(|line| {
                let logical_line_range = logical_line_offset..logical_line_offset + line.text.len();
                logical_line_offset += line.text.len() + 1;
                let syntax = diff_line_syntax_highlights(path, &line.text, line.tone, false, cx);
                let highlighted_line = navigation_searchable_styled_text(
                    line.text.to_string(),
                    syntax,
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
                    .h(harness_code_row_height(cx))
                    .px_2()
                    .flex()
                    .items_center()
                    .font_harness_code(cx)
                    .bg(if line.tone == DiffLineTone::Addition {
                        visuals.diff_added_surface
                    } else if line.tone == DiffLineTone::Deletion {
                        visuals.diff_deleted_surface
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(colors.text)
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
        self.render_file_change(item, index, search, navigation, window, cx)
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
        let visuals = HarnessVisualTheme::from_zed(&colors);
        let presentations = diff_file_presentations(&item.content);
        let allocations = progressive_file_line_allocations(
            &presentations
                .iter()
                .map(|presentation| zed_diff_lines(&presentation.content, "Modified").len())
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
            let logical_content_start = path_range.end.saturating_add(1);
            let (additions, deletions) = diff_content_counts(&presentation.content);
            let highlighted_path = navigation_searchable_styled_text(
                presentation.path.clone(),
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
                        this.border_t_1().border_color(colors.border_variant)
                    })
                    .child(
                        rich_card_identity_row(cx)
                            .px_1()
                            .when(section_index == 0, |this| this.pr_5())
                            .border_b_1()
                            .border_color(visuals.divider)
                            .bg(visuals.tool_header_surface)
                            .child(rich_file_identity_icon(&presentation.path, cx))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .font_harness_reading(cx)
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
                                    .blocks()
                                    .label_size(LabelSize::Small)
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
                                        &presentation.path,
                                        "Modified",
                                        visible_lines,
                                        search,
                                        navigation,
                                        logical_content_start,
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

    fn render_rich_file_change_row(
        row_data: &RichFileChangeData,
        row_index: usize,
        index: usize,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        owner: &WeakEntity<HarnessApp>,
        cx: &mut App,
    ) -> AnyElement {
        let row = &row_data.rows[row_index];
        match row {
            RichFileChangeRow::Header {
                section_index,
                logical_range,
            } => {
                let presentation = &row_data.presentations[*section_index];
                let colors = cx.theme().colors();
                let visuals = HarnessVisualTheme::from_zed(colors);
                let (additions, deletions) = file_change_counts(presentation);
                let highlighted_path = navigation_searchable_styled_text(
                    presentation.path.clone(),
                    Vec::new(),
                    search,
                    navigation,
                    logical_range.clone(),
                    cx,
                );
                let clickable_path = rich_clickable_styled_text(
                    format!("rich-file-change-path:{index}:{section_index}"),
                    highlighted_path,
                    index,
                    logical_range.clone(),
                    Some(owner.clone()),
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
                rich_card_identity_row(cx)
                    .px_1()
                    .pr_5()
                    .when(row_index > 0, |this| {
                        this.border_t_1().border_color(visuals.divider)
                    })
                    .bg(visuals.tool_header_surface)
                    .child(rich_file_identity_icon(&presentation.path, cx))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .font_harness_reading(cx)
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
                                    .blocks()
                                    .label_size(LabelSize::Small)
                                    .tooltip(format!(
                                        "{additions} lines added, {deletions} lines removed"
                                    )),
                                )
                            })
                            .when(additions == 0 && deletions == 0, |this| {
                                let operation = searchable_styled_text(
                                    presentation.operation.clone(),
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
                    .into_any_element()
            }
            RichFileChangeRow::Line {
                section_index,
                line_index,
                text,
                logical_range,
                tone,
            } => {
                let colors = cx.theme().colors();
                let visuals = HarnessVisualTheme::from_zed(colors);
                let presentation = &row_data.presentations[*section_index];
                let syntax =
                    diff_line_syntax_highlights(&presentation.path, text, *tone, false, cx);
                let highlighted_line = navigation_searchable_styled_text(
                    text.to_string(),
                    syntax,
                    search,
                    navigation,
                    logical_range.clone(),
                    cx,
                );
                let cursor_marker = rich_cursor_index_for_fragment(navigation, logical_range).map(
                    |rendered_index| {
                        rich_cursor_autoscroll_marker(
                            highlighted_line.layout().clone(),
                            rendered_index,
                            cx.theme().players().local().cursor.opacity(0.55),
                        )
                    },
                );
                let clickable_line = rich_clickable_styled_text(
                    format!("rich-file-change-line:{index}:{section_index}:{line_index}"),
                    highlighted_line,
                    index,
                    logical_range.clone(),
                    Some(owner.clone()),
                );
                div()
                    .w_full()
                    .h(harness_code_row_height(cx))
                    .px_2()
                    .flex()
                    .items_center()
                    .relative()
                    .font_harness_code(cx)
                    .bg(if *tone == DiffLineTone::Addition {
                        visuals.diff_added_surface
                    } else if *tone == DiffLineTone::Deletion {
                        visuals.diff_deleted_surface
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(colors.text)
                    .child(div().min_w_0().whitespace_nowrap().child(clickable_line))
                    .when_some(cursor_marker, |this, marker| this.child(marker))
                    .into_any_element()
            }
        }
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
        let (data, list_state, horizontal_handle) =
            self.rich_file_change_surface(item, navigation, cx);
        let colors = cx.theme().colors().clone();
        let owner = cx.weak_entity();
        let search = search.cloned();
        let navigation = navigation.cloned();
        let row_data = data.clone();
        let row_owner = owner.clone();
        let rows = list(list_state.clone(), move |row_index, _, cx| {
            Self::render_rich_file_change_row(
                &row_data,
                row_index,
                index,
                search.as_ref(),
                navigation.as_ref(),
                &row_owner,
                cx,
            )
        })
        .with_sizing_behavior(ListSizingBehavior::Infer)
        .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT));
        let vertical = div()
            .w_full()
            .min_w_0()
            .relative()
            .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
            .overflow_y_hidden()
            .child(rows)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical)
                    .id(("file-change-vertical-scrollbar", index))
                    .with_thumb_color(colors.text_muted.opacity(0.5))
                    .tracked_scroll_handle(&list_state),
                window,
                cx,
            );
        div()
            .id(("file-change-output", index))
            .w_full()
            .min_w_0()
            .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
            .overflow_x_scroll()
            .overflow_y_hidden()
            .track_scroll(&horizontal_handle)
            .child(vertical)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Horizontal)
                    .id(("file-change-horizontal-scrollbar", index))
                    .with_thumb_color(colors.text_muted.opacity(0.5))
                    .tracked_scroll_handle(&horizontal_handle),
                window,
                cx,
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
        let steps = reasoning_summary_lines(content)
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
            .gap_1()
            .ml_1p5()
            .pl_3p5()
            .border_l_1()
            .border_color(colors.border_variant)
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
                    .min_w_0()
                    .font_harness_reading(cx)
                    .text_color(colors.text)
                    .child(highlighted_step)
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
                    .min_h(harness_code_row_height(cx))
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
            .font_harness_code(cx)
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
            let content = sections
                .into_iter()
                .map(|section| section.body)
                .collect::<Vec<_>>()
                .join("\n");
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
                            .min_h(harness_code_row_height(cx))
                            .font_harness_code(cx)
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
        let (data, command_scroll_handle, output_list_state, output_horizontal_handle) =
            self.rich_command_surface(item, navigation, cx)?;
        let colors = cx.theme().colors().clone();
        let owner = cx.weak_entity();
        let search = search.cloned();
        let navigation = navigation.cloned();
        let command_data = data.clone();
        let command_search = search.clone();
        let command_navigation = navigation.clone();
        let command_owner = owner.clone();
        let command_status = item.command_execution_status();
        let show_status_in_command_body = expanded_item_uses_content_as_header(item);
        let command_rows = command_data
            .rows
            .iter()
            .take(command_data.command_row_count)
            .map(|row| {
                let line = &command_data.command[row.source_range.clone()];
                let first_command_row = row.line_index == 0;
                let logical_range = rich_command_row_navigation_range(&command_data, row);
                let rendered_line = line.to_owned();
                let base_highlights = shell_highlights(line, cx);
                let highlighted = navigation_searchable_styled_text(
                    rendered_line,
                    base_highlights,
                    command_search.as_ref(),
                    command_navigation.as_ref(),
                    logical_range.clone(),
                    cx,
                );
                let cursor_marker =
                    rich_cursor_index_for_fragment(command_navigation.as_ref(), &logical_range)
                        .map(|rendered_index| {
                            rich_cursor_autoscroll_marker(
                                highlighted.layout().clone(),
                                rendered_index,
                                cx.theme().players().local().cursor.opacity(0.55),
                            )
                        });
                let clickable = rich_clickable_styled_text(
                    format!("rich-command-text:{index}:{}", row.line_index),
                    highlighted,
                    index,
                    logical_range,
                    Some(command_owner.clone()),
                );
                let visual_status = (first_command_row && show_status_in_command_body)
                    .then_some(command_status)
                    .flatten()
                    .and_then(|status| render_command_visual_status(status, cx));
                let status_padding = visual_status
                    .as_ref()
                    .map(|(reserved_width, _)| *reserved_width)
                    .unwrap_or(0.);
                div()
                    .w_full()
                    .min_w_0()
                    .min_h(harness_code_row_height(cx))
                    .relative()
                    .flex()
                    .items_start()
                    .gap_1()
                    .whitespace_normal()
                    .when(first_command_row && status_padding > 0., |this| {
                        this.pr(px(status_padding))
                    })
                    .when(first_command_row, |this| {
                        this.child(rich_card_identity_icon(
                            IconName::ToolTerminal,
                            IconSize::Small,
                            Color::Accent,
                        ))
                    })
                    .child(div().min_w_0().flex_1().child(clickable))
                    .when_some(visual_status, |this, (_, status)| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(0.))
                                // The outer card's disclosure owns the
                                // rightmost 20 px. Status sits immediately
                                // before it, as in Zed's terminal header.
                                .right(px(21.))
                                .child(status),
                        )
                    })
                    .when_some(cursor_marker, |this, marker| this.child(marker))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

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
                .min_h(harness_code_row_height(cx))
                .relative()
                .whitespace_nowrap()
                .child(clickable)
                .when_some(cursor_marker, |this, marker| this.child(marker))
                .into_any_element()
        })
        .with_sizing_behavior(ListSizingBehavior::Infer)
        .max_h(px(RICH_NESTED_COMMAND_OUTPUT_MAX_HEIGHT));

        let command_vertical_region = div()
            .id(("command-input-vertical-scroll", index))
            .w_full()
            .min_w_0()
            .relative()
            .max_h(px(RICH_NESTED_COMMAND_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&command_scroll_handle)
            .children(command_rows)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical)
                    .id(("command-input-scrollbar", index))
                    .with_thumb_color(colors.text_muted.opacity(0.5))
                    .tracked_scroll_handle(&command_scroll_handle),
                window,
                cx,
            );
        let command_region = div()
            .id(("command-input-scroll", index))
            .w_full()
            .min_w_0()
            .overflow_x_hidden()
            .overflow_y_hidden()
            .child(command_vertical_region);

        let output_region = (!data.output.is_empty()).then(|| {
            let vertical = div()
                .id(("command-output-vertical-scroll", index))
                .w_full()
                .min_w_0()
                .relative()
                .max_h(px(RICH_NESTED_COMMAND_OUTPUT_MAX_HEIGHT))
                .overflow_y_hidden()
                .child(output_rows)
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical)
                        .id(("command-output-scrollbar", index))
                        .with_thumb_color(colors.text_muted.opacity(0.5))
                        .tracked_scroll_handle(&output_list_state),
                    window,
                    cx,
                );
            div()
                .id(("command-output-scroll", index))
                .w_full()
                .min_w_0()
                .border_t_1()
                .border_color(colors.border_variant)
                .px_1p5()
                .py_1()
                .overflow_x_scroll()
                .overflow_y_hidden()
                .track_scroll(&output_horizontal_handle)
                .child(vertical)
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Horizontal)
                        .id(("command-output-horizontal-scrollbar", index))
                        .with_thumb_color(colors.text_muted.opacity(0.5))
                        .tracked_scroll_handle(&output_horizontal_handle),
                    window,
                    cx,
                )
        });

        Some(
            div()
                .id(("command-output", index))
                .w_full()
                .min_w_0()
                .font_harness_code(cx)
                .line_height(relative(1.35))
                .text_color(colors.text)
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .p_1p5()
                        .bg(HarnessVisualTheme::from_zed(&colors).tool_header_surface)
                        .child(command_region),
                )
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
        let command_start = 0;
        // The ellipsis is presentation chrome, not a Vim byte. Keep its
        // clickable/highlight range clamped to the actual command prefix so a
        // long preview cannot spill into the output's logical row.
        let command_end = visible_command_source_len;
        let output_start = command_text.len()
            + usize::from(!command_text.is_empty() && !displayed_output.is_empty());
        let command_highlights = shell_highlights(&displayed_command, cx);
        let highlighted_command = navigation_searchable_styled_text(
            displayed_command,
            command_highlights,
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
                        .pr_5()
                        .flex()
                        .items_start()
                        .gap_1()
                        .font_harness_code(cx)
                        .line_height(relative(1.35))
                        .whitespace_normal()
                        .child(rich_card_identity_icon(
                            IconName::ToolTerminal,
                            IconSize::Small,
                            Color::Accent,
                        ))
                        .child(div().min_w_0().flex_1().child(clickable_command)),
                )
                .when(!displayed_output.is_empty(), |this| {
                    this.child(
                        div()
                            .id(("command-output-scroll", index))
                            .w_full()
                            .min_w_0()
                            .border_t_1()
                            .border_color(colors.border_variant)
                            .mt_1()
                            .pt_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .font_harness_code(cx)
                            .line_height(relative(1.35))
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
        // The first exact query is painted in the shared card identity row.
        // Begin body mapping after it so additional queries and result rows
        // retain stable Vim/search coordinates without repeating that query.
        let mut logical_cursor = presentation
            .queries
            .first()
            .map_or(0, |query| query.len().saturating_add(1));
        let mut row_ranges = Vec::new();
        let query_rows = presentation
            .queries
            .iter()
            .skip(1)
            .enumerate()
            .map(|(query_index, query)| {
                let range = rich_navigation_fragment_range(navigation, query, &mut logical_cursor);
                row_ranges.push(Some(range.clone()));
                let highlighted = navigation_searchable_styled_text(
                    query.clone(),
                    shell_highlights(query, cx),
                    search,
                    navigation,
                    range,
                    cx,
                );
                div()
                    .id(format!("web-query:{item_key}:{query_index}"))
                    .w_full()
                    .min_w_0()
                    .py_0p5()
                    .font_harness_code(cx)
                    .text_color(colors.text_muted)
                    .child(highlighted)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
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
                let domain = result.domain.clone().map(|domain| {
                    let range =
                        rich_navigation_fragment_range(navigation, &domain, &mut logical_cursor);
                    let highlighted = navigation_searchable_styled_text(
                        domain,
                        Vec::new(),
                        search,
                        navigation,
                        range.clone(),
                        cx,
                    );
                    (highlighted, range)
                });
                let result_end = domain
                    .as_ref()
                    .map(|(_, range)| range.end)
                    .unwrap_or(title_range.end);
                row_ranges.push(Some(result_start..result_end));
                let result_row = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .py_0p5()
                    .when(result_index > 0, |this| {
                        this.border_t_1().border_color(colors.border_variant)
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .font_harness_reading(cx)
                            .child(highlighted_title),
                    )
                    .when_some(domain, |this, (highlighted_domain, _)| {
                        this.child(div().flex_none().text_color(colors.text_muted).child("—"))
                            .child(
                                div()
                                    .max_w(relative(0.38))
                                    .truncate()
                                    .text_color(colors.text_muted)
                                    .child(highlighted_domain),
                            )
                    });
                if let Some(url) = result.url {
                    let open_url = url.clone();
                    result_row
                        .id(format!("web-result:{item_key}:{result_index}"))
                        .cursor_pointer()
                        .hover(|this| this.bg(colors.element_hover))
                        .tooltip(Tooltip::text(url))
                        .on_click(move |_, _, cx| cx.open_url(&open_url))
                        .into_any_element()
                } else {
                    result_row.into_any_element()
                }
            })
            .collect::<Vec<_>>();

        let results_scroll = (!query_rows.is_empty() || total_results > 0).then(|| {
            let binding = self.rich_nested_scroll_binding(&item.key, navigation);
            reveal_rich_nested_cursor(Some(&binding), navigation, &row_ranges);
            div()
                .id(("web-results-scroll", index))
                .w_full()
                .min_w_0()
                .max_h(px(RICH_NESTED_OUTPUT_MAX_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(&binding.handle)
                .children(query_rows)
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
        &mut self,
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
        let surface_size = surface
            .as_ref()
            .map(|surface| surface.read(cx).preview_size());
        let expanded_source = surface
            .as_ref()
            .and_then(|surface| surface.read(cx).preview_source());
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .when_some(
                surface.zip(surface_size),
                |this, (surface, (width, height))| {
                    let preview_source = expanded_source.clone();
                    this.child(
                        div()
                            .id(format!("viewed-image-preview:{}", item.key))
                            .w(px(width))
                            .max_w_full()
                            .h(px(height))
                            .overflow_hidden()
                            .rounded_xs()
                            .when(preview_source.is_some(), |this| this.cursor_pointer())
                            .when_some(preview_source, |this, source| {
                                this.on_click(cx.listener(move |this, _, _, cx| {
                                    this.expanded_user_image = Some(source.clone());
                                    cx.notify();
                                }))
                            })
                            .child(surface),
                    )
                },
            )
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

    fn render_ordered_user_content(
        &mut self,
        item_key: &str,
        blocks: Vec<model::UserContentBlock>,
        previews: Vec<UserImagePreview>,
        search: Option<&RichSearchPaint>,
        navigation: Option<&RichNavigationPaint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let mut text_offset = 0;
        let mut children = Vec::new();
        for (part_index, block) in blocks.into_iter().enumerate() {
            match block {
                model::UserContentBlock::Text(text) => {
                    let source_range = text_offset..text_offset + text.len();
                    text_offset += text.len();
                    if text.trim().is_empty() {
                        continue;
                    }
                    let part_navigation = navigation.map(|navigation| {
                        rich_navigation_slice(navigation, source_range.clone(), &text)
                    });
                    let cache_key = format!("{item_key}:user-text:{part_index}");
                    let markdown = self.markdown_for(
                        &cache_key,
                        &text,
                        search,
                        part_navigation.as_ref(),
                        None,
                        cx,
                    );
                    let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                    style.code_block_overflow_x_scroll = true;
                    if let Some(navigation) = part_navigation.as_ref() {
                        style.selection_background_color =
                            rich_navigation_markdown_highlight_background(navigation, cx);
                    }
                    let owner = cx.weak_entity();
                    let logical = text.clone();
                    let source = text.clone();
                    let base = source_range.start;
                    let click_boundary_id =
                        format!("rich-user-markdown-click:{item_key}:{part_index}");
                    let item_key = item_key.to_owned();
                    let element = MarkdownElement::new(markdown, style).on_source_click(
                        move |source_offset, _, window, cx| {
                            let body_offset = base
                                + markdown_logical_offset_for_source(
                                    &logical,
                                    &source,
                                    source_offset,
                                );
                            owner
                                .update(cx, |this, cx| {
                                    if let Some(index) = this
                                        .model
                                        .items
                                        .iter()
                                        .position(|item| item.key == item_key)
                                    {
                                        this.selected_item = index;
                                        this.visual_anchor = None;
                                        this.focus_mode = FocusMode::Buffer;
                                        this.transcript_cursor_initialized = true;
                                        this.list_state.pause_following_tail();
                                        this.place_rich_cursor_in_item(
                                            index,
                                            body_offset,
                                            window,
                                            cx,
                                        );
                                        this.transcript_editor.focus_handle(cx).focus(window, cx);
                                        this.transcript_editor.update(cx, |editor, cx| {
                                            editor.enter_normal_mode(window, cx)
                                        });
                                        cx.notify();
                                    }
                                })
                                .ok();
                            true
                        },
                    );
                    children.push(
                        div()
                            .id(click_boundary_id)
                            .w_full()
                            .min_w_0()
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(element)
                            .into_any_element(),
                    );
                }
                model::UserContentBlock::Image(source) => {
                    // Legacy transports may list the image payloads separately
                    // and refer to them out of order via `[Image #N]` markers.
                    // Match by semantic source rather than assuming that visual
                    // order and payload order are identical.
                    let preview = previews
                        .iter()
                        .find(|preview| preview.semantic_source == source)
                        .cloned();
                    let Some(preview) = preview else {
                        continue;
                    };
                    let expanded_source = preview.source.clone();
                    let (width, height) = user_image_preview_size(preview.dimensions);
                    children.push(
                        div()
                            .id(format!("user-image-preview:{item_key}:{part_index}"))
                            .w(px(width))
                            .max_w_full()
                            .h(px(height))
                            .overflow_hidden()
                            .rounded_xs()
                            .border_1()
                            .border_color(colors.border_variant)
                            .bg(colors.editor_background)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.expanded_user_image = Some(expanded_source.clone());
                                cx.notify();
                            }))
                            .child(
                                gpui::img(preview.source)
                                    .size_full()
                                    .object_fit(ObjectFit::ScaleDown),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .children(children)
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
                        .font_harness_code(cx)
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
                            .font_harness_code(cx)
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
        let mut item = self.model.items[index].clone();
        if let Some(presentation) = subagent_activity_presentation(&item, &self.child_threads) {
            item.title = presentation.title;
            item.content = presentation.content;
        }
        if let Some(started_at) = clone_started_at {
            let elapsed = started_at.elapsed();
            if elapsed >= slow_list_item_threshold() {
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
        let visual = !rich_vim_experiment()
            && self.visual_anchor.is_some_and(|anchor| {
                (anchor.min(self.selected_item)..=anchor.max(self.selected_item)).contains(&index)
            });
        let raw_visible = self.raw_visible.contains(&item.key);
        let compact_trace = item.kind == model::TranscriptKind::Trace && !item.expanded;
        let compact_activity = transcript_item_is_compact_activity(&item);
        let compact_activity_above = index
            .checked_sub(1)
            .and_then(|index| self.model.items.get(index))
            .is_some_and(transcript_item_is_compact_activity);
        let compact_activity_below = self
            .model
            .items
            .get(index + 1)
            .is_some_and(transcript_item_is_compact_activity);
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
        let visuals = HarnessVisualTheme::from_zed(&colors);
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
        let active_search_item = self.active_search_item == Some(index);
        let active_search_ordinal = active_search_item
            .then(|| {
                self.active_search_body_offset
                    .and_then(|active_offset| {
                        rich_navigation_item_projection(&self.model, index).and_then(|projection| {
                            search_match_byte_ranges(
                                projection.body_text(),
                                &self.search_query,
                                RICH_SEARCH_HIGHLIGHT_LIMIT,
                            )
                            .iter()
                            .position(|range| range.start == active_offset)
                        })
                    })
                    // The legacy non-Editor path only knows the matching card.
                    // Preserve its previous first-occurrence emphasis there.
                    .or_else(|| (!self.search_returns_to_buffer).then_some(0))
            })
            .flatten();
        let rich_search = (self.search_highlights_visible
            && !self.search_query.trim().is_empty()
            && search_item_position.is_some())
        .then(|| RichSearchPaint::new(self.search_query.clone(), active_search_ordinal));
        let icon = icon_for_item(&item);
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
        let pending_user_delivery = item.kind == model::TranscriptKind::User
            && matches!(
                item.status.as_deref(),
                Some("sending" | "sent" | "adding to response" | "awaiting incorporation")
            );
        let visible_status = item.display_status().map(ToOwned::to_owned);
        let header_activity_summary: Option<SharedString> = match item.kind {
            model::TranscriptKind::Web => Some({
                let presentation = web_search_presentation(&item.raw);
                match presentation.queries.len() {
                    0 | 1 => format!("{} results", presentation.results.len()).into(),
                    query_count => format!(
                        "{query_count} queries · {} results",
                        presentation.results.len()
                    )
                    .into(),
                }
            }),
            model::TranscriptKind::Plan => plan_progress(&item.raw)
                .map(|(completed, total)| format!("{completed}/{total}").into()),
            _ => None,
        };
        let header_title = transcript_item_header_title(&item);
        let header_uses_command_font = command_uses_raw_identity(&item);
        let has_collapsible_content = !item.content.trim().is_empty();
        let show_header = transcript_item_shows_header(&item);
        let headerless_expanded = show_header && expanded_item_uses_content_as_header(&item);
        let render_header = show_header && !headerless_expanded;
        let header_command_status = (render_header && item.kind == model::TranscriptKind::Command)
            .then(|| item.command_execution_status())
            .flatten()
            .and_then(|status| render_command_visual_status(status, cx));
        let disclosure_weak = cx.weak_entity();
        let disclosure_item_key = item.key.clone();
        let is_disclosure = has_collapsible_content
            && (item.kind.is_structured()
                || matches!(
                    item.kind,
                    model::TranscriptKind::Reasoning | model::TranscriptKind::Plan
                ));
        let raw_search_visible = raw_visible
            && self.search_highlights_visible
            && search_contains(&item.raw.to_string(), &self.search_query);
        let search_context = (self.search_highlights_visible
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
                .font_harness_code(cx)
                .text_color(colors.text_muted)
                .child(styled)
                .into_any_element()
        });
        let ordered_user_content = (item.kind == model::TranscriptKind::User)
            .then(|| self.model.user_content_blocks(&item.key).to_vec())
            .filter(|blocks| !blocks.is_empty());
        let ordered_user_previews = self
            .user_image_previews
            .get(&item.key)
            .cloned()
            .unwrap_or_default();
        let markdown = (narrative
            && item.kind != model::TranscriptKind::Reasoning
            && item.expanded
            && !item.content.is_empty()
            && ordered_user_content.is_none())
        .then(|| {
            self.markdown_for(
                &item.key,
                &item.content,
                rich_search.as_ref(),
                rich_navigation.as_ref(),
                (active_search_item && self.search_highlights_visible)
                    .then_some(self.search_navigation_generation),
                cx,
            )
        });
        let inline_turn_status = self.transient_turn_status.clone();

        let body = if request_method.is_some() || !item.expanded {
            None
        } else if let Some(blocks) = ordered_user_content {
            Some(self.render_ordered_user_content(
                &item.key,
                blocks,
                ordered_user_previews,
                rich_search.as_ref(),
                rich_navigation.as_ref(),
                window,
                cx,
            ))
        } else if item.content.is_empty() {
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
                            this.place_rich_cursor_in_item(index, body_offset, window, cx);
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
            let element = if streaming && item.kind == model::TranscriptKind::Agent {
                let activity_color = if inline_turn_status.is_some() {
                    cx.theme().status().warning
                } else {
                    colors.text_accent
                };
                element
                    .with_animation(
                        format!("streaming-agent-activity-{}", item.key),
                        gpui::Animation::new(Duration::from_millis(1000)).repeat_synced(),
                        move |element, delta| {
                            let activity = inline_turn_status
                                .as_ref()
                                .map(|status| status.clone())
                                .unwrap_or_default();
                            element.trailing_spinner_activity(activity, activity_color, delta)
                        },
                    )
                    .into_any_element()
            } else {
                element.into_any_element()
            };
            Some(
                div()
                    .id(format!("rich-markdown-click:{}", item.key))
                    .w_full()
                    .min_w_0()
                    .when(rich_vim_experiment(), |this| {
                        this.on_click(|_, _, cx| cx.stop_propagation())
                    })
                    .child(element)
                    .into_any_element(),
            )
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
                model::TranscriptKind::Image => self.render_image(
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
        let header_search = render_header.then_some(rich_search.as_ref()).flatten();
        // Every expanded Rich structured body is complete and scrollable. If
        // a renderer deliberately has no glyph for a protocol-only offset,
        // keep Vim visible on the header instead of mounting a second,
        // progressively expanded copy of the body.
        let body_left_navigation_unclaimed = !rich_item_defers_navigation_claim(&item)
            && rich_navigation
                .as_ref()
                .is_some_and(|navigation| !navigation.cursor_claimed.get());
        let header_cursor_range = render_header
            .then(|| {
                rich_header_navigation_range(
                    &header_title,
                    rich_navigation.as_ref(),
                    !rich_item_body_paints_navigation(&item) || body_left_navigation_unclaimed,
                )
            })
            .flatten();
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

        let header = rich_card_identity_row(cx)
            .id(format!("item-header:{}", item.key))
            .when(!narrative && !compact_trace, |this| {
                this.px_1().bg(visuals.tool_header_surface)
            })
            .child(rich_card_identity_icon(
                icon,
                IconSize::Small,
                transcript_icon_color(item.kind),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(header_uses_command_font, |this| this.font_harness_code(cx))
                    .when(!header_uses_command_font, |this| {
                        this.font_harness_reading(cx)
                    })
                    .text_color(colors.text_muted)
                    .child(highlighted_header_title),
            )
            .when_some(header_activity_summary, |this, summary| {
                this.child(
                    div()
                        .flex_none()
                        .font_harness_reading(cx)
                        .text_color(colors.text_muted)
                        .child(summary),
                )
            })
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
            .when_some(header_command_status, |this, (_, status)| {
                this.child(status)
            })
            .when(is_disclosure, |this| {
                this.cursor_pointer()
                    .on_click(move |_, window, cx| {
                        disclosure_weak
                            .update(cx, |this, cx| {
                                this.toggle_item_by_key(&disclosure_item_key, window, cx)
                            })
                            .ok();
                    })
                    .child(Disclosure::new(
                        format!("item-disclosure:{}", item.key),
                        item.expanded,
                    ))
            });

        let floating_disclosure = (headerless_expanded && is_disclosure).then(|| {
            let disclosure_weak = cx.weak_entity();
            let disclosure_item_key = item.key.clone();
            div()
                .id(format!("item-floating-disclosure:{}", item.key))
                .absolute()
                .top(px(1.))
                .right(px(1.))
                .size(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_pointer()
                .on_click(move |_, window, cx| {
                    disclosure_weak
                        .update(cx, |this, cx| {
                            this.toggle_item_by_key(&disclosure_item_key, window, cx)
                        })
                        .ok();
                })
                .child(Disclosure::new(
                    format!("item-floating-disclosure-icon:{}", item.key),
                    true,
                ))
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
                .font_harness_code(cx)
                .text_color(colors.text_muted)
                .child(highlighted)
                .into_any_element()
        });

        let content = if narrative {
            let narrative_panel = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .when(item.kind == model::TranscriptKind::User, |this| {
                    this.rounded_sm()
                        .border_1()
                        .border_color(visuals.divider)
                        .bg(visuals.raised_surface)
                        .px_2()
                        .py_1()
                })
                .when(pending_user_delivery, |this| this.opacity(0.58))
                .when(item.kind == model::TranscriptKind::Reasoning, |this| {
                    this.gap_1().py_1()
                })
                .when(item.kind == model::TranscriptKind::Plan, |this| {
                    this.gap_1().py_0p5()
                })
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
                .into_any_element();

            narrative_panel
        } else {
            let flush_tool_surface = matches!(
                item.kind,
                model::TranscriptKind::Command
                    | model::TranscriptKind::Diff
                    | model::TranscriptKind::FileChange
            );
            div()
                .w_full()
                .relative()
                .flex()
                .flex_col()
                .when(!compact_trace, |this| {
                    this.rounded_md()
                        .border_1()
                        .border_color(colors.border.opacity(0.6))
                        .bg(colors.editor_background)
                        .overflow_hidden()
                })
                .when(
                    matches!(
                        item.command_execution_status(),
                        Some(model::CommandExecutionStatus::Failed(_))
                    ),
                    |this| {
                        this.border_dashed()
                            .border_color(cx.theme().status().error.opacity(0.62))
                    },
                )
                .when(compact_trace, |this| this.px_1().py_0p5().gap_0p5())
                .when(render_header, |this| this.child(header))
                .when_some(search_context, |this, context| {
                    this.child(
                        div()
                            .when(!flush_tool_surface, |this| this.px_2().pt_1())
                            .child(context),
                    )
                })
                .when_some(request_surface, |this, surface| {
                    this.child(div().px_2().py_1().child(surface))
                })
                .when_some(body, |this, body| {
                    if flush_tool_surface || compact_trace {
                        this.child(body)
                    } else {
                        this.child(div().px_2().py_1().child(body))
                    }
                })
                .when_some(raw, |this, raw| this.child(div().px_2().pb_1().child(raw)))
                .when_some(pending_summary, |this, summary| {
                    this.child(div().px_2().py_1().child(summary))
                })
                .when_some(user_input, |this, input| {
                    this.child(div().px_2().py_1().child(input))
                })
                .when_some(mcp_elicitation, |this, elicitation| {
                    this.child(div().px_2().py_1().child(elicitation))
                })
                .when(has_approval, |this| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .border_t_1()
                            .border_color(colors.border_variant)
                            .flex()
                            .flex_wrap()
                            .gap_0p5()
                            .children(choice_buttons),
                    )
                })
                .when_some(floating_disclosure, |this, disclosure| {
                    this.child(disclosure)
                })
                .into_any_element()
        };

        let normal_vertical_padding = if compact_trace {
            px(3.)
        } else if !narrative {
            if narrow { px(4.) } else { px(5.) }
        } else {
            px(8.)
        };
        let element = div()
            .id(("transcript-item", index))
            .w_full()
            .px(if narrow { px(10.) } else { px(18.) })
            .pt(if compact_activity && compact_activity_above {
                px(0.)
            } else {
                normal_vertical_padding
            })
            .pb(if compact_activity && compact_activity_below {
                px(1.)
            } else {
                normal_vertical_padding
            })
            .when(visual, |this| {
                this.bg(visuals.selection_surface.opacity(0.45))
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
            if elapsed >= slow_list_item_threshold() {
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

    fn render_transcript_list_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if index < self.model.items.len() {
            return self.render_item(index, window, cx);
        }
        if index != self.model.items.len() || !self.turn_active() {
            return div().into_any_element();
        }

        let narrow = window.viewport_size().width < px(720.);
        let transient_status = self.transient_turn_status.clone();
        let inline_activity = transcript_has_inline_activity(&self.model);
        if inline_activity {
            // Keep the sentinel as a real list item so tail following remains
            // stable, but let the streaming Markdown own the visible activity
            // glyph and any transient transport status at its actual insertion
            // point.
            return div()
                .id("transcript-turn-tail")
                .w_full()
                .h(px(1.))
                .into_any_element();
        }
        let activity_color = if transient_status.is_some() {
            Color::Warning
        } else {
            Color::Accent
        };
        div()
            .id("transcript-turn-tail")
            .w_full()
            .px(if narrow { px(10.) } else { px(18.) })
            .pt_1()
            .pb_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(20.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                SpinnerLabel::dots()
                                    .size(LabelSize::Large)
                                    .color(activity_color),
                            ),
                    )
                    .when_some(transient_status, |this, status| {
                        this.child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    }),
            )
            .into_any_element()
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
        let visuals = HarnessVisualTheme::from_zed(&colors);
        let compact = window.viewport_size().width < px(COMPACT_SIDEBAR_THRESHOLD);
        let sidebar_visible = self.sidebar_open && (!compact || self.sidebar_user_override);
        let composer_text = self.composer.read(cx).text(cx);
        let composer_empty = composer_is_empty(&composer_text, self.composer_images.len());
        let send_blocked = composer_send_blocked(
            composer_empty,
            self.loading_thread,
            self.attaching_thread,
            self.settings_update_pending,
            self.thread_read_only_reason.is_some(),
            self.client.is_some() || self.replay_count.is_some(),
        );
        let composer_status: Option<(SharedString, Color)> =
            if let Some(error) = self.composer_attachment_error.clone() {
                Some((error, Color::Warning))
            } else if let Some(status) = self.performance_status.clone() {
                Some((status, Color::Muted))
            } else if self.loading_thread {
                Some(("Loading task history…".into(), Color::Muted))
            } else if self.attaching_thread {
                Some(("Connecting live…".into(), Color::Muted))
            } else if self.settings_update_pending {
                Some(("Updating task settings…".into(), Color::Muted))
            } else if self.thread_read_only_reason.is_some() {
                Some(("Read-only · Ctrl-N for a new thread".into(), Color::Warning))
            } else if self.connecting {
                Some(("Connecting…".into(), Color::Muted))
            } else if self.client.is_none() && self.replay_count.is_none() {
                Some(("Offline · refresh to reconnect".into(), Color::Warning))
            } else {
                None
            };
        let turn_active = self.turn_active();
        self.sync_turn_tail_indicator(turn_active);
        let list_state = self.list_state.clone();
        let task_list_state = self.task_list_state.clone();
        let command_palette = self.command_palette.clone();
        let expanded_user_image = self.expanded_user_image.clone();
        let appearance_settings = self
            .appearance_settings_open
            .then(|| self.render_appearance_settings(window, cx));
        let task_body = if self.replay_count.is_some() {
            div()
                .flex_1()
                .min_h_0()
                .child(self.render_replay_task(cx))
                .into_any_element()
        } else if self.sidebar_threads.is_empty() {
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
                    uniform_list(
                        "tasks",
                        self.sidebar_threads.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            range.map(|index| this.render_task(index, cx)).collect()
                        }),
                    )
                    .track_scroll(&task_list_state)
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
                .bg(visuals.transcript)
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
                        cx.processor(|this, index, window, cx| {
                            this.render_transcript_list_item(index, window, cx)
                        }),
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
            let show_latest = !list_state.is_following_tail();
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(visuals.transcript)
                .child(rich_list)
                .when(show_latest, |this| {
                    this.child(
                        div().absolute().right(px(10.)).bottom(px(10.)).child(
                            div()
                                .relative()
                                .child(
                                    IconButton::new("transcript-latest", IconName::ArrowDown)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Compact)
                                        .style(ButtonStyle::Subtle)
                                        .icon_color(if turn_active {
                                            Color::Accent
                                        } else {
                                            Color::Muted
                                        })
                                        .aria_label("Return to the live transcript")
                                        .tooltip(Tooltip::text(
                                            "Return to the live transcript · G or Ctrl-End",
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.go_to_transcript_tail(window, cx)
                                        })),
                                )
                                .when(turn_active, |this| {
                                    this.child(
                                        div()
                                            .absolute()
                                            .top(px(1.))
                                            .right(px(1.))
                                            .size(px(5.))
                                            .rounded_full()
                                            .bg(colors.text_accent),
                                    )
                                }),
                        ),
                    )
                })
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
        let command_line_input = self
            .search_visible
            .then(|| self.search_editor.read(cx).text(cx));
        let command_line_status: Option<(SharedString, Color)> = match self.vim_command_line {
            Some(VimCommandLine::Ex) => self
                .command_line_error
                .clone()
                .map(|error| (error, Color::Error)),
            Some(VimCommandLine::Search { .. })
                if command_line_input.as_deref() != Some(self.search_query.as_str()) =>
            {
                None
            }
            Some(VimCommandLine::Search { .. }) if self.search_match_count == 0 => {
                (!self.search_query.is_empty()).then(|| ("No matches".into(), Color::Muted))
            }
            Some(VimCommandLine::Search { .. }) => Some((
                format!(
                    "{} / {}",
                    self.active_search_match.saturating_add(1),
                    self.search_match_count
                )
                .into(),
                Color::Muted,
            )),
            None => None,
        };
        let command_line_prompt = self
            .vim_command_line
            .map(VimCommandLine::prompt)
            .unwrap_or_default();
        let context_usage = self.render_context_usage(cx);
        let model_selector = self.render_model_effort_selector(cx);
        let permission_selector = self.render_permission_selector(cx);
        let composer_action =
            self.render_composer_action(turn_active, composer_empty, send_blocked, cx);
        let queued_turns = self.render_queued_turns(cx);

        div()
            .key_context(self.key_context())
            .track_focus(&self.transcript_focus)
            .size_full()
            .flex()
            .bg(visuals.canvas)
            .text_color(colors.text)
            // Reading typography is the inherited prose/UI role. Individual
            // code surfaces override it with `font_harness_code`, while plain
            // transcript fallbacks and compact activity text now honor the
            // same configured weight as rich Markdown.
            .font_harness_reading(cx)
            .on_action(cx.listener(|this, _: &Send, window, cx| this.send(window, cx)))
            .on_action(cx.listener(|this, _: &Steer, window, cx| this.steer(window, cx)))
            .on_action(
                cx.listener(|this, _: &PasteComposer, window, cx| this.paste_composer(window, cx)),
            )
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
                this.open_ex_command("", window, cx)
            }))
            .on_action(cx.listener(|this, action: &OpenWithQuery, window, cx| {
                this.open_ex_command(&action.query, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenActionPalette, window, cx| {
                this.open_command_palette("", window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoTop, _, cx| {
                if this.focus_mode == FocusMode::Tasks {
                    this.selected_task = 0;
                    this.task_list_state.scroll_to_item(0, ScrollStrategy::Top);
                } else {
                    this.selected_item = 0;
                    this.list_state.scroll_to(gpui::ListOffset::default());
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &GoBottom, window, cx| {
                if this.focus_mode == FocusMode::Tasks {
                    this.selected_task = this.sidebar_threads.len().saturating_sub(1);
                    this.task_list_state.scroll_to_bottom();
                } else {
                    this.go_to_transcript_tail(window, cx);
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
                if !vim_search_available(this.buffer_view, rich_vim_experiment()) {
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
                this.search_returns_to_buffer = true;
                this.sync_native_search_state(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, action: &VimWordPrevious, window, cx| {
                if !vim_search_available(this.buffer_view, rich_vim_experiment()) {
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
                this.search_returns_to_buffer = true;
                this.sync_native_search_state(cx);
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &CommitSearch, window, cx| this.commit_search(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseSearch, window, cx| this.close_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ClearSearchHighlights, _, cx| {
                this.clear_search_highlights(cx)
            }))
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
                        .border_color(visuals.strong_divider)
                        .bg(visuals.rail)
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
                                .border_color(visuals.divider)
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
                                .child(self.render_appearance_selector(cx))
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
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .flex_none()
                                .px_4()
                                .py_2()
                                .border_b_1()
                                .border_color(visuals.error_border)
                                .bg(visuals.error_surface)
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
                    .when_some(queued_turns, |this, queued_turns| this.child(queued_turns))
                    .child(
                        div()
                            .flex_none()
                            .border_t_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .py_2()
                            .px_2()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .pt_1()
                                    .pr_2()
                                    .child(self.composer.clone()),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .flex_none()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_1()
                                    .when(self.search_visible, |this| {
                                        this.key_context("HarnessSearch")
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .flex()
                                                    .items_center()
                                                    .child(
                                                        div()
                                                            .h_full()
                                                            .flex_none()
                                                            .flex()
                                                            .items_center()
                                                            .font_ui(cx)
                                                            .text_ui_sm(cx)
                                                            .text_color(colors.text)
                                                            .child(command_line_prompt),
                                                    )
                                                    .child(self.search_editor.clone()),
                                            )
                                            .when_some(
                                                command_line_status,
                                                |this, (status, color)| {
                                                    this.child(
                                                        Label::new(status)
                                                            .size(LabelSize::XSmall)
                                                            .color(color),
                                                    )
                                                },
                                            )
                                    })
                                    .when(!self.search_visible, |this| {
                                        this.justify_between()
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1()
                                                    .child(self.mode_indicator.clone())
                                                    .when_some(
                                                        composer_status.clone(),
                                                        |this, (status, color)| {
                                                            this.when(
                                                                matches!(
                                                                    color,
                                                                    Color::Muted
                                                                        if self.loading_thread
                                                                            || self.attaching_thread
                                                                            || self.settings_update_pending
                                                                            || self.connecting
                                                                ),
                                                                |this| {
                                                                    this.child(
                                                                        SpinnerLabel::dots()
                                                                            .size(LabelSize::Small)
                                                                            .color(Color::Muted),
                                                                    )
                                                                },
                                                            )
                                                            .child(
                                                                Label::new(status)
                                                                    .size(LabelSize::Small)
                                                                    .color(color)
                                                                    .truncate(),
                                                            )
                                                        },
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1()
                                                    .when_some(context_usage, |this, usage| {
                                                        this.child(usage)
                                                    })
                                                    .child(permission_selector)
                                                    .child(model_selector)
                                                    .child(composer_action),
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
            .when_some(appearance_settings, |this, appearance_settings| {
                this.child(deferred(appearance_settings).with_priority(2))
            })
            .when_some(expanded_user_image, |this, image| {
                this.child(
                    deferred(
                        div()
                            .absolute()
                            .inset_0()
                            .p_6()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(gpui::black().opacity(0.78))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.expanded_user_image = None;
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .relative()
                                    .w(relative(0.94))
                                    .h(relative(0.9))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        gpui::img(image)
                                            .size_full()
                                            .object_fit(ObjectFit::ScaleDown),
                                    )
                                    .child(
                                        div()
                                            .absolute()
                                            .top_0()
                                            .right_0()
                                            .rounded_sm()
                                            .bg(colors.editor_background.opacity(0.9))
                                            .child(
                                                IconButton::new(
                                                    "close-user-image-preview",
                                                    IconName::Close,
                                                )
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Default)
                                                .style(ButtonStyle::Subtle)
                                                .aria_label("Close image preview")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.expanded_user_image = None;
                                                    cx.notify();
                                                })),
                                            ),
                                    ),
                            ),
                    )
                    .with_priority(3),
                )
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

#[cfg(test)]
fn route_server_request(
    method: &str,
    params: &Value,
    selected_thread_id: Option<&str>,
) -> RequestRoute {
    if !request_matches_thread(params, selected_thread_id) {
        return RequestRoute::Immediate(safe_request_rejection(method, params));
    }

    route_matching_server_request(method, params)
}

fn route_server_request_with_background(
    method: &str,
    params: &Value,
    selected_thread_id: Option<&str>,
    background_parent_thread_id: Option<&str>,
) -> RequestRoute {
    if request_matches_thread(params, selected_thread_id) {
        return route_matching_server_request(method, params);
    }
    if let Some(parent_thread_id) = background_parent_thread_id
        && request_matches_thread(params, Some(parent_thread_id))
    {
        return match route_matching_server_request(method, params) {
            RequestRoute::Interactive => RequestRoute::ReturnToThread(parent_thread_id.into()),
            route => route,
        };
    }
    RequestRoute::Immediate(safe_request_rejection(method, params))
}

fn route_matching_server_request(method: &str, params: &Value) -> RequestRoute {
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

/// Mirror Zed's ACP tool-kind icon vocabulary even though the Codex app-server
/// transcript does not expose ACP's enum directly. The title is structured by
/// Harness's protocol projector (`server · tool`), so this is semantic
/// projection rather than a renderer-specific list of individual tool names.
fn icon_for_item(item: &TranscriptItem) -> IconName {
    if item.kind != model::TranscriptKind::Tool {
        return icon_for_kind(item.kind);
    }

    let title = item.title.to_ascii_lowercase();
    if title.contains("web")
        || title.contains("fetch")
        || title.contains("browser")
        || title.contains("http")
    {
        IconName::ToolWeb
    } else if title.contains("read") || title.contains("search") || title.contains("find") {
        IconName::ToolSearch
    } else if title.contains("delete") || title.contains("remove") {
        IconName::ToolDeleteFile
    } else if title.contains("edit")
        || title.contains("write")
        || title.contains("patch")
        || title.contains("apply")
    {
        IconName::ToolPencil
    } else if title.contains("command")
        || title.contains("shell")
        || title.contains("exec")
        || title.contains("terminal")
    {
        IconName::ToolTerminal
    } else if title.contains("think") || title.contains("reason") {
        IconName::ToolThink
    } else {
        IconName::ToolHammer
    }
}

fn transcript_icon_color(kind: model::TranscriptKind) -> Color {
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

fn child_thread_identity(thread: &CodexThread) -> String {
    let mut identity = subagent_identity(thread);
    if thread.can_accept_direct_input == Some(false) {
        identity.push_str(" · Read only");
    }
    identity
}

fn listed_thread_status(thread: &CodexThread) -> AgentThreadStatus {
    match &thread.status {
        CodexThreadStatus::Active { active_flags } => {
            if active_flags
                .iter()
                .any(|flag| status_requires_user_confirmation(flag))
            {
                AgentThreadStatus::WaitingForConfirmation
            } else {
                AgentThreadStatus::Running
            }
        }
        CodexThreadStatus::SystemError => AgentThreadStatus::Error,
        CodexThreadStatus::NotLoaded | CodexThreadStatus::Idle => AgentThreadStatus::Completed,
        CodexThreadStatus::Unknown(value) => {
            let status = value
                .as_str()
                .or_else(|| value.get("type").and_then(Value::as_str))
                .unwrap_or_default()
                .to_ascii_lowercase();
            if status.contains("error") || status.contains("fail") {
                AgentThreadStatus::Error
            } else if status_requires_user_confirmation(&status) {
                AgentThreadStatus::WaitingForConfirmation
            } else if status.contains("active")
                || status.contains("running")
                || status.contains("progress")
                || status.contains("wait")
            {
                AgentThreadStatus::Running
            } else {
                AgentThreadStatus::Completed
            }
        }
    }
}

fn status_requires_user_confirmation(status: &str) -> bool {
    let status = status.to_ascii_lowercase();
    status.contains("approval")
        || status.contains("confirm")
        || status.contains("request_user_input")
        || status.contains("user_input")
        || status.contains("user input")
        || status.contains("userinput")
}

fn active_thread_turn_id(thread: &CodexThread) -> Option<&str> {
    thread.turns.iter().rev().find_map(|turn| {
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
        .then_some(turn.id.as_str())
    })
}

fn thread_has_active_turn(thread: &CodexThread) -> bool {
    active_thread_turn_id(thread).is_some()
}

fn lifecycle_ended_active_turn(events: &[model::TurnLifecycleEvent]) -> bool {
    events.iter().fold(false, |ended, event| match event {
        model::TurnLifecycleEvent::Started { .. } => false,
        model::TurnLifecycleEvent::Completed {
            was_active: true, ..
        } => true,
        model::TurnLifecycleEvent::Completed {
            was_active: false, ..
        } => ended,
    })
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

fn replay_streaming() -> bool {
    std::env::args().any(|argument| argument == "--replay-streaming")
        || std::env::var_os("HARNESS_REPLAY_STREAMING")
            .is_some_and(|value| !value.is_empty() && value != std::ffi::OsStr::new("0"))
}

fn automatic_performance_j() -> bool {
    std::env::args().any(|argument| argument == "--perf-j")
}

fn automatic_performance_scroll() -> bool {
    std::env::args().any(|argument| argument == "--perf-scroll")
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

fn thread_load_diagnostics_enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("HARNESS_THREAD_LOAD_TRACE")
            .is_some_and(|value| !value.is_empty() && value != std::ffi::OsStr::new("0"))
    });
    *ENABLED
}

fn load_harness_keymaps(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-p", OpenActionPalette, Some("Harness")),
        // In the modal composer, Ctrl-V should behave like an ordinary app
        // paste while typing: image clipboard entries become attachments and
        // text entries flow through the native Editor paste path. Keep this
        // insert-specific so Vim's Normal/Visual Ctrl-V semantics remain
        // available outside the composer typing state.
        KeyBinding::new(
            "ctrl-v",
            PasteComposer,
            Some("HarnessComposer && Editor && vim_mode == insert"),
        ),
        KeyBinding::new(
            "ctrl-shift-v",
            PasteComposer,
            Some("HarnessComposer && Editor"),
        ),
        KeyBinding::new(
            "shift-insert",
            PasteComposer,
            Some("HarnessComposer && Editor"),
        ),
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
        // Rich mode deliberately keeps the real Vim Editor focused behind the
        // structured projection. Override only its Normal-mode document-end
        // command so both that Editor and the visible List reach the same live
        // edge atomically.
        KeyBinding::new(
            "shift-g",
            GoBottom,
            Some("HarnessBuffer && Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "ctrl-end",
            GoBottom,
            Some("HarnessTranscript || HarnessBuffer"),
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
        KeyBinding::new("ctrl-[", CloseSearch, Some("HarnessSearch")),
        KeyBinding::new(
            "escape",
            CloseSearch,
            Some("HarnessTranscript && HarnessSearchVisible"),
        ),
        KeyBinding::new(
            "ctrl-[",
            CloseSearch,
            Some("HarnessTranscript && HarnessSearchVisible"),
        ),
        KeyBinding::new(
            "escape",
            CloseSearch,
            Some("HarnessBuffer && HarnessSearchVisible"),
        ),
        KeyBinding::new(
            "ctrl-[",
            CloseSearch,
            Some("HarnessBuffer && HarnessSearchVisible"),
        ),
        KeyBinding::new("j", MoveDown, Some("HarnessRequest && !Editor")),
        KeyBinding::new("k", MoveUp, Some("HarnessRequest && !Editor")),
        KeyBinding::new("h", MoveLeft, Some("HarnessRequest && !Editor")),
        KeyBinding::new("l", MoveRight, Some("HarnessRequest && !Editor")),
        KeyBinding::new("enter", ChooseRequest, Some("HarnessRequest && !Editor")),
        KeyBinding::new("i", EditRequest, Some("HarnessRequest && !Editor")),
        KeyBinding::new("ctrl-enter", SubmitRequest, Some("HarnessRequest")),
        KeyBinding::new("escape", ReturnFromRequest, Some("HarnessRequest")),
        KeyBinding::new("ctrl-[", ReturnFromRequest, Some("HarnessRequest")),
        KeyBinding::new("h", MoveLeft, Some("HarnessApproval")),
        KeyBinding::new("l", MoveRight, Some("HarnessApproval")),
        KeyBinding::new("enter", ChooseApproval, Some("HarnessApproval")),
        KeyBinding::new("escape", ReturnFromRequest, Some("HarnessApproval")),
        KeyBinding::new("ctrl-[", ReturnFromRequest, Some("HarnessApproval")),
        KeyBinding::new("ctrl-n", NewTask, Some("Harness")),
        KeyBinding::new("ctrl-r", RefreshTasks, Some("HarnessTranscript")),
        KeyBinding::new("ctrl-b", ToggleSidebar, Some("Harness")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AssetSource as _;

    fn cached_thread(id: &str, updated_at: i64) -> CodexThread {
        CodexThread {
            id: id.into(),
            name: None,
            preview: String::new(),
            cwd: String::new(),
            updated_at,
            turns: Vec::new(),
            ..CodexThread::default()
        }
    }

    fn child_thread(id: &str, parent_id: &str, updated_at: i64) -> CodexThread {
        CodexThread {
            id: id.into(),
            parent_thread_id: Some(parent_id.into()),
            updated_at,
            ..CodexThread::default()
        }
    }

    fn subagent_item(title: &str, content: &str, raw: Value) -> TranscriptItem {
        TranscriptItem {
            key: "subagent-item".into(),
            protocol_id: Some("subagent-item".into()),
            kind: model::TranscriptKind::Subagent,
            title: title.into(),
            status: Some("waiting".into()),
            content: content.into(),
            raw,
            event_count: 1,
            expanded: true,
            pending_request: None,
        }
    }

    #[test]
    fn child_thread_registry_reconciles_by_thread_id() {
        let mut registry = ChildThreadRegistry::default();
        registry.reconcile(vec![
            child_thread("child-a", "root", 1),
            child_thread("child-b", "root", 2),
        ]);

        registry.reconcile(vec![
            child_thread("child-a", "root", 9),
            child_thread("child-c", "root", 3),
            cached_thread("not-a-child", 4),
        ]);

        assert_eq!(registry.by_id.len(), 2);
        assert_eq!(registry.get("child-a").unwrap().updated_at, 9);
        assert!(registry.get("child-b").is_none());
        assert!(registry.get("child-c").is_some());
        assert!(registry.get("not-a-child").is_none());
    }

    #[test]
    fn unchanged_registry_and_render_projection_do_not_mutate_history() {
        let child = child_thread("child", "root", 2);
        let mut registry = ChildThreadRegistry::default();
        assert!(registry.reconcile(vec![child.clone()]));
        assert!(!registry.reconcile(vec![child]));

        let item = subagent_item(
            "Subagent · Wait",
            "Agents\nchild · Waiting",
            json!({"tool": "wait", "receiverThreadIds": ["child"]}),
        );
        let original = (item.title.clone(), item.content.clone(), item.event_count);
        let _ = subagent_activity_presentation(&item, &registry);
        assert_eq!(
            (item.title.clone(), item.content.clone(), item.event_count),
            original
        );
    }

    #[test]
    fn sidebar_thread_rows_nest_typed_descendants_beneath_roots() {
        let roots = vec![cached_thread("root-a", 1), cached_thread("root-b", 2)];
        let mut registry = ChildThreadRegistry::default();
        registry.reconcile(vec![
            child_thread("older-child", "root-a", 10),
            child_thread("newer-child", "root-a", 20),
            child_thread("grandchild", "older-child", 30),
            child_thread("orphan", "missing-root", 40),
        ]);

        let rows = sidebar_thread_rows(&roots, &registry);
        assert_eq!(
            rows.iter()
                .map(|row| (row.thread_id.as_str(), row.depth, row.root_index))
                .collect::<Vec<_>>(),
            vec![
                ("root-a", 0, Some(0)),
                ("newer-child", 1, None),
                ("older-child", 1, None),
                ("grandchild", 2, None),
                ("root-b", 0, Some(1)),
            ]
        );
    }

    #[test]
    fn sidebar_selection_tracks_child_ids_and_clamps_when_they_disappear() {
        let rows = vec![
            SidebarThreadRow {
                thread_id: "root".into(),
                depth: 0,
                root_index: Some(0),
            },
            SidebarThreadRow {
                thread_id: "child".into(),
                depth: 1,
                root_index: None,
            },
        ];

        assert_eq!(sidebar_selection_index(&rows, Some("child"), 0), 1);
        assert_eq!(sidebar_selection_index(&rows[..1], Some("child"), 8), 0);
        assert_eq!(sidebar_selection_index(&[], Some("child"), 8), 0);
    }

    #[test]
    fn subagent_activity_projects_compact_identity_without_rewriting_event_status() {
        let mut child = child_thread("child", "root", 2);
        child.agent_nickname = Some("Atlas".into());
        child.agent_role = Some("researcher".into());
        child.status = CodexThreadStatus::Active {
            active_flags: vec!["running".into()],
        };
        child.source = CodexSessionSource::SubAgent(CodexSubagentSource::ThreadSpawn(
            codex_app_server_client::CodexThreadSpawnSource {
                parent_thread_id: "root".into(),
                depth: 1,
                agent_nickname: Some("Atlas".into()),
                agent_role: Some("researcher".into()),
                agent_path: Some("/root/atlas".into()),
            },
        ));
        let mut registry = ChildThreadRegistry::default();
        registry.reconcile(vec![child]);
        let item = subagent_item(
            "Subagent · Wait",
            "Agents\nchild · Responding\nFound the protocol fields",
            json!({
                "tool": "wait",
                "agentsStates": {
                    "child": {"status": "responding", "message": "Found the protocol fields"}
                }
            }),
        );

        let presentation = subagent_activity_presentation(&item, &registry).unwrap();
        assert_eq!(presentation.title, "Subagent · Wait · Atlas");
        assert_eq!(
            presentation.content,
            "Agents\nchild · Responding\nFound the protocol fields"
        );
        assert!(!presentation.content.contains("Running"));
    }

    #[test]
    fn unidentified_subagent_wait_becomes_quiet_parent_coordination() {
        let item = subagent_item(
            "Subagent · Wait",
            "Subagent state unavailable",
            json!({"tool": "wait"}),
        );

        let presentation =
            subagent_activity_presentation(&item, &ChildThreadRegistry::default()).unwrap();
        assert_eq!(presentation.title, "Subagent coordination · Wait");
        assert!(presentation.content.is_empty());
        assert!(!presentation.content.contains("unavailable"));
    }

    #[test]
    fn agent_path_only_activity_survives_without_registry_identity() {
        let item = subagent_item(
            "Subagent · Message",
            "/root/researcher",
            json!({"type": "subAgentActivity", "kind": "message", "agentPath": "/root/researcher"}),
        );

        assert!(subagent_activity_presentation(&item, &ChildThreadRegistry::default()).is_none());
        assert_eq!(item.title, "Subagent · Message");
        assert_eq!(item.content, "/root/researcher");
    }

    #[test]
    fn collaboration_actions_keep_their_historical_titles() {
        let mut child = child_thread("child", "root", 2);
        child.agent_nickname = Some("Atlas".into());
        let mut registry = ChildThreadRegistry::default();
        registry.reconcile(vec![child]);

        for action in ["Spawn", "Wait", "Send input", "Close"] {
            let item = subagent_item(
                &format!("Subagent · {action}"),
                "Agents\nchild · Completed",
                json!({"tool": action, "receiverThreadIds": ["child"]}),
            );
            let presentation = subagent_activity_presentation(&item, &registry).unwrap();
            assert!(presentation.title.contains(action));
            assert!(presentation.title.contains("Atlas"));
            assert_eq!(presentation.content, "Agents\nchild · Completed");
        }
    }

    #[test]
    fn child_identity_prefers_nickname_then_role_then_typed_path() {
        let mut child = child_thread("child", "root", 2);
        child.agent_nickname = Some("Atlas".into());
        child.agent_role = Some("researcher".into());
        assert_eq!(child_thread_identity(&child), "Atlas");

        child.agent_nickname = None;
        assert_eq!(child_thread_identity(&child), "researcher");

        child.agent_nickname = Some("   ".into());
        assert_eq!(child_thread_identity(&child), "researcher");

        child.agent_role = None;
        child.source = CodexSessionSource::SubAgent(CodexSubagentSource::ThreadSpawn(
            codex_app_server_client::CodexThreadSpawnSource {
                parent_thread_id: "root".into(),
                depth: 1,
                agent_nickname: None,
                agent_role: None,
                agent_path: Some("/root/atlas".into()),
            },
        ));
        assert_eq!(child_thread_identity(&child), "/root/atlas");
    }

    #[test]
    fn ordinary_wait_flags_are_running_not_confirmation() {
        let mut child = child_thread("child", "root", 2);
        child.status = CodexThreadStatus::Active {
            active_flags: vec!["waiting".into()],
        };
        assert_eq!(listed_thread_status(&child), AgentThreadStatus::Running);

        child.status = CodexThreadStatus::Active {
            active_flags: vec!["waiting_for_approval".into()],
        };
        assert_eq!(
            listed_thread_status(&child),
            AgentThreadStatus::WaitingForConfirmation
        );
    }

    #[test]
    fn collaboration_notifications_request_hierarchy_refresh() {
        assert!(event_refreshes_child_hierarchy(
            &AppServerEvent::Notification {
                method: "item/started".into(),
                params: json!({"item": {"type": "collabAgentToolCall", "tool": "spawnAgent"}}),
            }
        ));
        assert!(event_refreshes_child_hierarchy(
            &AppServerEvent::Notification {
                method: "item/completed".into(),
                params: json!({"item": {"type": "subAgentActivity", "agentPath": "/root/a"}}),
            }
        ));
        assert!(!event_refreshes_child_hierarchy(
            &AppServerEvent::Notification {
                method: "item/completed".into(),
                params: json!({"item": {"type": "commandExecution"}}),
            }
        ));
    }

    #[test]
    fn read_only_child_inspection_blocks_only_live_unresolved_requests() {
        assert!(child_inspection_blocked(true, true));
        assert!(!child_inspection_blocked(true, false));
        assert!(!child_inspection_blocked(false, true));
    }

    #[test]
    fn inspected_child_hides_parent_queue_and_rejects_child_owned_callbacks() {
        assert!(queue_state_belongs_to_thread(
            "parent",
            Some("child"),
            Some("parent")
        ));
        assert!(!queue_state_belongs_to_thread(
            "child",
            Some("child"),
            Some("parent")
        ));
        assert!(!queue_state_is_visible(Some("child"), Some("parent")));
        assert!(!callback_origin_is_visible("parent", Some("child")));

        assert!(
            queue_state_is_visible(Some("parent"), Some("parent")),
            "the preserved queue becomes visible again only on its owning parent"
        );
        assert!(callback_origin_is_visible("parent", Some("parent")));
    }

    #[test]
    fn writable_child_uses_its_own_queue_instead_of_preserving_parent_queue() {
        assert!(queue_state_is_visible(Some("writable-child"), None));
        assert!(queue_state_belongs_to_thread(
            "writable-child",
            Some("writable-child"),
            None
        ));
        assert!(!queue_state_belongs_to_thread(
            "parent",
            Some("writable-child"),
            None
        ));
    }

    #[test]
    fn leaving_a_child_resolves_its_live_request_even_when_parent_work_is_preserved() {
        assert!(reject_pending_requests_on_switch(true, true));
        assert!(!reject_pending_requests_on_switch(true, false));
        assert!(reject_pending_requests_on_switch(false, false));
    }

    #[test]
    fn resumed_thread_reconstructs_only_its_latest_active_turn() {
        let mut thread = cached_thread("parent", 1);
        thread.turns = vec![
            codex_app_server_client::CodexTurn {
                id: "turn-a".into(),
                status: json!("completed"),
                items: Vec::new(),
            },
            codex_app_server_client::CodexTurn {
                id: "turn-b".into(),
                status: json!({"type": "inProgress"}),
                items: Vec::new(),
            },
        ];

        assert_eq!(active_thread_turn_id(&thread), Some("turn-b"));
        assert!(thread_has_active_turn(&thread));
        thread.turns[1].status = json!("completed");
        assert_eq!(active_thread_turn_id(&thread), None);
    }

    #[test]
    fn queue_drain_requires_the_active_completion_to_be_last_lifecycle_effect() {
        use model::TurnLifecycleEvent::{Completed, Started};

        assert!(!lifecycle_ended_active_turn(&[
            Started {
                turn_id: "turn-b".into(),
            },
            Completed {
                turn_id: "turn-a".into(),
                status: "completed".into(),
                was_active: false,
            },
        ]));
        assert!(lifecycle_ended_active_turn(&[Completed {
            turn_id: "turn-b".into(),
            status: "completed".into(),
            was_active: true,
        }]));
        assert!(!lifecycle_ended_active_turn(&[
            Completed {
                turn_id: "turn-b".into(),
                status: "completed".into(),
                was_active: true,
            },
            Started {
                turn_id: "turn-c".into(),
            },
        ]));
    }

    #[test]
    fn bundled_theme_assets_deserialize_with_unique_names() {
        let assets = Assets;
        let mut theme_names = HashSet::new();

        for path in assets.list("themes/").expect("list bundled themes") {
            if !path.ends_with(".json") {
                continue;
            }
            let bytes = assets
                .load(&path)
                .expect("load bundled theme")
                .expect("bundled theme exists");
            let family = theme_settings::deserialize_user_theme(bytes.as_ref())
                .unwrap_or_else(|error| panic!("invalid bundled theme {path}: {error:#}"));
            for theme in family.themes {
                assert!(
                    theme_names.insert(theme.name.clone()),
                    "duplicate bundled theme name: {}",
                    theme.name
                );
            }
        }

        for expected in [
            "Catppuccin Mocha",
            "Tokyo Night Moon",
            "Rosé Pine",
            "Kanagawa Wave",
            "Nord Dark",
            "Dracula",
            "Everforest Dark Medium (regular)",
            "GitHub Dark",
            "JetBrains Islands Dark",
            "Nightfox",
            "VSCode Dark Modern",
        ] {
            assert!(theme_names.contains(expected), "missing theme {expected}");
        }
        assert_eq!(theme_names.len(), 71);

        // Exercise the same registry path used by the live Appearance sheet,
        // rather than proving only that the JSON files happen to deserialize.
        let registry = theme::ThemeRegistry::new(Box::new(Assets));
        theme_settings::load_bundled_themes(&registry);
        let registered_names = registry.list_names();
        for expected in theme_names {
            assert!(
                registered_names
                    .iter()
                    .any(|name| name == expected.as_str()),
                "theme asset was not registered: {expected}"
            );
        }
        assert_eq!(registered_names.len(), 71);
    }

    #[test]
    fn thread_snapshot_cache_replaces_and_evicts_least_recent_snapshots() {
        let mut cache = ThreadSnapshotCache::default();
        for index in 0..THREAD_SNAPSHOT_CACHE_LIMIT {
            cache.insert(cached_thread(&format!("thread-{index}"), 1));
        }

        let newest = cache
            .take("thread-0")
            .expect("cached snapshot promoted to most recent");
        cache.insert(newest);
        cache.insert(cached_thread("overflow", 1));

        assert!(cache.take("thread-1").is_none());
        assert_eq!(cache.take("thread-0").unwrap().updated_at, 1);
        cache.insert(cached_thread("overflow", 2));
        assert_eq!(cache.take("overflow").unwrap().updated_at, 2);
    }

    #[test]
    fn server_option_parsers_preserve_selectable_models_and_permission_profiles() {
        let models = model_choices_from_response(&json!({
            "data": [
                {
                    "id": "gpt-5.6",
                    "model": "gpt-5.6-codex",
                    "displayName": "GPT-5.6 Codex",
                    "hidden": false,
                    "isDefault": true,
                    "defaultReasoningEffort": "high",
                    "supportedReasoningEfforts": [
                        {"reasoningEffort": "high", "description": "Deep"},
                        {"reasoningEffort": "xhigh", "description": "Deeper"}
                    ]
                },
                {
                    "id": "hidden",
                    "model": "hidden",
                    "displayName": "Hidden",
                    "hidden": true,
                    "defaultReasoningEffort": "medium",
                    "supportedReasoningEfforts": []
                }
            ]
        }));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "gpt-5.6-codex");
        assert_eq!(models[0].default_effort, "high");
        assert_eq!(models[0].efforts, ["high", "xhigh"]);
        assert!(models[0].is_default);
        assert_eq!(
            effective_model_choice(&models, None).map(|choice| choice.model.as_str()),
            Some("gpt-5.6-codex")
        );
        assert_eq!(
            effective_reasoning_effort(None, effective_model_choice(&models, None)).as_deref(),
            Some("high")
        );
        assert_eq!(reasoning_effort_label("xhigh").as_ref(), "X high");

        let profiles = permission_profile_choices_from_response(&json!({
            "data": [
                {"id": ":workspace", "allowed": true, "description": "Workspace only"},
                {"id": ":full-access", "allowed": false}
            ]
        }));
        assert_eq!(profiles.len(), 2);
        assert!(profiles[0].allowed);
        assert_eq!(profiles[0].description.as_deref(), Some("Workspace only"));
        assert!(!profiles[1].allowed);
    }

    #[test]
    fn active_turn_submissions_belong_only_to_the_authoritative_queue() {
        assert!(!show_submission_optimistically_in_transcript(true));
        assert!(show_submission_optimistically_in_transcript(false));
    }

    #[test]
    fn context_ring_uses_last_turn_tokens_and_model_capacity() {
        assert_eq!(
            context_window_usage(Some(&json!({
                "threadId": "thread-1",
                "tokenUsage": {
                    "last": {"totalTokens": 123_456},
                    "modelContextWindow": 400_000
                }
            }))),
            Some((123_456, 400_000))
        );
        assert_eq!(context_window_usage(Some(&json!({}))), None);
    }

    #[test]
    fn authoritative_queue_response_preserves_ids_text_and_attachments() {
        let queued = queued_submissions_from_response(&json!({
            "data": [{
                "id": "queued-1",
                "clientUserMessageId": "client-message-1",
                "input": [
                    {"type": "text", "text": "first line"},
                    {"type": "inputText", "text": "second line"},
                    {"type": "image", "url": "data:image/png;base64,AQID"}
                ]
            }]
        }));

        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id.as_deref(), Some("queued-1"));
        assert_eq!(queued[0].client_user_message_id, "client-message-1");
        assert_eq!(
            queued_submission_text(&queued[0].input),
            "first line\nsecond line"
        );
        assert_eq!(
            queued_submission_preview(&queued[0].input),
            "first line second line"
        );
        assert_eq!(queued_submission_image_count(&queued[0].input), 1);
        assert_eq!(queued[0].preview_images.len(), 1);
    }

    #[test]
    fn vim_slash_search_uses_the_native_editor_in_rich_and_text_views() {
        assert!(vim_search_available(false, true));
        assert!(search_uses_native_editor(false, FocusMode::Buffer, true));
        assert!(search_uses_native_editor(
            false,
            FocusMode::Transcript,
            true
        ));
        assert!(search_uses_native_editor(true, FocusMode::Buffer, false));
        assert!(!search_uses_native_editor(false, FocusMode::Composer, true));
        assert!(!vim_search_available(false, false));
    }

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
    fn rich_vim_markdown_replacement_tokens_round_trip_to_their_source() {
        let source = concat!(
            "before\n\n---\n\n- [X] after\n\n",
            "| left | right |\n| --- | --- |\n| one | two |\n\n",
            "![alt text](image.png)",
        );
        let logical = model::rich_markdown_navigation_text(source);

        for token in ["---", "- [X]"] {
            let logical_start = logical.find(token).unwrap();
            let source_start = source.find(token).unwrap();
            for offset in 0..token.len() {
                assert_eq!(
                    markdown_source_offset_for_logical(&logical, source, logical_start + offset),
                    source_start + offset,
                    "logical byte {offset} of {token:?} must retain its source owner"
                );
                assert_eq!(
                    markdown_logical_offset_for_source(&logical, source, source_start + offset),
                    logical_start + offset,
                    "source byte {offset} of {token:?} must retain its logical owner"
                );
            }
        }

        for (logical_offset, character) in logical.char_indices() {
            if character == '\n' {
                continue;
            }
            let source_offset =
                markdown_source_offset_for_logical(&logical, source, logical_offset);
            assert_eq!(
                source[source_offset..].chars().next(),
                Some(character),
                "every navigable glyph must be a source-owned token: {character:?} at {logical_offset}"
            );
        }
    }

    #[test]
    fn rich_command_cursor_marker_uses_the_exact_utf8_glyph_in_each_surface() {
        let command = "/usr/bin/bash -lc 'printf café'";
        let output = "ok\nfinished";
        let body = format!("{command}\n{output}");
        let cursor = body.find("é").unwrap();
        let command_start = 0;
        let output_start = command_start + command.len() + 1;
        let navigation = RichNavigationPaint {
            body_text: body.clone().into(),
            ranges: Vec::new(),
            head: Some(cursor),
            visual: false,
            linewise: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };

        assert_eq!(
            rich_cursor_index_for_fragment(
                Some(&navigation),
                &(command_start..command_start + command.len())
            ),
            Some(cursor - command_start),
            "the explicit painted cursor must retain its UTF-8 byte position in command text"
        );
        assert_eq!(
            rich_cursor_index_for_fragment(Some(&navigation), &(output_start..body.len())),
            None,
            "only the surface containing the cursor may paint it"
        );

        let output_cursor = body.find("finished").unwrap() + "fin".len();
        let output_navigation = RichNavigationPaint {
            body_text: body.into(),
            ranges: Vec::new(),
            head: Some(output_cursor),
            visual: false,
            linewise: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            rich_cursor_index_for_fragment(
                Some(&output_navigation),
                &(output_start..output_start + output.len())
            ),
            Some(output_cursor - output_start),
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
            linewise: false,
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
        let mut replay = TranscriptModel::replay(6);
        replay.items[5].expanded = true;
        let diff = rich_navigation_item_projection(&replay, 4).unwrap();
        assert!(
            diff.body_text()
                .starts_with("crates/harness_app/src/main.rs\n")
        );
        assert!(!diff.body_text().contains("@@"));
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
            "/tmp/a\nold\nnew"
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
            "zed\nZed Docs\nzed.dev"
        );
    }

    #[test]
    fn generic_commands_use_their_content_as_the_expanded_identity() {
        let mut replay = TranscriptModel::replay(6);
        let diff = &mut replay.items[4];
        assert!(expanded_item_uses_content_as_header(diff));
        diff.expanded = false;
        assert!(!expanded_item_uses_content_as_header(diff));

        let command = &replay.items[5];
        assert!(!command.expanded);
        assert!(!expanded_item_uses_content_as_header(command));
        assert_eq!(
            transcript_item_header_title(command),
            "cargo check -p harness_app"
        );

        replay.items[5].expanded = true;
        assert!(expanded_item_uses_content_as_header(&replay.items[5]));

        replay.items[5].title = "Search for identity in crates/harness_app".into();
        assert!(command_uses_raw_identity(&replay.items[5]));
        assert!(expanded_item_uses_content_as_header(&replay.items[5]));
        assert_eq!(
            transcript_item_header_title(&replay.items[5]),
            "cargo check -p harness_app"
        );
    }

    #[test]
    fn collapsed_command_navigation_contains_only_its_painted_identity() {
        let replay = TranscriptModel::replay(6);
        let command = &replay.items[5];
        assert!(!command.expanded);

        let projection = rich_navigation_item_projection(&replay, 5).unwrap();
        assert_eq!(projection.body_text(), "cargo check -p harness_app");
        assert!(!projection.text.contains("Finished replay frame"));
    }

    #[test]
    fn toggling_one_command_never_changes_a_sibling_command() {
        let mut replay = TranscriptModel::replay(6);
        let mut sibling = replay.items[5].clone();
        sibling.key = "fixture-command-sibling".into();
        sibling.protocol_id = Some("fixture-command-sibling".into());
        replay.items.push(sibling);

        let (item_key, collapsed) =
            toggle_model_item_expansion_at(&mut replay, 5).expect("command should toggle");

        assert_eq!(item_key, "replay:5");
        assert!(!collapsed);
        assert!(replay.items[5].expanded);
        assert!(!replay.items[6].expanded);
    }

    #[test]
    fn virtual_command_rows_preserve_the_navigation_document() {
        let mut replay = TranscriptModel::replay(6);
        replay.items[5].expanded = true;
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
        assert_eq!(
            rich_command_row_navigation_range(&data, &data.rows[0]),
            0..data.command.len()
        );

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
        let mut replay = TranscriptModel::replay(6);
        replay.items[5].expanded = true;
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

        let mut after_model = TranscriptModel::replay(6);
        after_model.items[5].expanded = true;
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
            linewise: false,
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
    fn rich_visual_cursor_stays_on_the_selected_line_at_an_exclusive_head() {
        let body = "$ command\"\nUTC";
        let newline = body.find('\n').unwrap();
        let selected_line_end = newline + 1;
        let quote = body.rfind('"').unwrap();
        for head in [newline, selected_line_end] {
            let navigation = RichNavigationPaint {
                body_text: body.into(),
                ranges: vec![0..selected_line_end],
                // Depending on the native linewise motion, Zed can expose the
                // head on the selected newline or at the exclusive beginning
                // of the next line. Neither internal endpoint is a visible
                // cursor on the following output row.
                head: Some(head),
                visual: true,
                linewise: true,
                cursor_claimed: Rc::new(Cell::new(false)),
            };

            assert_eq!(navigation.cursor_range(), Some(quote..quote + 1));
            assert_eq!(
                rich_cursor_index_for_fragment(Some(&navigation), &(0..selected_line_end)),
                Some(quote)
            );
            assert_eq!(
                rich_cursor_index_for_fragment(Some(&navigation), &(selected_line_end..body.len())),
                None
            );
        }
    }

    #[test]
    fn rich_navigation_cursor_crosses_newlines_and_hidden_furniture_once() {
        let navigation = RichNavigationPaint {
            body_text: "alpha\n<hidden>beta\ngamma".into(),
            ranges: Vec::new(),
            head: Some(7),
            visual: false,
            linewise: false,
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
            linewise: false,
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
                linewise: false,
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
            linewise: false,
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
            linewise: false,
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
            linewise: false,
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
            linewise: false,
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
        assert_eq!(body, "src/main.rs\nold\nnew");

        let normal = RichNavigationPaint {
            body_text: body.into(),
            ranges: Vec::new(),
            head: Some(0),
            visual: false,
            linewise: false,
            cursor_claimed: Rc::new(Cell::new(false)),
        };
        let mut logical_cursor = 0;
        let path_range =
            rich_navigation_fragment_range(Some(&normal), &presentation.path, &mut logical_cursor);
        let visible_content = zed_diff_visible_text(&presentation.content, "Modified");
        let content_range =
            rich_navigation_fragment_range(Some(&normal), &visible_content, &mut logical_cursor);
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
            linewise: false,
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
            linewise: false,
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
            linewise: false,
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
            linewise: false,
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
    fn active_turn_tail_is_one_real_transcript_list_item() {
        assert_eq!(turn_tail_list_splice(3, 3, true), Some((3..3, 1)));
        assert_eq!(turn_tail_list_splice(4, 3, true), None);
        assert_eq!(turn_tail_list_splice(4, 3, false), Some((3..4, 0)));
        assert_eq!(turn_tail_list_splice(3, 3, false), None);
        assert_eq!(
            turn_tail_list_splice(7, 3, true),
            Some((0..7, 4)),
            "thread replacement must reconcile stale list furniture atomically"
        );
    }

    #[test]
    fn transcript_tail_targets_the_final_real_body_not_list_furniture() {
        let document = model::TranscriptDocument {
            text: "first\nfinal glyph".into(),
            item_rows: vec![Some(0), None, Some(1), None],
            segments: vec![
                model::TranscriptDocumentSegment {
                    item_index: 0,
                    item_key: "first".into(),
                    kind: model::TranscriptKind::Agent,
                    whole_range: 0..6,
                    header_range: 0..0,
                    body_range: 0..5,
                    semantic_spans: Vec::new(),
                },
                model::TranscriptDocumentSegment {
                    item_index: 2,
                    item_key: "final".into(),
                    kind: model::TranscriptKind::Agent,
                    whole_range: 6..17,
                    header_range: 6..6,
                    body_range: 6..17,
                    semantic_spans: Vec::new(),
                },
            ],
        };

        assert_eq!(transcript_tail_target(&document), Some((2, 11)));
    }

    #[test]
    fn streamed_agent_text_owns_the_visible_turn_activity() {
        let mut model = TranscriptModel::default();
        model.items.push(TranscriptItem {
            key: "streaming-agent".into(),
            protocol_id: Some("streaming-agent".into()),
            kind: model::TranscriptKind::Agent,
            title: "Codex".into(),
            status: Some("streaming".into()),
            content: "The response has started".into(),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        });

        assert!(transcript_has_inline_activity(&model));
        model.items[0].content.clear();
        assert!(
            !transcript_has_inline_activity(&model),
            "the list-tail fallback remains visible before the first text delta"
        );
        model.items[0].content = "complete".into();
        model.items[0].status = Some("completed".into());
        assert!(!transcript_has_inline_activity(&model));
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
            ..CodexThread::default()
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
            transcript_icon_color(model::TranscriptKind::FileChange),
            Color::Modified
        );
        assert_eq!(
            transcript_icon_color(model::TranscriptKind::Error),
            Color::Error
        );
        assert_eq!(
            transcript_icon_color(model::TranscriptKind::Command),
            Color::Muted
        );
        assert_eq!(transcript_status_color("failed"), Color::Error);
        assert_eq!(transcript_status_color("waiting"), Color::Warning);
        assert_eq!(transcript_status_color("in progress"), Color::Accent);
        assert_eq!(transcript_status_color("custom status"), Color::Muted);
    }

    #[test]
    fn projected_tools_use_zeds_semantic_tool_icons() {
        let tool = |title: &str| TranscriptItem {
            key: title.into(),
            protocol_id: None,
            kind: model::TranscriptKind::Tool,
            title: title.into(),
            status: None,
            content: String::new(),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        assert_eq!(
            icon_for_item(&tool("App Server · thread/read")),
            IconName::ToolSearch
        );
        assert_eq!(
            icon_for_item(&tool("Web fetch · example.com")),
            IconName::ToolWeb
        );
        assert_eq!(icon_for_item(&tool("Apply patch")), IconName::ToolPencil);
        assert_eq!(icon_for_item(&tool("Unknown MCP")), IconName::ToolHammer);
    }

    #[test]
    fn visible_card_titles_never_use_interpunct_separators() {
        let item = TranscriptItem {
            key: "tool-title".into(),
            protocol_id: None,
            kind: model::TranscriptKind::Tool,
            title: "App Server · thread/read".into(),
            status: None,
            content: String::new(),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };
        assert_eq!(
            transcript_item_header_title(&item),
            "App Server — thread/read"
        );
    }

    #[test]
    fn plans_report_structural_progress_and_can_collapse() {
        assert_eq!(
            plan_progress(&json!({
                "plan": [
                    {"step": "Done", "status": "completed"},
                    {"step": "Working", "status": "inProgress"},
                    {"step": "Later", "status": "pending"}
                ]
            })),
            Some((1, 3))
        );
        assert_eq!(plan_progress(&json!({"plan": []})), None);

        let mut replay = TranscriptModel::replay(3);
        assert!(replay.items[2].expanded);
        let plan_key = replay.items[2].key.clone();
        assert_eq!(
            toggle_model_item_expansion_at(&mut replay, 2),
            Some((plan_key, true))
        );
        assert!(!replay.items[2].expanded);
    }

    #[test]
    fn only_collapsed_activity_rows_form_compact_stacks() {
        let replay = TranscriptModel::replay(6);
        assert!(!transcript_item_is_compact_activity(&replay.items[0]));
        assert!(transcript_item_is_compact_activity(&replay.items[5]));
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
            presentations
                .iter()
                .map(|presentation| diff_content_counts(&presentation.content))
                .fold(
                    (0, 0),
                    |(total_additions, total_deletions), (additions, deletions)| {
                        (total_additions + additions, total_deletions + deletions)
                    }
                ),
            (3, 2)
        );
    }

    #[test]
    fn unified_diff_compacts_common_indentation_per_hunk() {
        let presentations = diff_file_presentations(concat!(
            "diff --git a/src/main.rs b/src/main.rs\n",
            "index 111..222 100644\n",
            "--- a/src/main.rs\n",
            "+++ b/src/main.rs\n",
            "@@ -10,2 +10,2 @@\n",
            "         context\n",
            "-        old\n",
            "+            new\n",
            "@@ -30 +30 @@\n",
            "     second hunk",
        ));

        assert_eq!(presentations.len(), 1);
        assert_eq!(
            presentations[0].content,
            concat!(
                "index 111..222 100644\n",
                "--- a/src/main.rs\n",
                "+++ b/src/main.rs\n",
                "@@ -10,2 +10,2 @@\n",
                " context\n",
                "-old\n",
                "+    new\n",
                "@@ -30 +30 @@\n",
                " second hunk",
            )
        );
        assert_eq!(diff_content_counts(&presentations[0].content), (1, 1));
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
        assert!(!item_matches_search_query(&item, "/tmp/preview.png"));

        item.content = "/tmp/preview.png\n\nRevised prompt\nA detailed scene".into();
        assert_eq!(
            image_caption_for_display(&item),
            Some("/tmp/preview.png\n\nRevised prompt\nA detailed scene")
        );
        assert!(item_matches_search_query(&item, "detailed scene"));
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
    fn rich_search_uses_smart_case() {
        let text = "I can inspect it, and i can repeat it";
        assert_eq!(search_match_byte_ranges(text, "I", 8), vec![0..1]);
        assert_eq!(
            search_match_byte_ranges(text, "i", 8),
            vec![0..1, 6..7, 14..15, 22..23, 35..36]
        );
        assert!(
            search_match_byte_ranges("Semantic Needle", "semantic needle", 1)
                .first()
                .is_some()
        );
        assert!(
            search_match_byte_ranges("Semantic Needle", "Semantic Needle", 1)
                .first()
                .is_some()
        );
        assert!(search_match_byte_ranges("semantic needle", "Semantic Needle", 1).is_empty());
        assert_eq!(
            search_match_byte_ranges("needle · needle ", "needle ", 8),
            vec![0..7, 10..17]
        );
    }

    #[test]
    fn rich_search_range_budget_is_bounded_across_card_fragments() {
        let paint = RichSearchPaint::new("needle", Some(0));
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
    fn markdown_cursor_autoscroll_tracks_exact_source_changes_only() {
        let at_120 = RichMarkdownNavigationPaint {
            selections: Vec::new(),
            cursor: Some(120),
        };
        let same_cursor_with_selection = RichMarkdownNavigationPaint {
            selections: vec![100..140],
            cursor: Some(120),
        };
        let at_240 = RichMarkdownNavigationPaint {
            selections: Vec::new(),
            cursor: Some(240),
        };

        assert_eq!(changed_markdown_cursor(None, Some(&at_120)), Some(120));
        assert_eq!(
            changed_markdown_cursor(Some(&at_120), Some(&same_cursor_with_selection)),
            None
        );
        assert_eq!(
            changed_markdown_cursor(Some(&at_120), Some(&at_240)),
            Some(240)
        );
        assert_eq!(changed_markdown_cursor(Some(&at_240), None), None);
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

        assert!(item_matches_search_query(&item, "semantic needle"));
        assert!(item_matches_search_query(&item, "waiting for results"));
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

        assert!(!item_matches_search_query(&item, "codex"));
        assert!(item_matches_search_query(&item, "unrelated"));
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

        assert_eq!(
            zed_diff_lines(
                "@@ -29,2 +29,4 @@\n context\n-removed\n+added\n+another",
                "Modified",
            ),
            vec![
                ZedDiffLine {
                    text: "context".into(),
                    tone: DiffLineTone::Normal,
                },
                ZedDiffLine {
                    text: "removed".into(),
                    tone: DiffLineTone::Deletion,
                },
                ZedDiffLine {
                    text: "added".into(),
                    tone: DiffLineTone::Addition,
                },
                ZedDiffLine {
                    text: "another".into(),
                    tone: DiffLineTone::Addition,
                },
            ]
        );
    }

    #[test]
    fn virtualized_file_change_rows_preserve_the_vim_navigation_document() {
        let item = TranscriptItem {
            key: "file-change-navigation".into(),
            protocol_id: None,
            kind: model::TranscriptKind::FileChange,
            title: "File changes · 2 files".into(),
            status: None,
            content: "Modified · /tmp/first.rs\n@@ -1 +1 @@\n-old\n+new\n\n\
                      Added · /tmp/second.rs\ncreated"
                .into(),
            raw: Value::Null,
            event_count: 1,
            expanded: true,
            pending_request: None,
        };

        let body = rich_navigation_body_for_item(&item, "");
        let data = rich_file_change_data(&item);
        let visible_rows = data
            .rows
            .iter()
            .filter_map(RichFileChangeRow::logical_range)
            .map(|range| &body[range.clone()])
            .collect::<Vec<_>>();

        assert_eq!(body, "/tmp/first.rs\nold\nnew\n/tmp/second.rs\ncreated");
        assert_eq!(
            visible_rows,
            ["/tmp/first.rs", "old", "new", "/tmp/second.rs", "created",]
        );
        assert_eq!(
            data.presentations.iter().map(file_change_counts).fold(
                (0, 0),
                |(total_additions, total_deletions), (additions, deletions)| {
                    (total_additions + additions, total_deletions + deletions)
                }
            ),
            (2, 1)
        );

        let source = include_str!("main.rs");
        let renderer = source
            .split_once("fn render_file_change(")
            .and_then(|(_, after)| after.split_once("fn render_reasoning("))
            .map(|(renderer, _)| renderer)
            .expect("file-change renderer must remain independently auditable");
        assert!(
            renderer.contains(".with_sizing_behavior(ListSizingBehavior::Infer)"),
            "virtualized nested diffs must infer a real first-frame height instead of collapsing"
        );
    }

    #[test]
    fn aggregate_and_file_change_diffs_share_one_flat_virtual_document() {
        let mut replay = TranscriptModel::replay(6);
        let item = replay.items.remove(4);
        let body = rich_navigation_body_for_item(&item, "");
        let data = rich_file_change_data(&item);
        let visible_rows = data
            .rows
            .iter()
            .filter_map(RichFileChangeRow::logical_range)
            .map(|range| &body[range.clone()])
            .collect::<Vec<_>>();

        assert_eq!(data.presentations.len(), 3);
        assert_eq!(
            data.presentations
                .iter()
                .map(|presentation| presentation.path.as_str())
                .collect::<Vec<_>>(),
            [
                "crates/harness_app/src/main.rs",
                "crates/harness_editor/src/lib.rs",
                "README.md",
            ]
        );
        assert_eq!(visible_rows.len(), data.rows.len());
        assert_eq!(
            visible_rows.first().copied(),
            Some("crates/harness_app/src/main.rs")
        );
        assert_eq!(
            visible_rows
                .iter()
                .filter(|row| row.ends_with(".rs") || **row == "README.md")
                .count(),
            3,
            "each file contributes one ordinary row to the same virtualized surface"
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
        assert_eq!(presentation.queries, ["GPUI", "Zed GPUI"]);
        assert_eq!(presentation.results.len(), 2);
        assert_eq!(presentation.results[0].title, "GPUI framework");
        assert_eq!(
            presentation.results[0].domain.as_deref(),
            Some("example.test")
        );
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
    fn background_parent_request_returns_to_parent_before_becoming_interactive() {
        let params = json!({"threadId": "parent", "permissions": {}});
        assert_eq!(
            route_server_request_with_background(
                "item/permissions/requestApproval",
                &params,
                Some("read-only-child"),
                Some("parent"),
            ),
            RequestRoute::ReturnToThread("parent".into())
        );

        // After the parent transcript is loaded, replay follows the ordinary
        // selected-thread route and preserves the request as interactive.
        assert_eq!(
            route_server_request_with_background(
                "item/permissions/requestApproval",
                &params,
                Some("parent"),
                Some("parent"),
            ),
            RequestRoute::Interactive
        );
    }

    #[test]
    fn malformed_background_parent_request_is_resolved_without_navigation() {
        assert!(matches!(
            route_server_request_with_background(
                "item/tool/requestUserInput",
                &json!({"threadId": "parent", "questions": []}),
                Some("read-only-child"),
                Some("parent"),
            ),
            RequestRoute::Immediate(RequestReply::Error { code: -32602, .. })
        ));
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
    fn host_driven_rich_cursor_placement_updates_the_painted_snapshot_immediately() {
        let source = include_str!("main.rs");
        let production = source
            .rsplit_once("#[cfg(test)]\nmod tests")
            .map(|(production, _)| production)
            .expect("the production/test boundary must remain explicit");
        let cursor_bridge = production
            .split_once("fn place_rich_cursor_in_item(")
            .and_then(|(_, after)| after.split_once("fn place_rich_cursor_at_item_last_line("))
            .map(|(method, _)| method)
            .expect("Rich cursor placement must remain an auditable host/editor bridge");
        let last_line_bridge = production
            .split_once("fn place_rich_cursor_at_item_last_line(")
            .and_then(|(_, after)| after.split_once("fn show_rich_transcript("))
            .map(|(method, _)| method)
            .expect("last-line Rich cursor placement must share the same bridge contract");

        for bridge in [cursor_bridge, last_line_bridge] {
            assert!(bridge.contains("selection_snapshot(cx)"));
            assert!(bridge.contains("self.rich_navigation_selection = Some("));
        }
        assert_eq!(
            production.matches("editor.set_cursor_in_item(").count(),
            1,
            "Rich call sites must not bypass the placement/snapshot bridge"
        );
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
        assert!(composer_send_blocked(
            true, false, false, false, false, true
        ));
        assert!(composer_send_blocked(
            false, true, false, false, false, true
        ));
        assert!(composer_send_blocked(
            false, false, true, false, false, true
        ));
        assert!(composer_send_blocked(
            false, false, false, true, false, true
        ));
        assert!(composer_send_blocked(
            false, false, false, false, true, true
        ));
        assert!(composer_send_blocked(
            false, false, false, false, false, false
        ));
        assert!(!composer_send_blocked(
            false, false, false, false, false, true
        ));
    }

    #[test]
    fn composer_action_uses_one_context_sensitive_control() {
        assert_eq!(
            composer_action_state(false, true),
            ComposerActionState::Send
        );
        assert_eq!(
            composer_action_state(false, false),
            ComposerActionState::Send
        );
        assert_eq!(
            composer_action_state(true, false),
            ComposerActionState::Queue
        );
        assert_eq!(composer_action_state(true, true), ComposerActionState::Stop);
    }

    #[test]
    fn pasted_images_make_an_empty_text_draft_sendable() {
        assert!(composer_is_empty("   ", 0));
        assert!(!composer_is_empty("   ", 1));
        assert!(!composer_is_empty("describe this", 0));
    }

    #[test]
    fn composer_images_become_multimodal_app_server_input() {
        let images = vec![ComposerImageAttachment {
            id: 7,
            image: Arc::new(Image::from_bytes(gpui::ImageFormat::Png, vec![1, 2, 3])),
        }];

        let input = composer_app_server_input("what is this?", &images);
        assert_eq!(input[0], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(
            input[1],
            json!({"type": "image", "url": "data:image/png;base64,AQID"})
        );
        assert_eq!(composer_prompt_preview(&input), "what is this?");
    }

    #[test]
    fn composer_image_markers_preserve_multimodal_order() {
        let images = vec![ComposerImageAttachment {
            id: 7,
            image: Arc::new(Image::from_bytes(gpui::ImageFormat::Png, vec![1, 2, 3])),
        }];
        let input = composer_app_server_input("before [Image #7] after", &images);
        assert_eq!(
            input,
            vec![
                json!({"type": "text", "text": "before "}),
                json!({"type": "image", "url": "data:image/png;base64,AQID"}),
                json!({"type": "text", "text": " after"}),
            ]
        );
        assert_eq!(composer_prompt_preview(&input), "before  after");
    }

    #[test]
    fn transcript_data_images_decode_into_cached_gpui_images() {
        let source = transcript_user_image_source(&model::UserImageSource::Url(
            "data:image/png;base64,AQID".into(),
        ));

        let Some(UserImagePreview {
            semantic_source,
            source: ImageSource::Image(image),
            dimensions,
        }) = source
        else {
            panic!("expected a decoded GPUI image");
        };
        assert_eq!(image.format(), ImageFormat::Png);
        assert_eq!(image.bytes(), &[1, 2, 3]);
        assert_eq!(dimensions, None);
        assert_eq!(
            semantic_source,
            model::UserImageSource::Url("data:image/png;base64,AQID".into())
        );
    }

    #[test]
    fn user_image_preview_hugs_the_bitmap_aspect_ratio() {
        let wide = user_image_preview_size(Some((1522, 667)));
        assert_eq!(wide.0, 384.);
        assert!((wide.1 - 168.284).abs() < 0.001);
        assert_eq!(user_image_preview_size(Some((100, 80))), (100., 80.));
        assert_eq!(user_image_preview_size(None), (320., 180.));
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

    #[test]
    fn header_only_landmarks_cannot_disable_the_rich_vim_document() {
        let mut model = TranscriptModel::default();
        model.items = vec![
            TranscriptItem {
                key: "before".into(),
                protocol_id: None,
                kind: model::TranscriptKind::Agent,
                title: "Codex".into(),
                status: Some("completed".into()),
                content: "before compaction".into(),
                raw: Value::Null,
                event_count: 1,
                expanded: true,
                pending_request: None,
            },
            TranscriptItem {
                key: "compaction".into(),
                protocol_id: Some("compaction".into()),
                kind: model::TranscriptKind::Trace,
                title: "Context compacted".into(),
                status: Some("completed".into()),
                content: String::new(),
                raw: json!({"type": "contextCompaction"}),
                event_count: 1,
                expanded: false,
                pending_request: None,
            },
            TranscriptItem {
                key: "after".into(),
                protocol_id: None,
                kind: model::TranscriptKind::Agent,
                title: "Codex".into(),
                status: Some("completed".into()),
                content: "after compaction".into(),
                raw: Value::Null,
                event_count: 1,
                expanded: true,
                pending_request: None,
            },
        ];

        let document = rich_navigation_document(&model);
        assert_eq!(
            document
                .segments
                .iter()
                .map(|segment| segment.item_index)
                .collect::<Vec<_>>(),
            [0, 2]
        );
        assert_eq!(document.item_rows, [Some(0), None, Some(1)]);
        assert!(
            document
                .segments
                .iter()
                .all(|segment| !segment.whole_range.is_empty())
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
        // Standalone Harness does not construct Zed's production Client,
        // which ordinarily installs the process-wide HTTP implementation.
        // Register the same native reqwest stack explicitly so catalog and
        // extension downloads never fall through to GPUI's NoHttpClient.
        cx.set_http_client(Arc::new(reqwest_client::ReqwestClient::new()));
        release_channel::init_test(
            semver::Version::new(0, 1, 0),
            release_channel::ReleaseChannel::Dev,
            cx,
        );
        settings::init(cx);
        // Load the same bundled theme catalog as Zed instead of constraining
        // Harness to the test-only base theme. Components still consume
        // semantic Harness roles derived from the active Zed theme.
        theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
        let external_themes =
            theme_sources::load_external_themes(&theme::ThemeRegistry::global(cx));
        if !external_themes.errors.is_empty() {
            for error in &external_themes.errors {
                log::warn!("could not load external theme: {error}");
            }
        }
        log::info!(
            "loaded {} external theme files containing {} additional themes",
            external_themes.files_loaded,
            external_themes.themes_added
        );
        if let Err(error) = Assets.load_fonts(cx) {
            log::error!("failed to load fonts: {error}");
            return;
        }
        let initial_settings = preferred_preferences().settings_json();
        SettingsStore::update_global(cx, |store, cx| {
            if let Err(error) = store.set_user_settings(&initial_settings, cx).result() {
                log::error!("failed to initialize Harness settings: {error}");
            }
        });
        // The settings observer is deferred; make the first frame use the
        // selected Harness theme as well instead of briefly (or permanently,
        // in short replay sessions) painting the system-mode fallback.
        theme_settings::reload_theme(cx);
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
