use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write as _,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::Engine as _;
use codex_app_server_client::{CodexThread, Event};
use pulldown_cmark::{
    Event as MarkdownEvent, Options as MarkdownOptions, Parser as MarkdownParser, Tag, TagEnd,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Return the smallest UTF-8-safe replacement that transforms `old` into `new`.
/// Keeping this in the protocol/document crate lets transcript streaming tests
/// stay independent from GPUI and the Zed editor dependency graph.
pub fn minimal_text_edit(old: &str, new: &str) -> (std::ops::Range<usize>, String) {
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    let maximum_suffix = old.len().min(new.len()).saturating_sub(prefix);
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(maximum_suffix)
        .take_while(|(left, right)| left == right)
        .count();
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }

    (
        prefix..old.len() - suffix,
        new[prefix..new.len() - suffix].to_string(),
    )
}

/// Find the next or previous case-insensitive transcript match, wrapping at
/// the document boundary. Returned offsets are always valid UTF-8 boundaries.
pub fn find_wrapped_match(
    text: &str,
    query: &str,
    cursor_offset: usize,
    backwards: bool,
) -> Option<usize> {
    if query.is_empty() {
        return None;
    }

    // ASCII folding preserves UTF-8 byte offsets, which avoids an index map
    // while still providing the expected case-insensitive command search.
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    let cursor_offset = previous_char_boundary(text, cursor_offset.min(text.len()));
    if backwards {
        haystack[..cursor_offset].rfind(&needle).or_else(|| {
            haystack[cursor_offset..]
                .rfind(&needle)
                .map(|at| cursor_offset + at)
        })
    } else {
        let after_cursor = next_char_boundary(text, cursor_offset);
        haystack[after_cursor..]
            .find(&needle)
            .map(|at| after_cursor + at)
            .or_else(|| haystack[..after_cursor].find(&needle))
    }
}

fn previous_char_boundary(text: &str, offset: usize) -> usize {
    (0..=offset.min(text.len()))
        .rev()
        .find(|candidate| text.is_char_boundary(*candidate))
        .unwrap_or(0)
}

fn next_char_boundary(text: &str, offset: usize) -> usize {
    let offset = previous_char_boundary(text, offset);
    text[offset..]
        .chars()
        .next()
        .map_or(offset, |character| offset + character.len_utf8())
}

const RAW_EVENT_LIMIT: usize = 4_096;
const RAW_EVENT_EVICTION_BATCH: usize = 512;
const RAW_PAYLOAD_LIMIT: usize = 256 * 1_024;
const RAW_STRING_LIMIT: usize = 64 * 1_024;
const RAW_ARRAY_LIMIT: usize = 512;
const SNAPSHOT_VERSION: u32 = 1;
const SNAPSHOT_RAW_LIMIT: usize = 512 * 1_024;
const SNAPSHOT_CONTENT_LIMIT: usize = 2 * 1_024 * 1_024;
static SNAPSHOT_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static LATEST_SNAPSHOT_WRITES: LazyLock<Mutex<HashMap<PathBuf, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TranscriptKind {
    User,
    Agent,
    Reasoning,
    Plan,
    Command,
    FileChange,
    Tool,
    Diff,
    Image,
    Subagent,
    Web,
    Review,
    Trace,
    Error,
    Approval,
}

impl TranscriptKind {
    pub fn is_structured(self) -> bool {
        !matches!(
            self,
            Self::User | Self::Agent | Self::Reasoning | Self::Plan
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingRequest {
    pub id: Value,
    pub method: String,
    pub resolved: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TranscriptItem {
    pub key: String,
    pub protocol_id: Option<String>,
    pub kind: TranscriptKind,
    pub title: String,
    pub status: Option<String>,
    pub content: String,
    pub raw: Value,
    pub event_count: usize,
    pub expanded: bool,
    pub pending_request: Option<PendingRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandTranscript {
    pub command: String,
    pub cwd: Option<String>,
    pub output: String,
}

pub const COMMAND_PROMPT: &str = "$ ";

/// Present the program supplied to a conventional Bash login-shell wrapper.
///
/// App-server command items contain one flattened shell string rather than an
/// argv array. Parse the complete outer invocation and unwrap only the exact
/// three-argument form we understand. Any ambiguity falls back losslessly to
/// the raw command, which remains available on `TranscriptItem::raw` even when
/// the friendly script is used by the transcript projection.
pub fn command_for_display(command: &str) -> Cow<'_, str> {
    let Some(words) = shlex::split(command) else {
        return Cow::Borrowed(command);
    };
    let [program, flags, script] = words.as_slice() else {
        return Cow::Borrowed(command);
    };
    let is_bash = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("bash");
    if is_bash && matches!(flags.as_str(), "-lc" | "-cl") {
        Cow::Owned(script.clone())
    } else {
        Cow::Borrowed(command)
    }
}

#[derive(Deserialize, Serialize)]
struct PersistedTranscript {
    version: u32,
    thread_id: String,
    items: Vec<TranscriptItem>,
}

/// A fully serialized transcript cache entry whose filesystem commit can be
/// moved off the UI thread. Serialization remains synchronous so the bytes are
/// an exact snapshot of the model at the call site.
pub struct PreparedTranscriptSnapshot {
    path: PathBuf,
    serialized: Vec<u8>,
    sequence: u64,
}

impl PreparedTranscriptSnapshot {
    pub fn write(self) -> anyhow::Result<()> {
        let Some(directory) = self.path.parent() else {
            anyhow::bail!("transcript snapshot path has no parent");
        };
        fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }

        let temporary_path =
            self.path
                .with_extension(format!("{}.{}.tmp", std::process::id(), self.sequence));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary_path)?;
        file.write_all(&self.serialized)?;
        file.sync_all()?;
        let mut latest_writes = LATEST_SNAPSHOT_WRITES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if latest_writes.get(&self.path).copied() != Some(self.sequence) {
            drop(file);
            let _ = fs::remove_file(temporary_path);
            return Ok(());
        }
        fs::rename(&temporary_path, &self.path)?;
        latest_writes.remove(&self.path);
        Ok(())
    }
}

impl TranscriptItem {
    pub fn display_status(&self) -> Option<&str> {
        match self.status.as_deref()? {
            status
                if is_routine_terminal_status(status)
                    || matches!(
                        status,
                        "running" | "in progress" | "inProgress" | "in_progress" | "streaming"
                    ) =>
            {
                None
            }
            status => Some(status),
        }
    }

    /// Whether this item has anything useful to occupy transcript space.
    /// Raw protocol history remains available even when an empty terminal
    /// reasoning placeholder is omitted from both reading surfaces.
    pub fn is_presentationally_visible(&self) -> bool {
        let visible_trace = self.kind != TranscriptKind::Trace
            || string_at(&self.raw, "/type") == Some("contextCompaction");
        visible_trace
            && (self.kind != TranscriptKind::Reasoning
                || !self.content.trim().is_empty()
                || self.pending_request.is_some()
                || self.display_status().is_some())
    }

    /// Extract the stable invocation and the changing output from a command
    /// item. Typed app-server fields win; the text fallback keeps older raw
    /// tool-call snapshots presentable without changing their selectable
    /// transcript representation.
    pub fn command_transcript(&self) -> Option<CommandTranscript> {
        if self.kind != TranscriptKind::Command {
            return None;
        }

        let raw_command = string_at(&self.raw, "/command");
        let raw_cwd = string_at(&self.raw, "/cwd");
        let mut remainder = self.content.as_str();
        let command = if let Some(raw_command) = raw_command {
            let display_command = command_for_display(raw_command);
            if let Some(rest) = remainder.strip_prefix("$ ") {
                if let Some(rest) = rest.strip_prefix(display_command.as_ref()) {
                    remainder = rest;
                } else if let Some(rest) = rest.strip_prefix(raw_command) {
                    // Older persisted projections may still contain the raw
                    // wrapper. Consume it without reintroducing it visually.
                    remainder = rest;
                }
            }
            display_command.into_owned()
        } else {
            let rest = remainder.strip_prefix("$ ")?;
            let separator = ["\n\nWorking directory\n", "\n\nResult\n", "\n\n"]
                .into_iter()
                .filter_map(|marker| rest.find(marker))
                .min()
                .unwrap_or(rest.len());
            let command = rest[..separator].to_string();
            remainder = &rest[separator..];
            command
        };

        remainder = remainder.trim_start_matches('\n');
        let mut cwd = raw_cwd.map(ToOwned::to_owned);
        if let Some(rest) = remainder.strip_prefix("Working directory\n") {
            if let Some((working_directory, output)) = rest.split_once("\n\nResult\n") {
                cwd.get_or_insert_with(|| working_directory.trim().to_string());
                remainder = output;
            } else {
                cwd.get_or_insert_with(|| rest.trim().to_string());
                remainder = "";
            }
        } else if let Some(rest) = remainder.strip_prefix("Result\n") {
            remainder = rest;
        }

        let output = if remainder.is_empty() {
            string_at(&self.raw, "/aggregatedOutput")
                .unwrap_or_default()
                .to_string()
        } else {
            remainder.to_string()
        };
        Some(CommandTranscript {
            command,
            cwd,
            output,
        })
    }
}

fn is_routine_terminal_status(status: &str) -> bool {
    [
        "complete",
        "completed",
        "idle",
        "inactive",
        "ready",
        "replay",
        "resolved",
        "sent",
        "succeeded",
        "success",
    ]
    .iter()
    .any(|routine| status.eq_ignore_ascii_case(routine))
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct RawEvent {
    pub sequence: usize,
    pub method: String,
    pub payload: Value,
}

pub struct TranscriptDocument {
    pub text: String,
    pub item_rows: Vec<Option<u32>>,
    pub segments: Vec<TranscriptDocumentSegment>,
}

impl TranscriptDocument {
    /// Assemble independently projected semantic items into one Editor
    /// document. The caller controls each item's body representation while
    /// this constructor owns the offset and row bookkeeping.
    pub fn from_item_projections(
        model_item_count: usize,
        projections: impl IntoIterator<Item = TranscriptItemProjection>,
    ) -> Self {
        let mut text = String::new();
        let mut item_rows = vec![None; model_item_count];
        let mut segments = Vec::new();
        let mut current_row = 0_u32;

        for projection in projections {
            let item_index = projection.segment.item_index;
            if item_index >= model_item_count {
                continue;
            }
            item_rows[item_index] = Some(current_row);
            let whole_start = text.len();
            current_row += projection
                .text
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u32;
            segments.push(shifted_segment(&projection.segment, whole_start));
            text.push_str(&projection.text);
        }

        Self {
            text,
            item_rows,
            segments,
        }
    }
}

/// The independently renderable text projection for one semantic transcript
/// item. Segment ranges are relative to `text`, so consumers can relocate the
/// projection without rebuilding the rest of the document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptItemProjection {
    pub text: String,
    pub segment: TranscriptDocumentSegment,
}

impl TranscriptItemProjection {
    pub fn header_text(&self) -> &str {
        &self.text[self.segment.header_range.clone()]
    }

    pub fn body_text(&self) -> &str {
        &self.text[self.segment.body_range.clone()]
    }

    /// Replace the selectable body while retaining this projection's item
    /// identity and optional text-view header. Rich renderers use this to feed
    /// Vim the same canonical visible text that their structured cards paint,
    /// rather than protocol-only separator rows and metadata.
    pub fn with_body_text(mut self, body: String) -> Self {
        let body_start = self.segment.body_range.start;
        self.text.truncate(body_start);
        self.text.push_str(&body);
        let body_end = self.text.len();
        if !body.is_empty() {
            self.text.push('\n');
        }
        self.segment.body_range = body_start..body_end;
        self.segment.whole_range = 0..self.text.len();
        self.segment.semantic_spans.clear();
        self
    }

    /// Remove the item separator when this projection is the final visible
    /// item in a document. Item projections normally own one trailing newline
    /// so they can be appended independently, but retaining that separator at
    /// EOF creates a real empty Editor row that a document-end Vim motion can
    /// enter even though no Rich surface paints it.
    pub fn without_terminal_separator(mut self) -> Self {
        if self.text.ends_with('\n')
            && self.segment.whole_range.end == self.text.len()
            && self.segment.body_range.end + 1 == self.segment.whole_range.end
        {
            self.text.pop();
            self.segment.whole_range.end -= 1;
        }
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptDocumentSegment {
    pub item_index: usize,
    pub item_key: String,
    pub kind: TranscriptKind,
    pub whole_range: Range<usize>,
    pub header_range: Range<usize>,
    pub body_range: Range<usize>,
    /// Theme-independent semantics in the exact selectable output coordinate
    /// space. Ranges are absolute within the containing projection/document.
    /// Structured and streaming bodies intentionally carry no spans.
    pub semantic_spans: Vec<TranscriptSemanticSpan>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TranscriptSemanticStyle {
    Heading,
    Strong,
    Emphasis,
    InlineCode,
    Link,
    CodeBlock,
    BlockQuote,
    Strikethrough,
    CommandInvocation,
    CommandOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptSemanticSpan {
    pub range: Range<usize>,
    pub style: TranscriptSemanticStyle,
}

const MAX_SEMANTIC_SPANS_PER_ITEM: usize = 2_048;

#[derive(Default)]
struct SelectableBodyProjection {
    text: String,
    semantic_spans: Vec<TranscriptSemanticSpan>,
}

#[derive(Clone, Copy)]
struct OpenSemanticSpan {
    start: usize,
    style: TranscriptSemanticStyle,
}

fn open_semantic_span(
    open: &mut Vec<OpenSemanticSpan>,
    style: TranscriptSemanticStyle,
    output: &str,
) {
    open.push(OpenSemanticSpan {
        start: output.len(),
        style,
    });
}

fn close_semantic_span(
    open: &mut Vec<OpenSemanticSpan>,
    spans: &mut Vec<TranscriptSemanticSpan>,
    style: TranscriptSemanticStyle,
    output: &str,
) {
    close_semantic_span_at(open, spans, style, output.len());
}

fn close_semantic_span_at(
    open: &mut Vec<OpenSemanticSpan>,
    spans: &mut Vec<TranscriptSemanticSpan>,
    style: TranscriptSemanticStyle,
    end: usize,
) {
    let Some(position) = open.iter().rposition(|candidate| candidate.style == style) else {
        return;
    };
    let open = open.remove(position);
    if open.start < end && spans.len() < MAX_SEMANTIC_SPANS_PER_ITEM {
        spans.push(TranscriptSemanticSpan {
            range: open.start..end,
            style,
        });
    }
}

fn close_block_semantic_span(
    open: &mut Vec<OpenSemanticSpan>,
    spans: &mut Vec<TranscriptSemanticSpan>,
    style: TranscriptSemanticStyle,
    output: &str,
) {
    // Pulldown includes the block's terminating line break in Text/Paragraph
    // events. It remains in the selectable projection as layout, but is not
    // part of the semantic content painted by the block decoration.
    close_semantic_span_at(open, spans, style, output.trim_end_matches('\n').len());
}

fn push_inline_semantic_text(
    output: &mut String,
    spans: &mut Vec<TranscriptSemanticSpan>,
    text: &str,
    style: TranscriptSemanticStyle,
) {
    let start = output.len();
    output.push_str(text);
    if start < output.len() && spans.len() < MAX_SEMANTIC_SPANS_PER_ITEM {
        spans.push(TranscriptSemanticSpan {
            range: start..output.len(),
            style,
        });
    }
}

fn trim_body_projection(mut projection: SelectableBodyProjection) -> SelectableBodyProjection {
    let trimmed_start = projection.text.len() - projection.text.trim_start().len();
    let trimmed_end = projection.text.trim_end().len();
    if trimmed_start >= trimmed_end {
        projection.text.clear();
        projection.semantic_spans.clear();
        return projection;
    }
    if trimmed_start == 0 && trimmed_end == projection.text.len() {
        projection
    } else {
        projection.text = projection.text[trimmed_start..trimmed_end].to_owned();
        projection.semantic_spans = projection
            .semantic_spans
            .into_iter()
            .filter_map(|span| {
                let start = span.range.start.max(trimmed_start);
                let end = span.range.end.min(trimmed_end);
                (start < end).then_some(TranscriptSemanticSpan {
                    range: start - trimmed_start..end - trimmed_start,
                    style: span.style,
                })
            })
            .collect();
        projection
    }
}

fn ensure_line_break(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

fn markdown_item_marker_source_range(source: &str, item: &Range<usize>) -> Option<Range<usize>> {
    let start = item.start.min(source.len());
    let end = item.end.min(source.len());
    let line_end = source[start..end]
        .find('\n')
        .map_or(end, |offset| start + offset);
    let bytes = source[start..line_end].as_bytes();
    let mut cursor = 0;
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }

    match bytes.get(cursor).copied() {
        Some(b'-' | b'+' | b'*') => cursor += 1,
        Some(byte) if byte.is_ascii_digit() => {
            let digits_start = cursor;
            while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_digit())
                && cursor - digits_start < 9
            {
                cursor += 1;
            }
            if !matches!(bytes.get(cursor), Some(b'.' | b')')) {
                return None;
            }
            cursor += 1;
        }
        _ => return None,
    }
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    Some(start..start + cursor)
}

fn selectable_markdown_text_with_link_destinations(
    source: &str,
    include_link_destinations: bool,
    source_faithful_replacements: bool,
) -> SelectableBodyProjection {
    let options = MarkdownOptions::ENABLE_STRIKETHROUGH
        | MarkdownOptions::ENABLE_TABLES
        | MarkdownOptions::ENABLE_TASKLISTS
        | MarkdownOptions::ENABLE_FOOTNOTES
        | MarkdownOptions::ENABLE_MATH;
    let mut output = String::with_capacity(source.len());
    let mut semantic_spans = Vec::new();
    let mut open_semantic_spans = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut destinations: Vec<(String, usize, bool)> = Vec::new();

    for (event, source_range) in MarkdownParser::new_ext(source, options).into_offset_iter() {
        match event {
            MarkdownEvent::Start(Tag::Heading { .. }) => open_semantic_span(
                &mut open_semantic_spans,
                TranscriptSemanticStyle::Heading,
                &output,
            ),
            MarkdownEvent::Start(Tag::Strong) => open_semantic_span(
                &mut open_semantic_spans,
                TranscriptSemanticStyle::Strong,
                &output,
            ),
            MarkdownEvent::Start(Tag::Emphasis) => open_semantic_span(
                &mut open_semantic_spans,
                TranscriptSemanticStyle::Emphasis,
                &output,
            ),
            MarkdownEvent::Start(Tag::Strikethrough) => open_semantic_span(
                &mut open_semantic_spans,
                TranscriptSemanticStyle::Strikethrough,
                &output,
            ),
            MarkdownEvent::Start(Tag::List(start)) => {
                ensure_line_break(&mut output);
                lists.push(start);
            }
            MarkdownEvent::Start(Tag::Item) => {
                ensure_line_break(&mut output);
                if source_faithful_replacements {
                    if let Some(marker) = markdown_item_marker_source_range(source, &source_range) {
                        output.push_str(&source[marker]);
                    }
                } else if let Some(next) = lists.last_mut() {
                    match next {
                        Some(number) => {
                            output.push_str(&format!("{number}. "));
                            *number += 1;
                        }
                        None => output.push_str("• "),
                    }
                }
            }
            MarkdownEvent::Start(Tag::CodeBlock(_)) => {
                ensure_line_break(&mut output);
                open_semantic_span(
                    &mut open_semantic_spans,
                    TranscriptSemanticStyle::CodeBlock,
                    &output,
                );
            }
            MarkdownEvent::Start(Tag::BlockQuote(_)) => {
                ensure_line_break(&mut output);
                open_semantic_span(
                    &mut open_semantic_spans,
                    TranscriptSemanticStyle::BlockQuote,
                    &output,
                );
            }
            MarkdownEvent::Start(Tag::Link { dest_url, .. }) => {
                open_semantic_span(
                    &mut open_semantic_spans,
                    TranscriptSemanticStyle::Link,
                    &output,
                );
                destinations.push((dest_url.into_string(), output.len(), false));
            }
            MarkdownEvent::Start(Tag::Image { dest_url, .. }) => {
                if !source_faithful_replacements {
                    output.push_str("Image: ");
                }
                open_semantic_span(
                    &mut open_semantic_spans,
                    TranscriptSemanticStyle::Link,
                    &output,
                );
                destinations.push((dest_url.into_string(), output.len(), true));
            }
            MarkdownEvent::End(TagEnd::List(_)) => {
                lists.pop();
                ensure_line_break(&mut output);
            }
            MarkdownEvent::End(TagEnd::Link) | MarkdownEvent::End(TagEnd::Image) => {
                if let Some((destination, label_start, image)) = destinations.pop()
                    && include_link_destinations
                    && !destination.is_empty()
                    && !output[label_start..].contains(&destination)
                {
                    if image || !output[label_start..].trim().is_empty() {
                        output.push_str(" (");
                        output.push_str(&destination);
                        output.push(')');
                    } else {
                        output.push_str(&destination);
                    }
                }
                close_semantic_span(
                    &mut open_semantic_spans,
                    &mut semantic_spans,
                    TranscriptSemanticStyle::Link,
                    &output,
                );
            }
            MarkdownEvent::End(TagEnd::Strong) => close_semantic_span(
                &mut open_semantic_spans,
                &mut semantic_spans,
                TranscriptSemanticStyle::Strong,
                &output,
            ),
            MarkdownEvent::End(TagEnd::Emphasis) => close_semantic_span(
                &mut open_semantic_spans,
                &mut semantic_spans,
                TranscriptSemanticStyle::Emphasis,
                &output,
            ),
            MarkdownEvent::End(TagEnd::Strikethrough) => close_semantic_span(
                &mut open_semantic_spans,
                &mut semantic_spans,
                TranscriptSemanticStyle::Strikethrough,
                &output,
            ),
            MarkdownEvent::End(TagEnd::Heading(_)) => {
                close_semantic_span(
                    &mut open_semantic_spans,
                    &mut semantic_spans,
                    TranscriptSemanticStyle::Heading,
                    &output,
                );
                ensure_line_break(&mut output);
            }
            MarkdownEvent::End(TagEnd::CodeBlock) => {
                close_block_semantic_span(
                    &mut open_semantic_spans,
                    &mut semantic_spans,
                    TranscriptSemanticStyle::CodeBlock,
                    &output,
                );
                ensure_line_break(&mut output);
            }
            MarkdownEvent::End(TagEnd::BlockQuote(_)) => {
                close_block_semantic_span(
                    &mut open_semantic_spans,
                    &mut semantic_spans,
                    TranscriptSemanticStyle::BlockQuote,
                    &output,
                );
                ensure_line_break(&mut output);
            }
            MarkdownEvent::End(
                TagEnd::Paragraph | TagEnd::Item | TagEnd::TableHead | TagEnd::TableRow,
            ) => ensure_line_break(&mut output),
            MarkdownEvent::End(TagEnd::TableCell) => {
                if !source_faithful_replacements {
                    output.push('\t');
                }
            }
            MarkdownEvent::Code(text) => push_inline_semantic_text(
                &mut output,
                &mut semantic_spans,
                &text,
                TranscriptSemanticStyle::InlineCode,
            ),
            MarkdownEvent::Text(text)
            | MarkdownEvent::InlineMath(text)
            | MarkdownEvent::DisplayMath(text) => output.push_str(&text),
            MarkdownEvent::Html(html) | MarkdownEvent::InlineHtml(html) => output.push_str(&html),
            MarkdownEvent::FootnoteReference(label) => {
                if source_faithful_replacements {
                    output.push_str(&source[source_range]);
                } else {
                    output.push_str("[^");
                    output.push_str(&label);
                    output.push(']');
                }
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => output.push('\n'),
            MarkdownEvent::Rule => {
                ensure_line_break(&mut output);
                if source_faithful_replacements {
                    output.push_str(source[source_range].trim_end_matches(['\r', '\n']));
                } else {
                    output.push_str("────────");
                }
                output.push('\n');
            }
            MarkdownEvent::TaskListMarker(checked) => {
                if source_faithful_replacements {
                    output.push_str(&source[source_range]);
                    output.push(' ');
                } else {
                    output.push_str(if checked { "[x] " } else { "[ ] " });
                }
            }
            MarkdownEvent::Start(_) | MarkdownEvent::End(_) => {}
        }
    }

    semantic_spans.sort_by(|left, right| {
        left.range
            .start
            .cmp(&right.range.start)
            .then_with(|| left.range.end.cmp(&right.range.end))
            .then_with(|| left.style.cmp(&right.style))
    });
    trim_body_projection(SelectableBodyProjection {
        text: output,
        semantic_spans,
    })
}

/// Byte ranges in raw Markdown whose glyph geometry should use the buffer font.
///
/// The composer remains a real plaintext Editor: delimiters stay visible and
/// selectable, while inline and fenced code can use monospace metrics without
/// replacing any source bytes with ornamental UI.
pub fn markdown_monospace_source_ranges(source: &str) -> Vec<Range<usize>> {
    let options = MarkdownOptions::ENABLE_STRIKETHROUGH
        | MarkdownOptions::ENABLE_TABLES
        | MarkdownOptions::ENABLE_TASKLISTS
        | MarkdownOptions::ENABLE_FOOTNOTES
        | MarkdownOptions::ENABLE_MATH;
    let mut ranges = MarkdownParser::new_ext(source, options)
        .into_offset_iter()
        .filter_map(|(event, range)| {
            matches!(
                event,
                MarkdownEvent::Code(_)
                    | MarkdownEvent::Start(Tag::CodeBlock(_))
                    | MarkdownEvent::End(TagEnd::CodeBlock)
            )
            .then_some(range)
        })
        .filter(|range| range.start < range.end && range.end <= source.len())
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| (range.start, range.end));

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn selectable_markdown_text(source: &str) -> SelectableBodyProjection {
    selectable_markdown_text_with_link_destinations(source, true, false)
}

/// Plain-text coordinate space for a Rich Markdown surface. Link destinations
/// are intentionally absent because the Rich renderer paints only their
/// labels; retaining hidden destination bytes makes native cursor motion and
/// mouse placement diverge after the first link.
pub fn rich_markdown_navigation_text(source: &str) -> String {
    let source = normalize_buffer_line_endings(source.to_owned());
    selectable_markdown_text_with_link_destinations(&source, false, true).text
}

fn selectable_transcript_body(
    item: &TranscriptItem,
    normalized_content: &str,
) -> SelectableBodyProjection {
    let append_stable_stream = item.status.as_deref().is_some_and(|status| {
        matches!(
            status,
            "streaming" | "running" | "inProgress" | "in_progress"
        )
    });
    if matches!(
        item.kind,
        TranscriptKind::User
            | TranscriptKind::Agent
            | TranscriptKind::Reasoning
            | TranscriptKind::Plan
    ) {
        if append_stable_stream {
            SelectableBodyProjection {
                text: normalized_content.to_owned(),
                semantic_spans: Vec::new(),
            }
        } else {
            selectable_markdown_text(normalized_content)
        }
    } else if item.kind == TranscriptKind::Command {
        if let Some(command) = item.command_transcript() {
            let output = normalize_buffer_line_endings(command.output)
                .trim_end_matches(['\r', '\n'])
                .to_owned();
            let command = normalize_buffer_line_endings(command.command)
                .trim_end_matches(['\r', '\n'])
                .to_owned();
            let mut text = if command.is_empty() {
                String::new()
            } else {
                format!("{COMMAND_PROMPT}{command}")
            };
            let mut semantic_spans = Vec::new();
            if !command.is_empty() {
                semantic_spans.push(TranscriptSemanticSpan {
                    range: 0..text.len(),
                    style: TranscriptSemanticStyle::CommandInvocation,
                });
            }
            if !output.is_empty() {
                if !text.is_empty() {
                    // This newline is the only logical separator painted by
                    // the Rich command card. Keeping the protocol's blank
                    // presentation line here would give native Vim an
                    // invisible row on which its cursor could get stranded.
                    text.push('\n');
                }
                let output_start = text.len();
                text.push_str(&output);
                semantic_spans.push(TranscriptSemanticSpan {
                    range: output_start..text.len(),
                    style: TranscriptSemanticStyle::CommandOutput,
                });
            }
            SelectableBodyProjection {
                text,
                semantic_spans,
            }
        } else {
            SelectableBodyProjection {
                text: normalized_content
                    .strip_prefix("$ ")
                    .unwrap_or(normalized_content)
                    .trim()
                    .to_owned(),
                semantic_spans: Vec::new(),
            }
        }
    } else {
        SelectableBodyProjection {
            text: normalized_content.trim().to_owned(),
            semantic_spans: Vec::new(),
        }
    }
}

/// Match the normalization performed by Zed's text Buffer for every inserted
/// edit. Transcript byte ranges are consumed directly by that Buffer, so the
/// protocol projection must use the same coordinate space or every CRLF after
/// the first one shifts semantic headers, search matches, and diff styling.
fn normalize_buffer_line_endings(mut text: String) -> String {
    if text.contains('\r') {
        text = text.replace("\r\n", "\n").replace('\r', "\n");
    }
    text
}

fn project_transcript_item(
    item_index: usize,
    item: &TranscriptItem,
) -> Option<TranscriptItemProjection> {
    project_transcript_item_with_header(item_index, item, true)
}

fn project_transcript_item_with_header(
    item_index: usize,
    item: &TranscriptItem,
    include_header: bool,
) -> Option<TranscriptItemProjection> {
    // Visibility belongs to the protocol item, not to the broad `Trace`
    // presentation bucket. Most unknown trace events remain hidden, while
    // semantic trace landmarks such as context compaction participate in the
    // same Rich/Vim document as every other visible item.
    if !item.is_presentationally_visible() {
        return None;
    }

    let mut text = String::new();
    let header_start = text.len();
    let show_header = include_header
        && !matches!(
            (item.kind, item.title.as_str()),
            (TranscriptKind::Agent, "Codex") | (TranscriptKind::User, "You")
        );
    if show_header {
        text.push_str("━━━━ ");
        text.push_str(&normalize_buffer_line_endings(item.title.clone()));
        if let Some(status) = item.display_status() {
            text.push_str(" · ");
            text.push_str(&normalize_buffer_line_endings(status.to_owned()));
        }
        text.push_str(" ━━━━");
    }
    let header_end = text.len();
    // Match the Rich transcript's attribution policy: ordinary user and agent
    // messages are self-evident and do not consume a decorative header row.
    if show_header {
        text.push('\n');
    }

    let body_start = text.len();
    let normalized_content = normalize_buffer_line_endings(item.content.clone());
    let body = selectable_transcript_body(item, &normalized_content);
    if !body.text.is_empty() {
        text.push_str(&body.text);
        text.push('\n');
    }
    let body_end = body_start + body.text.len();

    let whole_end = text.len();
    Some(TranscriptItemProjection {
        text,
        segment: TranscriptDocumentSegment {
            item_index,
            item_key: item.key.clone(),
            kind: item.kind,
            whole_range: 0..whole_end,
            header_range: header_start..header_end,
            body_range: body_start..body_end,
            semantic_spans: body
                .semantic_spans
                .into_iter()
                .map(|span| TranscriptSemanticSpan {
                    range: shifted_range(&span.range, body_start),
                    style: span.style,
                })
                .collect(),
        },
    })
}

fn shifted_range(range: &Range<usize>, offset: usize) -> Range<usize> {
    range.start + offset..range.end + offset
}

fn shifted_segment(
    segment: &TranscriptDocumentSegment,
    offset: usize,
) -> TranscriptDocumentSegment {
    TranscriptDocumentSegment {
        item_index: segment.item_index,
        item_key: segment.item_key.clone(),
        kind: segment.kind,
        whole_range: shifted_range(&segment.whole_range, offset),
        header_range: shifted_range(&segment.header_range, offset),
        body_range: shifted_range(&segment.body_range, offset),
        semantic_spans: segment
            .semantic_spans
            .iter()
            .map(|span| TranscriptSemanticSpan {
                range: shifted_range(&span.range, offset),
                style: span.style,
            })
            .collect(),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ApprovalPolicySnapshot {
    Named(String),
    Granular { granular: GranularApprovalSnapshot },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GranularApprovalSnapshot {
    pub mcp_elicitations: Option<bool>,
    pub rules: Option<bool>,
    pub sandbox_approval: Option<bool>,
    pub request_permissions: Option<bool>,
    pub skill_approval: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SandboxPolicySnapshot {
    DangerFullAccess,
    ReadOnly {
        #[serde(rename = "networkAccess")]
        network_access: Option<bool>,
    },
    ExternalSandbox {
        #[serde(rename = "networkAccess")]
        network_access: Option<String>,
    },
    WorkspaceWrite {
        #[serde(rename = "networkAccess")]
        network_access: Option<bool>,
        #[serde(rename = "writableRoots")]
        writable_roots: Option<Vec<String>>,
        #[serde(rename = "excludeSlashTmp")]
        exclude_slash_tmp: Option<bool>,
        #[serde(rename = "excludeTmpdirEnvVar")]
        exclude_tmpdir_env_var: Option<bool>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivePermissionProfileSnapshot {
    pub id: Option<String>,
    pub extends: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CollaborationModeSettingsSnapshot {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub developer_instructions: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CollaborationModeSnapshot {
    pub mode: Option<String>,
    pub settings: Option<CollaborationModeSettingsSnapshot>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThreadSettingsSnapshot {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub effort: Option<String>,
    pub approval_policy: Option<ApprovalPolicySnapshot>,
    pub sandbox_policy: Option<SandboxPolicySnapshot>,
    pub active_permission_profile: Option<ActivePermissionProfileSnapshot>,
    pub collaboration_mode: Option<CollaborationModeSnapshot>,
    pub cwd: Option<String>,
    pub personality: Option<String>,
    pub summary: Option<String>,
    pub service_tier: Option<String>,
}

impl ThreadSettingsSnapshot {
    fn from_value(settings: &Value) -> Self {
        Self {
            model: string_at(settings, "/model").map(ToOwned::to_owned),
            model_provider: string_at(settings, "/modelProvider").map(ToOwned::to_owned),
            effort: string_at(settings, "/effort").map(ToOwned::to_owned),
            approval_policy: setting_value(settings, "approvalPolicy"),
            sandbox_policy: setting_value(settings, "sandboxPolicy"),
            active_permission_profile: setting_value(settings, "activePermissionProfile"),
            collaboration_mode: setting_value(settings, "collaborationMode"),
            cwd: string_at(settings, "/cwd").map(ToOwned::to_owned),
            personality: string_at(settings, "/personality").map(ToOwned::to_owned),
            summary: string_at(settings, "/summary").map(ToOwned::to_owned),
            service_tier: string_at(settings, "/serviceTier").map(ToOwned::to_owned),
        }
    }

    pub fn from_open_response(
        model: String,
        model_provider: String,
        effort: Option<String>,
        approval_policy: Value,
        sandbox_policy: Value,
        active_permission_profile: Option<Value>,
        service_tier: Option<String>,
        cwd: String,
    ) -> Self {
        Self {
            model: Some(model),
            model_provider: (!model_provider.is_empty()).then_some(model_provider),
            effort,
            approval_policy: serde_json::from_value(approval_policy).ok(),
            sandbox_policy: serde_json::from_value(sandbox_policy).ok(),
            active_permission_profile: active_permission_profile
                .and_then(|value| serde_json::from_value(value).ok()),
            cwd: (!cwd.is_empty()).then_some(cwd),
            service_tier,
            ..Self::default()
        }
    }
}

fn setting_value<T>(settings: &Value, field: &str) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    settings
        .get(field)
        .filter(|value| !value.is_null())
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

#[derive(Default)]
pub struct SessionTelemetry {
    pub thread_status: Option<String>,
    pub mcp_servers: HashMap<String, String>,
    pub token_usage: Option<Value>,
    pub goal_active: bool,
    pub rate_limits: Option<Value>,
    pub remote_control_status: Option<String>,
    pub model_status: Option<String>,
    pub thread_settings: Option<ThreadSettingsSnapshot>,
}

impl SessionTelemetry {
    pub fn set_thread_settings(&mut self, settings: ThreadSettingsSnapshot) {
        self.thread_settings = Some(settings);
    }

    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(status) = &self.thread_status {
            parts.push(status.to_uppercase());
        }
        if !self.mcp_servers.is_empty() {
            let ready_count = self
                .mcp_servers
                .values()
                .filter(|status| matches!(status.as_str(), "ready" | "completed"))
                .count();
            parts.push(format!(
                "MCP {}/{} ready",
                ready_count,
                self.mcp_servers.len()
            ));
        }
        if self.goal_active {
            parts.push("GOAL ACTIVE".into());
        }
        if let Some(status) = &self.model_status {
            parts.push(status.clone());
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    fn ingest(&mut self, method: &str, params: &Value) -> bool {
        let handled = match method {
            "thread/status/changed" => {
                self.thread_status = string_at(params, "/status/type")
                    .or_else(|| string_at(params, "/status"))
                    .or_else(|| string_at(params, "/type"))
                    .map(ToOwned::to_owned);
                true
            }
            "thread/tokenUsage/updated" => {
                self.token_usage = Some(params.clone());
                true
            }
            "thread/settings/updated" => {
                let settings = params.get("threadSettings").unwrap_or(&Value::Null);
                self.thread_settings = Some(ThreadSettingsSnapshot::from_value(settings));
                true
            }
            "thread/goal/updated" => {
                self.goal_active = true;
                true
            }
            "thread/goal/cleared" => {
                self.goal_active = false;
                true
            }
            "mcpServer/startupStatus/updated" => {
                let server = string_at(params, "/serverName")
                    .or_else(|| string_at(params, "/name"))
                    .unwrap_or("MCP server")
                    .to_string();
                let status = string_at(params, "/status")
                    .or_else(|| string_at(params, "/startupStatus/type"))
                    .or_else(|| string_at(params, "/startupStatus"));
                self.mcp_servers
                    .insert(server, status.unwrap_or("unknown").to_string());
                true
            }
            "account/rateLimits/updated" => {
                self.rate_limits = Some(params.clone());
                true
            }
            "remoteControl/status/changed" => {
                self.remote_control_status = string_at(params, "/status")
                    .or_else(|| string_at(params, "/state"))
                    .map(ToOwned::to_owned);
                true
            }
            "model/safetyBuffering/updated" => {
                self.model_status = params
                    .get("showBufferingUi")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then(|| "SAFETY BUFFER".into());
                true
            }
            _ => false,
        };
        handled
    }
}

#[derive(Default)]
pub struct BatchOutcome {
    pub dirty: HashSet<usize>,
    pub refresh_threads: bool,
    pub renamed_thread: Option<String>,
    pub transport_error: Option<String>,
}

#[derive(Default)]
pub struct ThreadRefreshOutcome {
    pub dirty: HashSet<usize>,
    pub old_len: usize,
    pub new_len: usize,
    pub reset: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserImageSource {
    Url(String),
    LocalPath(String),
}

#[derive(Default)]
pub struct TranscriptModel {
    pub items: Vec<TranscriptItem>,
    item_indices: HashMap<String, usize>,
    user_image_sources: HashMap<String, Vec<UserImageSource>>,
    pub raw_events: Vec<RawEvent>,
    pub telemetry: SessionTelemetry,
    pub current_turn_id: Option<String>,
    next_raw_sequence: usize,
    pub dropped_raw_events: usize,
}

impl TranscriptModel {
    pub fn replay(item_count: usize) -> Self {
        let mut this = Self::default();
        let templates = replay_templates()
            .into_iter()
            .filter(TranscriptItem::is_presentationally_visible)
            .collect::<Vec<_>>();
        for index in 0..item_count {
            let mut item = templates[index % templates.len()].clone();
            item.key = format!("replay:{index}");
            item.protocol_id = Some(format!("fixture-{index}"));
            if item.kind == TranscriptKind::Command {
                let command = "cargo check -p harness_app";
                item.content =
                    format!("$ {command}\n\nFinished replay frame {index} without blocking paint");
                if let Some(raw) = item.raw.as_object_mut() {
                    raw.insert("command".into(), command.into());
                }
            }
            this.push_without_splice(item);
        }
        this
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.item_indices.clear();
        self.user_image_sources.clear();
        self.raw_events.clear();
        self.telemetry = SessionTelemetry::default();
        self.next_raw_sequence = 0;
        self.dropped_raw_events = 0;
        self.current_turn_id = None;
    }

    pub fn document_window(&self, center: usize, item_limit: usize) -> TranscriptDocument {
        let item_limit = item_limit.max(1);
        let mut start = center.saturating_sub(item_limit / 2);
        let end = (start + item_limit).min(self.items.len());
        start = end.saturating_sub(item_limit);
        self.document_range(start, end, true, true)
    }

    /// Project every semantic transcript item into one selectable document.
    /// Unlike [`Self::document_window`], this never inserts truncation markers.
    pub fn full_document(&self) -> TranscriptDocument {
        self.document_range(0, self.items.len(), false, true)
    }

    /// Project the selectable content used to drive the Rich transcript's
    /// native Editor/Vim state. Rich headers are interactive chrome rather than
    /// hidden text rows, so this document contains only the bodies users can
    /// actually see and yank.
    pub fn rich_navigation_document(&self) -> TranscriptDocument {
        self.document_range(0, self.items.len(), false, false)
    }

    /// Project one model item without visiting or allocating the rest of the
    /// transcript. Trace-only events intentionally have no document projection.
    pub fn item_projection(&self, item_index: usize) -> Option<TranscriptItemProjection> {
        project_transcript_item(item_index, self.items.get(item_index)?)
    }

    /// Incremental counterpart to [`Self::rich_navigation_document`].
    pub fn rich_navigation_item_projection(
        &self,
        item_index: usize,
    ) -> Option<TranscriptItemProjection> {
        project_transcript_item_with_header(item_index, self.items.get(item_index)?, false)
    }

    fn document_range(
        &self,
        start: usize,
        end: usize,
        include_window_markers: bool,
        include_headers: bool,
    ) -> TranscriptDocument {
        let mut text = String::new();
        let mut item_rows = vec![None; self.items.len()];
        let mut segments = Vec::with_capacity(end.saturating_sub(start));
        let mut current_row = 0_u32;
        if include_window_markers && start > 0 {
            let marker = format!(
                "━━━━ {} EARLIER BLOCKS · RETURN TO RICH VIEW TO JUMP ━━━━\n\n",
                start
            );
            current_row += marker.bytes().filter(|byte| *byte == b'\n').count() as u32;
            text.push_str(&marker);
        }
        for (index, item) in self.items[start..end].iter().enumerate() {
            let index = start + index;
            let Some(projection) =
                project_transcript_item_with_header(index, item, include_headers)
            else {
                continue;
            };
            item_rows[index] = Some(current_row);

            let whole_start = text.len();
            current_row += projection
                .text
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u32;
            segments.push(shifted_segment(&projection.segment, whole_start));
            text.push_str(&projection.text);
        }
        if include_window_markers && end < self.items.len() {
            text.push_str(&format!(
                "━━━━ {} LATER BLOCKS · RETURN TO RICH VIEW TO JUMP ━━━━\n",
                self.items.len() - end
            ));
        }
        TranscriptDocument {
            text,
            item_rows,
            segments,
        }
    }

    pub fn load_thread(&mut self, thread: &CodexThread) {
        self.clear();
        for turn in &thread.turns {
            for protocol_item in &turn.items {
                let mut raw = protocol_item.body.clone();
                raw.insert("id".into(), Value::String(protocol_item.id.clone()));
                raw.insert("type".into(), Value::String(protocol_item.kind.clone()));
                let _ = self.upsert_protocol_item(Value::Object(raw), true, Some(&turn.id));
            }
        }
    }

    /// Refresh a thread/read snapshot without rebuilding stable transcript
    /// items. This is used when another Codex client owns the thread: Harness
    /// can observe it, but cannot subscribe to its live notifications.
    pub fn refresh_thread(&mut self, thread: &CodexThread) -> ThreadRefreshOutcome {
        let old_len = self.items.len();
        let mut fresh = Self::default();
        fresh.load_thread(thread);
        let mut fresh_items = std::mem::take(&mut fresh.items);
        let fresh_user_image_sources = std::mem::take(&mut fresh.user_image_sources);
        let new_len = fresh_items.len();
        let prefix_compatible = old_len <= new_len
            && self
                .items
                .iter()
                .zip(&fresh_items)
                .all(|(old, new)| old.key == new.key);

        if !prefix_compatible {
            let expanded = self
                .items
                .iter()
                .map(|item| (item.key.clone(), item.expanded))
                .collect::<HashMap<_, _>>();
            for item in &mut fresh_items {
                if let Some(expanded) = expanded.get(&item.key) {
                    item.expanded = *expanded;
                }
            }
            self.items = fresh_items;
            self.user_image_sources = fresh_user_image_sources;
            self.rebuild_item_indices();
            return ThreadRefreshOutcome {
                dirty: (0..new_len).collect(),
                old_len,
                new_len,
                reset: true,
            };
        }

        let mut dirty = HashSet::new();
        let appended = fresh_items.split_off(old_len);
        for (index, incoming) in fresh_items.into_iter().enumerate() {
            let current = &self.items[index];
            if thread_snapshot_items_equal(current, &incoming) {
                continue;
            }
            let expanded = current.expanded;
            let event_count = current.event_count.saturating_add(1);
            let mut incoming = incoming;
            incoming.expanded = expanded;
            incoming.event_count = event_count;
            self.items[index] = incoming;
            dirty.insert(index);
        }
        self.items.extend(appended);
        self.user_image_sources = fresh_user_image_sources;
        self.rebuild_item_indices();

        ThreadRefreshOutcome {
            dirty,
            old_len,
            new_len,
            reset: false,
        }
    }

    pub fn persist_transcript(&self, thread_id: &str) -> anyhow::Result<()> {
        self.prepare_transcript_snapshot(thread_id)?.write()
    }

    pub fn prepare_transcript_snapshot(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<PreparedTranscriptSnapshot> {
        let path = transcript_snapshot_path(thread_id)?;
        let sequence = SNAPSHOT_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        LATEST_SNAPSHOT_WRITES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.clone(), sequence);
        let snapshot = PersistedTranscript {
            version: SNAPSHOT_VERSION,
            thread_id: thread_id.to_string(),
            // Persistence is an invariant boundary, not a byte-for-byte dump
            // of potentially corrupt in-memory identity state. Older builds
            // could append a second copy of a restored history; never write
            // that ambiguity back out again.
            items: deduplicate_transcript_items_keep_last(
                self.items.iter().map(item_for_snapshot).collect(),
            ),
        };
        let serialized = serde_json::to_vec(&snapshot)?;
        Ok(PreparedTranscriptSnapshot {
            path,
            serialized,
            sequence,
        })
    }

    pub fn merge_persisted_transcript(&mut self, thread_id: &str) -> anyhow::Result<usize> {
        let Some(snapshot) = read_persisted_transcript(thread_id)? else {
            return Ok(0);
        };
        let fresh = std::mem::take(&mut self.items);
        let (items, restored) = merge_snapshot_items(fresh, snapshot.items);
        self.items = items;
        self.rebuild_item_indices();
        Ok(restored)
    }

    /// Restores a previously rendered transcript before the authoritative
    /// app-server snapshot arrives. Callers must still refresh from the server;
    /// this cache exists only to make revisiting large threads immediate.
    pub fn restore_persisted_transcript(&mut self, thread_id: &str) -> anyhow::Result<usize> {
        let Some(snapshot) = read_persisted_transcript(thread_id)? else {
            return Ok(0);
        };
        Ok(self.restore_persisted_snapshot(snapshot))
    }

    fn restore_persisted_snapshot(&mut self, snapshot: PersistedTranscript) -> usize {
        self.clear();
        self.items = deduplicate_transcript_items_keep_last(
            snapshot
                .items
                .into_iter()
                .filter(|item| !originates_from_raw_response(item))
                .collect(),
        );
        self.rebuild_item_indices();
        self.items.len()
    }

    pub fn push_local_user(
        &mut self,
        client_user_message_id: &str,
        content: String,
        content_blocks: &[Value],
    ) -> (usize, String) {
        let key = local_user_key(client_user_message_id);
        let image_sources = user_image_sources_from_blocks(content_blocks);
        let index = self.push_without_splice(TranscriptItem {
            key: key.clone(),
            protocol_id: None,
            kind: TranscriptKind::User,
            title: "You".into(),
            status: Some("sending".into()),
            raw: json!({
                "content": content,
                "imageCount": image_sources.len(),
            }),
            content,
            event_count: 1,
            expanded: true,
            pending_request: None,
        });
        if !image_sources.is_empty() {
            self.user_image_sources.insert(key.clone(), image_sources);
        }
        (index, key)
    }

    /// Ensure a successfully started queued submission has a visible user
    /// item even when its authoritative item notification is late or absent.
    /// A later authoritative item still reconciles through the stable client
    /// id (or the existing legacy content fallback) instead of duplicating it.
    pub fn ensure_local_user(
        &mut self,
        client_user_message_id: &str,
        content: String,
        content_blocks: &[Value],
    ) -> Option<(usize, String)> {
        let local_key = local_user_key(client_user_message_id);
        if self.item_indices.contains_key(&local_key)
            || self.items.iter().any(|item| {
                item.kind == TranscriptKind::User
                    && string_at(&item.raw, "/clientId") == Some(client_user_message_id)
            })
            || self.items.iter().rev().take(64).any(|item| {
                item.kind == TranscriptKind::User
                    && string_at(&item.raw, "/clientId").is_none()
                    && optimistic_user_content_matches(&item.content, &content)
            })
        {
            return None;
        }

        Some(self.push_local_user(client_user_message_id, content, content_blocks))
    }

    pub fn user_image_sources(&self, item_key: &str) -> &[UserImageSource] {
        self.user_image_sources
            .get(item_key)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn set_status_for_key(&mut self, key: &str, status: &str) -> Option<usize> {
        let index = self.item_indices.get(key).copied()?;
        self.items[index].status = Some(status.into());
        Some(index)
    }

    pub fn apply_batch(
        &mut self,
        events: Vec<Event>,
        selected_thread_id: Option<&str>,
    ) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();
        for event in events {
            self.apply_event(event, selected_thread_id, &mut outcome);
        }
        outcome
    }

    fn apply_event(
        &mut self,
        event: Event,
        selected_thread_id: Option<&str>,
        outcome: &mut BatchOutcome,
    ) {
        match event {
            Event::Notification { method, params } => {
                if !event_matches_thread(selected_thread_id, &params) {
                    return;
                }
                self.record_raw(method.clone(), params.clone());
                if self.telemetry.ingest(&method, &params) {
                    return;
                }
                match method.as_str() {
                    "item/started" | "item/completed" => {
                        if let Some(item) = params.get("item").cloned() {
                            let turn_id = string_at(&params, "/turnId");
                            if let Some(index) =
                                self.upsert_protocol_item(item, method == "item/completed", turn_id)
                            {
                                outcome.dirty.insert(index);
                            }
                        }
                    }
                    "item/agentMessage/delta"
                    | "item/plan/delta"
                    | "item/reasoning/textDelta"
                    | "item/reasoning/summaryTextDelta"
                    | "item/commandExecution/outputDelta"
                    | "item/fileChange/outputDelta" => {
                        let fallback_kind = match method.as_str() {
                            "item/agentMessage/delta" => TranscriptKind::Agent,
                            "item/plan/delta" => TranscriptKind::Plan,
                            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                                TranscriptKind::Reasoning
                            }
                            "item/commandExecution/outputDelta" => TranscriptKind::Command,
                            _ => TranscriptKind::FileChange,
                        };
                        let index = self.append_delta(&method, &params, fallback_kind);
                        outcome.dirty.insert(index);
                    }
                    "item/commandExecution/terminalInteraction" => {
                        let item_id = string_at(&params, "/itemId")
                            .unwrap_or("command")
                            .to_string();
                        let stdin = string_at(&params, "/stdin").unwrap_or_default().to_string();
                        let index = self.append_named_delta(
                            &item_id,
                            TranscriptKind::Command,
                            "Command",
                            &format!("\n› {stdin}"),
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "item/reasoning/summaryPartAdded" => {
                        let item_id = string_at(&params, "/itemId")
                            .unwrap_or("reasoning")
                            .to_string();
                        let index = self.append_named_delta(
                            &item_id,
                            TranscriptKind::Reasoning,
                            "Reasoning",
                            "",
                            params,
                        );
                        if !self.items[index].content.is_empty()
                            && !self.items[index].content.ends_with("\n\n")
                        {
                            self.items[index].content.push_str("\n\n");
                        }
                        outcome.dirty.insert(index);
                    }
                    "command/exec/outputDelta" | "process/outputDelta" => {
                        let process_id = string_at(&params, "/processId")
                            .or_else(|| string_at(&params, "/processHandle"))
                            .unwrap_or("process");
                        let stream = string_at(&params, "/stream").unwrap_or("output");
                        let delta = string_at(&params, "/deltaBase64")
                            .and_then(decode_base64)
                            .unwrap_or_else(|| "[binary output chunk]".into());
                        let index = self.append_named_delta(
                            &format!("{method}:{process_id}"),
                            TranscriptKind::Command,
                            &format!("Process · {process_id} · {stream}"),
                            &delta,
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "process/exited" => {
                        let process_id = string_at(&params, "/processHandle").unwrap_or("process");
                        let key = format!("process/outputDelta:{process_id}");
                        if let Some(index) = self.item_indices.get(&key).copied() {
                            let stdout = string_at(&params, "/stdout").unwrap_or_default();
                            let stderr = string_at(&params, "/stderr").unwrap_or_default();
                            if !stdout.is_empty() {
                                self.items[index].content.push_str(stdout);
                            }
                            if !stderr.is_empty() {
                                self.items[index].content.push_str(stderr);
                            }
                            let exit_code = params
                                .get("exitCode")
                                .and_then(Value::as_i64)
                                .unwrap_or_default();
                            self.items[index].status = Some(if exit_code == 0 {
                                "completed".into()
                            } else {
                                format!("exit {exit_code}")
                            });
                            self.items[index].raw = bounded_raw_payload(params);
                            self.items[index].event_count += 1;
                            outcome.dirty.insert(index);
                        }
                    }
                    // `rawResponseItem/completed` is the provider-facing record that backs the
                    // semantic `item/*` stream. In code mode it contains implementation wrappers
                    // such as `const r = await tools.exec_command(...)`; rendering it creates a
                    // second, noisier card next to the real `commandExecution`, `fileChange`, or
                    // tool item. `record_raw` above deliberately retains the exact payload for
                    // diagnostics, while the transcript treats the typed item stream as the sole
                    // presentation authority.
                    "rawResponseItem/completed" => {}
                    "thread/realtime/transcript/delta" => {
                        let thread_id = string_at(&params, "/threadId").unwrap_or("realtime");
                        let role = string_at(&params, "/role").unwrap_or("assistant");
                        let kind = if matches!(role, "user" | "human") {
                            TranscriptKind::User
                        } else {
                            TranscriptKind::Agent
                        };
                        let delta = string_at(&params, "/delta").unwrap_or_default().to_string();
                        let index = self.append_named_delta(
                            &format!("realtime:{thread_id}:{role}"),
                            kind,
                            &format!("Realtime · {role}"),
                            &delta,
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "serverRequest/resolved" => {
                        if let Some(request_id) = params.get("requestId") {
                            for (index, item) in self.items.iter_mut().enumerate() {
                                if item
                                    .pending_request
                                    .as_ref()
                                    .is_some_and(|request| request.id == *request_id)
                                {
                                    if let Some(request) = &mut item.pending_request {
                                        request.resolved = true;
                                    }
                                    item.status = Some("resolved".into());
                                    item.raw = bounded_raw_payload(json!({
                                        "request": item.raw.clone(),
                                        "resolution": params,
                                    }));
                                    outcome.dirty.insert(index);
                                    break;
                                }
                            }
                        }
                    }
                    "turn/diff/updated" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let index = self.upsert_generated(
                            format!("turn-diff:{turn_id}"),
                            TranscriptKind::Diff,
                            "Working tree diff",
                            string_at(&params, "/diff").unwrap_or_default().to_string(),
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "turn/plan/updated" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let content = render_plan(&params);
                        let index = self.upsert_generated(
                            format!("turn-plan:{turn_id}"),
                            TranscriptKind::Plan,
                            "Plan",
                            content,
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "turn/started" | "turn/completed" => {
                        let turn = params.get("turn").cloned().unwrap_or(Value::Null);
                        let turn_id = string_at(&turn, "/id").unwrap_or("unknown");
                        if method == "turn/started" {
                            self.current_turn_id = Some(turn_id.to_string());
                        } else {
                            if let Some(items) = turn.get("items").and_then(Value::as_array) {
                                for item in items {
                                    if let Some(index) =
                                        self.upsert_protocol_item(item.clone(), true, Some(turn_id))
                                    {
                                        outcome.dirty.insert(index);
                                    }
                                }
                            }

                            self.current_turn_id = None;
                            outcome.refresh_threads = true;

                            let status = string_at(&turn, "/status")
                                .unwrap_or("completed")
                                .to_string();
                            if matches!(status.as_str(), "failed" | "interrupted") {
                                let kind = if status == "failed" {
                                    TranscriptKind::Error
                                } else {
                                    TranscriptKind::Review
                                };
                                let index = self.upsert_generated(
                                    format!("turn-completion:{turn_id}"),
                                    kind,
                                    format!("Turn {}", protocol_status_text(&status)),
                                    render_turn_completion(&turn, &status),
                                    turn,
                                );
                                self.items[index].status = Some(protocol_status_text(&status));
                                outcome.dirty.insert(index);
                            }
                        }
                    }
                    "item/fileChange/patchUpdated" => {
                        let item_id = string_at(&params, "/itemId").unwrap_or("patch");
                        let content =
                            render_file_changes(params.get("changes").unwrap_or(&Value::Null));
                        let index = self.upsert_generated(
                            item_id.to_string(),
                            TranscriptKind::FileChange,
                            file_changes_title(params.get("changes").unwrap_or(&Value::Null)),
                            content,
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "item/mcpToolCall/progress" => {
                        let item_id = string_at(&params, "/itemId").unwrap_or("mcp").to_string();
                        let message = string_at(&params, "/message")
                            .map(|message| format!("{message}\n"))
                            .unwrap_or_default();
                        let index = self.append_named_delta(
                            &item_id,
                            TranscriptKind::Tool,
                            "MCP tool",
                            &message,
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "hook/started" | "hook/completed" => {
                        let run = params.get("run").unwrap_or(&Value::Null);
                        let hook_id = string_at(run, "/id").unwrap_or("hook").to_string();
                        let event_name = string_at(run, "/eventName")
                            .map(humanize_identifier)
                            .unwrap_or_else(|| "Hook".into());
                        let content = render_hook_run(run);
                        let status = string_at(run, "/status")
                            .map(protocol_status_text)
                            .unwrap_or_else(|| {
                                if method == "hook/completed" {
                                    "completed".into()
                                } else {
                                    "running".into()
                                }
                            });
                        let index = self.upsert_generated(
                            format!("hook:{hook_id}"),
                            TranscriptKind::Tool,
                            format!("Hook · {event_name}"),
                            content,
                            params,
                        );
                        self.items[index].status = Some(status);
                        outcome.dirty.insert(index);
                    }
                    "item/autoApprovalReview/started" | "item/autoApprovalReview/completed" => {
                        let review_id = string_at(&params, "/reviewId").unwrap_or("review");
                        let review_status = string_at(&params, "/review/status")
                            .map(protocol_status_text)
                            .unwrap_or_else(|| {
                                if method.ends_with("/completed") {
                                    "completed".into()
                                } else {
                                    "reviewing".into()
                                }
                            });
                        let target = string_at(&params, "/targetItemId")
                            .and_then(|id| self.item_indices.get(id).copied());
                        let index = if let Some(index) = target {
                            let item = &mut self.items[index];
                            item.status = Some(review_status);
                            item.raw = bounded_raw_payload(json!({
                                "item": item.raw.clone(),
                                "approvalReview": params,
                            }));
                            item.event_count += 1;
                            index
                        } else {
                            let index = self.upsert_generated(
                                format!("approval-review:{review_id}"),
                                TranscriptKind::Review,
                                approval_review_title(&params),
                                render_approval_review(&params),
                                params,
                            );
                            self.items[index].status = Some(review_status);
                            index
                        };
                        outcome.dirty.insert(index);
                    }
                    "model/rerouted" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let from = string_at(&params, "/fromModel").unwrap_or("model");
                        let to = string_at(&params, "/toModel").unwrap_or("model");
                        let reason = string_at(&params, "/reason")
                            .map(humanize_identifier)
                            .unwrap_or_else(|| "The requested model was unavailable".into());
                        let index = self.upsert_generated(
                            format!("model-reroute:{turn_id}"),
                            TranscriptKind::Review,
                            format!("Model rerouted · {from} → {to}"),
                            reason,
                            params,
                        );
                        self.items[index].status = Some("completed".into());
                        outcome.dirty.insert(index);
                    }
                    "model/verification" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let index = self.upsert_generated(
                            format!("model-verification:{turn_id}"),
                            TranscriptKind::Review,
                            "Model verification",
                            render_model_verifications(&params),
                            params,
                        );
                        self.items[index].status = Some("completed".into());
                        outcome.dirty.insert(index);
                    }
                    "warning" | "guardianWarning" => {
                        let message = string_at(&params, "/message")
                            .unwrap_or("Codex reported a warning")
                            .to_string();
                        let index = self.upsert_generated(
                            format!("{method}:{}", self.next_raw_sequence),
                            TranscriptKind::Error,
                            if method == "guardianWarning" {
                                "Safety warning"
                            } else {
                                "Codex warning"
                            },
                            message,
                            params,
                        );
                        self.items[index].status = Some("warning".into());
                        outcome.dirty.insert(index);
                    }
                    "configWarning" => {
                        outcome.transport_error = Some(render_config_warning(&params));
                    }
                    "thread/realtime/itemAdded" => {
                        if let Some(item) = params.get("item").cloned() {
                            if let Some(index) = self.upsert_protocol_item(item, true, None) {
                                outcome.dirty.insert(index);
                            }
                        }
                    }
                    "thread/realtime/transcript/done" => {
                        let thread_id = string_at(&params, "/threadId").unwrap_or("realtime");
                        let role = string_at(&params, "/role").unwrap_or("assistant");
                        let text = string_at(&params, "/text").unwrap_or_default();
                        let kind = if matches!(role, "user" | "human") {
                            TranscriptKind::User
                        } else {
                            TranscriptKind::Agent
                        };
                        let index = self.upsert_generated(
                            format!("realtime:{thread_id}:{role}"),
                            kind,
                            format!("Realtime · {role}"),
                            text.to_string(),
                            params,
                        );
                        self.items[index].status = Some("completed".into());
                        outcome.dirty.insert(index);
                    }
                    "thread/realtime/outputAudio/delta" => {
                        let thread_id = string_at(&params, "/threadId").unwrap_or("realtime");
                        let item_id = string_at(&params, "/audio/itemId").unwrap_or("output");
                        let key = format!("realtime-audio:{thread_id}:{item_id}");
                        let index = self.upsert_generated(
                            key,
                            TranscriptKind::Tool,
                            "Realtime audio",
                            String::new(),
                            params.clone(),
                        );
                        let chunks = self.items[index].event_count;
                        let sample_rate = params
                            .pointer("/audio/sampleRate")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        let channels = params
                            .pointer("/audio/numChannels")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        self.items[index].content = format!(
                            "Streaming PCM audio · {sample_rate} Hz · {channels} channel(s) · {chunks} chunk(s)"
                        );
                        self.items[index].status = Some("streaming".into());
                        outcome.dirty.insert(index);
                    }
                    "thread/realtime/error" => {
                        let thread_id = string_at(&params, "/threadId").unwrap_or("realtime");
                        let index = self.upsert_generated(
                            format!("realtime-error:{thread_id}"),
                            TranscriptKind::Error,
                            "Realtime error",
                            string_at(&params, "/message")
                                .unwrap_or("Realtime failed")
                                .into(),
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    "thread/name/updated" => {
                        outcome.renamed_thread = string_at(&params, "/threadName")
                            .or_else(|| string_at(&params, "/name"))
                            .map(ToOwned::to_owned);
                    }
                    "error" => {
                        let turn_id = string_at(&params, "/turnId").unwrap_or("unknown");
                        let index = self.upsert_generated(
                            format!("error:{turn_id}"),
                            TranscriptKind::Error,
                            "Codex error",
                            pretty_json(&params),
                            params,
                        );
                        outcome.dirty.insert(index);
                    }
                    _ => {}
                }
            }
            Event::ServerRequest { id, method, params } => {
                if !event_matches_thread(selected_thread_id, &params) {
                    return;
                }
                self.record_raw(method.clone(), params.clone());
                let key = format!("request:{}", compact_json(&id));
                let presentation = pending_request_presentation(&method, &params);
                let index = self.upsert_generated(
                    key,
                    TranscriptKind::Approval,
                    presentation.title,
                    presentation.content,
                    params,
                );
                self.items[index].status = Some("waiting".into());
                self.items[index].pending_request = Some(PendingRequest {
                    id,
                    method,
                    resolved: false,
                });
                outcome.dirty.insert(index);
            }
            Event::UnmatchedResponse { id, result, error } => {
                let payload =
                    json!({"id": id, "result": result, "error": error.map(|error| error.message)});
                self.record_raw("unmatchedResponse".into(), payload.clone());
                let index = self.upsert_generated(
                    format!("unmatched-response:{}", self.raw_events.len()),
                    TranscriptKind::Trace,
                    "Unmatched response",
                    pretty_json(&payload),
                    payload,
                );
                outcome.dirty.insert(index);
            }
            Event::Disconnected { reason } => {
                outcome.transport_error = Some(reason.clone());
                let payload = json!({"reason": reason});
                self.record_raw("disconnected".into(), payload.clone());
                let index = self.upsert_generated(
                    "app-server-disconnected".into(),
                    TranscriptKind::Error,
                    "App Server disconnected",
                    reason,
                    payload,
                );
                self.items[index].status = Some("offline".into());
                outcome.dirty.insert(index);
            }
        }
    }

    fn append_delta(&mut self, _method: &str, params: &Value, kind: TranscriptKind) -> usize {
        let item_id = string_at(params, "/itemId").unwrap_or("streaming-item");
        let index = if let Some(index) = self.item_indices.get(item_id).copied() {
            index
        } else {
            self.push_without_splice(TranscriptItem {
                key: item_id.to_string(),
                protocol_id: Some(item_id.to_string()),
                kind,
                title: title_for_kind(kind).into(),
                status: Some("streaming".into()),
                content: String::new(),
                raw: bounded_raw_payload(params.clone()),
                event_count: 0,
                expanded: kind != TranscriptKind::Trace,
                pending_request: None,
            })
        };
        let delta = string_at(params, "/delta").unwrap_or_default();
        let needs_command_separator = kind == TranscriptKind::Command
            && !delta.is_empty()
            && self.items[index]
                .command_transcript()
                .is_some_and(|command| command.output.is_empty())
            && !self.items[index].content.is_empty();
        if needs_command_separator {
            self.items[index].content.push_str("\n\n");
        }
        self.items[index].content.push_str(delta);
        if self.items[index].raw.get("type").is_none() {
            self.items[index].raw = bounded_raw_payload(params.clone());
        }
        self.items[index].event_count += 1;
        index
    }

    fn append_named_delta(
        &mut self,
        key: &str,
        kind: TranscriptKind,
        title: &str,
        delta: &str,
        raw: Value,
    ) -> usize {
        let index = if let Some(index) = self.item_indices.get(key).copied() {
            index
        } else {
            self.push_without_splice(TranscriptItem {
                key: key.to_string(),
                protocol_id: Some(key.to_string()),
                kind,
                title: title.into(),
                status: Some("streaming".into()),
                content: String::new(),
                raw: bounded_raw_payload(raw.clone()),
                event_count: 0,
                expanded: kind != TranscriptKind::Trace,
                pending_request: None,
            })
        };
        self.items[index].content.push_str(delta);
        self.items[index].raw = bounded_raw_payload(raw);
        self.items[index].event_count += 1;
        index
    }

    fn upsert_protocol_item(
        &mut self,
        value: Value,
        completed: bool,
        turn_id: Option<&str>,
    ) -> Option<usize> {
        let protocol_id = string_at(&value, "/id")
            .unwrap_or("unknown-item")
            .to_string();
        let incoming_user_image_sources = (string_at(&value, "/type") == Some("userMessage"))
            .then(|| user_image_sources_from_value(value.get("content").unwrap_or(&Value::Null)))
            .unwrap_or_default();
        let is_reasoning = string_at(&value, "/type") == Some("reasoning");
        if is_reasoning && let Some(turn_id) = turn_id {
            let aggregate_key = format!("turn-reasoning:{turn_id}");
            if let Some(index) = self.item_indices.get(&aggregate_key).copied() {
                let incoming = item_from_protocol(value, completed);
                let item = &mut self.items[index];
                item.content = merge_unique_reasoning(&item.content, &incoming.content);
                item.status = incoming.status;
                item.raw = incoming.raw;
                item.event_count += 1;
                self.item_indices.insert(protocol_id, index);
                return Some(index);
            }

            let mut item = item_from_protocol(value, completed);
            item.key = aggregate_key;
            let index = self.push_without_splice(item);
            self.item_indices.insert(protocol_id, index);
            return Some(index);
        }
        if let Some(index) = self.item_indices.get(&protocol_id).copied() {
            let old_events = self.items[index].event_count;
            let expanded = self.items[index].expanded;
            let pending_request = self.items[index].pending_request.clone();
            let mut item = item_from_protocol(value, completed);
            item.event_count = old_events + 1;
            item.expanded = expanded;
            item.pending_request = pending_request;
            self.items[index] = item;
            if self.items[index].kind == TranscriptKind::User {
                if incoming_user_image_sources.is_empty() {
                    self.user_image_sources.remove(&protocol_id);
                } else {
                    self.user_image_sources
                        .insert(protocol_id.clone(), incoming_user_image_sources);
                }
            }
            return Some(index);
        }

        let client_user_message_id = string_at(&value, "/clientId").map(ToOwned::to_owned);
        let item = item_from_protocol(value, completed);
        if item.kind == TranscriptKind::User
            && let Some(index) = client_user_message_id
                .as_deref()
                .and_then(|client_id| self.item_indices.get(&local_user_key(client_id)).copied())
                .or_else(|| {
                    self.items.iter().rposition(|candidate| {
                        candidate.key.starts_with("local-user:")
                            && optimistic_user_content_matches(&candidate.content, &item.content)
                    })
                })
        {
            let optimistic_key = self.items[index].key.clone();
            self.item_indices.remove(&optimistic_key);
            self.user_image_sources.remove(&optimistic_key);
            self.item_indices.insert(protocol_id, index);
            self.items[index] = item;
            if !incoming_user_image_sources.is_empty() {
                self.user_image_sources
                    .insert(self.items[index].key.clone(), incoming_user_image_sources);
            }
            return Some(index);
        }
        let item_key = item.key.clone();
        let index = self.push_without_splice(item);
        if !incoming_user_image_sources.is_empty() {
            self.user_image_sources
                .insert(item_key, incoming_user_image_sources);
        }
        Some(index)
    }

    fn upsert_generated(
        &mut self,
        key: String,
        kind: TranscriptKind,
        title: impl Into<String>,
        content: String,
        raw: Value,
    ) -> usize {
        if let Some(index) = self.item_indices.get(&key).copied() {
            let item = &mut self.items[index];
            item.kind = kind;
            item.title = title.into();
            item.content = content;
            item.raw = bounded_raw_payload(raw);
            item.event_count += 1;
            return index;
        }
        self.push_without_splice(TranscriptItem {
            key,
            protocol_id: None,
            kind,
            title: title.into(),
            status: None,
            content,
            raw: bounded_raw_payload(raw),
            event_count: 1,
            expanded: kind != TranscriptKind::Trace,
            pending_request: None,
        })
    }

    fn push_without_splice(&mut self, item: TranscriptItem) -> usize {
        let index = self.items.len();
        self.item_indices.insert(item.key.clone(), index);
        self.items.push(item);
        index
    }

    fn rebuild_item_indices(&mut self) {
        self.item_indices.clear();
        for (index, item) in self.items.iter().enumerate() {
            self.item_indices.insert(item.key.clone(), index);
            if let Some(protocol_id) = &item.protocol_id {
                self.item_indices.insert(protocol_id.clone(), index);
            }
        }
    }

    fn record_raw(&mut self, method: String, payload: Value) {
        if self.raw_events.len() >= RAW_EVENT_LIMIT {
            self.raw_events.drain(..RAW_EVENT_EVICTION_BATCH);
            self.dropped_raw_events += RAW_EVENT_EVICTION_BATCH;
        }
        self.next_raw_sequence += 1;
        self.raw_events.push(RawEvent {
            sequence: self.next_raw_sequence,
            method,
            payload: bounded_raw_payload(payload),
        });
    }
}

fn local_user_key(client_user_message_id: &str) -> String {
    format!("local-user:{client_user_message_id}")
}

fn optimistic_user_content_matches(local: &str, authoritative: &str) -> bool {
    fn normalized(content: &str) -> String {
        content
            .lines()
            .filter_map(|line| match line.trim() {
                "[Attached image]" | "Attached image" => None,
                "[Attached audio]" => Some("Attached audio"),
                _ => Some(line),
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    normalized(local) == normalized(authoritative)
}

fn bounded_raw_payload(mut payload: Value) -> Value {
    let original_size = serde_json::to_vec(&payload).map_or(0, |bytes| bytes.len());
    trim_large_json_values(&mut payload);
    let bounded_size = serde_json::to_vec(&payload).map_or(usize::MAX, |bytes| bytes.len());
    if bounded_size <= RAW_PAYLOAD_LIMIT {
        payload
    } else {
        json!({
            "rawPayload": "omitted because the diagnostic payload exceeded the in-memory limit",
            "originalBytes": original_size,
            "boundedBytes": bounded_size,
        })
    }
}

fn trim_large_json_values(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > RAW_STRING_LIMIT => {
            *value = Value::String(format!(
                "[{} bytes omitted from diagnostic payload]",
                text.len()
            ));
        }
        Value::Array(values) => {
            for value in values.iter_mut().take(RAW_ARRAY_LIMIT) {
                trim_large_json_values(value);
            }
            if values.len() > RAW_ARRAY_LIMIT {
                let omitted = values.len() - RAW_ARRAY_LIMIT;
                values.truncate(RAW_ARRAY_LIMIT);
                values.push(Value::String(format!(
                    "[{omitted} additional values omitted from diagnostic payload]"
                )));
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                trim_large_json_values(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn transcript_snapshot_path(thread_id: &str) -> anyhow::Result<PathBuf> {
    if thread_id.is_empty()
        || !thread_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!("invalid transcript thread id");
    }
    let state_root = std::env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join(".local/state"))
        })
        .ok_or_else(|| anyhow::anyhow!("neither XDG_STATE_HOME nor HOME is available"))?;
    Ok(state_root
        .join("harness")
        .join("transcripts")
        .join(format!("{thread_id}.json")))
}

fn read_persisted_transcript(thread_id: &str) -> anyhow::Result<Option<PersistedTranscript>> {
    let path = transcript_snapshot_path(thread_id)?;
    let serialized = match fs::read(path) {
        Ok(serialized) => serialized,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let snapshot: PersistedTranscript = serde_json::from_slice(&serialized)?;
    if snapshot.version != SNAPSHOT_VERSION || snapshot.thread_id != thread_id {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

fn item_for_snapshot(item: &TranscriptItem) -> TranscriptItem {
    let mut item = item.clone();
    if item.content.len() > SNAPSHOT_CONTENT_LIMIT {
        item.content = truncate_middle(&item.content, SNAPSHOT_CONTENT_LIMIT);
    }
    if serde_json::to_vec(&item.raw)
        .map(|raw| raw.len() > SNAPSHOT_RAW_LIMIT)
        .unwrap_or(true)
    {
        item.raw = json!({
            "snapshot": "Raw payload omitted from the local history cache because it exceeded 512 KiB. Live semantic content is preserved.",
            "protocolId": item.protocol_id,
        });
    }
    item
}

fn truncate_middle(value: &str, byte_limit: usize) -> String {
    if value.len() <= byte_limit {
        return value.to_string();
    }
    let marker = "\n\n… output truncated in local history cache …\n\n";
    let available = byte_limit.saturating_sub(marker.len());
    let mut head_end = available * 3 / 4;
    while head_end > 0 && !value.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = value.len().saturating_sub(available - head_end);
    while tail_start < value.len() && !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{}{}", &value[..head_end], marker, &value[tail_start..])
}

fn render_hook_run(run: &Value) -> String {
    let mut lines = Vec::new();
    let handler = string_at(run, "/handlerType").map(humanize_identifier);
    let mode = string_at(run, "/executionMode").map(humanize_identifier);
    if handler.is_some() || mode.is_some() {
        lines.push(
            [handler, mode]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    if let Some(path) = string_at(run, "/sourcePath") {
        lines.push(path.to_string());
    }
    if let Some(message) = string_at(run, "/statusMessage").filter(|text| !text.is_empty()) {
        lines.push(message.to_string());
    }
    if let Some(entries) = run.get("entries").and_then(Value::as_array) {
        lines.extend(entries.iter().filter_map(|entry| {
            let text = string_at(entry, "/text")?.trim();
            (!text.is_empty()).then(|| text.to_string())
        }));
    }
    if lines.is_empty() {
        "Hook activity".into()
    } else {
        lines.join("\n\n")
    }
}

fn approval_review_title(params: &Value) -> String {
    let action = string_at(params, "/action/type").unwrap_or("action");
    format!("Safety review · {}", humanize_identifier(action))
}

fn render_approval_review(params: &Value) -> String {
    let action = params.get("action").unwrap_or(&Value::Null);
    let mut lines = Vec::new();
    if let Some(command) = string_at(action, "/command") {
        lines.push(format!("$ {command}"));
    } else if let Some(tool) =
        string_at(action, "/toolTitle").or_else(|| string_at(action, "/toolName"))
    {
        lines.push(tool.to_string());
    } else if let Some(target) = string_at(action, "/target") {
        lines.push(target.to_string());
    } else if let Some(action_type) = string_at(action, "/type") {
        lines.push(humanize_identifier(action_type));
    }
    if let Some(rationale) = string_at(params, "/review/rationale").filter(|text| !text.is_empty())
    {
        lines.push(rationale.to_string());
    }
    if let Some(risk) = string_at(params, "/review/riskLevel") {
        lines.push(format!("Risk · {}", humanize_identifier(risk)));
    }
    if lines.is_empty() {
        "Codex reviewed an action before running it.".into()
    } else {
        lines.join("\n\n")
    }
}

struct PendingRequestPresentation {
    title: String,
    content: String,
}

fn pending_request_presentation(method: &str, params: &Value) -> PendingRequestPresentation {
    match method {
        "item/commandExecution/requestApproval" => render_command_approval(params, false),
        "item/fileChange/requestApproval" => render_file_change_approval(params, false),
        "item/permissions/requestApproval" => render_permissions_approval(params),
        "item/tool/requestUserInput" => render_user_input_request(params),
        "mcpServer/elicitation/request" => render_mcp_elicitation_request(params),
        "execCommandApproval" => render_command_approval(params, true),
        "applyPatchApproval" => render_file_change_approval(params, true),
        _ => PendingRequestPresentation {
            title: "Confirmation requested".into(),
            content:
                "Codex requested confirmation. The complete event is available in the raw journal."
                    .into(),
        },
    }
}

fn render_command_approval(params: &Value, legacy: bool) -> PendingRequestPresentation {
    let command = if legacy {
        params
            .get("command")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(safe_semantic_text)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    } else {
        string_at(params, "/command").and_then(safe_semantic_text)
    }
    .filter(|command| !command.is_empty());
    let mut sections = Vec::new();
    if let Some(command) = &command {
        sections.push(format!("$ {command}"));
    }
    push_labeled_text(
        &mut sections,
        "Working directory",
        string_at(params, "/cwd"),
    );
    push_labeled_text(&mut sections, "Reason", string_at(params, "/reason"));
    if !legacy && let Some(permissions) = params.get("additionalPermissions") {
        let permissions = permission_lines(permissions);
        if !permissions.is_empty() {
            sections.push(format!("Requested permissions\n{}", permissions.join("\n")));
        }
    }
    if sections.is_empty() {
        sections.push(
            "Codex wants to run a command, but no displayable command details were provided."
                .into(),
        );
    }

    let decisions = if legacy {
        legacy_approval_decisions()
    } else {
        command_approval_decision_labels(params)
    };
    if !decisions.is_empty() {
        sections.push(format!("Available decisions\n{}", bullet_lines(decisions)));
    }
    let title = command.as_deref().map_or_else(
        || "Command approval".into(),
        |command| format!("Command approval · {}", one_line(command, 72)),
    );
    PendingRequestPresentation {
        title,
        content: sections.join("\n\n"),
    }
}

fn render_file_change_approval(params: &Value, legacy: bool) -> PendingRequestPresentation {
    let mut sections = Vec::new();
    if legacy {
        let changes = legacy_file_change_lines(params.get("fileChanges"));
        if !changes.is_empty() {
            sections.push(format!("Requested file changes\n{}", changes.join("\n")));
        }
    }
    push_labeled_text(
        &mut sections,
        "Write scope",
        string_at(params, "/grantRoot"),
    );
    push_labeled_text(&mut sections, "Reason", string_at(params, "/reason"));
    sections.push(format!(
        "Available decisions\n{}",
        bullet_lines(if legacy {
            legacy_approval_decisions()
        } else {
            vec![
                "Allow once".into(),
                "Allow for this task".into(),
                "Deny".into(),
                "Deny and stop the turn".into(),
            ]
        })
    ));
    if sections.len() == 1 {
        sections.insert(
            0,
            "Codex wants to change files, but no displayable file scope was provided.".into(),
        );
    }
    PendingRequestPresentation {
        title: if legacy {
            "Patch approval".into()
        } else {
            "File changes approval".into()
        },
        content: sections.join("\n\n"),
    }
}

fn render_permissions_approval(params: &Value) -> PendingRequestPresentation {
    let mut sections = Vec::new();
    let permissions = params
        .get("permissions")
        .map(permission_lines)
        .unwrap_or_default();
    if permissions.is_empty() {
        sections.push(
            "Codex requested additional permissions, but no displayable permission subset was provided."
                .into(),
        );
    } else {
        sections.push(format!(
            "Requested permission subset\n{}",
            permissions.join("\n")
        ));
    }
    push_labeled_text(
        &mut sections,
        "Working directory",
        string_at(params, "/cwd"),
    );
    push_labeled_text(&mut sections, "Reason", string_at(params, "/reason"));
    sections.push("The response can grant exactly this subset for the turn, or deny it.".into());
    PendingRequestPresentation {
        title: "Permission approval".into(),
        content: sections.join("\n\n"),
    }
}

fn render_user_input_request(params: &Value) -> PendingRequestPresentation {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(render_user_input_question)
        .collect::<Vec<_>>();
    let detail = params
        .pointer("/questions/0/header")
        .and_then(Value::as_str)
        .and_then(safe_semantic_text);
    PendingRequestPresentation {
        title: detail.map_or_else(
            || "Input requested".into(),
            |detail| format!("Input requested · {}", one_line(&detail, 72)),
        ),
        content: if questions.is_empty() {
            "Codex requested input, but no displayable questions were provided.".into()
        } else {
            questions.join("\n\n")
        },
    }
}

fn render_user_input_question(question: &Value) -> Option<String> {
    let prompt = string_at(question, "/question").and_then(safe_semantic_text)?;
    let header = string_at(question, "/header").and_then(safe_semantic_text);
    let mut lines = vec![header.map_or(prompt.clone(), |header| format!("{header} — {prompt}"))];
    if let Some(options) = question.get("options").and_then(Value::as_array) {
        lines.extend(options.iter().filter_map(|option| {
            let label = string_at(option, "/label").and_then(safe_semantic_text)?;
            let description = string_at(option, "/description").and_then(safe_semantic_text);
            Some(description.map_or_else(
                || format!("- {label}"),
                |description| format!("- {label} — {description}"),
            ))
        }));
    }
    if question
        .get("isOther")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        lines.push("- A custom response is allowed".into());
    }
    if question
        .get("isSecret")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        lines.push("Your response will be hidden while you type.".into());
    }
    Some(lines.join("\n"))
}

fn render_mcp_elicitation_request(params: &Value) -> PendingRequestPresentation {
    let server = string_at(params, "/serverName").and_then(safe_semantic_text);
    let mut sections = Vec::new();
    if let Some(server) = &server {
        sections.push(format!("Server: {server}"));
    }
    if let Some(message) = string_at(params, "/message").and_then(safe_semantic_text) {
        sections.push(message);
    }
    match string_at(params, "/mode") {
        Some("form") => {
            let fields = render_mcp_form_fields(params.get("requestedSchema"));
            if fields.is_empty() {
                sections.push("The server requested a form, but no displayable fields were provided."
                    .into());
            } else {
                sections.push(format!("Requested fields\n{}", fields.join("\n")));
            }
        }
        Some("url") => {
            if let Some(url) = string_at(params, "/url").and_then(safe_http_url) {
                sections.push(format!("Open this link to continue:\n{url}"));
            } else {
                sections.push("The server requested a link that is not safe to display.".into());
            }
        }
        Some("openai/form") => sections.push(
            "The server requested an extended form. Its fields are available in the secure request UI."
                .into(),
        ),
        _ => sections.push("The server requested input in an unsupported format.".into()),
    }
    PendingRequestPresentation {
        title: server.map_or_else(
            || "MCP input requested".into(),
            |server| format!("MCP input requested · {}", one_line(&server, 64)),
        ),
        content: sections.join("\n\n"),
    }
}

fn render_mcp_form_fields(schema: Option<&Value>) -> Vec<String> {
    let Some(schema) = schema else {
        return Vec::new();
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| properties.iter())
        .filter_map(|(property_name, field)| {
            let is_required = required.contains(property_name.as_str());
            let name = safe_semantic_text(property_name)?;
            let title = string_at(field, "/title")
                .and_then(safe_semantic_text)
                .unwrap_or(name);
            let mut metadata = Vec::new();
            if is_required {
                metadata.push("required".into());
            }
            if let Some(kind) = string_at(field, "/type") {
                metadata.push(match kind {
                    "array" => "list".into(),
                    "boolean" => "true or false".into(),
                    other => humanize_identifier(other),
                });
            }
            let choices = mcp_schema_choices(field);
            if !choices.is_empty() {
                metadata.push(format!("choices: {}", choices.join(", ")));
            }
            if let Some(default) = field.get("default").and_then(display_primitive) {
                metadata.push(format!("default: {default}"));
            }
            let metadata = (!metadata.is_empty()).then(|| format!(" ({})", metadata.join(" · ")));
            let mut line = format!("- {title}{}", metadata.unwrap_or_default());
            if let Some(description) = string_at(field, "/description").and_then(safe_semantic_text)
            {
                line.push_str(" — ");
                line.push_str(&description);
            }
            Some(line)
        })
        .collect()
}

fn mcp_schema_choices(schema: &Value) -> Vec<String> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values.iter().filter_map(display_primitive).collect();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(values) = schema.get(key).and_then(Value::as_array) {
            let choices = values
                .iter()
                .filter_map(|choice| {
                    string_at(choice, "/title")
                        .and_then(safe_semantic_text)
                        .or_else(|| choice.get("const").and_then(display_primitive))
                })
                .collect::<Vec<_>>();
            if !choices.is_empty() {
                return choices;
            }
        }
    }
    if string_at(schema, "/type") == Some("array") {
        return schema
            .get("items")
            .map(mcp_schema_choices)
            .unwrap_or_default();
    }
    Vec::new()
}

fn command_approval_decision_labels(params: &Value) -> Vec<String> {
    match params.get("availableDecisions") {
        None | Some(Value::Null) => vec![
            "Allow once".into(),
            "Allow for this task".into(),
            "Deny".into(),
            "Deny and stop the turn".into(),
        ],
        Some(Value::Array(decisions)) => decisions
            .iter()
            .filter_map(command_approval_decision_label)
            .collect(),
        Some(_) => Vec::new(),
    }
}

fn command_approval_decision_label(decision: &Value) -> Option<String> {
    match decision.as_str() {
        Some("accept") => Some("Allow once".into()),
        Some("acceptForSession") => Some("Allow for this task".into()),
        Some("decline") => Some("Deny".into()),
        Some("cancel") => Some("Deny and stop the turn".into()),
        Some(_) => None,
        None if decision.get("acceptWithExecpolicyAmendment").is_some() => {
            Some("Allow and remember matching commands".into())
        }
        None if decision.get("applyNetworkPolicyAmendment").is_some() => {
            let amendment = decision.get("applyNetworkPolicyAmendment")?;
            let host =
                string_at(amendment, "/network_policy_amendment/host").and_then(safe_semantic_text);
            let action = string_at(amendment, "/network_policy_amendment/action")
                .map(humanize_identifier)
                .unwrap_or_else(|| "apply".into());
            Some(host.map_or_else(
                || "Apply the proposed network rule".into(),
                |host| format!("{action} and remember network access for {host}"),
            ))
        }
        None => None,
    }
}

fn legacy_approval_decisions() -> Vec<String> {
    vec![
        "Allow once".into(),
        "Allow for this task".into(),
        "Deny".into(),
        "Deny and stop the turn".into(),
    ]
}

fn legacy_file_change_lines(file_changes: Option<&Value>) -> Vec<String> {
    file_changes
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|changes| changes.iter())
        .filter_map(|(path, change)| {
            let path = safe_semantic_text(path)?;
            let action = string_at(change, "/type")
                .map(humanize_identifier)
                .unwrap_or_else(|| "change".into());
            let destination = string_at(change, "/move_path").and_then(safe_semantic_text);
            Some(destination.map_or_else(
                || format!("- {action}: {path}"),
                |destination| format!("- {action}: {path} → {destination}"),
            ))
        })
        .collect()
}

fn permission_lines(permissions: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(file_system) = permissions.get("fileSystem") {
        if let Some(entries) = file_system.get("entries").and_then(Value::as_array) {
            lines.extend(entries.iter().filter_map(|entry| {
                let access = string_at(entry, "/access")
                    .map(humanize_identifier)
                    .unwrap_or_else(|| "Access".into());
                permission_path(entry.get("path")).map(|path| format!("- {access}: {path}"))
            }));
        }
        for (key, label) in [("read", "Read"), ("write", "Write")] {
            if let Some(paths) = file_system.get(key).and_then(Value::as_array) {
                lines.extend(paths.iter().filter_map(|path| {
                    display_primitive(path).map(|path| format!("- {label}: {path}"))
                }));
            }
        }
    }
    if let Some(enabled) = permissions
        .pointer("/network/enabled")
        .and_then(Value::as_bool)
    {
        lines.push(if enabled {
            "- Network access".into()
        } else {
            "- Network access remains disabled".into()
        });
    }
    lines
}

fn permission_path(path: Option<&Value>) -> Option<String> {
    let path = path?;
    match string_at(path, "/type") {
        Some("path") => string_at(path, "/path").and_then(safe_semantic_text),
        Some("glob_pattern") => string_at(path, "/pattern")
            .and_then(safe_semantic_text)
            .map(|pattern| format!("glob {pattern}")),
        Some("special") => string_at(path, "/value")
            .and_then(safe_semantic_text)
            .map(|value| humanize_identifier(&value)),
        _ => path.as_str().and_then(safe_semantic_text),
    }
}

fn push_labeled_text(sections: &mut Vec<String>, label: &str, value: Option<&str>) {
    if let Some(value) = value.and_then(safe_semantic_text) {
        sections.push(format!("{label}: {value}"));
    }
}

fn bullet_lines(lines: Vec<String>) -> String {
    lines
        .into_iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn display_primitive(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => safe_semantic_text(value),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn safe_http_url(value: &str) -> Option<String> {
    let value = safe_semantic_text(value)?;
    let lower = value.to_ascii_lowercase();
    ((lower.starts_with("https://") || lower.starts_with("http://"))
        && !value.chars().any(char::is_control)
        && !value.chars().any(char::is_whitespace))
    .then_some(value)
}

fn safe_semantic_text(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.to_ascii_lowercase().contains(";base64,") {
        return Some("[encoded data omitted]".into());
    }
    let mut redacted = false;
    let words = value
        .split_whitespace()
        .map(|word| {
            if looks_like_encoded_payload(word) {
                redacted = true;
                "[encoded data omitted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>();
    let mut text = words.join(" ");
    if text.chars().count() > 8_192 {
        text = text.chars().take(8_192).collect::<String>();
        text.push('…');
    }
    if redacted && text == value {
        Some("[encoded data omitted]".into())
    } else {
        Some(text)
    }
}

fn looks_like_encoded_payload(value: &str) -> bool {
    let value = value.trim_matches(|character: char| {
        matches!(
            character,
            '\'' | '"' | '`' | ',' | ';' | '(' | ')' | '[' | ']'
        )
    });
    value.len() >= 128
        && value.len().is_multiple_of(4)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        && base64::engine::general_purpose::STANDARD
            .decode(value)
            .is_ok()
}

fn render_model_verifications(params: &Value) -> String {
    let Some(verifications) = params.get("verifications").and_then(Value::as_array) else {
        return "Model verification completed.".into();
    };
    if verifications.is_empty() {
        return "Model verification completed.".into();
    }
    verifications
        .iter()
        .map(|verification| match verification {
            Value::String(value) => humanize_identifier(value),
            other => pretty_json(other),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_config_warning(params: &Value) -> String {
    let mut warning = string_at(params, "/summary")
        .unwrap_or("Codex configuration warning")
        .to_string();
    if let Some(path) = string_at(params, "/path") {
        warning.push_str(" · ");
        warning.push_str(path);
    }
    if let Some(details) = string_at(params, "/details").filter(|text| !text.is_empty()) {
        warning.push_str(": ");
        warning.push_str(details);
    }
    warning
}

fn merge_snapshot_items(
    fresh: Vec<TranscriptItem>,
    persisted: Vec<TranscriptItem>,
) -> (Vec<TranscriptItem>, usize) {
    let fresh = deduplicate_transcript_items_keep_last(fresh);
    // Version-1 snapshots may contain provider-level tool wrappers that older
    // Harness builds promoted into transcript cards. They have no semantic
    // identity in `thread/read` and must not be resurrected beside their typed
    // command/file/tool counterparts.
    let persisted = deduplicate_transcript_items_keep_last(
        persisted
            .into_iter()
            .filter(|item| !originates_from_raw_response(item))
            .collect(),
    );
    let mut next_fresh = 0;
    let mut matches = vec![None; persisted.len()];
    for (persisted_index, persisted_item) in persisted.iter().enumerate() {
        let remaining = &fresh[next_fresh..];
        let relative_index = remaining
            .iter()
            .position(|fresh_item| fresh_item.key == persisted_item.key)
            .or_else(|| {
                remaining
                    .iter()
                    .position(|fresh_item| items_are_semantically_equal(persisted_item, fresh_item))
            });
        if let Some(relative_index) = relative_index {
            let fresh_index = next_fresh + relative_index;
            matches[persisted_index] = Some(fresh_index);
            next_fresh = fresh_index + 1;
        }
    }
    let Some(anchor) = matches.iter().rposition(Option::is_some) else {
        return (fresh, 0);
    };

    let mut next_unmerged_fresh = 0;
    let mut restored = 0;
    let mut merged = Vec::with_capacity(fresh.len() + anchor + 1);
    for (persisted_index, item) in persisted.into_iter().take(anchor + 1).enumerate() {
        if let Some(fresh_index) = matches[persisted_index] {
            merged.extend(fresh[next_unmerged_fresh..fresh_index].iter().cloned());
            merged.push(fresh[fresh_index].clone());
            next_unmerged_fresh = fresh_index + 1;
        } else {
            restored += 1;
            merged.push(item);
        }
    }
    merged.extend(fresh[next_unmerged_fresh..].iter().cloned());
    (merged, restored)
}

/// Repair snapshots written by older Harness builds that appended a second
/// copy of an already-loaded history. Transcript item keys are semantic
/// identities: retaining more than one makes lookup ambiguous and causes the
/// native editor to reject the entire navigation document. Keep the last
/// occurrence because it contains the newest streamed status/content while
/// preserving the relative order of every surviving item.
fn deduplicate_transcript_items_keep_last(items: Vec<TranscriptItem>) -> Vec<TranscriptItem> {
    let mut seen = HashSet::with_capacity(items.len());
    let mut unique = items
        .into_iter()
        .rev()
        .filter(|item| seen.insert(item.key.clone()))
        .collect::<Vec<_>>();
    unique.reverse();
    unique
}

fn originates_from_raw_response(item: &TranscriptItem) -> bool {
    let response_type = string_at(&item.raw, "/type")
        .or_else(|| string_at(&item.raw, "/call/type"))
        .or_else(|| string_at(&item.raw, "/output/type"));
    matches!(
        response_type,
        Some(
            "custom_tool_call"
                | "custom_tool_call_output"
                | "function_call"
                | "function_call_output"
                | "tool_search_call"
                | "tool_search_output"
        )
    )
}

fn items_are_semantically_equal(left: &TranscriptItem, right: &TranscriptItem) -> bool {
    left.kind == right.kind && left.content.trim() == right.content.trim()
}

fn thread_snapshot_items_equal(left: &TranscriptItem, right: &TranscriptItem) -> bool {
    left.key == right.key
        && left.protocol_id == right.protocol_id
        && left.kind == right.kind
        && left.title == right.title
        && left.status == right.status
        && left.content == right.content
        && left.raw == right.raw
        && match (&left.pending_request, &right.pending_request) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.id == right.id
                    && left.method == right.method
                    && left.resolved == right.resolved
            }
            _ => false,
        }
}

fn replay_templates() -> Vec<TranscriptItem> {
    let mut items = vec![
        replay_item(
            0,
            TranscriptKind::User,
            "You",
            "Build a standalone Codex client with a **real Vim composer**, a full-width transcript, and no IDE chrome.",
            json!({"id":"fixture-user","type":"userMessage","content":[{"type":"text","text":"Build a standalone Codex client"}]}),
        ),
        replay_item(
            1,
            TranscriptKind::Reasoning,
            "Reasoning",
            "The transcript should behave like a navigable document, not a pile of chat bubbles. Semantic blocks can stay visually rich while selection remains blockwise and keyboard-first.",
            json!({"id":"fixture-reasoning","type":"reasoning","summary":["Choose a document model"]}),
        ),
        replay_item(
            2,
            TranscriptKind::Plan,
            "Implementation plan",
            "- [x] Separate the app from the Zed shell\n- [x] Attach the real editor and Vim engine\n- [ ] Finish rich transcript renderers\n- [ ] Validate live streaming",
            json!({"turnId":"fixture-turn","plan":[{"step":"Separate the app","status":"completed"},{"step":"Finish renderers","status":"in_progress"}]}),
        ),
        replay_item(
            3,
            TranscriptKind::Tool,
            "App Server · thread/read",
            "{\n  \"threadId\": \"0198d69d-fixture\",\n  \"includeTurns\": true,\n  \"experimentalRawEvents\": true\n}",
            json!({"id":"fixture-tool","type":"mcpToolCall","server":"codex","tool":"thread/read","status":"completed"}),
        ),
        replay_item(
            4,
            TranscriptKind::Diff,
            "Working tree diff · 3 files",
            "diff --git a/crates/harness_app/src/main.rs b/crates/harness_app/src/main.rs\n@@ -83,7 +83,10 @@\n-    let transcript = DebugCards::new(events);\n+    let transcript = SemanticDocument::from_journal(events);\n+    transcript.enable_vim_navigation();\n+    transcript.enable_raw_inspection();\n \n     window.render(transcript);",
            json!({"threadId":"fixture-turn","diff":"@@ -83,7 +83,10 @@"}),
        ),
        replay_item(
            5,
            TranscriptKind::Command,
            "Command",
            "$ cargo test -p harness_app\n\nrunning 12 tests\ntest replay::ten_thousand_blocks ... ok\ntest navigation::visual_yank ... ok\ntest protocol::unknown_events_are_visible ... ok\n\ntest result: ok. 12 passed; 0 failed",
            json!({"id":"fixture-command","type":"commandExecution","command":"cargo test -p harness_app","cwd":"/home/smt/harness/app","status":"completed","exitCode":0}),
        ),
        replay_item(
            6,
            TranscriptKind::Image,
            "Viewed image · transcript-reference.png",
            "The image path is retained even when the source is no longer available.",
            json!({"id":"fixture-image","type":"imageView","path":"/tmp/harness/transcript-reference.png"}),
        ),
        replay_item(
            7,
            TranscriptKind::Subagent,
            "Subagent · protocol audit",
            "{\n  \"agent\": \"protocol-audit\",\n  \"status\": \"completed\",\n  \"eventsReviewed\": 184,\n  \"unknownEventsDropped\": 0\n}",
            json!({"id":"fixture-subagent","type":"collabAgentToolCall","tool":"spawnAgent","status":"completed"}),
        ),
        replay_item(
            8,
            TranscriptKind::Trace,
            "Context compacted",
            "Earlier context was summarized so the agent could keep working without interrupting the turn.",
            json!({"id":"fixture-compaction","type":"contextCompaction","status":"completed"}),
        ),
        replay_item(
            9,
            TranscriptKind::Agent,
            "Codex",
            "The standalone boundary is established. The remaining work is presentation and interaction—not extracting another application shell.\n\nPress `r` on any selected block to inspect its complete protocol payload.",
            json!({"id":"fixture-agent","type":"agentMessage","text":"The standalone boundary is established."}),
        ),
    ];
    let mut approval = replay_item(
        10,
        TranscriptKind::Approval,
        "Command approval · cargo test",
        "Codex wants to run `cargo test -p harness_app` in `/home/smt/harness/app`.",
        json!({"command":"cargo test -p harness_app","cwd":"/home/smt/harness/app"}),
    );
    approval.status = Some("waiting".into());
    approval.pending_request = Some(PendingRequest {
        id: json!(42),
        method: "item/commandExecution/requestApproval".into(),
        resolved: false,
    });
    items.push(approval);
    let mut user_input = replay_item(
        11,
        TranscriptKind::Approval,
        "Input requested · transcript density",
        "Codex needs a product decision before continuing.",
        json!({
            "threadId": "fixture-turn",
            "turnId": "fixture-turn",
            "itemId": "fixture-user-input",
            "isBlocking": true,
            "questions": [{
                "header": "Density",
                "id": "density",
                "question": "How should completed reasoning appear in the transcript?",
                "options": [
                    {"label": "Compact", "description": "Show the latest step and expand the full sequence on demand."},
                    {"label": "Expanded", "description": "Keep the complete reasoning summary visible in the timeline."}
                ]
            }]
        }),
    );
    user_input.status = Some("waiting".into());
    user_input.pending_request = Some(PendingRequest {
        id: json!(43),
        method: "item/tool/requestUserInput".into(),
        resolved: false,
    });
    items.push(user_input);
    items
}

fn replay_item(
    index: usize,
    kind: TranscriptKind,
    title: &str,
    content: &str,
    raw: Value,
) -> TranscriptItem {
    TranscriptItem {
        key: format!("template:{index}"),
        protocol_id: Some(format!("fixture-{index}")),
        kind,
        title: title.into(),
        status: Some("replay".into()),
        content: content.into(),
        raw: bounded_raw_payload(raw),
        event_count: 1,
        expanded: default_expanded(kind, true),
        pending_request: None,
    }
}

fn item_from_protocol(raw: Value, completed: bool) -> TranscriptItem {
    let protocol_id = string_at(&raw, "/id").unwrap_or("unknown-item").to_string();
    let protocol_kind = string_at(&raw, "/type").unwrap_or("unknown");
    let kind = kind_from_protocol(protocol_kind);
    TranscriptItem {
        key: protocol_id.clone(),
        protocol_id: Some(protocol_id),
        kind,
        title: title_from_protocol(protocol_kind, &raw),
        status: if protocol_kind == "contextCompaction" {
            // Compaction is a point-in-time history landmark. Treating an item
            // without an explicit status as an active operation leaves a
            // permanent, misleading `running` badge in restored histories.
            Some("completed".into())
        } else {
            raw.get("status")
                .and_then(protocol_status)
                .or_else(|| completed.then(|| "completed".into()))
                .or_else(|| Some("in progress".into()))
        },
        content: content_from_protocol(protocol_kind, &raw),
        raw: bounded_raw_payload(raw),
        event_count: 1,
        expanded: default_expanded(kind, completed),
        pending_request: None,
    }
}

fn event_matches_thread(selected_thread_id: Option<&str>, params: &Value) -> bool {
    match (selected_thread_id, string_at(params, "/threadId")) {
        (Some(selected), Some(event_thread)) => selected == event_thread,
        _ => true,
    }
}

fn kind_from_protocol(kind: &str) -> TranscriptKind {
    match kind {
        "userMessage" | "hookPrompt" => TranscriptKind::User,
        "agentMessage" => TranscriptKind::Agent,
        "reasoning" => TranscriptKind::Reasoning,
        "plan" => TranscriptKind::Plan,
        "commandExecution" | "sleep" => TranscriptKind::Command,
        "fileChange" => TranscriptKind::FileChange,
        "mcpToolCall" | "dynamicToolCall" => TranscriptKind::Tool,
        "collabAgentToolCall" | "subAgentActivity" => TranscriptKind::Subagent,
        "webSearch" => TranscriptKind::Web,
        "imageView" | "imageGeneration" => TranscriptKind::Image,
        "enteredReviewMode" | "exitedReviewMode" => TranscriptKind::Review,
        "contextCompaction" => TranscriptKind::Trace,
        _ => TranscriptKind::Trace,
    }
}

fn default_expanded(kind: TranscriptKind, _completed: bool) -> bool {
    match kind {
        TranscriptKind::User
        | TranscriptKind::Agent
        | TranscriptKind::Plan
        | TranscriptKind::FileChange
        | TranscriptKind::Diff
        | TranscriptKind::Image
        | TranscriptKind::Error
        | TranscriptKind::Approval
        | TranscriptKind::Command
        | TranscriptKind::Tool
        | TranscriptKind::Subagent
        | TranscriptKind::Web
        | TranscriptKind::Review
        | TranscriptKind::Reasoning => true,
        TranscriptKind::Trace => false,
    }
}

fn title_for_kind(kind: TranscriptKind) -> &'static str {
    match kind {
        TranscriptKind::User => "You",
        TranscriptKind::Agent => "Codex",
        TranscriptKind::Reasoning => "Reasoning",
        TranscriptKind::Plan => "Plan",
        TranscriptKind::Command => "Command",
        TranscriptKind::FileChange => "File changes",
        TranscriptKind::Tool => "Tool call",
        TranscriptKind::Diff => "Diff",
        TranscriptKind::Image => "Image",
        TranscriptKind::Subagent => "Subagent",
        TranscriptKind::Web => "Web search",
        TranscriptKind::Review => "Review",
        TranscriptKind::Trace => "Protocol event",
        TranscriptKind::Error => "Error",
        TranscriptKind::Approval => "Approval",
    }
}

fn title_from_protocol(kind: &str, raw: &Value) -> String {
    match kind {
        "userMessage" | "hookPrompt" => "You".into(),
        "agentMessage" => "Codex".into(),
        "reasoning" => "Reasoning".into(),
        "plan" => "Plan".into(),
        "commandExecution" => command_title(raw),
        "fileChange" => file_changes_title(raw.get("changes").unwrap_or(&Value::Null)),
        "mcpToolCall" => mcp_tool_title(raw),
        "dynamicToolCall" => humanize_identifier(string_at(raw, "/tool").unwrap_or("Dynamic tool")),
        "collabAgentToolCall" => format!(
            "Subagent · {}",
            humanize_identifier(string_at(raw, "/tool").unwrap_or("activity"))
        ),
        "subAgentActivity" => format!(
            "Subagent · {}",
            humanize_identifier(string_at(raw, "/kind").unwrap_or("activity"))
        ),
        "webSearch" => web_search_title(raw),
        "imageView" => media_title("Viewed image", string_at(raw, "/path")),
        "imageGeneration" => media_title("Generated image", string_at(raw, "/savedPath")),
        "sleep" => raw
            .get("durationMs")
            .and_then(Value::as_u64)
            .map(|duration| format!("Wait · {}", format_duration(duration)))
            .unwrap_or_else(|| "Wait".into()),
        "enteredReviewMode" => "Entered review".into(),
        "exitedReviewMode" => "Exited review".into(),
        "contextCompaction" => "Context compacted".into(),
        other => friendly_method(other),
    }
}

fn content_from_protocol(kind: &str, raw: &Value) -> String {
    match kind {
        "userMessage" => render_user_content(raw.get("content").unwrap_or(&Value::Null)),
        "hookPrompt" => render_hook_prompt(raw.get("fragments").unwrap_or(&Value::Null)),
        "agentMessage" | "plan" => string_at(raw, "/text").unwrap_or_default().to_string(),
        "reasoning" => {
            let mut sections = Vec::new();
            collect_text_sections(raw.get("summary"), &mut sections);
            collect_text_sections(raw.get("content"), &mut sections);
            unique_sections(sections).join("\n\n")
        }
        "commandExecution" => {
            let raw_command = string_at(raw, "/command").unwrap_or_default();
            let command = command_for_display(raw_command);
            let output = string_at(raw, "/aggregatedOutput").unwrap_or_default();
            if output.is_empty() {
                format!("$ {command}")
            } else {
                format!("$ {command}\n\n{output}")
            }
        }
        "fileChange" => render_file_changes(raw.get("changes").unwrap_or(&Value::Null)),
        "mcpToolCall" => render_mcp_tool_call(raw),
        "dynamicToolCall" => render_dynamic_tool_call(raw),
        "collabAgentToolCall" => render_collab_tool_call(raw),
        "subAgentActivity" => render_subagent_activity(raw),
        "webSearch" => render_web_search(raw),
        "imageView" => string_at(raw, "/path")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "Image path unavailable".into()),
        "imageGeneration" => string_at(raw, "/savedPath")
            .map(|path| {
                let prompt = string_at(raw, "/revisedPrompt").unwrap_or_default();
                if prompt.is_empty() {
                    path.to_string()
                } else {
                    format!("{path}\n\nRevised prompt\n{prompt}")
                }
            })
            .unwrap_or_else(|| "Generated image path unavailable".into()),
        "sleep" => raw
            .get("durationMs")
            .and_then(Value::as_u64)
            .map(|duration| format!("Waiting for {}", format_duration(duration)))
            .unwrap_or_else(|| "Waiting".into()),
        "enteredReviewMode" | "exitedReviewMode" => {
            string_at(raw, "/review").unwrap_or_default().to_string()
        }
        "contextCompaction" => {
            let mut summary = Vec::new();
            collect_text_sections(raw.get("summary"), &mut summary);
            collect_text_sections(raw.get("content"), &mut summary);
            let summary = unique_sections(summary).join("\n\n");
            summary
        }
        _ => pretty_json(raw),
    }
}

fn command_title(raw: &Value) -> String {
    let actions = raw.get("commandActions").and_then(Value::as_array);
    if let Some([action]) = actions.map(Vec::as_slice) {
        let title = match string_at(action, "/type") {
            Some("read") => string_at(action, "/path").map(|path| format!("Read · {path}")),
            Some("listFiles") => Some(match string_at(action, "/path") {
                Some(path) => format!("List files · {path}"),
                None => "List files".into(),
            }),
            Some("search") => {
                let query = string_at(action, "/query").unwrap_or("text");
                Some(match string_at(action, "/path") {
                    Some(path) => format!("Search · {query} · {path}"),
                    None => format!("Search · {query}"),
                })
            }
            _ => None,
        };
        if let Some(title) = title {
            return title;
        }
    }
    "Command".into()
}

fn mcp_tool_title(raw: &Value) -> String {
    match (
        string_at(raw, "/appContext/appName"),
        string_at(raw, "/appContext/actionName"),
    ) {
        (Some(app), Some(action)) => format!("{app} · {}", humanize_identifier(action)),
        (Some(app), None) => app.to_string(),
        _ => format!(
            "MCP · {} / {}",
            string_at(raw, "/server").unwrap_or("server"),
            string_at(raw, "/tool").unwrap_or("tool")
        ),
    }
}

fn media_title(prefix: &str, path: Option<&str>) -> String {
    path.and_then(|path| path.rsplit('/').find(|part| !part.is_empty()))
        .map(|name| format!("{prefix} · {name}"))
        .unwrap_or_else(|| prefix.into())
}

fn web_search_title(raw: &Value) -> String {
    let action = raw.get("action").unwrap_or(&Value::Null);
    match string_at(action, "/type") {
        Some("openPage" | "open_page") => format!(
            "Open page · {}",
            one_line(string_at(action, "/url").unwrap_or("page"), 96)
        ),
        Some("findInPage" | "find_in_page") => format!(
            "Find in page · {}",
            one_line(string_at(action, "/pattern").unwrap_or("text"), 72)
        ),
        _ => format!(
            "Web search · {}",
            one_line(string_at(raw, "/query").unwrap_or("search"), 96)
        ),
    }
}

fn render_user_content(content: &Value) -> String {
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    let image_count = blocks
        .iter()
        .filter(|block| matches!(string_at(block, "/type"), Some("localImage" | "image")))
        .count();
    let rendered = blocks
        .iter()
        .filter_map(|block| match string_at(block, "/type") {
            Some("text") => string_at(block, "/text").map(ToOwned::to_owned),
            // Images retain their semantic source separately so Rich mode can
            // paint the attachment itself. A fabricated text placeholder
            // would duplicate the visual and makes optimistic reconciliation
            // depend on presentation wording.
            Some("localImage") | Some("image") => None,
            Some("localAudio") | Some("audio") => Some("Attached audio".into()),
            Some("mention") | Some("skill") => {
                string_at(block, "/path").map(|path| format!("Mention: {path}"))
            }
            _ => Some(pretty_json(block)),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    strip_structured_image_labels(&rendered, image_count)
}

fn strip_structured_image_labels(content: &str, image_count: usize) -> String {
    if image_count == 0 {
        return content.to_owned();
    }

    content
        .lines()
        .filter_map(|line| {
            let leading_whitespace_len = line.len() - line.trim_start().len();
            let trimmed = &line[leading_whitespace_len..];
            let Some(suffix) = trimmed.strip_prefix("[Image #") else {
                return Some(line.to_owned());
            };
            let Some(marker_end) = suffix.find(']') else {
                return Some(line.to_owned());
            };
            let Ok(image_number) = suffix[..marker_end].parse::<usize>() else {
                return Some(line.to_owned());
            };
            if image_number == 0 || image_number > image_count {
                return Some(line.to_owned());
            }

            let remainder = suffix[marker_end + 1..].trim_start();
            (!remainder.is_empty()).then(|| {
                let mut visible = String::with_capacity(leading_whitespace_len + remainder.len());
                visible.push_str(&line[..leading_whitespace_len]);
                visible.push_str(remainder);
                visible
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_image_sources_from_value(content: &Value) -> Vec<UserImageSource> {
    content
        .as_array()
        .map(Vec::as_slice)
        .map(user_image_sources_from_blocks)
        .unwrap_or_default()
}

fn user_image_sources_from_blocks(content: &[Value]) -> Vec<UserImageSource> {
    content
        .iter()
        .filter_map(|block| match string_at(block, "/type") {
            Some("image") => {
                string_at(block, "/url").map(|url| UserImageSource::Url(url.to_owned()))
            }
            Some("localImage") => {
                string_at(block, "/path").map(|path| UserImageSource::LocalPath(path.to_owned()))
            }
            _ => None,
        })
        .collect()
}

fn render_hook_prompt(fragments: &Value) -> String {
    let Some(fragments) = fragments.as_array() else {
        return "Hook prompt unavailable".into();
    };
    fragments
        .iter()
        .filter_map(|fragment| string_at(fragment, "/text"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_mcp_tool_call(raw: &Value) -> String {
    let mut sections = Vec::new();
    push_json_section(&mut sections, "Arguments", raw.get("arguments"));

    if let Some(error) = string_at(raw, "/error/message") {
        sections.push(format!("Error\n{error}"));
    }
    if let Some(result) = raw.get("result").filter(|result| !result.is_null()) {
        let content = render_content_blocks(result.get("content"));
        if !content.is_empty() {
            sections.push(format!("Result\n{content}"));
        }
        push_json_section(
            &mut sections,
            "Structured result",
            result.get("structuredContent"),
        );
    }

    if sections.is_empty() {
        "Waiting for tool output…".into()
    } else {
        sections.join("\n\n")
    }
}

fn render_dynamic_tool_call(raw: &Value) -> String {
    let mut sections = Vec::new();
    push_json_section(&mut sections, "Arguments", raw.get("arguments"));
    if let Some(items) = raw.get("contentItems").and_then(Value::as_array) {
        let output = items
            .iter()
            .filter_map(|item| match string_at(item, "/type") {
                Some("inputText") => string_at(item, "/text").map(ToOwned::to_owned),
                Some("inputImage") => string_at(item, "/imageUrl")
                    .map(|url| format!("Image · {}", summarize_data_url(url))),
                Some("inputAudio") => string_at(item, "/audioUrl")
                    .map(|url| format!("Audio · {}", summarize_data_url(url))),
                _ => Some(pretty_json(item)),
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !output.is_empty() {
            sections.push(format!("Result\n{output}"));
        }
    }
    if sections.is_empty() {
        "Waiting for tool output…".into()
    } else {
        sections.join("\n\n")
    }
}

fn render_collab_tool_call(raw: &Value) -> String {
    let mut sections = Vec::new();
    if let Some(prompt) = string_at(raw, "/prompt").filter(|prompt| !prompt.is_empty()) {
        sections.push(format!("Prompt\n{prompt}"));
    }
    if let Some(states) = raw.get("agentsStates").and_then(Value::as_object) {
        let agents = states
            .iter()
            .map(|(thread_id, state)| {
                let status = string_at(state, "/status")
                    .map(humanize_identifier)
                    .unwrap_or_else(|| "Unknown".into());
                match string_at(state, "/message").filter(|message| !message.is_empty()) {
                    Some(message) => format!("{thread_id} · {status}\n{message}"),
                    None => format!("{thread_id} · {status}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !agents.is_empty() {
            sections.push(format!("Agents\n{agents}"));
        }
    } else if let Some(thread_ids) = raw.get("receiverThreadIds").and_then(Value::as_array) {
        let thread_ids = thread_ids
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if !thread_ids.is_empty() {
            sections.push(format!("Agents\n{thread_ids}"));
        }
    }
    if sections.is_empty() {
        "Subagent state unavailable".into()
    } else {
        sections.join("\n\n")
    }
}

fn render_subagent_activity(raw: &Value) -> String {
    let path = string_at(raw, "/agentPath").unwrap_or("subagent");
    let thread = string_at(raw, "/agentThreadId");
    match thread {
        Some(thread) => format!("{path}\n{thread}"),
        None => path.into(),
    }
}

fn render_web_search(raw: &Value) -> String {
    let mut sections = Vec::new();
    let action = raw.get("action").unwrap_or(&Value::Null);
    match string_at(action, "/type") {
        Some("openPage" | "open_page") => {
            if let Some(url) = string_at(action, "/url") {
                sections.push(format!("Page\n{url}"));
            }
        }
        Some("findInPage" | "find_in_page") => {
            let url = string_at(action, "/url").unwrap_or_default();
            let pattern = string_at(action, "/pattern").unwrap_or_default();
            sections.push(format!("Find\n{pattern}\n{url}"));
        }
        Some("search") | None => {
            let queries = action
                .get("queries")
                .and_then(Value::as_array)
                .map(|queries| {
                    queries
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|queries| !queries.is_empty())
                .or_else(|| string_at(action, "/query").map(ToOwned::to_owned))
                .or_else(|| string_at(raw, "/query").map(ToOwned::to_owned));
            if let Some(queries) = queries {
                sections.push(format!("Query\n{queries}"));
            }
        }
        Some(_) => {}
    }

    if let Some(results) = raw.get("results").and_then(Value::as_array) {
        let results = results
            .iter()
            .enumerate()
            .map(|(index, result)| render_web_result(index + 1, result))
            .collect::<Vec<_>>()
            .join("\n\n");
        if !results.is_empty() {
            sections.push(format!("Results\n{results}"));
        }
    }
    if sections.is_empty() {
        "Search details unavailable".into()
    } else {
        sections.join("\n\n")
    }
}

fn render_web_result(index: usize, result: &Value) -> String {
    if let Some(text) = result.as_str() {
        return format!("{index}. {text}");
    }
    let title = string_at(result, "/title")
        .or_else(|| string_at(result, "/name"))
        .or_else(|| string_at(result, "/text"));
    let url = string_at(result, "/url").or_else(|| string_at(result, "/link"));
    let snippet = string_at(result, "/snippet")
        .or_else(|| string_at(result, "/description"))
        .or_else(|| string_at(result, "/content"));
    if title.is_none() && url.is_none() && snippet.is_none() {
        return format!("{index}. {}", pretty_json(result));
    }
    let mut lines = vec![format!(
        "{index}. {}",
        title
            .map(|title| one_line(title, 140))
            .unwrap_or_else(|| "Result".into())
    )];
    if let Some(url) = url {
        lines.push(url.to_string());
    }
    if let Some(snippet) = snippet {
        lines.push(one_line(snippet, 320));
    }
    lines.join("\n")
}

fn render_content_blocks(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|block| match block {
            Value::String(text) => text.clone(),
            Value::Object(_) => match string_at(block, "/type") {
                Some("text" | "inputText" | "outputText" | "input_text" | "output_text") => {
                    string_at(block, "/text").unwrap_or_default().to_string()
                }
                Some("image" | "inputImage" | "input_image") => format!(
                    "Image · {}",
                    string_at(block, "/mimeType")
                        .or_else(|| string_at(block, "/imageUrl"))
                        .or_else(|| string_at(block, "/image_url"))
                        .map(summarize_data_url)
                        .unwrap_or("content".into())
                ),
                Some("audio" | "inputAudio" | "input_audio") => format!(
                    "Audio · {}",
                    string_at(block, "/mimeType")
                        .or_else(|| string_at(block, "/audioUrl"))
                        .or_else(|| string_at(block, "/audio_url"))
                        .map(summarize_data_url)
                        .unwrap_or("content".into())
                ),
                Some("encrypted_content") => "Encrypted tool output".into(),
                Some("resource_link") => {
                    let name = string_at(block, "/title")
                        .or_else(|| string_at(block, "/name"))
                        .unwrap_or("Resource");
                    let uri = string_at(block, "/uri").unwrap_or_default();
                    format!("{name}\n{uri}")
                }
                Some("resource") => {
                    let uri = string_at(block, "/resource/uri").unwrap_or("Resource");
                    match string_at(block, "/resource/text") {
                        Some(text) => format!("{uri}\n{text}"),
                        None => uri.into(),
                    }
                }
                _ => pretty_json(block),
            },
            _ => pretty_json(block),
        })
        .map(|section| section.trim_end_matches('\n').to_string())
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn push_json_section(sections: &mut Vec<String>, title: &str, value: Option<&Value>) {
    if let Some(value) = value.filter(|value| !value.is_null()) {
        let empty = value.as_object().is_some_and(serde_json::Map::is_empty)
            || value.as_array().is_some_and(Vec::is_empty);
        if !empty {
            sections.push(format!("{title}\n{}", pretty_json(value)));
        }
    }
}

fn summarize_data_url(value: &str) -> String {
    if let Some(header) = value
        .strip_prefix("data:")
        .and_then(|value| value.split(';').next())
    {
        return header.into();
    }
    one_line(value, 160)
}

fn render_file_changes(changes: &Value) -> String {
    let Some(changes) = changes.as_array() else {
        return pretty_json(changes);
    };
    changes
        .iter()
        .map(|change| {
            let path = string_at(change, "/path").unwrap_or("unknown");
            let kind = file_change_kind(change.get("kind"));
            let diff = string_at(change, "/diff").unwrap_or_default();
            let move_path = change
                .pointer("/kind/move_path")
                .and_then(Value::as_str)
                .filter(|move_path| !move_path.is_empty());
            let description = move_path
                .map(|move_path| format!("Moved · {path} → {move_path}"))
                .unwrap_or_else(|| format!("{kind} · {path}"));
            if diff.is_empty() {
                description
            } else {
                format!("{description}\n{diff}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn file_changes_title(changes: &Value) -> String {
    match changes.as_array().map(Vec::len) {
        Some(1) => "File change · 1 file".into(),
        Some(count) => format!("File changes · {count} files"),
        None => "File changes".into(),
    }
}

fn file_change_kind(kind: Option<&Value>) -> String {
    let kind = kind
        .and_then(|kind| {
            kind.as_str()
                .or_else(|| string_at(kind, "/type"))
                .or_else(|| string_at(kind, "/kind"))
        })
        .unwrap_or("update");

    match kind.to_ascii_lowercase().as_str() {
        "add" | "added" | "create" | "created" => "Added".into(),
        "delete" | "deleted" | "remove" | "removed" => "Deleted".into(),
        "move" | "moved" | "rename" | "renamed" => "Moved".into(),
        "update" | "updated" | "modify" | "modified" => "Modified".into(),
        _ => friendly_method(kind),
    }
}

fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms}ms")
    } else {
        let seconds = duration_ms as f64 / 1_000.;
        if seconds < 10. {
            format!("{seconds:.1}s")
        } else {
            format!("{seconds:.0}s")
        }
    }
}

fn humanize_identifier(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 4);
    let mut previous_was_lowercase = false;
    for character in value.chars() {
        if matches!(character, '_' | '-') {
            if !output.ends_with(' ') {
                output.push(' ');
            }
            previous_was_lowercase = false;
            continue;
        }
        if character.is_uppercase() && previous_was_lowercase {
            output.push(' ');
        }
        output.push(character);
        previous_was_lowercase = character.is_lowercase() || character.is_ascii_digit();
    }
    let mut characters = output.trim().chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_default()
}

fn render_plan(params: &Value) -> String {
    let mut output = string_at(params, "/explanation")
        .filter(|text| !text.is_empty())
        .map(|text| format!("{text}\n\n"))
        .unwrap_or_default();
    if let Some(steps) = params.get("plan").and_then(Value::as_array) {
        for step in steps {
            let status = string_at(step, "/status").unwrap_or("pending");
            let text = string_at(step, "/step").unwrap_or_default();
            // Status is structural protocol data, not prose. Markdown's task
            // marker is the visual status indicator, so repeating the raw
            // enum after the step produces a second, contradictory UI.
            let marker = match status {
                "completed" | "complete" => "x",
                "inProgress" | "in_progress" | "running" => "~",
                _ => " ",
            };
            output.push_str(&format!("- [{marker}] {text}\n"));
        }
    }
    output
}

fn render_turn_completion(turn: &Value, status: &str) -> String {
    let mut sections = Vec::new();
    if let Some(message) = string_at(turn, "/error/message").filter(|message| !message.is_empty()) {
        sections.push(message.to_string());
    }
    if let Some(details) =
        string_at(turn, "/error/additionalDetails").filter(|details| !details.is_empty())
    {
        sections.push(details.to_string());
    }
    if let Some(info) = turn
        .pointer("/error/codexErrorInfo")
        .filter(|info| !info.is_null())
    {
        sections.push(format!("Error details\n{}", pretty_json(info)));
    }
    if sections.is_empty() {
        sections.push(if status == "interrupted" {
            "The turn was interrupted before it completed.".into()
        } else {
            "The turn failed without an error message.".into()
        });
    }
    sections.join("\n\n")
}

fn collect_text_sections(value: Option<&Value>, sections: &mut Vec<String>) {
    let Some(value) = value else {
        return;
    };
    match value {
        Value::String(text) => sections.extend(
            text.split("\n\n")
                .flat_map(str::lines)
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned),
        ),
        Value::Array(values) => {
            for value in values {
                collect_text_sections(Some(value), sections);
            }
        }
        Value::Object(object) => {
            collect_text_sections(object.get("text"), sections);
            collect_text_sections(object.get("content"), sections);
            collect_text_sections(object.get("summary"), sections);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn unique_sections(sections: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    sections
        .into_iter()
        .filter(|section| seen.insert(section.clone()))
        .collect()
}

fn merge_unique_reasoning(existing: &str, incoming: &str) -> String {
    let mut sections = Vec::new();
    collect_text_sections(Some(&Value::String(existing.to_string())), &mut sections);
    collect_text_sections(Some(&Value::String(incoming.to_string())), &mut sections);
    unique_sections(sections).join("\n\n")
}

fn string_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn decode_base64(value: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn compact_json(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

fn protocol_status(value: &Value) -> Option<String> {
    match value {
        Value::String(status) => {
            let status = protocol_status_text(status);
            (!status.is_empty()).then_some(status)
        }
        Value::Object(object) => ["type", "status", "state"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(protocol_status)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => None,
    }
}

fn protocol_status_text(value: &str) -> String {
    humanize_identifier(value).to_lowercase()
}

fn one_line(value: &str, limit: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= limit {
        value
    } else {
        value.chars().take(limit).collect::<String>() + "…"
    }
}

fn friendly_method(method: &str) -> String {
    method
        .split(['/', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic_server_request(method: &str, params: Value) -> (TranscriptItem, String) {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![Event::ServerRequest {
                id: json!("rpc-request-id"),
                method: method.into(),
                params: params.clone(),
            }],
            None,
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.raw_events.len(), 1);
        assert_eq!(model.raw_events[0].payload, params);
        assert_eq!(model.items[0].raw, params);
        assert_eq!(model.items[0].status.as_deref(), Some("waiting"));
        let pending = model.items[0]
            .pending_request
            .as_ref()
            .expect("server request should retain response routing");
        assert_eq!(pending.id, json!("rpc-request-id"));
        assert_eq!(pending.method, method);
        assert!(!pending.resolved);
        let projection = model
            .item_projection(0)
            .expect("approval should have a selectable projection")
            .text;
        (model.items[0].clone(), projection)
    }

    fn assert_no_protocol_plumbing(text: &str, forbidden_values: &[&str]) {
        for wrapper in [
            "threadId",
            "turnId",
            "itemId",
            "approvalId",
            "callId",
            "conversationId",
            "startedAtMs",
            "_meta",
            "rpc-request-id",
        ] {
            assert!(!text.contains(wrapper), "leaked wrapper field {wrapper}");
        }
        for value in forbidden_values {
            assert!(!text.contains(value), "leaked opaque identifier {value}");
        }
        assert!(!text.contains("{\n"), "raw JSON leaked into projection");
        assert!(
            !text.contains("\": "),
            "raw JSON fields leaked into projection"
        );
    }

    #[test]
    fn command_approval_projects_command_context_permissions_and_exact_decisions() {
        let (_, text) = semantic_server_request(
            "item/commandExecution/requestApproval",
            json!({
                "threadId": "thread-secret",
                "turnId": "turn-secret",
                "itemId": "item-secret",
                "approvalId": "approval-secret",
                "startedAtMs": 123,
                "command": "cargo test -p harness_protocol",
                "cwd": "/home/smt/harness/app",
                "reason": "Verify semantic request projections",
                "additionalPermissions": {
                    "fileSystem": {"entries": [{
                        "access": "write",
                        "path": {"type": "path", "path": "/var/cache/harness"}
                    }]},
                    "network": {"enabled": true}
                },
                "availableDecisions": [
                    "accept",
                    {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["cargo", "test"]}},
                    {"applyNetworkPolicyAmendment": {"network_policy_amendment": {
                        "host": "crates.io", "action": "allow"
                    }}},
                    "decline",
                    "cancel"
                ]
            }),
        );

        assert!(text.contains("$ cargo test -p harness_protocol"));
        assert!(text.contains("Working directory: /home/smt/harness/app"));
        assert!(text.contains("Reason: Verify semantic request projections"));
        assert!(text.contains("Write: /var/cache/harness"));
        assert!(text.contains("Network access"));
        assert!(text.contains("Allow once"));
        assert!(text.contains("Allow and remember matching commands"));
        assert!(text.contains("Allow and remember network access for crates.io"));
        assert!(text.contains("Deny and stop the turn"));
        assert_no_protocol_plumbing(
            &text,
            &[
                "thread-secret",
                "turn-secret",
                "item-secret",
                "approval-secret",
            ],
        );
    }

    #[test]
    fn file_change_approval_projects_scope_reason_and_decisions() {
        let (_, text) = semantic_server_request(
            "item/fileChange/requestApproval",
            json!({
                "threadId": "thread-secret",
                "turnId": "turn-secret",
                "itemId": "item-secret",
                "startedAtMs": 123,
                "grantRoot": "/home/smt/harness",
                "reason": "Update the standalone client"
            }),
        );

        assert!(text.contains("Write scope: /home/smt/harness"));
        assert!(text.contains("Reason: Update the standalone client"));
        assert!(text.contains("Allow for this task"));
        assert!(text.contains("Deny and stop the turn"));
        assert_no_protocol_plumbing(&text, &["thread-secret", "turn-secret", "item-secret"]);
    }

    #[test]
    fn permissions_approval_projects_only_the_requested_subsets() {
        let (_, text) = semantic_server_request(
            "item/permissions/requestApproval",
            json!({
                "threadId": "thread-secret",
                "turnId": "turn-secret",
                "itemId": "item-secret",
                "startedAtMs": 123,
                "cwd": "/home/smt/harness",
                "reason": "Read fixtures and write generated snapshots",
                "permissions": {
                    "fileSystem": {
                        "entries": [
                            {"access": "read", "path": {"type": "glob_pattern", "pattern": "fixtures/**"}},
                            {"access": "write", "path": {"type": "path", "path": "/tmp/harness"}}
                        ],
                        "read": ["/etc/os-release"]
                    },
                    "network": {"enabled": false}
                }
            }),
        );

        assert!(text.contains("Read: glob fixtures/**"));
        assert!(text.contains("Write: /tmp/harness"));
        assert!(text.contains("Read: /etc/os-release"));
        assert!(text.contains("Network access remains disabled"));
        assert!(text.contains("grant exactly this subset"));
        assert_no_protocol_plumbing(&text, &["thread-secret", "turn-secret", "item-secret"]);
    }

    #[test]
    fn request_user_input_projects_questions_options_and_descriptions_without_ids() {
        let (_, text) = semantic_server_request(
            "item/tool/requestUserInput",
            json!({
                "threadId": "thread-secret",
                "turnId": "turn-secret",
                "itemId": "item-secret",
                "isBlocking": true,
                "questions": [{
                    "id": "density-internal-id",
                    "header": "Density",
                    "question": "How much live activity should remain visible?",
                    "options": [
                        {"label": "Everything", "description": "Show every semantic event."},
                        {"label": "Compact", "description": "Fold completed detail."}
                    ],
                    "isOther": true,
                    "isSecret": true
                }]
            }),
        );

        assert!(text.contains("Density — How much live activity should remain visible?"));
        assert!(text.contains("Everything — Show every semantic event."));
        assert!(text.contains("Compact — Fold completed detail."));
        assert!(text.contains("A custom response is allowed"));
        assert!(text.contains("response will be hidden"));
        assert_no_protocol_plumbing(
            &text,
            &[
                "thread-secret",
                "turn-secret",
                "item-secret",
                "density-internal-id",
            ],
        );
    }

    #[test]
    fn mcp_elicitation_projects_form_fields_and_only_safe_urls() {
        let (_, form) = semantic_server_request(
            "mcpServer/elicitation/request",
            json!({
                "threadId": "thread-secret",
                "turnId": "turn-secret",
                "serverName": "deployment-tools",
                "mode": "form",
                "message": "Choose a deployment target.",
                "_meta": {"opaque": "meta-secret"},
                "requestedSchema": {
                    "type": "object",
                    "required": ["region"],
                    "properties": {
                        "region": {
                            "type": "string",
                            "title": "Region",
                            "description": "Where the service should run.",
                            "enum": ["us-west", "eu-central"],
                            "default": "us-west"
                        },
                        "replicas": {"type": "number", "description": "Desired replica count."}
                    }
                }
            }),
        );
        assert!(form.contains("MCP input requested · deployment-tools"));
        assert!(form.contains("Choose a deployment target."));
        assert!(form.contains(
            "Region (required · String · choices: us-west, eu-central · default: us-west)"
        ));
        assert!(form.contains("Where the service should run."));
        assert!(form.contains("replicas (Number) — Desired replica count."));
        assert_no_protocol_plumbing(&form, &["thread-secret", "turn-secret", "meta-secret"]);

        let (_, safe_url) = semantic_server_request(
            "mcpServer/elicitation/request",
            json!({
                "threadId": "thread-secret",
                "serverName": "account-linker",
                "mode": "url",
                "message": "Link your account.",
                "elicitationId": "elicitation-secret",
                "url": "https://example.com/connect?scope=read"
            }),
        );
        assert!(safe_url.contains("https://example.com/connect?scope=read"));
        assert_no_protocol_plumbing(&safe_url, &["thread-secret", "elicitation-secret"]);

        let (_, unsafe_url) = semantic_server_request(
            "mcpServer/elicitation/request",
            json!({
                "threadId": "thread-secret",
                "serverName": "account-linker",
                "mode": "url",
                "message": "Link your account.",
                "elicitationId": "elicitation-secret",
                "url": "javascript:alert('url-secret')"
            }),
        );
        assert!(unsafe_url.contains("not safe to display"));
        assert!(!unsafe_url.contains("javascript:"));
        assert!(!unsafe_url.contains("url-secret"));
    }

    #[test]
    fn legacy_patch_approval_projects_file_actions_without_diff_payloads() {
        let (_, text) = semantic_server_request(
            "applyPatchApproval",
            json!({
                "conversationId": "conversation-secret",
                "callId": "call-secret",
                "grantRoot": "/home/smt/harness",
                "reason": "Apply the reviewed refactor",
                "fileChanges": {
                    "crates/a.rs": {"type": "add", "content": "content-secret"},
                    "crates/b.rs": {"type": "update", "move_path": "crates/c.rs", "unified_diff": "diff-secret"},
                    "crates/old.rs": {"type": "delete", "content": "deleted-secret"}
                }
            }),
        );

        assert!(text.contains("Add: crates/a.rs"));
        assert!(text.contains("Update: crates/b.rs → crates/c.rs"));
        assert!(text.contains("Delete: crates/old.rs"));
        assert!(text.contains("Write scope: /home/smt/harness"));
        assert!(!text.contains("content-secret"));
        assert!(!text.contains("diff-secret"));
        assert_no_protocol_plumbing(&text, &["conversation-secret", "call-secret"]);
    }

    #[test]
    fn legacy_command_approval_projects_argv_cwd_reason_and_decisions() {
        let (_, text) = semantic_server_request(
            "execCommandApproval",
            json!({
                "conversationId": "conversation-secret",
                "callId": "call-secret",
                "approvalId": "approval-secret",
                "command": ["cargo", "test", "-p", "harness_protocol"],
                "parsedCmd": [{"opaque": "parsed-secret"}],
                "cwd": "/home/smt/harness/app",
                "reason": "Validate request summaries"
            }),
        );

        assert!(text.contains("$ cargo test -p harness_protocol"));
        assert!(text.contains("Working directory: /home/smt/harness/app"));
        assert!(text.contains("Reason: Validate request summaries"));
        assert!(text.contains("Allow once"));
        assert!(!text.contains("parsed-secret"));
        assert_no_protocol_plumbing(
            &text,
            &["conversation-secret", "call-secret", "approval-secret"],
        );
    }

    #[test]
    fn malformed_request_fallbacks_never_dump_ids_json_or_encoded_payloads() {
        let encoded = "QUJD".repeat(64);
        let methods = [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/permissions/requestApproval",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "execCommandApproval",
            "applyPatchApproval",
        ];
        for method in methods {
            let (_, text) = semantic_server_request(
                method,
                json!({
                    "threadId": "thread-secret",
                    "turnId": "turn-secret",
                    "itemId": "item-secret",
                    "approvalId": "approval-secret",
                    "callId": "call-secret",
                    "conversationId": "conversation-secret",
                    "reason": format!("payload {encoded}"),
                    "blob": format!("data:image/png;base64,{encoded}"),
                    "unexpected": {"nested": "raw-json-secret"}
                }),
            );
            assert!(text.contains("[encoded data omitted]") || !text.contains("payload"));
            assert!(!text.contains(&encoded));
            assert!(!text.contains("raw-json-secret"));
            assert_no_protocol_plumbing(
                &text,
                &[
                    "thread-secret",
                    "turn-secret",
                    "item-secret",
                    "approval-secret",
                    "call-secret",
                    "conversation-secret",
                ],
            );
        }
    }

    #[test]
    fn maps_all_known_app_server_thread_items() {
        let known = [
            "userMessage",
            "agentMessage",
            "reasoning",
            "plan",
            "commandExecution",
            "fileChange",
            "mcpToolCall",
            "dynamicToolCall",
            "collabAgentToolCall",
            "subAgentActivity",
            "webSearch",
            "imageView",
            "imageGeneration",
            "enteredReviewMode",
            "exitedReviewMode",
            "contextCompaction",
        ];
        assert!(known.into_iter().all(|kind| {
            kind_from_protocol(kind) != TranscriptKind::Trace || kind == "contextCompaction"
        }));
    }

    #[test]
    fn context_compaction_is_a_compact_visible_transcript_landmark() {
        let mut model = TranscriptModel::default();
        let outcome = model.apply_batch(
            vec![Event::Notification {
                method: "item/completed".into(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "compaction-1",
                        "type": "contextCompaction",
                        "status": "completed"
                    }
                }),
            }],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(outcome.dirty, HashSet::from([0]));
        assert_eq!(model.items[0].kind, TranscriptKind::Trace);
        assert_eq!(model.items[0].title, "Context compacted");
        assert!(model.items[0].is_presentationally_visible());
        assert!(!model.items[0].expanded);
        assert!(model.items[0].content.is_empty());
        assert_eq!(model.items[0].display_status(), None);
        assert_eq!(model.raw_events.len(), 1);
        assert_eq!(model.raw_events[0].method, "item/completed");
        assert!(TranscriptModel::replay(120).items.iter().any(|item| {
            item.kind == TranscriptKind::Trace
                && string_at(&item.raw, "/type") == Some("contextCompaction")
        }));
    }

    #[test]
    fn context_compaction_uses_the_summary_when_the_server_provides_one() {
        let item = item_from_protocol(
            json!({
                "id": "compaction-1",
                "type": "contextCompaction",
                "status": "completed",
                "summary": [{"type": "text", "text": "Retained the implementation plan."}]
            }),
            true,
        );

        assert_eq!(item.content, "Retained the implementation plan.");
        assert!(item.is_presentationally_visible());
    }

    #[test]
    fn markdown_monospace_ranges_retain_plaintext_source_geometry() {
        let source = "before `running` after\n\n```rust\nprintln!(\"ok\");\n```";
        let ranges = markdown_monospace_source_ranges(source);

        assert!(ranges.windows(2).all(|pair| pair[0].end <= pair[1].start));
        assert!(ranges.iter().all(|range| {
            source.is_char_boundary(range.start) && source.is_char_boundary(range.end)
        }));
        assert!(
            ranges
                .iter()
                .any(|range| source[range.clone()].contains("running"))
        );
        assert!(
            ranges
                .iter()
                .any(|range| source[range.clone()].contains("println!"))
        );
    }

    #[test]
    fn file_changes_render_semantics_instead_of_protocol_metadata() {
        let changes = json!([
            {
                "path": "/tmp/updated.rs",
                "kind": {"type": "update", "move_path": null},
                "diff": "@@ -1 +1 @@\n-old\n+new"
            },
            {
                "path": "/tmp/created.rs",
                "kind": {"type": "add"},
                "diff": "+new file"
            }
        ]);

        let rendered = render_file_changes(&changes);
        assert!(rendered.contains("Modified · /tmp/updated.rs"));
        assert!(rendered.contains("Added · /tmp/created.rs"));
        assert!(!rendered.contains("move_path"));
        assert_eq!(file_changes_title(&changes), "File changes · 2 files");
    }

    #[test]
    fn thread_snapshot_refresh_upgrades_incomplete_file_change_without_resetting_it() {
        use codex_app_server_client::{CodexThreadItem, CodexTurn};

        let file_item = |diff: &str| CodexThreadItem {
            id: "file-change-1".into(),
            kind: "fileChange".into(),
            body: json!({
                "changes": [{
                    "path": "/tmp/REVERSE_ENGINEERING.md",
                    "kind": {"type": "update", "move_path": null},
                    "diff": diff,
                }],
                "status": if diff.is_empty() { "inProgress" } else { "completed" },
            })
            .as_object()
            .cloned()
            .unwrap(),
        };
        let thread = |item| CodexThread {
            id: "thread-1".into(),
            name: None,
            preview: String::new(),
            cwd: "/tmp".into(),
            updated_at: 1,
            turns: vec![CodexTurn {
                id: "turn-1".into(),
                status: json!("inProgress"),
                items: vec![item],
            }],
        };

        let mut model = TranscriptModel::default();
        model.load_thread(&thread(file_item("")));
        model.items[0].expanded = false;
        assert_eq!(
            model.items[0].content,
            "Modified · /tmp/REVERSE_ENGINEERING.md"
        );

        let completed = thread(file_item("@@ -1 +1,2 @@\n-old\n+new\n+another"));
        let outcome = model.refresh_thread(&completed);
        assert!(!outcome.reset);
        assert_eq!(outcome.dirty, HashSet::from([0]));
        assert_eq!(outcome.old_len, 1);
        assert_eq!(outcome.new_len, 1);
        assert!(!model.items[0].expanded);
        assert!(model.items[0].content.contains("@@ -1 +1,2 @@"));

        let unchanged = model.refresh_thread(&completed);
        assert!(unchanged.dirty.is_empty());
        assert!(!unchanged.reset);
    }

    #[test]
    fn known_tool_items_render_semantic_details_instead_of_whole_payloads() {
        let mcp = item_from_protocol(
            json!({
                "id": "mcp-1",
                "type": "mcpToolCall",
                "server": "filesystem",
                "tool": "read_file",
                "status": "inProgress",
                "arguments": {"path": "/tmp/demo.rs"},
                "result": {
                    "content": [
                        {"type": "text", "text": "fn main() {}"},
                        {"type": "image", "mimeType": "image/png", "data": "large-base64-data"}
                    ],
                    "structuredContent": {"lines": 1},
                    "_meta": {"internal": true}
                }
            }),
            false,
        );
        assert_eq!(mcp.title, "MCP · filesystem / read_file");
        assert_eq!(mcp.status.as_deref(), Some("in progress"));
        assert!(mcp.content.contains("Arguments\n"));
        assert!(mcp.content.contains("fn main() {}"));
        assert!(mcp.content.contains("Image · image/png"));
        assert!(mcp.content.contains("Structured result\n"));
        assert!(!mcp.content.contains("large-base64-data"));
        assert!(!mcp.content.contains("internal"));

        let web = item_from_protocol(
            json!({
                "id": "web-1",
                "type": "webSearch",
                "query": "GPUI editor",
                "action": {"type": "search", "queries": ["GPUI editor", "Zed GPUI"]},
                "results": [{
                    "title": "GPUI crate",
                    "url": "https://example.test/gpui",
                    "snippet": "A UI framework"
                }]
            }),
            true,
        );
        assert_eq!(web.title, "Web search · GPUI editor");
        assert!(web.content.contains("Query\nGPUI editor\nZed GPUI"));
        assert!(web.content.contains("1. GPUI crate"));
        assert!(!web.content.contains("\"id\""));
    }

    #[test]
    fn command_and_subagent_titles_describe_the_work() {
        let command = json!({
            "command": "sed -n '1,20p' src/main.rs",
            "commandActions": [{
                "type": "read",
                "command": "sed -n '1,20p' src/main.rs",
                "name": "main.rs",
                "path": "/workspace/src/main.rs"
            }]
        });
        assert_eq!(command_title(&command), "Read · /workspace/src/main.rs");

        let subagent = json!({
            "tool": "spawnAgent",
            "prompt": "Audit the protocol",
            "agentsStates": {
                "thread-2": {"status": "running", "message": "Inspecting events"}
            }
        });
        assert_eq!(
            title_from_protocol("collabAgentToolCall", &subagent),
            "Subagent · Spawn Agent"
        );
        assert!(render_collab_tool_call(&subagent).contains("thread-2 · Running"));
    }

    #[test]
    fn raw_code_mode_wrappers_stay_in_the_journal_beside_one_typed_command() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "rawResponseItem/completed".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "type": "custom_tool_call",
                            "call_id": "call-1",
                            "name": "exec",
                            "input": "const r = await tools.exec_command({\n  cmd: \"cargo test\",\n  workdir: \"/workspace\",\n  yield_time_ms: 1000\n});\ntext(JSON.stringify(r));"
                        }
                    }),
                },
                Event::Notification {
                    method: "rawResponseItem/completed".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "type": "custom_tool_call_output",
                            "call_id": "call-1",
                            "output": [
                                {"type": "input_text", "text": "Script completed\n"},
                                {"type": "input_text", "text": "2 tests passed"}
                            ]
                        }
                    }),
                },
                Event::Notification {
                    method: "item/started".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "command-1",
                            "type": "commandExecution",
                            "command": "cargo test",
                            "cwd": "/workspace",
                            "status": "inProgress"
                        }
                    }),
                },
                Event::Notification {
                    method: "item/completed".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "command-1",
                            "type": "commandExecution",
                            "command": "cargo test",
                            "cwd": "/workspace",
                            "aggregatedOutput": "2 tests passed\n",
                            "status": "completed"
                        }
                    }),
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].kind, TranscriptKind::Command);
        assert_eq!(model.items[0].title, "Command");
        assert_eq!(model.items[0].status.as_deref(), Some("completed"));
        assert!(model.items[0].content.contains("$ cargo test"));
        assert_eq!(model.items[0].content, "$ cargo test\n\n2 tests passed\n");
        assert!(!model.items[0].content.contains("const r ="));
        assert_eq!(
            model.items[0].command_transcript(),
            Some(CommandTranscript {
                command: "cargo test".into(),
                cwd: Some("/workspace".into()),
                output: "2 tests passed\n".into(),
            })
        );
        assert_eq!(model.raw_events.len(), 4);
        assert_eq!(
            model
                .raw_events
                .iter()
                .filter(|event| event.method == "rawResponseItem/completed")
                .count(),
            2
        );
        assert!(
            model.raw_events[0]
                .payload
                .to_string()
                .contains("const r =")
        );
    }

    #[test]
    fn command_stream_keeps_invocation_structured_and_output_separate() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "item/started".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "item": {
                            "id": "command-1",
                            "type": "commandExecution",
                            "command": "printf '%s' \"hello\"",
                            "cwd": "/workspace",
                            "status": "inProgress"
                        }
                    }),
                },
                Event::Notification {
                    method: "item/commandExecution/outputDelta".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "itemId": "command-1",
                        "delta": "hello"
                    }),
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.items[0].content, "$ printf '%s' \"hello\"\n\nhello");
        assert_eq!(
            model.items[0].raw.get("command").and_then(Value::as_str),
            Some("printf '%s' \"hello\"")
        );
        assert_eq!(
            model.items[0].command_transcript(),
            Some(CommandTranscript {
                command: "printf '%s' \"hello\"".into(),
                cwd: Some("/workspace".into()),
                output: "hello".into(),
            })
        );
        let projection = model.item_projection(0).unwrap();
        assert_eq!(
            projection.body_text(),
            "$ printf '%s' \"hello\"\nhello",
            "the navigation document must retain painted prompt text but not the unpainted blank separator"
        );
        let body_start = projection.segment.body_range.start;
        assert_eq!(
            projection.segment.semantic_spans,
            [
                TranscriptSemanticSpan {
                    range: body_start..body_start + 21,
                    style: TranscriptSemanticStyle::CommandInvocation,
                },
                TranscriptSemanticSpan {
                    range: body_start + 22..body_start + 27,
                    style: TranscriptSemanticStyle::CommandOutput,
                },
            ]
        );
    }

    #[test]
    fn exact_bash_login_wrapper_projects_its_script_but_retains_raw_invocation() {
        let raw = "/usr/bin/bash -lc \"printf '%s\\n' hello\"";
        assert_eq!(command_for_display(raw), "printf '%s\\n' hello");
        assert_eq!(
            command_for_display("cargo test -p harness_app"),
            "cargo test -p harness_app"
        );
        assert_eq!(
            command_for_display("/usr/bin/bash -lc 'echo ok' extra"),
            "/usr/bin/bash -lc 'echo ok' extra",
            "an invocation with additional argv must remain lossless"
        );

        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![Event::Notification {
                method: "item/started".into(),
                params: json!({
                    "threadId": "thread-1",
                    "item": {
                        "id": "command-1",
                        "type": "commandExecution",
                        "command": raw,
                        "cwd": "/workspace",
                        "status": "inProgress"
                    }
                }),
            }],
            Some("thread-1"),
        );

        assert_eq!(model.items[0].content, "$ printf '%s\\n' hello");
        assert_eq!(
            model.items[0].command_transcript().unwrap().command,
            "printf '%s\\n' hello"
        );
        assert_eq!(
            model.items[0].raw.get("command").and_then(Value::as_str),
            Some(raw)
        );
    }

    #[test]
    fn plan_status_is_encoded_by_the_task_marker_not_appended_as_prose() {
        let rendered = render_plan(&json!({
            "plan": [
                {"step": "Done", "status": "completed"},
                {"step": "Working", "status": "inProgress"},
                {"step": "Later", "status": "pending"}
            ]
        }));

        assert_eq!(rendered, "- [x] Done\n- [~] Working\n- [ ] Later\n");
        assert!(!rendered.contains("(pending)"));
        assert!(!rendered.contains("(inProgress)"));
    }

    #[test]
    fn process_exit_finishes_the_streaming_process_card() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "process/outputDelta".into(),
                    params: json!({
                        "processHandle": "process-1",
                        "stream": "stdout",
                        "deltaBase64": "bGl2ZSBvdXRwdXQK"
                    }),
                },
                Event::Notification {
                    method: "process/exited".into(),
                    params: json!({
                        "processHandle": "process-1",
                        "exitCode": 7,
                        "stdout": "",
                        "stderr": "failed\n"
                    }),
                },
            ],
            None,
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].content, "live output\nfailed\n");
        assert_eq!(model.items[0].status.as_deref(), Some("exit 7"));
    }

    #[test]
    fn local_snapshot_restores_live_only_items_in_their_original_order() {
        let persisted = vec![
            replay_item(0, TranscriptKind::User, "You", "prompt", json!(null)),
            replay_item(1, TranscriptKind::Agent, "Codex", "working", json!(null)),
            replay_item(
                2,
                TranscriptKind::Command,
                "Command · cargo test",
                "$ cargo test\npassed",
                json!(null),
            ),
            replay_item(
                3,
                TranscriptKind::Agent,
                "Codex",
                "fresh final",
                json!(null),
            ),
        ];
        let mut fresh = vec![
            replay_item(0, TranscriptKind::User, "You", "prompt", json!(null)),
            replay_item(1, TranscriptKind::Agent, "Codex", "working", json!(null)),
            replay_item(
                3,
                TranscriptKind::Agent,
                "Codex",
                "fresh final",
                json!(null),
            ),
        ];
        fresh[0].key = "server-item-1".into();
        fresh[1].key = "server-item-2".into();
        fresh[2].key = "server-item-3".into();
        fresh[2].status = Some("fresh status".into());

        let (merged, restored) = merge_snapshot_items(fresh, persisted);
        assert_eq!(restored, 1);
        assert_eq!(
            merged.iter().map(|item| item.kind).collect::<Vec<_>>(),
            [
                TranscriptKind::User,
                TranscriptKind::Agent,
                TranscriptKind::Command,
                TranscriptKind::Agent
            ]
        );
        assert_eq!(merged[3].content, "fresh final");
        assert_eq!(merged[3].status.as_deref(), Some("fresh status"));
    }

    #[test]
    fn local_snapshot_discards_legacy_provider_wrapper_cards() {
        let user = replay_item(0, TranscriptKind::User, "You", "prompt", json!(null));
        let command = replay_item(
            2,
            TranscriptKind::Command,
            "Command",
            "$ cargo test\n\n2 tests passed",
            json!({"type": "commandExecution"}),
        );
        let agent = replay_item(3, TranscriptKind::Agent, "Codex", "done", json!(null));
        let wrapper = replay_item(
            1,
            TranscriptKind::Tool,
            "Tool · Exec",
            "Arguments\nconst r = await tools.exec_command(...)",
            json!({
                "call": {
                    "type": "custom_tool_call",
                    "call_id": "call-1",
                    "name": "exec"
                },
                "output": {
                    "type": "custom_tool_call_output",
                    "call_id": "call-1"
                }
            }),
        );
        let persisted = vec![user.clone(), wrapper, command.clone(), agent.clone()];
        let mut fresh = vec![user, command, agent];
        for (index, item) in fresh.iter_mut().enumerate() {
            item.key = format!("server-item-{index}");
        }

        let (merged, restored) = merge_snapshot_items(fresh, persisted);

        assert_eq!(restored, 0);
        assert_eq!(merged.len(), 3);
        assert!(merged.iter().all(|item| item.title != "Tool · Exec"));
        assert!(
            merged
                .iter()
                .all(|item| !item.content.contains("const r ="))
        );
    }

    #[test]
    fn warm_snapshot_replaces_the_model_and_rebuilds_item_lookup() {
        let user = replay_item(0, TranscriptKind::User, "You", "cached prompt", json!(null));
        let agent = replay_item(
            2,
            TranscriptKind::Agent,
            "Codex",
            "cached answer",
            json!(null),
        );
        let wrapper = replay_item(
            1,
            TranscriptKind::Tool,
            "Tool · Exec",
            "legacy wrapper",
            json!({"type": "custom_tool_call_output"}),
        );
        let user_key = user.key.clone();
        let agent_key = agent.key.clone();
        let snapshot = PersistedTranscript {
            version: SNAPSHOT_VERSION,
            thread_id: "thread-1".into(),
            items: vec![user, wrapper, agent],
        };
        let mut model = TranscriptModel::replay(5);

        let restored = model.restore_persisted_snapshot(snapshot);

        assert_eq!(restored, 2);
        assert_eq!(model.items.len(), 2);
        assert_eq!(model.items[0].content, "cached prompt");
        assert_eq!(model.items[1].content, "cached answer");
        assert_eq!(model.item_indices.get(&user_key), Some(&0));
        assert_eq!(model.item_indices.get(&agent_key), Some(&1));
    }

    #[test]
    fn warm_snapshot_repairs_duplicate_item_identities_and_keeps_newest_state() {
        let prefix = replay_item(0, TranscriptKind::User, "You", "cached prompt", json!(null));
        let mut stale = replay_item(
            1,
            TranscriptKind::Agent,
            "Codex",
            "partial answer",
            json!(null),
        );
        let middle = replay_item(
            2,
            TranscriptKind::Command,
            "Command",
            "command output",
            json!(null),
        );
        let mut complete = stale.clone();
        complete.content = "complete answer".into();
        complete.status = Some("completed".into());
        complete.event_count = 7;
        stale.status = Some("in progress".into());
        let duplicate_key = stale.key.clone();
        let snapshot = PersistedTranscript {
            version: SNAPSHOT_VERSION,
            thread_id: "thread-1".into(),
            items: vec![prefix, stale, middle, complete],
        };
        let mut model = TranscriptModel::default();

        let restored = model.restore_persisted_snapshot(snapshot);

        assert_eq!(restored, 3);
        assert_eq!(model.items.len(), 3);
        assert_eq!(model.items[2].key, duplicate_key);
        assert_eq!(model.items[2].content, "complete answer");
        assert_eq!(model.items[2].status.as_deref(), Some("completed"));
        assert_eq!(model.item_indices.get(&duplicate_key), Some(&2));
        assert_eq!(
            model
                .items
                .iter()
                .map(|item| item.key.as_str())
                .collect::<HashSet<_>>()
                .len(),
            model.items.len()
        );
    }

    #[test]
    fn persisted_merge_does_not_restore_a_duplicate_history() {
        let user = replay_item(0, TranscriptKind::User, "You", "prompt", json!(null));
        let mut stale_command = replay_item(
            1,
            TranscriptKind::Command,
            "Command",
            "partial output",
            json!(null),
        );
        let agent = replay_item(2, TranscriptKind::Agent, "Codex", "done", json!(null));
        let mut complete_command = stale_command.clone();
        complete_command.content = "complete output".into();
        complete_command.event_count = 9;
        stale_command.event_count = 1;
        let persisted = vec![
            user.clone(),
            stale_command,
            agent.clone(),
            complete_command,
            agent.clone(),
        ];
        let mut fresh = vec![user, agent];
        fresh[0].key = "server-user".into();
        fresh[1].key = "server-agent".into();

        let (merged, restored) = merge_snapshot_items(fresh, persisted);

        assert_eq!(restored, 1);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[1].kind, TranscriptKind::Command);
        assert_eq!(merged[1].content, "complete output");
        assert_eq!(merged[2].key, "server-agent");
        assert_eq!(
            merged
                .iter()
                .map(|item| item.key.as_str())
                .collect::<HashSet<_>>()
                .len(),
            merged.len()
        );
    }

    #[test]
    fn prepared_snapshot_never_serializes_duplicate_item_identities() {
        let mut model = TranscriptModel::default();
        let stale = replay_item(
            0,
            TranscriptKind::Agent,
            "Codex",
            "partial answer",
            json!(null),
        );
        let mut complete = stale.clone();
        complete.content = "complete answer".into();
        complete.event_count = 4;
        model.items = vec![stale, complete];

        let prepared = model
            .prepare_transcript_snapshot("duplicate-snapshot-test")
            .expect("snapshot should serialize");
        let snapshot: PersistedTranscript =
            serde_json::from_slice(&prepared.serialized).expect("snapshot should be valid JSON");

        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].content, "complete answer");
        assert_eq!(snapshot.items[0].event_count, 4);
    }

    #[test]
    fn local_snapshot_does_not_resurrect_items_past_a_rollback_anchor() {
        let persisted = vec![
            replay_item(0, TranscriptKind::User, "You", "prompt", json!(null)),
            replay_item(1, TranscriptKind::Command, "Command", "output", json!(null)),
            replay_item(2, TranscriptKind::Agent, "Codex", "final", json!(null)),
        ];
        let mut fresh = vec![replay_item(
            0,
            TranscriptKind::User,
            "You",
            "prompt",
            json!(null),
        )];
        fresh[0].key = "server-item-1".into();

        let (merged, restored) = merge_snapshot_items(fresh, persisted);
        assert_eq!(restored, 0);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].kind, TranscriptKind::User);
    }

    #[test]
    fn replay_scales_to_large_transcripts() {
        let model = TranscriptModel::replay(10_000);
        assert_eq!(model.items.len(), 10_000);
        assert_eq!(model.item_indices.len(), 10_000);
    }

    #[test]
    fn vim_document_is_windowed_around_the_selected_semantic_block() {
        let model = TranscriptModel::replay(10_000);
        let document = model.document_window(5_000, 400);

        assert_eq!(document.item_rows.len(), 10_000);
        assert!(document.item_rows[5_001].is_some());
        assert!(document.item_rows[0].is_none());
        assert!(document.text.contains("4800 EARLIER BLOCKS"));
        assert!(document.text.contains("4800 LATER BLOCKS"));
        assert!(document.text.len() < 1_000_000);
    }

    #[test]
    fn document_segments_describe_utf8_safe_header_body_and_whole_ranges() {
        let mut model = TranscriptModel::default();
        let mut item = replay_item(
            0,
            TranscriptKind::Tool,
            "Café 🦀",
            "  α first\nβ second  ",
            json!(null),
        );
        item.key = "unicode:item".into();
        item.status = Some("prêt".into());
        model.push_without_splice(item);

        let document = model.full_document();
        let [segment] = document.segments.as_slice() else {
            panic!("expected exactly one document segment");
        };

        assert_eq!(segment.item_index, 0);
        assert_eq!(segment.item_key, "unicode:item");
        assert_eq!(segment.kind, TranscriptKind::Tool);
        assert_eq!(
            &document.text[segment.header_range.clone()],
            "━━━━ Café 🦀 · prêt ━━━━"
        );
        assert_eq!(
            &document.text[segment.body_range.clone()],
            "α first\nβ second"
        );
        assert_eq!(
            &document.text[segment.whole_range.clone()],
            "━━━━ Café 🦀 · prêt ━━━━\nα first\nβ second\n"
        );
        for offset in [
            segment.whole_range.start,
            segment.whole_range.end,
            segment.header_range.start,
            segment.header_range.end,
            segment.body_range.start,
            segment.body_range.end,
        ] {
            assert!(document.text.is_char_boundary(offset));
        }
    }

    #[test]
    fn rich_navigation_document_contains_no_invisible_header_ornaments() {
        let model = TranscriptModel::replay(8);
        let document = model.rich_navigation_document();

        assert!(!document.text.contains("━━━━"));
        assert!(
            document
                .segments
                .iter()
                .all(|segment| segment.header_range.is_empty())
        );
        for segment in &document.segments {
            let projection = model
                .rich_navigation_item_projection(segment.item_index)
                .unwrap();
            assert_eq!(projection.text, document.text[segment.whole_range.clone()]);
            assert_eq!(
                projection.body_text(),
                &document.text[segment.body_range.clone()]
            );
        }
    }

    #[test]
    fn callers_can_replace_rich_bodies_and_reassemble_exact_document_offsets() {
        let model = TranscriptModel::replay(6);
        let command = model
            .rich_navigation_item_projection(5)
            .unwrap()
            .with_body_text("cargo check -p harness_app\nfinished".into());
        assert_eq!(command.body_text(), "cargo check -p harness_app\nfinished");
        assert_eq!(command.text, "cargo check -p harness_app\nfinished\n");
        assert!(command.segment.header_range.is_empty());
        assert!(command.segment.semantic_spans.is_empty());

        let document = TranscriptDocument::from_item_projections(
            model.items.len(),
            [
                model.rich_navigation_item_projection(4).unwrap(),
                command.clone(),
            ],
        );
        assert_eq!(document.segments.len(), 2);
        assert_eq!(document.item_rows[4], Some(0));
        assert_eq!(
            document.item_rows[5],
            Some(
                model
                    .rich_navigation_item_projection(4)
                    .unwrap()
                    .text
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count() as u32
            )
        );
        assert_eq!(
            &document.text[document.segments[1].body_range.clone()],
            command.body_text()
        );
    }

    #[test]
    fn final_projection_can_drop_only_its_unpainted_separator_row() {
        let model = TranscriptModel::replay(6);
        let projection = model.rich_navigation_item_projection(5).unwrap();
        assert!(projection.text.ends_with('\n'));
        assert_eq!(
            projection.segment.body_range.end + 1,
            projection.segment.whole_range.end
        );

        let terminal = projection.clone().without_terminal_separator();
        assert!(!terminal.text.ends_with('\n'));
        assert_eq!(terminal.body_text(), projection.body_text());
        assert_eq!(terminal.segment.body_range, projection.segment.body_range);
        assert_eq!(terminal.segment.whole_range.end, terminal.text.len());
        assert_eq!(
            terminal.segment.whole_range.end,
            terminal.segment.body_range.end
        );

        assert_eq!(
            terminal.clone().without_terminal_separator(),
            terminal,
            "removing the terminal separator is idempotent"
        );
    }

    #[test]
    fn narrative_document_bodies_are_selectable_text_without_markdown_furniture() {
        let mut model = TranscriptModel::default();
        model.push_without_splice(replay_item(
            0,
            TranscriptKind::Agent,
            "Codex",
            "# Result\n\nA **real Vim composer** with [Zed](https://zed.dev).\n\n- [x] Native `motions`",
            json!(null),
        ));

        let document = model.full_document();
        let segment = &document.segments[0];
        assert!(segment.header_range.is_empty());
        assert_eq!(segment.whole_range.start, segment.body_range.start);
        let body = &document.text[document.segments[0].body_range.clone()];

        assert!(body.contains("Result"));
        assert!(body.contains("A real Vim composer with Zed (https://zed.dev)."));
        assert!(body.contains("• [x] Native motions"));
        assert!(!body.contains("**"));
        assert!(!body.contains("# Result"));
        assert!(!body.contains("`motions`"));
        assert!(!document.text.contains("Codex"));
    }

    #[test]
    fn default_user_messages_are_compact_bodies_without_attribution_rows() {
        let mut model = TranscriptModel::default();
        model.push_without_splice(replay_item(
            0,
            TranscriptKind::User,
            "You",
            "A compact message",
            json!(null),
        ));

        let document = model.full_document();
        let segment = &document.segments[0];
        assert!(segment.header_range.is_empty());
        assert_eq!(
            &document.text[segment.body_range.clone()],
            "A compact message"
        );
        assert_eq!(
            &document.text[segment.whole_range.clone()],
            "A compact message\n"
        );
    }

    #[test]
    fn routine_lifecycle_statuses_stay_out_of_every_transcript_kind() {
        for status in [
            "completed",
            "COMPLETED",
            "sent",
            "replay",
            "success",
            "succeeded",
            "idle",
            "inactive",
            "ready",
            "resolved",
            "streaming",
            "running",
            "in progress",
        ] {
            let mut item = replay_item(
                0,
                TranscriptKind::Reasoning,
                "Reasoning",
                "A useful step",
                json!(null),
            );
            item.status = Some(status.into());
            assert_eq!(item.display_status(), None, "status {status}");
        }

        for status in ["waiting", "responding", "failed", "interrupted", "offline"] {
            let mut item =
                replay_item(0, TranscriptKind::Command, "Command", "output", json!(null));
            item.status = Some(status.into());
            assert_eq!(item.display_status(), Some(status), "status {status}");
        }
    }

    #[test]
    fn object_statuses_are_semantic_and_opaque_shapes_never_render_as_json() {
        assert_eq!(
            protocol_status(&json!({"type": "completed"})),
            Some("completed".into())
        );
        assert_eq!(
            protocol_status(&json!({"state": {"status": "inProgress"}})),
            Some("in progress".into())
        );
        assert_eq!(protocol_status(&json!({"phaseCode": 7})), None);
        assert_eq!(protocol_status(&json!(["completed"])), None);

        let completed = item_from_protocol(
            json!({
                "id": "agent-completed",
                "type": "agentMessage",
                "text": "Finished work",
                "status": {"type": "completed"}
            }),
            false,
        );
        assert_eq!(completed.status.as_deref(), Some("completed"));
        assert_eq!(completed.display_status(), None);

        let running = item_from_protocol(
            json!({
                "id": "agent-running",
                "type": "agentMessage",
                "text": "Working",
                "status": {"state": {"status": "inProgress"}}
            }),
            false,
        );
        assert_eq!(running.display_status(), None);

        let opaque = item_from_protocol(
            json!({
                "id": "agent-opaque",
                "type": "agentMessage",
                "text": "Still working",
                "status": {"phaseCode": 7, "metadata": []}
            }),
            false,
        );
        assert_eq!(opaque.status.as_deref(), Some("in progress"));
        let projection = project_transcript_item(0, &opaque).unwrap();
        assert!(!projection.header_text().contains('{'));
        assert!(!projection.header_text().contains("phaseCode"));
    }

    #[test]
    fn trace_items_remain_diagnostic_but_never_enter_either_reading_surface() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "item/completed".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "future-item-1",
                            "type": "futureDiagnosticItem",
                            "status": "completed",
                            "details": {"message": "retained for diagnostics"}
                        }
                    }),
                },
                Event::UnmatchedResponse {
                    id: json!(99),
                    result: Some(json!({"unexpected": true})),
                    error: None,
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.raw_events.len(), 2);
        assert_eq!(model.raw_events[0].method, "item/completed");
        assert_eq!(model.raw_events[1].method, "unmatchedResponse");
        assert_eq!(model.items.len(), 2);
        assert!(
            model
                .items
                .iter()
                .all(|item| item.kind == TranscriptKind::Trace)
        );
        assert!(
            model
                .items
                .iter()
                .all(|item| !item.is_presentationally_visible())
        );
        assert!(model.item_projection(0).is_none());
        assert!(model.item_projection(1).is_none());
        assert!(model.full_document().text.is_empty());
    }

    #[test]
    fn empty_reasoning_has_no_false_document_disclosure_for_any_routine_status() {
        let mut model = TranscriptModel::default();
        let mut item = replay_item(
            0,
            TranscriptKind::Reasoning,
            "Reasoning",
            "",
            json!({"type": "reasoning"}),
        );
        item.status = Some("completed".into());
        model.push_without_splice(item);

        assert!(!model.items[0].is_presentationally_visible());
        assert!(model.item_projection(0).is_none());
        assert!(model.full_document().text.is_empty());
        assert_eq!(model.raw_events.len(), 0);

        model.items[0].status = Some("running".into());
        assert!(!model.items[0].is_presentationally_visible());
        assert!(model.item_projection(0).is_none());
    }

    #[test]
    fn meaningful_items_start_expanded_even_after_completion() {
        for kind in [
            TranscriptKind::Reasoning,
            TranscriptKind::Plan,
            TranscriptKind::Command,
            TranscriptKind::FileChange,
            TranscriptKind::Tool,
            TranscriptKind::Diff,
            TranscriptKind::Image,
            TranscriptKind::Subagent,
            TranscriptKind::Web,
            TranscriptKind::Review,
            TranscriptKind::Error,
            TranscriptKind::Approval,
        ] {
            assert!(default_expanded(kind, true), "kind {kind:?}");
        }
        assert!(!default_expanded(TranscriptKind::Trace, false));
    }

    #[test]
    fn per_item_projection_matches_its_full_document_segment() {
        let model = TranscriptModel::replay(24);
        let document = model.full_document();

        for segment in &document.segments {
            let projection = model.item_projection(segment.item_index).unwrap();
            assert_eq!(projection.segment.item_index, segment.item_index);
            assert_eq!(projection.segment.item_key, segment.item_key);
            assert_eq!(projection.segment.kind, segment.kind);
            assert_eq!(projection.text, document.text[segment.whole_range.clone()]);
            assert_eq!(
                projection.header_text(),
                &document.text[segment.header_range.clone()]
            );
            assert_eq!(
                projection.body_text(),
                &document.text[segment.body_range.clone()]
            );
            assert_eq!(
                segment.semantic_spans,
                projection
                    .segment
                    .semantic_spans
                    .iter()
                    .map(|span| TranscriptSemanticSpan {
                        range: shifted_range(&span.range, segment.whole_range.start),
                        style: span.style,
                    })
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(document.segments.len(), model.items.len());
        assert!(
            model
                .items
                .iter()
                .enumerate()
                .all(|(index, _)| model.item_projection(index).is_some())
        );
    }

    #[test]
    fn streaming_narrative_projection_is_append_stable_then_cleans_markdown_once() {
        let mut model = TranscriptModel::default();
        let mut item = replay_item(0, TranscriptKind::Agent, "Codex", "A **real", json!(null));
        item.status = Some("streaming".into());
        model.push_without_splice(item);

        let first = model.item_projection(0).unwrap();
        assert_eq!(first.body_text(), "A **real");
        assert!(first.segment.semantic_spans.is_empty());

        model.items[0].content.push_str(" Vim** composer");
        let second = model.item_projection(0).unwrap();
        assert_eq!(second.body_text(), "A **real Vim** composer");
        assert!(second.body_text().starts_with(first.body_text()));
        assert!(second.segment.semantic_spans.is_empty());

        model.items[0].status = Some("completed".into());
        let completed = model.item_projection(0).unwrap();
        assert_eq!(completed.body_text(), "A real Vim composer");
        assert!(!completed.body_text().contains("**"));
        assert_eq!(
            completed
                .segment
                .semantic_spans
                .iter()
                .map(|span| (span.style, &completed.text[span.range.clone()]))
                .collect::<Vec<_>>(),
            [(TranscriptSemanticStyle::Strong, "real Vim")]
        );
    }

    #[test]
    fn completed_markdown_semantics_use_nested_unicode_output_ranges() {
        let mut model = TranscriptModel::default();
        model.push_without_splice(replay_item(
            0,
            TranscriptKind::Agent,
            "Codex",
            "# Hé **bold and *嵌套***\n\nUse `café` at [Zed](https://zed.dev).",
            json!(null),
        ));

        let projection = model.item_projection(0).unwrap();
        assert_eq!(
            projection.body_text(),
            "Hé bold and 嵌套\nUse café at Zed (https://zed.dev)."
        );
        assert!(
            !projection
                .body_text()
                .chars()
                .any(|character| ['#', '*', '`', '[', ']'].contains(&character))
        );
        assert!(projection.segment.semantic_spans.iter().all(|span| {
            projection.text.is_char_boundary(span.range.start)
                && projection.text.is_char_boundary(span.range.end)
                && projection.segment.body_range.start <= span.range.start
                && span.range.end <= projection.segment.body_range.end
        }));

        let semantic_text = |style| {
            projection
                .segment
                .semantic_spans
                .iter()
                .filter(|span| span.style == style)
                .map(|span| &projection.text[span.range.clone()])
                .collect::<Vec<_>>()
        };
        assert_eq!(
            semantic_text(TranscriptSemanticStyle::Heading),
            ["Hé bold and 嵌套"]
        );
        assert_eq!(
            semantic_text(TranscriptSemanticStyle::Strong),
            ["bold and 嵌套"]
        );
        assert_eq!(semantic_text(TranscriptSemanticStyle::Emphasis), ["嵌套"]);
        assert_eq!(semantic_text(TranscriptSemanticStyle::InlineCode), ["café"]);
        assert_eq!(
            semantic_text(TranscriptSemanticStyle::Link),
            ["Zed (https://zed.dev)"]
        );
    }

    #[test]
    fn rich_markdown_navigation_omits_unpainted_link_destinations() {
        let source = "I updated [discord-canary.desktop](/home/smt/.local/share/applications/discord-canary.desktop) to inject:\n\n```text\nXDG_SESSION_TYPE=wayland\n```\n\nOne more restart.";

        assert_eq!(
            rich_markdown_navigation_text(source),
            "I updated discord-canary.desktop to inject:\nXDG_SESSION_TYPE=wayland\nOne more restart."
        );
    }

    #[test]
    fn rich_markdown_navigation_preserves_native_replacement_tokens() {
        let source = concat!(
            "before\n\n---\n\n- [X] first\n1) second\n\n",
            "| left | right |\n| --- | --- |\n| one | two |\n\n",
            "![alt text](image.png)",
        );
        let navigation = rich_markdown_navigation_text(source);

        assert!(navigation.starts_with("before\n---\n- [X] first\n1) second"));
        assert!(
            navigation.contains("leftright\nonetwo"),
            "unexpected navigation projection: {navigation:?}"
        );
        assert!(navigation.ends_with("alt text"));
        assert!(!navigation.contains('\t'));
        assert!(!navigation.contains("Image: "));
    }

    #[test]
    fn completed_block_markdown_styles_exact_selectable_output_ranges() {
        let source =
            "> Quote ~~retiré~~ and **kept**.\n>\n> δεύτερο\n\n```rust\nlet café = 1;\n```";
        let mut model = TranscriptModel::default();
        let mut item = replay_item(0, TranscriptKind::Agent, "Codex", source, json!(null));
        item.status = Some("streaming".into());
        model.push_without_splice(item);

        let streaming = model.item_projection(0).unwrap();
        assert_eq!(streaming.body_text(), source);
        assert!(streaming.segment.semantic_spans.is_empty());

        model.items[0].status = Some("completed".into());
        let completed = model.item_projection(0).unwrap();
        assert_eq!(
            completed.body_text(),
            "Quote retiré and kept.\nδεύτερο\nlet café = 1;"
        );
        assert!(completed.segment.semantic_spans.iter().all(|span| {
            completed.text.is_char_boundary(span.range.start)
                && completed.text.is_char_boundary(span.range.end)
                && completed.segment.body_range.start <= span.range.start
                && span.range.end <= completed.segment.body_range.end
        }));

        let semantic_text = |style| {
            completed
                .segment
                .semantic_spans
                .iter()
                .filter(|span| span.style == style)
                .map(|span| &completed.text[span.range.clone()])
                .collect::<Vec<_>>()
        };
        assert_eq!(
            semantic_text(TranscriptSemanticStyle::BlockQuote),
            ["Quote retiré and kept.\nδεύτερο"]
        );
        assert_eq!(
            semantic_text(TranscriptSemanticStyle::Strikethrough),
            ["retiré"]
        );
        assert_eq!(semantic_text(TranscriptSemanticStyle::Strong), ["kept"]);
        assert_eq!(
            semantic_text(TranscriptSemanticStyle::CodeBlock),
            ["let café = 1;"]
        );
        assert!(
            !completed
                .body_text()
                .chars()
                .any(|character| ['>', '~', '`'].contains(&character))
        );
    }

    #[test]
    fn completed_markdown_semantic_metadata_has_a_per_item_cap() {
        let source = (0..MAX_SEMANTIC_SPANS_PER_ITEM + 500)
            .map(|index| format!("**value-{index}** "))
            .collect::<String>();
        let projection = selectable_markdown_text(&source);

        assert_eq!(projection.semantic_spans.len(), MAX_SEMANTIC_SPANS_PER_ITEM);
        assert!(projection.semantic_spans.windows(2).all(|pair| {
            (pair[0].range.start, pair[0].range.end, pair[0].style)
                <= (pair[1].range.start, pair[1].range.end, pair[1].style)
        }));
        assert!(projection.semantic_spans.iter().all(|span| {
            projection.text.is_char_boundary(span.range.start)
                && projection.text.is_char_boundary(span.range.end)
        }));
    }

    #[test]
    fn document_offsets_match_zed_buffer_line_ending_normalization() {
        let mut model = TranscriptModel::default();
        model.push_without_splice(replay_item(
            0,
            TranscriptKind::Command,
            "Command",
            "first\r\nsecond\rthird\nfourth\r\n",
            json!(null),
        ));

        let projection = model.item_projection(0).unwrap();
        assert_eq!(projection.body_text(), "first\nsecond\nthird\nfourth");
        assert!(!projection.text.contains('\r'));
        assert_eq!(projection.segment.whole_range.end, projection.text.len());

        let document = model.full_document();
        assert_eq!(document.text, projection.text);
        assert_eq!(document.segments[0].whole_range.end, document.text.len());
        assert_eq!(
            &document.text[document.segments[0].body_range.clone()],
            "first\nsecond\nthird\nfourth"
        );
    }

    #[test]
    fn structured_document_bodies_preserve_markdown_like_output_exactly() {
        let mut model = TranscriptModel::default();
        model.push_without_splice(replay_item(
            0,
            TranscriptKind::Command,
            "Command",
            "  # literal output\n> still literal\n```sh\nprintf ok\n```\n~~not strikethrough~~  ",
            json!(null),
        ));

        let document = model.full_document();
        let body = &document.text[document.segments[0].body_range.clone()];

        assert_eq!(
            body,
            "# literal output\n> still literal\n```sh\nprintf ok\n```\n~~not strikethrough~~"
        );
        assert!(document.segments[0].semantic_spans.is_empty());
    }

    #[test]
    fn document_segments_exclude_protocol_noise_but_include_compaction_landmarks() {
        let mut model = TranscriptModel::default();
        model.push_without_splice(replay_item(
            0,
            TranscriptKind::User,
            "You",
            "visible before",
            json!(null),
        ));
        model.push_without_splice(replay_item(
            1,
            TranscriptKind::Trace,
            "Internal event",
            "must not enter the document",
            json!(null),
        ));
        model.push_without_splice(replay_item(
            2,
            TranscriptKind::Trace,
            "Context compacted",
            "Earlier context was summarized here",
            json!({"type": "contextCompaction"}),
        ));
        model.push_without_splice(replay_item(
            3,
            TranscriptKind::Agent,
            "Codex",
            "visible after",
            json!(null),
        ));

        let document = model.full_document();
        assert_eq!(
            document
                .segments
                .iter()
                .map(|segment| segment.item_index)
                .collect::<Vec<_>>(),
            [0, 2, 3]
        );
        assert!(document.item_rows[0].is_some());
        assert!(document.item_rows[1].is_none());
        assert!(document.item_rows[2].is_some());
        assert!(document.item_rows[3].is_some());
        assert!(!document.text.contains("Internal event"));
        assert!(!document.text.contains("must not enter the document"));
        assert!(document.text.contains("Context compacted"));
        assert!(
            document
                .text
                .contains("Earlier context was summarized here")
        );
    }

    #[test]
    fn full_document_covers_all_semantic_items_in_a_ten_thousand_item_replay() {
        let model = TranscriptModel::replay(10_000);
        let document = model.full_document();
        let semantic_indices = model
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.is_presentationally_visible().then_some(index))
            .collect::<Vec<_>>();

        assert_eq!(document.item_rows.len(), 10_000);
        assert_eq!(document.segments.len(), semantic_indices.len());
        assert_eq!(document.segments.first().unwrap().whole_range.start, 0);
        assert_eq!(
            document.segments.last().unwrap().whole_range.end,
            document.text.len()
        );
        assert!(!document.text.contains("EARLIER BLOCKS"));
        assert!(!document.text.contains("LATER BLOCKS"));

        for (segment, expected_index) in document.segments.iter().zip(semantic_indices) {
            let item = &model.items[expected_index];
            assert_eq!(segment.item_index, expected_index);
            assert_eq!(segment.item_key, item.key);
            assert_eq!(segment.kind, item.kind);
            assert!(document.item_rows[expected_index].is_some());
            assert!(document.text.is_char_boundary(segment.whole_range.start));
            assert!(document.text.is_char_boundary(segment.whole_range.end));
        }
        for (index, item) in model.items.iter().enumerate() {
            assert_eq!(
                document.item_rows[index].is_some(),
                item.is_presentationally_visible()
            );
        }
        assert!(
            document
                .segments
                .windows(2)
                .all(|pair| pair[0].whole_range.end == pair[1].whole_range.start)
        );
    }

    #[test]
    fn high_volume_process_deltas_are_aggregated_with_diagnostic_events_retained() {
        let mut model = TranscriptModel::default();
        let events = ["aGVs", "bG8="]
            .into_iter()
            .map(|delta| Event::Notification {
                method: "process/outputDelta".into(),
                params: json!({
                    "processHandle": "process-7",
                    "stream": "stdout",
                    "deltaBase64": delta,
                    "capReached": false,
                }),
            })
            .collect();
        model.apply_batch(events, None);

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].content, "hello");
        assert_eq!(model.items[0].event_count, 2);
        assert_eq!(model.raw_events.len(), 2);
    }

    #[test]
    fn diagnostic_journal_is_bounded_without_reusing_sequence_numbers() {
        let mut model = TranscriptModel::default();
        let event_count = RAW_EVENT_LIMIT + RAW_EVENT_EVICTION_BATCH;
        let events = (0..event_count)
            .map(|index| Event::Notification {
                method: "future/diagnostic".into(),
                params: json!({"index": index}),
            })
            .collect();

        model.apply_batch(events, None);

        assert_eq!(model.raw_events.len(), RAW_EVENT_LIMIT);
        assert_eq!(model.dropped_raw_events, RAW_EVENT_EVICTION_BATCH);
        assert_eq!(
            model.raw_events.first().map(|event| event.sequence),
            Some(RAW_EVENT_EVICTION_BATCH + 1)
        );
        assert_eq!(
            model.raw_events.last().map(|event| event.sequence),
            Some(event_count)
        );
    }

    #[test]
    fn reasoning_summary_parts_preserve_token_flow_and_paragraphs() {
        let mut model = TranscriptModel::default();
        let events = [
            ("item/reasoning/summaryPartAdded", json!({})),
            (
                "item/reasoning/summaryTextDelta",
                json!({"delta": "Planning "}),
            ),
            ("item/reasoning/summaryTextDelta", json!({"delta": "work"})),
            ("item/reasoning/summaryPartAdded", json!({})),
            ("item/reasoning/summaryTextDelta", json!({"delta": "Done"})),
        ]
        .into_iter()
        .map(|(method, payload)| Event::Notification {
            method: method.into(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "reasoning-1",
                "summaryIndex": 0,
                "delta": payload.get("delta").cloned().unwrap_or(Value::Null),
            }),
        })
        .collect();
        model.apply_batch(events, Some("thread-1"));

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].content, "Planning work\n\nDone");
        assert_eq!(model.raw_events.len(), 5);
    }

    #[test]
    fn session_telemetry_stays_out_of_the_conversation_and_remains_diagnostic() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "thread/status/changed".into(),
                    params: json!({"status": {"type": "idle"}}),
                },
                Event::Notification {
                    method: "thread/tokenUsage/updated".into(),
                    params: json!({"inputTokens": 1200, "outputTokens": 400}),
                },
                Event::Notification {
                    method: "thread/goal/cleared".into(),
                    params: json!({}),
                },
                Event::Notification {
                    method: "mcpServer/startupStatus/updated".into(),
                    params: json!({"name": "demo", "status": "ready"}),
                },
            ],
            None,
        );

        assert!(model.items.is_empty());
        assert_eq!(model.raw_events.len(), 4);
        assert_eq!(model.telemetry.thread_status.as_deref(), Some("idle"));
        assert_eq!(
            model.telemetry.summary().as_deref(),
            Some("IDLE · MCP 1/1 ready")
        );
    }

    #[test]
    fn partial_thread_settings_update_replaces_stale_selected_task_state() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "thread/settings/updated".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "threadSettings": {
                            "model": "gpt-old",
                            "modelProvider": "openai",
                            "effort": "medium",
                            "sandboxPolicy": {"type": "readOnly", "networkAccess": false}
                        }
                    }),
                },
                Event::Notification {
                    method: "thread/settings/updated".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "threadSettings": {
                            "model": "gpt-new",
                            "cwd": "/workspace/new",
                            "futureSetting": {"enabled": true}
                        }
                    }),
                },
            ],
            Some("thread-1"),
        );

        let settings = model.telemetry.thread_settings.as_ref().unwrap();
        assert_eq!(settings.model.as_deref(), Some("gpt-new"));
        assert_eq!(settings.cwd.as_deref(), Some("/workspace/new"));
        assert!(settings.model_provider.is_none());
        assert!(settings.effort.is_none());
        assert!(settings.sandbox_policy.is_none());
        assert!(model.items.is_empty());
        assert_eq!(model.raw_events.len(), 2);
        assert_eq!(
            model.raw_events[1]
                .payload
                .pointer("/threadSettings/futureSetting/enabled")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn full_v2_thread_settings_update_populates_typed_snapshot() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![Event::Notification {
                method: "thread/settings/updated".into(),
                params: json!({
                    "threadId": "thread-1",
                    "threadSettings": {
                        "model": "gpt-5.6-sol",
                        "modelProvider": "openai",
                        "effort": "high",
                        "approvalPolicy": {
                            "granular": {
                                "mcp_elicitations": true,
                                "rules": false,
                                "sandbox_approval": true,
                                "request_permissions": true,
                                "skill_approval": false
                            }
                        },
                        "approvalsReviewer": "auto_review",
                        "sandboxPolicy": {
                            "type": "workspaceWrite",
                            "networkAccess": true,
                            "writableRoots": ["/workspace", "/tmp/build"],
                            "excludeSlashTmp": true,
                            "excludeTmpdirEnvVar": false
                        },
                        "activePermissionProfile": {
                            "id": ":workspace",
                            "extends": "base"
                        },
                        "collaborationMode": {
                            "mode": "default",
                            "settings": {
                                "model": "gpt-5.6-sol",
                                "reasoning_effort": "high",
                                "developer_instructions": "Keep working until verified."
                            }
                        },
                        "cwd": "/workspace",
                        "personality": "pragmatic",
                        "summary": "detailed",
                        "serviceTier": "priority"
                    }
                }),
            }],
            Some("thread-1"),
        );

        let settings = model.telemetry.thread_settings.as_ref().unwrap();
        assert_eq!(settings.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(settings.model_provider.as_deref(), Some("openai"));
        assert_eq!(settings.effort.as_deref(), Some("high"));
        assert_eq!(
            settings.approval_policy,
            Some(ApprovalPolicySnapshot::Granular {
                granular: GranularApprovalSnapshot {
                    mcp_elicitations: Some(true),
                    rules: Some(false),
                    sandbox_approval: Some(true),
                    request_permissions: Some(true),
                    skill_approval: Some(false),
                }
            })
        );
        assert_eq!(
            settings.sandbox_policy,
            Some(SandboxPolicySnapshot::WorkspaceWrite {
                network_access: Some(true),
                writable_roots: Some(vec!["/workspace".into(), "/tmp/build".into()]),
                exclude_slash_tmp: Some(true),
                exclude_tmpdir_env_var: Some(false),
            })
        );
        assert_eq!(
            settings.active_permission_profile,
            Some(ActivePermissionProfileSnapshot {
                id: Some(":workspace".into()),
                extends: Some("base".into()),
            })
        );
        assert_eq!(
            settings.collaboration_mode,
            Some(CollaborationModeSnapshot {
                mode: Some("default".into()),
                settings: Some(CollaborationModeSettingsSnapshot {
                    model: Some("gpt-5.6-sol".into()),
                    reasoning_effort: Some("high".into()),
                    developer_instructions: Some("Keep working until verified.".into()),
                }),
            })
        );
        assert_eq!(settings.cwd.as_deref(), Some("/workspace"));
        assert_eq!(settings.personality.as_deref(), Some("pragmatic"));
        assert_eq!(settings.summary.as_deref(), Some("detailed"));
        assert_eq!(settings.service_tier.as_deref(), Some("priority"));
        assert!(model.items.is_empty());
        assert_eq!(
            model.raw_events[0]
                .payload
                .pointer("/threadSettings/approvalsReviewer")
                .and_then(Value::as_str),
            Some("auto_review")
        );
    }

    #[test]
    fn unknown_notifications_default_to_activity_not_conversation() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![Event::Notification {
                method: "future/sessionTelemetry/changed".into(),
                params: json!({"threadId": "thread-1", "state": "new"}),
            }],
            Some("thread-1"),
        );

        assert!(model.items.is_empty());
        assert_eq!(model.raw_events.len(), 1);
        assert_eq!(
            model.raw_events[0].method,
            "future/sessionTelemetry/changed"
        );
    }

    #[test]
    fn reasoning_snapshots_are_aggregated_per_turn_and_deduplicated() {
        let mut model = TranscriptModel::default();
        let events = [
            json!({
                "id": "reasoning-1",
                "type": "reasoning",
                "summary": ["Evaluating location", "Planning retrieval"]
            }),
            json!({
                "id": "reasoning-2",
                "type": "reasoning",
                "summary": [
                    "Evaluating location",
                    "Planning retrieval",
                    "Designing scheduler"
                ]
            }),
        ]
        .into_iter()
        .map(|item| Event::Notification {
            method: "item/completed".into(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": item,
            }),
        })
        .collect();

        model.apply_batch(events, Some("thread-1"));

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].kind, TranscriptKind::Reasoning);
        assert_eq!(
            model.items[0].content,
            "Evaluating location\n\nPlanning retrieval\n\nDesigning scheduler"
        );
        assert!(model.items[0].expanded);
        assert_eq!(model.items[0].event_count, 2);
        assert_eq!(model.raw_events.len(), 2);
    }

    #[test]
    fn turn_lifecycle_is_session_state_not_a_transcript_item() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "turn/started".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turn": {"id": "turn-1", "status": "inProgress"}
                    }),
                },
                Event::Notification {
                    method: "turn/completed".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turn": {"id": "turn-1", "status": "completed"}
                    }),
                },
            ],
            Some("thread-1"),
        );

        assert!(model.items.is_empty());
        assert!(model.current_turn_id.is_none());
        assert_eq!(model.raw_events.len(), 2);
    }

    #[test]
    fn authoritative_user_message_reconciles_by_client_id_before_content() {
        let mut model = TranscriptModel::default();
        let optimistic_blocks = json!([
            {"type": "text", "text": "local rendering can differ"},
            {"type": "image", "url": "data:image/png;base64,AQID"}
        ]);
        let (_, local_key) = model.push_local_user(
            "client-message-1",
            "local rendering can differ".into(),
            optimistic_blocks.as_array().unwrap(),
        );

        model.apply_batch(
            vec![Event::Notification {
                method: "item/completed".into(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "server-message-1",
                        "clientId": "client-message-1",
                        "type": "userMessage",
                        "content": [
                            {"type": "text", "text": "canonical rendering"},
                            {"type": "image", "url": "data:image/png;base64,AQID"}
                        ]
                    }
                }),
            }],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].key, "server-message-1");
        assert_eq!(
            model.items[0].protocol_id.as_deref(),
            Some("server-message-1")
        );
        assert_eq!(model.items[0].content, "canonical rendering");
        assert_eq!(
            model.user_image_sources("server-message-1"),
            &[UserImageSource::Url("data:image/png;base64,AQID".into())]
        );
        assert!(model.user_image_sources(&local_key).is_empty());
        assert!(!model.item_indices.contains_key(&local_key));
        assert_eq!(model.item_indices.get("server-message-1"), Some(&0));
    }

    #[test]
    fn queued_user_fallback_is_visible_once_and_reconciles_authoritatively() {
        let mut model = TranscriptModel::default();
        let blocks = json!([{"type": "text", "text": "queued follow-up"}]);

        let (_, local_key) = model
            .ensure_local_user(
                "queued-client-1",
                "queued follow-up".into(),
                blocks.as_array().unwrap(),
            )
            .expect("a missing queued user item should receive a local fallback");
        assert!(
            model
                .ensure_local_user(
                    "queued-client-1",
                    "queued follow-up".into(),
                    blocks.as_array().unwrap(),
                )
                .is_none()
        );

        model.apply_batch(
            vec![Event::Notification {
                method: "item/completed".into(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "queued-server-message-1",
                        "clientId": "queued-client-1",
                        "type": "userMessage",
                        "content": [{"type": "text", "text": "queued follow-up"}]
                    }
                }),
            }],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].key, "queued-server-message-1");
        assert!(!model.item_indices.contains_key(&local_key));
        assert!(
            model
                .ensure_local_user(
                    "queued-client-1",
                    "queued follow-up".into(),
                    blocks.as_array().unwrap(),
                )
                .is_none()
        );
    }

    #[test]
    fn structured_images_render_without_transport_labels() {
        let content = json!([
            {"type": "image", "url": "data:image/png;base64,AQID"},
            {"type": "text", "text": "[Image #1] what is this?\nkeep this line"}
        ]);

        assert_eq!(
            render_user_content(&content),
            "what is this?\nkeep this line"
        );
        assert_eq!(
            strip_structured_image_labels("[Image #2] literal reference", 1),
            "[Image #2] literal reference"
        );
        assert_eq!(
            strip_structured_image_labels("[Image #1]\ncaption", 1),
            "caption"
        );
        assert_eq!(
            strip_structured_image_labels("[Image #1] literal text", 0),
            "[Image #1] literal text"
        );
    }

    #[test]
    fn legacy_user_message_reconciliation_normalizes_attachment_placeholders() {
        let mut model = TranscriptModel::default();
        let optimistic_blocks = json!([
            {"type": "text", "text": "describe this"},
            {"type": "image", "url": "data:image/png;base64,AQID"}
        ]);
        model.push_local_user(
            "client-message-without-echo",
            "describe this\n\n[Attached image]".into(),
            optimistic_blocks.as_array().unwrap(),
        );

        model.apply_batch(
            vec![Event::Notification {
                method: "item/completed".into(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "legacy-server-message",
                        "clientId": null,
                        "type": "userMessage",
                        "content": [
                            {"type": "text", "text": "describe this"},
                            {"type": "image", "url": "data:image/png;base64,AQID"}
                        ]
                    }
                }),
            }],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].key, "legacy-server-message");
    }

    #[test]
    fn completed_turn_reconciles_items_missed_by_streaming_notifications() {
        let mut model = TranscriptModel {
            current_turn_id: Some("turn-1".into()),
            ..Default::default()
        };
        let outcome = model.apply_batch(
            vec![Event::Notification {
                method: "turn/completed".into(),
                params: json!({
                    "threadId": "thread-1",
                    "turn": {
                        "id": "turn-1",
                        "status": "completed",
                        "items": [
                            {
                                "id": "agent-1",
                                "type": "agentMessage",
                                "text": "Authoritative final answer"
                            },
                            {
                                "id": "command-1",
                                "type": "commandExecution",
                                "command": "cargo test",
                                "aggregatedOutput": "all tests passed",
                                "status": "completed"
                            }
                        ]
                    }
                }),
            }],
            Some("thread-1"),
        );

        assert!(model.current_turn_id.is_none());
        assert!(outcome.refresh_threads);
        assert_eq!(model.items.len(), 2);
        assert_eq!(model.items[0].protocol_id.as_deref(), Some("agent-1"));
        assert_eq!(model.items[0].content, "Authoritative final answer");
        assert_eq!(model.items[1].protocol_id.as_deref(), Some("command-1"));
        assert!(model.items[1].content.contains("all tests passed"));
        assert_eq!(model.items[1].status.as_deref(), Some("completed"));
    }

    #[test]
    fn failed_and_interrupted_turn_completions_are_visible() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "turn/completed".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turn": {
                            "id": "turn-failed",
                            "status": "failed",
                            "items": [],
                            "error": {
                                "message": "The model connection closed",
                                "additionalDetails": "Retry the turn"
                            }
                        }
                    }),
                },
                Event::Notification {
                    method: "turn/completed".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turn": {
                            "id": "turn-interrupted",
                            "status": "interrupted",
                            "items": []
                        }
                    }),
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 2);
        assert_eq!(model.items[0].kind, TranscriptKind::Error);
        assert_eq!(model.items[0].title, "Turn failed");
        assert_eq!(model.items[0].status.as_deref(), Some("failed"));
        assert!(
            model.items[0]
                .content
                .contains("The model connection closed")
        );
        assert!(model.items[0].content.contains("Retry the turn"));
        assert_eq!(model.items[1].kind, TranscriptKind::Review);
        assert_eq!(model.items[1].title, "Turn interrupted");
        assert_eq!(model.items[1].status.as_deref(), Some("interrupted"));
        assert!(
            model.items[1]
                .content
                .contains("interrupted before it completed")
        );
    }

    #[test]
    fn generated_thread_name_updates_use_the_v2_field() {
        let mut model = TranscriptModel::default();
        let outcome = model.apply_batch(
            vec![Event::Notification {
                method: "thread/name/updated".into(),
                params: json!({
                    "threadId": "thread-1",
                    "threadName": "Inspect streaming tool cards"
                }),
            }],
            Some("thread-1"),
        );

        assert_eq!(
            outcome.renamed_thread.as_deref(),
            Some("Inspect streaming tool cards")
        );
        assert!(model.items.is_empty());
    }

    #[test]
    fn resolved_server_requests_stop_presenting_approval_controls() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::ServerRequest {
                    id: json!(17),
                    method: "item/commandExecution/requestApproval".into(),
                    params: json!({"command": "cargo test"}),
                },
                Event::Notification {
                    method: "serverRequest/resolved".into(),
                    params: json!({"requestId": 17, "threadId": "thread-1"}),
                },
            ],
            None,
        );

        assert!(model.items[0].pending_request.as_ref().unwrap().resolved);
        assert_eq!(model.items[0].status.as_deref(), Some("resolved"));
        assert_eq!(model.raw_events.len(), 2);
    }

    #[test]
    fn cross_task_server_requests_do_not_contaminate_the_selected_transcript() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::ServerRequest {
                    id: json!(17),
                    method: "item/commandExecution/requestApproval".into(),
                    params: json!({
                        "threadId": "thread-2",
                        "command": "foreign command"
                    }),
                },
                Event::ServerRequest {
                    id: json!(18),
                    method: "currentTime/read".into(),
                    params: json!({}),
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].key, "request:18");
        assert_eq!(
            model.items[0].pending_request.as_ref().unwrap().method,
            "currentTime/read"
        );
        assert_eq!(model.raw_events.len(), 1);
        assert_eq!(model.raw_events[0].method, "currentTime/read");
    }

    #[test]
    fn minimal_document_edits_respect_utf8_boundaries() {
        let old = "━━━━ Codex\nhello 🌅 world\n";
        let new = "━━━━ Codex\nhello 🌇 world\n";
        let (range, replacement) = minimal_text_edit(old, new);
        let mut edited = old.to_string();
        edited.replace_range(range, &replacement);
        assert_eq!(edited, new);
    }

    #[test]
    fn minimal_document_edits_handle_append_and_truncation() {
        for (old, new) in [("alpha", "alpha beta"), ("alpha beta", "alpha")] {
            let (range, replacement) = minimal_text_edit(old, new);
            let mut edited = old.to_string();
            edited.replace_range(range, &replacement);
            assert_eq!(edited, new);
        }
    }

    #[test]
    fn forward_search_advances_across_unicode_cursor() {
        let text = "before ━ sunrise\nafter sunrise";
        let cursor = text.find('━').unwrap();

        assert_eq!(
            find_wrapped_match(text, "sunrise", cursor, false),
            text.find("sunrise")
        );
    }

    #[test]
    fn forward_search_wraps_without_slicing_inside_unicode() {
        let text = "sunrise first\nend ━";
        let cursor = text.find('━').unwrap();

        assert_eq!(find_wrapped_match(text, "sunrise", cursor, false), Some(0));
    }

    #[test]
    fn backwards_search_wraps_across_unicode() {
        let text = "sunrise first\nend ━ sunrise last";
        let cursor = text.find("sunrise first").unwrap();

        assert_eq!(
            find_wrapped_match(text, "sunrise", cursor, true),
            text.rfind("sunrise")
        );
    }

    #[test]
    fn hook_notifications_update_one_semantic_tool_item() {
        let mut model = TranscriptModel::default();
        let run = |status: &str, entries: Value| {
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "run": {
                    "id": "hook-1",
                    "eventName": "postToolUse",
                    "handlerType": "command",
                    "executionMode": "sync",
                    "sourcePath": "/tmp/hooks/check.sh",
                    "status": status,
                    "entries": entries,
                    "displayOrder": 0,
                    "startedAt": 1
                }
            })
        };
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "hook/started".into(),
                    params: run("running", json!([])),
                },
                Event::Notification {
                    method: "hook/completed".into(),
                    params: run(
                        "completed",
                        json!([{"kind": "stdout", "text": "hook passed"}]),
                    ),
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].kind, TranscriptKind::Tool);
        assert_eq!(model.items[0].title, "Hook · Post Tool Use");
        assert_eq!(model.items[0].status.as_deref(), Some("completed"));
        assert!(model.items[0].content.contains("hook passed"));
        assert_eq!(model.items[0].event_count, 2);
    }

    #[test]
    fn approval_review_updates_the_target_instead_of_adding_junk_rows() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "item/started".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "command-1",
                            "type": "commandExecution",
                            "command": "cargo test",
                            "status": "inProgress"
                        }
                    }),
                },
                Event::Notification {
                    method: "item/autoApprovalReview/started".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "reviewId": "review-1",
                        "targetItemId": "command-1",
                        "startedAtMs": 1,
                        "action": {"type": "command", "command": "cargo test", "cwd": "/tmp", "source": "tool"},
                        "review": {"status": "inProgress"}
                    }),
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].kind, TranscriptKind::Command);
        assert_eq!(model.items[0].status.as_deref(), Some("in progress"));
        assert!(model.items[0].raw.get("approvalReview").is_some());
    }

    #[test]
    fn newer_user_visible_notifications_receive_semantic_surfaces() {
        let mut model = TranscriptModel::default();
        model.apply_batch(
            vec![
                Event::Notification {
                    method: "warning".into(),
                    params: json!({"threadId": "thread-1", "message": "Check the generated command"}),
                },
                Event::Notification {
                    method: "model/rerouted".into(),
                    params: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "fromModel": "gpt-a",
                        "toModel": "gpt-b",
                        "reason": "highRiskCyberActivity"
                    }),
                },
                Event::Notification {
                    method: "thread/realtime/transcript/done".into(),
                    params: json!({"threadId": "thread-1", "role": "assistant", "text": "Final realtime text"}),
                },
            ],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 3);
        assert_eq!(model.items[0].kind, TranscriptKind::Error);
        assert_eq!(model.items[1].kind, TranscriptKind::Review);
        assert!(model.items[1].title.contains("gpt-a → gpt-b"));
        assert_eq!(model.items[2].kind, TranscriptKind::Agent);
        assert_eq!(model.items[2].content, "Final realtime text");
    }

    #[test]
    fn disconnect_is_semantic_prose_with_exact_raw_payload_retained() {
        let mut model = TranscriptModel::default();
        let outcome = model.apply_batch(
            vec![Event::Disconnected {
                reason: "app-server closed stdout".into(),
            }],
            None,
        );

        assert_eq!(
            outcome.transport_error.as_deref(),
            Some("app-server closed stdout")
        );
        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].title, "App Server disconnected");
        assert_eq!(model.items[0].content, "app-server closed stdout");
        assert_eq!(model.items[0].status.as_deref(), Some("offline"));
        assert_eq!(
            model.items[0].raw,
            json!({"reason": "app-server closed stdout"})
        );
        assert!(!model.items[0].content.contains('{'));
    }

    #[test]
    fn large_stream_payloads_are_semantic_but_not_retained_raw() {
        let mut model = TranscriptModel::default();
        let encoded_audio = "A".repeat(RAW_STRING_LIMIT + 8_192);
        model.apply_batch(
            vec![Event::Notification {
                method: "thread/realtime/outputAudio/delta".into(),
                params: json!({
                    "threadId": "thread-1",
                    "audio": {
                        "data": encoded_audio,
                        "itemId": "audio-1",
                        "numChannels": 1,
                        "sampleRate": 24_000
                    }
                }),
            }],
            Some("thread-1"),
        );

        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].title, "Realtime audio");
        assert!(model.items[0].content.contains("24000 Hz"));
        let item_raw = serde_json::to_string(&model.items[0].raw).unwrap();
        let journal_raw = serde_json::to_string(&model.raw_events[0].payload).unwrap();
        assert!(item_raw.contains("bytes omitted"));
        assert!(journal_raw.contains("bytes omitted"));
        assert!(item_raw.len() < 2_048);
        assert!(journal_raw.len() < 2_048);
    }
}
