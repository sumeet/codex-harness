use super::*;

fn claude_permission_label(mode: &str) -> &str {
    match mode {
        "default" => "Ask permissions",
        "acceptEdits" => "Accept edits",
        "plan" => "Plan mode",
        "auto" => "Auto permissions",
        "bypassPermissions" => "Bypass permissions",
        "" => "Permissions",
        other => other,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct PendingSend {
    id: String,
    text: String,
    session_id: String,
}

pub(super) fn merge_send_journal(
    store: &mut ComposerDraftStore,
    latest: &ComposerDraftStore,
) -> anyhow::Result<()> {
    store
        .claude_resolved_sends
        .extend(latest.claude_resolved_sends.iter().cloned());
    store
        .claude_accepted_sends
        .extend(latest.claude_accepted_sends.iter().cloned());
    for (key, pending) in &latest.claude_pending_sends {
        if store.claude_resolved_sends.contains(&pending.id) {
            continue;
        }
        if let Some(local) = store.claude_pending_sends.get(key) {
            anyhow::ensure!(
                local.id == pending.id
                    && local.text == pending.text
                    && local.session_id == pending.session_id,
                "Another window has a different unresolved Claude send for this draft. Neither send was overwritten"
            );
        } else {
            store
                .claude_pending_sends
                .insert(key.clone(), pending.clone());
        }
    }
    for (key, pending) in &store.claude_pending_sends {
        if store.claude_accepted_sends.contains(&pending.id)
            && store.drafts.get(key) == Some(&pending.text)
        {
            store.drafts.remove(key);
        }
    }
    store
        .claude_pending_sends
        .retain(|_, pending| !store.claude_resolved_sends.contains(&pending.id));
    Ok(())
}

fn prepare_submission(
    store: &mut ComposerDraftStore,
    draft_key: &str,
    session_id: &str,
    text: &str,
) -> anyhow::Result<PendingSend> {
    if let Some(pending) = store.claude_pending_sends.get(draft_key) {
        anyhow::ensure!(
            pending.session_id == session_id,
            "An earlier send belongs to the previous native session. It has not been resent; its saved receipt must be checked first"
        );
        return Ok(pending.clone());
    }
    anyhow::ensure!(
        !text.trim().is_empty() && text.encode_utf16().count() <= 100_000,
        "Claude prompts must contain between 1 and 100000 UTF-16 code units"
    );
    let pending = PendingSend {
        id: Uuid::new_v4().to_string(),
        text: text.to_owned(),
        session_id: session_id.to_owned(),
    };
    store
        .claude_pending_sends
        .insert(draft_key.to_owned(), pending.clone());
    store.drafts.insert(draft_key.to_owned(), text.to_owned());
    Ok(pending)
}

fn verified_submission_receipt(result: Value, submission_id: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(
        result["uuid"].as_str() == Some(submission_id),
        "Claude returned a receipt for a different submission; draft kept"
    );
    anyhow::ensure!(
        result["accepted"] == true,
        "{}",
        result["message"]
            .as_str()
            .unwrap_or("Claude has not confirmed acceptance; draft kept")
    );
    Ok(result)
}

fn choose_claude_draft(
    conversation: &claude_native::Conversation,
    preferred: Option<&str>,
    drafts: &HashMap<String, String>,
) -> String {
    let keys: Vec<_> = conversation
        .aliases
        .iter()
        .map(|alias| format!("claude:{alias}"))
        .collect();
    if let Some(preferred) = preferred.filter(|preferred| keys.iter().any(|key| key == preferred)) {
        return preferred.to_owned();
    }
    keys.iter()
        .find(|key| drafts.get(*key).is_some_and(|text| !text.is_empty()))
        .cloned()
        .unwrap_or_else(|| format!("claude:{}", conversation.id))
}

pub(super) fn empty_state_message(
    ready: bool,
    phase: Option<&claude_native::HostPhase>,
) -> &'static str {
    use claude_native::HostPhase;
    if ready {
        return "Send a prompt to start the conversation";
    }
    match phase {
        Some(HostPhase::Starting) => "Starting native Claude…",
        Some(HostPhase::CheckingCompatibility) => "Checking native Claude compatibility…",
        Some(HostPhase::NeedsSetup) => "Claude setup isn't finished — see the error above",
        Some(HostPhase::Stopped) => "This Claude host has stopped",
        Some(HostPhase::Failed) => "Native Claude needs attention — see the error above",
        Some(HostPhase::Unavailable) => "This Claude host is unavailable",
        Some(HostPhase::Saved) => "No saved messages in this conversation",
        Some(HostPhase::NeedsAdapter) => "Native session is not connected to Harness",
        Some(HostPhase::Available) | None => "Waiting for a connection to native Claude",
    }
}

pub(super) struct ClaudeWorkspace {
    pub sessions: Vec<claude_native::Conversation>,
    pub selected: Option<claude_native::Session>,
    pub selected_id: Option<String>,
    pub selected_draft_id: Option<String>,
    pub projection: claude_native::Projection,
    pub transcript: TranscriptModel,
    pub list: ListState,
    pub sidebar: ListState,
    pub error: Option<SharedString>,
    pub catalog_warnings: Vec<String>,
    pub selected_creation: Option<claude_native::creation::Pending>,
    pub show_startup_details: bool,
    pub statuses: HashMap<String, claude_native::HostStatus>,
    pub starting: bool,
    pub setup: claude_native::setup::Status,
    pub configuring: bool,
    pub show_hidden: bool,
    pub hidden_count: usize,
    pub opening: HashSet<String>,
    pub sending: bool,
    pub settings_pending: bool,
    pub stream_task: Task<()>,
}

#[cfg(test)]
mod tests {
    use super::{
        choose_claude_draft, empty_state_message, prepare_submission, verified_submission_receipt,
    };
    use crate::claude_native::HostPhase;

    #[test]
    fn retries_restore_the_original_id_and_do_not_substitute_an_edited_draft() -> anyhow::Result<()>
    {
        let mut store = crate::ComposerDraftStore::default();
        let first = prepare_submission(&mut store, "claude:thread", "native-session", "original")?;
        let mut reopened = serde_json::from_slice(&serde_json::to_vec(&store)?)?;
        let retry = prepare_submission(&mut reopened, "claude:thread", "native-session", "edited")?;
        assert_eq!(retry.id, first.id);
        assert_eq!(retry.text, "original");
        assert!(
            prepare_submission(
                &mut reopened,
                "claude:thread",
                "different-session",
                "original"
            )
            .is_err()
        );
        reopened.claude_pending_sends.remove("claude:thread");
        reopened.claude_resolved_sends.insert(first.id.clone());
        let deliberate_repeat =
            prepare_submission(&mut reopened, "claude:thread", "native-session", "original")?;
        assert_ne!(deliberate_repeat.id, first.id);
        Ok(())
    }

    #[test]
    fn stale_draft_saves_cannot_erase_or_reactivate_a_send_id() -> anyhow::Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "harness-claude-send-store-{}",
            uuid::Uuid::new_v4()
        ));
        let path = directory.join("drafts.json");
        let stale = crate::ComposerDraftStore::default();
        let mut sending = stale.clone();
        let first = prepare_submission(&mut sending, "claude:thread", "native", "text")?;
        crate::persist_composer_drafts_at(&path, &sending)?;
        crate::persist_composer_drafts_at(&path, &stale)?;
        let read = crate::read_composer_drafts_at(&path)?;
        assert_eq!(read.claude_pending_sends["claude:thread"].id, first.id);
        let mut acknowledged = sending.clone();
        acknowledged.claude_pending_sends.clear();
        acknowledged.claude_resolved_sends.insert(first.id.clone());
        acknowledged.claude_accepted_sends.insert(first.id.clone());
        crate::persist_composer_drafts_at(&path, &acknowledged)?;
        crate::persist_composer_drafts_at(&path, &sending)?;
        let read = crate::read_composer_drafts_at(&path)?;
        assert!(read.claude_pending_sends.is_empty());
        assert!(read.claude_resolved_sends.contains(&first.id));
        assert!(!read.drafts.contains_key("claude:thread"));
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn conflicting_unresolved_sends_do_not_overwrite_each_other() -> anyhow::Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "harness-claude-send-store-{}",
            uuid::Uuid::new_v4()
        ));
        let path = directory.join("drafts.json");
        let mut first = crate::ComposerDraftStore::default();
        let mut second = first.clone();
        let original = prepare_submission(&mut first, "claude:thread", "native", "first")?;
        prepare_submission(&mut second, "claude:thread", "native", "second")?;
        crate::persist_composer_drafts_at(&path, &first)?;
        assert!(crate::persist_composer_drafts_at(&path, &second).is_err());
        assert_eq!(
            crate::read_composer_drafts_at(&path)?.claude_pending_sends["claude:thread"].id,
            original.id
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn only_a_matching_accepted_receipt_consumes_the_draft() {
        use serde_json::json;
        for receipt in [
            json!({}),
            json!({"uuid":"intent", "accepted":false, "state":"uncertain"}),
            json!({"uuid":"intent", "accepted":false, "state":"rejected"}),
            json!({"uuid":"other", "accepted":true}),
        ] {
            assert!(verified_submission_receipt(receipt, "intent").is_err());
        }
        assert!(
            verified_submission_receipt(json!({"uuid":"intent", "accepted":true}), "intent")
                .is_ok()
        );
    }

    #[test]
    fn handoff_drafts_remain_distinct_and_a_consumed_draft_stays_selected() {
        use crate::claude_native::{Conversation, Session, SessionSource};
        let mut conversation = Conversation::from_session(Session {
            id: "original".into(),
            directory: "/private".into(),
            cwd: "/workspace".into(),
            title: "Example".into(),
            lifecycle_version: 0,
            created_at_ms: 0,
            source: SessionSource::Managed,
        });
        conversation.aliases.push("replacement".into());
        let mut drafts = std::collections::HashMap::from([
            ("claude:original".into(), "original draft".into()),
            ("claude:replacement".into(), "separate draft".into()),
        ]);
        assert_eq!(
            choose_claude_draft(&conversation, None, &drafts),
            "claude:original"
        );
        assert_eq!(
            choose_claude_draft(&conversation, Some("claude:replacement"), &drafts),
            "claude:replacement"
        );
        drafts.remove("claude:original");
        assert_eq!(
            choose_claude_draft(&conversation, None, &drafts),
            "claude:replacement"
        );
        assert_eq!(
            choose_claude_draft(&conversation, Some("claude:original"), &drafts),
            "claude:original"
        );
        assert_eq!(drafts["claude:replacement"], "separate draft");
    }

    #[test]
    fn empty_state_only_prompts_for_native_setup_when_setup_is_needed() {
        assert!(
            empty_state_message(false, Some(&HostPhase::NeedsSetup))
                .contains("setup isn't finished")
        );
        for phase in [
            HostPhase::Stopped,
            HostPhase::Failed,
            HostPhase::Unavailable,
            HostPhase::Available,
        ] {
            let message = empty_state_message(false, Some(&phase));
            assert!(!message.contains("setup"));
            assert!(!message.contains("send a prompt"));
        }
        assert!(empty_state_message(true, Some(&HostPhase::Available)).contains("Send a prompt"));
        assert!(empty_state_message(false, None).contains("Waiting for a connection"));
    }
}

impl ClaudeWorkspace {
    pub fn new(selected_id: Option<String>) -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(2048.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            sessions: Vec::new(),
            selected: None,
            selected_id,
            selected_draft_id: None,
            projection: claude_native::Projection::default(),
            transcript: TranscriptModel::default(),
            list,
            sidebar: ListState::new(0, ListAlignment::Top, px(512.)).measure_all(),
            error: None,
            catalog_warnings: Vec::new(),
            selected_creation: None,
            show_startup_details: false,
            statuses: HashMap::new(),
            starting: false,
            setup: claude_native::setup::Status::default(),
            configuring: false,
            show_hidden: false,
            hidden_count: 0,
            opening: HashSet::new(),
            sending: false,
            settings_pending: false,
            stream_task: Task::ready(()),
        }
    }
}

impl HarnessApp {
    pub(super) fn render_claude_controls(&self, cx: &Context<Self>) -> AnyElement {
        let mut row = div().flex().items_center().gap_1();
        row = row
            .child(self.render_claude_setting("permissionMode", cx))
            .child(self.render_claude_setting("model", cx));
        let supports_effort = self
            .claude
            .projection
            .settings
            .as_ref()
            .is_some_and(|settings| {
                settings.models.iter().any(|model| {
                    model.value == settings.model && !model.supported_effort_levels.is_empty()
                })
            });
        if supports_effort {
            row = row.child(self.render_claude_setting("effort", cx));
        }
        row.into_any_element()
    }

    fn render_claude_setting(&self, key: &'static str, cx: &Context<Self>) -> AnyElement {
        let settings = self.claude.projection.settings.as_ref();
        let mut entries: Vec<(String, String, bool, Option<String>)> = Vec::new();
        let (title, label, expected, selected) = match key {
            "model" => {
                if let Some(settings) = settings {
                    entries.extend(settings.models.iter().map(|choice| {
                        (
                            choice.value.clone(),
                            choice.display_name.clone(),
                            true,
                            None,
                        )
                    }));
                }
                let value = settings
                    .map(|settings| settings.model.clone())
                    .unwrap_or_default();
                let label = entries
                    .iter()
                    .find(|entry| entry.0 == value)
                    .map(|entry| entry.1.clone())
                    .unwrap_or_else(|| {
                        if value.is_empty() {
                            "Claude model".into()
                        } else {
                            value.clone()
                        }
                    });
                ("Claude model", label, json!(value), value)
            }
            "effort" => {
                if let Some(settings) = settings {
                    entries.push(("auto".into(), "Auto effort".into(), true, None));
                    if let Some(model) = settings
                        .models
                        .iter()
                        .find(|model| model.value == settings.model)
                    {
                        entries.extend(model.supported_effort_levels.iter().map(|effort| {
                            (
                                effort.clone(),
                                reasoning_effort_label(effort).to_string(),
                                true,
                                None,
                            )
                        }));
                    }
                }
                let expected = settings
                    .map(|settings| settings.effort.clone())
                    .unwrap_or(Value::Null);
                let value = match expected["kind"].as_str() {
                    Some("level") => expected["value"].as_str().unwrap_or("inherit"),
                    Some("default") => "auto",
                    _ => "inherit",
                }
                .to_owned();
                let label = match value.as_str() {
                    "auto" => "Auto effort".to_owned(),
                    "inherit" => "Inherited effort".to_owned(),
                    value => reasoning_effort_label(value).to_string(),
                };
                ("Claude effort", label, expected, value)
            }
            _ => {
                if let Some(settings) = settings {
                    entries.extend(settings.permissions.iter().map(|choice| {
                        (
                            choice.value.clone(),
                            claude_permission_label(&choice.value).to_owned(),
                            choice.available,
                            choice.reason.clone(),
                        )
                    }));
                }
                let value = settings
                    .and_then(|settings| settings.permission_mode.clone())
                    .unwrap_or_default();
                (
                    "Claude permissions",
                    claude_permission_label(&value).to_owned(),
                    json!(value),
                    value,
                )
            }
        };
        let disabled = !self.claude.projection.ready
            || self.claude.settings_pending
            || self.claude.projection.active
            || !self.claude.projection.dialogs.is_empty();
        let available = self.claude.projection.settings_controls;
        let session_id = self.claude.projection.session_id.clone();
        let epoch = self.claude.projection.epoch.clone();
        let weak = cx.weak_entity();
        let trigger = Button::new(format!("claude-{key}-trigger"), label)
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .when(key == "model", |button| {
                button.start_icon(
                    Icon::new(IconName::AiClaude)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .end_icon(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .disabled(disabled)
            .aria_label(title);
        PopoverMenu::new(format!("claude-{key}-menu"))
            .trigger(trigger)
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let entries = entries.clone();
                let selected = selected.clone();
                let expected = expected.clone();
                let session_id = session_id.clone();
                let epoch = epoch.clone();
                let weak = weak.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header(title);
                    if !available {
                        return menu.custom_row(|_, cx| {
                            div()
                                .w(px(320.))
                                .p_2()
                                .whitespace_normal()
                                .text_sm()
                                .text_color(cx.theme().colors().text_muted)
                                .child("These controls aren't supported by this running Claude connection. Updating Harness alone doesn't update existing Claude sessions.")
                                .into_any_element()
                        });
                    }
                    if entries.is_empty() {
                        return menu.item(
                            ContextMenuEntry::new("Waiting for Claude's settings…").disabled(true),
                        );
                    }
                    for (value, label, enabled, reason) in entries {
                        let request = json!({"method":"set_setting", "key":key, "value":value,
                            "expected":expected, "sessionId":session_id, "epoch":epoch});
                        let weak = weak.clone();
                        let entry = ContextMenuEntry::new(label)
                            .toggleable(IconPosition::End, value == selected)
                            .disabled(!enabled)
                            .handler(move |_, cx| {
                                if let Err(error) = weak.update(cx, |this, cx| {
                                    this.change_claude_setting(request.clone(), cx)
                                }) {
                                    log::debug!("Claude window closed: {error}");
                                }
                            });
                        let entry = if let Some(reason) = reason.filter(|_| !enabled) {
                            entry.documentation_aside(DocumentationSide::Left, move |_| {
                                div()
                                    .w(px(300.))
                                    .whitespace_normal()
                                    .child(reason.clone())
                                    .into_any_element()
                            })
                        } else {
                            entry
                        };
                        menu = menu.item(entry);
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    fn change_claude_setting(&mut self, request: Value, cx: &mut Context<Self>) {
        if !self.claude.projection.ready
            || self.claude.settings_pending
            || request["sessionId"] != self.claude.projection.session_id
            || request["epoch"] != self.claude.projection.epoch
        {
            return;
        }
        let Some(session) = self.claude.selected.clone() else {
            return;
        };
        let epoch = self.claude.projection.epoch.clone();
        let selected_id = self.claude.selected_id.clone();
        self.claude.settings_pending = true;
        self.claude.error = None;
        cx.spawn(async move |this, cx| {
            let result = claude_native::request(&session, request).await;
            if let Err(error) = this.update(cx, |this, cx| {
                if this.claude.selected_id != selected_id || this.claude.projection.epoch != epoch {
                    return;
                }
                this.claude.settings_pending = false;
                if let Err(error) = result {
                    this.claude.error =
                        Some(format!("Could not change Claude setting: {error:#}").into());
                }
                cx.notify();
            }) {
                log::debug!("Claude window closed: {error}");
            }
        })
        .detach();
        cx.notify();
    }

    async fn ensure_claude_setup(
        owner: &WeakEntity<Self>,
        cx: &mut gpui::AsyncWindowContext,
    ) -> anyhow::Result<bool> {
        let status = cx
            .background_spawn(async { claude_native::setup::status() })
            .await;
        if status.configured {
            owner.update(cx, |this, _| this.claude.setup = status)?;
            return Ok(true);
        }
        let answer = owner.update_in(cx, |this, window, cx| {
            if this.claude.configuring || this.workspace_mode != WorkspaceMode::Claude {
                return None;
            }
            this.claude.configuring = true;
            this.claude.setup = status;
            cx.notify();
            Some(window.prompt(gpui::PromptLevel::Info,
                "Use Claude in Harness?",
                Some("Harness needs to add its integration to your Claude settings. It will back up those settings, preserve unrelated options, and keep Claude's executable unchanged. After setup, this conversation will open here. Existing sessions will not be restarted."),
                &["Set up and continue", "Cancel"], cx))
        })?;
        let Some(answer) = answer else {
            return Ok(false);
        };
        if answer.await != Ok(0) {
            owner.update(cx, |this, cx| {
                this.claude.configuring = false;
                cx.notify();
            })?;
            return Ok(false);
        }
        let result = cx
            .background_spawn(async { claude_native::setup::enable() })
            .await;
        owner.update(cx, |this, cx| {
            this.claude.configuring = false;
            if let Ok(status) = &result {
                this.claude.setup = status.clone();
            }
            cx.notify();
        })?;
        result?;
        Ok(true)
    }

    fn activate_claude(
        &mut self,
        session: claude_native::Conversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session
            .aliases
            .iter()
            .any(|id| self.claude.opening.contains(id))
        {
            return;
        }
        self.claude.opening.insert(session.id.clone());
        self.claude.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let selected = session.id.clone();
            let result = async {
                let result = cx
                    .background_spawn({
                        let session = session.clone();
                        async move { session.open() }
                    })
                    .await;
                if !result
                    .as_ref()
                    .is_err_and(|error| error.is::<claude_native::setup::ConsentRequired>())
                {
                    return result.map(Some);
                }
                if !Self::ensure_claude_setup(&this, cx).await? {
                    return Ok(None);
                }
                cx.background_spawn(async move { session.open() })
                    .await
                    .map(Some)
            }
            .await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.claude.opening.remove(&selected);
                let selected = this
                    .claude
                    .sessions
                    .iter()
                    .find(|entry| entry.aliases.contains(&selected))
                    .map(|entry| entry.id.clone())
                    .unwrap_or(selected);
                this.claude.opening.remove(&selected);
                match result {
                    Ok(Some(session)) => {
                        if let Some(entry) = this
                            .claude
                            .sessions
                            .iter_mut()
                            .find(|entry| entry.id == selected)
                        {
                            entry.bind(session.clone());
                        }
                        if this.claude.selected_id.as_deref() == Some(&selected) {
                            this.connect_claude(session, selected, true, cx);
                        }
                        this.refresh_claude(cx);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        log::warn!("Could not open Claude conversation {selected}: {error:#}");
                        if this.claude.selected_id.as_deref() == Some(&selected) {
                            if let Some(request) = &mut this.claude.selected_creation {
                                request.detail = format!("{error:#}");
                            } else {
                                this.claude.error =
                                    Some(format!("Could not open Claude: {error:#}").into());
                            }
                        }
                    }
                }
                cx.notify();
            }) {
                log::debug!("Claude continuation view closed: {error}");
            }
        })
        .detach();
    }

    fn configure_claude(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.claude.configuring {
            return;
        }
        let configured = self.claude.setup.configured;
        let answer = window.prompt(gpui::PromptLevel::Warning,
            if configured { "Claude settings" } else { "Use Claude in Harness?" },
            Some(if configured {
                "Refresh checks compatibility and repairs Harness's integration settings. Disable prevents future Claude sessions from connecting to Harness. Neither action restarts existing conversations or changes Claude's executable."
            } else {
                "Adds Harness's integration to your Claude settings, preserving unrelated options and saving a backup. Claude's executable is unchanged. Existing conversations will not be restarted."
            }), if configured { &["Refresh integration", "Disable integration", "Cancel"] } else { &["Set up Claude", "Cancel"] }, cx);
        self.claude.configuring = true;
        cx.spawn(async move |this, cx| {
            let answer = answer.await;
            let proceed = answer == Ok(0) || (configured && answer == Ok(1));
            let disable = configured && answer == Ok(1);
            if !proceed {
                if let Err(error) = this.update(cx, |this, cx| {
                    this.claude.configuring = false;
                    cx.notify();
                }) {
                    log::debug!("Claude setup view closed: {error}");
                }
                return;
            }
            let result = cx
                .background_spawn(async move {
                    if disable {
                        claude_native::setup::disable()
                    } else {
                        claude_native::setup::enable()
                    }
                })
                .await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.claude.configuring = false;
                match result {
                    Ok(status) => {
                        this.claude.setup = status;
                        this.claude.error = None;
                    }
                    Err(error) => {
                        this.claude.error =
                            Some(format!("Claude setup did not complete: {error:#}").into())
                    }
                }
                cx.notify();
            }) {
                log::debug!("Claude setup view closed: {error}");
            }
        })
        .detach();
        cx.notify();
    }

    pub(super) fn refresh_claude(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async { claude_native::sessions() })
                .await;
            if let Err(error) = this.update(cx, |this, cx| {
                match result {
                    Ok(mut catalog) => {
                        let hidden: HashSet<_> = catalog.hidden_conversations.into_iter().collect();
                        this.claude.hidden_count = catalog
                            .conversations
                            .iter()
                            .filter(|conversation| {
                                conversation
                                    .aliases
                                    .iter()
                                    .any(|alias| hidden.contains(alias))
                            })
                            .count();
                        if !this.claude.show_hidden {
                            catalog.conversations.retain(|conversation| {
                                !conversation
                                    .aliases
                                    .iter()
                                    .any(|alias| hidden.contains(alias))
                            });
                        }
                        this.claude.sidebar.reset(catalog.conversations.len());
                        this.claude.sessions = catalog.conversations;
                        this.claude.opening = this
                            .claude
                            .opening
                            .iter()
                            .map(|id| {
                                this.claude
                                    .sessions
                                    .iter()
                                    .find(|entry| entry.aliases.contains(id))
                                    .map(|entry| entry.id.clone())
                                    .unwrap_or_else(|| id.clone())
                            })
                            .collect();
                        this.claude.statuses = catalog.statuses;
                        if let (Some(selected_id), Some(endpoint)) =
                            (&this.claude.selected_id, &this.claude.selected)
                            && let Some(status) = this.claude.statuses.get(&endpoint.id).cloned()
                        {
                            this.claude.statuses.insert(selected_id.clone(), status);
                        }
                        this.claude.catalog_warnings = catalog.warnings;
                        this.claude.setup = catalog.setup;
                        if let Some(conversation) =
                            this.claude.sessions.iter().find(|conversation| {
                                this.claude
                                    .selected_id
                                    .as_ref()
                                    .is_some_and(|id| conversation.aliases.contains(id))
                            })
                        {
                            this.claude.selected_draft_id = Some(choose_claude_draft(
                                conversation,
                                this.claude.selected_draft_id.as_deref(),
                                &this.composer_drafts.drafts,
                            ));
                            if this.claude.selected_id.as_ref() != Some(&conversation.id) {
                                this.claude.selected_id = Some(conversation.id.clone());
                                this.persist_session();
                            }
                        }
                        let selected_index = this
                            .claude
                            .sessions
                            .iter()
                            .position(|session| {
                                Some(&session.id) == this.claude.selected_id.as_ref()
                            })
                            .unwrap_or(0);
                        if !this.claude.sessions.is_empty() {
                            this.claude.sidebar.scroll_to_reveal_item(selected_index);
                        }
                        let selected = this
                            .claude
                            .sessions
                            .iter()
                            .find(|session| Some(&session.id) == this.claude.selected_id.as_ref())
                            .cloned();
                        if let Some(session) = selected
                            && !this.claude.projection.ready
                            && !this.claude.opening.contains(&session.id)
                        {
                            if let Some(request) = session.creation() {
                                this.claude.selected_creation = Some(request.clone());
                                this.claude.show_startup_details = false;
                                cx.notify();
                                return;
                            }
                            let Some(current) = session.current().cloned() else {
                                return;
                            };
                            let connect_live = session.aliases.len() == 1
                                || this
                                    .claude
                                    .selected
                                    .as_ref()
                                    .is_some_and(|active| active.id == current.id);
                            this.connect_claude(current, session.id, connect_live, cx);
                        }
                    }
                    Err(error) => {
                        this.claude.error =
                            Some(format!("Could not list Claude sessions: {error:#}").into())
                    }
                }
                cx.notify();
            }) {
                log::debug!("Claude workspace closed: {error}");
            }
        })
        .detach();
    }

    pub(super) fn new_claude(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_task_picker_open || self.claude.starting {
            return;
        }
        self.new_task_picker_open = true;
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Start Claude Here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths = paths.await;
            let path = match paths {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => {
                    if let Err(update_error) = this.update(cx, |this, cx| {
                        this.claude.error =
                            Some(format!("Could not choose a project: {error}").into());
                        cx.notify();
                    }) {
                        log::debug!("Claude workspace closed: {update_error}");
                    }
                    None
                }
            };
            if let Err(error) = this.update(cx, |this, cx| {
                this.new_task_picker_open = false;
                cx.notify();
            }) {
                log::debug!("Claude workspace closed: {error}");
                return;
            }
            let Some(path) = path else {
                return;
            };
            if let Err(error) = this.update(cx, |this, cx| {
                this.claude.starting = true;
                this.claude.error = None;
                cx.notify();
            }) {
                log::debug!("Claude workspace closed: {error}");
                return;
            }
            let result = async {
                if !Self::ensure_claude_setup(&this, cx).await? {
                    return Ok(None);
                }
                cx.background_spawn(async move { claude_native::start(path) })
                    .await
                    .map(Some)
            }
            .await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.claude.starting = false;
                match result {
                    Ok(Some(session)) => {
                        let session = claude_native::Conversation::from_session(session);
                        this.claude.sessions.push(session.clone());
                        this.claude.sidebar.splice(
                            this.claude.sessions.len() - 1..this.claude.sessions.len() - 1,
                            1,
                        );
                        this.open_claude(session, window, cx);
                        this.focus_composer(window, cx);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.claude.error =
                            Some(format!("Could not start Claude: {error:#}").into())
                    }
                }
                this.refresh_claude(cx);
                cx.notify();
            }) {
                log::debug!("Claude workspace closed: {error}");
            }
        })
        .detach();
    }

    pub(super) fn open_claude(
        &mut self,
        session: claude_native::Conversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.claude.selected_id.as_deref() == Some(&session.id)
            && (self.claude.projection.ready || self.claude.opening.contains(&session.id))
        {
            return;
        }
        self.claude.selected_draft_id = Some(choose_claude_draft(
            &session,
            self.claude.selected_draft_id.as_deref(),
            &self.composer_drafts.drafts,
        ));
        if let Some(request) = session.creation() {
            self.claude.stream_task = Task::ready(());
            self.claude.selected = session.current().cloned();
            self.claude.selected_id = Some(session.id.clone());
            self.claude.selected_creation = Some(request.clone());
            self.claude.show_startup_details = false;
            self.claude.settings_pending = false;
            self.claude.projection = claude_native::Projection::default();
            self.claude.error = None;
            self.switch_composer_draft_context(self.claude.selected_draft_id.clone(), cx);
            self.rebuild_claude(cx);
            self.persist_session();
            self.activate_claude(session, window, cx);
            return;
        }
        let (Some(current), Some(entry)) = (session.current(), session.entry()) else {
            return;
        };
        let history = if matches!(
            &current.source,
            claude_native::SessionSource::Native {
                transcript: None,
                ..
            }
        ) {
            entry.clone()
        } else {
            current.clone()
        };
        self.connect_claude(history, session.id.clone(), entry.is_managed(), cx);
        if !entry.is_managed() {
            self.activate_claude(session, window, cx);
        }
    }

    fn connect_claude(
        &mut self,
        session: claude_native::Session,
        selected_id: String,
        connect_live: bool,
        cx: &mut Context<Self>,
    ) {
        self.claude.stream_task = Task::ready(());
        self.claude.selected_creation = None;
        self.claude.show_startup_details = false;
        self.claude.sending = false;
        self.claude.settings_pending = false;
        if self.claude.selected_id.as_deref() == Some(&selected_id) {
            self.claude.projection.disconnect();
        } else {
            self.claude.projection = claude_native::Projection::default();
        }
        // Native /bg changes the transport UUID. Selection and unsent text stay
        // attached to the conversation the user opened, not its replacement PID/UUID.
        self.claude.selected_id = Some(selected_id.clone());
        self.claude.selected = Some(session.clone());
        self.claude.error = None;
        if self.workspace_mode == WorkspaceMode::Claude {
            self.switch_composer_draft_context(
                self.claude
                    .selected_draft_id
                    .clone()
                    .or_else(|| Some(format!("claude:{selected_id}"))),
                cx,
            );
        }
        self.rebuild_claude(cx);
        self.persist_session();
        self.claude.stream_task = cx.spawn(async move |this, cx| {
            if !session.is_managed() {
                let result = cx.background_spawn({
                    let session = session.clone();
                    async move { session.saved_history() }
                }).await;
                if let Err(error) = this.update(cx, |this, cx| {
                    match result {
                        Ok(projection) => this.claude.projection = projection,
                        Err(error) => this.claude.error = Some(format!("Could not read saved Claude history: {error:#}").into()),
                    }
                    this.rebuild_claude(cx);
                }) { log::debug!("Claude workspace closed: {error}"); return; }
            }
            if !connect_live { return; }
            let mut failures = 0;
            loop {
                let status = cx.background_spawn({
                    let session = session.clone();
                    async move { session.status() }
                }).await;
                if !status.can_reconnect() {
                    if let Err(error) = this.update(cx, |this, cx| {
                        if !this.claude.opening.contains(&selected_id)
                            && !matches!(status.phase, claude_native::HostPhase::Saved | claude_native::HostPhase::NeedsAdapter) {
                            this.claude.error = Some(status.message.clone().into());
                        }
                        this.claude.statuses.insert(selected_id.clone(), status);
                        this.claude.projection.disconnect();
                        this.rebuild_claude(cx);
                    }) { log::debug!("Claude workspace closed: {error}"); }
                    return;
                }
                if let Err(error) = this.update(cx, |this, cx| {
                    this.claude.statuses.insert(selected_id.clone(), status.clone());
                    cx.notify();
                }) { log::debug!("Claude workspace closed: {error}"); return; }
                let attempt_started = std::time::Instant::now();
                let monitor = cx.background_spawn({
                    let session = session.clone();
                    async move {
                        loop {
                            smol::Timer::after(Duration::from_secs(2)).await;
                            let status = session.status();
                            if !status.can_reconnect() { return Err::<(), _>(anyhow::anyhow!(status.message)); }
                        }
                    }
                });
                let result: anyhow::Result<()> = smol::future::race(async {
                    let (mut reader, snapshot) = claude_native::snapshot_connection(&session).await?;
                    this.update(cx, |this, cx| -> anyhow::Result<()> {
                        this.claude.projection.apply(snapshot)?;
                        this.claude.settings_pending = false;
                        this.claude.error = None;
                        this.claude.statuses.insert(selected_id.clone(), claude_native::HostStatus {
                            phase: claude_native::HostPhase::Available,
                            message: "Connected to native Claude".into(),
                        });
                        this.rebuild_claude(cx);
                        Ok(())
                    })??;
                    loop {
                        let frame = claude_native::read_frame(&mut reader).await?;
                        let settings_changed = frame["event"] == "settings";
                        let settings_error = (frame["event"] == "settings_error")
                            .then(|| frame["data"]["message"].as_str().unwrap_or("Could not load Claude settings").to_owned());
                        let visual = matches!(frame["event"].as_str(), Some("transcript" | "turn" | "dialogs"))
                            || frame["id"] == "snapshot"
                            || (frame["event"] == "engine_event" && frame["data"]["type"] == "stream_event"
                                && matches!(frame["data"]["event"]["type"].as_str(), Some("message_start" | "content_block_start" | "content_block_delta")));
                        this.update(cx, |this, cx| -> anyhow::Result<()> {
                            this.claude.projection.apply(frame)?;
                            if this.claude.projection.ready { this.claude.error = None; }
                            if let Some(error) = settings_error {
                                this.claude.error = Some(error.into());
                                cx.notify();
                            }
                            if settings_changed { cx.notify(); }
                            if visual { this.rebuild_claude(cx); }
                            Ok(())
                        })??;
                    }
                }, monitor).await;
                if let Err(error) = result {
                    let status = cx.background_spawn({ let session = session.clone(); async move { session.status() } }).await;
                    failures = if attempt_started.elapsed() >= Duration::from_secs(30) { 1 } else { failures + 1 };
                    let retry = claude_native::should_reconnect(&status, failures);
                    if this.update(cx, |this, cx| {
                        this.claude.projection.disconnect();
                        this.claude.settings_pending = false;
                        this.claude.error = Some(if !status.can_reconnect() || status.phase != claude_native::HostPhase::Available { status.message.clone() }
                            else if retry { format!("Claude disconnected: {error:#} · retry {failures}/5; no prompts are resent") }
                            else { format!("Could not reconnect to Claude: {error:#}. Automatic retries paused; use Reconnect to try again. Your native process was not restarted.") }.into());
                        this.claude.statuses.insert(selected_id.clone(), status);
                        this.rebuild_claude(cx);
                    }).is_err() { return; }
                    if !retry { return; }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        });
        cx.notify();
    }

    fn rebuild_claude(&mut self, cx: &mut Context<Self>) {
        let mut items = self.claude.projection.items();
        let expanded: HashMap<_, _> = self
            .claude
            .transcript
            .items
            .iter()
            .map(|item| (item.key.clone(), item.expanded))
            .collect();
        for item in &mut items {
            if let Some(expanded) = expanded.get(&item.key) {
                item.expanded = *expanded;
            }
        }
        let unchanged = self
            .claude
            .transcript
            .items
            .iter()
            .zip(&items)
            .take_while(|(old, new)| {
                old.key == new.key
                    && old.content == new.content
                    && old.status == new.status
                    && old.raw == new.raw
                    && old.expanded == new.expanded
            })
            .count();
        let count = items.len() + usize::from(self.claude.projection.active);
        let old_count = self.claude.list.item_count();
        let anchor = self.claude.list.logical_scroll_top();
        let preserve_anchor = !self.claude.list.is_following_tail()
            && anchor.item_ix >= unchanged
            && self
                .claude
                .transcript
                .items
                .get(anchor.item_ix)
                .zip(items.get(anchor.item_ix))
                .is_some_and(|(old, new)| old.key == new.key);
        self.claude.transcript.replace_presentational_items(items);
        if unchanged < old_count || unchanged < count {
            self.claude
                .list
                .splice(unchanged..old_count, count - unchanged);
            if preserve_anchor {
                self.claude.list.scroll_to(anchor);
            }
        }
        if self.workspace_mode == WorkspaceMode::Claude {
            self.dirty_request_surfaces
                .extend(self.request_surfaces.keys().cloned());
            self.live_request_keys
                .retain(|key| !key.starts_with("claude:"));
            for item in &self.claude.transcript.items {
                if item.pending_request.is_some() {
                    self.live_request_keys.insert(item.key.clone());
                    self.dirty_request_surfaces.insert(item.key.clone());
                }
            }
            self.selected_item = self
                .selected_item
                .min(self.claude.transcript.items.len().saturating_sub(1));
            drop(self.sync_transcript_document(cx));
        }
        cx.notify();
    }

    pub(super) fn send_claude(&mut self, cx: &mut Context<Self>) {
        if !self.claude.projection.ready || self.claude.sending || self.claude.settings_pending {
            return;
        }
        if !self.claude.projection.durable_submissions {
            self.claude.error = Some("This running Claude session needs a newer Harness connection before sending is safe. Your draft is saved; nothing was sent.".into());
            cx.notify();
            return;
        }
        let has_pending = self
            .claude
            .selected_draft_id
            .as_ref()
            .is_some_and(|key| self.composer_drafts.claude_pending_sends.contains_key(key));
        if !self.composer_images.is_empty() && !has_pending {
            self.claude.error = Some("Image attachments aren't supported in Claude conversations yet. Your draft and attachments are saved; nothing was sent.".into());
            cx.notify();
            return;
        }
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() && !has_pending {
            return;
        }
        let Some(session) = self.claude.selected.clone() else {
            return;
        };
        let Some(selected_id) = self.claude.selected_id.clone() else {
            return;
        };
        let Some(draft_key) = self.claude.selected_draft_id.clone() else {
            return;
        };
        let native_session_id = self.claude.projection.session_id.clone();
        let epoch = self.claude.projection.epoch.clone();
        let before = self.composer_drafts.clone();
        let prepared = (|| -> anyhow::Result<PendingSend> {
            let path = composer_drafts_path().context("No draft store is available")?;
            let latest = read_composer_drafts_at(&path)?;
            // A window may still be waiting for a receipt already consumed by
            // another window. Keep that exact ID until this window sees it too.
            let pending = self
                .composer_drafts
                .claude_pending_sends
                .get(&draft_key)
                .cloned();
            merge_send_journal(&mut self.composer_drafts, &latest)?;
            if let Some(pending) = pending {
                self.composer_drafts
                    .claude_pending_sends
                    .insert(draft_key.clone(), pending);
            }
            prepare_submission(
                &mut self.composer_drafts,
                &draft_key,
                &native_session_id,
                &text,
            )
        })()
        .and_then(|pending| {
            self.persist_composer_state_now()?;
            Ok(pending)
        });
        let pending = match prepared {
            Ok(pending) => pending,
            Err(error) => {
                self.composer_drafts = before;
                self.claude.error = Some(
                    format!("Could not prepare Claude send: {error:#}. Nothing was sent.").into(),
                );
                cx.notify();
                return;
            }
        };
        self.claude.sending = true;
        self.claude.error = None;
        let changed_draft = pending.text != text || !self.composer_images.is_empty();
        cx.spawn(async move |this, cx| {
            let response = claude_native::request(&session, json!({"method": if changed_draft { "submission_status" } else { "prompt" }, "text":pending.text, "submissionId":pending.id, "sessionId":native_session_id, "epoch":epoch})).await;
            if let Err(error) = this.update(cx, |this, cx| {
                let rejected = response.as_ref().is_ok_and(|result| result["uuid"] == pending.id && result["state"] == "rejected" && result["accepted"] == false);
                let mut response = response.and_then(|result| verified_submission_receipt(result, &pending.id));
                if response.is_ok() || rejected {
                    let before = this.composer_drafts.clone();
                    if this.composer_drafts.claude_pending_sends.get(&draft_key).is_some_and(|entry| entry.id == pending.id) {
                        this.composer_drafts.claude_pending_sends.remove(&draft_key);
                    }
                    this.composer_drafts.claude_resolved_sends.insert(pending.id.clone());
                    if response.is_ok() {
                        this.composer_drafts.claude_accepted_sends.insert(pending.id.clone());
                    }
                    if response.is_ok() && !changed_draft && this.composer_drafts.drafts.get(&draft_key) == Some(&pending.text) {
                        this.composer_drafts.drafts.remove(&draft_key);
                    }
                    if let Err(error) = this.persist_composer_state_now() {
                        this.composer_drafts = before;
                        response = Err(anyhow::anyhow!("Claude's receipt arrived, but its local draft update could not be saved: {error:#}. The original send ID is retained; do not send a new copy"));
                    }
                }
                if this.claude.selected_id.as_deref() != Some(&selected_id) { return; }
                this.claude.sending = false;
                match response {
                    Ok(result) => {
                        // Admission, not send-button press, is the point at which a draft is consumed.
                        if this.workspace_mode == WorkspaceMode::Claude
                            && !changed_draft
                            && this.composer_draft_thread_id.as_ref() == Some(&draft_key)
                            && this.composer.read(cx).text(cx) == pending.text {
                            this.composer.update(cx, |editor, cx| editor.restore_text(String::new(), cx));
                            this.update_composer_draft("", cx);
                        }
                        if changed_draft {
                            this.claude.error = Some("The previous prompt was accepted. Your edited draft has been kept and has not been sent.".into());
                        } else if let Some(warning) = result["warning"].as_str() {
                            this.claude.error = Some(warning.to_owned().into());
                        }
                    }
                    Err(error) => this.claude.error = Some(format!("Could not confirm Claude send: {error:#} · draft kept").into()),
                }
                cx.notify();
            }) { log::debug!("Claude workspace closed: {error}"); }
        }).detach();
        cx.notify();
    }

    pub(super) fn claude_action(
        &mut self,
        mut request: Value,
        item_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !self.claude.projection.ready {
            return;
        }
        request["sessionId"] = json!(self.claude.projection.session_id);
        request["epoch"] = json!(self.claude.projection.epoch);
        let Some(session) = self.claude.selected.clone() else {
            return;
        };
        let selected_id = self.claude.selected_id.clone();
        if let Some(entry) = item_key
            .as_ref()
            .and_then(|key| self.request_surfaces.get(key))
        {
            entry
                .entity
                .update(cx, |surface, cx| surface.set_responding(true, cx));
        }
        cx.spawn(async move |this, cx| {
            let result = claude_native::request(&session, request).await;
            if let Err(error) = this.update(cx, |this, cx| {
                if this.claude.selected_id != selected_id {
                    return;
                }
                if let Some(entry) = item_key
                    .as_ref()
                    .and_then(|key| this.request_surfaces.get(key))
                {
                    entry
                        .entity
                        .update(cx, |surface, cx| surface.set_responding(false, cx));
                }
                if let Err(error) = result {
                    this.claude.error = Some(format!("Claude action failed: {error:#}").into());
                }
                cx.notify();
            }) {
                log::debug!("Claude workspace closed: {error}");
            }
        })
        .detach();
    }

    pub(super) fn render_claude_startup(&self, cx: &Context<Self>) -> AnyElement {
        let Some(request) = &self.claude.selected_creation else {
            return div().into_any_element();
        };
        let opening = self
            .claude
            .selected_id
            .as_ref()
            .is_some_and(|id| self.claude.opening.contains(id));
        div()
            .absolute()
            .top(px(32.))
            .left_0()
            .right_0()
            .bottom(px(150.))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(
                div()
                    .w_full()
                    .max_w(px(560.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(Label::new(if opening {
                        "Opening conversation…"
                    } else {
                        "Couldn't start this conversation"
                    }))
                    .child(
                        div()
                            .text_color(cx.theme().colors().text_muted)
                            .child(if opening {
                                "Checking whether Claude is ready. Your draft hasn't been sent."
                            } else {
                                "Claude hasn't connected yet. Your draft hasn't been sent."
                            }),
                    )
                    .when(!opening, |this| {
                        this.child(
                            div()
                                .flex()
                                .gap_2()
                                .child(Button::new("claude-retry-startup", "Try again").on_click(
                                    cx.listener(|this, _, window, cx| {
                                        if let Some(conversation) = this
                                            .claude
                                            .sessions
                                            .iter()
                                            .find(|entry| {
                                                Some(&entry.id) == this.claude.selected_id.as_ref()
                                            })
                                            .cloned()
                                        {
                                            this.activate_claude(conversation, window, cx);
                                        }
                                    }),
                                ))
                                .child(
                                    Button::new(
                                        "claude-startup-details",
                                        if self.claude.show_startup_details {
                                            "Hide details"
                                        } else {
                                            "Technical details"
                                        },
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.claude.show_startup_details =
                                                !this.claude.show_startup_details;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        )
                    })
                    .when(self.claude.show_startup_details, |this| {
                        this.child(
                            div()
                                .id("claude-startup-diagnostic")
                                .max_h(px(200.))
                                .overflow_y_scroll()
                                .text_sm()
                                .text_color(cx.theme().colors().text_muted)
                                .child(request.detail.clone()),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_claude_sidebar(&self, cx: &Context<Self>) -> AnyElement {
        let saved_drafts: Vec<_> = self
            .claude
            .sessions
            .iter()
            .find(|conversation| Some(&conversation.id) == self.claude.selected_id.as_ref())
            .into_iter()
            .flat_map(|conversation| conversation.aliases.iter())
            .map(|alias| format!("claude:{alias}"))
            .filter_map(|key| {
                self.composer_drafts
                    .drafts
                    .get(&key)
                    .filter(|text| !text.is_empty())
                    .map(|text| (key, text.clone()))
            })
            .collect();
        let other_saved_drafts = saved_drafts
            .iter()
            .any(|(key, _)| Some(key) != self.claude.selected_draft_id.as_ref());
        div().flex_1().min_h_0().flex().flex_col()
            .child(div().p_2().text_sm().text_color(cx.theme().colors().text_muted)
                .child("Claude · preview"))
            .child(div().px_2().pb_2().child(Button::new("claude-new-session", if self.claude.starting { "Starting Claude…" } else { "New Claude session" })
                .disabled(self.claude.starting || self.new_task_picker_open)
                .on_click(cx.listener(|this, _, window, cx| this.new_claude(window, cx)))))
            .child(div().px_2().pb_2().child(Button::new("claude-setup", if self.claude.setup.configured { "Claude settings…" } else { "Set up Claude…" })
                .disabled(self.claude.configuring)
                .on_click(cx.listener(|this, _, window, cx| this.configure_claude(window, cx)))))
            .when(self.claude.hidden_count > 0, |this| this.child(div().px_2().pb_2().child(
                Button::new("claude-show-hidden", if self.claude.show_hidden { "Hide saved test conversations".to_owned() }
                    else { format!("Show {} hidden conversations", self.claude.hidden_count) })
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.claude.show_hidden = !this.claude.show_hidden;
                        this.refresh_claude(cx);
                    })))))
            .when(!self.claude.statuses.values().any(|status| status.can_reconnect() || status.phase == claude_native::HostPhase::NeedsAdapter), |this| this.child(
                div().px_2().pb_2().text_sm().text_color(cx.theme().colors().text_muted)
                    .child("Select a conversation to open it, or start a new one.")))
            .when(!self.claude.catalog_warnings.is_empty(), |this| this.child(
                div().p_2().text_sm().text_color(cx.theme().status().warning)
                    .children(self.claude.catalog_warnings.iter().cloned())))
            .when(self.claude.sessions.is_empty(), |this| this.child(div().p_3().child("Use + to choose a project and start Claude.")))
            .child(list(self.claude.sidebar.clone(), cx.processor(|this, index: usize, _, cx| {
                let Some(session) = this.claude.sessions.get(index).cloned() else { return div().into_any_element(); };
                let selected = this.claude.selected_id.as_deref() == Some(&session.id);
                ThreadItem::new(format!("claude-{}", session.id), session.title.clone())
                    .icon(IconName::AiClaude)
                    .project_name(project_display_name(&session.cwd, "Claude"))
                    .timestamp(if this.claude.opening.contains(&session.id) { "Opening…" }
                        else if selected && this.claude.projection.ready { if this.claude.projection.active { "Working" } else { "" } }
                        else if session.creation().is_some() { "Couldn't start" }
                        else { this.claude.statuses.get(&session.id).map(|status| status.sidebar_label()).unwrap_or("") })
                    .selected(selected)
                    .focused(this.focus_mode == FocusMode::Tasks && this.selected_task == index)
                    .base_bg(cx.theme().colors().panel_background)
                    .is_truncated(false)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected_task = index;
                        this.open_claude(session.clone(), window, cx);
                    }))
                    .into_any_element()
            })).flex_1().min_h_0())
            .when(other_saved_drafts, |this| this.child(div().p_2().flex().flex_col().gap_1()
                .child("Saved drafts for this conversation")
                .children(saved_drafts.into_iter().enumerate().map(|(index, (key, text))| {
                    Button::new(format!("claude-saved-draft-{index}"), format!("Draft {}", index + 1))
                        .style(if self.claude.selected_draft_id.as_ref() == Some(&key) { ButtonStyle::Tinted(TintColor::Accent) } else { ButtonStyle::Subtle })
                        .tooltip(Tooltip::text(text.chars().take(200).collect::<String>()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.claude.selected_draft_id = Some(key.clone());
                            this.switch_composer_draft_context(Some(key.clone()), cx);
                            cx.notify();
                        }))
                })) ))
            .when_some(self.claude.selected.clone(), |this, session| this.child(
                div().p_2()
                .when(!self.claude.projection.ready && self.claude.selected_creation.is_none() && self.claude.error.is_some(), |this| this.child(Button::new("claude-reconnect", "Retry opening")
                    .disabled(self.claude.selected_id.as_ref().is_some_and(|id| self.claude.opening.contains(id)))
                    .tooltip(Tooltip::text("Try opening this conversation again. Your draft will not be sent."))
                    .on_click({ let session = session.clone(); cx.listener(move |this, _, window, cx| {
                        let selected = this.claude.sessions.iter()
                            .find(|entry| Some(&entry.id) == this.claude.selected_id.as_ref())
                            .cloned().unwrap_or_else(|| claude_native::Conversation::from_session(session.clone()));
                        this.open_claude(selected, window, cx);
                    }) })))
            )).into_any_element()
    }
}
