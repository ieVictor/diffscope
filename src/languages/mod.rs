mod typescript;

use std::path::Path;

use crate::DiffScopeError;

pub use typescript::TypeScriptAnalyzer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Language {
    TypeScript,
    Tsx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FunctionKind {
    Function,
    Method,
    Constructor,
    ArrowFunction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDefinition {
    pub language: Language,
    pub kind: FunctionKind,
    pub qualified_name: String,
    pub range: SourceRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAnalysis {
    pub language: Option<Language>,
    pub functions: Vec<FunctionDefinition>,
    pub diagnostics: Vec<LanguageDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageDiagnostic {
    pub code: LanguageDiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub range: Option<SourceRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageDiagnosticCode {
    UnsupportedLanguage,
    InvalidUtf8,
    MalformedSource,
    ParseError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// Analyze one source blob using the analyzer selected from its path.
///
/// # Errors
///
/// Returns an error when a supported analyzer cannot be initialized or parsing
/// fails before a syntax tree can be produced.
pub fn analyze_source(path: &Path, source: &[u8]) -> Result<SourceAnalysis, DiffScopeError> {
    let Some(language) = detect_language(path) else {
        return Ok(SourceAnalysis {
            language: None,
            functions: Vec::new(),
            diagnostics: vec![LanguageDiagnostic {
                code: LanguageDiagnosticCode::UnsupportedLanguage,
                severity: DiagnosticSeverity::Info,
                message: "unsupported language".to_owned(),
                range: None,
            }],
        });
    };

    match language {
        Language::TypeScript | Language::Tsx => TypeScriptAnalyzer::new(language)?.analyze(source),
    }
}

pub fn detect_language(path: &Path) -> Option<Language> {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("ts" | "mts" | "cts") => Some(Language::TypeScript),
        Some("tsx") => Some(Language::Tsx),
        _ => None,
    }
}

impl SourceRange {
    pub(crate) fn from_tree_sitter(range: tree_sitter::Range) -> Result<Self, DiffScopeError> {
        Ok(Self {
            start_line: to_u32(range.start_point.row + 1, "start line")?,
            start_column: to_u32(range.start_point.column, "start column")?,
            end_line: to_u32(range.end_point.row + 1, "end line")?,
            end_column: to_u32(range.end_point.column, "end column")?,
        })
    }
}

fn to_u32(value: usize, name: &str) -> Result<u32, DiffScopeError> {
    u32::try_from(value)
        .map_err(|error| DiffScopeError::Language(format!("{name} is too large: {error}")))
}
