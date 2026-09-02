use std::{fs, path::PathBuf, sync::LazyLock};

use anyhow::{Context as _, bail};
use gpui::{Hsla, TextRenderingMode};
use serde::Deserialize;
use theme::ThemeColors;

use crate::visual_theme::{
    HarnessPreferences, MAX_HARNESS_FONT_SIZE, MAX_HARNESS_FONT_WEIGHT, MIN_HARNESS_FONT_SIZE,
    MIN_HARNESS_FONT_WEIGHT,
};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ComparisonProfile {
    pub(crate) name: Option<String>,
    pub(crate) transcript_background: Option<String>,
    pub(crate) transcript_foreground: Option<String>,
    pub(crate) reading: ComparisonFont,
    pub(crate) code: ComparisonFont,
    pub(crate) line_height: Option<f32>,
    pub(crate) text_rendering: Option<ComparisonTextRendering>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ComparisonFont {
    pub(crate) family: Option<String>,
    pub(crate) size: Option<f32>,
    pub(crate) weight: Option<f32>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ComparisonTextRendering {
    PlatformDefault,
    Subpixel,
    Grayscale,
}

impl From<ComparisonTextRendering> for TextRenderingMode {
    fn from(value: ComparisonTextRendering) -> Self {
        match value {
            ComparisonTextRendering::PlatformDefault => TextRenderingMode::PlatformDefault,
            ComparisonTextRendering::Subpixel => TextRenderingMode::Subpixel,
            ComparisonTextRendering::Grayscale => TextRenderingMode::Grayscale,
        }
    }
}

impl ComparisonProfile {
    fn parse(contents: &str) -> anyhow::Result<Self> {
        let mut profile: Self = serde_json::from_str(contents)?;
        profile.name = normalized_string(profile.name);
        profile.transcript_background = normalized_string(profile.transcript_background);
        profile.transcript_foreground = normalized_string(profile.transcript_foreground);
        profile.reading.normalize("reading")?;
        profile.code.normalize("code")?;
        if let Some(line_height) = profile.line_height
            && (!line_height.is_finite() || !(0.8..=2.4).contains(&line_height))
        {
            bail!("line_height must be a finite number from 0.8 through 2.4");
        }
        validate_color_spec(profile.transcript_background.as_deref(), true)?;
        validate_color_spec(profile.transcript_foreground.as_deref(), false)?;
        Ok(profile)
    }

    pub(crate) fn apply_typography(&self, preferences: &mut HarnessPreferences) {
        if let Some(family) = &self.reading.family {
            preferences.reading_font_family = Some(family.clone());
        }
        if let Some(size) = self.reading.size {
            preferences.reading_font_size = Some(size);
        }
        if let Some(weight) = self.reading.weight {
            preferences.reading_font_weight = Some(weight);
        }
        if let Some(family) = &self.code.family {
            preferences.code_font_family = Some(family.clone());
        }
        if let Some(size) = self.code.size {
            preferences.code_font_size = Some(size);
        }
        if let Some(weight) = self.code.weight {
            preferences.code_font_weight = Some(weight);
        }
    }

    pub(crate) fn transcript_background(&self, colors: &ThemeColors) -> Option<Hsla> {
        self.transcript_background
            .as_deref()
            .and_then(|spec| resolve_color(spec, colors, true))
    }

    pub(crate) fn transcript_foreground(&self, colors: &ThemeColors) -> Option<Hsla> {
        self.transcript_foreground
            .as_deref()
            .and_then(|spec| resolve_color(spec, colors, false))
    }
}

impl ComparisonFont {
    fn normalize(&mut self, role: &str) -> anyhow::Result<()> {
        self.family = normalized_string(self.family.take());
        if let Some(size) = self.size
            && (!size.is_finite()
                || !(MIN_HARNESS_FONT_SIZE..=MAX_HARNESS_FONT_SIZE).contains(&size))
        {
            bail!(
                "{role}.size must be a finite number from {MIN_HARNESS_FONT_SIZE} through {MAX_HARNESS_FONT_SIZE}"
            );
        }
        if let Some(weight) = self.weight
            && (!weight.is_finite()
                || !(MIN_HARNESS_FONT_WEIGHT..=MAX_HARNESS_FONT_WEIGHT).contains(&weight)
                || weight % 100. != 0.)
        {
            bail!("{role}.weight must be one of 100, 200, 300, 400, 500, 600, 700, 800, or 900");
        }
        Ok(())
    }
}

fn normalized_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn validate_color_spec(spec: Option<&str>, background: bool) -> anyhow::Result<()> {
    let Some(spec) = spec else {
        return Ok(());
    };
    let known_role = if background {
        matches!(spec, "editor" | "surface" | "panel" | "app")
    } else {
        matches!(spec, "text" | "muted" | "editor")
    };
    if !known_role {
        theme::try_parse_color(spec)
            .with_context(|| format!("invalid comparison color or theme role `{spec}`"))?;
    }
    Ok(())
}

fn resolve_color(spec: &str, colors: &ThemeColors, background: bool) -> Option<Hsla> {
    let role = if background {
        match spec {
            "editor" => Some(colors.editor_background),
            "surface" => Some(colors.surface_background),
            "panel" => Some(colors.panel_background),
            "app" => Some(colors.background),
            _ => None,
        }
    } else {
        match spec {
            "text" => Some(colors.text),
            "muted" => Some(colors.text_muted),
            "editor" => Some(colors.editor_foreground),
            _ => None,
        }
    };
    role.or_else(|| theme::try_parse_color(spec).ok())
}

fn profile_argument() -> Option<String> {
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--comparison-profile" {
            return arguments.next();
        }
        if let Some(value) = argument.strip_prefix("--comparison-profile=") {
            return Some(value.to_owned());
        }
    }
    std::env::var("HARNESS_COMPARISON_PROFILE").ok()
}

fn load_profile() -> anyhow::Result<Option<ComparisonProfile>> {
    let Some(argument) = normalized_string(profile_argument()) else {
        return Ok(None);
    };
    let contents = if argument.trim_start().starts_with('{') {
        argument
    } else {
        let path = PathBuf::from(&argument);
        fs::read_to_string(&path)
            .with_context(|| format!("reading comparison profile {}", path.display()))?
    };
    ComparisonProfile::parse(&contents).map(Some)
}

static PROFILE: LazyLock<Result<Option<ComparisonProfile>, String>> =
    LazyLock::new(|| load_profile().map_err(|error| format!("{error:#}")));

pub(crate) fn profile() -> Option<&'static ComparisonProfile> {
    PROFILE.as_ref().ok().and_then(Option::as_ref)
}

pub(crate) fn initialization_error(fixture_present: bool) -> Option<String> {
    match PROFILE.as_ref() {
        Err(error) => Some(error.clone()),
        Ok(Some(_)) if !fixture_present => {
            Some("--comparison-profile requires --comparison-fixture".to_owned())
        }
        Ok(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_parses_every_controlled_renderer_dimension() {
        let profile = ComparisonProfile::parse(
            r##"{
                "name": "Zed-like 400",
                "transcript_background": "surface",
                "transcript_foreground": "#242529",
                "reading": {"family": "IBM Plex Sans Condensed", "size": 16, "weight": 400},
                "code": {"family": ".ZedMono", "size": 14, "weight": 400},
                "line_height": 1.3125,
                "text_rendering": "subpixel"
            }"##,
        )
        .expect("profile should parse");

        assert_eq!(profile.name.as_deref(), Some("Zed-like 400"));
        assert_eq!(profile.transcript_background.as_deref(), Some("surface"));
        assert_eq!(profile.reading.weight, Some(400.));
        assert_eq!(profile.code.family.as_deref(), Some(".ZedMono"));
        assert_eq!(profile.line_height, Some(1.3125));
        assert_eq!(
            profile.text_rendering,
            Some(ComparisonTextRendering::Subpixel)
        );
    }

    #[test]
    fn invalid_profiles_fail_instead_of_silently_changing_the_experiment() {
        assert!(ComparisonProfile::parse(r#"{"reading":{"weight":350}}"#).is_err());
        assert!(ComparisonProfile::parse(r#"{"line_height":3}"#).is_err());
        assert!(ComparisonProfile::parse(r#"{"transcript_background":"not-a-color"}"#).is_err());
        assert!(ComparisonProfile::parse(r#"{"unknown":true}"#).is_err());
    }
}
