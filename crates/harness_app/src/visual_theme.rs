use std::{fs, path::PathBuf};

use anyhow::Context as _;
use gpui::Hsla;
use serde_json::{Value, json};
use theme::ThemeColors;

pub(crate) const DEFAULT_HARNESS_THEME: &str = "One Dark";

fn preferences_path() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("harness").join("preferences.json"))
}

fn theme_name_from_preferences(contents: &str) -> Option<String> {
    serde_json::from_str::<Value>(contents)
        .ok()?
        .get("theme")?
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

/// Returns an explicit launch override, then the last in-app selection, then
/// Harness's bundled default. The environment variable remains useful for
/// replay screenshots, but ordinary users never need it.
pub(crate) fn preferred_theme_name() -> String {
    std::env::var("HARNESS_THEME")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            let path = preferences_path()?;
            let contents = fs::read_to_string(path).ok()?;
            theme_name_from_preferences(&contents)
        })
        .unwrap_or_else(|| DEFAULT_HARNESS_THEME.to_owned())
}

/// Persists only Harness-owned preferences rather than mutating Zed's user
/// settings file. A later settings surface can extend this small object.
pub(crate) fn remember_theme_name(theme_name: &str) -> anyhow::Result<()> {
    let path = preferences_path().context("no user configuration directory is available")?;
    let parent = path
        .parent()
        .context("Harness preferences path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let contents = serde_json::to_string_pretty(&json!({ "theme": theme_name }))?;
    fs::write(&path, contents).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// Harness's semantic visual vocabulary.
///
/// Components should describe their role instead of independently choosing a
/// convenient Zed token. Keeping this translation in one place lets every Zed
/// theme remain authoritative while making the transcript, queue, and composer
/// read as one application rather than several locally styled widgets.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HarnessVisualTheme {
    pub(crate) canvas: Hsla,
    pub(crate) transcript: Hsla,
    pub(crate) rail: Hsla,
    pub(crate) composer: Hsla,
    pub(crate) pending_surface: Hsla,
    pub(crate) activity_surface: Hsla,
    pub(crate) divider: Hsla,
    pub(crate) strong_divider: Hsla,
    pub(crate) focus_wash: Hsla,
}

impl HarnessVisualTheme {
    pub(crate) fn from_zed(colors: &ThemeColors) -> Self {
        Self {
            canvas: colors.background,
            transcript: colors.editor_background,
            rail: colors.panel_background,
            // The composer is part of the document's working surface, but its
            // slightly raised subheader tone separates it without a bright
            // focus-colored rule.
            composer: colors.editor_subheader_background,
            pending_surface: colors.surface_background,
            activity_surface: colors.editor_subheader_background,
            divider: colors.border_variant,
            strong_divider: colors.border,
            // Focus is a low-amplitude surface change. Explicit controls still
            // use the theme's ordinary focused/selected tokens.
            focus_wash: colors.element_selected.opacity(0.16),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_theme_requires_a_nonempty_string() {
        assert_eq!(
            theme_name_from_preferences(r#"{ "theme": "Ayu Mirage" }"#).as_deref(),
            Some("Ayu Mirage")
        );
        assert_eq!(theme_name_from_preferences(r#"{ "theme": "  " }"#), None);
        assert_eq!(theme_name_from_preferences(r#"{ "theme": 3 }"#), None);
        assert_eq!(theme_name_from_preferences("not json"), None);
    }

    #[test]
    fn semantic_theme_is_derived_only_from_zed_theme_colors() {
        // Keep this module intentionally data-only. A compile-time construction
        // test is more useful here than hard-coding values from one dark theme.
        let derive: fn(&ThemeColors) -> HarnessVisualTheme = HarnessVisualTheme::from_zed;
        let _ = derive;
    }
}
