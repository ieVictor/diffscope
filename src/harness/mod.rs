use std::path::PathBuf;

use crate::{AnalysisRequest, AnalysisResult, analyze};

/// Transport-neutral request accepted by harness adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessRequest {
    pub id: String,
    pub repository: String,
    pub base_revision: String,
    pub target_revision: String,
}

/// Transport-neutral response returned by every harness adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessResponse {
    pub id: String,
    pub outcome: HarnessOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessOutcome {
    Success(Box<AnalysisResult>),
    Error(HarnessError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessError {
    pub code: HarnessErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessErrorCode {
    AnalysisFailed,
}

/// Execute one transport-neutral harness request.
#[must_use]
pub fn execute(request: HarnessRequest) -> HarnessResponse {
    let result = analyze(&AnalysisRequest {
        repository_path: PathBuf::from(&request.repository),
        base_revision: request.base_revision,
        target_revision: request.target_revision,
    });
    let outcome = match result {
        Ok(result) => HarnessOutcome::Success(Box::new(result)),
        Err(error) => HarnessOutcome::Error(HarnessError {
            code: HarnessErrorCode::AnalysisFailed,
            message: error.to_string(),
        }),
    };
    HarnessResponse {
        id: request.id,
        outcome,
    }
}
