use gpui::Hsla;
use theme::ThemeColors;

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
    fn semantic_theme_is_derived_only_from_zed_theme_colors() {
        // Keep this module intentionally data-only. A compile-time construction
        // test is more useful here than hard-coding values from one dark theme.
        let derive: fn(&ThemeColors) -> HarnessVisualTheme = HarnessVisualTheme::from_zed;
        let _ = derive;
    }
}
