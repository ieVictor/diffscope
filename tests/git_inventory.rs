use std::{fs, path::Path, process::Command};

use diffscope::{AnalysisRequest, BlobContent, FileStatus, inventory_changes};

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
