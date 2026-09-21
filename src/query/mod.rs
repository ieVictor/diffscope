//! Answer narrow questions about an analysis instead of returning all of it.
//!
//! A whole-repository analysis is far larger than any single question needs: a
//! real 49-file diff produces well over a thousand function records, most of
//! them untouched by the change. Returning that to a coding agent spends its
//! context on data it did not ask for and buries the few functions that matter.
//!
//! The queries here project one analysis into the shape a caller asked for:
//! an overview, a filtered and ranked page of candidates, or one function in
//! full. Unchanged functions are never returned unless they are asked for.
//!
//! These types carry `Serialize` because they are a transport projection with a
//! single representation, not a domain model. The analysis model itself stays
//! free of serialization concerns, and renderers for it live in [`crate::output`].
//!
//! Every answer is a projection of one immutable analysis, and the transport
//! pairs it with that analysis's identity and with the query that was actually
//! applied. A list answers with a window over a ranked list plus the offset of
//! the next row; turning that offset into something a caller can hand back is
//! the transport's job.

pub mod classify;
pub mod impact;
pub mod risk;

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{
    DiffHunk,
    analysis::FunctionChangeStatus,
    imports::ImportIndex,
    languages::{DiagnosticSeverity, SourceRange, symbol_id},
    metrics::FunctionMetrics,
    result::{AnalysisResult, Diagnostic, FileResult, FunctionResult},
};

use classify::FileClassification;
use risk::{
    ExportStatus, REVIEW_PRIORITY_MAXIMUM_SCORE, REVIEW_PRIORITY_MODEL, RISK_MAXIMUM_SCORE,
    RISK_MODEL, RiskAssessment, RiskLevel, RiskSignals, ScoreAssessment,
};

/// Largest page any query will return, whatever limit is requested.
pub const MAX_LIMIT: usize = 200;
/// Page size used when a caller does not ask for one.
pub const DEFAULT_LIMIT: usize = 50;

/// The page size a request is applied with, after defaults and bounds.
///
/// Canonical, so a caller can be told exactly what was applied and a cursor can
/// be bound to it: two requests that differ only in an out-of-range limit mean
/// the same page.
#[must_use]
pub fn canonical_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Review candidates included in the overview.
const SUMMARY_CANDIDATES: usize = 5;

/// Cognitive complexity an added function must carry to count as a helper that
/// took real work out of another function rather than delegating one line.
const SUBSTANTIAL_HELPER_COGNITIVE: u32 = 5;

/// Shape of a change that moved complexity out of a function into new ones.
pub const COMPLEXITY_EXTRACTION: &str = "complexity_extraction";
/// Shape of every change that is not an extraction.
pub const OTHER_CHANGE_SHAPE: &str = "other";

/// Directory names that hold one project per child directory.
const PACKAGE_ROOTS: &[&str] = &[
    "apps",
    "crates",
    "libs",
    "modules",
    "packages",
    "packages-private",
    "services",
];

// ---------------------------------------------------------------- queries ---

/// Which part of a ranked list to return.
///
/// The offset is not a caller-facing parameter: the transport sets it only from
/// a cursor it issued itself, validated against the same query and the same
/// analysis, so a page is always a continuation of one answer rather than an
/// arbitrary slice of another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub limit: Option<usize>,
    pub offset: usize,
}

impl Page {
    fn window(&self) -> (usize, usize) {
        (self.offset, canonical_limit(self.limit))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileFilter {
    pub classification: Option<FileClassification>,
    pub minimum_risk: Option<RiskLevel>,
    pub page: Page,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionFilter {
    pub file: Option<String>,
    pub status: Option<FunctionChangeStatus>,
    pub classification: Option<FileClassification>,
    pub minimum_risk: Option<RiskLevel>,
    pub min_complexity_delta: Option<i64>,
    pub include_unchanged: bool,
    pub page: Page,
}

// --------------------------------------------------------------- responses ---

#[derive(Debug, Clone, Serialize)]
pub struct ChangeSummary<'a> {
    pub base: RevisionView<'a>,
    pub target: RevisionView<'a>,
    pub files: FileCounts,
    pub lines: LineCounts,
    pub functions: FunctionCounts,
    pub diagnostics: DiagnosticCounts,
    pub change_areas: Vec<ChangeArea>,
    /// The highest-ranked changed functions, as a starting point for review.
    ///
    /// Deliberately compact: the overview exists to point somewhere, and the
    /// full record for any of these is one detail query away.
    pub review_candidates: Vec<ReviewCandidate<'a>>,
}

/// One entry in the overview's ranked shortlist.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewCandidate<'a> {
    /// Identity of the full record, for a detail query.
    pub function_id: String,
    pub file: &'a str,
    pub symbol: String,
    pub status: &'static str,
    pub complexity_delta: ComplexityDelta,
    /// Level of the intrinsic-risk model.
    pub risk: &'static str,
    /// Level of the review-priority model, which is what the shortlist is
    /// ranked by.
    pub review_priority: &'static str,
    pub match_confidence: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ComplexityDelta {
    pub cyclomatic: i64,
    pub cognitive: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevisionView<'a> {
    pub id: &'a str,
    pub display_name: &'a str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FileCounts {
    pub changed: u32,
    pub supported: u32,
    pub unsupported: u32,
    /// Changed file count per role, omitting roles with no changed files.
    pub by_classification: BTreeMap<&'static str, u32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LineCounts {
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct FunctionCounts {
    pub added: u32,
    pub removed: u32,
    pub modified: u32,
    pub unchanged: u32,
}

/// Diagnostics reported, by severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiagnosticCounts {
    pub info: u32,
    pub warnings: u32,
    pub errors: u32,
    /// Every diagnostic, so a caller can size the whole list before reading it.
    pub total: u32,
}

impl DiagnosticCounts {
    /// Count one more diagnostic of the given severity.
    fn add(&mut self, severity: DiagnosticSeverity) {
        self.add_many(severity, 1);
    }

    /// Add a tally of diagnostics of one severity.
    fn add_many(&mut self, severity: DiagnosticSeverity, count: u32) {
        match severity {
            DiagnosticSeverity::Info => self.info += count,
            DiagnosticSeverity::Warning => self.warnings += count,
            DiagnosticSeverity::Error => self.errors += count,
        }
        self.total = self.info + self.warnings + self.errors;
    }

    /// Counts of an analysis's own summary.
    fn from_summary(counts: &crate::result::DiagnosticCounts) -> Self {
        let mut view = Self::default();
        view.add_many(DiagnosticSeverity::Info, counts.info);
        view.add_many(DiagnosticSeverity::Warning, counts.warning);
        view.add_many(DiagnosticSeverity::Error, counts.error);
        view
    }
}

/// A group of changed files that belong to the same project area.
///
/// Areas are derived from paths, so their names are paths. `DiffScope` does not
/// invent a label such as "runtime rendering" for a directory, because nothing
/// in a diff says what a directory is for.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeArea {
    pub name: String,
    pub files: u32,
    pub lines: LineCounts,
    /// Highest intrinsic risk level among the area's changed functions.
    pub risk: &'static str,
    /// Highest review priority level among the area's changed functions.
    pub review_priority: &'static str,
    pub complexity: AggregateComplexity,
}

/// Complexity summed over every function of a file or area.
///
/// Summing before and after makes a refactor legible as one unit: extracting a
/// helper moves complexity out of one function and into a new one, and only the
/// totals show whether the change reduced complexity or merely relocated it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AggregateComplexity {
    pub cyclomatic_before: u32,
    pub cyclomatic_after: u32,
    pub cyclomatic_delta: i64,
    pub cognitive_before: u32,
    pub cognitive_after: u32,
    pub cognitive_delta: i64,
    pub source_loc_before: u32,
    pub source_loc_after: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile<'a> {
    pub path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<&'a str>,
    pub status: &'static str,
    pub classification: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<&'static str>,
    pub area: String,
    pub lines: LineCounts,
    pub functions: FunctionCounts,
    pub complexity: AggregateComplexity,
    /// Intrinsic risk of the riskiest function this change touched here.
    ///
    /// A function's own code, and nothing about where it sits: the module's
    /// reach and the file's classification are exposure, and belong to
    /// [`Self::review_priority`].
    pub risk: ScoreAssessment,
    /// The same functions ranked by how much review attention they are owed.
    ///
    /// Always at least [`Self::risk`] in level, because it starts from that
    /// score and adds what the function's position in the module exposes.
    pub review_priority: ScoreAssessment,
    /// Whether this change moved complexity out of a function into new ones.
    pub change_shape: &'static str,
    /// Names this change adds to and removes from the module's public surface.
    #[serde(skip_serializing_if = "ExportChange::is_empty")]
    pub exports: ExportChange<'a>,
    /// What else in the revision reaches this file. Absent when the import
    /// graph was not built, which is not the same as nothing reaching it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<ImpactView>,
    pub diagnostics: DiagnosticCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactView {
    pub direct_importers: u32,
    pub nearby_importers: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_tests: Vec<RelatedTestView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedTestView {
    pub file: String,
    pub reason: &'static str,
    pub confidence: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExportChange<'a> {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<&'a str>,
}

impl ExportChange<'_> {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFunction<'a> {
    /// Identity to pass back to a detail query.
    ///
    /// Unique within the analysis: the path and symbol are separated by the
    /// revision side the definition sits on and its position there, so two
    /// functions that share a name are still addressed apart.
    pub function_id: String,
    pub file: &'a str,
    /// Human-readable symbol, such as `fn:patch`.
    pub symbol: String,
    pub qualified_name: &'a str,
    pub kind: &'static str,
    pub status: &'static str,
    pub classification: &'static str,
    pub metrics: MetricDeltas,
    pub change: ChurnView,
    /// What the change did to this function's own code.
    pub risk: ScoreAssessment,
    /// Intrinsic risk plus how exposed this function is: its name in the
    /// module's public surface, the reach of the module containing it, and that
    /// module's classification.
    pub review_priority: ScoreAssessment,
    pub match_confidence: f64,
    pub range: RangeView,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MetricDeltas {
    pub physical_loc: MetricDelta,
    pub source_loc: MetricDelta,
    pub cyclomatic_complexity: MetricDelta,
    pub cognitive_complexity: MetricDelta,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MetricDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u32>,
    pub delta: i64,
}

impl MetricDelta {
    fn new(before: Option<u32>, after: Option<u32>) -> Self {
        Self {
            before,
            after,
            delta: i64::from(after.unwrap_or(0)) - i64::from(before.unwrap_or(0)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ChurnView {
    pub changed_hunks: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
    /// Changed lines inside the function over its length, `0.0` to `1.0`.
    pub hunk_overlap: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RangeView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<LineRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<LineRange>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct LineRange {
    pub start_line: u32,
    pub end_line: u32,
}

/// How much of a ranked list one answer carried.
///
/// The transport turns `next_offset` into the opaque cursor a caller sends
/// back; nothing outside the query layer sees a bare offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageWindow {
    pub returned: usize,
    pub total: usize,
    pub has_more: bool,
    /// Offset of the next row, when one exists.
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct FileList<'a> {
    pub files: Vec<ChangedFile<'a>>,
    pub page: PageWindow,
}

#[derive(Debug, Clone)]
pub struct FunctionList<'a> {
    pub functions: Vec<ChangedFunction<'a>>,
    pub page: PageWindow,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDetail<'a> {
    pub function: ChangedFunction<'a>,
    /// Hunks of the containing file that touch this function.
    pub hunks: Vec<HunkView>,
    /// Tests likely to exercise this file, each saying how it was connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<ImpactView>,
    pub diagnostics: Vec<DiagnosticView<'a>>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HunkView {
    pub base_start: u32,
    pub base_count: u32,
    pub target_start: u32,
    pub target_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticView<'a> {
    pub code: &'static str,
    pub severity: &'static str,
    pub message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_entity_ids: Vec<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticList<'a> {
    pub diagnostics: Vec<DiagnosticView<'a>>,
    pub counts: DiagnosticCounts,
}

/// A detail query named a function the analysis does not contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFunction {
    pub function_id: String,
    /// Identities in this analysis that come closest to what was asked for.
    pub known_function_ids: Vec<String>,
}

/// Identity of one function record, unique within an analysis.
///
/// A path and a symbol are not enough on their own: the same file can declare
/// one name on both sides of a change, and a removal and an addition can share
/// a name. The revision side the definition was read from and its position
/// there separate them. The target side is preferred because that is what a
/// caller reads and reviews; a removal exists only on the base side.
#[must_use]
pub fn function_id(file: &FileResult, function: &FunctionResult) -> String {
    let (side, range) = function_side(function);
    let (line, column) = range.map_or((0, 0), |range| (range.start_line, range.start_column));
    format!(
        "{}#{}@{side}:{line}:{column}",
        display_path(file),
        symbol_id(function.kind, &function.qualified_name)
    )
}

/// The revision side a function is identified on, and the range it sits at.
fn function_side(function: &FunctionResult) -> (&'static str, Option<&SourceRange>) {
    let base = function.base_range.as_ref();
    let target = function.target_range.as_ref();
    match (function.status, base, target) {
        // A removal exists only on the base side, and a function with no
        // target range is named where it still exists.
        (FunctionChangeStatus::Removed, Some(base), _) | (_, Some(base), None) => {
            ("base", Some(base))
        }
        (_, _, Some(target)) => ("target", Some(target)),
        (_, None, None) => ("none", None),
    }
}

// ---------------------------------------------------------------- answers ---

/// Summarize the whole change, with a short ranked list to start from.
#[must_use]
pub fn change_summary<'a>(
    result: &'a AnalysisResult,
    index: Option<&ImportIndex>,
) -> ChangeSummary<'a> {
    let mut files = FileCounts {
        changed: result.summary.changed_files,
        supported: result.summary.supported_files,
        unsupported: result.summary.unsupported_files,
        ..FileCounts::default()
    };
    let mut functions = FunctionCounts::default();
    let mut areas: BTreeMap<String, ChangeArea> = BTreeMap::new();

    for file in &result.files {
        let path = display_path(file);
        let classification = classify::classify(path);
        *files
            .by_classification
            .entry(classification.as_str())
            .or_default() += 1;
        for function in &file.functions {
            count_status(&mut functions, function.status);
        }

        let area_name = change_area(path);
        let complexity = aggregate_complexity(file);
        let assessment = file_assessment(file, classification, direct_importers(index, path));
        let area = areas.entry(area_name.clone()).or_insert(ChangeArea {
            name: area_name,
            files: 0,
            lines: LineCounts::default(),
            risk: RiskLevel::Low.as_str(),
            review_priority: RiskLevel::Low.as_str(),
            complexity: AggregateComplexity::default(),
        });
        area.files += 1;
        area.lines.added += file.added_lines;
        area.lines.removed += file.removed_lines;
        area.complexity.merge(&complexity);
        if RiskLevel::parse(area.risk) < Some(assessment.risk.level) {
            area.risk = assessment.risk.level.as_str();
        }
        if RiskLevel::parse(area.review_priority) < Some(assessment.review_priority.level) {
            area.review_priority = assessment.review_priority.level.as_str();
        }
    }

    let mut change_areas = areas.into_values().collect::<Vec<_>>();
    change_areas.sort_by(|left, right| {
        RiskLevel::parse(right.review_priority)
            .cmp(&RiskLevel::parse(left.review_priority))
            .then_with(|| RiskLevel::parse(right.risk).cmp(&RiskLevel::parse(left.risk)))
            .then_with(|| right.files.cmp(&left.files))
            .then_with(|| left.name.cmp(&right.name))
    });

    let review_candidates = ranked_functions(result, &FunctionFilter::default(), index)
        .into_iter()
        .take(SUMMARY_CANDIDATES)
        .map(|function| ReviewCandidate {
            function_id: function.function_id,
            file: function.file,
            symbol: function.symbol,
            status: function.status,
            complexity_delta: ComplexityDelta {
                cyclomatic: function.metrics.cyclomatic_complexity.delta,
                cognitive: function.metrics.cognitive_complexity.delta,
            },
            risk: function.risk.level.as_str(),
            review_priority: function.review_priority.level.as_str(),
            match_confidence: function.match_confidence,
        })
        .collect();

    ChangeSummary {
        base: RevisionView {
            id: &result.base.id,
            display_name: &result.base.display_name,
        },
        target: RevisionView {
            id: &result.target.id,
            display_name: &result.target.display_name,
        },
        files,
        lines: LineCounts {
            added: result.summary.added_lines,
            removed: result.summary.removed_lines,
        },
        functions,
        diagnostics: DiagnosticCounts::from_summary(&result.summary.diagnostics),
        change_areas,
        review_candidates,
    }
}

/// List changed files, most in need of review first.
#[must_use]
pub fn list_changed_files<'a>(
    result: &'a AnalysisResult,
    filter: &FileFilter,
    index: Option<&ImportIndex>,
) -> FileList<'a> {
    let mut rows = result
        .files
        .iter()
        .filter_map(|file| {
            let path = display_path(file);
            let classification = classify::classify(path);
            if filter
                .classification
                .is_some_and(|wanted| wanted != classification)
            {
                return None;
            }
            let assessment = file_assessment(file, classification, direct_importers(index, path));
            if filter
                .minimum_risk
                .is_some_and(|minimum| assessment.risk.level < minimum)
            {
                return None;
            }
            let mut functions = FunctionCounts::default();
            for function in &file.functions {
                count_status(&mut functions, function.status);
            }
            Some(ChangedFile {
                path,
                renamed_from: file
                    .base_path
                    .as_deref()
                    .filter(|base| Some(*base) != file.target_path.as_deref()),
                status: crate::output::file_status(file.status),
                classification: classification.as_str(),
                language: file.language.map(crate::output::language),
                area: change_area(path),
                lines: LineCounts {
                    added: file.added_lines,
                    removed: file.removed_lines,
                },
                functions,
                complexity: aggregate_complexity(file),
                risk: assessment.risk,
                review_priority: assessment.review_priority,
                change_shape: change_shape(file),
                exports: ExportChange {
                    added: file.exports_added.iter().map(String::as_str).collect(),
                    removed: file.exports_removed.iter().map(String::as_str).collect(),
                },
                impact: index.map(|index| impact_view(index, path)),
                diagnostics: diagnostic_counts(&file.diagnostics),
            })
        })
        .collect::<Vec<_>>();

    rows.sort_by(|left, right| {
        right
            .review_priority
            .level
            .cmp(&left.review_priority.level)
            .then_with(|| right.risk.level.cmp(&left.risk.level))
            .then_with(|| {
                (right.lines.added + right.lines.removed)
                    .cmp(&(left.lines.added + left.lines.removed))
            })
            .then_with(|| left.path.cmp(right.path))
    });

    let (files, page) = paginate(rows, &filter.page);
    FileList { files, page }
}

/// List changed functions, most in need of review first.
#[must_use]
pub fn list_changed_functions<'a>(
    result: &'a AnalysisResult,
    filter: &FunctionFilter,
    index: Option<&ImportIndex>,
) -> FunctionList<'a> {
    let (functions, page) = paginate(ranked_functions(result, filter, index), &filter.page);
    FunctionList { functions, page }
}

/// Describe one function in full.
///
/// # Errors
///
/// Returns the identities this analysis does contain when the requested one is
/// not among them, so a caller that guessed can correct itself in one step.
pub fn get_function_change<'a>(
    result: &'a AnalysisResult,
    id: &str,
    index: Option<&ImportIndex>,
) -> Result<FunctionDetail<'a>, UnknownFunction> {
    let found = result.files.iter().find_map(|file| {
        file.functions
            .iter()
            .find(|function| function_id(file, function) == id)
            .map(|function| (file, function))
    });

    let Some((file, function)) = found else {
        return Err(UnknownFunction {
            function_id: id.to_owned(),
            known_function_ids: closest_function_ids(result, id),
        });
    };

    let path = display_path(file);
    let classification = classify::classify(path);
    let importers = direct_importers(index, path);
    Ok(FunctionDetail {
        hunks: touching_hunks(file, function),
        impact: index.map(|index| impact_view(index, path)),
        diagnostics: file
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .related_entity_ids
                    .iter()
                    .any(|id| id == &function.qualified_name)
            })
            .chain(function.diagnostics.iter())
            .map(DiagnosticView::from)
            .collect(),
        function: changed_function(path, classification, file, function, importers),
    })
}

/// Identities to offer when a requested one does not resolve.
///
/// What a caller most likely meant comes first: the identities that name the
/// same symbol or path. A caller that guessed blindly gets the first few
/// identities of the analysis instead, so it can see the shape of a real one.
fn closest_function_ids(result: &AnalysisResult, requested: &str) -> Vec<String> {
    /// Identities named before the list is summarized.
    const SUGGESTIONS: usize = 10;

    let mut matching = Vec::new();
    let mut leading = Vec::new();
    for file in &result.files {
        for function in &file.functions {
            let id = function_id(file, function);
            if matching.len() < SUGGESTIONS && id.contains(requested) {
                matching.push(id.clone());
            }
            if leading.len() < SUGGESTIONS {
                leading.push(id);
            }
        }
    }
    if matching.is_empty() {
        leading
    } else {
        matching
    }
}

/// Report diagnostics, for the whole analysis or for one file.
#[must_use]
pub fn get_analysis_diagnostics<'a>(
    result: &'a AnalysisResult,
    file_path: Option<&str>,
) -> DiagnosticList<'a> {
    let diagnostics = result
        .files
        .iter()
        .filter(|file| file_path.is_none_or(|wanted| display_path(file) == wanted))
        .flat_map(|file| file.diagnostics.iter())
        .chain(result.diagnostics.iter().filter(|_| file_path.is_none()))
        .collect::<Vec<_>>();

    DiagnosticList {
        counts: diagnostic_counts_iter(diagnostics.iter().copied()),
        diagnostics: diagnostics.into_iter().map(DiagnosticView::from).collect(),
    }
}

// ---------------------------------------------------------------- helpers ---

/// The path a file is addressed by: its target path, or its base path when the
/// file was deleted. Renames report their old path separately.
fn display_path(file: &FileResult) -> &str {
    file.target_path
        .as_deref()
        .or(file.base_path.as_deref())
        .unwrap_or("<unknown>")
}

/// Group a path by the project it belongs to.
///
/// Inside a directory that holds one project per child, such as `packages`, the
/// area is that child. Otherwise it is the top-level directory. The name is the
/// path itself: nothing in a diff says what a directory is for.
fn change_area(path: &str) -> String {
    let mut segments = path.split('/');
    let Some(first) = segments.next() else {
        return "<root>".to_owned();
    };
    match segments.next() {
        None => "<root>".to_owned(),
        Some(second) if PACKAGE_ROOTS.contains(&first) => format!("{first}/{second}"),
        Some(_) => first.to_owned(),
    }
}

fn count_status(counts: &mut FunctionCounts, status: FunctionChangeStatus) {
    match status {
        FunctionChangeStatus::Added => counts.added += 1,
        FunctionChangeStatus::Removed => counts.removed += 1,
        FunctionChangeStatus::Modified => counts.modified += 1,
        FunctionChangeStatus::Unchanged => counts.unchanged += 1,
    }
}

impl AggregateComplexity {
    fn merge(&mut self, other: &Self) {
        self.cyclomatic_before += other.cyclomatic_before;
        self.cyclomatic_after += other.cyclomatic_after;
        self.cognitive_before += other.cognitive_before;
        self.cognitive_after += other.cognitive_after;
        self.source_loc_before += other.source_loc_before;
        self.source_loc_after += other.source_loc_after;
        self.recompute_deltas();
    }

    fn recompute_deltas(&mut self) {
        self.cyclomatic_delta =
            i64::from(self.cyclomatic_after) - i64::from(self.cyclomatic_before);
        self.cognitive_delta = i64::from(self.cognitive_after) - i64::from(self.cognitive_before);
    }
}

/// Sum a file's complexity across every function it contains.
///
/// Every function is counted, including the ones the diff did not touch,
/// because the totals only answer "did this change move complexity or remove
/// it?" when both revisions are measured whole.
fn aggregate_complexity(file: &FileResult) -> AggregateComplexity {
    let mut total = AggregateComplexity::default();
    for function in &file.functions {
        if let Some(before) = &function.metrics_before {
            total.cyclomatic_before += before.cyclomatic_complexity;
            total.cognitive_before += before.cognitive_complexity;
            total.source_loc_before += before.source_loc;
        }
        if let Some(after) = &function.metrics_after {
            total.cyclomatic_after += after.cyclomatic_complexity;
            total.cognitive_after += after.cognitive_complexity;
            total.source_loc_after += after.source_loc;
        }
    }
    total.recompute_deltas();
    total
}

/// Project one file's reach into the response shape.
fn impact_view(index: &ImportIndex, path: &str) -> ImpactView {
    let reach = impact::for_file(index, path);
    ImpactView {
        direct_importers: reach.direct_importers,
        nearby_importers: reach.nearby_importers,
        related_tests: reach
            .related_tests
            .into_iter()
            .map(|test| RelatedTestView {
                file: test.file,
                reason: test.link.reason(),
                confidence: test.link.confidence(),
            })
            .collect(),
    }
}

/// How many modules import a path directly, when an index is available.
fn direct_importers(index: Option<&ImportIndex>, path: &str) -> Option<u32> {
    index.map(|index| u32::try_from(index.importers(path).len()).unwrap_or(u32::MAX))
}

/// A file is as deep in review as the riskiest function changed inside it.
///
/// The two models are kept apart rather than collapsed into one number: a
/// module can hold a function that is complex but little depended on, and one
/// that is simple but breaks every importer of it, and a reviewer acts on the
/// two differently. A file whose change touched no function carries a zero
/// assessment, which is the same thing as a change with nothing to score.
fn file_assessment(
    file: &FileResult,
    classification: FileClassification,
    importers: Option<u32>,
) -> RiskAssessment {
    file.functions
        .iter()
        .filter(|function| function.status != FunctionChangeStatus::Unchanged)
        .map(|function| {
            risk::assess(&signals_for(
                function,
                classification,
                export_status(file, function),
                importers,
            ))
        })
        .reduce(|highest, candidate| RiskAssessment {
            risk: greater(highest.risk, candidate.risk),
            review_priority: greater(highest.review_priority, candidate.review_priority),
        })
        .unwrap_or_else(|| RiskAssessment {
            risk: zero_assessment(RISK_MODEL, RISK_MAXIMUM_SCORE),
            review_priority: zero_assessment(REVIEW_PRIORITY_MODEL, REVIEW_PRIORITY_MAXIMUM_SCORE),
        })
}

/// The higher-scoring of two assessments, preferring the first on a tie so the
/// file's reasons stay in function order.
fn greater(highest: ScoreAssessment, candidate: ScoreAssessment) -> ScoreAssessment {
    if candidate.score > highest.score {
        candidate
    } else {
        highest
    }
}

/// A model's verdict on a function that scored nothing.
fn zero_assessment(model: &'static str, maximum_score: u32) -> ScoreAssessment {
    ScoreAssessment {
        model,
        maximum_score,
        score: 0,
        level: RiskLevel::Low,
        reasons: Vec::new(),
    }
}

fn signals_for(
    function: &FunctionResult,
    classification: FileClassification,
    exported: ExportStatus,
    direct_importers: Option<u32>,
) -> RiskSignals {
    let before = function.metrics_before.as_ref();
    let after = function.metrics_after.as_ref();
    RiskSignals {
        classification,
        status: function.status,
        cyclomatic_delta: delta(before, after, |metrics| metrics.cyclomatic_complexity),
        cognitive_delta: delta(before, after, |metrics| metrics.cognitive_complexity),
        cognitive_after: after.map_or(0, |metrics| metrics.cognitive_complexity),
        cyclomatic_after: after.map_or(0, |metrics| metrics.cyclomatic_complexity),
        churned_lines: function.churn.lines_added + function.churn.lines_removed,
        match_confidence: function.match_confidence,
        exported,
        direct_importers,
    }
}

/// Whether a change moved complexity out of a function into new ones.
///
/// The pattern this names is an extraction: a function shrinks, a helper takes
/// the work it gave up, and the file ends up no more complex than it started.
/// A helper that adds more complexity than the function gave up is growth
/// rather than extraction, and the file's own total is what tells them apart:
/// moving complexity into a helper does not raise it, adding new complexity
/// does.
fn change_shape(file: &FileResult) -> &'static str {
    let no_more_complex = aggregate_complexity(file).cognitive_delta <= 0;
    let helper = file.functions.iter().any(|function| {
        function.status == FunctionChangeStatus::Added
            && cognitive_after(function) >= SUBSTANTIAL_HELPER_COGNITIVE
    });
    let shrunk = file.functions.iter().any(|function| {
        matches!(
            function.status,
            FunctionChangeStatus::Modified | FunctionChangeStatus::Removed
        ) && cognitive_delta(function) < 0
    });
    if no_more_complex && helper && shrunk {
        COMPLEXITY_EXTRACTION
    } else {
        OTHER_CHANGE_SHAPE
    }
}

/// Cognitive complexity a function ends up with.
fn cognitive_after(function: &FunctionResult) -> u32 {
    function
        .metrics_after
        .as_ref()
        .map_or(0, |metrics| metrics.cognitive_complexity)
}

/// What the change did to a function's cognitive complexity.
fn cognitive_delta(function: &FunctionResult) -> i64 {
    delta(
        function.metrics_before.as_ref(),
        function.metrics_after.as_ref(),
        |metrics| metrics.cognitive_complexity,
    )
}

/// Whether a function's own name moved in or out of the module's exports.
///
/// Derived from the file's export delta rather than from the function record,
/// because a name can leave the public surface while the function it named
/// stays exactly where it was.
fn export_status(file: &FileResult, function: &FunctionResult) -> ExportStatus {
    let name = leaf_name(&function.qualified_name);
    if file.exports_removed.iter().any(|export| export == name) {
        ExportStatus::Removed
    } else if file.exports_added.iter().any(|export| export == name) {
        ExportStatus::Added
    } else {
        ExportStatus::Unchanged
    }
}

/// The last segment of a qualified name, which is the name an importer writes.
fn leaf_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit_once('.')
        .map_or(qualified_name, |(_, leaf)| leaf)
}

fn delta(
    before: Option<&FunctionMetrics>,
    after: Option<&FunctionMetrics>,
    field: impl Fn(&FunctionMetrics) -> u32,
) -> i64 {
    i64::from(after.map_or(0, &field)) - i64::from(before.map_or(0, &field))
}

/// Select, score, and rank the functions a filter asks for.
fn ranked_functions<'a>(
    result: &'a AnalysisResult,
    filter: &FunctionFilter,
    index: Option<&ImportIndex>,
) -> Vec<ChangedFunction<'a>> {
    let mut rows = Vec::new();
    for file in &result.files {
        let path = display_path(file);
        if filter.file.as_deref().is_some_and(|wanted| wanted != path) {
            continue;
        }
        let classification = classify::classify(path);
        if filter
            .classification
            .is_some_and(|wanted| wanted != classification)
        {
            continue;
        }
        let importers = direct_importers(index, path);

        for function in &file.functions {
            if !filter.include_unchanged && function.status == FunctionChangeStatus::Unchanged {
                continue;
            }
            if filter
                .status
                .is_some_and(|wanted| wanted != function.status)
            {
                continue;
            }
            let row = changed_function(path, classification, file, function, importers);
            if filter
                .minimum_risk
                .is_some_and(|minimum| row.risk.level < minimum)
            {
                continue;
            }
            if let Some(minimum) = filter.min_complexity_delta {
                let largest = row
                    .metrics
                    .cognitive_complexity
                    .delta
                    .max(row.metrics.cyclomatic_complexity.delta);
                if largest < minimum {
                    continue;
                }
            }
            rows.push(row);
        }
    }

    rows.sort_by(|left, right| {
        right
            .review_priority
            .score
            .cmp(&left.review_priority.score)
            .then_with(|| right.risk.score.cmp(&left.risk.score))
            .then_with(|| {
                right
                    .metrics
                    .cognitive_complexity
                    .delta
                    .cmp(&left.metrics.cognitive_complexity.delta)
            })
            .then_with(|| {
                (right.change.lines_added + right.change.lines_removed)
                    .cmp(&(left.change.lines_added + left.change.lines_removed))
            })
            .then_with(|| left.file.cmp(right.file))
            .then_with(|| left.symbol.cmp(&right.symbol))
    });
    rows
}

fn changed_function<'a>(
    path: &'a str,
    classification: FileClassification,
    file: &FileResult,
    function: &'a FunctionResult,
    importers: Option<u32>,
) -> ChangedFunction<'a> {
    let before = function.metrics_before.as_ref();
    let after = function.metrics_after.as_ref();
    let assessment = risk::assess(&signals_for(
        function,
        classification,
        export_status(file, function),
        importers,
    ));
    ChangedFunction {
        function_id: function_id(file, function),
        file: path,
        symbol: symbol_id(function.kind, &function.qualified_name),
        qualified_name: &function.qualified_name,
        kind: crate::output::function_kind(function.kind),
        status: crate::output::function_status(function.status),
        classification: classification.as_str(),
        metrics: MetricDeltas {
            physical_loc: MetricDelta::new(
                before.map(|metrics| metrics.physical_loc),
                after.map(|metrics| metrics.physical_loc),
            ),
            source_loc: MetricDelta::new(
                before.map(|metrics| metrics.source_loc),
                after.map(|metrics| metrics.source_loc),
            ),
            cyclomatic_complexity: MetricDelta::new(
                before.map(|metrics| metrics.cyclomatic_complexity),
                after.map(|metrics| metrics.cyclomatic_complexity),
            ),
            cognitive_complexity: MetricDelta::new(
                before.map(|metrics| metrics.cognitive_complexity),
                after.map(|metrics| metrics.cognitive_complexity),
            ),
        },
        change: ChurnView {
            changed_hunks: function.churn.changed_hunks,
            lines_added: function.churn.lines_added,
            lines_removed: function.churn.lines_removed,
            hunk_overlap: overlap_fraction(function),
        },
        risk: assessment.risk,
        review_priority: assessment.review_priority,
        match_confidence: function.match_confidence.as_fraction(),
        range: RangeView {
            before: function.base_range.as_ref().map(LineRange::from),
            after: function.target_range.as_ref().map(LineRange::from),
        },
    }
}

/// Share of a function's lines the diff touches, against the revision it still
/// exists in. Rounded so the value is stable wherever it is rendered.
fn overlap_fraction(function: &FunctionResult) -> f64 {
    let length = function
        .metrics_after
        .as_ref()
        .or(function.metrics_before.as_ref())
        .map_or(0, |metrics| metrics.physical_loc);
    if length == 0 {
        return 0.0;
    }
    let touched = function.churn.lines_added.max(function.churn.lines_removed);
    let ratio = f64::from(touched.min(length)) / f64::from(length);
    (ratio * 100.0).round() / 100.0
}

impl From<&SourceRange> for LineRange {
    fn from(range: &SourceRange) -> Self {
        Self {
            start_line: range.start_line,
            end_line: range.end_line,
        }
    }
}

fn touching_hunks(file: &FileResult, function: &FunctionResult) -> Vec<HunkView> {
    file.hunks
        .iter()
        .filter(|hunk| touches(function, hunk))
        .map(|hunk| HunkView {
            base_start: hunk.base_start,
            base_count: hunk.base_count,
            target_start: hunk.target_start,
            target_count: hunk.target_count,
        })
        .collect()
}

fn touches(function: &FunctionResult, hunk: &DiffHunk) -> bool {
    let spans = |range: Option<&SourceRange>, start: u32, count: u32| {
        range.is_some_and(|range| {
            let end = start.saturating_add(count.saturating_sub(1)).max(start);
            start <= range.end_line && end >= range.start_line.saturating_sub(1)
        })
    };
    spans(
        function.base_range.as_ref(),
        hunk.base_start,
        hunk.base_count,
    ) || spans(
        function.target_range.as_ref(),
        hunk.target_start,
        hunk.target_count,
    )
}

impl<'a> From<&'a Diagnostic> for DiagnosticView<'a> {
    fn from(diagnostic: &'a Diagnostic) -> Self {
        Self {
            code: crate::output::diagnostic_code(diagnostic.code),
            severity: crate::output::severity(diagnostic.severity),
            message: &diagnostic.message,
            path: diagnostic.path.as_deref(),
            related_entity_ids: diagnostic
                .related_entity_ids
                .iter()
                .map(String::as_str)
                .collect(),
        }
    }
}

fn diagnostic_counts(diagnostics: &[Diagnostic]) -> DiagnosticCounts {
    diagnostic_counts_iter(diagnostics.iter())
}

fn diagnostic_counts_iter<'a>(
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) -> DiagnosticCounts {
    let mut counts = DiagnosticCounts::default();
    for diagnostic in diagnostics {
        counts.add(diagnostic.severity);
    }
    counts
}

/// Take one page out of a ranked list.
fn paginate<T>(rows: Vec<T>, page: &Page) -> (Vec<T>, PageWindow) {
    let total = rows.len();
    let (offset, limit) = page.window();
    let selected = rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let returned = selected.len();
    let next = offset + returned;
    let has_more = next < total;
    (
        selected,
        PageWindow {
            returned,
            total,
            has_more,
            next_offset: has_more.then_some(next),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{
        COMPLEXITY_EXTRACTION, FileFilter, FunctionFilter, OTHER_CHANGE_SHAPE, Page,
        aggregate_complexity, change_area, change_summary, get_analysis_diagnostics,
        get_function_change, list_changed_files, list_changed_functions,
    };
    use crate::{
        DiffHunk, FileStatus,
        analysis::{FunctionChangeStatus, FunctionChurn, MatchConfidence},
        languages::{DiagnosticSeverity, FunctionKind, Language, SourceRange},
        metrics::FunctionMetrics,
        query::{
            classify::FileClassification,
            risk::{REVIEW_PRIORITY_MODEL, RISK_MODEL, RiskLevel},
        },
        result::{
            AnalysisResult, AnalysisSummary, Diagnostic, DiagnosticCode, FileResult,
            FunctionResult, RevisionResult, SCHEMA_VERSION,
        },
    };

    #[test]
    fn groups_paths_into_areas_by_project_boundary() {
        assert_eq!(
            change_area("packages/runtime-core/src/renderer.ts"),
            "packages/runtime-core"
        );
        assert_eq!(change_area("crates/parser/src/lib.rs"), "crates/parser");
        assert_eq!(change_area("src/index.ts"), "src");
        assert_eq!(change_area("README.md"), "<root>");
    }

    #[test]
    fn omits_unchanged_functions_unless_they_are_asked_for() {
        let result = fixture();

        let default = list_changed_functions(&result, &FunctionFilter::default(), None);
        assert!(
            default
                .functions
                .iter()
                .all(|function| function.status != "unchanged")
        );

        let including = list_changed_functions(
            &result,
            &FunctionFilter {
                include_unchanged: true,
                ..FunctionFilter::default()
            },
            None,
        );
        assert!(including.functions.len() > default.functions.len());
    }

    #[test]
    fn filters_functions_by_classification_and_risk() {
        let result = fixture();

        let source_only = list_changed_functions(
            &result,
            &FunctionFilter {
                classification: Some(FileClassification::Source),
                ..FunctionFilter::default()
            },
            None,
        );
        assert!(
            source_only
                .functions
                .iter()
                .all(|function| function.classification == "source")
        );
        assert!(
            source_only
                .functions
                .iter()
                .all(|function| !function.file.contains("__tests__"))
        );

        let high_only = list_changed_functions(
            &result,
            &FunctionFilter {
                minimum_risk: Some(RiskLevel::High),
                ..FunctionFilter::default()
            },
            None,
        );
        assert!(
            high_only
                .functions
                .iter()
                .all(|function| function.risk.level == RiskLevel::High)
        );
    }

    #[test]
    fn pages_deterministically_without_dropping_or_repeating_rows() {
        let result = fixture();
        let all = list_changed_functions(
            &result,
            &FunctionFilter {
                page: Page {
                    limit: Some(100),
                    offset: 0,
                },
                ..FunctionFilter::default()
            },
            None,
        );

        let first = list_changed_functions(
            &result,
            &FunctionFilter {
                page: Page {
                    limit: Some(1),
                    offset: 0,
                },
                ..FunctionFilter::default()
            },
            None,
        );
        assert_eq!(first.page.returned, 1);
        assert_eq!(first.page.total, all.page.total);
        assert!(first.page.has_more);
        assert_eq!(first.page.next_offset, Some(1));

        let second = list_changed_functions(
            &result,
            &FunctionFilter {
                page: Page {
                    limit: Some(1),
                    offset: 1,
                },
                ..FunctionFilter::default()
            },
            None,
        );
        assert_eq!(first.functions[0].symbol, all.functions[0].symbol);
        assert_eq!(second.functions[0].symbol, all.functions[1].symbol);
    }

    #[test]
    fn addresses_a_function_by_the_identity_a_list_reported() {
        let result = fixture();
        let listed = list_changed_functions(&result, &FunctionFilter::default(), None);
        let listed = listed.functions.first().expect("a changed function");

        let detail =
            get_function_change(&result, &listed.function_id, None).expect("an identity resolves");

        assert_eq!(detail.function.function_id, listed.function_id);
        assert_eq!(detail.function.qualified_name, listed.qualified_name);
        assert_eq!(detail.function.symbol, listed.symbol);
    }

    #[test]
    fn tells_apart_two_functions_that_share_a_name() {
        // One file can declare the same name on both sides of a change, and
        // each record still has to be addressable on its own.
        let result = duplicate_name_fixture();
        let listed = list_changed_functions(&result, &FunctionFilter::default(), None);

        let identities = listed
            .functions
            .iter()
            .map(|function| function.function_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(identities.len(), 2);
        assert_ne!(identities[0], identities[1]);
        // The removal is named on the base side, the modified function on the
        // side a reader will find it on.
        assert!(identities.iter().any(|id| id.contains("@base:")));
        assert!(identities.iter().any(|id| id.contains("@target:")));

        for identity in &identities {
            let detail = get_function_change(&result, identity, None).expect("identity resolves");
            assert_eq!(&detail.function.function_id, identity);
        }
    }

    #[test]
    fn reports_identities_an_unknown_one_would_have_to_be() {
        let result = fixture();

        let requested = "src/renderer.ts#fn:absent@target:9:9";
        let error = get_function_change(&result, requested, None)
            .expect_err("unknown identity is rejected");

        assert_eq!(error.function_id, requested);
        assert!(
            error
                .known_function_ids
                .iter()
                .any(|identity| identity.contains("fn:patch"))
        );
    }

    #[test]
    fn sums_complexity_across_both_revisions_of_a_file() {
        let result = fixture();
        let renderer = result
            .files
            .iter()
            .find(|file| file.target_path.as_deref() == Some("src/renderer.ts"))
            .expect("renderer file");

        let total = aggregate_complexity(renderer);

        // `patch` 10 -> 15 and the untouched `mount` 4 -> 4.
        assert_eq!(total.cyclomatic_before, 14);
        assert_eq!(total.cyclomatic_after, 19);
        assert_eq!(total.cyclomatic_delta, 5);
    }

    #[test]
    fn overview_counts_every_file_and_ranks_candidates() {
        let result = fixture();
        let summary = change_summary(&result, None);

        assert_eq!(summary.files.changed, 2);
        assert_eq!(summary.files.by_classification.get("source"), Some(&1));
        assert_eq!(summary.files.by_classification.get("test"), Some(&1));
        assert!(!summary.review_candidates.is_empty());
        // The riskiest candidate leads.
        assert_eq!(summary.review_candidates[0].symbol, "fn:patch");
    }

    #[test]
    fn lists_files_and_scopes_diagnostics_to_one_file() {
        let result = fixture();

        let source = list_changed_files(
            &result,
            &FileFilter {
                classification: Some(FileClassification::Source),
                ..FileFilter::default()
            },
            None,
        );
        assert_eq!(source.files.len(), 1);
        assert_eq!(source.files[0].path, "src/renderer.ts");
        assert_eq!(source.files[0].area, "src");

        let all = get_analysis_diagnostics(&result, None);
        let scoped = get_analysis_diagnostics(&result, Some("src/renderer.ts"));
        assert!(scoped.diagnostics.len() <= all.diagnostics.len());
    }

    #[test]
    fn counts_every_diagnostic_severity_and_the_total() {
        let mut result = fixture();
        result.files[0].diagnostics = vec![
            diagnostic(DiagnosticSeverity::Info, "for reference"),
            diagnostic(DiagnosticSeverity::Warning, "worth a look"),
            diagnostic(DiagnosticSeverity::Error, "cannot be parsed"),
        ];

        let listed = get_analysis_diagnostics(&result, Some("src/renderer.ts"));

        assert_eq!(listed.counts.info, 1);
        assert_eq!(listed.counts.warnings, 1);
        assert_eq!(listed.counts.errors, 1);
        assert_eq!(listed.counts.total, 3);
        assert_eq!(listed.diagnostics.len(), 3);
    }

    #[test]
    fn reports_defect_risk_and_review_priority_as_separate_models() {
        let result = fixture();
        let listed = list_changed_functions(&result, &FunctionFilter::default(), None);
        let source = listed
            .functions
            .iter()
            .find(|function| function.file == "src/renderer.ts")
            .expect("a source function");

        assert_eq!(source.risk.model, RISK_MODEL);
        assert_eq!(source.review_priority.model, REVIEW_PRIORITY_MODEL);
        assert_ne!(source.risk.model, source.review_priority.model);
        // Production source is exposure, not intrinsic risk: the same function
        // is deeper in the review queue than its own code alone makes it.
        assert!(source.review_priority.score > source.risk.score);
        assert!(
            source
                .review_priority
                .reasons
                .iter()
                .any(|reason| reason.code == "production_source")
        );
        assert!(
            !source
                .risk
                .reasons
                .iter()
                .any(|reason| reason.code == "production_source")
        );
    }

    #[test]
    fn names_a_change_that_extracted_a_helper_out_of_a_function() {
        let mut result = fixture();
        result.files[0].functions = vec![
            // The function that gave work up.
            function(
                "patch",
                FunctionChangeStatus::Modified,
                Some(18),
                Some(6),
                12,
            ),
            // The helper that took it.
            function("patchInner", FunctionChangeStatus::Added, None, Some(8), 8),
        ];

        let listed = list_changed_files(&result, &FileFilter::default(), None);
        let extracted = listed
            .files
            .iter()
            .find(|file| file.path == "src/renderer.ts")
            .expect("the source file");

        assert_eq!(extracted.change_shape, COMPLEXITY_EXTRACTION);
        // The file ends up less complex than it started.
        assert!(extracted.complexity.cognitive_delta < 0);
    }

    #[test]
    fn still_names_an_extraction_that_only_moved_complexity() {
        let mut result = fixture();
        result.files[0].functions = vec![
            // Gives up its complexity entirely.
            function(
                "patch",
                FunctionChangeStatus::Modified,
                Some(15),
                Some(0),
                12,
            ),
            // The helper takes exactly what was given up.
            function(
                "patchInner",
                FunctionChangeStatus::Added,
                None,
                Some(15),
                15,
            ),
        ];

        let listed = list_changed_files(&result, &FileFilter::default(), None);
        let extracted = listed
            .files
            .iter()
            .find(|file| file.path == "src/renderer.ts")
            .expect("the source file");

        assert_eq!(extracted.complexity.cognitive_delta, 0);
        assert_eq!(extracted.change_shape, COMPLEXITY_EXTRACTION);
    }

    #[test]
    fn calls_a_helper_that_added_more_than_it_took_other() {
        let mut result = fixture();
        result.files[0].functions = vec![
            function("patch", FunctionChangeStatus::Modified, Some(4), Some(2), 6),
            function(
                "patchInner",
                FunctionChangeStatus::Added,
                None,
                Some(20),
                20,
            ),
        ];

        let listed = list_changed_files(&result, &FileFilter::default(), None);
        let grown = listed
            .files
            .iter()
            .find(|file| file.path == "src/renderer.ts")
            .expect("the source file");

        assert!(grown.complexity.cognitive_delta > 0);
        assert_eq!(grown.change_shape, OTHER_CHANGE_SHAPE);
    }

    #[test]
    fn calls_a_change_that_moved_no_complexity_out_of_a_function_other() {
        let result = fixture();

        let listed = list_changed_files(&result, &FileFilter::default(), None);

        assert!(!listed.files.is_empty());
        assert!(
            listed
                .files
                .iter()
                .all(|file| file.change_shape == OTHER_CHANGE_SHAPE)
        );
    }

    /// A file that declares one name on each side of the change: the version
    /// being removed, and the one that replaced it.
    fn duplicate_name_fixture() -> AnalysisResult {
        let mut result = fixture();
        let mut removed = function("render", FunctionChangeStatus::Removed, Some(12), None, 12);
        removed.base_range = Some(source_range(60, 70));
        removed.target_range = None;
        let mut current = function(
            "render",
            FunctionChangeStatus::Modified,
            Some(4),
            Some(9),
            8,
        );
        current.base_range = Some(source_range(5, 10));
        current.target_range = Some(source_range(5, 14));
        result.files = vec![file("src/renderer.ts", vec![current, removed])];
        result
    }

    fn source_range(start_line: u32, end_line: u32) -> SourceRange {
        SourceRange {
            start_line,
            start_column: 0,
            end_line,
            end_column: 1,
        }
    }

    fn diagnostic(severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            code: DiagnosticCode::MetricUnavailable,
            severity,
            message: message.to_owned(),
            path: Some("src/renderer.ts".to_owned()),
            range: None,
            related_entity_ids: Vec::new(),
        }
    }

    fn fixture() -> AnalysisResult {
        AnalysisResult {
            schema_version: SCHEMA_VERSION,
            tool_version: "0.2.0".to_owned(),
            repository: "/repo".to_owned(),
            base: RevisionResult {
                id: "aaa".to_owned(),
                display_name: "base".to_owned(),
            },
            target: RevisionResult {
                id: "bbb".to_owned(),
                display_name: "target".to_owned(),
            },
            summary: AnalysisSummary {
                changed_files: 2,
                added_lines: 30,
                removed_lines: 10,
                supported_files: 2,
                unsupported_files: 0,
                ..AnalysisSummary::default()
            },
            files: vec![
                file(
                    "src/renderer.ts",
                    vec![
                        function(
                            "patch",
                            FunctionChangeStatus::Modified,
                            Some(10),
                            Some(15),
                            20,
                        ),
                        function(
                            "mount",
                            FunctionChangeStatus::Unchanged,
                            Some(4),
                            Some(4),
                            0,
                        ),
                    ],
                ),
                file(
                    "src/__tests__/renderer.spec.ts",
                    vec![function(
                        "describe(\"renderer\").<anonymous>#1",
                        FunctionChangeStatus::Modified,
                        Some(2),
                        Some(9),
                        12,
                    )],
                ),
            ],
            diagnostics: Vec::new(),
        }
    }

    fn file(path: &str, functions: Vec<FunctionResult>) -> FileResult {
        FileResult {
            base_path: Some(path.to_owned()),
            target_path: Some(path.to_owned()),
            status: FileStatus::Modified,
            language: Some(Language::TypeScript),
            is_binary: false,
            added_lines: 15,
            removed_lines: 5,
            hunks: vec![DiffHunk {
                base_start: 1,
                base_count: 5,
                target_start: 1,
                target_count: 15,
                added_lines: 15,
                removed_lines: 5,
            }],
            functions,
            exports_added: Vec::new(),
            exports_removed: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn function(
        name: &str,
        status: FunctionChangeStatus,
        before: Option<u32>,
        after: Option<u32>,
        churned: u32,
    ) -> FunctionResult {
        FunctionResult {
            id: format!("function-{name}"),
            status,
            kind: FunctionKind::Function,
            qualified_name: name.to_owned(),
            base_range: Some(SourceRange {
                start_line: 1,
                start_column: 0,
                end_line: 20,
                end_column: 1,
            }),
            target_range: Some(SourceRange {
                start_line: 1,
                start_column: 0,
                end_line: 30,
                end_column: 1,
            }),
            metrics_before: before.map(metrics),
            metrics_after: after.map(metrics),
            churn: FunctionChurn {
                lines_removed: 0,
                lines_added: churned,
                changed_hunks: 1,
            },
            match_confidence: MatchConfidence::Exact,
            diagnostics: Vec::new(),
        }
    }

    fn metrics(complexity: u32) -> FunctionMetrics {
        FunctionMetrics {
            physical_loc: 30,
            source_loc: 25,
            cyclomatic_complexity: complexity,
            cognitive_complexity: complexity,
        }
    }
}
