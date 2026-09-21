use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use diffscope::{
    AnalysisRequest, analyze,
    harness::{self, HarnessSession, jsonl, mcp},
    output,
    setup::{self, DoctorOptions, Environment, HarnessSelection, Mode, Options, Scope},
};
use serde_json::{Map, Value};

#[derive(Debug, Parser)]
#[command(
    name = "diffscope",
    version,
    about = "Measure the scope and impact of changes between Git revisions",
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Base Git revision.
    #[arg(required_unless_present = "jsonl")]
    base: Option<String>,

    /// Target Git revision.
    #[arg(required_unless_present = "jsonl")]
    target: Option<String>,

    /// Repository path (a path inside the work tree is accepted).
    #[arg(short, long, default_value = ".")]
    repository: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,

    /// Serve the versioned JSONL harness protocol over stdin and stdout.
    #[arg(long, conflicts_with_all = ["base", "target"])]
    jsonl: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the Model Context Protocol over stdin and stdout.
    Mcp,

    /// Register this executable as a read-only MCP server with coding harnesses.
    Setup(SetupArguments),

    /// Report whether each installed harness launches this executable.
    Doctor(DoctorArguments),

    /// Describe the dependency and impact changes one comparison introduces.
    Graph(GraphArguments),
}

#[derive(Debug, Args)]
struct SetupArguments {
    /// Harnesses to configure: `all`, or a comma-separated list such as `claude,cursor`
    /// (default: every harness detected as installed).
    #[arg(long, value_name = "NAMES")]
    harness: Option<String>,

    /// Configuration scope to write.
    #[arg(long, value_name = "SCOPE", default_value = "user")]
    scope: String,

    /// Report what would change without touching any configuration.
    #[arg(long)]
    dry_run: bool,

    /// Replace an entry that launches a different command.
    #[arg(long)]
    force: bool,

    /// Remove the entry instead of writing it.
    #[arg(long)]
    remove: bool,
}

#[derive(Debug, Args)]
struct DoctorArguments {
    /// Harnesses to inspect: `all`, or a comma-separated list such as `claude,cursor`
    /// (default: every supported harness).
    #[arg(long, value_name = "NAMES")]
    harness: Option<String>,

    /// Configuration scope to inspect.
    #[arg(long, value_name = "SCOPE", default_value = "user")]
    scope: String,
}

#[derive(Debug, Args)]
struct GraphArguments {
    /// Base Git revision.
    base: String,

    /// Target Git revision.
    target: String,

    /// Repository path (a path inside the work tree is accepted).
    #[arg(short, long, default_value = ".")]
    repository: PathBuf,

    /// Root the graph at one changed file.
    #[arg(long, value_name = "PATH", conflicts_with = "function")]
    file: Option<String>,

    /// Root the graph at one function.
    #[arg(long, value_name = "FUNCTION_ID")]
    function: Option<String>,

    /// Which way to walk: `upstream`, `downstream`, or `both`.
    #[arg(long, value_name = "DIRECTION", default_value = "both")]
    direction: String,

    /// Relations to follow, comma-separated (default: every supported relation).
    #[arg(long, value_name = "NAMES")]
    relations: Option<String>,

    /// Hops from the root.
    #[arg(long, value_name = "N", default_value_t = 1)]
    depth: u32,

    /// Edge set to show: `delta`, `base`, or `target`.
    #[arg(long, value_name = "VIEW", default_value = "delta")]
    view: String,

    /// Nodes the graph may carry.
    #[arg(long, value_name = "N", default_value_t = 30)]
    max_nodes: u32,

    /// Edges the graph may carry.
    #[arg(long, value_name = "N", default_value_t = 60)]
    max_edges: u32,

    /// Output format.
    #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
    format: GraphFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GraphFormat {
    /// The comparison, the root, the dependency diff, and the recommendation.
    Text,

    /// The dependency diff alone.
    Diff,

    /// The Mermaid source alone.
    Mermaid,

    /// The answer envelope the query API returns.
    Json,
}

impl GraphFormat {
    /// The renderings the query must produce for this format to print.
    fn renderings(self) -> Vec<&'static str> {
        match self {
            Self::Text | Self::Diff => vec!["diff"],
            Self::Mermaid => vec!["mermaid"],
            Self::Json => Vec::new(),
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(message) => {
            let _ignored = writeln!(io::stderr().lock(), "diffscope: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    if let Some(command) = &cli.command {
        return run_command(command);
    }

    if cli.jsonl {
        jsonl::serve(io::stdin().lock(), io::stdout().lock()).map_err(|error| error.to_string())?;
        return Ok(ExitCode::SUCCESS);
    }

    let base_revision = cli
        .base
        .ok_or_else(|| "base revision is required".to_owned())?;
    let target_revision = cli
        .target
        .ok_or_else(|| "target revision is required".to_owned())?;
    let result = analyze(&AnalysisRequest {
        repository_path: cli.repository,
        base_revision,
        target_revision,
    })
    .map_err(|error| error.to_string())?;

    let rendered = match cli.format {
        OutputFormat::Human => output::render_human(&result),
        OutputFormat::Json => output::render_json(&result)
            .map_err(|error| format!("could not serialize result: {error}"))?,
    };
    write_stdout(&rendered)?;
    Ok(ExitCode::SUCCESS)
}

/// Run the subcommand the caller named.
fn run_command(command: &Command) -> Result<ExitCode, String> {
    match command {
        Command::Mcp => mcp::serve().map(|()| ExitCode::SUCCESS),
        Command::Setup(arguments) => setup(arguments),
        Command::Doctor(arguments) => doctor(arguments),
        Command::Graph(arguments) => graph(arguments),
    }
}

/// Register, or remove, the MCP server entry in each selected harness.
fn setup(arguments: &SetupArguments) -> Result<ExitCode, String> {
    let mut options = Options::new(environment()?);
    options.selection = selection(arguments.harness.as_deref(), HarnessSelection::Detected)?;
    options.scope = scope(&arguments.scope)?;
    options.mode = if arguments.remove {
        Mode::Remove
    } else {
        Mode::Install
    };
    options.dry_run = arguments.dry_run;
    options.force = arguments.force;

    let report = setup::run(&options).map_err(|error| error.to_string())?;
    write_stdout(&report.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Report whether each selected harness launches this executable.
fn doctor(arguments: &DoctorArguments) -> Result<ExitCode, String> {
    let mut options = DoctorOptions::new(environment()?);
    options.selection = selection(arguments.harness.as_deref(), HarnessSelection::All)?;
    options.scope = scope(&arguments.scope)?;

    let report = setup::doctor(&options).map_err(|error| error.to_string())?;
    write_stdout(&report.to_string())?;
    // The report already names what is wrong with each harness, so an unhealthy
    // machine is a failed run rather than a failed message.
    Ok(if report.is_healthy() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Describe the dependency and impact changes one comparison introduces.
///
/// The answer is asked for through the harness projection rather than computed
/// here, so what this prints and what a harness reports for the same comparison
/// cannot drift apart.
fn graph(arguments: &GraphArguments) -> Result<ExitCode, String> {
    let answer = harness::answer_json(
        &HarnessSession::new(),
        &AnalysisRequest {
            repository_path: arguments.repository.clone(),
            base_revision: arguments.base.clone(),
            target_revision: arguments.target.clone(),
        },
        Some("get_impact_graph"),
        graph_params(arguments),
    )
    .map_err(|error| error.message)?;

    match arguments.format {
        GraphFormat::Text => write_stdout(&render_graph_text(&answer)?)?,
        GraphFormat::Diff => write_rendering(rendering(&answer, "dependency_diff")?)?,
        GraphFormat::Mermaid => write_rendering(rendering(&answer, "mermaid")?)?,
        GraphFormat::Json => write_stdout(&pretty_json(&answer)?)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The impact-graph parameters the caller named, plus the renderings the chosen
/// format prints.
///
/// Option values travel as written: the query layer decides what each one
/// accepts, so a rejected option is reported by the layer that knows the
/// accepted set instead of being guessed at twice.
fn graph_params(arguments: &GraphArguments) -> Value {
    let mut params = Map::new();
    if let Some(file) = arguments.file.as_deref() {
        params.insert("file".to_owned(), Value::from(file));
    }
    if let Some(function) = arguments.function.as_deref() {
        params.insert("function_id".to_owned(), Value::from(function));
    }
    params.insert(
        "direction".to_owned(),
        Value::from(arguments.direction.as_str()),
    );
    if let Some(relations) = arguments.relations.as_deref() {
        params.insert(
            "relations".to_owned(),
            Value::from(relation_names(relations)),
        );
    }
    params.insert("depth".to_owned(), Value::from(arguments.depth));
    params.insert("view".to_owned(), Value::from(arguments.view.as_str()));
    params.insert("max_nodes".to_owned(), Value::from(arguments.max_nodes));
    params.insert("max_edges".to_owned(), Value::from(arguments.max_edges));
    params.insert(
        "render".to_owned(),
        Value::from(arguments.format.renderings()),
    );
    Value::Object(params)
}

/// The relations a comma-separated list names, in the order it names them.
fn relation_names(named: &str) -> Vec<&str> {
    named.split(',').map(str::trim).collect()
}

/// Render one impact graph for a person: the comparison it describes, the root
/// the walk started from, the dependency diff, and whether a diagram is worth
/// drawing, with one line per reason.
fn render_graph_text(answer: &Value) -> Result<String, String> {
    let base = revision_name(answer, "base")?;
    let target = revision_name(answer, "target")?;

    let mut output = String::new();
    push_line(&mut output, &format!("DiffScope {base}..{target}"));
    push_line(&mut output, &root_line(answer));
    let diff = rendering(answer, "dependency_diff")?;
    if !diff.is_empty() {
        push_line(&mut output, diff);
    }

    let visualization = answer
        .get("data")
        .and_then(|data| data.get("visualization"))
        .ok_or_else(|| "the answer carried no `visualization`".to_owned())?;
    let recommended = visualization
        .get("recommended")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    push_line(
        &mut output,
        if recommended {
            "Diagram: recommended"
        } else {
            "Diagram: not recommended"
        },
    );
    for reason in visualization
        .get("reasons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let code = reason
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("reason");
        let message = reason.get("message").and_then(Value::as_str).unwrap_or("");
        push_line(&mut output, &format!("  {code}: {message}"));
    }
    Ok(output)
}

/// The name the answer reports for one revision of the comparison.
fn revision_name<'a>(answer: &'a Value, revision: &str) -> Result<&'a str, String> {
    answer
        .get("analysis")
        .and_then(|analysis| analysis.get(revision))
        .and_then(|revision| revision.get("display_name"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the answer carried no `{revision}` display name"))
}

/// The root the walk started from, or the changed set when none was named.
fn root_line(answer: &Value) -> String {
    let named = answer
        .get("data")
        .and_then(|data| data.get("root"))
        .and_then(|root| root.get("path").or_else(|| root.get("id")))
        .and_then(Value::as_str);
    match named {
        Some(name) => format!("Root: {name}"),
        None => "Root: none (centered on the changed set)".to_owned(),
    }
}

/// One rendering the answer carries, without its trailing newline.
fn rendering<'a>(answer: &'a Value, field: &str) -> Result<&'a str, String> {
    answer
        .get("data")
        .and_then(|data| data.get(field))
        .and_then(Value::as_str)
        .map(|text| text.trim_end_matches('\n'))
        .ok_or_else(|| format!("the answer carried no `{field}`"))
}

/// Write one rendering, ended by exactly one newline so it can be piped.
fn write_rendering(text: &str) -> Result<(), String> {
    let mut output = String::with_capacity(text.len() + 1);
    output.push_str(text);
    output.push('\n');
    write_stdout(&output)
}

/// Serialize one answer the way an analysis is serialized, with a trailing newline.
fn pretty_json(answer: &Value) -> Result<String, String> {
    let mut output = serde_json::to_string_pretty(answer)
        .map_err(|error| format!("could not serialize answer: {error}"))?;
    output.push('\n');
    Ok(output)
}

fn push_line(output: &mut String, line: &str) {
    output.push_str(line);
    output.push('\n');
}

/// The facts about this machine that setup and doctor act on.
fn environment() -> Result<Environment, String> {
    Environment::detected().map_err(|error| error.to_string())
}

/// The harnesses a command acts on: the ones named, or the command's default.
fn selection(named: Option<&str>, default: HarnessSelection) -> Result<HarnessSelection, String> {
    match named {
        Some(named) => HarnessSelection::parse(named).map_err(|error| error.to_string()),
        None => Ok(default),
    }
}

/// The configuration scope a command acts on.
fn scope(named: &str) -> Result<Scope, String> {
    Scope::parse(named).ok_or_else(|| format!("unknown scope `{named}`; expected user or project"))
}

fn write_stdout(text: &str) -> Result<(), String> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(|error| format!("could not write output: {error}"))
}
