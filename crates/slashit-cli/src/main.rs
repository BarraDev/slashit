//! `slashit` — the command line front end for a running SlashIt instance.
//!
//! Every subcommand is one request and one response over the control channel.
//! The instance may be the desktop app or the headless daemon; the CLI does not
//! care, and `slashit ping` is how a user finds out which one answered.

mod client;
mod output;

use anyhow::Result;
use clap::{Parser, Subcommand};
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

    let request = match &cli.command {
        Commands::Status => IpcRequest::Status,
        Commands::Projects => IpcRequest::ListProjects,
        Commands::Tasks { project } => IpcRequest::ListTasks {
            project_id: project.clone(),
        },
        Commands::Create {
            project,
            title,
            description,
            priority,
        } => IpcRequest::CreateTask {
            project_id: project.clone(),
            title: title.clone(),
            description: description.clone(),
            priority: priority.clone(),
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
        // Not a plain request/response: "no daemon is running" is an answer
        // rather than a failure, and it has to set the exit status.
        Commands::Daemon { command } => match command {
            DaemonCommands::Status => return daemon_status(&options, cli.json).await,
        },
    };

    let response = slashit_ipc::client::send(&request, &options).await?;

    if !response.ok {
        let msg = response.error.unwrap_or_else(|| "Unknown error".to_string());
        eprintln!("Error: {msg}");
        std::process::exit(1);
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response.data)?);
        return Ok(());
    }

    // Format output based on command type
    match &cli.command {
        Commands::Status => output::print_status(&response.data),
        Commands::Projects => output::print_projects(&response.data),
        Commands::Tasks { .. } => output::print_tasks(&response.data),
        Commands::Queue => output::print_queue(&response.data),
        Commands::Terminals => output::print_terminals(&response.data),
        Commands::Ping => output::print_instance(&response.data),
        Commands::Features => output::print_features(&response.data),
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
            eprintln!("Error: {msg}");
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
