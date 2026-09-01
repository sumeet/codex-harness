//! Compatibility client for the consumer ChatGPT surface shipped by Codex Desktop.
//!
//! This is deliberately isolated from the Codex App Server client. ChatGPT account
//! history is served by a private desktop HTTP contract, while Codex tasks use the
//! public App Server protocol. Keep credentials out of logs and keep every private
//! route in this module so a desktop update has one small compatibility boundary.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Command,
    sync::{Arc, OnceLock},
};

use anyhow::{Context as _, bail};
use async_process::{Command as AsyncCommand, Stdio};
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _,
    channel::mpsc::UnboundedSender, io::BufReader,
};
use http_client::{AsyncBody, HttpClient, http};
use serde::Deserialize;
use serde_json::Value;

const API_BASE: &str = "https://chatgpt.com/backend-api";
const DESKTOP_ORIGINATOR: &str = "Codex Desktop";
const DEFAULT_DESKTOP_VERSION: &str = "26.825.51511";
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub update_time: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageRole {
    User,
    Assistant,
    Reasoning,
    Activity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivityKind {
    WebSearch,
    Command,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchResult {
    pub title: String,
    pub url: String,
    pub domain: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Activity {
    pub kind: ActivityKind,
    pub title: String,
    pub detail: Option<String>,
    pub results: Vec<SearchResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Message {
    pub id: String,
    pub role: MessageRole,
    pub content: String,
    pub activity: Option<Activity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Conversation {
    pub id: String,
    pub title: String,
    pub current_node: Option<String>,
    pub default_model: Option<String>,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelChoice {
    pub model: String,
    pub label: String,
    pub tagline: Option<String>,
    pub lane: String,
    pub legacy: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelCatalog {
    pub default_model: String,
    pub choices: Vec<ModelChoice>,
}

#[derive(Deserialize)]
struct AuthFile {
    tokens: AuthTokens,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: String,
    account_id: String,
}

#[derive(Deserialize)]
struct ConversationListResponse {
    #[serde(default)]
    items: Vec<ConversationSummary>,
}

#[derive(Deserialize)]
struct ConversationResponse {
    conversation_id: String,
    title: String,
    current_node: Option<String>,
    default_model_slug: Option<String>,
    #[serde(default)]
    mapping: HashMap<String, ConversationNode>,
}

#[derive(Deserialize)]
struct ConversationNode {
    parent: Option<String>,
    message: Option<ApiMessage>,
}

#[derive(Deserialize)]
struct ApiMessage {
    id: String,
    author: ApiAuthor,
    content: Value,
    #[serde(default)]
    recipient: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    metadata: Value,
}

#[derive(Deserialize)]
struct ApiAuthor {
    role: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct ModelCatalogResponse {
    default_model_slug: String,
    #[serde(default)]
    categories: Vec<ModelCategory>,
}

#[derive(Deserialize)]
struct ModelCategory {
    default_model: String,
    human_category_name: String,
    human_category_short_name: String,
    model_lane: String,
    #[serde(default)]
    tagline: String,
    #[serde(default)]
    subcategory: Option<String>,
}

pub(crate) async fn list_conversations(
    client: Arc<dyn HttpClient>,
) -> anyhow::Result<Vec<ConversationSummary>> {
    let response: ConversationListResponse =
        get_json(client, "/conversations?offset=0&limit=100&order=updated").await?;
    Ok(response.items)
}

pub(crate) async fn get_conversation(
    client: Arc<dyn HttpClient>,
    conversation_id: &str,
) -> anyhow::Result<Conversation> {
    if conversation_id.is_empty()
        || !conversation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid ChatGPT conversation id");
    }
    let response: ConversationResponse =
        get_json(client, &format!("/conversation/{conversation_id}")).await?;
    Ok(project_conversation(response))
}

pub(crate) async fn list_models(client: Arc<dyn HttpClient>) -> anyhow::Result<ModelCatalog> {
    let response: ModelCatalogResponse =
        get_json(client, "/models?iim=false&include_icons=false").await?;
    Ok(project_model_catalog(response))
}

fn project_model_catalog(response: ModelCatalogResponse) -> ModelCatalog {
    let mut choices = response
        .categories
        .into_iter()
        .filter(|category| !category.default_model.trim().is_empty())
        .map(|category| {
            let short = category.human_category_short_name.trim();
            let name = category.human_category_name.trim();
            let label = if short.is_empty() {
                name.to_owned()
            } else if name.is_empty()
                || short
                    .to_ascii_lowercase()
                    .contains(&name.to_ascii_lowercase())
            {
                short.to_owned()
            } else {
                format!("{name} · {short}")
            };
            ModelChoice {
                model: category.default_model,
                label,
                tagline: (!category.tagline.trim().is_empty()).then_some(category.tagline),
                lane: category.model_lane,
                legacy: category.subcategory.is_some(),
            }
        })
        .collect::<Vec<_>>();
    choices.sort_by_key(|choice| {
        let lane = match choice.lane.as_str() {
            "auto" => 0,
            "instant" => 1,
            "thinking" => 2,
            "pro" => 3,
            "thinking_mini" => 4,
            _ => 5,
        };
        (choice.legacy, lane)
    });
    choices.dedup_by(|left, right| left.model == right.model);
    ModelCatalog {
        default_model: response.default_model_slug,
        choices,
    }
}

pub(crate) async fn send_message(
    conversation_id: &str,
    parent_message_id: &str,
    model: &str,
    prompt: &str,
    message_id: &str,
    stream_updates: UnboundedSender<Message>,
) -> anyhow::Result<()> {
    validate_identifier(conversation_id, "conversation")?;
    validate_identifier(parent_message_id, "parent message")?;
    validate_identifier(message_id, "message")?;
    if model.is_empty()
        || model.len() > 128
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid ChatGPT model");
    }
    if prompt.trim().is_empty() {
        bail!("ChatGPT prompt is empty");
    }

    let request = build_send_request(
        conversation_id,
        parent_message_id,
        model,
        prompt,
        message_id,
    );
    let bridge_input = serde_json::json!({
        "device_id": chatgpt_device_id(),
        "request": request,
    });

    let mut command = AsyncCommand::new(chatgpt_node());
    command
        .arg(chatgpt_bridge_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("starting the ChatGPT conversation worker")?;
    let mut stdin = child.stdin.take().context("opening ChatGPT worker input")?;
    let stdout = child
        .stdout
        .take()
        .context("opening ChatGPT worker output")?;
    let input = serde_json::to_vec(&bridge_input).context("encoding ChatGPT send request")?;
    stdin
        .write_all(&input)
        .await
        .context("sending request to ChatGPT worker")?;
    stdin
        .close()
        .await
        .context("closing ChatGPT worker input")?;

    let mut accepted = false;
    let mut worker_error = None;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next().await {
        let line = line.context("reading ChatGPT worker status")?;
        let event: Value = serde_json::from_str(&line).context("decoding ChatGPT worker status")?;
        match event.get("type").and_then(Value::as_str) {
            Some("accepted") => accepted = true,
            Some("event") => {
                if let Some(payload) = event.get("data") {
                    for message in project_stream_messages(payload) {
                        _ = stream_updates.unbounded_send(message);
                    }
                }
            }
            Some("error") => {
                worker_error = event
                    .get("message")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }
            _ => {}
        }
    }
    let status = child
        .status()
        .await
        .context("waiting for ChatGPT conversation worker")?;
    if let Some(error) = worker_error {
        bail!(error);
    }
    if !status.success() {
        bail!("ChatGPT conversation worker exited with {status}");
    }
    if !accepted {
        bail!("ChatGPT conversation request ended before it was accepted");
    }
    Ok(())
}

fn build_send_request(
    conversation_id: &str,
    parent_message_id: &str,
    model: &str,
    prompt: &str,
    message_id: &str,
) -> Value {
    serde_json::json!({
        "action": "next",
        "client_prepare_state": "sent",
        "conversation_id": conversation_id,
        "messages": [{
            "author": {"role": "user"},
            "content": {"content_type": "text", "parts": [prompt]},
            "id": message_id,
            "metadata": {}
        }],
        "model": model,
        "parent_message_id": parent_message_id,
        "timezone": system_timezone(),
    })
}

fn validate_identifier(identifier: &str, label: &str) -> anyhow::Result<()> {
    if identifier.is_empty()
        || identifier.len() > 128
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid ChatGPT {label} id");
    }
    Ok(())
}

fn chatgpt_node() -> PathBuf {
    std::env::var_os("HARNESS_CHATGPT_NODE")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            let bundled = PathBuf::from("/opt/codex-desktop/resources/cua_node/bin/node");
            bundled.is_file().then_some(bundled)
        })
        .unwrap_or_else(|| PathBuf::from("node"))
}

fn chatgpt_bridge_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/chatgpt_bridge.mjs")
}

fn chatgpt_device_id() -> &'static str {
    static DEVICE_ID: OnceLock<String> = OnceLock::new();
    DEVICE_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .as_str()
}

fn system_timezone() -> String {
    std::env::var("TZ")
        .ok()
        .filter(|timezone| !timezone.trim().is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/timezone")
                .ok()
                .map(|timezone| timezone.trim().to_owned())
                .filter(|timezone| !timezone.is_empty())
        })
        .unwrap_or_else(|| "UTC".into())
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    client: Arc<dyn HttpClient>,
    route: &str,
) -> anyhow::Result<T> {
    let auth = load_auth()?;
    let request = build_request(route, &auth)?;
    let mut response = client
        .send(request)
        .await
        .context("requesting ChatGPT desktop history")?;
    if !response.status().is_success() {
        bail!(
            "ChatGPT desktop history returned HTTP {}",
            response.status()
        );
    }
    if response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        bail!("ChatGPT desktop history response is unexpectedly large");
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .context("reading ChatGPT desktop history")?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        bail!("ChatGPT desktop history response is unexpectedly large");
    }
    serde_json::from_slice(&bytes).context("decoding ChatGPT desktop history")
}

fn build_request(route: &str, auth: &AuthTokens) -> anyhow::Result<http::Request<AsyncBody>> {
    let user_agent = format!(
        "Codex Desktop/{} (X11; Linux; {})",
        desktop_version(),
        desktop_arch()
    );
    http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("{API_BASE}{route}"))
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {}", auth.access_token),
        )
        .header("ChatGPT-Account-Id", &auth.account_id)
        .header("originator", DESKTOP_ORIGINATOR)
        .header("OAI-Language", "en")
        .header(http::header::USER_AGENT, user_agent)
        .body(AsyncBody::default())
        .context("building ChatGPT desktop request")
}

fn load_auth() -> anyhow::Result<AuthTokens> {
    let path = codex_auth_path().context("the Codex auth file is unavailable")?;
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let auth: AuthFile = serde_json::from_slice(&bytes).context("decoding Codex authentication")?;
    if auth.tokens.access_token.trim().is_empty() || auth.tokens.account_id.trim().is_empty() {
        bail!("Codex is not signed in with a ChatGPT account");
    }
    Ok(auth.tokens)
}

fn codex_auth_path() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .map(|root| root.join("auth.json"))
}

fn desktop_version() -> String {
    std::env::var("HARNESS_CHATGPT_DESKTOP_VERSION")
        .ok()
        .filter(|version| valid_desktop_version(version))
        .or_else(installed_desktop_version)
        .unwrap_or_else(|| DEFAULT_DESKTOP_VERSION.to_owned())
}

fn installed_desktop_version() -> Option<String> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            ["codex-desktop", "/opt/codex-desktop/codex-desktop"]
                .into_iter()
                .find_map(|program| {
                    let output = Command::new(program).arg("--version").output().ok()?;
                    output
                        .status
                        .success()
                        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                })
                .filter(|version| valid_desktop_version(version))
        })
        .clone()
}

fn valid_desktop_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

fn desktop_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    }
}

fn project_conversation(response: ConversationResponse) -> Conversation {
    let mut node_ids = Vec::new();
    let mut cursor = response.current_node.clone();
    let mut visited = std::collections::HashSet::new();
    while let Some(node_id) = cursor {
        if !visited.insert(node_id.clone()) {
            break;
        }
        let Some(node) = response.mapping.get(&node_id) else {
            break;
        };
        node_ids.push(node_id);
        cursor = node.parent.clone();
    }
    node_ids.reverse();

    let messages = node_ids
        .into_iter()
        .filter_map(|node_id| response.mapping.get(&node_id)?.message.as_ref())
        .filter_map(project_message)
        .collect();
    Conversation {
        id: response.conversation_id,
        title: response.title,
        current_node: response.current_node,
        default_model: response.default_model_slug,
        messages,
    }
}

fn project_message(message: &ApiMessage) -> Option<Message> {
    let content_type = message
        .content
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("text");
    if message.author.role == "assistant"
        && message
            .recipient
            .as_deref()
            .is_some_and(|recipient| recipient != "all")
    {
        // The assistant-side code item is a tool-call envelope. ChatGPT's
        // renderer pairs it with the following tool result instead of showing
        // the JSON/query payload as an assistant code block.
        return None;
    }
    if message.author.role == "tool" {
        return project_tool_message(message);
    }
    let role = match (message.author.role.as_str(), content_type) {
        ("user", _) => MessageRole::User,
        ("assistant", "reasoning_recap" | "thoughts") => MessageRole::Reasoning,
        ("assistant", _) => MessageRole::Assistant,
        _ => return None,
    };
    let content = message_content(&message.content, &message.metadata)?;
    (!content.trim().is_empty()).then(|| Message {
        id: message.id.clone(),
        role,
        content,
        activity: None,
    })
}

fn project_stream_messages(payload: &Value) -> Vec<Message> {
    fn visit(value: &Value, depth: usize, messages: &mut Vec<Message>) {
        if depth > 6 {
            return;
        }
        match value {
            Value::Object(object) => {
                if object.contains_key("id")
                    && object.contains_key("author")
                    && object.contains_key("content")
                    && let Ok(message) = serde_json::from_value::<ApiMessage>(value.clone())
                    && let Some(message) = project_message(&message)
                {
                    if let Some(existing) = messages
                        .iter_mut()
                        .find(|existing| existing.id == message.id)
                    {
                        *existing = message;
                    } else {
                        messages.push(message);
                    }
                    return;
                }
                for nested in object.values() {
                    visit(nested, depth + 1, messages);
                }
            }
            Value::Array(values) => {
                for nested in values {
                    visit(nested, depth + 1, messages);
                }
            }
            _ => {}
        }
    }

    let mut messages = Vec::new();
    visit(payload, 0, &mut messages);
    messages
}

fn project_tool_message(message: &ApiMessage) -> Option<Message> {
    let name = message.author.name.as_deref().unwrap_or("tool");
    let completed = message.status.as_deref() != Some("in_progress");
    let (kind, title) = match name {
        "web.run" => (
            ActivityKind::WebSearch,
            if completed {
                "Searched the web"
            } else {
                "Searching the web"
            },
        ),
        "container.exec" => (
            ActivityKind::Command,
            if completed {
                "Ran command"
            } else {
                "Running command"
            },
        ),
        "python" => (
            ActivityKind::Command,
            if completed {
                "Ran Python"
            } else {
                "Running Python"
            },
        ),
        _ => (
            ActivityKind::Tool,
            if completed { "Used tool" } else { "Using tool" },
        ),
    };
    let results = if kind == ActivityKind::WebSearch {
        search_results(&message.metadata)
    } else {
        Vec::new()
    };
    let detail = if kind == ActivityKind::WebSearch {
        let domains = results
            .iter()
            .map(|result| result.domain.as_str())
            .filter(|domain| !domain.is_empty())
            .fold(Vec::<&str>::new(), |mut domains, domain| {
                if !domains.contains(&domain) {
                    domains.push(domain);
                }
                domains
            });
        match (results.len(), domains.len()) {
            (0, _) => None,
            (1, 0) => Some("1 source".into()),
            (results, 0) => Some(format!("{results} sources")),
            (1, 1) => Some("1 source · 1 site".into()),
            (1, domains) => Some(format!("1 source · {domains} sites")),
            (results, 1) => Some(format!("{results} sources · 1 site")),
            (results, domains) => Some(format!("{results} sources · {domains} sites")),
        }
    } else {
        message
            .metadata
            .get("reasoning_title")
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| (name != "tool").then(|| name.to_owned()))
    };
    let content = (kind != ActivityKind::WebSearch)
        .then(|| message_content(&message.content, &message.metadata))
        .flatten()
        .unwrap_or_default();
    Some(Message {
        id: message.id.clone(),
        role: MessageRole::Activity,
        content,
        activity: Some(Activity {
            kind,
            title: title.into(),
            detail,
            results,
        }),
    })
}

fn search_results(metadata: &Value) -> Vec<SearchResult> {
    let mut results = Vec::new();
    for group in metadata
        .get("search_result_groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for entry in group
            .get("entries")
            .or_else(|| group.get("results"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(url) = entry.get("url").and_then(Value::as_str) else {
                continue;
            };
            if results
                .iter()
                .any(|result: &SearchResult| result.url == url)
            {
                continue;
            }
            let title = entry
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("Web result")
                .to_owned();
            let domain = entry
                .get("attribution")
                .and_then(Value::as_str)
                .filter(|domain| !domain.trim().is_empty())
                .map(ToOwned::to_owned)
                .or_else(|| web_domain(url))
                .unwrap_or_default();
            results.push(SearchResult {
                title,
                url: url.to_owned(),
                domain,
            });
        }
    }
    results
}

fn web_domain(url: &str) -> Option<String> {
    let authority = url.split_once("://").map_or(url, |(_, rest)| rest);
    authority
        .split(['/', '?', '#'])
        .next()
        .filter(|domain| !domain.is_empty())
        .map(|domain| domain.trim_start_matches("www.").to_owned())
}

fn message_content(content: &Value, metadata: &Value) -> Option<String> {
    match content.get("content_type").and_then(Value::as_str) {
        Some("text") => Some(sanitize_chatgpt_text(
            &content
                .get("parts")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            metadata,
        )),
        Some("code") => {
            let text = content.get("text")?.as_str()?;
            let language = content
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(format!("```{language}\n{text}\n```"))
        }
        Some("execution_output") => content
            .get("text")
            .and_then(Value::as_str)
            .map(|text| format!("```text\n{text}\n```")),
        Some("reasoning_recap") => content.get("content").and_then(|value| {
            let content = match value {
                Value::String(content) => content.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return None,
            };
            Some(sanitize_chatgpt_text(&content, metadata))
        }),
        _ => None,
    }
}

fn sanitize_chatgpt_text(text: &str, metadata: &Value) -> String {
    const REFERENCE_START: char = '\u{e200}';
    const REFERENCE_END: char = '\u{e201}';
    const REFERENCE_SEPARATOR: char = '\u{e202}';

    let mut sanitized = String::with_capacity(text.len());
    let mut remainder = text;
    while let Some(start) = remainder.find(REFERENCE_START) {
        sanitized.push_str(&remainder[..start]);
        let payload = &remainder[start + REFERENCE_START.len_utf8()..];
        let Some(end) = payload.find(REFERENCE_END) else {
            // A truncated private transport span has no user-facing meaning.
            // Dropping its tail is preferable to painting protocol bytes as
            // the mojibake-like glyphs that originally exposed this format.
            remainder = "";
            break;
        };
        let after_reference = &payload[end + REFERENCE_END.len_utf8()..];
        let fields = payload[..end]
            .split(REFERENCE_SEPARATOR)
            .collect::<Vec<_>>();
        let marker =
            &remainder[start..start + REFERENCE_START.len_utf8() + end + REFERENCE_END.len_utf8()];
        match fields.as_slice() {
            ["url", label, destination, ..]
                if destination.starts_with("https://") || destination.starts_with("http://") =>
            {
                sanitized.push('[');
                sanitized.push_str(&escape_markdown_link_label(label));
                sanitized.push_str("](");
                sanitized.push_str(&destination.replace(' ', "%20").replace(')', "%29"));
                sanitized.push(')');
            }
            // Citation references are represented separately in message
            // metadata. Until Harness has native citation cards, omit the
            // transport span instead of exposing its private turn ids.
            ["cite", ..] => {
                if let Some(replacement) = citation_replacement(marker, metadata) {
                    sanitized.push_str(&replacement);
                }
            }
            // Unknown private-use spans are protocol metadata, never prose.
            // Failing closed prevents future marker kinds from painting as
            // mojibake while keeping all text outside the span intact.
            _ => {}
        }
        remainder = after_reference;
    }
    sanitized.push_str(remainder);
    sanitized
}

fn citation_replacement(marker: &str, metadata: &Value) -> Option<String> {
    metadata
        .get("content_references")
        .and_then(Value::as_array)?
        .iter()
        .find(|reference| reference.get("matched_text").and_then(Value::as_str) == Some(marker))
        .and_then(|reference| reference.get("alt").and_then(Value::as_str))
        .filter(|alternative| !alternative.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn escape_markdown_link_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_only_the_selected_conversation_branch() {
        let response = serde_json::from_value::<ConversationResponse>(json!({
            "conversation_id": "conversation-1",
            "title": "A chat",
            "current_node": "assistant-b",
            "mapping": {
                "root": {"parent": null, "message": null},
                "user": {
                    "parent": "root",
                    "message": {
                        "id": "message-user",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": ["hello"]}
                    }
                },
                "assistant-a": {
                    "parent": "user",
                    "message": {
                        "id": "message-a",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["old branch"]}
                    }
                },
                "assistant-b": {
                    "parent": "user",
                    "message": {
                        "id": "message-b",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["selected branch"]}
                    }
                }
            }
        }))
        .unwrap();

        let projected = project_conversation(response);
        assert_eq!(
            projected
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            ["hello", "selected branch"]
        );
    }

    #[test]
    fn projects_code_and_reasoning_without_tool_noise() {
        let code = ApiMessage {
            id: "code".into(),
            author: ApiAuthor {
                role: "assistant".into(),
                name: None,
            },
            content: json!({"content_type": "code", "language": "rust", "text": "fn main() {}"}),
            recipient: Some("all".into()),
            status: Some("finished_successfully".into()),
            metadata: Value::Null,
        };
        let tool_call = ApiMessage {
            id: "tool-call".into(),
            author: ApiAuthor {
                role: "assistant".into(),
                name: None,
            },
            content: json!({"content_type": "code", "text": "raw request"}),
            recipient: Some("web.run".into()),
            status: Some("finished_successfully".into()),
            metadata: Value::Null,
        };
        assert_eq!(
            project_message(&code).unwrap().content,
            "```rust\nfn main() {}\n```"
        );
        assert!(project_message(&tool_call).is_none());
    }

    #[test]
    fn citation_transport_markers_are_not_painted_as_transcript_text() {
        let text = ApiMessage {
            id: "cited-answer".into(),
            author: ApiAuthor {
                role: "assistant".into(),
                name: None,
            },
            content: json!({
                "content_type": "text",
                "parts": [
                    "First claim. \u{e200}cite\u{e202}turn1search0\u{e202}turn1search2\u{e201}\nSecond claim."
                ]
            }),
            recipient: Some("all".into()),
            status: Some("finished_successfully".into()),
            metadata: Value::Null,
        };

        assert_eq!(
            project_message(&text).unwrap().content,
            "First claim. \nSecond claim."
        );
    }

    #[test]
    fn url_transport_markers_become_markdown_links() {
        let text = sanitize_chatgpt_text(
            "See \u{e200}url\u{e202}JimLiu/decode-codex\u{e202}https://github.com/JimLiu/decode-codex\u{e201} for details, and hide \u{e200}future\u{e202}private-data\u{e201}.",
            &Value::Null,
        );

        assert_eq!(
            text,
            "See [JimLiu/decode-codex](https://github.com/JimLiu/decode-codex) for details, and hide ."
        );
        assert!(
            !text
                .chars()
                .any(|character| { matches!(character, '\u{e200}' | '\u{e201}' | '\u{e202}') })
        );
    }

    #[test]
    fn citation_transport_markers_use_the_server_reference_label() {
        let marker = "\u{e200}cite\u{e202}turn1search0\u{e201}";
        let text = sanitize_chatgpt_text(
            &format!("Supported claim {marker}."),
            &json!({
                "content_references": [{
                    "matched_text": marker,
                    "alt": "([Example](https://example.com/source))"
                }]
            }),
        );

        assert_eq!(
            text,
            "Supported claim ([Example](https://example.com/source))."
        );
    }

    #[test]
    fn web_tool_results_become_one_compact_activity() {
        let tool = ApiMessage {
            id: "tool-result".into(),
            author: ApiAuthor {
                role: "tool".into(),
                name: Some("web.run".into()),
            },
            content: json!({"content_type": "text", "parts": ["raw transport output"]}),
            recipient: Some("all".into()),
            status: Some("finished_successfully".into()),
            metadata: json!({
                "search_result_groups": [{
                    "entries": [{
                        "title": "Example result",
                        "url": "https://example.com/article",
                        "attribution": "example.com"
                    }]
                }]
            }),
        };

        let projected = project_message(&tool).unwrap();
        assert_eq!(projected.role, MessageRole::Activity);
        let activity = projected.activity.unwrap();
        assert_eq!(activity.kind, ActivityKind::WebSearch);
        assert_eq!(activity.title, "Searched the web");
        assert_eq!(activity.detail.as_deref(), Some("1 source · 1 site"));
        assert_eq!(activity.results[0].domain, "example.com");
        assert!(projected.content.is_empty());
    }

    #[test]
    fn stream_payloads_project_messages_without_protocol_envelopes() {
        let messages = project_stream_messages(&json!({
            "type": "message",
            "conversation_id": "conversation-1",
            "message": {
                "id": "assistant-1",
                "author": {"role": "assistant"},
                "content": {"content_type": "text", "parts": ["Streaming answer"]},
                "recipient": "all",
                "status": "in_progress"
            }
        }));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "assistant-1");
        assert_eq!(messages[0].content, "Streaming answer");
        assert_eq!(messages[0].role, MessageRole::Assistant);
    }

    #[test]
    fn send_request_uses_the_optimistic_message_identity() {
        let request = build_send_request(
            "conversation-1",
            "parent-1",
            "gpt-5-6",
            "Hello",
            "message-1",
        );

        assert_eq!(request["action"], "next");
        assert_eq!(request["conversation_id"], "conversation-1");
        assert_eq!(request["parent_message_id"], "parent-1");
        assert_eq!(request["model"], "gpt-5-6");
        assert_eq!(request["messages"][0]["id"], "message-1");
        assert_eq!(request["messages"][0]["content"]["parts"][0], "Hello");
    }

    #[test]
    fn model_catalog_preserves_current_and_legacy_lanes() {
        let catalog = project_model_catalog(
            serde_json::from_value(json!({
                "default_model_slug": "gpt-5-6",
                "categories": [
                    {
                        "default_model": "gpt-5-6-pro",
                        "human_category_name": "Pro",
                        "human_category_short_name": "5.6 Pro",
                        "model_lane": "pro",
                        "tagline": "Maximum capability"
                    },
                    {
                        "default_model": "gpt-5-5",
                        "human_category_name": "Instant",
                        "human_category_short_name": "5.5",
                        "model_lane": "instant",
                        "tagline": "",
                        "subcategory": "legacy"
                    },
                    {
                        "default_model": "gpt-5-6",
                        "human_category_name": "Auto",
                        "human_category_short_name": "5.6",
                        "model_lane": "auto",
                        "tagline": "Recommended"
                    }
                ]
            }))
            .unwrap(),
        );

        assert_eq!(catalog.default_model, "gpt-5-6");
        assert_eq!(catalog.choices[0].model, "gpt-5-6");
        assert_eq!(catalog.choices[1].model, "gpt-5-6-pro");
        assert!(!catalog.choices[1].legacy);
        assert!(catalog.choices[2].legacy);
    }

    #[test]
    fn truncated_private_markers_do_not_render_as_glyphs() {
        let text = sanitize_chatgpt_text(
            "Visible prose. \u{e200}cite\u{e202}turn1search0",
            &Value::Null,
        );
        assert_eq!(text, "Visible prose. ");
        assert!(
            !text
                .chars()
                .any(|character| character >= '\u{e000}' && character <= '\u{f8ff}')
        );
    }

    #[test]
    fn desktop_request_keeps_the_private_contract_at_one_boundary() {
        let request = build_request(
            "/conversations?offset=0&limit=1",
            &AuthTokens {
                access_token: "test-access-token".into(),
                account_id: "test-account".into(),
            },
        )
        .unwrap();
        assert_eq!(request.uri().host(), Some("chatgpt.com"));
        assert_eq!(request.headers()["originator"], DESKTOP_ORIGINATOR);
        assert_eq!(request.headers()["ChatGPT-Account-Id"], "test-account");
        assert!(
            request.headers()[http::header::USER_AGENT]
                .to_str()
                .unwrap()
                .starts_with("Codex Desktop/")
        );
    }
}
