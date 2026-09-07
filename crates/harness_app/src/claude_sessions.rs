use super::*;
use std::io::{BufReader, Seek, SeekFrom};

const METADATA_LIMIT: u64 = 2 * 1024 * 1024;
const HISTORY_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeJob {
    #[serde(default)]
    pub id: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub kind: String,
    pub pid: Option<u32>,
    pub name: Option<String>,
    #[serde(default)]
    pub started_at: u64,
}

impl NativeJob {
    fn title(&self) -> Option<String> {
        self.name
            .as_deref()
            .and_then(short_title)
            .filter(|title| title != &self.id && title != &self.session_id)
    }
}

pub(super) fn configuration_directory() -> anyhow::Result<PathBuf> {
    let directory = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .context("No Claude configuration directory")?;
    ensure!(
        directory.is_absolute(),
        "CLAUDE_CONFIG_DIR must be absolute"
    );
    Ok(directory)
}

pub(super) fn native_id(configuration: &Path, conversation: &str) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    format!(
        "native:{}:{conversation}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(configuration.as_os_str().as_bytes())
    )
}

pub(super) fn hidden_conversations() -> anyhow::Result<Vec<String>> {
    let configuration = configuration_directory()?;
    let directory = configuration.join("harness-adapter");
    if !directory.try_exists()? {
        return Ok(Vec::new());
    }
    validate_private_directory(&directory)?;
    let Some(bytes) = read_optional(&directory.join("hidden-conversations.json"))? else {
        return Ok(Vec::new());
    };
    hidden_conversation_ids(&configuration, &bytes)
}

fn hidden_conversation_ids(configuration: &Path, bytes: &[u8]) -> anyhow::Result<Vec<String>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Visibility {
        version: u32,
        conversations: Vec<String>,
    }
    let visibility: Visibility = serde_json::from_slice(bytes)?;
    ensure!(
        visibility.version == 1,
        "Unsupported Claude visibility settings"
    );
    visibility
        .conversations
        .into_iter()
        .map(|identifier| {
            ensure!(
                Uuid::parse_str(&identifier).is_ok(),
                "Invalid hidden conversation identity"
            );
            Ok(native_id(configuration, &identifier))
        })
        .collect()
}

pub(super) fn validate_session(session: &Session) -> anyhow::Result<()> {
    let SessionSource::Native {
        configuration,
        conversation_id,
        transcript,
        job,
    } = &session.source
    else {
        bail!("Expected a native session");
    };
    ensure!(
        configuration.is_absolute(),
        "Invalid Claude configuration path"
    );
    ensure!(
        Uuid::parse_str(conversation_id).is_ok(),
        "Invalid native conversation ID"
    );
    ensure!(
        session.id == native_id(configuration, conversation_id),
        "Native session identity mismatch"
    );
    if let Some(path) = transcript {
        ensure!(
            path.parent().and_then(Path::parent) == Some(configuration.join("projects").as_path())
                && path.file_name().and_then(|name| name.to_str())
                    == Some(format!("{conversation_id}.jsonl").as_str()),
            "Transcript escaped its native project directory"
        );
    }
    if let Some(job) = job {
        ensure!(
            job.session_id == *conversation_id && job.cwd == session.cwd,
            "Native job identity mismatch"
        );
        validate_job(job)?;
    }
    Ok(())
}

fn validate_job(job: &NativeJob) -> anyhow::Result<()> {
    match job.kind.as_str() {
        "background" => ensure!(
            (8..=64).contains(&job.id.len()) && job.id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid native background job ID"
        ),
        "interactive" => ensure!(
            job.pid.is_some_and(|pid| pid > 0),
            "Native terminal has no valid process ID"
        ),
        _ => bail!("Unknown native session kind"),
    }
    Ok(())
}

fn owned_directory(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
        "Claude directory is not a real, user-owned directory: {}",
        path.display()
    );
    Ok(())
}

fn transcript_file(path: &Path) -> anyhow::Result<File> {
    owned_directory(path.parent().context("Missing project directory")?)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
        "Claude history is not a regular, user-owned file"
    );
    Ok(file)
}

#[derive(Default)]
struct Metadata {
    cwd: Option<PathBuf>,
    title: Option<String>,
    prompt: Option<String>,
    handoff_observed: bool,
}

fn short_title(value: &str) -> Option<String> {
    let title = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(160).collect())
}

impl Metadata {
    fn observe(&mut self, record: &Value, conversation: &str) {
        if record["isSidechain"] == true
            || record["sessionId"]
                .as_str()
                .is_some_and(|id| id != conversation)
        {
            return;
        }
        if let Some(cwd) = record["cwd"]
            .as_str()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            self.cwd = Some(cwd);
        }
        if record["type"] == "custom-title" {
            self.title = record["customTitle"].as_str().and_then(short_title);
        }
        self.handoff_observed |= record["type"] == "continued-in";
        if self.prompt.is_none() && record["type"] == "user" && record["isMeta"] != true {
            let content = block_text(&record["message"]["content"]);
            if !content.trim_start().starts_with('<') {
                self.prompt = short_title(&content);
            }
        }
    }
}

fn scan_metadata(file: &mut File, conversation: &str) -> anyhow::Result<Metadata> {
    let length = file.metadata()?.len();
    let mut metadata = Metadata::default();
    let mut prefix = Vec::new();
    Read::by_ref(file)
        .take(METADATA_LIMIT)
        .read_to_end(&mut prefix)?;
    scan_complete_lines(&prefix, |record| metadata.observe(record, conversation));
    if length > METADATA_LIMIT {
        file.seek(SeekFrom::Start(length.saturating_sub(64 * 1024)))?;
        let mut tail = Vec::new();
        Read::by_ref(file).take(64 * 1024).read_to_end(&mut tail)?;
        if let Some(newline) = tail.iter().position(|byte| *byte == b'\n') {
            scan_complete_lines(&tail[newline + 1..], |record| {
                metadata.observe(record, conversation)
            });
        }
    }
    Ok(metadata)
}

fn scan_complete_lines(bytes: &[u8], mut observe: impl FnMut(&Value)) {
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.last() == Some(&b'\n')
            && let Ok(record) = serde_json::from_slice::<Value>(line)
        {
            observe(&record);
        }
    }
}

fn saved_sessions(configuration: &Path, catalog: &mut Catalog) -> anyhow::Result<()> {
    let projects = configuration.join("projects");
    if !projects.try_exists()? {
        return Ok(());
    }
    owned_directory(&projects)?;
    for project in fs::read_dir(projects)? {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let result = (|| -> anyhow::Result<()> {
            owned_directory(&project.path())?;
            for entry in fs::read_dir(project.path())? {
                let entry = entry?;
                let path = entry.path();
                if path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                {
                    continue;
                }
                let Some(conversation) = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .filter(|name| Uuid::parse_str(name).is_ok())
                else {
                    continue;
                };
                let result = (|| -> anyhow::Result<Option<Session>> {
                    let mut file = transcript_file(&path)?;
                    let modified = file
                        .metadata()?
                        .modified()?
                        .duration_since(UNIX_EPOCH)?
                        .as_millis() as u64;
                    let metadata = scan_metadata(&mut file, conversation)?;
                    let Some(cwd) = metadata.cwd else {
                        return Ok(None);
                    };
                    if metadata.handoff_observed {
                        file.rewind()?;
                        match continuation_record(
                            BufReader::new(file.take(HISTORY_LIMIT + 1)),
                            conversation,
                        ) {
                            Ok(Some(target)) => {
                                catalog.handoffs.insert(
                                    native_id(configuration, conversation),
                                    native_id(configuration, &target),
                                );
                            }
                            Ok(None) => {}
                            Err(error) => catalog.warnings.push(format!(
                                "Could not group Claude handoff {conversation}: {error:#}"
                            )),
                        }
                    }
                    Ok(Some(Session {
                        id: native_id(configuration, conversation),
                        directory: configuration.to_owned(),
                        title: metadata
                            .title
                            .or(metadata.prompt)
                            .unwrap_or_else(|| format!("Claude · {}", &conversation[..8])),
                        cwd,
                        created_at_ms: modified,
                        lifecycle_version: 0,
                        source: SessionSource::Native {
                            configuration: configuration.to_owned(),
                            conversation_id: conversation.to_owned(),
                            transcript: Some(path.clone()),
                            job: None,
                        },
                    }))
                })();
                match result {
                    Ok(Some(session)) => {
                        if let Some(existing) = catalog
                            .sessions
                            .iter_mut()
                            .find(|existing| existing.id == session.id)
                        {
                            catalog.handoffs.remove(&session.id);
                            if let SessionSource::Native { transcript, .. } = &mut existing.source {
                                *transcript = None;
                            }
                            catalog.warnings.push(format!("Multiple saved files claim Claude conversation {conversation}; ambiguous saved history will not be opened"));
                        } else {
                            catalog.sessions.push(session);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => catalog
                        .warnings
                        .push(format!("Could not index {}: {error:#}", path.display())),
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            catalog
                .warnings
                .push(format!("Could not index Claude project: {error:#}"));
        }
    }
    Ok(())
}

pub(super) fn native_jobs(configuration: &Path) -> anyhow::Result<Vec<Value>> {
    smol::block_on(with_timeout(
        async {
            use futures::io::AsyncReadExt as _;
            let mut command = async_process::Command::new(native_binary()?);
            command
                .args(["agents", "--json", "--all"])
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
            let mut child = command.spawn()?;
            let output = child
                .stdout
                .take()
                .context("Missing native listing output")?;
            let errors = child
                .stderr
                .take()
                .context("Missing native listing errors")?;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = output.take(4 * 1024 * 1024 + 1);
            let mut errors = errors.take(65537);
            futures::try_join!(
                output.read_to_end(&mut stdout),
                errors.read_to_end(&mut stderr)
            )?;
            ensure!(
                stdout.len() <= 4 * 1024 * 1024 && stderr.len() <= 65536,
                "Native listing exceeds its size limit"
            );
            let status = child.status().await?;
            ensure!(
                status.success(),
                "Native session listing failed ({status}): {}",
                String::from_utf8_lossy(&stderr)
            );
            let rows: Vec<Value> =
                serde_json::from_slice(&stdout).context("Invalid native session listing")?;
            ensure!(rows.len() <= 10000, "Native catalog is too large");
            Ok(rows)
        },
        Duration::from_secs(5),
        "Native session discovery timed out; saved history is still available",
    ))
}

pub(super) fn extend_catalog(catalog: &mut Catalog) -> anyhow::Result<()> {
    let configuration = configuration_directory()?;
    if !configuration.try_exists()? {
        return Ok(());
    }
    let configuration = fs::canonicalize(configuration)?;
    owned_directory(&configuration)?;
    saved_sessions(&configuration, catalog)?;
    match native_jobs(&configuration) {
        Ok(jobs) => {
            for row in jobs {
                let job: NativeJob = match serde_json::from_value(row) {
                    Ok(job) => job,
                    Err(error) => {
                        catalog
                            .warnings
                            .push(format!("Skipped incomplete native catalog row: {error}"));
                        continue;
                    }
                };
                if let Err(error) = validate_job(&job) {
                    catalog
                        .warnings
                        .push(format!("Skipped invalid native process: {error:#}"));
                    continue;
                }
                if Uuid::parse_str(&job.session_id).is_err() || !job.cwd.is_absolute() {
                    catalog.warnings.push(
                        "Skipped native job with an invalid conversation ID or workspace".into(),
                    );
                    continue;
                }
                let id = native_id(&configuration, &job.session_id);
                let existing = catalog.sessions.iter_mut().find(|session| session.id == id);
                if let Some(session) = existing {
                    if job.kind == "interactive"
                        && matches!(&session.source,
                        SessionSource::Native { job: Some(existing), .. } if existing.kind == "background")
                    {
                        continue;
                    }
                    session.cwd = job.cwd.clone();
                    if let Some(name) = job.title() {
                        session.title = name;
                    }
                    if let SessionSource::Native {
                        job: native_job, ..
                    } = &mut session.source
                    {
                        *native_job = Some(job);
                    }
                } else {
                    catalog.sessions.push(Session {
                        id,
                        directory: configuration.clone(),
                        cwd: job.cwd.clone(),
                        title: job.title().unwrap_or_else(|| {
                            job.cwd
                                .file_name()
                                .unwrap_or(job.cwd.as_os_str())
                                .to_string_lossy()
                                .into_owned()
                        }),
                        lifecycle_version: 0,
                        created_at_ms: job.started_at,
                        source: SessionSource::Native {
                            configuration: configuration.clone(),
                            conversation_id: job.session_id.clone(),
                            transcript: None,
                            job: Some(job),
                        },
                    });
                }
            }
        }
        Err(error) => catalog
            .warnings
            .push(format!("Live Claude discovery unavailable: {error:#}")),
    }
    for session in &catalog.sessions {
        if !session.is_managed() {
            catalog
                .statuses
                .insert(session.id.clone(), session.status());
        }
    }
    Ok(())
}

pub(super) fn group_conversations(catalog: &mut Catalog) {
    let sessions: HashMap<_, _> = catalog
        .sessions
        .iter()
        .map(|session| (session.id.clone(), session))
        .collect();
    let mut neighbors = HashMap::<String, Vec<String>>::new();
    let mut predecessors = HashMap::<String, Vec<String>>::new();
    for (source, target) in &catalog.handoffs {
        if !sessions.contains_key(source) || !sessions.contains_key(target) {
            catalog.warnings.push(format!("Claude handoff destination is missing for {source}; its original history is retained"));
            continue;
        }
        neighbors
            .entry(source.clone())
            .or_default()
            .push(target.clone());
        neighbors
            .entry(target.clone())
            .or_default()
            .push(source.clone());
        predecessors
            .entry(target.clone())
            .or_default()
            .push(source.clone());
    }
    let mut visited = std::collections::HashSet::new();
    let mut conversations = Vec::new();
    for session in &catalog.sessions {
        if visited.contains(&session.id) {
            continue;
        }
        let mut component = Vec::new();
        let mut pending = vec![session.id.clone()];
        while let Some(identifier) = pending.pop() {
            if !visited.insert(identifier.clone()) {
                continue;
            }
            if let Some(adjacent) = neighbors.get(&identifier) {
                pending.extend(adjacent.iter().cloned());
            }
            component.push(identifier);
        }
        let roots: Vec<_> = component
            .iter()
            .filter(|identifier| !predecessors.contains_key(*identifier))
            .collect();
        let unambiguous = roots.len() == 1
            && component.len() <= 32
            && component.iter().all(|identifier| {
                predecessors
                    .get(identifier)
                    .is_none_or(|parents| parents.len() == 1)
            });
        if !unambiguous {
            catalog.warnings.push(
                "Ambiguous or cyclic Claude handoff history; keeping its native entries separate"
                    .into(),
            );
            conversations.extend(
                component
                    .iter()
                    .filter_map(|identifier| sessions.get(identifier))
                    .map(|session| Conversation::from_session((*session).clone())),
            );
            continue;
        }
        let Some(root) = roots
            .first()
            .and_then(|identifier| sessions.get(*identifier))
        else {
            continue;
        };
        let mut conversation = Conversation::from_session((*root).clone());
        while let Some(next) = conversation
            .current()
            .and_then(|current| catalog.handoffs.get(&current.id))
            .and_then(|identifier| sessions.get(identifier))
        {
            // Components above must be simple directed paths; a missing target
            // remains an individual history entry and is refused by open().
            conversation.updated_at_ms = conversation.updated_at_ms.max(next.created_at_ms);
            conversation.bind((*next).clone());
        }
        let Some(current) = conversation.current().cloned() else {
            continue;
        };
        conversation.cwd = current.cwd.clone();
        let has_saved_title = matches!(
            &current.source,
            SessionSource::Native {
                transcript: Some(_),
                ..
            } | SessionSource::Managed
        );
        let has_job_title = matches!(&current.source,
            SessionSource::Native { job: Some(job), .. } if job.title().is_some());
        if has_saved_title || has_job_title {
            conversation.title = current.title.clone();
        }
        if let Some(status) = catalog.statuses.get(&current.id).cloned() {
            catalog.statuses.insert(conversation.id.clone(), status);
        }
        conversations.push(conversation);
    }
    conversations.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    catalog.conversations = conversations;
}

pub(super) fn status(session: &Session) -> anyhow::Result<HostStatus> {
    let SessionSource::Native { job, .. } = &session.source else {
        bail!("Not a native session");
    };
    Ok(if job.as_ref().and_then(|job| job.pid).is_some() {
        let job = job.as_ref().context("Missing native job")?;
        let pid = job.pid.context("Missing native process")?;
        if !Path::new(&format!("/proc/{pid}")).try_exists()? {
            return Ok(HostStatus::new(
                HostPhase::Stopped,
                "Native Claude worker stopped. Refresh conversation to read its saved history; no process was restarted.",
            ));
        }
        if job.kind == "background" && endpoint_path(job)?.try_exists()? {
            binding(session)?;
            super::resume::check_connection(session, pid, None)?;
            HostStatus::new(HostPhase::Available, "Claude is ready to connect")
        } else {
            HostStatus::new(
                HostPhase::NeedsAdapter,
                if job.kind == "interactive" {
                    "This conversation is active in a Claude terminal. Run /bg there to hand it over, then select it here. Harness will not restart an active terminal."
                } else {
                    "This running Claude session has no Harness connection. It was left running unchanged."
                },
            )
        }
    } else {
        HostStatus::new(
            HostPhase::Saved,
            "Not running. Select this conversation to open it. Refreshing the list does not start Claude.",
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProcessIdentity {
    boot_id: String,
    start_ticks: String,
    executable: PathBuf,
    cwd: PathBuf,
    uid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Binding {
    endpoint_version: u32,
    job_id: String,
    pid: u32,
    runtime: PathBuf,
    socket_path: PathBuf,
    identity: ProcessIdentity,
    sha256: String,
}

fn start_ticks(stat: &str) -> anyhow::Result<String> {
    let fields = stat
        .rsplit_once(") ")
        .context("Invalid native process stat")?
        .1;
    let ticks = fields
        .split_whitespace()
        .nth(19)
        .context("Missing native process start time")?;
    ensure!(
        !ticks.is_empty() && ticks.bytes().all(|byte| byte.is_ascii_digit()),
        "Invalid native process start time"
    );
    Ok(ticks.to_owned())
}

fn process_identity(pid: u32) -> anyhow::Result<ProcessIdentity> {
    ensure!(pid > 0, "Native worker has stopped");
    let process = PathBuf::from(format!("/proc/{pid}"));
    let ticks = start_ticks(&fs::read_to_string(process.join("stat"))?)?;
    let identity = ProcessIdentity {
        boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned(),
        start_ticks: ticks.clone(),
        executable: fs::read_link(process.join("exe"))?,
        cwd: fs::read_link(process.join("cwd"))?,
        uid: fs::metadata(&process)?.uid(),
    };
    ensure!(
        start_ticks(&fs::read_to_string(process.join("stat"))?)? == ticks,
        "Native process changed during discovery"
    );
    Ok(identity)
}

fn endpoint_root() -> anyhow::Result<PathBuf> {
    let root = match std::env::var_os("HARNESS_CLAUDE_ENDPOINT_ROOT") {
        Some(path) => PathBuf::from(path),
        None => super::setup::endpoint_root(&fs::canonicalize(configuration_directory()?)?)?,
    };
    ensure!(root.is_absolute(), "Native endpoint root must be absolute");
    Ok(root)
}

fn endpoint_path(job: &NativeJob) -> anyhow::Result<PathBuf> {
    ensure!(
        (8..=64).contains(&job.id.len()) && job.id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid native job ID"
    );
    Ok(endpoint_root()?
        .join("jobs")
        .join(&job.id)
        .join("current.json"))
}

fn binding(session: &Session) -> anyhow::Result<Binding> {
    let SessionSource::Native { job: Some(job), .. } = &session.source else {
        bail!("No native worker endpoint");
    };
    ensure!(
        job.kind == "background",
        "An ordinary terminal session cannot be hot-attached"
    );
    let pid = job
        .pid
        .filter(|pid| *pid > 0)
        .context("Worker is stopped; reconnect never wakes it")?;
    let root = endpoint_root()?;
    let directory = root.join("jobs").join(&job.id);
    for path in [&root, &root.join("jobs"), &directory] {
        validate_private_directory(path)?;
    }
    let record: Binding = serde_json::from_slice(
        &read_optional(&endpoint_path(job)?)?.context("No native adapter endpoint")?,
    )?;
    validate_binding(job, &directory, &record, &process_identity(pid)?)?;
    validate_private_directory(&record.runtime)?;
    let socket = fs::symlink_metadata(&record.socket_path)?;
    ensure!(
        socket.file_type().is_socket()
            && socket.uid() == unsafe { libc::geteuid() }
            && socket.mode() & 0o077 == 0,
        "Native socket must be private, user-owned, and not a symlink"
    );
    Ok(record)
}

fn validate_binding(
    job: &NativeJob,
    directory: &Path,
    record: &Binding,
    identity: &ProcessIdentity,
) -> anyhow::Result<()> {
    ensure!(
        record.endpoint_version == 1,
        "Unsupported native endpoint version"
    );
    ensure!(
        record.job_id == job.id && Some(record.pid) == job.pid,
        "Native worker changed; refresh the session list"
    );
    ensure!(
        record.runtime == directory.join(record.pid.to_string())
            && record.socket_path == record.runtime.join("native.sock"),
        "Native endpoint escaped its job directory"
    );
    ensure!(
        record.sha256 == "b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca",
        "Native adapter build has not been verified"
    );
    ensure!(
        &record.identity == identity && identity.uid == unsafe { libc::geteuid() },
        "Stale native process identity; connection refused"
    );
    ensure!(
        identity.cwd == fs::canonicalize(&job.cwd)?,
        "Native worker workspace changed"
    );
    ensure!(
        identity
            .executable
            .file_name()
            .is_some_and(|name| name == "2.1.263"),
        "Unexpected native executable; compatibility must be rechecked"
    );
    Ok(())
}

fn validate_handshake(session: &Session, record: &Binding, hello: &Value) -> anyhow::Result<()> {
    let SessionSource::Native {
        conversation_id, ..
    } = &session.source
    else {
        bail!("Missing native identity");
    };
    ensure!(
        hello["event"] == "hello" && hello["protocol"] == "harness-native-lab/0",
        "Unsupported native adapter handshake"
    );
    ensure!(
        hello["pid"].as_u64() == Some(record.pid as u64)
            && hello["sessionId"].as_str() == Some(conversation_id),
        "Native conversation or process changed; refresh discovery"
    );
    ensure!(
        hello["epoch"]
            .as_str()
            .is_some_and(|epoch| Uuid::parse_str(epoch).is_ok())
            && hello["sequence"].as_u64().is_some(),
        "Native handshake is missing its epoch or event cursor"
    );
    Ok(())
}

async fn bound_connection(
    session: &Session,
    verification: bool,
) -> anyhow::Result<(
    futures::io::BufReader<smol::net::unix::UnixStream>,
    Binding,
    Value,
)> {
    let record = binding(session)?;
    let socket = smol::net::unix::UnixStream::connect(&record.socket_path).await?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0,
        "Could not identify native socket peer: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        credentials.pid > 0
            && credentials.pid as u32 == record.pid
            && credentials.uid == unsafe { libc::geteuid() },
        "Native socket belongs to a different worker"
    );
    let mut reader = futures::io::BufReader::new(socket);
    let hello = read_frame(&mut reader).await?;
    validate_handshake(session, &record, &hello)?;
    if !verification {
        super::resume::check_connection(session, record.pid, hello["epoch"].as_str())?;
    }
    ensure!(
        binding(session)? == record,
        "Native endpoint changed during handshake"
    );
    Ok((reader, record, hello))
}

pub(super) async fn connection(
    session: &Session,
) -> anyhow::Result<futures::io::BufReader<smol::net::unix::UnixStream>> {
    let (reader, _, _) = bound_connection(session, false).await?;
    Ok(reader)
}

pub(super) async fn snapshot_connection(
    session: &Session,
) -> anyhow::Result<(futures::io::BufReader<smol::net::unix::UnixStream>, Value)> {
    snapshot_for_verification(session, false).await
}

pub(super) async fn snapshot_for_verification(
    session: &Session,
    verification: bool,
) -> anyhow::Result<(futures::io::BufReader<smol::net::unix::UnixStream>, Value)> {
    let (mut reader, record, hello) = bound_connection(session, verification).await?;
    reader
        .get_mut()
        .write_all(b"{\"id\":\"snapshot\",\"method\":\"snapshot\"}\n")
        .await?;
    loop {
        let frame = read_frame(&mut reader).await?;
        if frame["id"] != "snapshot" {
            continue;
        }
        ensure!(
            frame.get("error").is_none(),
            "Native snapshot failed: {}",
            frame["error"]
        );
        let snapshot = &frame["result"];
        ensure!(
            snapshot["pid"] == hello["pid"]
                && snapshot["sessionId"] == hello["sessionId"]
                && snapshot["epoch"] == hello["epoch"],
            "Native identity changed during snapshot"
        );
        ensure!(
            snapshot["sequence"]
                .as_u64()
                .zip(hello["sequence"].as_u64())
                .is_some_and(|(snapshot, hello)| snapshot >= hello),
            "Native snapshot cursor moved backwards"
        );
        ensure!(
            binding(session)? == record,
            "Native worker changed during snapshot"
        );
        return Ok((reader, frame));
    }
}

pub(super) fn history(session: &Session) -> anyhow::Result<Projection> {
    validate_session(session)?;
    let SessionSource::Native {
        conversation_id,
        transcript,
        ..
    } = &session.source
    else {
        bail!("Not saved native history");
    };
    let mut projection = Projection {
        session_id: conversation_id.clone(),
        ..Projection::default()
    };
    if let Some(path) = transcript {
        let file = transcript_file(path)?;
        ensure!(
            file.metadata()?.len() <= HISTORY_LIMIT,
            "Saved history exceeds the 256 MiB reader limit; nothing was truncated"
        );
        projection.messages = read_history(
            BufReader::new(file.take(HISTORY_LIMIT + 1)),
            conversation_id,
        )?;
    }
    Ok(projection)
}

pub(super) fn complete_history(session: &Session) -> anyhow::Result<(Vec<Value>, String)> {
    complete_history_at(session, None)
}

pub(super) fn original_continuation_history(
    path: &Path,
    conversation: &str,
    source_digest: &str,
    handoff: bool,
) -> anyhow::Result<(Vec<Value>, Vec<Value>)> {
    use sha2::{Digest as _, Sha256};
    let mut reader = BufReader::new(transcript_file(path)?.take(HISTORY_LIMIT + 1));
    let mut bytes = Vec::new();
    let mut digest = Sha256::new();
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_FRAME + 1)
            .read_until(b'\n', &mut line)?;
        ensure!(
            count > 0 && line.last() == Some(&b'\n'),
            "The original continuation source is no longer available unchanged"
        );
        ensure!(
            count as u64 <= MAX_FRAME && (bytes.len() + count) as u64 <= HISTORY_LIMIT,
            "Original continuation history exceeds its reader limit"
        );
        digest.update(&line);
        bytes.extend_from_slice(&line);
        // Native resume can append records. Only a byte-exact original prefix
        // can justify repairing a proof produced by the old parent-only reader.
        if format!("{:x}", digest.clone().finalize()) == source_digest {
            break;
        }
    }
    let history = if handoff {
        let destination = continuation_record(std::io::Cursor::new(&bytes), conversation)?
            .context("Missing original handoff destination")?;
        handoff_prefix(&bytes, conversation, &destination)?
    } else {
        &bytes
    };
    Ok((
        read_history_with_parallel_results(std::io::Cursor::new(history), conversation, false)?,
        read_history(std::io::Cursor::new(history), conversation)?,
    ))
}

pub(super) fn handoff_history(
    session: &Session,
    destination: &str,
) -> anyhow::Result<(Vec<Value>, String)> {
    complete_history_at(session, Some(destination))
}

fn complete_history_at(
    session: &Session,
    handoff: Option<&str>,
) -> anyhow::Result<(Vec<Value>, String)> {
    use sha2::{Digest as _, Sha256};
    validate_session(session)?;
    let SessionSource::Native {
        conversation_id,
        transcript: Some(path),
        ..
    } = &session.source
    else {
        bail!("There is no saved transcript to continue");
    };
    let file = transcript_file(path)?;
    let mut bytes = Vec::new();
    file.take(HISTORY_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= HISTORY_LIMIT,
        "Saved history exceeds the continuation limit"
    );
    ensure!(
        !bytes.is_empty() && bytes.last() == Some(&b'\n'),
        "Saved history is empty or has an unfinished final record; no worker was started"
    );
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let history = match handoff {
        Some(destination) => handoff_prefix(&bytes, conversation_id, destination)?,
        None => bytes.as_slice(),
    };
    let messages = read_history(std::io::Cursor::new(history), conversation_id)?;
    Ok((messages, digest))
}

fn handoff_prefix<'a>(
    bytes: &'a [u8],
    conversation: &str,
    destination: &str,
) -> anyhow::Result<&'a [u8]> {
    ensure!(
        continuation_record(std::io::Cursor::new(bytes), conversation)?.as_deref()
            == Some(destination),
        "Native handoff destination changed; refresh before opening"
    );
    let mut offset = 0;
    let mut boundary = None;
    let mut suffix = Vec::new();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let record: Value = serde_json::from_slice(line)?;
        if boundary.is_none()
            && record["type"] == "continued-in"
            && record["sessionId"] == conversation
            && record["isSidechain"] != true
        {
            boundary = Some(offset);
        } else if boundary.is_some()
            && record["isSidechain"] != true
            && matches!(record["type"].as_str(), Some("user" | "assistant"))
        {
            ensure!(
                suffix.len() < 3,
                "Source conversation changed after native handoff"
            );
            suffix.push(record);
        }
        offset += line.len();
    }
    let prefix = bytes
        .get(..boundary.context("Missing native handoff boundary")?)
        .context("Invalid native handoff boundary")?;
    let messages = read_history(std::io::Cursor::new(prefix), conversation)?;
    let mut parent = messages.last().and_then(|message| message["uuid"].as_str());
    // Claude clones the conversation before appending /background's local-only
    // UI records. Exclude only this known suffix, never later conversational work.
    for (index, record) in suffix.iter().enumerate() {
        let content = record["message"]["content"].as_str().unwrap_or("");
        let recognized = match index {
            0 => {
                record["isMeta"] == true
                    && content
                        == "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands. DO NOT respond to these messages or otherwise consider them in your response unless the user explicitly asks you to.</local-command-caveat>"
            }
            1 => {
                content.split_whitespace().collect::<Vec<_>>().join(" ")
                    == "<command-name>/background</command-name> <command-message>background</command-message> <command-args></command-args>"
            }
            2 => content == "<local-command-stdout>(no content)</local-command-stdout>",
            _ => false,
        };
        ensure!(
            recognized
                && record["type"] == "user"
                && record["message"]["role"] == "user"
                && record["sessionId"] == conversation
                && record["parentUuid"].as_str() == parent
                && record["uuid"]
                    .as_str()
                    .is_some_and(|identifier| Uuid::parse_str(identifier).is_ok()),
            "Source conversation changed after native handoff; refusing to discard its messages"
        );
        parent = record["uuid"].as_str();
    }
    Ok(prefix)
}

pub(super) fn continued_in(session: &Session) -> anyhow::Result<Option<String>> {
    validate_session(session)?;
    let SessionSource::Native {
        conversation_id,
        transcript: Some(path),
        ..
    } = &session.source
    else {
        return Ok(None);
    };
    continuation_record(
        BufReader::new(transcript_file(path)?.take(HISTORY_LIMIT + 1)),
        conversation_id,
    )
}

fn continuation_record(
    mut reader: impl BufRead,
    conversation: &str,
) -> anyhow::Result<Option<String>> {
    let mut target = None;
    let mut consumed = 0u64;
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_FRAME + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            return Ok(target);
        }
        consumed += count as u64;
        ensure!(
            consumed <= HISTORY_LIMIT && count as u64 <= MAX_FRAME,
            "Native handoff history exceeds its limit"
        );
        ensure!(
            line.last() == Some(&b'\n'),
            "Native history is still being written; retry opening the conversation"
        );
        let record: Value =
            serde_json::from_slice(&line).context("Malformed native handoff history")?;
        if record["type"] != "continued-in"
            || record["sessionId"] != conversation
            || record["isSidechain"] == true
        {
            continue;
        }
        let next = record["continuedInSessionId"]
            .as_str()
            .context("Native handoff has no destination")?;
        ensure!(
            Uuid::parse_str(next).is_ok() && next != conversation,
            "Native handoff has an invalid destination"
        );
        ensure!(
            target.as_deref().is_none_or(|previous| previous == next),
            "Native history has conflicting handoff destinations"
        );
        target = Some(next.to_owned());
    }
}

fn read_history(mut reader: impl BufRead, conversation: &str) -> anyhow::Result<Vec<Value>> {
    read_history_with_parallel_results(&mut reader, conversation, true)
}

fn read_history_with_parallel_results(
    mut reader: impl BufRead,
    conversation: &str,
    recover_parallel: bool,
) -> anyhow::Result<Vec<Value>> {
    let mut records = HashMap::<String, Value>::new();
    let mut file_order = Vec::new();
    let mut leaf = None;
    let mut consumed = 0u64;
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_FRAME + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        consumed += count as u64;
        ensure!(
            consumed <= HISTORY_LIMIT,
            "Saved history grew beyond the reader limit; refresh to retry"
        );
        ensure!(
            count as u64 <= MAX_FRAME,
            "Saved history contains a record larger than 32 MiB"
        );
        // A native writer may be halfway through appending its last record.
        if line.last() != Some(&b'\n') {
            break;
        }
        let record: Value = serde_json::from_slice(&line).context(
            "Malformed complete record in Claude history; refusing a partial transcript",
        )?;
        if record["isSidechain"] == true {
            continue;
        }
        let Some(identifier) = record["uuid"].as_str().map(ToOwned::to_owned) else {
            continue;
        };
        if record["parentUuid"].is_null()
            && !record
                .as_object()
                .is_some_and(|record| record.contains_key("parentUuid"))
        {
            continue;
        }
        // Forks can retain ancestors with the original session ID. The selected
        // leaf must belong to this session, but its parent chain need not.
        if record["sessionId"] == conversation {
            leaf = Some(identifier.clone());
        }
        if !records.contains_key(&identifier) {
            file_order.push(identifier.clone());
        }
        records.insert(identifier, record);
    }
    let mut messages = Vec::new();
    let mut visited = std::collections::HashSet::new();
    while let Some(identifier) = leaf {
        ensure!(
            visited.insert(identifier.clone()),
            "Cycle in saved Claude history"
        );
        let record = records.get(&identifier).context(
            "Saved Claude history is missing an ancestor; refusing to splice unrelated branches",
        )?;
        leaf = record["parentUuid"].as_str().map(ToOwned::to_owned);
        messages.push(record.clone());
    }
    messages.reverse();
    if recover_parallel {
        restore_parallel_results(messages, &records, &file_order, visited)
    } else {
        Ok(messages)
    }
}

fn restore_parallel_results(
    messages: Vec<Value>,
    records: &HashMap<String, Value>,
    file_order: &[String],
    mut selected: std::collections::HashSet<String>,
) -> anyhow::Result<Vec<Value>> {
    let mut responses = HashMap::<&str, Vec<&Value>>::new();
    let mut results = HashMap::<&str, Vec<&Value>>::new();
    let mut positions = HashMap::new();
    for (position, identifier) in file_order.iter().enumerate() {
        positions.insert(identifier.as_str(), position);
        let record = records
            .get(identifier)
            .context("Missing saved history record")?;
        if record["type"] == "assistant" {
            if let Some(response) = record["message"]["id"].as_str() {
                responses.entry(response).or_default().push(record);
            }
        } else if record["type"] == "user"
            && let Some(parent) = record["parentUuid"].as_str()
            && record["message"]["content"]
                .as_array()
                .is_some_and(|content| content.iter().any(|block| block["type"] == "tool_result"))
        {
            results.entry(parent).or_default().push(record);
        }
    }
    let chain_positions = messages
        .iter()
        .enumerate()
        .filter_map(|(position, message)| {
            message["uuid"]
                .as_str()
                .map(|identifier| (identifier, position))
        })
        .collect::<HashMap<_, _>>();
    let mut visited_responses = std::collections::HashSet::new();
    let mut patches = Vec::<(usize, usize, Vec<&Value>)>::new();
    for (start, message) in messages.iter().enumerate() {
        if message["type"] != "assistant" {
            continue;
        }
        let Some(response) = message["message"]["id"].as_str() else {
            continue;
        };
        if !visited_responses.insert(response) {
            continue;
        }
        let chunks = responses
            .get(response)
            .context("Missing selected assistant response")?;
        let mut end = start + 1;
        let mut recovered = Vec::new();
        // Native parallel results point to their originating assistant chunk,
        // not to the preceding file record. A parent-only walk drops siblings.
        for chunk in chunks {
            let identifier = chunk["uuid"].as_str().context("Missing assistant UUID")?;
            if let Some(position) = chain_positions.get(identifier) {
                end = end.max(position + 1);
            }
            if selected.insert(identifier.to_owned()) {
                recovered.push(*chunk);
            }
            for result in results.get(identifier).into_iter().flatten() {
                let result_identifier = result["uuid"]
                    .as_str()
                    .context("Missing tool result UUID")?;
                if selected.contains(result_identifier) {
                    continue;
                }
                let content = result["message"]["content"]
                    .as_array()
                    .context("Missing tool results")?;
                let inputs = chunk["message"]["content"]
                    .as_array()
                    .context("Missing tool inputs")?;
                ensure!(
                    content
                        .iter()
                        .filter(|block| block["type"] == "tool_result")
                        .all(|block| {
                            block["tool_use_id"].as_str().is_some_and(|tool| {
                                inputs
                                    .iter()
                                    .any(|input| input["type"] == "tool_use" && input["id"] == tool)
                            })
                        }),
                    "Saved parallel tool result does not match its assistant's tool call"
                );
                ensure!(
                    result["sourceToolAssistantUUID"].is_null()
                        || result["sourceToolAssistantUUID"] == identifier,
                    "Saved parallel tool result has conflicting parent identities"
                );
                selected.insert(result_identifier.to_owned());
                recovered.push(*result);
            }
        }
        if recovered.is_empty() {
            continue;
        }
        while messages
            .get(end)
            .is_some_and(|record| match record["type"].as_str() {
                Some("assistant") => record["message"]["id"] == response,
                Some("user") => {
                    record["isMeta"] == true
                        || record["message"]["content"]
                            .as_array()
                            .is_some_and(|content| {
                                content.iter().any(|block| block["type"] == "tool_result")
                            })
                }
                Some("attachment") => true,
                Some("system") => record["subtype"] != "compact_boundary",
                _ => false,
            })
        {
            end += 1;
        }
        if let Some((_, prior_end, prior_recovered)) = patches.last_mut()
            && start < *prior_end
        {
            *prior_end = (*prior_end).max(end);
            prior_recovered.extend(recovered);
        } else {
            patches.push((start, end, recovered));
        }
    }
    let position = |message: &Value| -> anyhow::Result<usize> {
        message["uuid"]
            .as_str()
            .and_then(|identifier| positions.get(identifier))
            .copied()
            .context("Missing saved message position")
    };
    let mut restored = Vec::new();
    let mut cursor = 0;
    for (start, end, recovered) in patches {
        restored.extend_from_slice(
            messages
                .get(cursor..=start)
                .context("Invalid response start")?,
        );
        let mut recovered = recovered
            .into_iter()
            .map(|record| Ok((position(record)?, record)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        recovered.sort_by_key(|(position, _)| *position);
        let mut recovered = recovered.into_iter().peekable();
        for record in messages
            .get(start + 1..end)
            .context("Invalid response end")?
        {
            let position = position(record)?;
            while recovered
                .peek()
                .is_some_and(|(candidate, _)| *candidate < position)
            {
                if let Some((_, record)) = recovered.next() {
                    restored.push(record.clone());
                }
            }
            restored.push(record.clone());
        }
        restored.extend(recovered.map(|(_, record)| record.clone()));
        cursor = end;
    }
    restored.extend_from_slice(messages.get(cursor..).context("Invalid history tail")?);
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    #[test]
    fn hidden_conversations_are_explicit_ids_scoped_to_the_profile() -> anyhow::Result<()> {
        let identifier = "44860356-aa4d-4520-8884-909e4258efde";
        let bytes = serde_json::to_vec(&json!({"version":1,"conversations":[identifier]}))?;
        let hidden = hidden_conversation_ids(Path::new("/private/profile"), &bytes)?;
        assert_eq!(
            hidden,
            vec![native_id(Path::new("/private/profile"), identifier)]
        );
        assert_ne!(
            hidden,
            hidden_conversation_ids(Path::new("/another/profile"), &bytes)?
        );
        for invalid in [
            json!({"version":2,"conversations":[identifier]}),
            json!({"version":1,"conversations":["harness-*"]}),
            json!({"version":1,"conversations":["/tmp"]}),
        ] {
            assert!(
                hidden_conversation_ids(
                    Path::new("/private/profile"),
                    &serde_json::to_vec(&invalid)?
                )
                .is_err()
            );
        }
        Ok(())
    }

    fn encoded(records: &[Value]) -> Vec<u8> {
        records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>()
            .into_bytes()
    }

    fn message(identifier: &str, parent: Option<&str>) -> Value {
        json!({"type":"user", "uuid":identifier, "parentUuid":parent, "sessionId":"conversation", "message":{"role":"user", "content":identifier}})
    }

    fn parallel_fixture() -> Vec<Value> {
        let assistant = |identifier: &str, parent: &str, tool: &str| {
            json!({
                "type":"assistant", "uuid":identifier, "parentUuid":parent, "sessionId":"conversation",
                "message":{"id":"response", "role":"assistant", "content":[{"type":"tool_use", "id":tool, "name":"Bash", "input":{"command":"true"}}]}
            })
        };
        let result = |identifier: &str, parent: &str, tool: &str| {
            json!({
                "type":"user", "uuid":identifier, "parentUuid":parent, "sourceToolAssistantUUID":parent,
                "sessionId":"conversation", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":tool, "content":"saved output"}]}
            })
        };
        vec![
            message("root", None),
            assistant("first", "root", "tool1"),
            assistant("second", "first", "tool2"),
            result("result1", "first", "tool1"),
            result("result2", "second", "tool2"),
            message("next", Some("result2")),
        ]
    }

    fn identifiers(messages: &[Value]) -> Vec<&str> {
        messages
            .iter()
            .filter_map(|message| message["uuid"].as_str())
            .collect()
    }

    #[test]
    fn history_recovers_parallel_results_in_native_file_order() -> anyhow::Result<()> {
        let records = parallel_fixture();
        let restored = read_history(encoded(&records).as_slice(), "conversation")?;
        assert_eq!(identifiers(&restored), identifiers(&records));
        let legacy = read_history_with_parallel_results(
            encoded(&records).as_slice(),
            "conversation",
            false,
        )?;
        assert_eq!(
            identifiers(&legacy),
            ["root", "first", "second", "result2", "next"]
        );
        Ok(())
    }

    #[test]
    fn history_recovers_off_chain_chunks_but_not_other_responses_or_sidechains()
    -> anyhow::Result<()> {
        let mut records = parallel_fixture();
        records.get_mut(2).context("Missing chunk")?["parentUuid"] = json!("root");
        let mut unrelated = records.get(1).context("Missing chunk")?.clone();
        unrelated["uuid"] = json!("unrelated");
        unrelated["message"]["id"] = json!("different-response");
        records.insert(1, unrelated);
        let mut sidechain = records.get(2).context("Missing chunk")?.clone();
        sidechain["uuid"] = json!("sidechain");
        sidechain["isSidechain"] = json!(true);
        records.insert(2, sidechain);
        let restored = read_history(encoded(&records).as_slice(), "conversation")?;
        // Native keeps the first selected chunk at the start of a recovered span.
        assert_eq!(
            identifiers(&restored),
            ["root", "second", "first", "result1", "result2", "next"]
        );
        Ok(())
    }

    #[test]
    fn history_recovery_never_reorders_selected_ancestors() -> anyhow::Result<()> {
        let mut records = parallel_fixture();
        records.swap(1, 2);
        let restored = read_history(encoded(&records).as_slice(), "conversation")?;
        assert_eq!(
            identifiers(&restored),
            ["root", "first", "second", "result1", "result2", "next"]
        );
        Ok(())
    }

    #[test]
    fn history_rejects_unexplained_parallel_results() -> anyhow::Result<()> {
        let mut records = parallel_fixture();
        records.get_mut(3).context("Missing result")?["message"]["content"][0]["tool_use_id"] =
            json!("other-tool");
        assert!(read_history(encoded(&records).as_slice(), "conversation").is_err());
        let mut records = parallel_fixture();
        records.get_mut(3).context("Missing result")?["sourceToolAssistantUUID"] =
            json!("other-parent");
        assert!(read_history(encoded(&records).as_slice(), "conversation").is_err());
        Ok(())
    }

    #[test]
    fn history_follows_the_selected_branch_not_every_line() -> anyhow::Result<()> {
        let messages = read_history(encoded(&[
            message("root", None), message("discarded", Some("root")), message("chosen", Some("root")),
            json!({"type":"cost-state", "sessionId":"conversation"}),
            json!({"type":"assistant", "uuid":"subagent", "parentUuid":null, "sessionId":"conversation", "isSidechain":true}),
        ]).as_slice(), "conversation")?;
        assert_eq!(
            messages
                .iter()
                .map(|message| text(message, "uuid"))
                .collect::<Vec<_>>(),
            ["root", "chosen"]
        );
        Ok(())
    }

    #[test]
    fn history_allows_fork_ancestors_but_does_not_select_a_foreign_leaf() -> anyhow::Result<()> {
        let mut ancestor = message("ancestor", None);
        ancestor["sessionId"] = json!("original-conversation");
        let mut foreign = message("foreign", Some("ancestor"));
        foreign["sessionId"] = json!("other-conversation");
        let messages = read_history(
            encoded(&[ancestor, message("fork", Some("ancestor")), foreign]).as_slice(),
            "conversation",
        )?;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages.last().context("Missing leaf")?["uuid"], "fork");
        Ok(())
    }

    #[test]
    fn history_tolerates_only_an_incomplete_final_record() -> anyhow::Result<()> {
        let mut bytes = encoded(&[message("root", None)]);
        bytes.extend_from_slice(b"{\"type\":");
        assert_eq!(read_history(bytes.as_slice(), "conversation")?.len(), 1);
        bytes.push(b'\n');
        assert!(read_history(bytes.as_slice(), "conversation").is_err());
        Ok(())
    }

    #[test]
    fn history_rejects_cycles_and_missing_ancestors() {
        assert!(
            read_history(
                encoded(&[message("root", Some("missing"))]).as_slice(),
                "conversation"
            )
            .is_err()
        );
        assert!(
            read_history(
                encoded(&[
                    message("root", Some("child")),
                    message("child", Some("root"))
                ])
                .as_slice(),
                "conversation"
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_catalog_rows_do_not_require_background_job_ids() -> anyhow::Result<()> {
        let value = json!({"kind":"interactive", "sessionId":Uuid::new_v4(),
            "pid":std::process::id(), "cwd":"/tmp"});
        let terminal: NativeJob = serde_json::from_value(value.clone())?;
        validate_job(&terminal)?;
        assert!(terminal.id.is_empty());
        let mut background = value.clone();
        background["kind"] = json!("background");
        assert!(validate_job(&serde_json::from_value(background)?).is_err());
        let mut stopped_terminal = value;
        stopped_terminal["pid"] = Value::Null;
        assert!(validate_job(&serde_json::from_value(stopped_terminal)?).is_err());
        Ok(())
    }

    fn grouping_session(title: &str) -> Session {
        let configuration = PathBuf::from("/tmp/claude-grouping-fixture");
        let conversation_id = Uuid::new_v4().to_string();
        Session {
            id: native_id(&configuration, &conversation_id),
            directory: configuration.clone(),
            cwd: configuration.clone(),
            title: title.into(),
            lifecycle_version: 0,
            created_at_ms: 0,
            source: SessionSource::Native {
                configuration,
                conversation_id,
                transcript: None,
                job: None,
            },
        }
    }

    #[test]
    fn native_handoffs_have_one_logical_conversation_and_an_explicit_current_endpoint()
    -> anyhow::Result<()> {
        let original = grouping_session("Original title");
        let intermediate = grouping_session("Intermediate");
        let current = grouping_session("Current");
        let mut catalog = Catalog::default();
        catalog.sessions = vec![current.clone(), original.clone(), intermediate.clone()];
        catalog
            .handoffs
            .insert(original.id.clone(), intermediate.id.clone());
        catalog
            .handoffs
            .insert(intermediate.id.clone(), current.id.clone());
        group_conversations(&mut catalog);
        assert_eq!(catalog.conversations.len(), 1);
        let conversation = catalog
            .conversations
            .first()
            .context("Missing conversation")?;
        assert_eq!(conversation.id, original.id);
        assert_eq!(
            conversation.entry().map(|session| &session.id),
            Some(&original.id)
        );
        assert_eq!(
            conversation.current().map(|session| &session.id),
            Some(&current.id)
        );
        assert_eq!(
            conversation.aliases,
            vec![original.id, intermediate.id, current.id]
        );
        assert_eq!(conversation.title, "Original title");
        assert_eq!(
            catalog.sessions.len(),
            3,
            "Native records remain individually addressable"
        );
        Ok(())
    }

    #[test]
    fn ambiguous_handoff_graphs_never_hide_native_histories() {
        let first = grouping_session("First");
        let second = grouping_session("Second");
        let third = grouping_session("Third");
        for links in [
            vec![
                (first.id.clone(), second.id.clone()),
                (second.id.clone(), first.id.clone()),
            ],
            vec![
                (first.id.clone(), third.id.clone()),
                (second.id.clone(), third.id.clone()),
            ],
            vec![(first.id.clone(), "missing-target".into())],
        ] {
            let mut catalog = Catalog::default();
            catalog.sessions = vec![first.clone(), second.clone(), third.clone()];
            catalog.handoffs = links.into_iter().collect();
            group_conversations(&mut catalog);
            assert_eq!(catalog.conversations.len(), 3);
            assert!(!catalog.warnings.is_empty());
        }
    }

    #[test]
    fn saved_history_is_read_only_and_uses_shared_tool_projection() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let configuration = fixture.0.join("claude");
        let project = configuration.join("projects/test");
        private_directory(&project)?;
        let conversation = Uuid::new_v4().to_string();
        let mut user = message("prompt", None);
        user["sessionId"] = json!(conversation);
        user["cwd"] = json!(fixture.0);
        let records = encoded(&[
            user,
            json!({"type":"assistant", "uuid":"tool", "parentUuid":"prompt", "sessionId":conversation,
            "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"write", "name":"Write", "input":{"file_path":"/tmp/example.rs", "content":"fn example() {}"}}]}}),
        ]);
        let path = project.join(format!("{conversation}.jsonl"));
        write_new(&path, &records)?;
        let mut catalog = Catalog::default();
        saved_sessions(&configuration, &mut catalog)?;
        assert_eq!(catalog.sessions.len(), 1);
        let session = catalog.sessions.first().context("No saved session")?;
        validate_session(session)?;
        let projection = history(session)?;
        assert!(!projection.ready && !projection.active && projection.dialogs.is_empty());
        assert_eq!(projection.messages.len(), 2);
        let items = projection.items();
        assert!(items.iter().any(
            |item| item.kind == TranscriptKind::FileChange && item.title.contains("example.rs")
        ));
        assert_eq!(fs::read(path)?, records);
        assert_eq!(status(session)?.phase, HostPhase::Saved);
        assert!(!status(session)?.can_reconnect());
        Ok(())
    }

    #[test]
    fn metadata_prefers_authored_titles_and_ignores_sidechains() {
        let mut metadata = Metadata::default();
        metadata.observe(&message("first prompt", None), "conversation");
        metadata.observe(&json!({"type":"custom-title", "customTitle":"A useful title", "sessionId":"conversation"}), "conversation");
        metadata.observe(
            &json!({"type":"custom-title", "customTitle":"Wrong", "sessionId":"another"}),
            "conversation",
        );
        assert_eq!(metadata.title.as_deref(), Some("A useful title"));
        assert_eq!(metadata.prompt.as_deref(), Some("first prompt"));
    }

    #[test]
    fn native_handoff_links_are_explicit_and_unambiguous() -> anyhow::Result<()> {
        let destination = Uuid::new_v4().to_string();
        let link = json!({"type":"continued-in", "sessionId":"conversation", "continuedInSessionId":destination});
        assert_eq!(
            continuation_record(encoded(&[link.clone()]).as_slice(), "conversation")?,
            Some(destination)
        );
        assert_eq!(
            continuation_record(encoded(&[link.clone()]).as_slice(), "another")?,
            None
        );
        let mut conflicting = link.clone();
        conflicting["continuedInSessionId"] = json!(Uuid::new_v4());
        assert!(
            continuation_record(
                encoded(&[link.clone(), conflicting]).as_slice(),
                "conversation"
            )
            .is_err()
        );
        let mut unsafe_link = link.clone();
        unsafe_link["continuedInSessionId"] = json!("../../other-profile");
        assert!(continuation_record(encoded(&[unsafe_link]).as_slice(), "conversation").is_err());
        let mut incomplete = encoded(&[link]);
        incomplete.pop();
        assert!(continuation_record(incomplete.as_slice(), "conversation").is_err());
        Ok(())
    }

    fn handoff_records(destination: &str) -> Vec<Value> {
        let mut records = vec![
            message("root", None),
            json!({"type":"continued-in", "sessionId":"conversation", "continuedInSessionId":destination}),
        ];
        let mut parent = "root".to_owned();
        for (index, content) in [
            "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands. DO NOT respond to these messages or otherwise consider them in your response unless the user explicitly asks you to.</local-command-caveat>",
            "<command-name>/background</command-name>\n <command-message>background</command-message>\n <command-args></command-args>",
            "<local-command-stdout>(no content)</local-command-stdout>",
        ].into_iter().enumerate() {
            let identifier = Uuid::new_v4().to_string();
            let mut record = message(&identifier, Some(&parent));
            record["message"]["content"] = json!(content);
            if index == 0 {
                record["isMeta"] = json!(true);
            }
            records.push(record);
            parent = identifier;
        }
        records
    }

    #[test]
    fn handoff_proof_excludes_only_native_post_transfer_command_bookkeeping() -> anyhow::Result<()>
    {
        let destination = Uuid::new_v4().to_string();
        let records = handoff_records(&destination);
        for length in 2..=records.len() {
            let bytes = encoded(&records[..length]);
            let prefix = handoff_prefix(&bytes, "conversation", &destination)?;
            assert_eq!(
                read_history(prefix, "conversation")?,
                vec![message("root", None)]
            );
            assert_eq!(
                read_history(bytes.as_slice(), "conversation")?.len(),
                length - 1
            );
        }
        Ok(())
    }

    #[test]
    fn handoff_proof_refuses_unrecognized_or_divergent_source_changes() -> anyhow::Result<()> {
        let destination = Uuid::new_v4().to_string();
        let records = handoff_records(&destination);
        let rejects = |records: &[Value], destination: &str| {
            assert!(handoff_prefix(&encoded(records), "conversation", destination).is_err());
        };
        rejects(&records, &Uuid::new_v4().to_string());
        rejects(&records[..1], &destination);
        for (index, path, value) in [
            (2, "/isMeta", json!(false)),
            (2, "/parentUuid", json!("different-parent")),
            (2, "/uuid", json!("invalid-identifier")),
            (2, "/sessionId", json!("another-conversation")),
            (3, "/message/content", json!("An actual later request")),
            (3, "/type", json!("assistant")),
            (3, "/message/role", json!("assistant")),
            (
                4,
                "/message/content",
                json!("<local-command-stdout>different output</local-command-stdout>"),
            ),
        ] {
            let mut changed = records.clone();
            *changed
                .get_mut(index)
                .and_then(|record| record.pointer_mut(path))
                .context("Missing fixture field")? = value;
            rejects(&changed, &destination);
        }
        let mut later_work = records.clone();
        later_work.push(message(
            "real-later-work",
            records.last().and_then(|record| record["uuid"].as_str()),
        ));
        rejects(&later_work, &destination);
        let mut conflict = records.clone();
        conflict.push(json!({"type":"continued-in", "sessionId":"conversation", "continuedInSessionId":Uuid::new_v4()}));
        rejects(&conflict, &destination);
        Ok(())
    }

    #[test]
    fn catalog_ignores_nested_agents_and_rejects_symlinked_transcripts() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let projects = fixture.0.join("projects");
        let project = projects.join("test");
        private_directory(&project.join("subagents"))?;
        let conversation = Uuid::new_v4().to_string();
        let mut user = message("root", None);
        user["sessionId"] = json!(conversation);
        user["cwd"] = json!(fixture.0);
        let nested = project
            .join("subagents")
            .join(format!("{conversation}.jsonl"));
        write_new(&nested, &encoded(&[user]))?;
        std::os::unix::fs::symlink(&nested, project.join(format!("{conversation}.jsonl")))?;
        let mut catalog = Catalog::default();
        saved_sessions(&fixture.0, &mut catalog)?;
        assert!(catalog.sessions.is_empty());
        assert_eq!(catalog.warnings.len(), 1);
        Ok(())
    }

    #[test]
    fn configuration_namespaces_and_legacy_catalogs_remain_distinct() -> anyhow::Result<()> {
        let id = Uuid::new_v4().to_string();
        assert_ne!(
            native_id(Path::new("/one"), &id),
            native_id(Path::new("/two"), &id)
        );
        let session: Session = serde_json::from_value(
            json!({"id":id, "directory":format!("/tmp/harness-claude-{id}"), "cwd":"/tmp", "title":"Existing host"}),
        )?;
        assert!(session.is_managed());
        session.validate()?;
        Ok(())
    }

    #[test]
    fn process_identity_handles_parentheses_and_pid_reuse() -> anyhow::Result<()> {
        let stat = format!(
            "123 (a name ) with parentheses) S {} 987 0",
            (0..18).map(|_| "0").collect::<Vec<_>>().join(" ")
        );
        assert_eq!(start_ticks(&stat)?, "987");
        let identity = process_identity(std::process::id())?;
        assert_eq!(identity.uid, unsafe { libc::geteuid() });
        assert!(start_ticks("not a stat record").is_err());
        Ok(())
    }

    #[test]
    fn native_binding_rejects_stale_workers_paths_and_builds() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let job = NativeJob {
            id: "12345678".into(),
            session_id: Uuid::new_v4().to_string(),
            cwd: fixture.0.clone(),
            kind: "background".into(),
            pid: Some(123),
            name: None,
            started_at: 0,
        };
        let identity = ProcessIdentity {
            boot_id: "boot".into(),
            start_ticks: "100".into(),
            executable: "/native/2.1.263".into(),
            cwd: fs::canonicalize(&fixture.0)?,
            uid: unsafe { libc::geteuid() },
        };
        let directory = fixture.0.join("jobs/12345678");
        let original = Binding {
            endpoint_version: 1,
            job_id: job.id.clone(),
            pid: 123,
            runtime: directory.join("123"),
            socket_path: directory.join("123/native.sock"),
            identity: identity.clone(),
            sha256: "b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca".into(),
        };
        validate_binding(&job, &directory, &original, &identity)?;
        let mut changed = original.clone();
        changed.identity.start_ticks = "101".into();
        assert!(validate_binding(&job, &directory, &changed, &identity).is_err());
        let mut changed = original.clone();
        changed.pid = 124;
        assert!(validate_binding(&job, &directory, &changed, &identity).is_err());
        let mut changed = original.clone();
        changed.socket_path = PathBuf::from("/tmp/other.sock");
        assert!(validate_binding(&job, &directory, &changed, &identity).is_err());
        let mut changed = original;
        changed.sha256 = "new-build".into();
        assert!(validate_binding(&job, &directory, &changed, &identity).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "Reads the explicitly selected local Claude profile; never starts or resumes a worker"]
    fn local_native_catalog_and_history_read_only_smoke() -> anyhow::Result<()> {
        let catalog = sessions()?;
        let mut saved = 0;
        let mut connected = 0;
        for session in &catalog.sessions {
            if session.is_managed() {
                continue;
            }
            let projection = session
                .saved_history()
                .with_context(|| format!("History reader failed for {}", session.id))?;
            ensure!(
                !projection.ready && !projection.active && projection.dialogs.is_empty(),
                "Saved history exposed live controls"
            );
            saved += 1;
            if session.status().can_reconnect() {
                smol::block_on(async {
                    let (_reader, snapshot) = super::super::snapshot_connection(session).await?;
                    let mut projection = Projection::default();
                    projection.apply(snapshot)?;
                    ensure!(projection.ready, "Native snapshot did not become ready");
                    Ok::<_, anyhow::Error>(())
                })?;
                connected += 1;
            }
        }
        println!(
            "Read {saved} native conversations; validated {connected} live connections; {} discovery warnings",
            catalog.warnings.len()
        );
        for warning in catalog.warnings {
            println!("{warning}");
        }
        Ok(())
    }

    #[test]
    fn projection_rejects_missing_duplicate_and_skipped_event_cursors() -> anyhow::Result<()> {
        let snapshot = json!({"id":"snapshot", "result":{"messages":[], "dialogs":[], "epoch":"test-epoch", "sessionId":"test-session", "sequence":10, "turn":{}}});
        for sequence in [json!(10), json!(12), Value::Null] {
            let mut projection = Projection::default();
            projection.apply(snapshot.clone())?;
            assert!(projection.apply(json!({"event":"turn", "epoch":"test-epoch", "sequence":sequence, "data":{}})).is_err());
        }
        let mut projection = Projection::default();
        projection.apply(snapshot)?;
        projection
            .apply(json!({"event":"turn", "epoch":"test-epoch", "sequence":11, "data":{}}))?;
        assert_eq!(projection.sequence, Some(11));
        projection.disconnect();
        assert_eq!(projection.sequence, None);
        Ok(())
    }
}
