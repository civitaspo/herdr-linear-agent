//! The command line. People use Herdr actions and Linear; the commands here
//! are for the plugin manifest, the ticker and the agents it starts, so every
//! name is spelled out in full.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::actions::{self, Action};
use crate::commands::{self, Session, WorkerStart};
use crate::config::Config;
use crate::files::read_text_arg;
use crate::herdr;
use crate::paths::{Ctx, Env};
use crate::process::RealRunner;
use crate::{progress, ticker};

#[derive(Parser)]
#[command(name = "herdr-linear-agent", version = crate::VERSION, about = "Run a coordinator and per-repository workers for Linear issues delegated to this plugin's Linear app user")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The plugin's startup hook: starts the ticker detached and exits.
    Startup,
    /// A Herdr action.
    Action {
        action: Action,
        /// For `login`: log in to this workspace of the config again.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Manage the background ticker.
    Ticker {
        #[command(subcommand)]
        command: TickerCommand,
    },
    /// Browse past runs, their files and their agents' transcripts.
    History,
    /// Print the coordinator sheet.
    Skill {
        /// The issue key to fill into the commands.
        key: Option<String>,
    },
    /// Print a run's digest: issue, conversation, catalog, profiles, workers and inbox.
    Context { key: String },
    /// Mark inbox items as handled.
    Inbox {
        #[command(subcommand)]
        command: InboxCommand,
    },
    /// Replace the plan shown in the Linear session.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Post a progress note to the Linear session.
    Say(TextArgs),
    /// Ask a person a question in the Linear session.
    Ask {
        #[command(flatten)]
        text: TextArgs,
        /// A choice, as `label=value`; repeatable.
        #[arg(long = "option", value_parser = parse_option)]
        options: Vec<(String, String)>,
    },
    /// End the coordinator's turn without finishing the run.
    Wait(TextArgs),
    /// Post a summary and advance the issue to review or postmortem.
    Finish(TextArgs),
    /// Start, prompt or restart workers.
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    /// Manage the local supervisor's reports and portable memory.
    Supervisor {
        #[command(subcommand)]
        command: SupervisorCommand,
    },
    /// Report a worker's progress from its own pane.
    Report(ReportArgs),
    /// Tools for checking the plugin against a real Herdr.
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

#[derive(Subcommand)]
enum DebugCommand {
    /// Run development commands with isolated config, state and a dedicated session.
    Harness {
        #[command(subcommand)]
        command: crate::dev_harness::Command,
    },
    /// Start one real coordinator without prompting it, then remove its workspace.
    AgentSmoke(crate::agent_smoke::Options),
    /// Follow the Herdr session and print one JSON line per wake.
    HerdrWatch {
        /// The socket to watch; the configured session's by default.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum TickerCommand {
    /// Start the ticker unless one of this version is running.
    Start,
    /// Ask the running ticker to exit.
    Stop,
    /// Show whether a ticker is running.
    Status,
    /// Run the loop in the foreground (the detached process runs this).
    Run,
}

#[derive(Subcommand)]
enum InboxCommand {
    Done {
        key: String,
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    Set {
        key: String,
        /// A Markdown checklist; `-` reads standard input.
        #[arg(long)]
        file: String,
    },
}

#[derive(Args)]
struct TextArgs {
    key: String,
    /// The text; `-` reads standard input.
    #[arg(long)]
    text_file: String,
}

#[derive(Subcommand)]
enum WorkerCommand {
    /// Start a worker in a new worktree of a catalog repository.
    Start {
        key: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        profile: String,
        #[arg(long)]
        title: String,
        /// The task; `-` reads standard input.
        #[arg(long)]
        task_file: String,
    },
    /// Send a worker a follow-up.
    Prompt {
        key: String,
        id: String,
        #[arg(long)]
        text_file: String,
    },
    /// Start a worker again in its worktree, optionally with another profile.
    Restart {
        key: String,
        id: String,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Request one-time Linear authorization to merge this worker's PR.
    RequestMerge { key: String, id: String },
    /// Squash-merge an authorized worker PR.
    Merge { key: String, id: String },
}

#[derive(Subcommand)]
enum SupervisorCommand {
    /// Read or resume the current bounded report batch.
    Context {
        #[arg(long)]
        generation: u64,
    },
    /// Commit a decision and its portable memory delta.
    Decide {
        #[arg(long)]
        generation: u64,
        #[arg(long)]
        decision_file: String,
    },
    /// Search committed knowledge in the current authorization scope.
    Memory {
        #[command(subcommand)]
        command: SupervisorMemoryCommand,
    },
    /// Read a plugin-issued report, decision or topic handle.
    Show { handle: String },
    /// Inspect and maintain the committed generated-profile catalog.
    Profiles {
        #[command(subcommand)]
        command: SupervisorProfilesCommand,
    },
}

#[derive(Subcommand)]
enum SupervisorProfilesCommand {
    /// List authorized generated revisions and mark each current catalog entry.
    List,
    /// Show one exact committed generated revision.
    Show {
        id: String,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Retire the expected active revision for future runs.
    Retire {
        id: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long)]
        reason: String,
        #[arg(long = "source-report")]
        source_report_ids: Vec<String>,
    },
    /// Publish a new revision using the content of a retained revision.
    Rollback {
        id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        reason: String,
    },
    /// Export a revision as ordinary profile files into a new exact directory.
    Export {
        id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        name: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum SupervisorMemoryCommand {
    Search { query: String },
}

#[derive(Args)]
struct ReportArgs {
    /// Rough progress of the whole task, 0 to 100.
    #[arg(long, conflicts_with = "unknown")]
    percent: Option<u8>,
    /// The scope is not clear yet.
    #[arg(long)]
    unknown: bool,
    /// Two to four words, for example `Testing changes` or `Waiting for you`.
    #[arg(long)]
    activity: String,
}

fn parse_option(text: &str) -> Result<(String, String), String> {
    commands::parse_option(text).map_err(|e| e.to_string())
}

pub async fn run() -> Result<()> {
    let command = Cli::parse().command;
    let env = Env::from_process()?;
    // This opt-in check must never inspect or chmod the ordinary state folder,
    // start the ticker, or enter any credential-loading path.
    let command = match command {
        Command::Debug {
            command: DebugCommand::AgentSmoke(options),
        } => return crate::agent_smoke::run(&env, options).await,
        Command::Debug {
            command: DebugCommand::Harness { command },
        } => return crate::dev_harness::run(&env, command),
        command => command,
    };
    let runner = RealRunner;
    let ctx = Ctx {
        env: &env,
        runner: &runner,
        detached_ticker: true,
    };

    let state_dir = ctx.state_dir();
    match std::fs::symlink_metadata(&state_dir) {
        Ok(_) => {
            ctx.ensure_state_dir()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("cannot inspect the state folder {}", state_dir.display())
            });
        }
    }

    match command {
        Command::Debug {
            command: DebugCommand::AgentSmoke(_) | DebugCommand::Harness { .. },
        } => unreachable!("handled before ordinary state initialization"),
        Command::Debug {
            command: DebugCommand::HerdrWatch { socket },
        } => herdr_watch(socket).await,
        Command::Startup => ticker::start(&ctx).await,
        Command::History => {
            let (env, runs) = (env.clone(), ctx.runs_dir());
            let session = Config::load(&ctx.config_dir())
                .ok()
                .and_then(|c| c.herdr.session);
            tokio::task::spawn_blocking(move || crate::history::run(&env, &runs, session)).await?
        }
        Command::Action { action, workspace } => {
            actions::run(&ctx, action, workspace.as_deref()).await
        }
        Command::Ticker { command } => match command {
            TickerCommand::Start => ticker::start(&ctx).await,
            TickerCommand::Stop => ticker::stop(&ctx.state_dir()).await,
            TickerCommand::Status => {
                println!("{}", ticker::describe(&ctx.state_dir()));
                Ok(())
            }
            TickerCommand::Run => ticker::run(&ctx).await,
        },
        Command::Skill { key } => commands::skill(key.as_deref()),
        Command::Context { key } => {
            // Without Herdr the digest shows the recorded groups.
            let session = Session::configured(&ctx).await.ok();
            commands::context(&ctx, session.as_ref(), &key).await
        }
        Command::Inbox {
            command: InboxCommand::Done { key, ids, all },
        } => commands::inbox_done(&ctx, &key, &ids, all),
        Command::Plan {
            command: PlanCommand::Set { key, file },
        } => commands::plan_set(&ctx, &key, &read_text_arg(&file)?).await,
        Command::Say(args) => {
            commands::say(&ctx, &args.key, &read_text_arg(&args.text_file)?).await
        }
        Command::Ask { text, options } => {
            commands::ask(&ctx, &text.key, &read_text_arg(&text.text_file)?, &options).await
        }
        Command::Wait(args) => {
            let text = read_text_arg(&args.text_file)?;
            let session = Session::configured(&ctx).await?;
            commands::wait(&ctx, &session, &args.key, &text).await
        }
        Command::Finish(args) => {
            let text = read_text_arg(&args.text_file)?;
            let session = Session::configured(&ctx).await?;
            commands::finish(&ctx, &session, &args.key, &text).await
        }
        Command::Worker { command } => worker(&ctx, command).await,
        Command::Supervisor { command } => match command {
            SupervisorCommand::Context { generation } => {
                commands::supervisor_context(&ctx, generation)
            }
            SupervisorCommand::Decide {
                generation,
                decision_file,
            } => commands::supervisor_decide(&ctx, generation, &read_text_arg(&decision_file)?),
            SupervisorCommand::Memory {
                command: SupervisorMemoryCommand::Search { query },
            } => commands::supervisor_memory_search(&ctx, &query),
            SupervisorCommand::Show { handle } => commands::supervisor_show(&ctx, &handle),
            SupervisorCommand::Profiles { command } => match command {
                SupervisorProfilesCommand::List => commands::supervisor_profiles_list(&ctx),
                SupervisorProfilesCommand::Show { id, revision } => {
                    commands::supervisor_profiles_show(&ctx, &id, revision)
                }
                SupervisorProfilesCommand::Retire {
                    id,
                    expected_revision,
                    reason,
                    source_report_ids,
                } => commands::supervisor_profiles_retire(
                    &ctx,
                    &id,
                    expected_revision,
                    &reason,
                    &source_report_ids,
                ),
                SupervisorProfilesCommand::Rollback {
                    id,
                    revision,
                    reason,
                } => commands::supervisor_profiles_rollback(&ctx, &id, revision, &reason),
                SupervisorProfilesCommand::Export {
                    id,
                    revision,
                    name,
                    output,
                } => commands::supervisor_profiles_export(&ctx, &id, revision, &name, &output),
            },
        },
        Command::Report(args) => {
            progress::report(&env, args.percent.filter(|_| !args.unknown), &args.activity).await
        }
    }
}

async fn worker(ctx: &Ctx<'_>, command: WorkerCommand) -> Result<()> {
    match command {
        WorkerCommand::Start {
            key,
            repo,
            profile,
            title,
            task_file,
        } => {
            let session = Session::configured(ctx).await?;
            let args = WorkerStart {
                repo,
                profile,
                title,
                task: read_text_arg(&task_file)?,
            };
            commands::worker_start(ctx, &session, &key, &args)
                .await
                .map(|_| ())
        }
        WorkerCommand::Prompt { key, id, text_file } => {
            let session = Session::configured(ctx).await?;
            let text = read_text_arg(&text_file)?;
            commands::worker_prompt(ctx, &session, &key, &id, &text).await
        }
        WorkerCommand::Restart { key, id, profile } => {
            let session = Session::configured(ctx).await?;
            commands::worker_restart(ctx, &session, &key, &id, profile.as_deref())
                .await
                .map(|_| ())
        }
        WorkerCommand::RequestMerge { key, id } => {
            commands::worker_request_merge(ctx, &key, &id).await
        }
        WorkerCommand::Merge { key, id } => commands::worker_merge(ctx, &key, &id).await,
    }
}

async fn herdr_watch(socket: Option<PathBuf>) -> Result<()> {
    let socket = match socket {
        Some(socket) => socket,
        None => {
            let env = Env::from_process()?;
            let config = Config::load(&env.config_dir())?;
            herdr::session_socket(&env.herdr_bin(), config.herdr.session.as_deref()).await?
        }
    };
    let client = herdr::Client::new(socket);
    let mut rx = herdr::wake(client.clone());
    let mut printed = None;
    while rx.changed().await.is_ok() {
        let link = rx.borrow_and_update().clone();
        // A new error alone is not a wake.
        if printed == Some((link.connected, link.wakes)) {
            continue;
        }
        printed = Some((link.connected, link.wakes));
        let snapshot = client.snapshot().await.ok();
        let agents: Vec<_> = snapshot
            .iter()
            .flat_map(|s| &s.agents)
            .map(|a| {
                serde_json::json!({
                    "pane": a.pane, "name": a.name, "status": a.status, "seq": a.state_change_seq,
                })
            })
            .collect();
        let line = serde_json::json!({
            "at": jiff::Timestamp::now().to_string(),
            "connected": link.connected,
            "wakes": link.wakes,
            "panes": snapshot.as_ref().map(|s| s.panes.len()),
            "agents": agents,
        });
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn agent_smoke_requires_an_explicit_session_and_exactly_one_target() {
        let prefix = ["herdr-linear-agent", "debug", "agent-smoke"];
        for args in [
            vec![],
            vec!["--kind", "codex"],
            vec!["--session", "test"],
            vec!["--session", "test", "--kind", "codex", "--profile", "deep"],
        ] {
            assert!(Cli::try_parse_from(prefix.into_iter().chain(args)).is_err());
        }
        assert!(
            Cli::try_parse_from(
                prefix
                    .into_iter()
                    .chain(["--session", "test", "--kind", "codex"])
            )
            .is_ok()
        );
    }

    #[test]
    fn the_manifest_names_only_existing_actions() {
        let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
        for action in manifest["actions"].as_array().unwrap() {
            let command: Vec<&str> = action["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let mut argv = vec!["herdr-linear-agent"];
            argv.extend(&command[1..]);
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
            assert_eq!(command[2], action["id"].as_str().unwrap());
        }
        let panes = manifest["panes"].as_array().unwrap();
        for pane in panes {
            let command: Vec<&str> = pane["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let mut argv = vec!["herdr-linear-agent"];
            argv.extend(&command[1..]);
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
        }
        assert!(
            panes.iter().any(|p| p["id"].as_str() == Some("history")),
            "the browse action opens the history pane"
        );
        // Herdr refuses a manifest whose link handler has no title (0.9.1).
        for handler in manifest["link_handlers"].as_array().unwrap() {
            assert!(
                handler["title"].as_str().is_some_and(|t| !t.is_empty()),
                "{handler}"
            );
        }
    }

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
        assert!(
            Cli::try_parse_from(["herdr-linear-agent", "wait", "DATA-1", "--text-file", "-"])
                .is_ok(),
            "wait ends the coordinator turn without finishing the run"
        );
        let parsed = Cli::try_parse_from([
            "herdr-linear-agent",
            "ask",
            "DATA-1",
            "--text-file",
            "-",
            "--option",
            "Yes=yes",
            "--option",
            "No",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Command::Ask { ref options, .. } if options.len() == 2));
        assert!(Cli::try_parse_from(["herdr-linear-agent", "action", "open-issue"]).is_ok());
        assert!(Cli::try_parse_from(["herdr-linear-agent", "action", "logout"]).is_err());
        assert!(
            Cli::try_parse_from([
                "herdr-linear-agent",
                "debug",
                "herdr-watch",
                "--socket",
                "/s"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "herdr-linear-agent",
                "worker",
                "start",
                "DATA-1",
                "--repo",
                "api",
                "--profile",
                "standard",
                "--title",
                "T",
                "--task-file",
                "-"
            ])
            .is_ok()
        );
    }

    #[test]
    fn supervisor_profile_operator_commands_parse() {
        for argv in [
            vec!["herdr-linear-agent", "supervisor", "profiles", "list"],
            vec![
                "herdr-linear-agent",
                "supervisor",
                "profiles",
                "show",
                "p-example",
                "--revision",
                "2",
            ],
            vec![
                "herdr-linear-agent",
                "supervisor",
                "profiles",
                "retire",
                "p-example",
                "--expected-revision",
                "2",
                "--reason",
                "Adopt manual profile",
                "--source-report",
                "r-00000000000000000000000000000000",
            ],
            vec![
                "herdr-linear-agent",
                "supervisor",
                "profiles",
                "rollback",
                "p-example",
                "--revision",
                "1",
                "--reason",
                "Restore prior instructions",
            ],
            vec![
                "herdr-linear-agent",
                "supervisor",
                "profiles",
                "export",
                "p-example",
                "--revision",
                "1",
                "--name",
                "manual-example",
                "--output",
                "/tmp/profile",
            ],
        ] {
            assert!(Cli::try_parse_from(argv.clone()).is_ok(), "{argv:?}");
        }
    }
}
