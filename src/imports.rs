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
    languages::{ImportScanner, detect_language},
};

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
    /// Files scanned only up to the import scan limit, whose later imports are
    /// therefore not represented.
    truncated: BTreeSet<String>,
    /// Specifiers that named no file in this revision, such as npm packages.
    unresolved: u32,
    /// Files whose imports were read.
    scanned: u32,
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

    #[must_use]
    pub fn scanned_files(&self) -> u32 {
        self.scanned
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
        index.scanned += 1;
        if scan.truncated {
            index.truncated.insert(entry.path.clone());
        }

        for specifier in &scan.specifiers {
            match resolve(specifier, &entry.path, &paths, &aliases) {
                Some(target) => {
                    index
                        .outbound
                        .entry(entry.path.clone())
                        .or_default()
                        .insert(target.clone());
                    index
                        .inbound
                        .entry(target)
                        .or_default()
                        .insert(entry.path.clone());
                }
                // An unresolved specifier names something outside this
                // revision, almost always an installed package. It is counted,
                // not guessed at.
                None => index.unresolved += 1,
            }
        }
    }
    Ok(index)
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
