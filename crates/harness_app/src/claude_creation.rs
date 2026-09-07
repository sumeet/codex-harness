use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    id: String,
    configuration: PathBuf,
    cwd: PathBuf,
    executable: PathBuf,
    requested_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum State {
    Prepared,
    NotStarted { detail: String },
    Dispatching,
    Connected { session_id: String },
    Uncertain { detail: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct Pending {
    pub directory: PathBuf,
    pub cwd: PathBuf,
    pub detail: String,
    pub requested_at: u64,
}

impl Pending {
    pub fn id(&self) -> String {
        format!(
            "claude-creation:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(self.directory.as_os_str().as_encoded_bytes())
        )
    }
    pub fn title(&self) -> String {
        self.cwd
            .file_name()
            .unwrap_or(self.cwd.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

fn request_directory(configuration: &Path, id: &str) -> PathBuf {
    configuration.join("harness-adapter/creations").join(id)
}

fn read_request(directory: &Path) -> anyhow::Result<Request> {
    validate_private_directory(directory)?;
    let request: Request = serde_json::from_slice(
        &read_optional(&directory.join("request.json"))?.context("Startup request is missing")?,
    )?;
    ensure!(
        request.version == 1,
        "Unknown Claude creation request version"
    );
    ensure!(
        Uuid::parse_str(&request.id).is_ok(),
        "Invalid creation request ID"
    );
    ensure!(
        request_directory(&request.configuration, &request.id) == directory
            && fs::canonicalize(discovery::configuration_directory()?)? == request.configuration,
        "Startup request belongs to another profile or directory"
    );
    ensure!(
        request.cwd.is_absolute() && request.executable.is_absolute(),
        "Invalid startup paths"
    );
    let settings: Value = serde_json::from_slice(
        &read_optional(&directory.join("settings.json"))?.context("Startup marker is missing")?,
    )?;
    ensure!(
        settings == json!({}),
        "Startup marker settings changed; creation refused"
    );
    Ok(request)
}

fn read_state(directory: &Path) -> anyhow::Result<State> {
    serde_json::from_slice(
        &read_optional(&directory.join("state.json"))?.context("Startup state is missing")?,
    )
    .map_err(Into::into)
}

fn save_state(directory: &Path, state: &State) -> anyhow::Result<()> {
    publish_file(
        &directory.join("state.json"),
        &serde_json::to_vec(state)?,
        true,
    )
}

fn operation_lock(directory: &Path) -> anyhow::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("operation.lock"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Unsafe startup operation lock"
    );
    Ok(try_host_lock(&file)?.then_some(file))
}

fn spawn_worker(directory: &Path) -> anyhow::Result<std::sync::mpsc::Receiver<Result<(), String>>> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("coordinator.log"))?;
    let metadata = log.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Unsafe startup log"
    );
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--claude-create-worker")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // The coordinator only finishes a durable operation; Claude's supervisor,
    // not this child or the frontend, owns the actual conversation process.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("Could not start Claude creation coordinator")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = child
            .wait()
            .map_err(|error| error.to_string())
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err(format!("Startup coordinator exited with {status}"))
                }
            });
        if let Err(error) = sender.send(result) {
            log::debug!("Claude creation waiter closed: {error}");
        }
    });
    Ok(receiver)
}

pub(super) fn start(cwd: PathBuf) -> anyhow::Result<Session> {
    ensure!(cwd.is_dir(), "Choose a project folder");
    setup::require_enabled()?;
    setup::prepare()?;
    let configuration = fs::canonicalize(discovery::configuration_directory()?)?;
    let request = Request {
        version: 1,
        id: Uuid::new_v4().to_string(),
        configuration,
        cwd: fs::canonicalize(cwd)?,
        executable: fs::canonicalize(native_binary()?)?,
        requested_at: now_ms(),
    };
    let directory = request_directory(&request.configuration, &request.id);
    private_directory(&directory)?;
    write_new(&directory.join("settings.json"), b"{}")?;
    write_new(
        &directory.join("request.json"),
        &serde_json::to_vec(&request)?,
    )?;
    save_state(&directory, &State::Prepared)?;
    File::open(directory.parent().context("Missing creation directory")?)?.sync_all()?;
    recover(&directory)
}

pub fn recover(directory: &Path) -> anyhow::Result<Session> {
    let request = read_request(directory)?;
    let state = read_state(directory)?;
    if let State::Connected { session_id } = &state {
        return discovered_session(&request, session_id);
    }
    if matches!(state, State::Prepared | State::NotStarted { .. })
        && find_created(&request, directory)?.is_none()
    {
        setup::require_enabled()?;
    }
    let receiver = spawn_worker(directory)?;
    let mut finished = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(50);
    loop {
        match receiver.try_recv() {
            Ok(result) => {
                finished = true;
                if let Err(error) = result {
                    log::warn!("Claude startup: {error}");
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => finished = true,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if let State::Connected { session_id } = read_state(directory)? {
            return discovered_session(&request, &session_id);
        }
        // Another recovery caller can exit while the first coordinator still
        // works. Read the final state under its lock before reporting failure.
        if finished && let Some(_lock) = operation_lock(directory)? {
            match read_state(directory)? {
                State::Connected { session_id } => {
                    return discovered_session(&request, &session_id);
                }
                State::Uncertain { detail } | State::NotStarted { detail } => {
                    bail!(
                        "{detail}. Startup request saved at {}; no prompt was sent",
                        directory.display()
                    );
                }
                _ => bail!(
                    "Claude startup stopped before its outcome was recorded. Check the saved request at {}; creation was not repeated",
                    directory.display()
                ),
            }
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "Claude startup is still pending. Its request is saved at {}; closing Harness will not cancel it",
            directory.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn discovered_session(request: &Request, conversation: &str) -> anyhow::Result<Session> {
    let mut catalog = Catalog::default();
    discovery::extend_catalog(&mut catalog)?;
    catalog
        .sessions
        .into_iter()
        .find(|session| {
            matches!(&session.source, SessionSource::Native { configuration, conversation_id, .. }
            if configuration == &request.configuration && conversation_id == conversation)
        })
        .context(
            "Created native conversation is not currently discoverable; no replacement was started",
        )
}

fn marker_matches(state: &Value, marker: &Path) -> bool {
    state["respawnFlags"].as_array().is_some_and(|flags| {
        flags.windows(2).any(|pair| {
            pair.first().and_then(Value::as_str) == Some("--settings")
                && pair.get(1).and_then(Value::as_str) == marker.to_str()
        })
    })
}

fn find_created(request: &Request, directory: &Path) -> anyhow::Result<Option<String>> {
    let jobs = request.configuration.join("jobs");
    if !jobs.try_exists()? {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(&jobs)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "Unsafe native jobs directory"
    );
    let mut matching = Vec::new();
    for entry in fs::read_dir(&jobs)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(8..=64).contains(&name.len()) || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        validate_private_directory(&entry.path())?;
        let Some(bytes) = read_optional(&entry.path().join("state.json"))? else {
            continue;
        };
        let state: Value = serde_json::from_slice(&bytes)?;
        if !marker_matches(&state, &directory.join("settings.json")) {
            continue;
        }
        ensure!(
            state["cwd"].as_str().map(Path::new) == Some(request.cwd.as_path()),
            "Created job workspace differs from the request; manual inspection required"
        );
        let conversation = state["sessionId"]
            .as_str()
            .context("Created job has no conversation ID")?;
        ensure!(
            Uuid::parse_str(conversation).is_ok(),
            "Created job has an invalid conversation ID"
        );
        matching.push(conversation.to_owned());
    }
    ensure!(
        matching.len() <= 1,
        "Multiple native jobs match this creation request; Harness will not guess or create another"
    );
    Ok(matching.pop())
}

async fn launch(request: &Request, directory: &Path) -> anyhow::Result<()> {
    use futures::io::AsyncReadExt as _;
    let mut command = async_process::Command::new(&request.executable);
    command
        .arg("--bg")
        .arg("--settings")
        .arg(directory.join("settings.json"))
        .current_dir(&request.cwd)
        .env("CLAUDE_CONFIG_DIR", &request.configuration)
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
    let mut child = command.spawn().context("Could not launch native Claude")?;
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
        "Native creation output exceeded its limit"
    );
    let status = child.status().await?;
    publish_file(
        &directory.join("native-output.json"),
        &serde_json::to_vec(&json!({
            "status":status.code(), "stdout":String::from_utf8_lossy(&stdout), "stderr":String::from_utf8_lossy(&stderr)
        }))?,
        true,
    )?;
    ensure!(
        status.success(),
        "Native creation failed ({status}): {}",
        String::from_utf8_lossy(&stderr)
    );
    Ok(())
}

pub fn worker(directory: &Path) -> anyhow::Result<()> {
    let request = read_request(directory)?;
    let Some(_lock) = operation_lock(directory)? else {
        return Ok(());
    };
    let result = coordinate(&request, directory);
    if let Err(error) = &result {
        let detail = format!("{error:#}");
        let failure = if matches!(
            read_state(directory)?,
            State::Prepared | State::NotStarted { .. }
        ) {
            State::NotStarted { detail }
        } else {
            State::Uncertain { detail }
        };
        save_state(directory, &failure)?;
    }
    result
}

fn coordinate(request: &Request, directory: &Path) -> anyhow::Result<()> {
    let state = read_state(directory)?;
    if let State::Connected { session_id } = &state {
        ensure!(
            find_created(request, directory)?.as_ref() == Some(session_id),
            "Created conversation identity changed"
        );
        return Ok(());
    }
    let mut created = find_created(request, directory)?;
    let mut acknowledgement_error = None;
    if created.is_none() && matches!(state, State::Prepared | State::NotStarted { .. }) {
        setup::require_enabled()?;
        ensure!(
            fs::canonicalize(native_binary()?)? == request.executable,
            "Claude executable changed after creation was prepared; no worker was started"
        );
        setup::prepare()?;
        // Native --bg does not accept an idempotency key. Once dispatch might
        // have happened, only its persisted marker can establish the outcome.
        save_state(directory, &State::Dispatching)?;
        if let Err(error) = smol::block_on(with_timeout(
            launch(request, directory),
            Duration::from_secs(20),
            "Native creation timed out",
        )) {
            log::warn!("Native creation acknowledgement missing: {error:#}");
            acknowledgement_error = Some(format!("{error:#}"));
        }
        created = find_created(request, directory)?;
    }
    let conversation = created.with_context(|| format!(
        "Startup outcome is uncertain and no unique native job was found. Creation will not be repeated automatically{}",
        acknowledgement_error.map(|error| format!(": {error}")).unwrap_or_default()
    ))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    let mut last_error = "Native worker has not connected".to_owned();
    while std::time::Instant::now() < deadline {
        let verified = (|| -> anyhow::Result<()> {
            let session = discovered_session(request, &conversation)?;
            ensure!(
                matches!(&session.source, SessionSource::Native { job:Some(job), .. } if job.pid.is_some()),
                "Created worker is stopped; it will not be blindly restarted from missing history"
            );
            smol::block_on(with_timeout(
                discovery::snapshot_for_verification(&session, false),
                Duration::from_secs(2),
                "Native startup has not provided a snapshot",
            ))?;
            Ok(())
        })();
        match verified {
            Ok(()) => {
                return save_state(
                    directory,
                    &State::Connected {
                        session_id: conversation,
                    },
                );
            }
            Err(error) => last_error = format!("{error:#}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!(
        "Created native conversation {conversation}, but it is not ready: {last_error}. Finish any native setup, then check this saved startup request"
    )
}

pub(super) fn extend_catalog(catalog: &mut Catalog) -> anyhow::Result<()> {
    let configuration = discovery::configuration_directory()?;
    if !configuration.try_exists()? {
        return Ok(());
    }
    let configuration = fs::canonicalize(configuration)?;
    let root = configuration.join("harness-adapter/creations");
    if !root.try_exists()? {
        return Ok(());
    }
    validate_private_directory(&root)?;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err() {
            continue;
        }
        let result = (|| -> anyhow::Result<Option<Pending>> {
            let directory = entry.path();
            let request = read_request(&directory)?;
            let detail = match read_state(&directory)? {
                State::Connected { session_id } => {
                    if let Some(conversation) = catalog.conversations.iter_mut().find(|conversation| {
                        matches!(conversation.current().map(|session| &session.source), Some(SessionSource::Native {configuration:profile,conversation_id,..})
                            if profile == &configuration && conversation_id == &session_id)
                        || conversation.aliases.contains(&discovery::native_id(&configuration,&session_id))
                    }) {
                        let alias = Pending { directory:directory.clone(),cwd:request.cwd.clone(),detail:String::new(),requested_at:request.requested_at }.id();
                        if !conversation.aliases.contains(&alias) { conversation.aliases.push(alias); }
                    }
                    return Ok(None);
                }
                State::Prepared => "Claude startup prepared".to_owned(),
                State::Dispatching => {
                    "Claude startup pending — check its outcome before starting another".to_owned()
                }
                State::Uncertain { detail } | State::NotStarted { detail } => detail,
            };
            let pending = Pending {
                directory: directory.clone(),
                cwd: request.cwd.clone(),
                detail,
                requested_at: request.requested_at,
            };
            if let Some(session_id) = find_created(&request, &directory)?
                && let Some(conversation) = catalog.conversations.iter_mut().find(|conversation| {
                    conversation
                        .aliases
                        .contains(&discovery::native_id(&configuration, &session_id))
                })
            {
                let current = conversation.current().cloned();
                conversation.aliases.push(pending.id());
                conversation.target = ConversationTarget::Creation {
                    request: pending.clone(),
                    current,
                };
            }
            Ok(Some(pending))
        })();
        match result {
            Ok(Some(pending)) => catalog.creations.push(pending),
            Ok(None) => {}
            Err(error) => catalog
                .warnings
                .push(format!("Invalid Claude startup request: {error:#}")),
        }
    }
    for pending in &catalog.creations {
        if !catalog
            .conversations
            .iter()
            .any(|conversation| conversation.aliases.contains(&pending.id()))
        {
            catalog
                .conversations
                .push(Conversation::from_creation(pending.clone()));
        }
    }
    catalog.conversations.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    fn request(fixture: &Fixture) -> Request {
        Request {
            version: 1,
            id: Uuid::new_v4().to_string(),
            configuration: fixture.0.clone(),
            cwd: fixture.0.clone(),
            executable: PathBuf::from("/must-not-be-launched"),
            requested_at: now_ms(),
        }
    }

    #[test]
    fn pending_creation_is_a_conversation_without_a_fabricated_native_endpoint() {
        let pending = Pending {
            directory: "/private/profile/harness-adapter/creations/request".into(),
            cwd: "/projects/example".into(),
            detail: "Private diagnostic".into(),
            requested_at: 42,
        };
        let conversation = Conversation::from_creation(pending.clone());
        assert_eq!(conversation.title, "example");
        assert_eq!(conversation.id, pending.id());
        assert_eq!(conversation.updated_at_ms, 42);
        assert!(conversation.entry().is_none());
        assert!(conversation.current().is_none());
        assert!(conversation.creation().is_some());
    }

    #[test]
    fn uncertain_dispatch_does_not_create_again() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let request = request(&fixture);
        let directory = request_directory(&request.configuration, &request.id);
        private_directory(&directory)?;
        for state in [
            State::Dispatching,
            State::Uncertain {
                detail: "lost acknowledgement".into(),
            },
        ] {
            save_state(&directory, &state)?;
            let error = coordinate(&request, &directory)
                .expect_err("An uncertain creation must not be relaunched");
            assert!(error.to_string().contains("will not be repeated"));
            assert!(!directory.join("native-output.json").exists());
        }
        Ok(())
    }

    #[test]
    fn duplicate_creation_markers_are_ambiguous_even_for_the_same_conversation()
    -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let request = request(&fixture);
        let directory = request_directory(&request.configuration, &request.id);
        let conversation = Uuid::new_v4().to_string();
        for (index, id) in ["aaaaaaaa", "bbbbbbbb"].iter().enumerate() {
            let job = fixture.0.join("jobs").join(id);
            private_directory(&job)?;
            write_new(
                &job.join("state.json"),
                &serde_json::to_vec(&json!({
                    "cwd":request.cwd,"sessionId":conversation,"respawnFlags":["--settings",directory.join("settings.json")]
                }))?,
            )?;
            if index == 0 {
                assert_eq!(
                    find_created(&request, &directory)?,
                    Some(conversation.clone())
                );
            }
        }
        assert!(find_created(&request, &directory).is_err());
        Ok(())
    }

    #[test]
    fn concurrent_creation_coordinators_have_one_dispatch_owner() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let owner = operation_lock(&fixture.0)?.context("First coordinator must own the lock")?;
        assert!(operation_lock(&fixture.0)?.is_none());
        drop(owner);
        // Concurrent tests can fork with this descriptor open; CLOEXEC releases
        // the child's inherited lock when it execs, not when our owner drops.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while operation_lock(&fixture.0)?.is_none() {
            ensure!(
                std::time::Instant::now() < deadline,
                "Startup operation lock remained held after its owner closed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    #[test]
    fn creation_marker_is_an_exact_settings_argument_not_a_title_or_substring() {
        let marker = Path::new("/private/creation/settings.json");
        assert!(marker_matches(
            &json!({"respawnFlags":["--settings",marker]}),
            marker
        ));
        assert!(!marker_matches(&json!({"name":marker}), marker));
        assert!(!marker_matches(
            &json!({"respawnFlags":["--name",marker]}),
            marker
        ));
        assert!(!marker_matches(
            &json!({"respawnFlags":["--settings","/private/creation/settings.json.other"]}),
            marker
        ));
        assert!(!marker_matches(
            &json!({"respawnFlags":["--settings"]}),
            marker
        ));
    }
}
