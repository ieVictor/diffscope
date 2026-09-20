use std::{ffi::OsStr, path::Path, process::Command};

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
        for file in files {
            let path = file
                .target_path
                .as_ref()
                .or(file.base_path.as_ref())
                .ok_or_else(|| {
                    DiffScopeError::InvalidGitOutput("file change without any path".to_owned())
                })?;

            file.hunks = self.diff_hunks(base_commit, target_commit, path)?;
            file.added_lines = file.hunks.iter().map(|hunk| hunk.added_lines).sum();
            file.removed_lines = file.hunks.iter().map(|hunk| hunk.removed_lines).sum();

            if self.is_binary(base_commit, target_commit, path)? {
                file.status = FileStatus::Binary;
                file.base_blob = blob_content_for_binary(file.old_blob_id.as_deref());
                file.target_blob = blob_content_for_binary(file.new_blob_id.as_deref());
                continue;
            }

            file.base_blob = self.load_blob(file.old_blob_id.as_deref())?;
            file.target_blob = self.load_blob(file.new_blob_id.as_deref())?;
        }
        Ok(())
    }

    fn diff_hunks(
        &self,
        base_commit: &str,
        target_commit: &str,
        path: &str,
    ) -> Result<Vec<DiffHunk>, DiffScopeError> {
        let output = self.git([
            "diff",
            "--unified=0",
            "--no-ext-diff",
            base_commit,
            target_commit,
            "--",
            path,
        ])?;
        parse_hunks(&output)
    }

    fn is_binary(
        &self,
        base_commit: &str,
        target_commit: &str,
        path: &str,
    ) -> Result<bool, DiffScopeError> {
        let output = self.git(["diff", "--numstat", base_commit, target_commit, "--", path])?;
        Ok(output.lines().any(|line| line.starts_with("-\t-\t")))
    }

    fn load_blob(&self, oid: Option<&str>) -> Result<BlobContent, DiffScopeError> {
        let Some(oid) = oid else {
            return Ok(BlobContent::NotApplicable);
        };
        if oid == ZERO_OID {
            return Ok(BlobContent::NotApplicable);
        }
        let bytes = self.git_bytes(["cat-file", "-p", oid])?;
        Ok(BlobContent::Available(bytes))
    }

    fn git<const N: usize>(&self, args: [&str; N]) -> Result<String, DiffScopeError> {
        run_git(&self.root, args)
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

fn parse_hunks(diff: &str) -> Result<Vec<DiffHunk>, DiffScopeError> {
    let mut hunks = Vec::new();
    let mut current: Option<DiffHunk> = None;

    for line in diff.lines() {
        if line.starts_with("@@ ") {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(parse_hunk_header(line)?);
        } else if let Some(hunk) = current.as_mut() {
            if line.starts_with('+') && !line.starts_with("+++") {
                hunk.added_lines += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                hunk.removed_lines += 1;
            }
        }
    }

    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    Ok(hunks)
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
    use super::{parse_hunk_header, parse_hunks};

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
        let hunks = parse_hunks("diff --git a/a b/a\n@@ -1 +1,2 @@\n-old\n+new\n+more\n")
            .expect("diff parses");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].removed_lines, 1);
        assert_eq!(hunks[0].added_lines, 2);
    }
}
