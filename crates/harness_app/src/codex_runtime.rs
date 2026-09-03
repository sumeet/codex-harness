use std::{process::Stdio, time::Duration};

use anyhow::{Context as _, bail};
use async_process::Command;
use semver::Version;
use serde_json::Value;

pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableUpdate {
    pub installed_version: String,
    pub latest_version: String,
    pub update_action: Option<String>,
}

pub async fn check_for_update() -> anyhow::Result<Option<AvailableUpdate>> {
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
    parse_update_report(&output.stdout)
}

pub async fn install_update() -> anyhow::Result<()> {
    run_codex_command(&["update"]).await
}

pub async fn restart_app_server() -> anyhow::Result<()> {
    run_codex_command(&["app-server", "daemon", "restart"]).await
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

fn parse_update_report(bytes: &[u8]) -> anyhow::Result<Option<AvailableUpdate>> {
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
        return Ok(None);
    }

    Ok(Some(AvailableUpdate {
        installed_version: installed_version.to_owned(),
        latest_version: latest_version.to_owned(),
        update_action: update_details
            .get("update action")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }))
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
        let update = parse_update_report(&report("0.152.1", "0.153.0"))
            .expect("parse report")
            .expect("newer update");
        assert_eq!(update.installed_version, "0.152.1");
        assert_eq!(update.latest_version, "0.153.0");
        assert_eq!(
            update.update_action.as_deref(),
            Some("standalone installer")
        );
    }

    #[test]
    fn current_and_older_versions_do_not_offer_an_update() {
        assert!(
            parse_update_report(&report("0.153.0", "0.153.0"))
                .expect("parse current report")
                .is_none()
        );
        assert!(
            parse_update_report(&report("0.153.1", "0.153.0"))
                .expect("parse development report")
                .is_none()
        );
    }
}
