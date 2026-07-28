//! herdr-board — a task board fed by Linear and GitHub, dispatching into herdr
//! panes and reconciling pane state back to the board.

use herdr_board::{cli, config, conventions, dispatch, gc, herdr, integration, log, stats, ui};

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Paths;
use log::Logger;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "herdr-board",
    version,
    about = "Task board: Linear/GitHub in, herdr panes out"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum IntegrationAction {
    /// Install the agent-detection override (Claude Code only).
    Install { agent: String },
    /// Remove it again, touching nothing else.
    Uninstall { agent: String },
    /// Report whether they are installed.
    Status,
    /// Write the board conventions into a runtime's global instruction file.
    ///
    /// Omit the agent for every runtime that has one. Claude's copy is the
    /// skill at ~/.claude/skills/board/SKILL.md and is left alone.
    InstallConventions { agent: Option<String> },
    /// Take them out again, keeping anything else in the file.
    UninstallConventions { agent: Option<String> },
}

#[derive(Subcommand)]
enum Command {
    /// The board TUI (runs as a herdr pane).
    Pane,
    /// Dispatch popup for the selected ready task.
    Picker,
    /// Sync daemon.
    Syncd {
        /// Start the daemon only if it is not already running, then exit.
        #[arg(long)]
        ensure: bool,
    },
    /// Reconcile pane state only — no network. Fired by herdr agent events.
    Reconcile,
    /// Run one sync cycle.
    Sync {
        #[arg(long, default_value_t = true)]
        once: bool,
    },
    /// Dispatch a task without the picker.
    Dispatch {
        #[arg(long)]
        task: String,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        runtime: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        /// Parent task id, when one agent releases work for another. Normally
        /// omitted: a dispatch from inside an agent's pane is recognised as
        /// that agent's on its own. Pass this to name a parent *issue* as well,
        /// which only exists if the board dispatched the parent too.
        #[arg(long)]
        via: Option<String>,
    },
    /// Write a starter routing.toml from your herdr workspaces.
    Init {
        /// Overwrite an existing routing.toml.
        #[arg(long)]
        force: bool,
    },
    /// Open the board, or close it if it is already up. Bind this to a key.
    Toggle,
    /// Select the clicked issue on the board (the link-handler target).
    OpenLink,
    /// List what is on the board. `--json` for orchestrating agents.
    List {
        /// Only this state: ready, working, blocked, review, failed, done.
        #[arg(long)]
        state: Option<String>,
        /// Only this source: linear or github.
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Block until watched work settles. The counterpart to `dispatch`.
    Wait {
        /// Task to watch; repeat for several. Omit to watch everything in
        /// flight right now.
        #[arg(long)]
        task: Vec<String>,
        /// States that count as settled. Defaults to review, failed and done.
        #[arg(long)]
        state: Vec<String>,
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Write a ticket. Cheaper than not writing one.
    New {
        title: String,
        /// Description. `-` reads it from stdin.
        #[arg(long)]
        body: Option<String>,
        /// Linear team key. Only needed when you have more than one.
        #[arg(long)]
        team: Option<String>,
        /// Labels to apply — this is what routes it.
        #[arg(long)]
        label: Vec<String>,
        /// Which tracker to write to: linear or github. Defaults to
        /// `[defaults] new_source`.
        #[arg(long)]
        source: Option<String>,
        /// `owner/repo` when writing to GitHub and more than one is configured.
        #[arg(long)]
        repo: Option<String>,
        /// Dispatch it as soon as it exists.
        #[arg(long)]
        dispatch: bool,
    },
    /// What the board knows about its own throughput.
    Stats {
        /// Only the last N days. Omit for everything.
        #[arg(long)]
        since_days: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a task's live attempt: kill the pane, keep the issue open.
    Cancel {
        #[arg(long)]
        task: String,
    },
    /// Remove the worktrees of finished attempts. Branches are left alone.
    Gc {
        /// Only checkouts whose last attempt ended longer ago than this:
        /// `14d`, `36h`, `2w`. A bare number is refused.
        #[arg(long, default_value = gc::DEFAULT_OLDER_THAN)]
        older_than: String,
        /// Say what would go, remove nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Teach herdr to read an agent's state, and agents that the board exists.
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    /// Check the environment.
    Doctor,
    /// Render the TUI against fixtures, for design review.
    Demo {
        /// Scenario name; omit for the populated board.
        scenario: Option<String>,
        /// List the available scenarios.
        #[arg(long)]
        list: bool,
    },
}

/// One named agent, or every runtime with a global instruction file.
///
/// Defaulting to all is the point: the reason the board was Claude-only for two
/// dozen attempts is that keeping several copies in step was somebody's manual
/// job, and nobody's.
fn agents_for(agent: Option<&str>) -> Vec<String> {
    match agent {
        Some(a) => vec![a.to_string()],
        None => conventions::AGENTS.iter().map(|a| a.to_string()).collect(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;

    match cli.command {
        // The board and the picker draw on the terminal, so their logs go to the
        // file only — stderr would corrupt the screen.
        Command::Pane => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            let board = ui::app::open_board(paths, log)?;
            ui::app::run(board)
        }
        Command::Picker => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            ui::picker_run::run(paths, log)
        }
        Command::Syncd { ensure } => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            if ensure {
                cli::syncd_ensure(&paths, log)
            } else {
                cli::syncd(&paths, log)
            }
        }
        Command::Reconcile => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            cli::reconcile_once(&paths, log)
        }
        Command::Sync { .. } => {
            let log = Arc::new(Logger::new(paths.logfile(), true));
            cli::sync_once(&paths, log)
        }
        Command::Dispatch {
            task,
            workspace,
            runtime,
            branch,
            via,
        } => {
            let log = Arc::new(Logger::new(paths.logfile(), true));
            let engine = cli::engine_from_paths(&paths, log.clone())?;
            let h = herdr::Herdr::discover(log.clone());
            let t = engine
                .db
                .get_task(&task)?
                .ok_or_else(|| anyhow::anyhow!("no task {task}"))?;
            let result = dispatch::dispatch(
                &engine,
                &h,
                &log,
                &t,
                &dispatch::Overrides {
                    workspace,
                    runtime,
                    branch,
                    via,
                },
            );
            // Report through the database as well as stdout: when this runs
            // detached from the picker there is nobody reading stdout, and a
            // failure that only reached a log would be invisible.
            match &result {
                Ok(plan) => {
                    let _ = engine.db.meta_set(
                        herdr_board::ui::app::PICKER_RESULT,
                        &format!(
                            "dispatched {} → ws:{} · {}",
                            plan.identifier, plan.workspace, plan.runtime
                        ),
                    );
                    println!(
                        "dispatched {} to ws:{} ({}, attempt {}, by {})",
                        plan.identifier,
                        plan.workspace,
                        plan.runtime,
                        plan.attempt_no,
                        dispatch::dispatcher_phrase(&engine.db, &plan.dispatcher),
                    );
                }
                Err(e) => {
                    let _ = engine.db.meta_set(
                        herdr_board::ui::app::PICKER_RESULT,
                        &format!("{} could not be dispatched: {e}", t.identifier),
                    );
                }
            }
            let _ = engine.rederive_all();
            result.map(|_| ())
        }
        Command::Init { force } => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            cli::init(&paths, log, force)
        }
        Command::Toggle => {
            let log = Arc::new(Logger::new(paths.logfile(), true));
            cli::toggle_board(&paths, log)
        }
        Command::OpenLink => {
            let log = Arc::new(Logger::new(paths.logfile(), true));
            cli::open_link(&paths, log)
        }
        Command::List {
            state,
            source,
            json,
        } => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            let rows = cli::list_tasks(&paths, log, state.as_deref(), source.as_deref())?;
            cli::print_tasks(&rows, json)
        }
        Command::Wait {
            task,
            state,
            timeout,
            json,
        } => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            // Finished, one way or another: work to look at, work that broke,
            // or work whose ticket closed under it.
            let states = if state.is_empty() {
                vec!["review".to_string(), "failed".into(), "done".into()]
            } else {
                state
            };
            let rows = cli::wait_for(
                &paths,
                log,
                &task,
                &states,
                timeout.map(std::time::Duration::from_secs),
            )?;
            cli::print_tasks(&rows, json)
        }
        Command::New {
            title,
            body,
            team,
            label,
            source,
            repo,
            dispatch: and_dispatch,
        } => {
            let log = Arc::new(Logger::new(paths.logfile(), true));
            let body = match body.as_deref() {
                Some("-") => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    Some(buf)
                }
                other => other.map(str::to_string),
            };
            let (identifier, url) = cli::new_task(
                &paths,
                log.clone(),
                &cli::NewTask {
                    title: &title,
                    body: body.as_deref(),
                    team: team.as_deref(),
                    labels: &label,
                    source: source.as_deref(),
                    repo: repo.as_deref(),
                },
            )?;
            println!("{identifier}  {url}");

            if and_dispatch {
                // It has to be on the board before it can be dispatched.
                cli::sync_once(&paths, log.clone())?;
                let engine = cli::engine_from_paths(&paths, log.clone())?;
                // `AGE-14` is Linear; `owner/repo#87` is GitHub.
                let id = if identifier.contains('/') {
                    format!("gh:{identifier}")
                } else {
                    format!("linear:{identifier}")
                };
                let t = engine
                    .db
                    .get_task(&id)?
                    .ok_or_else(|| anyhow::anyhow!("{identifier} did not reach the board"))?;
                let h = herdr::Herdr::discover(log.clone());
                let plan = dispatch::dispatch(&engine, &h, &log, &t, &dispatch::Overrides::default())?;
                println!("dispatched to ws:{} ({})", plan.workspace, plan.runtime);
            }
            Ok(())
        }
        Command::Stats { since_days, json } => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            let s = stats::run(&paths, log, since_days)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                stats::print(&s);
            }
            Ok(())
        }
        Command::Cancel { task } => {
            let log = Arc::new(Logger::new(paths.logfile(), true));
            let engine = cli::engine_from_paths(&paths, log.clone())?;
            let h = herdr::Herdr::discover(log.clone());
            let t = engine
                .db
                .get_task(&task)?
                .ok_or_else(|| anyhow::anyhow!("no task {task}"))?;
            let parent = dispatch::cancel(&engine, &h, &log, &t)?;
            engine.rederive_all()?;
            println!("cancelled {} — the issue is still open", t.identifier);
            // Nothing tells the parent. Say so where the operator will see it,
            // rather than leaving a blocked agent to be discovered later.
            if let Some(p) = parent {
                println!("{} — not notified", p.phrase());
            }
            Ok(())
        }
        Command::Gc {
            older_than,
            dry_run,
        } => {
            // File only: gc's stdout *is* the report, and argv logging on
            // stderr interleaves straight through the middle of it.
            let log = Arc::new(Logger::new(paths.logfile(), false));
            // herdr owns the checkouts, so it is the one that knows where they
            // are; the routes say which repositories to ask about.
            let cfg = config::RoutingConfig::load_or_default(&paths.routing());
            let h = herdr::Herdr::discover(log.clone());
            let report = gc::run(&paths, &cfg, &h, log, &older_than, dry_run)?;
            gc::print_report(&report);
            // A checkout that could not be removed is a result the operator has
            // to act on, not a detail in the middle of a summary.
            if !report.skipped.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Integration { action } => {
            let log = Arc::new(Logger::new(paths.logfile(), true));
            match action {
                IntegrationAction::Install { agent } => {
                    integration::check_supported(&agent)?;
                    integration::install(&log)?;
                    println!("wrote {}", integration::override_path()?.display());
                    // The running server holds manifests in memory.
                    let h = herdr::Herdr::discover(log.clone());
                    match h.reload_agent_manifests() {
                        Ok(()) => println!("herdr reloaded its manifests — working state is live"),
                        Err(e) => println!(
                            "could not reload herdr's manifests ({e}); \
                             run `herdr server reload-agent-manifests`"
                        ),
                    }
                }
                IntegrationAction::Uninstall { agent } => {
                    integration::check_supported(&agent)?;
                    integration::uninstall(&log)?;
                    let h = herdr::Herdr::discover(log.clone());
                    let _ = h.reload_agent_manifests();
                    println!("removed");
                }
                IntegrationAction::Status => {
                    if integration::installed() {
                        println!(
                            "installed ({})",
                            integration::override_path()?.display()
                        );
                    } else {
                        println!(
                            "not installed — a working or blocked Claude pane reports `idle`"
                        );
                    }
                    for agent in conventions::AGENTS {
                        let path = conventions::instruction_path(agent)?;
                        println!(
                            "{agent} conventions: {} ({})",
                            if conventions::installed(agent) {
                                "installed"
                            } else {
                                "missing or stale"
                            },
                            path.display()
                        );
                    }
                }
                IntegrationAction::InstallConventions { agent } => {
                    for agent in agents_for(agent.as_deref()) {
                        let path = conventions::install(&log, &agent)?;
                        println!("wrote {}", path.display());
                    }
                }
                IntegrationAction::UninstallConventions { agent } => {
                    for agent in agents_for(agent.as_deref()) {
                        match conventions::uninstall(&log, &agent)? {
                            Some(path) => println!("removed from {}", path.display()),
                            None => println!("{agent}: nothing of ours to remove"),
                        }
                    }
                }
            }
            Ok(())
        }
        Command::Doctor => {
            let checks = cli::doctor(&paths)?;
            if !cli::print_doctor(&checks) {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Demo { scenario, list } => {
            if list {
                for s in ui::fixtures::ALL {
                    println!("{s}");
                }
                return Ok(());
            }
            ui::demo::run(scenario.as_deref().unwrap_or(ui::fixtures::POPULATED))
        }
    }
}
