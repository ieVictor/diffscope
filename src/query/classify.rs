//! Classify a repository path by the role its file plays.
//!
//! Classification is derived from the path alone: it never touches the
//! filesystem, never depends on file contents, and is therefore identical for
//! the same path on every machine. Rules are ordered and the first match wins,
//! so a snapshot under `__tests__` is reported as generated rather than as a
//! test that someone is expected to read.
//!
//! These are heuristics over naming conventions, not facts about the project.
//! Every result carries the classification it was given, so a caller that
//! disagrees can rank on the underlying numbers instead.

/// The role a changed file plays, in the order the rules are evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileClassification {
    /// A dependency lock file. Large, machine-written, rarely reviewed by hand.
    Lockfile,
    /// Code vendored from another project.
    Vendored,
    /// Build output, snapshots, and other machine-written files.
    Generated,
    /// Tests, fixtures, and mocks.
    Test,
    /// Build, tooling, and CI configuration.
    Config,
    /// Prose documentation.
    Docs,
    /// Everything else: the code the change is actually about.
    Source,
}

impl FileClassification {
    /// Whether this file is the project's own production code.
    #[must_use]
    pub fn is_source(self) -> bool {
        self == Self::Source
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lockfile => "lockfile",
            Self::Vendored => "vendored",
            Self::Generated => "generated",
            Self::Test => "test",
            Self::Config => "config",
            Self::Docs => "docs",
            Self::Source => "source",
        }
    }

    /// Parse a classification name as a caller supplies it in a query.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "lockfile" => Some(Self::Lockfile),
            "vendored" => Some(Self::Vendored),
            "generated" => Some(Self::Generated),
            "test" => Some(Self::Test),
            "config" => Some(Self::Config),
            "docs" => Some(Self::Docs),
            "source" => Some(Self::Source),
            _ => None,
        }
    }
}

/// Lock files, matched by exact file name.
const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "Gemfile.lock",
    "bun.lockb",
    "composer.lock",
    "go.sum",
    "package-lock.json",
    "pnpm-lock.yaml",
    "poetry.lock",
    "yarn.lock",
];

/// Directory names that mark code owned by another project.
const VENDORED_SEGMENTS: &[&str] = &["node_modules", "third_party", "thirdparty", "vendor"];

/// Directory names whose contents are machine-written.
///
/// Deliberately excludes `build`, `out`, and `target`, which are common enough
/// as ordinary source directory names that matching them anywhere in a path
/// would misreport real source files.
const GENERATED_SEGMENTS: &[&str] = &[".next", "__snapshots__", "coverage", "dist", "generated"];

/// Directory names that mark tests, fixtures, and mocks.
const TEST_SEGMENTS: &[&str] = &[
    "__mocks__",
    "__tests__",
    "e2e",
    "spec",
    "test",
    "tests",
    "testing",
];

/// File-name infixes that mark a single test file.
const TEST_INFIXES: &[&str] = &[".spec.", ".test.", "_test.", "-test.", ".test-d."];

/// Configuration file names, matched exactly.
const CONFIG_FILES: &[&str] = &[
    "Dockerfile",
    "Justfile",
    "Makefile",
    "package.json",
    "renovate.json",
];

/// Rules, in evaluation order, as the documentation describes them.
#[cfg(test)]
const DOCUMENTED_ORDER: &[FileClassification] = &[
    FileClassification::Lockfile,
    FileClassification::Vendored,
    FileClassification::Generated,
    FileClassification::Test,
    FileClassification::Config,
    FileClassification::Docs,
    FileClassification::Source,
];

/// Documentation file extensions.
const DOCS_EXTENSIONS: &[&str] = &["adoc", "md", "mdx", "rst", "txt"];

/// Configuration file extensions, checked only for dotfiles and `*.config.*`.
const CONFIG_EXTENSIONS: &[&str] = &["cfg", "ini", "toml", "yaml", "yml"];

/// Decide what role a repository-relative path plays.
#[must_use]
pub fn classify(path: &str) -> FileClassification {
    let segments = path.split('/').collect::<Vec<_>>();
    let name = segments.last().copied().unwrap_or(path);
    let directories = &segments[..segments.len().saturating_sub(1)];

    if LOCKFILES.contains(&name) {
        return FileClassification::Lockfile;
    }
    if contains_segment(directories, VENDORED_SEGMENTS) {
        return FileClassification::Vendored;
    }
    if contains_segment(directories, GENERATED_SEGMENTS)
        || extension(name) == Some("snap")
        || has_infix(name, &[".min.", ".generated.", ".g."])
    {
        return FileClassification::Generated;
    }
    if contains_segment(directories, TEST_SEGMENTS)
        || has_infix(name, TEST_INFIXES)
        || name.starts_with("test_")
    {
        return FileClassification::Test;
    }
    if is_config(name, directories) {
        return FileClassification::Config;
    }
    if extension(name).is_some_and(|extension| DOCS_EXTENSIONS.contains(&extension)) {
        return FileClassification::Docs;
    }
    FileClassification::Source
}

fn is_config(name: &str, directories: &[&str]) -> bool {
    if CONFIG_FILES.contains(&name) || name.starts_with('.') {
        return true;
    }
    // A dotted directory holds tooling: `.github`, `.husky`, `.vscode`. The
    // files inside it are rarely named like configuration themselves.
    if directories.iter().any(|segment| segment.starts_with('.')) {
        return true;
    }
    if name.starts_with("tsconfig") && extension(name) == Some("json") {
        return true;
    }
    if has_infix(name, &[".config."]) {
        return true;
    }
    extension(name).is_some_and(|extension| CONFIG_EXTENSIONS.contains(&extension))
}

fn contains_segment(directories: &[&str], candidates: &[&str]) -> bool {
    directories
        .iter()
        .any(|segment| candidates.contains(segment))
}

fn has_infix(name: &str, infixes: &[&str]) -> bool {
    infixes.iter().any(|infix| name.contains(infix))
}

fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.').map(|(_, extension)| extension)
}

#[cfg(test)]
mod tests {
    use super::{DOCUMENTED_ORDER, FileClassification, classify};

    #[test]
    fn classifies_each_documented_role() {
        for (path, expected) in [
            ("pnpm-lock.yaml", FileClassification::Lockfile),
            ("node_modules/vue/index.js", FileClassification::Vendored),
            ("packages/vue/dist/vue.js", FileClassification::Generated),
            (
                "packages/compiler-sfc/__tests__/compileScript.spec.ts",
                FileClassification::Test,
            ),
            (".github/workflows/ci.yml", FileClassification::Config),
            ("CHANGELOG.md", FileClassification::Docs),
            (
                "packages/runtime-core/src/renderer.ts",
                FileClassification::Source,
            ),
        ] {
            assert_eq!(classify(path), expected, "{path}");
        }
    }

    #[test]
    fn resolves_overlapping_roles_in_documented_order() {
        // A snapshot lives under `__tests__` but is machine-written, and the
        // earlier rule must win so it is not offered up as a test to read.
        assert_eq!(
            classify("packages/compiler-sfc/__tests__/__snapshots__/compileScript.spec.ts.snap"),
            FileClassification::Generated
        );
        // A lock file inside a vendored tree is still a lock file.
        assert_eq!(
            classify("vendor/project/Cargo.lock"),
            FileClassification::Lockfile
        );
        // Declaration tests are tests, not type declarations.
        assert_eq!(
            classify("packages-private/dts-test/setupHelpers.test-d.ts"),
            FileClassification::Test
        );
    }

    #[test]
    fn treats_tooling_directories_as_configuration() {
        assert_eq!(
            classify(".vite-hooks/_/commit-msg"),
            FileClassification::Config
        );
        assert_eq!(classify(".gitignore"), FileClassification::Config);
        assert_eq!(classify("tsconfig.build.json"), FileClassification::Config);
    }

    #[test]
    fn keeps_source_directories_that_merely_look_generated() {
        // `build` and `out` are excluded from the generated rule precisely
        // because they are ordinary source directory names.
        assert_eq!(
            classify("packages/compiler/src/build/emitter.ts"),
            FileClassification::Source
        );
        assert!(!classify("src/testing/harness.ts").is_source());
    }

    #[test]
    fn round_trips_every_classification_name() {
        for classification in DOCUMENTED_ORDER {
            assert_eq!(
                FileClassification::parse(classification.as_str()),
                Some(*classification)
            );
        }
        assert_eq!(FileClassification::parse("nonsense"), None);
    }
}
