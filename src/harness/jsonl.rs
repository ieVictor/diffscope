//! Answer queries over newline-delimited JSON.
//!
//! Every successful response carries the same envelope: which analysis answered
//! the request, what was actually asked after defaults and normalization, and
//! the answer itself. A caller that paged a list gets a cursor back and hands
//! it over unchanged to continue; nothing else about the transport's state is
//! expressed in the answer.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use std::path::PathBuf;

use super::{
    HarnessErrorCode, HarnessOutcome, HarnessRequest, HarnessSession, SCHEMA_VERSION, analysis_id,
};
use crate::AnalysisRequest;
use crate::{
    analysis::FunctionChangeStatus,
    imports::ImportIndex,
    output,
    query::{
        self, FileFilter, FunctionFilter, Page, classify::FileClassification, risk::RiskLevel,
    },
    result::AnalysisResult,
};

/// Version of the request and response transport.
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug)]
pub enum JsonlAdapterError {
    Io(io::Error),
    Serialization(serde_json::Error),
}

impl std::fmt::Display for JsonlAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "JSONL I/O failed: {error}"),
            Self::Serialization(error) => write!(formatter, "JSONL serialization failed: {error}"),
        }
    }
}

impl std::error::Error for JsonlAdapterError {}

/// Serve independent analysis requests from newline-delimited JSON.
///
/// One response is written and flushed for each non-empty input line. Malformed
/// requests produce an error response without terminating the stream.
///
/// # Errors
///
/// Returns an error only when the input/output transport fails or a response
/// cannot be serialized.
pub fn serve<R: BufRead, W: Write>(mut reader: R, mut writer: W) -> Result<(), JsonlAdapterError> {
    let session = HarnessSession::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line)
            .map_err(JsonlAdapterError::Io)?;
        if bytes_read == 0 {
            return Ok(());
        }
        trim_line_ending(&mut line);
        if line.is_empty() {
            continue;
        }

        let response = process_line(&session, &line);
        serde_json::to_writer(&mut writer, &response).map_err(JsonlAdapterError::Serialization)?;
        writer.write_all(b"\n").map_err(JsonlAdapterError::Io)?;
        writer.flush().map_err(JsonlAdapterError::Io)?;
    }
}

fn process_line(session: &HarnessSession, line: &[u8]) -> WireResponse {
    let request = match serde_json::from_slice::<WireRequest>(line) {
        Ok(request) => request,
        Err(error) => {
            return WireResponse::error(
                request_id(line),
                "malformed_request",
                format!("invalid request: {error}"),
            );
        }
    };
    if request.protocol_version != PROTOCOL_VERSION {
        return WireResponse::error(
            Some(request.id),
            "unsupported_protocol_version",
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.protocol_version
            ),
        );
    }

    let method = match Method::parse(request.method.as_deref()) {
        Ok(method) => method,
        Err(message) => {
            return WireResponse::error(Some(request.id), "unknown_method", message);
        }
    };

    let analysis_request = AnalysisRequest {
        repository_path: PathBuf::from(&request.repository),
        base_revision: request.base.clone(),
        target_revision: request.target.clone(),
    };
    let response = session.execute(HarnessRequest {
        id: request.id,
        repository: request.repository,
        base_revision: request.base,
        target_revision: request.target,
    });
    let id = response.id.clone();
    let result = match response.outcome {
        HarnessOutcome::Error(error) => {
            return WireResponse::error(
                Some(id),
                match error.code {
                    HarnessErrorCode::AnalysisFailed => "analysis_failed",
                },
                error.message,
            );
        }
        HarnessOutcome::Success(result) => result,
    };

    // The import graph is built only for the questions that use it, because it
    // reads every source file of the revision rather than only the changed ones.
    let index = if method.needs_import_graph() {
        match session.import_index(&analysis_request) {
            Ok(index) => Some(index),
            Err(error) => {
                return WireResponse::error(Some(id), "analysis_failed", error.to_string());
            }
        }
    } else {
        None
    };

    let analysis = analysis_id(&result.base.id, &result.target.id);
    let params = request.params.unwrap_or_default();
    match project(method, &result, &params, index.as_deref(), &analysis) {
        Ok(answer) => WireResponse::success(id, analysis_view(&result, analysis), answer),
        Err(error) => WireResponse::error(Some(id), error.code, error.message),
    }
}

/// The question a request is asking about a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    /// The complete analysis, exactly as the CLI emits it.
    Analyze,
    ChangeSummary,
    ListChangedFiles,
    ListChangedFunctions,
    GetFunctionChange,
    GetAnalysisDiagnostics,
}

impl Method {
    /// Whether answering this question needs the revision's import graph.
    fn needs_import_graph(self) -> bool {
        matches!(
            self,
            Self::ChangeSummary
                | Self::ListChangedFiles
                | Self::ListChangedFunctions
                | Self::GetFunctionChange
        )
    }

    /// Whether this question is answered a page at a time.
    fn paginated(self) -> bool {
        matches!(self, Self::ListChangedFiles | Self::ListChangedFunctions)
    }

    /// A request without a method asks for the complete analysis.
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("analyze") => Ok(Self::Analyze),
            Some("get_change_summary") => Ok(Self::ChangeSummary),
            Some("list_changed_files") => Ok(Self::ListChangedFiles),
            Some("list_changed_functions") => Ok(Self::ListChangedFunctions),
            Some("get_function_change") => Ok(Self::GetFunctionChange),
            Some("get_analysis_diagnostics") => Ok(Self::GetAnalysisDiagnostics),
            Some(other) => Err(format!(
                "unknown method `{other}`; expected analyze, get_change_summary, \
                 list_changed_files, list_changed_functions, get_function_change, \
                 or get_analysis_diagnostics"
            )),
        }
    }
}

struct ProjectionError {
    code: &'static str,
    message: String,
}

fn invalid_params(message: String) -> ProjectionError {
    ProjectionError {
        code: "invalid_params",
        message,
    }
}

/// One successful answer, before it is put in the envelope.
struct Projection {
    /// The parameters that were actually applied, defaults included.
    query: serde_json::Value,
    data: serde_json::Value,
    /// How much of a list this answer carried. Absent for whole-value methods.
    page: Option<WirePage>,
}

/// Render the answer to one method.
fn project(
    method: Method,
    result: &AnalysisResult,
    params: &WireParams,
    index: Option<&ImportIndex>,
    analysis: &str,
) -> Result<Projection, ProjectionError> {
    if params.cursor.is_some() && !method.paginated() {
        return Err(invalid_params(
            "`cursor` is only accepted by list_changed_files and list_changed_functions".to_owned(),
        ));
    }

    match method {
        Method::Analyze => complete_analysis(result),
        Method::ChangeSummary => summary(result, index),
        Method::ListChangedFiles => file_list(result, params, index, analysis),
        Method::ListChangedFunctions => function_list(result, params, index, analysis),
        Method::GetFunctionChange => function_detail(result, params, index),
        Method::GetAnalysisDiagnostics => diagnostics(result, params),
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
    params: &WireParams,
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
    let page = WirePage::for_listing(
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
    params: &WireParams,
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
    let page = WirePage::for_listing(
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
    params: &WireParams,
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

/// Diagnostics for the analysis, or for one file of it.
fn diagnostics(
    result: &AnalysisResult,
    params: &WireParams,
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

fn request_id(line: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

fn trim_line_ending(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        let _newline = line.pop();
    }
    if line.last() == Some(&b'\r') {
        let _carriage_return = line.pop();
    }
}

// ------------------------------------------------------------- the request ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    protocol_version: u32,
    id: String,
    repository: String,
    base: String,
    target: String,
    /// The question being asked. Absent means the complete analysis.
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<WireParams>,
}

/// Every parameter any method accepts.
///
/// One shape for all methods keeps the wire format obvious and lets an unknown
/// field be rejected outright rather than silently ignored. Parameters that do
/// not apply to the method being called are unused.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireParams {
    file: Option<String>,
    function_id: Option<String>,
    status: Option<String>,
    classification: Option<String>,
    minimum_risk: Option<String>,
    min_complexity_delta: Option<i64>,
    include_unchanged: Option<bool>,
    limit: Option<usize>,
    cursor: Option<String>,
}

impl WireParams {
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

// -------------------------------------------------------------- the cursor ---

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

// ------------------------------------------------------------ the response ---

#[derive(Serialize)]
struct WireResponse {
    protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<WireResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

impl WireResponse {
    fn success(id: String, analysis: AnalysisView, projection: Projection) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id: Some(id),
            result: Some(WireResult {
                analysis,
                query: projection.query,
                data: projection.data,
                page: projection.page,
            }),
            error: None,
        }
    }

    fn error(id: Option<String>, code: &'static str, message: String) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(WireError { code, message }),
        }
    }
}

/// Which analysis answered, in what shape, by which tool.
#[derive(Serialize)]
struct WireResult {
    analysis: AnalysisView,
    query: serde_json::Value,
    data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<WirePage>,
}

#[derive(Serialize)]
struct AnalysisView {
    id: String,
    schema_version: u32,
    tool_version: &'static str,
    base: RevisionView,
    target: RevisionView,
}

#[derive(Serialize)]
struct RevisionView {
    id: String,
    display_name: String,
}

/// How much of a list an answer carried, and how to continue it.
///
/// `next_cursor` is always present, and is `null` exactly when the page is the
/// last one: a caller can hand it straight back in the next request without
/// deciding whether there is a next one.
#[derive(Serialize)]
struct WirePage {
    returned: usize,
    total: usize,
    has_more: bool,
    next_cursor: Option<String>,
}

impl WirePage {
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

fn analysis_view(result: &AnalysisResult, id: String) -> AnalysisView {
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

#[derive(Serialize)]
struct WireError {
    code: &'static str,
    message: String,
}
