use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use super::{HarnessErrorCode, HarnessOutcome, HarnessRequest, HarnessResponse, execute};
use crate::output;

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

        let response = process_line(&line)?;
        serde_json::to_writer(&mut writer, &response).map_err(JsonlAdapterError::Serialization)?;
        writer.write_all(b"\n").map_err(JsonlAdapterError::Io)?;
        writer.flush().map_err(JsonlAdapterError::Io)?;
    }
}

fn process_line(line: &[u8]) -> Result<WireResponse, JsonlAdapterError> {
    let request = match serde_json::from_slice::<WireRequest>(line) {
        Ok(request) => request,
        Err(error) => {
            return Ok(WireResponse::error(
                request_id(line),
                "malformed_request",
                format!("invalid request: {error}"),
            ));
        }
    };
    if request.protocol_version != PROTOCOL_VERSION {
        return Ok(WireResponse::error(
            Some(request.id),
            "unsupported_protocol_version",
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.protocol_version
            ),
        ));
    }

    response_for(execute(HarnessRequest {
        id: request.id,
        repository: request.repository,
        base_revision: request.base,
        target_revision: request.target,
    }))
}

fn response_for(response: HarnessResponse) -> Result<WireResponse, JsonlAdapterError> {
    match response.outcome {
        HarnessOutcome::Success(result) => Ok(WireResponse {
            protocol_version: PROTOCOL_VERSION,
            id: Some(response.id),
            result: Some(output::json_value(&result).map_err(JsonlAdapterError::Serialization)?),
            error: None,
        }),
        HarnessOutcome::Error(error) => Ok(WireResponse::error(
            Some(response.id),
            match error.code {
                HarnessErrorCode::AnalysisFailed => "analysis_failed",
            },
            error.message,
        )),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    protocol_version: u32,
    id: String,
    repository: String,
    base: String,
    target: String,
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
