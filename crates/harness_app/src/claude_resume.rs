use super::*;
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    version: u32,
    conversation_id: String,
    transcript: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    source_digest: String,
    history_digest: String,
    message_count: usize,
    requested_at: u64,
    verified: Option<Verified>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Verified {
    pid: u32,
    process_stat: String,
    boot: String,
    epoch: String,
}

fn paths(session: &Session) -> anyhow::Result<(PathBuf, PathBuf)> {
    session.validate()?;
    let SessionSource::Native {
        configuration,
        conversation_id,
        ..
    } = &session.source
    else {
        bail!("Only saved native conversations can be continued here");
    };
    let root = configuration.join("harness-adapter").join("continuations");
    Ok((root.clone(), root.join(format!("{conversation_id}.json"))))
}

fn read_attempt(session: &Session) -> anyhow::Result<Option<Attempt>> {
    let (root, path) = paths(session)?;
    if !root.try_exists()? {
        return Ok(None);
    }
    validate_private_directory(root.parent().context("Missing adapter root")?)?;
    validate_private_directory(&root)?;
    read_optional(&path)?
        .map(|bytes| {
            let attempt: Attempt = serde_json::from_slice(&bytes)?;
            let SessionSource::Native {
                conversation_id,
                transcript,
                ..
            } = &session.source
            else {
                bail!("Missing native identity");
            };
            ensure!(
                (1..=2).contains(&attempt.version) && attempt.conversation_id == *conversation_id,
                "Unsupported continuation record"
            );
            if let Some(origin) = &attempt.origin {
                ensure!(
                    attempt.version == 2 && Uuid::parse_str(origin).is_ok(),
                    "Invalid handoff origin"
                );
                let SessionSource::Native { configuration, .. } = &session.source else {
                    bail!("Missing native profile");
                };
                ensure!(
                    attempt.transcript.parent().and_then(Path::parent)
                        == Some(configuration.join("projects").as_path())
                        && attempt
                            .transcript
                            .file_name()
                            .and_then(|name| name.to_str())
                            == Some(format!("{origin}.jsonl").as_str()),
                    "Handoff history escaped its native profile"
                );
            } else {
                ensure!(
                    transcript
                        .as_ref()
                        .is_some_and(|path| *path == attempt.transcript),
                    "Continuation transcript changed; refusing a different history"
                );
            }
            Ok(attempt)
        })
        .transpose()
}

fn save_attempt(session: &Session, attempt: &Attempt) -> anyhow::Result<()> {
    let (_, path) = paths(session)?;
    publish_file(&path, &serde_json::to_vec(attempt)?, true)
}

fn continuation_lock(session: &Session, timeout: Duration) -> anyhow::Result<File> {
    let (root, path) = paths(session)?;
    private_directory(root.parent().context("Missing adapter root")?)?;
    private_directory(&root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.with_extension("lock"))?;
    let metadata = lock.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Unsafe continuation lock"
    );
    let deadline = std::time::Instant::now() + timeout;
    while !try_host_lock(&lock)? {
        ensure!(
            std::time::Instant::now() < deadline,
            "Another Harness window is still opening this conversation; no second launch was attempted"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(lock)
}

fn process_fingerprint(pid: u32) -> anyhow::Result<(String, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat.rsplit_once(')').context("Invalid process identity")?.1;
    let ticks = fields
        .split_whitespace()
        .nth(19)
        .context("Missing process start time")?;
    ensure!(ticks.parse::<u64>().is_ok(), "Invalid process start time");
    Ok((
        ticks.to_owned(),
        fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned(),
    ))
}

pub(super) fn check_connection(
    session: &Session,
    pid: u32,
    epoch: Option<&str>,
) -> anyhow::Result<()> {
    let Some(attempt) = read_attempt(session)? else {
        return Ok(());
    };
    let verified = attempt.verified.context(
        "The worker's restored history has not been verified. Retry opening to check it; no automatic resend or second launch")?;
    let (ticks, boot) = process_fingerprint(pid)?;
    ensure!(
        verified.pid == pid
            && verified.process_stat == ticks
            && verified.boot == boot
            && epoch.is_none_or(|epoch| epoch == verified.epoch),
        "Native worker changed since its history was verified. Retry opening to verify it again"
    );
    Ok(())
}

fn semantic_messages(messages: &[Value]) -> anyhow::Result<Vec<Value>> {
    let mut identifiers = std::collections::HashSet::new();
    messages.iter().filter(|message| matches!(message["type"].as_str(), Some("user" | "assistant")))
        .map(|message| {
            let identifier = message["uuid"].as_str().context("Saved message has no UUID")?;
            ensure!(Uuid::parse_str(identifier).is_ok() && identifiers.insert(identifier),
                "Saved message UUID is invalid or duplicated");
            let mut content = message["message"]["content"].clone();
            ensure!(!content.is_null(), "Saved message has no content");
            if let Some(value) = content.as_str() {
                content = json!([{"type":"text", "text":value}]);
            }
            let mut semantic = json!({"uuid":identifier,"type":message["type"],"role":message["message"]["role"],"content":content});
            semantic.sort_all_objects();
            Ok(semantic)
        }).collect()
}

fn digest(messages: &[Value]) -> anyhow::Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(messages)?)
    ))
}

fn verify_history(attempt: &Attempt, snapshot: &Value) -> anyhow::Result<()> {
    ensure!(
        snapshot["sessionId"] == attempt.conversation_id,
        "Claude opened a different conversation; the requested history remains read-only"
    );
    let messages = semantic_messages(
        snapshot["messages"]
            .as_array()
            .context("Missing native history")?,
    )?;
    let prefix = messages
        .get(..attempt.message_count)
        .context("Claude did not restore all expected messages; the composer remains disabled")?;
    ensure!(
        digest(prefix)? == attempt.history_digest,
        "Claude restored different message identities, order, or contents; the composer remains disabled"
    );
    Ok(())
}

fn verified_expectation(mut attempt: Attempt, snapshot: &Value) -> anyhow::Result<Attempt> {
    let Err(original_error) = verify_history(&attempt, snapshot) else {
        return Ok(attempt);
    };
    let repair = (|| -> anyhow::Result<()> {
        let (legacy, corrected) = discovery::original_continuation_history(
            &attempt.transcript,
            attempt
                .origin
                .as_deref()
                .unwrap_or(&attempt.conversation_id),
            &attempt.source_digest,
            attempt.origin.is_some(),
        )?;
        let legacy = semantic_messages(&legacy)?;
        ensure!(
            legacy.len() == attempt.message_count && digest(&legacy)? == attempt.history_digest,
            "The saved continuation proof is not explained by the original parent-only history"
        );
        let corrected = semantic_messages(&corrected)?;
        ensure!(
            corrected.len() > legacy.len(),
            "No missing parallel results explain this mismatch"
        );
        attempt.message_count = corrected.len();
        attempt.history_digest = digest(&corrected)?;
        verify_history(&attempt, snapshot)
    })();
    repair.map_err(|error| {
        original_error.context(format!("History reconciliation refused: {error:#}"))
    })?;
    Ok(attempt)
}

pub fn check_restored_history(session: &Session) -> anyhow::Result<Value> {
    session.validate()?;
    let attempt = read_attempt(session)?.context("There is no pending continuation proof")?;
    let expected = attempt.message_count;
    let (_, frame) = smol::block_on(with_timeout(
        discovery::snapshot_for_verification(session, true),
        Duration::from_secs(5),
        "The existing worker did not provide a history snapshot",
    ))?;
    let verified = verified_expectation(attempt, &frame["result"])?;
    Ok(json!({
        "conversationId": verified.conversation_id,
        "pid": frame["result"]["pid"],
        "previousExpectedMessages": expected,
        "verifiedMessages": verified.message_count,
        "readOnly": true,
    }))
}

fn owners(
    configuration: &Path,
    conversation: &str,
    allowed: Option<&discovery::NativeJob>,
) -> anyhow::Result<()> {
    let directory = configuration.join("sessions");
    if !directory.try_exists()? {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
        "Unsafe native owner registry"
    );
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == 0 || !Path::new(&format!("/proc/{pid}")).try_exists()? {
            continue;
        }
        let Some(bytes) = read_optional(&path)? else {
            continue;
        };
        let record: Value = serde_json::from_slice(&bytes)
            .context("Unreadable live native owner record; continuation refused")?;
        ensure!(
            record["pid"].as_u64() == Some(pid as u64),
            "Native owner identity mismatch"
        );
        if record["sessionId"] != conversation {
            continue;
        }
        if allowed.is_some_and(|job| {
            job.pid == Some(pid) || record["parkedJobId"].as_str() == Some(job.id.as_str())
        }) {
            continue;
        }
        bail!(
            "This conversation is still open in native Claude (PID {pid}). Run /bg in that terminal, then select it again here. Harness will follow the native handoff; it will not restart the terminal or create another owner"
        );
    }
    Ok(())
}

fn refreshed(session: &Session) -> anyhow::Result<Session> {
    let SessionSource::Native {
        configuration,
        conversation_id,
        ..
    } = &session.source
    else {
        bail!("Not a native conversation");
    };
    let mut next = session.clone();
    let mut matching = Vec::new();
    for value in discovery::native_jobs(configuration)? {
        let job: discovery::NativeJob =
            serde_json::from_value(value).context("Invalid native job; continuation refused")?;
        if job.session_id == *conversation_id {
            matching.push(job);
        }
    }
    if matching.iter().any(|job| job.kind == "background") {
        // A terminal attached to a supervised job is also listed. Its owner
        // record is checked separately; it is not a second background worker.
        matching.retain(|job| job.kind == "background");
    }
    ensure!(
        matching.len() <= 1,
        "Multiple native jobs claim this conversation; continuation refused"
    );
    if let SessionSource::Native { job, .. } = &mut next.source {
        *job = matching.pop();
    }
    next.validate()?;
    Ok(next)
}

async fn launch(configuration: &Path, conversation: &str, cwd: &Path) -> anyhow::Result<()> {
    use futures::io::AsyncReadExt as _;
    let mut command = async_process::Command::new(native_binary()?);
    // Adding unrelated CLI options can turn a native same-ID wake into a fork.
    command
        .args(["--bg", "--resume", conversation])
        .current_dir(cwd)
        .env("CLAUDE_CONFIG_DIR", configuration)
        .env("DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in [
        "LD_PRELOAD",
        "BUN_OPTIONS",
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_PROCESS_WRAPPER",
    ] {
        command.env_remove(name);
    }
    let mut child = command
        .spawn()
        .context("Could not start native continuation")?;
    let mut output = child
        .stdout
        .take()
        .context("Missing native output")?
        .take(65537);
    let mut errors = child
        .stderr
        .take()
        .context("Missing native errors")?
        .take(65537);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    futures::try_join!(
        output.read_to_end(&mut stdout),
        errors.read_to_end(&mut stderr)
    )?;
    ensure!(
        stdout.len() <= 65536 && stderr.len() <= 65536,
        "Native continuation output exceeded its limit"
    );
    let status = child.status().await?;
    ensure!(
        status.success(),
        "Native continuation failed ({status}): {}",
        String::from_utf8_lossy(&stderr)
    );
    ensure!(
        !String::from_utf8_lossy(&stderr).contains("started a copy"),
        "Native Claude started a copy instead of continuing. Harness did not connect or send a prompt; inspect the native session list before proceeding"
    );
    Ok(())
}

pub fn continue_conversation(selected: &Session) -> anyhow::Result<Session> {
    let SessionSource::Native { configuration, .. } = &selected.source else {
        bail!("Choose a native conversation");
    };
    ensure!(
        fs::canonicalize(configuration)?
            == fs::canonicalize(discovery::configuration_directory()?)?,
        "The selected conversation belongs to another Claude profile"
    );
    let chain = continuation_chain(selected)?;
    let destination = chain.last().context("Missing native conversation")?;
    if chain.len() > 1 {
        let destination = refreshed(destination)?;
        let SessionSource::Native {
            conversation_id,
            job,
            ..
        } = &destination.source
        else {
            bail!("Missing handoff destination");
        };
        if job.as_ref().is_some_and(|job| job.pid.is_some()) {
            let _lock = continuation_lock(&destination, Duration::from_secs(45))?;
            let mut expectations = read_attempt(&destination)?.into_iter().collect::<Vec<_>>();
            if matches!(
                &destination.source,
                SessionSource::Native {
                    transcript: Some(_),
                    ..
                }
            ) {
                expectations.push(history_expectation(&destination)?);
            }
            let (_, frame) = smol::block_on(with_timeout(
                discovery::snapshot_for_verification(&destination, true),
                Duration::from_secs(5),
                "Native handoff is not ready",
            ))?;
            for pair in chain.windows(2) {
                let [source, next] = pair else {
                    bail!("Invalid handoff chain")
                };
                let mut expectation = handoff_expectation(source, next)?;
                let SessionSource::Native {
                    conversation_id: origin,
                    ..
                } = &source.source
                else {
                    bail!("Missing handoff origin");
                };
                expectation.origin = Some(origin.clone());
                expectation.version = 2;
                expectation.conversation_id = conversation_id.clone();
                expectations.push(expectation);
            }
            let mut expectation = strongest_expectation(expectations, &frame["result"])?;
            record_verified(&destination, &mut expectation, &frame["result"])?;
            return Ok(destination);
        }
        // A handoff can exist in memory before the replacement transcript is
        // persisted. Never wake it from the original prompt if that file is absent.
        let (messages, _) = discovery::complete_history(&destination)?;
        for pair in chain.windows(2) {
            let [source, next] = pair else {
                bail!("Invalid handoff chain")
            };
            let mut expectation = handoff_expectation(source, next)?;
            expectation.conversation_id = conversation_id.clone();
            verify_history(
                &expectation,
                &json!({"sessionId":conversation_id,"messages":messages}),
            )?;
        }
    }
    continue_one(destination)
}

fn strongest_expectation(expectations: Vec<Attempt>, snapshot: &Value) -> anyhow::Result<Attempt> {
    let expectations = expectations
        .into_iter()
        .map(|expectation| verified_expectation(expectation, snapshot))
        .collect::<anyhow::Result<Vec<_>>>()?;
    // An older alias must not weaken a destination's pending or verified history
    // requirement, including messages added between multiple terminal handoffs.
    expectations
        .into_iter()
        .max_by_key(|expectation| expectation.message_count)
        .context("Missing handoff history requirement")
}

fn continuation_chain(selected: &Session) -> anyhow::Result<Vec<Session>> {
    let mut chain = vec![selected.clone()];
    let mut visited = std::collections::HashSet::new();
    visited.insert(selected.id.clone());
    let mut catalog = None;
    while let Some(target) = discovery::continued_in(chain.last().context("Missing conversation")?)?
    {
        ensure!(chain.len() < 32, "Native handoff chain exceeds its limit");
        let SessionSource::Native { configuration, .. } = &selected.source else {
            bail!("Missing native profile");
        };
        if catalog.is_none() {
            catalog = Some(sessions()?);
        }
        let destination = catalog.as_ref().context("Missing native catalog")?.sessions.iter().find(|session| {
            matches!(&session.source, SessionSource::Native { configuration: profile, conversation_id, .. }
                if profile == configuration && conversation_id == &target)
        }).context("This conversation moved to another native session, but its destination is unavailable; the old history will not be restarted")?;
        ensure!(
            visited.insert(destination.id.clone()),
            "Cycle in native conversation handoffs"
        );
        chain.push(destination.clone());
    }
    Ok(chain)
}

fn history_expectation(session: &Session) -> anyhow::Result<Attempt> {
    let (messages, source_digest) = discovery::complete_history(session)?;
    history_expectation_for(session, messages, source_digest)
}

fn handoff_expectation(source: &Session, next: &Session) -> anyhow::Result<Attempt> {
    let SessionSource::Native {
        conversation_id, ..
    } = &next.source
    else {
        bail!("Missing handoff destination");
    };
    let (messages, source_digest) = discovery::handoff_history(source, conversation_id)?;
    history_expectation_for(source, messages, source_digest)
}

fn history_expectation_for(
    session: &Session,
    messages: Vec<Value>,
    source_digest: String,
) -> anyhow::Result<Attempt> {
    let SessionSource::Native {
        conversation_id,
        transcript: Some(transcript),
        ..
    } = &session.source
    else {
        bail!("Choose a conversation with saved history");
    };
    let messages = semantic_messages(&messages)?;
    ensure!(
        !messages.is_empty(),
        "There are no saved conversational messages to continue"
    );
    Ok(Attempt {
        version: 1,
        conversation_id: conversation_id.clone(),
        transcript: transcript.clone(),
        origin: None,
        source_digest,
        history_digest: digest(&messages)?,
        message_count: messages.len(),
        requested_at: now_ms(),
        verified: None,
    })
}

fn record_verified(
    session: &Session,
    attempt: &mut Attempt,
    snapshot: &Value,
) -> anyhow::Result<()> {
    let pid = snapshot["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .context("Invalid native PID")?;
    let (ticks, boot) = process_fingerprint(pid)?;
    let epoch = snapshot["epoch"]
        .as_str()
        .context("Missing native epoch")?
        .to_owned();
    let SessionSource::Native {
        configuration,
        conversation_id,
        job,
        ..
    } = &session.source
    else {
        bail!("Missing native identity");
    };
    owners(configuration, conversation_id, job.as_ref())?;
    attempt.verified = Some(Verified {
        pid,
        process_stat: ticks,
        boot,
        epoch,
    });
    save_attempt(session, attempt)
}

fn continue_one(selected: &Session) -> anyhow::Result<Session> {
    let SessionSource::Native {
        configuration,
        conversation_id,
        ..
    } = &selected.source
    else {
        bail!("Choose a conversation with saved history");
    };
    ensure!(
        fs::canonicalize(configuration)?
            == fs::canonicalize(discovery::configuration_directory()?)?,
        "The selected conversation belongs to another Claude profile"
    );
    let _lock = continuation_lock(selected, Duration::from_secs(45))?;
    let mut session = refreshed(selected)?;
    let prior = read_attempt(&session)?;
    let job = match &session.source {
        SessionSource::Native { job, .. } => job.as_ref(),
        _ => None,
    };
    let live = job.filter(|job| job.pid.is_some());
    if live.is_some_and(|job| job.kind != "background") {
        owners(configuration, conversation_id, None)?;
        bail!(
            "This conversation is owned by a native terminal. Run /bg there, then select it again in Harness; no restart was attempted"
        );
    }
    if live.is_some() && prior.is_none() {
        session.status().can_reconnect().then_some(()).context(
            "This session is already running without a verified Harness connection. No restart was attempted")?;
        return Ok(session);
    }
    owners(configuration, conversation_id, live)?;
    let mut attempt = if let Some(prior) =
        prior.filter(|prior| prior.verified.is_none() || live.is_some())
    {
        ensure!(
            live.is_some(),
            "An earlier continuation has an uncertain outcome and no live worker. Harness will not launch it again automatically; inspect native Claude before retrying"
        );
        prior
    } else {
        setup::require_enabled()?;
        setup::prepare()?;
        history_expectation(&session)?
    };
    if live.is_none() {
        owners(configuration, conversation_id, None)?;
        let (_, current_digest) = discovery::complete_history(&session)?;
        ensure!(
            current_digest == attempt.source_digest,
            "Saved history changed during continuation setup; no worker was started"
        );
        save_attempt(&session, &attempt)?;
        smol::block_on(with_timeout(
            launch(configuration, conversation_id, &session.cwd),
            Duration::from_secs(15),
            "Native launch timed out; outcome is uncertain. No prompt was sent and no automatic retry will occur",
        ))?;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut last_error = "Waiting for the native worker".to_owned();
    while std::time::Instant::now() < deadline {
        session = refreshed(&session)?;
        let snapshot = smol::block_on(with_timeout(
            async {
                let (_, frame) = discovery::snapshot_for_verification(&session, true).await?;
                Ok(frame["result"].clone())
            },
            Duration::from_secs(3),
            "Native continuation has not provided a snapshot",
        ));
        match snapshot {
            Ok(snapshot) => {
                attempt = verified_expectation(attempt, &snapshot)?;
                record_verified(&session, &mut attempt, &snapshot)?;
                return Ok(session);
            }
            Err(error) => last_error = format!("{error:#}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!(
        "Opening is not ready: {last_error}. History/draft kept; no prompt was sent. If native setup needs input, finish it in Claude, then retry opening to verify the existing worker"
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    const CONVERSATION: &str = "a6a4ff36-3b24-4aee-ab83-f57085fc96fa";
    const MESSAGE: &str = "5a051b93-85d4-4c1d-9c59-299c01d096b1";

    fn message() -> Value {
        json!({"type":"user", "uuid":MESSAGE, "parentUuid":null, "sessionId":CONVERSATION,
            "message":{"role":"user", "content":"retain this content"}})
    }

    fn session(fixture: &Fixture) -> anyhow::Result<Session> {
        let project = fixture.0.join("projects").join("fixture");
        private_directory(&project)?;
        let transcript = project.join(format!("{CONVERSATION}.jsonl"));
        write_new(&transcript, format!("{}\n", message()).as_bytes())?;
        Ok(Session {
            id: format!(
                "native:{}:{CONVERSATION}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(fixture.0.as_os_str().as_encoded_bytes())
            ),
            directory: fixture.0.clone(),
            cwd: fixture.0.clone(),
            title: "fixture".into(),
            lifecycle_version: 0,
            created_at_ms: 0,
            source: SessionSource::Native {
                configuration: fixture.0.clone(),
                conversation_id: CONVERSATION.into(),
                transcript: Some(transcript),
                job: None,
            },
        })
    }

    fn attempt(session: &Session) -> anyhow::Result<Attempt> {
        let SessionSource::Native {
            transcript: Some(transcript),
            ..
        } = &session.source
        else {
            bail!("Missing fixture transcript");
        };
        Ok(Attempt {
            version: 1,
            conversation_id: CONVERSATION.into(),
            transcript: transcript.clone(),
            origin: None,
            source_digest: "fixture".into(),
            history_digest: digest(&semantic_messages(&[message()])?)?,
            message_count: 1,
            requested_at: 0,
            verified: None,
        })
    }

    fn parallel_attempt(session: &Session) -> anyhow::Result<(Attempt, Vec<Value>)> {
        let mut attempt = attempt(session)?;
        let first = Uuid::new_v4().to_string();
        let second = Uuid::new_v4().to_string();
        let result = Uuid::new_v4().to_string();
        let mut messages = vec![
            message(),
            json!({"type":"assistant", "uuid":first, "parentUuid":MESSAGE, "sessionId":CONVERSATION,
                "message":{"id":"response", "role":"assistant", "content":[{"type":"tool_use", "id":"tool1", "name":"Bash", "input":{"command":"true"}}]}}),
            json!({"type":"assistant", "uuid":second, "parentUuid":first, "sessionId":CONVERSATION,
                "message":{"id":"response", "role":"assistant", "content":[{"type":"tool_use", "id":"tool2", "name":"Bash", "input":{"command":"true"}}]}}),
            json!({"type":"user", "uuid":Uuid::new_v4(), "parentUuid":first, "sourceToolAssistantUUID":first, "sessionId":CONVERSATION,
                "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"tool1", "content":"preserve first output"}]}}),
            json!({"type":"user", "uuid":result, "parentUuid":second, "sourceToolAssistantUUID":second, "sessionId":CONVERSATION,
                "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"tool2", "content":"preserve second output"}]}}),
        ];
        let mut next = message();
        next["uuid"] = json!(Uuid::new_v4());
        next["parentUuid"] = json!(result);
        messages.push(next);
        let bytes = messages
            .iter()
            .map(|message| format!("{message}\n"))
            .collect::<String>();
        fs::write(&attempt.transcript, &bytes)?;
        attempt.source_digest = format!("{:x}", Sha256::digest(bytes));
        let legacy = messages
            .iter()
            .enumerate()
            .filter(|(position, _)| *position != 3)
            .map(|(_, message)| message.clone())
            .collect::<Vec<_>>();
        attempt.message_count = legacy.len();
        attempt.history_digest = digest(&semantic_messages(&legacy)?)?;
        Ok((attempt, messages))
    }

    #[test]
    fn pending_parent_only_proof_is_repaired_from_unchanged_original_source() -> anyhow::Result<()>
    {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        let (attempt, messages) = parallel_attempt(&session)?;
        let _lock = continuation_lock(&session, Duration::ZERO)?;
        save_attempt(&session, &attempt)?;
        let before = fs::read(paths(&session)?.1)?;
        OpenOptions::new()
            .append(true)
            .open(&attempt.transcript)?
            .write_all(b"{\"type\":\"cost-state\"}\n{\"unfinished\":")?;
        let snapshot = json!({"sessionId":CONVERSATION, "messages":messages});
        assert!(verify_history(&attempt, &snapshot).is_err());
        let repaired = verified_expectation(attempt.clone(), &snapshot)?;
        assert_eq!(repaired.message_count, 6);
        assert_eq!(repaired.source_digest, attempt.source_digest);
        verify_history(&repaired, &snapshot)?;
        assert_eq!(
            fs::read(paths(&session)?.1)?,
            before,
            "Checking must not publish a verified proof"
        );
        assert_eq!(
            strongest_expectation(vec![attempt, repaired], &snapshot)?.message_count,
            6
        );
        Ok(())
    }

    #[test]
    fn parallel_proof_repair_preserves_handoff_boundaries() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        let (mut attempt, messages) = parallel_attempt(&session)?;
        let destination = Uuid::new_v4().to_string();
        let marker = json!({"type":"continued-in", "sessionId":CONVERSATION, "continuedInSessionId":destination});
        OpenOptions::new()
            .append(true)
            .open(&attempt.transcript)?
            .write_all(format!("{marker}\n").as_bytes())?;
        attempt.origin = Some(CONVERSATION.into());
        attempt.conversation_id = destination.clone();
        attempt.version = 2;
        attempt.source_digest = format!("{:x}", Sha256::digest(fs::read(&attempt.transcript)?));
        let snapshot = json!({"sessionId":destination,"messages":messages});
        assert_eq!(
            verified_expectation(attempt.clone(), &snapshot)?.message_count,
            6
        );
        let mut later = message();
        later["uuid"] = json!(Uuid::new_v4());
        later["parentUuid"] = messages.last().context("Missing last message")?["uuid"].clone();
        later["message"]["content"] = json!("real conversational work after handoff");
        OpenOptions::new()
            .append(true)
            .open(&attempt.transcript)?
            .write_all(format!("{later}\n").as_bytes())?;
        attempt.source_digest = format!("{:x}", Sha256::digest(fs::read(&attempt.transcript)?));
        assert!(verified_expectation(attempt, &snapshot).is_err());
        Ok(())
    }

    #[test]
    fn parallel_proof_repair_rejects_mutation_reordering_missing_or_unrelated_messages()
    -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        let (attempt, messages) = parallel_attempt(&session)?;
        let mut changed = messages.clone();
        changed.get_mut(3).context("Missing result")?["message"]["content"][0]["content"] =
            json!("changed");
        let mut reordered = messages.clone();
        reordered.swap(3, 4);
        let mut missing = messages.clone();
        missing.remove(4);
        let mut unrelated = messages.clone();
        let mut extra = message();
        extra["uuid"] = json!(Uuid::new_v4());
        unrelated.insert(3, extra);
        for snapshot in [
            json!({"sessionId":CONVERSATION,"messages":changed}),
            json!({"sessionId":CONVERSATION,"messages":reordered}),
            json!({"sessionId":CONVERSATION,"messages":missing}),
            json!({"sessionId":CONVERSATION,"messages":unrelated}),
            json!({"sessionId":Uuid::new_v4(),"messages":messages}),
        ] {
            assert!(verified_expectation(attempt.clone(), &snapshot).is_err());
        }
        let mut wrong_proof = attempt.clone();
        wrong_proof.history_digest = "wrong".into();
        let snapshot = json!({"sessionId":CONVERSATION,"messages":messages});
        assert!(verified_expectation(wrong_proof, &snapshot).is_err());
        fs::write(&attempt.transcript, b"{\"type\":\"changed-source\"}\n")?;
        assert!(verified_expectation(attempt, &snapshot).is_err());
        Ok(())
    }

    #[test]
    fn continuation_requires_message_identity_order_and_content_not_only_conversation_id()
    -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        let attempt = attempt(&session)?;
        let snapshot = json!({"sessionId":CONVERSATION,"messages":[message()]});
        verify_history(&attempt, &snapshot)?;
        let mut different = snapshot.clone();
        different["messages"] = json!([]);
        assert!(verify_history(&attempt, &different).is_err());
        different = snapshot.clone();
        different["messages"][0]["uuid"] = json!(Uuid::new_v4());
        assert!(verify_history(&attempt, &different).is_err());
        different = snapshot.clone();
        different["messages"][0]["message"]["content"] = json!("changed");
        assert!(verify_history(&attempt, &different).is_err());
        different = snapshot;
        different["sessionId"] = json!(Uuid::new_v4());
        assert!(verify_history(&attempt, &different).is_err());
        let mut second = message();
        second["uuid"] = json!(Uuid::new_v4());
        let ordered = vec![message(), second.clone()];
        let mut pair = attempt;
        pair.message_count = 2;
        pair.history_digest = digest(&semantic_messages(&ordered)?)?;
        verify_history(&pair, &json!({"sessionId":CONVERSATION,"messages":ordered}))?;
        assert!(
            verify_history(
                &pair,
                &json!({"sessionId":CONVERSATION,"messages":[second, message()]})
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn continuation_normalizes_text_representation_but_preserves_tool_inputs() -> anyhow::Result<()>
    {
        let plain = message();
        let mut blocks = plain.clone();
        blocks["message"]["content"] = json!([{"type":"text","text":"retain this content"}]);
        assert_eq!(semantic_messages(&[plain])?, semantic_messages(&[blocks])?);
        let mut tool = message();
        tool["message"]["content"] =
            json!([{"type":"tool_use", "name":"Read", "input":{"file_path":"one"}}]);
        let before = digest(&semantic_messages(&[tool.clone()])?)?;
        tool["message"]["content"][0]["input"]["file_path"] = json!("two");
        assert_ne!(before, digest(&semantic_messages(&[tool])?)?);
        assert!(semantic_messages(&[message(), message()]).is_err());
        Ok(())
    }

    #[test]
    fn pending_and_replaced_workers_cannot_pass_the_connection_gate() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        let _lock = continuation_lock(&session, Duration::ZERO)?;
        let mut attempt = attempt(&session)?;
        save_attempt(&session, &attempt)?;
        let pid = std::process::id();
        assert!(check_connection(&session, pid, None).is_err());
        let (ticks, boot) = process_fingerprint(pid)?;
        let epoch = Uuid::new_v4().to_string();
        attempt.verified = Some(Verified {
            pid,
            process_stat: ticks,
            boot,
            epoch: epoch.clone(),
        });
        save_attempt(&session, &attempt)?;
        check_connection(&session, pid, Some(&epoch))?;
        assert!(check_connection(&session, pid, Some("different-epoch")).is_err());
        attempt
            .verified
            .as_mut()
            .context("Missing verification")?
            .process_stat = "0".into();
        save_attempt(&session, &attempt)?;
        assert!(check_connection(&session, pid, Some(&epoch)).is_err());
        Ok(())
    }

    #[test]
    fn handoff_preserves_the_strongest_history_requirement() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        let origin = attempt(&session)?;
        let mut second = message();
        second["uuid"] = json!(Uuid::new_v4());
        second["message"]["content"] = json!("added after the first handoff");
        let messages = vec![message(), second];
        let mut destination = origin.clone();
        destination.message_count = messages.len();
        destination.history_digest = digest(&semantic_messages(&messages)?)?;
        let snapshot = json!({"sessionId":CONVERSATION,"messages":messages});
        let retained = strongest_expectation(vec![destination.clone(), origin.clone()], &snapshot)?;
        assert_eq!(retained.message_count, 2);
        assert!(
            strongest_expectation(
                vec![destination.clone(), origin.clone()],
                &json!({"sessionId":CONVERSATION,"messages":[message()]})
            )
            .is_err()
        );
        let mut conflict = destination;
        conflict.history_digest = "conflicting prior proof".into();
        assert!(strongest_expectation(vec![origin, conflict], &snapshot).is_err());
        Ok(())
    }

    #[test]
    fn continuation_lock_excludes_another_frontend() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        let lock = continuation_lock(&session, Duration::ZERO)?;
        assert!(continuation_lock(&session, Duration::ZERO).is_err());
        drop(lock);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match continuation_lock(&session, Duration::ZERO) {
                Ok(_lock) => break,
                Err(error) if std::time::Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        Ok(())
    }

    #[test]
    fn continuation_refuses_a_live_interactive_owner() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let registry = fixture.0.join("sessions");
        private_directory(&registry)?;
        let pid = std::process::id();
        write_new(
            &registry.join(format!("{pid}.json")),
            &serde_json::to_vec(&json!({"pid":pid,"sessionId":CONVERSATION,"kind":"interactive"}))?,
        )?;
        assert!(owners(&fixture.0, CONVERSATION, None).is_err());
        owners(&fixture.0, "another-conversation", None)?;
        let job = discovery::NativeJob {
            id: "a6a4ff36".into(),
            session_id: CONVERSATION.into(),
            cwd: fixture.0.clone(),
            kind: "background".into(),
            pid: Some(pid),
            name: None,
            started_at: 0,
        };
        owners(&fixture.0, CONVERSATION, Some(&job))?;
        Ok(())
    }

    #[test]
    fn continuation_rejects_an_unfinished_history_write() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = session(&fixture)?;
        discovery::complete_history(&session)?;
        let SessionSource::Native {
            transcript: Some(path),
            ..
        } = &session.source
        else {
            bail!("Missing fixture");
        };
        OpenOptions::new()
            .append(true)
            .open(path)?
            .write_all(b"{\"unfinished\":")?;
        assert!(discovery::complete_history(&session).is_err());
        assert!(!paths(&session)?.1.try_exists()?);
        Ok(())
    }
}
