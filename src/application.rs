use std::{
    path::Path,
    sync::{
        Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{
    AnalysisRequest, BlobContent, DiffScopeError, FileChange, FileStatus,
    analysis::{
        FileFunctionChanges, FunctionMappingDiagnostic, FunctionMappingDiagnosticCode,
        map_changed_functions,
    },
    inventory_changes,
    languages::{DiagnosticSeverity, MAX_ANALYZED_BLOB_BYTES, detect_language},
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
    let budget = ByteBudget::new();
    let mut claimed = Vec::with_capacity(files.len());
    let mut worker_lost = false;

    std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut mapped = Vec::new();
                    while let Some(index) = claim_index(&cursor, files.len()) {
                        if let Some(file) = files.get(index) {
                            let cost = analysis_cost(file);
                            budget.acquire(cost);
                            let analyzed = map_changed_functions(file);
                            budget.release(cost);
                            mapped.push((index, analyzed));
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

/// Source bytes that may be under analysis at any one time.
///
/// A syntax tree costs many times the source it describes, and a file's two
/// revisions are parsed together, so peak memory follows the bytes in flight
/// rather than the number of workers. Ordinary diffs stay far below this limit
/// and run fully parallel; a diff of many large files is throttled instead of
/// holding one syntax tree per worker at once.
const ANALYSIS_BYTES_IN_FLIGHT: usize = 8 * 1024 * 1024;

/// A waiting room that keeps the bytes under analysis below a fixed budget.
struct ByteBudget {
    in_flight: Mutex<usize>,
    released: Condvar,
}

impl ByteBudget {
    fn new() -> Self {
        Self {
            in_flight: Mutex::new(0),
            released: Condvar::new(),
        }
    }

    /// Wait until this file's bytes fit alongside the work already in flight.
    ///
    /// A file larger than the whole budget is admitted whenever nothing else is
    /// in flight, so no file can deadlock the analysis by being too large.
    fn acquire(&self, cost: usize) {
        let mut in_flight = lock(&self.in_flight);
        while *in_flight > 0 && in_flight.saturating_add(cost) > ANALYSIS_BYTES_IN_FLIGHT {
            in_flight = self
                .released
                .wait(in_flight)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *in_flight = in_flight.saturating_add(cost);
    }

    fn release(&self, cost: usize) {
        let mut in_flight = lock(&self.in_flight);
        *in_flight = in_flight.saturating_sub(cost);
        drop(in_flight);
        self.released.notify_all();
    }
}

/// A poisoned budget means another worker failed; its count is still usable,
/// and that worker's failure is reported when it is joined.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bytes a file contributes to the budget: the blobs that will actually be
/// parsed. Blobs above the analysis size limit are never parsed and cost
/// nothing, so one oversized file does not reserve the whole budget.
fn analysis_cost(file: &FileChange) -> usize {
    [&file.base_blob, &file.target_blob]
        .into_iter()
        .map(|blob| match blob {
            BlobContent::Available(source) if source.len() <= MAX_ANALYZED_BLOB_BYTES => {
                source.len()
            }
            _ => 0,
        })
        .sum()
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

    let exports_added = mapped.exports_added.clone();
    let exports_removed = mapped.exports_removed.clone();
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
                churn: function.churn,
                match_confidence: function.match_confidence,
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
            exports_added,
            exports_removed,
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

#[cfg(test)]
mod tests {
    use super::{ANALYSIS_BYTES_IN_FLIGHT, ByteBudget, analysis_cost};
    use crate::{BlobContent, FileChange, FileStatus, languages::MAX_ANALYZED_BLOB_BYTES};

    #[test]
    fn admits_work_larger_than_the_whole_budget() {
        // A file too large for the budget must still be analyzed rather than
        // waiting for room that can never appear.
        let budget = ByteBudget::new();

        budget.acquire(ANALYSIS_BYTES_IN_FLIGHT * 4);
        budget.release(ANALYSIS_BYTES_IN_FLIGHT * 4);
    }

    #[test]
    fn admits_every_worker_even_when_they_exceed_the_budget_together() {
        let budget = ByteBudget::new();
        let each = ANALYSIS_BYTES_IN_FLIGHT * 3 / 4;
        let completed = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    budget.acquire(each);
                    completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    budget.release(each);
                });
            }
        });

        assert_eq!(completed.load(std::sync::atomic::Ordering::Relaxed), 8);
    }

    #[test]
    fn counts_only_the_blobs_that_will_be_parsed() {
        let small = vec![b'x'; 128];
        let oversized = vec![b'x'; MAX_ANALYZED_BLOB_BYTES + 1];

        assert_eq!(analysis_cost(&file_change(&small, &small)), 256);
        // An oversized blob is never parsed, so it must not reserve budget.
        assert_eq!(analysis_cost(&file_change(&oversized, &small)), 128);
        assert_eq!(analysis_cost(&file_change(&oversized, &oversized)), 0);
    }

    fn file_change(base: &[u8], target: &[u8]) -> FileChange {
        FileChange {
            base_path: Some("sample.ts".to_owned()),
            target_path: Some("sample.ts".to_owned()),
            status: FileStatus::Modified,
            old_blob_id: Some("base".to_owned()),
            new_blob_id: Some("target".to_owned()),
            base_blob: BlobContent::Available(base.to_vec()),
            target_blob: BlobContent::Available(target.to_vec()),
            added_lines: 0,
            removed_lines: 0,
            hunks: Vec::new(),
        }
    }
}
