//! Small editor-facing value types shared with project integrations.
//!
//! These types describe text presentation and navigation, not a Project. Keeping
//! them below both `editor` and `project` lets a local Editor retain its real
//! display map without importing the project/client stack.

use schemars::JsonSchema;
use serde::Deserialize;

/// A direction through an ordered editor collection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    Prev,
    Next,
}

/// Stable identity for text inserted into an editor display map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InlayId {
    EditPrediction(usize),
    DebuggerValue(usize),
    // LSP
    Hint(usize),
    Color(usize),
    ReplResult(usize),
}

impl InlayId {
    pub fn id(&self) -> usize {
        match self {
            Self::EditPrediction(id)
            | Self::DebuggerValue(id)
            | Self::Hint(id)
            | Self::Color(id)
            | Self::ReplResult(id) => *id,
        }
    }
}

/// An opaque semantic-token kind used while composing editor highlight layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenType(pub u32);

/// Maximum diagnostic severity presented by an editor surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DiagnosticSeverity {
    /// No diagnostics are shown.
    Off,
    Error,
    Warning,
    Info,
    Hint,
}

impl DiagnosticSeverity {
    /// Compatibility aliases for callers that previously received LSP severities
    /// through `language::DiagnosticSeverity`.
    pub const ERROR: Self = Self::Error;
    pub const WARNING: Self = Self::Warning;
    pub const INFORMATION: Self = Self::Info;
    pub const HINT: Self = Self::Hint;

    pub fn into_lsp(self) -> Option<lsp::DiagnosticSeverity> {
        match self {
            Self::Off => None,
            Self::Error => Some(lsp::DiagnosticSeverity::ERROR),
            Self::Warning => Some(lsp::DiagnosticSeverity::WARNING),
            Self::Info => Some(lsp::DiagnosticSeverity::INFORMATION),
            Self::Hint => Some(lsp::DiagnosticSeverity::HINT),
        }
    }
}

impl From<settings::DiagnosticSeverityContent> for DiagnosticSeverity {
    fn from(severity: settings::DiagnosticSeverityContent) -> Self {
        match severity {
            settings::DiagnosticSeverityContent::Off => Self::Off,
            settings::DiagnosticSeverityContent::Error => Self::Error,
            settings::DiagnosticSeverityContent::Warning => Self::Warning,
            settings::DiagnosticSeverityContent::Info => Self::Info,
            settings::DiagnosticSeverityContent::Hint
            | settings::DiagnosticSeverityContent::All => Self::Hint,
        }
    }
}

/// Determines the severity of the diagnostic that should be moved to.
#[derive(PartialEq, PartialOrd, Clone, Copy, Debug, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoToDiagnosticSeverity {
    /// Errors.
    Error = 3,
    /// Warnings.
    Warning = 2,
    /// Information.
    Information = 1,
    /// Hints.
    Hint = 0,
}

impl From<lsp::DiagnosticSeverity> for GoToDiagnosticSeverity {
    fn from(severity: lsp::DiagnosticSeverity) -> Self {
        match severity {
            lsp::DiagnosticSeverity::ERROR => Self::Error,
            lsp::DiagnosticSeverity::WARNING => Self::Warning,
            lsp::DiagnosticSeverity::INFORMATION => Self::Information,
            lsp::DiagnosticSeverity::HINT => Self::Hint,
            _ => Self::Error,
        }
    }
}

impl GoToDiagnosticSeverity {
    pub fn min() -> Self {
        Self::Hint
    }

    pub fn max() -> Self {
        Self::Error
    }
}

/// Allows filtering diagnostics that should be moved to.
#[derive(PartialEq, Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum GoToDiagnosticSeverityFilter {
    /// Move to diagnostics of a specific severity.
    Only(GoToDiagnosticSeverity),
    /// Specify a range of severities to include.
    Range {
        /// Minimum severity to move to.
        #[serde(default = "GoToDiagnosticSeverity::min")]
        min: GoToDiagnosticSeverity,
        /// Maximum severity to move to.
        #[serde(default = "GoToDiagnosticSeverity::max")]
        max: GoToDiagnosticSeverity,
    },
}

impl Default for GoToDiagnosticSeverityFilter {
    fn default() -> Self {
        Self::Range {
            min: GoToDiagnosticSeverity::min(),
            max: GoToDiagnosticSeverity::max(),
        }
    }
}

impl GoToDiagnosticSeverityFilter {
    pub fn matches(&self, severity: lsp::DiagnosticSeverity) -> bool {
        let severity: GoToDiagnosticSeverity = severity.into();
        match self {
            Self::Only(target) => *target == severity,
            Self::Range { min, max } => severity >= *min && severity <= *max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_editor_value_types_keep_their_exact_semantics() {
        assert_eq!(InlayId::EditPrediction(7).id(), 7);
        assert_eq!(DiagnosticSeverity::Off.into_lsp(), None);
        assert!(GoToDiagnosticSeverityFilter::default().matches(lsp::DiagnosticSeverity::HINT));
        assert!(GoToDiagnosticSeverityFilter::default().matches(lsp::DiagnosticSeverity::ERROR));
        assert!(
            !GoToDiagnosticSeverityFilter::Only(GoToDiagnosticSeverity::Warning)
                .matches(lsp::DiagnosticSeverity::ERROR)
        );
    }
}
