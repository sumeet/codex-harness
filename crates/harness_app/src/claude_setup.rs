use super::*;
use sha2::{Digest as _, Sha256};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;

const SUPERVISOR_ASSETS: &[(&str, &str)] = &[
    (
        "supervisor_preload.mjs",
        include_str!("../../../research/claude-native-lab/supervisor_preload.mjs"),
    ),
    (
        "supervisor_wrapper.mjs",
        include_str!("../../../research/claude-native-lab/supervisor_wrapper.mjs"),
    ),
];

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub configured: bool,
    pub detail: String,
}

#[derive(Debug)]
pub struct ConsentRequired;

impl std::fmt::Display for ConsentRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "Set up Claude in Harness before starting this conversation; no session was started",
        )
    }
}

impl std::error::Error for ConsentRequired {}

pub(super) fn require_enabled() -> anyhow::Result<()> {
    if !status().configured {
        return Err(ConsentRequired.into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    version: u32,
    configuration: PathBuf,
    package: PathBuf,
    executable: PathBuf,
    node: PathBuf,
}

fn profile_key(configuration: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    format!("{:x}", Sha256::digest(configuration.as_os_str().as_bytes()))
}

pub(super) fn endpoint_root(configuration: &Path) -> anyhow::Result<PathBuf> {
    let runtime =
        dirs::runtime_dir().context("XDG_RUNTIME_DIR is required for native connections")?;
    validate_private_directory(&runtime)?;
    Ok(runtime.join(format!(
        "harness-claude-{}",
        &profile_key(configuration)[..16]
    )))
}

fn installation_root(configuration: &Path) -> anyhow::Result<PathBuf> {
    let metadata = fs::symlink_metadata(configuration)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "Claude profile must be a real, user-owned directory not writable by others"
    );
    Ok(configuration.join("harness-adapter"))
}

fn registered_packages(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    read_optional(&root.join("installed.json"))?
        .map(|bytes| Ok(serde_json::from_slice(&bytes)?))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn owned_launcher(
    configuration: &Path,
    root: &Path,
    settings: &Value,
) -> anyhow::Result<Option<String>> {
    let Some(value) = settings["processWrapper"].as_str() else {
        return Ok(None);
    };
    for path in registered_packages(root)? {
        ensure!(
            path.parent().and_then(Path::parent) == Some(root),
            "Adapter registry escaped its package directory"
        );
        let manifest = read_manifest(&path)?;
        ensure!(
            manifest.configuration == configuration,
            "Registered adapter belongs to another profile"
        );
        if launcher_value(&manifest)? == value {
            return Ok(Some(value.to_owned()));
        }
    }
    Ok(None)
}

fn read_settings(path: &Path) -> anyhow::Result<(Option<Vec<u8>>, Value)> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, json!({}))),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "Claude settings must be a regular, user-owned file not writable by others"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "Claude settings exceed the setup limit"
    );
    let settings: Value = serde_json::from_slice(&bytes)
        .context("Claude settings are not valid JSON; nothing was changed")?;
    ensure!(settings.is_object(), "Claude settings must be an object");
    Ok((Some(bytes), settings))
}

fn launcher_value(manifest: &Manifest) -> anyhow::Result<String> {
    Ok(serde_json::to_string(&[manifest
        .package
        .join("launcher.sh")])?)
}

fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(
        &read_optional(path)?.context("Adapter package manifest is missing")?,
    )?;
    ensure!(
        manifest.version == 1 && manifest.package.join("manifest.json") == path,
        "Unsupported or relocated adapter package"
    );
    validate_private_directory(&manifest.package)?;
    ensure!(
        manifest.configuration.is_absolute()
            && manifest.node.is_absolute()
            && manifest.executable.is_absolute(),
        "Adapter paths must be absolute"
    );
    Ok(manifest)
}

pub fn status() -> Status {
    match inspect_status() {
        Ok(status) => status,
        Err(error) => Status {
            configured: false,
            detail: format!("Native connection setup needs attention: {error:#}"),
        },
    }
}

fn inspect_status() -> anyhow::Result<Status> {
    let configuration = discovery::configuration_directory()?;
    if !configuration.try_exists()? {
        return Ok(Status {
            configured: false,
            detail: "Install and sign in to native Claude before enabling connections.".into(),
        });
    }
    let configuration = fs::canonicalize(configuration)?;
    let (_, settings) = read_settings(&configuration.join("settings.json"))?;
    let root = installation_root(&configuration)?;
    if owned_launcher(&configuration, &root, &settings)?.is_none() {
        return Ok(Status {
            configured: false,
            detail:
                "Saved history is available. Native connections require an opt-in startup hook."
                    .into(),
        });
    }
    ensure!(
        settings["env"]["CLAUDE_CODE_PROCESS_WRAPPER"].is_null()
            && std::env::var_os("CLAUDE_CODE_PROCESS_WRAPPER").is_none(),
        "Another environment launcher takes precedence over the Harness hook"
    );
    let mut detail = "Startup hook configured for future native background sessions. Existing sessions are not restarted; ordinary terminals still need a guided handoff.".to_owned();
    if let Some(bytes) = read_optional(&endpoint_root(&configuration)?.join("last-startup.json"))? {
        let last: Value = serde_json::from_slice(&bytes)?;
        if last["state"] == "unavailable" {
            detail = format!(
                "Hook configured, but its last launch could not connect: {}. Claude was left running normally.",
                text(&last, "detail")
            );
        }
    }
    Ok(Status {
        configured: true,
        detail,
    })
}

fn shell_quote(path: &Path) -> anyhow::Result<String> {
    let path = path
        .to_str()
        .context("Adapter launcher paths must be UTF-8")?;
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}

fn executable_on_path(name: &str) -> anyhow::Result<PathBuf> {
    let paths = std::env::var_os("PATH").context("PATH is not set")?;
    for directory in std::env::split_paths(&paths) {
        if !directory.is_absolute() {
            continue;
        }
        let path = directory.join(name);
        if let Ok(metadata) = fs::metadata(&path)
            && metadata.is_file()
            && metadata.mode() & 0o111 != 0
        {
            return Ok(fs::canonicalize(path)?);
        }
    }
    bail!("{name} is required to check the native adapter")
}

fn package(
    configuration: &Path,
    root: &Path,
    executable: &Path,
    node: &Path,
) -> anyhow::Result<Manifest> {
    let mut digest = Sha256::new();
    for (name, source) in ASSETS.iter().chain(SUPERVISOR_ASSETS) {
        digest.update(name);
        digest.update([0]);
        digest.update(source);
    }
    digest.update(configuration.as_os_str().as_encoded_bytes());
    digest.update(executable.as_os_str().as_encoded_bytes());
    digest.update(node.as_os_str().as_encoded_bytes());
    let directory = root.join(format!("v1-{:x}", digest.finalize()));
    let manifest = Manifest {
        version: 1,
        configuration: configuration.to_owned(),
        package: directory.clone(),
        executable: executable.to_owned(),
        node: node.to_owned(),
    };
    let launcher = format!(
        "#!/bin/sh\nif [ -x {executable} ]; then\n  exec {executable} --claude-wrap {manifest} \"$@\"\nfi\nexec \"$@\"\n",
        executable = shell_quote(executable)?,
        manifest = shell_quote(&directory.join("manifest.json"))?
    );
    private_directory(root)?;
    if directory.try_exists()? {
        ensure!(
            read_manifest(&directory.join("manifest.json"))? == manifest,
            "Installed adapter manifest changed"
        );
        for (name, source) in ASSETS.iter().chain(SUPERVISOR_ASSETS) {
            ensure!(
                read_optional(&directory.join(name))?.as_deref() == Some(source.as_bytes()),
                "Installed adapter asset changed: {name}"
            );
        }
        ensure!(
            read_optional(&directory.join("launcher.sh"))?.as_deref() == Some(launcher.as_bytes()),
            "Installed adapter launcher changed"
        );
        return Ok(manifest);
    }
    let staging = root.join(format!(".staging-{}", Uuid::new_v4()));
    private_directory(&staging)?;
    for (name, source) in ASSETS.iter().chain(SUPERVISOR_ASSETS) {
        write_new(&staging.join(name), source.as_bytes())?;
    }
    write_new(
        &staging.join("manifest.json"),
        &serde_json::to_vec(&manifest)?,
    )?;
    // The dispatcher survives Harness upgrades at the same installation path.
    // If Harness is uninstalled, native self-spawns remain usable.
    write_new(&staging.join("launcher.sh"), launcher.as_bytes())?;
    fs::set_permissions(
        staging.join("launcher.sh"),
        fs::Permissions::from_mode(0o700),
    )?;
    fs::rename(&staging, &directory)?;
    File::open(root)?.sync_all()?;
    Ok(manifest)
}

fn compatibility(manifest: &Manifest, binary: &Path) -> anyhow::Result<Value> {
    let mut command = async_process::Command::new(&manifest.node);
    command
        .arg("--max-old-space-size=512")
        .arg(manifest.package.join("discover.mjs"))
        .arg(binary)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    // These options configure Node itself, not Claude's inherited environment.
    command.env_remove("NODE_OPTIONS");
    let output = smol::block_on(with_timeout(
        async { Ok(command.output().await?) },
        Duration::from_secs(2),
        "Native compatibility check exceeded the launcher budget",
    ))?;
    ensure!(output.status.success(), "Native compatibility check failed");
    let specification: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        specification["verified"] == true,
        "Claude build {} is not verified",
        text(&specification, "sha256")
    );
    Ok(specification)
}

fn conflicting_settings(settings: &Value, previous: Option<&str>) -> anyhow::Result<()> {
    ensure!(
        settings["env"]["CLAUDE_CODE_PROCESS_WRAPPER"].is_null(),
        "Claude settings already configure an environment launcher; it will not be overwritten"
    );
    if let Some(value) = settings.get("processWrapper") {
        ensure!(
            value.as_str().is_some_and(|value| Some(value) == previous),
            "Claude already has a different process launcher; compose it explicitly before setup"
        );
    }
    ensure!(
        settings["env"]["BUN_OPTIONS"].is_null(),
        "Claude settings already configure a Bun startup hook; it will not be overwritten"
    );
    Ok(())
}

fn commit_settings(
    configuration: &Path,
    expected: Option<&[u8]>,
    settings: &Value,
    root: &Path,
) -> anyhow::Result<()> {
    let path = configuration.join("settings.json");
    let (current, _) = read_settings(&path)?;
    ensure!(
        current.as_deref() == expected,
        "Claude settings changed during setup; nothing was overwritten. Retry setup."
    );
    if let Some(bytes) = expected {
        write_new(
            &root.join(format!("settings-backup-{}.json", Uuid::new_v4())),
            bytes,
        )?;
    }
    publish_file(
        &path,
        &serde_json::to_vec_pretty(settings)?,
        expected.is_some(),
    )
}

fn setup_lock(root: &Path) -> anyhow::Result<File> {
    setup_lock_for(root, Duration::ZERO)
}

fn setup_lock_for(root: &Path, timeout: Duration) -> anyhow::Result<File> {
    private_directory(root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("setup.lock"))?;
    ensure!(
        lock.metadata()?.is_file() && lock.metadata()?.uid() == unsafe { libc::geteuid() },
        "Invalid setup lock"
    );
    let deadline = std::time::Instant::now() + timeout;
    while !try_host_lock(&lock)? {
        ensure!(
            std::time::Instant::now() < deadline,
            "Another Harness window is changing native setup; retry when it finishes"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(lock)
}

pub fn prepare() -> anyhow::Result<PathBuf> {
    let configuration = fs::canonicalize(discovery::configuration_directory()?)?;
    let root = installation_root(&configuration)?;
    let _lock = setup_lock(&root)?;
    let manifest = package(
        &configuration,
        &root,
        &fs::canonicalize(std::env::current_exe()?)?,
        &executable_on_path("node")?,
    )?;
    compatibility(&manifest, &native_binary()?)?;
    endpoint_root(&configuration)?;
    ensure!(
        !manifest
            .package
            .to_string_lossy()
            .contains(char::is_whitespace),
        "Native preload paths containing whitespace are not supported"
    );
    Ok(manifest.package.join("manifest.json"))
}

pub fn enable() -> anyhow::Result<Status> {
    ensure!(
        std::env::var_os("CLAUDE_CODE_PROCESS_WRAPPER").is_none()
            && std::env::var_os("BUN_OPTIONS").is_none(),
        "An inherited launcher or Bun hook is already configured; it will not be overwritten"
    );
    let configuration = fs::canonicalize(discovery::configuration_directory()?)?;
    let root = installation_root(&configuration)?;
    let _lock = setup_lock(&root)?;
    let (before, mut settings) = read_settings(&configuration.join("settings.json"))?;
    let previous = owned_launcher(&configuration, &root, &settings)?;
    conflicting_settings(&settings, previous.as_deref())?;
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let manifest = package(
        &configuration,
        &root,
        &executable,
        &executable_on_path("node")?,
    )?;
    compatibility(&manifest, &native_binary()?)?;
    endpoint_root(&configuration)?;
    ensure!(
        !manifest
            .package
            .to_string_lossy()
            .contains(char::is_whitespace),
        "Native preload paths containing whitespace are not supported; settings were not changed"
    );
    settings["processWrapper"] = json!(launcher_value(&manifest)?);
    // Publish recovery information first. A crash cannot leave an enabled
    // launcher whose ownership record was never saved.
    let mut packages = registered_packages(&root)?;
    let manifest_path = manifest.package.join("manifest.json");
    if !packages.contains(&manifest_path) {
        packages.push(manifest_path);
    }
    publish_file(
        &root.join("installed.json"),
        &serde_json::to_vec(&packages)?,
        true,
    )?;
    commit_settings(&configuration, before.as_deref(), &settings, &root)?;
    inspect_status()
}

pub fn disable() -> anyhow::Result<Status> {
    let configuration = fs::canonicalize(discovery::configuration_directory()?)?;
    let root = installation_root(&configuration)?;
    let _lock = setup_lock(&root)?;
    let (before, mut settings) = read_settings(&configuration.join("settings.json"))?;
    if settings.get("processWrapper").is_some() {
        ensure!(
            owned_launcher(&configuration, &root, &settings)?.is_some(),
            "Another launcher replaced Harness; it will not be removed"
        );
        settings
            .as_object_mut()
            .context("Invalid settings")?
            .remove("processWrapper");
        commit_settings(&configuration, before.as_deref(), &settings, &root)?;
    }
    // Assets remain for already-running supervisors that inherited this path.
    Ok(Status { configured: false, detail: "Startup hook disabled for future sessions. Existing workers and adapter assets were left intact.".into() })
}

fn worker_environment(
    manifest: &Manifest,
    command: &[OsString],
) -> anyhow::Result<Option<(Value, PathBuf)>> {
    let role = command.get(1).and_then(|argument| argument.to_str());
    let worker = std::env::var("CLAUDE_CODE_SESSION_KIND").as_deref() == Ok("bg")
        && std::env::var_os("CLAUDE_JOB_DIR").is_some();
    if matches!(role, Some("daemon" | "--bg-pty-host")) || (!worker && role != Some("--bg-spare")) {
        return Ok(None);
    }
    let configuration = fs::canonicalize(discovery::configuration_directory()?)?;
    if configuration != manifest.configuration {
        return Ok(None);
    }
    let root = installation_root(&configuration)?;
    ensure!(
        manifest.package.parent() == Some(root.as_path()),
        "Adapter package belongs to another installation"
    );
    let original = command.first().context("Missing original Claude command")?;
    // Native supervisors cache their launcher's path. That path dispatches to
    // the current Harness binary, so select its immutable assets here instead
    // of pinning every future worker to the supervisor's original package.
    let current = {
        let _lock = setup_lock_for(&root, Duration::from_secs(2))?;
        let (_, settings) = read_settings(&configuration.join("settings.json"))?;
        if owned_launcher(&configuration, &root, &settings)?.is_none() {
            return Ok(None);
        }
        ensure!(
            std::env::var_os("BUN_OPTIONS").is_none(),
            "An existing Bun startup hook prevents Harness attachment"
        );
        package(
            &configuration,
            &root,
            &fs::canonicalize(std::env::current_exe()?)?,
            &executable_on_path("node")?,
        )?
    };
    let specification = compatibility(&current, Path::new(original))?;
    let directory = endpoint_root(&configuration)?;
    private_directory(&directory)?;
    let preload = current.package.join("supervisor_preload.mjs");
    ensure!(
        !preload.to_string_lossy().contains(char::is_whitespace),
        "Native preload paths containing whitespace are not supported"
    );
    let bootstrap =
        json!({"directory":directory,"configuration":configuration,"discovery":specification});
    publish_file(
        &directory.join("last-startup.json"),
        &serde_json::to_vec(&json!({"state":"prepared", "at":now_ms()}))?,
        true,
    )?;
    Ok(Some((bootstrap, preload)))
}

pub fn wrap(manifest_path: &Path, command: Vec<OsString>) -> anyhow::Result<()> {
    let original = command
        .first()
        .context("Native wrapper requires the original command")?;
    ensure!(
        Path::new(original).is_absolute(),
        "Native wrapper command must be absolute"
    );
    let mut native = Command::new(original);
    native.args(command.iter().skip(1));
    let environment =
        read_manifest(manifest_path).and_then(|manifest| worker_environment(&manifest, &command));
    match environment {
        Ok(Some((bootstrap, preload))) => {
            native.env("BUN_OPTIONS", format!("--preload {}", preload.display()));
            native.env(
                "HARNESS_CLAUDE_SUPERVISOR_BOOTSTRAP",
                serde_json::to_string(&bootstrap)?,
            );
        }
        Ok(None) => {}
        Err(error) => {
            // An unsupported optional UI adapter must not break native Claude.
            // Do not print before exec: Claude treats that as a startup failure.
            if let Err(report_error) = (|| -> anyhow::Result<()> {
                let configuration = fs::canonicalize(discovery::configuration_directory()?)?;
                let directory = endpoint_root(&configuration)?;
                private_directory(&directory)?;
                publish_file(
                    &directory.join("last-startup.json"),
                    &serde_json::to_vec(
                        &json!({"state":"unavailable", "detail":format!("{error:#}"), "at":now_ms()}),
                    )?,
                    true,
                )
            })() {
                log::debug!("Could not record native adapter failure: {report_error}");
            }
        }
    }
    Err(native.exec().into())
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    #[test]
    fn setup_preserves_settings_and_keeps_exact_backups() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let root = installation_root(&fixture.0)?;
        private_directory(&root)?;
        let original =
            b"{\"theme\":\"dark\",\"env\":{\"EXAMPLE\":\"kept\"},\"permissions\":{\"allow\":[]}}\n";
        let path = fixture.0.join("settings.json");
        write_new(&path, original)?;
        let (before, mut settings) = read_settings(&path)?;
        settings["processWrapper"] = json!("launcher");
        commit_settings(&fixture.0, before.as_deref(), &settings, &root)?;
        let (_, result) = read_settings(&path)?;
        assert_eq!(result["theme"], "dark");
        assert_eq!(result["env"]["EXAMPLE"], "kept");
        assert_eq!(result["permissions"], json!({"allow":[]}));
        let backup = fs::read_dir(root)?
            .next()
            .context("Missing backup")??
            .path();
        assert_eq!(fs::read(backup)?, original);
        Ok(())
    }

    #[test]
    fn setup_refuses_concurrent_settings_edits_without_overwriting_them() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let root = installation_root(&fixture.0)?;
        private_directory(&root)?;
        let path = fixture.0.join("settings.json");
        write_new(&path, b"{}")?;
        let (before, _) = read_settings(&path)?;
        publish_file(&path, b"{\"changed\":true}", true)?;
        assert!(
            commit_settings(
                &fixture.0,
                before.as_deref(),
                &json!({"processWrapper":"ours"}),
                &root
            )
            .is_err()
        );
        assert_eq!(fs::read(&path)?, b"{\"changed\":true}");
        assert_eq!(fs::read_dir(root)?.count(), 0);
        Ok(())
    }

    #[test]
    fn setup_does_not_replace_corporate_or_bun_hooks() {
        for settings in [
            json!({"processWrapper":"corporate"}),
            json!({"processWrapper":false}),
            json!({"env":{"CLAUDE_CODE_PROCESS_WRAPPER":"corporate"}}),
            json!({"env":{"BUN_OPTIONS":"--preload another"}}),
        ] {
            assert!(conflicting_settings(&settings, Some("ours")).is_err());
        }
        assert!(conflicting_settings(&json!({}), None).is_ok());
        assert!(
            conflicting_settings(
                &json!({"processWrapper":"ours","model":"unchanged"}),
                Some("ours")
            )
            .is_ok()
        );
    }

    #[test]
    fn immutable_package_is_reused_and_tampering_is_not_overwritten() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let root = installation_root(&fixture.0)?;
        let executable = Path::new("/nonexistent/harness");
        let node = Path::new("/usr/bin/node");
        let first = package(&fixture.0, &root, executable, node)?;
        assert_eq!(first, package(&fixture.0, &root, executable, node)?);
        publish_file(&first.package.join("bridge.mjs"), b"modified", true)?;
        assert!(package(&fixture.0, &root, executable, node).is_err());
        assert_eq!(fs::read(first.package.join("bridge.mjs"))?, b"modified");
        Ok(())
    }

    #[test]
    fn launcher_falls_back_to_native_when_harness_is_uninstalled() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let root = installation_root(&fixture.0)?;
        let manifest = package(
            &fixture.0,
            &root,
            Path::new("/nonexistent/harness"),
            Path::new("/usr/bin/node"),
        )?;
        let mut child = Command::new(manifest.package.join("launcher.sh"))
            .args([
                "/bin/sh",
                "-c",
                "printf '%s %s' \"$$\" \"$HARNESS_TEST_VALUE\"",
            ])
            .env("HARNESS_TEST_VALUE", "preserved")
            .stdout(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        let mut output = String::new();
        child
            .stdout
            .take()
            .context("Missing launcher output")?
            .read_to_string(&mut output)?;
        assert!(child.wait()?.success());
        assert_eq!(output, format!("{pid} preserved"));
        Ok(())
    }

    #[test]
    fn package_registry_retains_ownership_across_an_interrupted_upgrade() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let root = installation_root(&fixture.0)?;
        let first = package(
            &fixture.0,
            &root,
            Path::new("/old/harness"),
            Path::new("/usr/bin/node"),
        )?;
        let second = package(
            &fixture.0,
            &root,
            Path::new("/new/harness"),
            Path::new("/usr/bin/node"),
        )?;
        write_new(
            &root.join("installed.json"),
            &serde_json::to_vec(&[
                first.package.join("manifest.json"),
                second.package.join("manifest.json"),
            ])?,
        )?;
        let first_value = launcher_value(&first)?;
        let second_value = launcher_value(&second)?;
        for value in [first_value, second_value] {
            assert_eq!(
                owned_launcher(&fixture.0, &root, &json!({"processWrapper":value}))?,
                Some(value)
            );
        }
        assert!(
            owned_launcher(
                &fixture.0,
                &root,
                &json!({"processWrapper":"somebody-else"})
            )?
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn setup_lock_excludes_another_frontend_and_releases_on_drop() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let root = installation_root(&fixture.0)?;
        let lock = setup_lock(&root)?;
        assert!(setup_lock(&root).is_err());
        drop(lock);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match setup_lock(&root) {
                Ok(_lock) => break,
                Err(error) => {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "Lock was not released: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn unsafe_settings_and_cross_profile_endpoints_are_rejected() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let original = fixture.0.join("original.json");
        write_new(&original, b"{}")?;
        let path = fixture.0.join("settings.json");
        std::os::unix::fs::symlink(&original, &path)?;
        assert!(read_settings(&path).is_err());
        assert_eq!(fs::read(original)?, b"{}");
        assert_ne!(
            profile_key(Path::new("/one")),
            profile_key(Path::new("/two"))
        );
        assert_eq!(shell_quote(Path::new("/path/a'b"))?, "'/path/a'\\''b'");
        Ok(())
    }

    #[test]
    fn worker_package_wait_is_bounded_and_can_follow_another_installer() -> anyhow::Result<()> {
        let fixture = Fixture::new()?;
        let root = installation_root(&fixture.0)?;
        let lock = setup_lock(&root)?;
        assert!(setup_lock_for(&root, Duration::from_millis(10)).is_err());
        let (sender, receiver) = std::sync::mpsc::channel();
        let installer = std::thread::spawn(move || -> anyhow::Result<File> {
            sender.send(())?;
            setup_lock_for(&root, Duration::from_secs(2))
        });
        receiver.recv_timeout(Duration::from_secs(1))?;
        drop(lock);
        let _lock = installer
            .join()
            .map_err(|_| anyhow::anyhow!("Installer thread panicked"))??;
        Ok(())
    }
}
