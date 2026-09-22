mod typescript;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use crate::{DiffScopeError, metrics::FunctionMetrics};

pub use typescript::TypeScriptAnalyzer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Language {
    TypeScript,
    Tsx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FunctionKind {
    Function,
    Method,
    Constructor,
    ArrowFunction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// One call written inside a function body, as the collector saw it.
///
/// A plain identifier callee is a call to a name the file has: a declaration
/// of its own, or something it imports. A member callee is recorded in the
/// two shapes that write a property name — `receiver.name()` and
/// `receiver["name"]` — because a receiver that is a namespace import names
/// a module whose exports are known, and a property name a receiver does not
/// explain is still a name a heuristic may match. A computed callee with a
/// non-literal key (`a[b]()`), a call on a call, and a deeper chain record
/// nothing at all: they name no property for any stage to work from.
///
/// The file is not recorded per call: a call site belongs to the function whose
/// body contains it, and that function's own path is the file. Storing it again
/// per call would repeat one path across thousands of sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// Callee text as written, or the property name for a member callee.
    pub name: String,
    /// The receiver a member callee was written on, if it was one.
    ///
    /// `None` is a bare `name()`, which resolves against the file's own
    /// declarations and its imports. `Some` is a property or computed access,
    /// which resolves exactly only when the receiver is a namespace import.
    pub receiver: Option<String>,
    /// 1-based line the call expression starts on.
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDefinition {
    pub language: Language,
    pub kind: FunctionKind,
    pub qualified_name: String,
    pub range: SourceRange,
    pub metrics: FunctionMetrics,
    /// Calls written directly in this body, in source order, duplicates kept.
    ///
    /// A call inside a nested function belongs to that nested function, not to
    /// the one enclosing it, for the same reason a decision there does not
    /// raise the enclosing function's complexity: attributing it upward would
    /// make the enclosing function appear to call things it does not, and the
    /// nested function is a node of its own.
    pub calls: Vec<CallSite>,
    /// Hash of the function's source with whitespace runs collapsed.
    ///
    /// Used only to pair functions that share one identity, so that a group of
    /// same-named functions can be matched by what they contain rather than
    /// abandoned. It is never serialized and never compared across processes.
    pub body_hash: u64,
}

/// One name a module exports, and what stands behind it.
///
/// [`SourceAnalysis::exports`] answers "is this name part of the surface?",
/// which is all an export-surface delta needs. Resolving a call needs the
/// opposite direction: knowing `"stripComments"` is exported does not say
/// which function it is, so the record keeps the name behind the export and
/// the position of the definition when the export names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedSymbol {
    /// Name in the module the export reads from: the local name for
    /// `export { a as b }`, and the source name for
    /// `export { a as b } from "x"`, which is what the next hop looks up.
    pub local: String,
    /// Specifier this name is forwarded from, when it is a re-export.
    ///
    /// `None` means the name is declared here, so a lookup stops. `Some`
    /// means the definition lives in another module and resolution continues
    /// there, one bounded hop at a time.
    pub from: Option<String>,
    /// Where the function this name declares is written, when it declares one
    /// in this file.
    ///
    /// A name exported as a constant, a class, or a type carries `None`: the
    /// graph reports calls into functions, and a range for something that is
    /// not one would be evidence for an edge that does not exist.
    pub function: Option<SourceRange>,
    /// 1-based line the export is written on.
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAnalysis {
    pub language: Option<Language>,
    pub functions: Vec<FunctionDefinition>,
    /// Names this module exports, as a caller would import them.
    ///
    /// Sorted and deduplicated, so comparing two revisions' sets reports what a
    /// change adds to or removes from the module's public surface.
    pub exports: BTreeSet<String>,
    /// The same names, each with what stands behind it, keyed by the exported
    /// name.
    ///
    /// This is a parallel record rather than a richer `exports`, so the
    /// export-surface delta keeps comparing the sets it always compared. A
    /// whole-module re-export is never a key here: `exports` records it as `*`
    /// precisely because the names it forwards live in a file this analysis
    /// has not read, and two `export *` clauses would collide on one key.
    pub exported_symbols: BTreeMap<String, ExportedSymbol>,
    /// Specifiers this module re-exports from, including `export * from`.
    ///
    /// A re-export is a structural relationship of its own, and one that
    /// breaks callers when it goes away, so it is recorded whether or not the
    /// names behind it could be read.
    pub re_exported_modules: BTreeSet<String>,
    pub diagnostics: Vec<LanguageDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageDiagnostic {
    pub code: LanguageDiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub range: Option<SourceRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageDiagnosticCode {
    UnsupportedLanguage,
    InvalidUtf8,
    MalformedSource,
    ParseError,
    OversizedFile,
}

/// Upper bound on the prefix parsed when collecting one file's imports.
///
/// The prefix is normally chosen from the file's own contents, but an absolute
/// bound is still needed: analyzed repositories are untrusted, and a file whose
/// last `import` token sits at its very end would otherwise be parsed whole.
/// A file cut by this bound reports it, so an edge is never quietly missing.
pub const MAX_IMPORT_SCAN_BYTES: usize = 64 * 1024;

/// Bytes kept after the last import-like token in a file.
///
/// An import statement is `import ... from <specifier>`, so its specifier
/// always follows the last `import` or `from` token in the statement. Keeping a
/// margin past that token therefore reads the statement whole, including a
/// multiline one, without reading the rest of the file.
///
/// Measured against a real Vue revision, this recovers every import of all 491
/// TypeScript files while parsing 56% of their bytes. Anchoring more tightly,
/// on a quoted `from '` instead, parses 20% but loses one file's imports; this
/// index is built once per revision and cached, so the cheaper anchor is not
/// worth an edge that silently does not exist.
const IMPORT_SCAN_MARGIN: usize = 512;

/// What one name an import statement binds refers to in the module it came
/// from.
///
/// The distinction is what makes a cross-file call resolvable: `b()` where
/// `import { a as b } from "x"` means the exported name `a`, and `ns.name()`
/// where `import * as ns from "x"` means the exported name `name`. A record
/// that kept only the local name would resolve neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedName {
    /// A named import, under the name the exporting module publishes.
    Named(String),
    /// The module's default export.
    Default,
    /// The module itself, bound as a namespace object.
    Namespace,
}

/// One name a file imports, and where it came from.
///
/// The specifier is kept unresolved, as written, because resolution needs the
/// revision's file set and its `tsconfig.json` aliases, and neither belongs to
/// a single file's scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBinding {
    /// What the local name refers to in the module it came from.
    pub imported: ImportedName,
    /// Module specifier exactly as written, before any resolution.
    pub specifier: String,
    /// 1-based line the import statement's specifier sits on, matching the
    /// line [`ImportScan::specifiers`] reports for the same statement.
    pub line: u32,
}

/// Everything one parse of a file's import region yields.
///
/// Specifiers and bindings are read from the same statements in one walk: a
/// second pass to collect the names behind specifiers the first pass already
/// visited would double the cost of the scan that covers a whole revision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportFacts {
    pub specifiers: BTreeMap<String, u32>,
    pub bindings: BTreeMap<String, ImportBinding>,
}

/// What one file imports, and whether the whole file was examined.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportScan {
    /// Module specifiers exactly as written, before any resolution, each with
    /// the 1-based line its statement sits on.
    ///
    /// The specifier is what an edge is resolved from and the line is where a
    /// reader checks that the edge exists, so the scan reports the two
    /// together. A specifier imported twice is one edge with one site, and the
    /// earliest line seen for it is kept. The keys are ordered exactly as the
    /// set of specifiers was, so nothing about the scan's determinism moves.
    pub specifiers: BTreeMap<String, u32>,
    /// The names this file binds from those specifiers, keyed by the local
    /// name the file uses.
    ///
    /// Keyed locally because that is the only name a call site writes: a
    /// resolver holds `b` from `b()` and needs what `b` stands for. A local
    /// name is bound once per module in well-formed TypeScript, so the key is
    /// unique; when a prefix truncation leaves two statements binding one
    /// name, the first wins and the scan reports itself truncated.
    ///
    /// These come from the same statements as the specifiers, inside the
    /// prefix already parsed, so recording them reads no further into a file.
    pub bindings: BTreeMap<String, ImportBinding>,
    /// The file was longer than [`MAX_IMPORT_SCAN_BYTES`] and was not read whole.
    pub truncated: bool,
}

/// A reusable scanner for the imports of many files.
///
/// Loading a Tree-sitter grammar costs far more than scanning one file's
/// leading region, and an index covers every source file of a revision, so the
/// grammar is loaded once and the parser reused across the whole walk.
pub struct ImportScanner {
    typescript: TypeScriptAnalyzer,
    tsx: TypeScriptAnalyzer,
}

impl ImportScanner {
    /// Load the analyzers an import scan needs.
    ///
    /// # Errors
    ///
    /// Returns an error when a Tree-sitter grammar cannot be loaded.
    pub fn new() -> Result<Self, DiffScopeError> {
        Ok(Self {
            typescript: TypeScriptAnalyzer::new(Language::TypeScript)?,
            tsx: TypeScriptAnalyzer::new(Language::Tsx)?,
        })
    }

    /// Collect the module specifiers one source file imports or re-exports,
    /// and the names it binds from them.
    ///
    /// Only the file's leading region is parsed and no metrics are computed:
    /// this runs over every file of a revision, not only the changed ones, so
    /// it must cost far less than a full analysis.
    ///
    /// # Errors
    ///
    /// Returns an error when parsing fails before a syntax tree is produced.
    pub fn scan(&mut self, path: &Path, source: &[u8]) -> Result<ImportScan, DiffScopeError> {
        let Some(language) = detect_language(path) else {
            return Ok(ImportScan::default());
        };

        let wanted = import_prefix_len(source);
        // The prefix is lossy only when the absolute bound cuts it short of
        // where the file's own contents said the imports end.
        let truncated = wanted > MAX_IMPORT_SCAN_BYTES;
        let mut end = wanted.min(MAX_IMPORT_SCAN_BYTES).min(source.len());
        // Never split a multi-byte character: the parser takes `str`, and a
        // partial character would make the whole prefix unreadable.
        while end > 0 && !is_utf8_boundary(source, end) {
            end -= 1;
        }
        let head = &source[..end];

        let analyzer = match language {
            Language::TypeScript => &mut self.typescript,
            Language::Tsx => &mut self.tsx,
        };
        let ImportFacts {
            mut specifiers,
            mut bindings,
        } = analyzer.scan_imports(head)?;
        specifiers.retain(|specifier, _| !specifier.is_empty());
        bindings.retain(|_, binding| !binding.specifier.is_empty());
        Ok(ImportScan {
            specifiers,
            bindings,
            truncated,
        })
    }
}

/// How much of a file must be parsed to see every import it declares.
fn import_prefix_len(source: &[u8]) -> usize {
    let anchor = last_occurrence(source, b"import").max(last_occurrence(source, b"from"));
    anchor.saturating_add(IMPORT_SCAN_MARGIN)
}

/// Offset of the last occurrence of `needle`, or `0` when it does not occur.
fn last_occurrence(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|start| &haystack[*start..start + needle.len()] == needle)
        .unwrap_or(0)
}

/// Whether `index` starts a UTF-8 character, without decoding the whole input.
fn is_utf8_boundary(source: &[u8], index: usize) -> bool {
    source
        .get(index)
        .is_none_or(|byte| (*byte & 0b1100_0000) != 0b1000_0000)
}

/// Largest source blob that is parsed for metrics.
///
/// Analyzed repositories are untrusted and routinely contain generated or
/// vendored sources of arbitrary size. A syntax tree costs many times the
/// source it describes, and both revisions of a file are analyzed, so an
/// unbounded file size is an unbounded memory requirement. Files above this
/// limit are still inventoried, with their diff statistics and an
/// `oversized_file` diagnostic, but no function metrics.
pub const MAX_ANALYZED_BLOB_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// Analyze one source blob using the analyzer selected from its path.
///
/// # Errors
///
/// Returns an error when a supported analyzer cannot be initialized or parsing
/// fails before a syntax tree can be produced.
pub fn analyze_source(path: &Path, source: &[u8]) -> Result<SourceAnalysis, DiffScopeError> {
    let Some(language) = detect_language(path) else {
        return Ok(SourceAnalysis {
            language: None,
            functions: Vec::new(),
            exports: BTreeSet::new(),
            exported_symbols: BTreeMap::new(),
            re_exported_modules: BTreeSet::new(),
            diagnostics: vec![LanguageDiagnostic {
                code: LanguageDiagnosticCode::UnsupportedLanguage,
                severity: DiagnosticSeverity::Info,
                message: "unsupported language".to_owned(),
                range: None,
            }],
        });
    };

    if source.len() > MAX_ANALYZED_BLOB_BYTES {
        return Ok(SourceAnalysis {
            language: Some(language),
            functions: Vec::new(),
            exports: BTreeSet::new(),
            exported_symbols: BTreeMap::new(),
            re_exported_modules: BTreeSet::new(),
            diagnostics: vec![LanguageDiagnostic {
                code: LanguageDiagnosticCode::OversizedFile,
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "source is {} bytes, above the {MAX_ANALYZED_BLOB_BYTES} byte analysis limit; function metrics are unavailable",
                    source.len()
                ),
                range: None,
            }],
        });
    }

    match language {
        Language::TypeScript | Language::Tsx => TypeScriptAnalyzer::new(language)?.analyze(source),
    }
}

/// Address one function within its file, independently of line numbers.
///
/// The kind is included because a name alone does not separate a method from an
/// arrow function assigned to a property of the same name. Callers pass this
/// back to ask about one function, and a bare qualified name is also accepted
/// when it is unambiguous.
#[must_use]
pub fn symbol_id(kind: FunctionKind, qualified_name: &str) -> String {
    let tag = match kind {
        FunctionKind::Function => "fn",
        FunctionKind::Method => "method",
        FunctionKind::Constructor => "ctor",
        FunctionKind::ArrowFunction => "arrow",
    };
    format!("{tag}:{qualified_name}")
}

/// Hash source text, treating every run of whitespace as a single space.
///
/// FNV-1a is written out rather than using the standard hasher because the
/// standard hasher's output is explicitly allowed to change between Rust
/// releases, and pairing behavior should not change with the compiler.
#[must_use]
pub(crate) fn body_hash(text: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    let mut pending_space = false;
    for byte in text.trim().bytes() {
        if byte.is_ascii_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            hash = (hash ^ u64::from(b' ')).wrapping_mul(PRIME);
            pending_space = false;
        }
        hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    hash
}

pub fn detect_language(path: &Path) -> Option<Language> {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("ts" | "mts" | "cts") => Some(Language::TypeScript),
        Some("tsx") => Some(Language::Tsx),
        _ => None,
    }
}

impl SourceRange {
    pub(crate) fn from_tree_sitter(range: tree_sitter::Range) -> Result<Self, DiffScopeError> {
        Ok(Self {
            start_line: to_u32(range.start_point.row + 1, "start line")?,
            start_column: to_u32(range.start_point.column, "start column")?,
            end_line: to_u32(range.end_point.row + 1, "end line")?,
            end_column: to_u32(range.end_point.column, "end column")?,
        })
    }
}

fn to_u32(value: usize, name: &str) -> Result<u32, DiffScopeError> {
    u32::try_from(value)
        .map_err(|error| DiffScopeError::Language(format!("{name} is too large: {error}")))
}
