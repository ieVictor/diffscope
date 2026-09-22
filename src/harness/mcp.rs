//! Serve the harness as a Model Context Protocol server over stdio.
//!
//! The protocol is the official Rust SDK's job; this module only says what the
//! tools are and routes each call to the same projection every other transport
//! uses. An MCP client and a JSONL client asking the same question therefore
//! get the same answer, down to the cursors.
//!
//! Only protocol messages reach stdout, because a client parses that stream
//! unconditionally; anything a person needs to read goes to stderr.

use std::{path::PathBuf, sync::Arc};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations, object,
};
use rmcp::service::{QuitReason, RequestContext};
use rmcp::transport::io::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, serve_server};
use serde_json::{Value, json};

use super::{HarnessSession, Method, ProjectionError, QueryParams, answer};
use crate::AnalysisRequest;

/// What a client is told about this server during the handshake.
///
/// Instructions are the one thing a client's model reads before it chooses a
/// tool, so they describe the workflow rather than the implementation.
const INSTRUCTIONS: &str = "\
Measure the scope and impact of a change between two Git revisions of one repository. Every tool \
compares a committed base revision with a committed target revision and requires `repository`, `base`, and \
`target`; there is no default repository, so a call never analyzes a tree the caller did not name. Start \
with get_change_summary, see the shape of what the change reaches with get_impact_graph, page through the \
change with list_changed_files or list_changed_functions (handing `page.next_cursor` back unchanged to \
continue), read one function in full with get_function_change, and check get_analysis_diagnostics when an \
analysis looks incomplete. Every tool is read-only and reuses the analyses already made in this session.";

/// Serve the harness over stdin and stdout until the client disconnects.
///
/// The protocol runs on a small async runtime of its own: the SDK's stdio
/// transport is asynchronous while the analysis behind every tool is
/// synchronous, and the runtime is what keeps a long comparison from stalling
/// the protocol stream.
///
/// # Errors
///
/// Returns an error when the runtime cannot be started, or when the session
/// ends in failure rather than at the client's request.
pub fn serve() -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the MCP runtime: {error}"))?;
    runtime.block_on(serve_session())
}

async fn serve_session() -> Result<(), String> {
    let running = serve_server(Server::new(), stdio())
        .await
        .map_err(|error| format!("MCP initialization failed: {error}"))?;
    match running.waiting().await {
        Ok(QuitReason::JoinError(error)) | Err(error) => {
            Err(format!("MCP session failed: {error}"))
        }
        // Closed when the client disconnected, Cancelled when the transport was
        // torn down: both are the end of a session rather than a failure of one.
        Ok(_) => Ok(()),
    }
}

/// The server: one session's analyses, and the questions it will answer.
struct Server {
    session: Arc<HarnessSession>,
    tools: Vec<ToolSpec>,
}

/// One tool: the question it asks, and how that question is described.
struct ToolSpec {
    method: Method,
    tool: Tool,
}

impl Server {
    fn new() -> Self {
        Self {
            session: Arc::new(HarnessSession::new()),
            tools: tool_specs(),
        }
    }

    /// The tool a name refers to.
    ///
    /// # Errors
    ///
    /// Returns an invalid-parameter error naming the tools that do exist, so a
    /// caller that guessed can correct itself in one step.
    fn tool(&self, name: &str) -> Result<&ToolSpec, ErrorData> {
        self.tools
            .iter()
            .find(|spec| spec.tool.name.as_ref() == name)
            .ok_or_else(|| {
                let known = self
                    .tools
                    .iter()
                    .map(|spec| spec.tool.name.as_ref())
                    .collect::<Vec<_>>()
                    .join(", ");
                ErrorData::invalid_params(
                    format!("unknown tool `{name}`; this server exposes {known}"),
                    None,
                )
            })
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("DiffScope"),
            )
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.tools.iter().map(|spec| spec.tool.clone()).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let spec = self.tool(&request.name)?;
        let call = ToolCall::decode(request.arguments.unwrap_or_default())?;

        // Analyzing a repository reads Git objects and parses every changed
        // source file: work that would otherwise stall the runtime's threads.
        let session = Arc::clone(&self.session);
        let method = spec.method;
        let analyzed = tokio::task::spawn_blocking(move || {
            answer(&session, method, &call.request, &call.params)
        })
        .await
        .map_err(|error| {
            ErrorData::internal_error(format!("the analysis task failed: {error}"), None)
        })?;

        match analyzed {
            Ok(answer) => {
                let structured =
                    serde_json::to_value(&answer).map_err(|error| unrenderable(&error))?;
                Ok(CallToolResult::structured(structured).into())
            }
            Err(error) => Ok(CallToolResult::structured_error(error_value(&error)).into()),
        }
    }
}

fn unrenderable(error: &serde_json::Error) -> ErrorData {
    ErrorData::internal_error(format!("could not render the answer: {error}"), None)
}

/// The structured half of a failed call, in the same shape the JSONL transport
/// reports the same failure.
fn error_value(error: &ProjectionError) -> Value {
    json!({ "error": { "code": error.code, "message": error.message } })
}

/// One decoded tool call: the comparison to analyze, and how to ask about it.
struct ToolCall {
    request: AnalysisRequest,
    params: QueryParams,
}

impl ToolCall {
    /// Decode the arguments of one tool call.
    ///
    /// # Errors
    ///
    /// Returns an invalid-parameter error when the arguments are not a call
    /// this server can answer, phrased for the caller who wrote them.
    fn decode(mut arguments: JsonObject) -> Result<Self, ErrorData> {
        let repository = take_string(&mut arguments, "repository")?;
        let base = take_string(&mut arguments, "base")?;
        let target = take_string(&mut arguments, "target")?;
        let params = QueryParams::decode(Value::Object(arguments))
            .map_err(|error| ErrorData::invalid_params(error.message, None))?;
        Ok(Self {
            request: AnalysisRequest {
                repository_path: PathBuf::from(repository),
                base_revision: base,
                target_revision: target,
            },
            params,
        })
    }
}

/// Remove one required string argument, or say what is wrong with it.
fn take_string(arguments: &mut JsonObject, field: &str) -> Result<String, ErrorData> {
    let invalid = |message: String| ErrorData::invalid_params(message, None);
    match arguments.remove(field) {
        None => Err(invalid(format!("`{field}` is required"))),
        Some(Value::String(value)) if value.is_empty() => {
            Err(invalid(format!("`{field}` must not be empty")))
        }
        Some(Value::String(value)) => Ok(value),
        Some(other) => Err(invalid(format!(
            "`{field}` must be a string; received {other}"
        ))),
    }
}

// ------------------------------------------------------------------ tools ---

/// The six questions this server answers, in the order they are worth asking.
fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            method: Method::ChangeSummary,
            tool: describe(
                "get_change_summary",
                "Summarize one comparison: what changed, where, and which functions are worth \
                 reviewing first.",
                Schema::new(),
            ),
        },
        ToolSpec {
            method: Method::GetImpactGraph,
            tool: describe(
                "get_impact_graph",
                "Show the shape of what one comparison reaches: the modules importing a changed \
                 file, the tests related to it, and — rooted at a function — the functions it \
                 calls in its own file and the ones that call it there, with which of those \
                 relationships the change added or removed.",
                Schema::new()
                    .optional("file", root_file_argument())
                    .optional("function_id", root_function_argument())
                    .optional("direction", direction_argument())
                    .optional("relations", relations_argument())
                    .optional("depth", depth_argument())
                    .optional("view", view_argument())
                    .optional("max_nodes", max_nodes_argument())
                    .optional("max_edges", max_edges_argument())
                    .optional("render", render_argument()),
            ),
        },
        ToolSpec {
            method: Method::ListChangedFiles,
            tool: describe(
                "list_changed_files",
                "List the files one comparison changed, ranked, one page at a time.",
                Schema::new()
                    .optional("classification", classification_argument())
                    .optional("minimum_risk", minimum_risk_argument())
                    .optional("limit", limit_argument())
                    .optional("cursor", cursor_argument()),
            ),
        },
        ToolSpec {
            method: Method::ListChangedFunctions,
            tool: describe(
                "list_changed_functions",
                "List the functions one comparison changed, ranked, one page at a time.",
                Schema::new()
                    .optional("file", file_argument())
                    .optional("status", status_argument())
                    .optional("classification", classification_argument())
                    .optional("minimum_risk", minimum_risk_argument())
                    .optional("min_complexity_delta", complexity_delta_argument())
                    .optional("include_unchanged", unchanged_argument())
                    .optional("limit", limit_argument())
                    .optional("cursor", cursor_argument()),
            ),
        },
        ToolSpec {
            method: Method::GetFunctionChange,
            tool: describe(
                "get_function_change",
                "Describe one changed function in full, by the `function_id` an earlier listing \
                 reported.",
                Schema::new().required("function_id", function_argument()),
            ),
        },
        ToolSpec {
            method: Method::GetAnalysisDiagnostics,
            tool: describe(
                "get_analysis_diagnostics",
                "Report what one comparison could not analyze, and where.",
                Schema::new().optional("file", file_argument()),
            ),
        },
    ]
}

/// Describe one tool: what it answers, and the arguments it accepts.
///
/// Every tool is read-only: it reads Git objects and source files, changes
/// nothing, and answers the same question the same way for as long as the
/// revisions resolve to the same commits.
fn describe(name: &'static str, description: &'static str, schema: Schema) -> Tool {
    let mut tool = Tool::new(name, description, schema.build());
    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true),
    );
    tool
}

/// The input schema of one tool, under construction.
struct Schema {
    properties: JsonObject,
    required: Vec<&'static str>,
}

impl Schema {
    /// A schema for a question about one comparison, which every tool asks.
    fn new() -> Self {
        Self {
            properties: JsonObject::new(),
            required: Vec::new(),
        }
        .required("repository", repository_argument())
        .required("base", revision_argument("base"))
        .required("target", revision_argument("target"))
    }

    fn required(mut self, name: &'static str, argument: Value) -> Self {
        self.required.push(name);
        self.properties.insert(name.to_owned(), argument);
        self
    }

    fn optional(mut self, name: &'static str, argument: Value) -> Self {
        self.properties.insert(name.to_owned(), argument);
        self
    }

    fn build(self) -> Arc<JsonObject> {
        Arc::new(object(json!({
            "type": "object",
            "properties": self.properties,
            "required": self.required,
            "additionalProperties": false,
        })))
    }
}

fn repository_argument() -> Value {
    json!({
        "type": "string",
        "description": "Path to a Git repository, or to any path inside its work tree. Relative \
                        paths resolve against the directory this server was started in.",
    })
}

fn revision_argument(revision: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "The {revision} revision of the comparison: any revision Git resolves, such as a \
             commit id, branch, tag, or `HEAD~1`. Committed revisions only; the working tree and \
             the index are never read."
        ),
    })
}

fn file_argument() -> Value {
    json!({
        "type": "string",
        "description": "Restrict the answer to one file path, as the comparison reports it.",
    })
}

fn root_file_argument() -> Value {
    json!({
        "type": "string",
        "description": "Root the graph at one changed file, as the comparison reports it. Without \
                        one, the graph is centered on every changed file.",
    })
}

fn root_function_argument() -> Value {
    json!({
        "type": "string",
        "description": "Root the graph at one function, by the `function_id` an earlier listing \
                        reported. Without one, the graph is centered on every changed file. \
                        Mutually exclusive with `file`.",
    })
}

fn direction_argument() -> Value {
    json!({
        "type": "string",
        "enum": ["upstream", "downstream", "both"],
        "default": "both",
        "description": "Which way get_impact_graph walks from its root: upstream follows edges \
                        backwards, to what reaches the root; downstream follows them forwards, to \
                        what the root reaches.",
    })
}

fn relations_argument() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": ["imports", "tested_by", "calls", "contains", "re_exports"] },
        "description": "Which relationships get_impact_graph may follow. Defaults to every \
                        relation this version resolves.",
    })
}

fn depth_argument() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": 3,
        "default": 1,
        "description": "Hops get_impact_graph walks from its root.",
    })
}

fn view_argument() -> Value {
    json!({
        "type": "string",
        "enum": ["delta", "base", "target"],
        "default": "delta",
        "description": "Which revision's relationships get_impact_graph shows. A view narrows what \
                        is shown without changing what is true, so an added edge still reads \
                        `added` under `target`.",
    })
}

fn max_nodes_argument() -> Value {
    json!({
        "type": "integer",
        "minimum": 3,
        "maximum": 100,
        "default": 30,
        "description": "Nodes get_impact_graph delivers. A graph that exceeds a budget reports \
                        what it omitted.",
    })
}

fn max_edges_argument() -> Value {
    json!({
        "type": "integer",
        "minimum": 3,
        "maximum": 200,
        "default": 60,
        "description": "Relationships get_impact_graph delivers. A graph that exceeds a budget \
                        reports what it omitted.",
    })
}

fn render_argument() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": ["diff", "mermaid"] },
        "default": [],
        "description": "Renderings to include beside the structured graph: the dependency diff, \
                        the Mermaid diagram, or neither by default.",
    })
}

fn function_argument() -> Value {
    json!({
        "type": "string",
        "description": "Identity of one function, as list_changed_functions reports it.",
    })
}

fn status_argument() -> Value {
    json!({
        "type": "string",
        "enum": ["added", "removed", "modified", "unchanged"],
        "description": "Only functions whose change has this status.",
    })
}

fn classification_argument() -> Value {
    json!({
        "type": "string",
        "enum": ["lockfile", "vendored", "generated", "test", "config", "docs", "source"],
        "description": "Only files of this classification.",
    })
}

fn minimum_risk_argument() -> Value {
    json!({
        "type": "string",
        "enum": ["low", "medium", "high"],
        "description": "Only entries at or above this risk.",
    })
}

fn complexity_delta_argument() -> Value {
    json!({
        "type": "integer",
        "description": "Only functions whose cognitive-complexity delta is at least this value.",
    })
}

fn unchanged_argument() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Include functions the comparison found unchanged.",
    })
}

fn limit_argument() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": 200,
        "default": 50,
        "description": "Page size.",
    })
}

fn cursor_argument() -> Value {
    json!({
        "type": "string",
        "description": "Opaque continuation token from an earlier answer's `page.next_cursor`. \
                        Pass it back unchanged; the cursor, not the other arguments, is the \
                        authority on what is being paged through.",
    })
}
