use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }

    pub fn from_lsp_int(n: u64) -> Self {
        match n {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Information,
            _ => Self::Hint,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub line: u32,
    pub character: u32,
    pub code: Option<String>,
}

impl Diagnostic {
    pub fn format_line(&self, path: &str) -> String {
        let code_part = self
            .code
            .as_deref()
            .map(|c| format!(" ({c})"))
            .unwrap_or_default();
        format!(
            "  {}:{}:{}: [{}]{} {}",
            path,
            self.line.saturating_add(1),
            self.character.saturating_add(1),
            self.severity.as_str(),
            code_part,
            self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_lsp_int_maps_all_severities() {
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(1),
            DiagnosticSeverity::Error
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(2),
            DiagnosticSeverity::Warning
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(3),
            DiagnosticSeverity::Information
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(4),
            DiagnosticSeverity::Hint
        );
    }

    #[test]
    fn from_lsp_int_out_of_range_falls_back_to_hint() {
        // 0 and anything > 4 are not valid LSP severities; treat as the least-urgent bucket.
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(0),
            DiagnosticSeverity::Hint
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(5),
            DiagnosticSeverity::Hint
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(u64::MAX),
            DiagnosticSeverity::Hint
        );
    }

    #[test]
    fn as_str_labels() {
        assert_eq!(DiagnosticSeverity::Error.as_str(), "error");
        assert_eq!(DiagnosticSeverity::Warning.as_str(), "warning");
        assert_eq!(DiagnosticSeverity::Information.as_str(), "info");
        assert_eq!(DiagnosticSeverity::Hint.as_str(), "hint");
    }

    #[test]
    fn severity_serde_roundtrip() {
        for sev in [
            DiagnosticSeverity::Error,
            DiagnosticSeverity::Warning,
            DiagnosticSeverity::Information,
            DiagnosticSeverity::Hint,
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            let back: DiagnosticSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(sev, back);
        }
    }

    #[test]
    fn format_line_with_code_uses_one_based_positions() {
        let d = Diagnostic {
            severity: DiagnosticSeverity::Error,
            message: "unused variable".to_string(),
            line: 9,
            character: 4,
            code: Some("E0001".to_string()),
        };
        // LSP positions are 0-based; the human-facing output is 1-based.
        assert_eq!(
            d.format_line("src/main.rs"),
            "  src/main.rs:10:5: [error] (E0001) unused variable"
        );
    }

    #[test]
    fn format_line_near_u32_max_does_not_panic_or_wrap() {
        // A server reporting a line/character at u32::MAX (e.g. from a truncated u64) must
        // saturate rather than overflow-panic (debug) or silently wrap to 0 (release).
        let d = Diagnostic {
            severity: DiagnosticSeverity::Error,
            message: "huge position".to_string(),
            line: u32::MAX,
            character: u32::MAX,
            code: None,
        };
        assert_eq!(
            d.format_line("a.rs"),
            format!("  a.rs:{}:{}: [error] huge position", u32::MAX, u32::MAX)
        );
    }

    #[test]
    fn format_line_without_code_omits_parenthetical() {
        let d = Diagnostic {
            severity: DiagnosticSeverity::Warning,
            message: "deprecated".to_string(),
            line: 0,
            character: 0,
            code: None,
        };
        assert_eq!(d.format_line("a.ts"), "  a.ts:1:1: [warning] deprecated");
    }

    #[test]
    fn diagnostic_serde_roundtrip() {
        let d = Diagnostic {
            severity: DiagnosticSeverity::Hint,
            message: "consider".to_string(),
            line: 3,
            character: 7,
            code: None,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Diagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(back.severity, d.severity);
        assert_eq!(back.message, d.message);
        assert_eq!(back.line, d.line);
        assert_eq!(back.character, d.character);
        assert_eq!(back.code, d.code);
    }
}
