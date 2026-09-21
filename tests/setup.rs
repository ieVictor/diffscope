//! Behavioral tests for harness setup and doctor.
//!
//! Every test drives the public entry points (`setup::run`, `setup::doctor`)
//! against a pinned [`setup::Environment`], so the assertions are about files a
//! real harness would read: what the merge kept, what the conflict did not
//! touch, what the removal left behind.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::{Value, json};

// The module is compiled into this test crate so the tests can run against a
// pinned environment without a library-level re-export. `pub` keeps the items
// the tests do not call reachable, matching how the library exposes them.
#[path = "../src/setup.rs"]
pub mod setup;

use setup::{
    Action, DoctorOptions, DoctorStatus, Environment, Error, Harness, HarnessSelection, Mechanism,
    Mode, Options, SERVER_NAME, Scope,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A throwaway home, configuration root, project, and program directory.
struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    config_home: PathBuf,
    project: PathBuf,
    programs: PathBuf,
    executable: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let unique = format!(
            "{label}-{}-{}",
            process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let base = std::env::temp_dir().join("diffscope-setup-tests");
        let root = fs::canonicalize(create_directory(&base.join(unique))).expect("canonical root");
        let home = create_directory(&root.join("home"));
        let config_home = create_directory(&root.join("config"));
        let project = create_directory(&root.join("project"));
        let programs = create_directory(&root.join("programs"));
        let executable = root.join("diffscope");
        write_file(&executable, "#!/bin/sh\nexit 0\n");
        make_executable(&executable);
        Self {
            root,
            home,
            config_home,
            project,
            programs,
            executable,
        }
    }

    /// The environment a CLI would build, with `programs` as the only `PATH` entry.
    fn environment(&self) -> Environment {
        Environment::injected(
            self.home.clone(),
            self.config_home.clone(),
            self.project.clone(),
            self.executable.clone(),
        )
        .with_program_paths(vec![self.programs.clone()])
    }

    /// A fake harness program that logs its working directory and arguments,
    /// then writes the files a real program would write.
    #[cfg(unix)]
    fn fake_program(&self, name: &str, log: &Path, writes: &[(&Path, String)]) {
        let mut script = String::from("#!/bin/sh\nset -eu\n");
        writeln!(script, "printf 'cwd=%s\\n' \"$PWD\" >> {}", quoted(log)).expect("script");
        writeln!(
            script,
            "for argument in \"$@\"; do printf 'arg=%s\\n' \"$argument\" >> {}; done",
            quoted(log)
        )
        .expect("script");
        for (path, contents) in writes {
            let parent = path.parent().expect("configuration has a parent");
            writeln!(script, "mkdir -p {}", quoted(parent)).expect("script");
            writeln!(
                script,
                "cat > {} <<'DIFFSCOPE_EOF'\n{contents}DIFFSCOPE_EOF",
                quoted(path)
            )
            .expect("script");
        }
        let program = self.programs.join(name);
        write_file(&program, &script);
        make_executable(&program);
    }

    /// The entry `DiffScope` must write for one harness, as JSON.
    fn expected_entry(&self, harness: Harness) -> Value {
        let executable = self.executable.display().to_string();
        match harness {
            Harness::Opencode => json!({
                "type": "local",
                "command": [executable, "mcp"],
                "enabled": true,
            }),
            Harness::Vscode => json!({
                "type": "stdio",
                "command": executable,
                "args": ["mcp"],
            }),
            _ => json!({"command": executable, "args": ["mcp"]}),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn create_directory(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("directory is created");
    path.to_path_buf()
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory is created");
    }
    fs::write(path, contents).expect("file is written");
}

fn read_file(path: &Path) -> String {
    fs::read_to_string(path).expect("configuration is readable")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read_file(path)).expect("configuration is valid JSON")
}

fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("directory is readable")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn log_lines(log: &Path) -> Vec<String> {
    read_file(log).lines().map(str::to_owned).collect()
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("permissions are set");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[cfg(unix)]
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// Options for one harness at one scope, in install mode.
fn install_options(environment: Environment, harnesses: Vec<Harness>, scope: Scope) -> Options {
    Options {
        selection: HarnessSelection::Named(harnesses),
        scope,
        mode: Mode::Install,
        dry_run: false,
        force: false,
        environment,
    }
}

fn error_message(error: &Error) -> String {
    error.to_string()
}

#[test]
fn user_scope_install_merges_the_entry_and_preserves_unrelated_content() {
    let sandbox = Sandbox::new("cursor-merge");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Cursor, Scope::User)
        .expect("cursor has user configuration");
    write_file(
        &config,
        &format!(
            "{}\n",
            json!({
                "mcpServers": {"pencil": {"command": "/usr/bin/pen", "args": ["serve"]}},
                "theme": "dark",
                "editor": {"tabSize": 2},
            })
        ),
    );

    let report = setup::run(&install_options(
        environment,
        vec![Harness::Cursor],
        Scope::User,
    ))
    .expect("setup succeeds");

    assert_eq!(report.outcomes.len(), 1);
    let outcome = &report.outcomes[0];
    assert_eq!(outcome.action, Action::Installed);
    assert_eq!(outcome.mechanism, Some(Mechanism::ConfigFile));
    assert_eq!(outcome.location.as_deref(), Some(config.as_path()));

    let written = read_json(&config);
    assert_eq!(written["theme"], json!("dark"));
    assert_eq!(written["editor"], json!({"tabSize": 2}));
    assert_eq!(
        written["mcpServers"]["pencil"],
        json!({"command": "/usr/bin/pen", "args": ["serve"]})
    );
    assert_eq!(
        written["mcpServers"][SERVER_NAME],
        sandbox.expected_entry(Harness::Cursor)
    );
    assert_eq!(entries(&sandbox.home.join(".cursor")), ["mcp.json"]);
}

#[test]
fn rerunning_setup_is_idempotent_and_leaves_no_temporary_files() {
    let sandbox = Sandbox::new("idempotent");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Vscode, Scope::User)
        .expect("vscode has user configuration");

    let first = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Vscode],
        Scope::User,
    ))
    .expect("first setup succeeds");
    assert_eq!(first.outcomes[0].action, Action::Installed);
    let after_first = read_file(&config);

    let second = setup::run(&install_options(
        environment,
        vec![Harness::Vscode],
        Scope::User,
    ))
    .expect("second setup succeeds");
    assert_eq!(second.outcomes[0].action, Action::Unchanged);
    assert_eq!(read_file(&config), after_first);
    assert_eq!(
        entries(config.parent().expect("profile directory")),
        ["mcp.json"]
    );
}

#[test]
fn conflicting_entry_is_rejected_without_force_and_replaced_with_it() {
    let sandbox = Sandbox::new("conflict");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Cursor, Scope::User)
        .expect("cursor has user configuration");
    write_file(
        &config,
        &format!(
            "{}\n",
            json!({
                "mcpServers": {
                    "pencil": {"command": "/usr/bin/pen"},
                    SERVER_NAME: {"command": "/opt/old/diffscope", "args": ["mcp"], "env": {"KEEP": "1"}},
                }
            })
        ),
    );
    let before = read_file(&config);

    let conflict = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Cursor],
        Scope::User,
    ))
    .expect_err("a conflicting entry needs --force");
    let message = error_message(&conflict);
    assert!(message.contains(&config.display().to_string()), "{message}");
    assert!(message.contains("--force"), "{message}");
    assert_eq!(read_file(&config), before, "conflict changed the file");

    let mut forced = install_options(environment, vec![Harness::Cursor], Scope::User);
    forced.force = true;
    let report = setup::run(&forced).expect("forced setup succeeds");
    assert_eq!(report.outcomes[0].action, Action::Updated);

    let written = read_json(&config);
    assert_eq!(
        written["mcpServers"]["pencil"],
        json!({"command": "/usr/bin/pen"})
    );
    assert_eq!(
        written["mcpServers"][SERVER_NAME],
        json!({
            "command": sandbox.executable.display().to_string(),
            "args": ["mcp"],
            "env": {"KEEP": "1"},
        })
    );
}

#[test]
fn malformed_configuration_fails_safely() {
    let sandbox = Sandbox::new("malformed");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Cursor, Scope::User)
        .expect("cursor has user configuration");
    write_file(&config, "{\"mcpServers\": {");
    let before = read_file(&config);

    let failure = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Cursor],
        Scope::User,
    ))
    .expect_err("malformed configuration is rejected");
    let message = error_message(&failure);
    assert!(message.contains(&config.display().to_string()), "{message}");
    assert!(message.contains("invalid JSON"), "{message}");
    assert_eq!(read_file(&config), before, "malformed file was rewritten");
    assert_eq!(entries(&sandbox.home.join(".cursor")), ["mcp.json"]);

    let mut options = DoctorOptions::new(environment);
    options.selection = HarnessSelection::Named(vec![Harness::Cursor]);
    let report = setup::doctor(&options).expect("doctor reports instead of failing");
    assert_eq!(report.checks[0].status, DoctorStatus::Invalid);
    assert!(!report.is_healthy());
}

#[test]
fn removal_deletes_only_the_entry_and_is_repeatable() {
    let sandbox = Sandbox::new("removal");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Opencode, Scope::User)
        .expect("opencode has user configuration");
    write_file(
        &config,
        &format!(
            "{}\n",
            json!({
                "$schema": "https://opencode.ai/config.json",
                "autoupdate": true,
                "mcp": {
                    "pencil": {"type": "local", "command": ["/usr/bin/pen"], "enabled": true},
                    SERVER_NAME: {"type": "local", "command": ["/usr/bin/diffscope", "mcp"], "enabled": true},
                },
            })
        ),
    );

    let mut options = install_options(environment.clone(), vec![Harness::Opencode], Scope::User);
    options.mode = Mode::Remove;
    let report = setup::run(&options).expect("removal succeeds");
    assert_eq!(report.outcomes[0].action, Action::Removed);

    let written = read_json(&config);
    assert_eq!(written["mcp"][SERVER_NAME], Value::Null);
    assert_eq!(written["autoupdate"], json!(true));
    assert_eq!(written["$schema"], json!("https://opencode.ai/config.json"));
    assert_eq!(written["mcp"]["pencil"]["command"], json!(["/usr/bin/pen"]));
    let after_removal = read_file(&config);

    let report = setup::run(&options).expect("second removal succeeds");
    assert_eq!(report.outcomes[0].action, Action::NotPresent);
    assert_eq!(read_file(&config), after_removal);
}

#[test]
fn project_scope_writes_the_configuration_each_harness_reads() {
    let sandbox = Sandbox::new("project-scope");
    let environment = sandbox.environment();
    let expected = [
        (Harness::Claude, PathBuf::from(".mcp.json"), "mcpServers"),
        (
            Harness::Gemini,
            PathBuf::from(".gemini/settings.json"),
            "mcpServers",
        ),
        (
            Harness::Cursor,
            PathBuf::from(".cursor/mcp.json"),
            "mcpServers",
        ),
        (Harness::Opencode, PathBuf::from("opencode.json"), "mcp"),
        (
            Harness::Vscode,
            PathBuf::from(".vscode/mcp.json"),
            "servers",
        ),
    ];

    for (harness, relative, container) in &expected {
        let report = setup::run(&install_options(
            environment.clone(),
            vec![*harness],
            Scope::Project,
        ))
        .expect("setup succeeds");
        let location = report.outcomes[0].location.clone().expect("location");
        assert_eq!(
            location,
            environment.project_root().join(relative),
            "{harness:?}"
        );
        assert_eq!(
            read_json(&location)[container][SERVER_NAME],
            sandbox.expected_entry(*harness),
            "{harness:?}"
        );
    }

    let mut options = DoctorOptions::new(environment);
    options.selection =
        HarnessSelection::Named(expected.iter().map(|(harness, ..)| *harness).collect());
    options.scope = Scope::Project;
    let report = setup::doctor(&options).expect("doctor succeeds");
    for check in &report.checks {
        assert_eq!(check.status, DoctorStatus::Configured, "{check:?}");
    }
    assert!(report.is_healthy());
}

#[test]
fn dry_run_reports_the_plan_without_writing() {
    let sandbox = Sandbox::new("dry-run");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Vscode, Scope::User)
        .expect("vscode has user configuration");

    let mut options = install_options(environment.clone(), vec![Harness::Vscode], Scope::User);
    options.dry_run = true;
    let report = setup::run(&options).expect("dry run succeeds");
    assert!(report.dry_run);
    assert_eq!(report.outcomes[0].action, Action::Installed);
    let rendered = report.to_string();
    assert!(rendered.contains("dry-run"), "{rendered}");
    assert!(rendered.contains("would create"), "{rendered}");
    assert!(!config.exists(), "dry run wrote configuration");

    write_file(
        &config,
        &format!(
            "{}\n",
            json!({"servers": {SERVER_NAME: {"command": "/opt/old"}}})
        ),
    );
    let failure = setup::run(&options).expect_err("dry run surfaces the conflict");
    assert!(error_message(&failure).contains("--force"));
    assert!(!read_file(&config).contains(&sandbox.executable.display().to_string()));
}

#[test]
fn codex_needs_its_program_and_has_no_project_scope() {
    let sandbox = Sandbox::new("codex");
    let environment = sandbox.environment();

    let failure = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Codex],
        Scope::User,
    ))
    .expect_err("codex is written by its own program");
    let message = error_message(&failure);
    assert!(message.contains("codex mcp add"), "{message}");
    assert!(message.contains("PATH"), "{message}");

    let failure = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Codex],
        Scope::Project,
    ))
    .expect_err("codex has no project scope");
    assert!(error_message(&failure).contains("--scope user"));

    let mut options = install_options(environment, Harness::ALL.to_vec(), Scope::Project);
    options.selection = HarnessSelection::All;
    let report = setup::run(&options).expect("bulk selection skips codex");
    assert_eq!(report.outcomes.len(), Harness::ALL.len());
    let skipped: Vec<Harness> = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.action == Action::Skipped)
        .map(|outcome| outcome.harness)
        .collect();
    assert_eq!(skipped, [Harness::Codex]);
    assert!(report.to_string().contains("--scope user"));
}

#[test]
fn detected_selection_only_uses_installed_harnesses() {
    let sandbox = Sandbox::new("detected");
    let environment = sandbox.environment();

    let failure =
        setup::run(&Options::new(environment.clone())).expect_err("nothing is installed here");
    assert!(matches!(failure, Error::NoHarnessDetected));
    assert!(error_message(&failure).contains("--harness all"));

    let cursor_config = environment
        .config_path(Harness::Cursor, Scope::User)
        .expect("cursor has user configuration");
    write_file(&cursor_config, "{}\n");
    let report = setup::run(&Options::new(environment)).expect("setup succeeds");
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].harness, Harness::Cursor);
}

#[cfg(unix)]
#[test]
fn claude_is_configured_through_its_program_when_available() {
    let sandbox = Sandbox::new("claude-cli");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Claude, Scope::User)
        .expect("claude has user configuration");
    let log = sandbox.root.join("claude.log");
    let program = sandbox.programs.join("claude");
    sandbox.fake_program(
        "claude",
        &log,
        &[(
            &config,
            format!(
                "{}\n",
                json!({"mcpServers": {SERVER_NAME: sandbox.expected_entry(Harness::Claude)}})
            ),
        )],
    );

    let report = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Claude],
        Scope::User,
    ))
    .expect("setup succeeds");
    assert_eq!(report.outcomes[0].action, Action::Installed);
    assert_eq!(
        report.outcomes[0].mechanism,
        Some(Mechanism::Cli {
            program: program.clone()
        })
    );
    assert!(report.outcomes[0].detail.contains("verified"));
    assert_eq!(
        read_json(&config)["mcpServers"][SERVER_NAME],
        sandbox.expected_entry(Harness::Claude)
    );
    assert_eq!(
        log_lines(&log),
        [
            format!("cwd={}", environment.project_root().display()),
            "arg=mcp".to_owned(),
            "arg=add".to_owned(),
            "arg=--scope".to_owned(),
            "arg=user".to_owned(),
            format!("arg={SERVER_NAME}"),
            "arg=--".to_owned(),
            format!("arg={}", sandbox.executable.display()),
            "arg=mcp".to_owned(),
        ]
    );

    let report = setup::run(&install_options(
        environment,
        vec![Harness::Claude],
        Scope::User,
    ))
    .expect("setup succeeds");
    assert_eq!(report.outcomes[0].action, Action::Unchanged);
    assert_eq!(log_lines(&log).len(), 9, "the program ran a second time");
}

#[cfg(unix)]
#[test]
fn forced_install_replaces_the_entry_through_the_program() {
    let sandbox = Sandbox::new("claude-force");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Claude, Scope::User)
        .expect("claude has user configuration");
    write_file(
        &config,
        &format!(
            "{}\n",
            json!({"mcpServers": {SERVER_NAME: {"command": "/opt/old"}}})
        ),
    );
    let log = sandbox.root.join("claude.log");
    sandbox.fake_program(
        "claude",
        &log,
        &[(
            &config,
            format!(
                "{}\n",
                json!({"mcpServers": {SERVER_NAME: sandbox.expected_entry(Harness::Claude)}})
            ),
        )],
    );

    let mut options = install_options(environment.clone(), vec![Harness::Claude], Scope::User);
    options.force = true;
    let report = setup::run(&options).expect("forced setup succeeds");
    assert_eq!(report.outcomes[0].action, Action::Updated);
    assert_eq!(
        read_json(&config)["mcpServers"][SERVER_NAME],
        sandbox.expected_entry(Harness::Claude)
    );

    let expected = vec![
        format!("cwd={}", environment.project_root().display()),
        "arg=mcp".to_owned(),
        "arg=remove".to_owned(),
        "arg=--scope".to_owned(),
        "arg=user".to_owned(),
        format!("arg={SERVER_NAME}"),
        format!("cwd={}", environment.project_root().display()),
        "arg=mcp".to_owned(),
        "arg=add".to_owned(),
        "arg=--scope".to_owned(),
        "arg=user".to_owned(),
        format!("arg={SERVER_NAME}"),
        "arg=--".to_owned(),
        format!("arg={}", sandbox.executable.display()),
        "arg=mcp".to_owned(),
    ];
    assert_eq!(log_lines(&log), expected);
}

#[cfg(unix)]
#[test]
fn codex_configuration_is_read_back_for_doctor() {
    let sandbox = Sandbox::new("codex-doctor");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Codex, Scope::User)
        .expect("codex has user configuration");
    let log = sandbox.root.join("codex.log");
    sandbox.fake_program(
        "codex",
        &log,
        &[(
            &config,
            format!(
                "[mcp_servers.pencil]\ncommand = \"/usr/bin/pen\"\n\n[mcp_servers.{SERVER_NAME}]\ncommand = \"{}\"\nargs = [\"mcp\"]\n",
                sandbox.executable.display()
            ),
        )],
    );

    let report = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Codex],
        Scope::User,
    ))
    .expect("setup succeeds");
    assert_eq!(report.outcomes[0].action, Action::Installed);
    assert!(report.outcomes[0].detail.contains("verified"));
    assert_eq!(
        log_lines(&log),
        [
            format!("cwd={}", environment.project_root().display()),
            "arg=mcp".to_owned(),
            "arg=add".to_owned(),
            format!("arg={SERVER_NAME}"),
            "arg=--".to_owned(),
            format!("arg={}", sandbox.executable.display()),
            "arg=mcp".to_owned(),
        ]
    );

    let mut options = DoctorOptions::new(environment);
    options.selection = HarnessSelection::Named(vec![Harness::Codex]);
    let doctor = setup::doctor(&options).expect("doctor succeeds");
    assert_eq!(doctor.checks[0].status, DoctorStatus::Configured);
    assert_eq!(
        doctor.checks[0].command,
        Some(vec![
            sandbox.executable.display().to_string(),
            "mcp".to_owned()
        ])
    );
    assert!(doctor.is_healthy());
}

#[test]
fn doctor_reports_every_state_it_finds() {
    let sandbox = Sandbox::new("doctor-states");
    let environment = sandbox.environment();
    let path = |harness: Harness| {
        environment
            .config_path(harness, Scope::User)
            .expect("user configuration")
    };

    write_file(
        &path(Harness::Claude),
        &format!(
            "{}\n",
            json!({"mcpServers": {SERVER_NAME: sandbox.expected_entry(Harness::Claude)}})
        ),
    );
    write_file(
        &path(Harness::Cursor),
        &format!(
            "{}\n",
            json!({"mcpServers": {SERVER_NAME: {"command": "/opt/old/diffscope", "args": ["mcp"]}}})
        ),
    );
    write_file(&path(Harness::Gemini), "{ not json");
    write_file(
        &path(Harness::Opencode),
        &format!(
            "{}\n",
            json!({"mcp": {SERVER_NAME: {"type": "remote", "url": "https://example.test/mcp", "enabled": true}}})
        ),
    );
    create_directory(&sandbox.config_home.join("Code"));

    let mut options = DoctorOptions::new(environment.clone());
    let report = setup::doctor(&options).expect("doctor succeeds");

    let status = |harness: Harness| {
        report
            .checks
            .iter()
            .find(|check| check.harness == harness)
            .expect("check exists")
            .status
    };
    assert_eq!(status(Harness::Claude), DoctorStatus::Configured);
    assert_eq!(status(Harness::Cursor), DoctorStatus::Stale);
    assert_eq!(status(Harness::Gemini), DoctorStatus::Invalid);
    assert_eq!(status(Harness::Opencode), DoctorStatus::Unrecognized);
    assert_eq!(status(Harness::Vscode), DoctorStatus::Missing);
    assert_eq!(status(Harness::Codex), DoctorStatus::Missing);
    assert!(!report.live_connection_checked);
    assert!(!report.is_healthy());

    let rendered = report.to_string();
    assert!(
        rendered.contains("no live MCP handshake was performed"),
        "{rendered}"
    );
    assert!(rendered.contains("configured"), "{rendered}");
    assert!(rendered.contains("not found"), "{rendered}");

    options.scope = Scope::Project;
    let project = setup::doctor(&options).expect("doctor succeeds");
    let codex = project
        .checks
        .iter()
        .find(|check| check.harness == Harness::Codex)
        .expect("check exists");
    assert_eq!(codex.status, DoctorStatus::UnsupportedScope);
}

#[test]
fn doctor_is_healthy_after_setup_configures_every_json_harness() {
    let sandbox = Sandbox::new("doctor-healthy");
    let environment = sandbox.environment();
    let harnesses = vec![
        Harness::Claude,
        Harness::Gemini,
        Harness::Cursor,
        Harness::Opencode,
        Harness::Vscode,
    ];

    setup::run(&install_options(
        environment.clone(),
        harnesses.clone(),
        Scope::User,
    ))
    .expect("setup succeeds");

    let mut options = DoctorOptions::new(environment);
    options.selection = HarnessSelection::Named(harnesses);
    let report = setup::doctor(&options).expect("doctor succeeds");

    for check in &report.checks {
        assert_eq!(check.status, DoctorStatus::Configured, "{check:?}");
        assert_eq!(
            check.command,
            Some(vec![
                sandbox.executable.display().to_string(),
                "mcp".to_owned()
            ]),
            "{check:?}"
        );
    }
    assert!(report.is_healthy());
}

#[test]
fn selection_and_scope_values_are_validated() {
    assert_eq!(HarnessSelection::parse("all"), Ok(HarnessSelection::All));
    assert_eq!(
        HarnessSelection::parse("cursor, Claude ,cursor"),
        Ok(HarnessSelection::Named(vec![
            Harness::Cursor,
            Harness::Claude
        ]))
    );
    let failure = HarnessSelection::parse("claud").expect_err("unknown name is rejected");
    let message = error_message(&failure);
    assert!(message.contains("claud"), "{message}");
    assert!(
        message.contains("all, claude, codex, gemini, cursor, opencode, vscode"),
        "{message}"
    );
    assert!(HarnessSelection::parse(" , ").is_err());
    assert_eq!(Scope::parse("PROJECT"), Some(Scope::Project));
    assert_eq!(Scope::parse("user"), Some(Scope::User));
    assert_eq!(Scope::parse("global"), None);
    assert_eq!(Harness::parse("OpenCode"), Some(Harness::Opencode));
    assert!(!Harness::Codex.supports_scope(Scope::Project));
    assert!(Harness::Codex.supports_scope(Scope::User));
    assert!(Harness::Vscode.supports_scope(Scope::Project));
}

#[test]
fn detected_environment_reads_the_process_roots() {
    let environment = Environment::detected().expect("this process has a home directory");

    assert!(environment.home().is_absolute());
    assert!(environment.config_home().is_absolute());
    assert_eq!(
        environment.project_root(),
        fs::canonicalize(std::env::current_dir().expect("current directory")).expect("canonical")
    );
    assert_eq!(
        environment.executable(),
        fs::canonicalize(std::env::current_exe().expect("current executable")).expect("canonical")
    );
    let claude = environment
        .config_path(Harness::Claude, Scope::User)
        .expect("claude has a user configuration");
    assert!(
        claude.starts_with(environment.home()),
        "{} is not below the detected home directory",
        claude.display()
    );
}

#[test]
fn environment_roots_are_pinned_to_the_injected_directories() {
    let sandbox = Sandbox::new("environment");
    let environment = sandbox.environment();

    assert_eq!(environment.home(), sandbox.home.as_path());
    assert_eq!(environment.config_home(), sandbox.config_home.as_path());
    assert_eq!(environment.project_root(), sandbox.project.as_path());
    assert_eq!(environment.executable(), sandbox.executable.as_path());
    assert!(!environment.is_installed(Harness::Cursor));
    assert_eq!(
        environment.config_path(Harness::Codex, Scope::Project),
        None,
        "codex has no project configuration"
    );
}

#[test]
fn opencode_jsonc_is_refused_instead_of_silently_ignored() {
    let sandbox = Sandbox::new("opencode-jsonc");
    let environment = sandbox.environment();
    let jsonc = sandbox.config_home.join("opencode").join("opencode.jsonc");
    write_file(&jsonc, "{// project configuration\n}\n");

    let failure = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Opencode],
        Scope::User,
    ))
    .expect_err("a jsonc file cannot be merged");
    let message = error_message(&failure);
    assert!(message.contains("opencode.jsonc"), "{message}");
    assert!(message.contains("rename"), "{message}");
    assert!(
        !environment
            .config_path(Harness::Opencode, Scope::User)
            .expect("opencode has user configuration")
            .exists(),
        "the JSON file was created next to the jsonc file"
    );
}

#[test]
fn setup_creates_missing_containers_and_accepts_blank_files() {
    let sandbox = Sandbox::new("containers");
    let environment = sandbox.environment();
    let cursor = environment
        .config_path(Harness::Cursor, Scope::User)
        .expect("cursor has user configuration");
    let gemini = environment
        .config_path(Harness::Gemini, Scope::User)
        .expect("gemini has user configuration");
    write_file(&cursor, "{\"theme\": \"dark\"}\n");
    write_file(&gemini, "");

    setup::run(&install_options(
        environment,
        vec![Harness::Cursor, Harness::Gemini],
        Scope::User,
    ))
    .expect("setup succeeds");

    let cursor_document = read_json(&cursor);
    assert_eq!(cursor_document["theme"], json!("dark"));
    assert_eq!(
        cursor_document["mcpServers"][SERVER_NAME],
        sandbox.expected_entry(Harness::Cursor)
    );
    assert_eq!(
        read_json(&gemini)["mcpServers"][SERVER_NAME],
        sandbox.expected_entry(Harness::Gemini)
    );
}

#[cfg(unix)]
#[test]
fn a_program_that_changes_nothing_falls_back_to_writing_the_entry() {
    let sandbox = Sandbox::new("fallback");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Gemini, Scope::User)
        .expect("gemini has user configuration");
    let log = sandbox.root.join("gemini.log");
    sandbox.fake_program("gemini", &log, &[]);

    let report = setup::run(&install_options(
        environment.clone(),
        vec![Harness::Gemini],
        Scope::User,
    ))
    .expect("setup succeeds");
    assert_eq!(report.outcomes[0].action, Action::Installed);
    assert!(
        report.outcomes[0].detail.contains("directly"),
        "{:?}",
        report.outcomes[0]
    );
    assert_eq!(
        read_json(&config)["mcpServers"][SERVER_NAME],
        sandbox.expected_entry(Harness::Gemini)
    );

    let mut options = install_options(environment, vec![Harness::Gemini], Scope::User);
    options.mode = Mode::Remove;
    let report = setup::run(&options).expect("removal succeeds");
    assert_eq!(report.outcomes[0].action, Action::Removed);
    assert!(
        report.outcomes[0].detail.contains("directly"),
        "{:?}",
        report.outcomes[0]
    );
    assert_eq!(read_json(&config)["mcpServers"][SERVER_NAME], Value::Null);
    assert!(
        !log_lines(&log).is_empty(),
        "the program was not preferred first"
    );
}

#[cfg(unix)]
#[test]
fn a_codex_program_that_writes_another_command_is_an_error() {
    let sandbox = Sandbox::new("codex-mismatch");
    let environment = sandbox.environment();
    let config = environment
        .config_path(Harness::Codex, Scope::User)
        .expect("codex has user configuration");
    let log = sandbox.root.join("codex.log");
    sandbox.fake_program(
        "codex",
        &log,
        &[(
            &config,
            format!("[mcp_servers.{SERVER_NAME}]\ncommand = \"/opt/old\"\nargs = [\"mcp\"]\n"),
        )],
    );

    let failure = setup::run(&install_options(
        environment,
        vec![Harness::Codex],
        Scope::User,
    ))
    .expect_err("a mismatch is not reported as success");
    assert!(error_message(&failure).contains("still does not launch"));
}

#[test]
fn dry_run_conflicts_report_planned_actions_only() {
    let sandbox = Sandbox::new("dry-run-conflict");
    let environment = sandbox.environment();
    let cursor = environment
        .config_path(Harness::Cursor, Scope::User)
        .expect("cursor has user configuration");
    let vscode = environment
        .config_path(Harness::Vscode, Scope::User)
        .expect("vscode has user configuration");
    write_file(
        &cursor,
        &format!(
            "{}\n",
            json!({"mcpServers": {SERVER_NAME: {"command": "/opt/old"}}})
        ),
    );

    let mut options = install_options(
        environment,
        vec![Harness::Vscode, Harness::Cursor],
        Scope::User,
    );
    options.dry_run = true;
    let failure = setup::run(&options).expect_err("the conflict surfaces in a dry run");
    let message = error_message(&failure);
    assert!(message.contains("would create"), "{message}");
    assert!(!message.contains("created"), "{message}");
    assert!(!vscode.exists(), "the dry run created a configuration file");
    assert_eq!(
        read_file(&cursor),
        format!(
            "{}\n",
            json!({"mcpServers": {SERVER_NAME: {"command": "/opt/old"}}})
        ),
        "the dry run rewrote the conflicting configuration"
    );
}
