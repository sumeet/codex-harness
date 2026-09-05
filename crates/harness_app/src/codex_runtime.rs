use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail};
use async_process::Command;
use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, Signal, System, UpdateKind};

pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
pub const RESTART_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const UNMANAGED_SERVER_ERROR: &str =
    "app server is running but is not managed by codex app-server daemon";
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(70);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableUpdate {
    pub installed_version: String,
    pub latest_version: String,
    pub update_action: Option<String>,
    pub app_server_version: Option<String>,
    pub app_server_managed: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateCheck {
    Current,
    Available(AvailableUpdate),
    RestartRequired(AvailableUpdate),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonRuntime {
    #[serde(default)]
    backend: Option<String>,
    managed_codex_path: PathBuf,
    socket_path: PathBuf,
    cli_version: String,
    app_server_version: String,
}

pub async fn check_for_update() -> anyhow::Result<UpdateCheck> {
    let output = codex_command()
        .args(["doctor", "--json"])
        .output()
        .await
        .context("could not run `codex doctor --json`")?;
    if !output.status.success() {
        bail!(
            "`codex doctor --json` exited with {}: {}",
            output.status,
            command_error(&output.stderr, &output.stdout)
        );
    }
    let (installed_version, update) = parse_update_report(&output.stdout)?;
    if let Some(update) = update {
        return Ok(UpdateCheck::Available(update));
    }

    let Ok(runtime) = daemon_runtime().await else {
        return Ok(UpdateCheck::Current);
    };
    let Some(update) = restart_required_update(&installed_version, runtime)? else {
        return Ok(UpdateCheck::Current);
    };
    Ok(UpdateCheck::RestartRequired(update))
}

/// An installed update can be applied outside Harness. Check only the local
/// runtime while waiting for that restart, without repeating the network and
/// database work performed by `codex doctor`.
pub async fn check_for_restart() -> anyhow::Result<UpdateCheck> {
    let runtime = daemon_runtime().await?;
    let installed_version = runtime.cli_version.clone();
    Ok(
        match restart_required_update(&installed_version, runtime)? {
            Some(update) => UpdateCheck::RestartRequired(update),
            None => UpdateCheck::Current,
        },
    )
}

fn restart_required_update(
    installed_version: &str,
    runtime: DaemonRuntime,
) -> anyhow::Result<Option<AvailableUpdate>> {
    if runtime.cli_version != installed_version {
        bail!(
            "Codex doctor reported version {}, but the daemon command reported CLI version {}",
            installed_version,
            runtime.cli_version
        );
    }
    if runtime.app_server_version == installed_version {
        return Ok(None);
    }

    Ok(Some(AvailableUpdate {
        installed_version: installed_version.to_owned(),
        latest_version: installed_version.to_owned(),
        update_action: None,
        app_server_version: Some(runtime.app_server_version),
        app_server_managed: Some(runtime.backend.is_some()),
    }))
}

pub async fn install_update() -> anyhow::Result<()> {
    run_codex_command(&["update"]).await
}

pub async fn restart_app_server() -> anyhow::Result<()> {
    run_codex_command(&["app-server", "daemon", "restart"]).await
}

pub fn is_unmanaged_app_server_error(error: &anyhow::Error) -> bool {
    error.to_string().contains(UNMANAGED_SERVER_ERROR)
}

pub async fn replace_unmanaged_app_server() -> anyhow::Result<()> {
    let runtime = daemon_runtime().await?;
    if runtime.backend.is_some() {
        return run_codex_command(&["app-server", "daemon", "restart"]).await;
    }

    let owner_process_ids = socket_owner_process_ids(&runtime.socket_path).await?;
    let managed_codex_path = runtime.managed_codex_path;
    smol::unblock(move || stop_unmanaged_app_server(&managed_codex_path, &owner_process_ids))
        .await?;
    run_codex_command(&["app-server", "daemon", "start"]).await
}

async fn daemon_runtime() -> anyhow::Result<DaemonRuntime> {
    let output = codex_command()
        .args(["app-server", "daemon", "version"])
        .output()
        .await
        .context("could not inspect the running Codex App Server")?;
    if !output.status.success() {
        bail!(
            "`codex app-server daemon version` exited with {}: {}",
            output.status,
            command_error(&output.stderr, &output.stdout)
        );
    }
    serde_json::from_slice(&output.stdout).context("invalid App Server version report")
}

async fn socket_owner_process_ids(socket_path: &Path) -> anyhow::Result<Vec<u32>> {
    let mut last_not_found = None;
    for executable in ["lsof", "/usr/sbin/lsof", "/usr/bin/lsof"] {
        let output = match Command::new(executable)
            .args(["-t", "--"])
            .arg(socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_not_found = Some(error);
                continue;
            }
            Err(error) => return Err(error).context("could not locate the App Server process"),
        };
        if !output.status.success() {
            bail!(
                "could not locate the process listening on {}: {}",
                socket_path.display(),
                command_error(&output.stderr, &output.stdout)
            );
        }

        let process_ids = String::from_utf8(output.stdout)
            .context("lsof returned non-UTF-8 process identifiers")?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.parse::<u32>()
                    .with_context(|| format!("lsof returned invalid process identifier {line:?}"))
            })
            .collect::<anyhow::Result<BTreeSet<_>>>()?;
        if process_ids.is_empty() {
            bail!(
                "no process owns the App Server socket {}",
                socket_path.display()
            );
        }
        return Ok(process_ids.into_iter().collect());
    }

    Err(last_not_found
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("lsof is unavailable")))
    .context("replacing an unmanaged App Server requires `lsof`")
}

fn stop_unmanaged_app_server(
    managed_codex_path: &Path,
    owner_process_ids: &[u32],
) -> anyhow::Result<()> {
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always);
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    let candidates = owner_process_ids
        .iter()
        .filter_map(|process_id| {
            let process_id = sysinfo::Pid::from_u32(*process_id);
            system
                .process(process_id)
                .filter(|process| is_expected_app_server_command(process.cmd(), managed_codex_path))
                .map(|process| (process_id, process.start_time()))
        })
        .collect::<Vec<_>>();
    let [(process_id, process_start_time)] = candidates.as_slice() else {
        bail!(
            "refusing to stop an unverified socket owner; expected exactly one `{}` App Server, found {}",
            managed_codex_path.display(),
            candidates.len()
        );
    };

    let process_id = *process_id;
    let process_start_time = *process_start_time;
    let process = system
        .process(process_id)
        .context("the App Server process exited before it could be stopped")?;
    match process.kill_with(Signal::Term) {
        Some(true) => {}
        Some(false) => bail!("the operating system refused to stop the App Server"),
        None => bail!("this platform cannot terminate the unmanaged App Server"),
    }

    let started_at = Instant::now();
    let mut forced = false;
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[process_id]), true, refresh);
        let Some(process) = system.process(process_id) else {
            return Ok(());
        };
        if process.start_time() != process_start_time {
            return Ok(());
        }

        if started_at.elapsed() >= STOP_TIMEOUT {
            bail!("timed out waiting for the unmanaged App Server to stop");
        }
        if !forced && started_at.elapsed() >= STOP_GRACE_PERIOD {
            match process.kill_with(Signal::Kill) {
                Some(true) => forced = true,
                Some(false) => bail!("the operating system refused to kill the App Server"),
                None => bail!("this platform cannot kill the unmanaged App Server"),
            }
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
    }
}

fn is_expected_app_server_command(command: &[OsString], managed_codex_path: &Path) -> bool {
    let Some(executable) = command.first() else {
        return false;
    };
    if Path::new(executable) != managed_codex_path {
        return false;
    }

    command_arguments_equal(&command[1..], &["app-server", "--listen", "unix://"])
        || command_arguments_equal(
            &command[1..],
            &["app-server", "--remote-control", "--listen", "unix://"],
        )
}

fn command_arguments_equal(arguments: &[OsString], expected: &[&str]) -> bool {
    arguments.len() == expected.len()
        && arguments
            .iter()
            .zip(expected)
            .all(|(argument, expected)| argument == OsStr::new(expected))
}

fn codex_command() -> Command {
    let mut command = Command::new("codex");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn run_codex_command(arguments: &[&str]) -> anyhow::Result<()> {
    let output = codex_command()
        .args(arguments)
        .output()
        .await
        .with_context(|| format!("could not run `codex {}`", arguments.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`codex {}` exited with {}: {}",
            arguments.join(" "),
            output.status,
            command_error(&output.stderr, &output.stdout)
        );
    }
    Ok(())
}

fn command_error(stderr: &[u8], stdout: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = String::from_utf8_lossy(stdout);
    let stdout = stdout.trim();
    if stdout.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        stdout.to_owned()
    }
}

fn parse_update_report(bytes: &[u8]) -> anyhow::Result<(String, Option<AvailableUpdate>)> {
    let report: Value = serde_json::from_slice(bytes).context("invalid Codex doctor report")?;
    let installed_version = report
        .get("codexVersion")
        .and_then(Value::as_str)
        .context("Codex doctor report omitted codexVersion")?;
    let update_details = report
        .pointer("/checks/updates.status/details")
        .and_then(Value::as_object)
        .context("Codex doctor report omitted updates.status details")?;
    let latest_version = update_details
        .get("latest version")
        .and_then(Value::as_str)
        .context("Codex doctor report omitted the latest version")?;
    let installed = Version::parse(installed_version)
        .with_context(|| format!("invalid installed Codex version {installed_version:?}"))?;
    let latest = Version::parse(latest_version)
        .with_context(|| format!("invalid latest Codex version {latest_version:?}"))?;

    if latest <= installed {
        return Ok((installed_version.to_owned(), None));
    }

    Ok((
        installed_version.to_owned(),
        Some(AvailableUpdate {
            installed_version: installed_version.to_owned(),
            latest_version: latest_version.to_owned(),
            update_action: update_details
                .get("update action")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            app_server_version: None,
            app_server_managed: None,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report(installed: &str, latest: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "codexVersion": installed,
            "checks": {
                "updates.status": {
                    "details": {
                        "latest version": latest,
                        "latest version status": "newer version is available",
                        "update action": "standalone installer"
                    }
                }
            }
        }))
        .expect("serialize report")
    }

    #[test]
    fn doctor_report_exposes_a_newer_runtime() {
        let (installed, update) =
            parse_update_report(&report("0.152.1", "0.153.0")).expect("parse report");
        let update = update.expect("newer update");
        assert_eq!(installed, "0.152.1");
        assert_eq!(update.installed_version, "0.152.1");
        assert_eq!(update.latest_version, "0.153.0");
        assert_eq!(
            update.update_action.as_deref(),
            Some("standalone installer")
        );
        assert_eq!(update.app_server_version, None);
        assert_eq!(update.app_server_managed, None);
    }

    #[test]
    fn current_and_older_versions_do_not_offer_an_update() {
        assert!(
            parse_update_report(&report("0.153.0", "0.153.0"))
                .expect("parse current report")
                .1
                .is_none()
        );
        assert!(
            parse_update_report(&report("0.153.1", "0.153.0"))
                .expect("parse development report")
                .1
                .is_none()
        );
    }

    #[test]
    fn version_drift_exposes_an_unmanaged_server_replacement() {
        let runtime = DaemonRuntime {
            backend: None,
            managed_codex_path: PathBuf::from("/opt/codex/current/codex"),
            socket_path: PathBuf::from("/tmp/codex.sock"),
            cli_version: "0.153.0".into(),
            app_server_version: "0.152.1".into(),
        };
        let update = restart_required_update("0.153.0", runtime)
            .expect("consistent CLI version")
            .expect("outdated server");

        assert_eq!(update.latest_version, "0.153.0");
        assert_eq!(update.app_server_version.as_deref(), Some("0.152.1"));
        assert_eq!(update.app_server_managed, Some(false));
    }

    #[test]
    fn matching_app_server_version_requires_no_restart() {
        let runtime = DaemonRuntime {
            backend: Some("pid".into()),
            managed_codex_path: PathBuf::from("/opt/codex/current/codex"),
            socket_path: PathBuf::from("/tmp/codex.sock"),
            cli_version: "0.153.0".into(),
            app_server_version: "0.153.0".into(),
        };

        assert!(
            restart_required_update("0.153.0", runtime)
                .expect("consistent versions")
                .is_none()
        );
    }

    #[test]
    fn only_the_exact_managed_app_server_command_is_replaceable() {
        let managed = Path::new("/opt/codex/current/codex");
        let command = |arguments: &[&str]| {
            std::iter::once(managed.as_os_str().to_owned())
                .chain(arguments.iter().map(OsString::from))
                .collect::<Vec<_>>()
        };

        assert!(is_expected_app_server_command(
            &command(&["app-server", "--listen", "unix://"]),
            managed
        ));
        assert!(is_expected_app_server_command(
            &command(&["app-server", "--remote-control", "--listen", "unix://"]),
            managed
        ));
        assert!(!is_expected_app_server_command(
            &command(&["app-server", "--listen", "unix:///tmp/other.sock"]),
            managed
        ));
        assert!(!is_expected_app_server_command(
            &[
                OsString::from("/tmp/not-codex"),
                OsString::from("app-server"),
                OsString::from("--listen"),
                OsString::from("unix://"),
            ],
            managed
        ));
    }
}
