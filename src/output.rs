use serde::Serialize;

use crate::{
    DiffHunk, FileStatus,
    analysis::FunctionChangeStatus,
    languages::{DiagnosticSeverity, FunctionKind, Language, SourceRange},
    metrics::FunctionMetrics,
    result::{AnalysisResult, Diagnostic, DiagnosticCode, FileResult, FunctionResult},
};

/// Render a concise deterministic report for terminal users.
#[must_use]
pub fn render_human(result: &AnalysisResult) -> String {
    let mut output = String::new();
    push_line(
        &mut output,
        &format!(
            "DiffScope {}..{}",
            result.base.display_name, result.target.display_name
        ),
    );
    push_line(&mut output, &format!("Repository: {}", result.repository));
    push_line(
        &mut output,
        &format!(
            "{} changed files, +{} -{} ({} supported, {} unsupported)",
            result.summary.changed_files,
            result.summary.added_lines,
            result.summary.removed_lines,
            result.summary.supported_files,
            result.summary.unsupported_files
        ),
    );

    for file in &result.files {
        let path = display_path(file);
        push_line(
            &mut output,
            &format!(
                "{} {} (+{} -{})",
                file_status(file.status),
                path,
                file.added_lines,
                file.removed_lines
            ),
        );
        for function in &file.functions {
            let before = function
                .metrics_before
                .as_ref()
                .map_or("-".to_owned(), concise_metrics);
            let after = function
                .metrics_after
                .as_ref()
                .map_or("-".to_owned(), concise_metrics);
            push_line(
                &mut output,
                &format!(
                    "  {} {} [{} -> {}]",
                    function_status(function.status),
                    function.qualified_name,
                    before,
                    after
                ),
            );
        }
        for diagnostic in &file.diagnostics {
            render_diagnostic(&mut output, diagnostic, "  ");
        }
    }
    for diagnostic in &result.diagnostics {
        render_diagnostic(&mut output, diagnostic, "");
    }
    output
}

/// Render schema-versioned, pretty-printed JSON with a trailing newline.
///
/// # Errors
///
/// Returns an error if serialization unexpectedly fails.
pub fn render_json(result: &AnalysisResult) -> Result<String, serde_json::Error> {
    let mut output = serde_json::to_string_pretty(&JsonResult::from(result))?;
    output.push('\n');
    Ok(output)
}

pub(crate) fn json_value(result: &AnalysisResult) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(JsonResult::from(result))
}

fn push_line(output: &mut String, line: &str) {
    output.push_str(line);
    output.push('\n');
}

fn display_path(file: &FileResult) -> String {
    match (&file.base_path, &file.target_path) {
        (Some(base), Some(target)) if base != target => format!("{base} -> {target}"),
        (_, Some(target)) => target.clone(),
        (Some(base), None) => base.clone(),
        (None, None) => "<unknown>".to_owned(),
    }
}

fn concise_metrics(metrics: &FunctionMetrics) -> String {
    format!(
        "loc {}/{}, cyclo {}, cognitive {}",
        metrics.physical_loc,
        metrics.source_loc,
        metrics.cyclomatic_complexity,
        metrics.cognitive_complexity
    )
}

fn render_diagnostic(output: &mut String, diagnostic: &Diagnostic, indent: &str) {
    push_line(
        output,
        &format!(
            "{indent}{} {}: {}",
            severity(diagnostic.severity),
            diagnostic_code(diagnostic.code),
            diagnostic.message
        ),
    );
}

fn file_status(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "added",
        FileStatus::Deleted => "deleted",
        FileStatus::Modified => "modified",
        FileStatus::Renamed => "renamed",
        FileStatus::Binary => "binary",
    }
}

fn function_status(status: FunctionChangeStatus) -> &'static str {
    match status {
        FunctionChangeStatus::Added => "added",
        FunctionChangeStatus::Removed => "removed",
        FunctionChangeStatus::Modified => "modified",
        FunctionChangeStatus::Unchanged => "unchanged",
    }
}

fn language(language: Language) -> &'static str {
    match language {
        Language::TypeScript => "typescript",
        Language::Tsx => "tsx",
    }
}

fn function_kind(kind: FunctionKind) -> &'static str {
    match kind {
        FunctionKind::Function => "function",
        FunctionKind::Method => "method",
        FunctionKind::Constructor => "constructor",
        FunctionKind::ArrowFunction => "arrow_function",
    }
}

fn severity(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Info => "info",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Error => "error",
    }
}

fn diagnostic_code(code: DiagnosticCode) -> &'static str {
    match code {
        DiagnosticCode::UnsupportedLanguage => "unsupported_language",
        DiagnosticCode::BinaryFile => "binary_file",
        DiagnosticCode::MalformedSource => "malformed_source",
        DiagnosticCode::ParseError => "parse_error",
        DiagnosticCode::InvalidUtf8 => "invalid_utf8",
        DiagnosticCode::OversizedFile => "oversized_file",
        DiagnosticCode::MissingBlob => "missing_blob",
        DiagnosticCode::GitError => "git_error",
        DiagnosticCode::AmbiguousFunctionMatch => "ambiguous_function_match",
        DiagnosticCode::MetricUnavailable => "metric_unavailable",
    }
}

#[derive(Serialize)]
struct JsonResult<'a> {
    schema_version: u32,
    tool_version: &'a str,
    repository: &'a str,
    base: JsonRevision<'a>,
    target: JsonRevision<'a>,
    summary: JsonSummary,
    files: Vec<JsonFile<'a>>,
    diagnostics: Vec<JsonDiagnostic<'a>>,
}

impl<'a> From<&'a AnalysisResult> for JsonResult<'a> {
    fn from(result: &'a AnalysisResult) -> Self {
        Self {
            schema_version: result.schema_version,
            tool_version: &result.tool_version,
            repository: &result.repository,
            base: JsonRevision {
                id: &result.base.id,
                display_name: &result.base.display_name,
            },
            target: JsonRevision {
                id: &result.target.id,
                display_name: &result.target.display_name,
            },
            summary: JsonSummary {
                changed_files: result.summary.changed_files,
                added_lines: result.summary.added_lines,
                removed_lines: result.summary.removed_lines,
                supported_files: result.summary.supported_files,
                unsupported_files: result.summary.unsupported_files,
                diagnostics: JsonDiagnosticCounts {
                    info: result.summary.diagnostics.info,
                    warning: result.summary.diagnostics.warning,
                    error: result.summary.diagnostics.error,
                },
            },
            files: result.files.iter().map(JsonFile::from).collect(),
            diagnostics: result
                .diagnostics
                .iter()
                .map(JsonDiagnostic::from)
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct JsonRevision<'a> {
    id: &'a str,
    display_name: &'a str,
}

#[derive(Serialize)]
struct JsonSummary {
    changed_files: u32,
    added_lines: u32,
    removed_lines: u32,
    supported_files: u32,
    unsupported_files: u32,
    diagnostics: JsonDiagnosticCounts,
}

#[derive(Serialize)]
struct JsonDiagnosticCounts {
    info: u32,
    warning: u32,
    error: u32,
}

#[derive(Serialize)]
struct JsonFile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    base_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_path: Option<&'a str>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'static str>,
    is_binary: bool,
    added_lines: u32,
    removed_lines: u32,
    hunks: Vec<JsonHunk>,
    functions: Vec<JsonFunction<'a>>,
    diagnostics: Vec<JsonDiagnostic<'a>>,
}

impl<'a> From<&'a FileResult> for JsonFile<'a> {
    fn from(file: &'a FileResult) -> Self {
        Self {
            base_path: file.base_path.as_deref(),
            target_path: file.target_path.as_deref(),
            status: file_status(file.status),
            language: file.language.map(language),
            is_binary: file.is_binary,
            added_lines: file.added_lines,
            removed_lines: file.removed_lines,
            hunks: file.hunks.iter().map(JsonHunk::from).collect(),
            functions: file.functions.iter().map(JsonFunction::from).collect(),
            diagnostics: file.diagnostics.iter().map(JsonDiagnostic::from).collect(),
        }
    }
}

#[derive(Serialize)]
struct JsonHunk {
    base_start: u32,
    base_count: u32,
    target_start: u32,
    target_count: u32,
    added_lines: u32,
    removed_lines: u32,
}

impl From<&DiffHunk> for JsonHunk {
    fn from(hunk: &DiffHunk) -> Self {
        Self {
            base_start: hunk.base_start,
            base_count: hunk.base_count,
            target_start: hunk.target_start,
            target_count: hunk.target_count,
            added_lines: hunk.added_lines,
            removed_lines: hunk.removed_lines,
        }
    }
}

#[derive(Serialize)]
struct JsonFunction<'a> {
    id: &'a str,
    status: &'static str,
    kind: &'static str,
    qualified_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_range: Option<JsonRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_range: Option<JsonRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_before: Option<JsonMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_after: Option<JsonMetrics>,
    diagnostics: Vec<JsonDiagnostic<'a>>,
}

impl<'a> From<&'a FunctionResult> for JsonFunction<'a> {
    fn from(function: &'a FunctionResult) -> Self {
        Self {
            id: &function.id,
            status: function_status(function.status),
            kind: function_kind(function.kind),
            qualified_name: &function.qualified_name,
            base_range: function.base_range.as_ref().map(JsonRange::from),
            target_range: function.target_range.as_ref().map(JsonRange::from),
            metrics_before: function.metrics_before.as_ref().map(JsonMetrics::from),
            metrics_after: function.metrics_after.as_ref().map(JsonMetrics::from),
            diagnostics: function
                .diagnostics
                .iter()
                .map(JsonDiagnostic::from)
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct JsonMetrics {
    physical_loc: u32,
    source_loc: u32,
    cyclomatic_complexity: u32,
    cognitive_complexity: u32,
}

impl From<&FunctionMetrics> for JsonMetrics {
    fn from(metrics: &FunctionMetrics) -> Self {
        Self {
            physical_loc: metrics.physical_loc,
            source_loc: metrics.source_loc,
            cyclomatic_complexity: metrics.cyclomatic_complexity,
            cognitive_complexity: metrics.cognitive_complexity,
        }
    }
}

#[derive(Serialize)]
struct JsonDiagnostic<'a> {
    code: &'static str,
    severity: &'static str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<JsonRange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    related_entity_ids: Vec<&'a str>,
}

impl<'a> From<&'a Diagnostic> for JsonDiagnostic<'a> {
    fn from(diagnostic: &'a Diagnostic) -> Self {
        Self {
            code: diagnostic_code(diagnostic.code),
            severity: severity(diagnostic.severity),
            message: &diagnostic.message,
            path: diagnostic.path.as_deref(),
            range: diagnostic.range.as_ref().map(JsonRange::from),
            related_entity_ids: diagnostic
                .related_entity_ids
                .iter()
                .map(String::as_str)
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct JsonRange {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

impl From<&SourceRange> for JsonRange {
    fn from(range: &SourceRange) -> Self {
        Self {
            start_line: range.start_line,
            start_column: range.start_column,
            end_line: range.end_line,
            end_column: range.end_column,
        }
    }
}
