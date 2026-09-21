use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use std::path::PathBuf;

use super::{HarnessErrorCode, HarnessOutcome, HarnessRequest, HarnessSession};
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

pub const PROTOCOL_VERSION: u32 = 1;

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

    let params = request.params.unwrap_or_default();
    match project(method, &result, &params, index.as_deref()) {
        Ok(value) => WireResponse {
            protocol_version: PROTOCOL_VERSION,
            id: Some(id),
            result: Some(value),
            error: None,
        },
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

    /// A request without a method asks for the complete analysis, which is what
    /// the protocol has always returned. Older clients therefore keep working
    /// unchanged.
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

/// Render the answer to one method as JSON.
fn project(
    method: Method,
    result: &AnalysisResult,
    params: &WireParams,
    index: Option<&ImportIndex>,
) -> Result<serde_json::Value, ProjectionError> {
    let value = match method {
        Method::Analyze => {
            output::json_value(result).map_err(|error| serialization_failed(&error))?
        }
        Method::ChangeSummary => to_value(&query::change_summary(result, index))
            .map_err(|error| serialization_failed(&error))?,
        Method::ListChangedFiles => {
            let filter = params.file_filter()?;
            to_value(&query::list_changed_files(result, &filter, index))
                .map_err(|error| serialization_failed(&error))?
        }
        Method::ListChangedFunctions => {
            let filter = params.function_filter()?;
            to_value(&query::list_changed_functions(result, &filter, index))
                .map_err(|error| serialization_failed(&error))?
        }
        Method::GetFunctionChange => {
            let file = params
                .file
                .as_deref()
                .ok_or_else(|| invalid_params("`file` is required".to_owned()))?;
            let symbol = params
                .symbol
                .as_deref()
                .ok_or_else(|| invalid_params("`symbol` is required".to_owned()))?;
            match query::get_function_change(result, file, symbol, index) {
                Ok(detail) => to_value(&detail).map_err(|error| serialization_failed(&error))?,
                Err(unknown) => {
                    return Err(ProjectionError {
                        code: "unknown_function",
                        message: describe_unknown(&unknown),
                    });
                }
            }
        }
        Method::GetAnalysisDiagnostics => to_value(&query::get_analysis_diagnostics(
            result,
            params.file.as_deref(),
        ))
        .map_err(|error| serialization_failed(&error))?,
    };
    Ok(value)
}

fn to_value<T: Serialize>(value: &T) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(value)
}

fn serialization_failed(error: &serde_json::Error) -> ProjectionError {
    ProjectionError {
        code: "serialization_failed",
        message: error.to_string(),
    }
}

/// Name the symbols the file does contain, so a caller can correct itself in
/// one step instead of guessing again.
fn describe_unknown(unknown: &query::UnknownFunction) -> String {
    /// Symbols named in the error before it is summarized.
    const SUGGESTIONS: usize = 10;

    if unknown.known_symbols.is_empty() {
        return format!(
            "no file `{}` in this comparison, so `{}` cannot be resolved",
            unknown.file, unknown.symbol
        );
    }
    let shown = unknown
        .known_symbols
        .iter()
        .take(SUGGESTIONS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let remainder = unknown.known_symbols.len().saturating_sub(SUGGESTIONS);
    let more = if remainder == 0 {
        String::new()
    } else {
        format!(" and {remainder} more")
    };
    format!(
        "`{}` has no symbol `{}`; it contains {shown}{more}",
        unknown.file, unknown.symbol
    )
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
/// not apply to the method being called are simply unused.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireParams {
    file: Option<String>,
    symbol: Option<String>,
    status: Option<String>,
    classification: Option<String>,
    minimum_risk: Option<String>,
    min_complexity_delta: Option<i64>,
    include_unchanged: Option<bool>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl WireParams {
    fn page(&self) -> Page {
        Page {
            limit: self.limit,
            offset: self.offset.unwrap_or(0),
        }
    }

    fn file_filter(&self) -> Result<FileFilter, ProjectionError> {
        Ok(FileFilter {
            classification: self.classification()?,
            minimum_risk: self.minimum_risk()?,
            page: self.page(),
        })
    }

    fn function_filter(&self) -> Result<FunctionFilter, ProjectionError> {
        Ok(FunctionFilter {
            file: self.file.clone(),
            status: self.status()?,
            classification: self.classification()?,
            minimum_risk: self.minimum_risk()?,
            min_complexity_delta: self.min_complexity_delta,
            include_unchanged: self.include_unchanged.unwrap_or(false),
            page: self.page(),
        })
    }

    fn classification(&self) -> Result<Option<FileClassification>, ProjectionError> {
        parse_enum(
            self.classification.as_deref(),
            FileClassification::parse,
            "classification",
            "lockfile, vendored, generated, test, config, docs, source",
        )
    }

    fn minimum_risk(&self) -> Result<Option<RiskLevel>, ProjectionError> {
        parse_enum(
            self.minimum_risk.as_deref(),
            RiskLevel::parse,
            "minimum_risk",
            "low, medium, high",
        )
    }

    fn status(&self) -> Result<Option<FunctionChangeStatus>, ProjectionError> {
        parse_enum(
            self.status.as_deref(),
            |value| match value {
                "added" => Some(FunctionChangeStatus::Added),
                "removed" => Some(FunctionChangeStatus::Removed),
                "modified" => Some(FunctionChangeStatus::Modified),
                "unchanged" => Some(FunctionChangeStatus::Unchanged),
                _ => None,
            },
            "status",
            "added, removed, modified, unchanged",
        )
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

#[derive(Serialize)]
struct WireResponse {
    protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

impl WireResponse {
    fn error(id: Option<String>, code: &'static str, message: String) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(WireError { code, message }),
        }
    }
}

#[derive(Serialize)]
struct WireError {
    code: &'static str,
    message: String,
}
