mod typescript;

use std::{collections::BTreeSet, path::Path};

use crate::{DiffScopeError, metrics::FunctionMetrics};

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
    pub metrics: FunctionMetrics,
    /// Hash of the function's source with whitespace runs collapsed.
    ///
    /// Used only to pair functions that share one identity, so that a group of
    /// same-named functions can be matched by what they contain rather than
    /// abandoned. It is never serialized and never compared across processes.
    pub body_hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAnalysis {
    pub language: Option<Language>,
    pub functions: Vec<FunctionDefinition>,
    /// Names this module exports, as a caller would import them.
    ///
    /// Sorted and deduplicated, so comparing two revisions' sets reports what a
    /// change adds to or removes from the module's public surface.
    pub exports: BTreeSet<String>,
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
    OversizedFile,
}

/// Largest source blob that is parsed for metrics.
///
/// Analyzed repositories are untrusted and routinely contain generated or
/// vendored sources of arbitrary size. A syntax tree costs many times the
/// source it describes, and both revisions of a file are analyzed, so an
/// unbounded file size is an unbounded memory requirement. Files above this
/// limit are still inventoried, with their diff statistics and an
/// `oversized_file` diagnostic, but no function metrics.
pub const MAX_ANALYZED_BLOB_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
            exports: BTreeSet::new(),
            diagnostics: vec![LanguageDiagnostic {
                code: LanguageDiagnosticCode::UnsupportedLanguage,
                severity: DiagnosticSeverity::Info,
                message: "unsupported language".to_owned(),
                range: None,
            }],
        });
    };

    if source.len() > MAX_ANALYZED_BLOB_BYTES {
        return Ok(SourceAnalysis {
            language: Some(language),
            functions: Vec::new(),
            exports: BTreeSet::new(),
            diagnostics: vec![LanguageDiagnostic {
                code: LanguageDiagnosticCode::OversizedFile,
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "source is {} bytes, above the {MAX_ANALYZED_BLOB_BYTES} byte analysis limit; function metrics are unavailable",
                    source.len()
                ),
                range: None,
            }],
        });
    }

    match language {
        Language::TypeScript | Language::Tsx => TypeScriptAnalyzer::new(language)?.analyze(source),
    }
}

/// Address one function within its file, independently of line numbers.
///
/// The kind is included because a name alone does not separate a method from an
/// arrow function assigned to a property of the same name. Callers pass this
/// back to ask about one function, and a bare qualified name is also accepted
/// when it is unambiguous.
#[must_use]
pub fn symbol_id(kind: FunctionKind, qualified_name: &str) -> String {
    let tag = match kind {
        FunctionKind::Function => "fn",
        FunctionKind::Method => "method",
        FunctionKind::Constructor => "ctor",
        FunctionKind::ArrowFunction => "arrow",
    };
    format!("{tag}:{qualified_name}")
}

/// Hash source text, treating every run of whitespace as a single space.
///
/// FNV-1a is written out rather than using the standard hasher because the
/// standard hasher's output is explicitly allowed to change between Rust
/// releases, and pairing behavior should not change with the compiler.
#[must_use]
pub(crate) fn body_hash(text: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    let mut pending_space = false;
    for byte in text.trim().bytes() {
        if byte.is_ascii_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            hash = (hash ^ u64::from(b' ')).wrapping_mul(PRIME);
            pending_space = false;
        }
        hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    hash
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
