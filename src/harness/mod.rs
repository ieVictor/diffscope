use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use crate::{AnalysisRequest, AnalysisResult, DiffScopeError, analyze, git};

pub mod jsonl;

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
    Success(Arc<AnalysisResult>),
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

/// Analyses retained for reuse across requests.
///
/// An agent asks several questions about one comparison: an overview, then a
/// filtered page, then one function. Each is a projection of the same analysis,
/// and re-deriving it per question is the dominant cost of answering.
const MAX_CACHED_ANALYSES: usize = 4;

/// Upper bound on the function records held across all cached analyses.
///
/// Entry count alone does not bound memory, because one analysis of a large
/// repository can hold far more than several small ones. Function records are
/// the bulk of a result, so they stand in for its size.
const MAX_CACHED_FUNCTIONS: usize = 200_000;

/// A long-lived harness process that reuses analyses between requests.
///
/// Entries are keyed by the commits the two revisions resolve to, never by the
/// revision names themselves. `HEAD` and a branch name point at different
/// commits over time, so caching against the name would serve a stale analysis
/// after the branch moved; caching against the commit cannot, because a commit
/// is immutable. Resolving the two names first costs one `rev-parse` each.
#[derive(Default)]
pub struct HarnessSession {
    cached: Mutex<VecDeque<CacheEntry>>,
}

struct CacheEntry {
    key: CacheKey,
    result: Arc<AnalysisResult>,
    functions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheKey {
    repository_root: PathBuf,
    base_commit: String,
    target_commit: String,
}

impl HarnessSession {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Analyze a comparison, reusing a cached analysis of the same commits.
    ///
    /// # Errors
    ///
    /// Returns an error when the repository or either revision cannot be
    /// resolved, or when the analysis itself fails.
    pub fn analysis(
        &self,
        request: &AnalysisRequest,
    ) -> Result<Arc<AnalysisResult>, DiffScopeError> {
        let repository = git::Repository::open(&request.repository_path)?;
        let key = CacheKey {
            repository_root: repository.root().to_path_buf(),
            base_commit: repository
                .resolve_revision(&request.base_revision)?
                .commit_id,
            target_commit: repository
                .resolve_revision(&request.target_revision)?
                .commit_id,
        };

        if let Some(cached) = self.take_cached(&key) {
            return Ok(cached);
        }

        let result = Arc::new(analyze(request)?);
        self.store(key, &result);
        Ok(result)
    }

    fn take_cached(&self, key: &CacheKey) -> Option<Arc<AnalysisResult>> {
        let mut cached = lock(&self.cached);
        let index = cached.iter().position(|entry| &entry.key == key)?;
        // Move the hit to the back so the least recently used entry is the one
        // evicted when room is needed.
        let entry = cached.remove(index)?;
        let result = Arc::clone(&entry.result);
        cached.push_back(entry);
        Some(result)
    }

    fn store(&self, key: CacheKey, result: &Arc<AnalysisResult>) {
        let functions = result
            .files
            .iter()
            .map(|file| file.functions.len())
            .sum::<usize>();
        let mut cached = lock(&self.cached);
        cached.retain(|entry| entry.key != key);
        cached.push_back(CacheEntry {
            key,
            result: Arc::clone(result),
            functions,
        });

        while cached.len() > MAX_CACHED_ANALYSES
            || (cached.len() > 1
                && cached.iter().map(|entry| entry.functions).sum::<usize>() > MAX_CACHED_FUNCTIONS)
        {
            let _evicted = cached.pop_front();
        }
    }
}

/// A poisoned cache means a previous request panicked while holding it. The
/// entries themselves are immutable analyses and stay valid, so the lock is
/// recovered rather than propagating the panic to every later request.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Execute one transport-neutral harness request without reusing analyses.
#[must_use]
pub fn execute(request: HarnessRequest) -> HarnessResponse {
    HarnessSession::new().execute(request)
}

impl HarnessSession {
    /// Execute one transport-neutral harness request.
    #[must_use]
    pub fn execute(&self, request: HarnessRequest) -> HarnessResponse {
        let result = self.analysis(&AnalysisRequest {
            repository_path: PathBuf::from(&request.repository),
            base_revision: request.base_revision,
            target_revision: request.target_revision,
        });
        let outcome = match result {
            Ok(result) => HarnessOutcome::Success(result),
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
}
