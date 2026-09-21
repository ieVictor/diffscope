//! Answer queries over newline-delimited JSON.
//!
//! Every successful response carries the same envelope: which analysis answered
//! the request, what was actually asked after defaults and normalization, and
//! the answer itself. A caller that paged a list gets a cursor back and hands
//! it over unchanged to continue; nothing else about the transport's state is
//! expressed in the answer.
//!
//! What an answer contains is not this transport's concern: the projection
//! from analysis to answer lives in the harness core, so every transport asks
//! the same questions and reports the same answers.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use std::path::PathBuf;

use super::{Answer, HarnessSession, Method, QueryParams, answer};
use crate::AnalysisRequest;

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
        base_revision: request.base,
        target_revision: request.target,
    };
    let params = request.params.unwrap_or_default();
    match answer(session, method, &analysis_request, &params) {
        Ok(answer) => WireResponse::success(request.id, answer),
        Err(error) => WireResponse::error(Some(request.id), error.code, error.message),
    }
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
    params: Option<QueryParams>,
}

// ------------------------------------------------------------ the response ---

#[derive(Serialize)]
struct WireResponse {
    protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Answer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

impl WireResponse {
    fn success(id: String, answer: Answer) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id: Some(id),
            result: Some(answer),
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

#[derive(Serialize)]
struct WireError {
    code: &'static str,
    message: String,
}
