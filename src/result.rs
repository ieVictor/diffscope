use crate::{
    DiffHunk, FileStatus,
    analysis::{FunctionChangeStatus, FunctionChurn, MatchConfidence},
    languages::{DiagnosticSeverity, FunctionKind, Language, SourceRange},
    metrics::FunctionMetrics,
};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisResult {
    pub schema_version: u32,
    pub tool_version: String,
    pub repository: String,
    pub base: RevisionResult,
    pub target: RevisionResult,
    pub summary: AnalysisSummary,
    pub files: Vec<FileResult>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionResult {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalysisSummary {
    pub changed_files: u32,
    pub added_lines: u32,
    pub removed_lines: u32,
    pub supported_files: u32,
    pub unsupported_files: u32,
    pub diagnostics: DiagnosticCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticCounts {
    pub info: u32,
    pub warning: u32,
    pub error: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileResult {
    pub base_path: Option<String>,
    pub target_path: Option<String>,
    pub status: FileStatus,
    pub language: Option<Language>,
    pub is_binary: bool,
    pub added_lines: u32,
    pub removed_lines: u32,
    pub hunks: Vec<DiffHunk>,
    pub functions: Vec<FunctionResult>,
    /// Names added to and removed from this module's public surface.
    pub exports_added: Vec<String>,
    pub exports_removed: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionResult {
    pub id: String,
    pub status: FunctionChangeStatus,
    pub kind: FunctionKind,
    pub qualified_name: String,
    pub base_range: Option<SourceRange>,
    pub target_range: Option<SourceRange>,
    pub metrics_before: Option<FunctionMetrics>,
    pub metrics_after: Option<FunctionMetrics>,
    pub churn: FunctionChurn,
    pub match_confidence: MatchConfidence,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub path: Option<String>,
    pub range: Option<SourceRange>,
    pub related_entity_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticCode {
    UnsupportedLanguage,
    BinaryFile,
    MalformedSource,
    ParseError,
    InvalidUtf8,
    OversizedFile,
    MissingBlob,
    GitError,
    AmbiguousFunctionMatch,
    MetricUnavailable,
}

impl DiagnosticCounts {
    pub(crate) fn add(&mut self, severity: DiagnosticSeverity) {
        match severity {
            DiagnosticSeverity::Info => self.info += 1,
            DiagnosticSeverity::Warning => self.warning += 1,
            DiagnosticSeverity::Error => self.error += 1,
        }
    }
}

pub(crate) fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|left, right| {
        severity_rank(left.severity)
            .cmp(&severity_rank(right.severity))
            .then_with(|| left.code.cmp(&right.code))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| range_key(left.range.as_ref()).cmp(&range_key(right.range.as_ref())))
            .then_with(|| left.message.cmp(&right.message))
    });
}

fn severity_rank(severity: DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Info => 2,
    }
}

fn range_key(range: Option<&SourceRange>) -> (u32, u32, u32, u32) {
    range.map_or((u32::MAX, u32::MAX, u32::MAX, u32::MAX), |range| {
        (
            range.start_line,
            range.start_column,
            range.end_line,
            range.end_column,
        )
    })
}
