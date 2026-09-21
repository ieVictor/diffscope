use std::{fmt::Write as _, fs, path::Path, process::Command};

use diffscope::{
    AnalysisRequest, BlobContent, DiagnosticCode, FileStatus, analysis::FunctionChangeStatus,
    analyze, inventory_changes,
};

#[test]
fn inventories_added_modified_deleted_renamed_and_binary_files() {
    let repo = TestRepo::new("inventory_statuses");
    repo.git(["init"]);
    repo.git(["config", "user.email", "diffscope@example.invalid"]);
    repo.git(["config", "user.name", "DiffScope"]);

    repo.write("modified.txt", "one\ntwo\n");
    repo.write("deleted.txt", "remove me\n");
    repo.write("old-name.txt", "rename me\n");
    repo.write_bytes("image.bin", &[0, 159, 146, 150]);
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "base"]);
    let base = repo.rev_parse("HEAD");

    repo.write("modified.txt", "one\ntwo changed\nthree\n");
    fs::remove_file(repo.path().join("deleted.txt")).expect("delete fixture file");
    fs::rename(
        repo.path().join("old-name.txt"),
        repo.path().join("new-name.txt"),
    )
    .expect("rename fixture file");
    repo.write("added.txt", "brand new\n");
    repo.write_bytes("image.bin", &[0, 159, 146, 151]);
    repo.git(["add", "-A"]);
    repo.git(["commit", "-m", "target"]);
    let target = repo.rev_parse("HEAD");

    let inventory = inventory_changes(&AnalysisRequest {
        repository_path: repo.path().to_path_buf(),
        base_revision: base,
        target_revision: target,
    })
    .expect("inventory succeeds");

    assert_eq!(inventory.summary.changed_files, 5);
    assert_eq!(inventory.summary.binary_files, 1);

    let added = find(&inventory.files, "added.txt");
    assert_eq!(added.status, FileStatus::Added);
    assert_eq!(added.added_lines, 1);
    assert!(matches!(added.base_blob, BlobContent::NotApplicable));
    assert!(matches!(added.target_blob, BlobContent::Available(_)));

    let deleted = find(&inventory.files, "deleted.txt");
    assert_eq!(deleted.status, FileStatus::Deleted);
    assert_eq!(deleted.removed_lines, 1);

    let modified = find(&inventory.files, "modified.txt");
    assert_eq!(modified.status, FileStatus::Modified);
    assert_eq!(modified.added_lines, 2);
    assert_eq!(modified.removed_lines, 1);
    assert_eq!(modified.hunks.len(), 1);

    let renamed = find(&inventory.files, "new-name.txt");
    assert_eq!(renamed.status, FileStatus::Renamed);
    assert_eq!(renamed.base_path.as_deref(), Some("old-name.txt"));
    assert_eq!(renamed.target_path.as_deref(), Some("new-name.txt"));

    let binary = find(&inventory.files, "image.bin");
    assert_eq!(binary.status, FileStatus::Binary);
    assert!(matches!(binary.base_blob, BlobContent::Binary));
    assert!(matches!(binary.target_blob, BlobContent::Binary));
}

#[test]
fn renamed_files_report_the_rename_delta_and_keep_untouched_functions_unchanged() {
    let repo = TestRepo::new("rename_delta");
    repo.git(["init"]);
    repo.git(["config", "user.email", "diffscope@example.invalid"]);
    repo.git(["config", "user.name", "DiffScope"]);

    repo.write(
        "before.ts",
        "export function untouched(value: number): number {\n  return value * 3;\n}\n\nexport function edited(value: number): number {\n  return value + 1;\n}\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "base"]);
    let base = repo.rev_parse("HEAD");

    fs::remove_file(repo.path().join("before.ts")).expect("remove renamed fixture source");
    repo.write(
        "after.ts",
        "export function untouched(value: number): number {\n  return value * 3;\n}\n\nexport function edited(value: number): number {\n  return value + 2;\n}\n",
    );
    repo.git(["add", "-A"]);
    repo.git(["commit", "-m", "target"]);
    let target = repo.rev_parse("HEAD");

    let request = AnalysisRequest {
        repository_path: repo.path().to_path_buf(),
        base_revision: base,
        target_revision: target,
    };

    let inventory = inventory_changes(&request).expect("inventory succeeds");
    let renamed = find(&inventory.files, "after.ts");
    assert_eq!(renamed.status, FileStatus::Renamed);
    assert_eq!(renamed.base_path.as_deref(), Some("before.ts"));
    assert_eq!(renamed.added_lines, 1);
    assert_eq!(renamed.removed_lines, 1);
    assert_eq!(renamed.hunks.len(), 1);
    assert_eq!(inventory.summary.added_lines, 1);
    assert_eq!(inventory.summary.removed_lines, 1);

    let result = analyze(&request).expect("analysis succeeds");
    let file = result
        .files
        .iter()
        .find(|file| file.target_path.as_deref() == Some("after.ts"))
        .expect("renamed file is analyzed");
    let status_of = |name: &str| {
        file.functions
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
fn non_utf8_file_content_reports_a_diagnostic_instead_of_failing() {
    let repo = TestRepo::new("non_utf8_content");
    repo.git(["init"]);
    repo.git(["config", "user.email", "diffscope@example.invalid"]);
    repo.git(["config", "user.name", "DiffScope"]);

    repo.write(
        "kept.ts",
        "export function kept(): number {\n  return 1;\n}\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "base"]);
    let base = repo.rev_parse("HEAD");

    let mut latin1 = b"export function latin(): string {\n  return \"".to_vec();
    latin1.extend_from_slice(&[0xe9]);
    latin1.extend_from_slice(b"\";\n}\n");
    repo.write_bytes("latin1.ts", &latin1);
    repo.git(["add", "-A"]);
    repo.git(["commit", "-m", "target"]);
    let target = repo.rev_parse("HEAD");

    let request = AnalysisRequest {
        repository_path: repo.path().to_path_buf(),
        base_revision: base,
        target_revision: target,
    };

    let inventory =
        inventory_changes(&request).expect("inventory succeeds despite non-UTF-8 content");
    let file = find(&inventory.files, "latin1.ts");
    assert_eq!(file.status, FileStatus::Added);
    assert_eq!(file.added_lines, 3);

    let result = analyze(&request).expect("analysis succeeds despite non-UTF-8 content");
    let analyzed = result
        .files
        .iter()
        .find(|file| file.target_path.as_deref() == Some("latin1.ts"))
        .expect("non-UTF-8 file is inventoried");
    assert!(
        analyzed
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::InvalidUtf8),
        "expected an invalid_utf8 diagnostic, got {:?}",
        analyzed.diagnostics
    );
}

#[test]
fn parallel_analysis_preserves_file_and_function_order() {
    let repo = TestRepo::new("parallel_order");
    repo.git(["init"]);
    repo.git(["config", "user.email", "diffscope@example.invalid"]);
    repo.git(["config", "user.name", "DiffScope"]);

    // Enough files to occupy every worker, in an order Git does not report
    // alphabetically by accident, and one much larger file so that workers
    // finish at different times.
    for index in 0..40 {
        let body = if index == 17 { 400 } else { 1 };
        let mut source = String::new();
        for line in 0..body {
            writeln!(
                source,
                "export function file{index}fn{line}(value: number): number {{\n  if (value > {line}) {{\n    return value;\n  }}\n  return {line};\n}}"
            )
            .expect("write fixture source");
        }
        repo.write(&format!("src/file{index:02}.ts"), &source);
    }
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "base"]);
    let base = repo.rev_parse("HEAD");

    for index in 0..40 {
        repo.write(
            &format!("src/file{index:02}.ts"),
            &format!("export function file{index}fn0(value: number): number {{\n  return value + {index};\n}}\n"),
        );
    }
    repo.git(["add", "-A"]);
    repo.git(["commit", "-m", "target"]);
    let target = repo.rev_parse("HEAD");

    let request = AnalysisRequest {
        repository_path: repo.path().to_path_buf(),
        base_revision: base,
        target_revision: target,
    };

    let first = analyze(&request).expect("analysis succeeds");
    let second = analyze(&request).expect("analysis succeeds");

    assert_eq!(first, second, "parallel analysis must be deterministic");
    assert_eq!(first.files.len(), 40);

    let paths = first
        .files
        .iter()
        .filter_map(|file| file.target_path.clone())
        .collect::<Vec<_>>();
    let mut sorted_paths = paths.clone();
    sorted_paths.sort();
    assert_eq!(paths, sorted_paths, "files must stay ordered by path");

    let ids = first
        .files
        .iter()
        .flat_map(|file| file.functions.iter().map(|function| function.id.clone()))
        .collect::<Vec<_>>();
    let expected_ids = (1..=ids.len())
        .map(|index| format!("function-{index}"))
        .collect::<Vec<_>>();
    assert_eq!(ids, expected_ids, "function ids must follow file order");
}

fn find<'a>(files: &'a [diffscope::FileChange], path: &str) -> &'a diffscope::FileChange {
    files
        .iter()
        .find(|file| {
            file.target_path.as_deref() == Some(path) || file.base_path.as_deref() == Some(path)
        })
        .unwrap_or_else(|| panic!("missing file {path}"))
}

struct TestRepo {
    path: std::path::PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Self {
        let unique = format!(
            "diffscope-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir(&path).expect("create test repository directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative: &str, contents: &str) {
        self.write_bytes(relative, contents.as_bytes());
    }

    fn write_bytes(&self, relative: &str, contents: &[u8]) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    fn rev_parse(&self, revision: &str) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .arg("rev-parse")
            .arg(revision)
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success(), "git rev-parse failed");
        String::from_utf8(output.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_owned()
    }

    fn git<const N: usize>(&self, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}
