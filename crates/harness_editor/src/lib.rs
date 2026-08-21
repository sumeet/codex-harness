//! The only boundary between Harness and Zed's editor/Vim implementation.
//!
//! Keep the public API local-buffer-shaped. Project, workspace, LSP, DAP, Git,
//! collaboration, and IDE navigation do not belong on this side of the seam.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{Range, RangeInclusive},
    sync::{Arc, LazyLock},
};

use editor::{
    Addon, Bias, Editor, EditorEvent, Inlay, InlayHighlight, RowExt as _, RowHighlightOptions,
    RowOverlayOptions, SelectionEffects,
    display_map::{
        BlockContext, BlockPlacement, BlockProperties, BlockStyle, Crease, CustomBlockId,
        FoldPlaceholder, HighlightKey, NavigationOverlayKey, RenderBlock,
    },
    scroll::Autoscroll,
};
use gpui::{
    AnyView, App, AppContext as _, Context, Edges, Entity, EventEmitter, FocusHandle, Focusable,
    Font, FontFamilyVariant, FontWeight, Global, HighlightStyle, Hsla, IntoElement, KeyBinding,
    KeyContext, Pixels, Render, SharedString, TextStyle, TextStyleRefinement, WeakEntity, Window,
    div, point, prelude::*, px,
};
use harness_protocol::{
    TranscriptDocument, TranscriptDocumentSegment, TranscriptItemProjection, TranscriptKind,
    TranscriptSemanticSpan, TranscriptSemanticStyle, minimal_text_edit,
};
use language::{Buffer, InlayId, Language, LanguageRegistry, Point};
use multi_buffer::{
    Anchor, MultiBufferOffset, MultiBufferRow, MultiBufferSnapshot, ToOffset as _, ToPoint as _,
};
use settings::{KeybindSource, KeymapFile, Settings as _};
use theme_settings::ThemeSettings;
use tree_sitter::{Query, StreamingIterator as _};
use ui::{
    Color, DiffStat, Disclosure, Icon, IconName, IconSize, Label, LabelSize,
    prelude::{ActiveTheme, LabelCommon as _, StyledTypography as _},
};

pub use editor::actions::{LocalNavigationBack, LocalNavigationForward};
pub use vim::Search as VimSearch;
pub use vim::{
    ModeIndicator, MoveToNext as VimWordNext, MoveToNextMatch as VimNextMatch,
    MoveToPrevious as VimWordPrevious, MoveToPreviousMatch as VimPreviousMatch,
};

pub fn init(cx: &mut App) -> anyhow::Result<()> {
    editor::init(cx);
    vim::init(cx);
    let language_set = HarnessLanguageSet::new(cx)?;
    cx.set_global(language_set);

    let mut defaults =
        KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)?;
    for binding in &mut defaults {
        binding.set_meta(KeybindSource::Default.meta());
    }
    cx.bind_keys(defaults);

    let mut vim = KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)?;
    // Local editors have no Zed Workspace panes. Leaving these bindings in the
    // keymap consumes navigation before the host can provide its own
    // transcript/composer/task focus graph or editor-local history.
    const ABSENT_WORKSPACE_ACTIONS: &[&str] = &[
        "workspace::ActivatePaneLeft",
        "workspace::ActivatePaneRight",
        "workspace::ActivatePaneUp",
        "workspace::ActivatePaneDown",
        "pane::GoBack",
        "pane::GoForward",
    ];
    vim.retain(|binding| !ABSENT_WORKSPACE_ACTIONS.contains(&binding.action().name()));
    for binding in &mut vim {
        binding.set_meta(KeybindSource::Vim.meta());
    }
    cx.bind_keys(vim);
    cx.bind_keys([
        KeyBinding::new(
            "ctrl-o",
            LocalNavigationBack,
            Some("Editor && VimControl && vim_mode == normal"),
        ),
        KeyBinding::new(
            "ctrl-i",
            LocalNavigationForward,
            Some("Editor && VimControl && vim_mode == normal"),
        ),
    ]);
    Ok(())
}

/// The deliberately small language surface embedded by the standalone Harness.
///
/// This is enough to style Markdown prose and its common fenced snippets without
/// pulling Zed's project-aware `languages` application crate into the editor seam.
struct HarnessLanguageSet {
    registry: Arc<LanguageRegistry>,
    markdown: Arc<Language>,
}

impl Global for HarnessLanguageSet {}

impl HarnessLanguageSet {
    fn new(cx: &mut App) -> anyhow::Result<Self> {
        let registry = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
        let markdown = embedded_language("markdown", tree_sitter_md::LANGUAGE.into())?;
        for language in [
            markdown.clone(),
            embedded_language("markdown-inline", tree_sitter_md::INLINE_LANGUAGE.into())?,
            embedded_language("bash", tree_sitter_bash::LANGUAGE.into())?,
            embedded_language("rust", tree_sitter_rust::LANGUAGE.into())?,
            embedded_language("json", tree_sitter_json::LANGUAGE.into())?,
        ] {
            registry.add(language);
        }
        registry.set_theme(cx.theme().clone());
        Ok(Self { registry, markdown })
    }
}

fn embedded_language(name: &str, grammar: tree_sitter::Language) -> anyhow::Result<Arc<Language>> {
    Ok(Arc::new(
        Language::new(grammars::load_config(name), Some(grammar))
            .with_queries(grammars::load_queries(name))?,
    ))
}

pub struct LocalEditor {
    editor: Entity<Editor>,
    typography_profile: TranscriptTypographyProfile,
}

/// Emitted whenever a host-owned local editor's Buffer text changes.
pub struct LocalEditorChanged;

impl EventEmitter<LocalEditorChanged> for LocalEditor {}

impl LocalEditor {
    pub fn modal_composer(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let typography_profile = TranscriptTypographyProfile::Reading;
        let (language_registry, markdown) = {
            let languages = cx.global::<HarnessLanguageSet>();
            (languages.registry.clone(), languages.markdown.clone())
        };
        let editor = cx.new(move |cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text("Ask Codex…", window, cx);
            editor.set_use_modal_editing(true);
            if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                buffer.update(cx, |buffer, cx| {
                    buffer.set_language_registry(language_registry);
                    buffer.set_language(Some(markdown), cx);
                });
            }
            apply_typography_profile_to_editor(&mut editor, typography_profile, window, cx);
            editor
        });
        cx.subscribe(&editor, |_, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.emit(LocalEditorChanged);
            }
        })
        .detach();
        Self {
            editor,
            typography_profile,
        }
    }

    pub fn plain_single_line(
        placeholder: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = placeholder.into();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder.as_ref(), window, cx);
            editor.set_use_modal_editing(false);
            editor
        });
        cx.subscribe(&editor, |_, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.emit(LocalEditorChanged);
            }
        })
        .detach();
        Self {
            editor,
            // Zed's single-line and auto-height Editors both use the UI font
            // by default. Record that identity explicitly so a host can apply
            // the same Reading/Mono choice without replacing the Editor.
            typography_profile: TranscriptTypographyProfile::Reading,
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    pub fn set_text(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.editor.update(cx, |editor, cx| {
            editor.set_text(text.to_string(), window, cx)
        });
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_masked(masked, cx));
    }

    pub fn typography_profile(&self) -> TranscriptTypographyProfile {
        self.typography_profile
    }

    /// Switch only this Editor's font identity.
    ///
    /// The Buffer, selections, focus handle, Vim state, and undo history stay
    /// on the existing Editor entity. `Editor::set_style` propagates the new
    /// font metrics into the display map so soft wrapping is recalculated.
    pub fn set_typography_profile(
        &mut self,
        profile: TranscriptTypographyProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !typography_profile_changed(self.typography_profile, profile) {
            return false;
        }

        self.editor.update(cx, |editor, cx| {
            apply_typography_profile_to_editor(editor, profile, window, cx)
        });
        self.typography_profile = profile;
        cx.notify();
        true
    }

    /// Put a modal host input into Vim Insert mode after focus transfer.
    ///
    /// Hosts defer this until the focusing keybinding finishes dispatching so
    /// the same `i`/`a`/`o` keystroke cannot also edit the newly focused input.
    pub fn enter_insert_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(action) = cx.build_action("vim::SwitchToInsertMode", None) {
            window.dispatch_action(action, cx);
        }
    }
}

impl Focusable for LocalEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for LocalEditor {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

pub struct TranscriptEditor {
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    input_only: bool,
    typography_profile: TranscriptTypographyProfile,
    segments: Vec<TranscriptDocumentSegment>,
    segment_header_texts: Vec<String>,
    // The Rich selection bridge reads the selected logical body on every Vim
    // motion. Retain the projected bodies with their segments so that motion
    // clones an Arc instead of copying text back out of the Buffer.
    segment_body_texts: Vec<Arc<str>>,
    model_item_count: usize,
    // Segment positions are stable across body-only streaming updates and
    // append-only growth. Keeping the ids keyed by position lets viewport
    // shifts retain blocks in the overlap instead of recreating every header.
    header_blocks: BTreeMap<usize, CustomBlockId>,
    // Diff file headers are compact structural rows layered into otherwise
    // selectable diff bodies. They are viewport-bounded and remounted when a
    // streamed diff changes its file sections or counts.
    diff_file_blocks: Vec<CustomBlockId>,
    collapsed_items: BTreeSet<String>,
    padding_inlays: Vec<InlayId>,
    next_padding_inlay_id: usize,
    supplements: BTreeMap<String, MountedTranscriptSupplement>,
    replacements: BTreeMap<String, MountedTranscriptReplacement>,
    viewport_decorations: Option<ViewportDecorationWindow>,
    viewport_refresh_pending: bool,
    refresh_when_rendered: bool,
    // A streaming body edit can change unified-diff line classes without
    // moving the viewport. Reparse only the visible Diff bodies on the next
    // refresh; never rescan the full transcript.
    diff_highlights_dirty: bool,
    semantic_highlights_dirty: bool,
    search: TranscriptSearchState,
    follow_tail: bool,
    last_selection_head: Option<Anchor>,
    pending_tail_intent: Option<PendingTailIntent>,
}

/// The font geometry used by Harness's full Editor surfaces.
///
/// Both profiles retain Zed's native Buffer, display map, selections, Vim, and
/// undo implementation. `Reading` changes only the whole-surface font identity;
/// size and line height remain those of the surface's existing Editor style.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TranscriptTypographyProfile {
    Buffer,
    #[default]
    Reading,
}

fn typography_profile_changed(
    current: TranscriptTypographyProfile,
    requested: TranscriptTypographyProfile,
) -> bool {
    current != requested
}

fn font_for_typography_profile(profile: TranscriptTypographyProfile, cx: &App) -> Font {
    let settings = ThemeSettings::get_global(cx);
    match profile {
        TranscriptTypographyProfile::Buffer => settings.buffer_font.clone(),
        TranscriptTypographyProfile::Reading => settings.ui_font.clone(),
    }
}

fn typography_refinement(font: &Font) -> TextStyleRefinement {
    TextStyleRefinement {
        font_family: Some(font.family.clone()),
        font_features: Some(font.features.clone()),
        font_fallbacks: font.fallbacks.clone(),
        font_weight: Some(font.weight),
        font_style: Some(font.style),
        ..TextStyleRefinement::default()
    }
}

fn apply_typography_font(style: &mut TextStyle, font: &Font) {
    style.font_family = font.family.clone();
    style.font_features = font.features.clone();
    style.font_fallbacks.clone_from(&font.fallbacks);
    style.font_weight = font.weight;
    style.font_style = font.style;
}

fn apply_typography_profile_to_editor(
    editor: &mut Editor,
    profile: TranscriptTypographyProfile,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let font = font_for_typography_profile(profile, cx);
    editor.set_text_style_refinement(typography_refinement(&font));
    let mut style = editor.style(cx).clone();
    apply_typography_font(&mut style.text, &font);
    editor.set_style(style, window, cx);
}

/// Marks only the transcript's Zed Editor in its intrinsic key context.
///
/// An active Editor installs its own window key context, so a context placed
/// on Harness's parent element is not visible to bindings while the transcript
/// has focus. Using Zed's addon seam keeps transcript-only bindings such as
/// `i`/`a`/`o` more specific than stock Vim without leaking them into the
/// composer or other local Editors.
struct TranscriptKeyContextAddon;

impl Addon for TranscriptKeyContextAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        key_context.add("HarnessBuffer");
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A host-owned rich view anchored to one semantic transcript item.
///
/// The transcript editor owns only placement and display-map lifetime. The
/// embedded view owns its domain state and emits its own events to the host.
/// Its height is expressed in editor rows so layout remains deterministic.
#[derive(Clone)]
pub struct TranscriptSupplement {
    pub key: String,
    pub item_key: String,
    pub rows: u32,
    pub view: AnyView,
}

impl TranscriptSupplement {
    pub fn new(
        key: impl Into<String>,
        item_key: impl Into<String>,
        rows: u32,
        view: AnyView,
    ) -> Self {
        Self {
            key: key.into(),
            item_key: item_key.into(),
            rows: rows.max(1),
            view,
        }
    }
}

struct MountedTranscriptSupplement {
    item_key: String,
    rows: u32,
    view: AnyView,
    block_id: Option<CustomBlockId>,
}

/// A host-owned rich view that replaces one semantic transcript item.
///
/// The item's bytes remain in the Buffer, so search, yank, and raw inspection
/// still operate on the canonical transcript. The display map treats the rich
/// surface as one atomic Vim object while it is mounted.
#[derive(Clone)]
pub struct TranscriptReplacement {
    pub key: String,
    pub item_key: String,
    pub rows: u32,
    pub view: AnyView,
}

impl TranscriptReplacement {
    pub fn new(
        key: impl Into<String>,
        item_key: impl Into<String>,
        rows: u32,
        view: AnyView,
    ) -> Self {
        Self {
            key: key.into(),
            item_key: item_key.into(),
            rows: rows.max(1),
            view,
        }
    }
}

struct MountedTranscriptReplacement {
    item_key: String,
    rows: u32,
    view: AnyView,
    block_id: Option<CustomBlockId>,
}

pub struct TranscriptSelectionChanged;

impl EventEmitter<TranscriptSelectionChanged> for TranscriptEditor {}

/// The portion of the real Editor/Vim selection that belongs to one Rich
/// transcript item. Offsets are relative to the item's selectable body text,
/// so a renderer can translate them into its own visual runs without knowing
/// anything about ornamental transcript headers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptItemSelection {
    pub item_index: usize,
    pub body_text: Arc<str>,
    pub range: Range<usize>,
    pub head: Option<usize>,
}

/// A renderer-independent snapshot of the newest native Editor selection.
///
/// Rich mode consumes this snapshot while the Editor remains the sole Vim
/// state machine. `visual` distinguishes a selection from the normal-mode
/// block cursor represented by `head`; `linewise` records that Vim expanded
/// the projected ranges to whole logical lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptSelectionSnapshot {
    pub visual: bool,
    pub linewise: bool,
    pub reversed: bool,
    pub items: Vec<TranscriptItemSelection>,
}

fn linewise_selection_rows(start: Point, end: Point) -> RangeInclusive<u32> {
    // A range ending at column zero owns the preceding newline, not the next
    // row. This is the same boundary rule used by Vim's linewise yank path.
    let last_row = if end.column == 0 && end.row > start.row {
        end.row - 1
    } else {
        end.row
    };
    start.row..=last_row
}

struct TranscriptHeaderHighlight;
struct UserTranscriptRows;
struct ReasoningTranscriptRows;
struct StructuredTranscriptRows;
struct ErrorTranscriptRows;
struct ReasoningBodyHighlight;
struct PlanBodyHighlight;
struct MarkdownHeadingHighlight;
struct MarkdownStrongHighlight;
struct MarkdownEmphasisHighlight;
struct MarkdownInlineCodeHighlight;
struct MarkdownLinkHighlight;
struct MarkdownCodeBlockHighlight;
struct MarkdownMonospaceGeometryHighlight;
struct MarkdownBlockQuoteHighlight;
struct MarkdownStrikethroughHighlight;
struct ShellFunctionHighlight;
struct ShellVariableHighlight;
struct ShellKeywordHighlight;
struct ShellOperatorHighlight;
struct ShellConstantHighlight;
struct ShellStringHighlight;
struct ShellCommentHighlight;
struct ShellEmbeddedHighlight;
struct ShellPunctuationHighlight;
struct DiffFileHeaderHighlight;
struct DiffHunkHighlight;
struct DiffAdditionHighlight;
struct DiffDeletionHighlight;
struct DiffAdditionRows;
struct DiffDeletionRows;
struct DiffGutterInlayHighlight;

#[derive(Default)]
struct TranscriptSearchState {
    query: String,
    backwards: bool,
    case_sensitive: bool,
    whole_word: bool,
    // MultiBuffer anchors follow streaming edits while byte offsets do not.
    // The highlights remain decorations; Vim's actual selection stays owned
    // by the Editor and is changed only by explicit match navigation.
    active_match: Option<Range<Anchor>>,
    highlights_dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTailIntent {
    DirectScroll,
    Selection { was_following: bool },
}

// Row highlights are flattened to display rows during paint and native blocks
// participate in display-map layout. Keep both bounded by the real editor
// viewport while leaving the full selectable semantic document in the Buffer.
const VIEWPORT_OVERSCAN_ROWS: u32 = 64;
const FOLLOW_TAIL_SLOP_ROWS: f64 = 1.;
const MAX_SEMANTIC_SPANS_PER_SEGMENT: usize = 2_048;
const MAX_SCANNED_SEMANTIC_SPANS_PER_VIEWPORT: usize = 4_096;
const MAX_SEMANTIC_SPAN_LOOKBEHIND: usize = 128;
const MAX_NATIVE_DIFF_HEADER_SCAN_BYTES: usize = 512 * 1_024;

#[derive(Clone, Debug)]
struct ViewportDecorationWindow {
    byte_range: Range<usize>,
    header_segment_range: Range<usize>,
    anchor_range: Range<Anchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewportWindowChoice {
    ReuseCached,
    Recenter,
}

fn viewport_window_choice(
    visible: &Range<usize>,
    cached: Option<&Range<usize>>,
) -> ViewportWindowChoice {
    if cached.is_some_and(|cached| cached.start <= visible.start && visible.end <= cached.end) {
        ViewportWindowChoice::ReuseCached
    } else {
        ViewportWindowChoice::Recenter
    }
}

fn overscanned_point_range(
    visible_range: Range<Point>,
    max_point: Point,
    overscan_rows: u32,
) -> Range<Point> {
    let start_row = visible_range.start.row.saturating_sub(overscan_rows);
    let end_row = visible_range
        .end
        .row
        .saturating_add(overscan_rows)
        .saturating_add(1);
    let start = if start_row >= max_point.row {
        Point::new(max_point.row, 0).min(max_point)
    } else {
        Point::new(start_row, 0)
    };
    let end = if end_row > max_point.row {
        max_point
    } else {
        Point::new(end_row, 0)
    };
    start..end.max(start)
}

fn header_segments_intersecting(
    segments: &[TranscriptDocumentSegment],
    byte_range: &Range<usize>,
) -> Range<usize> {
    if byte_range.is_empty() {
        return 0..0;
    }
    let start = segments.partition_point(|segment| segment.header_range.end <= byte_range.start);
    let end = segments.partition_point(|segment| segment.header_range.start < byte_range.end);
    start.min(end)..end
}

fn header_window_delta(
    mounted_positions: impl IntoIterator<Item = usize>,
    desired: Range<usize>,
) -> (Vec<usize>, Vec<usize>) {
    let mut mounted = mounted_positions.into_iter().collect::<Vec<_>>();
    mounted.sort_unstable();
    mounted.dedup();
    let remove = mounted
        .iter()
        .copied()
        .filter(|position| !desired.contains(position))
        .collect();
    let insert = desired
        .filter(|position| mounted.binary_search(position).is_err())
        .collect();
    (remove, insert)
}

fn segments_intersecting(
    segments: &[TranscriptDocumentSegment],
    byte_range: &Range<usize>,
) -> Range<usize> {
    if byte_range.is_empty() {
        return 0..0;
    }
    let start = segments.partition_point(|segment| segment.whole_range.end <= byte_range.start);
    let end = segments.partition_point(|segment| segment.whole_range.start < byte_range.end);
    start.min(end)..end
}

fn intersect_ranges(left: &Range<usize>, right: &Range<usize>) -> Option<Range<usize>> {
    let intersection = left.start.max(right.start)..left.end.min(right.end);
    (!intersection.is_empty()).then_some(intersection)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DiffHighlightRanges {
    file_headers: Vec<Range<usize>>,
    hunks: Vec<Range<usize>>,
    additions: Vec<Range<usize>>,
    deletions: Vec<Range<usize>>,
    parsed_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeDiffFileHeader {
    line_range: Range<usize>,
    item_key: String,
    file_index: usize,
    path: String,
    additions: usize,
    deletions: usize,
    counts_complete: bool,
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
            .to_owned(),
    )
}

fn diff_git_path(line: &str) -> Option<String> {
    let header = line.strip_prefix("diff --git ")?;
    header
        .rsplit_once(" b/")
        .map(|(_, path)| path)
        .or_else(|| header.rsplit_once(" \"b/").map(|(_, path)| path))
        .and_then(normalized_diff_path)
}

fn native_diff_file_headers(
    text: &str,
    base_offset: usize,
    item_key: &str,
    counts_complete: bool,
) -> Vec<NativeDiffFileHeader> {
    struct PendingHeader {
        line_range: Range<usize>,
        path: String,
        additions: usize,
        deletions: usize,
        in_hunk: bool,
    }

    let mut headers = Vec::new();
    let mut pending: Option<PendingHeader> = None;
    let mut line_offset = 0;
    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line_range = base_offset + line_offset..base_offset + line_offset + line.len();
        line_offset += raw_line.len();

        if let Some(path) = diff_git_path(line) {
            if let Some(previous) = pending.take() {
                let file_index = headers.len();
                headers.push(NativeDiffFileHeader {
                    line_range: previous.line_range,
                    item_key: item_key.to_owned(),
                    file_index,
                    path: previous.path,
                    additions: previous.additions,
                    deletions: previous.deletions,
                    counts_complete,
                });
            }
            pending = Some(PendingHeader {
                line_range,
                path,
                additions: 0,
                deletions: 0,
                in_hunk: false,
            });
            continue;
        }

        let Some(pending) = pending.as_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            pending.in_hunk = true;
        } else if pending.in_hunk && line.starts_with('+') && !line.starts_with("+++") {
            pending.additions += 1;
        } else if pending.in_hunk && line.starts_with('-') && !line.starts_with("---") {
            pending.deletions += 1;
        }
    }
    if let Some(previous) = pending {
        let file_index = headers.len();
        headers.push(NativeDiffFileHeader {
            line_range: previous.line_range,
            item_key: item_key.to_owned(),
            file_index,
            path: previous.path,
            additions: previous.additions,
            deletions: previous.deletions,
            counts_complete,
        });
    }
    headers
}

fn bounded_buffer_text(buffer: &Buffer, range: Range<usize>, byte_limit: usize) -> (String, bool) {
    let complete = range.len() <= byte_limit;
    let mut remaining = byte_limit;
    let mut text = String::with_capacity(range.len().min(byte_limit));
    for chunk in buffer.text_for_range(range) {
        if remaining == 0 {
            break;
        }
        if chunk.len() <= remaining {
            text.push_str(chunk);
            remaining -= chunk.len();
        } else {
            let mut end = remaining;
            while end > 0 && !chunk.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&chunk[..end]);
            break;
        }
    }
    (text, complete)
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

fn diff_gutter_inlays(text: &str, base_offset: usize) -> Vec<(usize, String)> {
    let mut inlays = Vec::new();
    let mut in_hunk = false;
    let mut old_line = None;
    let mut new_line = None;
    let mut line_offset = 0;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (displayed_old, displayed_new) =
            if line.starts_with("diff --git ") || line.starts_with("--- ") {
                in_hunk = false;
                (None, None)
            } else if line.starts_with("@@") {
                in_hunk = true;
                if let Some((old_start, new_start)) = diff_hunk_starts(line) {
                    old_line = Some(old_start);
                    new_line = Some(new_start);
                }
                (None, None)
            } else if in_hunk && line.starts_with('+') && !line.starts_with("+++") {
                let displayed = new_line;
                new_line = new_line.map(|line| line + 1);
                (None, displayed)
            } else if in_hunk && line.starts_with('-') && !line.starts_with("---") {
                let displayed = old_line;
                old_line = old_line.map(|line| line + 1);
                (displayed, None)
            } else if in_hunk && !line.starts_with("\\ No newline") {
                let displayed = (old_line, new_line);
                old_line = old_line.map(|line| line + 1);
                new_line = new_line.map(|line| line + 1);
                displayed
            } else {
                (None, None)
            };
        let old = displayed_old
            .map(|line| line.to_string())
            .unwrap_or_default();
        let new = displayed_new
            .map(|line| line.to_string())
            .unwrap_or_default();
        inlays.push((base_offset + line_offset, format!("{old:>3} {new:>3} ")));
        line_offset += raw_line.len();
    }
    inlays
}

impl DiffHighlightRanges {
    fn parse_body(&mut self, text: &str, base_offset: usize) {
        self.parsed_bytes += text.len();
        let mut line_offset = 0;
        for raw_line in text.split_inclusive('\n') {
            let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let line_range = base_offset + line_offset..base_offset + line_offset + line.len();
            line_offset += raw_line.len();
            if line_range.is_empty() {
                continue;
            }

            if is_diff_file_header(line) {
                self.file_headers.push(line_range);
            } else if line.starts_with("@@") {
                self.hunks.push(line_range);
            } else if line.starts_with('+') {
                self.additions.push(line_range);
            } else if line.starts_with('-') {
                self.deletions.push(line_range);
            }
        }
    }
}

fn is_diff_file_header(line: &str) -> bool {
    [
        "diff --git ",
        "--- ",
        "+++ ",
        "index ",
        "Index: ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "Binary files ",
        "GIT binary patch",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn visible_diff_body_ranges(
    segments: &[TranscriptDocumentSegment],
    viewport: &Range<usize>,
) -> Vec<Range<usize>> {
    let visible_segments = segments_intersecting(segments, viewport);
    segments[visible_segments]
        .iter()
        .filter(|segment| segment.kind == TranscriptKind::Diff)
        .filter_map(|segment| intersect_ranges(&segment.body_range, viewport))
        .collect()
}

#[derive(Debug, Default, Eq, PartialEq)]
struct SemanticHighlightRanges {
    headings: Vec<Range<usize>>,
    strong: Vec<Range<usize>>,
    emphasis: Vec<Range<usize>>,
    inline_code: Vec<Range<usize>>,
    links: Vec<Range<usize>>,
    code_blocks: Vec<Range<usize>>,
    block_quotes: Vec<Range<usize>>,
    strikethrough: Vec<Range<usize>>,
    command_invocations: Vec<Range<usize>>,
    command_outputs: Vec<Range<usize>>,
    scanned_spans: usize,
}

fn visible_semantic_highlight_ranges(
    segments: &[TranscriptDocumentSegment],
    viewport: &Range<usize>,
) -> SemanticHighlightRanges {
    let mut highlights = SemanticHighlightRanges::default();
    let visible_segments = segments_intersecting(segments, viewport);
    'segments: for segment in &segments[visible_segments] {
        // Spans are sorted by output start. Begin near the viewport instead of
        // rescanning all semantics above it in one huge narrative item. A
        // bounded lookbehind retains enclosing Heading/Strong/Link spans.
        let first_starting_in_viewport = segment
            .semantic_spans
            .partition_point(|span| span.range.start < viewport.start);
        let last_starting_in_viewport = segment
            .semantic_spans
            .partition_point(|span| span.range.start < viewport.end);
        let scan_start = first_starting_in_viewport.saturating_sub(MAX_SEMANTIC_SPAN_LOOKBEHIND);
        for span in &segment.semantic_spans[scan_start..last_starting_in_viewport] {
            if highlights.scanned_spans >= MAX_SCANNED_SEMANTIC_SPANS_PER_VIEWPORT {
                break 'segments;
            }
            highlights.scanned_spans += 1;
            let Some(range) = intersect_ranges(&span.range, viewport) else {
                continue;
            };
            match span.style {
                TranscriptSemanticStyle::Heading => highlights.headings.push(range),
                TranscriptSemanticStyle::Strong => highlights.strong.push(range),
                TranscriptSemanticStyle::Emphasis => highlights.emphasis.push(range),
                TranscriptSemanticStyle::InlineCode => highlights.inline_code.push(range),
                TranscriptSemanticStyle::Link => highlights.links.push(range),
                TranscriptSemanticStyle::CodeBlock => highlights.code_blocks.push(range),
                TranscriptSemanticStyle::BlockQuote => highlights.block_quotes.push(range),
                TranscriptSemanticStyle::Strikethrough => highlights.strikethrough.push(range),
                TranscriptSemanticStyle::CommandInvocation => {
                    highlights.command_invocations.push(range)
                }
                TranscriptSemanticStyle::CommandOutput => highlights.command_outputs.push(range),
            }
        }
    }
    highlights
}

fn semantic_monospace_ranges(segments: &[TranscriptDocumentSegment]) -> Vec<Range<usize>> {
    let mut ranges = segments
        .iter()
        .flat_map(|segment| segment.semantic_spans.iter())
        .filter(|span| {
            matches!(
                span.style,
                TranscriptSemanticStyle::InlineCode
                    | TranscriptSemanticStyle::CodeBlock
                    | TranscriptSemanticStyle::CommandInvocation
                    | TranscriptSemanticStyle::CommandOutput
            )
        })
        .map(|span| span.range.clone())
        .collect::<Vec<_>>();
    ranges.extend(
        segments
            .iter()
            .filter(|segment| segment.kind == TranscriptKind::Diff)
            .map(|segment| segment.body_range.clone()),
    );
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else if range.start < range.end {
            merged.push(range);
        }
    }
    merged
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellSemanticKind {
    Function,
    Variable,
    Keyword,
    Operator,
    Constant,
    String,
    Comment,
    Embedded,
    Punctuation,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ShellSemanticHighlightRanges {
    functions: Vec<Range<usize>>,
    variables: Vec<Range<usize>>,
    keywords: Vec<Range<usize>>,
    operators: Vec<Range<usize>>,
    constants: Vec<Range<usize>>,
    strings: Vec<Range<usize>>,
    comments: Vec<Range<usize>>,
    embedded: Vec<Range<usize>>,
    punctuation: Vec<Range<usize>>,
}

impl ShellSemanticHighlightRanges {
    fn push(&mut self, kind: ShellSemanticKind, range: Range<usize>) {
        match kind {
            ShellSemanticKind::Function => self.functions.push(range),
            ShellSemanticKind::Variable => self.variables.push(range),
            ShellSemanticKind::Keyword => self.keywords.push(range),
            ShellSemanticKind::Operator => self.operators.push(range),
            ShellSemanticKind::Constant => self.constants.push(range),
            ShellSemanticKind::String => self.strings.push(range),
            ShellSemanticKind::Comment => self.comments.push(range),
            ShellSemanticKind::Embedded => self.embedded.push(range),
            ShellSemanticKind::Punctuation => self.punctuation.push(range),
        }
    }
}

pub fn shell_capture_priority(capture_name: &str) -> u8 {
    match capture_name {
        "function" => 60,
        "variable" | "variable.special" => 55,
        "keyword" | "keyword.control" | "keyword.operator" => 50,
        "operator" => 45,
        "constant" | "number" => 40,
        "comment" | "keyword.directive" => 35,
        "embedded" | "punctuation.special" => 30,
        "punctuation.delimiter" | "punctuation.bracket" => 20,
        _ => 10,
    }
}

/// Parse shell syntax once for both transcript surfaces. Keeping capture ranges
/// in the Editor-side crate prevents the rich card and selectable buffer from
/// quietly developing different shell grammars or precedence rules.
pub fn shell_capture_ranges(command: &str) -> Vec<(Range<usize>, String)> {
    static QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
        Query::new(
            &tree_sitter_bash::LANGUAGE.into(),
            include_str!("../../grammars/src/bash/highlights.scm"),
        )
        .ok()
    });
    let Some(query) = QUERY.as_ref() else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(command, None) else {
        return Vec::new();
    };

    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), command.as_bytes());
    let mut captures = Vec::new();
    while let Some(query_match) = matches.next() {
        for capture in query_match.captures {
            let range = capture.node.byte_range();
            if !range.is_empty()
                && range.end <= command.len()
                && let Some(name) = capture_names.get(capture.index as usize)
            {
                captures.push((range, (*name).to_string()));
            }
        }
    }
    captures
}

fn shell_capture_kind(capture_name: &str) -> ShellSemanticKind {
    match capture_name {
        "function" => ShellSemanticKind::Function,
        "variable" | "variable.special" => ShellSemanticKind::Variable,
        "keyword" | "keyword.control" | "keyword.operator" => ShellSemanticKind::Keyword,
        "operator" => ShellSemanticKind::Operator,
        "constant" | "number" => ShellSemanticKind::Constant,
        "comment" | "keyword.directive" => ShellSemanticKind::Comment,
        "embedded" => ShellSemanticKind::Embedded,
        "string" | "string.escape" | "string.regex" | "character" => ShellSemanticKind::String,
        _ => ShellSemanticKind::Punctuation,
    }
}

fn shell_semantic_highlights(command: &str, base: usize) -> ShellSemanticHighlightRanges {
    let mut byte_styles = vec![(0_u8, None); command.len()];
    for (range, capture_name) in shell_capture_ranges(command) {
        let priority = shell_capture_priority(&capture_name);
        let kind = shell_capture_kind(&capture_name);
        for byte_style in &mut byte_styles[range] {
            if priority >= byte_style.0 {
                *byte_style = (priority, Some(kind));
            }
        }
    }

    let mut highlights = ShellSemanticHighlightRanges::default();
    let mut active_kind = None;
    let mut active_start = 0;
    for offset in command
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(command.len()))
    {
        let kind = byte_styles
            .get(offset)
            .and_then(|(_, kind)| kind.as_ref())
            .copied();
        if kind != active_kind {
            if let Some(kind) = active_kind {
                highlights.push(kind, base + active_start..base + offset);
            }
            active_kind = kind;
            active_start = offset;
        }
    }
    highlights
}

fn visible_shell_semantic_highlights(
    text: &str,
    text_base: usize,
    invocations: &[Range<usize>],
) -> ShellSemanticHighlightRanges {
    let mut highlights = ShellSemanticHighlightRanges::default();
    for range in invocations {
        let relative = range.start.saturating_sub(text_base)..range.end.saturating_sub(text_base);
        let Some(command) = text.get(relative) else {
            continue;
        };
        let parsed = shell_semantic_highlights(command, range.start);
        highlights.functions.extend(parsed.functions);
        highlights.variables.extend(parsed.variables);
        highlights.keywords.extend(parsed.keywords);
        highlights.operators.extend(parsed.operators);
        highlights.constants.extend(parsed.constants);
        highlights.strings.extend(parsed.strings);
        highlights.comments.extend(parsed.comments);
        highlights.embedded.extend(parsed.embedded);
        highlights.punctuation.extend(parsed.punctuation);
    }
    highlights
}

/// Return overlapping default search matches in one already bounded window.
/// The production helper below also carries the case and keyword-boundary
/// policy used by Vim word search.
#[cfg(test)]
fn literal_match_ranges(text: &str, query: &str, base_offset: usize) -> Vec<Range<usize>> {
    literal_match_ranges_with_options(
        text,
        query,
        base_offset,
        text.len() + base_offset,
        false,
        false,
    )
}

fn literal_match_ranges_with_options(
    text: &str,
    query: &str,
    base_offset: usize,
    document_len: usize,
    case_sensitive: bool,
    whole_word: bool,
) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let haystack = if case_sensitive {
        text.to_string()
    } else {
        text.to_ascii_lowercase()
    };
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_ascii_lowercase()
    };
    let mut matches = Vec::new();
    let mut search_start = 0;
    while search_start <= haystack.len() {
        let Some(relative_start) = haystack[search_start..].find(&needle) else {
            break;
        };
        let start = search_start + relative_start;
        let end = start + needle.len();
        if !whole_word || is_whole_keyword_match(text, start, end, base_offset, document_len) {
            matches.push(base_offset + start..base_offset + end);
        }
        search_start = next_char_boundary(text, start);
    }
    matches
}

fn is_keyword_character(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn is_whole_keyword_match(
    text: &str,
    start: usize,
    end: usize,
    base_offset: usize,
    document_len: usize,
) -> bool {
    let starts_at_boundary = if start == 0 {
        base_offset == 0
    } else {
        text[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !is_keyword_character(character))
    };
    let ends_at_boundary = if end == text.len() {
        base_offset + end == document_len
    } else {
        text[end..]
            .chars()
            .next()
            .is_none_or(|character| !is_keyword_character(character))
    };
    starts_at_boundary && ends_at_boundary
}

fn keyword_range_at_offset(text: &str, offset: usize) -> Option<Range<usize>> {
    let offset = previous_char_boundary(text, offset.min(text.len()));
    let character = text[offset..].chars().next()?;
    if !is_keyword_character(character) {
        return None;
    }

    let mut start = offset;
    while let Some((previous, character)) = text[..start].char_indices().next_back() {
        if !is_keyword_character(character) {
            break;
        }
        start = previous;
    }
    let mut end = offset + character.len_utf8();
    while let Some(character) = text[end..].chars().next() {
        if !is_keyword_character(character) {
            break;
        }
        end += character.len_utf8();
    }
    Some(start..end)
}

fn find_wrapped_literal_match(
    text: &str,
    query: &str,
    cursor_offset: usize,
    backwards: bool,
    case_sensitive: bool,
    whole_word: bool,
) -> Option<usize> {
    let matches =
        literal_match_ranges_with_options(text, query, 0, text.len(), case_sensitive, whole_word);
    let cursor_offset = previous_char_boundary(text, cursor_offset.min(text.len()));
    if backwards {
        matches
            .iter()
            .rev()
            .find(|range| range.start < cursor_offset)
            .or_else(|| matches.last())
            .map(|range| range.start)
    } else {
        matches
            .iter()
            .find(|range| range.start > cursor_offset)
            .or_else(|| matches.first())
            .map(|range| range.start)
    }
}

fn next_char_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    offset + text[offset..].chars().next().map_or(1, char::len_utf8)
}

fn repeated_search_backwards(original_backwards: bool, reverse: bool) -> bool {
    original_backwards ^ reverse
}

fn direct_scroll_can_change_follow_tail(local: bool, autoscroll: bool) -> bool {
    local && !autoscroll
}

fn viewport_bottom_is_near_tail(scroll_top: f64, visible_rows: f64, max_display_row: f64) -> bool {
    let viewport_bottom = scroll_top.max(0.) + visible_rows.max(0.);
    let document_bottom = max_display_row.max(0.) + 1.;
    viewport_bottom + FOLLOW_TAIL_SLOP_ROWS >= document_bottom
}

fn follow_tail_after_selection(
    was_following: bool,
    previous_offset: Option<usize>,
    current_offset: usize,
    document_len: usize,
) -> bool {
    current_offset == document_len
        || (was_following && previous_offset.is_some_and(|previous| current_offset >= previous))
}

fn should_request_tail_autoscroll(follow_tail: bool, content_changed: bool) -> bool {
    follow_tail && content_changed
}

fn collapse_relocation_offset(
    selection_start: usize,
    selection_end: usize,
    selection_head: usize,
    body: &Range<usize>,
) -> Option<usize> {
    (body.contains(&selection_head) || selection_start < body.end && body.start < selection_end)
        .then_some(body.end)
}

fn user_transcript_background(cx: &App) -> Hsla {
    cx.theme().colors().element_background
}

fn reasoning_transcript_background(cx: &App) -> Hsla {
    let _ = cx;
    Hsla::transparent_black()
}

fn structured_transcript_background(cx: &App) -> Hsla {
    cx.theme()
        .colors()
        .editor_subheader_background
        .opacity(0.28)
}

fn error_transcript_background(cx: &App) -> Hsla {
    cx.theme().status().error.opacity(0.1)
}

fn diff_addition_background(cx: &App) -> Hsla {
    cx.theme().status().created_background.opacity(0.14)
}

fn diff_deletion_background(cx: &App) -> Hsla {
    cx.theme().status().deleted_background.opacity(0.14)
}

fn transcript_card_border(cx: &App) -> Hsla {
    cx.theme().colors().border_variant.opacity(0.72)
}

fn transcript_section_rail(cx: &App) -> Hsla {
    cx.theme().colors().text_accent.opacity(0.55)
}

fn error_transcript_card_border(cx: &App) -> Hsla {
    cx.theme().status().error.opacity(0.46)
}

fn transcript_kind_is_card(kind: TranscriptKind) -> bool {
    matches!(
        kind,
        TranscriptKind::User
            | TranscriptKind::Command
            | TranscriptKind::FileChange
            | TranscriptKind::Tool
            | TranscriptKind::Diff
            | TranscriptKind::Image
            | TranscriptKind::Subagent
            | TranscriptKind::Web
            | TranscriptKind::Review
            | TranscriptKind::Error
            | TranscriptKind::Approval
    )
}

fn transcript_row_options(kind: TranscriptKind) -> RowHighlightOptions {
    let card = transcript_kind_is_card(kind);
    let section = matches!(kind, TranscriptKind::Reasoning | TranscriptKind::Plan);
    let border: Option<fn(&App) -> Hsla> = if section {
        Some(transcript_section_rail)
    } else if kind == TranscriptKind::Error {
        Some(error_transcript_card_border)
    } else if card {
        Some(transcript_card_border)
    } else {
        None
    };
    RowHighlightOptions {
        include_gutter: false,
        border,
        border_widths: section.then_some(Edges {
            left: px(2.),
            ..Edges::default()
        }),
        corner_radius: if card { px(6.) } else { Pixels::ZERO },
        vertical_margin: if card { px(3.) } else { Pixels::ZERO },
        merge_adjacent: !card,
        ..RowHighlightOptions::default()
    }
}

fn transcript_icon(kind: TranscriptKind) -> IconName {
    match kind {
        TranscriptKind::User => IconName::Person,
        TranscriptKind::Agent => IconName::AiOpenAi,
        TranscriptKind::Reasoning => IconName::ToolThink,
        TranscriptKind::Plan => IconName::ListTodo,
        TranscriptKind::Command => IconName::ToolTerminal,
        TranscriptKind::FileChange => IconName::FileDiff,
        TranscriptKind::Tool => IconName::ToolHammer,
        TranscriptKind::Diff => IconName::Diff,
        TranscriptKind::Image => IconName::Image,
        TranscriptKind::Subagent => IconName::UserGroup,
        TranscriptKind::Web => IconName::ToolWeb,
        TranscriptKind::Review => IconName::Eye,
        TranscriptKind::Trace => IconName::Code,
        TranscriptKind::Error => IconName::Warning,
        TranscriptKind::Approval => IconName::Lock,
    }
}

fn transcript_icon_color(kind: TranscriptKind, selected: bool) -> Color {
    if selected {
        return Color::Accent;
    }
    match kind {
        TranscriptKind::Error => Color::Error,
        TranscriptKind::User | TranscriptKind::Agent => Color::Default,
        _ => Color::Muted,
    }
}

fn native_header_text(text: &str) -> &str {
    text.strip_prefix("━━━━ ")
        .and_then(|text| text.strip_suffix(" ━━━━"))
        .unwrap_or(text)
}

fn native_header_shows_label(kind: TranscriptKind, label: &str) -> bool {
    kind != TranscriptKind::Agent || label != "Codex"
}

/// Whether an edit can retain the native header blocks' existing anchors.
///
/// Edits wholly contained by one semantic body are the common streaming case:
/// anchors before and after that body move with the buffer without losing
/// their identity. Everything structural is deliberately conservative. In
/// particular, a single minimal replacement may span two dirty bodies and the
/// unchanged header between them; anchors attached to that deleted header must
/// be rebuilt from the new document.
#[cfg(test)]
fn edit_invalidates_native_header_blocks(
    old_range: &Range<usize>,
    segments: &[TranscriptDocumentSegment],
) -> bool {
    if old_range.start > old_range.end {
        return true;
    }

    !segments.iter().any(|segment| {
        segment.body_range.start <= old_range.start && old_range.end <= segment.body_range.end
    })
}

fn item_body_start_at_offset(
    requested_offset: usize,
    segments: &[TranscriptDocumentSegment],
) -> usize {
    segments
        .iter()
        .find(|segment| {
            segment.whole_range.start == requested_offset
                || segment.whole_range.contains(&requested_offset)
        })
        .map_or(requested_offset, |segment| segment.body_range.start)
}

fn projection_has_valid_relative_ranges(projection: &TranscriptItemProjection) -> bool {
    let segment = &projection.segment;
    segment.whole_range == (0..projection.text.len())
        && segment.header_range.start <= segment.header_range.end
        && segment.header_range.end <= projection.text.len()
        && projection.text.is_char_boundary(segment.header_range.start)
        && projection.text.is_char_boundary(segment.header_range.end)
        && segment.body_range.start <= segment.body_range.end
        && segment.body_range.end <= projection.text.len()
        && projection.text.is_char_boundary(segment.body_range.start)
        && projection.text.is_char_boundary(segment.body_range.end)
        && segment.header_range.end <= segment.body_range.start
        && semantic_spans_are_valid(
            &segment.semantic_spans,
            &segment.body_range,
            &projection.text,
        )
}

fn semantic_spans_are_valid(
    spans: &[TranscriptSemanticSpan],
    body_range: &Range<usize>,
    text: &str,
) -> bool {
    spans.len() <= MAX_SEMANTIC_SPANS_PER_SEGMENT
        && spans.iter().all(|span| {
            body_range.start <= span.range.start
                && span.range.start < span.range.end
                && span.range.end <= body_range.end
                && text.is_char_boundary(span.range.start)
                && text.is_char_boundary(span.range.end)
        })
        && spans.windows(2).all(|pair| {
            let left = &pair[0];
            let right = &pair[1];
            (left.range.start, left.range.end, left.style)
                <= (right.range.start, right.range.end, right.style)
        })
}

/// Validate the semantic byte index before any range is sliced or converted
/// into display-map anchors. The underlying Buffer remains the source of truth
/// for selectable text; malformed metadata must only disable decoration, never
/// panic the UI or attach a header/tool surface to the wrong message.
fn document_has_valid_segment_ranges(document: &TranscriptDocument) -> bool {
    let text_len = document.text.len();
    if document
        .item_rows
        .iter()
        .filter(|row| row.is_some())
        .count()
        != document.segments.len()
    {
        return false;
    }

    let mut previous_item_index = None;
    let mut previous_whole_end = 0;
    let mut previous_row = None;
    let mut item_keys = BTreeSet::new();
    for segment in &document.segments {
        let Some(row) = document
            .item_rows
            .get(segment.item_index)
            .copied()
            .flatten()
        else {
            return false;
        };
        if previous_item_index.is_some_and(|previous| previous >= segment.item_index)
            || previous_row.is_some_and(|previous| previous >= row)
            || previous_whole_end > segment.whole_range.start
            || !item_keys.insert(segment.item_key.as_str())
        {
            return false;
        }

        let ranges_are_ordered = segment.whole_range.start < segment.whole_range.end
            && segment.header_range.start == segment.whole_range.start
            && segment.header_range.start <= segment.header_range.end
            && segment.header_range.end <= segment.body_range.start
            && segment.body_range.start <= segment.body_range.end
            && segment.body_range.end <= segment.whole_range.end
            && segment.whole_range.end <= text_len;
        let boundaries_are_utf8 = [
            segment.whole_range.start,
            segment.whole_range.end,
            segment.header_range.start,
            segment.header_range.end,
            segment.body_range.start,
            segment.body_range.end,
        ]
        .into_iter()
        .all(|offset| document.text.is_char_boundary(offset));
        if !ranges_are_ordered || !boundaries_are_utf8 {
            return false;
        }
        if !semantic_spans_are_valid(&segment.semantic_spans, &segment.body_range, &document.text) {
            return false;
        }

        previous_item_index = Some(segment.item_index);
        previous_whole_end = segment.whole_range.end;
        previous_row = Some(row);
    }
    true
}

fn shift_range(range: &mut Range<usize>, delta: isize) -> bool {
    let Some(start) = range.start.checked_add_signed(delta) else {
        return false;
    };
    let Some(end) = range.end.checked_add_signed(delta) else {
        return false;
    };
    range.start = start;
    range.end = end;
    true
}

fn range_at_offset(range: &Range<usize>, offset: usize) -> Range<usize> {
    range.start + offset..range.end + offset
}

/// Convert protocol/document byte offsets into Zed anchors without allowing a
/// stale streaming range to panic the UI process. Protocol projections are
/// expected to contain UTF-8 boundaries, but clipping here is the final trust
/// boundary before entering the Editor display map.
fn clipped_anchor_pair(snapshot: &MultiBufferSnapshot, range: Range<usize>) -> (Anchor, Anchor) {
    let start = snapshot.clip_offset(MultiBufferOffset(range.start), Bias::Left);
    let end = snapshot.clip_offset(MultiBufferOffset(range.end), Bias::Right);
    (snapshot.anchor_before(start), snapshot.anchor_after(end))
}

fn clipped_anchor_range(snapshot: &MultiBufferSnapshot, range: Range<usize>) -> Range<Anchor> {
    let (start, end) = clipped_anchor_pair(snapshot, range);
    start..end
}

fn clipped_anchor_after(snapshot: &MultiBufferSnapshot, offset: usize) -> Anchor {
    let offset = snapshot.clip_offset(MultiBufferOffset(offset), Bias::Right);
    snapshot.anchor_after(offset)
}

/// Apply the length/offset effects of one already-validated item projection.
/// Callers process items from the end of the document toward the beginning.
fn apply_projected_segment_shape(
    segments: &mut [TranscriptDocumentSegment],
    segment_position: usize,
    projected: &TranscriptDocumentSegment,
) -> bool {
    let Some(current) = segments.get(segment_position) else {
        return false;
    };
    let current_body_offset = current.body_range.start - current.whole_range.start;
    let projected_body_offset = projected.body_range.start - projected.whole_range.start;
    if current_body_offset != projected_body_offset {
        return false;
    }

    let old_whole_len = current.whole_range.len();
    let new_whole_len = projected.whole_range.len();
    let delta = new_whole_len as isize - old_whole_len as isize;
    let body_start = current.body_range.start;
    let whole_start = current.whole_range.start;
    let new_body_end = body_start + projected.body_range.len();
    let new_whole_end = whole_start + new_whole_len;

    let current = &mut segments[segment_position];
    current.body_range.end = new_body_end;
    current.whole_range.end = new_whole_end;
    current.semantic_spans = projected
        .semantic_spans
        .iter()
        .map(|span| TranscriptSemanticSpan {
            range: range_at_offset(&span.range, whole_start),
            style: span.style,
        })
        .collect();
    for later in &mut segments[segment_position + 1..] {
        if !shift_range(&mut later.whole_range, delta)
            || !shift_range(&mut later.header_range, delta)
            || !shift_range(&mut later.body_range, delta)
        {
            return false;
        }
        for span in &mut later.semantic_spans {
            if !shift_range(&mut span.range, delta) {
                return false;
            }
        }
    }
    true
}

fn native_header_block(
    placement: std::ops::RangeInclusive<Anchor>,
    item_key: String,
    kind: TranscriptKind,
    label: SharedString,
    foldable: bool,
    transcript: WeakEntity<TranscriptEditor>,
) -> BlockProperties<Anchor> {
    let show_label = native_header_shows_label(kind, &label);
    BlockProperties {
        placement: BlockPlacement::Replace(placement),
        height: Some(1),
        // Spacer blocks scroll with the document but do not reserve the hidden
        // editor gutter, which makes the header read as part of the transcript.
        style: BlockStyle::Spacer,
        render: Arc::new(move |cx: &mut BlockContext| {
            let collapsed = transcript.upgrade().is_some_and(|transcript| {
                transcript.read(cx.app).collapsed_items.contains(&item_key)
            });
            let icon_color = transcript_icon_color(kind, cx.selected);
            let colors = cx.theme().colors();
            let card = transcript_kind_is_card(kind);
            let background = if card || !cx.selected {
                Hsla::transparent_black()
            } else {
                colors.editor_highlighted_line_background
            };

            let header = div()
                .id(cx.block_id)
                .h(cx.line_height)
                .w_full()
                .min_w_0()
                .pl_2()
                .pr_2()
                .flex()
                .items_center()
                .gap_2()
                .overflow_hidden()
                .bg(background)
                .child(
                    Icon::new(transcript_icon(kind))
                        .size(IconSize::XSmall)
                        .color(icon_color),
                )
                .when(show_label, |this| {
                    this.child(
                        Label::new(label.clone())
                            .size(LabelSize::XSmall)
                            .color(if cx.selected {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .truncate(),
                    )
                });

            if foldable {
                let item_key = item_key.clone();
                let disclosure_id = format!("transcript-fold:{item_key}");
                let transcript = transcript.clone();
                header
                    .cursor_pointer()
                    .on_click(move |_, window, cx| {
                        transcript
                            .update(cx, |transcript, cx| {
                                transcript.toggle_item_collapsed(&item_key, window, cx);
                            })
                            .ok();
                    })
                    .child(Disclosure::new(disclosure_id, !collapsed))
                    .into_any_element()
            } else {
                header.into_any_element()
            }
        }),
        priority: 0,
    }
}

fn native_diff_file_header_block(
    placement: RangeInclusive<Anchor>,
    header: NativeDiffFileHeader,
) -> BlockProperties<Anchor> {
    BlockProperties {
        placement: BlockPlacement::Replace(placement),
        height: Some(1),
        style: BlockStyle::Spacer,
        render: Arc::new(move |cx: &mut BlockContext| {
            let colors = cx.theme().colors();
            let path = header.path.clone();
            let stat_id = format!("native-diff-stat:{}:{}", header.item_key, header.file_index);
            div()
                .id(cx.block_id)
                .h(cx.line_height)
                .w_full()
                .min_w_0()
                .px_2()
                .flex()
                .items_center()
                .gap_2()
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
                        .font_buffer(cx.app)
                        .text_ui_sm(cx.app)
                        .truncate()
                        .child(path),
                )
                .when(
                    header.counts_complete && (header.additions > 0 || header.deletions > 0),
                    |this| {
                        this.child(
                            DiffStat::new(stat_id, header.additions, header.deletions)
                                .label_size(LabelSize::XSmall),
                        )
                    },
                )
                .into_any_element()
        }),
        priority: 1,
    }
}

fn transcript_item_is_foldable(segment: &TranscriptDocumentSegment) -> bool {
    !segment.body_range.is_empty()
        && matches!(
            segment.kind,
            TranscriptKind::Reasoning
                | TranscriptKind::Command
                | TranscriptKind::FileChange
                | TranscriptKind::Tool
                | TranscriptKind::Diff
                | TranscriptKind::Subagent
                | TranscriptKind::Web
                | TranscriptKind::Review
        )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupplementUpdate {
    Unchanged,
    Resize,
    ReplaceRenderer,
    ResizeAndReplaceRenderer,
    Reanchor,
}

fn supplement_update(
    item_changed: bool,
    rows_changed: bool,
    view_changed: bool,
) -> SupplementUpdate {
    if item_changed {
        SupplementUpdate::Reanchor
    } else {
        match (rows_changed, view_changed) {
            (false, false) => SupplementUpdate::Unchanged,
            (true, false) => SupplementUpdate::Resize,
            (false, true) => SupplementUpdate::ReplaceRenderer,
            (true, true) => SupplementUpdate::ResizeAndReplaceRenderer,
        }
    }
}

fn supplement_anchor_offset(
    item_key: &str,
    segments: &[TranscriptDocumentSegment],
) -> Option<usize> {
    segments
        .iter()
        .find(|segment| segment.item_key == item_key)
        .map(|segment| segment.body_range.end)
}

fn scroll_top_after_supplement_removal(
    block_row: f64,
    block_rows: u32,
    scroll_top: f64,
) -> Option<f64> {
    (block_row + f64::from(block_rows) <= scroll_top)
        .then(|| (scroll_top - f64::from(block_rows)).max(0.))
}

fn supplemental_renderer(view: AnyView) -> RenderBlock {
    Arc::new(move |cx: &mut BlockContext| {
        div()
            .id(cx.block_id)
            .size_full()
            .min_w_0()
            .block_mouse_except_scroll()
            .child(view.clone())
            .into_any_element()
    })
}

fn supplemental_block(placement: Anchor, rows: u32, view: AnyView) -> BlockProperties<Anchor> {
    BlockProperties {
        placement: BlockPlacement::Below(placement),
        height: Some(rows.max(1)),
        style: BlockStyle::Spacer,
        render: supplemental_renderer(view),
        priority: 0,
    }
}

fn replacement_anchor_range(
    item_key: &str,
    segments: &[TranscriptDocumentSegment],
) -> Option<Range<usize>> {
    segments
        .iter()
        .find(|segment| segment.item_key == item_key)
        .map(|segment| segment.whole_range.clone())
}

/// Translate a half-open document item range into the inclusive byte offsets
/// expected by an Editor replacement block. Using the exclusive end directly
/// makes adjacent items share a display row, so BlockMap coalesces their two
/// replacement renderers and only one remains visible.
fn replacement_anchor_offsets(range: Range<usize>) -> Option<(usize, usize)> {
    (!range.is_empty()).then(|| (range.start, range.end - 1))
}

fn replacement_renderer(view: AnyView) -> RenderBlock {
    Arc::new(move |cx: &mut BlockContext| {
        div()
            .id(cx.block_id)
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .block_mouse_except_scroll()
            .child(view.clone())
            .into_any_element()
    })
}

fn replacement_block(
    placement: RangeInclusive<Anchor>,
    rows: u32,
    view: AnyView,
) -> BlockProperties<Anchor> {
    BlockProperties {
        placement: BlockPlacement::Replace(placement),
        height: Some(rows.max(1)),
        style: BlockStyle::Spacer,
        render: replacement_renderer(view),
        priority: 1,
    }
}

impl TranscriptEditor {
    pub fn read_only(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Unlike the composer, this Buffer is a mixed semantic document.
        // Completed narrative bodies are already projected to readable text,
        // while command/tool bodies intentionally preserve Markdown-looking
        // output literally. Applying one Markdown grammar to the entire Buffer
        // would therefore style terminal output as prose markup.
        let buffer = cx.new(|cx| Buffer::local("", cx));
        let reading_font = font_for_typography_profile(TranscriptTypographyProfile::Reading, cx);
        let editor = cx.new({
            let buffer = buffer.clone();
            |cx| {
                let mut editor = Editor::for_local_buffer(buffer, window, cx);
                editor.set_read_only(true);
                editor.set_use_modal_editing(true);
                editor.set_current_line_highlight(None);
                editor.set_soft_wrap();
                editor.set_show_gutter(false, cx);
                editor.set_show_indent_guides(false, cx);
                editor.set_show_wrap_guides(false, cx);
                editor.set_show_horizontal_scrollbar(false, cx);
                editor.disable_mouse_wheel_zoom();
                editor.register_addon(TranscriptKeyContextAddon);
                editor.set_text_style_refinement(typography_refinement(&reading_font));
                let mut style = editor.style(cx).clone();
                apply_typography_font(&mut style.text, &reading_font);
                editor.set_style(style, window, cx);
                editor
            }
        });
        cx.subscribe_in(&editor, window, |transcript, _, event, window, cx| {
            if matches!(event, EditorEvent::SelectionsChanged { local: true }) {
                cx.emit(TranscriptSelectionChanged);
                transcript.note_local_selection_for_follow_tail();
                transcript.schedule_viewport_refresh(window, cx);
            }
            if let EditorEvent::ScrollPositionChanged { local, autoscroll } = event {
                if direct_scroll_can_change_follow_tail(*local, *autoscroll) {
                    transcript.follow_tail = false;
                    transcript.pending_tail_intent = Some(PendingTailIntent::DirectScroll);
                }
                transcript.schedule_viewport_refresh(window, cx);
            }
        })
        .detach();
        // A bounds observer gives viewport decoration one post-layout refresh
        // for a real resize. Do not request a frame from `Render`: GPUI's
        // next-frame callback itself creates frame demand, so doing that on
        // every render keeps an otherwise-idle transcript pumping frames.
        cx.observe_window_bounds(window, |transcript, window, cx| {
            transcript.schedule_viewport_refresh(window, cx);
        })
        .detach();
        Self {
            buffer,
            editor,
            input_only: false,
            typography_profile: TranscriptTypographyProfile::Reading,
            segments: Vec::new(),
            segment_header_texts: Vec::new(),
            segment_body_texts: Vec::new(),
            model_item_count: 0,
            header_blocks: BTreeMap::new(),
            diff_file_blocks: Vec::new(),
            collapsed_items: BTreeSet::new(),
            padding_inlays: Vec::new(),
            next_padding_inlay_id: 0,
            supplements: BTreeMap::new(),
            replacements: BTreeMap::new(),
            viewport_decorations: None,
            viewport_refresh_pending: false,
            refresh_when_rendered: false,
            diff_highlights_dirty: true,
            semantic_highlights_dirty: true,
            search: TranscriptSearchState::default(),
            follow_tail: false,
            last_selection_head: None,
            pending_tail_intent: None,
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.buffer.read(cx).text()
    }

    pub fn typography_profile(&self) -> TranscriptTypographyProfile {
        self.typography_profile
    }

    pub fn set_input_only(&mut self, input_only: bool, cx: &mut Context<Self>) {
        if self.input_only == input_only {
            self.editor
                .update(cx, |editor, cx| editor.set_input_only(input_only, cx));
            return;
        }
        self.input_only = input_only;

        // Rich mode keeps this Editor mounted only for input, selection, Vim,
        // and text geometry. Native header/diff/supplement replacement blocks
        // exist solely to paint the Text view. Leaving them mounted in the
        // hidden mirror makes an ordinary j/k motion expand to the replaced
        // source range, which Vim correctly interprets as a visual selection.
        // Drop those paint-only blocks while input-only and remount them from
        // the retained logical specs when the Text view is restored.
        if input_only {
            self.unmount_all_supplements(cx);
            self.unmount_all_replacements(cx);
            let header_blocks = std::mem::take(&mut self.header_blocks);
            let diff_file_blocks = std::mem::take(&mut self.diff_file_blocks);
            if !header_blocks.is_empty() || !diff_file_blocks.is_empty() {
                self.editor.update(cx, |editor, cx| {
                    editor.remove_blocks(
                        header_blocks
                            .into_values()
                            .chain(diff_file_blocks)
                            .collect(),
                        None,
                        cx,
                    );
                });
            }
        }
        self.viewport_decorations = None;
        self.diff_highlights_dirty = true;
        self.semantic_highlights_dirty = true;
        self.search.highlights_dirty = true;
        self.editor
            .update(cx, |editor, cx| editor.set_input_only(input_only, cx));
    }

    /// Change the whole transcript between the user's buffer font and Zed's UI
    /// reading font without rebuilding or replacing the selectable document.
    ///
    /// `Editor::set_style` immediately propagates the new font metrics into the
    /// display map, forcing a correct soft-wrap pass. The persistent refinement
    /// ensures subsequent Editor renders keep the selected family while leaving
    /// buffer font size and line height untouched.
    pub fn set_typography_profile(
        &mut self,
        profile: TranscriptTypographyProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !typography_profile_changed(self.typography_profile, profile) {
            return false;
        }

        self.editor.update(cx, |editor, cx| {
            apply_typography_profile_to_editor(editor, profile, window, cx)
        });
        self.typography_profile = profile;
        cx.notify();
        true
    }

    pub fn cursor_offset(&self, cx: &mut App) -> usize {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            editor
                .selections
                .newest_anchor()
                .head()
                .to_offset(&snapshot)
                .0
        })
    }

    /// Project the real Editor/Vim selection into the selectable body of every
    /// Rich item it intersects. Rich rendering remains completely independent
    /// of Editor layout; this is only a shared logical-offset contract.
    pub fn selection_snapshot(&self, cx: &mut App) -> TranscriptSelectionSnapshot {
        let (selection, linewise) = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let mut selection = editor
                .selections
                .newest_anchor()
                .map(|anchor| anchor.to_offset(&snapshot));
            let linewise = editor.selections.line_mode() && !selection.is_empty();
            if linewise {
                let start_point = selection.start.to_point(&snapshot);
                let end_point = selection.end.to_point(&snapshot);
                let rows = linewise_selection_rows(start_point, end_point);
                selection.start = Point::new(*rows.start(), 0).to_offset(&snapshot);
                let last_row = *rows.end();
                selection.end = if last_row < snapshot.max_point().row {
                    Point::new(last_row + 1, 0).to_offset(&snapshot)
                } else {
                    Point::new(last_row, snapshot.line_len(MultiBufferRow(last_row)))
                        .to_offset(&snapshot)
                };
            }
            (selection.map(|offset| offset.0), linewise)
        });
        let head = selection.head();
        let visual = !selection.is_empty();
        let mut items = Vec::new();

        for (segment_position, segment) in self.segments.iter().enumerate() {
            let body = segment.body_range.clone();
            let intersects = if visual {
                selection.start < body.end && body.start < selection.end
            } else {
                segment.whole_range.start <= head && head <= segment.whole_range.end
            };
            if !intersects {
                continue;
            }

            let body_text = self
                .segment_body_texts
                .get(segment_position)
                .cloned()
                .unwrap_or_else(|| {
                    Arc::from(
                        self.buffer
                            .read(cx)
                            .text_for_range(body.clone())
                            .collect::<String>(),
                    )
                });
            let range = if visual {
                selection.start.max(body.start) - body.start
                    ..selection.end.min(body.end) - body.start
            } else {
                let offset = head.clamp(body.start, body.end) - body.start;
                offset..offset
            };
            let item_head = (body.start..=body.end)
                .contains(&head)
                .then_some(head.clamp(body.start, body.end) - body.start);
            items.push(TranscriptItemSelection {
                item_index: segment.item_index,
                body_text,
                range,
                head: item_head,
            });
        }

        TranscriptSelectionSnapshot {
            visual,
            linewise,
            reversed: selection.reversed,
            items,
        }
    }

    /// Place the native cursor at an item-relative logical byte offset. This
    /// is the mouse-placement bridge for Rich text runs.
    pub fn set_cursor_in_item(
        &mut self,
        item_index: usize,
        body_offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(segment) = self
            .segments
            .iter()
            .find(|segment| segment.item_index == item_index)
        else {
            return false;
        };
        let offset = (segment.body_range.start + body_offset).min(segment.body_range.end);
        let text = self.buffer.read(cx).text();
        let point = offset_to_point(&text, offset);
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([point..point]);
            });
        });
        true
    }

    pub fn selected_item(&self, cx: &mut App) -> Option<usize> {
        let cursor_offset = self.cursor_offset(cx);
        self.item_at_offset(cursor_offset)
    }

    /// Return the semantic item intersecting the top of the visible Editor
    /// viewport. Hosts use this as a scroll anchor when switching between a
    /// rich list projection and the selectable text projection.
    pub fn top_visible_item(&mut self, cx: &mut App) -> Option<usize> {
        let visible_offset = self.editor.update(cx, |editor, cx| {
            let display_snapshot = editor.display_snapshot(cx);
            let visible_range = editor.multi_buffer_visible_range(&display_snapshot, cx);
            visible_range
                .start
                .to_offset(display_snapshot.buffer_snapshot())
                .0
        });
        self.item_at_offset(visible_offset)
    }

    fn item_at_offset(&self, offset: usize) -> Option<usize> {
        let segment_index = self
            .segments
            .partition_point(|segment| segment.whole_range.end <= offset)
            .min(self.segments.len().saturating_sub(1));
        self.segments
            .get(segment_index)
            .or_else(|| {
                self.segments
                    .iter()
                    .rev()
                    .find(|segment| segment.whole_range.start <= offset)
            })
            .map(|segment| segment.item_index)
    }

    pub fn edit(&mut self, old_range: Range<usize>, replacement: String, cx: &mut Context<Self>) {
        let text_changed = !old_range.is_empty() || !replacement.is_empty();
        self.buffer.update(cx, |buffer, cx| {
            buffer.edit([(old_range, replacement)], None, cx);
        });
        self.search.highlights_dirty = true;
        if should_request_tail_autoscroll(self.follow_tail, text_changed) {
            self.request_tail_autoscroll(cx);
        }
    }

    /// Insert or update a rich view below the selectable body of an item.
    ///
    /// Resizing and replacing the host view retain the existing Editor block
    /// id. Moving the same logical supplement to a different item deliberately
    /// re-anchors it. If its item is not projected yet, the spec remains queued
    /// and mounts as soon as that segment is appended or the document rebuilds.
    pub fn upsert_supplement(&mut self, supplement: TranscriptSupplement, cx: &mut Context<Self>) {
        let key = supplement.key;
        let was_present = self.supplements.contains_key(&key);
        let rows = supplement.rows.max(1);
        let mut mounted =
            self.supplements
                .remove(&key)
                .unwrap_or_else(|| MountedTranscriptSupplement {
                    item_key: supplement.item_key.clone(),
                    rows,
                    view: supplement.view.clone(),
                    block_id: None,
                });
        let update = supplement_update(
            mounted.item_key != supplement.item_key,
            mounted.rows != rows,
            mounted.view != supplement.view,
        );
        let display_changed = !was_present || update != SupplementUpdate::Unchanged;
        mounted.item_key = supplement.item_key;
        mounted.rows = rows;
        mounted.view = supplement.view;

        if let Some(block_id) = mounted.block_id {
            match update {
                SupplementUpdate::Unchanged => {}
                SupplementUpdate::Resize => {
                    self.editor.update(cx, |editor, cx| {
                        editor.resize_blocks([(block_id, rows)].into_iter().collect(), None, cx)
                    });
                }
                SupplementUpdate::ReplaceRenderer => {
                    let renderer = supplemental_renderer(mounted.view.clone());
                    self.editor.update(cx, |editor, cx| {
                        editor.replace_blocks(
                            [(block_id, renderer)].into_iter().collect(),
                            None,
                            cx,
                        )
                    });
                }
                SupplementUpdate::ResizeAndReplaceRenderer => {
                    let renderer = supplemental_renderer(mounted.view.clone());
                    self.editor.update(cx, |editor, cx| {
                        editor.resize_blocks([(block_id, rows)].into_iter().collect(), None, cx);
                        editor.replace_blocks(
                            [(block_id, renderer)].into_iter().collect(),
                            None,
                            cx,
                        );
                    });
                }
                SupplementUpdate::Reanchor => {
                    self.editor.update(cx, |editor, cx| {
                        editor.remove_blocks([block_id].into_iter().collect(), None, cx)
                    });
                    mounted.block_id = None;
                }
            }
        }
        self.supplements.insert(key, mounted);
        self.mount_unmounted_supplements(cx);
        if should_request_tail_autoscroll(self.follow_tail, display_changed) {
            self.request_tail_autoscroll(cx);
        }
    }

    /// Remove one logical supplement without disturbing the buffer selection.
    /// If the block is wholly above a paused viewport, preserve the same
    /// visible buffer content by compensating for the removed display rows.
    pub fn remove_supplement(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(supplement) = self.supplements.remove(key) else {
            return false;
        };
        let Some(block_id) = supplement.block_id else {
            return true;
        };
        let was_following_tail = self.follow_tail;
        self.editor.update(cx, |editor, cx| {
            let scroll_position = editor.scroll_position(cx);
            let adjusted_scroll_top = (!was_following_tail)
                .then(|| editor.row_for_block(block_id, cx))
                .flatten()
                .and_then(|row| {
                    scroll_top_after_supplement_removal(
                        row.0 as f64,
                        supplement.rows,
                        scroll_position.y,
                    )
                });
            editor.remove_blocks([block_id].into_iter().collect(), None, cx);
            if let Some(scroll_top) = adjusted_scroll_top {
                editor.set_scroll_position(point(scroll_position.x, scroll_top), window, cx);
            }
        });
        if was_following_tail {
            self.request_tail_autoscroll(cx);
        }
        true
    }

    /// Drop all host-owned supplemental views and their mounted blocks.
    pub fn clear_supplements(&mut self, cx: &mut Context<Self>) {
        let block_ids = self
            .supplements
            .values()
            .filter_map(|supplement| supplement.block_id)
            .collect::<Vec<_>>();
        self.supplements.clear();
        if !block_ids.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(block_ids.into_iter().collect(), None, cx)
            });
            if self.follow_tail {
                self.request_tail_autoscroll(cx);
            }
        }
    }

    /// Replace one projected item with a host-rendered semantic component.
    ///
    /// This is the hybrid transcript seam: the Buffer remains authoritative,
    /// while the Editor display map presents a richer atomic view for item
    /// types whose layout cannot be expressed by row and text highlights.
    pub fn upsert_replacement(
        &mut self,
        replacement: TranscriptReplacement,
        cx: &mut Context<Self>,
    ) {
        let key = replacement.key;
        let was_present = self.replacements.contains_key(&key);
        let rows = replacement.rows.max(1);
        let mut mounted =
            self.replacements
                .remove(&key)
                .unwrap_or_else(|| MountedTranscriptReplacement {
                    item_key: replacement.item_key.clone(),
                    rows,
                    view: replacement.view.clone(),
                    block_id: None,
                });
        let update = supplement_update(
            mounted.item_key != replacement.item_key,
            mounted.rows != rows,
            mounted.view != replacement.view,
        );
        let display_changed = !was_present || update != SupplementUpdate::Unchanged;
        mounted.item_key = replacement.item_key;
        mounted.rows = rows;
        mounted.view = replacement.view;

        if let Some(block_id) = mounted.block_id {
            match update {
                SupplementUpdate::Unchanged => {}
                SupplementUpdate::Resize => self.editor.update(cx, |editor, cx| {
                    editor.resize_blocks([(block_id, rows)].into_iter().collect(), None, cx)
                }),
                SupplementUpdate::ReplaceRenderer => {
                    let renderer = replacement_renderer(mounted.view.clone());
                    self.editor.update(cx, |editor, cx| {
                        editor.replace_blocks(
                            [(block_id, renderer)].into_iter().collect(),
                            None,
                            cx,
                        )
                    });
                }
                SupplementUpdate::ResizeAndReplaceRenderer => {
                    let renderer = replacement_renderer(mounted.view.clone());
                    self.editor.update(cx, |editor, cx| {
                        editor.resize_blocks([(block_id, rows)].into_iter().collect(), None, cx);
                        editor.replace_blocks(
                            [(block_id, renderer)].into_iter().collect(),
                            None,
                            cx,
                        );
                    });
                }
                SupplementUpdate::Reanchor => {
                    self.editor.update(cx, |editor, cx| {
                        editor.remove_blocks([block_id].into_iter().collect(), None, cx)
                    });
                    mounted.block_id = None;
                }
            }
        }
        self.replacements.insert(key, mounted);
        self.mount_unmounted_replacements(cx);
        if should_request_tail_autoscroll(self.follow_tail, display_changed) {
            self.request_tail_autoscroll(cx);
        }
    }

    pub fn remove_replacement(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(replacement) = self.replacements.remove(key) else {
            return false;
        };
        if let Some(block_id) = replacement.block_id {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks([block_id].into_iter().collect(), None, cx)
            });
        }
        self.viewport_decorations = None;
        true
    }

    /// Reveal an already-mounted supplement without changing the transcript
    /// cursor or selection. Tall blocks align to the top; shorter blocks move
    /// only enough to fit inside the current Editor viewport.
    pub fn reveal_supplement(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(supplement) = self.supplements.get(key) else {
            return false;
        };
        let Some(block_id) = supplement.block_id else {
            return false;
        };
        let rows = f64::from(supplement.rows);
        self.editor.update(cx, |editor, cx| {
            let Some(block_row) = editor.row_for_block(block_id, cx).map(|row| row.0 as f64) else {
                return false;
            };
            let visible_rows = editor.visible_line_count().unwrap_or(1.).max(1.);
            let scroll_position = editor.scroll_position(cx);
            let scroll_bottom = scroll_position.y + visible_rows;
            let target = if rows >= visible_rows || block_row < scroll_position.y {
                Some(block_row)
            } else if block_row + rows > scroll_bottom {
                Some(block_row + rows - visible_rows)
            } else {
                None
            };
            if let Some(scroll_top) = target {
                editor.set_scroll_position(
                    point(scroll_position.x, scroll_top.max(0.)),
                    window,
                    cx,
                );
            }
            true
        })
    }

    fn unmount_all_supplements(&mut self, cx: &mut Context<Self>) {
        let block_ids = self
            .supplements
            .values_mut()
            .filter_map(|supplement| supplement.block_id.take())
            .collect::<Vec<_>>();
        if !block_ids.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(block_ids.into_iter().collect(), None, cx)
            });
        }
    }

    fn unmount_all_replacements(&mut self, cx: &mut Context<Self>) {
        let block_ids = self
            .replacements
            .values_mut()
            .filter_map(|replacement| replacement.block_id.take())
            .collect::<Vec<_>>();
        if !block_ids.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(block_ids.into_iter().collect(), None, cx)
            });
        }
    }

    fn mount_unmounted_supplements(&mut self, cx: &mut Context<Self>) {
        if self.input_only {
            return;
        }
        let pending = self
            .supplements
            .iter()
            .filter(|(_, supplement)| supplement.block_id.is_none())
            .filter_map(|(key, supplement)| {
                Some((
                    key.clone(),
                    supplement_anchor_offset(&supplement.item_key, &self.segments)?,
                    supplement.rows,
                    supplement.view.clone(),
                ))
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return;
        }
        let block_ids = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let blocks = pending.iter().map(|(_, offset, rows, view)| {
                supplemental_block(
                    clipped_anchor_after(&snapshot, *offset),
                    *rows,
                    view.clone(),
                )
            });
            editor.insert_blocks(blocks, None, cx)
        });
        debug_assert_eq!(pending.len(), block_ids.len());
        for ((key, _, _, _), block_id) in pending.into_iter().zip(block_ids) {
            if let Some(supplement) = self.supplements.get_mut(&key) {
                supplement.block_id = Some(block_id);
            }
        }
    }

    fn mount_unmounted_replacements(&mut self, cx: &mut Context<Self>) {
        if self.input_only {
            return;
        }
        let pending = self
            .replacements
            .iter()
            .filter(|(_, replacement)| replacement.block_id.is_none())
            .filter_map(|(key, replacement)| {
                Some((
                    key.clone(),
                    replacement_anchor_range(&replacement.item_key, &self.segments)?,
                    replacement.rows,
                    replacement.view.clone(),
                ))
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return;
        }

        let replaced_item_keys = pending
            .iter()
            .filter_map(|(key, _, _, _)| {
                self.replacements
                    .get(key)
                    .map(|replacement| replacement.item_key.clone())
            })
            .collect::<BTreeSet<_>>();
        let header_blocks_to_remove = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, segment)| replaced_item_keys.contains(&segment.item_key))
            .filter_map(|(position, _)| self.header_blocks.remove(&position))
            .collect::<Vec<_>>();
        let diff_file_blocks_to_remove = std::mem::take(&mut self.diff_file_blocks);

        let block_ids = self.editor.update(cx, |editor, cx| {
            if !header_blocks_to_remove.is_empty() {
                editor.remove_blocks(header_blocks_to_remove.into_iter().collect(), None, cx);
            }
            if !diff_file_blocks_to_remove.is_empty() {
                editor.remove_blocks(diff_file_blocks_to_remove.into_iter().collect(), None, cx);
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let blocks = pending.iter().map(|(_, range, rows, view)| {
                let (start_offset, end_offset) =
                    replacement_anchor_offsets(range.clone()).unwrap_or((range.start, range.start));
                let start = snapshot.clip_offset(MultiBufferOffset(start_offset), Bias::Left);
                let end = snapshot.clip_offset(MultiBufferOffset(end_offset), Bias::Left);
                let start = snapshot.anchor_before(start);
                let end = snapshot.anchor_before(end);
                replacement_block(start..=end, *rows, view.clone())
            });
            editor.insert_blocks(blocks, None, cx)
        });
        debug_assert_eq!(pending.len(), block_ids.len());
        for ((key, _, _, _), block_id) in pending.into_iter().zip(block_ids) {
            if let Some(replacement) = self.replacements.get_mut(&key) {
                replacement.block_id = Some(block_id);
            }
        }
        self.viewport_decorations = None;
    }

    /// Explicitly opt into streaming tail-follow for an initial/full thread
    /// open. This scrolls by an Editor display-map anchor and leaves Vim's
    /// cursor, visual selection, and registers untouched.
    pub fn reveal_tail(&mut self, cx: &mut Context<Self>) {
        self.follow_tail = true;
        self.pending_tail_intent = None;
        self.last_selection_head = Some(self.editor.update(cx, |editor, cx| {
            let current = editor.selections.newest_anchor().head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let tail = snapshot.anchor_after(snapshot.len());
            editor.request_autoscroll(Autoscroll::bottom().for_anchor(tail), cx);
            current
        }));
    }

    /// Whether streaming/appended content is currently pinned to the bottom.
    /// Hosts use this when switching between two projections of the same
    /// transcript so the hidden projection cannot dictate scroll behavior.
    pub fn is_following_tail(&self) -> bool {
        self.follow_tail
    }

    /// Preserve the current viewport while disabling streaming tail-follow.
    pub fn pause_tail_follow(&mut self) {
        self.follow_tail = false;
        self.pending_tail_intent = None;
    }

    /// Toggle one structured item's body using the Editor's native fold map.
    ///
    /// The selectable bytes remain in the Buffer. Native Vim unfold motions,
    /// search navigation, and mouse interaction with the fold placeholder can
    /// therefore reveal the exact same text without a second scroll surface.
    fn toggle_item_collapsed(
        &mut self,
        item_key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let collapsed = !self.collapsed_items.contains(item_key);
        self.set_item_collapsed(item_key, collapsed, window, cx)
    }

    /// Make the native navigation document agree with a Rich card's disclosure
    /// state. This is idempotent so the Rich renderer can call it whenever the
    /// user toggles a card without maintaining a second fold state machine.
    pub fn set_item_collapsed(
        &mut self,
        item_key: &str,
        collapsed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(segment) = self
            .segments
            .iter()
            .find(|segment| segment.item_key == item_key && transcript_item_is_foldable(segment))
        else {
            return false;
        };
        let body_range = segment.body_range.clone();
        let line_count = self
            .buffer
            .read(cx)
            .text_for_range(body_range.clone())
            .collect::<String>()
            .lines()
            .count()
            .max(1);
        let body_end_point = offset_to_point(&self.buffer.read(cx).text(), body_range.end);
        let item_key = item_key.to_owned();
        let changed = self.editor.update(cx, |editor, cx| {
            let currently_folded = editor
                .display_snapshot(cx)
                .folds_in_range(
                    MultiBufferOffset(body_range.start)..MultiBufferOffset(body_range.end),
                )
                .next()
                .is_some();
            if currently_folded == collapsed {
                return false;
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            if collapsed {
                let selection = editor
                    .selections
                    .newest_anchor()
                    .map(|anchor| anchor.to_offset(&snapshot).0);
                if let Some(relocation_offset) = collapse_relocation_offset(
                    selection.start,
                    selection.end,
                    selection.head(),
                    &body_range,
                ) {
                    debug_assert_eq!(relocation_offset, body_range.end);
                    editor.change_selections(
                        SelectionEffects::default(),
                        window,
                        cx,
                        |selections| selections.select_ranges([body_end_point..body_end_point]),
                    );
                }
            }
            let anchors = clipped_anchor_range(&snapshot, body_range);
            if !collapsed {
                editor.unfold_ranges(&[anchors], true, false, cx);
            } else {
                let mut placeholder: FoldPlaceholder = editor.default_fold_placeholder(cx);
                placeholder.constrain_width = false;
                placeholder.merge_adjacent = false;
                placeholder.collapsed_text = Some(
                    format!(
                        "  … {line_count} {} hidden  ",
                        if line_count == 1 { "line" } else { "lines" }
                    )
                    .into(),
                );
                editor.fold_creases(
                    vec![Crease::simple(anchors, placeholder)],
                    false,
                    window,
                    cx,
                );
            }
            true
        });
        if collapsed {
            self.collapsed_items.insert(item_key);
        } else {
            self.collapsed_items.remove(&item_key);
        }
        if changed {
            self.pause_tail_follow();
            cx.notify();
        }
        true
    }

    /// Align a Buffer row to the top of the viewport without changing the Vim
    /// cursor or selection. The request is anchor-based so soft wrapping and
    /// supplemental display-map blocks are accounted for during layout.
    pub fn reveal_row_at_top(&mut self, row: u32, cx: &mut Context<Self>) {
        self.pause_tail_follow();
        let text = self.buffer.read(cx).text();
        let offset = point_to_offset(&text, Point::new(row, 0));
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchor = snapshot.anchor_before(MultiBufferOffset(offset));
            editor.request_autoscroll(Autoscroll::top().for_anchor(anchor), cx);
        });
    }

    /// Refresh viewport-scoped decorations once the host has made this Editor
    /// visible and GPUI has laid it out. Hidden transcript projections can
    /// retain a valid cache for their previous viewport, so restoring their
    /// scroll position alone is not sufficient to mount the newly visible
    /// native headers.
    pub fn refresh_after_becoming_visible(&mut self, cx: &mut Context<Self>) {
        let stale_header_blocks = std::mem::take(&mut self.header_blocks);
        self.viewport_decorations = None;
        if !stale_header_blocks.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(stale_header_blocks.into_values().collect(), None, cx);
            });
        }
        // Schedule from the first Render in which the Editor is actually a
        // visible child. Scheduling before that point observes its stale hidden
        // layout and can leave ornamental Buffer headers exposed until the user
        // scrolls. `Render` consumes this flag, so this requests exactly one
        // frame rather than creating perpetual frame demand.
        self.refresh_when_rendered = true;
        cx.notify();
    }

    fn request_tail_autoscroll(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let tail = snapshot.anchor_after(snapshot.len());
            editor.request_autoscroll(Autoscroll::bottom().for_anchor(tail), cx);
        });
    }

    fn note_local_selection_for_follow_tail(&mut self) {
        let was_following = match self.pending_tail_intent {
            Some(PendingTailIntent::Selection { was_following }) => was_following,
            _ => self.follow_tail,
        };
        // Pause synchronously so a streaming update arriving before the next
        // layout cannot pull a reader back to the bottom.
        self.follow_tail = false;
        self.pending_tail_intent = Some(PendingTailIntent::Selection { was_following });
    }

    fn apply_pending_tail_intent(&mut self, cx: &mut Context<Self>) {
        let Some(intent) = self.pending_tail_intent.take() else {
            return;
        };
        match intent {
            PendingTailIntent::DirectScroll => {
                self.follow_tail = self.editor.update(cx, |editor, cx| {
                    let Some(visible_rows) = editor.visible_line_count() else {
                        return false;
                    };
                    let display_snapshot = editor.display_snapshot(cx);
                    viewport_bottom_is_near_tail(
                        editor.scroll_position(cx).y,
                        visible_rows,
                        display_snapshot.max_point().row().as_f64(),
                    )
                });
            }
            PendingTailIntent::Selection { was_following } => {
                let previous = self.last_selection_head.take();
                let (current, previous_offset, current_offset, document_len) =
                    self.editor.update(cx, |editor, cx| {
                        let current = editor.selections.newest_anchor().head();
                        let snapshot = editor.buffer().read(cx).snapshot(cx);
                        let previous_offset = previous
                            .as_ref()
                            .map(|anchor| anchor.to_offset(&snapshot).0);
                        let current_offset = current.to_offset(&snapshot).0;
                        (current, previous_offset, current_offset, snapshot.len().0)
                    });
                self.last_selection_head = Some(current);
                self.follow_tail = follow_tail_after_selection(
                    was_following,
                    previous_offset,
                    current_offset,
                    document_len,
                );
            }
        }
    }

    fn schedule_viewport_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.viewport_refresh_pending {
            return;
        }
        self.viewport_refresh_pending = true;
        cx.on_next_frame(window, |transcript, _, cx| {
            transcript.viewport_refresh_pending = false;
            transcript.apply_pending_tail_intent(cx);
            transcript.refresh_viewport_decorations(cx);
        });
    }

    fn current_viewport_decoration_window(
        &mut self,
        cx: &mut Context<Self>,
    ) -> ViewportDecorationWindow {
        let cached_anchor_range = self
            .viewport_decorations
            .as_ref()
            .map(|window| window.anchor_range.clone());
        let (byte_range, anchor_range) = self.editor.update(cx, |editor, cx| {
            let display_snapshot = editor.display_snapshot(cx);
            let visible_range = editor.multi_buffer_visible_range(&display_snapshot, cx);
            let buffer_snapshot = display_snapshot.buffer_snapshot();
            let visible_byte_range = visible_range.start.to_offset(buffer_snapshot).0
                ..visible_range.end.to_offset(buffer_snapshot).0;
            let cached_byte_range = cached_anchor_range.as_ref().map(|range| {
                range.start.to_offset(buffer_snapshot).0..range.end.to_offset(buffer_snapshot).0
            });

            if viewport_window_choice(&visible_byte_range, cached_byte_range.as_ref())
                == ViewportWindowChoice::ReuseCached
            {
                return (
                    cached_byte_range.expect("a reused viewport window has cached offsets"),
                    cached_anchor_range
                        .clone()
                        .expect("a reused viewport window has cached anchors"),
                );
            }

            let point_range = overscanned_point_range(
                visible_range,
                buffer_snapshot.max_point(),
                VIEWPORT_OVERSCAN_ROWS,
            );
            let byte_range = point_range.start.to_offset(buffer_snapshot).0
                ..point_range.end.to_offset(buffer_snapshot).0;
            let anchor_range = clipped_anchor_range(buffer_snapshot, byte_range.clone());
            (byte_range, anchor_range)
        });
        let header_segment_range = header_segments_intersecting(&self.segments, &byte_range);
        ViewportDecorationWindow {
            byte_range,
            header_segment_range,
            anchor_range,
        }
    }

    fn refresh_viewport_decorations(&mut self, cx: &mut Context<Self>) {
        if self.input_only {
            return;
        }
        let desired = self.current_viewport_decoration_window(cx);
        let rebuild_headers = self
            .viewport_decorations
            .as_ref()
            .is_none_or(|current| current.header_segment_range != desired.header_segment_range);
        let rebuild_rows = self
            .viewport_decorations
            .as_ref()
            .is_none_or(|current| current.byte_range != desired.byte_range);
        let rebuild_diff_highlights = rebuild_rows || self.diff_highlights_dirty;
        let rebuild_semantic_highlights = rebuild_rows || self.semantic_highlights_dirty;
        let rebuild_search_highlights = rebuild_rows || self.search.highlights_dirty;
        if !rebuild_headers
            && !rebuild_rows
            && !rebuild_diff_highlights
            && !rebuild_semantic_highlights
            && !rebuild_search_highlights
        {
            return;
        }

        let (header_positions_to_remove, header_positions_to_insert) = if rebuild_headers {
            header_window_delta(
                self.header_blocks.keys().copied(),
                desired.header_segment_range.clone(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let replacement_item_keys = self
            .replacements
            .values()
            .filter(|replacement| replacement.block_id.is_some())
            .map(|replacement| replacement.item_key.as_str())
            .collect::<BTreeSet<_>>();
        let header_positions_to_insert = header_positions_to_insert
            .into_iter()
            .filter(|position| {
                !replacement_item_keys.contains(self.segments[*position].item_key.as_str())
            })
            .collect::<Vec<_>>();
        let header_segments = header_positions_to_insert
            .iter()
            .map(|position| self.segments[*position].clone())
            .collect::<Vec<_>>();
        let header_texts = header_positions_to_insert
            .iter()
            .map(|position| self.segment_header_texts[*position].clone())
            .collect::<Vec<_>>();
        let row_segment_range = segments_intersecting(&self.segments, &desired.byte_range);
        let row_segments = rebuild_rows
            .then(|| self.segments[row_segment_range].to_vec())
            .unwrap_or_default();
        let row_byte_range = desired.byte_range.clone();
        let diff_file_blocks_to_remove = rebuild_diff_highlights
            .then(|| std::mem::take(&mut self.diff_file_blocks))
            .unwrap_or_default();
        let diff_file_headers = rebuild_diff_highlights
            .then(|| {
                let buffer = self.buffer.read(cx);
                let visible_segments = segments_intersecting(&self.segments, &desired.byte_range);
                self.segments[visible_segments]
                    .iter()
                    .filter(|segment| segment.kind == TranscriptKind::Diff)
                    .filter(|segment| !replacement_item_keys.contains(segment.item_key.as_str()))
                    .flat_map(|segment| {
                        let (text, complete) = bounded_buffer_text(
                            buffer,
                            segment.body_range.clone(),
                            MAX_NATIVE_DIFF_HEADER_SCAN_BYTES,
                        );
                        native_diff_file_headers(
                            &text,
                            segment.body_range.start,
                            &segment.item_key,
                            complete,
                        )
                        .into_iter()
                        .filter(|header| {
                            intersect_ranges(&header.line_range, &desired.byte_range).is_some()
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let padding_specs = rebuild_rows
            .then(|| {
                let buffer = self.buffer.read(cx);
                let mut specs = Vec::new();
                for segment in &row_segments {
                    if !transcript_kind_is_card(segment.kind) {
                        continue;
                    }
                    let Some(range) = intersect_ranges(&segment.body_range, &row_byte_range) else {
                        continue;
                    };
                    if range.is_empty() {
                        continue;
                    }
                    if segment.kind == TranscriptKind::Diff {
                        let (text, _) = bounded_buffer_text(
                            buffer,
                            segment.body_range.clone(),
                            MAX_NATIVE_DIFF_HEADER_SCAN_BYTES,
                        );
                        specs.extend(
                            diff_gutter_inlays(&text, segment.body_range.start)
                                .into_iter()
                                .filter(|(offset, _)| range.contains(offset))
                                .map(|(offset, text)| (offset, text, true)),
                        );
                        continue;
                    }
                    let text = buffer.text_for_range(range.clone()).collect::<String>();
                    // The body start and every real hard-line start receive
                    // display-only padding. Soft wraps inherit the same indent
                    // from GPUI's line wrapper, while yanks remain byte-exact.
                    specs.push((range.start, "   ".to_owned(), false));
                    specs.extend(
                        text.match_indices('\n')
                            .map(|(offset, _)| range.start + offset + 1)
                            .filter(|offset| *offset < range.end)
                            .map(|offset| (offset, "   ".to_owned(), false)),
                    );
                }
                specs
            })
            .unwrap_or_default();
        let padding_inlays_to_remove = rebuild_rows
            .then(|| std::mem::take(&mut self.padding_inlays))
            .unwrap_or_default();
        let padding_inlays_to_insert = padding_specs
            .into_iter()
            .map(|(offset, text, diff_gutter)| {
                let id = self.next_padding_inlay_id;
                self.next_padding_inlay_id = self.next_padding_inlay_id.wrapping_add(1);
                self.padding_inlays.push(InlayId::Custom(id));
                (id, offset, text, diff_gutter)
            })
            .collect::<Vec<_>>();
        let body_highlights = rebuild_rows.then(|| {
            let mut reasoning = Vec::new();
            let mut plans = Vec::new();
            for segment in &row_segments {
                let Some(range) = intersect_ranges(&segment.body_range, &row_byte_range) else {
                    continue;
                };
                match segment.kind {
                    TranscriptKind::Reasoning => reasoning.push(range),
                    TranscriptKind::Plan => plans.push(range),
                    _ => {}
                }
            }
            (reasoning, plans)
        });
        let diff_highlights = rebuild_diff_highlights.then(|| {
            let body_ranges = visible_diff_body_ranges(&self.segments, &desired.byte_range);
            let buffer = self.buffer.read(cx);
            let mut highlights = DiffHighlightRanges::default();
            for body_range in body_ranges {
                let body = buffer
                    .text_for_range(body_range.clone())
                    .collect::<String>();
                highlights.parse_body(&body, body_range.start);
            }
            highlights
        });
        let semantic_highlights = rebuild_semantic_highlights
            .then(|| visible_semantic_highlight_ranges(&self.segments, &desired.byte_range));
        let shell_semantic_highlights = semantic_highlights.as_ref().map(|semantic| {
            let buffer = self.buffer.read(cx);
            let text = buffer
                .text_for_range(desired.byte_range.clone())
                .collect::<String>();
            visible_shell_semantic_highlights(
                &text,
                desired.byte_range.start,
                &semantic.command_invocations,
            )
        });
        let search_case_sensitive = self.search.case_sensitive;
        let search_whole_word = self.search.whole_word;
        let search_highlights = rebuild_search_highlights.then(|| {
            if self.search.query.is_empty() {
                None
            } else {
                let buffer = self.buffer.read(cx);
                let document_len = buffer.len();
                let text = buffer
                    .text_for_range(desired.byte_range.clone())
                    .collect::<String>();
                Some(literal_match_ranges_with_options(
                    &text,
                    &self.search.query,
                    desired.byte_range.start,
                    document_len,
                    search_case_sensitive,
                    search_whole_word,
                ))
            }
        });
        let active_search_match = self.search.active_match.clone();
        let header_blocks_to_remove = header_positions_to_remove
            .iter()
            .filter_map(|position| self.header_blocks.remove(position))
            .collect::<Vec<_>>();

        let transcript = cx.weak_entity();
        let (inserted_header_blocks, inserted_diff_file_blocks) =
            self.editor.update(cx, |editor, cx| {
                if !header_blocks_to_remove.is_empty() {
                    editor.remove_blocks(header_blocks_to_remove.into_iter().collect(), None, cx);
                }
                if !diff_file_blocks_to_remove.is_empty() {
                    editor.remove_blocks(
                        diff_file_blocks_to_remove.into_iter().collect(),
                        None,
                        cx,
                    );
                }

                let snapshot = editor.buffer().read(cx).snapshot(cx);
                if rebuild_rows {
                    let diff_gutter_highlights = padding_inlays_to_insert
                        .iter()
                        .filter(|(_, _, _, diff_gutter)| *diff_gutter)
                        .map(|(id, offset, text, _)| InlayHighlight {
                            inlay: InlayId::Custom(*id),
                            inlay_position: clipped_anchor_after(&snapshot, *offset),
                            range: 0..text.len(),
                        })
                        .collect();
                    let inlays = padding_inlays_to_insert
                        .into_iter()
                        .map(|(id, offset, text, _)| {
                            Inlay::custom(id, clipped_anchor_after(&snapshot, offset), text)
                        })
                        .collect();
                    editor.splice_inlays(&padding_inlays_to_remove, inlays, cx);
                    editor.highlight_inlays(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            DiffGutterInlayHighlight,
                        >()),
                        diff_gutter_highlights,
                        HighlightStyle {
                            color: Some(cx.theme().colors().text_muted),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                }
                let native_headers: Vec<_> = header_segments
                    .iter()
                    .zip(header_texts)
                    .map(|(segment, header_text)| {
                        let (start, end) =
                            clipped_anchor_pair(&snapshot, segment.header_range.clone());
                        native_header_block(
                            start..=end,
                            segment.item_key.clone(),
                            segment.kind,
                            native_header_text(&header_text).into(),
                            transcript_item_is_foldable(segment),
                            transcript.clone(),
                        )
                    })
                    .collect();
                let inserted_header_blocks = if !header_segments.is_empty() {
                    editor.insert_blocks(native_headers, None, cx)
                } else {
                    Vec::new()
                };
                let native_diff_headers = diff_file_headers.into_iter().filter_map(|header| {
                    let (start_offset, end_offset) =
                        replacement_anchor_offsets(header.line_range.clone())?;
                    let start = snapshot.clip_offset(MultiBufferOffset(start_offset), Bias::Left);
                    let end = snapshot.clip_offset(MultiBufferOffset(end_offset), Bias::Left);
                    Some(native_diff_file_header_block(
                        snapshot.anchor_before(start)..=snapshot.anchor_before(end),
                        header,
                    ))
                });
                let inserted_diff_file_blocks = editor.insert_blocks(native_diff_headers, None, cx);

                if rebuild_rows {
                    editor.clear_row_highlights::<UserTranscriptRows>();
                    editor.clear_row_highlights::<ReasoningTranscriptRows>();
                    editor.clear_row_highlights::<StructuredTranscriptRows>();
                    editor.clear_row_highlights::<ErrorTranscriptRows>();
                    editor.clear_row_overlays::<DiffAdditionRows>();
                    editor.clear_row_overlays::<DiffDeletionRows>();
                    for segment in row_segments {
                        let Some(range) = intersect_ranges(&segment.whole_range, &row_byte_range)
                        else {
                            continue;
                        };
                        let anchors = clipped_anchor_range(&snapshot, range);
                        let options = transcript_row_options(segment.kind);
                        match segment.kind {
                            TranscriptKind::User => editor.highlight_rows::<UserTranscriptRows>(
                                anchors,
                                user_transcript_background,
                                options,
                                cx,
                            ),
                            TranscriptKind::Reasoning | TranscriptKind::Plan => {
                                editor.highlight_rows::<ReasoningTranscriptRows>(
                                    anchors,
                                    reasoning_transcript_background,
                                    options,
                                    cx,
                                )
                            }
                            TranscriptKind::Error => editor.highlight_rows::<ErrorTranscriptRows>(
                                anchors,
                                error_transcript_background,
                                options,
                                cx,
                            ),
                            TranscriptKind::Agent | TranscriptKind::Trace => {}
                            _ => editor.highlight_rows::<StructuredTranscriptRows>(
                                anchors,
                                structured_transcript_background,
                                options,
                                cx,
                            ),
                        }
                    }

                    if let Some((reasoning, plans)) = body_highlights {
                        let anchors = |ranges: Vec<Range<usize>>| {
                            ranges
                                .into_iter()
                                .map(|range| clipped_anchor_range(&snapshot, range))
                                .collect::<Vec<_>>()
                        };
                        editor.highlight_text(
                            HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                                ReasoningBodyHighlight,
                            >()),
                            anchors(reasoning),
                            HighlightStyle {
                                color: Some(cx.theme().colors().text_muted),
                                font_style: Some(gpui::FontStyle::Italic),
                                ..HighlightStyle::default()
                            },
                            cx,
                        );
                        editor.highlight_text(
                            HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                                PlanBodyHighlight,
                            >()),
                            anchors(plans),
                            HighlightStyle {
                                color: Some(cx.theme().colors().text_accent),
                                font_weight: Some(FontWeight::BOLD),
                                ..HighlightStyle::default()
                            },
                            cx,
                        );
                    }
                }

                if let Some(diff_highlights) = diff_highlights {
                    let DiffHighlightRanges {
                        file_headers,
                        hunks,
                        additions,
                        deletions,
                        parsed_bytes: _,
                    } = diff_highlights;
                    let anchors = |ranges: Vec<Range<usize>>| {
                        ranges
                            .into_iter()
                            .map(|range| clipped_anchor_range(&snapshot, range))
                            .collect::<Vec<_>>()
                    };
                    let overlay_options = RowOverlayOptions {
                        horizontal_inset: px(1.),
                        ..RowOverlayOptions::default()
                    };
                    editor.clear_row_overlays::<DiffAdditionRows>();
                    editor.clear_row_overlays::<DiffDeletionRows>();
                    for range in &additions {
                        editor.highlight_row_overlay::<DiffAdditionRows>(
                            clipped_anchor_range(&snapshot, range.clone()),
                            diff_addition_background,
                            overlay_options,
                            cx,
                        );
                    }
                    for range in &deletions {
                        editor.highlight_row_overlay::<DiffDeletionRows>(
                            clipped_anchor_range(&snapshot, range.clone()),
                            diff_deletion_background,
                            overlay_options,
                            cx,
                        );
                    }

                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            DiffFileHeaderHighlight,
                        >()),
                        anchors(file_headers),
                        HighlightStyle {
                            color: Some(cx.theme().colors().text_muted),
                            font_weight: Some(FontWeight::BOLD),
                            background_color: Some(
                                cx.theme()
                                    .colors()
                                    .editor_subheader_background
                                    .opacity(0.36),
                            ),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            DiffHunkHighlight,
                        >()),
                        anchors(hunks),
                        HighlightStyle {
                            color: Some(cx.theme().status().modified),
                            font_weight: Some(FontWeight::BOLD),
                            background_color: Some(
                                cx.theme().status().modified_background.opacity(0.16),
                            ),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            DiffAdditionHighlight,
                        >()),
                        anchors(additions),
                        HighlightStyle {
                            color: Some(cx.theme().status().created),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            DiffDeletionHighlight,
                        >()),
                        anchors(deletions),
                        HighlightStyle {
                            color: Some(cx.theme().status().deleted),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                }

                if let Some(semantic_highlights) = semantic_highlights {
                    let SemanticHighlightRanges {
                        headings,
                        strong,
                        emphasis,
                        inline_code,
                        links,
                        code_blocks,
                        block_quotes,
                        strikethrough,
                        command_invocations: _,
                        command_outputs: _,
                        scanned_spans: _,
                    } = semantic_highlights;
                    let anchors = |ranges: Vec<Range<usize>>| {
                        ranges
                            .into_iter()
                            .map(|range| clipped_anchor_range(&snapshot, range))
                            .collect::<Vec<_>>()
                    };
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownCodeBlockHighlight,
                        >()),
                        anchors(code_blocks),
                        HighlightStyle {
                            background_color: Some(
                                cx.theme()
                                    .colors()
                                    .editor_subheader_background
                                    .opacity(0.72),
                            ),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownBlockQuoteHighlight,
                        >()),
                        anchors(block_quotes),
                        HighlightStyle {
                            color: Some(cx.theme().colors().text_muted),
                            font_style: Some(gpui::FontStyle::Italic),
                            background_color: Some(
                                cx.theme()
                                    .colors()
                                    .editor_subheader_background
                                    .opacity(0.28),
                            ),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownHeadingHighlight,
                        >()),
                        anchors(headings),
                        HighlightStyle {
                            color: Some(cx.theme().colors().text_accent),
                            font_weight: Some(FontWeight::BOLD),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownStrongHighlight,
                        >()),
                        anchors(strong),
                        HighlightStyle {
                            font_weight: Some(FontWeight::BOLD),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownEmphasisHighlight,
                        >()),
                        anchors(emphasis),
                        HighlightStyle {
                            font_style: Some(gpui::FontStyle::Italic),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownInlineCodeHighlight,
                        >()),
                        anchors(inline_code),
                        HighlightStyle {
                            color: Some(cx.theme().colors().text_accent),
                            background_color: Some(
                                cx.theme()
                                    .colors()
                                    .editor_subheader_background
                                    .opacity(0.72),
                            ),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    let link_color = cx.theme().colors().text_accent;
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownLinkHighlight,
                        >()),
                        anchors(links),
                        HighlightStyle {
                            color: Some(link_color),
                            underline: Some(gpui::UnderlineStyle {
                                thickness: px(1.),
                                color: Some(link_color),
                                wavy: false,
                            }),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                    let strikethrough_color = cx.theme().colors().text_muted;
                    editor.highlight_text(
                        HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                            MarkdownStrikethroughHighlight,
                        >()),
                        anchors(strikethrough),
                        HighlightStyle {
                            color: Some(strikethrough_color),
                            strikethrough: Some(gpui::StrikethroughStyle {
                                thickness: px(1.),
                                color: Some(strikethrough_color),
                            }),
                            ..HighlightStyle::default()
                        },
                        cx,
                    );
                }

                if let Some(shell_highlights) = shell_semantic_highlights {
                    let anchors = |ranges: Vec<Range<usize>>| {
                        ranges
                            .into_iter()
                            .map(|range| clipped_anchor_range(&snapshot, range))
                            .collect::<Vec<_>>()
                    };
                    let fallback = HighlightStyle {
                        color: Some(cx.theme().colors().text_accent),
                        ..HighlightStyle::default()
                    };
                    let syntax = cx.theme().syntax();
                    let function_style = syntax.style_for_name("function").unwrap_or(fallback);
                    let variable_style = syntax.style_for_name("variable").unwrap_or(fallback);
                    let keyword_style = syntax.style_for_name("keyword").unwrap_or(fallback);
                    let operator_style = syntax.style_for_name("operator").unwrap_or(fallback);
                    let constant_style = syntax.style_for_name("constant").unwrap_or(fallback);
                    let string_style = syntax.style_for_name("string").unwrap_or(fallback);
                    let comment_style = syntax.style_for_name("comment").unwrap_or(fallback);
                    let embedded_style = syntax.style_for_name("embedded").unwrap_or(fallback);
                    let punctuation_style =
                        syntax.style_for_name("punctuation").unwrap_or(fallback);
                    macro_rules! paint_shell {
                        ($marker:ty, $ranges:expr, $style:expr) => {
                            editor.highlight_text(
                                HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                                    $marker,
                                >()),
                                anchors($ranges),
                                $style,
                                cx,
                            );
                        };
                    }
                    paint_shell!(
                        ShellFunctionHighlight,
                        shell_highlights.functions,
                        function_style
                    );
                    paint_shell!(
                        ShellVariableHighlight,
                        shell_highlights.variables,
                        variable_style
                    );
                    paint_shell!(
                        ShellKeywordHighlight,
                        shell_highlights.keywords,
                        keyword_style
                    );
                    paint_shell!(
                        ShellOperatorHighlight,
                        shell_highlights.operators,
                        operator_style
                    );
                    paint_shell!(
                        ShellConstantHighlight,
                        shell_highlights.constants,
                        constant_style
                    );
                    paint_shell!(ShellStringHighlight, shell_highlights.strings, string_style);
                    paint_shell!(
                        ShellCommentHighlight,
                        shell_highlights.comments,
                        comment_style
                    );
                    paint_shell!(
                        ShellEmbeddedHighlight,
                        shell_highlights.embedded,
                        embedded_style
                    );
                    paint_shell!(
                        ShellPunctuationHighlight,
                        shell_highlights.punctuation,
                        punctuation_style
                    );
                }

                if let Some(search_highlights) = search_highlights {
                    if let Some(search_highlights) = search_highlights {
                        let active_search_match = active_search_match.map(|range| {
                            range.start.to_offset(&snapshot).0..range.end.to_offset(&snapshot).0
                        });
                        let active_match_index = active_search_match.as_ref().and_then(|active| {
                            search_highlights
                                .iter()
                                .position(|candidate| candidate == active)
                        });
                        let anchors = search_highlights
                            .into_iter()
                            .map(|range| clipped_anchor_range(&snapshot, range))
                            .collect::<Vec<_>>();
                        editor.highlight_background(
                            HighlightKey::BufferSearchHighlights,
                            &anchors,
                            move |index, theme| {
                                if active_match_index == Some(*index) {
                                    theme.colors().search_active_match_background
                                } else {
                                    theme.colors().search_match_background
                                }
                            },
                            cx,
                        );
                    } else {
                        editor
                            .clear_background_highlights(HighlightKey::BufferSearchHighlights, cx);
                    }
                }
                (inserted_header_blocks, inserted_diff_file_blocks)
            });
        debug_assert_eq!(
            header_positions_to_insert.len(),
            inserted_header_blocks.len()
        );
        for (position, block_id) in header_positions_to_insert
            .into_iter()
            .zip(inserted_header_blocks)
        {
            self.header_blocks.insert(position, block_id);
        }
        self.diff_file_blocks = inserted_diff_file_blocks;
        self.viewport_decorations = Some(desired);
        self.diff_highlights_dirty = false;
        self.semantic_highlights_dirty = false;
        self.search.highlights_dirty = false;
    }

    /// Apply semantic hierarchy without replacing selectable transcript bodies.
    ///
    /// Message bodies remain real buffer ranges, so Zed's cursor, visual
    /// selections, registers, and yank continue to operate across items. The
    /// compact native headers only replace the ornamental header rows in the
    /// display map; their source text remains in the Buffer and in yanked ranges.
    /// Zed treats a cursor that enters a replacement block as selecting that
    /// whole underlying row, so headers intentionally do not have character-level
    /// visual selection; every body on either side still does.
    fn refresh_semantic_font_geometry(&mut self, cx: &mut Context<Self>) {
        let ranges = semantic_monospace_ranges(&self.segments);
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchors = ranges
                .into_iter()
                .map(|range| clipped_anchor_range(&snapshot, range))
                .collect();
            editor.highlight_text(
                HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                    MarkdownMonospaceGeometryHighlight,
                >()),
                anchors,
                HighlightStyle {
                    font_family: Some(FontFamilyVariant::Monospace),
                    ..HighlightStyle::default()
                },
                cx,
            );
        });
    }

    pub fn decorate(&mut self, document: &TranscriptDocument, cx: &mut Context<Self>) -> bool {
        if !document_has_valid_segment_ranges(document) {
            // Keep the raw Buffer readable/selectable, but discard every
            // decoration whose semantic ownership can no longer be proven.
            // A later valid full sync remounts the retained logical supplement
            // specs against fresh ranges.
            self.unmount_all_supplements(cx);
            self.unmount_all_replacements(cx);
            self.segments.clear();
            self.segment_header_texts.clear();
            self.segment_body_texts.clear();
            self.collapsed_items.clear();
            self.model_item_count = document.item_rows.len();
            let previous_header_blocks = std::mem::take(&mut self.header_blocks);
            let previous_diff_file_blocks = std::mem::take(&mut self.diff_file_blocks);
            self.viewport_decorations = None;
            self.diff_highlights_dirty = false;
            self.semantic_highlights_dirty = false;
            self.search.highlights_dirty = true;
            self.editor.update(cx, |editor, cx| {
                if !previous_header_blocks.is_empty() {
                    editor.remove_blocks(previous_header_blocks.into_values().collect(), None, cx);
                }
                if !previous_diff_file_blocks.is_empty() {
                    editor.remove_blocks(previous_diff_file_blocks.into_iter().collect(), None, cx);
                }
                editor.clear_row_highlights::<UserTranscriptRows>();
                editor.clear_row_highlights::<ReasoningTranscriptRows>();
                editor.clear_row_highlights::<StructuredTranscriptRows>();
                editor.clear_row_highlights::<ErrorTranscriptRows>();
                editor.clear_row_overlays::<DiffAdditionRows>();
                editor.clear_row_overlays::<DiffDeletionRows>();
                for key in [
                    NavigationOverlayKey::unique::<TranscriptHeaderHighlight>(),
                    NavigationOverlayKey::unique::<ReasoningBodyHighlight>(),
                    NavigationOverlayKey::unique::<PlanBodyHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownHeadingHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownStrongHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownEmphasisHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownInlineCodeHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownLinkHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownCodeBlockHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownMonospaceGeometryHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownBlockQuoteHighlight>(),
                    NavigationOverlayKey::unique::<MarkdownStrikethroughHighlight>(),
                    NavigationOverlayKey::unique::<ShellFunctionHighlight>(),
                    NavigationOverlayKey::unique::<ShellVariableHighlight>(),
                    NavigationOverlayKey::unique::<ShellKeywordHighlight>(),
                    NavigationOverlayKey::unique::<ShellOperatorHighlight>(),
                    NavigationOverlayKey::unique::<ShellConstantHighlight>(),
                    NavigationOverlayKey::unique::<ShellStringHighlight>(),
                    NavigationOverlayKey::unique::<ShellCommentHighlight>(),
                    NavigationOverlayKey::unique::<ShellEmbeddedHighlight>(),
                    NavigationOverlayKey::unique::<ShellPunctuationHighlight>(),
                    NavigationOverlayKey::unique::<DiffFileHeaderHighlight>(),
                    NavigationOverlayKey::unique::<DiffHunkHighlight>(),
                    NavigationOverlayKey::unique::<DiffAdditionHighlight>(),
                    NavigationOverlayKey::unique::<DiffDeletionHighlight>(),
                ] {
                    editor.clear_highlights(HighlightKey::NavigationOverlay(key), cx);
                }
                editor.clear_background_highlights(HighlightKey::BufferSearchHighlights, cx);
            });
            return false;
        }

        // A full document rebuild can replace the anchors under a supplement.
        // Keep the logical specs and host views, but remount their Editor blocks
        // against the new per-item body ranges below.
        self.unmount_all_supplements(cx);
        self.unmount_all_replacements(cx);
        self.segments.clone_from(&document.segments);
        self.collapsed_items.retain(|item_key| {
            document
                .segments
                .iter()
                .any(|segment| &segment.item_key == item_key)
        });
        self.segment_header_texts = document
            .segments
            .iter()
            .map(|segment| document.text[segment.header_range.clone()].to_owned())
            .collect();
        self.segment_body_texts = document
            .segments
            .iter()
            .map(|segment| Arc::<str>::from(&document.text[segment.body_range.clone()]))
            .collect();
        self.model_item_count = document.item_rows.len();
        let previous_header_blocks = std::mem::take(&mut self.header_blocks);
        let previous_diff_file_blocks = std::mem::take(&mut self.diff_file_blocks);
        self.viewport_decorations = None;
        self.diff_highlights_dirty = true;
        self.semantic_highlights_dirty = true;
        self.search.highlights_dirty = true;
        self.editor.update(cx, |editor, cx| {
            if !previous_header_blocks.is_empty() {
                editor.remove_blocks(previous_header_blocks.into_values().collect(), None, cx);
            }
            if !previous_diff_file_blocks.is_empty() {
                editor.remove_blocks(previous_diff_file_blocks.into_iter().collect(), None, cx);
            }
            editor.clear_row_highlights::<UserTranscriptRows>();
            editor.clear_row_highlights::<ReasoningTranscriptRows>();
            editor.clear_row_highlights::<StructuredTranscriptRows>();
            editor.clear_row_highlights::<ErrorTranscriptRows>();
            editor.clear_row_overlays::<DiffAdditionRows>();
            editor.clear_row_overlays::<DiffDeletionRows>();

            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let headers = document
                .segments
                .iter()
                .map(|segment| clipped_anchor_range(&snapshot, segment.header_range.clone()))
                .collect();
            let header_key = HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                TranscriptHeaderHighlight,
            >());
            editor.highlight_text(
                header_key,
                headers,
                HighlightStyle {
                    color: Some(cx.theme().colors().text_muted),
                    font_weight: Some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                },
                cx,
            );
        });
        self.refresh_semantic_font_geometry(cx);
        if !self.input_only {
            self.mount_unmounted_supplements(cx);
            self.mount_unmounted_replacements(cx);
            self.refresh_viewport_decorations(cx);
        }
        true
    }

    /// Apply per-item document projections without rebuilding the full buffer.
    ///
    /// Existing updates must preserve item identity, kind, and header text; only
    /// their selectable body/tail may change. New model items are represented in
    /// order by `appended`, with `None` for trace-only items. Any structural
    /// ambiguity returns `false` before mutating the buffer so the caller can
    /// perform an explicit full rebuild.
    pub fn apply_item_projections(
        &mut self,
        old_model_item_count: usize,
        existing_updates: &[(usize, Option<TranscriptItemProjection>)],
        appended: &[Option<TranscriptItemProjection>],
        cx: &mut Context<Self>,
    ) -> bool {
        if self.model_item_count != old_model_item_count
            || self.segment_header_texts.len() != self.segments.len()
            || self.segment_body_texts.len() != self.segments.len()
            || self.buffer.read(cx).len()
                != self
                    .segments
                    .last()
                    .map_or(0, |segment| segment.whole_range.end)
        {
            return false;
        }

        let mut updates = Vec::with_capacity(existing_updates.len());
        for (item_index, projection) in existing_updates {
            if *item_index >= old_model_item_count {
                return false;
            }
            let segment_position = self
                .segments
                .binary_search_by_key(item_index, |segment| segment.item_index)
                .ok();
            match (segment_position, projection) {
                (None, None) => {}
                (Some(_), None) | (None, Some(_)) => return false,
                (Some(segment_position), Some(projection)) => {
                    if !projection_has_valid_relative_ranges(projection)
                        || projection.segment.item_index != *item_index
                    {
                        return false;
                    }
                    let current = &self.segments[segment_position];
                    if current.item_key != projection.segment.item_key
                        || current.kind != projection.segment.kind
                        || self.segment_header_texts[segment_position] != projection.header_text()
                    {
                        return false;
                    }
                    updates.push((segment_position, projection));
                }
            }
        }
        updates.sort_unstable_by(|(left, _), (right, _)| right.cmp(left));
        if updates.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return false;
        }

        let mut appended_projections = Vec::new();
        for (offset, projection) in appended.iter().enumerate() {
            let item_index = old_model_item_count + offset;
            let Some(projection) = projection else {
                continue;
            };
            if !projection_has_valid_relative_ranges(projection)
                || projection.segment.item_index != item_index
                || self
                    .segments
                    .iter()
                    .any(|segment| segment.item_key == projection.segment.item_key)
                || appended_projections
                    .iter()
                    .any(|previous: &&TranscriptItemProjection| {
                        previous.segment.item_key == projection.segment.item_key
                    })
            {
                return false;
            }
            appended_projections.push(projection);
        }
        let diff_bodies_changed = updates.iter().any(|(segment_position, _)| {
            self.segments[*segment_position].kind == TranscriptKind::Diff
        }) || appended_projections
            .iter()
            .any(|projection| projection.segment.kind == TranscriptKind::Diff);
        let semantic_bodies_changed = !updates.is_empty()
            || appended_projections
                .iter()
                .any(|projection| !projection.segment.semantic_spans.is_empty());

        struct PendingEdit {
            old_range: Range<usize>,
            replacement: String,
        }

        let mut pending_edits = Vec::with_capacity(updates.len());
        {
            let buffer = self.buffer.read(cx);
            for (segment_position, projection) in &updates {
                let current = &self.segments[*segment_position];
                let old_tail = buffer
                    .text_for_range(current.body_range.start..current.whole_range.end)
                    .collect::<String>();
                let new_tail = &projection.text
                    [projection.segment.body_range.start..projection.segment.whole_range.end];
                let (local_range, replacement) = minimal_text_edit(&old_tail, new_tail);
                pending_edits.push(PendingEdit {
                    old_range: current.body_range.start + local_range.start
                        ..current.body_range.start + local_range.end,
                    replacement,
                });
            }
        }

        let mut next_segments = self.segments.clone();
        let mut next_body_texts = self.segment_body_texts.clone();
        for (segment_position, projection) in &updates {
            if !apply_projected_segment_shape(
                &mut next_segments,
                *segment_position,
                &projection.segment,
            ) {
                return false;
            }
            next_body_texts[*segment_position] = Arc::from(projection.body_text());
        }

        let mut append_text = String::new();
        let mut appended_segments = Vec::with_capacity(appended_projections.len());
        let mut appended_headers = Vec::with_capacity(appended_projections.len());
        let mut appended_bodies = Vec::with_capacity(appended_projections.len());
        let mut next_offset = next_segments
            .last()
            .map_or(0, |segment| segment.whole_range.end);
        for projection in appended_projections {
            appended_segments.push(TranscriptDocumentSegment {
                item_index: projection.segment.item_index,
                item_key: projection.segment.item_key.clone(),
                kind: projection.segment.kind,
                whole_range: range_at_offset(&projection.segment.whole_range, next_offset),
                header_range: range_at_offset(&projection.segment.header_range, next_offset),
                body_range: range_at_offset(&projection.segment.body_range, next_offset),
                semantic_spans: projection
                    .segment
                    .semantic_spans
                    .iter()
                    .map(|span| TranscriptSemanticSpan {
                        range: range_at_offset(&span.range, next_offset),
                        style: span.style,
                    })
                    .collect(),
            });
            appended_headers.push(projection.header_text().to_owned());
            appended_bodies.push(Arc::from(projection.body_text()));
            next_offset += projection.text.len();
            append_text.push_str(&projection.text);
        }

        let search_text_changed = pending_edits
            .iter()
            .any(|edit| !edit.old_range.is_empty() || !edit.replacement.is_empty())
            || !append_text.is_empty();
        self.buffer.update(cx, |buffer, cx| {
            for edit in pending_edits {
                buffer.edit([(edit.old_range, edit.replacement)], None, cx);
            }
            if !append_text.is_empty() {
                let end = buffer.len();
                buffer.edit([(end..end, append_text)], None, cx);
            }
        });

        let appended_segment_start = next_segments.len();
        next_segments.extend(appended_segments);
        next_body_texts.extend(appended_bodies);
        self.segments = next_segments;
        self.segment_header_texts.extend(appended_headers);
        self.segment_body_texts = next_body_texts;
        self.model_item_count = old_model_item_count + appended.len();
        self.diff_highlights_dirty |= diff_bodies_changed;
        self.semantic_highlights_dirty |= semantic_bodies_changed;
        self.search.highlights_dirty |= search_text_changed;
        if self.segments.len() > appended_segment_start {
            self.decorate_appended_segments(appended_segment_start, cx);
        }
        if semantic_bodies_changed {
            self.refresh_semantic_font_geometry(cx);
        }
        if !self.input_only {
            self.mount_unmounted_supplements(cx);
            self.mount_unmounted_replacements(cx);
            self.refresh_viewport_decorations(cx);
        }
        if should_request_tail_autoscroll(self.follow_tail, search_text_changed) {
            self.request_tail_autoscroll(cx);
        }
        true
    }

    fn decorate_appended_segments(
        &mut self,
        appended_segment_start: usize,
        cx: &mut Context<Self>,
    ) {
        let segments = self.segments[appended_segment_start..].to_vec();
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let mut headers = Vec::with_capacity(segments.len());
            for segment in &segments {
                headers.push(clipped_anchor_range(
                    &snapshot,
                    segment.header_range.clone(),
                ));
            }

            let header_key = HighlightKey::NavigationOverlay(NavigationOverlayKey::unique::<
                TranscriptHeaderHighlight,
            >());
            editor.highlight_text_key(
                header_key,
                headers,
                HighlightStyle {
                    color: Some(cx.theme().colors().text_muted),
                    font_weight: Some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                },
                true,
                cx,
            );
        });
    }

    pub fn set_cursor_row(&mut self, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.buffer.read(cx).text();
        let requested_offset = point_to_offset(&text, Point::new(row, 0));
        // `item_rows` points at the ornamental header. Enter the selectable
        // body instead, so the first Vim motion does not touch a replacement
        // block and expand to its hidden source row.
        let target_offset = item_body_start_at_offset(requested_offset, &self.segments);
        let point = offset_to_point(&text, target_offset);
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([point..point]);
            });
        });
    }

    /// Re-enter Vim normal mode after focus is transferred by a Harness keybinding.
    ///
    /// This is intentionally deferred by the caller: the keystroke that opens the
    /// transcript buffer (Shift-V) must finish dispatching before Zed's Vim layer
    /// sees the editor, or that same keystroke can start a visual-line selection.
    pub fn enter_normal_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(action) = cx.build_action("vim::SwitchToNormalMode", None) {
            window.dispatch_action(action, cx);
        }
    }

    /// Persist a transcript query and repaint its viewport-local matches without
    /// moving or replacing the Editor's Vim selection.
    pub fn set_search_query(&mut self, query: &str, backwards: bool, cx: &mut Context<Self>) {
        if query.is_empty() {
            self.clear_search(cx);
            return;
        }
        let query_changed =
            self.search.query != query || self.search.case_sensitive || self.search.whole_word;
        self.search.backwards = backwards;
        self.search.case_sensitive = false;
        self.search.whole_word = false;
        if query_changed {
            self.search.query.clear();
            self.search.query.push_str(query);
            self.search.active_match = None;
            self.search.highlights_dirty = true;
            self.refresh_viewport_decorations(cx);
        }
    }

    /// Clear native match decoration without touching the cursor, visual
    /// selection, registers, or selectable transcript text.
    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search = TranscriptSearchState::default();
        self.editor.update(cx, |editor, cx| {
            editor.clear_background_highlights(HighlightKey::BufferSearchHighlights, cx);
        });
    }

    pub fn search_query(&self) -> Option<&str> {
        (!self.search.query.is_empty()).then_some(self.search.query.as_str())
    }

    /// Repeat in the original `/` or `?` direction. `reverse` implements Vim's
    /// `N`; a false value implements `n`.
    pub fn repeat_search(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let backwards = repeated_search_backwards(self.search.backwards, reverse);
        self.move_search_in_direction(backwards, window, cx)
    }

    /// Compatibility entry point for the current Harness search bar. New
    /// callers should set the query once and use `repeat_search` for n/N.
    pub fn search(
        &mut self,
        query: &str,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if query.is_empty() {
            self.clear_search(cx);
            return false;
        }
        let query_changed =
            self.search.query != query || self.search.case_sensitive || self.search.whole_word;
        self.search.backwards = backwards;
        self.search.case_sensitive = false;
        self.search.whole_word = false;
        if query_changed {
            self.search.query.clear();
            self.search.query.push_str(query);
            self.search.active_match = None;
            self.search.highlights_dirty = true;
        }
        self.move_search_in_direction(backwards, window, cx)
    }

    /// Search from the keyword under the Vim cursor. Whole-word `*`/`#` and
    /// partial-word `g*`/`g#` share the same persistent state as `/`, so n/N
    /// continue in the expected original direction.
    pub fn search_word_under_cursor(
        &mut self,
        backwards: bool,
        partial_word: bool,
        case_sensitive: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = self.buffer.read(cx).text();
        let cursor = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        });
        let cursor_offset = point_to_offset(&text, cursor);
        let Some(word_range) = keyword_range_at_offset(&text, cursor_offset) else {
            return false;
        };
        let query = text[word_range.clone()].to_string();
        self.search.query = query;
        self.search.backwards = backwards;
        self.search.case_sensitive = case_sensitive;
        self.search.whole_word = !partial_word;
        self.search.active_match = None;
        self.search.highlights_dirty = true;
        self.move_search_from_offset(word_range.start, backwards, window, cx)
    }

    fn move_search_in_direction(
        &mut self,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.search.query.is_empty() {
            return false;
        }
        let text = self.buffer.read(cx).text();
        let cursor = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        });
        let cursor_offset = point_to_offset(&text, cursor);
        self.move_search_from_offset(cursor_offset, backwards, window, cx)
    }

    fn move_search_from_offset(
        &mut self,
        cursor_offset: usize,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = self.buffer.read(cx).text();
        let match_offset = find_wrapped_literal_match(
            &text,
            &self.search.query,
            cursor_offset,
            backwards,
            self.search.case_sensitive,
            self.search.whole_word,
        );
        let Some(match_offset) = match_offset else {
            self.search.active_match = None;
            self.search.highlights_dirty = true;
            self.refresh_viewport_decorations(cx);
            return false;
        };
        let match_range = match_offset..match_offset + self.search.query.len();
        let point = offset_to_point(&text, match_offset);
        self.search.active_match = Some(self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchored_match = clipped_anchor_range(&snapshot, match_range);
            editor.change_selections(
                SelectionEffects::default().from_search(true),
                window,
                cx,
                |selections| selections.select_ranges([point..point]),
            );
            anchored_match
        }));
        self.search.highlights_dirty = true;
        self.refresh_viewport_decorations(cx);
        // Autoscroll is applied during layout. Refresh once more on the next
        // frame so a far-away active match receives emphasis after reveal.
        self.schedule_viewport_refresh(window, cx);
        true
    }
}

fn previous_char_boundary(text: &str, offset: usize) -> usize {
    (0..=offset.min(text.len()))
        .rev()
        .find(|candidate| text.is_char_boundary(*candidate))
        .unwrap_or(0)
}

fn point_to_offset(text: &str, point: Point) -> usize {
    let mut offset = 0;
    for _ in 0..point.row {
        let Some(line_end) = text[offset..].find('\n') else {
            return text.len();
        };
        offset += line_end + 1;
    }
    let requested = offset.saturating_add(point.column as usize).min(text.len());
    previous_char_boundary(text, requested).max(offset)
}

fn offset_to_point(text: &str, offset: usize) -> Point {
    let offset = offset.min(text.len());
    let prefix = &text[..offset];
    let row = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let column = prefix
        .rfind('\n')
        .map_or(offset, |line_end| offset - line_end - 1) as u32;
    Point::new(row, column)
}

impl Focusable for TranscriptEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for TranscriptEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.refresh_when_rendered) {
            self.schedule_viewport_refresh(window, cx);
        }
        self.editor.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linewise_selection_does_not_claim_a_row_at_its_exclusive_boundary() {
        assert_eq!(
            linewise_selection_rows(Point::new(4, 3), Point::new(6, 0)),
            4..=5
        );
        assert_eq!(
            linewise_selection_rows(Point::new(4, 3), Point::new(6, 1)),
            4..=6
        );
        assert_eq!(
            linewise_selection_rows(Point::new(4, 3), Point::new(4, 4)),
            4..=4
        );
    }

    #[test]
    fn collapsing_a_body_relocates_only_selections_that_would_disappear() {
        let body = 10..30;

        assert_eq!(collapse_relocation_offset(10, 10, 10, &body), Some(30));
        assert_eq!(collapse_relocation_offset(18, 18, 18, &body), Some(30));
        assert_eq!(collapse_relocation_offset(5, 15, 15, &body), Some(30));
        assert_eq!(collapse_relocation_offset(25, 35, 25, &body), Some(30));

        assert_eq!(collapse_relocation_offset(5, 5, 5, &body), None);
        assert_eq!(collapse_relocation_offset(30, 30, 30, &body), None);
        assert_eq!(collapse_relocation_offset(35, 35, 35, &body), None);
    }

    #[test]
    fn transcript_and_vim_clipboard_use_project_free_editor_seams() {
        let transcript_source = include_str!("lib.rs")
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(production, _)| production)
            .expect("the source guard must inspect production code only");
        assert!(transcript_source.contains("Editor::for_local_buffer(buffer, window, cx)"));
        assert!(!transcript_source.contains("Editor::for_buffer(buffer, None, window, cx)"));

        let yank_source = include_str!("../../vim/src/normal/yank.rs");
        let yank_method = yank_source
            .split_once("pub(crate) fn copy_ranges")
            .and_then(|(_, after)| after.split_once("let highlight_duration"))
            .map(|(method, _)| method)
            .expect("copy_ranges must build clipboard metadata before highlighting");
        assert!(yank_method.contains("editor.clipboard_selection("));
        assert!(!yank_method.contains("editor.project()"));

        let clipboard_source = include_str!("../../editor/src/clipboard.rs");
        let legacy_constructor = clipboard_source
            .split_once("pub fn for_buffer(")
            .and_then(|(_, after)| after.split_once("pub fn for_buffer_with_path_resolver("))
            .map(|(constructor, _)| constructor)
            .expect("project-aware clipboard API must wrap the neutral resolver API");
        assert!(legacy_constructor.contains("project: Option<&Entity<Project>>"));
        assert!(legacy_constructor.contains("Self::for_buffer_with_path_resolver("));

        let display_map_source = include_str!("../../editor/src/display_map.rs");
        assert!(!display_map_source.contains("use project::"));
        assert!(display_map_source.contains("ranges: Vec<DocumentFoldingRange>"));
    }

    #[test]
    fn native_language_profile_parses_without_a_wasm_store() {
        let language = tree_sitter_json::LANGUAGE.into();
        language::with_parser(|parser| {
            parser.set_language(&language).unwrap();
            let tree = parser.parse(r#"{"native": true}"#, None).unwrap();
            assert!(!tree.root_node().has_error());
        });
    }

    #[test]
    fn typography_profile_transition_is_stable_and_idempotent() {
        assert_eq!(
            TranscriptTypographyProfile::default(),
            TranscriptTypographyProfile::Reading
        );
        assert!(!typography_profile_changed(
            TranscriptTypographyProfile::Buffer,
            TranscriptTypographyProfile::Buffer,
        ));
        assert!(!typography_profile_changed(
            TranscriptTypographyProfile::Reading,
            TranscriptTypographyProfile::Reading,
        ));
        assert!(typography_profile_changed(
            TranscriptTypographyProfile::Buffer,
            TranscriptTypographyProfile::Reading,
        ));
        assert!(typography_profile_changed(
            TranscriptTypographyProfile::Reading,
            TranscriptTypographyProfile::Buffer,
        ));
    }

    #[test]
    fn typography_font_swap_preserves_editor_geometry_and_non_font_paint() {
        let mut style = TextStyle {
            color: gpui::red(),
            font_size: gpui::px(17.).into(),
            line_height: gpui::relative(1.6),
            background_color: Some(gpui::blue()),
            ..TextStyle::default()
        };
        let geometry = (style.font_size, style.line_height);
        let paint = (style.color, style.background_color, style.underline);
        let reading_font = Font {
            family: "Harness Variable Reading".into(),
            features: gpui::FontFeatures::disable_ligatures(),
            fallbacks: Some(gpui::FontFallbacks::from_fonts(vec![
                "Harness Fallback".into(),
            ])),
            weight: FontWeight::BOLD,
            style: gpui::FontStyle::Italic,
        };

        apply_typography_font(&mut style, &reading_font);

        assert_eq!((style.font_size, style.line_height), geometry);
        assert_eq!(
            (style.color, style.background_color, style.underline),
            paint
        );
        assert_eq!(style.font_family, reading_font.family);
        assert_eq!(style.font_features, reading_font.features);
        assert_eq!(style.font_fallbacks, reading_font.fallbacks);
        assert_eq!(style.font_weight, reading_font.weight);
        assert_eq!(style.font_style, reading_font.style);
    }

    #[test]
    fn typography_refinement_never_changes_size_or_line_height() {
        let font = gpui::font("Harness Reading");
        let refinement = typography_refinement(&font);

        assert_eq!(refinement.font_family, Some(font.family));
        assert_eq!(refinement.font_size, None);
        assert_eq!(refinement.line_height, None);
    }

    #[test]
    fn typography_switch_is_paint_only_for_the_persistent_editor() {
        let source = include_str!("lib.rs");
        let method = source
            .split_once("fn apply_typography_profile_to_editor(")
            .and_then(|(_, after)| after.split_once("/// Marks only the transcript's Zed Editor"))
            .map(|(method, _)| method)
            .expect("the shared typography helper must remain independently auditable");

        assert!(method.contains("editor.set_text_style_refinement("));
        assert!(method.contains("editor.set_style("));
        for forbidden in [
            "set_text(",
            "buffer.edit(",
            "change_selections(",
            "focus(",
            "undo(",
        ] {
            assert!(
                !method.contains(forbidden),
                "typography must not mutate persistent Editor state via {forbidden}"
            );
        }
    }

    #[test]
    fn default_agent_header_keeps_a_boundary_without_a_redundant_label() {
        assert!(!native_header_shows_label(TranscriptKind::Agent, "Codex"));
        assert!(native_header_shows_label(
            TranscriptKind::Agent,
            "Delegated reviewer"
        ));
        assert!(native_header_shows_label(
            TranscriptKind::Subagent,
            "protocol audit"
        ));
    }

    #[test]
    fn projection_rejects_offsets_inside_multibyte_status_separator() {
        let text = "x·y".to_string();
        let valid = TranscriptItemProjection {
            text: text.clone(),
            segment: TranscriptDocumentSegment {
                item_index: 0,
                item_key: "valid".into(),
                kind: TranscriptKind::Agent,
                whole_range: 0..text.len(),
                header_range: 0..3,
                body_range: 3..text.len(),
                semantic_spans: Vec::new(),
            },
        };
        assert!(projection_has_valid_relative_ranges(&valid));

        let invalid = TranscriptItemProjection {
            segment: TranscriptDocumentSegment {
                header_range: 0..2,
                ..valid.segment.clone()
            },
            ..valid.clone()
        };
        assert!(!projection_has_valid_relative_ranges(&invalid));

        let mut valid_semantics = valid.clone();
        valid_semantics.segment.header_range = 0..0;
        valid_semantics.segment.body_range = 0..text.len();
        valid_semantics.segment.semantic_spans = vec![TranscriptSemanticSpan {
            range: 1..3,
            style: TranscriptSemanticStyle::Strong,
        }];
        assert!(projection_has_valid_relative_ranges(&valid_semantics));

        let mut invalid_semantics = valid_semantics;
        invalid_semantics.segment.semantic_spans[0].range = 1..2;
        assert!(!projection_has_valid_relative_ranges(&invalid_semantics));
    }

    fn indexed_document(
        text: &str,
        segments: Vec<TranscriptDocumentSegment>,
    ) -> TranscriptDocument {
        let mut item_rows = vec![None; segments.last().map_or(0, |segment| segment.item_index + 1)];
        for (row, segment) in segments.iter().enumerate() {
            item_rows[segment.item_index] = Some(row as u32 * 3);
        }
        TranscriptDocument {
            text: text.into(),
            item_rows,
            segments,
        }
    }

    #[test]
    fn full_document_ranges_are_a_strict_utf8_semantic_index() {
        let text = "界 head\nbody\nnext\n";
        let first_end = "界 head\nbody\n".len();
        let valid = indexed_document(
            text,
            vec![
                TranscriptDocumentSegment {
                    item_index: 0,
                    item_key: "first".into(),
                    kind: TranscriptKind::Agent,
                    whole_range: 0..first_end,
                    header_range: 0.."界 head".len(),
                    body_range: "界 head\n".len().."界 head\nbody".len(),
                    semantic_spans: Vec::new(),
                },
                TranscriptDocumentSegment {
                    item_index: 1,
                    item_key: "second".into(),
                    kind: TranscriptKind::Command,
                    whole_range: first_end..text.len(),
                    header_range: first_end..first_end + "next".len(),
                    body_range: text.len()..text.len(),
                    semantic_spans: Vec::new(),
                },
            ],
        );
        assert!(document_has_valid_segment_ranges(&valid));

        let mut invalid = indexed_document(text, valid.segments.clone());
        invalid.segments[0].header_range.end = 1;
        assert!(!document_has_valid_segment_ranges(&invalid));

        let mut invalid = indexed_document(text, valid.segments.clone());
        invalid.segments[1].whole_range.start = first_end - 1;
        assert!(!document_has_valid_segment_ranges(&invalid));

        let mut invalid = indexed_document(text, valid.segments.clone());
        invalid.segments[1].item_index = invalid.item_rows.len();
        assert!(!document_has_valid_segment_ranges(&invalid));

        let mut invalid = indexed_document(text, valid.segments.clone());
        invalid.segments[1].item_key = "first".into();
        assert!(!document_has_valid_segment_ranges(&invalid));

        let mut invalid = indexed_document(text, valid.segments.clone());
        invalid.segments[1].whole_range.end = text.len() + 1;
        assert!(!document_has_valid_segment_ranges(&invalid));
    }

    #[test]
    fn transcript_render_only_schedules_a_bounded_visibility_refresh() {
        // This is deliberately a source-level invariant. Scheduling a
        // next-frame callback from Render looks harmless, but GPUI treats the
        // callback as platform frame demand; it caused an unfocused 10k-item
        // transcript to keep the UI thread active at the inactive-window frame
        // rate. Scroll and bounds observers are the event-driven refresh path.
        let source = include_str!("lib.rs");
        let render_impl = source
            .split_once("impl Render for TranscriptEditor")
            .and_then(|(_, after)| after.split_once("#[cfg(test)]"))
            .map(|(render_impl, _)| render_impl)
            .expect("TranscriptEditor Render implementation must precede its tests");

        assert_eq!(render_impl.matches("schedule_viewport_refresh").count(), 1);
        assert!(render_impl.contains("std::mem::take(&mut self.refresh_when_rendered)"));
        assert!(!render_impl.contains("on_next_frame"));
    }

    #[test]
    fn becoming_visible_requests_only_one_event_driven_refresh() {
        let source = include_str!("lib.rs");
        let method = source
            .split_once("pub fn refresh_after_becoming_visible")
            .and_then(|(_, after)| after.split_once("fn request_tail_autoscroll"))
            .map(|(method, _)| method)
            .expect("visibility refresh must precede tail autoscroll");

        assert_eq!(method.matches("schedule_viewport_refresh").count(), 0);
        assert_eq!(method.matches("cx.on_next_frame").count(), 0);
        assert!(method.contains("std::mem::take(&mut self.header_blocks)"));
        assert!(method.contains("self.viewport_decorations = None"));
        assert!(method.contains("self.refresh_when_rendered = true"));
    }

    #[test]
    fn persistent_search_uses_edit_stable_anchors_and_stream_dirtying() {
        // Search painting deliberately keeps no document-wide byte-range
        // index. Only the active range needs to survive streaming edits, and
        // Zed's anchors already provide the required edit semantics.
        let source = include_str!("lib.rs");
        let search_state = source
            .split_once("struct TranscriptSearchState")
            .and_then(|(_, after)| after.split_once("const VIEWPORT_OVERSCAN_ROWS"))
            .map(|(state, _)| state)
            .expect("search state must precede viewport decoration constants");
        assert!(search_state.contains("active_match: Option<Range<Anchor>>"));
        assert!(!search_state.contains("active_match: Option<Range<usize>>"));
        assert!(
            source.contains("self.search.highlights_dirty |= search_text_changed"),
            "streaming edits must invalidate only the next viewport search paint"
        );
    }

    #[test]
    fn literal_search_is_ascii_case_insensitive_and_utf8_exact() {
        let prefix = "before 🧭\n";
        let window = "🦀 Code café CODE code";
        let text = format!("{prefix}{window}");
        let matches = literal_match_ranges(window, "cOdE", prefix.len());

        assert_eq!(
            highlighted_text(&text, &matches),
            vec!["Code", "CODE", "code"]
        );
        assert!(matches.iter().all(|range| {
            text.is_char_boundary(range.start) && text.is_char_boundary(range.end)
        }));
        // Non-ASCII matching remains exact rather than changing byte lengths
        // through full Unicode case folding.
        assert!(literal_match_ranges(window, "CAFÉ", prefix.len()).is_empty());
        assert_eq!(
            highlighted_text(&text, &literal_match_ranges(window, "café", prefix.len())),
            vec!["café"]
        );
    }

    #[test]
    fn literal_search_keeps_overlapping_matches() {
        assert_eq!(
            literal_match_ranges("aaaa", "aa", 20),
            vec![20..22, 21..23, 22..24]
        );
    }

    #[test]
    fn keyword_search_distinguishes_whole_partial_case_and_utf8_ranges() {
        let text = "cat scatter cat Cat naïve_naïve";
        let document_len = text.len();
        let whole = literal_match_ranges_with_options(text, "cat", 0, document_len, true, true);
        assert_eq!(highlighted_text(text, &whole), vec!["cat", "cat"]);

        let partial = literal_match_ranges_with_options(text, "cat", 0, document_len, true, false);
        assert_eq!(highlighted_text(text, &partial), vec!["cat", "cat", "cat"]);

        let folded = literal_match_ranges_with_options(text, "cat", 0, document_len, false, true);
        assert_eq!(highlighted_text(text, &folded), vec!["cat", "cat", "Cat"]);
        let unicode_offset = text.find("naïve_naïve").unwrap() + "na".len();
        assert_eq!(
            keyword_range_at_offset(text, unicode_offset).map(|range| &text[range]),
            Some("naïve_naïve")
        );
    }

    #[test]
    fn word_search_skips_the_current_occurrence_and_wraps() {
        let text = "cat scatter cat";
        let second_cat = text.rfind("cat").unwrap();
        assert_eq!(
            find_wrapped_literal_match(text, "cat", 0, false, true, true),
            Some(second_cat)
        );
        assert_eq!(
            find_wrapped_literal_match(text, "cat", second_cat, true, true, true),
            Some(0)
        );
        assert_eq!(
            find_wrapped_literal_match(text, "cat", second_cat, false, true, true),
            Some(0),
            "forward search wraps after the last whole-word occurrence"
        );
    }

    #[test]
    fn repeat_search_respects_original_question_mark_direction() {
        assert!(!repeated_search_backwards(false, false));
        assert!(repeated_search_backwards(false, true));
        assert!(repeated_search_backwards(true, false));
        assert!(!repeated_search_backwards(true, true));
    }

    #[test]
    fn ten_thousand_item_search_paint_scans_only_viewport_text() {
        let item = "match filler payload\n";
        let transcript = item.repeat(10_000);
        let viewport = 8_000 * item.len()..8_050 * item.len();
        let viewport_text = &transcript[viewport.clone()];

        let matches = literal_match_ranges(viewport_text, "MATCH", viewport.start);

        assert_eq!(matches.len(), 50);
        assert!(
            matches
                .iter()
                .all(|range| viewport.start <= range.start && range.end <= viewport.end)
        );
        assert_eq!(viewport_text.len(), 50 * item.len());
        assert!(viewport_text.len() < transcript.len() / 100);
    }

    #[test]
    fn direct_scroll_away_pauses_streaming_tail_follow() {
        assert!(direct_scroll_can_change_follow_tail(true, false));
        assert!(!viewport_bottom_is_near_tail(8_000., 50., 9_999.));

        let following_after_scroll = viewport_bottom_is_near_tail(8_000., 50., 9_999.);
        assert!(!should_request_tail_autoscroll(
            following_after_scroll,
            true
        ));
    }

    #[test]
    fn direct_scroll_back_to_threshold_repins_tail() {
        assert!(viewport_bottom_is_near_tail(9_949., 50., 9_999.));
        assert!(!viewport_bottom_is_near_tail(9_947., 50., 9_999.));
        assert!(should_request_tail_autoscroll(true, true));
        assert!(!should_request_tail_autoscroll(true, false));
    }

    #[test]
    fn backward_vim_motion_pauses_but_forward_motion_retains_pin() {
        assert!(!follow_tail_after_selection(true, Some(900), 800, 1_000));
        assert!(follow_tail_after_selection(true, Some(800), 900, 1_000));
        assert!(!follow_tail_after_selection(false, Some(800), 900, 1_000));
        assert!(follow_tail_after_selection(false, Some(900), 1_000, 1_000));
    }

    #[test]
    fn autoscroll_events_do_not_self_pause_or_create_a_frame_loop() {
        assert!(!direct_scroll_can_change_follow_tail(true, true));
        assert!(!direct_scroll_can_change_follow_tail(false, false));

        let source = include_str!("lib.rs");
        let request = source
            .split_once("fn request_tail_autoscroll")
            .and_then(|(_, after)| after.split_once("fn note_local_selection_for_follow_tail"))
            .map(|(request, _)| request)
            .expect("tail request must precede selection intent handling");
        assert!(!request.contains("schedule_viewport_refresh"));
        assert!(!request.contains("on_next_frame"));
    }

    #[test]
    fn body_and_supplement_updates_request_tail_at_most_once() {
        let source = include_str!("lib.rs");
        let edit = source
            .split_once("pub fn edit(")
            .and_then(|(_, after)| after.split_once("pub fn upsert_supplement"))
            .map(|(edit, _)| edit)
            .expect("edit must precede supplement upsert");
        let upsert = source
            .split_once("pub fn upsert_supplement")
            .and_then(|(_, after)| after.split_once("pub fn remove_supplement"))
            .map(|(upsert, _)| upsert)
            .expect("upsert must precede supplement removal");
        let projections = source
            .split_once("pub fn apply_item_projections")
            .and_then(|(_, after)| after.split_once("fn decorate_appended_segments"))
            .map(|(projections, _)| projections)
            .expect("projection updates must precede appended decoration");

        for update_path in [edit, upsert, projections] {
            assert_eq!(
                update_path.matches("request_tail_autoscroll(cx)").count(),
                1
            );
        }
    }

    fn segment(
        item_index: usize,
        whole_range: Range<usize>,
        header_range: Range<usize>,
        body_range: Range<usize>,
    ) -> TranscriptDocumentSegment {
        segment_with_kind(
            item_index,
            TranscriptKind::Agent,
            whole_range,
            header_range,
            body_range,
        )
    }

    fn segment_with_kind(
        item_index: usize,
        kind: TranscriptKind,
        whole_range: Range<usize>,
        header_range: Range<usize>,
        body_range: Range<usize>,
    ) -> TranscriptDocumentSegment {
        TranscriptDocumentSegment {
            item_index,
            item_key: format!("item-{item_index}"),
            kind,
            whole_range,
            header_range,
            body_range,
            semantic_spans: Vec::new(),
        }
    }

    fn highlighted_text<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    }

    #[test]
    fn diff_highlights_are_exact_utf8_byte_ranges() {
        let prefix = "before 🧭\n";
        let body = concat!(
            "diff --git a/café.rs b/café.rs\r\n",
            "index 123..456 100644\n",
            "--- a/café.rs\n",
            "+++ b/café.rs\n",
            "@@ -1,2 +1,2 @@ fn café()\n",
            " unchanged\n",
            "-old 🦀\n",
            "+new 🌱\n",
            "\\ No newline at end of file"
        );
        let text = format!("{prefix}{body}");
        let mut highlights = DiffHighlightRanges::default();
        highlights.parse_body(body, prefix.len());

        assert_eq!(
            highlighted_text(&text, &highlights.file_headers),
            vec![
                "diff --git a/café.rs b/café.rs",
                "index 123..456 100644",
                "--- a/café.rs",
                "+++ b/café.rs",
            ]
        );
        assert_eq!(
            highlighted_text(&text, &highlights.hunks),
            vec!["@@ -1,2 +1,2 @@ fn café()"]
        );
        assert_eq!(
            highlighted_text(&text, &highlights.deletions),
            vec!["-old 🦀"]
        );
        assert_eq!(
            highlighted_text(&text, &highlights.additions),
            vec!["+new 🌱"]
        );
        assert_eq!(highlights.parsed_bytes, body.len());
        for range in highlights
            .file_headers
            .iter()
            .chain(&highlights.hunks)
            .chain(&highlights.deletions)
            .chain(&highlights.additions)
        {
            assert!(text.is_char_boundary(range.start));
            assert!(text.is_char_boundary(range.end));
            assert!(!highlighted_text(&text, std::slice::from_ref(range))[0].contains('\n'));
            assert!(!highlighted_text(&text, std::slice::from_ref(range))[0].contains('\r'));
        }
    }

    #[test]
    fn diff_headers_are_not_misclassified_as_changed_lines() {
        let body = concat!(
            "--- a/file.rs\n",
            "+++ b/file.rs\n",
            "@@@ -1,1 -1,1 +1,1 @@@\n",
            "-removed\n",
            "+added\n"
        );
        let mut highlights = DiffHighlightRanges::default();
        highlights.parse_body(body, 0);

        assert_eq!(
            highlighted_text(body, &highlights.file_headers),
            vec!["--- a/file.rs", "+++ b/file.rs"]
        );
        assert_eq!(
            highlighted_text(body, &highlights.hunks),
            vec!["@@@ -1,1 -1,1 +1,1 @@@"]
        );
        assert_eq!(
            highlighted_text(body, &highlights.deletions),
            vec!["-removed"]
        );
        assert_eq!(
            highlighted_text(body, &highlights.additions),
            vec!["+added"]
        );
    }

    #[test]
    fn streaming_diff_reclassification_replaces_prior_ranges() {
        let mut before = DiffHighlightRanges::default();
        before.parse_body("+draft", 100);
        let mut after = DiffHighlightRanges::default();
        after.parse_body("-old\n+final", 100);

        assert_eq!(before.additions, vec![100..106]);
        assert!(before.deletions.is_empty());
        assert_eq!(after.deletions, vec![100..104]);
        assert_eq!(after.additions, vec![105..111]);
    }

    #[test]
    fn body_only_edits_retain_native_header_anchors() {
        let segments = [
            segment(0, 0..30, 0..8, 10..28),
            segment(1, 30..60, 30..38, 40..58),
        ];

        assert!(!edit_invalidates_native_header_blocks(&(12..18), &segments));
        assert!(!edit_invalidates_native_header_blocks(&(28..28), &segments));
        assert!(!edit_invalidates_native_header_blocks(&(40..40), &segments));
    }

    #[test]
    fn structural_and_multi_body_edits_rebuild_native_headers() {
        let segments = [
            segment(0, 0..30, 0..8, 10..28),
            segment(1, 30..60, 30..38, 40..58),
        ];

        assert!(edit_invalidates_native_header_blocks(&(2..6), &segments));
        assert!(edit_invalidates_native_header_blocks(&(8..8), &segments));
        assert!(edit_invalidates_native_header_blocks(&(20..44), &segments));
        assert!(edit_invalidates_native_header_blocks(&(30..30), &segments));
        assert!(edit_invalidates_native_header_blocks(&(28..30), &segments));
        assert!(edit_invalidates_native_header_blocks(&(20..10), &segments));
    }

    #[test]
    fn item_entry_offsets_resolve_to_selectable_body_starts() {
        let segments = [
            segment(0, 0..30, 0..8, 10..28),
            segment(1, 30..60, 30..38, 40..58),
        ];

        assert_eq!(item_body_start_at_offset(0, &segments), 10);
        assert_eq!(item_body_start_at_offset(4, &segments), 10);
        assert_eq!(item_body_start_at_offset(30, &segments), 40);
        assert_eq!(item_body_start_at_offset(60, &segments), 60);
    }

    #[test]
    fn supplemental_updates_keep_block_identity_until_the_item_changes() {
        assert_eq!(
            supplement_update(false, false, false),
            SupplementUpdate::Unchanged
        );
        assert_eq!(
            supplement_update(false, true, false),
            SupplementUpdate::Resize
        );
        assert_eq!(
            supplement_update(false, false, true),
            SupplementUpdate::ReplaceRenderer
        );
        assert_eq!(
            supplement_update(false, true, true),
            SupplementUpdate::ResizeAndReplaceRenderer
        );
        assert_eq!(
            supplement_update(true, false, false),
            SupplementUpdate::Reanchor
        );
        assert_eq!(
            supplement_update(true, true, true),
            SupplementUpdate::Reanchor
        );
    }

    #[test]
    fn supplemental_anchor_follows_its_item_across_document_rebuilds() {
        let before = [
            segment(0, 0..30, 0..8, 10..28),
            segment(1, 30..60, 30..38, 40..58),
        ];
        let after = [
            segment(0, 0..75, 0..8, 10..73),
            segment(1, 75..105, 75..83, 85..103),
        ];

        assert_eq!(supplement_anchor_offset("item-1", &before), Some(58));
        assert_eq!(supplement_anchor_offset("item-1", &after), Some(103));
        assert_eq!(supplement_anchor_offset("missing", &after), None);
    }

    #[test]
    fn replacement_covers_the_whole_semantic_item_across_rebuilds() {
        let before = [
            segment(0, 0..30, 0..8, 10..28),
            segment(1, 30..60, 30..38, 40..58),
        ];
        let after = [
            segment(0, 0..75, 0..8, 10..73),
            segment(1, 75..105, 75..83, 85..103),
        ];

        assert_eq!(replacement_anchor_range("item-1", &before), Some(30..60));
        assert_eq!(replacement_anchor_range("item-1", &after), Some(75..105));
        assert_eq!(replacement_anchor_range("missing", &after), None);
    }

    #[test]
    fn adjacent_replacement_offsets_do_not_share_the_exclusive_boundary() {
        assert_eq!(replacement_anchor_offsets(0..23), Some((0, 22)));
        assert_eq!(replacement_anchor_offsets(23..43), Some((23, 42)));
        assert_eq!(replacement_anchor_offsets(9..9), None);

        let (_, first_end) = replacement_anchor_offsets(0..23).unwrap();
        let (second_start, _) = replacement_anchor_offsets(23..43).unwrap();
        assert!(first_end < second_start);
    }

    #[test]
    fn removing_supplement_above_paused_viewport_preserves_visible_rows() {
        assert_eq!(scroll_top_after_supplement_removal(10., 4, 30.), Some(26.));
        assert_eq!(scroll_top_after_supplement_removal(10., 4, 14.), Some(10.));
        assert_eq!(scroll_top_after_supplement_removal(0., 4, 4.), Some(0.));

        assert_eq!(scroll_top_after_supplement_removal(10., 4, 12.), None);
        assert_eq!(scroll_top_after_supplement_removal(20., 4, 12.), None);
    }

    #[test]
    fn multiple_nonadjacent_body_updates_shift_segments_without_overlap() {
        let mut segments = vec![
            segment(0, 0..20, 0..5, 7..18),
            segment(1, 20..40, 20..25, 27..38),
            segment(2, 40..60, 40..45, 47..58),
        ];
        let projected_last = segment(2, 0..25, 0..5, 7..23);
        let projected_first = segment(0, 0..23, 0..5, 7..21);

        assert!(apply_projected_segment_shape(
            &mut segments,
            2,
            &projected_last
        ));
        assert!(apply_projected_segment_shape(
            &mut segments,
            0,
            &projected_first
        ));

        assert_eq!(segments[0].whole_range, 0..23);
        assert_eq!(segments[0].body_range, 7..21);
        assert_eq!(segments[1].whole_range, 23..43);
        assert_eq!(segments[1].header_range, 23..28);
        assert_eq!(segments[1].body_range, 30..41);
        assert_eq!(segments[2].whole_range, 43..68);
        assert_eq!(segments[2].header_range, 43..48);
        assert_eq!(segments[2].body_range, 50..66);
        assert!(
            segments
                .windows(2)
                .all(|pair| pair[0].whole_range.end == pair[1].whole_range.start)
        );
    }

    #[test]
    fn incremental_body_updates_replace_and_shift_semantic_output_ranges() {
        let mut first = segment(0, 0..20, 0..5, 7..18);
        first.semantic_spans = vec![TranscriptSemanticSpan {
            range: 8..12,
            style: TranscriptSemanticStyle::Strong,
        }];
        let mut second = segment(1, 20..40, 20..25, 27..38);
        second.semantic_spans = vec![TranscriptSemanticSpan {
            range: 29..34,
            style: TranscriptSemanticStyle::Link,
        }];
        let mut segments = vec![first, second];

        let mut projected = segment(0, 0..24, 0..5, 7..22);
        projected.semantic_spans = vec![
            TranscriptSemanticSpan {
                range: 8..20,
                style: TranscriptSemanticStyle::Heading,
            },
            TranscriptSemanticSpan {
                range: 12..18,
                style: TranscriptSemanticStyle::Emphasis,
            },
        ];
        assert!(apply_projected_segment_shape(&mut segments, 0, &projected));

        assert_eq!(segments[0].semantic_spans, projected.semantic_spans);
        assert_eq!(segments[1].whole_range, 24..44);
        assert_eq!(
            segments[1].semantic_spans,
            [TranscriptSemanticSpan {
                range: 33..38,
                style: TranscriptSemanticStyle::Link,
            }]
        );
    }

    #[test]
    fn overscanned_window_tracks_viewport_size_and_document_edges() {
        assert_eq!(
            overscanned_point_range(
                Point::new(200, 3)..Point::new(220, 8),
                Point::new(1_000, 12),
                64,
            ),
            Point::new(136, 0)..Point::new(285, 0)
        );
        assert_eq!(
            overscanned_point_range(
                Point::new(240, 0)..Point::new(250, 12),
                Point::new(250, 12),
                64,
            ),
            Point::new(176, 0)..Point::new(250, 12)
        );
        assert_eq!(
            overscanned_point_range(Point::new(0, 0)..Point::new(0, 0), Point::new(0, 100), 64,),
            Point::new(0, 0)..Point::new(0, 100)
        );
    }

    #[test]
    fn one_row_viewport_movements_reuse_cached_decoration_coverage() {
        let cached = 136..285;

        assert_eq!(
            viewport_window_choice(&(200..220), Some(&cached)),
            ViewportWindowChoice::ReuseCached
        );
        assert_eq!(
            viewport_window_choice(&(201..221), Some(&cached)),
            ViewportWindowChoice::ReuseCached
        );
        assert_eq!(
            viewport_window_choice(&(199..219), Some(&cached)),
            ViewportWindowChoice::ReuseCached
        );
    }

    #[test]
    fn decoration_coverage_reuses_exact_edges_and_recenters_after_crossing_them() {
        let cached = 136..285;

        assert_eq!(
            viewport_window_choice(&(136..285), Some(&cached)),
            ViewportWindowChoice::ReuseCached
        );
        assert_eq!(
            viewport_window_choice(&(135..155), Some(&cached)),
            ViewportWindowChoice::Recenter
        );
        assert_eq!(
            viewport_window_choice(&(265..286), Some(&cached)),
            ViewportWindowChoice::Recenter
        );
        assert_eq!(
            viewport_window_choice(&(200..220), None),
            ViewportWindowChoice::Recenter
        );

        assert_eq!(
            overscanned_point_range(
                Point::new(135, 0)..Point::new(155, 0),
                Point::new(1_000, 0),
                64,
            ),
            Point::new(71, 0)..Point::new(220, 0)
        );
        assert_eq!(
            overscanned_point_range(
                Point::new(265, 0)..Point::new(286, 0),
                Point::new(1_000, 0),
                64,
            ),
            Point::new(201, 0)..Point::new(351, 0)
        );
    }

    #[test]
    fn ten_thousand_items_keep_native_headers_viewport_bounded() {
        let segments = (0..10_000)
            .map(|item_index| {
                let start = item_index * 20;
                segment(
                    item_index,
                    start..start + 20,
                    start..start + 6,
                    start + 8..start + 18,
                )
            })
            .collect::<Vec<_>>();

        let early = header_segments_intersecting(&segments, &(2_000..3_000));
        let late = header_segments_intersecting(&segments, &(160_000..161_000));
        assert_eq!(early, 100..150);
        assert_eq!(late, 8_000..8_050);
        assert!(early.len() < 100);
        assert!(late.len() < 100);

        let mut appended = segments.clone();
        appended.push(segment(
            10_000,
            200_000..200_020,
            200_000..200_006,
            200_008..200_018,
        ));
        assert_eq!(
            header_segments_intersecting(&appended, &(160_000..161_000)),
            late
        );
    }

    #[test]
    fn ten_thousand_diff_bodies_keep_parsing_viewport_bounded() {
        const ITEM_BYTES: usize = 64;
        let segments = (0..10_000)
            .map(|item_index| {
                let start = item_index * ITEM_BYTES;
                segment_with_kind(
                    item_index,
                    TranscriptKind::Diff,
                    start..start + ITEM_BYTES,
                    start..start + 8,
                    start + 10..start + 62,
                )
            })
            .collect::<Vec<_>>();
        let viewport = 8_000 * ITEM_BYTES..8_050 * ITEM_BYTES;

        let body_ranges = visible_diff_body_ranges(&segments, &viewport);
        let parsed_bytes = body_ranges.iter().map(Range::len).sum::<usize>();

        assert_eq!(body_ranges.len(), 50);
        assert_eq!(parsed_bytes, 50 * 52);
        assert!(body_ranges.iter().all(|range| {
            viewport.start <= range.start && range.end <= viewport.end && range.len() <= ITEM_BYTES
        }));
        assert!(parsed_bytes < 10_000 * ITEM_BYTES / 100);

        let mut mixed = segments;
        mixed[8_025].kind = TranscriptKind::Agent;
        assert_eq!(visible_diff_body_ranges(&mixed, &viewport).len(), 49);
    }

    #[test]
    fn ten_thousand_semantic_items_keep_paint_work_viewport_bounded() {
        const ITEM_BYTES: usize = 64;
        let segments = (0..10_000)
            .map(|item_index| {
                let start = item_index * ITEM_BYTES;
                let mut segment = segment(
                    item_index,
                    start..start + ITEM_BYTES,
                    start..start + 8,
                    start + 10..start + 62,
                );
                segment.semantic_spans = vec![TranscriptSemanticSpan {
                    range: start + 12..start + 24,
                    style: TranscriptSemanticStyle::Strong,
                }];
                segment
            })
            .collect::<Vec<_>>();
        let viewport = 8_000 * ITEM_BYTES..8_050 * ITEM_BYTES;

        let highlights = visible_semantic_highlight_ranges(&segments, &viewport);
        assert_eq!(highlights.strong.len(), 50);
        assert_eq!(highlights.scanned_spans, 50);
        assert!(
            highlights
                .strong
                .iter()
                .all(|range| { viewport.start <= range.start && range.end <= viewport.end })
        );
        assert!(highlights.headings.is_empty());
        assert!(highlights.links.is_empty());
    }

    #[test]
    fn overlapping_semantic_styles_are_clipped_without_flattening_channels() {
        let mut segment = segment(0, 0..80, 0..8, 10..78);
        segment.semantic_spans = vec![
            TranscriptSemanticSpan {
                range: 12..60,
                style: TranscriptSemanticStyle::Heading,
            },
            TranscriptSemanticSpan {
                range: 14..58,
                style: TranscriptSemanticStyle::CodeBlock,
            },
            TranscriptSemanticSpan {
                range: 16..56,
                style: TranscriptSemanticStyle::BlockQuote,
            },
            TranscriptSemanticSpan {
                range: 20..42,
                style: TranscriptSemanticStyle::Strong,
            },
            TranscriptSemanticSpan {
                range: 24..38,
                style: TranscriptSemanticStyle::Emphasis,
            },
            TranscriptSemanticSpan {
                range: 28..36,
                style: TranscriptSemanticStyle::InlineCode,
            },
            TranscriptSemanticSpan {
                range: 30..50,
                style: TranscriptSemanticStyle::Link,
            },
            TranscriptSemanticSpan {
                range: 32..48,
                style: TranscriptSemanticStyle::Strikethrough,
            },
        ];

        let highlights = visible_semantic_highlight_ranges(&[segment], &(32..34));
        assert_eq!(highlights.headings, [32..34]);
        assert_eq!(highlights.strong, [32..34]);
        assert_eq!(highlights.emphasis, [32..34]);
        assert_eq!(highlights.inline_code, [32..34]);
        assert_eq!(highlights.links, [32..34]);
        assert_eq!(highlights.code_blocks, [32..34]);
        assert_eq!(highlights.block_quotes, [32..34]);
        assert_eq!(highlights.strikethrough, [32..34]);
        assert_eq!(highlights.scanned_spans, 8);
    }

    #[test]
    fn monospace_semantics_form_stable_full_document_font_ranges() {
        let mut first = segment(0, 0..80, 0..10, 10..80);
        first.semantic_spans = vec![
            TranscriptSemanticSpan {
                range: 20..30,
                style: TranscriptSemanticStyle::InlineCode,
            },
            TranscriptSemanticSpan {
                range: 30..50,
                style: TranscriptSemanticStyle::CodeBlock,
            },
            TranscriptSemanticSpan {
                range: 22..26,
                style: TranscriptSemanticStyle::Strong,
            },
        ];
        let mut second = segment(1, 80..140, 80..90, 90..140);
        second.semantic_spans = vec![
            TranscriptSemanticSpan {
                range: 100..120,
                style: TranscriptSemanticStyle::CodeBlock,
            },
            TranscriptSemanticSpan {
                range: 122..130,
                style: TranscriptSemanticStyle::CommandInvocation,
            },
            TranscriptSemanticSpan {
                range: 132..140,
                style: TranscriptSemanticStyle::CommandOutput,
            },
        ];

        assert_eq!(
            semantic_monospace_ranges(&[first, second]),
            [20..50, 100..120, 122..130, 132..140]
        );
    }

    #[test]
    fn shell_semantics_keep_exact_utf8_offsets_for_selectable_commands() {
        let command = "printf '%s' \"héllo\" | rg -i hello";
        let highlights = shell_semantic_highlights(command, 100);
        let all_ranges = highlights
            .functions
            .iter()
            .chain(&highlights.variables)
            .chain(&highlights.keywords)
            .chain(&highlights.operators)
            .chain(&highlights.constants)
            .chain(&highlights.strings)
            .chain(&highlights.comments)
            .chain(&highlights.embedded)
            .chain(&highlights.punctuation)
            .collect::<Vec<_>>();

        assert!(!all_ranges.is_empty());
        assert!(all_ranges.iter().all(|range| {
            range.start >= 100
                && range.end <= 100 + command.len()
                && command.is_char_boundary(range.start - 100)
                && command.is_char_boundary(range.end - 100)
        }));
        assert!(
            all_ranges
                .iter()
                .any(|range| &command[range.start - 100..range.end - 100] == "printf")
        );
    }

    #[test]
    fn transcript_cards_and_folds_are_semantic_not_global() {
        let user = transcript_row_options(TranscriptKind::User);
        assert!(user.border.is_some());
        assert_eq!(user.corner_radius, px(6.));
        assert_eq!(user.vertical_margin, px(3.));
        assert!(!user.merge_adjacent);

        let agent = transcript_row_options(TranscriptKind::Agent);
        assert!(agent.border.is_none());
        assert_eq!(agent.corner_radius, Pixels::ZERO);
        assert_eq!(agent.vertical_margin, Pixels::ZERO);
        assert!(agent.merge_adjacent);

        let command = segment_with_kind(0, TranscriptKind::Command, 0..80, 0..10, 10..78);
        let narrative = segment_with_kind(1, TranscriptKind::Agent, 80..160, 80..90, 90..158);
        let empty_tool = segment_with_kind(2, TranscriptKind::Tool, 160..180, 160..170, 170..170);
        assert!(transcript_item_is_foldable(&command));
        assert!(!transcript_item_is_foldable(&narrative));
        assert!(!transcript_item_is_foldable(&empty_tool));
    }

    #[test]
    fn semantic_and_search_ranges_coexist_on_the_same_selectable_bytes() {
        let text = "header\nrich code remains selectable\n";
        let code_start = text.find("code").unwrap();
        let code_range = code_start..code_start + "code".len();
        let mut segment = segment(0, 0..text.len(), 0..6, 7..text.len());
        segment.semantic_spans = vec![TranscriptSemanticSpan {
            range: code_range.clone(),
            style: TranscriptSemanticStyle::InlineCode,
        }];

        let semantic = visible_semantic_highlight_ranges(&[segment], &(0..text.len()));
        let search = literal_match_ranges(text, "code", 0);
        assert_eq!(semantic.inline_code, [code_range.clone()]);
        assert_eq!(search, [code_range]);
    }

    #[test]
    fn pathological_semantic_metadata_has_a_hard_viewport_scan_budget() {
        let mut segment = segment(0, 0..20_000, 0..8, 10..19_998);
        segment.semantic_spans = (0..10_000)
            .map(|index| TranscriptSemanticSpan {
                range: 10 + index..12 + index,
                style: TranscriptSemanticStyle::Strong,
            })
            .collect();

        let highlights = visible_semantic_highlight_ranges(&[segment], &(0..20_000));
        assert_eq!(
            highlights.scanned_spans,
            MAX_SCANNED_SEMANTIC_SPANS_PER_VIEWPORT
        );
        assert_eq!(
            highlights.strong.len(),
            MAX_SCANNED_SEMANTIC_SPANS_PER_VIEWPORT
        );
    }

    #[test]
    fn viewport_shift_retains_overlapping_header_block_positions() {
        let (remove, insert) = header_window_delta(8_000..8_050, 8_025..8_075);

        assert_eq!(remove, (8_000..8_025).collect::<Vec<_>>());
        assert_eq!(insert, (8_050..8_075).collect::<Vec<_>>());
        assert!(
            (8_025..8_050)
                .all(|position| { !remove.contains(&position) && !insert.contains(&position) })
        );
    }

    #[test]
    fn append_and_stream_updates_do_not_churn_stable_viewport_headers() {
        let mounted = [100, 101, 102, 103];

        assert_eq!(
            header_window_delta(mounted, 100..104),
            (Vec::new(), Vec::new())
        );
        assert_eq!(
            header_window_delta(mounted, 100..105),
            (Vec::new(), vec![104])
        );
    }

    #[test]
    fn body_streaming_keeps_header_window_identity_stable() {
        let mut segments = vec![
            segment(0, 0..1_000, 0..10, 12..998),
            segment(1, 1_000..1_200, 1_000..1_010, 1_012..1_198),
        ];
        let before = header_segments_intersecting(&segments, &(950..1_100));
        let longer_first = segment(0, 0..1_100, 0..10, 12..1_098);
        assert!(apply_projected_segment_shape(
            &mut segments,
            0,
            &longer_first
        ));
        let after = header_segments_intersecting(&segments, &(1_050..1_200));

        assert_eq!(before, 1..2);
        assert_eq!(after, before);
        assert_eq!(segments[1].header_range, 1_100..1_110);
    }

    #[test]
    fn row_highlights_are_clipped_even_for_one_huge_item() {
        assert_eq!(
            intersect_ranges(&(0..100_000), &(50_000..50_200)),
            Some(50_000..50_200)
        );
        assert_eq!(intersect_ranges(&(0..10), &(20..30)), None);

        let segments = [segment(0, 0..100_000, 0..10, 12..99_998)];
        assert_eq!(segments_intersecting(&segments, &(50_000..50_200)), 0..1);
    }
}
