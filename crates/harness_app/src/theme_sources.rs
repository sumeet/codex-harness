use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::ArchiveBuilder;
use futures::io::{AsyncReadExt as _, BufReader};
use http_client::{AsyncBody, HttpClient};
use serde::Deserialize;
use theme::ThemeRegistry;

const ZED_THEME_CATALOG_URL: &str =
    "https://api.zed.dev/extensions?max_schema_version=1&provides=themes";
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_THEME_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct ThemeCatalogEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) download_count: u64,
}

#[derive(Debug, Deserialize)]
struct ThemeCatalogResponse {
    data: Vec<ThemeCatalogEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InstalledThemePack {
    pub(crate) theme_files: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ExternalThemeLoadReport {
    pub(crate) files_loaded: usize,
    pub(crate) themes_added: usize,
    pub(crate) errors: Vec<String>,
}

/// Load loose Harness themes and theme packs installed by Zed.
///
/// Bundled themes are registered before this function is called. Zed sources
/// are loaded next and Harness's own directory is loaded last, so a deliberate
/// local override wins deterministically when two packs use the same name.
pub(crate) fn load_external_themes(registry: &ThemeRegistry) -> ExternalThemeLoadReport {
    let mut roots = zed_theme_roots();
    if let Some(harness_root) = harness_theme_dir() {
        if let Err(error) = fs::create_dir_all(&harness_root) {
            log::warn!(
                "could not create Harness theme directory {}: {error}",
                harness_root.display()
            );
        }
        roots.push(harness_root);
    }
    load_external_theme_roots(registry, roots)
}

pub(crate) fn harness_theme_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("harness").join("themes"))
}

pub(crate) fn installed_harness_theme_packs() -> HashSet<String> {
    let Some(root) = harness_theme_dir() else {
        return HashSet::new();
    };
    let Ok(entries) = fs::read_dir(root) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let mut themes = Vec::new();
            collect_theme_json(&path, 0, &mut themes);
            (!themes.is_empty()).then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect()
}

pub(crate) async fn fetch_theme_catalog(
    client: Arc<dyn HttpClient>,
) -> anyhow::Result<Vec<ThemeCatalogEntry>> {
    let mut response = client
        .get(ZED_THEME_CATALOG_URL, AsyncBody::default(), true)
        .await
        .context("requesting the Zed theme catalog")?;
    if !response.status().is_success() {
        bail!("Zed theme catalog returned HTTP {}", response.status());
    }

    if response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_CATALOG_BYTES)
    {
        bail!("Zed theme catalog is unexpectedly large");
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .take((MAX_CATALOG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .context("reading the Zed theme catalog")?;
    if bytes.len() > MAX_CATALOG_BYTES {
        bail!("Zed theme catalog is unexpectedly large");
    }

    parse_theme_catalog(&bytes)
}

fn parse_theme_catalog(bytes: &[u8]) -> anyhow::Result<Vec<ThemeCatalogEntry>> {
    let mut entries = serde_json::from_slice::<ThemeCatalogResponse>(bytes)
        .context("decoding the Zed theme catalog")?
        .data;
    entries.retain(|entry| !entry.id.trim().is_empty() && !entry.name.trim().is_empty());
    entries.sort_unstable_by(|left, right| {
        right
            .download_count
            .cmp(&left.download_count)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

pub(crate) async fn install_theme_pack(
    client: Arc<dyn HttpClient>,
    extension_id: &str,
) -> anyhow::Result<InstalledThemePack> {
    validate_extension_id(extension_id)?;
    let root = harness_theme_dir().context("the Harness theme directory is unavailable")?;
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    let url =
        format!("https://api.zed.dev/extensions/{extension_id}/download?max_schema_version=1");
    let mut response = client
        .get(&url, AsyncBody::default(), true)
        .await
        .with_context(|| format!("downloading theme pack {extension_id}"))?;
    if !response.status().is_success() {
        bail!(
            "theme pack {extension_id} returned HTTP {}",
            response.status()
        );
    }
    if response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_THEME_ARCHIVE_BYTES)
    {
        bail!("theme pack {extension_id} is unexpectedly large");
    }

    let mut bytes = Vec::new();
    response
        .body_mut()
        .take((MAX_THEME_ARCHIVE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("reading theme pack {extension_id}"))?;
    if bytes.len() > MAX_THEME_ARCHIVE_BYTES {
        bail!("theme pack {extension_id} is unexpectedly large");
    }

    let staging = root.join(format!(".install-{extension_id}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let unpack_result = async {
        let decoder = GzipDecoder::new(BufReader::new(bytes.as_slice()));
        ArchiveBuilder::new(decoder)
            .set_preserve_mtime(false)
            .build()
            .unpack(&staging)
            .await
            .with_context(|| format!("unpacking theme pack {extension_id}"))?;

        let mut theme_files = Vec::new();
        collect_theme_json(&staging, 0, &mut theme_files);
        if theme_files.is_empty() {
            bail!("theme pack {extension_id} contains no theme JSON");
        }

        replace_installed_pack(&staging, &root.join(extension_id), extension_id)?;
        Ok(InstalledThemePack {
            theme_files: theme_files.len(),
        })
    }
    .await;
    if staging.exists() {
        _ = fs::remove_dir_all(&staging);
    }
    unpack_result
}

fn validate_extension_id(extension_id: &str) -> anyhow::Result<()> {
    if extension_id.is_empty()
        || !extension_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid theme extension id {extension_id:?}");
    }
    Ok(())
}

fn replace_installed_pack(
    staging: &Path,
    destination: &Path,
    extension_id: &str,
) -> anyhow::Result<()> {
    let backup =
        destination.with_file_name(format!(".backup-{extension_id}-{}", uuid::Uuid::new_v4()));
    if destination.exists() {
        fs::rename(destination, &backup).with_context(|| {
            format!(
                "moving the previous theme pack from {} to {}",
                destination.display(),
                backup.display()
            )
        })?;
    }

    if let Err(error) = fs::rename(staging, destination) {
        if backup.exists() {
            _ = fs::rename(&backup, destination);
        }
        return Err(error)
            .with_context(|| format!("installing the theme pack at {}", destination.display()));
    }
    if backup.exists() {
        fs::remove_dir_all(&backup)
            .with_context(|| format!("removing old theme pack at {}", backup.display()))?;
    }
    Ok(())
}

fn zed_theme_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(config) = dirs::config_dir() {
        for application in ["zed", "zed-preview"] {
            roots.push(config.join(application).join("themes"));
        }
    }
    if let Some(data) = dirs::data_dir() {
        for application in ["zed", "zed-preview"] {
            roots.push(data.join(application).join("extensions").join("installed"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        for application in ["dev.zed.Zed", "dev.zed.Zed-Preview"] {
            let flatpak = home.join(".var").join("app").join(application);
            roots.push(flatpak.join("config").join("zed").join("themes"));
            roots.push(
                flatpak
                    .join("data")
                    .join("zed")
                    .join("extensions")
                    .join("installed"),
            );
        }
    }
    roots
}

fn load_external_theme_roots(
    registry: &ThemeRegistry,
    roots: impl IntoIterator<Item = PathBuf>,
) -> ExternalThemeLoadReport {
    let names_before = registry.list_names().into_iter().collect::<HashSet<_>>();
    let mut report = ExternalThemeLoadReport::default();
    let mut visited = HashSet::new();

    for root in roots {
        let mut paths = Vec::new();
        collect_theme_json(&root, 0, &mut paths);
        paths.sort();
        for path in paths {
            let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !visited.insert(identity) {
                continue;
            }
            match fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| theme_settings::load_user_theme(registry, &bytes))
            {
                Ok(()) => report.files_loaded += 1,
                Err(error) => report.errors.push(format!("{}: {error:#}", path.display())),
            }
        }
    }

    report.themes_added = registry
        .list_names()
        .into_iter()
        .filter(|name| !names_before.contains(name))
        .count();
    report
}

fn collect_theme_json(directory: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    // Installed extension layouts are `installed/<id>/themes/*.json`; four
    // levels also leave room for a pack to group variants without permitting
    // an accidental walk over an unrelated data tree.
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_theme_json(&path, depth + 1, output);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            && path
                .components()
                .any(|component| component.as_os_str() == "themes")
        {
            output.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assets::Assets;
    use gpui::AssetSource as _;
    use uuid::Uuid;

    #[test]
    fn installed_zed_pack_is_loaded_from_its_themes_directory() {
        let root = std::env::temp_dir().join(format!("harness-theme-test-{}", Uuid::new_v4()));
        let themes = root.join("installed").join("nord").join("themes");
        fs::create_dir_all(&themes).unwrap();
        let bytes = Assets
            .load("themes/nord/nord.json")
            .unwrap()
            .expect("bundled Nord theme");
        fs::write(themes.join("nord.json"), bytes).unwrap();

        let registry = ThemeRegistry::new(Box::new(()));
        let report = load_external_theme_roots(&registry, [root.clone()]);
        assert_eq!(report.files_loaded, 1);
        assert!(report.errors.is_empty());
        assert!(registry.get("Nord Dark").is_ok());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrelated_json_files_are_not_treated_as_themes() {
        let root = std::env::temp_dir().join(format!("harness-theme-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("preferences.json"), b"{}").unwrap();

        let registry = ThemeRegistry::new(Box::new(()));
        let report = load_external_theme_roots(&registry, [root.clone()]);
        assert_eq!(report.files_loaded, 0);
        assert!(report.errors.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn catalog_is_sorted_by_popularity_and_ignores_invalid_entries() {
        let entries = parse_theme_catalog(
            br#"{
                "data": [
                    {"id":"quiet", "name":"Quiet", "download_count":12},
                    {"id":"popular", "name":"Popular", "download_count":9000},
                    {"id":"", "name":"Invalid", "download_count":99999}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["popular", "quiet"]
        );
    }

    #[test]
    fn marketplace_extension_ids_cannot_escape_the_theme_directory() {
        assert!(validate_extension_id("tokyo-night").is_ok());
        assert!(validate_extension_id("../../outside").is_err());
        assert!(validate_extension_id("has/slash").is_err());
        assert!(validate_extension_id("").is_err());
    }
}
