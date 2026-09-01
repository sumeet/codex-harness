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
use futures::AsyncReadExt as _;
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Message {
    pub id: String,
    pub role: MessageRole,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Conversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<Message>,
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
}

#[derive(Deserialize)]
struct ApiAuthor {
    role: String,
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
        messages,
    }
}

fn project_message(message: &ApiMessage) -> Option<Message> {
    let content_type = message
        .content
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("text");
    let role = match (message.author.role.as_str(), content_type) {
        ("user", _) => MessageRole::User,
        ("assistant", "reasoning_recap" | "thoughts") => MessageRole::Reasoning,
        ("assistant", _) => MessageRole::Assistant,
        _ => return None,
    };
    let content = message_content(&message.content)?;
    (!content.trim().is_empty()).then(|| Message {
        id: message.id.clone(),
        role,
        content,
    })
}

fn message_content(content: &Value) -> Option<String> {
    match content.get("content_type").and_then(Value::as_str) {
        Some("text") => Some(sanitize_chatgpt_text(
            &content
                .get("parts")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        )),
        Some("code") => {
            let text = content.get("text")?.as_str()?;
            let language = content
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(format!("```{language}\n{text}\n```"))
        }
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
            Some(sanitize_chatgpt_text(&content))
        }),
        _ => None,
    }
}

fn sanitize_chatgpt_text(text: &str) -> String {
    const REFERENCE_START: char = '\u{e200}';
    const REFERENCE_END: char = '\u{e201}';

    let mut sanitized = String::with_capacity(text.len());
    let mut remainder = text;
    while let Some(start) = remainder.find(REFERENCE_START) {
        sanitized.push_str(&remainder[..start]);
        let payload = &remainder[start + REFERENCE_START.len_utf8()..];
        let Some(end) = payload.find(REFERENCE_END) else {
            // Preserve unknown or truncated backend markup instead of silently
            // deleting user-visible prose.
            sanitized.push(REFERENCE_START);
            remainder = payload;
            continue;
        };
        let after_reference = &payload[end + REFERENCE_END.len_utf8()..];
        if payload[..end].starts_with("cite") {
            remainder = after_reference;
        } else {
            sanitized.push(REFERENCE_START);
            sanitized.push_str(&payload[..end]);
            sanitized.push(REFERENCE_END);
            remainder = after_reference;
        }
    }
    sanitized.push_str(remainder);
    sanitized
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
            },
            content: json!({"content_type": "code", "language": "rust", "text": "fn main() {}"}),
        };
        let tool = ApiMessage {
            id: "tool".into(),
            author: ApiAuthor {
                role: "tool".into(),
            },
            content: json!({"content_type": "text", "parts": ["internal"]}),
        };
        assert_eq!(
            project_message(&code).unwrap().content,
            "```rust\nfn main() {}\n```"
        );
        assert!(project_message(&tool).is_none());
    }

    #[test]
    fn citation_transport_markers_are_not_painted_as_transcript_text() {
        let text = ApiMessage {
            id: "cited-answer".into(),
            author: ApiAuthor {
                role: "assistant".into(),
            },
            content: json!({
                "content_type": "text",
                "parts": [
                    "First claim. \u{e200}cite\u{e202}turn1search0\u{e202}turn1search2\u{e201}\nSecond claim."
                ]
            }),
        };

        assert_eq!(
            project_message(&text).unwrap().content,
            "First claim. \nSecond claim."
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
