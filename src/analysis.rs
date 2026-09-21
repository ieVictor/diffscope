use std::{collections::BTreeMap, path::Path};

use crate::{
    BlobContent, DiffHunk, DiffScopeError, FileChange, FileStatus,
    languages::{
        DiagnosticSeverity, FunctionDefinition, FunctionKind, Language, LanguageDiagnostic,
        LanguageDiagnosticCode, SourceRange, analyze_source,
    },
    metrics::FunctionMetrics,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFunctionChanges {
    pub base_path: Option<String>,
    pub target_path: Option<String>,
    pub functions: Vec<FunctionChange>,
    pub diagnostics: Vec<FunctionMappingDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionChange {
    pub status: FunctionChangeStatus,
    pub language: Language,
    pub kind: FunctionKind,
    pub qualified_name: String,
    pub base_range: Option<SourceRange>,
    pub target_range: Option<SourceRange>,
    pub metrics_before: Option<FunctionMetrics>,
    pub metrics_after: Option<FunctionMetrics>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionChangeStatus {
    Added,
    Removed,
    Modified,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionMappingDiagnostic {
    pub code: FunctionMappingDiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub range: Option<SourceRange>,
    pub qualified_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionMappingDiagnosticCode {
    AmbiguousFunctionMatch,
    UnsupportedLanguage,
    MalformedSource,
    ParseError,
    InvalidUtf8,
    OversizedFile,
    BlobUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FunctionKey {
    language: Language,
    kind: FunctionKind,
    qualified_name: String,
}

/// Map one file-level Git change to before-and-after function changes.
///
/// # Errors
///
/// Returns an error when language analysis for an available supported source
/// blob fails before producing an analysis result.
pub fn map_changed_functions(file: &FileChange) -> Result<FileFunctionChanges, DiffScopeError> {
    let (base_functions, target_functions) = analyze_both_blobs(file)?;
    let mut diagnostics = Vec::new();
    diagnostics.extend(language_diagnostics(&base_functions.diagnostics));
    diagnostics.extend(language_diagnostics(&target_functions.diagnostics));

    if base_functions.unavailable {
        diagnostics.push(unavailable_diagnostic("base"));
    }
    if target_functions.unavailable {
        diagnostics.push(unavailable_diagnostic("target"));
    }

    let base_by_key = group_by_key(base_functions.functions);
    let target_by_key = group_by_key(target_functions.functions);
    let mut keys = base_by_key.keys().cloned().collect::<Vec<_>>();
    keys.extend(
        target_by_key
            .keys()
            .filter(|key| !base_by_key.contains_key(*key))
            .cloned(),
    );
    keys.sort();

    let mut functions = Vec::new();
    for key in keys {
        let base: &[FunctionDefinition] = base_by_key.get(&key).map_or(&[], Vec::as_slice);
        let target: &[FunctionDefinition] = target_by_key.get(&key).map_or(&[], Vec::as_slice);

        if base.len() > 1 || target.len() > 1 {
            diagnostics.push(ambiguous_diagnostic(key.qualified_name));
            continue;
        }

        match (base.first(), target.first()) {
            (Some(base_function), Some(target_function)) => {
                let status = if file.status == FileStatus::Added {
                    FunctionChangeStatus::Added
                } else if file.status == FileStatus::Deleted {
                    FunctionChangeStatus::Removed
                } else if function_changed(base_function, target_function, &file.hunks) {
                    FunctionChangeStatus::Modified
                } else {
                    FunctionChangeStatus::Unchanged
                };
                functions.push(FunctionChange {
                    status,
                    language: key.language,
                    kind: key.kind,
                    qualified_name: key.qualified_name,
                    base_range: Some(base_function.range.clone()),
                    target_range: Some(target_function.range.clone()),
                    metrics_before: Some(base_function.metrics.clone()),
                    metrics_after: Some(target_function.metrics.clone()),
                });
            }
            (Some(base_function), None) => functions.push(FunctionChange {
                status: FunctionChangeStatus::Removed,
                language: key.language,
                kind: key.kind,
                qualified_name: key.qualified_name,
                base_range: Some(base_function.range.clone()),
                target_range: None,
                metrics_before: Some(base_function.metrics.clone()),
                metrics_after: None,
            }),
            (None, Some(target_function)) => functions.push(FunctionChange {
                status: FunctionChangeStatus::Added,
                language: key.language,
                kind: key.kind,
                qualified_name: key.qualified_name,
                base_range: None,
                target_range: Some(target_function.range.clone()),
                metrics_before: None,
                metrics_after: Some(target_function.metrics.clone()),
            }),
            (None, None) => {}
        }
    }

    functions.sort_by(compare_function_changes);
    diagnostics.sort_by(|left, right| {
        format!("{:?}", left.code)
            .cmp(&format!("{:?}", right.code))
            .then_with(|| left.qualified_name.cmp(&right.qualified_name))
            .then_with(|| left.message.cmp(&right.message))
    });

    Ok(FileFunctionChanges {
        base_path: file.base_path.clone(),
        target_path: file.target_path.clone(),
        functions,
        diagnostics,
    })
}

/// Parsing work below this many bytes is not worth handing to another thread.
///
/// The saving from parsing both revisions at once is bounded by the smaller of
/// the two parses, so the smaller blob decides. Measured parse throughput is a
/// few MiB per second, which puts this threshold at roughly ten milliseconds of
/// work -- far above the cost of starting a thread, and far below the size at
/// which a file dominates an analysis.
const PARALLEL_BLOB_THRESHOLD: usize = 64 * 1024;

/// Analyze a file's base and target blobs, concurrently when both are large.
///
/// The two blobs are independent. A diff dominated by one very large file
/// cannot be spread across files, so parsing its two revisions at once is the
/// only available parallelism. Small blobs stay on the current thread, which
/// keeps file-heavy diffs from oversubscribing the machine with a second
/// thread per file.
///
/// A base failure takes precedence over a target failure, exactly as it does
/// when the two blobs are analyzed in sequence.
fn analyze_both_blobs(file: &FileChange) -> Result<(BlobFunctions, BlobFunctions), DiffScopeError> {
    if !both_blobs_are_large(file) {
        let base = analyze_blob(file.base_path.as_deref(), &file.base_blob)?;
        let target = analyze_blob(file.target_path.as_deref(), &file.target_blob)?;
        return Ok((base, target));
    }

    let (base, target) = std::thread::scope(|scope| {
        let base = scope.spawn(|| analyze_blob(file.base_path.as_deref(), &file.base_blob));
        let target = analyze_blob(file.target_path.as_deref(), &file.target_blob);
        (base.join(), target)
    });

    let Ok(base) = base else {
        return Err(DiffScopeError::Language(
            "a blob analysis worker terminated unexpectedly".to_owned(),
        ));
    };
    Ok((base?, target?))
}

fn both_blobs_are_large(file: &FileChange) -> bool {
    blob_len(&file.base_blob) >= PARALLEL_BLOB_THRESHOLD
        && blob_len(&file.target_blob) >= PARALLEL_BLOB_THRESHOLD
}

fn blob_len(blob: &BlobContent) -> usize {
    match blob {
        BlobContent::Available(source) => source.len(),
        BlobContent::Missing | BlobContent::NotApplicable | BlobContent::Binary => 0,
    }
}

#[derive(Debug, Default)]
struct BlobFunctions {
    functions: Vec<FunctionDefinition>,
    diagnostics: Vec<LanguageDiagnostic>,
    unavailable: bool,
}

fn analyze_blob(path: Option<&str>, blob: &BlobContent) -> Result<BlobFunctions, DiffScopeError> {
    let Some(path) = path else {
        return Ok(BlobFunctions::default());
    };

    match blob {
        BlobContent::Available(source) => {
            let analysis = analyze_source(Path::new(path), source)?;
            Ok(BlobFunctions {
                functions: analysis.functions,
                diagnostics: analysis.diagnostics,
                unavailable: false,
            })
        }
        BlobContent::NotApplicable | BlobContent::Binary => Ok(BlobFunctions::default()),
        BlobContent::Missing => Ok(BlobFunctions {
            unavailable: true,
            ..BlobFunctions::default()
        }),
    }
}

fn unavailable_diagnostic(side: &str) -> FunctionMappingDiagnostic {
    FunctionMappingDiagnostic {
        code: FunctionMappingDiagnosticCode::BlobUnavailable,
        severity: DiagnosticSeverity::Error,
        message: format!("{side} blob is unavailable for function mapping"),
        range: None,
        qualified_name: None,
    }
}

fn ambiguous_diagnostic(qualified_name: String) -> FunctionMappingDiagnostic {
    FunctionMappingDiagnostic {
        code: FunctionMappingDiagnosticCode::AmbiguousFunctionMatch,
        severity: DiagnosticSeverity::Warning,
        message: "multiple functions share the same semantic identity".to_owned(),
        range: None,
        qualified_name: Some(qualified_name),
    }
}

fn language_diagnostics(diagnostics: &[LanguageDiagnostic]) -> Vec<FunctionMappingDiagnostic> {
    diagnostics
        .iter()
        .map(|diagnostic| FunctionMappingDiagnostic {
            code: match diagnostic.code {
                LanguageDiagnosticCode::UnsupportedLanguage => {
                    FunctionMappingDiagnosticCode::UnsupportedLanguage
                }
                LanguageDiagnosticCode::InvalidUtf8 => FunctionMappingDiagnosticCode::InvalidUtf8,
                LanguageDiagnosticCode::MalformedSource => {
                    FunctionMappingDiagnosticCode::MalformedSource
                }
                LanguageDiagnosticCode::ParseError => FunctionMappingDiagnosticCode::ParseError,
                LanguageDiagnosticCode::OversizedFile => {
                    FunctionMappingDiagnosticCode::OversizedFile
                }
            },
            severity: diagnostic.severity,
            message: diagnostic.message.clone(),
            range: diagnostic.range.clone(),
            qualified_name: None,
        })
        .collect()
}

fn group_by_key(
    functions: Vec<FunctionDefinition>,
) -> BTreeMap<FunctionKey, Vec<FunctionDefinition>> {
    let mut groups: BTreeMap<FunctionKey, Vec<FunctionDefinition>> = BTreeMap::new();
    for function in functions {
        groups.entry(key_for(&function)).or_default().push(function);
    }
    groups
}

fn key_for(function: &FunctionDefinition) -> FunctionKey {
    FunctionKey {
        language: function.language,
        kind: function.kind,
        qualified_name: function.qualified_name.clone(),
    }
}

/// Decide whether a matched function changed between the two revisions.
///
/// A function is modified when a diff hunk touches it on either side, or when
/// its metrics differ. Absolute source ranges are deliberately not compared:
/// an edit anywhere above a function shifts every later function's line
/// numbers, and reporting those as modified buries the functions that really
/// changed. Moved functions still intersect a hunk at both their old and new
/// positions, so they remain modified.
fn function_changed(
    base_function: &FunctionDefinition,
    target_function: &FunctionDefinition,
    hunks: &[DiffHunk],
) -> bool {
    base_function.metrics != target_function.metrics
        || hunks.iter().any(|hunk| {
            range_intersects_hunk_side(&base_function.range, hunk.base_start, hunk.base_count)
                || range_intersects_hunk_side(
                    &target_function.range,
                    hunk.target_start,
                    hunk.target_count,
                )
        })
}

fn range_intersects_hunk_side(range: &SourceRange, start: u32, count: u32) -> bool {
    if count == 0 {
        return start >= range.start_line.saturating_sub(1) && start <= range.end_line;
    }
    let end = start.saturating_add(count.saturating_sub(1));
    start <= range.end_line && end >= range.start_line
}

fn compare_function_changes(left: &FunctionChange, right: &FunctionChange) -> std::cmp::Ordering {
    let left_range = left.target_range.as_ref().or(left.base_range.as_ref());
    let right_range = right.target_range.as_ref().or(right.base_range.as_ref());
    range_start(left_range)
        .cmp(&range_start(right_range))
        .then_with(|| left.qualified_name.cmp(&right.qualified_name))
        .then_with(|| format!("{:?}", left.kind).cmp(&format!("{:?}", right.kind)))
}

fn range_start(range: Option<&SourceRange>) -> (u32, u32) {
    range.map_or((u32::MAX, u32::MAX), |range| {
        (range.start_line, range.start_column)
    })
}

#[cfg(test)]
mod tests {
    use crate::{BlobContent, FileChange, FileStatus};

    use super::{
        FunctionChangeStatus, FunctionMappingDiagnosticCode, map_changed_functions,
        range_intersects_hunk_side,
    };

    #[test]
    fn maps_signature_and_body_edits_to_modified_functions() {
        let file = file_change(
            b"function greet(name: string) {\n  return name;\n}\n",
            b"function greet(name: string): string {\n  return `hi ${name}`;\n}\n",
            1,
            3,
            1,
            3,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        assert_eq!(mapped.functions.len(), 1);
        assert_eq!(mapped.functions[0].qualified_name, "greet");
        assert_eq!(mapped.functions[0].status, FunctionChangeStatus::Modified);
        assert!(mapped.functions[0].metrics_before.is_some());
        assert!(mapped.functions[0].metrics_after.is_some());
    }

    #[test]
    fn matches_moved_functions_by_identity() {
        let file = file_change(
            b"function first() { return 1; }\nfunction second() { return 2; }\n",
            b"function second() { return 2; }\nfunction first() { return 1; }\n",
            1,
            2,
            1,
            2,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        assert_eq!(mapped.functions.len(), 2);
        assert!(
            mapped
                .functions
                .iter()
                .all(|function| function.status == FunctionChangeStatus::Modified)
        );
    }

    #[test]
    fn classifies_added_and_removed_functions() {
        let file = file_change(
            b"function removed() { return 1; }\nfunction kept() { return 2; }\n",
            b"function kept() { return 2; }\nfunction added() { return 3; }\n",
            1,
            2,
            1,
            2,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        assert!(mapped.functions.iter().any(|function| {
            function.qualified_name == "removed" && function.status == FunctionChangeStatus::Removed
        }));
        assert!(mapped.functions.iter().any(|function| {
            function.qualified_name == "added" && function.status == FunctionChangeStatus::Added
        }));
    }

    #[test]
    fn keeps_functions_unchanged_when_an_edit_above_only_shifts_them() {
        // Two lines are inserted near the top of the file. `shifted` moves
        // down by two lines, but its content, and every metric derived from
        // it, is identical.
        let file = file_change(
            b"const top = 1;\n\nfunction shifted() {\n  return 1;\n}\n",
            b"const top = 1;\nconst added = 2;\nconst alsoAdded = 3;\n\nfunction shifted() {\n  return 1;\n}\n",
            1,
            0,
            2,
            2,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");
        let function = &mapped.functions[0];

        assert_eq!(mapped.functions.len(), 1);
        assert_eq!(function.qualified_name, "shifted");
        assert_eq!(
            function.base_range.as_ref().map(|range| range.start_line),
            Some(3)
        );
        assert_eq!(
            function.target_range.as_ref().map(|range| range.start_line),
            Some(5)
        );
        assert_eq!(function.status, FunctionChangeStatus::Unchanged);
    }

    #[test]
    fn reports_functions_the_edit_actually_touches_as_modified() {
        let file = file_change(
            b"function untouched() { return 1; }\nfunction edited() { return 2; }\n",
            b"function untouched() { return 1; }\nfunction edited() { return 3; }\n",
            2,
            1,
            2,
            1,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");
        let status_of = |name: &str| {
            mapped
                .functions
                .iter()
                .find(|function| function.qualified_name == name)
                .map_or_else(
                    || panic!("missing function {name}"),
                    |function| function.status,
                )
        };

        assert_eq!(status_of("untouched"), FunctionChangeStatus::Unchanged);
        assert_eq!(status_of("edited"), FunctionChangeStatus::Modified);
    }

    #[test]
    fn analyzes_large_blobs_on_separate_threads_with_the_same_result() {
        use std::fmt::Write as _;

        // Both revisions exceed PARALLEL_BLOB_THRESHOLD, so they are parsed
        // concurrently. The mapping must match what a sequential parse of the
        // same sources produces.
        let mut base = String::new();
        let mut target = String::new();
        for index in 0..2_000 {
            writeln!(
                base,
                "export function fn{index}(value: number): number {{ return value + {index}; }}"
            )
            .expect("write base fixture");
            let body = if index == 1_000 {
                "if (value > 0) { return value; } return 0;".to_owned()
            } else {
                format!("return value + {index};")
            };
            writeln!(
                target,
                "export function fn{index}(value: number): number {{ {body} }}"
            )
            .expect("write target fixture");
        }

        assert!(base.len() > super::PARALLEL_BLOB_THRESHOLD);
        assert!(target.len() > super::PARALLEL_BLOB_THRESHOLD);

        let file = file_change(base.as_bytes(), target.as_bytes(), 1_001, 1, 1_001, 1);
        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        assert_eq!(mapped.functions.len(), 2_000);
        assert!(mapped.diagnostics.is_empty());

        let edited = mapped
            .functions
            .iter()
            .find(|function| function.qualified_name == "fn1000")
            .expect("edited function is mapped");
        assert_eq!(edited.status, FunctionChangeStatus::Modified);
        assert_eq!(
            edited
                .metrics_before
                .as_ref()
                .map(|metrics| metrics.cyclomatic_complexity),
            Some(1)
        );
        assert_eq!(
            edited
                .metrics_after
                .as_ref()
                .map(|metrics| metrics.cyclomatic_complexity),
            Some(2)
        );

        let untouched = mapped
            .functions
            .iter()
            .find(|function| function.qualified_name == "fn0")
            .expect("untouched function is mapped");
        assert_eq!(untouched.status, FunctionChangeStatus::Unchanged);
    }

    #[test]
    fn inserting_a_callback_does_not_cross_match_the_callbacks_below_it() {
        // Anonymous identities were once numbered across the whole file, so an
        // inserted callback renumbered every later one and matched unrelated
        // bodies to each other: the callback of `a` was compared against the
        // body of `new`, reporting complexity churn that no edit caused. Each
        // surviving callback must still match its own previous self.
        let file = file_change(
            b"describe('s', () => {\n  test('a', () => { return 1; })\n  test('b', () => { if (x) { return 2; } return 3; })\n})\n",
            b"describe('s', () => {\n  test('new', () => { while (y) { break; } })\n  test('a', () => { return 1; })\n  test('b', () => { if (x) { return 2; } return 3; })\n})\n",
            2,
            0,
            2,
            1,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        let added = mapped
            .functions
            .iter()
            .filter(|function| function.status == FunctionChangeStatus::Added)
            .collect::<Vec<_>>();
        assert_eq!(added.len(), 1);
        assert_eq!(
            added[0].qualified_name,
            "describe(\"s\").test(\"new\").<anonymous>#1"
        );

        // The two surviving callbacks kept their own metrics. The enclosing
        // `describe` callback legitimately grew by the inserted line, so only
        // the callbacks the edit did not touch are asserted here.
        for name in [
            "describe(\"s\").test(\"a\").<anonymous>#1",
            "describe(\"s\").test(\"b\").<anonymous>#1",
        ] {
            let function = mapped
                .functions
                .iter()
                .find(|function| function.qualified_name == name)
                .unwrap_or_else(|| panic!("{name} is still matched"));
            assert_eq!(
                function.metrics_before, function.metrics_after,
                "{name} was matched against a different function"
            );
        }
    }

    #[test]
    fn matches_functions_across_file_renames() {
        let mut file = file_change(
            b"function stable() { return 1; }\n",
            b"function stable() { return 1; }\n",
            1,
            0,
            1,
            0,
        );
        file.status = FileStatus::Renamed;
        file.base_path = Some("old.ts".to_owned());
        file.target_path = Some("new.ts".to_owned());
        file.hunks.clear();

        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        assert_eq!(mapped.functions.len(), 1);
        assert_eq!(mapped.functions[0].qualified_name, "stable");
        assert_eq!(mapped.functions[0].status, FunctionChangeStatus::Unchanged);
    }

    #[test]
    fn maps_nested_functions_independently() {
        let file = file_change(
            b"function outer() {\n  function inner() { return 1; }\n  return inner();\n}\n",
            b"function outer() {\n  function inner() { return 2; }\n  return inner();\n}\n",
            2,
            1,
            2,
            1,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        assert!(mapped.functions.iter().any(|function| {
            function.qualified_name == "inner" && function.status == FunctionChangeStatus::Modified
        }));
    }

    #[test]
    fn reports_ambiguous_duplicate_identities() {
        let file = file_change(
            b"function duplicate() { return 1; }\nfunction duplicate() { return 2; }\n",
            b"function duplicate() { return 3; }\n",
            1,
            2,
            1,
            1,
        );

        let mapped = map_changed_functions(&file).expect("mapping succeeds");

        assert!(mapped.functions.is_empty());
        assert!(mapped.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == FunctionMappingDiagnosticCode::AmbiguousFunctionMatch
                && diagnostic.qualified_name.as_deref() == Some("duplicate")
        }));
    }

    #[test]
    fn insertion_at_function_boundary_intersects_function() {
        assert!(range_intersects_hunk_side(
            &crate::languages::SourceRange {
                start_line: 10,
                start_column: 0,
                end_line: 12,
                end_column: 1,
            },
            9,
            0,
        ));
    }

    fn file_change(
        base: &[u8],
        target: &[u8],
        base_start: u32,
        base_count: u32,
        target_start: u32,
        target_count: u32,
    ) -> FileChange {
        FileChange {
            base_path: Some("sample.ts".to_owned()),
            target_path: Some("sample.ts".to_owned()),
            status: FileStatus::Modified,
            old_blob_id: Some("base".to_owned()),
            new_blob_id: Some("target".to_owned()),
            base_blob: BlobContent::Available(base.to_vec()),
            target_blob: BlobContent::Available(target.to_vec()),
            added_lines: target_count,
            removed_lines: base_count,
            hunks: vec![crate::DiffHunk {
                base_start,
                base_count,
                target_start,
                target_count,
                added_lines: target_count,
                removed_lines: base_count,
            }],
        }
    }
}
