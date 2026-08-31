//! Async client for local Codex app-server transports.
//!
//! The embedded transport speaks newline-delimited JSON over a directly owned
//! app-server child. The managed transport speaks WebSocket text frames through
//! an owned proxy child connected to a separately managed app-server daemon.
//! Both transports carry messages shaped like JSON-RPC 2.0 with the `jsonrpc`
//! field omitted. This crate correlates responses with requests and exposes
//! notifications and server-initiated requests as an event stream.

use std::{
    collections::HashMap,
    ffi::OsStr,
    io,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_process::{Child, ChildStdin, ChildStdout, Command};
use async_tungstenite::tungstenite::Message as WebSocketMessage;
use futures::channel::oneshot;
use futures::io::{AsyncRead, AsyncWrite};
use futures_lite::{
    StreamExt,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
};
use parking_lot::Mutex;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use thiserror::Error;

type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Error>>>>>;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    UnmatchedResponse {
        id: Value,
        result: Option<Value>,
        error: Option<RpcError>,
    },
    Disconnected {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodexThread {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    #[serde(default)]
    pub forked_from_id: Option<String>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub source: CodexSessionSource,
    #[serde(default)]
    pub thread_source: Option<String>,
    #[serde(default)]
    pub status: CodexThreadStatus,
    #[serde(default)]
    pub agent_nickname: Option<String>,
    #[serde(default)]
    pub agent_role: Option<String>,
    #[serde(default)]
    pub can_accept_direct_input: Option<bool>,
    #[serde(default)]
    pub turns: Vec<CodexTurn>,
}

impl CodexThread {
    /// Returns the direct parent reported by app-server, with a fallback to
    /// the source metadata written by older sub-agent rollouts.
    pub fn effective_parent_thread_id(&self) -> Option<&str> {
        self.parent_thread_id.as_deref().or_else(|| {
            let CodexSessionSource::SubAgent(CodexSubagentSource::ThreadSpawn(spawn)) =
                &self.source
            else {
                return None;
            };
            Some(spawn.parent_thread_id.as_str())
        })
    }
}

/// The runtime status app-server reports for a thread.
///
/// Unknown variants retain their raw payload so a newer app-server does not
/// make an older client unable to list threads.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    Active { active_flags: Vec<String> },
    Unknown(Value),
}

impl Default for CodexThreadStatus {
    fn default() -> Self {
        Self::Unknown(Value::Null)
    }
}

impl<'de> Deserialize<'de> for CodexThreadStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let status = value.get("type").and_then(Value::as_str);
        Ok(match status {
            Some("notLoaded") => Self::NotLoaded,
            Some("idle") => Self::Idle,
            Some("systemError") => Self::SystemError,
            Some("active") => Self::Active {
                active_flags: value
                    .get("activeFlags")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            },
            _ => Self::Unknown(value),
        })
    }
}

/// Origin of a Codex session. Sub-agent sessions carry the spawn metadata
/// needed to reconstruct a hierarchy even when older records omit the newer
/// top-level convenience fields.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexSessionSource {
    Cli,
    VsCode,
    Exec,
    AppServer,
    Custom(String),
    SubAgent(CodexSubagentSource),
    Unknown(Value),
}

impl Default for CodexSessionSource {
    fn default() -> Self {
        Self::Unknown(Value::Null)
    }
}

impl<'de> Deserialize<'de> for CodexSessionSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(match &value {
            Value::String(source) => match source.as_str() {
                "cli" => Self::Cli,
                "vscode" => Self::VsCode,
                "exec" => Self::Exec,
                "appServer" => Self::AppServer,
                _ => Self::Unknown(value),
            },
            Value::Object(source) => {
                if let Some(custom) = source.get("custom").and_then(Value::as_str) {
                    Self::Custom(custom.to_owned())
                } else if let Some(subagent) = source.get("subAgent") {
                    Self::SubAgent(CodexSubagentSource::from_value(subagent.clone()))
                } else {
                    Self::Unknown(value)
                }
            }
            _ => Self::Unknown(value),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexSubagentSource {
    Review,
    Compact,
    MemoryConsolidation,
    ThreadSpawn(CodexThreadSpawnSource),
    Other(String),
    Unknown(Value),
}

impl CodexSubagentSource {
    fn from_value(value: Value) -> Self {
        match &value {
            Value::String(source) => match source.as_str() {
                "review" => Self::Review,
                "compact" => Self::Compact,
                "memory_consolidation" => Self::MemoryConsolidation,
                _ => Self::Unknown(value),
            },
            Value::Object(source) => {
                if let Some(other) = source.get("other").and_then(Value::as_str) {
                    Self::Other(other.to_owned())
                } else if let Some(spawn) = source.get("thread_spawn") {
                    serde_json::from_value(spawn.clone())
                        .map(Self::ThreadSpawn)
                        .unwrap_or(Self::Unknown(value))
                } else {
                    Self::Unknown(value)
                }
            }
            _ => Self::Unknown(value),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexThreadSpawnSource {
    pub parent_thread_id: String,
    pub depth: i32,
    #[serde(default)]
    pub agent_nickname: Option<String>,
    #[serde(default)]
    pub agent_role: Option<String>,
    #[serde(default)]
    pub agent_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexTurn {
    pub id: String,
    #[serde(default)]
    pub status: Value,
    #[serde(default)]
    pub items: Vec<CodexThreadItem>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexThreadItem {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub body: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    pub data: Vec<CodexThread>,
    #[serde(default)]
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub backwards_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ThreadReadResponse {
    pub thread: CodexThread,
}

/// A started or resumed thread together with the settings that the app-server
/// actually resolved. Clients should display these values instead of inferring
/// them from local config, since managed settings and permission profiles may
/// override the requested defaults.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadOpenResponse {
    pub thread: CodexThread,
    #[serde(default)]
    pub cwd: String,
    pub model: String,
    #[serde(default)]
    pub model_provider: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub approval_policy: Value,
    #[serde(default)]
    pub sandbox: Value,
    #[serde(default)]
    pub active_permission_profile: Option<Value>,
    #[serde(default)]
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Incoming {
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<RpcError>,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to launch codex app-server: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("app-server stdio was unavailable")]
    MissingStdio,
    #[error("app-server transport closed")]
    TransportClosed,
    #[error("app-server response channel closed")]
    ResponseChannelClosed,
    #[error("app-server emitted invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to run `codex app-server daemon start`: {0}")]
    DaemonStart(#[source] std::io::Error),
    #[error("`codex app-server daemon start` exited with {status}: {stderr}")]
    DaemonStartFailed { status: String, stderr: String },
    #[error("invalid app-server daemon lifecycle response: {0}")]
    InvalidDaemonLifecycle(String),
    #[error("failed to launch codex app-server proxy: {0}")]
    ProxySpawn(#[source] std::io::Error),
    #[error("failed to connect to app-server proxy WebSocket: {0}")]
    WebSocketHandshake(String),
    #[error("invalid app-server message: {0}")]
    InvalidMessage(String),
    #[error("app-server returned error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

#[derive(Debug, Deserialize)]
// Require every lifecycle invariant we rely on, but allow newer Codex releases
// to add informational fields without making an otherwise compatible daemon
// impossible to attach to.
#[serde(rename_all = "camelCase")]
struct DaemonLifecycleOutput {
    status: DaemonLifecycleStatus,
    backend: DaemonBackend,
    pid: u32,
    managed_codex_path: PathBuf,
    managed_codex_version: String,
    socket_path: PathBuf,
    cli_version: String,
    app_server_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum DaemonLifecycleStatus {
    Started,
    AlreadyRunning,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DaemonBackend {
    Pid,
}

struct SplitDuplex<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> SplitDuplex<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for SplitDuplex<R, W> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.reader).poll_read(cx, buffer)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for SplitDuplex<R, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_close(cx)
    }
}

pub struct Client {
    outbound: async_channel::Sender<Value>,
    events: async_channel::Receiver<Event>,
    pending: PendingRequests,
    next_id: AtomicU64,
    _child: Child,
    _reader_task: smol::Task<()>,
    _writer_task: smol::Task<()>,
}

impl Client {
    pub fn launch(codex: impl AsRef<OsStr>) -> Result<Self, Error> {
        let mut command = Command::new(codex);
        command
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(Error::Spawn)?;
        let stdin = child.stdin.take().ok_or(Error::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(Error::MissingStdio)?;
        let (outbound_tx, outbound_rx) = async_channel::unbounded::<Value>();
        let (event_tx, event_rx) = async_channel::unbounded::<Event>();
        let pending = PendingRequests::default();

        let writer_pending = pending.clone();
        let writer_events = event_tx.clone();
        let writer_task = smol::spawn(async move {
            let mut stdin = stdin;
            while let Ok(message) = outbound_rx.recv().await {
                let write_result = async {
                    let bytes = serde_json::to_vec(&message)?;
                    stdin.write_all(&bytes).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                }
                .await;

                if let Err(error) = write_result {
                    disconnect(
                        &writer_pending,
                        &writer_events,
                        format!("failed to write to app-server: {error}"),
                    )
                    .await;
                    break;
                }
            }
        });

        let reader_pending = pending.clone();
        let reader_events = event_tx;
        let reader_outbound = outbound_tx.clone();
        let reader_task = smol::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let trace = app_server_trace_enabled();
            loop {
                let read_started_at = trace.then(Instant::now);
                let Some(line) = lines.next().await else {
                    break;
                };
                let read_elapsed = read_started_at.map(|started_at| started_at.elapsed());
                let (incoming, line_bytes, decode_elapsed) = match line {
                    Ok(line) => {
                        let line_bytes = line.len();
                        let decode_started_at = trace.then(Instant::now);
                        let incoming = decode_line(&line);
                        let decode_elapsed =
                            decode_started_at.map(|started_at| started_at.elapsed());
                        (incoming, line_bytes, decode_elapsed)
                    }
                    Err(error) => {
                        let reason = format!("failed to read from app-server: {error}");
                        disconnect(&reader_pending, &reader_events, reason).await;
                        return;
                    }
                };
                if let (Some(read_elapsed), Some(decode_elapsed)) = (read_elapsed, decode_elapsed) {
                    eprintln!(
                        "app-server-line bytes={line_bytes} wait_read_ms={:.1} decode_ms={:.1}",
                        read_elapsed.as_secs_f64() * 1_000.,
                        decode_elapsed.as_secs_f64() * 1_000.,
                    );
                }

                let incoming = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => {
                        disconnect(&reader_pending, &reader_events, error.to_string()).await;
                        return;
                    }
                };

                if !dispatch_incoming(incoming, &reader_pending, &reader_events, &reader_outbound)
                    .await
                {
                    return;
                }
            }

            disconnect(
                &reader_pending,
                &reader_events,
                "app-server closed stdout".into(),
            )
            .await;
        });

        Ok(Self {
            outbound: outbound_tx,
            events: event_rx,
            pending,
            next_id: AtomicU64::new(0),
            _child: child,
            _reader_task: reader_task,
            _writer_task: writer_task,
        })
    }

    /// Connect through Codex's restart-safe app-server daemon.
    ///
    /// The daemon start command is idempotent and is fully reaped before an
    /// owned stdio proxy is launched. Dropping this client terminates only the
    /// proxy; daemon socket and process lifecycle remain owned by Codex.
    pub async fn launch_managed(codex: impl AsRef<OsStr>) -> Result<Self, Error> {
        let codex = codex.as_ref();
        let socket_path = start_managed_daemon(codex).await?;

        let mut command = Command::new(codex);
        command
            .args(["app-server", "proxy", "--sock"])
            .arg(socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(Error::ProxySpawn)?;
        let stdin = child.stdin.take().ok_or(Error::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(Error::MissingStdio)?;
        let duplex = SplitDuplex::<ChildStdout, ChildStdin>::new(stdout, stdin);
        let (websocket, _) = async_tungstenite::client_async("ws://localhost/rpc", duplex)
            .await
            .map_err(|error| Error::WebSocketHandshake(error.to_string()))?;
        let (mut websocket_writer, mut websocket_reader) = futures::StreamExt::split(websocket);

        let (outbound_tx, outbound_rx) = async_channel::unbounded::<Value>();
        let (event_tx, event_rx) = async_channel::unbounded::<Event>();
        let pending = PendingRequests::default();

        let writer_pending = pending.clone();
        let writer_events = event_tx.clone();
        let writer_task = smol::spawn(async move {
            while let Ok(message) = outbound_rx.recv().await {
                let payload = match serde_json::to_string(&message) {
                    Ok(payload) => payload,
                    Err(error) => {
                        disconnect(
                            &writer_pending,
                            &writer_events,
                            format!("failed to encode app-server message: {error}"),
                        )
                        .await;
                        return;
                    }
                };
                if let Err(error) = futures::SinkExt::send(
                    &mut websocket_writer,
                    WebSocketMessage::Text(payload.into()),
                )
                .await
                {
                    disconnect(
                        &writer_pending,
                        &writer_events,
                        format!("failed to write to app-server WebSocket: {error}"),
                    )
                    .await;
                    return;
                }
            }
        });

        let reader_pending = pending.clone();
        let reader_events = event_tx;
        let reader_outbound = outbound_tx.clone();
        let reader_task = smol::spawn(async move {
            let trace = app_server_trace_enabled();
            let disconnect_reason = loop {
                let read_started_at = trace.then(Instant::now);
                let message = futures::StreamExt::next(&mut websocket_reader).await;
                let read_elapsed = read_started_at.map(|started_at| started_at.elapsed());
                let text = match message {
                    Some(Ok(WebSocketMessage::Text(text))) => text,
                    Some(Ok(WebSocketMessage::Close(frame))) => {
                        break format!("app-server closed WebSocket: {frame:?}");
                    }
                    Some(Ok(WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_))) => continue,
                    Some(Ok(WebSocketMessage::Binary(_))) => {
                        break "app-server emitted an unexpected binary WebSocket frame".into();
                    }
                    Some(Ok(WebSocketMessage::Frame(_))) => {
                        break "app-server emitted an unexpected raw WebSocket frame".into();
                    }
                    Some(Err(error)) => {
                        break format!("failed to read from app-server WebSocket: {error}");
                    }
                    None => break "app-server proxy closed WebSocket".into(),
                };

                let decode_started_at = trace.then(Instant::now);
                let incoming = decode_line(text.as_str());
                let decode_elapsed = decode_started_at.map(|started_at| started_at.elapsed());
                if let (Some(read_elapsed), Some(decode_elapsed)) = (read_elapsed, decode_elapsed) {
                    eprintln!(
                        "app-server-frame bytes={} wait_read_ms={:.1} decode_ms={:.1}",
                        text.len(),
                        read_elapsed.as_secs_f64() * 1_000.,
                        decode_elapsed.as_secs_f64() * 1_000.,
                    );
                }

                let incoming = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => break error.to_string(),
                };
                if !dispatch_incoming(incoming, &reader_pending, &reader_events, &reader_outbound)
                    .await
                {
                    return;
                }
            };

            disconnect(&reader_pending, &reader_events, disconnect_reason).await;
        });

        Ok(Self {
            outbound: outbound_tx,
            events: event_rx,
            pending,
            next_id: AtomicU64::new(0),
            _child: child,
            _reader_task: reader_task,
            _writer_task: writer_task,
        })
    }

    pub async fn initialize(&self, name: &str, title: &str, version: &str) -> Result<Value, Error> {
        let result = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": name,
                        "title": title,
                        "version": version,
                    },
                    "capabilities": {
                        "experimentalApi": true,
                        "extensions": {
                            "openai/form": {}
                        },
                        "mcpServerOpenaiFormElicitation": true,
                        "requestAttestation": false
                    },
                }),
            )
            .await?;
        self.notify("initialized", json!({})).await?;
        Ok(result)
    }

    pub async fn list_threads(
        &self,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ThreadListResponse, Error> {
        self.list_threads_with_filters(limit, cursor, None, None)
            .await
    }

    /// List sessions created by the collaboration `spawnAgent` tool.
    ///
    /// App-server deliberately defaults an unfiltered `thread/list` request to
    /// interactive sources. Keeping this as a separate request preserves that
    /// root-thread behavior while allowing clients to build a hierarchy without
    /// also exposing review/compaction maintenance sessions.
    pub async fn list_spawned_subagent_threads(
        &self,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ThreadListResponse, Error> {
        self.list_threads_with_filters(limit, cursor, None, Some(&["subAgentThreadSpawn"]))
            .await
    }

    /// List threads spawned directly by `parent_thread_id`.
    pub async fn list_child_threads(
        &self,
        parent_thread_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ThreadListResponse, Error> {
        self.list_threads_with_filters(
            limit,
            cursor,
            Some(ThreadListRelationship::DirectChildren(parent_thread_id)),
            Some(&["subAgentThreadSpawn"]),
        )
        .await
    }

    /// List every spawned descendant of `ancestor_thread_id`, at any depth.
    /// The ancestor itself is not included in the response.
    pub async fn list_descendant_threads(
        &self,
        ancestor_thread_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ThreadListResponse, Error> {
        self.list_threads_with_filters(
            limit,
            cursor,
            Some(ThreadListRelationship::Descendants(ancestor_thread_id)),
            Some(&["subAgentThreadSpawn"]),
        )
        .await
    }

    async fn list_threads_with_filters(
        &self,
        limit: usize,
        cursor: Option<&str>,
        relationship: Option<ThreadListRelationship<'_>>,
        source_kinds: Option<&[&str]>,
    ) -> Result<ThreadListResponse, Error> {
        let response = self
            .request(
                "thread/list",
                thread_list_params(limit, cursor, relationship, source_kinds),
            )
            .await?;
        Ok(serde_json::from_value(response)?)
    }

    pub async fn read_thread(&self, thread_id: &str) -> Result<CodexThread, Error> {
        let response = self
            .request(
                "thread/read",
                json!({
                    "includeTurns": true,
                    "threadId": thread_id,
                }),
            )
            .await?;
        Ok(serde_json::from_value::<ThreadReadResponse>(response)?.thread)
    }

    /// Start a persistent Codex thread rooted in `cwd` and subscribe this
    /// connection to every event it emits.
    pub async fn start_thread(&self, cwd: &str) -> Result<CodexThread, Error> {
        Ok(self.start_thread_with_settings(cwd).await?.thread)
    }

    pub async fn start_thread_with_settings(&self, cwd: &str) -> Result<ThreadOpenResponse, Error> {
        let response = self
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "experimentalRawEvents": true,
                    "sessionStartSource": "startup",
                    "threadSource": "harness",
                }),
            )
            .await?;
        decode_thread_open_response(response)
    }

    /// Resume an existing thread and subscribe this connection to its live
    /// events. The response includes all turns so a client can render it
    /// without a lossy intermediary protocol.
    pub async fn resume_thread(&self, thread_id: &str) -> Result<CodexThread, Error> {
        Ok(self.resume_thread_with_settings(thread_id).await?.thread)
    }

    pub async fn resume_thread_with_settings(
        &self,
        thread_id: &str,
    ) -> Result<ThreadOpenResponse, Error> {
        self.resume_thread_with_exclude_turns(thread_id, false)
            .await
    }

    /// Attach this connection to an existing thread's live events without
    /// returning its historical turns. This is intended for reconnect paths
    /// that already retain the rendered transcript.
    pub async fn attach_thread_with_settings(
        &self,
        thread_id: &str,
    ) -> Result<ThreadOpenResponse, Error> {
        self.resume_thread_with_exclude_turns(thread_id, true).await
    }

    async fn resume_thread_with_exclude_turns(
        &self,
        thread_id: &str,
        exclude_turns: bool,
    ) -> Result<ThreadOpenResponse, Error> {
        let response = self
            .request(
                "thread/resume",
                thread_resume_params(thread_id, exclude_turns),
            )
            .await?;
        decode_thread_open_response(response)
    }

    pub async fn start_turn(&self, thread_id: &str, input: Value) -> Result<Value, Error> {
        self.start_turn_with_client_user_message_id(thread_id, input, None)
            .await
    }

    pub async fn start_turn_with_client_user_message_id(
        &self,
        thread_id: &str,
        input: Value,
        client_user_message_id: Option<&str>,
    ) -> Result<Value, Error> {
        self.request(
            "turn/start",
            turn_start_params(thread_id, input, client_user_message_id),
        )
        .await
    }

    pub async fn interrupt_turn(&self, thread_id: &str, turn_id: &str) -> Result<Value, Error> {
        self.request(
            "turn/interrupt",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
            }),
        )
        .await
    }

    /// Add input to the active turn without interrupting it. The expected turn
    /// id makes a stale composer submission fail instead of steering a newer
    /// turn accidentally.
    pub async fn steer_turn(
        &self,
        thread_id: &str,
        expected_turn_id: &str,
        input: Value,
        client_user_message_id: &str,
    ) -> Result<Value, Error> {
        self.request(
            "turn/steer",
            turn_steer_params(thread_id, expected_turn_id, input, client_user_message_id),
        )
        .await
    }

    /// Persist input in Codex's thread-owned queue. Unlike a Harness-local
    /// queue this remains authoritative across windows and process restarts.
    pub async fn queue_turn(
        &self,
        thread_id: &str,
        input: Value,
        client_user_message_id: &str,
    ) -> Result<Value, Error> {
        self.request(
            "thread/queue/add",
            thread_queue_add_params(thread_id, input, client_user_message_id),
        )
        .await
    }

    pub async fn list_queued_turns(&self, thread_id: &str) -> Result<Value, Error> {
        self.request("thread/queue/list", thread_queue_list_params(thread_id))
            .await
    }

    pub async fn update_queued_turn(
        &self,
        thread_id: &str,
        queued_submission_id: &str,
        input: Value,
    ) -> Result<Value, Error> {
        self.request(
            "thread/queue/update",
            thread_queue_update_params(thread_id, queued_submission_id, input),
        )
        .await
    }

    pub async fn delete_queued_turn(
        &self,
        thread_id: &str,
        queued_submission_id: &str,
    ) -> Result<Value, Error> {
        self.request(
            "thread/queue/delete",
            thread_queue_delete_params(thread_id, queued_submission_id),
        )
        .await
    }

    pub async fn reorder_queued_turns(
        &self,
        thread_id: &str,
        queued_submission_ids: Vec<String>,
    ) -> Result<Value, Error> {
        self.request(
            "thread/queue/reorder",
            thread_queue_reorder_params(thread_id, queued_submission_ids),
        )
        .await
    }

    pub async fn start_next_queued_turn(&self, thread_id: &str) -> Result<Value, Error> {
        self.start_queued_turn(thread_id, None).await
    }

    pub async fn start_queued_turn(
        &self,
        thread_id: &str,
        queued_submission_id: Option<&str>,
    ) -> Result<Value, Error> {
        self.request(
            "thread/queue/start",
            thread_queue_start_params(thread_id, queued_submission_id),
        )
        .await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().insert(id, response_tx);

        if self
            .outbound
            .send(json!({ "method": method, "id": id, "params": params }))
            .await
            .is_err()
        {
            self.pending.lock().remove(&id);
            return Err(Error::TransportClosed);
        }

        response_rx
            .await
            .map_err(|_| Error::ResponseChannelClosed)?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), Error> {
        self.send(json!({ "method": method, "params": params }))
            .await
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<(), Error> {
        self.send(json!({ "id": id, "result": result })).await
    }

    pub async fn respond_error(
        &self,
        id: Value,
        code: i64,
        message: impl Into<String>,
    ) -> Result<(), Error> {
        self.send(json!({
            "id": id,
            "error": { "code": code, "message": message.into() }
        }))
        .await
    }

    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }

    async fn send(&self, value: Value) -> Result<(), Error> {
        self.outbound
            .send(value)
            .await
            .map_err(|_| Error::TransportClosed)
    }
}

async fn start_managed_daemon(codex: &OsStr) -> Result<PathBuf, Error> {
    let output = Command::new(codex)
        .args(["app-server", "daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(Error::DaemonStart)?;

    if !output.status.success() {
        return Err(Error::DaemonStartFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    parse_daemon_lifecycle(&output.stdout)
}

fn parse_daemon_lifecycle(stdout: &[u8]) -> Result<PathBuf, Error> {
    let lifecycle: DaemonLifecycleOutput = serde_json::from_slice(stdout)
        .map_err(|error| Error::InvalidDaemonLifecycle(error.to_string()))?;

    // Deserializing the closed enums above verifies the accepted status and
    // managed PID backend. Validate the remaining cross-field invariants here.
    let _ = (&lifecycle.status, &lifecycle.backend);
    if lifecycle.pid == 0 {
        return Err(Error::InvalidDaemonLifecycle(
            "managed daemon pid must be non-zero".into(),
        ));
    }
    if lifecycle.managed_codex_path.as_os_str().is_empty() {
        return Err(Error::InvalidDaemonLifecycle(
            "managedCodexPath must not be empty".into(),
        ));
    }
    if !lifecycle.socket_path.is_absolute() {
        return Err(Error::InvalidDaemonLifecycle(
            "socketPath must be absolute".into(),
        ));
    }
    if lifecycle.cli_version.is_empty()
        || lifecycle.managed_codex_version.is_empty()
        || lifecycle.app_server_version.is_empty()
    {
        return Err(Error::InvalidDaemonLifecycle(
            "cliVersion, managedCodexVersion, and appServerVersion must be present".into(),
        ));
    }
    if lifecycle.cli_version != lifecycle.managed_codex_version
        || lifecycle.cli_version != lifecycle.app_server_version
    {
        return Err(Error::InvalidDaemonLifecycle(format!(
            "version mismatch: cliVersion={}, managedCodexVersion={}, appServerVersion={}",
            lifecycle.cli_version, lifecycle.managed_codex_version, lifecycle.app_server_version,
        )));
    }

    Ok(lifecycle.socket_path)
}

async fn dispatch_incoming(
    incoming: Incoming,
    pending: &PendingRequests,
    events: &async_channel::Sender<Event>,
    outbound: &async_channel::Sender<Value>,
) -> bool {
    match incoming {
        Incoming::Response { id, result, error } => {
            let request_id = id.as_u64();
            let responder = request_id.and_then(|id| pending.lock().remove(&id));
            if let Some(responder) = responder {
                let response = error.map_or_else(
                    || Ok(result.unwrap_or(Value::Null)),
                    |error| {
                        Err(Error::Rpc {
                            code: error.code,
                            message: error.message,
                            data: error.data,
                        })
                    },
                );
                if responder.send(response).is_err() {
                    log::debug!("app-server request future was dropped");
                }
                true
            } else {
                events
                    .send(Event::UnmatchedResponse { id, result, error })
                    .await
                    .is_ok()
            }
        }
        Incoming::ServerRequest { id, method, params } => {
            let current_time_at = if method == "currentTime/read" {
                current_unix_seconds()
            } else {
                0
            };
            if let Some(response) = automatic_server_request_response(&id, &method, current_time_at)
            {
                log::debug!("automatically answered app-server request {method} with id {id}");
                if outbound.try_send(response).is_err() {
                    disconnect(pending, events, "app-server writer queue closed".into()).await;
                    return false;
                }
                true
            } else {
                events
                    .send(Event::ServerRequest { id, method, params })
                    .await
                    .is_ok()
            }
        }
        Incoming::Notification { method, params } => events
            .send(Event::Notification { method, params })
            .await
            .is_ok(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadListRelationship<'a> {
    DirectChildren(&'a str),
    Descendants(&'a str),
}

fn thread_list_params(
    limit: usize,
    cursor: Option<&str>,
    relationship: Option<ThreadListRelationship<'_>>,
    source_kinds: Option<&[&str]>,
) -> Value {
    let mut params = json!({
        "archived": false,
        "cursor": cursor,
        "limit": limit,
        "sortDirection": "desc",
        "sortKey": "recency_at",
    });
    match relationship {
        Some(ThreadListRelationship::DirectChildren(parent_thread_id)) => {
            params["parentThreadId"] = parent_thread_id.into();
        }
        Some(ThreadListRelationship::Descendants(ancestor_thread_id)) => {
            params["ancestorThreadId"] = ancestor_thread_id.into();
        }
        None => {}
    }
    if let Some(source_kinds) = source_kinds {
        params["sourceKinds"] = source_kinds.into();
    }
    params
}

fn thread_resume_params(thread_id: &str, exclude_turns: bool) -> Value {
    json!({
        "excludeTurns": exclude_turns,
        "threadId": thread_id,
    })
}

fn turn_start_params(thread_id: &str, input: Value, client_user_message_id: Option<&str>) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": input,
    });
    if let Some(client_user_message_id) = client_user_message_id {
        params["clientUserMessageId"] = client_user_message_id.into();
    }
    params
}

fn turn_steer_params(
    thread_id: &str,
    expected_turn_id: &str,
    input: Value,
    client_user_message_id: &str,
) -> Value {
    json!({
        "threadId": thread_id,
        "expectedTurnId": expected_turn_id,
        "clientUserMessageId": client_user_message_id,
        "input": input,
    })
}

fn thread_queue_add_params(thread_id: &str, input: Value, client_user_message_id: &str) -> Value {
    json!({
        "threadId": thread_id,
        "clientUserMessageId": client_user_message_id,
        "input": input,
    })
}

fn thread_queue_list_params(thread_id: &str) -> Value {
    json!({ "threadId": thread_id })
}

fn thread_queue_update_params(thread_id: &str, queued_submission_id: &str, input: Value) -> Value {
    json!({
        "threadId": thread_id,
        "queuedSubmissionId": queued_submission_id,
        "input": input,
    })
}

fn thread_queue_delete_params(thread_id: &str, queued_submission_id: &str) -> Value {
    json!({
        "threadId": thread_id,
        "queuedSubmissionId": queued_submission_id,
    })
}

fn thread_queue_reorder_params(thread_id: &str, queued_submission_ids: Vec<String>) -> Value {
    json!({
        "threadId": thread_id,
        "queuedSubmissionIds": queued_submission_ids,
    })
}

fn thread_queue_start_params(thread_id: &str, queued_submission_id: Option<&str>) -> Value {
    json!({
        "threadId": thread_id,
        "queuedSubmissionId": queued_submission_id,
    })
}

fn decode_thread_open_response(response: Value) -> Result<ThreadOpenResponse, Error> {
    Ok(serde_json::from_value(response)?)
}

fn current_unix_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

fn automatic_server_request_response(
    id: &Value,
    method: &str,
    current_time_at: i64,
) -> Option<Value> {
    match method {
        "currentTime/read" => Some(json!({
            "id": id,
            "result": {
                "currentTimeAt": current_time_at,
            },
        })),
        "account/chatgptAuthTokens/refresh" => Some(json!({
            "id": id,
            "error": {
                "code": -32000,
                "message": "external ChatGPT auth token refresh is not available",
            },
        })),
        "attestation/generate" => Some(json!({
            "id": id,
            "error": {
                "code": -32000,
                "message": "client attestation is not available",
            },
        })),
        "item/tool/call" => Some(json!({
            "id": id,
            "result": {
                "contentItems": [{
                    "type": "inputText",
                    "text": "Dynamic tools are not registered by this client.",
                }],
                "success": false,
            },
        })),
        _ => None,
    }
}

async fn disconnect(
    pending: &PendingRequests,
    events: &async_channel::Sender<Event>,
    reason: String,
) {
    let responders = pending
        .lock()
        .drain()
        .map(|(_, sender)| sender)
        .collect::<Vec<_>>();
    for responder in responders {
        if responder.send(Err(Error::TransportClosed)).is_err() {
            log::debug!("app-server request future was dropped during disconnect");
        }
    }
    if events.send(Event::Disconnected { reason }).await.is_err() {
        log::debug!("app-server event receiver was dropped during disconnect");
    }
    // Disconnection is terminal for this transport session. Closing the channel is
    // what lets hosts retire the Client after consuming the final semantic
    // event; otherwise the writer task's Sender can keep `recv()` pending
    // forever after stdout has already closed.
    events.close();
}

fn decode_line(line: &str) -> Result<Incoming, Error> {
    let value: Value = serde_json::from_str(line)?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidMessage("expected an object".into()))?;

    match (object.get("method"), object.get("id")) {
        (Some(Value::String(method)), Some(id)) => Ok(Incoming::ServerRequest {
            id: id.clone(),
            method: method.clone(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(Value::String(method)), None) => Ok(Incoming::Notification {
            method: method.clone(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(id)) => {
            let result = object.get("result").cloned();
            let error = object.get("error").map(parse_rpc_error).transpose()?;
            if result.is_none() && error.is_none() {
                return Err(Error::InvalidMessage(
                    "response had neither result nor error".into(),
                ));
            }
            Ok(Incoming::Response {
                id: id.clone(),
                result,
                error,
            })
        }
        _ => Err(Error::InvalidMessage(
            "expected a request, response, or notification".into(),
        )),
    }
}

fn app_server_trace_enabled() -> bool {
    std::env::var_os("CODEX_APP_SERVER_CLIENT_TRACE")
        .is_some_and(|value| !value.is_empty() && value != OsStr::new("0"))
}

fn parse_rpc_error(value: &Value) -> Result<RpcError, Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidMessage("error must be an object".into()))?;
    let code = object
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| Error::InvalidMessage("error.code must be an integer".into()))?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidMessage("error.message must be a string".into()))?;

    Ok(RpcError {
        code,
        message: message.into(),
        data: object.get("data").cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon_lifecycle_json(status: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "status": status,
            "backend": "pid",
            "pid": 4242,
            "managedCodexPath": "/opt/codex/bin/codex",
            "managedCodexVersion": "0.151.0",
            "socketPath": "/tmp/codex/app-server-control.sock",
            "cliVersion": "0.151.0",
            "appServerVersion": "0.151.0"
        }))
        .expect("lifecycle fixture should encode")
    }

    #[test]
    fn accepts_started_and_already_running_managed_daemons() {
        for status in ["started", "alreadyRunning"] {
            assert_eq!(
                parse_daemon_lifecycle(&daemon_lifecycle_json(status))
                    .expect("managed lifecycle should validate"),
                PathBuf::from("/tmp/codex/app-server-control.sock")
            );
        }

        let mut newer_lifecycle: Value =
            serde_json::from_slice(&daemon_lifecycle_json("started")).unwrap();
        newer_lifecycle["newInformationalField"] = json!("forward-compatible");
        assert!(
            parse_daemon_lifecycle(&serde_json::to_vec(&newer_lifecycle).unwrap()).is_ok(),
            "new informational fields must not break a compatible daemon"
        );
    }

    #[test]
    fn rejects_unmanaged_or_incompatible_daemon_lifecycles() {
        let mut lifecycle: Value =
            serde_json::from_slice(&daemon_lifecycle_json("started")).unwrap();
        lifecycle["backend"] = Value::Null;
        assert!(parse_daemon_lifecycle(&serde_json::to_vec(&lifecycle).unwrap()).is_err());

        let mut lifecycle: Value =
            serde_json::from_slice(&daemon_lifecycle_json("started")).unwrap();
        lifecycle["status"] = json!("stopped");
        assert!(parse_daemon_lifecycle(&serde_json::to_vec(&lifecycle).unwrap()).is_err());

        let mut lifecycle: Value =
            serde_json::from_slice(&daemon_lifecycle_json("started")).unwrap();
        lifecycle["socketPath"] = json!("relative.sock");
        assert!(
            parse_daemon_lifecycle(&serde_json::to_vec(&lifecycle).unwrap())
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );

        let mut lifecycle: Value =
            serde_json::from_slice(&daemon_lifecycle_json("started")).unwrap();
        lifecycle["appServerVersion"] = json!("0.152.0");
        assert!(
            parse_daemon_lifecycle(&serde_json::to_vec(&lifecycle).unwrap())
                .unwrap_err()
                .to_string()
                .contains("version mismatch")
        );

        let mut lifecycle: Value =
            serde_json::from_slice(&daemon_lifecycle_json("started")).unwrap();
        lifecycle
            .as_object_mut()
            .unwrap()
            .remove("managedCodexVersion");
        assert!(parse_daemon_lifecycle(&serde_json::to_vec(&lifecycle).unwrap()).is_err());
    }

    #[test]
    fn split_duplex_delegates_reads_and_writes_to_separate_halves() {
        smol::block_on(async {
            use futures_lite::io::AsyncReadExt;

            let mut duplex = SplitDuplex::new(
                futures_lite::io::Cursor::new(b"incoming".to_vec()),
                futures_lite::io::Cursor::new(Vec::new()),
            );
            let mut incoming = String::new();
            duplex.read_to_string(&mut incoming).await.unwrap();
            duplex.write_all(b"outgoing").await.unwrap();

            assert_eq!(incoming, "incoming");
            assert_eq!(duplex.writer.into_inner(), b"outgoing");
        });
    }

    #[test]
    fn disconnect_event_is_terminal_for_the_event_stream() {
        smol::block_on(async {
            let pending = PendingRequests::default();
            let (events, receiver) = async_channel::unbounded();

            disconnect(&pending, &events, "stdout closed".into()).await;

            assert_eq!(
                receiver.recv().await,
                Ok(Event::Disconnected {
                    reason: "stdout closed".into(),
                })
            );
            assert!(
                receiver.recv().await.is_err(),
                "the final disconnect event must be followed by channel closure"
            );
        });
    }

    #[test]
    fn decodes_response() {
        assert_eq!(
            decode_line(r#"{"id":3,"result":{"ok":true}}"#).expect("response should be valid"),
            Incoming::Response {
                id: json!(3),
                result: Some(json!({ "ok": true })),
                error: None,
            }
        );
    }

    #[test]
    fn decodes_notification() {
        assert_eq!(
            decode_line(r#"{"method":"turn/started","params":{"id":"turn_1"}}"#)
                .expect("notification should be valid"),
            Incoming::Notification {
                method: "turn/started".into(),
                params: json!({ "id": "turn_1" }),
            }
        );
    }

    #[test]
    fn turn_start_carries_the_optimistic_user_message_id() {
        assert_eq!(
            turn_start_params(
                "thread-1",
                json!([{"type": "text", "text": "hello"}]),
                Some("client-message-1"),
            ),
            json!({
                "threadId": "thread-1",
                "clientUserMessageId": "client-message-1",
                "input": [{"type": "text", "text": "hello"}],
            })
        );
        assert_eq!(
            turn_start_params("thread-1", json!([]), None),
            json!({"threadId": "thread-1", "input": []})
        );
    }

    #[test]
    fn thread_relationship_filters_are_mutually_exclusive_on_the_wire() {
        assert_eq!(
            thread_list_params(
                50,
                Some("next-page"),
                Some(ThreadListRelationship::DirectChildren("parent-1")),
                Some(&["subAgentThreadSpawn"]),
            ),
            json!({
                "archived": false,
                "cursor": "next-page",
                "limit": 50,
                "parentThreadId": "parent-1",
                "sourceKinds": ["subAgentThreadSpawn"],
                "sortDirection": "desc",
                "sortKey": "recency_at",
            })
        );
        assert_eq!(
            thread_list_params(
                100,
                None,
                Some(ThreadListRelationship::Descendants("ancestor-1")),
                Some(&["subAgentThreadSpawn"]),
            ),
            json!({
                "ancestorThreadId": "ancestor-1",
                "archived": false,
                "cursor": null,
                "limit": 100,
                "sourceKinds": ["subAgentThreadSpawn"],
                "sortDirection": "desc",
                "sortKey": "recency_at",
            })
        );
    }

    #[test]
    fn spawned_subagent_filter_does_not_change_the_root_thread_query() {
        assert_eq!(
            thread_list_params(300, None, None, None),
            json!({
                "archived": false,
                "cursor": null,
                "limit": 300,
                "sortDirection": "desc",
                "sortKey": "recency_at",
            })
        );
        assert_eq!(
            thread_list_params(300, None, None, Some(&["subAgentThreadSpawn"])),
            json!({
                "archived": false,
                "cursor": null,
                "limit": 300,
                "sortDirection": "desc",
                "sortKey": "recency_at",
                "sourceKinds": ["subAgentThreadSpawn"],
            })
        );
    }

    #[test]
    fn resume_and_attach_use_distinct_turn_history_flags_on_the_wire() {
        assert_eq!(
            thread_resume_params("thread-1", false),
            json!({
                "excludeTurns": false,
                "threadId": "thread-1",
            })
        );
        assert_eq!(
            thread_resume_params("thread-1", true),
            json!({
                "excludeTurns": true,
                "threadId": "thread-1",
            })
        );
    }

    #[test]
    fn steering_and_queueing_carry_stable_user_message_ids() {
        let input = json!([{"type": "text", "text": "keep going"}]);
        assert_eq!(
            turn_steer_params("thread-1", "turn-1", input.clone(), "client-message-1",),
            json!({
                "threadId": "thread-1",
                "expectedTurnId": "turn-1",
                "clientUserMessageId": "client-message-1",
                "input": input,
            })
        );
        assert_eq!(
            thread_queue_add_params(
                "thread-1",
                json!([{"type": "text", "text": "next"}]),
                "client-message-2",
            ),
            json!({
                "threadId": "thread-1",
                "clientUserMessageId": "client-message-2",
                "input": [{"type": "text", "text": "next"}],
            })
        );
    }

    #[test]
    fn queue_management_methods_use_authoritative_submission_ids() {
        let input = json!([{"type": "text", "text": "edited"}]);
        assert_eq!(
            thread_queue_list_params("thread-1"),
            json!({"threadId": "thread-1"})
        );
        assert_eq!(
            thread_queue_update_params("thread-1", "queued-1", input.clone()),
            json!({
                "threadId": "thread-1",
                "queuedSubmissionId": "queued-1",
                "input": input,
            })
        );
        assert_eq!(
            thread_queue_delete_params("thread-1", "queued-1"),
            json!({
                "threadId": "thread-1",
                "queuedSubmissionId": "queued-1",
            })
        );
        assert_eq!(
            thread_queue_reorder_params("thread-1", vec!["queued-2".into(), "queued-1".into()],),
            json!({
                "threadId": "thread-1",
                "queuedSubmissionIds": ["queued-2", "queued-1"],
            })
        );
        assert_eq!(
            thread_queue_start_params("thread-1", Some("queued-1")),
            json!({
                "threadId": "thread-1",
                "queuedSubmissionId": "queued-1",
            })
        );
        assert_eq!(
            thread_queue_start_params("thread-1", None),
            json!({
                "threadId": "thread-1",
                "queuedSubmissionId": null,
            })
        );
    }

    #[test]
    fn decodes_server_request() {
        assert_eq!(
            decode_line(
                r#"{"method":"item/commandExecution/requestApproval","id":9,"params":{}}"#,
            )
            .expect("server request should be valid"),
            Incoming::ServerRequest {
                id: json!(9),
                method: "item/commandExecution/requestApproval".into(),
                params: json!({}),
            }
        );
    }

    #[test]
    fn builds_exact_automatic_infrastructure_responses() {
        assert_eq!(
            automatic_server_request_response(&json!(10), "currentTime/read", 1_725_000_123),
            Some(json!({
                "id": 10,
                "result": {
                    "currentTimeAt": 1_725_000_123_i64,
                },
            }))
        );
        assert_eq!(
            automatic_server_request_response(
                &json!("auth-request"),
                "account/chatgptAuthTokens/refresh",
                0,
            ),
            Some(json!({
                "id": "auth-request",
                "error": {
                    "code": -32000,
                    "message": "external ChatGPT auth token refresh is not available",
                },
            }))
        );
        assert_eq!(
            automatic_server_request_response(&json!(12), "attestation/generate", 0),
            Some(json!({
                "id": 12,
                "error": {
                    "code": -32000,
                    "message": "client attestation is not available",
                },
            }))
        );
        assert_eq!(
            automatic_server_request_response(&json!(13), "item/tool/call", 0),
            Some(json!({
                "id": 13,
                "result": {
                    "contentItems": [{
                        "type": "inputText",
                        "text": "Dynamic tools are not registered by this client.",
                    }],
                    "success": false,
                },
            }))
        );
    }

    #[test]
    fn decoded_infrastructure_request_builds_exact_response() {
        let incoming = decode_line(
            r#"{"method":"item/tool/call","id":"tool-request","params":{"tool":"preview"}}"#,
        )
        .expect("dynamic tool request should decode");
        let Incoming::ServerRequest { id, method, .. } = incoming else {
            panic!("expected a server request");
        };

        assert_eq!(
            automatic_server_request_response(&id, &method, 0),
            Some(json!({
                "id": "tool-request",
                "result": {
                    "contentItems": [{
                        "type": "inputText",
                        "text": "Dynamic tools are not registered by this client.",
                    }],
                    "success": false,
                },
            }))
        );
    }

    #[test]
    fn leaves_all_interactive_requests_for_the_app() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "item/permissions/requestApproval",
            "applyPatchApproval",
            "execCommandApproval",
        ] {
            assert_eq!(
                automatic_server_request_response(&json!(9), method, 0),
                None,
                "{method} must remain interactive"
            );
        }
    }

    #[test]
    fn rejects_malformed_response() {
        let error = decode_line(r#"{"id":3}"#).unwrap_err();
        assert!(error.to_string().contains("neither result nor error"));
    }

    #[test]
    fn decodes_thread_list_response_with_subagent_hierarchy_metadata() {
        let response = serde_json::from_value::<ThreadListResponse>(json!({
            "data": [{
                "id": "thread_1",
                "name": "Fast UI",
                "preview": "Build a fast agent UI",
                "cwd": "/work/harness",
                "updatedAt": 42,
                "cliVersion": "1.0.0",
                "createdAt": 1,
                "ephemeral": false,
                "modelProvider": "openai",
                "sessionId": "session_1",
                "parentThreadId": "parent_1",
                "forkedFromId": "template_1",
                "source": {"subAgent": {"thread_spawn": {
                    "parent_thread_id": "parent_1",
                    "depth": 2,
                    "agent_nickname": "Scout",
                    "agent_role": "explorer",
                    "agent_path": "/root/scout"
                }}},
                "threadSource": "harness",
                "status": {
                    "type": "active",
                    "activeFlags": ["waitingOnApproval"]
                },
                "agentNickname": "Scout",
                "agentRole": "explorer",
                "canAcceptDirectInput": true,
                "turns": []
            }],
            "nextCursor": "next"
        }))
        .expect("thread list should decode");

        assert_eq!(response.data[0].name.as_deref(), Some("Fast UI"));
        assert_eq!(response.data[0].updated_at, 42);
        assert_eq!(response.data[0].session_id, "session_1");
        assert_eq!(
            response.data[0].effective_parent_thread_id(),
            Some("parent_1")
        );
        assert_eq!(
            response.data[0].forked_from_id.as_deref(),
            Some("template_1")
        );
        assert_eq!(
            response.data[0].status,
            CodexThreadStatus::Active {
                active_flags: vec!["waitingOnApproval".into()]
            }
        );
        assert_eq!(response.data[0].agent_nickname.as_deref(), Some("Scout"));
        assert_eq!(response.data[0].agent_role.as_deref(), Some("explorer"));
        assert_eq!(response.data[0].can_accept_direct_input, Some(true));
        assert_eq!(response.data[0].thread_source.as_deref(), Some("harness"));
        assert!(matches!(
            &response.data[0].source,
            CodexSessionSource::SubAgent(CodexSubagentSource::ThreadSpawn(spawn))
                if spawn.parent_thread_id == "parent_1"
                    && spawn.depth == 2
                    && spawn.agent_path.as_deref() == Some("/root/scout")
        ));
        assert_eq!(response.next_cursor.as_deref(), Some("next"));
    }

    #[test]
    fn hierarchy_parsing_is_forward_compatible_and_supports_legacy_parent_metadata() {
        let legacy = serde_json::from_value::<CodexThread>(json!({
            "id": "child_1",
            "source": {"subAgent": {"thread_spawn": {
                "parent_thread_id": "parent_1",
                "depth": 1
            }}},
            "status": {"type": "futureStatus", "detail": "new"}
        }))
        .expect("future metadata should not prevent thread parsing");

        assert_eq!(legacy.effective_parent_thread_id(), Some("parent_1"));
        assert_eq!(
            legacy.status,
            CodexThreadStatus::Unknown(json!({
                "type": "futureStatus",
                "detail": "new"
            }))
        );

        let future_source = serde_json::from_value::<CodexThread>(json!({
            "id": "child_2",
            "source": {"futureSource": {"version": 2}}
        }))
        .expect("future source should not prevent thread parsing");
        assert_eq!(
            future_source.source,
            CodexSessionSource::Unknown(json!({"futureSource": {"version": 2}}))
        );
    }

    #[test]
    fn decodes_flexible_thread_items() {
        let response = serde_json::from_value::<ThreadReadResponse>(json!({
            "thread": {
                "id": "thread_1",
                "preview": "Hello",
                "cwd": "/work/harness",
                "updatedAt": 42,
                "turns": [{
                    "id": "turn_1",
                    "status": "completed",
                    "items": [{
                        "id": "item_1",
                        "type": "agentMessage",
                        "text": "Done"
                    }]
                }]
            }
        }))
        .expect("thread should decode");

        let item = &response.thread.turns[0].items[0];
        assert_eq!(item.kind, "agentMessage");
        assert_eq!(item.body.get("text"), Some(&json!("Done")));
    }

    #[test]
    fn decodes_start_and_resume_thread_responses_with_extra_settings() {
        let response = decode_thread_open_response(json!({
            "thread": {
                "id": "thread_2",
                "preview": "Direct app-server",
                "cwd": "/work/harness",
                "updatedAt": 42,
                "turns": []
            },
            "cwd": "/work/harness",
            "model": "gpt-5.6",
            "modelProvider": "openai",
            "reasoningEffort": "xhigh",
            "approvalPolicy": "on-request",
            "sandbox": {"type": "workspaceWrite"},
            "activePermissionProfile": {"id": ":workspace"}
        }))
        .expect("thread response should retain effective settings");

        assert_eq!(response.thread.id, "thread_2");
        assert_eq!(response.thread.cwd, "/work/harness");
        assert_eq!(response.reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(response.model_provider, "openai");
        assert_eq!(
            response
                .active_permission_profile
                .as_ref()
                .and_then(|profile| profile.get("id"))
                .and_then(Value::as_str),
            Some(":workspace")
        );
    }

    #[test]
    fn decodes_turnless_attach_response_with_extra_settings() {
        let response = decode_thread_open_response(json!({
            "thread": {
                "id": "thread_3",
                "preview": "Already rendered",
                "cwd": "/work/harness",
                "updatedAt": 43,
                "turns": []
            },
            "cwd": "/work/harness",
            "model": "gpt-5.6",
            "modelProvider": "openai",
            "reasoningEffort": "high",
            "approvalPolicy": "never",
            "sandbox": {"type": "dangerFullAccess"},
            "activePermissionProfile": {"id": ":full"},
            "serviceTier": "priority"
        }))
        .expect("turnless attach response should retain effective settings");

        assert_eq!(response.thread.id, "thread_3");
        assert!(response.thread.turns.is_empty());
        assert_eq!(response.cwd, "/work/harness");
        assert_eq!(response.model, "gpt-5.6");
        assert_eq!(response.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(response.approval_policy, json!("never"));
        assert_eq!(response.sandbox, json!({"type": "dangerFullAccess"}));
        assert_eq!(response.service_tier.as_deref(), Some("priority"));
        assert_eq!(
            response
                .active_permission_profile
                .as_ref()
                .and_then(|profile| profile.get("id"))
                .and_then(Value::as_str),
            Some(":full")
        );
    }
}
