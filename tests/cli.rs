use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

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

#[test]
fn graph_text_reports_the_comparison_root_diff_and_recommendation() {
    let repo = graph_repo();

    let output = run_graph(&repo, &["--file", "src/util.ts", "HEAD~1", "HEAD"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "DiffScope HEAD~1..HEAD");
    assert_eq!(lines[1], "Root: src/util.ts");
    assert!(
        stdout.contains("+ src/app.ts -> src/util.ts\n"),
        "the dependency diff belongs in the human rendering:\n{stdout}"
    );

    let diagram = lines
        .iter()
        .position(|line| line.starts_with("Diagram: "))
        .unwrap_or_else(|| panic!("no recommendation line:\n{stdout}"));
    assert_eq!(lines[diagram], "Diagram: recommended");
    let reasons = &lines[diagram + 1..];
    assert!(!reasons.is_empty(), "a recommendation states its reasons");
    for reason in reasons {
        assert!(
            reason.starts_with("  ") && reason.contains(": "),
            "a reason line names its code and message: {reason}"
        );
    }
}

#[test]
fn graph_diff_prints_the_dependency_diff_alone() {
    let repo = graph_repo();

    let output = run_graph(
        &repo,
        &[
            "--format",
            "diff",
            "--file",
            "src/util.ts",
            "HEAD~1",
            "HEAD",
        ],
    );

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let answer = jsonl_graph(&repo, &graph_params(("file", "src/util.ts"), &["diff"]));
    let expected = format!("{}\n", trimmed(rendering(&answer, "dependency_diff")));
    assert_eq!(stdout, expected);
    assert!(!stdout.contains("DiffScope"), "{stdout}");
}

#[test]
fn graph_mermaid_prints_the_diagram_alone() {
    let repo = graph_repo();

    let output = run_graph(
        &repo,
        &[
            "--format",
            "mermaid",
            "--file",
            "src/util.ts",
            "HEAD~1",
            "HEAD",
        ],
    );

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let answer = jsonl_graph(&repo, &graph_params(("file", "src/util.ts"), &["mermaid"]));
    let expected = format!("{}\n", trimmed(rendering(&answer, "mermaid")));
    assert_eq!(stdout, expected);
    assert!(stdout.starts_with("flowchart LR"), "{stdout}");
    assert!(!stdout.contains("DiffScope"), "{stdout}");
}

#[test]
fn graph_json_returns_the_answer_envelope() {
    let repo = graph_repo();

    let output = run_graph(
        &repo,
        &[
            "--format",
            "json",
            "--file",
            "src/util.ts",
            "HEAD~1",
            "HEAD",
        ],
    );

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.ends_with(b"\n"), "a JSON answer ends a line");
    let answer: Value = serde_json::from_slice(&output.stdout).expect("the answer is JSON");
    let mut keys = answer
        .as_object()
        .expect("the answer is an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(keys, ["analysis", "data", "query"]);
    assert_eq!(answer["analysis"]["schema_version"], json!(2));
    assert_eq!(answer["analysis"]["base"]["display_name"], "HEAD~1");
    assert_eq!(answer["analysis"]["target"]["display_name"], "HEAD");
    assert_eq!(answer["data"]["root"]["path"], "src/util.ts");
    assert_eq!(answer["data"]["graph"]["truncated"], json!(false));
    assert!(
        !answer["data"]["graph"]["nodes"]
            .as_array()
            .expect("the graph carries nodes")
            .is_empty()
    );
}

#[test]
fn graph_json_answers_match_the_jsonl_transport() {
    let repo = graph_repo();

    let output = run_graph(
        &repo,
        &[
            "--format",
            "json",
            "--file",
            "src/util.ts",
            "HEAD~1",
            "HEAD",
        ],
    );

    assert!(output.status.success(), "{output:?}");
    let cli: Value = serde_json::from_slice(&output.stdout).expect("the answer is JSON");
    let jsonl = jsonl_graph(&repo, &graph_params(("file", "src/util.ts"), &[]));

    assert_eq!(
        serde_json::to_string(&cli["data"]).expect("serialize the CLI answer"),
        serde_json::to_string(&jsonl["data"]).expect("serialize the transport answer"),
        "one comparison answers the same question once"
    );
}

#[test]
fn graph_function_root_reports_the_function_and_its_diff() {
    let repo = graph_repo();

    // The identity the transports publish, so the flag is exercised with a real
    // one.
    let responses = run_jsonl(&[json!({
        "protocol_version": 2,
        "id": "functions",
        "repository": repo.path().to_str().expect("the temporary path is UTF-8"),
        "base": "HEAD~1",
        "target": "HEAD",
        "method": "list_changed_functions",
        "params": { "file": "src/util.ts" },
    })]);
    let id = responses[0]["result"]["data"]["functions"][0]["function_id"]
        .as_str()
        .expect("a listed function identity")
        .to_owned();

    let output = run_graph(&repo, &["--function", &id, "HEAD~1", "HEAD"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "DiffScope HEAD~1..HEAD");
    assert_eq!(
        lines[1],
        format!("Root: function:{id}"),
        "a function root is named by its identity: {stdout}"
    );
    assert!(
        stdout.contains("src/util.ts::score"),
        "the function endpoint is named in the diff: {stdout}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with("Diagram: ")),
        "{stdout}"
    );

    // The diff rendering is the document the query layer produces for the same
    // root, printed alone.
    let output = run_graph(
        &repo,
        &["--format", "diff", "--function", &id, "HEAD~1", "HEAD"],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let answer = jsonl_graph(&repo, &graph_params(("function_id", &id), &["diff"]));
    let expected = format!("{}\n", trimmed(rendering(&answer, "dependency_diff")));
    assert_eq!(stdout, expected);
    assert!(stdout.contains("src/util.ts::score"), "{stdout}");
}

#[test]
fn graph_rejects_an_unsupported_relation_naming_what_is_accepted() {
    let repo = graph_repo();

    let output = run_graph(
        &repo,
        &[
            "--file",
            "src/util.ts",
            "--relations",
            "extends",
            "HEAD~1",
            "HEAD",
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.starts_with("diffscope: "), "{stderr}");
    assert!(
        stderr.contains("extends"),
        "the rejected name is echoed: {stderr}"
    );
    for accepted in ["imports", "tested_by", "calls", "contains"] {
        assert!(
            stderr.contains(accepted),
            "the message names `{accepted}` as accepted: {stderr}"
        );
    }
}

fn run(repo: &TestRepo, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .arg("--repository")
        .arg(repo.path())
        .args(arguments)
        .output()
        .expect("run diffscope")
}

/// Run one `diffscope graph` invocation against a fixture repository.
fn run_graph(repo: &TestRepo, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .arg("graph")
        .arg("--repository")
        .arg(repo.path())
        .args(arguments)
        .output()
        .expect("run diffscope graph")
}

/// Serve newline-delimited requests through a freshly started adapter.
fn run_jsonl(requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .arg("--jsonl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the JSONL adapter");
    {
        let stdin = child.stdin.as_mut().expect("adapter stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write a request");
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for the adapter");
    assert!(output.status.success(), "the adapter exited: {output:?}");
    String::from_utf8(output.stdout)
        .expect("adapter output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a response is JSON"))
        .collect()
}

/// The answer one JSONL transport returns for an impact-graph request.
fn jsonl_graph(repo: &TestRepo, params: &Value) -> Value {
    let responses = run_jsonl(&[json!({
        "protocol_version": 2,
        "id": "graph-equivalence",
        "repository": repo.path().to_str().expect("the temporary path is UTF-8"),
        "base": "HEAD~1",
        "target": "HEAD",
        "method": "get_impact_graph",
        "params": params,
    })]);
    let response = responses.first().expect("one response");
    assert!(response.get("error").is_none(), "{response}");
    response
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("the response carries no result: {response}"))
}

/// The parameters a `diffscope graph` run asks with: the root it named and the
/// defaults the CLI applies, for the renderings the chosen format requests.
///
/// The root is the field and value the invocation named — `("file",
/// "src/util.ts")` for a file root, `("function_id", id)` for a function root —
/// so one helper describes either run.
fn graph_params(root: (&str, &str), render: &[&str]) -> Value {
    let mut params = json!({
        "direction": "both",
        "depth": 1,
        "view": "delta",
        "max_nodes": 30,
        "max_edges": 60,
        "render": render,
    });
    params[root.0] = json!(root.1);
    params
}

/// A rendering an answer carries, as a single line of output.
fn rendering<'a>(answer: &'a Value, field: &str) -> &'a str {
    answer["data"][field]
        .as_str()
        .unwrap_or_else(|| panic!("the answer carries no `{field}`: {answer}"))
}

fn trimmed(text: &str) -> &str {
    text.trim_end_matches('\n')
}

/// A comparison whose changed module has importers, one of which redirected its
/// import from a legacy module, and a test file beside it.
fn graph_repo() -> TestRepo {
    const UTIL_BASE: &str = "export function score(value: number) {\n  return value;\n}\n";
    const UTIL_TARGET: &str =
        "export function score(value: number) {\n  return value > 0 ? value * 2 : -value;\n}\n";
    const LEGACY: &str = "export function score(value: number) {\n  return value;\n}\n";
    const APP_BASE: &str =
        "import { score } from './legacy';\n\nexport function run() {\n  return score(1);\n}\n";
    const APP_TARGET: &str =
        "import { score } from './util';\n\nexport function run() {\n  return score(1);\n}\n";
    const SPEC: &str = "import { score } from '../util';\n\nexport function testsScore() {\n  return score(1);\n}\n";

    let repo = TestRepo::with_base();
    repo.write("src/util.ts", UTIL_BASE);
    repo.write("src/legacy.ts", LEGACY);
    repo.write("src/app.ts", APP_BASE);
    repo.write("src/__tests__/util.spec.ts", SPEC);
    for name in ["other", "third", "fourth"] {
        repo.write(
            &format!("src/{name}.ts"),
            &format!(
                "import {{ score }} from './util';\n\nexport function use{name}() {{\n  return score(1);\n}}\n"
            ),
        );
    }
    repo.commit("add sources");

    repo.write("src/util.ts", UTIL_TARGET);
    repo.write("src/app.ts", APP_TARGET);
    repo.commit("change sources");
    repo
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
