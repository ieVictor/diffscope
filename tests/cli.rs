use std::{fs, path::Path, process::Command};

#[test]
fn successful_analysis_returns_json_and_zero() {
    let repo = TestRepo::with_base();
    repo.write(
        "src/example.ts",
        "export function answer() { return 42; }\n",
    );
    repo.commit("add TypeScript");

    let output = run(&repo, &["--format", "json", "HEAD~1", "HEAD"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(stdout.contains("\"schema_version\": 1"));
    assert!(stdout.contains("\"qualified_name\": \"answer\""));
    assert!(stdout.contains("\"supported_files\": 1"));
}

#[test]
fn partially_supported_analysis_reports_diagnostic_and_zero() {
    let repo = TestRepo::with_base();
    repo.write("notes.md", "# Notes\n");
    repo.commit("add unsupported file");

    let output = run(&repo, &["HEAD~1", "HEAD"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(stdout.contains("1 unsupported"));
    assert!(stdout.contains("info unsupported_language: unsupported language"));
}

#[test]
fn failed_analysis_returns_one_and_actionable_error() {
    let repo = TestRepo::with_base();

    let output = run(&repo, &["missing-revision", "HEAD"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.starts_with("diffscope: git command failed"));
    assert!(stderr.contains("missing-revision"));
}

fn run(repo: &TestRepo, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .arg("--repository")
        .arg(repo.path())
        .args(arguments)
        .output()
        .expect("run diffscope")
}

struct TestRepo {
    path: std::path::PathBuf,
}

impl TestRepo {
    fn with_base() -> Self {
        let path = std::env::temp_dir().join(format!(
            "diffscope-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos()
        ));
        fs::create_dir(&path).expect("create repository");
        let repo = Self { path };
        repo.git(&["init"]);
        repo.git(&["config", "user.email", "diffscope@example.invalid"]);
        repo.git(&["config", "user.name", "DiffScope"]);
        repo.git(&["commit", "--allow-empty", "-m", "base"]);
        repo
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, contents).expect("write source file");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", message]);
    }

    fn git(&self, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(arguments)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}
