pub mod analysis;
pub mod application;
pub mod git;
pub mod languages;
pub mod metrics;
pub mod output;
pub mod result;

pub use application::analyze;
pub use result::{
    AnalysisResult, AnalysisSummary, Diagnostic, DiagnosticCode, DiagnosticCounts, FileResult,
    FunctionResult, RevisionResult, SCHEMA_VERSION,
};

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisRequest {
    pub repository_path: PathBuf,
    pub base_revision: String,
    pub target_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeInventory {
    pub repository_path: PathBuf,
    pub base: ResolvedRevision,
    pub target: ResolvedRevision,
    pub files: Vec<FileChange>,
    pub summary: ChangeSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRevision {
    pub input: String,
    pub commit_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub base_path: Option<String>,
    pub target_path: Option<String>,
    pub status: FileStatus,
    pub old_blob_id: Option<String>,
    pub new_blob_id: Option<String>,
    pub base_blob: BlobContent,
    pub target_blob: BlobContent,
    pub added_lines: u32,
    pub removed_lines: u32,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobContent {
    Available(Vec<u8>),
    Missing,
    NotApplicable,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub base_start: u32,
    pub base_count: u32,
    pub target_start: u32,
    pub target_count: u32,
    pub added_lines: u32,
    pub removed_lines: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSummary {
    pub changed_files: u32,
    pub added_lines: u32,
    pub removed_lines: u32,
    pub binary_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffScopeError {
    Git { command: String, message: String },
    InvalidGitOutput(String),
    Language(String),
}

impl std::fmt::Display for DiffScopeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git { command, message } => {
                write!(formatter, "git command failed (`{command}`): {message}")
            }
            Self::InvalidGitOutput(message) => write!(formatter, "invalid git output: {message}"),
            Self::Language(message) => write!(formatter, "language analysis failed: {message}"),
        }
    }
}

impl std::error::Error for DiffScopeError {}

/// Build a Git-backed change inventory for the requested revision pair.
///
/// # Errors
///
/// Returns an error when the repository cannot be resolved, Git rejects either
/// revision, or Git emits output that does not match the documented inventory
/// format.
pub fn inventory_changes(request: &AnalysisRequest) -> Result<ChangeInventory, DiffScopeError> {
    git::Repository::open(&request.repository_path)?
        .inventory_changes(&request.base_revision, &request.target_revision)
}
