//! Async client for the local Codex app-server stdio transport.
//!
//! Codex app-server speaks newline-delimited JSON messages shaped like JSON-RPC
//! 2.0 with the `jsonrpc` field omitted. This crate owns the child process,
//! correlates responses with requests, and exposes notifications and
//! server-initiated requests as an event stream.

use std::{
    collections::HashMap,
    ffi::OsStr,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_process::{Child, Command};
use futures::channel::oneshot;
use futures_lite::{
    StreamExt,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
};
use parking_lot::Mutex;
use serde::Deserialize;
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
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
    pub turns: Vec<CodexTurn>,
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
    #[error("invalid app-server message: {0}")]
    InvalidMessage(String),
    #[error("app-server returned error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
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

                match incoming {
                    Incoming::Response { id, result, error } => {
                        let request_id = id.as_u64();
                        let responder = request_id.and_then(|id| reader_pending.lock().remove(&id));
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
                        } else {
                            if reader_events
                                .send(Event::UnmatchedResponse { id, result, error })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Incoming::ServerRequest { id, method, params } => {
                        let current_time_at = if method == "currentTime/read" {
                            current_unix_seconds()
                        } else {
                            0
                        };
                        if let Some(response) =
                            automatic_server_request_response(&id, &method, current_time_at)
                        {
                            log::debug!(
                                "automatically answered app-server request {method} with id {id}"
                            );
                            if reader_outbound.try_send(response).is_err() {
                                disconnect(
                                    &reader_pending,
                                    &reader_events,
                                    "app-server writer queue closed".into(),
                                )
                                .await;
                                return;
                            }
                            continue;
                        }
                        if reader_events
                            .send(Event::ServerRequest { id, method, params })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Incoming::Notification { method, params } => {
                        if reader_events
                            .send(Event::Notification { method, params })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
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
        let response = self
            .request(
                "thread/list",
                json!({
                    "archived": false,
                    "cursor": cursor,
                    "limit": limit,
                    "sortDirection": "desc",
                    "sortKey": "recency_at",
                }),
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
        decode_thread_response(response)
    }

    /// Resume an existing thread and subscribe this connection to its live
    /// events. The response includes all turns so a client can render it
    /// without a lossy intermediary protocol.
    pub async fn resume_thread(&self, thread_id: &str) -> Result<CodexThread, Error> {
        let response = self
            .request(
                "thread/resume",
                json!({
                    "excludeTurns": false,
                    "threadId": thread_id,
                }),
            )
            .await?;
        decode_thread_response(response)
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

fn decode_thread_response(response: Value) -> Result<CodexThread, Error> {
    Ok(serde_json::from_value::<ThreadReadResponse>(response)?.thread)
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
    // Disconnection is terminal for this stdio session. Closing the channel is
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
    fn decodes_thread_list_response_without_unneeded_protocol_fields() {
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
                "source": "cli",
                "status": {"type": "idle"},
                "turns": []
            }],
            "nextCursor": "next"
        }))
        .expect("thread list should decode");

        assert_eq!(response.data[0].name.as_deref(), Some("Fast UI"));
        assert_eq!(response.data[0].updated_at, 42);
        assert_eq!(response.next_cursor.as_deref(), Some("next"));
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
        let thread = decode_thread_response(json!({
            "thread": {
                "id": "thread_2",
                "preview": "Direct app-server",
                "cwd": "/work/harness",
                "updatedAt": 42,
                "turns": []
            },
            "model": "gpt-5.6",
            "approvalPolicy": "on-request",
            "sandbox": {"type": "workspace-write"}
        }))
        .expect("thread response should ignore unrelated settings");

        assert_eq!(thread.id, "thread_2");
        assert_eq!(thread.cwd, "/work/harness");
    }
}
