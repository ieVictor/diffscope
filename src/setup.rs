//! Harness integration for `diffscope setup` and `diffscope doctor`.
//!
//! Setup registers the running executable as a read-only MCP server
//! (`<executable> mcp`) with every supported coding harness. Harnesses whose
//! configuration is owned by a command line interface (`claude`, `codex`,
//! `gemini`) are configured through that program whenever it is on `PATH`; the
//! remaining harnesses are configured by merging one entry into their JSON
//! configuration and leaving every other key untouched.
//!
//! Writes are atomic (a temporary file in the target directory followed by a
//! rename) and repeated runs are idempotent: an entry that already launches
//! this executable is left alone. A configuration file that cannot be parsed is
//! never rewritten, and an entry that belongs to something else is only
//! replaced when the caller passes `--force`.
//!
//! [`Environment`] carries every filesystem and process fact the module needs,
//! so setup and doctor can run against a pinned directory tree (tests) or the
//! real process environment ([`Environment::detected`]).

use std::{
    env, fmt, fs, io,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

/// Name of the MCP server entry written into every harness configuration.
pub const SERVER_NAME: &str = "diffscope";

/// Argument that makes the executable serve the MCP protocol over stdio.
const MCP_ARGUMENT: &str = "mcp";

/// Prefix of the temporary files used to replace configuration atomically.
const TEMPORARY_PREFIX: &str = ".diffscope-tmp";

/// A coding harness whose MCP configuration `DiffScope` can manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Claude Code.
    Claude,
    /// Codex CLI.
    Codex,
    /// Gemini CLI.
    Gemini,
    /// Cursor.
    Cursor,
    /// `OpenCode`.
    Opencode,
    /// Visual Studio Code.
    Vscode,
}

impl Harness {
    /// Every supported harness, in reporting order.
    pub const ALL: [Self; 6] = [
        Self::Claude,
        Self::Codex,
        Self::Gemini,
        Self::Cursor,
        Self::Opencode,
        Self::Vscode,
    ];

    /// The name accepted by `--harness` and printed in reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
            Self::Opencode => "opencode",
            Self::Vscode => "vscode",
        }
    }

    /// Parse one harness name as `--harness` spells it, ignoring case.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|harness| harness.name().eq_ignore_ascii_case(value.trim()))
    }

    /// Whether this harness has a configuration file at the given scope.
    #[must_use]
    pub const fn supports_scope(self, scope: Scope) -> bool {
        !matches!((self, scope), (Self::Codex, Scope::Project))
    }

    /// Every value `--harness` accepts, for error messages and `--help` text.
    #[must_use]
    pub fn supported_names() -> String {
        let mut names = vec!["all"];
        names.extend(Self::ALL.into_iter().map(Self::name));
        names.join(", ")
    }

    /// The program that owns this harness configuration, when one exists.
    const fn configuration_program(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::Gemini => Some("gemini"),
            Self::Cursor | Self::Opencode | Self::Vscode => None,
        }
    }

    /// Programs whose presence on `PATH` indicates the harness is installed.
    const fn detection_programs(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude"],
            Self::Codex => &["codex"],
            Self::Gemini => &["gemini"],
            Self::Cursor => &["cursor", "cursor-agent"],
            Self::Opencode => &["opencode"],
            Self::Vscode => &["code", "code-insiders"],
        }
    }

    /// How this harness stores MCP servers.
    const fn format(self) -> Format {
        match self {
            Self::Codex => Format::Toml,
            Self::Opencode => Format::Json(JsonLayout {
                container: "mcp",
                style: EntryStyle::LocalCommand,
            }),
            Self::Vscode => Format::Json(JsonLayout {
                container: "servers",
                style: EntryStyle::CommandArgs { declare_type: true },
            }),
            Self::Claude | Self::Gemini | Self::Cursor => Format::Json(JsonLayout {
                container: "mcpServers",
                style: EntryStyle::CommandArgs {
                    declare_type: false,
                },
            }),
        }
    }
}

/// Configuration scope: the current user or the current project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Configuration in the user's home or configuration directory.
    User,
    /// Configuration inside the current project.
    Project,
}

impl Scope {
    /// The name accepted by `--scope` and printed in reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    /// Parse one scope name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            value if value.eq_ignore_ascii_case("user") => Some(Self::User),
            value if value.eq_ignore_ascii_case("project") => Some(Self::Project),
            _ => None,
        }
    }
}

/// Which harnesses a command acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessSelection {
    /// Harnesses detected as installed on this machine.
    Detected,
    /// Every supported harness.
    All,
    /// Exactly the named harnesses, in the given order.
    Named(Vec<Harness>),
}

impl HarnessSelection {
    /// Parse the `--harness` value: `all`, or a comma-separated list of names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Selection`] for an empty value or an unknown name.
    pub fn parse(value: &str) -> Result<Self, Error> {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        let mut named: Vec<Harness> = Vec::new();
        for part in trimmed
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let harness = Harness::parse(part).ok_or_else(|| Error::Selection {
                message: format!(
                    "unknown harness \"{part}\"; supported names are {}",
                    Harness::supported_names()
                ),
            })?;
            if !named.contains(&harness) {
                named.push(harness);
            }
        }
        if named.is_empty() {
            return Err(Error::Selection {
                message: format!(
                    "no harness names given; pass --harness all or a comma-separated list such as \
                     claude,cursor ({})",
                    Harness::supported_names()
                ),
            });
        }
        Ok(Self::Named(named))
    }

    /// Whether the caller named harnesses individually instead of selecting in bulk.
    fn is_named(&self) -> bool {
        matches!(self, Self::Named(_))
    }
}

/// Which change [`run`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Create or reconcile the `diffscope` entry.
    Install,
    /// Delete the `diffscope` entry.
    Remove,
}

/// Filesystem and process facts that setup and doctor operate on.
///
/// [`Environment::detected`] reads the process environment.
/// [`Environment::injected`] pins every root, which lets tests run against a
/// temporary tree without touching the real home directory or invoking real
/// harness programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    home: PathBuf,
    config_home: PathBuf,
    project_root: PathBuf,
    executable: PathBuf,
    program_paths: Vec<PathBuf>,
    codex_home: PathBuf,
    claude_config_dir: PathBuf,
    app_data: Option<PathBuf>,
}

impl Environment {
    /// Read the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Environment`] when `HOME` or the current directory
    /// cannot be determined, or when the running executable has no path.
    pub fn detected() -> Result<Self, Error> {
        let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
            return Err(Error::Environment {
                message: "HOME is not set; run setup from a normal login shell".to_owned(),
            });
        };
        let config_home =
            env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
        let codex_home =
            env::var_os("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from);
        let claude_config_dir =
            env::var_os("CLAUDE_CONFIG_DIR").map_or_else(|| home.clone(), PathBuf::from);
        let project_root = env::current_dir().map_err(|error| Error::Environment {
            message: format!("could not determine the current directory: {error}"),
        })?;
        let executable = env::current_exe().map_err(|error| Error::Environment {
            message: format!("could not determine the running executable: {error}"),
        })?;
        let program_paths = env::var_os("PATH")
            .map(|value| env::split_paths(&value).collect())
            .unwrap_or_default();
        Ok(Self {
            home: resolve(home),
            config_home: resolve(config_home),
            project_root: resolve(project_root),
            executable: resolve(executable),
            program_paths,
            codex_home: resolve(codex_home),
            claude_config_dir: resolve(claude_config_dir),
            app_data: env::var_os("APPDATA").map(PathBuf::from),
        })
    }

    /// Pin every root and treat no harness program as available.
    ///
    /// Add harness programs with [`Environment::with_program_paths`].
    #[must_use]
    pub fn injected(
        home: PathBuf,
        config_home: PathBuf,
        project_root: PathBuf,
        executable: PathBuf,
    ) -> Self {
        let home = resolve(home);
        Self {
            codex_home: home.join(".codex"),
            claude_config_dir: home.clone(),
            home,
            config_home: resolve(config_home),
            project_root: resolve(project_root),
            executable: resolve(executable),
            program_paths: Vec::new(),
            app_data: None,
        }
    }

    /// Directories searched for harness programs, in order.
    #[must_use]
    pub fn with_program_paths(mut self, program_paths: Vec<PathBuf>) -> Self {
        self.program_paths = program_paths;
        self
    }

    /// The user's home directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The directory that holds user-level application configuration.
    #[must_use]
    pub fn config_home(&self) -> &Path {
        &self.config_home
    }

    /// The project directory that project-scope configuration lands in.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// The executable to register, which is also the identity doctor compares against.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The configuration file a harness uses at a scope, if it has one.
    #[must_use]
    pub fn config_path(&self, harness: Harness, scope: Scope) -> Option<PathBuf> {
        let path = match (harness, scope) {
            (Harness::Claude, Scope::User) => self.claude_config_dir.join(".claude.json"),
            (Harness::Claude, Scope::Project) => self.project_root.join(".mcp.json"),
            (Harness::Codex, Scope::User) => self.codex_home.join("config.toml"),
            (Harness::Codex, Scope::Project) => return None,
            (Harness::Gemini, Scope::User) => self.home.join(".gemini").join("settings.json"),
            (Harness::Gemini, Scope::Project) => {
                self.project_root.join(".gemini").join("settings.json")
            }
            (Harness::Cursor, Scope::User) => self.home.join(".cursor").join("mcp.json"),
            (Harness::Cursor, Scope::Project) => self.project_root.join(".cursor").join("mcp.json"),
            (Harness::Opencode, Scope::User) => {
                self.config_home.join("opencode").join("opencode.json")
            }
            (Harness::Opencode, Scope::Project) => self.project_root.join("opencode.json"),
            (Harness::Vscode, Scope::User) => self
                .user_application_root()
                .join("Code")
                .join("User")
                .join("mcp.json"),
            (Harness::Vscode, Scope::Project) => self.project_root.join(".vscode").join("mcp.json"),
        };
        Some(path)
    }

    /// Whether the harness appears to be installed: a program on `PATH` or a
    /// configuration location that already exists.
    #[must_use]
    pub fn is_installed(&self, harness: Harness) -> bool {
        if harness
            .detection_programs()
            .iter()
            .any(|name| self.find_program(name).is_some())
        {
            return true;
        }
        match harness {
            Harness::Claude => {
                self.claude_config_dir.join(".claude.json").exists()
                    || self.claude_config_dir.join(".claude").exists()
            }
            Harness::Codex => self.codex_home.exists(),
            Harness::Gemini => self.home.join(".gemini").exists(),
            Harness::Cursor => self.home.join(".cursor").exists(),
            Harness::Opencode => self.config_home.join("opencode").exists(),
            Harness::Vscode => self.user_application_root().join("Code").exists(),
        }
    }

    /// The program that writes this harness configuration, when one exists.
    fn program(&self, harness: Harness) -> Option<PathBuf> {
        let name = harness.configuration_program()?;
        self.find_program(name)
    }

    /// A second configuration file the harness accepts that `DiffScope` cannot
    /// write safely (JSON with comments).
    fn alternate_config_path(&self, harness: Harness, scope: Scope) -> Option<PathBuf> {
        match harness {
            Harness::Opencode => Some(match scope {
                Scope::User => self.config_home.join("opencode").join("opencode.jsonc"),
                Scope::Project => self.project_root.join("opencode.jsonc"),
            }),
            _ => None,
        }
    }

    /// The root that holds per-application state: XDG on Linux, application
    /// support on macOS, roaming application data on Windows.
    fn user_application_root(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library").join("Application Support")
        } else if cfg!(target_os = "windows") {
            self.app_data
                .clone()
                .unwrap_or_else(|| self.home.join("AppData").join("Roaming"))
        } else {
            self.config_home.clone()
        }
    }

    /// Create the directory a harness program stores its configuration in when
    /// it is missing.
    ///
    /// Only codex needs this: it refuses to start when `CODEX_HOME` names a
    /// missing directory, even though it creates that same directory itself
    /// when the variable is unset.
    fn prepare_configuration_directory(&self, harness: Harness) -> Result<(), Error> {
        if harness != Harness::Codex || self.codex_home.is_dir() {
            return Ok(());
        }
        fs::create_dir_all(&self.codex_home).map_err(|error| Error::Io {
            action: "create",
            path: self.codex_home.clone(),
            message: error.to_string(),
        })
    }

    /// Variables handed to harness programs so they read and write the same
    /// configuration this module inspects.
    ///
    /// A variable is exported only when its directory exists: programs such as
    /// codex refuse to start when `CODEX_HOME` names a missing directory, and
    /// their own default already resolves to the same path through `HOME`.
    fn child_environment(&self) -> Vec<(&'static str, PathBuf)> {
        let mut variables = vec![("HOME", self.home.clone())];
        if self.config_home.exists() {
            variables.push(("XDG_CONFIG_HOME", self.config_home.clone()));
        }
        if self.codex_home.exists() {
            variables.push(("CODEX_HOME", self.codex_home.clone()));
        }
        if self.claude_config_dir != self.home && self.claude_config_dir.exists() {
            variables.push(("CLAUDE_CONFIG_DIR", self.claude_config_dir.clone()));
        }
        variables
    }

    fn find_program(&self, name: &str) -> Option<PathBuf> {
        self.program_paths
            .iter()
            .map(|directory| directory.join(name))
            .find(|candidate| is_executable(candidate))
    }
}

/// What [`run`] should do, for which harnesses, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Harnesses to act on.
    pub selection: HarnessSelection,
    /// Scope to configure.
    pub scope: Scope,
    /// Whether to install or remove the entry.
    pub mode: Mode,
    /// Report the plan without touching any configuration.
    pub dry_run: bool,
    /// Replace an entry that launches a different command.
    pub force: bool,
    /// Filesystem and process facts.
    pub environment: Environment,
}

impl Options {
    /// Detected harnesses, user scope, install, nothing forced.
    #[must_use]
    pub fn new(environment: Environment) -> Self {
        Self {
            selection: HarnessSelection::Detected,
            scope: Scope::User,
            mode: Mode::Install,
            dry_run: false,
            force: false,
            environment,
        }
    }
}

/// What [`doctor`] should inspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorOptions {
    /// Harnesses to inspect.
    pub selection: HarnessSelection,
    /// Scope to inspect.
    pub scope: Scope,
    /// Filesystem and process facts.
    pub environment: Environment,
}

impl DoctorOptions {
    /// Every supported harness at user scope.
    #[must_use]
    pub fn new(environment: Environment) -> Self {
        Self {
            selection: HarnessSelection::All,
            scope: Scope::User,
            environment,
        }
    }
}

/// What [`run`] did, one outcome per harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Whether the run only planned changes.
    pub dry_run: bool,
    /// Scope the run configured.
    pub scope: Scope,
    /// Executable that was registered.
    pub executable: PathBuf,
    /// Home directory, used to shorten configured paths when printing.
    pub home: PathBuf,
    /// One outcome per harness, in the order the harnesses were processed.
    pub outcomes: Vec<Outcome>,
}

impl fmt::Display for Report {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = if self.dry_run { "dry-run" } else { "applied" };
        let executable = self.executable.display();
        writeln!(
            formatter,
            "diffscope setup ({mode}, scope {}, executable {executable})",
            self.scope.name()
        )?;
        for outcome in &self.outcomes {
            writeln!(
                formatter,
                "  {:<8} {:<8} {:<12} {:<9} {:<38} {}",
                outcome.harness.name(),
                outcome.scope.name(),
                outcome.action.label(self.dry_run),
                outcome.mechanism_label(),
                outcome.location_label(&self.home),
                outcome.detail
            )?;
        }
        Ok(())
    }
}

/// What happened to one harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Harness the outcome belongs to.
    pub harness: Harness,
    /// Scope that was configured.
    pub scope: Scope,
    /// How the configuration was, or would be, changed.
    pub mechanism: Option<Mechanism>,
    /// Observable result.
    pub action: Action,
    /// Configuration file that was, or would be, touched.
    pub location: Option<PathBuf>,
    /// One sentence explaining the result.
    pub detail: String,
}

impl Outcome {
    fn mechanism_label(&self) -> String {
        match &self.mechanism {
            Some(Mechanism::Cli { program }) => {
                let name = program.file_name().map_or_else(
                    || program.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                format!("cli:{name}")
            }
            Some(Mechanism::ConfigFile) => "config".to_owned(),
            None => "-".to_owned(),
        }
    }

    fn location_label(&self, home: &Path) -> String {
        self.location
            .as_deref()
            .map_or_else(|| "-".to_owned(), |path| shorten(path, home))
    }

    fn summary(&self, dry_run: bool) -> String {
        format!("{} {}", self.harness.name(), self.action.label(dry_run))
    }
}

/// How a harness configuration was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mechanism {
    /// The harness program performed the change.
    Cli {
        /// Program that was invoked.
        program: PathBuf,
    },
    /// `DiffScope` merged one entry into the harness JSON configuration.
    ConfigFile,
}

/// The observable result for one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The entry did not exist and was created.
    Installed,
    /// The entry existed with a different command and was replaced.
    Updated,
    /// The entry already launches this executable.
    Unchanged,
    /// The entry was deleted.
    Removed,
    /// There was no entry to delete.
    NotPresent,
    /// The harness has no configuration for the requested scope.
    Skipped,
}

impl Action {
    fn label(self, dry_run: bool) -> &'static str {
        match (self, dry_run) {
            (Self::Installed, false) => "created",
            (Self::Installed, true) => "would create",
            (Self::Updated, false) => "replaced",
            (Self::Updated, true) => "would update",
            (Self::Unchanged, _) => "unchanged",
            (Self::Removed, false) => "removed",
            (Self::Removed, true) => "would remove",
            (Self::NotPresent, _) => "not present",
            (Self::Skipped, _) => "skipped",
        }
    }
}

/// What [`doctor`] observed, one check per harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// Executable that configurations were compared against.
    pub executable: PathBuf,
    /// Home directory, used to shorten configured paths when printing.
    pub home: PathBuf,
    /// Scope that was inspected.
    pub scope: Scope,
    /// One check per harness.
    pub checks: Vec<HarnessCheck>,
    /// Always `false`: doctor reads configuration files only and never opens an
    /// MCP session, so it never claims a live connection.
    pub live_connection_checked: bool,
}

impl DoctorReport {
    /// Whether every installed harness has an entry that launches this
    /// executable. Harnesses that are not installed, and harnesses that have no
    /// configuration at the inspected scope, do not make the report unhealthy.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.checks.iter().all(|check| {
            !check.installed
                || matches!(
                    check.status,
                    DoctorStatus::Configured | DoctorStatus::UnsupportedScope
                )
        })
    }

    fn summary(&self) -> String {
        let mut parts: Vec<String> = DoctorStatus::ALL
            .into_iter()
            .filter_map(|status| {
                let count = self
                    .checks
                    .iter()
                    .filter(|check| check.status == status)
                    .count();
                (count > 0).then(|| format!("{count} {}", status.label()))
            })
            .collect();
        let missing = self.checks.iter().filter(|check| !check.installed).count();
        if missing > 0 {
            parts.push(format!("{missing} not installed"));
        }
        parts.join(", ")
    }
}

impl fmt::Display for DoctorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let executable = self.executable.display();
        writeln!(
            formatter,
            "diffscope doctor (scope {}, executable {executable})",
            self.scope.name()
        )?;
        for check in &self.checks {
            let location = check
                .location
                .as_deref()
                .map_or_else(|| "-".to_owned(), |path| shorten(path, &self.home));
            writeln!(
                formatter,
                "  {:<8} {:<8} {:<13} {:<10} {:<38} {}",
                check.harness.name(),
                check.scope.name(),
                check.status.label(),
                check.installation_label(),
                location,
                check.detail
            )?;
        }
        writeln!(formatter, "  {}", self.summary())?;
        writeln!(
            formatter,
            "  configuration presence only: no live MCP handshake was performed"
        )
    }
}

/// The state of one harness configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCheck {
    /// Harness that was inspected.
    pub harness: Harness,
    /// Scope that was inspected.
    pub scope: Scope,
    /// Whether the harness appears to be installed.
    pub installed: bool,
    /// Configuration file that was inspected.
    pub location: Option<PathBuf>,
    /// What the configuration contains.
    pub status: DoctorStatus,
    /// Configured command line, when the entry was readable.
    pub command: Option<Vec<String>>,
    /// One sentence explaining the status.
    pub detail: String,
}

impl HarnessCheck {
    fn installation_label(&self) -> &'static str {
        if self.installed {
            "installed"
        } else {
            "not found"
        }
    }
}

/// What doctor found in one harness configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    /// The entry launches this executable.
    Configured,
    /// The entry exists but launches a different command.
    Stale,
    /// No entry exists for this harness.
    Missing,
    /// An entry exists but is not a stdio command entry.
    Unrecognized,
    /// The configuration file could not be read or parsed.
    Invalid,
    /// The harness has no configuration at the requested scope.
    UnsupportedScope,
}

impl DoctorStatus {
    /// Every status, in reporting order.
    const ALL: [Self; 6] = [
        Self::Configured,
        Self::Missing,
        Self::Stale,
        Self::Unrecognized,
        Self::Invalid,
        Self::UnsupportedScope,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Stale => "stale",
            Self::Missing => "missing",
            Self::Unrecognized => "unrecognized",
            Self::Invalid => "invalid",
            Self::UnsupportedScope => "unsupported",
        }
    }
}

/// Everything that can stop setup or doctor, with actionable context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The process environment does not provide what setup needs.
    Environment {
        /// What is missing and what to do about it.
        message: String,
    },
    /// The requested harness list is empty or contains an unknown name.
    Selection {
        /// What was requested and which values are accepted.
        message: String,
    },
    /// No supported harness was detected on this machine.
    NoHarnessDetected,
    /// A harness has no configuration at the requested scope.
    Unsupported {
        /// Scope that was requested.
        scope: Scope,
        /// What to do instead.
        advice: String,
    },
    /// One or more harnesses could not be configured.
    Harness {
        /// One entry per failed harness, each naming the harness and the cause.
        failures: Vec<String>,
        /// Harnesses that were already handled successfully.
        completed: Vec<String>,
    },
    /// A configuration file exists but cannot be used safely.
    Configuration {
        /// File that was rejected.
        path: PathBuf,
        /// Why it was rejected and what to do about it.
        message: String,
    },
    /// A filesystem or subprocess operation failed.
    Io {
        /// Verb describing the failed operation.
        action: &'static str,
        /// Path or program the operation was applied to.
        path: PathBuf,
        /// Operating system or process diagnostic.
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment { message } | Self::Selection { message } => {
                write!(formatter, "{message}")
            }
            Self::NoHarnessDetected => write!(
                formatter,
                "no supported harness detected; pass --harness all or a comma-separated list of {}",
                Harness::supported_names()
            ),
            Self::Unsupported { scope, advice } => {
                write!(
                    formatter,
                    "nothing to configure at {}-scope: {advice}",
                    scope.name()
                )
            }
            Self::Harness {
                failures,
                completed,
            } => {
                write!(formatter, "{}", failures.join("; "))?;
                if !completed.is_empty() {
                    write!(formatter, " (already handled: {})", completed.join(", "))?;
                }
                Ok(())
            }
            Self::Configuration { path, message } => {
                let path = path.display();
                write!(formatter, "invalid configuration at {path}: {message}")
            }
            Self::Io {
                action,
                path,
                message,
            } => {
                let path = path.display();
                write!(formatter, "could not {action} {path}: {message}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Register (or remove) the `DiffScope` MCP server entry for every selected harness.
///
/// # Errors
///
/// Returns [`Error::Environment`], [`Error::Selection`], or
/// [`Error::NoHarnessDetected`] when the request itself cannot be honored, and
/// [`Error::Harness`] when one or more harnesses could not be configured. Every
/// per-harness failure names the file or program involved and what to do next;
/// harnesses handled before the failure are listed in the same message.
pub fn run(options: &Options) -> Result<Report, Error> {
    let harnesses = resolve_harnesses(&options.selection, &options.environment)?;
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();
    let mut completed = Vec::new();
    for harness in harnesses {
        let result = match unsupported_scope_advice(harness, options.scope) {
            Some(advice) if options.selection.is_named() => Err(Error::Unsupported {
                scope: options.scope,
                advice,
            }),
            Some(advice) => Ok(Outcome {
                harness,
                scope: options.scope,
                mechanism: None,
                action: Action::Skipped,
                location: options.environment.config_path(harness, options.scope),
                detail: advice,
            }),
            None => match options.mode {
                Mode::Install => install(options, harness),
                Mode::Remove => remove(options, harness),
            },
        };
        match result {
            Ok(outcome) => {
                completed.push(outcome.summary(options.dry_run));
                outcomes.push(outcome);
            }
            Err(error) => failures.push(format!("{}: {error}", harness.name())),
        }
    }
    if !failures.is_empty() {
        return Err(Error::Harness {
            failures,
            completed,
        });
    }
    Ok(Report {
        dry_run: options.dry_run,
        scope: options.scope,
        executable: options.environment.executable().to_path_buf(),
        home: options.environment.home().to_path_buf(),
        outcomes,
    })
}

/// Report whether every installed harness launches this executable.
///
/// Doctor never writes configuration and never opens an MCP session: it reads
/// the files setup writes and compares the configured command with the running
/// executable. Configuration problems are reported per harness rather than
/// raised, so a single broken file still produces a complete report.
///
/// # Errors
///
/// Returns [`Error::Environment`], [`Error::Selection`], or
/// [`Error::NoHarnessDetected`] when the inspected selection itself is invalid.
pub fn doctor(options: &DoctorOptions) -> Result<DoctorReport, Error> {
    let harnesses = resolve_harnesses(&options.selection, &options.environment)?;
    let command = CommandLine::for_executable(options.environment.executable());
    let mut checks = Vec::new();
    for harness in harnesses {
        let installed = options.environment.is_installed(harness);
        let Some(location) = options.environment.config_path(harness, options.scope) else {
            checks.push(HarnessCheck {
                harness,
                scope: options.scope,
                installed,
                location: None,
                status: DoctorStatus::UnsupportedScope,
                command: None,
                detail: unsupported_scope_advice(harness, options.scope).unwrap_or_default(),
            });
            continue;
        };
        let (status, configured, detail) = match read_entry_state(&location, harness.format()) {
            Ok(EntryState::Command(words)) if command.matches(&words) => (
                DoctorStatus::Configured,
                Some(words),
                "entry launches this executable".to_owned(),
            ),
            Ok(EntryState::Command(words)) => (
                DoctorStatus::Stale,
                Some(words.clone()),
                format!(
                    "entry launches `{}` instead of `{}`; rerun `diffscope setup --force`",
                    words.join(" "),
                    command.words().join(" ")
                ),
            ),
            Ok(EntryState::Absent) if installed => (
                DoctorStatus::Missing,
                None,
                "no entry; run `diffscope setup`".to_owned(),
            ),
            Ok(EntryState::Absent) => (
                DoctorStatus::Missing,
                None,
                "harness does not appear to be installed".to_owned(),
            ),
            Ok(EntryState::Unrecognized(reason)) => (DoctorStatus::Unrecognized, None, reason),
            Err(error) => (DoctorStatus::Invalid, None, error.to_string()),
        };
        checks.push(HarnessCheck {
            harness,
            scope: options.scope,
            installed,
            location: Some(location),
            status,
            command: configured,
            detail,
        });
    }
    Ok(DoctorReport {
        executable: options.environment.executable().to_path_buf(),
        home: options.environment.home().to_path_buf(),
        scope: options.scope,
        checks,
        live_connection_checked: false,
    })
}

/// Create or reconcile the entry for one JSON or CLI-backed harness.
fn install(options: &Options, harness: Harness) -> Result<Outcome, Error> {
    let (location, format, command, cli) = prepare(options, harness)?;
    if let Some(alternate) =
        json_only_guard(&options.environment, harness, options.scope, &location)
    {
        return Err(Error::Configuration {
            path: alternate,
            message: format!(
                "DiffScope merges JSON only; add a \"{SERVER_NAME}\" entry to this file by hand or \
                 rename it to {}",
                file_name(&location)
            ),
        });
    }
    if cli.is_none() && matches!(format, Format::Toml) {
        return Err(missing_configuration_program(&location));
    }
    let state = read_entry_state(&location, format)?;
    let had_entry = state != EntryState::Absent;
    let mechanism = cli
        .as_deref()
        .map_or(Mechanism::ConfigFile, change_mechanism);
    if state.matches(&command) {
        return Ok(outcome(
            options,
            harness,
            Some(mechanism),
            Action::Unchanged,
            location,
            format!("entry already launches `{}`", command.words().join(" ")),
        ));
    }
    if had_entry && !options.force {
        let description = state.describe().unwrap_or_default();
        return Err(Error::Configuration {
            path: location,
            message: format!(
                "a \"{SERVER_NAME}\" entry {description}; rerun with --force to replace it"
            ),
        });
    }
    if options.dry_run {
        return Ok(outcome(
            options,
            harness,
            Some(mechanism),
            replacement(had_entry),
            location,
            format!(
                "dry run: the \"{SERVER_NAME}\" entry would launch `{}`",
                command.words().join(" ")
            ),
        ));
    }
    apply_install(
        options,
        harness,
        &location,
        format,
        &command,
        cli.as_deref(),
        had_entry,
    )
}

/// Perform an install that is known to be needed.
fn apply_install(
    options: &Options,
    harness: Harness,
    location: &Path,
    format: Format,
    command: &CommandLine,
    cli: Option<&Path>,
    had_entry: bool,
) -> Result<Outcome, Error> {
    if let Some(program) = cli {
        options
            .environment
            .prepare_configuration_directory(harness)?;
        if had_entry {
            run_program(
                &options.environment,
                program,
                &remove_arguments(harness, options.scope),
            )?;
        }
        run_program(
            &options.environment,
            program,
            &install_arguments(harness, options.scope, command),
        )?;
        let detail = verify_install(location, format, command)?;
        return Ok(outcome(
            options,
            harness,
            Some(change_mechanism(program)),
            replacement(had_entry),
            location.to_path_buf(),
            detail,
        ));
    }
    let Format::Json(layout) = format else {
        return Err(missing_configuration_program(location));
    };
    write_entry(location, layout, command, had_entry)?;
    Ok(outcome(
        options,
        harness,
        Some(Mechanism::ConfigFile),
        replacement(had_entry),
        location.to_path_buf(),
        format!("wrote the \"{SERVER_NAME}\" entry"),
    ))
}

/// Delete the entry for one JSON or CLI-backed harness.
fn remove(options: &Options, harness: Harness) -> Result<Outcome, Error> {
    let (location, format, _, cli) = prepare(options, harness)?;
    let state = read_entry_state(&location, format)?;
    if state == EntryState::Absent {
        return Ok(outcome(
            options,
            harness,
            None,
            Action::NotPresent,
            location,
            format!("no \"{SERVER_NAME}\" entry to remove"),
        ));
    }
    if options.dry_run {
        let mechanism = cli
            .as_deref()
            .map_or(Mechanism::ConfigFile, change_mechanism);
        return Ok(outcome(
            options,
            harness,
            Some(mechanism),
            Action::Removed,
            location,
            format!("dry run: the \"{SERVER_NAME}\" entry would be removed"),
        ));
    }
    if let Some(program) = cli {
        options
            .environment
            .prepare_configuration_directory(harness)?;
        run_program(
            &options.environment,
            &program,
            &remove_arguments(harness, options.scope),
        )?;
        let detail = match read_entry_state(&location, format) {
            Ok(EntryState::Absent) => format!("removed and verified in {}", file_name(&location)),
            Ok(_) => match format {
                Format::Json(layout) => {
                    remove_entry(&location, layout)?;
                    format!(
                        "the harness program left the \"{SERVER_NAME}\" entry in place; DiffScope \
                         removed it directly from {}",
                        file_name(&location)
                    )
                }
                Format::Toml => {
                    return Err(Error::Configuration {
                        path: location,
                        message: format!(
                            "the harness program reported success but the \"{SERVER_NAME}\" entry \
                             is still present in its configuration"
                        ),
                    });
                }
            },
            Err(_) => format!(
                "removed by the harness program; {} could not be verified",
                file_name(&location)
            ),
        };
        return Ok(outcome(
            options,
            harness,
            Some(change_mechanism(&program)),
            Action::Removed,
            location,
            detail,
        ));
    }
    let Format::Json(layout) = format else {
        return Err(missing_configuration_program(&location));
    };
    remove_entry(&location, layout)?;
    Ok(outcome(
        options,
        harness,
        Some(Mechanism::ConfigFile),
        Action::Removed,
        location,
        format!("removed the \"{SERVER_NAME}\" entry"),
    ))
}

/// The configuration target, entry format, desired command line, and the
/// program that owns the file, for one harness at one scope.
fn prepare(
    options: &Options,
    harness: Harness,
) -> Result<(PathBuf, Format, CommandLine, Option<PathBuf>), Error> {
    let Some(location) = options.environment.config_path(harness, options.scope) else {
        return Err(Error::Unsupported {
            scope: options.scope,
            advice: unsupported_scope_advice(harness, options.scope).unwrap_or_default(),
        });
    };
    let format = harness.format();
    let command = CommandLine::for_executable(options.environment.executable());
    let cli = options.environment.program(harness);
    Ok((location, format, command, cli))
}

fn outcome(
    options: &Options,
    harness: Harness,
    mechanism: Option<Mechanism>,
    action: Action,
    location: PathBuf,
    detail: String,
) -> Outcome {
    Outcome {
        harness,
        scope: options.scope,
        mechanism,
        action,
        location: Some(location),
        detail,
    }
}

/// The action that describes creating versus replacing an entry.
fn replacement(had_entry: bool) -> Action {
    if had_entry {
        Action::Updated
    } else {
        Action::Installed
    }
}

/// Decide which harnesses a command acts on.
fn resolve_harnesses(
    selection: &HarnessSelection,
    environment: &Environment,
) -> Result<Vec<Harness>, Error> {
    match selection {
        HarnessSelection::All => Ok(Harness::ALL.to_vec()),
        HarnessSelection::Named(named) if named.is_empty() => Err(Error::Selection {
            message: format!(
                "no harness names given; pass --harness all or a comma-separated list ({})",
                Harness::supported_names()
            ),
        }),
        HarnessSelection::Named(named) => Ok(named.clone()),
        HarnessSelection::Detected => {
            let detected: Vec<Harness> = Harness::ALL
                .into_iter()
                .filter(|harness| environment.is_installed(*harness))
                .collect();
            if detected.is_empty() {
                return Err(Error::NoHarnessDetected);
            }
            Ok(detected)
        }
    }
}

/// Why a harness cannot be configured at a scope, when it cannot.
fn unsupported_scope_advice(harness: Harness, scope: Scope) -> Option<String> {
    if harness.supports_scope(scope) {
        return None;
    }
    Some(format!(
        "the {} program keeps MCP servers in its user configuration only; rerun with --scope user",
        harness.name()
    ))
}

/// A configuration file the harness accepts that cannot be merged safely.
fn json_only_guard(
    environment: &Environment,
    harness: Harness,
    scope: Scope,
    location: &Path,
) -> Option<PathBuf> {
    let alternate = environment.alternate_config_path(harness, scope)?;
    (alternate.exists() && !location.exists()).then_some(alternate)
}

/// The error raised when a harness has no program that can write its configuration.
fn missing_configuration_program(location: &Path) -> Error {
    Error::Configuration {
        path: location.to_path_buf(),
        message: "install the Codex CLI so `codex` is on PATH; MCP entries for codex are written \
                  by `codex mcp add`"
            .to_owned(),
    }
}

/// The mechanism that changes, or would change, a configuration.
fn change_mechanism(program: &Path) -> Mechanism {
    Mechanism::Cli {
        program: program.to_path_buf(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Arguments that add the entry through the harness program.
fn install_arguments(harness: Harness, scope: Scope, command: &CommandLine) -> Vec<String> {
    let mut arguments = if harness == Harness::Codex {
        vec![
            "mcp".to_owned(),
            "add".to_owned(),
            SERVER_NAME.to_owned(),
            "--".to_owned(),
        ]
    } else {
        vec![
            "mcp".to_owned(),
            "add".to_owned(),
            "--scope".to_owned(),
            scope.name().to_owned(),
            SERVER_NAME.to_owned(),
        ]
    };
    if harness == Harness::Claude {
        arguments.push("--".to_owned());
    }
    arguments.extend(command.words());
    arguments
}

/// Arguments that delete the entry through the harness program.
fn remove_arguments(harness: Harness, scope: Scope) -> Vec<String> {
    if harness == Harness::Codex {
        return vec![
            "mcp".to_owned(),
            "remove".to_owned(),
            SERVER_NAME.to_owned(),
        ];
    }
    vec![
        "mcp".to_owned(),
        "remove".to_owned(),
        "--scope".to_owned(),
        scope.name().to_owned(),
        SERVER_NAME.to_owned(),
    ]
}

/// Run one harness program with the environment it expects.
fn run_program(
    environment: &Environment,
    program: &Path,
    arguments: &[String],
) -> Result<(), Error> {
    let mut child = Command::new(program);
    child
        .args(arguments)
        .current_dir(environment.project_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in environment.child_environment() {
        child.env(key, value);
    }
    let output = child.output().map_err(|error| Error::Io {
        action: "run",
        path: program.to_path_buf(),
        message: error.to_string(),
    })?;
    if output.status.success() {
        return Ok(());
    }
    let status = output.status;
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    let diagnostics = diagnostics.trim();
    let detail = if diagnostics.is_empty() {
        status.to_string()
    } else {
        format!("{status}: {diagnostics}")
    };
    Err(Error::Io {
        action: "run",
        path: program.to_path_buf(),
        message: format!(
            "`{} {}` reported {detail}",
            file_name(program),
            arguments.join(" ")
        ),
    })
}

/// Confirm that a harness program really wrote the entry it claimed to write.
///
/// A program that reports success without changing the configuration is not
/// trusted: for JSON harnesses the entry is written directly instead (the
/// actual `gemini` CLI, for example, refuses to remove a project-scope entry
/// it can list), and for codex, whose TOML is owned by the program, a
/// disagreement is an error.
fn verify_install(location: &Path, format: Format, command: &CommandLine) -> Result<String, Error> {
    let name = file_name(location);
    match read_entry_state(location, format) {
        Ok(EntryState::Command(words)) if command.matches(&words) => Ok(format!(
            "wrote and verified the \"{SERVER_NAME}\" entry in {name}"
        )),
        Ok(state) => match format {
            Format::Json(layout) => {
                write_entry(
                    location,
                    layout,
                    command,
                    matches!(state, EntryState::Command(_)),
                )?;
                Ok(format!(
                    "the harness program left {name} unchanged; DiffScope wrote the \
                     \"{SERVER_NAME}\" entry directly"
                ))
            }
            Format::Toml if state == EntryState::Absent => Ok(format!(
                "the harness program reported success; {name} does not show the \
                 \"{SERVER_NAME}\" entry"
            )),
            Format::Toml => Err(Error::Configuration {
                path: location.to_path_buf(),
                message: format!(
                    "the harness program reported success but the \"{SERVER_NAME}\" entry in \
                     {name} still does not launch this executable; rerun with --force"
                ),
            }),
        },
        Err(_) => Ok(format!(
            "the harness program reported success; {name} could not be read back for verification"
        )),
    }
}

/// The state of the `diffscope` entry inside one configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryState {
    /// No file, or no entry with this name.
    Absent,
    /// The entry launches the given command line.
    Command(Vec<String>),
    /// The entry exists but is not a stdio command entry.
    Unrecognized(String),
}

impl EntryState {
    fn matches(&self, command: &CommandLine) -> bool {
        matches!(self, Self::Command(words) if command.matches(words))
    }

    fn describe(&self) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::Command(words) => Some(format!("currently launches `{}`", words.join(" "))),
            Self::Unrecognized(reason) => Some(format!("is present but {reason}")),
        }
    }
}

/// How one harness stores MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Json(JsonLayout),
    Toml,
}

/// Where a JSON configuration keeps its MCP servers and what one looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsonLayout {
    container: &'static str,
    style: EntryStyle,
}

/// The shape of one MCP server entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryStyle {
    /// `{"command": "<program>", "args": [...], "type": "stdio"?}`
    CommandArgs {
        /// Whether the harness requires an explicit `type`.
        declare_type: bool,
    },
    /// `{"type": "local", "command": ["<program>", ...], "enabled": true}`
    LocalCommand,
}

/// A command line that launches the `DiffScope` MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandLine {
    program: PathBuf,
    arguments: Vec<String>,
}

impl CommandLine {
    fn for_executable(executable: &Path) -> Self {
        Self {
            program: executable.to_path_buf(),
            arguments: vec![MCP_ARGUMENT.to_owned()],
        }
    }

    fn words(&self) -> Vec<String> {
        std::iter::once(self.program.display().to_string())
            .chain(self.arguments.iter().cloned())
            .collect()
    }

    /// Whether a configured command line launches this program and arguments.
    fn matches(&self, words: &[String]) -> bool {
        let Some((program, arguments)) = words.split_first() else {
            return false;
        };
        if self.arguments.as_slice() != arguments {
            return false;
        }
        self.program == Path::new(program)
            || canonical_string(&self.program) == canonical_string(Path::new(program))
    }
}

/// Read the state of the entry one harness would use.
fn read_entry_state(path: &Path, format: Format) -> Result<EntryState, Error> {
    match format {
        Format::Json(layout) => match load_document(path)? {
            None => Ok(EntryState::Absent),
            Some(document) => json_entry_state(&document, path, layout),
        },
        Format::Toml => match read_text(path)? {
            None => Ok(EntryState::Absent),
            Some(text) => Ok(toml_entry_state(&text)),
        },
    }
}

/// Read a harness JSON configuration, treating an absent or blank file as an
/// empty document.
fn load_document(path: &Path) -> Result<Option<Value>, Error> {
    let Some(bytes) = read_bytes(path)? else {
        return Ok(None);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(Value::Object(Map::new())));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| Error::Configuration {
            path: path.to_path_buf(),
            message: format!("invalid JSON: {error}"),
        })
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io {
            action: "read",
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

fn read_text(path: &Path) -> Result<Option<String>, Error> {
    let Some(bytes) = read_bytes(path)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| Error::Configuration {
            path: path.to_path_buf(),
            message: "configuration is not valid UTF-8".to_owned(),
        })
}

/// Extract the entry from a parsed JSON document.
fn json_entry_state(
    document: &Value,
    path: &Path,
    layout: JsonLayout,
) -> Result<EntryState, Error> {
    let Some(root) = document.as_object() else {
        return Err(Error::Configuration {
            path: path.to_path_buf(),
            message: "the configuration root is not a JSON object".to_owned(),
        });
    };
    let Some(container) = root.get(layout.container) else {
        return Ok(EntryState::Absent);
    };
    let Some(container) = container.as_object() else {
        return Err(Error::Configuration {
            path: path.to_path_buf(),
            message: format!("\"{}\" is not a JSON object", layout.container),
        });
    };
    match container.get(SERVER_NAME) {
        None => Ok(EntryState::Absent),
        Some(entry) => Ok(match entry_words(layout.style, entry) {
            Ok(words) => EntryState::Command(words),
            Err(reason) => EntryState::Unrecognized(reason),
        }),
    }
}

/// The command line an entry launches, or why it does not launch one.
fn entry_words(style: EntryStyle, entry: &Value) -> Result<Vec<String>, String> {
    let object = entry
        .as_object()
        .ok_or_else(|| "the entry is not a JSON object".to_owned())?;
    match style {
        EntryStyle::CommandArgs { .. } => {
            let program = object
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "the entry has no string \"command\"".to_owned())?;
            let arguments = string_array(object.get("args"))
                .map_err(|reason| format!("the entry \"args\" {reason}"))?
                .unwrap_or_default();
            let mut words = vec![program.to_owned()];
            words.extend(arguments);
            Ok(words)
        }
        EntryStyle::LocalCommand => {
            if object.get("enabled").and_then(Value::as_bool) == Some(false) {
                return Err("the entry is disabled (\"enabled\": false)".to_owned());
            }
            string_array(object.get("command"))?
                .ok_or_else(|| "the entry has no \"command\" array".to_owned())
        }
    }
}

fn string_array(value: Option<&Value>) -> Result<Option<Vec<String>>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "must contain only strings".to_owned())
            })
            .collect::<Result<Vec<String>, String>>()
            .map(Some),
        Some(_) => Err("must be an array of strings".to_owned()),
    }
}

/// Merge the entry into a JSON configuration and replace the file atomically.
fn write_entry(
    path: &Path,
    layout: JsonLayout,
    command: &CommandLine,
    preserve: bool,
) -> Result<(), Error> {
    let mut document = load_document(path)?.unwrap_or_else(|| Value::Object(Map::new()));
    let root = document
        .as_object_mut()
        .ok_or_else(|| Error::Configuration {
            path: path.to_path_buf(),
            message: "the configuration root is not a JSON object".to_owned(),
        })?;
    let container = root
        .entry(layout.container.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let container = container
        .as_object_mut()
        .ok_or_else(|| Error::Configuration {
            path: path.to_path_buf(),
            message: format!("\"{}\" is not a JSON object", layout.container),
        })?;
    let replace = match container.get_mut(SERVER_NAME) {
        Some(Value::Object(existing)) if preserve => {
            for (key, value) in entry_fields(layout.style, command) {
                existing.insert(key, value);
            }
            false
        }
        _ => true,
    };
    if replace {
        container.insert(
            SERVER_NAME.to_owned(),
            Value::Object(entry_fields(layout.style, command)),
        );
    }
    write_json(path, &document)
}

/// Delete the entry from a JSON configuration and replace the file atomically.
fn remove_entry(path: &Path, layout: JsonLayout) -> Result<(), Error> {
    let mut document = load_document(path)?.ok_or_else(|| Error::Configuration {
        path: path.to_path_buf(),
        message: format!("the configuration disappeared while removing the {SERVER_NAME} entry"),
    })?;
    if let Some(root) = document.as_object_mut()
        && let Some(container) = root
            .get_mut(layout.container)
            .and_then(Value::as_object_mut)
    {
        let _removed = container.remove(SERVER_NAME);
    }
    write_json(path, &document)
}

fn entry_fields(style: EntryStyle, command: &CommandLine) -> Map<String, Value> {
    let mut fields = Map::new();
    let program = command.program.display().to_string();
    match style {
        EntryStyle::CommandArgs { declare_type } => {
            if declare_type {
                fields.insert("type".to_owned(), Value::String("stdio".to_owned()));
            }
            fields.insert("command".to_owned(), Value::String(program));
            fields.insert(
                "args".to_owned(),
                Value::Array(
                    command
                        .arguments
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        EntryStyle::LocalCommand => {
            fields.insert("type".to_owned(), Value::String("local".to_owned()));
            fields.insert(
                "command".to_owned(),
                Value::Array(command.words().into_iter().map(Value::String).collect()),
            );
            fields.insert("enabled".to_owned(), Value::Bool(true));
        }
    }
    fields
}

fn write_json(path: &Path, document: &Value) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| Error::Configuration {
        path: path.to_path_buf(),
        message: format!("could not serialize the configuration: {error}"),
    })?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

/// Replace a file by writing a temporary sibling and renaming it into place, so
/// a failed write never leaves a truncated configuration behind.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), Error> {
    let Some(directory) = path.parent() else {
        return Err(Error::Io {
            action: "resolve the parent of",
            path: path.to_path_buf(),
            message: "the path has no parent directory".to_owned(),
        });
    };
    fs::create_dir_all(directory).map_err(|error| Error::Io {
        action: "create",
        path: directory.to_path_buf(),
        message: error.to_string(),
    })?;
    let temporary = directory.join(format!(
        "{TEMPORARY_PREFIX}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| Error::Io {
            action: "create",
            path: temporary.clone(),
            message: error.to_string(),
        })?;
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(Error::Io {
            action: "write",
            path: path.to_path_buf(),
            message: error.to_string(),
        });
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(Error::Io {
            action: "replace",
            path: path.to_path_buf(),
            message: error.to_string(),
        });
    }
    Ok(())
}

/// Extract the entry from a Codex `config.toml` without parsing the whole file.
fn toml_entry_state(text: &str) -> EntryState {
    let mut found = false;
    let mut command: Option<String> = None;
    let mut arguments: Option<Vec<String>> = None;
    let mut problem: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if found {
                break;
            }
            found = is_entry_section(trimmed);
            continue;
        }
        if !found || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        match key.trim() {
            "command" => match toml_string(value.trim()) {
                Some(program) => command = Some(program),
                None => {
                    problem.get_or_insert_with(|| {
                        format!("could not parse the \"command\" value {}", value.trim())
                    });
                }
            },
            "args" => match toml_string_array(value.trim()) {
                Some(parsed) => arguments = Some(parsed),
                None => {
                    problem.get_or_insert_with(|| {
                        format!("could not parse the \"args\" value {}", value.trim())
                    });
                }
            },
            _ => {}
        }
    }
    if !found {
        return EntryState::Absent;
    }
    if let Some(reason) = problem {
        return EntryState::Unrecognized(reason);
    }
    match command {
        None => EntryState::Unrecognized("the entry has no \"command\" key".to_owned()),
        Some(program) => {
            let mut words = vec![program];
            words.extend(arguments.unwrap_or_default());
            EntryState::Command(words)
        }
    }
}

/// Whether a TOML section header names the entry, tolerating quotes and spaces.
fn is_entry_section(header: &str) -> bool {
    let Some(inner) = header
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let normalized: String = inner
        .chars()
        .filter(|character| *character != '"' && !character.is_whitespace())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    normalized == "mcp_servers.diffscope"
}

fn toml_string(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('"') {
        let mut parsed = String::new();
        let mut characters = rest.chars();
        while let Some(character) = characters.next() {
            match character {
                '"' => return Some(parsed),
                '\\' => match characters.next()? {
                    'n' => parsed.push('\n'),
                    't' => parsed.push('\t'),
                    'r' => parsed.push('\r'),
                    '"' => parsed.push('"'),
                    '\\' => parsed.push('\\'),
                    _ => return None,
                },
                other => parsed.push(other),
            }
        }
        return None;
    }
    let rest = value.strip_prefix('\'')?;
    rest.split_once('\'').map(|(text, _)| text.to_owned())
}

fn toml_string_array(value: &str) -> Option<Vec<String>> {
    let inner = value.trim().strip_prefix('[')?;
    let body = &inner[..inner.find(']')?];
    if body.trim().is_empty() {
        return Some(Vec::new());
    }
    body.split(',')
        .map(|item| toml_string(item.trim()))
        .collect()
}

/// Shorten a configured path relative to the home directory for printing.
fn canonical_string(path: &Path) -> String {
    fs::canonicalize(path).map_or_else(
        |_| path.display().to_string(),
        |resolved| resolved.display().to_string(),
    )
}

fn resolve(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Print a path relative to the home directory, the way the user would type it.
fn shorten(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}
