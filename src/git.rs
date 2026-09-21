use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ffi::OsStr,
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
};

use crate::{
    BlobContent, ChangeInventory, ChangeSummary, DiffHunk, DiffScopeError, FileChange, FileStatus,
    ResolvedRevision,
};

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Clone)]
pub struct Repository {
    root: std::path::PathBuf,
}

#[derive(Debug, Clone)]
struct RawChange {
    old_oid: Option<String>,
    new_oid: Option<String>,
    status: FileStatus,
}

impl Repository {
    /// Open a Git repository from any path inside its work tree.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot resolve the repository root or emits
    /// invalid UTF-8 for the root path.
    pub fn open(path: &Path) -> Result<Self, DiffScopeError> {
        let output = run_git(path, ["rev-parse", "--show-toplevel"])?;
        let root = output.trim_end_matches(['\r', '\n']).into();
        Ok(Self { root })
    }

    /// Build the file-level change inventory between two Git revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when revision resolution, raw diff collection, hunk
    /// parsing, binary detection, or blob loading fails.
    pub fn inventory_changes(
        &self,
        base_revision: &str,
        target_revision: &str,
    ) -> Result<ChangeInventory, DiffScopeError> {
        let base = self.resolve_revision(base_revision)?;
        let target = self.resolve_revision(target_revision)?;
        let mut files = self.raw_changes(&base.commit_id, &target.commit_id)?;
        self.attach_hunks_and_blobs(&base.commit_id, &target.commit_id, &mut files)?;
        files.sort_by(compare_file_changes);

        let summary = files
            .iter()
            .fold(ChangeSummary::default(), |mut summary, file| {
                summary.changed_files += 1;
                summary.added_lines += file.added_lines;
                summary.removed_lines += file.removed_lines;
                if file.status == FileStatus::Binary {
                    summary.binary_files += 1;
                }
                summary
            });

        Ok(ChangeInventory {
            repository_path: self.root.clone(),
            base,
            target,
            files,
            summary,
        })
    }

    fn resolve_revision(&self, revision: &str) -> Result<ResolvedRevision, DiffScopeError> {
        let spec = format!("{revision}^{{commit}}");
        let output = self.git(["rev-parse", "--verify", spec.as_str()])?;
        Ok(ResolvedRevision {
            input: revision.to_owned(),
            commit_id: output.trim_end_matches(['\r', '\n']).to_owned(),
        })
    }

    fn raw_changes(
        &self,
        base_commit: &str,
        target_commit: &str,
    ) -> Result<Vec<FileChange>, DiffScopeError> {
        let output = self.git_bytes([
            "diff",
            "--raw",
            "-z",
            "--find-renames",
            "--no-abbrev",
            base_commit,
            target_commit,
        ])?;
        let records = split_nul(&output);
        let mut index = 0;
        let mut changes = Vec::new();

        while index < records.len() {
            let header = utf8(records[index], "raw diff header")?;
            index += 1;
            if header.is_empty() {
                continue;
            }

            let parsed = parse_raw_header(header)?;
            let base_path = take_path(&records, &mut index, "raw diff path")?;
            let target_path = if parsed.status == FileStatus::Renamed {
                Some(take_path(&records, &mut index, "raw diff rename target")?)
            } else {
                None
            };

            let (base_path, target_path) = match parsed.status {
                FileStatus::Added => (None, Some(base_path)),
                FileStatus::Deleted => (Some(base_path), None),
                FileStatus::Renamed => (Some(base_path), target_path),
                FileStatus::Modified | FileStatus::Binary => {
                    (Some(base_path.clone()), Some(base_path))
                }
            };

            changes.push(FileChange {
                base_path,
                target_path,
                status: parsed.status,
                old_blob_id: parsed.old_oid,
                new_blob_id: parsed.new_oid,
                base_blob: BlobContent::NotApplicable,
                target_blob: BlobContent::NotApplicable,
                added_lines: 0,
                removed_lines: 0,
                hunks: Vec::new(),
            });
        }

        Ok(changes)
    }

    fn attach_hunks_and_blobs(
        &self,
        base_commit: &str,
        target_commit: &str,
        files: &mut [FileChange],
    ) -> Result<(), DiffScopeError> {
        let mut hunks_by_path = self.diff_hunks_by_path(base_commit, target_commit)?;
        let binary_paths = self.binary_paths(base_commit, target_commit)?;

        for file in &mut *files {
            let path = diff_key(file)?.to_owned();

            file.hunks = hunks_by_path.remove(&path).unwrap_or_default();
            file.added_lines = file.hunks.iter().map(|hunk| hunk.added_lines).sum();
            file.removed_lines = file.hunks.iter().map(|hunk| hunk.removed_lines).sum();

            if binary_paths.contains(&path) {
                file.status = FileStatus::Binary;
                file.base_blob = blob_content_for_binary(file.old_blob_id.as_deref());
                file.target_blob = blob_content_for_binary(file.new_blob_id.as_deref());
            }
        }

        let blobs = self.load_blobs(files)?;
        for file in files
            .iter_mut()
            .filter(|file| file.status != FileStatus::Binary)
        {
            file.base_blob = blob_content(file.old_blob_id.as_deref(), &blobs);
            file.target_blob = blob_content(file.new_blob_id.as_deref(), &blobs);
        }
        Ok(())
    }

    /// Collect every file's hunks from one diff of the whole revision range.
    ///
    /// Hunks are keyed by target path, or by base path for deletions, which is
    /// the same key [`diff_key`] derives from a change. Renames are paired by
    /// Git, so a renamed file reports its rename delta rather than a
    /// whole-file addition.
    fn diff_hunks_by_path(
        &self,
        base_commit: &str,
        target_commit: &str,
    ) -> Result<HashMap<String, Vec<DiffHunk>>, DiffScopeError> {
        let output = self.git_lossy([
            "-c",
            "core.quotePath=false",
            "diff",
            "--unified=0",
            "--no-ext-diff",
            "--find-renames",
            base_commit,
            target_commit,
        ])?;
        parse_patch(&output)
    }

    /// Collect the paths Git reports as binary from one numstat of the range.
    ///
    /// `-z` terminates every path with NUL, so paths containing whitespace or
    /// non-ASCII bytes stay unambiguous.
    fn binary_paths(
        &self,
        base_commit: &str,
        target_commit: &str,
    ) -> Result<HashSet<String>, DiffScopeError> {
        let output = self.git_bytes([
            "diff",
            "--numstat",
            "-z",
            "--find-renames",
            base_commit,
            target_commit,
        ])?;
        parse_numstat_binary_paths(&output)
    }

    fn load_blobs(
        &self,
        files: &[FileChange],
    ) -> Result<HashMap<String, BlobContent>, DiffScopeError> {
        let object_ids = files
            .iter()
            .filter(|file| file.status != FileStatus::Binary)
            .flat_map(|file| [file.old_blob_id.as_deref(), file.new_blob_id.as_deref()])
            .flatten()
            .filter(|oid| *oid != ZERO_OID)
            .collect::<BTreeSet<_>>();
        if object_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| DiffScopeError::Git {
                command: "git cat-file --batch".to_owned(),
                message: error.to_string(),
            })?;
        let mut stdin = child.stdin.take().ok_or_else(|| DiffScopeError::Git {
            command: "git cat-file --batch".to_owned(),
            message: "could not open standard input".to_owned(),
        })?;
        let write_object_ids = object_ids.clone();
        let (write_result, output_result) = std::thread::scope(|scope| {
            let writer = scope.spawn(move || -> io::Result<()> {
                for oid in &write_object_ids {
                    writeln!(stdin, "{oid}")?;
                }
                Ok(())
            });
            let output = child.wait_with_output();
            let write = writer.join();
            (write, output)
        });
        let output = output_result.map_err(|error| DiffScopeError::Git {
            command: "git cat-file --batch".to_owned(),
            message: error.to_string(),
        })?;
        if !output.status.success() {
            return Err(DiffScopeError::Git {
                command: "git cat-file --batch".to_owned(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        write_result
            .map_err(|_| DiffScopeError::Git {
                command: "git cat-file --batch".to_owned(),
                message: "standard-input writer terminated unexpectedly".to_owned(),
            })?
            .map_err(|error| DiffScopeError::Git {
                command: "git cat-file --batch".to_owned(),
                message: error.to_string(),
            })?;
        parse_batch_blobs(&output.stdout, &object_ids)
    }

    fn git<const N: usize>(&self, args: [&str; N]) -> Result<String, DiffScopeError> {
        run_git(&self.root, args)
    }

    fn git_lossy<const N: usize>(&self, args: [&str; N]) -> Result<String, DiffScopeError> {
        run_git_lossy(&self.root, args)
    }

    fn git_bytes<const N: usize>(&self, args: [&str; N]) -> Result<Vec<u8>, DiffScopeError> {
        run_git_bytes(&self.root, args)
    }
}

fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String, DiffScopeError> {
    let bytes = run_git_bytes(cwd, args)?;
    String::from_utf8(bytes).map_err(|error| {
        DiffScopeError::InvalidGitOutput(format!("git emitted non-UTF-8 text: {error}"))
    })
}

/// Run Git and decode its output with lossy UTF-8 replacement.
///
/// Diff output embeds content from the analyzed repository, which is untrusted
/// and need not be valid UTF-8. Git writes every field this module parses out
/// of a diff -- hunk headers, line prefixes, and numstat columns -- as ASCII,
/// so replacing invalid sequences inside content preserves those fields while
/// keeping one malformed file from failing the whole analysis.
fn run_git_lossy<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String, DiffScopeError> {
    let bytes = run_git_bytes(cwd, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn run_git_bytes<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<Vec<u8>, DiffScopeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args.map(OsStr::new))
        .output()
        .map_err(|error| DiffScopeError::Git {
            command: "git".to_owned(),
            message: error.to_string(),
        })?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(DiffScopeError::Git {
            command: format!("git {}", args.join(" ")),
            message,
        })
    }
}

/// The key a change is looked up by: target path, or base path for deletions.
fn diff_key(file: &FileChange) -> Result<&str, DiffScopeError> {
    file.target_path
        .as_deref()
        .or(file.base_path.as_deref())
        .ok_or_else(|| DiffScopeError::InvalidGitOutput("file change without any path".to_owned()))
}

fn split_nul(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect()
}

fn utf8<'a>(bytes: &'a [u8], context: &str) -> Result<&'a str, DiffScopeError> {
    std::str::from_utf8(bytes).map_err(|error| {
        DiffScopeError::InvalidGitOutput(format!("{context} is not UTF-8: {error}"))
    })
}

fn take_path(
    records: &[&[u8]],
    index: &mut usize,
    context: &str,
) -> Result<String, DiffScopeError> {
    let bytes = records
        .get(*index)
        .ok_or_else(|| DiffScopeError::InvalidGitOutput(format!("missing {context}")))?;
    *index += 1;
    Ok(utf8(bytes, context)?.to_owned())
}

fn parse_raw_header(header: &str) -> Result<RawChange, DiffScopeError> {
    let mut parts = header.split_whitespace();
    let _modes = (parts.next(), parts.next());
    let old_oid = normalized_oid(parts.next());
    let new_oid = normalized_oid(parts.next());
    let status_text = parts
        .next()
        .ok_or_else(|| DiffScopeError::InvalidGitOutput(format!("missing status in {header:?}")))?;
    let status = match status_text.as_bytes().first().copied() {
        Some(b'A') => FileStatus::Added,
        Some(b'D') => FileStatus::Deleted,
        Some(b'M') => FileStatus::Modified,
        Some(b'R') => FileStatus::Renamed,
        other => {
            return Err(DiffScopeError::InvalidGitOutput(format!(
                "unsupported raw diff status {other:?} in {header:?}"
            )));
        }
    };
    Ok(RawChange {
        old_oid,
        new_oid,
        status,
    })
}

fn normalized_oid(value: Option<&str>) -> Option<String> {
    match value {
        Some(oid) if oid != ZERO_OID => Some(oid.to_owned()),
        _ => None,
    }
}

fn parse_patch(diff: &str) -> Result<HashMap<String, Vec<DiffHunk>>, DiffScopeError> {
    let mut by_path: HashMap<String, Vec<DiffHunk>> = HashMap::new();
    let mut base_path: Option<String> = None;
    let mut path: Option<String> = None;
    let mut current: Option<DiffHunk> = None;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            finish_hunk(&mut current, path.as_deref(), &mut by_path);
            base_path = None;
            path = None;
        } else if line.starts_with("@@ ") {
            finish_hunk(&mut current, path.as_deref(), &mut by_path);
            current = Some(parse_hunk_header(line)?);
        } else if current.is_none() {
            // File headers only appear before the first hunk of a file, so a
            // removed line such as `--- three dashes` is never mistaken for one.
            if let Some(rest) = line.strip_prefix("--- ") {
                base_path = patch_path(rest, "a/");
            } else if let Some(rest) = line.strip_prefix("+++ ") {
                path = patch_path(rest, "b/").or_else(|| base_path.clone());
            }
        } else if let Some(hunk) = current.as_mut() {
            if line.starts_with('+') {
                hunk.added_lines += 1;
            } else if line.starts_with('-') {
                hunk.removed_lines += 1;
            }
        }
    }

    finish_hunk(&mut current, path.as_deref(), &mut by_path);
    Ok(by_path)
}

fn finish_hunk(
    current: &mut Option<DiffHunk>,
    path: Option<&str>,
    by_path: &mut HashMap<String, Vec<DiffHunk>>,
) {
    if let (Some(hunk), Some(path)) = (current.take(), path) {
        by_path.entry(path.to_owned()).or_default().push(hunk);
    }
}

fn patch_path(text: &str, prefix: &str) -> Option<String> {
    if text == "/dev/null" {
        return None;
    }
    Some(text.strip_prefix(prefix).unwrap_or(text).to_owned())
}

fn parse_numstat_binary_paths(output: &[u8]) -> Result<HashSet<String>, DiffScopeError> {
    let records = split_nul(output);
    let mut binary_paths = HashSet::new();
    let mut index = 0;

    while index < records.len() {
        let record = utf8(records[index], "numstat record")?;
        index += 1;
        let mut fields = record.splitn(3, '\t');
        let added = fields.next().unwrap_or_default();
        let removed = fields.next().unwrap_or_default();
        let Some(inline_path) = fields.next() else {
            continue;
        };

        // A rename leaves the path field empty and follows with the base path
        // and the target path as their own NUL-terminated records.
        let path = if inline_path.is_empty() {
            let _base_path = take_path(&records, &mut index, "numstat rename base path")?;
            take_path(&records, &mut index, "numstat rename target path")?
        } else {
            inline_path.to_owned()
        };

        if added == "-" && removed == "-" {
            binary_paths.insert(path);
        }
    }

    Ok(binary_paths)
}

fn parse_hunk_header(line: &str) -> Result<DiffHunk, DiffScopeError> {
    let mut parts = line.split_whitespace();
    let _marker = parts.next();
    let base = parts.next().ok_or_else(|| {
        DiffScopeError::InvalidGitOutput(format!("missing base range in {line:?}"))
    })?;
    let target = parts.next().ok_or_else(|| {
        DiffScopeError::InvalidGitOutput(format!("missing target range in {line:?}"))
    })?;
    let (base_start, base_count) = parse_range(base, '-')?;
    let (target_start, target_count) = parse_range(target, '+')?;
    Ok(DiffHunk {
        base_start,
        base_count,
        target_start,
        target_count,
        added_lines: 0,
        removed_lines: 0,
    })
}

fn parse_range(text: &str, prefix: char) -> Result<(u32, u32), DiffScopeError> {
    let range = text.strip_prefix(prefix).ok_or_else(|| {
        DiffScopeError::InvalidGitOutput(format!("range {text:?} missing prefix {prefix:?}"))
    })?;
    let mut parts = range.split(',');
    let start = parse_u32(parts.next(), text)?;
    let count = parts
        .next()
        .map_or(Ok(1), |part| parse_u32(Some(part), text))?;
    Ok((start, count))
}

fn parse_u32(value: Option<&str>, context: &str) -> Result<u32, DiffScopeError> {
    value
        .ok_or_else(|| DiffScopeError::InvalidGitOutput(format!("missing integer in {context:?}")))?
        .parse()
        .map_err(|error| {
            DiffScopeError::InvalidGitOutput(format!("invalid integer in {context:?}: {error}"))
        })
}

fn parse_batch_blobs(
    output: &[u8],
    object_ids: &BTreeSet<&str>,
) -> Result<HashMap<String, BlobContent>, DiffScopeError> {
    let mut blobs = HashMap::with_capacity(object_ids.len());
    let mut offset = 0;
    for expected_oid in object_ids {
        let header_end = output
            .get(offset..)
            .and_then(|remaining| remaining.iter().position(|byte| *byte == b'\n'))
            .map(|position| offset + position)
            .ok_or_else(|| {
                DiffScopeError::InvalidGitOutput("truncated cat-file batch header".to_owned())
            })?;
        let header = utf8(&output[offset..header_end], "cat-file batch header")?;
        offset = header_end + 1;
        let mut fields = header.split_whitespace();
        let oid = fields.next().ok_or_else(|| {
            DiffScopeError::InvalidGitOutput("cat-file batch header has no object id".to_owned())
        })?;
        if oid != *expected_oid {
            return Err(DiffScopeError::InvalidGitOutput(format!(
                "cat-file returned object {oid}, expected {expected_oid}"
            )));
        }
        let object_type = fields.next().ok_or_else(|| {
            DiffScopeError::InvalidGitOutput("cat-file batch header has no type".to_owned())
        })?;
        if object_type == "missing" {
            blobs.insert(oid.to_owned(), BlobContent::Missing);
            continue;
        }
        if object_type != "blob" {
            return Err(DiffScopeError::InvalidGitOutput(format!(
                "cat-file returned unsupported object type {object_type:?}"
            )));
        }
        let size = fields
            .next()
            .ok_or_else(|| {
                DiffScopeError::InvalidGitOutput("cat-file batch header has no size".to_owned())
            })?
            .parse::<usize>()
            .map_err(|error| {
                DiffScopeError::InvalidGitOutput(format!(
                    "invalid cat-file batch object size: {error}"
                ))
            })?;
        let content_end = offset.checked_add(size).ok_or_else(|| {
            DiffScopeError::InvalidGitOutput("cat-file batch object size overflow".to_owned())
        })?;
        let content = output.get(offset..content_end).ok_or_else(|| {
            DiffScopeError::InvalidGitOutput("truncated cat-file batch object".to_owned())
        })?;
        if output.get(content_end) != Some(&b'\n') {
            return Err(DiffScopeError::InvalidGitOutput(
                "cat-file batch object has no terminator".to_owned(),
            ));
        }
        blobs.insert(oid.to_owned(), BlobContent::Available(content.to_vec()));
        offset = content_end + 1;
    }
    Ok(blobs)
}

fn blob_content(oid: Option<&str>, blobs: &HashMap<String, BlobContent>) -> BlobContent {
    oid.map_or(BlobContent::NotApplicable, |oid| {
        blobs.get(oid).cloned().unwrap_or(BlobContent::Missing)
    })
}

fn blob_content_for_binary(oid: Option<&str>) -> BlobContent {
    if oid.is_some() {
        BlobContent::Binary
    } else {
        BlobContent::NotApplicable
    }
}

fn compare_file_changes(left: &FileChange, right: &FileChange) -> std::cmp::Ordering {
    let left_key = left.target_path.as_ref().or(left.base_path.as_ref());
    let right_key = right.target_path.as_ref().or(right.base_path.as_ref());
    left_key
        .cmp(&right_key)
        .then_with(|| left.base_path.cmp(&right.base_path))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::BlobContent;

    use super::{parse_batch_blobs, parse_hunk_header, parse_numstat_binary_paths, parse_patch};

    #[test]
    fn parses_hunk_header_with_counts() {
        let hunk = parse_hunk_header("@@ -10,2 +20,3 @@ fn main").expect("hunk parses");
        assert_eq!(hunk.base_start, 10);
        assert_eq!(hunk.base_count, 2);
        assert_eq!(hunk.target_start, 20);
        assert_eq!(hunk.target_count, 3);
    }

    #[test]
    fn counts_changed_lines_inside_hunks() {
        let by_path =
            parse_patch("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1,2 @@\n-old\n+new\n+more\n")
                .expect("diff parses");
        let hunks = by_path.get("a").expect("file a has hunks");

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].removed_lines, 1);
        assert_eq!(hunks[0].added_lines, 2);
    }

    #[test]
    fn separates_hunks_of_every_file_in_one_patch() {
        let by_path = parse_patch(concat!(
            "diff --git a/one.ts b/one.ts\n--- a/one.ts\n+++ b/one.ts\n",
            "@@ -1 +1 @@\n-a\n+b\n",
            "diff --git a/gone.ts b/gone.ts\n--- a/gone.ts\n+++ /dev/null\n",
            "@@ -1,2 +0,0 @@\n-a\n-b\n",
            "diff --git a/before.ts b/after.ts\n--- a/before.ts\n+++ b/after.ts\n",
            "@@ -2 +2 @@\n-old\n+new\n",
        ))
        .expect("diff parses");

        assert_eq!(by_path.len(), 3);
        assert_eq!(by_path["one.ts"].len(), 1);
        assert_eq!(by_path["gone.ts"][0].removed_lines, 2);
        assert_eq!(by_path["after.ts"][0].added_lines, 1);
        assert_eq!(by_path["after.ts"][0].base_start, 2);
    }

    #[test]
    fn keeps_content_lines_that_look_like_file_headers() {
        let by_path = parse_patch(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n--- not a header\n+++ also not\n",
        )
        .expect("diff parses");
        let hunks = by_path.get("a").expect("file a has hunks");

        assert_eq!(hunks[0].removed_lines, 1);
        assert_eq!(hunks[0].added_lines, 1);
    }

    #[test]
    fn reads_binary_paths_including_renames_from_numstat() {
        let binary_paths = parse_numstat_binary_paths(
            b"1\t1\ttext.ts\0-\t-\timage.bin\0-\t-\t\0old.bin\0new.bin\0",
        )
        .expect("numstat parses");

        assert!(binary_paths.contains("image.bin"));
        assert!(binary_paths.contains("new.bin"));
        assert!(!binary_paths.contains("text.ts"));
        assert_eq!(binary_paths.len(), 2);
    }

    #[test]
    fn parses_available_and_missing_batch_blobs() {
        let object_ids = BTreeSet::from(["aaaa", "bbbb"]);
        let blobs = parse_batch_blobs(b"aaaa blob 4\nx\0y\n\nbbbb missing\n", &object_ids)
            .expect("batch output parses");

        assert_eq!(
            blobs.get("aaaa"),
            Some(&BlobContent::Available(b"x\0y\n".to_vec()))
        );
        assert_eq!(blobs.get("bbbb"), Some(&BlobContent::Missing));
    }
}
