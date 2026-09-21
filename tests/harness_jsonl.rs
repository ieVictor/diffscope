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

/// Serve a batch of requests through one adapter process, as a long-lived
/// harness does, and return one parsed response per request.
fn serve_all(requests: &[Value]) -> Vec<Value> {
    let input = requests
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut output = Vec::new();
    jsonl::serve(Cursor::new(input), &mut output).expect("adapter serves requests");
    String::from_utf8(output)
        .expect("output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("response is JSON"))
        .collect()
}

fn query(repo: &TestRepo, id: &str, method: &str, params: &Value) -> Value {
    json!({
        "protocol_version": 1,
        "id": id,
        "repository": repo.path_str(),
        "base": "HEAD~1",
        "target": "HEAD",
        "method": method,
        "params": params
    })
}

fn sample_repo() -> TestRepo {
    // `helper` sits above the edit so it neither moves nor is touched by a
    // hunk: it is the unchanged function these queries must withhold.
    const HELPER: &str = "export function helper(value: number) {\n  return value + 1;\n}\n";
    let repo = TestRepo::with_base();
    repo.write(
        "src/app.ts",
        &format!("{HELPER}export function run(flag: boolean) {{\n  return flag;\n}}\n"),
    );
    repo.write(
        "src/__tests__/app.spec.ts",
        "describe('app', () => {\n  it('runs', () => { return 1; })\n})\n",
    );
    repo.commit("add sources");
    repo.write(
        "src/app.ts",
        &format!(
            "{HELPER}export function run(flag: boolean) {{\n  if (flag) {{\n    for (const item of []) {{ console.log(item); }}\n    return 1;\n  }}\n  return flag;\n}}\n"
        ),
    );
    repo.commit("grow run");
    repo
}

#[test]
fn queries_return_only_what_was_asked_for() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "summary", "get_change_summary", &json!({})),
        query(
            &repo,
            "functions",
            "list_changed_functions",
            &json!({ "classification": "source" }),
        ),
        query(&repo, "files", "list_changed_files", &json!({})),
    ]);

    let summary = &responses[0]["result"];
    assert_eq!(summary["files"]["changed"], 1);
    assert!(summary["review_candidates"].as_array().is_some());
    // The overview never carries the full file or function inventory.
    assert!(
        summary
            .get("files")
            .and_then(|files| files.get("functions"))
            .is_none()
    );

    let functions = responses[1]["result"]["functions"]
        .as_array()
        .expect("functions array");
    assert!(!functions.is_empty());
    for function in functions {
        assert_eq!(function["classification"], "source");
        assert_ne!(function["status"], "unchanged");
    }

    assert!(
        responses[2]["result"]["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
    );
}

#[test]
fn unchanged_functions_are_withheld_until_they_are_requested() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "default", "list_changed_functions", &json!({})),
        query(
            &repo,
            "including",
            "list_changed_functions",
            &json!({ "include_unchanged": true }),
        ),
    ]);

    let changed = responses[0]["result"]["page"]["total"]
        .as_u64()
        .expect("total");
    let all = responses[1]["result"]["page"]["total"]
        .as_u64()
        .expect("total");
    assert!(all > changed, "unchanged functions must be retrievable");
}

#[test]
fn a_detail_query_names_the_symbols_a_file_does_contain() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(
            &repo,
            "found",
            "get_function_change",
            &json!({ "file": "src/app.ts", "symbol": "run" }),
        ),
        query(
            &repo,
            "missing",
            "get_function_change",
            &json!({ "file": "src/app.ts", "symbol": "absent" }),
        ),
    ]);

    assert_eq!(responses[0]["result"]["qualified_name"], "run");
    assert!(responses[0]["result"]["hunks"].as_array().is_some());

    assert_eq!(responses[1]["error"]["code"], "unknown_function");
    let message = responses[1]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("fn:run"), "{message}");
}

#[test]
fn rejects_unknown_methods_and_parameters_without_stopping_the_stream() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "method", "no_such_method", &json!({})),
        query(
            &repo,
            "params",
            "list_changed_functions",
            &json!({ "minimum_risk": "extreme" }),
        ),
        query(&repo, "after", "get_change_summary", &json!({})),
    ]);

    assert_eq!(responses[0]["error"]["code"], "unknown_method");
    assert_eq!(responses[1]["error"]["code"], "invalid_params");
    // The stream survives both, and the request after them is still answered.
    assert!(responses[2]["result"].is_object());
}

#[test]
fn a_reused_analysis_answers_identically_to_a_fresh_one() {
    let repo = sample_repo();
    let request = query(&repo, "q", "get_change_summary", &json!({}));

    // Two adapter processes each analyze once; one process answers twice and
    // serves the second from its cache. All three answers must agree.
    let fresh = serve_all(std::slice::from_ref(&request));
    let reused = serve_all(&[request.clone(), request]);

    assert_eq!(fresh[0]["result"], reused[0]["result"]);
    assert_eq!(reused[0]["result"], reused[1]["result"]);
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
