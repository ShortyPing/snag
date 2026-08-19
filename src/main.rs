pub mod discovery;
pub mod manifest;
pub mod report;
pub mod runner;
pub mod scripting_registration;

use crate::discovery::discover;
use crate::runner::run;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use std::io;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "snag",
    version,
    about = "Run regression test suites from declarative definition files.",
    arg_required_else_help = false,
    propagate_version = true
)]
pub struct Cli {
    /// Output format / reporter.
    #[arg(long, short = 'f', value_enum, default_value_t = Format::Human, global = true)]
    pub format: Format,

    /// Control color output (auto = on when writing to a TTY).
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    pub color: ColorChoice,

    /// Increase log verbosity (-v, -vv, -vvv).
    #[arg(long, short = 'v', action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Silence non-essential human output.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Subcommand. Defaults to `run` when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Execute test suites (default).
    Run(RunArgs),

    /// Discover and print tests without executing them.
    List(ListArgs),

    /// Parse manifests and compile scripts without running anything.
    Check(CheckArgs),

    /// Scaffold a new example definition file in the current directory.
    Init(InitArgs),

    /// Generate shell completion scripts.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Args)]
pub struct SelectionArgs {
    /// Definition files or directories to search. Globs allowed.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Only run tests whose name or id contains this substring (repeatable).
    #[arg(long, short = 'k', value_name = "FILTER")]
    pub filter: Vec<String>,

    /// Only include tests carrying this tag (repeatable, OR-ed).
    #[arg(long, short = 't', value_name = "TAG")]
    pub tag: Vec<String>,

    /// Skip tests matching this substring (repeatable).
    #[arg(long, short = 'e', value_name = "SUBSTR")]
    pub exclude: Vec<String>,

    /// Treat filters as regular expressions instead of substrings.
    #[arg(long)]
    pub regex: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    /// Number of tests to run in parallel. 0 = number of logical CPUs.
    #[arg(long, short = 'j', value_name = "N", default_value_t = 0)]
    pub jobs: usize,

    /// Stop after the first failure.
    #[arg(long)]
    pub fail_fast: bool,

    /// Per-test timeout, e.g. "500ms", "30s". Overrides file-level value.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    pub timeout: Option<std::time::Duration>,

    /// Retry a failing test up to N times before declaring it failed.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub retries: u32,

    /// Seed for randomized ordering, so a run is reproducible.
    #[arg(long, value_name = "SEED")]
    pub seed: Option<u64>,

    /// Don't execute; show what would run.
    #[arg(long)]
    pub dry_run: bool,

    /// Write the machine report to a file instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    /// Also print each test's tags and source location.
    #[arg(long)]
    pub long: bool,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Path for the new definition file.
    #[arg(default_value = "suite.toml")]
    pub path: PathBuf,

    /// Overwrite if it already exists.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Pretty, colored, for humans.
    Human,
    /// One JSON object per test, streamed as each finishes.
    Jsonl,
    /// A single JSON document emitted at the end.
    Json,
    /// `TeamCity` service messages.
    Teamcity,
    /// `JUnit` XML.
    Junit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[repr(u8)]
pub enum Exit {
    Ok = 0,
    TestsFailed = 1,
    Error = 2,
}

impl From<Exit> for ExitCode {
    fn from(e: Exit) -> Self {
        ExitCode::from(e as u8)
    }
}

pub struct Ctx {
    pub format: Format,
    pub color: bool,
    pub verbose: u8,
    pub quiet: bool,
}

impl Ctx {
    fn from_cli(cli: &Cli) -> Self {
        let color = match cli.color {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => io::stdout().is_terminal(),
        };
        Ctx {
            format: cli.format,
            color,
            verbose: cli.verbose,
            quiet: cli.quiet,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse_from(implicit_run(std::env::args()));
    let ctx = Ctx::from_cli(&cli);

    let command = cli.command.unwrap_or_else(default_run);

    let result = match command {
        Command::Run(args) => cmd_run(&ctx, args),
        Command::List(args) => cmd_list(&ctx, args),
        Command::Check(args) => cmd_check(&ctx, args),
        Command::Init(args) => cmd_init(&ctx, args),
        Command::Completions { shell } => cmd_completions(shell),
    };

    match result {
        Ok(exit) => exit.into(),
        // A closed pipe (`snag list | head`) isn't an error.
        Err(err) if is_broken_pipe(&err) => Exit::Ok.into(),
        Err(err) => {
            eprintln!("Error: {err}");
            Exit::Error.into()
        }
    }
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    })
}

const VERBS: &[&str] = &["run", "list", "check", "init", "completions", "help"];

// Turn `snag ./suite.toml` into `snag run ./suite.toml`. clap won't accept a
// variadic positional alongside subcommands, so the verb goes in by hand.
fn implicit_run<I: IntoIterator<Item = String>>(args: I) -> Vec<String> {
    const TAKES_VALUE: &[&str] = &[
        "-f",
        "--format",
        "--color",
        "-k",
        "--filter",
        "-t",
        "--tag",
        "-e",
        "--exclude",
        "-j",
        "--jobs",
        "--timeout",
        "--retries",
        "--seed",
        "--report",
    ];

    let mut args: Vec<String> = args.into_iter().collect();
    let mut i = 1;

    while i < args.len() {
        let arg = args[i].clone();
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            if TAKES_VALUE.contains(&arg.as_str()) {
                i += 1;
            }
            i += 1;
            continue;
        }
        if !VERBS.contains(&arg.as_str()) {
            args.insert(i, "run".to_string());
        }
        return args;
    }

    args
}

fn default_run() -> Command {
    Command::Run(RunArgs {
        selection: SelectionArgs {
            paths: vec![],
            filter: vec![],
            tag: vec![],
            exclude: vec![],
            regex: false,
        },
        jobs: 0,
        fail_fast: false,
        timeout: None,
        retries: 0,
        seed: None,
        dry_run: false,
        report: None,
    })
}

fn cmd_run(ctx: &Ctx, args: RunArgs) -> anyhow::Result<Exit> {
    run(ctx, &args)
}

fn cmd_list(ctx: &Ctx, args: ListArgs) -> anyhow::Result<Exit> {
    let tests = discover(&args.selection)?;
    let mut out = io::stdout().lock();

    match ctx.format {
        Format::Json | Format::Jsonl | Format::Teamcity | Format::Junit => {
            let items: Vec<_> = tests
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.qualified_id(),
                        "test_id": t.id,
                        "name": t.name,
                        "suite": t.suite_title,
                        "suite_path": t.suite_path.display().to_string(),
                        "script": t.script.display().to_string(),
                        "tags": t.tags,
                        "timeout_ms": t.timeout.map(|d| d.as_millis() as u64),
                        "parallel_safe": t.parallel_safe,
                    })
                })
                .collect();

            if ctx.format == Format::Jsonl {
                for item in &items {
                    writeln!(out, "{item}")?;
                }
            } else {
                writeln!(out, "{}", serde_json::to_string_pretty(&items)?)?;
            }
        }
        Format::Human => {
            for test in &tests {
                if args.long {
                    writeln!(
                        out,
                        "{}\n    name:   {}\n    tags:   {}\n    script: {}",
                        test.qualified_id(),
                        test.name,
                        if test.tags.is_empty() {
                            "-".into()
                        } else {
                            test.tags.join(", ")
                        },
                        test.script.display()
                    )?;
                } else {
                    writeln!(out, "{}", test.name)?;
                }
            }
            if !ctx.quiet {
                writeln!(out, "\n{} test(s)", tests.len())?;
            }
        }
    }

    Ok(Exit::Ok)
}

fn cmd_check(ctx: &Ctx, args: CheckArgs) -> anyhow::Result<Exit> {
    let tests = discover(&args.selection)?;
    runner::check(ctx, &tests)
}

fn cmd_init(ctx: &Ctx, args: InitArgs) -> anyhow::Result<Exit> {
    const SUITE_TEMPLATE: &str = r#"title = "Example suite"

[variables]
base_url = "https://httpbin.org"

[[test]]
id = "get-status"
name = "GET /status/200 returns 200"
tags = ["smoke"]
timeout = "10s"
file = "./get_status.snag"

[test.variables]
expected = "200"
"#;

    const SCRIPT_TEMPLATE: &str = r"let res = get(`${base_url}/status/200`).send();

assert_status(res, 200);
assert_faster_than(res, 5000);
";

    if args.path.exists() && !args.force {
        anyhow::bail!(
            "{} already exists (use --force to overwrite)",
            args.path.display()
        );
    }

    let dir = args.path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&args.path, SUITE_TEMPLATE)?;

    let script = dir
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("get_status.snag");
    if !script.exists() || args.force {
        std::fs::write(&script, SCRIPT_TEMPLATE)?;
    }

    if !ctx.quiet {
        println!("created {} and {}", args.path.display(), script.display());
        println!("run it with: snag {}", args.path.display());
    }
    Ok(Exit::Ok)
}

fn cmd_completions(shell: clap_complete::Shell) -> anyhow::Result<Exit> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut io::stdout());
    Ok(Exit::Ok)
}

use std::io::IsTerminal;
use std::process::ExitCode;
