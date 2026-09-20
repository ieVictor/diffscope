use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, ValueEnum};
use diffscope::{AnalysisRequest, analyze, output};

#[derive(Debug, Parser)]
#[command(
    name = "diffscope",
    version,
    about = "Measure the scope and impact of changes between Git revisions"
)]
struct Cli {
    /// Base Git revision.
    base: String,

    /// Target Git revision.
    target: String,

    /// Repository path (a path inside the work tree is accepted).
    #[arg(short, long, default_value = ".")]
    repository: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ignored = writeln!(io::stderr().lock(), "diffscope: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let result = analyze(&AnalysisRequest {
        repository_path: cli.repository,
        base_revision: cli.base,
        target_revision: cli.target,
    })
    .map_err(|error| error.to_string())?;

    let rendered = match cli.format {
        OutputFormat::Human => output::render_human(&result),
        OutputFormat::Json => output::render_json(&result)
            .map_err(|error| format!("could not serialize result: {error}"))?,
    };
    io::stdout()
        .lock()
        .write_all(rendered.as_bytes())
        .map_err(|error| format!("could not write output: {error}"))
}
