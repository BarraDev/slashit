use slashit_ipc::{AppStatus, ProjectSummary, QueueStatusInfo, TaskSummary, TerminalSummary};

pub fn print_status(data: &serde_json::Value) {
    if let Ok(status) = serde_json::from_value::<AppStatus>(data.clone()) {
        println!("SlashIt Status");
        println!("{}", "-".repeat(30));
        println!("  Terminals:    {}", status.active_terminals);
        println!("  Agents:       {}", status.running_agents);
        println!("  Queued:       {}", status.queued_tasks);
        println!("  In Progress:  {}", status.in_progress_tasks);
    } else {
        print_raw(data);
    }
}

pub fn print_projects(data: &serde_json::Value) {
    if let Ok(projects) = serde_json::from_value::<Vec<ProjectSummary>>(data.clone()) {
        if projects.is_empty() {
            println!("No projects found.");
            return;
        }
        println!("{:<38} {:<30} PATH", "ID", "NAME");
        println!("{}", "-".repeat(80));
        for p in &projects {
            println!(
                "{:<38} {:<30} {}",
                p.id,
                truncate(&p.name, 28),
                p.path.as_deref().unwrap_or("-")
            );
        }
        println!("\n{} project(s)", projects.len());
    } else {
        print_raw(data);
    }
}

pub fn print_tasks(data: &serde_json::Value) {
    if let Ok(tasks) = serde_json::from_value::<Vec<TaskSummary>>(data.clone()) {
        if tasks.is_empty() {
            println!("No tasks found.");
            return;
        }
        println!(
            "{:<10} {:<18} {:<14} {:<10} {:<8} {:<40}",
            "ID", "PROJECT", "STATUS", "PRIORITY", "PROG", "TITLE"
        );
        println!("{}", "-".repeat(104));
        for t in &tasks {
            println!(
                "{:<10} {:<18} {:<14} {:<10} {:>4}%   {}",
                short_id(&t.id),
                truncate(&t.project_name, 16),
                t.status,
                t.priority,
                t.overall_progress,
                truncate(&t.title, 38),
            );
        }
        println!("\n{} task(s)", tasks.len());
    } else {
        print_raw(data);
    }
}

pub fn print_queue(data: &serde_json::Value) {
    if let Ok(info) = serde_json::from_value::<QueueStatusInfo>(data.clone()) {
        println!("Queue Status");
        println!("{}", "-".repeat(30));
        println!("  Queued:         {}", info.queued_count);
        println!("  In Progress:    {}", info.in_progress_count);
        println!("  Parallel Limit: {}", info.parallel_limit);
        println!(
            "  Auto Promote:   {}",
            if info.auto_promote { "yes" } else { "no" }
        );
        println!(
            "  FIFO Ordering:  {}",
            if info.fifo_ordering { "yes" } else { "no" }
        );
    } else {
        print_raw(data);
    }
}

pub fn print_terminals(data: &serde_json::Value) {
    if let Ok(terminals) = serde_json::from_value::<Vec<TerminalSummary>>(data.clone()) {
        if terminals.is_empty() {
            println!("No active terminal sessions.");
            return;
        }
        println!("{:<38} {:<30} SIZE", "ID", "NAME");
        println!("{}", "-".repeat(76));
        for t in &terminals {
            println!(
                "{:<38} {:<30} {}x{}",
                t.id,
                truncate(&t.name, 28),
                t.cols,
                t.rows
            );
        }
        println!("\n{} session(s)", terminals.len());
    } else {
        print_raw(data);
    }
}

fn print_raw(data: &serde_json::Value) {
    if let Ok(s) = serde_json::to_string_pretty(data) {
        println!("{}", s);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

fn short_id(id: &str) -> String {
    if id.len() > 8 {
        id[..8].to_string()
    } else {
        id.to_string()
    }
}
