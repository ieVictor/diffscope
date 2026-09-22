//! Who imports what, across a whole revision.
//!
//! Every other part of `DiffScope` answers questions about the files a diff
//! contains. This one cannot: "what breaks if this changes?" is a question
//! about the files the diff *does not* contain, so the index is built over a
//! whole commit tree rather than over a change.
//!
//! That makes cost the governing constraint. Only each file's leading region is
//! parsed, because import statements live there, and the result is keyed by the
//! tree it describes so that it can be reused for every comparison that shares
//! a target revision.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{
    BlobContent, DiffScopeError, TreeEntry,
    git::Repository,
    languages::{
        ExportedSymbol, FunctionDefinition, ImportBinding, ImportScanner, SourceRange,
        analyze_source, detect_language,
    },
};

#[cfg(test)]
use crate::languages::ImportedName;

/// Extensions tried, in order, when a specifier names no file extension.
const CANDIDATE_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "d.ts", "js", "jsx"];

/// File stems tried when a specifier resolves to a directory.
const DIRECTORY_ENTRIES: &[&str] = &["index"];

/// A resolved import graph over one revision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportIndex {
    /// Repository-relative path to the paths it imports.
    outbound: BTreeMap<String, BTreeSet<String>>,
    /// Repository-relative path to the paths that import it.
    inbound: BTreeMap<String, BTreeSet<String>>,
    /// Importing path to imported path to the line the import statement sits
    /// on.
    ///
    /// One entry per resolved edge, so a revision pays for the edges it has and
    /// not for its specifiers: an unresolved specifier is counted instead, and
    /// never gets a line to report. Nesting by importer rather than keying a
    /// flat `(importer, imported)` pair keeps a lookup borrowing the two names
    /// a caller already has instead of building an owned key for it.
    lines: BTreeMap<String, BTreeMap<String, u32>>,
    /// Importing path to local name to what that name binds.
    ///
    /// The scan reads these from the statements it already parsed, so a
    /// revision's bindings cost no extra parsing. They are kept for every file
    /// rather than for the changed ones because the files that call into a
    /// change are exactly the files the diff does not contain.
    bindings: BTreeMap<String, BTreeMap<String, ImportBinding>>,
    /// Importing path to specifier to the path it resolved to.
    ///
    /// Resolution needs the revision's file set and its `tsconfig.json`
    /// aliases, neither of which survives the build; recording the answer
    /// instead means a later lookup resolves a specifier exactly as the edge
    /// that was built from it did, and cannot drift from it.
    targets: BTreeMap<String, BTreeMap<String, String>>,
    /// Files scanned only up to the import scan limit, whose later imports are
    /// therefore not represented.
    truncated: BTreeSet<String>,
    /// Specifiers that named no file in this revision, such as npm packages.
    unresolved: u32,
    /// Every source file scanned, whether or not it takes part in an edge.
    files: BTreeSet<String>,
}

impl ImportIndex {
    /// Paths that import `path` directly.
    #[must_use]
    pub fn importers(&self, path: &str) -> &BTreeSet<String> {
        const EMPTY: &BTreeSet<String> = &BTreeSet::new();
        self.inbound.get(path).unwrap_or(EMPTY)
    }

    /// Paths `path` imports directly.
    #[must_use]
    pub fn dependencies(&self, path: &str) -> &BTreeSet<String> {
        const EMPTY: &BTreeSet<String> = &BTreeSet::new();
        self.outbound.get(path).unwrap_or(EMPTY)
    }

    /// Line the import statement that produced this edge sits on.
    ///
    /// `None` when this revision has no resolved edge between the two paths: a
    /// line number for a relationship the index does not contain would be
    /// evidence for something that is not there.
    ///
    /// A module imported by several statements is one edge with one site, and
    /// the earliest line is kept, because that is the first place a reader can
    /// look.
    #[must_use]
    pub fn import_line(&self, importer: &str, target: &str) -> Option<u32> {
        self.lines
            .get(importer)
            .and_then(|imports| imports.get(target))
            .copied()
    }

    /// What one file's local names bind, keyed by the local name.
    ///
    /// A call site writes a local name and nothing else, so this is the map a
    /// cross-file resolver starts from.
    #[must_use]
    pub fn bindings(&self, path: &str) -> &BTreeMap<String, ImportBinding> {
        const EMPTY: &BTreeMap<String, ImportBinding> = &BTreeMap::new();
        self.bindings.get(path).unwrap_or(EMPTY)
    }

    /// The path one file's specifier resolved to in this revision.
    ///
    /// `None` for a specifier that named nothing here, which is almost always
    /// an installed package: it is counted at build time and never guessed at.
    #[must_use]
    pub fn resolve_specifier(&self, from: &str, specifier: &str) -> Option<&str> {
        self.targets
            .get(from)
            .and_then(|targets| targets.get(specifier))
            .map(String::as_str)
    }

    /// Record one resolved edge, keeping the earliest line it was seen at.
    ///
    /// The three views of one edge — who imports, who is imported, and where it
    /// is written — move together, so the index can never hold a line for an
    /// edge it does not have.
    fn add_edge(&mut self, importer: &str, target: &str, line: u32) {
        self.outbound
            .entry(importer.to_owned())
            .or_default()
            .insert(target.to_owned());
        self.inbound
            .entry(target.to_owned())
            .or_default()
            .insert(importer.to_owned());
        self.lines
            .entry(importer.to_owned())
            .or_default()
            .entry(target.to_owned())
            .and_modify(|existing| *existing = (*existing).min(line))
            .or_insert(line);
    }

    /// Paths that reach `path` by importing it, directly or through others.
    ///
    /// The walk is breadth-first and bounded by `max_depth`, and reports the
    /// fewest hops by which each importer reaches the file. An unbounded walk
    /// through a barrel file reaches most of a package and says little.
    #[must_use]
    pub fn reachable_importers(&self, path: &str, max_depth: u32) -> BTreeMap<String, u32> {
        let mut reached = BTreeMap::new();
        let mut queue = VecDeque::from([(path.to_owned(), 0_u32)]);
        let mut seen = BTreeSet::from([path.to_owned()]);

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for importer in self.importers(&current) {
                if !seen.insert(importer.clone()) {
                    continue;
                }
                reached.insert(importer.clone(), depth + 1);
                queue.push_back((importer.clone(), depth + 1));
            }
        }
        reached
    }

    /// Files whose imports were read only up to the scan limit.
    #[must_use]
    pub fn truncated_files(&self) -> &BTreeSet<String> {
        &self.truncated
    }

    /// Every source file this index covers.
    #[must_use]
    pub fn files(&self) -> &BTreeSet<String> {
        &self.files
    }

    #[must_use]
    pub fn scanned_files(&self) -> u32 {
        u32::try_from(self.files.len()).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn unresolved_specifiers(&self) -> u32 {
        self.unresolved
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outbound.is_empty()
    }
}

impl ImportIndex {
    /// Build an index directly from edges, for tests of code that consumes one.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_edges(edges: &[(&str, &str)], extra_files: &[&str]) -> Self {
        let mut index = Self::default();
        for (importer, imported) in edges {
            index
                .outbound
                .entry((*importer).to_owned())
                .or_default()
                .insert((*imported).to_owned());
            index
                .inbound
                .entry((*imported).to_owned())
                .or_default()
                .insert((*importer).to_owned());
            index.files.insert((*importer).to_owned());
            index.files.insert((*imported).to_owned());
        }
        for file in extra_files {
            index.files.insert((*file).to_owned());
        }
        index
    }

    /// Record one import binding and the path its specifier resolves to, for
    /// tests of cross-file resolution.
    ///
    /// This is what [`index_revision`] records for one statement: the edge,
    /// the binding, and the specifier's answer, which move together.
    #[cfg(test)]
    pub(crate) fn bind(
        &mut self,
        from: &str,
        local: &str,
        imported: ImportedName,
        specifier: &str,
        target: &str,
        line: u32,
    ) {
        self.bindings.entry(from.to_owned()).or_default().insert(
            local.to_owned(),
            ImportBinding {
                imported,
                specifier: specifier.to_owned(),
                line,
            },
        );
        self.targets
            .entry(from.to_owned())
            .or_default()
            .insert(specifier.to_owned(), target.to_owned());
        self.files.insert(from.to_owned());
        self.files.insert(target.to_owned());
        self.add_edge(from, target, line);
    }

    /// Record a binding whose specifier named nothing in this revision, which
    /// is what an installed package looks like.
    #[cfg(test)]
    pub(crate) fn bind_external(
        &mut self,
        from: &str,
        local: &str,
        imported: ImportedName,
        specifier: &str,
        line: u32,
    ) {
        self.bindings.entry(from.to_owned()).or_default().insert(
            local.to_owned(),
            ImportBinding {
                imported,
                specifier: specifier.to_owned(),
                line,
            },
        );
        self.files.insert(from.to_owned());
        self.unresolved += 1;
    }

    /// Record a re-export's resolved specifier without binding a local name.
    #[cfg(test)]
    pub(crate) fn forward(&mut self, from: &str, specifier: &str, target: &str, line: u32) {
        self.targets
            .entry(from.to_owned())
            .or_default()
            .insert(specifier.to_owned(), target.to_owned());
        self.files.insert(from.to_owned());
        self.files.insert(target.to_owned());
        self.add_edge(from, target, line);
    }
}

/// Build the import graph of one commit's tree.
///
/// # Errors
///
/// Returns an error when Git cannot list the tree or read its blobs, or when a
/// supported analyzer cannot be initialized.
pub fn index_revision(
    repository: &Repository,
    commit: &str,
) -> Result<ImportIndex, DiffScopeError> {
    let entries = repository.list_tree(commit)?;
    let paths = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    let aliases = read_path_aliases(repository, &entries);

    let sources = entries
        .iter()
        .filter(|entry| detect_language(std::path::Path::new(&entry.path)).is_some())
        .collect::<Vec<_>>();
    let object_ids = sources
        .iter()
        .map(|entry| entry.object_id.as_str())
        .collect::<BTreeSet<_>>();
    let blobs = repository.load_objects(&object_ids)?;

    let mut scanner = ImportScanner::new()?;
    let mut index = ImportIndex::default();
    for entry in sources {
        let Some(BlobContent::Available(source)) = blobs.get(&entry.object_id) else {
            continue;
        };
        let scan = scanner.scan(std::path::Path::new(&entry.path), source)?;
        index.files.insert(entry.path.clone());
        if scan.truncated {
            index.truncated.insert(entry.path.clone());
        }

        for (specifier, line) in &scan.specifiers {
            match resolve(specifier, &entry.path, &paths, &aliases) {
                Some(target) => {
                    index
                        .targets
                        .entry(entry.path.clone())
                        .or_default()
                        .insert(specifier.clone(), target.clone());
                    index.add_edge(&entry.path, &target, *line);
                }
                // An unresolved specifier names something outside this
                // revision, almost always an installed package. It is counted,
                // not guessed at.
                None => index.unresolved += 1,
            }
        }
        if !scan.bindings.is_empty() {
            index.bindings.insert(entry.path.clone(), scan.bindings);
        }
    }
    Ok(index)
}

/// Re-export hops followed before a lookup gives up.
///
/// A barrel module forwarding through a barrel module is ordinary; a chain
/// longer than this is either generated or circular, and following it further
/// costs a parse per hop for an edge whose confidence is already reduced. The
/// bound also makes a cyclic `export ... from` terminate without a seen set.
const MAX_RE_EXPORT_HOPS: u32 = 4;

/// What one file publishes and what it declares.
///
/// This is the part of a full analysis a cross-file resolver needs, kept per
/// file so that the files a graph resolves through are parsed once per commit
/// rather than once per query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSymbols {
    /// Exported name to what stands behind it.
    pub exports: BTreeMap<String, ExportedSymbol>,
    /// Specifiers this file re-exports from, including `export * from`.
    pub re_exported_modules: BTreeSet<String>,
    /// Every function the file declares, with the calls written in each body.
    pub functions: Vec<FunctionDefinition>,
}

/// One revision's exported symbols, over the files a graph may resolve through.
///
/// Unlike [`ImportIndex`], this is not built over a whole revision: parsing
/// every file whole costs 676 ms on a Vue revision against 381.5 ms for the
/// bounded import prefix, before the function collector that is roughly half
/// of analysis time. A file can only call into a changed module if it imports
/// that module, and the import index already names those files, so the index
/// covers the changed files and their direct importers and grows only when a
/// later query admits a file it does not yet hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolIndex {
    files: BTreeMap<String, FileSymbols>,
}

/// Where an exported name's definition was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedDefinition {
    /// File the definition is written in.
    pub path: String,
    /// Name it is declared under there, which is not always the exported name.
    pub local: String,
    /// Where it is written, so a caller can point at it.
    pub range: SourceRange,
    /// Re-export hops crossed to reach it; zero for a direct export.
    ///
    /// A caller turns this into confidence: each hop is a separate resolution
    /// that could be wrong, so a definition reached through one is reported
    /// less confidently than one the importing module exports itself.
    pub hops: u32,
}

impl SymbolIndex {
    /// What one file publishes, when the index covers that file.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&FileSymbols> {
        self.files.get(path)
    }

    /// Whether this index was built over `path`.
    ///
    /// A path it does not cover was never parsed, which is not the same as a
    /// file that exports nothing: the first is a bound, the second is a fact.
    #[must_use]
    pub fn covers(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Every path this index covers.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The function one module publishes under one exported name.
    ///
    /// Re-exports are followed, using `imports` to resolve each hop's
    /// specifier exactly as the edge built from it was resolved. A name that
    /// is exported but does not name a function in a covered file resolves to
    /// nothing: this stage reports a call it can prove or none at all.
    ///
    /// `export *` is never followed. The names it forwards live in a file the
    /// scan has not read, so choosing one of them would invent an edge.
    #[must_use]
    pub fn definition(
        &self,
        imports: &ImportIndex,
        path: &str,
        exported: &str,
    ) -> Option<ExportedDefinition> {
        let mut path = path.to_owned();
        let mut exported = exported.to_owned();
        for hops in 0..=MAX_RE_EXPORT_HOPS {
            let symbol = self.files.get(&path)?.exports.get(&exported)?;
            let Some(specifier) = symbol.from.as_deref() else {
                return symbol.function.clone().map(|range| ExportedDefinition {
                    path,
                    local: symbol.local.clone(),
                    range,
                    hops,
                });
            };
            // `export * as ns from "x"` publishes the module under a name; the
            // name behind it is `*`, which no module exports.
            let next = imports.resolve_specifier(&path, specifier)?;
            exported = symbol.local.clone();
            next.clone_into(&mut path);
        }
        None
    }

    /// Merge another index's files into this one, keeping what is here.
    ///
    /// A file parsed twice from one commit yields the same record, so the
    /// existing entry is kept rather than replaced; this is how a cached index
    /// grows to cover a comparison that admits files an earlier one did not.
    fn absorb(&mut self, other: Self) {
        for (path, symbols) in other.files {
            let _existing = self.files.entry(path).or_insert(symbols);
        }
    }

    /// Record what one file publishes, for tests of code that consumes an
    /// index.
    #[cfg(test)]
    pub(crate) fn insert(&mut self, path: &str, symbols: FileSymbols) {
        let _replaced = self.files.insert(path.to_owned(), symbols);
    }
}

/// Parse the named files of one commit and record what they publish.
///
/// Only the paths given are read. The caller decides which files the bound
/// admits, because that decision belongs to the graph: it is the changed files
/// and their direct importers, and nothing here knows what changed.
///
/// # Errors
///
/// Returns an error when Git cannot list the tree or read its blobs, or when a
/// supported analyzer cannot be initialized.
pub fn index_symbols(
    repository: &Repository,
    commit: &str,
    paths: &BTreeSet<String>,
) -> Result<SymbolIndex, DiffScopeError> {
    let mut index = SymbolIndex::default();
    if paths.is_empty() {
        return Ok(index);
    }

    let entries = repository.list_tree(commit)?;
    let wanted = entries
        .iter()
        .filter(|entry| paths.contains(&entry.path))
        .filter(|entry| detect_language(std::path::Path::new(&entry.path)).is_some())
        .collect::<Vec<_>>();
    let object_ids = wanted
        .iter()
        .map(|entry| entry.object_id.as_str())
        .collect::<BTreeSet<_>>();
    let blobs = repository.load_objects(&object_ids)?;

    for entry in wanted {
        let Some(BlobContent::Available(source)) = blobs.get(&entry.object_id) else {
            continue;
        };
        let analysis = analyze_source(std::path::Path::new(&entry.path), source)?;
        index.files.insert(
            entry.path.clone(),
            FileSymbols {
                exports: analysis.exported_symbols,
                re_exported_modules: analysis.re_exported_modules,
                functions: analysis.functions,
            },
        );
    }
    Ok(index)
}

/// Extend one commit's symbol index to cover `paths`, parsing only what is
/// missing.
///
/// A symbol index describes a commit, so it is cached per commit; the file set
/// it covers describes a comparison, and two comparisons on one commit admit
/// different files. Extending rather than rebuilding is what lets the cache
/// key stay the commit without a later query silently missing a caller.
///
/// # Errors
///
/// Returns an error when Git cannot list the tree or read its blobs.
pub fn extend_symbols(
    repository: &Repository,
    commit: &str,
    existing: &SymbolIndex,
    paths: &BTreeSet<String>,
) -> Result<SymbolIndex, DiffScopeError> {
    let missing = paths
        .iter()
        .filter(|path| !existing.covers(path))
        .cloned()
        .collect::<BTreeSet<_>>();
    if missing.is_empty() {
        return Ok(existing.clone());
    }
    let mut extended = index_symbols(repository, commit, &missing)?;
    extended.absorb(existing.clone());
    Ok(extended)
}

/// A `compilerOptions.paths` entry: a prefix, and the prefixes it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathAlias {
    pattern: String,
    targets: Vec<String>,
}

/// Resolve one module specifier to a path in this revision.
fn resolve(
    specifier: &str,
    from: &str,
    paths: &BTreeSet<&str>,
    aliases: &[PathAlias],
) -> Option<String> {
    if specifier.starts_with('.') {
        let base = from.rsplit_once('/').map_or("", |(parent, _)| parent);
        return resolve_file(&join(base, specifier), paths);
    }
    for alias in aliases {
        if let Some(candidate) = apply_alias(alias, specifier)
            && let Some(resolved) = resolve_file(&candidate, paths)
        {
            return Some(resolved);
        }
    }
    None
}

/// Substitute a specifier into one `paths` mapping, if the pattern matches.
fn apply_alias(alias: &PathAlias, specifier: &str) -> Option<String> {
    let target = alias.targets.first()?;
    match alias.pattern.split_once('*') {
        None => (alias.pattern == specifier).then(|| target.clone()),
        Some((prefix, suffix)) => {
            let rest = specifier
                .strip_prefix(prefix)?
                .strip_suffix(suffix)
                .filter(|_| specifier.len() >= prefix.len() + suffix.len())?;
            Some(target.replace('*', rest))
        }
    }
}

/// Find the file a path-like specifier names, trying the usual extensions.
fn resolve_file(candidate: &str, paths: &BTreeSet<&str>) -> Option<String> {
    let candidate = normalize(candidate);
    if paths.contains(candidate.as_str()) && candidate.contains('.') {
        return Some(candidate);
    }
    for extension in CANDIDATE_EXTENSIONS {
        let with_extension = format!("{candidate}.{extension}");
        if paths.contains(with_extension.as_str()) {
            return Some(with_extension);
        }
    }
    for entry in DIRECTORY_ENTRIES {
        for extension in CANDIDATE_EXTENSIONS {
            let inside = format!("{candidate}/{entry}.{extension}");
            if paths.contains(inside.as_str()) {
                return Some(inside);
            }
        }
    }
    None
}

/// Join a directory and a relative specifier, resolving `.` and `..`.
fn join(base: &str, specifier: &str) -> String {
    let mut segments = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect::<Vec<_>>()
    };
    for segment in specifier.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                let _removed = segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Strip a leading `./` and any trailing slash from a configured target.
fn normalize(path: &str) -> String {
    path.trim_start_matches("./")
        .trim_end_matches('/')
        .to_owned()
}

/// Read `compilerOptions.paths` from the revision's `tsconfig.json`.
///
/// In a monorepo most cross-package imports are written through these aliases:
/// in a real Vue revision, 18.4% of all specifiers are `@vue/*`, and without
/// the alias map every one of them looks external. The reverse graph would then
/// show packages as uncoupled when they are not.
///
/// A missing, unreadable, or malformed config yields no aliases rather than an
/// error. The index is still correct for relative imports, which are the
/// majority, and a build configuration is not something this tool validates.
fn read_path_aliases(repository: &Repository, entries: &[TreeEntry]) -> Vec<PathAlias> {
    let Some(entry) = entries.iter().find(|entry| entry.path == "tsconfig.json") else {
        return Vec::new();
    };
    let object_ids = BTreeSet::from([entry.object_id.as_str()]);
    let Ok(blobs) = repository.load_objects(&object_ids) else {
        return Vec::new();
    };
    let Some(BlobContent::Available(source)) = blobs.get(&entry.object_id) else {
        return Vec::new();
    };
    let Ok(text) = std::str::from_utf8(source) else {
        return Vec::new();
    };

    let Ok(config) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(text)) else {
        return Vec::new();
    };
    let Some(paths) = config
        .get("compilerOptions")
        .and_then(|options| options.get("paths"))
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };

    let mut aliases = paths
        .iter()
        .filter_map(|(pattern, targets)| {
            let targets = targets
                .as_array()?
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(normalize)
                .collect::<Vec<_>>();
            (!targets.is_empty()).then(|| PathAlias {
                pattern: pattern.clone(),
                targets,
            })
        })
        .collect::<Vec<_>>();

    // Longer patterns first, so `@vue/compat` is preferred over `@vue/*`.
    aliases.sort_by(|left, right| {
        right
            .pattern
            .len()
            .cmp(&left.pattern.len())
            .then_with(|| left.pattern.cmp(&right.pattern))
    });
    aliases
}

/// Remove comments and trailing commas so a `tsconfig.json` parses as JSON.
///
/// TypeScript accepts both, and real configurations use them; a strict JSON
/// parser rejects the file outright. Characters inside strings are left alone,
/// because a `//` in a path is not a comment.
fn strip_jsonc(text: &str) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Code,
        Text,
        LineComment,
        BlockComment,
    }

    let mut output = String::with_capacity(text.len());
    let mut state = State::Code;
    let mut escaped = false;
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        match state {
            State::Code => match character {
                '"' => {
                    state = State::Text;
                    output.push(character);
                }
                '/' if characters.peek() == Some(&'/') => {
                    let _slash = characters.next();
                    state = State::LineComment;
                }
                '/' if characters.peek() == Some(&'*') => {
                    let _star = characters.next();
                    state = State::BlockComment;
                }
                _ => output.push(character),
            },
            State::Text => {
                output.push(character);
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    state = State::Code;
                }
            }
            State::LineComment => {
                if character == '\n' {
                    state = State::Code;
                    output.push(character);
                }
            }
            State::BlockComment => {
                if character == '*' && characters.peek() == Some(&'/') {
                    let _slash = characters.next();
                    state = State::Code;
                }
            }
        }
    }

    remove_trailing_commas(&output)
}

/// Drop commas that directly precede a closing brace or bracket.
fn remove_trailing_commas(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut pending_comma = false;
    let mut in_text = false;
    let mut escaped = false;

    for character in text.chars() {
        if in_text {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_text = false;
            }
            continue;
        }
        match character {
            ',' => {
                if pending_comma {
                    output.push(',');
                }
                pending_comma = true;
                continue;
            }
            '}' | ']' => pending_comma = false,
            character if character.is_whitespace() => {}
            _ => {
                if pending_comma {
                    output.push(',');
                    pending_comma = false;
                }
                if character == '"' {
                    in_text = true;
                }
            }
        }
        if pending_comma && !character.is_whitespace() {
            pending_comma = false;
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{
        FileSymbols, ImportIndex, PathAlias, SymbolIndex, apply_alias, join, resolve, strip_jsonc,
    };
    use crate::languages::{ExportedSymbol, ImportedName, SourceRange};

    fn range(line: u32) -> SourceRange {
        SourceRange {
            start_line: line,
            start_column: 0,
            end_line: line + 2,
            end_column: 1,
        }
    }

    /// A file exporting one name, defined locally as a function.
    fn defines(name: &str, line: u32) -> FileSymbols {
        FileSymbols {
            exports: BTreeMap::from([(
                name.to_owned(),
                ExportedSymbol {
                    local: name.to_owned(),
                    from: None,
                    function: Some(range(line)),
                    line,
                },
            )]),
            ..FileSymbols::default()
        }
    }

    /// A file forwarding one name from another module.
    fn forwards(exported: &str, local: &str, specifier: &str) -> FileSymbols {
        FileSymbols {
            exports: BTreeMap::from([(
                exported.to_owned(),
                ExportedSymbol {
                    local: local.to_owned(),
                    from: Some(specifier.to_owned()),
                    function: None,
                    line: 1,
                },
            )]),
            re_exported_modules: BTreeSet::from([specifier.to_owned()]),
            ..FileSymbols::default()
        }
    }

    fn tree<'a>(paths: &[&'a str]) -> BTreeSet<&'a str> {
        paths.iter().copied().collect()
    }

    #[test]
    fn resolves_relative_specifiers_to_real_files() {
        let paths = tree(&[
            "src/renderer.ts",
            "src/shared/index.ts",
            "src/vnode.tsx",
            "src/legacy.js",
        ]);

        let from = "src/renderer.ts";
        let at = |specifier| resolve(specifier, from, &paths, &[]);

        assert_eq!(at("./vnode"), Some("src/vnode.tsx".to_owned()));
        assert_eq!(at("./shared"), Some("src/shared/index.ts".to_owned()));
        assert_eq!(at("./legacy"), Some("src/legacy.js".to_owned()));
        assert_eq!(
            resolve("../renderer", "src/shared/index.ts", &paths, &[]),
            Some("src/renderer.ts".to_owned())
        );
        // A package this revision does not contain is external, not a guess.
        assert_eq!(at("estree-walker"), None);
        assert_eq!(at("./missing"), None);
    }

    #[test]
    fn resolves_workspace_specifiers_through_tsconfig_aliases() {
        // Without these, 18.4% of a real Vue revision's specifiers look
        // external and every cross-package edge disappears.
        let paths = tree(&[
            "packages/shared/src/index.ts",
            "packages/vue-compat/src/index.ts",
            "packages/runtime-core/src/renderer.ts",
        ]);
        let aliases = vec![
            PathAlias {
                pattern: "@vue/compat".to_owned(),
                targets: vec!["packages/vue-compat/src".to_owned()],
            },
            PathAlias {
                pattern: "@vue/*".to_owned(),
                targets: vec!["packages/*/src".to_owned()],
            },
        ];
        let from = "packages/runtime-core/src/renderer.ts";

        assert_eq!(
            resolve("@vue/shared", from, &paths, &aliases),
            Some("packages/shared/src/index.ts".to_owned())
        );
        // The exact pattern must win over the wildcard.
        assert_eq!(
            resolve("@vue/compat", from, &paths, &aliases),
            Some("packages/vue-compat/src/index.ts".to_owned())
        );
    }

    #[test]
    fn substitutes_only_matching_alias_patterns() {
        let wildcard = PathAlias {
            pattern: "@vue/*".to_owned(),
            targets: vec!["packages/*/src".to_owned()],
        };
        assert_eq!(
            apply_alias(&wildcard, "@vue/shared"),
            Some("packages/shared/src".to_owned())
        );
        assert_eq!(apply_alias(&wildcard, "lodash"), None);

        let exact = PathAlias {
            pattern: "vue".to_owned(),
            targets: vec!["packages/vue/src".to_owned()],
        };
        assert_eq!(
            apply_alias(&exact, "vue"),
            Some("packages/vue/src".to_owned())
        );
        assert_eq!(apply_alias(&exact, "vuex"), None);
    }

    #[test]
    fn joins_relative_paths_through_parent_segments() {
        assert_eq!(join("src/compiler", "./parse"), "src/compiler/parse");
        assert_eq!(
            join("src/compiler", "../runtime/index"),
            "src/runtime/index"
        );
        assert_eq!(join("", "./root"), "root");
    }

    #[test]
    fn reads_tsconfig_with_comments_and_trailing_commas() {
        // TypeScript accepts both and real configurations use them, so a strict
        // JSON parse of a `tsconfig.json` fails outright.
        let text = r#"{
  // a line comment
  "compilerOptions": {
    /* a block comment */
    "paths": {
      "@vue/*": ["./packages/*/src"],
    },
  },
}"#;
        let value: serde_json::Value =
            serde_json::from_str(&strip_jsonc(text)).expect("config parses after stripping");

        assert_eq!(
            value["compilerOptions"]["paths"]["@vue/*"][0],
            "./packages/*/src"
        );
    }

    #[test]
    fn leaves_comment_markers_inside_strings_alone() {
        let text = r#"{"paths":{"http://example.invalid/*":["./a"]}}"#;
        let value: serde_json::Value =
            serde_json::from_str(&strip_jsonc(text)).expect("config parses");

        assert!(value["paths"].get("http://example.invalid/*").is_some());
    }

    #[test]
    fn walks_importers_breadth_first_within_a_depth_limit() {
        let mut index = ImportIndex::default();
        for (importer, imported) in [
            ("app.ts", "middle.ts"),
            ("middle.ts", "core.ts"),
            ("direct.ts", "core.ts"),
        ] {
            index
                .outbound
                .entry(importer.to_owned())
                .or_default()
                .insert(imported.to_owned());
            index
                .inbound
                .entry(imported.to_owned())
                .or_default()
                .insert(importer.to_owned());
        }

        assert_eq!(
            index
                .importers("core.ts")
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["direct.ts".to_owned(), "middle.ts".to_owned()]
        );
        assert_eq!(
            index.reachable_importers("core.ts", 1),
            BTreeMap::from([("direct.ts".to_owned(), 1), ("middle.ts".to_owned(), 1)])
        );
        // `app.ts` only reaches `core.ts` through `middle.ts`.
        assert_eq!(
            index.reachable_importers("core.ts", 2),
            BTreeMap::from([
                ("direct.ts".to_owned(), 1),
                ("middle.ts".to_owned(), 1),
                ("app.ts".to_owned(), 2)
            ])
        );
    }

    #[test]
    fn reports_the_earliest_line_a_resolved_edge_was_seen_at() {
        let mut index = ImportIndex::default();
        index.add_edge("src/app.ts", "src/core.ts", 7);
        index.add_edge("src/app.ts", "src/core.ts", 3);

        assert_eq!(index.import_line("src/app.ts", "src/core.ts"), Some(3));
        // An edge this revision does not have has no site to report, in either
        // direction.
        assert_eq!(index.import_line("src/core.ts", "src/app.ts"), None);
        assert_eq!(index.import_line("src/app.ts", "src/other.ts"), None);
    }

    #[test]
    fn resolves_a_name_the_importing_module_exports_itself() {
        let mut imports = ImportIndex::default();
        imports.bind(
            "src/app.ts",
            "strip",
            ImportedName::Named("stripComments".to_owned()),
            "./parse",
            "src/parse.ts",
            3,
        );
        let mut symbols = SymbolIndex::default();
        symbols.insert("src/parse.ts", defines("stripComments", 12));

        let binding = &imports.bindings("src/app.ts")["strip"];
        let ImportedName::Named(exported) = &binding.imported else {
            panic!("named import");
        };
        let target = imports
            .resolve_specifier("src/app.ts", &binding.specifier)
            .expect("specifier resolves");
        let definition = symbols
            .definition(&imports, target, exported)
            .expect("definition resolves");

        assert_eq!(definition.path, "src/parse.ts");
        assert_eq!(definition.local, "stripComments");
        assert_eq!(definition.range.start_line, 12);
        // Reached without leaving the module the import named.
        assert_eq!(definition.hops, 0);
    }

    #[test]
    fn follows_a_re_export_chain_and_counts_its_hops() {
        let mut imports = ImportIndex::default();
        imports.forward("src/index.ts", "./barrel", "src/barrel.ts", 1);
        imports.forward("src/barrel.ts", "./parse", "src/parse.ts", 1);
        let mut symbols = SymbolIndex::default();
        // `export { stripComments } from "./barrel"`, itself forwarding from
        // the module that declares it.
        symbols.insert(
            "src/index.ts",
            forwards("stripComments", "stripComments", "./barrel"),
        );
        symbols.insert(
            "src/barrel.ts",
            forwards("stripComments", "stripComments", "./parse"),
        );
        symbols.insert("src/parse.ts", defines("stripComments", 40));

        let definition = symbols
            .definition(&imports, "src/index.ts", "stripComments")
            .expect("definition resolves");

        assert_eq!(definition.path, "src/parse.ts");
        assert_eq!(definition.hops, 2);
    }

    #[test]
    fn resolves_a_renamed_export_under_the_name_behind_it() {
        let mut imports = ImportIndex::default();
        imports.forward("src/index.ts", "./parse", "src/parse.ts", 1);
        let mut symbols = SymbolIndex::default();
        symbols.insert(
            "src/index.ts",
            forwards("strip", "stripComments", "./parse"),
        );
        symbols.insert("src/parse.ts", defines("stripComments", 9));

        let definition = symbols
            .definition(&imports, "src/index.ts", "strip")
            .expect("definition resolves");

        assert_eq!(definition.local, "stripComments");
        assert_eq!(definition.hops, 1);
    }

    #[test]
    fn refuses_to_guess_through_a_whole_module_re_export() {
        let mut imports = ImportIndex::default();
        imports.forward("src/index.ts", "./parse", "src/parse.ts", 1);
        let mut symbols = SymbolIndex::default();
        // `export * from "./parse"` records the module it forwards from and no
        // name, because the names it publishes live in a file the scan of the
        // re-exporting module never read.
        symbols.insert(
            "src/index.ts",
            FileSymbols {
                re_exported_modules: BTreeSet::from(["./parse".to_owned()]),
                ..FileSymbols::default()
            },
        );
        symbols.insert("src/parse.ts", defines("stripComments", 9));

        assert_eq!(
            symbols.definition(&imports, "src/index.ts", "stripComments"),
            None
        );
    }

    #[test]
    fn resolves_nothing_through_a_specifier_this_revision_does_not_have() {
        let imports = ImportIndex::default();
        let mut symbols = SymbolIndex::default();
        symbols.insert("src/index.ts", forwards("strip", "strip", "@vue/shared"));

        assert_eq!(symbols.definition(&imports, "src/index.ts", "strip"), None);
    }

    #[test]
    fn resolves_nothing_for_an_exported_name_that_is_not_a_function() {
        let imports = ImportIndex::default();
        let mut symbols = SymbolIndex::default();
        symbols.insert(
            "src/config.ts",
            FileSymbols {
                exports: BTreeMap::from([(
                    "LIMIT".to_owned(),
                    ExportedSymbol {
                        local: "LIMIT".to_owned(),
                        from: None,
                        function: None,
                        line: 2,
                    },
                )]),
                ..FileSymbols::default()
            },
        );

        assert_eq!(symbols.definition(&imports, "src/config.ts", "LIMIT"), None);
    }

    #[test]
    fn stops_following_a_cyclic_re_export_chain() {
        let mut imports = ImportIndex::default();
        imports.forward("src/a.ts", "./b", "src/b.ts", 1);
        imports.forward("src/b.ts", "./a", "src/a.ts", 1);
        let mut symbols = SymbolIndex::default();
        symbols.insert("src/a.ts", forwards("loop", "loop", "./b"));
        symbols.insert("src/b.ts", forwards("loop", "loop", "./a"));

        assert_eq!(symbols.definition(&imports, "src/a.ts", "loop"), None);
    }

    #[test]
    fn covers_only_the_paths_it_was_built_over() {
        let mut symbols = SymbolIndex::default();
        symbols.insert("src/parse.ts", defines("stripComments", 4));

        assert!(symbols.covers("src/parse.ts"));
        // A path outside the bound was never parsed, which is not the same as
        // a file that exports nothing.
        assert!(!symbols.covers("src/untouched.ts"));
        assert_eq!(symbols.paths().collect::<Vec<_>>(), vec!["src/parse.ts"]);
    }
}
