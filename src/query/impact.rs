//! What else a change reaches, and which tests are likely to cover it.
//!
//! These answers come from the import graph of the target revision, so they
//! describe files the diff does not contain. Everything here is measured from
//! resolved edges or from naming convention, and each result says which, so a
//! caller can weigh them separately.

use crate::{
    imports::ImportIndex,
    query::classify::{self, FileClassification},
};

/// Hops followed when measuring how much of a revision reaches a file.
///
/// A barrel module is imported by most of its package, so this number grows
/// quickly and is reported as a coupling measure, not as a list.
const MAX_REACH_DEPTH: u32 = 2;

/// How a test was connected to a changed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestLink {
    /// The test imports the changed file itself.
    Imports,
    /// The test's name follows the repository's convention for this file.
    Convention,
}

impl TestLink {
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Imports => "imports the changed module",
            Self::Convention => "name matches the changed file",
        }
    }

    /// Confidence that this test actually exercises the change.
    ///
    /// A direct import is the strongest evidence short of running the test. A
    /// name match is weaker, because nothing verifies it.
    ///
    /// Indirect imports are deliberately not offered as related tests at all.
    /// Through a package's barrel module almost every test reaches almost every
    /// file: on a real Vue revision a single shared utility is reached by 166
    /// modules within two hops, and the tests that surface are the compiler's,
    /// not the utility's. A list that long is not a suggestion. The reach is
    /// still reported, as a number, by [`FileImpact::nearby_importers`].
    #[must_use]
    pub fn confidence(self) -> f64 {
        match self {
            Self::Imports => 0.9,
            Self::Convention => 0.8,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelatedTest {
    pub file: String,
    pub link: TestLink,
}

/// What a changed file reaches in the revision it was changed into.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileImpact {
    /// Modules importing this file directly.
    pub direct_importers: u32,
    /// Modules reaching this file within [`MAX_REACH_DEPTH`] hops. A large
    /// number usually means the file is re-exported by a barrel module.
    pub nearby_importers: u32,
    pub related_tests: Vec<RelatedTest>,
}

/// Describe what one changed file reaches.
#[must_use]
pub fn for_file(index: &ImportIndex, path: &str) -> FileImpact {
    let reachable = index.reachable_importers(path, MAX_REACH_DEPTH);
    let mut related = Vec::new();

    for importer in index.importers(path) {
        if classify::classify(importer) == FileClassification::Test {
            related.push(RelatedTest {
                file: importer.clone(),
                link: TestLink::Imports,
            });
        }
    }

    for candidate in index.files() {
        if classify::classify(candidate) != FileClassification::Test {
            continue;
        }
        if names_match(path, candidate)
            && !related.iter().any(|existing| &existing.file == candidate)
        {
            related.push(RelatedTest {
                file: candidate.clone(),
                link: TestLink::Convention,
            });
        }
    }

    related.sort_by(|left, right| {
        right
            .link
            .confidence()
            .total_cmp(&left.link.confidence())
            .then_with(|| left.file.cmp(&right.file))
    });

    FileImpact {
        direct_importers: count(index.importers(path).len()),
        nearby_importers: count(reachable.len()),
        related_tests: related,
    }
}

/// Whether a test file is named after a source file.
///
/// `src/renderer.ts` and `__tests__/renderer.spec.ts` share the stem `renderer`
/// once the test suffixes are removed.
fn names_match(source: &str, test: &str) -> bool {
    let source_stem = stem(source);
    !source_stem.is_empty() && source_stem == stem(test)
}

/// A file's name with its extension and any test suffix removed.
fn stem(path: &str) -> &str {
    let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
    name.split_once('.').map_or(name, |(base, _)| base)
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
