// SPDX-License-Identifier: MPL-2.0
//! `slashit` — the command line front end for a running SlashIt instance.
//!
//! Every subcommand is one request and one response over the control channel.
//! The instance may be the desktop app or the headless daemon; the CLI does not
//! care, and `slashit ping` is how a user finds out which one answered.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use slashit::{client, output, pr_review, task_ref};
use slashit_ipc::client::{ClientError, ClientOptions};
use slashit_ipc::{InstanceInfo, IpcRequest, PrCommentReview};

#[derive(Parser)]
#[command(name = "slashit", about = "Control the SlashIt workspace manager", version)]
struct Cli {
    /// Output raw JSON instead of formatted tables
    #[arg(long, global = true)]
    json: bool,

    /// Wait for the app to start if not running
    #[arg(long, global = true)]
    wait: bool,

    /// Timeout in seconds when using --wait (default: 30)
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    /// Endpoint to connect to, overriding SLASHIT_IPC_ENDPOINT.
    ///
    /// Forms: unix:/path/to.sock, pipe:\\.\pipe\name, tcp:127.0.0.1:8731.
    /// A bare path is treated as a Unix socket.
    #[arg(long, global = true, value_name = "ENDPOINT")]
    endpoint: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show application status
    Status,
    /// List all projects
    Projects,
    /// List tasks
    Tasks {
        /// Filter by project ID
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one task's full record as JSON (read-only)
    Inspect {
        /// Task ID
        task_id: String,
    },
    /// Show one Task's recent execution evidence (read-only)
    ///
    /// The answer reports the Task's persisted runs and their outcomes, the
    /// AI-review outcome and signoff where kept, the configured Worker model
    /// (the saved Task model) and the Task's latest observed model where a
    /// run recorded one — a Task-level identity, not attributed to any single
    /// run — plus the newest Activity milestones, and this instance's hold on
    /// the latest run, which is memory only; the answer says when it is gone.
    /// The number of runs and the size of the answer are bounded by the
    /// instance.
    Runs {
        /// Task ID
        task_id: String,
    },
    /// Show a Task's review state as JSON (read-only)
    ///
    /// Decisions, whether the approval still covers the branch, the branch's
    /// commit, and the requests agents filed with what became of them.
    Review {
        /// Task ID
        task_id: String,
    },
    /// Show a Task's delivery state as JSON (read-only)
    Delivery {
        /// Task ID
        task_id: String,
    },
    /// Show or export a Task's PR Comment Review (read-only)
    ///
    /// The stored analysis of the pull request's review comments: each
    /// finding's stable ID, the reviewer comment, the agent's summary and
    /// reasoning, the original proposal, any Human override, the effective
    /// instruction, the decision and approval, and what was fixed and
    /// published. Nothing is analyzed, no agent is started, and no approval or
    /// plan is changed. `--json` prints the full answer.
    PrReview {
        /// Task ID
        task_id: String,
        /// Also write the review as Markdown to this new file; an existing
        /// file is never replaced
        #[arg(long, value_name = "PATH")]
        markdown: Option<PathBuf>,
    },
    /// Ask a person to approve a commit, or to authorize opening its pull request
    ///
    /// This only files the request. A person decides it in the desktop app;
    /// `slashit review` shows the answer.
    Request {
        /// Task ID
        task_id: String,
        #[arg(value_parser = ["approve", "open-pr"])]
        operation: String,
        /// Full object ID of the commit, as `slashit review` reports it
        #[arg(long)]
        commit: String,
        /// The name you give yourself, shown to the person and recorded
        #[arg(long)]
        actor: String,
    },
    /// Open the pull request a person authorized
    ///
    /// Works once, for the commit the request names, within the time the
    /// authorization allows.
    OpenPr {
        /// Task ID
        task_id: String,
        /// Request ID that `slashit request <task> open-pr` returned
        #[arg(long)]
        request: String,
        /// The name you give yourself, recorded as who opened it
        #[arg(long)]
        actor: String,
    },
    /// Create a new task
    Create {
        /// Project ID
        #[arg(long)]
        project: String,
        /// Task title
        title: String,
        /// Task description
        #[arg(long)]
        description: Option<String>,
        /// Priority: urgent, high, medium, low
        #[arg(long)]
        priority: Option<String>,
        /// Review mode: manual (no AI Review, run goes to Human Review) or
        /// automatic (AI Review runs after each run; the default)
        #[arg(long, value_parser = ["manual", "automatic"])]
        review_mode: Option<String>,
    },
    /// Move a task to a different status
    Move {
        /// Task ID
        task_id: String,
        /// Target status: backlog, queue, in_progress, ai_review, human_review, done, error
        status: String,
    },
    /// Edit task properties
    Edit {
        /// Task ID
        task_id: String,
        /// New title
        #[arg(long)]
        title: Option<String>,
        /// New description
        #[arg(long)]
        description: Option<String>,
        /// New priority
        #[arg(long)]
        priority: Option<String>,
    },
    /// Set the model a Task's coding runs are started on.
    ///
    /// The value is saved exactly as given: for a Claude Code Task it is `default`
    /// or a model the model picker offers, and for an OpenCode Task it is a
    /// `provider/model` id. The instance refuses an identifier it cannot give the
    /// Task's Worker rather than saving something else.
    SetModel {
        /// Task ID
        task_id: String,
        /// Model identifier for the Task's coding runs
        model: String,
    },
    /// Set how a Task's finished coding runs are reviewed
    ///
    /// manual (no AI Review, the run goes to Human Review) or automatic (AI
    /// Review runs after each run; the default). A change affects only runs
    /// that have not started.
    SetReviewMode {
        /// Task ID
        task_id: String,
        #[arg(value_parser = ["manual", "automatic"])]
        review_mode: String,
    },
    /// Set or clear a Task's structured dependency
    ///
    /// `slashit set-dependencies <task> <dependency>` names the single Task this
    /// Task runs after, and `slashit set-dependencies <task>` clears it. The
    /// instance refuses a malformed or foreign Task id, a cycle, and a Task that
    /// holds a checkout or is awaiting a review decision.
    SetDependencies {
        /// Task ID
        task_id: String,
        /// Task ID this Task depends on; at most one
        dependencies: Vec<String>,
    },
    /// Delete a task
    Delete {
        /// Task ID
        task_id: String,
    },
    /// Show queue status
    Queue,
    /// Add a task to the queue
    Enqueue {
        /// Task ID
        task_id: String,
    },
    /// List active terminal sessions
    Terminals,
    /// Bring the app window to front
    Show,
    /// Ask the running instance to shut down gracefully
    ///
    /// The GUI confirms first when agents or terminals are still running; a
    /// daemon drains them within its grace period.
    Quit,
    /// Close admission on the running instance and report the work a restart
    /// would interrupt. Admission stays closed until `resume` with the same
    /// operation. The reply prints the operation; without --operation a fresh
    /// one is generated.
    Quiesce {
        /// Operation id that owns the quiesce (1 to 64 letters, digits or hyphens)
        #[arg(long)]
        operation: Option<String>,
    },
    /// Reopen admission closed by `quiesce` for the same operation. Refused
    /// for any other operation, and never reopens a shutdown.
    Resume {
        /// The operation id the quiesce printed
        #[arg(long)]
        operation: String,
    },
    /// Report which instance is answering, and on which endpoint
    Ping,
    /// Show every feature flag with the value in force and where it came from
    Features,
    /// Write one Project's portable export to a new file
    ///
    /// The bundle holds the Project, its Repository identity (never its local
    /// path) and its Tasks. Credentials, runtime state and worktree paths are
    /// left out. The file is created owner-only and never replaces an
    /// existing path. A Project with a Task that is queued, running or in
    /// automatic AI Review cannot be exported yet; a Task waiting for a
    /// Human Review decision can.
    Export {
        /// Project ID
        project: String,
        /// New file to write; relative paths are resolved from this shell
        path: PathBuf,
    },
    /// Validate an export file, and describe or apply its import
    ///
    /// `--preview` changes nothing. `--apply` creates the Project and its Tasks
    /// as inert records, and needs the `--checksum` that a preview of the same
    /// file reported. The Project is created without a Repository.
    Import {
        /// Export file to read
        path: PathBuf,
        /// Describe the import without applying it
        #[arg(long, conflicts_with = "apply")]
        preview: bool,
        /// Create the Project in the running instance
        #[arg(long, conflicts_with = "preview")]
        apply: bool,
        /// Payload checksum reported by `--preview` for this file
        #[arg(long, value_name = "SHA256", requires = "apply")]
        checksum: Option<String>,
    },
    /// Inspect the headless daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Report whether a daemon is answering on the endpoint
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let options = client::options(cli.endpoint.as_deref(), cli.wait, cli.timeout)?;
    if let Commands::Import { preview, apply, checksum, .. } = &cli.command {
        if preview == apply {
            eprintln!("Error: pass exactly one of --preview or --apply");
            std::process::exit(2);
        }
        if *apply && checksum.is_none() {
            eprintln!("Error: --apply needs --checksum, the payload checksum that --preview reports for this file");
            std::process::exit(2);
        }
    }

    // Not a plain request/response: "no daemon is running" is an answer
    // rather than a failure, and it has to set the exit status.
    if let Commands::Daemon { command: DaemonCommands::Status } = &cli.command {
        return daemon_status(&options, cli.json).await;
    }
    let mut cli = cli;
    resolve_task_refs(&mut cli.command, &options).await;
    let request = request_for(&cli.command)?;
    if let IpcRequest::Quiesce { operation } = &request {
        // Printed before the send: if the reply is lost after the instance
        // closed admission, this is the only way to name the operation to resume.
        eprintln!(
            "quiesce operation: {operation} (reopen with: slashit resume --operation {operation})"
        );
    }

    let response = slashit_ipc::client::send(&request, &options).await?;

    if !response.ok {
        let msg = response.error.unwrap_or_else(|| "Unknown error".to_string());
        eprintln!("{}", output::error_line(&msg));
        std::process::exit(1);
    }

    if let Commands::PrReview { markdown: Some(path), .. } = &cli.command {
        export_pr_review(&response.data, path);
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response.data)?);
        return Ok(());
    }

    // Format output based on command type
    match &cli.command {
        // The record is JSON by nature; there is no table form.
        Commands::Inspect { .. }
        | Commands::Review { .. }
        | Commands::Delivery { .. }
        | Commands::Request { .. }
        | Commands::OpenPr { .. }
        | Commands::Quiesce { .. }
        | Commands::Resume { .. } => {
            println!("{}", serde_json::to_string_pretty(&response.data)?)
        }
        Commands::Status => output::print_status(&response.data),
        Commands::Projects => output::print_projects(&response.data),
        Commands::Tasks { .. } => output::print_tasks(&response.data),
        Commands::Queue => output::print_queue(&response.data),
        Commands::Terminals => output::print_terminals(&response.data),
        Commands::Ping => output::print_instance(&response.data),
        Commands::Features => output::print_features(&response.data),
        Commands::Runs { .. } => output::print_runs(&response.data),
        Commands::PrReview { .. } => {
            match serde_json::from_value::<PrCommentReview>(response.data.clone()) {
                Ok(review) => print!("{}", pr_review::summary_text(&review)),
                Err(_) => println!("{}", serde_json::to_string_pretty(&response.data)?),
            }
        }
        _ => {
            // For mutation commands, print a simple success message
            if let Some(msg) = response.data.as_str() {
                println!("{msg}");
            } else if response.data.is_null() {
                println!("Done.");
            } else {
                println!("{}", serde_json::to_string_pretty(&response.data)?);
            }
        }
    }

    Ok(())
}

/// Every Task reference a command carries, including dependencies.
fn task_refs(command: &mut Commands) -> Vec<&mut String> {
    match command {
        Commands::Inspect { task_id }
        | Commands::Runs { task_id }
        | Commands::Review { task_id }
        | Commands::Delivery { task_id }
        | Commands::PrReview { task_id, .. }
        | Commands::Request { task_id, .. }
        | Commands::OpenPr { task_id, .. }
        | Commands::Move { task_id, .. }
        | Commands::Edit { task_id, .. }
        | Commands::SetModel { task_id, .. }
        | Commands::SetReviewMode { task_id, .. }
        | Commands::Delete { task_id }
        | Commands::Enqueue { task_id } => vec![task_id],
        Commands::SetDependencies { task_id, dependencies } => {
            let mut refs = vec![task_id];
            refs.extend(dependencies.iter_mut());
            refs
        }
        _ => Vec::new(),
    }
}

/// Replaces each Task reference with a full Task ID, or exits with status 1
/// before anything is sent. A full UUID needs no lookup; a prefix is resolved
/// from the Tasks the instance lists, so a mutation never acts on a guess.
async fn resolve_task_refs(command: &mut Commands, options: &ClientOptions) {
    let mut refs = task_refs(command);
    if refs.is_empty() {
        return;
    }
    let mut parsed = Vec::new();
    for r in &refs {
        match task_ref::parse(r) {
            Ok(p) => parsed.push(p),
            Err(e) => fail(&e),
        }
    }
    let mut tasks = None;
    if parsed.iter().any(|p| matches!(p, task_ref::TaskRef::Prefix(_))) {
        let request = IpcRequest::ListTasks { project_id: None };
        match slashit_ipc::client::send(&request, options).await {
            Ok(response) if response.ok => tasks = Some(response.data),
            Ok(response) => fail(&response.error.unwrap_or_else(|| "Unknown error".to_string())),
            Err(e) => fail(&e.to_string()),
        }
    }
    for (slot, p) in refs.iter_mut().zip(parsed) {
        **slot = match p {
            task_ref::TaskRef::Full(id) => id,
            task_ref::TaskRef::Prefix(prefix) => {
                match task_ref::resolve_prefix(&prefix, tasks.as_ref().unwrap_or(&serde_json::Value::Null)) {
                    Ok(id) => id,
                    Err(e) => fail(&e),
                }
            }
        };
    }
}

/// Write the review's Markdown export to the new file `path`, or exit with
/// status 1. The instance only read; the file is written by this process.
fn export_pr_review(data: &serde_json::Value, path: &std::path::Path) {
    let review: PrCommentReview = match serde_json::from_value(data.clone()) {
        Ok(review) => review,
        Err(e) => fail(&format!("could not understand the instance's reply: {e}")),
    };
    if let Err(e) = pr_review::write_new_file(path, &pr_review::markdown(&review)) {
        fail(&format!("could not write {}: {e}", path.display()));
    }
    eprintln!("Wrote {}", path.display());
}

fn fail(message: &str) -> ! {
    eprintln!("{}", output::error_line(message));
    std::process::exit(1)
}

/// The path as an absolute one, resolved from this shell's directory. The
/// instance writes relative to its own directory, so a relative path must be
/// made absolute here.
fn absolute_path(path: &std::path::Path) -> Result<String> {
    Ok(std::path::absolute(path)?.to_string_lossy().into_owned())
}

/// A quiesce operation id for a `quiesce` run without --operation. It is unique
/// per process and time, and well inside the 64-character limit.
fn generated_operation_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("cli-{}-{nanos:x}", std::process::id())
}

/// The request a command sends to the running instance. `main` answers the
/// daemon status check itself, so it never reaches this mapping.
fn request_for(command: &Commands) -> Result<IpcRequest> {
    Ok(match command {
        Commands::Status => IpcRequest::Status,
        Commands::Projects => IpcRequest::ListProjects,
        Commands::Tasks { project } => IpcRequest::ListTasks {
            project_id: project.clone(),
        },
        Commands::Inspect { task_id } => IpcRequest::GetTask {
            task_id: task_id.clone(),
        },
        Commands::Runs { task_id } => IpcRequest::GetTaskEvidence {
            task_id: task_id.clone(),
        },
        Commands::Review { task_id } => IpcRequest::GetReviewState {
            task_id: task_id.clone(),
        },
        Commands::Delivery { task_id } => IpcRequest::GetDeliveryState {
            task_id: task_id.clone(),
        },
        Commands::PrReview { task_id, .. } => IpcRequest::GetPrCommentReview {
            task_id: task_id.clone(),
        },
        Commands::Request { task_id, operation, commit, actor } => IpcRequest::RequestOperation {
            task_id: task_id.clone(),
            operation: operation.clone(),
            commit: commit.clone(),
            actor: actor.clone(),
        },
        Commands::OpenPr { task_id, request, actor } => IpcRequest::OpenPr {
            task_id: task_id.clone(),
            request_id: request.clone(),
            actor: actor.clone(),
        },
        Commands::Create {
            project,
            title,
            description,
            priority,
            review_mode,
        } => IpcRequest::CreateTask {
            project_id: project.clone(),
            title: title.clone(),
            description: description.clone(),
            priority: priority.clone(),
            review_mode: review_mode.clone(),
        },
        Commands::Move { task_id, status } => IpcRequest::MoveTask {
            task_id: task_id.clone(),
            status: status.clone(),
        },
        Commands::Edit {
            task_id,
            title,
            description,
            priority,
        } => IpcRequest::EditTask {
            task_id: task_id.clone(),
            title: title.clone(),
            description: description.clone(),
            priority: priority.clone(),
        },
        Commands::SetModel { task_id, model } => IpcRequest::SetTaskModel {
            task_id: task_id.clone(),
            model: model.clone(),
        },
        Commands::SetReviewMode { task_id, review_mode } => IpcRequest::SetTaskReviewMode {
            task_id: task_id.clone(),
            review_mode: review_mode.clone(),
        },
        Commands::SetDependencies { task_id, dependencies } => IpcRequest::SetTaskDependencies {
            task_id: task_id.clone(),
            dependencies: dependencies.clone(),
        },
        Commands::Delete { task_id } => IpcRequest::DeleteTask {
            task_id: task_id.clone(),
        },
        Commands::Queue => IpcRequest::QueueStatus,
        Commands::Enqueue { task_id } => IpcRequest::EnqueueTask {
            task_id: task_id.clone(),
        },
        Commands::Terminals => IpcRequest::ListTerminals,
        Commands::Show => IpcRequest::Show,
        Commands::Quit => IpcRequest::Quit,
        Commands::Quiesce { operation } => IpcRequest::Quiesce {
            operation: operation.clone().unwrap_or_else(generated_operation_id),
        },
        Commands::Resume { operation } => IpcRequest::Resume {
            operation: operation.clone(),
        },
        Commands::Ping => IpcRequest::Ping,
        Commands::Features => IpcRequest::Features,
        Commands::Export { project, path } => IpcRequest::ExportProject {
            project_id: project.clone(),
            path: absolute_path(path)?,
        },
        Commands::Import { path, apply: true, checksum, .. } => IpcRequest::ApplyImport {
            path: absolute_path(path)?,
            checksum: checksum.clone().unwrap_or_default(),
        },
        Commands::Import { path, .. } => IpcRequest::PreviewImport {
            path: absolute_path(path)?,
        },
        Commands::Daemon { .. } => {
            anyhow::bail!("daemon status is answered by `main`, not sent as a request")
        }
    })
}

/// Answer "is a daemon running?" with an exit status a script can branch on.
///
/// A GUI instance answering is deliberately *not* success: the question is
/// whether work continues without a desktop session, and the GUI is the case
/// where it does not.
async fn daemon_status(options: &ClientOptions, json: bool) -> Result<()> {
    let info = match slashit_ipc::client::send(&IpcRequest::Ping, options).await {
        Ok(response) if response.ok => serde_json::from_value::<InstanceInfo>(response.data)
            .map_err(|e| anyhow::anyhow!("could not understand the instance's reply: {e}"))?,
        Ok(response) => {
            let msg = response.error.unwrap_or_else(|| "Unknown error".to_string());
            eprintln!("{}", output::error_line(&msg));
            std::process::exit(1);
        }
        // Nothing listening is the expected negative answer, not a crash.
        Err(e @ ClientError::NotRunning(_)) => {
            report_no_daemon(json, &e.to_string(), &options.endpoint.to_string());
            std::process::exit(1);
        }
        Err(e) => return Err(e.into()),
    };

    let is_daemon = info.mode == "daemon";

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "running": is_daemon,
                "instance": info,
            }))?
        );
    } else if is_daemon {
        println!(
            "SlashIt daemon is running (pid {}, version {}) on {}",
            info.pid, info.version, info.endpoint
        );
    } else {
        println!(
            "No daemon: a {} instance (pid {}) is answering on {}",
            info.mode, info.pid, info.endpoint
        );
    }

    if is_daemon {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

fn report_no_daemon(json: bool, reason: &str, endpoint: &str) {
    if json {
        let payload = serde_json::json!({
            "running": false,
            "endpoint": endpoint,
            "reason": reason,
        });
        // Serialising a literal object cannot fail.
        if let Ok(text) = serde_json::to_string_pretty(&payload) {
            println!("{text}");
        }
    } else {
        println!("No daemon is answering on {endpoint}.");
        eprintln!("{reason}");
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    const TASK: &str = "6f96fefa-0000-0000-0000-000000000003";

    fn request_for_args(args: &[&str]) -> IpcRequest {
        let cli = Cli::try_parse_from(args).expect("the arguments parse");
        request_for(&cli.command).expect("the command maps to a request")
    }

    /// Every Task reference a command carries, dependencies included, is
    /// offered for prefix resolution; commands without one offer none.
    #[test]
    fn task_refs_cover_ids_and_dependencies() {
        let mut cli = Cli::try_parse_from(["slashit", "set-dependencies", "aaaaaaaa", "bbbbbbbb"])
            .expect("the arguments parse");
        let refs: Vec<String> = task_refs(&mut cli.command).into_iter().map(|r| r.clone()).collect();
        assert_eq!(refs, ["aaaaaaaa", "bbbbbbbb"]);
        let mut cli = Cli::try_parse_from(["slashit", "inspect", "aaaaaaaa"]).unwrap();
        assert_eq!(task_refs(&mut cli.command).len(), 1);
    }

    /// `slashit runs` reaches the evidence verb, and only that verb: it must
    /// not fall through to the full Task record `inspect` asks for.
    #[test]
    fn runs_sends_the_read_only_evidence_request() {
        match request_for_args(&["slashit", "runs", TASK]) {
            IpcRequest::GetTaskEvidence { task_id } => assert_eq!(task_id, TASK),
            other => panic!("runs mapped to {other:?}"),
        }
    }

    #[test]
    fn inspect_still_sends_the_full_task_record_request() {
        match request_for_args(&["slashit", "inspect", TASK]) {
            IpcRequest::GetTask { task_id } => assert_eq!(task_id, TASK),
            other => panic!("inspect mapped to {other:?}"),
        }
    }

    #[test]
    fn review_and_delivery_commands_map_to_their_requests() {
        match request_for_args(&["slashit", "review", TASK]) {
            IpcRequest::GetReviewState { task_id } => assert_eq!(task_id, TASK),
            other => panic!("review mapped to {other:?}"),
        }
        match request_for_args(&["slashit", "delivery", TASK]) {
            IpcRequest::GetDeliveryState { task_id } => assert_eq!(task_id, TASK),
            other => panic!("delivery mapped to {other:?}"),
        }
        let args = ["slashit", "request", TASK, "open-pr", "--commit", "c0", "--actor", "a"];
        match request_for_args(&args) {
            IpcRequest::RequestOperation { operation, commit, actor, .. } => {
                assert_eq!((operation.as_str(), commit.as_str(), actor.as_str()), ("open-pr", "c0", "a"));
            }
            other => panic!("request mapped to {other:?}"),
        }
        match request_for_args(&["slashit", "open-pr", TASK, "--request", "r", "--actor", "a"]) {
            IpcRequest::OpenPr { request_id, .. } => assert_eq!(request_id, "r"),
            other => panic!("open-pr mapped to {other:?}"),
        }
        assert!(Cli::try_parse_from(["slashit", "approve", TASK]).is_err());
        match request_for_args(&["slashit", "pr-review", TASK, "--markdown", "out.md"]) {
            IpcRequest::GetPrCommentReview { task_id } => assert_eq!(task_id, TASK),
            other => panic!("pr-review mapped to {other:?}"),
        }
    }

    #[test]
    fn the_daemon_command_is_not_a_request() {
        let cli = Cli::try_parse_from(["slashit", "daemon", "status"]).expect("parses");
        assert!(request_for(&cli.command).is_err());
    }
}
