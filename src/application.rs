use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    AnalysisRequest, DiffScopeError, FileChange, FileStatus,
    analysis::{
        FileFunctionChanges, FunctionMappingDiagnostic, FunctionMappingDiagnosticCode,
        map_changed_functions,
    },
    inventory_changes,
    languages::{DiagnosticSeverity, detect_language},
    result::{
        AnalysisResult, AnalysisSummary, Diagnostic, DiagnosticCode, FileResult, FunctionResult,
        RevisionResult, SCHEMA_VERSION, sort_diagnostics,
    },
};

/// Analyze all changed files and functions between the requested Git revisions.
///
/// This is the stable application entry point used by the CLI and future
/// adapters. It never reads from or modifies the working tree.
///
/// # Errors
///
/// Returns a typed error when the repository or revisions cannot be resolved,
/// Git cannot produce the change inventory, or a supported analyzer cannot be
/// initialized.
pub fn analyze(request: &AnalysisRequest) -> Result<AnalysisResult, DiffScopeError> {
    let inventory = inventory_changes(request)?;
    let mut files = Vec::with_capacity(inventory.files.len());
    let mut supported_files = 0;
    let mut unsupported_files = 0;
    let mut next_function_id = 1_u64;

    let mapped_files = map_files(&inventory.files)?;

    for (file, mapped) in inventory.files.iter().zip(mapped_files) {
        let (result, supported) = build_file_result(file, mapped, &mut next_function_id);
        if supported {
            supported_files += 1;
        } else {
            unsupported_files += 1;
        }
        files.push(result);
    }

    let mut summary = AnalysisSummary {
        changed_files: inventory.summary.changed_files,
        added_lines: inventory.summary.added_lines,
        removed_lines: inventory.summary.removed_lines,
        supported_files,
        unsupported_files,
        ..AnalysisSummary::default()
    };
    for diagnostic in
        files
            .iter()
            .flat_map(|file| file.diagnostics.iter())
            .chain(files.iter().flat_map(|file| {
                file.functions
                    .iter()
                    .flat_map(|function| function.diagnostics.iter())
            }))
    {
        summary.diagnostics.add(diagnostic.severity);
    }

    Ok(AnalysisResult {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        repository: inventory.repository_path.to_string_lossy().into_owned(),
        base: RevisionResult {
            id: inventory.base.commit_id,
            display_name: inventory.base.input,
        },
        target: RevisionResult {
            id: inventory.target.commit_id,
            display_name: inventory.target.input,
        },
        summary,
        files,
        diagnostics: Vec::new(),
    })
}

/// Map every changed file to its function changes, using the available cores.
///
/// Parsing both revisions of a file is the dominant cost of an analysis and is
/// independent per file. Workers claim files from one shared cursor rather than
/// taking a fixed slice each, so a single very large file cannot leave the
/// other workers idle.
///
/// Each result carries the index of its input, and results are restored to
/// input order before they are returned. Parallel execution therefore does not
/// affect result ordering, and a failing analysis reports the error of the
/// earliest file exactly as a sequential pass does.
fn map_files(files: &[FileChange]) -> Result<Vec<FileFunctionChanges>, DiffScopeError> {
    let workers = worker_count(files.len());
    if workers < 2 {
        return files.iter().map(map_changed_functions).collect();
    }

    let cursor = AtomicUsize::new(0);
    let mut claimed = Vec::with_capacity(files.len());
    let mut worker_lost = false;

    std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut mapped = Vec::new();
                    while let Some(index) = claim_index(&cursor, files.len()) {
                        if let Some(file) = files.get(index) {
                            mapped.push((index, map_changed_functions(file)));
                        }
                    }
                    mapped
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            match handle.join() {
                Ok(mapped) => claimed.extend(mapped),
                Err(_) => worker_lost = true,
            }
        }
    });

    if worker_lost {
        return Err(DiffScopeError::Language(
            "a file analysis worker terminated unexpectedly".to_owned(),
        ));
    }

    claimed.sort_by_key(|(index, _)| *index);
    claimed
        .into_iter()
        .map(|(_, mapped)| mapped)
        .collect::<Result<Vec<_>, _>>()
}

fn worker_count(file_count: usize) -> usize {
    if file_count < 2 {
        return 1;
    }
    let available = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    available.min(file_count)
}

/// Claim the next unanalyzed file index, or report that none is left.
fn claim_index(cursor: &AtomicUsize, file_count: usize) -> Option<usize> {
    let index = cursor.fetch_add(1, Ordering::Relaxed);
    (index < file_count).then_some(index)
}

fn build_file_result(
    file: &FileChange,
    mapped: FileFunctionChanges,
    next_function_id: &mut u64,
) -> (FileResult, bool) {
    let path = file.target_path.as_deref().or(file.base_path.as_deref());
    let language = path.and_then(|path| detect_language(Path::new(path)));
    let is_binary = file.status == FileStatus::Binary;
    let mut diagnostics = mapped
        .diagnostics
        .iter()
        .map(|diagnostic| mapping_diagnostic(diagnostic, path))
        .collect::<Vec<_>>();
    if is_binary {
        diagnostics.push(Diagnostic {
            code: DiagnosticCode::BinaryFile,
            severity: DiagnosticSeverity::Info,
            message: "binary file is inventoried without source metrics".to_owned(),
            path: path.map(str::to_owned),
            range: None,
            related_entity_ids: Vec::new(),
        });
    }
    diagnostics.sort_by(|left, right| diagnostic_identity(left).cmp(&diagnostic_identity(right)));
    diagnostics.dedup();
    sort_diagnostics(&mut diagnostics);

    let functions = mapped
        .functions
        .into_iter()
        .map(|function| {
            let id = format!("function-{next_function_id}");
            *next_function_id += 1;
            FunctionResult {
                id,
                status: function.status,
                kind: function.kind,
                qualified_name: function.qualified_name,
                base_range: function.base_range,
                target_range: function.target_range,
                metrics_before: function.metrics_before,
                metrics_after: function.metrics_after,
                diagnostics: Vec::new(),
            }
        })
        .collect();

    (
        FileResult {
            base_path: file.base_path.clone(),
            target_path: file.target_path.clone(),
            status: file.status,
            language,
            is_binary,
            added_lines: file.added_lines,
            removed_lines: file.removed_lines,
            hunks: file.hunks.clone(),
            functions,
            diagnostics,
        },
        language.is_some() && !is_binary,
    )
}

fn mapping_diagnostic(diagnostic: &FunctionMappingDiagnostic, path: Option<&str>) -> Diagnostic {
    Diagnostic {
        code: match diagnostic.code {
            FunctionMappingDiagnosticCode::AmbiguousFunctionMatch => {
                DiagnosticCode::AmbiguousFunctionMatch
            }
            FunctionMappingDiagnosticCode::UnsupportedLanguage => {
                DiagnosticCode::UnsupportedLanguage
            }
            FunctionMappingDiagnosticCode::MalformedSource => DiagnosticCode::MalformedSource,
            FunctionMappingDiagnosticCode::ParseError => DiagnosticCode::ParseError,
            FunctionMappingDiagnosticCode::InvalidUtf8 => DiagnosticCode::InvalidUtf8,
            FunctionMappingDiagnosticCode::OversizedFile => DiagnosticCode::OversizedFile,
            FunctionMappingDiagnosticCode::BlobUnavailable => DiagnosticCode::MissingBlob,
        },
        severity: diagnostic.severity,
        message: diagnostic.message.clone(),
        path: path.map(str::to_owned),
        range: diagnostic.range.clone(),
        related_entity_ids: diagnostic.qualified_name.iter().cloned().collect(),
    }
}

type DiagnosticIdentity<'a> = (
    DiagnosticCode,
    DiagnosticSeverity,
    &'a str,
    Option<(u32, u32, u32, u32)>,
);

fn diagnostic_identity(diagnostic: &Diagnostic) -> DiagnosticIdentity<'_> {
    (
        diagnostic.code,
        diagnostic.severity,
        &diagnostic.message,
        diagnostic.range.as_ref().map(|range| {
            (
                range.start_line,
                range.start_column,
                range.end_line,
                range.end_column,
            )
        }),
    )
}
