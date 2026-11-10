// SPDX-License-Identifier: MPL-2.0
//! `slashit` — the command line front end for a running SlashIt instance.
//!
//! Every subcommand is one request and one response over the control channel.
//! The instance may be the desktop app or the headless daemon; the CLI does not
//! care, and `slashit ping` is how a user finds out which one answered.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use slashit::{client, output};
use slashit_ipc::client::{ClientError, ClientOptions};
use slashit_ipc::{InstanceInfo, IpcRequest};

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
    let request = request_for(&cli.command)?;

    let response = slashit_ipc::client::send(&request, &options).await?;

    if !response.ok {
        let msg = response.error.unwrap_or_else(|| "Unknown error".to_string());
        eprintln!("{}", output::error_line(&msg));
        std::process::exit(1);
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response.data)?);
        return Ok(());
    }

    // Format output based on command type
    match &cli.command {
        // The record is JSON by nature; there is no table form.
        Commands::Inspect { .. } => println!("{}", serde_json::to_string_pretty(&response.data)?),
        Commands::Status => output::print_status(&response.data),
        Commands::Projects => output::print_projects(&response.data),
        Commands::Tasks { .. } => output::print_tasks(&response.data),
        Commands::Queue => output::print_queue(&response.data),
        Commands::Terminals => output::print_terminals(&response.data),
        Commands::Ping => output::print_instance(&response.data),
        Commands::Features => output::print_features(&response.data),
        Commands::Runs { .. } => output::print_runs(&response.data),
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

/// The path as an absolute one, resolved from this shell's directory. The
/// instance writes relative to its own directory, so a relative path must be
/// made absolute here.
fn absolute_path(path: &std::path::Path) -> Result<String> {
    Ok(std::path::absolute(path)?.to_string_lossy().into_owned())
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

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    const TASK: &str = "6f96fefa-0000-0000-0000-000000000003";

    fn request_for_args(args: &[&str]) -> IpcRequest {
        let cli = Cli::try_parse_from(args).expect("the arguments parse");
        request_for(&cli.command).expect("the command maps to a request")
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
    fn the_daemon_command_is_not_a_request() {
        let cli = Cli::try_parse_from(["slashit", "daemon", "status"]).expect("parses");
        assert!(request_for(&cli.command).is_err());
    }
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
