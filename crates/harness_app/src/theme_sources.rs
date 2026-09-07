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

use crate::visual_theme::HarnessVisualTheme;

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

pub(crate) fn export_appearance_catalog(path: &Path) -> anyhow::Result<()> {
    let registry = ThemeRegistry::new(Box::new(assets::Assets));
    theme_settings::load_bundled_themes(&registry);
    let report = load_external_themes(&registry);
    if !report.errors.is_empty() {
        bail!("theme catalog is incomplete:\n{}", report.errors.join("\n"));
    }
    let themes = registry
        .list_names()
        .iter()
        .map(|name| appearance_snapshot(registry.get(name)?.as_ref()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let catalog = serde_json::json!({
        "schema_version": 1,
        "color_encoding": "unpremultiplied sRGB RGBA floats",
        "scope": "Resolved ThemeColors, syntax, Harness surfaces, selected status roles, and local player; not an exhaustive rendering equivalence test",
        "external_files_loaded": report.files_loaded,
        "themes": themes,
    });
    // An audit must not overwrite an earlier capture or a user's theme file.
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating appearance export {}", path.display()))?;
    serde_json::to_writer_pretty(file, &catalog).context("writing appearance export")
}

fn appearance_snapshot(theme: &theme::Theme) -> anyhow::Result<serde_json::Value> {
    let rgba = |color: gpui::Hsla| {
        let color = gpui::Rgba::from(color);
        [color.r, color.g, color.b, color.a]
    };
    let colors = theme
        .colors()
        .iter()
        .map(|(field, color)| (field.as_ref().to_owned(), serde_json::json!(rgba(color))))
        .collect::<serde_json::Map<_, _>>();
    let mut syntax = serde_json::Map::new();
    let mut index = 0usize;
    while let Some(highlight) = theme.syntax().get(index) {
        let name = theme
            .syntax()
            .get_capture_name(index)
            .context("syntax highlight is missing its capture name")?;
        syntax.insert(
            name.to_owned(),
            serde_json::json!({
                "color": highlight.color.map(rgba),
                "background": highlight.background_color.map(rgba),
                "font_style": highlight.font_style,
                "font_weight": highlight.font_weight.map(|weight| weight.0),
            }),
        );
        index += 1;
    }
    let status = theme.status();
    let visual = HarnessVisualTheme::from_zed(theme.colors(), status);
    Ok(serde_json::json!({
        "name": theme.name.as_ref(),
        "appearance": if theme.appearance().is_light() { "light" } else { "dark" },
        "colors": colors,
        "syntax": syntax,
        "window_background": format!("{:?}", theme.window_background_appearance()),
        "harness": {
            "canvas": rgba(visual.canvas), "transcript": rgba(visual.transcript),
            "rail": rgba(visual.rail), "raised_surface": rgba(visual.raised_surface),
            "tool_surface": rgba(visual.tool_surface), "tool_header_surface": rgba(visual.tool_header_surface),
            "tool_border": rgba(visual.tool_border), "pending_surface": rgba(visual.pending_surface),
            "error_surface": rgba(visual.error_surface), "error_border": rgba(visual.error_border),
            "selection_surface": rgba(visual.selection_surface),
            "diff_added_surface": rgba(visual.diff_added_surface),
            "diff_deleted_surface": rgba(visual.diff_deleted_surface),
            "divider": rgba(visual.divider), "strong_divider": rgba(visual.strong_divider),
        },
        "status": {
            "error": rgba(status.error), "warning": rgba(status.warning),
            "success": rgba(status.success), "info": rgba(status.info),
            "hint": rgba(status.hint), "error_background": rgba(status.error_background),
            "warning_background": rgba(status.warning_background),
            "success_background": rgba(status.success_background),
        },
        "local_player": theme.players().0.first().map(|player| serde_json::json!({
            "cursor": rgba(player.cursor), "selection": rgba(player.selection),
            "background": rgba(player.background),
        })),
    }))
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
            && !path
                .components()
                .any(|component| component.as_os_str() == "icon_themes")
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
    fn appearance_export_preserves_resolved_surfaces_and_syntax_assignments() {
        let registry = ThemeRegistry::new(Box::new(()));
        theme_settings::load_user_theme(&registry, br##"{
            "name": "Audit", "author": "Fixture", "themes": [{
                "name": "Audit", "appearance": "dark", "style": {
                    "editor.background": "#102030", "surface.background": "#304050",
                    "syntax": {"function": {"color": "#abcdef", "font_style": "italic", "font_weight": 700}}
                }
            }]
        }"##).expect("valid audit fixture");
        let theme = registry.get("Audit").expect("audit theme");
        let snapshot = appearance_snapshot(&theme).expect("snapshot");
        assert_eq!(
            snapshot["harness"]["transcript"],
            snapshot["colors"]["editor_background"]
        );
        assert_eq!(
            snapshot["harness"]["tool_surface"],
            snapshot["harness"]["tool_header_surface"]
        );
        assert_ne!(
            snapshot["harness"]["tool_surface"],
            snapshot["harness"]["transcript"]
        );
        assert_eq!(snapshot["syntax"]["function"]["font_style"], "Italic");
        assert_eq!(snapshot["syntax"]["function"]["font_weight"], 700.0);
        assert!(
            snapshot["colors"]["text"].is_array(),
            "omitted roles must be resolved"
        );
        assert!(
            snapshot.get("id").is_none(),
            "random runtime IDs must not enter the audit"
        );
    }

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
        // Harness installs the extension catalog under its own `themes`
        // directory, so a component test for `themes` alone also matches the
        // sibling Zed `icon_themes` payload unless it is excluded explicitly.
        let icon_themes = root.join("themes").join("min-theme").join("icon_themes");
        fs::create_dir_all(&icon_themes).unwrap();
        fs::write(
            icon_themes.join("min-icon-theme.json"),
            br#"{"name":"Min Icons","themes":[]}"#,
        )
        .unwrap();

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

    #[test]
    #[ignore = "contacts the live Zed extension catalog"]
    fn native_http_client_fetches_the_live_zed_catalog() {
        let client: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());
        let catalog = futures::executor::block_on(fetch_theme_catalog(client))
            .expect("fetch live Zed theme catalog");
        assert!(catalog.len() > 100, "unexpectedly small catalog");
        assert!(catalog.iter().any(|entry| entry.id == "catppuccin"));
    }
}
