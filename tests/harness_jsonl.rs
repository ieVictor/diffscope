use std::{
    fs,
    io::{Cursor, Write},
    process::{Command, Stdio},
};

use diffscope::harness::jsonl;
use serde_json::{Value, json};

#[test]
fn cli_and_jsonl_adapter_return_equivalent_results() {
    let repo = TestRepo::with_base();
    repo.write("example.ts", "function changed() { return 1; }\n");
    repo.commit("add source");

    let cli_output = Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .args(["--repository", repo.path_str(), "--format", "json"])
        .args(["HEAD~1", "HEAD"])
        .output()
        .expect("run CLI");
    assert!(cli_output.status.success());
    let cli_result: Value = serde_json::from_slice(&cli_output.stdout).expect("parse CLI result");

    let mut child = Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .arg("--jsonl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start JSONL adapter");
    let request = json!({
        "protocol_version": 1,
        "id": "contract-test",
        "repository": repo.path_str(),
        "base": "HEAD~1",
        "target": "HEAD"
    });
    writeln!(child.stdin.as_mut().expect("adapter stdin"), "{request}").expect("write request");
    drop(child.stdin.take());
    let adapter_output = child.wait_with_output().expect("wait for adapter");
    assert!(adapter_output.status.success());

    let response: Value =
        serde_json::from_slice(&adapter_output.stdout).expect("parse adapter response");
    assert_eq!(response["protocol_version"], 1);
    assert_eq!(response["id"], "contract-test");
    assert_eq!(response["result"], cli_result);
}

#[test]
fn malformed_requests_return_errors_without_stopping_stream() {
    let input = concat!(
        "not JSON\n",
        "{\"protocol_version\":9,\"id\":\"second\",",
        "\"repository\":\".\",\"base\":\"a\",\"target\":\"b\"}\n"
    );
    let mut output = Vec::new();

    jsonl::serve(Cursor::new(input), &mut output).expect("adapter serves requests");

    let responses = String::from_utf8(output)
        .expect("output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("response is JSON"))
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["error"]["code"], "malformed_request");
    assert_eq!(
        responses[1]["error"]["code"],
        "unsupported_protocol_version"
    );
    assert_eq!(responses[1]["id"], "second");
}

struct TestRepo {
    path: std::path::PathBuf,
}

impl TestRepo {
    fn with_base() -> Self {
        let path = std::env::temp_dir().join(format!(
            "diffscope-harness-{}-{}",
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

    fn path_str(&self) -> &str {
        self.path.to_str().expect("temporary path is UTF-8")
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
