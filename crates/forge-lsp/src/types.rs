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
            self.line + 1,
            self.character + 1,
            self.severity.as_str(),
            code_part,
            self.message
        )
    }
}
