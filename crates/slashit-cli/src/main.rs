mod client;
mod output;

use anyhow::Result;
use clap::{Parser, Subcommand};
use slashit_ipc::IpcRequest;
use std::time::Duration;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

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
    };

    let response = client::send(&request, cli.wait, Duration::from_secs(cli.timeout)).await?;

    if !response.ok {
        let msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        eprintln!("Error: {}", msg);
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
        _ => {
            // For mutation commands, print a simple success message
            if let Some(msg) = response.data.as_str() {
                println!("{}", msg);
            } else if response.data.is_null() {
                println!("Done.");
            } else {
                println!("{}", serde_json::to_string_pretty(&response.data)?);
            }
        }
    }

    Ok(())
}
