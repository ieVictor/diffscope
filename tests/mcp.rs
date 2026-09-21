//! Drive the MCP server the way a client does: over stdio, one JSON-RPC
//! message at a time.
//!
//! What is asserted here is the contract a client depends on: the handshake,
//! the tool list, and the answers themselves — including that the answer to a
//! question is the same object whichever transport asked it.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

/// The protocol revision the tests speak.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The six tools the server exposes, in the order it lists them.
const TOOLS: [&str; 6] = [
    "get_change_summary",
    "get_impact_graph",
    "list_changed_files",
    "list_changed_functions",
    "get_function_change",
    "get_analysis_diagnostics",
];

#[test]
fn handshake_negotiates_and_lists_exactly_the_six_tools() {
    let repo = sample_repo();
    let mut server = Server::start(repo.path());

    let info = server.initialization.clone();
    assert_eq!(info["protocolVersion"], json!(PROTOCOL_VERSION));
    assert_eq!(info["serverInfo"]["name"], json!("diffscope"));
    assert_eq!(
        info["serverInfo"]["version"],
        json!(env!("CARGO_PKG_VERSION"))
    );
    assert!(info["capabilities"]["tools"].is_object());
    assert!(
        info["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("repository")),
        "instructions should name the arguments every tool needs: {info}"
    );

    let listing = server.request("tools/list", &json!({}));
    let tools = listing["result"]["tools"]
        .as_array()
        .expect("the tool list is an array")
        .clone();
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, TOOLS);

    for tool in &tools {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], json!("object"), "{tool}");
        assert_eq!(schema["additionalProperties"], json!(false), "{tool}");
        let required = schema["required"].as_array().expect("a required list");
        for field in ["repository", "base", "target"] {
            assert!(
                required.iter().any(|name| name == field),
                "{} does not require {field}",
                tool["name"]
            );
            assert!(schema["properties"][field].is_object(), "{tool}");
        }
        assert_eq!(tool["annotations"]["readOnlyHint"], json!(true), "{tool}");
        assert_eq!(
            tool["annotations"]["destructiveHint"],
            json!(false),
            "{tool}"
        );
    }

    // The graph tool advertises the parameters this version accepts and no
    // others: a client must not be shown an argument the server would reject.
    let graph = tools
        .iter()
        .find(|tool| tool["name"] == json!("get_impact_graph"))
        .expect("the impact graph tool");
    let properties = &graph["inputSchema"]["properties"];
    for parameter in [
        "file",
        "direction",
        "relations",
        "depth",
        "view",
        "max_nodes",
        "max_edges",
        "render",
    ] {
        assert!(properties[parameter].is_object(), "{graph}");
    }
    assert!(
        properties.get("function_id").is_none(),
        "function roots are not supported yet, so the schema must not offer one: {graph}"
    );
}

#[test]
fn summary_answers_exactly_as_the_jsonl_transport_does() {
    let repo = sample_repo();
    let mut server = Server::start(repo.path());

    let answer = server.call("get_change_summary", &comparison(&repo));
    let jsonl = jsonl_result(repo.path(), "get_change_summary");

    assert_eq!(answer["isError"], json!(false));
    assert_eq!(answer["structuredContent"], jsonl);
    assert_eq!(answer["content"][0]["type"], json!("text"));
    // Clients that render text only must still receive the whole answer.
    let text = answer["content"][0]["text"].as_str().expect("text content");
    let rendered: Value = serde_json::from_str(text).expect("the text block is JSON");
    assert_eq!(rendered, jsonl);
}

#[test]
fn every_tool_answers_the_question_it_names() {
    let repo = sample_repo();
    let mut server = Server::start(repo.path());

    let summary = server.call("get_change_summary", &comparison(&repo));
    assert_eq!(summary["isError"], json!(false));
    assert!(summary["structuredContent"]["data"]["files"].is_object());

    let graph = server.call("get_impact_graph", &comparison(&repo));
    let rendered = &graph["structuredContent"]["data"];
    assert_eq!(rendered["root"], Value::Null);
    assert_eq!(
        rendered["graph"]["nodes"].as_array().map(Vec::len),
        Some(3),
        "the sample change modifies three modules: {rendered}"
    );
    assert!(rendered["graph"]["edges"].is_array());
    assert!(rendered["visualization"]["recommended"].is_boolean());

    let files = server.call("list_changed_files", &comparison(&repo));
    assert!(files["structuredContent"]["data"]["files"].is_array());

    let functions = server.call("list_changed_functions", &comparison(&repo));
    let listed = functions["structuredContent"]["data"]["functions"]
        .as_array()
        .expect("a page of functions");
    assert!(!listed.is_empty(), "the sample change adds functions");

    let mut arguments = comparison(&repo);
    arguments["function_id"] = listed[0]["function_id"].clone();
    let detail = server.call("get_function_change", &arguments);
    assert!(detail["structuredContent"]["data"]["function"].is_object());
    assert!(detail["structuredContent"]["data"]["hunks"].is_array());

    let diagnostics = server.call("get_analysis_diagnostics", &comparison(&repo));
    assert!(diagnostics["structuredContent"]["data"]["diagnostics"].is_array());
    assert!(
        diagnostics["structuredContent"]["data"]["counts"]["total"].is_number(),
        "the diagnostic answer should count what it reports: {diagnostics}"
    );
}

#[test]
fn function_detail_round_trips_an_identity_from_a_listing() {
    let repo = sample_repo();
    let mut server = Server::start(repo.path());

    let listing = server.call("list_changed_functions", &comparison(&repo));
    let function_id = listing["structuredContent"]["data"]["functions"][0]["function_id"]
        .as_str()
        .expect("a listed function identity")
        .to_owned();

    let mut arguments = comparison(&repo);
    arguments["function_id"] = json!(function_id);
    let detail = server.call("get_function_change", &arguments);

    assert_eq!(detail["isError"], json!(false));
    assert_eq!(
        detail["structuredContent"]["data"]["function"]["function_id"],
        json!(function_id)
    );
}

#[test]
fn listing_pages_continue_with_the_cursor_the_answer_returned() {
    let repo = sample_repo();
    let mut server = Server::start(repo.path());

    let mut arguments = comparison(&repo);
    arguments["limit"] = json!(2);
    let first = server.call("list_changed_files", &arguments.clone());
    let page = &first["structuredContent"]["page"];
    assert_eq!(page["returned"], json!(2));
    assert_eq!(page["total"], json!(3));
    assert_eq!(page["has_more"], json!(true));
    let cursor = page["next_cursor"].as_str().expect("a cursor to continue");

    arguments["cursor"] = json!(cursor);
    let second = server.call("list_changed_files", &arguments);
    let page = &second["structuredContent"]["page"];
    assert_eq!(page["returned"], json!(1));
    assert_eq!(page["has_more"], json!(false));
    assert_eq!(page["next_cursor"], Value::Null);

    let listed = |answer: &Value| -> Vec<String> {
        answer["structuredContent"]["data"]["files"]
            .as_array()
            .expect("a page of files")
            .iter()
            .filter_map(|file| file["path"].as_str().map(str::to_owned))
            .collect()
    };
    let repeated: Vec<String> = listed(&second)
        .into_iter()
        .filter(|path| listed(&first).contains(path))
        .collect();
    assert!(repeated.is_empty(), "the second page repeated {repeated:?}");
}

#[test]
fn failures_reach_the_caller_where_it_can_read_them() {
    let repo = sample_repo();
    let mut server = Server::start(repo.path());

    // A call the server cannot route is a protocol error: the caller can
    // correct it from the tool list without reading a message.
    let unrouted = server.request(
        "tools/call",
        &json!({ "name": "analyze", "arguments": comparison(&repo) }),
    );
    assert_eq!(unrouted["error"]["code"], json!(-32602));
    assert!(unrouted.get("result").is_none());

    // A call that runs and fails is a tool error, so the caller reads why.
    let mut arguments = comparison(&repo);
    arguments["function_id"] = json!("src/absent.ts#fn:missing@target:1:1");
    let unknown = server.call("get_function_change", &arguments);
    assert_eq!(unknown["isError"], json!(true));
    assert_eq!(
        unknown["structuredContent"]["error"]["code"],
        json!("unknown_function")
    );
    assert!(
        unknown["structuredContent"]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("missing")),
        "the message should name the identity that was not found: {unknown}"
    );

    let mut arguments = comparison(&repo);
    arguments["base"] = json!("missing-revision");
    let failed = server.call("get_change_summary", &arguments);
    assert_eq!(failed["isError"], json!(true));
    assert_eq!(
        failed["structuredContent"]["error"]["code"],
        json!("analysis_failed")
    );
}

#[test]
fn client_disconnect_ends_the_session_cleanly() {
    let repo = sample_repo();
    let mut server = Server::start(repo.path());

    let listing = server.request("tools/list", &json!({}));
    assert_eq!(listing["result"]["tools"].as_array().map(Vec::len), Some(6));

    assert_eq!(server.close(), Some(0));
}

// ------------------------------------------------------------------ server ---

/// A running `diffscope mcp`, addressed the way a client addresses it.
struct Server {
    process: Child,
    /// The request stream, until the client closes it.
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    diagnostics: Arc<Mutex<String>>,
    initialization: Value,
    next_id: u64,
}

impl Server {
    /// Start a server in `directory`, handshake included.
    fn start(directory: &Path) -> Self {
        let mut process = Command::new(env!("CARGO_BIN_EXE_diffscope"))
            .arg("mcp")
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start diffscope mcp");
        let input = process.stdin.take().expect("capture stdin");
        let output = BufReader::new(process.stdout.take().expect("capture stdout"));
        let diagnostics = drain(process.stderr.take().expect("capture stderr"));
        let mut server = Self {
            process,
            input: Some(input),
            output,
            diagnostics,
            initialization: Value::Null,
            next_id: 0,
        };

        let handshake = server.request(
            "initialize",
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "diffscope-tests", "version": "0" },
            }),
        );
        server.initialization = handshake["result"].clone();
        server.notify("notifications/initialized", &json!({}));
        server
    }

    /// Call one tool and return the result the caller would read.
    fn call(&mut self, tool: &str, arguments: &Value) -> Value {
        let response = self.request(
            "tools/call",
            &json!({ "name": tool, "arguments": arguments }),
        );
        assert!(
            response.get("error").is_none(),
            "{tool} was not routed: {response}"
        );
        response["result"].clone()
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let response = self.read();
        assert_eq!(response["id"], json!(id), "answer to another request");
        response
    }

    fn notify(&mut self, method: &str, params: &Value) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn write(&mut self, message: &Value) {
        let input = self.input.as_mut().expect("the request stream is open");
        writeln!(input, "{message}").expect("write a request");
        input.flush().expect("flush a request");
    }

    /// Read one message, or fail with whatever the server said before it
    /// stopped answering.
    fn read(&mut self) -> Value {
        let mut line = String::new();
        let bytes = self.output.read_line(&mut line).expect("read an answer");
        assert!(
            bytes > 0,
            "the server stopped answering; stderr: {}",
            self.diagnostics()
        );
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("invalid answer: {error}: {line}"))
    }

    fn diagnostics(&self) -> String {
        self.diagnostics
            .lock()
            .expect("the diagnostics log is not poisoned")
            .clone()
    }

    /// Close the request stream the way a client does when it exits, and report
    /// the status the server exited with.
    fn close(&mut self) -> Option<i32> {
        drop(self.input.take());
        self.process.wait().expect("wait for the server").code()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ignored = self.process.kill();
        let _ignored = self.process.wait();
    }
}

/// Keep reading the server's diagnostics, so a full pipe can never stall it and
/// a failure can be reported with what it said.
fn drain(mut stderr: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let log = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&log);
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ignored = stderr.read_to_string(&mut text);
        sink.lock()
            .expect("the diagnostics log is not poisoned")
            .push_str(&text);
    });
    log
}

// ------------------------------------------------------------------- jsonl ---

/// Ask the JSONL transport the same question, and return its answer.
fn jsonl_result(repository: &Path, method: &str) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .arg("--jsonl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start diffscope --jsonl");
    let request = json!({
        "protocol_version": 2,
        "id": "parity",
        "repository": repository.to_string_lossy(),
        "base": "HEAD~1",
        "target": "HEAD",
        "method": method,
        "params": {},
    });
    {
        let mut input = child.stdin.take().expect("capture stdin");
        writeln!(input, "{request}").expect("write a request");
    }

    let output = child.wait_with_output().expect("run diffscope --jsonl");
    assert!(
        output.status.success(),
        "JSONL failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("parse the JSONL answer");
    assert!(response.get("error").is_none(), "JSONL failed: {response}");
    response["result"].clone()
}

/// The comparison every test asks about.
fn comparison(repo: &TestRepo) -> Value {
    json!({
        "repository": repo.path().to_string_lossy(),
        "base": "HEAD~1",
        "target": "HEAD",
    })
}

// ---------------------------------------------------------------- repository ---

/// A repository whose last commit changes three files.
fn sample_repo() -> TestRepo {
    let repo = TestRepo::with_base();
    for name in ["src/one.ts", "src/two.ts", "src/three.ts"] {
        repo.write(name, "export function untouched() { return 0; }\n");
    }
    repo.commit("add sources");
    for (name, body) in [
        ("src/one.ts", "export function answer() { return 42; }\n"),
        ("src/two.ts", "export function answer() { return 43; }\n"),
        ("src/three.ts", "export function answer() { return 44; }\n"),
    ] {
        repo.write(name, body);
    }
    repo.commit("change sources");
    repo
}

/// A throwaway Git repository, following the conventions of the CLI tests.
struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    fn with_base() -> Self {
        let path = std::env::temp_dir().join(format!(
            "diffscope-mcp-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
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
