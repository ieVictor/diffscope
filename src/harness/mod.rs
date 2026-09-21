use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use serde::{Deserialize, Serialize};

use crate::{
    AnalysisRequest, AnalysisResult, DiffScopeError,
    analysis::FunctionChangeStatus,
    analyze,
    git::Repository,
    graph::{Direction, Limits, View, canonical_depth},
    imports,
    imports::ImportIndex,
    output,
    query::{
        self, FileFilter, FunctionFilter, Page,
        classify::FileClassification,
        graph::{GraphRequest, Requested},
        risk::RiskLevel,
    },
};

pub mod jsonl;
pub mod mcp;

/// Version of the schema every query answer is written in.
///
/// Separate from the analysis document's own version: the document describes
/// one comparison, while this schema describes how the harness answers
/// questions about it. Cursors are bound to it, so an answer produced under one
/// version is never continued under another.
pub const SCHEMA_VERSION: u32 = 2;

/// Deterministic identifier of one analyzed comparison.
///
/// Derived from what an answer actually depends on: the commits the two
/// revisions resolved to, the tool that produced the analysis, and the answer
/// schema. Revision names are deliberately not inputs, because `HEAD` and a
/// branch name resolve to different commits over time; two requests describing
/// the same comparison get the same identifier, and any change to the analysis
/// or to its meaning produces a different one.
///
/// The value is a fingerprint rather than a name: callers treat it as opaque
/// and only ever compare it with another identifier.
#[must_use]
pub fn analysis_id(base_commit: &str, target_commit: &str) -> String {
    /// FNV-1a over 128 bits: cheap, dependency-free, and identical in every
    /// process. Nothing here is a security boundary; the identifier names an
    /// analysis whose inputs the caller already holds.
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut hash = OFFSET_BASIS;
    let mut mix = |bytes: &[u8]| {
        // Length-prefixed, so that moving a byte across a field boundary
        // changes the result instead of quietly producing the same digest.
        for byte in u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes() {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
        for byte in bytes {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    mix(base_commit.as_bytes());
    mix(target_commit.as_bytes());
    mix(env!("CARGO_PKG_VERSION").as_bytes());
    mix(&SCHEMA_VERSION.to_le_bytes());

    format!("{hash:032x}")
}

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

/// What kind of failure a harness request hit.
///
/// The codes are the ones the projections report, so a caller that reads a code
/// over one transport reads the same code over another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessErrorCode {
    /// The comparison could not be analyzed.
    AnalysisFailed,
    /// The request named a method this server does not answer.
    UnknownMethod,
    /// The parameters are not a question this analysis can be asked.
    InvalidParams,
    /// The request named a function identity the analysis does not contain.
    UnknownFunction,
    /// The answer could not be rendered as JSON.
    SerializationFailed,
    /// A step failed for a reason that is not the caller's to correct.
    Internal,
}

impl HarnessError {
    /// The error a projection failure becomes, carrying its code and message
    /// unchanged, so the harness core and the transports report one failure one
    /// way.
    fn from_projection(error: ProjectionError) -> Self {
        Self {
            code: match error.code {
                "analysis_failed" => HarnessErrorCode::AnalysisFailed,
                "invalid_params" => HarnessErrorCode::InvalidParams,
                "unknown_function" => HarnessErrorCode::UnknownFunction,
                "serialization_failed" => HarnessErrorCode::SerializationFailed,
                // `internal_error` is produced only for a step that failed
                // outside the caller's control, and the projection's codes are
                // a closed set produced in this module: anything else reaching
                // here is a bug, not a caller error.
                _ => HarnessErrorCode::Internal,
            },
            message: error.message,
        }
    }
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

/// Import graphs retained for reuse.
///
/// A graph is far smaller than an analysis, holding one entry per source file
/// rather than one per function, so a few cost little. Eight rather than four
/// because a delta comparison holds two of them: that keeps the four
/// comparisons' worth of headroom the constant was chosen for, where four
/// entries would let two alternating comparisons evict each other's indexes on
/// every query.
const MAX_CACHED_INDEXES: usize = 8;

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
    indexes: Mutex<VecDeque<IndexEntry>>,
}

struct IndexEntry {
    repository_root: PathBuf,
    commit: String,
    index: Arc<ImportIndex>,
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
        let repository = Repository::open(&request.repository_path)?;
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

    /// Build, or reuse, the import graph of one commit.
    ///
    /// The graph describes one revision, not a comparison, so it is keyed by
    /// the commit alone: every comparison that touches the same commit shares
    /// one index, however many different bases or targets name it. Commits
    /// rather than revision names, because a name like `HEAD` points at
    /// different commits over time.
    ///
    /// Resolving *which* commit to index happens in the caller, because a
    /// comparison needs the base's graph as well as the target's and both are
    /// cached through this one lookup.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot list the tree or read its blobs.
    pub fn import_index(
        &self,
        repository: &Repository,
        commit: &str,
    ) -> Result<Arc<ImportIndex>, DiffScopeError> {
        let root = repository.root().to_path_buf();
        if let Some(cached) = self.take_index(&root, commit) {
            return Ok(cached);
        }

        let index = Arc::new(imports::index_revision(repository, commit)?);
        let mut indexes = lock(&self.indexes);
        indexes.retain(|entry| entry.repository_root != root || entry.commit != commit);
        indexes.push_back(IndexEntry {
            repository_root: root,
            commit: commit.to_owned(),
            index: Arc::clone(&index),
        });
        while indexes.len() > MAX_CACHED_INDEXES {
            let _evicted = indexes.pop_front();
        }
        Ok(index)
    }

    fn take_index(&self, root: &Path, commit: &str) -> Option<Arc<ImportIndex>> {
        let mut indexes = lock(&self.indexes);
        let position = indexes
            .iter()
            .position(|entry| entry.repository_root == root && entry.commit == commit)?;
        let entry = indexes.remove(position)?;
        let index = Arc::clone(&entry.index);
        indexes.push_back(entry);
        Some(index)
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

/// Answer one question and return the complete answer envelope, exactly as a
/// transport would serialize it: `{"analysis": …, "query": …, "data": …}`.
///
/// The CLI asks through here, so a question answered from the command line is
/// the same projection, in the same envelope, as the same question asked over
/// JSONL or MCP — and no transport can drift from another.
///
/// # Errors
///
/// Returns the failure a transport would report, with the same code and
/// message: a method this server does not answer, parameters its question
/// cannot be asked with, a comparison that cannot be analyzed, or an answer
/// that cannot be rendered.
pub fn answer_json(
    session: &HarnessSession,
    request: &AnalysisRequest,
    method: Option<&str>,
    params: serde_json::Value,
) -> Result<serde_json::Value, HarnessError> {
    let method = Method::parse(method).map_err(|message| HarnessError {
        code: HarnessErrorCode::UnknownMethod,
        message,
    })?;
    let params = QueryParams::decode(params).map_err(HarnessError::from_projection)?;
    let answer =
        answer(session, method, request, &params).map_err(HarnessError::from_projection)?;
    serde_json::to_value(&answer).map_err(|error| HarnessError {
        code: HarnessErrorCode::SerializationFailed,
        message: format!("could not render the answer: {error}"),
    })
}

// ----------------------------------------------------------------- answers ---
//
// One comparison, many questions: the answer to each is a projection of the
// same analysis, and the projection is identical no matter which transport
// asked. A transport turns the transport's own request into a [`Method`] and a
// [`QueryParams`], hands both to [`project`], and wraps the [`Answer`] it gets
// back in whatever envelope that transport speaks.

/// The question a request asks about a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    /// The complete analysis, exactly as the CLI emits it.
    Analyze,
    ChangeSummary,
    ListChangedFiles,
    ListChangedFunctions,
    GetFunctionChange,
    GetAnalysisDiagnostics,
    /// The modules and tests a change reaches, and what it changed about those
    /// relationships.
    GetImpactGraph,
}

/// Which revisions' import graphs answering a question needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportGraph {
    /// The analysis alone answers it.
    None,
    /// The target revision's graph answers it.
    Target,
    /// Neither revision alone answers it: the answer compares the two.
    Both,
}

impl Method {
    /// Which import graphs answering this question needs.
    ///
    /// The graph is built only for the questions that use it, because it reads
    /// every source file of the revision rather than only the changed ones. A
    /// delta needs both revisions — one can show what exists, only two can
    /// prove what was removed — which is why this is three-valued rather than
    /// a flag.
    pub(crate) fn needs_import_graph(self) -> ImportGraph {
        match self {
            Self::Analyze | Self::GetAnalysisDiagnostics => ImportGraph::None,
            Self::ChangeSummary
            | Self::ListChangedFiles
            | Self::ListChangedFunctions
            | Self::GetFunctionChange => ImportGraph::Target,
            Self::GetImpactGraph => ImportGraph::Both,
        }
    }

    /// Whether this question is answered a page at a time.
    pub(crate) fn paginated(self) -> bool {
        matches!(self, Self::ListChangedFiles | Self::ListChangedFunctions)
    }

    /// A request without a method asks for the complete analysis.
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("analyze") => Ok(Self::Analyze),
            Some("get_change_summary") => Ok(Self::ChangeSummary),
            Some("list_changed_files") => Ok(Self::ListChangedFiles),
            Some("list_changed_functions") => Ok(Self::ListChangedFunctions),
            Some("get_function_change") => Ok(Self::GetFunctionChange),
            Some("get_analysis_diagnostics") => Ok(Self::GetAnalysisDiagnostics),
            Some("get_impact_graph") => Ok(Self::GetImpactGraph),
            Some(other) => Err(format!(
                "unknown method `{other}`; expected analyze, get_change_summary, \
                 list_changed_files, list_changed_functions, get_function_change, \
                 get_analysis_diagnostics, or get_impact_graph"
            )),
        }
    }
}

/// A question that cannot be answered as asked.
///
/// The code is the machine-readable half of the failure, shared by every
/// transport: `invalid_params` for a question the analysis cannot be asked,
/// `unknown_function` for an identity the analysis does not contain, and
/// `serialization_failed` for an answer that cannot be rendered.
pub(crate) struct ProjectionError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

fn invalid_params(message: String) -> ProjectionError {
    ProjectionError {
        code: "invalid_params",
        message,
    }
}

/// One successful answer, before it is put in a transport's envelope.
pub(crate) struct Projection {
    /// The parameters that were actually applied, defaults included.
    pub(crate) query: serde_json::Value,
    pub(crate) data: serde_json::Value,
    /// How much of a list this answer carried. Absent for whole-value methods.
    pub(crate) page: Option<PageView>,
}

/// Answer one question about a comparison, reusing the session's analyses.
///
/// Every transport asks its questions through here: the work of resolving the
/// revisions, analyzing them once, and rendering the answer is the same
/// whichever way the question arrived.
///
/// # Errors
///
/// Returns [`ProjectionError`] when the comparison cannot be analyzed, or when
/// the question cannot be asked of the analysis.
pub(crate) fn answer(
    session: &HarnessSession,
    method: Method,
    request: &AnalysisRequest,
    params: &QueryParams,
) -> Result<Answer, ProjectionError> {
    let result = session
        .analysis(request)
        .map_err(|error| analysis_failed(&error))?;

    let indexes = Indexes::resolve(session, method, request, &result)?;

    let analysis = analysis_id(&result.base.id, &result.target.id);
    let projection = project(method, &result, params, &indexes, &analysis)?;
    Ok(Answer::new(analysis_view(&result, analysis), projection))
}

/// The import graphs a question was answered with.
///
/// Held apart from the analysis because they are built and cached per revision
/// rather than per comparison: a delta needs both sides, every other question
/// needs the target alone, and the session's cache serves whoever asks.
#[derive(Debug, Default)]
struct Indexes {
    base: Option<Arc<ImportIndex>>,
    target: Option<Arc<ImportIndex>>,
}

impl Indexes {
    /// Build, or reuse, the graphs the method asked for.
    ///
    /// The commits come from the analysis rather than from a second resolution:
    /// the analysis already resolved both revision names, and its ids are what
    /// the index cache is keyed by.
    fn resolve(
        session: &HarnessSession,
        method: Method,
        request: &AnalysisRequest,
        result: &AnalysisResult,
    ) -> Result<Self, ProjectionError> {
        let requirement = method.needs_import_graph();
        if requirement == ImportGraph::None {
            return Ok(Self::default());
        }
        let repository =
            Repository::open(&request.repository_path).map_err(|error| analysis_failed(&error))?;
        let target = session
            .import_index(&repository, &result.target.id)
            .map_err(|error| analysis_failed(&error))?;
        let base = if requirement == ImportGraph::Both {
            Some(
                session
                    .import_index(&repository, &result.base.id)
                    .map_err(|error| analysis_failed(&error))?,
            )
        } else {
            None
        };
        Ok(Self {
            base,
            target: Some(target),
        })
    }

    /// The target revision's graph, which every graph-using method is answered
    /// with.
    fn target(&self) -> Option<&ImportIndex> {
        self.target.as_deref()
    }

    /// Both revisions' graphs.
    ///
    /// # Errors
    ///
    /// Returns an internal error when one is missing, which is a bug here
    /// rather than a caller error: [`Method::needs_import_graph`] resolving to
    /// [`ImportGraph::Both`] is what builds both.
    fn both(&self) -> Result<(&ImportIndex, &ImportIndex), ProjectionError> {
        match (self.base.as_deref(), self.target.as_deref()) {
            (Some(base), Some(target)) => Ok((base, target)),
            _ => Err(ProjectionError {
                code: "internal_error",
                message: "the impact graph needs both revisions' import graphs".to_owned(),
            }),
        }
    }
}

fn analysis_failed(error: &DiffScopeError) -> ProjectionError {
    ProjectionError {
        code: "analysis_failed",
        message: error.to_string(),
    }
}

/// Render the answer to one method.
///
/// # Errors
///
/// Returns [`ProjectionError`] when the question cannot be asked of this
/// analysis, or when an answer cannot be rendered as JSON.
fn project(
    method: Method,
    result: &AnalysisResult,
    params: &QueryParams,
    indexes: &Indexes,
    analysis: &str,
) -> Result<Projection, ProjectionError> {
    if params.cursor.is_some() && !method.paginated() {
        return Err(invalid_params(
            "`cursor` is only accepted by list_changed_files and list_changed_functions".to_owned(),
        ));
    }

    match method {
        Method::Analyze => complete_analysis(result),
        Method::ChangeSummary => summary(result, indexes.target()),
        Method::ListChangedFiles => file_list(result, params, indexes.target(), analysis),
        Method::ListChangedFunctions => function_list(result, params, indexes.target(), analysis),
        Method::GetFunctionChange => function_detail(result, params, indexes.target()),
        Method::GetAnalysisDiagnostics => diagnostics(result, params),
        Method::GetImpactGraph => {
            let (base, target) = indexes.both()?;
            impact_graph(result, params, base, target)
        }
    }
}

/// The complete analysis, exactly as the CLI emits it.
fn complete_analysis(result: &AnalysisResult) -> Result<Projection, ProjectionError> {
    Ok(Projection {
        query: serde_json::json!({}),
        data: output::json_value(result).map_err(|error| serialization_failed(&error))?,
        page: None,
    })
}

/// Counts, areas, and the ranked shortlist.
fn summary(
    result: &AnalysisResult,
    index: Option<&ImportIndex>,
) -> Result<Projection, ProjectionError> {
    Ok(Projection {
        query: serde_json::json!({}),
        data: to_value(&query::change_summary(result, index))?,
        page: None,
    })
}

/// One page of changed files, ranked.
fn file_list(
    result: &AnalysisResult,
    params: &QueryParams,
    index: Option<&ImportIndex>,
    analysis: &str,
) -> Result<Projection, ProjectionError> {
    let (applied, offset) = params.file_query(analysis)?;
    let filter = FileFilter {
        classification: parse_enum(
            applied.classification.as_deref(),
            FileClassification::parse,
            "classification",
            "lockfile, vendored, generated, test, config, docs, source",
        )?,
        minimum_risk: parse_enum(
            applied.minimum_risk.as_deref(),
            RiskLevel::parse,
            "minimum_risk",
            "low, medium, high",
        )?,
        page: Page {
            limit: Some(applied.limit),
            offset,
        },
    };
    let listed = query::list_changed_files(result, &filter, index);
    let echoed = to_value(&applied)?;
    let page = PageView::for_listing(
        listed.page,
        CursorQuery::ListChangedFiles(applied),
        analysis,
    )?;
    Ok(Projection {
        query: echoed,
        data: serde_json::json!({ "files": listed.files }),
        page: Some(page),
    })
}

/// One page of changed functions, ranked.
fn function_list(
    result: &AnalysisResult,
    params: &QueryParams,
    index: Option<&ImportIndex>,
    analysis: &str,
) -> Result<Projection, ProjectionError> {
    let (applied, offset) = params.function_query(analysis)?;
    let filter = FunctionFilter {
        file: applied.file.clone(),
        status: parse_enum(
            applied.status.as_deref(),
            parse_status,
            "status",
            "added, removed, modified, unchanged",
        )?,
        classification: parse_enum(
            applied.classification.as_deref(),
            FileClassification::parse,
            "classification",
            "lockfile, vendored, generated, test, config, docs, source",
        )?,
        minimum_risk: parse_enum(
            applied.minimum_risk.as_deref(),
            RiskLevel::parse,
            "minimum_risk",
            "low, medium, high",
        )?,
        min_complexity_delta: applied.min_complexity_delta,
        include_unchanged: applied.include_unchanged,
        page: Page {
            limit: Some(applied.limit),
            offset,
        },
    };
    let listed = query::list_changed_functions(result, &filter, index);
    let echoed = to_value(&applied)?;
    let page = PageView::for_listing(
        listed.page,
        CursorQuery::ListChangedFunctions(applied),
        analysis,
    )?;
    Ok(Projection {
        query: echoed,
        data: serde_json::json!({ "functions": listed.functions }),
        page: Some(page),
    })
}

/// One function in full.
fn function_detail(
    result: &AnalysisResult,
    params: &QueryParams,
    index: Option<&ImportIndex>,
) -> Result<Projection, ProjectionError> {
    let function_id = params
        .function_id
        .as_deref()
        .ok_or_else(|| invalid_params("`function_id` is required".to_owned()))?;
    match query::get_function_change(result, function_id, index) {
        Ok(detail) => Ok(Projection {
            query: serde_json::json!({ "function_id": function_id }),
            data: to_value(&detail)?,
            page: None,
        }),
        Err(unknown) => Err(ProjectionError {
            code: "unknown_function",
            message: describe_unknown(&unknown),
        }),
    }
}

/// One impact graph: the modules and tests a change reaches, and which of those
/// relationships the comparison added or removed.
///
/// The request is validated against the analysis before anything is walked:
/// every rejection a caller can provoke — a root this version cannot resolve, a
/// path the change does not contain, a relation or rendering this version does
/// not produce — is reported as `invalid_params` with the field it names.
fn impact_graph(
    result: &AnalysisResult,
    params: &QueryParams,
    base: &ImportIndex,
    target: &ImportIndex,
) -> Result<Projection, ProjectionError> {
    let direction = parse_enum(
        params.direction.as_deref(),
        Direction::parse,
        "direction",
        "upstream, downstream, both",
    )?;
    let view = parse_enum(
        params.view.as_deref(),
        View::parse,
        "view",
        "delta, base, target",
    )?;
    let request = GraphRequest::validate(
        result,
        &Requested {
            file: params.file.as_deref(),
            function_id: params.function_id.as_deref(),
            direction,
            relations: &params.relations,
            depth: canonical_depth(params.depth),
            view,
            limits: Limits::canonical(params.max_nodes, params.max_edges),
            render: &params.render,
        },
    )
    .map_err(|error| invalid_params(error.message))?;

    Ok(Projection {
        query: to_value(&request.applied())?,
        data: to_value(&query::graph::project(result, base, target, &request))?,
        page: None,
    })
}

/// Diagnostics for the analysis, or for one file of it.
fn diagnostics(
    result: &AnalysisResult,
    params: &QueryParams,
) -> Result<Projection, ProjectionError> {
    Ok(Projection {
        query: serde_json::json!({ "file": params.file }),
        data: to_value(&query::get_analysis_diagnostics(
            result,
            params.file.as_deref(),
        ))?,
        page: None,
    })
}

fn to_value<T: Serialize>(value: &T) -> Result<serde_json::Value, ProjectionError> {
    serde_json::to_value(value).map_err(|error| serialization_failed(&error))
}

fn serialization_failed(error: &serde_json::Error) -> ProjectionError {
    ProjectionError {
        code: "serialization_failed",
        message: error.to_string(),
    }
}

/// Name the identities the analysis does contain, so a caller that guessed can
/// correct itself in one step instead of guessing again.
fn describe_unknown(unknown: &query::UnknownFunction) -> String {
    /// Identities named in the error before it is summarized.
    const SUGGESTIONS: usize = 10;

    if unknown.known_function_ids.is_empty() {
        return format!(
            "no function `{}` in this analysis, which contains no functions",
            unknown.function_id
        );
    }
    let shown = unknown
        .known_function_ids
        .iter()
        .take(SUGGESTIONS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let remainder = unknown.known_function_ids.len().saturating_sub(SUGGESTIONS);
    let more = if remainder == 0 {
        String::new()
    } else {
        format!(" and {remainder} more")
    };
    format!(
        "no function `{}` in this analysis; it contains {shown}{more}",
        unknown.function_id
    )
}

fn parse_status(value: &str) -> Option<FunctionChangeStatus> {
    match value {
        "added" => Some(FunctionChangeStatus::Added),
        "removed" => Some(FunctionChangeStatus::Removed),
        "modified" => Some(FunctionChangeStatus::Modified),
        "unchanged" => Some(FunctionChangeStatus::Unchanged),
        _ => None,
    }
}

fn parse_enum<T>(
    value: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
    field: &str,
    accepted: &str,
) -> Result<Option<T>, ProjectionError> {
    let Some(value) = value else {
        return Ok(None);
    };
    parse(value).map(Some).ok_or_else(|| {
        invalid_params(format!(
            "`{field}` must be one of {accepted}; received `{value}`"
        ))
    })
}

/// Every parameter any method accepts.
///
/// One shape for all methods keeps every transport's request obvious and lets
/// an unknown field be rejected outright rather than silently ignored.
/// Parameters that do not apply to the method being called are unused.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryParams {
    file: Option<String>,
    function_id: Option<String>,
    status: Option<String>,
    classification: Option<String>,
    minimum_risk: Option<String>,
    min_complexity_delta: Option<i64>,
    include_unchanged: Option<bool>,
    limit: Option<usize>,
    cursor: Option<String>,
    /// Which way an impact graph walks from its root.
    direction: Option<String>,
    /// The relations it may follow. Empty means every supported relation.
    #[serde(default)]
    relations: Vec<String>,
    /// Hops it walks from the root.
    depth: Option<u32>,
    /// Which revision's relationships it shows.
    view: Option<String>,
    /// Node and edge budgets.
    max_nodes: Option<usize>,
    max_edges: Option<usize>,
    /// The renderings to include. Empty means none.
    #[serde(default)]
    render: Vec<String>,
}

impl QueryParams {
    /// Decode the parameters of one question.
    ///
    /// # Errors
    ///
    /// Returns the reason the parameters are not a question this server can
    /// answer, phrased for the caller who wrote them.
    pub(crate) fn decode(value: serde_json::Value) -> Result<Self, ProjectionError> {
        serde_json::from_value(value)
            .map_err(|error| invalid_params(format!("invalid query parameters: {error}")))
    }

    /// The page size this request asks for, after defaults and bounds, when it
    /// asks for one at all.
    fn canonical_limit(&self) -> Option<usize> {
        self.limit.map(|limit| query::canonical_limit(Some(limit)))
    }

    /// The file list this request asks for, and the row to start at.
    ///
    /// Without a cursor the request starts at the beginning. With one, the
    /// cursor is the authority on what was asked and where to continue: a
    /// parameter that contradicts it is rejected rather than quietly answering
    /// a different question than the one being paged through.
    fn file_query(&self, analysis: &str) -> Result<(FileQuery, usize), ProjectionError> {
        let Some(payload) = self.cursor_payload(analysis)? else {
            return Ok((
                FileQuery {
                    classification: self.classification.clone(),
                    minimum_risk: self.minimum_risk.clone(),
                    limit: query::canonical_limit(self.limit),
                },
                0,
            ));
        };
        let CursorQuery::ListChangedFiles(applied) = payload.query else {
            return Err(invalid_params(
                "`cursor` was issued for another method; list_changed_files cannot continue it"
                    .to_owned(),
            ));
        };
        agrees(
            "classification",
            self.classification.as_deref(),
            applied.classification.as_deref(),
        )?;
        agrees(
            "minimum_risk",
            self.minimum_risk.as_deref(),
            applied.minimum_risk.as_deref(),
        )?;
        agrees(
            "limit",
            self.canonical_limit().as_ref(),
            Some(&applied.limit),
        )?;
        Ok((applied, payload.offset))
    }

    /// The function list this request asks for, and the row to start at.
    fn function_query(&self, analysis: &str) -> Result<(FunctionQuery, usize), ProjectionError> {
        let Some(payload) = self.cursor_payload(analysis)? else {
            return Ok((
                FunctionQuery {
                    file: self.file.clone(),
                    status: self.status.clone(),
                    classification: self.classification.clone(),
                    minimum_risk: self.minimum_risk.clone(),
                    min_complexity_delta: self.min_complexity_delta,
                    include_unchanged: self.include_unchanged.unwrap_or(false),
                    limit: query::canonical_limit(self.limit),
                },
                0,
            ));
        };
        let CursorQuery::ListChangedFunctions(applied) = payload.query else {
            return Err(invalid_params(
                "`cursor` was issued for another method; list_changed_functions cannot continue it"
                    .to_owned(),
            ));
        };
        agrees("file", self.file.as_deref(), applied.file.as_deref())?;
        agrees("status", self.status.as_deref(), applied.status.as_deref())?;
        agrees(
            "classification",
            self.classification.as_deref(),
            applied.classification.as_deref(),
        )?;
        agrees(
            "minimum_risk",
            self.minimum_risk.as_deref(),
            applied.minimum_risk.as_deref(),
        )?;
        agrees(
            "min_complexity_delta",
            self.min_complexity_delta.as_ref(),
            applied.min_complexity_delta.as_ref(),
        )?;
        if self
            .include_unchanged
            .is_some_and(|requested| requested != applied.include_unchanged)
        {
            return Err(disagreement("include_unchanged"));
        }
        agrees(
            "limit",
            self.canonical_limit().as_ref(),
            Some(&applied.limit),
        )?;
        Ok((applied, payload.offset))
    }

    /// What the cursor carries, once it is known to belong to this analysis.
    fn cursor_payload(&self, analysis: &str) -> Result<Option<CursorPayload>, ProjectionError> {
        let Some(cursor) = self.cursor.as_deref() else {
            return Ok(None);
        };
        let payload = decode_cursor(cursor)?;
        if payload.schema_version != SCHEMA_VERSION {
            return Err(invalid_params(format!(
                "`cursor` was issued under schema version {}; this server answers in version \
                 {SCHEMA_VERSION}",
                payload.schema_version
            )));
        }
        if payload.analysis != analysis {
            return Err(invalid_params(format!(
                "`cursor` was issued for analysis `{}`, not `{analysis}`",
                payload.analysis
            )));
        }
        Ok(Some(payload))
    }
}

/// Reject a parameter that contradicts the query a cursor continues.
fn agrees<T: PartialEq + ?Sized>(
    field: &str,
    requested: Option<&T>,
    applied: Option<&T>,
) -> Result<(), ProjectionError> {
    if requested.is_some_and(|value| applied != Some(value)) {
        return Err(disagreement(field));
    }
    Ok(())
}

fn disagreement(field: &str) -> ProjectionError {
    invalid_params(format!(
        "`{field}` does not match the query `cursor` continues; a cursor continues one query, so \
         start a new page without it to ask something else"
    ))
}

// ---------------------------------------------------------------- cursors ---

/// The parameters a list of files was applied with, after defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileQuery {
    classification: Option<String>,
    minimum_risk: Option<String>,
    limit: usize,
}

/// The parameters a list of functions was applied with, after defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FunctionQuery {
    file: Option<String>,
    status: Option<String>,
    classification: Option<String>,
    minimum_risk: Option<String>,
    min_complexity_delta: Option<i64>,
    include_unchanged: bool,
    limit: usize,
}

/// The applied query a cursor continues, labelled with the method it belongs to.
///
/// The label is what stops a cursor from one list being spent on another: the
/// two share a shape in part, but never a meaning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
enum CursorQuery {
    ListChangedFiles(FileQuery),
    ListChangedFunctions(FunctionQuery),
}

/// What a cursor carries: which answer it continues, and where it got to.
///
/// Bound to the schema, the analysis, the method, and the applied query, so a
/// token from one question can never be spent on another, and a stale token
/// from a differently-built analysis is rejected instead of silently reading
/// rows the caller never saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CursorPayload {
    schema_version: u32,
    analysis: String,
    query: CursorQuery,
    offset: usize,
}

/// Encode a cursor: its fields as JSON, hex-encoded.
///
/// Opaque by construction rather than by obscurity: a caller has no reason to
/// read one, and every reason to hand back exactly what it was given.
fn encode_cursor(payload: &CursorPayload) -> Result<String, ProjectionError> {
    let bytes = serde_json::to_vec(payload).map_err(|error| serialization_failed(&error))?;
    let mut cursor = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            cursor.push(char::from(HEX_DIGITS[usize::from(nibble)]));
        }
    }
    Ok(cursor)
}

fn decode_cursor(cursor: &str) -> Result<CursorPayload, ProjectionError> {
    let undecodable = || invalid_params("`cursor` is not a cursor this server issued".to_owned());
    let bytes = hex_decode(cursor).ok_or_else(undecodable)?;
    serde_json::from_slice(&bytes).map_err(|_| undecodable())
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.chunks_exact(2).remainder().is_empty() {
        return None;
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        decoded.push((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?);
    }
    Some(decoded)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------- envelope ---

/// Which analysis answered, in what shape, by which tool.
#[derive(Serialize)]
pub(crate) struct AnalysisView {
    pub(crate) id: String,
    pub(crate) schema_version: u32,
    pub(crate) tool_version: &'static str,
    pub(crate) base: RevisionView,
    pub(crate) target: RevisionView,
}

#[derive(Serialize)]
pub(crate) struct RevisionView {
    pub(crate) id: String,
    pub(crate) display_name: String,
}

pub(crate) fn analysis_view(result: &AnalysisResult, id: String) -> AnalysisView {
    AnalysisView {
        id,
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION"),
        base: RevisionView {
            id: result.base.id.clone(),
            display_name: result.base.display_name.clone(),
        },
        target: RevisionView {
            id: result.target.id.clone(),
            display_name: result.target.display_name.clone(),
        },
    }
}

/// How much of a list an answer carried, and how to continue it.
///
/// `next_cursor` is always present, and is `null` exactly when the page is the
/// last one: a caller can hand it straight back in the next request without
/// deciding whether there is a next one.
#[derive(Serialize)]
pub(crate) struct PageView {
    pub(crate) returned: usize,
    pub(crate) total: usize,
    pub(crate) has_more: bool,
    pub(crate) next_cursor: Option<String>,
}

impl PageView {
    fn for_listing(
        window: query::PageWindow,
        applied: CursorQuery,
        analysis: &str,
    ) -> Result<Self, ProjectionError> {
        let next_cursor = match window.next_offset {
            Some(offset) => Some(encode_cursor(&CursorPayload {
                schema_version: SCHEMA_VERSION,
                analysis: analysis.to_owned(),
                query: applied,
                offset,
            })?),
            None => None,
        };
        Ok(Self {
            returned: window.returned,
            total: window.total,
            has_more: window.has_more,
            next_cursor,
        })
    }
}

/// One complete answer: which analysis answered, what was asked, and what was
/// found.
///
/// This is the answer itself rather than any transport's framing of it, so
/// every transport presents the same object: the answer to a question does not
/// depend on which adapter asked it.
#[derive(Serialize)]
pub(crate) struct Answer {
    pub(crate) analysis: AnalysisView,
    pub(crate) query: serde_json::Value,
    pub(crate) data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) page: Option<PageView>,
}

impl Answer {
    pub(crate) fn new(analysis: AnalysisView, projection: Projection) -> Self {
        Self {
            analysis,
            query: projection.query,
            data: projection.data,
            page: projection.page,
        }
    }
}
