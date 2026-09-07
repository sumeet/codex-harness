use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, Read, Write},
    os::fd::AsRawFd as _,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail, ensure};
use base64::Engine as _;
use futures::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use harness_protocol::{PendingRequest, TranscriptItem, TranscriptKind};
use portable_pty::{CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[path = "claude_creation.rs"]
pub mod creation;
#[path = "claude_sessions.rs"]
mod discovery;
#[path = "claude_resume.rs"]
pub mod resume;
#[path = "claude_setup.rs"]
pub mod setup;

const MAX_FRAME: u64 = 32 * 1024 * 1024;
const ASSETS: &[(&str, &str)] = &[
    (
        "preload.mjs",
        include_str!("../../../research/claude-native-lab/preload.mjs"),
    ),
    (
        "bridge.mjs",
        include_str!("../../../research/claude-native-lab/bridge.mjs"),
    ),
    (
        "discover.mjs",
        include_str!("../../../research/claude-native-lab/discover.mjs"),
    ),
    (
        "bun_container.mjs",
        include_str!("../../../research/claude-native-lab/bun_container.mjs"),
    ),
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    pub directory: PathBuf,
    pub cwd: PathBuf,
    pub title: String,
    #[serde(default)]
    pub lifecycle_version: u32,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub source: SessionSource,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSource {
    #[default]
    Managed,
    Native {
        configuration: PathBuf,
        conversation_id: String,
        transcript: Option<PathBuf>,
        job: Option<discovery::NativeJob>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPhase {
    Starting,
    CheckingCompatibility,
    NeedsSetup,
    Available,
    Stopped,
    Failed,
    Unavailable,
    Saved,
    NeedsAdapter,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub phase: HostPhase,
    pub message: String,
}

impl HostStatus {
    fn new(phase: HostPhase, message: impl Into<String>) -> Self {
        Self {
            phase,
            message: message.into(),
        }
    }

    pub fn can_reconnect(&self) -> bool {
        matches!(
            self.phase,
            HostPhase::Starting
                | HostPhase::CheckingCompatibility
                | HostPhase::NeedsSetup
                | HostPhase::Available
        )
    }

    pub fn sidebar_label(&self) -> &'static str {
        match self.phase {
            HostPhase::Starting => "Starting",
            HostPhase::CheckingCompatibility => "Checking compatibility",
            HostPhase::NeedsSetup => "Waiting for native setup",
            HostPhase::Available | HostPhase::Saved => "",
            HostPhase::Stopped => "Stopped",
            HostPhase::Failed => "Needs attention",
            HostPhase::Unavailable => "Unavailable",
            HostPhase::NeedsAdapter => "Not connected",
        }
    }
}

pub fn should_reconnect(status: &HostStatus, failures: usize) -> bool {
    status.can_reconnect() && (status.phase != HostPhase::Available || failures < 5)
}

#[derive(Default, Serialize)]
pub struct Catalog {
    pub sessions: Vec<Session>,
    pub conversations: Vec<Conversation>,
    pub creations: Vec<creation::Pending>,
    #[serde(skip)]
    handoffs: HashMap<String, String>,
    pub statuses: HashMap<String, HostStatus>,
    pub warnings: Vec<String>,
    pub setup: setup::Status,
    pub hidden_conversations: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub updated_at_ms: u64,
    pub aliases: Vec<String>,
    pub target: ConversationTarget,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationTarget {
    Session {
        entry: Session,
        current: Session,
    },
    Creation {
        request: creation::Pending,
        current: Option<Session>,
    },
}

impl Conversation {
    pub fn from_session(session: Session) -> Self {
        Self {
            id: session.id.clone(),
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            updated_at_ms: session.created_at_ms,
            aliases: vec![session.id.clone()],
            target: ConversationTarget::Session {
                entry: session.clone(),
                current: session,
            },
        }
    }

    pub fn from_creation(request: creation::Pending) -> Self {
        Self {
            id: request.id(),
            title: request.title(),
            cwd: request.cwd.clone(),
            updated_at_ms: request.requested_at,
            aliases: vec![request.id()],
            target: ConversationTarget::Creation {
                request,
                current: None,
            },
        }
    }

    pub fn entry(&self) -> Option<&Session> {
        match &self.target {
            ConversationTarget::Session { entry, .. } => Some(entry),
            _ => None,
        }
    }

    pub fn current(&self) -> Option<&Session> {
        match &self.target {
            ConversationTarget::Session { current, .. } => Some(current),
            ConversationTarget::Creation { current, .. } => current.as_ref(),
        }
    }

    pub fn creation(&self) -> Option<&creation::Pending> {
        match &self.target {
            ConversationTarget::Creation { request, .. } => Some(request),
            _ => None,
        }
    }

    pub fn bind(&mut self, session: Session) {
        if !self.aliases.contains(&session.id) {
            self.aliases.push(session.id.clone());
        }
        match &mut self.target {
            ConversationTarget::Session { current, .. } => *current = session,
            ConversationTarget::Creation { .. } => {
                self.target = ConversationTarget::Session {
                    entry: session.clone(),
                    current: session,
                }
            }
        }
    }

    pub fn open(&self) -> anyhow::Result<Session> {
        match &self.target {
            ConversationTarget::Session { entry, .. } if entry.is_managed() => Ok(entry.clone()),
            ConversationTarget::Session { entry, .. } => resume::continue_conversation(entry),
            ConversationTarget::Creation { request, .. } => creation::recover(&request.directory),
        }
    }
}

impl Session {
    pub fn is_managed(&self) -> bool {
        matches!(self.source, SessionSource::Managed)
    }

    pub fn saved_history(&self) -> anyhow::Result<Projection> {
        discovery::history(self)
    }

    pub fn socket(&self) -> PathBuf {
        self.directory.join("native.sock")
    }
    fn validate(&self) -> anyhow::Result<()> {
        if !self.is_managed() {
            return discovery::validate_session(self);
        }
        ensure!(
            self.lifecycle_version <= 1,
            "Session was created by a newer Harness lifecycle version"
        );
        ensure!(Uuid::parse_str(&self.id).is_ok(), "Invalid native host ID");
        ensure!(
            self.cwd.is_absolute(),
            "Native workspace must be an absolute path"
        );
        ensure!(
            self.directory.is_absolute()
                && self.directory.file_name().and_then(|name| name.to_str())
                    == Some(format!("harness-claude-{}", self.id).as_str()),
            "Native runtime directory does not match its host ID"
        );
        Ok(())
    }

    pub fn status(&self) -> HostStatus {
        self.read_status().unwrap_or_else(|error| {
            HostStatus::new(
                HostPhase::Unavailable,
                format!("Claude session unavailable: {error:#}. No new process was started."),
            )
        })
    }

    fn read_status(&self) -> anyhow::Result<HostStatus> {
        self.validate()?;
        if !self.is_managed() {
            return discovery::status(self);
        }
        validate_private_directory(&self.directory).context("Runtime directory is missing or unsafe; it may have been removed during cleanup or reboot")?;
        for name in ["host-error.txt", "host-exit.txt"] {
            if let Some(contents) = read_optional(&self.directory.join(name))? {
                return Ok(HostStatus::new(
                    if name == "host-error.txt" {
                        HostPhase::Failed
                    } else {
                        HostPhase::Stopped
                    },
                    String::from_utf8(contents)?,
                ));
            }
        }
        if self.lifecycle_version == 1 && !host_lock_held(&self.directory)? {
            let prepared = read_optional(&self.directory.join("host-started.json"))?.is_none();
            let age = now_ms().saturating_sub(self.created_at_ms);
            if prepared && self.created_at_ms <= now_ms() && age < 10_000 {
                return Ok(HostStatus::new(
                    HostPhase::Starting,
                    "Starting native Claude host…",
                ));
            }
            return Ok(HostStatus::new(
                HostPhase::Stopped,
                "Claude host is no longer running. It was not automatically restarted or resumed.",
            ));
        }
        let preload = read_optional(&self.directory.join("preload-status.json"))?
            .map(|contents| serde_json::from_slice::<Value>(&contents))
            .transpose()?;
        if let Some(status) = &preload
            && matches!(
                status["state"].as_str(),
                Some("error" | "unsupported" | "not-mounted")
            )
        {
            return Ok(HostStatus::new(
                HostPhase::Failed,
                format!(
                    "Could not connect to Claude: {}. Its process was not restarted.",
                    status
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("Native UI did not mount before adapter discovery stopped")
                ),
            ));
        }
        // Older hosts have no lifetime lock. A leftover socket pathname is not
        // proof of a live listener, so only a successful connection can vouch for it.
        if self.lifecycle_version == 0 {
            if socket_accepts(&self.socket()) {
                return Ok(HostStatus::new(
                    HostPhase::Available,
                    "Existing native Claude host is available",
                ));
            }
            if socket_accepts(&self.directory.join("terminal.sock")) {
                return Ok(HostStatus::new(
                    HostPhase::NeedsSetup,
                    "Claude is waiting for sign-in or setup that Harness cannot complete yet",
                ));
            }
            return Ok(HostStatus::new(
                HostPhase::Unavailable,
                "Legacy native host is not reachable. No process was started; its conversation is still owned by Claude's native history.",
            ));
        }
        if preload
            .as_ref()
            .is_some_and(|status| status["state"] == "attached")
        {
            return Ok(HostStatus::new(
                HostPhase::Available,
                "Native Claude host is running",
            ));
        }
        if let Some(contents) = read_optional(&self.directory.join("host-state.json"))? {
            return Ok(serde_json::from_slice(&contents)?);
        }
        Ok(HostStatus::new(
            HostPhase::Starting,
            "Starting native Claude host…",
        ))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    // The sockets can approve tools; a shared or substituted directory is not safe.
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Claude session directory must be private and owned by you: {}",
        path.display()
    );
    Ok(())
}

fn read_optional(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Unsafe session file: {}",
        path.display()
    );
    let mut contents = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut contents)?;
    ensure!(
        contents.len() <= 1024 * 1024,
        "Session file is too large: {}",
        path.display()
    );
    Ok(Some(contents))
}

fn try_host_lock(file: &File) -> anyhow::Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error.into())
    }
}

fn probe_host_lock(file: &File) -> anyhow::Result<bool> {
    if !try_host_lock(file)? {
        return Ok(true);
    }
    // A concurrent fork can retain this open file description until exec.
    // A read-only status probe must not leave that child looking like an owner.
    file.unlock()?;
    Ok(false)
}

fn host_lock_held(directory: &Path) -> anyhow::Result<bool> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("host.lock"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(file.metadata()?.is_file(), "Invalid host lock");
    probe_host_lock(&file)
}

fn socket_accepts(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_socket()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
    }) && smol::block_on(with_timeout(
        async { Ok(smol::net::unix::UnixStream::connect(path).await?) },
        Duration::from_millis(250),
        "Legacy native socket probe timed out",
    ))
    .is_ok()
}

fn root() -> anyhow::Result<PathBuf> {
    let root = dirs::data_local_dir()
        .context("No local data directory")?
        .join("harness/claude");
    private_directory(&root)?;
    Ok(root)
}

pub fn sessions() -> anyhow::Result<Catalog> {
    let mut catalog = read_catalog(&root()?)?;
    catalog.setup = setup::status();
    if let Err(error) = discovery::extend_catalog(&mut catalog) {
        catalog.warnings.push(format!(
            "Could not discover native Claude history: {error:#}"
        ));
    }
    catalog.sessions.sort_by(|left, right| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    discovery::group_conversations(&mut catalog);
    if let Err(error) = creation::extend_catalog(&mut catalog) {
        catalog
            .warnings
            .push(format!("Could not read Claude startup requests: {error:#}"));
    }
    match discovery::hidden_conversations() {
        Ok(hidden) => catalog.hidden_conversations = hidden,
        Err(error) => catalog.warnings.push(format!(
            "Could not read hidden Claude conversations: {error:#}"
        )),
    }
    Ok(catalog)
}

fn read_catalog(directory: &Path) -> anyhow::Result<Catalog> {
    validate_private_directory(directory)?;
    let mut catalog = Catalog::default();
    for entry in fs::read_dir(directory)? {
        let result = (|| -> anyhow::Result<Option<Session>> {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                return Ok(None);
            }
            let session: Session = serde_json::from_slice(
                &read_optional(&path)?.context("Session record disappeared")?,
            )
            .with_context(|| format!("Invalid session record {}", path.display()))?;
            session.validate()?;
            ensure!(
                session.is_managed(),
                "Unexpected source in managed session catalog"
            );
            ensure!(
                path.file_stem().and_then(|name| name.to_str()) == Some(session.id.as_str()),
                "Session filename does not match its ID: {}",
                path.display()
            );
            Ok(Some(session))
        })();
        match result {
            Ok(Some(session)) => {
                catalog
                    .statuses
                    .insert(session.id.clone(), session.status());
                catalog.sessions.push(session);
            }
            Ok(None) => {}
            Err(error) => catalog
                .warnings
                .push(format!("Skipped Claude session record: {error:#}")),
        }
    }
    catalog.sessions.sort_by(|left, right| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(catalog)
}

fn write_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn publish_file(path: &Path, bytes: &[u8], replace: bool) -> anyhow::Result<()> {
    let directory = path.parent().context("Missing session directory")?;
    let temporary = directory.join(format!(".pending-{}", Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        write_new(&temporary, bytes)?;
        if replace {
            fs::rename(&temporary, path)?;
        } else {
            fs::hard_link(&temporary, path)?;
        }
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = fs::remove_file(&temporary) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "Could not remove incomplete session record {}: {error}",
                temporary.display()
            );
        }
    }
    result
}

fn record_state(
    directory: &Path,
    phase: HostPhase,
    message: impl Into<String>,
) -> anyhow::Result<()> {
    publish_file(
        &directory.join("host-state.json"),
        &serde_json::to_vec(&HostStatus::new(phase, message))?,
        true,
    )
}

pub fn start(cwd: PathBuf) -> anyhow::Result<Session> {
    creation::start(cwd)
}

#[cfg(test)]
fn start_with(
    cwd: PathBuf,
    catalog: &Path,
    runtime_root: &Path,
    launch: impl FnOnce(&Session) -> anyhow::Result<()>,
) -> anyhow::Result<Session> {
    ensure!(cwd.is_dir(), "Choose a project folder");
    validate_private_directory(catalog)?;
    let id = Uuid::new_v4().to_string();
    // Keep Unix socket addresses short even with a long XDG data directory.
    let directory = runtime_root.join(format!("harness-claude-{id}"));
    private_directory(&directory)?;
    let session = Session {
        title: cwd
            .file_name()
            .unwrap_or(cwd.as_os_str())
            .to_string_lossy()
            .into_owned(),
        id,
        directory,
        cwd: fs::canonicalize(cwd)?,
        lifecycle_version: 1,
        created_at_ms: now_ms(),
        source: SessionSource::Managed,
    };
    for (name, contents) in ASSETS {
        write_new(&session.directory.join(name), contents.as_bytes())?;
    }
    let record = serde_json::to_vec(&session)?;
    publish_file(&session.directory.join("session.json"), &record, false)?;
    record_state(
        &session.directory,
        HostPhase::Starting,
        "Preparing native Claude host…",
    )?;
    // Publish discovery before any child can exist. A failed write cannot leave
    // an invisible running session, and a failed spawn stays diagnosable.
    publish_file(
        &catalog.join(format!("{}.json", session.id)),
        &record,
        false,
    )?;
    if let Err(error) = launch(&session) {
        if let Err(record_error) = publish_file(
            &session.directory.join("host-error.txt"),
            format!("Could not start native host: {error:#}").as_bytes(),
            false,
        ) {
            return Err(error).context(format!(
                "Also could not record startup failure: {record_error:#}"
            ));
        }
        return Err(error);
    }
    Ok(session)
}

fn native_binary() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("HARNESS_CLAUDE_BINARY") {
        return Ok(PathBuf::from(path));
    }
    let directory = dirs::home_dir()
        .context("No home directory")?
        .join(".local/share/claude/versions");
    let mut versions = Vec::new();
    for entry in fs::read_dir(directory)
        .context("Native Claude not found; set HARNESS_CLAUDE_BINARY to its executable")?
    {
        let entry = entry?;
        if let Ok(version) = semver::Version::parse(&entry.file_name().to_string_lossy()) {
            versions.push((version, entry.path()));
        }
    }
    versions.sort_by(|left, right| left.0.cmp(&right.0));
    versions
        .pop()
        .map(|(_, path)| path)
        .context("No native Claude version found")
}

pub fn host(directory: &Path) -> anyhow::Result<()> {
    validate_private_directory(directory)?;
    let session: Session = serde_json::from_slice(
        &read_optional(&directory.join("session.json"))?.context("Missing host session record")?,
    )?;
    session.validate()?;
    ensure!(
        session.is_managed(),
        "Only a managed host record can launch a native process"
    );
    ensure!(
        session.directory == directory,
        "Host directory differs from the recorded session"
    );
    let lifetime_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("host.lock"))?;
    ensure!(
        lifetime_lock.metadata()?.is_file(),
        "Invalid native host lock"
    );
    ensure!(
        try_host_lock(&lifetime_lock)?,
        "This native host is already running; reconnect instead of launching another owner"
    );
    // A duplicate launch must not overwrite the original host's failure/state files.
    publish_file(
        &directory.join("host-started.json"),
        &serde_json::to_vec(&json!({"pid":std::process::id(),"started_at_ms":now_ms()}))?,
        false,
    )
    .context("This host has already been started. Automatic restart/resume is not supported")?;
    let result = host_inner(directory);
    if let Err(error) = &result {
        publish_file(
            &directory.join("host-error.txt"),
            format!("Could not start Claude: {error:#}").as_bytes(),
            false,
        )
        .with_context(|| format!("Could not record original failure: {error:#}"))?;
    }
    result
}

fn host_inner(directory: &Path) -> anyhow::Result<()> {
    let session: Session = serde_json::from_slice(&fs::read(directory.join("session.json"))?)?;
    let binary = native_binary()?;
    record_state(
        directory,
        HostPhase::CheckingCompatibility,
        format!("Checking native Claude compatibility: {}", binary.display()),
    )?;
    let mut discovery = async_process::Command::new("node");
    discovery
        .arg("--max-old-space-size=512")
        .arg(directory.join("discover.mjs"))
        .arg(&binary)
        .kill_on_drop(true);
    let discovery = smol::block_on(with_timeout(
        async {
            Ok(discovery
                .output()
                .await
                .context("Node.js is required to check the native Claude adapter")?)
        },
        Duration::from_secs(20),
        "Claude compatibility check timed out; its checker was cancelled",
    ))?;
    ensure!(
        discovery.status.success(),
        "Claude compatibility check failed: {}",
        String::from_utf8_lossy(&discovery.stderr)
    );
    let specification: Value = serde_json::from_slice(&discovery.stdout)?;
    ensure!(
        specification["verified"] == true,
        "Claude at {} (SHA-256 {}) has not passed the native adapter checks. No Claude process was launched and no patch was applied. Use the ordinary terminal until this build is verified; Harness has not silently selected an older release.",
        binary.display(),
        specification["sha256"]
    );
    ensure!(
        std::env::var_os("BUN_OPTIONS").is_none(),
        "Existing BUN_OPTIONS must be cleared explicitly before launching native Claude"
    );
    let preload = directory.join("preload.mjs");
    ensure!(
        !preload.to_string_lossy().contains(char::is_whitespace),
        "Native preload path contains whitespace"
    );
    let pair = portable_pty::native_pty_system().openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(binary);
    command.cwd(&session.cwd);
    command.args(["--settings", "{\"remoteControlAtStartup\":false}"]);
    command.env("BUN_OPTIONS", format!("--preload {}", preload.display()));
    command.env(
        "HARNESS_CLAUDE_PRELOAD_SPEC",
        serde_json::to_string(&specification)?,
    );
    command.env(
        "HARNESS_CLAUDE_PRELOAD_STATUS",
        directory.join("preload-status.json"),
    );
    command.env("HARNESS_CLAUDE_BRIDGE_MODULE", directory.join("bridge.mjs"));
    command.env("HARNESS_CLAUDE_BRIDGE_SOCKET", session.socket());
    command.env("TERM", "xterm-256color");
    for name in ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "LD_PRELOAD"] {
        command.env_remove(name);
    }
    // Finish fallible terminal setup before spawning Claude. Any later failure
    // must terminate/reap only the child this host just created.
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    let mut reader = pair.master.try_clone_reader()?;
    let output = Arc::new(Mutex::new(VecDeque::<u8>::new()));
    let clients = Arc::new(Mutex::new(Vec::<UnixStream>::new()));
    let listener = UnixListener::bind(directory.join("terminal.sock"))?;
    fs::set_permissions(
        directory.join("terminal.sock"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    let mut child = NativeChild::new(pair.slave.spawn_command(command)?);
    drop(pair.slave);
    let master = Arc::new(Mutex::new(pair.master));
    record_state(
        directory,
        HostPhase::NeedsSetup,
        "Claude is starting or waiting for sign-in/setup. No prompt has been sent.",
    )?;
    std::thread::spawn({
        let output = output.clone();
        let clients = clients.clone();
        move || {
            let mut buffer = [0; 8192];
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) => {
                        log::warn!("Claude PTY read: {error}");
                        break;
                    }
                };
                let Ok(mut output) = output.lock() else {
                    break;
                };
                output.extend(buffer.iter().take(count).copied());
                while output.len() > 256 * 1024 {
                    output.pop_front();
                }
                let Ok(mut clients) = clients.lock() else {
                    break;
                };
                let frame = terminal_frame(&buffer[..count]);
                clients.retain_mut(|client| client.write_all(frame.as_bytes()).is_ok());
            }
        }
    });
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let result = (|| -> anyhow::Result<()> {
                let mut client = incoming?;
                client.set_write_timeout(Some(Duration::from_millis(100)))?;
                {
                    let output = output
                        .lock()
                        .map_err(|_| anyhow::anyhow!("PTY output lock poisoned"))?;
                    let mut clients = clients
                        .lock()
                        .map_err(|_| anyhow::anyhow!("PTY clients lock poisoned"))?;
                    client.write_all(
                        terminal_frame(&output.iter().copied().collect::<Vec<_>>()).as_bytes(),
                    )?;
                    clients.push(client.try_clone()?);
                }
                let writer = writer.clone();
                let master = master.clone();
                std::thread::spawn(move || {
                    let result = (|| -> anyhow::Result<()> {
                        let mut reader = std::io::BufReader::new(client);
                        loop {
                            let mut line = String::new();
                            if reader.by_ref().take(1024 * 1024).read_line(&mut line)? == 0 {
                                break;
                            }
                            ensure!(line.ends_with('\n'), "Oversized terminal input");
                            let request: Value = serde_json::from_str(&line)?;
                            if let Some(data) = request["data"].as_str() {
                                let data =
                                    base64::engine::general_purpose::STANDARD.decode(data)?;
                                writer
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("PTY writer lock poisoned"))?
                                    .write_all(&data)?;
                            }
                            if let (Some(rows), Some(cols)) =
                                (request["rows"].as_u64(), request["cols"].as_u64())
                            {
                                master
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("PTY size lock poisoned"))?
                                    .resize(PtySize {
                                        rows: rows.clamp(1, 500) as u16,
                                        cols: cols.clamp(1, 1000) as u16,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    })?;
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        log::warn!("Claude terminal client: {error:#}");
                    }
                });
                Ok(())
            })();
            if let Err(error) = result {
                log::warn!("Claude terminal attach: {error:#}");
            }
        }
    });
    let status = child.wait()?;
    publish_file(
        &directory.join("host-exit.txt"),
        format!("Native Claude exited ({status}). Start a new session to continue.").as_bytes(),
        false,
    )?;
    Ok(())
}

fn terminal_frame(bytes: &[u8]) -> String {
    format!(
        "{}\n",
        json!({"data": base64::engine::general_purpose::STANDARD.encode(bytes)})
    )
}

struct NativeChild {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reaped: bool,
}

impl NativeChild {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn wait(&mut self) -> anyhow::Result<portable_pty::ExitStatus> {
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        if !self.reaped {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => log::warn!("Could not inspect failed native startup child: {error}"),
            }
            if let Err(error) = self.child.kill() {
                log::error!("Could not terminate failed native startup child: {error}");
                return;
            }
            if let Err(error) = self.child.wait() {
                log::error!("Could not reap failed native startup child: {error}");
            }
        }
    }
}

async fn with_timeout<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    timeout: Duration,
    message: &str,
) -> anyhow::Result<T> {
    futures::pin_mut!(future);
    match futures::future::select(future, Box::pin(smol::Timer::after(timeout))).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => bail!("{message}"),
    }
}

pub fn job_terminal(identifier: &str) -> anyhow::Result<()> {
    let session = sessions()?
        .sessions
        .into_iter()
        .find(|session| session.id == identifier)
        .context("Native conversation is no longer discoverable; no terminal was attached")?;
    let mut command = native_terminal_command(&session, &native_binary()?)?;
    Err(command.exec()).context("Could not open Claude's native terminal")
}

fn native_terminal_command(session: &Session, executable: &Path) -> anyhow::Result<Command> {
    session.validate()?;
    let SessionSource::Native {
        configuration,
        job: Some(job),
        ..
    } = &session.source
    else {
        bail!("This conversation has no native background job to attach");
    };
    ensure!(
        job.kind == "background",
        "Cannot attach an ordinary terminal; run /bg there first"
    );
    // This is an explicitly requested native-terminal action, not Harness's
    // verified opening path: stock attach may wake a stopped conversation.
    let mut command = Command::new(executable);
    command
        .args(["attach", &job.id])
        .current_dir(&session.cwd)
        .env("CLAUDE_CONFIG_DIR", configuration)
        .env("DISABLE_AUTOUPDATER", "1");
    for name in [
        "LD_PRELOAD",
        "BUN_OPTIONS",
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_PROCESS_WRAPPER",
    ] {
        command.env_remove(name);
    }
    Ok(command)
}

pub fn terminal(directory: &Path) -> anyhow::Result<()> {
    validate_private_directory(directory)?;
    let mut client = UnixStream::connect(directory.join("terminal.sock"))?;
    // Preserve the caller's terminal settings even if the remote PTY disconnects.
    struct RawTerminal(libc::termios);
    impl Drop for RawTerminal {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.0);
            }
        }
    }
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    ensure!(
        unsafe { libc::tcgetattr(0, original.as_mut_ptr()) } == 0,
        "Open native Claude from a terminal"
    );
    let original = unsafe { original.assume_init() };
    let _restore = RawTerminal(original);
    let mut raw = original;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    ensure!(
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } == 0,
        "Could not enter terminal raw mode"
    );
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } < 0 {
        log::warn!("Could not read terminal dimensions");
    }
    writeln!(client, "{}", json!({"rows":size.ws_row,"cols":size.ws_col}))?;
    let mut input = client.try_clone()?;
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        loop {
            match std::io::stdin().read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if buffer[..count].contains(&0x1d) {
                        break;
                    }
                    if let Err(error) = input.write_all(terminal_frame(&buffer[..count]).as_bytes())
                    {
                        log::warn!("Native terminal input: {error}");
                        break;
                    }
                }
                Err(error) => {
                    log::warn!("Terminal input: {error}");
                    break;
                }
            }
        }
        if let Err(error) = input.shutdown(std::net::Shutdown::Both) {
            log::warn!("Terminal detach: {error}");
        }
    });
    let mut reader = std::io::BufReader::new(client);
    loop {
        let mut line = String::new();
        if reader.by_ref().take(MAX_FRAME).read_line(&mut line)? == 0 {
            break;
        }
        ensure!(line.ends_with('\n'), "Oversized terminal frame");
        let frame: Value = serde_json::from_str(&line)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(frame["data"].as_str().context("Missing terminal data")?)?;
        std::io::stdout().write_all(&bytes)?;
        std::io::stdout().flush()?;
    }
    Ok(())
}

pub async fn read_frame(
    reader: &mut futures::io::BufReader<smol::net::unix::UnixStream>,
) -> anyhow::Result<Value> {
    use futures::io::AsyncReadExt as _;
    let mut line = String::new();
    let count = (&mut *reader).take(MAX_FRAME).read_line(&mut line).await?;
    ensure!(count > 0, "Native Claude disconnected");
    ensure!(line.ends_with('\n'), "Native Claude frame exceeds 32 MiB");
    Ok(serde_json::from_str(&line)?)
}

pub async fn connection(
    session: &Session,
) -> anyhow::Result<futures::io::BufReader<smol::net::unix::UnixStream>> {
    session.validate()?;
    if !session.is_managed() {
        return with_timeout(
            discovery::connection(session),
            Duration::from_secs(5),
            "Native worker handshake timed out; no process was started",
        )
        .await;
    }
    validate_private_directory(&session.directory)?;
    with_timeout(
        async {
            Ok(futures::io::BufReader::new(
                smol::net::unix::UnixStream::connect(session.socket()).await?,
            ))
        },
        Duration::from_secs(5),
        "Connecting to the native socket timed out",
    )
    .await
}

pub async fn snapshot_connection(
    session: &Session,
) -> anyhow::Result<(futures::io::BufReader<smol::net::unix::UnixStream>, Value)> {
    with_timeout(
        async {
            if !session.is_managed() {
                return discovery::snapshot_connection(session).await;
            }
            let mut reader = connection(session).await?;
            reader
                .get_mut()
                .write_all(b"{\"id\":\"snapshot\",\"method\":\"snapshot\"}\n")
                .await?;
            loop {
                let frame = read_frame(&mut reader).await?;
                if frame["id"] == "snapshot" {
                    return Ok((reader, frame));
                }
            }
        },
        Duration::from_secs(10),
        "Native host accepted a connection but did not provide a snapshot; no prompt was sent",
    )
    .await
}

pub async fn request(session: &Session, mut request: Value) -> anyhow::Result<Value> {
    if let SessionSource::Native {
        conversation_id, ..
    } = &session.source
    {
        ensure!(
            request["sessionId"].as_str() == Some(conversation_id)
                && request["epoch"]
                    .as_str()
                    .is_some_and(|epoch| Uuid::parse_str(epoch).is_ok()),
            "Native actions require a current conversation and adapter epoch; reconnect first"
        );
    }
    with_timeout(async {
        let mut reader = connection(session).await?;
        request["id"] = json!("harness-action");
        reader.get_mut().write_all(format!("{request}\n").as_bytes()).await?;
        loop {
            let frame = read_frame(&mut reader).await?;
            if frame["id"] == "harness-action" {
                if let Some(error) = frame["error"].as_str() {
                    bail!("{error}");
                }
                return Ok(frame["result"].clone());
            }
        }
    }, Duration::from_secs(15), "Native request timed out; delivery is uncertain. Check the transcript before sending again.").await
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSettings {
    pub model: String,
    pub effort: Value,
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelChoice>,
    #[serde(default)]
    pub permissions: Vec<PermissionChoice>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    pub value: String,
    pub display_name: String,
    #[serde(default)]
    pub supported_effort_levels: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PermissionChoice {
    pub value: String,
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Default)]
pub struct Projection {
    pub messages: Vec<Value>,
    pub dialogs: Vec<Value>,
    pub active: bool,
    pub ready: bool,
    pub durable_submissions: bool,
    pub checked_dialog_replies: bool,
    pub epoch: String,
    pub session_id: String,
    pub model: String,
    pub settings: Option<SessionSettings>,
    pub settings_controls: bool,
    streaming_id: String,
    streaming: BTreeMap<u64, Value>,
    sequence: Option<u64>,
}

impl Projection {
    pub fn disconnect(&mut self) {
        self.ready = false;
        self.durable_submissions = false;
        self.checked_dialog_replies = false;
        self.settings_controls = false;
        self.active = false;
        self.streaming.clear();
        self.dialogs.clear();
        self.sequence = None;
    }

    pub fn apply(&mut self, frame: Value) -> anyhow::Result<()> {
        if frame["id"] == "snapshot" {
            ensure!(
                frame.get("error").is_none(),
                "Native snapshot failed: {}",
                frame["error"]
            );
            let snapshot = &frame["result"];
            self.messages = snapshot["messages"]
                .as_array()
                .cloned()
                .context("Native snapshot has no messages")?;
            self.dialogs = snapshot["dialogs"]
                .as_array()
                .cloned()
                .context("Native snapshot has no dialogs")?;
            self.epoch = text(snapshot, "epoch");
            self.session_id = text(snapshot, "sessionId");
            self.durable_submissions =
                snapshot["capabilities"]["durableSubmissionDeduplication"] == 1;
            self.checked_dialog_replies = snapshot["capabilities"]["checkedDialogReplies"] == 1;
            self.settings_controls = snapshot["capabilities"]["sessionSettings"] == 1;
            self.settings = snapshot
                .get("settings")
                .filter(|settings| !settings.is_null())
                .map(|settings| serde_json::from_value(settings.clone()))
                .transpose()
                .context("Invalid Claude settings")?;
            self.sequence = snapshot["sequence"].as_u64();
            self.set_turn(&snapshot["turn"]);
            self.streaming.clear();
            self.ready = true;
            return Ok(());
        }
        if !self.ready {
            return Ok(());
        }
        if let Some(epoch) = frame["epoch"].as_str() {
            ensure!(epoch == self.epoch, "Native adapter changed; reconnecting");
        }
        if frame.get("event").is_some()
            && let Some(previous) = self.sequence
        {
            let sequence = frame["sequence"]
                .as_u64()
                .context("Native event has no cursor; reconnecting")?;
            ensure!(
                previous.checked_add(1) == Some(sequence),
                "Native event stream has a gap or duplicate; reconnecting from a fresh snapshot"
            );
            self.sequence = Some(sequence);
        }
        let data = &frame["data"];
        match frame["event"].as_str() {
            Some("settings") => {
                self.settings = Some(
                    serde_json::from_value(data.clone())
                        .context("Invalid Claude settings event")?,
                );
            }
            Some("transcript") => {
                self.messages = data
                    .as_array()
                    .cloned()
                    .context("Invalid native transcript")?
            }
            Some("turn") => self.set_turn(data),
            Some("dialogs") => {
                self.dialogs = data.as_array().cloned().context("Invalid native dialogs")?
            }
            Some("session_changed") => bail!("Native session changed; reconnecting"),
            Some("adapter_error") => bail!("Native adapter: {data}"),
            Some("engine_event") if data["type"] == "stream_event" => {
                if data
                    .get("parent_tool_use_id")
                    .is_some_and(|parent| !parent.is_null())
                    || data["session_id"].as_str().is_some_and(|session| {
                        !self.session_id.is_empty() && session != self.session_id
                    })
                {
                    return Ok(());
                }
                let event = &data["event"];
                match event["type"].as_str() {
                    Some("message_start") => {
                        self.streaming.clear();
                        self.streaming_id = text(&event["message"], "id");
                        self.model = text(&event["message"], "model");
                    }
                    Some("content_block_start") => {
                        self.streaming.insert(
                            event["index"].as_u64().unwrap_or(0),
                            event["content_block"].clone(),
                        );
                    }
                    Some("content_block_delta") => {
                        if let Some(block) = event["index"]
                            .as_u64()
                            .and_then(|index| self.streaming.get_mut(&index))
                        {
                            for field in ["text", "thinking"] {
                                if let Some(delta) = event["delta"][field].as_str() {
                                    block[field] = json!(format!(
                                        "{}{delta}",
                                        block[field].as_str().unwrap_or_default()
                                    ));
                                }
                            }
                            if let Some(delta) = event["delta"]["partial_json"].as_str() {
                                block["partial_input"] = json!(format!(
                                    "{}{delta}",
                                    block["partial_input"].as_str().unwrap_or_default()
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn set_turn(&mut self, turn: &Value) {
        self.active = ["isLoading", "isQueryActive", "isExternalLoading"]
            .iter()
            .any(|field| turn[*field] == true);
        if !self.active {
            self.streaming.clear();
        }
    }

    pub fn items(&self) -> Vec<TranscriptItem> {
        let mut items = Vec::new();
        let mut tools: HashMap<String, usize> = HashMap::new();
        let mut local_command_end = 0;
        let current_turn_start = self
            .messages
            .iter()
            .rposition(|message| {
                message["type"] == "user"
                    && message["isMeta"] != true
                    && (message["message"]["content"].is_string()
                        || message["message"]["content"]
                            .as_array()
                            .is_some_and(|blocks| {
                                blocks.iter().any(|block| block["type"] == "text")
                            }))
            })
            .unwrap_or(0);
        for (message_index, message) in self.messages.iter().enumerate() {
            if message_index < local_command_end {
                continue;
            }
            let kind = text(message, "type");
            let identifier = message["uuid"]
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("message-{message_index}"));
            let message_key = format!("claude:{}:{identifier}", self.session_id);
            if let Some((entry, count)) =
                local_command_item(&self.messages[message_index..], message_key.clone())
            {
                local_command_end = message_index + count;
                items.push(entry);
                continue;
            }
            if kind == "system" {
                let subtype = text(message, "subtype");
                if subtype == "turn_duration" {
                    continue;
                }
                if subtype == "away_summary" {
                    let content = text(message, "content");
                    let display = content
                        .trim()
                        .strip_suffix("(disable recaps in /config)")
                        .unwrap_or(content.trim())
                        .trim_end()
                        .to_owned();
                    let mut recap = item(
                        message_key,
                        TranscriptKind::Trace,
                        "While you were away",
                        display,
                        json!({"provider":"claude", "type":"sessionRecap", "native":message}),
                    );
                    recap.expanded = true;
                    items.push(recap);
                    continue;
                }
                let compaction = subtype == "compact_boundary";
                let error = matches!(subtype.as_str(), "api_error" | "error");
                let mut entry = item(
                    message_key,
                    if error {
                        TranscriptKind::Error
                    } else if compaction {
                        TranscriptKind::Trace
                    } else {
                        TranscriptKind::Tool
                    },
                    if compaction {
                        "Context compacted"
                    } else if error {
                        "Claude error"
                    } else {
                        "Claude session event"
                    },
                    message["content"]
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| pretty(message)),
                    json!({"provider":"claude", "native":message, "type":if compaction {"contextCompaction"} else {"nativeSystem"}}),
                );
                entry.expanded = error;
                items.push(entry);
                continue;
            }
            if !matches!(kind.as_str(), "user" | "assistant") {
                continue;
            }
            if message["isApiErrorMessage"] == true {
                let mut entry = item(
                    message_key,
                    TranscriptKind::Error,
                    "Claude error",
                    block_text(&message["message"]["content"]),
                    json!({"provider":"claude","native":message}),
                );
                entry.status = Some("failed".into());
                entry.expanded = true;
                items.push(entry);
                continue;
            }
            let content = &message["message"]["content"];
            if let Some(content) = content.as_str() {
                items.push(item(
                    message_key,
                    if kind == "user" {
                        TranscriptKind::User
                    } else {
                        TranscriptKind::Agent
                    },
                    if kind == "user" { "You" } else { "Claude" },
                    content.to_owned(),
                    message.clone(),
                ));
            } else if let Some(blocks) = content.as_array() {
                for (index, block) in blocks.iter().enumerate() {
                    let key = format!("claude:{}:{identifier}:{index}", self.session_id);
                    match block["type"].as_str() {
                        Some("text" | "thinking") => {
                            let thinking = block["type"] == "thinking";
                            let kind = if thinking {
                                TranscriptKind::Reasoning
                            } else if kind == "user" {
                                TranscriptKind::User
                            } else {
                                TranscriptKind::Agent
                            };
                            items.push(item(
                                key,
                                kind,
                                if thinking {
                                    "Thinking"
                                } else if kind == TranscriptKind::User {
                                    "You"
                                } else {
                                    "Claude"
                                },
                                text(block, if thinking { "thinking" } else { "text" }),
                                message.clone(),
                            ));
                        }
                        Some("tool_use") => {
                            let mut entry = tool_item(
                                format!("claude:{}:tool:{}", self.session_id, text(block, "id")),
                                block,
                            );
                            entry.status = Some(
                                if self.active && message_index >= current_turn_start {
                                    "inProgress"
                                } else {
                                    "result unavailable"
                                }
                                .into(),
                            );
                            tools.insert(text(block, "id"), items.len());
                            items.push(entry);
                        }
                        Some("tool_result") => {
                            // Parallel blocks share one envelope. Only associate its structured
                            // result when there is exactly one invocation result to identify it.
                            let metadata = if blocks
                                .iter()
                                .filter(|block| block["type"] == "tool_result")
                                .count()
                                == 1
                            {
                                message
                                    .get("toolUseResult")
                                    .or_else(|| message.get("tool_use_result"))
                                    .unwrap_or(&Value::Null)
                            } else {
                                &Value::Null
                            };
                            if let Some(entry) = tools
                                .get(&text(block, "tool_use_id"))
                                .and_then(|index| items.get_mut(*index))
                            {
                                finish_tool(entry, block, metadata);
                            } else {
                                let mut entry = item(
                                    key,
                                    TranscriptKind::Tool,
                                    "Claude tool result · invocation unavailable",
                                    block_text(&block["content"]),
                                    json!({"provider":"claude","nativeResult":block,"nativeToolResult":metadata}),
                                );
                                entry.status = Some(
                                    if block["is_error"] == true {
                                        "failed"
                                    } else {
                                        "result received"
                                    }
                                    .into(),
                                );
                                items.push(entry);
                            }
                        }
                        Some("image") => items.push(item(
                            key,
                            TranscriptKind::Tool,
                            "Image attachment",
                            "View this attachment in Native terminal".into(),
                            block.clone(),
                        )),
                        Some("redacted_thinking") => {}
                        _ => items.push(item(
                            key,
                            TranscriptKind::Tool,
                            "Claude content · native presentation unavailable",
                            pretty(block),
                            json!({"provider":"claude","native":block}),
                        )),
                    }
                }
            }
        }
        if self.active {
            for (index, block) in &self.streaming {
                // Native Claude commits individual blocks of one API message.
                // Committing its thinking must not hide a still-streaming answer.
                if self.messages.iter().any(|message| {
                    message["message"]["id"] == self.streaming_id
                        && message
                            .get("apiBlockIndex")
                            .and_then(Value::as_u64)
                            .map_or_else(
                                || {
                                    message["message"]["content"]
                                        .as_array()
                                        .is_some_and(|blocks| blocks.get(*index as usize).is_some())
                                },
                                |committed_index| committed_index == *index,
                            )
                }) {
                    continue;
                }
                if block["type"] == "tool_use" {
                    if tools.contains_key(&text(block, "id")) {
                        continue;
                    }
                    let mut block = block.clone();
                    if let Some(partial) = block["partial_input"].as_str() {
                        block["input"] = serde_json::from_str::<Value>(partial)
                            .unwrap_or_else(|_| json!({"receivingInput":partial}));
                    }
                    let mut entry = tool_item(
                        format!("claude:{}:tool:{}", self.session_id, text(&block, "id")),
                        &block,
                    );
                    entry.status = Some("receiving input".into());
                    items.push(entry);
                    continue;
                }
                let thinking = block["type"] == "thinking";
                if block["type"] != "text" && !thinking {
                    continue;
                }
                let mut entry = item(
                    format!(
                        "claude:{}:stream:{}:{index}",
                        self.session_id, self.streaming_id
                    ),
                    if thinking {
                        TranscriptKind::Reasoning
                    } else {
                        TranscriptKind::Agent
                    },
                    if thinking { "Thinking" } else { "Claude" },
                    text(block, if thinking { "thinking" } else { "text" }),
                    block.clone(),
                );
                entry.status = Some("streaming".into());
                items.push(entry);
            }
        }
        for dialog in &self.dialogs {
            let payload = &dialog["payload"];
            let questions = self
                .checked_dialog_replies
                .then(|| native_questions(dialog).ok())
                .flatten();
            let permission = supports_permission(dialog);
            let mut entry = item(
                format!("claude:{}:dialog:{}", self.session_id, text(dialog, "id")),
                TranscriptKind::Approval,
                if questions.is_some() {
                    "Input requested"
                } else if permission {
                    "Claude permission"
                } else {
                    "This Claude request isn't supported in Harness yet"
                },
                format!(
                    "{}\n{}\n{}",
                    text(payload, "title"),
                    text(payload, "subtitle"),
                    serde_json::to_string_pretty(&payload["input"]).unwrap_or_default()
                ),
                json!({"provider":"claude", "native":dialog, "command":payload["input"]["command"], "reason":payload["permissionResult"]["message"], "input":payload["input"]}),
            );
            if let Some(questions) = questions {
                entry.raw["questions"] = json!(questions);
                entry.pending_request = Some(PendingRequest {
                    id: dialog["id"].clone(),
                    method: "claude/questions".into(),
                    resolved: false,
                });
            } else if permission {
                entry.pending_request = Some(PendingRequest {
                    id: dialog["id"].clone(),
                    method: "claude/permission".into(),
                    resolved: false,
                });
            }
            entry.expanded = true;
            items.push(entry);
        }
        items
    }
}

fn take_native_tag<'a>(text: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let content = text.trim_start().strip_prefix(&format!("<{name}>"))?;
    let (value, rest) = content.split_once(&format!("</{name}>"))?;
    Some((value, rest))
}

fn local_command_item(messages: &[Value], key: String) -> Option<(TranscriptItem, usize)> {
    let caveat = messages.first()?;
    if caveat["type"] != "user" || caveat["isMeta"] != true {
        return None;
    }
    let (_, remainder) = take_native_tag(
        caveat["message"]["content"].as_str()?,
        "local-command-caveat",
    )?;
    if !remainder.trim().is_empty() {
        return None;
    }
    let command = messages.get(1)?;
    if command["type"] != "user" {
        return None;
    }
    let (name, remainder) =
        take_native_tag(command["message"]["content"].as_str()?, "command-name")?;
    if !name.starts_with('/') || name.contains(char::is_whitespace) {
        return None;
    }
    let (_, remainder) = take_native_tag(remainder, "command-message")?;
    let (arguments, remainder) = take_native_tag(remainder, "command-args")?;
    if !remainder.trim().is_empty() {
        return None;
    }
    let mut count = 2;
    let mut body = arguments.to_owned();
    if let Some(output) = messages.get(count)
        && output["type"] == "user"
        && let Some(content) = output["message"]["content"].as_str()
        && let Some((stdout, remainder)) = take_native_tag(content, "local-command-stdout")
        && remainder.trim().is_empty()
    {
        count += 1;
        if !stdout.is_empty() && stdout != "(no content)" {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(stdout);
        }
    }
    if let Some(response) = messages.get(count)
        && response["type"] == "assistant"
        && response["message"]["model"] == "<synthetic>"
        && response["message"]["content"]
            == json!([{"type":"text","text":"No response requested."}])
    {
        count += 1;
    }
    let entry = item(
        key,
        TranscriptKind::Tool,
        &format!("Native {name}"),
        body,
        json!({"provider":"claude","type":"nativeLocalCommand","native":messages.get(..count)?}),
    );
    Some((entry, count))
}

fn tool_item(key: String, block: &Value) -> TranscriptItem {
    let name = text(block, "name");
    let input = &block["input"];
    let detail = match name.as_str() {
        "Write" | "Edit" | "Read" => text(input, "file_path"),
        "NotebookEdit" => text(input, "notebook_path"),
        "Grep" | "Glob" => [text(input, "pattern"), text(input, "path")]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        "ToolSearch" | "WebSearch" => text(input, "query"),
        "WebFetch" => text(input, "url"),
        "Agent" | "Task" | "Workflow" => text(input, "description"),
        "Skill" => text(input, "skill"),
        "TaskOutput" | "TaskStop" => text(input, "task_id"),
        "Artifact" => text(input, "operation"),
        _ => String::new(),
    };
    let title = if detail.is_empty() {
        name.clone()
    } else {
        format!(
            "{name} · {}",
            detail.split_whitespace().collect::<Vec<_>>().join(" ")
        )
    };
    let mut raw = json!({"provider":"claude", "native":block});
    let kind = match name.as_str() {
        "Bash" => {
            raw["command"] = input["command"].clone();
            raw["cwd"] = input["cwd"].clone();
            TranscriptKind::Command
        }
        "Write" | "Edit" => TranscriptKind::FileChange,
        "WebSearch" => {
            raw["action"] = json!({"type":"search","query":input["query"]});
            TranscriptKind::Web
        }
        "WebFetch" => {
            raw["action"] = json!({"type":"openPage","url":input["url"]});
            TranscriptKind::Web
        }
        "Agent" | "Task" | "Workflow" => TranscriptKind::Subagent,
        _ => TranscriptKind::Tool,
    };
    let content = if kind == TranscriptKind::Command {
        format!("$ {}", text(input, "command"))
    } else if kind == TranscriptKind::FileChange {
        // Input is a proposal, not evidence that an on-disk change happened.
        format!(
            "Proposed {} · {}\n{}",
            name.to_lowercase(),
            text(input, "file_path"),
            pretty(input)
        )
    } else {
        format!("Input\n{}", pretty(input))
    };
    let mut entry = item(key, kind, &title, content, raw);
    entry.protocol_id = block["id"].as_str().map(ToOwned::to_owned);
    entry
}

fn finish_tool(entry: &mut TranscriptItem, block: &Value, metadata: &Value) {
    let output = block_text(&block["content"]);
    let failed = block["is_error"] == true
        || metadata["isError"] == true
        || metadata["exitCode"].as_i64().is_some_and(|code| code != 0);
    let interrupted = metadata["interrupted"] == true;
    let background =
        metadata["backgroundTaskId"].as_str().is_some() || metadata["status"] == "async_launched";
    entry.raw["aggregatedOutput"] = json!(output);
    entry.raw["nativeResult"] = block.clone();
    entry.raw["nativeToolResult"] = metadata.clone();
    entry.status = Some(
        if interrupted {
            "interrupted"
        } else if failed {
            "failed"
        } else if background {
            "background task started"
        } else {
            "completed"
        }
        .into(),
    );
    entry.expanded = failed || interrupted;
    let exit_code = metadata["exitCode"].as_i64().or_else(|| {
        (entry.kind == TranscriptKind::Command && block["is_error"] == true)
            .then(|| {
                metadata
                    .as_str()?
                    .strip_prefix("Error: Exit code ")?
                    .parse::<i64>()
                    .ok()
            })
            .flatten()
    });
    if let Some(code) = exit_code {
        entry.raw["exitCode"] = json!(code);
    }
    if entry.kind == TranscriptKind::FileChange && !failed && !interrupted {
        let input = &entry.raw["native"]["input"];
        let path = metadata["filePath"]
            .as_str()
            .or_else(|| input["file_path"].as_str())
            .unwrap_or("Path unavailable");
        if let Some(diff) = file_result_diff(metadata) {
            let operation = if metadata["type"] == "create" {
                "Added"
            } else {
                "Modified"
            };
            entry.content = format!("{operation} · {path}\n{diff}");
            entry.title = format!("{operation} · {path}");
            return;
        }
        entry.title = format!(
            "{} · {path} · diff unavailable",
            text(&entry.raw["native"], "name")
        );
    }
    if entry.kind == TranscriptKind::Web {
        let name = text(&entry.raw["native"], "name");
        let mut results = Vec::new();
        if name == "WebSearch" {
            for result in metadata["results"].as_array().into_iter().flatten() {
                if let Some(links) = result["content"].as_array() {
                    results.extend(links.iter().filter(|link| link["url"].is_string()).cloned());
                } else if result["url"].is_string() {
                    results.push(result.clone());
                }
            }
        }
        // The shared web body renders results, not generic tool text. Preserve
        // errors, fetch output, and unknown result shapes there as well.
        if results.is_empty() && !output.is_empty() {
            results.push(json!({"title":if failed { "Request failed" } else { "Response" },"content":output}));
        }
        entry.raw["results"] = json!(results);
    }
    entry.content.push_str(&format!("\n\nResult\n{output}"));
    if !metadata.is_null() && entry.kind != TranscriptKind::Command {
        entry
            .content
            .push_str(&format!("\n\nNative details\n{}", pretty(metadata)));
    }
}

fn supports_permission(dialog: &Value) -> bool {
    let payload = &dialog["payload"];
    if !payload["input"].is_object() || !dialog["id"].is_string() {
        return false;
    }
    match (dialog["kind"].as_str(), payload["toolName"].as_str()) {
        (Some("permission_file"), Some("Write")) => {
            payload["input"]["file_path"].is_string() && payload["input"]["content"].is_string()
        }
        (Some("permission_file"), Some("Edit")) => {
            payload["input"]["file_path"].is_string()
                && payload["input"]["old_string"].is_string()
                && payload["input"]["new_string"].is_string()
        }
        (Some("permission_bash"), Some("Bash")) => payload["input"]["command"].is_string(),
        _ => false,
    }
}

fn native_question_input(dialog: &Value) -> anyhow::Result<Value> {
    ensure!(
        dialog["kind"] == "permission_ask_user_question"
            && dialog["payload"]["toolName"] == "AskUserQuestion"
            && dialog["id"].as_str().is_some_and(|id| !id.is_empty()),
        "Not a supported Claude question request"
    );
    let payload = &dialog["payload"];
    let mut input = payload["input"]
        .as_object()
        .context("Missing question input")?
        .clone();
    if let Some(updated) = payload["permissionResult"].get("updatedInput") {
        input.extend(
            updated
                .as_object()
                .context("Invalid updated question input")?
                .clone(),
        );
    }
    ensure!(
        input.get("questions") == payload.get("questions"),
        "Claude's displayed questions don't match its tool input"
    );
    Ok(Value::Object(input))
}

fn native_questions(dialog: &Value) -> anyhow::Result<Vec<Value>> {
    let input = native_question_input(dialog)?;
    let questions = input["questions"].as_array().context("Missing questions")?;
    ensure!(
        (1..=4).contains(&questions.len()),
        "Unsupported question count"
    );
    let mut texts = std::collections::HashSet::new();
    questions.iter().enumerate().map(|(index, question)| {
        let fields = question.as_object().context("Invalid question")?;
        ensure!(fields.keys().all(|key| matches!(key.as_str(),
            "question" | "header" | "options" | "multiSelect" | "kind")),
            "This question has presentation fields Harness doesn't support yet");
        ensure!(question.get("kind").is_none_or(|kind| kind == "choice"), "Unsupported question kind");
        let text = question["question"].as_str().filter(|text| !text.trim().is_empty()).context("Missing question text")?;
        ensure!(texts.insert(text), "Duplicate question text");
        ensure!(question["header"].is_string(), "Missing question header");
        let multiple = match question.get("multiSelect") {
            Some(value) => value.as_bool().context("Invalid multi-select setting")?,
            None => false,
        };
        let options = question["options"].as_array().context("Missing options")?;
        ensure!((2..=4).contains(&options.len()), "Unsupported option count");
        let mut labels = std::collections::HashSet::new();
        for option in options {
            let fields = option.as_object().context("Invalid option")?;
            ensure!(fields.keys().all(|key| matches!(key.as_str(), "label" | "description")),
                "This option has a preview Harness doesn't support yet");
            let label = option["label"].as_str().filter(|label| !label.trim().is_empty()).context("Missing option label")?;
            ensure!(labels.insert(label), "Duplicate option label");
            ensure!(option["description"].is_string(), "Missing option description");
        }
        Ok(json!({"id":format!("question-{index}"), "question":text,
            "header":question["header"], "options":options, "multiSelect":multiple, "isOther":true}))
    }).collect()
}

pub(super) fn question_action(dialog: &Value, response: &Value) -> anyhow::Result<Value> {
    ensure!(
        response.get("nativeDialog") == Some(dialog),
        "Claude changed this question. Review the current request before answering"
    );
    let questions = native_questions(dialog)?;
    let answers = response["answers"].as_object().context("Missing answers")?;
    ensure!(
        answers.len() == questions.len(),
        "Answer each question before submitting"
    );
    let mut native_answers = serde_json::Map::new();
    for question in questions {
        let identifier = question["id"]
            .as_str()
            .context("Missing question identifier")?;
        let selected = answers
            .get(identifier)
            .and_then(|answer| answer["answers"].as_array())
            .context("Answer each question before submitting")?;
        ensure!(
            !selected.is_empty() && (question["multiSelect"] == true || selected.len() == 1),
            "Choose one answer for a single-choice question"
        );
        let values = selected
            .iter()
            .map(|answer| {
                answer
                    .as_str()
                    .filter(|answer| !answer.trim().is_empty())
                    .context("Answers must contain text")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        ensure!(
            values
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == values.len(),
            "Duplicate answer"
        );
        native_answers.insert(
            question["question"]
                .as_str()
                .context("Missing question text")?
                .into(),
            Value::String(values.join(", ")),
        );
    }
    let mut input = native_question_input(dialog)?;
    input["answers"] = Value::Object(native_answers);
    Ok(
        json!({"method":"dialog_reply", "dialogId":dialog["id"], "expectedDialog":dialog,
        "reply":{"result":{"behavior":"allow", "updatedInput":input}}}),
    )
}

fn file_result_diff(metadata: &Value) -> Option<String> {
    if let Some(hunks) = metadata["structuredPatch"]
        .as_array()
        .filter(|hunks| !hunks.is_empty())
    {
        return hunks
            .iter()
            .map(|hunk| {
                let lines = hunk["lines"]
                    .as_array()?
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()?;
                Some(format!(
                    "@@ -{},{} +{},{} @@\n{}",
                    hunk["oldStart"].as_u64()?,
                    hunk["oldLines"].as_u64()?,
                    hunk["newStart"].as_u64()?,
                    hunk["newLines"].as_u64()?,
                    lines.join("\n")
                ))
            })
            .collect::<Option<Vec<_>>>()
            .map(|hunks| hunks.join("\n"));
    }
    if metadata["type"] == "create" {
        let content = metadata["content"].as_str()?;
        if content.is_empty() {
            return Some(String::new());
        }
        let mut diff = format!("@@ -0,0 +1,{} @@", content.lines().count());
        for line in content.lines() {
            diff.push_str(&format!("\n+{line}"));
        }
        if !content.ends_with('\n') {
            diff.push_str("\n\\ No newline at end of file");
        }
        return Some(diff);
    }
    None
}

fn pretty(value: &Value) -> String {
    // serde_json::Value contains no user-defined serializers or non-string keys.
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|error| format!("Could not display native data: {error}"))
}

fn text(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap_or_default().to_owned()
}
fn block_text(value: &Value) -> String {
    value.as_str().map(ToOwned::to_owned).unwrap_or_else(|| {
        value
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .map(|block| {
                        block["text"]
                            .as_str()
                            .map(ToOwned::to_owned)
                            .unwrap_or_else(|| pretty(block))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| {
                if value.is_null() {
                    String::new()
                } else {
                    pretty(value)
                }
            })
    })
}
fn item(
    key: String,
    kind: TranscriptKind,
    title: &str,
    content: String,
    raw: Value,
) -> TranscriptItem {
    TranscriptItem {
        key,
        protocol_id: None,
        kind,
        title: title.into(),
        status: None,
        content,
        raw,
        event_count: 1,
        expanded: matches!(kind, TranscriptKind::User | TranscriptKind::Agent),
        pending_request: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn away_summary_is_a_readable_recap_with_the_original_event_preserved() {
        let raw = json!({"type":"system", "subtype":"away_summary", "uuid":"recap",
            "content":"Working on the editor. (disable recaps in /config)"});
        let projection = Projection {
            messages: vec![raw.clone()],
            ..Default::default()
        };
        let items = projection.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "While you were away");
        assert_eq!(items[0].content, "Working on the editor.");
        assert_eq!(items[0].raw["type"], "sessionRecap");
        assert!(
            serde_json::to_string(&items[0].raw)
                .unwrap()
                .contains("disable recaps")
        );
    }

    #[test]
    fn native_settings_snapshot_and_events_do_not_use_last_response_model() -> anyhow::Result<()> {
        let mut projection = Projection::default();
        projection.apply(json!({"id":"snapshot", "result":{
            "messages":[],"dialogs":[],"epoch":"epoch", "sessionId":"session", "sequence":0,
            "capabilities":{"sessionSettings":1}, "settings":{"model":"sonnet",
                "effort":{"kind":"inherit"}, "permissionMode":"default"}
        }}))?;
        assert!(projection.settings_controls);
        assert_eq!(projection.settings.as_ref().unwrap().model, "sonnet");
        projection.apply(
            json!({"event":"settings","epoch":"epoch","sequence":1,"data":{
            "model":"haiku","effort":{"kind":"inherit"},"permissionMode":"plan"}}),
        )?;
        assert_eq!(projection.settings.as_ref().unwrap().model, "haiku");
        projection.disconnect();
        assert!(!projection.settings_controls);
        assert!(!projection.ready);
        Ok(())
    }

    fn question_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../research/claude-native-lab/fixtures/question-dialog.json"
        ))
        .expect("question fixture")
    }

    #[test]
    fn native_questions_preserve_input_and_answer_by_question_text() -> anyhow::Result<()> {
        let dialog = question_fixture();
        let questions = native_questions(&dialog)?;
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0]["multiSelect"], false);
        assert_eq!(questions[1]["multiSelect"], true);
        let response = json!({"nativeDialog":dialog, "answers":{
            "question-0":{"answers":["Amber"]}, "question-1":{"answers":["Unit", "Integration", "Manual review"]}
        }});
        let action = question_action(&dialog, &response)?;
        assert_eq!(action["expectedDialog"], dialog);
        assert_eq!(action["dialogId"], dialog["id"]);
        let input = &action["reply"]["result"]["updatedInput"];
        assert_eq!(input["metadata"], dialog["payload"]["input"]["metadata"]);
        assert_eq!(input["questions"], dialog["payload"]["questions"]);
        assert_eq!(
            input["answers"],
            json!({"Which marker do you prefer?":"Amber", "Which checks should run?":"Unit, Integration, Manual review"})
        );
        let mut custom = response.clone();
        custom["answers"]["question-0"]["answers"] = json!(["A different marker"]);
        assert_eq!(
            question_action(&dialog, &custom)?["reply"]["result"]["updatedInput"]["answers"]["Which marker do you prefer?"],
            "A different marker"
        );
        Ok(())
    }

    #[test]
    fn native_questions_reject_stale_missing_duplicate_and_invalid_answers() {
        let dialog = question_fixture();
        let response = json!({"nativeDialog":dialog, "answers":{
            "question-0":{"answers":["Amber"]}, "question-1":{"answers":["Unit"]}
        }});
        let mut stale = response.clone();
        stale["nativeDialog"]["payload"]["questions"][0]["question"] = json!("Changed question");
        assert!(question_action(&dialog, &stale).is_err());
        for invalid in [
            json!([]),
            json!([""]),
            json!([null]),
            json!(["Blue", "Amber"]),
        ] {
            let mut response = response.clone();
            response["answers"]["question-0"]["answers"] = invalid;
            assert!(question_action(&dialog, &response).is_err());
        }
        let mut duplicate = response.clone();
        duplicate["answers"]["question-1"]["answers"] = json!(["Unit", "Unit"]);
        assert!(question_action(&dialog, &duplicate).is_err());
        let mut missing = response;
        missing["answers"] = json!({});
        assert!(question_action(&dialog, &missing).is_err());
    }

    #[test]
    fn native_questions_use_permission_updated_input_without_losing_metadata() -> anyhow::Result<()>
    {
        let mut dialog = question_fixture();
        let mut updated = dialog["payload"]["input"].clone();
        updated["questions"][0]["header"] = json!("Updated marker");
        dialog["payload"]["questions"] = updated["questions"].clone();
        dialog["payload"]["permissionResult"]["updatedInput"] = updated.clone();
        assert_eq!(native_questions(&dialog)?[0]["header"], "Updated marker");
        let response = json!({"nativeDialog":dialog, "answers":{
            "question-0":{"answers":["Blue"]}, "question-1":{"answers":["Unit"]}
        }});
        let action = question_action(&dialog, &response)?;
        updated["answers"] =
            json!({"Which marker do you prefer?":"Blue", "Which checks should run?":"Unit"});
        assert_eq!(action["reply"]["result"]["updatedInput"], updated);
        Ok(())
    }

    #[test]
    fn unsupported_question_presentations_never_become_generic_approvals() {
        let dialog = question_fixture();
        let mut projection = Projection {
            checked_dialog_replies: true,
            dialogs: vec![dialog.clone()],
            ..Projection::default()
        };
        assert_eq!(
            projection.items()[0]
                .pending_request
                .as_ref()
                .map(|request| request.method.as_str()),
            Some("claude/questions")
        );
        projection.checked_dialog_replies = false;
        assert!(
            projection.items()[0].pending_request.is_none(),
            "Old adapters cannot safely compare changed requests"
        );
        projection.checked_dialog_replies = true;
        for change in [
            "preview",
            "duplicate",
            "different-input",
            "new-kind",
            "invalid-multiple",
        ] {
            let mut changed = dialog.clone();
            match change {
                "preview" => {
                    changed["payload"]["questions"][0]["options"][0]["preview"] =
                        json!("Important preview")
                }
                "duplicate" => {
                    changed["payload"]["questions"][1]["question"] =
                        changed["payload"]["questions"][0]["question"].clone()
                }
                "new-kind" => changed["payload"]["questions"][0]["kind"] = json!("freeform"),
                "invalid-multiple" => {
                    changed["payload"]["questions"][0]["multiSelect"] = json!("yes")
                }
                _ => changed["payload"]["questions"][0]["header"] = json!("Changed"),
            }
            if change != "different-input" {
                changed["payload"]["input"]["questions"] = changed["payload"]["questions"].clone();
            }
            projection.dialogs = vec![changed];
            assert!(projection.items()[0].pending_request.is_none(), "{change}");
        }
    }

    pub(super) struct Fixture(pub(super) PathBuf);

    impl Fixture {
        pub(super) fn new() -> anyhow::Result<Self> {
            let path = std::env::temp_dir().join(format!("hc-{}", Uuid::new_v4().simple()));
            private_directory(&path)?;
            Ok(Self(path))
        }

        fn session(&self) -> anyhow::Result<Session> {
            let id = Uuid::new_v4().to_string();
            let directory = self.0.join(format!("harness-claude-{id}"));
            private_directory(&directory)?;
            Ok(Session {
                id,
                directory,
                cwd: self.0.clone(),
                title: "Test workspace".into(),
                lifecycle_version: 1,
                created_at_ms: 1,
                source: SessionSource::Managed,
            })
        }

        fn catalog(&self) -> anyhow::Result<PathBuf> {
            let path = self.0.join("catalog");
            private_directory(&path)?;
            Ok(path)
        }

        fn lock(&self, session: &Session) -> anyhow::Result<File> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(session.directory.join("host.lock"))?;
            ensure!(try_host_lock(&file)?, "Test lock unexpectedly held");
            publish_file(&session.directory.join("host-started.json"), b"{}", false)?;
            Ok(file)
        }

        fn wait_for_lock_release(&self, session: &Session) -> anyhow::Result<()> {
            // Another test can fork while this descriptor is open. CLOEXEC
            // releases the child's inherited lock only when that child execs.
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while host_lock_held(&session.directory)? {
                ensure!(
                    std::time::Instant::now() < deadline,
                    "Test host lock remained held after its owner closed"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                log::warn!(
                    "Could not clean lifecycle fixture {}: {error}",
                    self.0.display()
                );
            }
        }
    }

    #[test]
    fn discovery_keeps_good_records_and_does_not_recreate_missing_runtimes() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let catalog = fixture.catalog()?;
        let mut first = fixture.session()?;
        first.directory = fixture
            .0
            .join("missing")
            .join(format!("harness-claude-{}", first.id));
        let mut second = fixture.session()?;
        second.created_at_ms = 2;
        for session in [&first, &second] {
            publish_file(
                &catalog.join(format!("{}.json", session.id)),
                &serde_json::to_vec(session)?,
                false,
            )?;
        }
        write_new(&catalog.join("corrupt.json"), b"{broken")?;
        let result = read_catalog(&catalog)?;
        assert_eq!(result.sessions.len(), 2);
        assert_eq!(
            result.sessions.first().map(|session| &session.id),
            Some(&second.id)
        );
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("corrupt.json"));
        assert_eq!(result.statuses[&first.id].phase, HostPhase::Unavailable);
        assert!(!first.directory.exists());
        assert!(!fixture.0.join("missing").exists());
        Ok(())
    }

    #[test]
    fn discovery_rejects_symlinks_mismatched_identity_and_future_records() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let catalog = fixture.catalog()?;
        let mut session = fixture.session()?;
        let record = fixture.0.join("outside.json");
        write_new(&record, &serde_json::to_vec(&session)?)?;
        std::os::unix::fs::symlink(&record, catalog.join(format!("{}.json", session.id)))?;
        publish_file(
            &catalog.join("wrong-id.json"),
            &serde_json::to_vec(&session)?,
            false,
        )?;
        session.id = Uuid::new_v4().to_string();
        session.lifecycle_version = 99;
        publish_file(
            &catalog.join(format!("{}.json", session.id)),
            &serde_json::to_vec(&session)?,
            false,
        )?;
        let result = read_catalog(&catalog)?;
        assert!(result.sessions.is_empty());
        assert_eq!(result.warnings.len(), 3);
        Ok(())
    }

    #[test]
    fn launch_publishes_discovery_before_spawn_and_records_spawn_failure() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let catalog = fixture.catalog()?;
        let result = start_with(fixture.0.clone(), &catalog, &fixture.0, |session| {
            let records = read_catalog(&catalog)?;
            assert_eq!(records.sessions.len(), 1);
            assert_eq!(records.sessions[0].id, session.id);
            assert!(session.directory.join("session.json").exists());
            bail!("Injected host spawn failure")
        });
        assert!(result.is_err());
        let records = read_catalog(&catalog)?;
        assert_eq!(records.sessions.len(), 1);
        let status = &records.statuses[&records.sessions[0].id];
        assert_eq!(status.phase, HostPhase::Failed);
        assert!(status.message.contains("Injected host spawn failure"));
        assert!(!status.can_reconnect());
        Ok(())
    }

    #[test]
    fn invalid_catalog_prevents_launch_and_atomic_publish_does_not_overwrite_records()
    -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let invalid = fixture.0.join("not-a-directory");
        write_new(&invalid, b"original")?;
        let mut launched = false;
        let result = start_with(fixture.0.clone(), &invalid, &fixture.0, |_| {
            launched = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!launched);
        assert!(publish_file(&invalid, b"replacement", false).is_err());
        assert_eq!(fs::read(&invalid)?, b"original");
        assert_eq!(fs::read_dir(&fixture.0)?.count(), 1);
        Ok(())
    }

    #[test]
    fn host_status_probe_does_not_leave_an_inherited_descriptor_holding_ownership()
    -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = fixture.session()?;
        let path = session.directory.join("host.lock");
        let observer = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let inherited = observer.try_clone()?;
        assert!(!probe_host_lock(&observer)?);
        drop(observer);
        let owner = File::open(&path)?;
        assert!(try_host_lock(&owner)?);
        assert!(probe_host_lock(&inherited)?);
        assert!(!try_host_lock(&inherited)?);
        Ok(())
    }

    #[test]
    fn lifetime_lock_overrules_stale_attached_markers_and_releases_on_owner_exit()
    -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = fixture.session()?;
        let lock = fixture.lock(&session)?;
        record_state(&session.directory, HostPhase::NeedsSetup, "Trust prompt")?;
        assert_eq!(session.status().phase, HostPhase::NeedsSetup);
        publish_file(
            &session.directory.join("preload-status.json"),
            br#"{"state":"attached","pid":1}"#,
            false,
        )?;
        assert_eq!(session.status().phase, HostPhase::Available);
        let duplicate = File::open(session.directory.join("host.lock"))?;
        assert!(!try_host_lock(&duplicate)?);
        drop(lock);
        fixture.wait_for_lock_release(&session)?;
        assert_eq!(session.status().phase, HostPhase::Stopped);
        assert!(!session.status().can_reconnect());
        Ok(())
    }

    #[test]
    fn duplicate_host_cannot_replace_state_or_relaunch_a_used_runtime() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = fixture.session()?;
        publish_file(
            &session.directory.join("session.json"),
            &serde_json::to_vec(&session)?,
            false,
        )?;
        let lock = fixture.lock(&session)?;
        record_state(&session.directory, HostPhase::NeedsSetup, "Original state")?;
        assert!(
            host(&session.directory)
                .expect_err("duplicate host")
                .to_string()
                .contains("already running")
        );
        assert!(!session.directory.join("host-error.txt").exists());
        assert_eq!(session.status().message, "Original state");
        drop(lock);
        fixture.wait_for_lock_release(&session)?;
        assert!(
            host(&session.directory)
                .expect_err("reused host")
                .to_string()
                .contains("already been started")
        );
        assert!(!session.directory.join("host-error.txt").exists());
        Ok(())
    }

    #[test]
    fn legacy_sessions_need_a_live_listener_not_a_socket_path() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let mut session = fixture.session()?;
        session.lifecycle_version = 0;
        let listener = UnixListener::bind(session.socket())?;
        fs::set_permissions(
            session.socket(),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        assert_eq!(session.status().phase, HostPhase::Available);
        drop(listener);
        assert!(session.socket().exists());
        // Concurrent process-spawn tests can briefly inherit this listener
        // between fork and exec, even though the descriptor is CLOEXEC.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while socket_accepts(&session.socket()) {
            ensure!(
                std::time::Instant::now() < deadline,
                "Test listener remained alive after its owner closed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.status().phase, HostPhase::Unavailable);
        Ok(())
    }

    #[test]
    fn sidebar_omits_worker_residency_but_keeps_actionable_status() {
        for phase in [HostPhase::Available, HostPhase::Saved] {
            assert_eq!(HostStatus::new(phase, "internal detail").sidebar_label(), "");
        }
        assert_eq!(
            HostStatus::new(HostPhase::Starting, "starting").sidebar_label(),
            "Starting"
        );
        assert_eq!(
            HostStatus::new(HostPhase::Failed, "failure detail").sidebar_label(),
            "Needs attention"
        );
        assert_eq!(
            HostStatus::new(HostPhase::NeedsAdapter, "unreachable").sidebar_label(),
            "Not connected"
        );
    }

    #[test]
    fn adapter_startup_failures_are_terminal_but_human_setup_can_wait() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let session = fixture.session()?;
        let _lock = fixture.lock(&session)?;
        record_state(
            &session.directory,
            HostPhase::NeedsSetup,
            "Waiting for trust/login",
        )?;
        assert!(should_reconnect(&session.status(), 100));
        for state in ["unsupported", "error", "not-mounted"] {
            publish_file(
                &session.directory.join("preload-status.json"),
                &serde_json::to_vec(&json!({"state":state}))?,
                true,
            )?;
            assert_eq!(session.status().phase, HostPhase::Failed);
            assert!(!should_reconnect(&session.status(), 0));
        }
        let available = HostStatus::new(HostPhase::Available, "Running");
        assert!(should_reconnect(&available, 4));
        assert!(!should_reconnect(&available, 5));
        Ok(())
    }

    #[test]
    fn timeouts_cancel_waits_without_resubmitting() {
        let result = smol::block_on(with_timeout(
            std::future::pending::<anyhow::Result<()>>(),
            Duration::from_millis(10),
            "Test timeout",
        ));
        assert!(
            result
                .expect_err("timeout")
                .to_string()
                .contains("Test timeout")
        );
    }

    #[test]
    fn failed_startup_guard_terminates_and_reaps_only_its_own_child() -> anyhow::Result<()> {
        let child = Command::new("/bin/sleep").arg("60").spawn()?;
        let pid = child.id() as i32;
        drop(NativeChild::new(Box::new(child)));
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        Ok(())
    }

    #[test]
    fn disconnect_preserves_history_but_removes_stale_live_controls() {
        let mut projection = Projection {
            ready: true,
            active: true,
            messages: vec![json!({"type":"user","uuid":"user","message":{"content":"Kept"}})],
            dialogs: vec![json!({"id":"old"})],
            ..Default::default()
        };
        projection.disconnect();
        assert!(!projection.ready && !projection.active);
        assert!(projection.dialogs.is_empty());
        assert_eq!(projection.items().len(), 1);
    }

    #[test]
    fn reconnect_snapshot_restores_pending_permission_and_rejects_changed_epoch() {
        let mut projection = Projection::default();
        projection.apply(json!({"id":"snapshot", "result": {
            "messages":[], "dialogs":[{"id":"permission-1", "kind":"permission_file", "payload":{"toolName":"Write","input":{"file_path":"/project/file","content":"exact contents"}}}],
            "epoch":"first", "sessionId":"native-session", "turn":{"isLoading":true}
        }})).expect("snapshot");
        assert!(projection.ready && projection.active);
        let items = projection.items();
        let permission = items.first().expect("permission in shared transcript");
        assert_eq!(
            permission
                .pending_request
                .as_ref()
                .map(|request| request.method.as_str()),
            Some("claude/permission")
        );
        assert_eq!(permission.raw["input"]["content"], "exact contents");
        assert!(
            projection
                .apply(json!({"epoch":"second","event":"turn","data":{}}))
                .is_err()
        );
        projection
            .apply(json!({"epoch":"first","event":"dialogs","data":[]}))
            .expect("native resolution");
        assert!(projection.items().is_empty());
    }

    #[test]
    fn internal_attachment_context_is_not_presented_as_user_prose() {
        let projection = Projection {
            messages: vec![
                json!({"type":"attachment","attachment":{"type":"skill_listing","content":"internal context"}}),
                json!({"type":"user","uuid":"user","message":{"content":"A real prompt"}}),
            ],
            ..Default::default()
        };
        let items = projection.items();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items.first().map(|item| item.content.as_str()),
            Some("A real prompt")
        );
    }

    #[test]
    fn native_local_commands_render_as_one_card_without_discarding_raw_history() {
        let messages = vec![
            json!({"type":"user","isMeta":true,"uuid":"caveat","message":{"content":"<local-command-caveat>Native command context</local-command-caveat>"}}),
            json!({"type":"user","uuid":"command","message":{"content":"<command-name>/background</command-name>\n<command-message>background</command-message>\n<command-args></command-args>"}}),
            json!({"type":"user","uuid":"output","message":{"content":"<local-command-stdout>(no content)</local-command-stdout>"}}),
            json!({"type":"assistant","uuid":"synthetic","message":{"model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}),
        ];
        let projection = Projection {
            messages: messages.clone(),
            ..Default::default()
        };
        let items = projection.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, TranscriptKind::Tool);
        assert_eq!(items[0].title, "Native /background");
        assert_eq!(items[0].raw["native"], json!(messages));
        assert!(!items[0].expanded);
        let mut ordinary = messages;
        ordinary[0]["isMeta"] = json!(false);
        assert!(local_command_item(&ordinary, "test".into()).is_none());
        ordinary[0]["isMeta"] = json!(true);
        ordinary[1]["message"]["content"] =
            json!("<command-name>/background</command-name> user prose");
        assert!(local_command_item(&ordinary, "test".into()).is_none());
    }

    #[test]
    fn native_tools_use_shared_command_shape() {
        let projection = Projection {
            session_id: "session".into(),
            messages: vec![
                json!({"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"tool","name":"Bash","input":{"command":"pwd"}}]}}),
                json!({"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"tool","content":"/project"}]}}),
            ],
            ..Default::default()
        };
        let items = projection.items();
        assert_eq!(items.len(), 1);
        let command = items
            .first()
            .and_then(TranscriptItem::command_transcript)
            .expect("shared command rendering");
        assert_eq!(command.command, "pwd");
        assert_eq!(command.output, "/project");
        assert_eq!(
            items.first().and_then(|item| item.status.as_deref()),
            Some("completed")
        );
    }

    #[test]
    fn native_file_results_use_actual_edits_not_the_proposed_input() {
        let mut write = tool_item(
            "write".into(),
            &json!({"id":"write","name":"Write","input":{"file_path":"/project/new.rs","content":"proposed"}}),
        );
        assert_eq!(write.kind, TranscriptKind::FileChange);
        assert!(
            write
                .content
                .starts_with("Proposed write · /project/new.rs")
        );
        finish_tool(
            &mut write,
            &json!({"content":"Created file"}),
            &json!({"type":"create","filePath":"/project/new.rs","content":"approved\n","structuredPatch":[]}),
        );
        assert_eq!(write.title, "Added · /project/new.rs");
        assert_eq!(
            write.content,
            "Added · /project/new.rs\n@@ -0,0 +1,1 @@\n+approved"
        );
        assert!(!write.content.contains("proposed"));

        let mut edit = tool_item(
            "edit".into(),
            &json!({"id":"edit","name":"Edit","input":{"file_path":"/project/new.rs","old_string":"a","new_string":"b"}}),
        );
        finish_tool(
            &mut edit,
            &json!({"content":"Updated file"}),
            &json!({"filePath":"/project/new.rs","structuredPatch":[{"oldStart":42,"oldLines":1,"newStart":42,"newLines":1,"lines":["-a","+b"]}]}),
        );
        assert_eq!(
            edit.content,
            "Modified · /project/new.rs\n@@ -42,1 +42,1 @@\n-a\n+b"
        );
    }

    #[test]
    fn denied_and_unknown_file_results_never_invent_an_applied_diff() {
        let mut entry = tool_item(
            "write".into(),
            &json!({"name":"Write","input":{"file_path":"/project/existing","content":"replacement"}}),
        );
        finish_tool(
            &mut entry,
            &json!({"is_error":true,"content":"Denied"}),
            &Value::Null,
        );
        assert_eq!(entry.status.as_deref(), Some("failed"));
        assert!(entry.content.starts_with("Proposed write"));
        assert!(!entry.content.contains("@@"));
        assert!(entry.expanded);
        assert_eq!(
            file_result_diff(&json!({"type":"update","content":"replacement"})),
            None
        );
        assert_eq!(
            file_result_diff(&json!({"structuredPatch":[{"lines":["+unlocated"]}]})),
            None
        );
        assert_eq!(
            file_result_diff(&json!({"type":"create","content":""})),
            Some(String::new())
        );
    }

    #[test]
    fn parallel_results_are_correlated_by_tool_id_without_misassigning_metadata() {
        let projection = Projection {
            messages: vec![
                json!({"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"one","name":"Read","input":{"file_path":"/one"}},{"type":"tool_use","id":"two","name":"Bash","input":{"command":"exit 7"}}]}}),
                json!({"type":"user","uuid":"b","toolUseResult":{"exitCode":99},"message":{"content":[{"type":"tool_result","tool_use_id":"two","is_error":true,"content":"Exit code 7"},{"type":"tool_result","tool_use_id":"one","content":"read contents"}]}}),
                json!({"type":"user","uuid":"c","message":{"content":[{"type":"tool_result","tool_use_id":"missing","content":[{"type":"text","text":"kept"},{"type":"future","value":12}]}]}}),
            ],
            ..Default::default()
        };
        let items = projection.items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].title, "Read · /one");
        assert_eq!(items[0].raw["aggregatedOutput"], "read contents");
        assert_eq!(items[1].status.as_deref(), Some("failed"));
        assert!(items[1].raw["exitCode"].is_null());
        assert!(items[2].title.contains("invocation unavailable"));
        assert!(items[2].content.contains("kept") && items[2].content.contains("future"));
    }

    #[test]
    fn native_command_error_and_background_admission_are_not_successful_completion() {
        let mut entry = tool_item(
            "bash".into(),
            &json!({"name":"Bash","input":{"command":"exit 7"}}),
        );
        finish_tool(
            &mut entry,
            &json!({"is_error":true,"content":"Exit code 7"}),
            &json!("Error: Exit code 7"),
        );
        assert_eq!(entry.raw["exitCode"], 7);
        assert_eq!(
            entry.command_execution_status(),
            Some(harness_protocol::CommandExecutionStatus::Failed(Some(7)))
        );
        let mut background = tool_item(
            "background".into(),
            &json!({"name":"Bash","input":{"command":"background job"}}),
        );
        finish_tool(
            &mut background,
            &json!({"content":"Started job"}),
            &json!({"backgroundTaskId":"job"}),
        );
        assert_eq!(
            background.status.as_deref(),
            Some("background task started")
        );
        assert_ne!(
            background.command_execution_status(),
            Some(harness_protocol::CommandExecutionStatus::Succeeded)
        );
    }

    #[test]
    fn semantic_headers_and_web_bodies_preserve_query_results_and_failures() {
        for (name, input, kind, expected) in [
            (
                "Read",
                json!({"file_path":"/project/file"}),
                TranscriptKind::Tool,
                "/project/file",
            ),
            (
                "Grep",
                json!({"pattern":"needle","path":"/project"}),
                TranscriptKind::Tool,
                "needle · /project",
            ),
            (
                "Glob",
                json!({"pattern":"*.rs"}),
                TranscriptKind::Tool,
                "*.rs",
            ),
            (
                "ToolSearch",
                json!({"query":"select:Read"}),
                TranscriptKind::Tool,
                "select:Read",
            ),
            (
                "Agent",
                json!({"description":"Inspect fixture","prompt":"task"}),
                TranscriptKind::Subagent,
                "Inspect fixture",
            ),
            (
                "WebSearch",
                json!({"query":"color relationships"}),
                TranscriptKind::Web,
                "color relationships",
            ),
            (
                "WebFetch",
                json!({"url":"https://example.com"}),
                TranscriptKind::Web,
                "https://example.com",
            ),
        ] {
            let entry = tool_item(name.into(), &json!({"name":name,"input":input}));
            assert_eq!(entry.kind, kind);
            assert!(entry.title.contains(expected), "{}", entry.title);
            assert_eq!(entry.raw["native"]["input"], input);
        }
        let mut search = tool_item(
            "search".into(),
            &json!({"name":"WebSearch","input":{"query":"query"}}),
        );
        finish_tool(
            &mut search,
            &json!({"content":"search answer"}),
            &json!({"results":[{"content":[{"title":"A result","url":"https://example.com"}]}]}),
        );
        assert_eq!(search.raw["action"]["query"], "query");
        assert_eq!(search.raw["results"][0]["url"], "https://example.com");
        let mut fetch = tool_item(
            "fetch".into(),
            &json!({"name":"WebFetch","input":{"url":"https://example.com"}}),
        );
        finish_tool(
            &mut fetch,
            &json!({"is_error":true,"content":"Fetch denied"}),
            &Value::Null,
        );
        assert_eq!(fetch.raw["results"][0]["content"], "Fetch denied");
    }

    #[test]
    fn partial_block_commits_do_not_hide_the_rest_of_the_stream() {
        let mut projection = Projection {
            active: true,
            streaming_id: "message".into(),
            messages: vec![
                json!({"type":"assistant","uuid":"a","apiBlockIndex":0,"message":{"id":"message","content":[{"type":"thinking","thinking":"thought"}]}}),
            ],
            ..Default::default()
        };
        projection
            .streaming
            .insert(0, json!({"type":"thinking","thinking":"thought"}));
        projection
            .streaming
            .insert(1, json!({"type":"text","text":"answer still streaming"}));
        let items = projection.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].status.as_deref(), Some("streaming"));
        projection.ready = true;
        projection.apply(json!({"event":"engine_event","data":{"type":"stream_event","event":{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tool","name":"Read","input":{}}}}})).expect("start");
        projection.apply(json!({"event":"engine_event","data":{"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"partial_json":"{\"file_path\":\"/project/file\"}"}}}})).expect("input");
        let items = projection.items();
        assert_eq!(items[2].title, "Read · /project/file");
        assert_eq!(items[2].status.as_deref(), Some("receiving input"));
    }

    #[test]
    fn session_errors_compaction_and_unknown_content_remain_visible() {
        let projection = Projection {
            messages: vec![
                json!({"type":"assistant","uuid":"a","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"Rate limited"}]}}),
                json!({"type":"system","uuid":"b","subtype":"compact_boundary"}),
                json!({"type":"system","uuid":"c","subtype":"turn_duration"}),
                json!({"type":"assistant","uuid":"d","message":{"content":[{"type":"future_block","value":"kept"}]}}),
                json!({"type":"assistant","uuid":"e","message":{"content":"string answer"}}),
            ],
            ..Default::default()
        };
        let items = projection.items();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].kind, TranscriptKind::Error);
        assert!(items[0].expanded);
        assert_eq!(items[1].kind, TranscriptKind::Trace);
        assert!(items[1].is_presentationally_visible());
        assert!(items[2].content.contains("kept"));
        assert_eq!(items[3].kind, TranscriptKind::Agent);
    }

    #[test]
    fn old_missing_results_and_child_streams_do_not_become_current_parent_activity() {
        let mut projection = Projection {
            ready: true,
            active: true,
            session_id: "parent".into(),
            messages: vec![
                json!({"type":"assistant","uuid":"old","message":{"content":[{"type":"tool_use","id":"old-tool","name":"Read","input":{"file_path":"/old"}}]}}),
                json!({"type":"user","uuid":"new","message":{"content":"Next turn"}}),
            ],
            ..Default::default()
        };
        assert_eq!(
            projection.items()[0].status.as_deref(),
            Some("result unavailable")
        );
        projection.apply(json!({"event":"engine_event","data":{"type":"stream_event","session_id":"child","event":{"type":"message_start","message":{"id":"child-message"}}}})).expect("child event");
        assert!(projection.streaming_id.is_empty());
        projection.apply(json!({"event":"engine_event","data":{"type":"stream_event","session_id":"parent","parent_tool_use_id":"agent","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"child text"}}}})).expect("nested event");
        assert!(projection.streaming.is_empty());
    }

    #[test]
    fn permission_controls_require_a_verified_kind_and_input_shape() {
        assert!(supports_permission(
            &json!({"id":"bash","kind":"permission_bash","payload":{"toolName":"Bash","input":{"command":"exit 7"}}})
        ));
        assert!(!supports_permission(
            &json!({"id":"unknown","kind":"permission_file","payload":{"toolName":"FutureTool","input":{"file_path":"/project/file"}}})
        ));
        assert!(!supports_permission(
            &json!({"id":"write","kind":"permission_file","payload":{"toolName":"Write","input":{"file_path":"/project/file"}}})
        ));
    }

    #[test]
    fn committed_text_replaces_stream_and_unknown_dialogs_never_get_approval() {
        let mut projection = Projection {
            active: true,
            streaming_id: "message".into(),
            ..Default::default()
        };
        projection
            .streaming
            .insert(0, json!({"type":"text","text":"hello"}));
        assert_eq!(projection.items().len(), 1);
        projection.messages.push(json!({"type":"assistant","uuid":"a","message":{"id":"message","content":[{"type":"text","text":"hello"}]}}));
        assert_eq!(projection.items().len(), 1);
        projection
            .dialogs
            .push(json!({"id":"unknown","kind":"future_dialog","payload":{}}));
        assert!(
            projection
                .items()
                .last()
                .is_some_and(|item| item.pending_request.is_none())
        );
    }
}
