//! The `copybera` command: validate a workflow, migrate, print a receipt.

mod cap;
mod closure_cmd;
mod git;
mod run;
mod subjects;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::git::Git;

#[derive(Parser)]
#[command(
    name = "copybera",
    version,
    about = "Copybara-style repository transformer"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse and check a workflow file with no git and no run
    Validate {
        /// Workflow TOML
        workflow: PathBuf,
    },
    /// Run a workflow against a local origin and destination
    Migrate {
        /// Workflow TOML
        #[arg(required_unless_present = "schedule")]
        workflow: Option<PathBuf>,
        /// Run this workflow repeatedly instead of a one-off migration
        #[arg(
            long,
            value_name = "WORKFLOW",
            conflicts_with = "workflow",
            requires = "every"
        )]
        schedule: Option<PathBuf>,
        /// Time between scheduled runs (Ns, Nm, or Nh; at least 60 seconds)
        #[arg(long, value_parser = run::parse_schedule_duration, requires = "schedule")]
        every: Option<Duration>,
        /// Print the next three scheduled runs without migrating
        #[arg(long, requires = "schedule", conflicts_with = "once")]
        plan: bool,
        /// Run one scheduled migration and exit with its status
        #[arg(long, requires = "schedule", conflicts_with = "plan")]
        once: bool,
        /// List what would move; write a receipt; do not write the destination
        #[arg(long)]
        dry_run: bool,
        /// Limit the number of origin commits walked
        #[arg(long)]
        max_commits: Option<usize>,
        /// Continue from the bounded receipt written by an earlier run
        #[arg(long)]
        resume: bool,
    },
    /// Print a receipt file
    Receipt {
        /// Receipt JSON
        receipt: PathBuf,
    },
    /// Walk destination history and refuse reference ids (`[lint] ids`, else the workflow's `strip_references`
    /// patterns), excluded paths and private patterns
    Lint {
        /// Destination git repository
        #[arg(
            required_unless_present = "from_receipt",
            conflicts_with = "from_receipt"
        )]
        dest: Option<PathBuf>,
        /// Lint only the destination commits named by this receipt
        #[arg(long, conflicts_with_all = ["range", "workflow"])]
        from_receipt: Option<PathBuf>,
        /// Limit the walk to this range (a..b); default: the workflow's destination.push branch
        #[arg(long, conflicts_with = "from_receipt")]
        range: Option<String>,
        /// The workflow whose push branch and excluded paths the lint reads (default: ./workflow.toml)
        #[arg(long, conflicts_with = "from_receipt")]
        workflow: Option<PathBuf>,
        /// A private-patterns file (one regex per line, outside git): names, hosts and personal facts the
        /// destination must not carry (default: the workflow's or the receipt's `[lint] private`)
        #[arg(long)]
        private: Option<PathBuf>,
        /// Do not refuse reference ids in messages and file contents: the export keeps them on purpose (the
        /// workflow's `[lint] references = "keep"`); a path that matches one still refuses
        #[arg(long)]
        keep_references: bool,
        /// Walk everything and print one count row per check (`private pattern N`, `reference`, ...) instead of
        /// stopping at the first finding; any count refuses
        #[arg(long)]
        count: bool,
    },
    /// Scan the destination's push-branch tree by class (home and volume paths, emails, LAN addresses and hosts,
    /// tokens, secret references, source maps, person entities, private terms and paths, leftover names, licences and
    /// notices) and print classes and counts, never the matched text; any finding refuses
    Gate {
        /// Workflow TOML (its `[gate]` table and destination)
        workflow: PathBuf,
        /// A private-patterns file (default: the workflow's `[lint] private`)
        #[arg(long)]
        private: Option<PathBuf>,
        /// Also print one `class count path` row per finding (never the matched text; a path that carries a private
        /// term is withheld)
        #[arg(long)]
        files: bool,
        /// A TOML file whose `[gate]` table replaces the workflow's (a tracked gate kept apart from a generated
        /// workflow); its relative paths are read from its own folder
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print the include list of the named packages' source closure at an origin revision (their path dependencies,
    /// workspace roots, locks, cargo config and the files outside a package its build sources name), or check that a
    /// workflow's include covers it
    Closure {
        /// Origin git repository
        origin: PathBuf,
        /// The origin revision to read
        #[arg(long = "ref", default_value = "HEAD")]
        rev: String,
        /// A Cargo package folder to start from (repeatable)
        #[arg(long = "cargo", value_name = "DIR")]
        cargo: Vec<String>,
        /// A JavaScript package folder to start from (repeatable)
        #[arg(long = "node", value_name = "DIR")]
        node: Vec<String>,
        /// Follow dev-dependencies and read test sources too
        #[arg(long)]
        dev: bool,
        /// Check that this workflow's [paths.origin] include covers the closure; a missing entry refuses
        #[arg(long, value_name = "WORKFLOW")]
        check: Option<PathBuf>,
        /// Print each include entry with the reason it is in the closure
        #[arg(long, conflicts_with = "check")]
        why: bool,
    },
    /// Read the subjects of the range a migrate would walk (origin.since..origin.ref) against the private patterns, as
    /// the lint reads a squash message; print pattern numbers and commit ids, never the text, and the earliest start
    /// that leaves every dirty subject out; a dirty subject refuses
    Subjects {
        /// Workflow TOML (its origin and `[lint] private`)
        workflow: PathBuf,
        /// Try this start instead of the workflow's origin.since
        #[arg(long)]
        since: Option<String>,
    },
    /// Check a workflow's origin, destination, and receipt health
    Doctor {
        /// Workflow TOML
        workflow: PathBuf,
    },
    /// One row per workflow: its receipt's last_rev, bounded, commits, seconds and subjects, and the doctor's verdict
    Status {
        /// Workflow files or folders of them (default: a `workflows` folder in the current directory, the
        /// conventional place for a repository's workflow files)
        paths: Vec<PathBuf>,
        /// Reprint the status table at an interval
        #[arg(long)]
        watch: bool,
        /// Time between status tables (Ns, Nm, or Nh; default: 10s)
        #[arg(long, value_parser = run::parse_duration, requires = "watch")]
        interval: Option<Duration>,
        /// Number of status tables to print while watching
        #[arg(long, value_parser = run::parse_count, requires = "watch")]
        count: Option<usize>,
    },
}

fn main() -> ExitCode {
    // Before anything else, `--help` included: copybera runs only under a hard memory cap.
    let allow = std::env::var_os(cap::ALLOW_UNCAPPED).is_some_and(|value| value == "1");
    match cap::require(std::path::Path::new("/"), allow) {
        Ok(None) => {}
        Ok(Some(warning)) => eprintln!("{warning}"),
        Err(refusal) => {
            eprintln!("{refusal}");
            return ExitCode::from(cap::REFUSED_EXIT);
        }
    }
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let git = Git::new(std::env::var_os("HOME").map(PathBuf::from));
    match cli.command {
        Commands::Validate { workflow } => {
            let parsed = run::validate(&workflow)?;
            println!(
                "ok: {} ({}){}",
                parsed.name,
                parsed.mode.as_str(),
                if parsed.dry_run { " dry_run" } else { "" }
            );
        }
        Commands::Migrate {
            workflow,
            schedule,
            every,
            plan,
            once,
            dry_run,
            max_commits,
            resume,
        } => {
            let mut stdout = std::io::stdout();
            if let Some(workflow) = schedule {
                run::scheduled_migrate(
                    &workflow,
                    every.expect("clap requires --every with --schedule"),
                    plan,
                    once,
                    dry_run,
                    max_commits,
                    resume,
                    &git,
                    &mut stdout,
                )?;
            } else {
                run::migrate(
                    workflow
                        .as_deref()
                        .expect("clap requires a workflow without --schedule"),
                    dry_run,
                    max_commits,
                    resume,
                    &git,
                    &mut stdout,
                )?;
            }
        }
        Commands::Receipt { receipt } => {
            run::print_receipt(&receipt, &mut std::io::stdout())?;
        }
        Commands::Lint {
            dest,
            from_receipt,
            range,
            workflow,
            private,
            keep_references,
            count,
        } => {
            let cwd = std::env::current_dir()?;
            run::lint(
                run::LintArgs {
                    dest: dest.as_deref(),
                    from_receipt: from_receipt.as_deref(),
                    range: range.as_deref(),
                    workflow: workflow.as_deref(),
                    private: private.as_deref(),
                    keep_references,
                    count,
                },
                &git,
                &cwd,
                &mut std::io::stdout(),
            )?;
        }
        Commands::Gate {
            workflow,
            private,
            files,
            config,
        } => {
            let cwd = std::env::current_dir()?;
            run::gate(
                &cwd.join(&workflow),
                private.map(|p| cwd.join(p)).as_deref(),
                config.map(|p| cwd.join(p)).as_deref(),
                files,
                &git,
                &mut std::io::stdout(),
            )?;
        }
        Commands::Closure {
            origin,
            rev,
            cargo,
            node,
            dev,
            check,
            why,
        } => {
            closure_cmd::run(
                closure_cmd::ClosureArgs {
                    origin: &origin,
                    rev: &rev,
                    request: copybera_rules::closure::ClosureRequest { cargo, node, dev },
                    check: check.as_deref(),
                    why,
                },
                &git,
                &mut std::io::stdout(),
            )?;
        }
        Commands::Subjects { workflow, since } => {
            subjects::run(&workflow, since.as_deref(), &git, &mut std::io::stdout())?;
        }
        Commands::Doctor { workflow } => {
            run::doctor(&workflow, &git, &mut std::io::stdout())?;
        }
        Commands::Status {
            paths,
            watch,
            interval,
            count,
        } => {
            let cwd = std::env::current_dir()?;
            let paths = if paths.is_empty() {
                vec![PathBuf::from("workflows")]
            } else {
                paths
            };
            let mut stdout = std::io::stdout();
            if watch {
                run::watch_status(
                    &paths,
                    interval.unwrap_or(Duration::from_secs(10)),
                    count,
                    &git,
                    &cwd,
                    &mut stdout,
                )?;
            } else {
                run::status(&paths, &git, &cwd, &mut stdout)?;
            }
        }
    }
    Ok(())
}
