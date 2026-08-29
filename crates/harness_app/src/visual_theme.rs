use std::{fs, path::PathBuf};

use anyhow::Context as _;
use gpui::Hsla;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use theme::ThemeColors;

pub(crate) const DEFAULT_HARNESS_THEME: &str = "One Dark";
pub(crate) const MIN_HARNESS_FONT_SIZE: f32 = 10.;
pub(crate) const MAX_HARNESS_FONT_SIZE: f32 = 28.;
pub(crate) const MIN_HARNESS_FONT_WEIGHT: f32 = 100.;
pub(crate) const MAX_HARNESS_FONT_WEIGHT: f32 = 900.;

/// The deliberately small set of Harness-owned appearance preferences.
///
/// Harness feeds these values into Zed's `ThemeSettings` rather than carrying
/// a parallel font system. Reading settings cover UI chrome, transcript prose,
/// and the composer; code settings cover command output, diffs, and Markdown
/// code spans. Keeping these as two semantic roles makes the useful choices
/// configurable without exposing every renderer's internal metric.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct HarnessPreferences {
    pub(crate) theme: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reading_font_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reading_font_size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reading_font_weight: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) code_font_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) code_font_size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) code_font_weight: Option<f32>,
}

impl Default for HarnessPreferences {
    fn default() -> Self {
        Self {
            theme: DEFAULT_HARNESS_THEME.to_owned(),
            reading_font_family: None,
            reading_font_size: None,
            reading_font_weight: None,
            code_font_family: None,
            code_font_size: None,
            code_font_weight: None,
        }
    }
}

impl HarnessPreferences {
    fn normalize(mut self) -> Self {
        self.theme = nonempty(self.theme).unwrap_or_else(|| DEFAULT_HARNESS_THEME.to_owned());
        self.reading_font_family = self.reading_font_family.and_then(nonempty);
        self.code_font_family = self.code_font_family.and_then(nonempty);
        self.reading_font_size = self.reading_font_size.and_then(normalize_font_size);
        self.code_font_size = self.code_font_size.and_then(normalize_font_size);
        self.reading_font_weight = self.reading_font_weight.and_then(normalize_font_weight);
        self.code_font_weight = self.code_font_weight.and_then(normalize_font_weight);
        self
    }

    pub(crate) fn settings_json(&self) -> String {
        let mut settings = Map::from_iter([
            ("vim_mode".to_owned(), Value::Bool(true)),
            ("theme".to_owned(), Value::String(self.theme.clone())),
        ]);
        if let Some(family) = &self.reading_font_family {
            settings.insert("ui_font_family".to_owned(), json!(family));
            settings.insert("agent_ui_font_family".to_owned(), json!(family));
        }
        if let Some(size) = self.reading_font_size {
            settings.insert("ui_font_size".to_owned(), json!(size));
            settings.insert("agent_ui_font_size".to_owned(), json!(size));
        }
        if let Some(weight) = self.reading_font_weight {
            settings.insert("ui_font_weight".to_owned(), json!(weight));
        }
        if let Some(family) = &self.code_font_family {
            settings.insert("buffer_font_family".to_owned(), json!(family));
            settings.insert("agent_buffer_font_family".to_owned(), json!(family));
        }
        if let Some(size) = self.code_font_size {
            settings.insert("buffer_font_size".to_owned(), json!(size));
            settings.insert("agent_buffer_font_size".to_owned(), json!(size));
        }
        if let Some(weight) = self.code_font_weight {
            settings.insert("buffer_font_weight".to_owned(), json!(weight));
        }
        Value::Object(settings).to_string()
    }

    pub(crate) fn reset_typography(&mut self) {
        self.reading_font_family = None;
        self.reading_font_size = None;
        self.reading_font_weight = None;
        self.code_font_family = None;
        self.code_font_size = None;
        self.code_font_weight = None;
    }
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn normalize_font_size(size: f32) -> Option<f32> {
    size.is_finite()
        .then(|| size.clamp(MIN_HARNESS_FONT_SIZE, MAX_HARNESS_FONT_SIZE))
}

fn normalize_font_weight(weight: f32) -> Option<f32> {
    weight.is_finite().then(|| {
        (weight.clamp(MIN_HARNESS_FONT_WEIGHT, MAX_HARNESS_FONT_WEIGHT) / 100.).round() * 100.
    })
}

fn preferences_path() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("harness").join("preferences.json"))
}

fn preferences_from_contents(contents: &str) -> Option<HarnessPreferences> {
    serde_json::from_str::<HarnessPreferences>(contents)
        .ok()
        .map(HarnessPreferences::normalize)
}

/// Returns the persisted appearance with an optional launch-only theme
/// override. The environment variable remains useful for replay screenshots,
/// but ordinary users never need it.
pub(crate) fn preferred_preferences() -> HarnessPreferences {
    let mut preferences = preferences_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| preferences_from_contents(&contents))
        .unwrap_or_default();
    if let Some(theme) = std::env::var("HARNESS_THEME").ok().and_then(nonempty) {
        preferences.theme = theme;
    }
    preferences
}

/// Persists only Harness-owned preferences rather than mutating Zed's user
/// settings file.
pub(crate) fn remember_preferences(preferences: &HarnessPreferences) -> anyhow::Result<()> {
    let path = preferences_path().context("no user configuration directory is available")?;
    let parent = path
        .parent()
        .context("Harness preferences path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let contents = serde_json::to_string_pretty(preferences)?;
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
    pub(crate) raised_surface: Hsla,
    pub(crate) tool_header_surface: Hsla,
    pub(crate) pending_surface: Hsla,
    pub(crate) error_surface: Hsla,
    pub(crate) error_border: Hsla,
    pub(crate) selection_surface: Hsla,
    pub(crate) diff_added_surface: Hsla,
    pub(crate) diff_deleted_surface: Hsla,
    pub(crate) divider: Hsla,
    pub(crate) strong_divider: Hsla,
}

impl HarnessVisualTheme {
    pub(crate) fn from_zed(colors: &ThemeColors) -> Self {
        Self {
            canvas: colors.background,
            transcript: colors.editor_background,
            rail: colors.panel_background,
            raised_surface: colors.surface_background,
            // Match the header wash used by Zed's agent tool cards. This is
            // intentionally subtler than `surface_background`: it separates
            // identity from output without turning every tool into a banner.
            tool_header_surface: colors
                .element_background
                .blend(colors.editor_foreground.opacity(0.025)),
            pending_surface: colors
                .editor_background
                .blend(colors.surface_background.opacity(0.86)),
            error_surface: colors
                .editor_background
                .blend(colors.version_control_deleted.opacity(0.12)),
            error_border: colors.version_control_deleted.opacity(0.58),
            selection_surface: colors.element_selection_background,
            // Use the same surfaces painted by Zed's native Editor for
            // expanded BufferDiff hunks. Version-control tints are intended
            // for compact status glyphs and produce a visibly harsher card.
            diff_added_surface: colors.editor_diff_hunk_added_background,
            diff_deleted_surface: colors.editor_diff_hunk_deleted_background,
            divider: colors.border_variant.opacity(0.82),
            strong_divider: colors.border,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_theme_only_preferences_still_load() {
        assert_eq!(
            preferences_from_contents(r#"{ "theme": "Ayu Mirage" }"#)
                .unwrap()
                .theme,
            "Ayu Mirage"
        );
        assert_eq!(
            preferences_from_contents(r#"{ "theme": "  " }"#)
                .unwrap()
                .theme,
            DEFAULT_HARNESS_THEME
        );
        assert!(preferences_from_contents(r#"{ "theme": 3 }"#).is_none());
        assert!(preferences_from_contents("not json").is_none());
    }

    #[test]
    fn typography_preferences_feed_both_zed_font_roles() {
        let preferences = HarnessPreferences {
            theme: "Ayu Mirage".to_owned(),
            reading_font_family: Some("Inter".to_owned()),
            reading_font_size: Some(16.),
            reading_font_weight: Some(300.),
            code_font_family: Some("Zed Mono".to_owned()),
            code_font_size: Some(15.),
            code_font_weight: Some(500.),
        };
        let settings: Value = serde_json::from_str(&preferences.settings_json()).unwrap();
        assert_eq!(settings["ui_font_family"], "Inter");
        assert_eq!(settings["agent_ui_font_family"], "Inter");
        assert_eq!(settings["buffer_font_family"], "Zed Mono");
        assert_eq!(settings["agent_buffer_font_family"], "Zed Mono");
        assert_eq!(settings["ui_font_size"], 16.);
        assert_eq!(settings["ui_font_weight"], 300.);
        assert_eq!(settings["agent_buffer_font_size"], 15.);
        assert_eq!(settings["buffer_font_weight"], 500.);
    }

    #[test]
    fn invalid_font_sizes_are_discarded_or_clamped() {
        let preferences =
            preferences_from_contents(r#"{ "reading_font_size": 2, "code_font_size": 200 }"#)
                .unwrap();
        assert_eq!(preferences.reading_font_size, Some(MIN_HARNESS_FONT_SIZE));
        assert_eq!(preferences.code_font_size, Some(MAX_HARNESS_FONT_SIZE));
    }

    #[test]
    fn font_weights_are_snapped_to_supported_css_steps() {
        let preferences =
            preferences_from_contents(r#"{ "reading_font_weight": 249, "code_font_weight": 950 }"#)
                .unwrap();
        assert_eq!(preferences.reading_font_weight, Some(200.));
        assert_eq!(preferences.code_font_weight, Some(MAX_HARNESS_FONT_WEIGHT));
    }

    #[test]
    fn semantic_theme_is_derived_only_from_zed_theme_colors() {
        // Keep this module intentionally data-only. A compile-time construction
        // test is more useful here than hard-coding values from one dark theme.
        let derive: fn(&ThemeColors) -> HarnessVisualTheme = HarnessVisualTheme::from_zed;
        let _ = derive;
    }
}
