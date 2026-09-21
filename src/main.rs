use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use diffscope::{
    AnalysisRequest, analyze,
    harness::{jsonl, mcp},
    output,
    setup::{self, DoctorOptions, Environment, HarnessSelection, Mode, Options, Scope},
};

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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
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
