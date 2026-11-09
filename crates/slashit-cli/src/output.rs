// SPDX-License-Identifier: MPL-2.0
use slashit_ipc::{
    AppStatus, FeatureFlagInfo, InstanceInfo, ProjectSummary, QueueStatusInfo, TaskSummary,
    TerminalSummary,
};

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

pub fn print_instance(data: &serde_json::Value) {
    if let Ok(info) = serde_json::from_value::<InstanceInfo>(data.clone()) {
        println!("SlashIt Instance");
        println!("{}", "-".repeat(30));
        println!("  Mode:      {}", info.mode);
        println!("  Version:   {}", info.version);
        println!("  Protocol:  {}", info.protocol_version);
        println!("  PID:       {}", info.pid);
        println!("  Endpoint:  {}", info.endpoint);
    } else {
        print_raw(data);
    }
}

/// The resolved flags, with the layer that decided each one.
///
/// The source column is the point of the command: a flag whose value comes from
/// the environment looks identical to one from the config file until you are
/// told otherwise, and that is exactly the situation people spend an afternoon
/// on.
pub fn print_features(data: &serde_json::Value) {
    if let Ok(flags) = serde_json::from_value::<Vec<FeatureFlagInfo>>(data.clone()) {
        if flags.is_empty() {
            println!("No feature flags.");
            return;
        }
        println!(
            "{:<20} {:<8} {:<10} DESCRIPTION",
            "NAME", "VALUE", "SOURCE"
        );
        println!("{}", "-".repeat(96));
        for f in &flags {
            println!(
                "{:<20} {:<8} {:<10} {}",
                truncate(&f.name, 18),
                if f.value { "on" } else { "off" },
                truncate(&f.source, 8),
                truncate(&f.description, 54),
            );
        }
        println!("\n{} flag(s)", flags.len());
    } else {
        print_raw(data);
    }
}

/// One Task's execution evidence, as `slashit runs` shows it.
///
/// The report is read out of the response's JSON the same way the machine form
/// is, defensively: a field the instance does not send (an older one answering)
/// degrades to a placeholder rather than a panic.
pub fn print_runs(data: &serde_json::Value) {
    println!("{}", run_report_text(data));
}

/// The `runs` report, at a glance.
///
/// Run output is the one thing this read serves that is not persisted, so the
/// report always says where it lives, and never silently omits a bound: what
/// the instance left out is counted, in runs and in milestones.
pub(crate) fn run_report_text(data: &serde_json::Value) -> String {
    let mut out = String::new();

    let title = data["title"].as_str().unwrap_or("(untitled)");
    let provider = data["provider"].as_str().unwrap_or("?");
    let short = short_id(data["task_id"].as_str().unwrap_or("?"));
    out.push_str(&format!("Task {short} \"{title}\" — provider {provider}\n"));

    match data["configured_model"].as_str() {
        Some(model) => out.push_str(&format!("  Model configured: {model} (saved on the Task)\n")),
        None => out.push_str("  Model configured: provider default (saved on the Task)\n"),
    }
    match data["observed_model"].as_str() {
        Some(model) => out.push_str(&format!(
            "  Model observed:   {model} (reported by a run; never inferred from the configured id)\n"
        )),
        None => out.push_str(
            "  Model observed:   none — no run has recorded a model; an alias is a request, never evidence\n",
        ),
    }

    match &data["session"] {
        serde_json::Value::Null => {
            out.push_str("  Session:          this instance supervises no run state\n");
        }
        session => {
            let live = session["live"].as_bool().unwrap_or(false);
            let state = describe_status(&session["status"]);
            let holding = if live { "live run" } else { "no live run" };
            out.push_str(&format!("  Session:          {holding}; state {state}\n"));
        }
    }

    if let Some(review) = data["ai_review"].as_object() {
        let outcome = review["outcome"].as_str().unwrap_or("unknown");
        out.push_str(&format!(
            "  AI review:        {} (review {}, signoff: {})\n",
            outcome,
            review["review"].as_u64().unwrap_or(0),
            data["qa_signoff"].as_str().unwrap_or("none"),
        ));
    } else {
        out.push_str("  AI review:        none on record\n");
    }

    match data["failure_reason"].as_str() {
        Some(reason) => out.push_str(&format!("  Failure:          {reason}\n")),
        None => out.push_str("  Failure:          none\n"),
    }

    let runs = data["runs"].as_array().cloned().unwrap_or_default();
    let described = data["runs_omitted"].as_u64().unwrap_or(0) as usize;
    out.push_str(&format!(
        "  Runs:             {} described, {} older not described\n",
        runs.len(),
        described,
    ));
    for run in &runs {
        let number = run["run"].as_u64().unwrap_or(0);
        let status = run["status"].as_str().unwrap_or("unknown");
        out.push_str(&format!("    run {number} · {status}"));
        if let Some(reason) = run["failure_reason"].as_str() {
            out.push_str(&format!(" · {reason}"));
        }
        out.push('\n');
        if let Some(text) = run["outcome"]["text"].as_str() {
            let bound = run["outcome"]["echo_chars_omitted"].as_u64();
            match bound {
                Some(0) | None => out.push_str(&format!("      result: {text}\n")),
                Some(hidden) => out.push_str(&format!(
                    "      result ({hidden} further chars in the record): {text}…\n"
                )),
            }
        } else if let Some(reason) = run["outcome"]["reason"].as_str() {
            out.push_str(&format!("      result unavailable: {reason}\n"));
        }
    }

    let output = &data["output"];
    let entries = output["entries"].as_array().cloned().unwrap_or_default();
    out.push_str(&format!(
        "  Run output:       kept only in this instance's memory, not persisted; {} entry line(s) echoed\n",
        entries.len(),
    ));
    if let Some(note) = output["note"].as_str() {
        // The note is the honest sentence for what is here — or for why there
        // is nothing here — so a script (or a person) is told the hold, not
        // just the absence.
        out.push_str(&format!("                    {note}\n"));
    }
    for entry in &entries {
        let level = entry["level"].as_str().unwrap_or("info");
        let message = entry["message"].as_str().unwrap_or("");
        out.push_str(&format!("    [{level}] {message}\n"));
    }

    let activity = data["activity"].as_array().cloned().unwrap_or_default();
    let omitted = data["activity_omitted"].as_u64().unwrap_or(0) as usize;
    out.push_str(&format!(
        "  Activity:         {} milestone(s) echoed, {} older not echoed\n",
        activity.len(),
        omitted,
    ));

    out
}

/// A run state, for one line: `starting`, `running`, `stopping`, `stopped`,
/// `failed (reason)` or `none` when this instance never saw the task run.
/// The failed reason arrives already redacted and bounded by the instance,
/// in line with everything else the answer echoes.
fn describe_status(status: &serde_json::Value) -> String {
    match status {
        serde_json::Value::Null => "none (nothing has run in this instance)".to_string(),
        serde_json::Value::String(state) => state.clone(),
        other => {
            let reason = other["failed"].as_str().unwrap_or("?");
            format!("failed ({reason})")
        }
    }
}

fn print_raw(data: &serde_json::Value) {
    if let Ok(s) = serde_json::to_string_pretty(data) {
        println!("{}", s);
    }
}

/// Shorten a value to fit a fixed-width column.
///
/// Counted in characters rather than bytes. A task title carrying an accent or
/// an em dash would otherwise be sliced through the middle of a UTF-8 sequence
/// and panic, which is a poor way for a status command to fail.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn short_id(id: &str) -> String {
    if id.len() > 8 {
        id[..8].to_string()
    } else {
        id.to_string()
    }
}

#[cfg(test)]
mod runs_report_tests {
    use super::*;
    use serde_json::json;

    /// The shaped answer, as the build it targets serializes it.
    fn sample() -> serde_json::Value {
        json!({
            "task_id": "39bd237c-abcd-abcd-abcd-abcdabcdabcd",
            "title": "Fix the login flow",
            "provider": "OpenCode",
            "configured_model": "ollama/llama3.2:3b",
            "observed_model": "ollama/llama3.2:3b",
            "session": {
                "live": false,
                "status": "stopped",
                "execution_id": "0b84d3d3-8000-8000-8000-000000000001",
                "started_at": "2026-11-09T10:00:00Z",
                "stopped_at": "2026-11-09T10:05:00Z",
            },
            "output": {
                "available": true,
                "memory_only": true,
                "note": "The entries echoed here are held only in this instance's memory. Run output is not persisted: a restart loses all of it.",
                "entries_omitted": 0,
                "truncated": false,
                "entries": [
                    {"timestamp": "2026-11-09T10:05:00Z", "level": "info", "message": "ran the fix", "message_truncated": false}
                ],
            },
            "runs": [
                {"run": 2, "status": "completed", "started_at": "2026-11-09T09:00:00Z", "ended_at": "2026-11-09T09:10:00Z",
                 "failure_reason": null,
                 "outcome": {"kind": "available", "text": "the work was committed", "original_chars": null, "echo_chars_omitted": null, "reason": null}},
                {"run": 1, "status": "failed", "started_at": "2026-11-09T08:00:00Z", "ended_at": "2026-11-09T08:01:00Z",
                 "failure_reason": "provider unreachable",
                 "outcome": {"kind": "truncated", "text": "x".repeat(60), "original_chars": 4_000, "echo_chars_omitted": 500, "reason": null}},
            ],
            "runs_omitted": 0,
            "ai_review": {"review": 2, "outcome": "approved", "issues": null, "reason": null},
            "qa_signoff": "approved",
            "failure_reason": null,
            "failure_reason_truncated": false,
            "activity": [
                {"seq": 2, "at": "2026-11-09T09:00:00Z", "kind": {"type": "run_started", "run": 2, "addressing_feedback": false}}
            ],
            "activity_omitted": 0,
        })
    }

    #[test]
    fn the_report_names_the_identity_models_and_runs() {
        let text = run_report_text(&sample());
        assert!(text.contains("Fix the login flow"), "{text}");
        assert!(text.contains("provider OpenCode"), "{text}");
        assert!(text.contains("ollama/llama3.2:3b"), "{text}");
        assert!(text.contains("run 2 · completed"), "{text}");
        assert!(text.contains("run 1 · failed · provider unreachable"), "{text}");
        assert!(text.contains("result unavailable") || text.contains("500 further chars"), "{text}");
        assert!(text.contains("review 2"), "{text}");
        assert!(text.contains("signoff: approved"), "{text}");
    }

    #[test]
    fn the_report_says_where_run_output_lives() {
        let text = run_report_text(&sample());
        assert!(
            text.contains("held only in this instance's memory"),
            "the memory-only sentence is the point of the flag: {text}"
        );
        assert!(text.contains("not persisted"), "{text}");
        assert!(text.contains("[info] ran the fix"), "{text}");
    }

    #[test]
    fn without_a_session_hold_the_report_says_so() {
        let mut value = sample();
        value["output"] = json!({
            "available": false,
            "memory_only": true,
            "note": "This instance holds no run output for this Task. Run output lives only in an instance's memory and is not persisted, so what a previous instance held was lost when that instance ended. The Task's own record survives: the runs, their outcomes and its Activity milestones.",
            "entries_omitted": 0,
            "truncated": false,
            "entries": [],
        });
        let text = run_report_text(&value);
        assert!(text.contains("lost when that instance ended"), "{text}");
    }

    #[test]
    fn a_configured_alias_is_never_passed_off_as_an_observed_model() {
        let mut value = sample();
        value["observed_model"] = json!(null);
        let text = run_report_text(&value);
        assert!(
            text.contains("Model observed:   none"),
            "a reported model must not be invented from the configuration: {text}"
        );
    }

    #[test]
    fn shapes_the_instance_never_sends_degrade_defensively() {
        let text = run_report_text(&serde_json::json!({}));
        // No panic, and every line the report promises is honest about the gap.
        assert!(text.contains("(untitled)"), "{text}");
        assert!(text.contains("Runs:"), "{text}");
        assert!(text.contains("none"), "{text}");
    }
}
