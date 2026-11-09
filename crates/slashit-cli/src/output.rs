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

/// One free-text value made safe to print into a terminal.
///
/// Everything this report echoes that the Task, the provider or a run shaped —
/// title, model identifiers, failure reasons, results, output lines — can
/// carry terminal control sequences: ANSI CSI, OSC (including OSC 52, which
/// can read the clipboard), ESC and BEL, the 8-bit C1 controls (a bare U+009B
/// is a CSI in itself), DEL and carriage returns that redraw the line. Every
/// such character is replaced by a visible `\u{xx}` escape, so no untrusted
/// sequence can reach the terminal as itself; a backslash is doubled, so an
/// escape is never ambiguous.
///
/// The same is true for the invisible Unicode that cannot move a cursor but
/// can still mislead a person reading the report: the bidirectional controls
/// U+202A–U+202E and the isolates U+2066–U+2069, which reorder everything
/// after them (right-to-left override is one of these); the directional marks
/// U+200E/U+200F, which nudge the direction of neutral characters between
/// runs; and the line and paragraph separators U+2028/U+2029, which break up
/// the report's own lines on a terminal that honors them. Each becomes a
/// visible `\u{xx}` escape too. Ordinary Unicode — accented characters, and
/// ZWJ and ZWNJ, which deliberately shape ordinary text — passes through
/// untouched. Only an untrusted value's own characters are touched; the
/// report's labels, the line breaks it lays out itself and the JSON the
/// instance sends are not affected by this function.
fn escape_terminal(text: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ if (ch as u32) < 0x20 || (0x7F..=0x9F).contains(&(ch as u32)) => {
                let _ = write!(out, "\\u{{{:02x}}}", ch as u32);
            }
            // The bidirectional embeds and overrides (U+202A–U+202E), the
            // isolates (U+2066–U+2069), the directional marks (U+200E and
            // U+200F) and the line/paragraph separators (U+2028 and U+2029).
            // U+200C and U+200D, the zero-width non-joiner and joiner, are
            // not among them: they are ordinary text shaping.
            '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => {
                let _ = write!(out, "\\u{{{:04x}}}", ch as u32);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// One Task's execution evidence, as `slashit runs` shows it.
///
/// The report is read out of the response's JSON the same way the machine form
/// is, defensively: a field the instance does not send (an older one answering)
/// degrades to a placeholder rather than a panic. The full handler-to-CLI
/// contract — the real typed answer, rendered — is exercised where the handler
/// lives, in the backend's own tests; the tests here pin the degraded shapes
/// and the per-field lines a shaped answer produces.
pub fn print_runs(data: &serde_json::Value) {
    println!("{}", run_report_text(data));
}

/// The `runs` report, at a glance.
///
/// Run output is the one thing this read serves that is not persisted, so the
/// report always says where it lives, and never silently omits a bound: what
/// the instance left out is counted, in runs and in milestones.
///
/// Public so the backend's own tests can run the real report against answers
/// its handler really produced — the handler-to-CLI contract, not a
/// reconstruction of one.
pub fn run_report_text(data: &serde_json::Value) -> String {
    let mut out = String::new();

    let title = escape_terminal(data["title"].as_str().unwrap_or("(untitled)"));
    let provider = escape_terminal(data["provider"].as_str().unwrap_or("?"));
    let short = short_id(data["task_id"].as_str().unwrap_or("?"));
    out.push_str(&format!("Task {short} \"{title}\" — provider {provider}\n"));

    match data["configured_model"].as_str() {
        Some(model) => out.push_str(&format!(
            "  Model configured: {} (saved on the Task)\n",
            escape_terminal(model)
        )),
        None => out.push_str("  Model configured: provider default (saved on the Task)\n"),
    }
    match data["latest_observed_model"].as_str() {
        Some(model) => out.push_str(&format!(
            "  Model observed:   {} (the Task's latest observed; no per-run identity is kept, and no alias is read back as evidence)\n",
            escape_terminal(model)
        )),
        None => out.push_str(
            "  Model observed:   none — no run has ever reported a model; an alias is a request, never evidence\n",
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
            escape_terminal(outcome),
            review["review"].as_u64().unwrap_or(0),
            escape_terminal(data["qa_signoff"].as_str().unwrap_or("none")),
        ));
        // A review a newer run began after is still the latest one on record,
        // so it is shown — with what makes it honest: it predates that run,
        // and it is never passed off as evidence about it. A missing field
        // (an older instance answering) means nothing was superseded.
        if let Some(run) = review.get("superseded_by_run").and_then(|v| v.as_u64()) {
            out.push_str(&format!(
                "                    superseded by run {run}: recorded before run {run} began, not evidence about run {run}\n"
            ));
        }
        // A review or fix pass that recorded a reason — failed, skipped, a
        // fix that could not deliver its changes — keeps it bounded and
        // redacted by the instance; the report shows it rather than a bare
        // outcome word.
        if let Some(reason) = review["reason"].as_str().filter(|r| !r.is_empty()) {
            out.push_str(&format!(
                "                    reason: {}\n",
                escape_terminal(reason)
            ));
        }
    } else {
        out.push_str("  AI review:        none on record\n");
    }

    match data["failure_reason"].as_str() {
        Some(reason) => out.push_str(&format!(
            "  Failure:          {}\n",
            escape_terminal(reason)
        )),
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
        let status = escape_terminal(run["status"].as_str().unwrap_or("unknown"));
        out.push_str(&format!("    run {number} · {status}"));
        if let Some(reason) = run["failure_reason"].as_str() {
            out.push_str(&format!(" · {}", escape_terminal(reason)));
        }
        out.push('\n');
        if let Some(text) = run["outcome"]["text"].as_str() {
            let text = escape_terminal(text);
            let bound = run["outcome"]["echo_chars_omitted"].as_u64();
            match bound {
                Some(0) | None => out.push_str(&format!("      result: {text}\n")),
                // `echo_chars_omitted` counts the characters of the recorded
                // text that this echo left out when it cut further — not the
                // full length the record itself kept, and not a count of
                // anything the record holds beyond it.
                Some(hidden) => out.push_str(&format!(
                    "      result (this echo left out {hidden} more of the recorded text's characters): {text}…\n"
                )),
            }
        } else if let Some(reason) = run["outcome"]["reason"].as_str() {
            out.push_str(&format!(
                "      result unavailable: {}\n",
                escape_terminal(reason)
            ));
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
        // just the absence. It is this instance's own wording, escaped like
        // everything else the report echoes.
        out.push_str(&format!("                    {}\n", escape_terminal(note)));
    }
    for entry in &entries {
        let level = escape_terminal(entry["level"].as_str().unwrap_or("info"));
        let message = escape_terminal(entry["message"].as_str().unwrap_or(""));
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
/// in line with everything else the answer echoes, and is escaped here like
/// every other echoed free text.
fn describe_status(status: &serde_json::Value) -> String {
    match status {
        serde_json::Value::Null => "none (nothing has run in this instance)".to_string(),
        serde_json::Value::String(state) => escape_terminal(state),
        other => {
            let reason = other["failed"].as_str().unwrap_or("?");
            format!("failed ({})", escape_terminal(reason))
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

    /// The shaped answer, as the handler serializes it: same keys, same
    /// nesting. The keys are pinned field names of the backend's
    /// `GetTaskEvidence` answer — the handler-to-CLI contract is exercised
    /// end to end in the backend's own tests against the real handler; this
    /// sample only has to agree with it, and the backend contract test fails
    /// if it does not.
    fn sample() -> serde_json::Value {
        json!({
            "task_id": "39bd237c-abcd-abcd-abcd-abcdabcdabcd",
            "title": "Fix the login flow",
            "provider": "OpenCode",
            "configured_model": "ollama/llama3.2:3b",
            "latest_observed_model": "ollama/llama3.2:3b",
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
                {"run": 3, "status": "completed", "started_at": "2026-11-09T10:00:00Z", "ended_at": "2026-11-09T10:10:00Z",
                 "failure_reason": null,
                 "outcome": {"kind": "available", "text": "the work was committed", "original_chars": null, "echo_chars_omitted": null, "reason": null}},
                {"run": 2, "status": "failed", "started_at": "2026-11-09T09:00:00Z", "ended_at": "2026-11-09T09:10:00Z",
                 "failure_reason": null,
                 "outcome": {"kind": "truncated", "text": "x".repeat(60), "original_chars": 4_000, "echo_chars_omitted": 500, "reason": null}},
                {"run": 1, "status": "failed", "started_at": "2026-11-09T08:00:00Z", "ended_at": "2026-11-09T08:01:00Z",
                 "failure_reason": "provider unreachable",
                 "outcome": {"kind": "unavailable", "text": null, "original_chars": null, "echo_chars_omitted": null, "reason": "the provider said nothing"}},
            ],
            "runs_omitted": 0,
            "ai_review": {"review": 2, "outcome": "approved", "issues": null, "reason": null, "superseded_by_run": null},
            "qa_signoff": "approved",
            "failure_reason": null,
            "failure_reason_truncated": false,
            "activity": [
                {"seq": 2, "at": "2026-11-09T10:10:00Z", "kind": {"type": "run_result", "run": 3, "result": {"outcome": "available", "text": "the work was committed"}}}
            ],
            "activity_omitted": 0,
        })
    }

    #[test]
    fn the_report_names_the_identity_models_and_runs() {
        let text = run_report_text(&sample());
        assert!(text.contains("Fix the login flow"), "{text}");
        assert!(text.contains("provider OpenCode"), "{text}");
        // The configured model, saved on the Task, and the latest observed
        // one, recorded by a run — with the Task-level identity spelled out.
        assert!(
            text.contains("Model configured: ollama/llama3.2:3b"),
            "{text}"
        );
        assert!(
            text.contains(
                "Model observed:   ollama/llama3.2:3b (the Task's latest observed; no per-run identity is kept, and no alias is read back as evidence)"
            ),
            "{text}"
        );
        assert!(text.contains("run 3 · completed"), "{text}");
        assert!(
            text.contains("result: the work was committed\n"),
            "an available outcome is echoed whole: {text}"
        );
        assert!(
            text.contains(
                "result (this echo left out 500 more of the recorded text's characters): xxxxxx"
            ),
            "a truncated outcome names exactly what the count is: this echo's own \
             cut of the recorded text, and the echoed text starts at the cut: {text}"
        );
        assert!(
            text.contains("result unavailable: the provider said nothing"),
            "an unavailable outcome is named with the record's reason: {text}"
        );
        assert!(
            text.contains("run 1 · failed · provider unreachable"),
            "{text}"
        );
        assert!(text.contains("run 2 · failed"), "{text}");
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
    fn a_failed_session_state_is_shown_as_failed_with_its_reason() {
        let mut value = sample();
        value["session"] = json!({
            "live": false,
            "status": {"failed": "the provider replied 401"},
            "execution_id": null,
            "started_at": "2026-11-09T10:00:00Z",
            "stopped_at": "2026-11-09T10:01:00Z",
        });
        let text = run_report_text(&value);
        assert!(
            text.contains(
                "Session:          no live run; state failed (the provider replied 401)\n"
            ),
            "a failed session names its reason, not just a word: {text}"
        );
    }

    #[test]
    fn an_absent_session_is_shown_as_supervised_by_nothing() {
        let mut value = sample();
        value["session"] = json!(null);
        let text = run_report_text(&value);
        assert!(
            text.contains("Session:          this instance supervises no run state\n"),
            "an absent session is the restart shape, and says so: {text}"
        );
    }

    #[test]
    fn a_failed_fix_pass_is_reported_as_such() {
        let mut value = sample();
        value["ai_review"] = json!({
            "review": 2,
            "outcome": "fix_failed",
            "issues": null,
            "reason": "the fix agent could not write the checkout, ***",
        });
        let text = run_report_text(&value);
        assert!(
            text.contains("AI review:        fix_failed (review 2, signoff: approved)\n"),
            "a fix failure is its own outcome line, not a changes request: \n{text}"
        );
        assert!(
            text.contains("fix agent could not write the checkout, ***"),
            "the fix's bounded reason travels with it: {text}"
        );
    }

    #[test]
    fn a_configured_alias_is_never_passed_off_as_an_observed_model() {
        let mut value = sample();
        value["latest_observed_model"] = json!(null);
        let text = run_report_text(&value);
        assert!(
            text.contains(
                "Model observed:   none — no run has ever reported a model; an alias is a request, never evidence\n"
            ),
            "a reported model must not be invented from the configuration: {text}"
        );
    }

    /// A review the record still keeps is not erased by a run that started
    /// later: it is shown, with the run that followed it named — and so the
    /// word "superseded" names the review's age, never a claim about that run.
    #[test]
    fn a_superseded_review_is_shown_with_the_run_that_followed_it() {
        let mut value = sample();
        value["ai_review"] = json!({
            "review": 2,
            "outcome": "approved",
            "issues": null,
            "reason": null,
            "superseded_by_run": 3,
        });
        let text = run_report_text(&value);
        assert!(
            text.contains("AI review:        approved (review 2, signoff: approved)\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "superseded by run 3: recorded before run 3 began, not evidence about run 3\n"
            ),
            "{text}"
        );
    }

    /// An older instance that does not send the supersession field, or a
    /// review nothing followed, must not cause the report to say "superseded"
    /// of anything.
    #[test]
    fn an_unsuperseded_review_says_nothing_about_supersession() {
        let mut value = sample();
        value["ai_review"] = json!({
            "review": 2,
            "outcome": "approved",
            "issues": null,
            "reason": null,
            "superseded_by_run": null,
        });
        let text = run_report_text(&value);
        assert!(!text.contains("superseded"), "{text}");
    }

    /// Nothing a Task title, a provider error, a run's result or an output
    /// line carries may reach the terminal as itself. ANSI CSI, an OSC 52
    /// clipboard write, a bare ESC, BEL, the 8-bit C1 CSI (U+009B alone) and
    /// carriage returns become visible `\u{xx}` escapes in every free-text
    /// field, so an untrusted sequence cannot move the cursor, recolor the
    /// report or read a clipboard.
    #[test]
    fn control_sequences_cannot_reach_the_report_as_themselves() {
        let csi = "\x1b[31mred\x1b[0m";
        let osc52 = "\x1b]52;c;aGVsbG8=\x07";
        let bel = "bel\u{7}led";
        let c1_csi = "\u{9b}31m";
        let cr = "over\rwritten";
        let mut value = sample();
        value["title"] = json!(format!("title {csi} {osc52}"));
        value["ai_review"]["reason"] = json!(format!("why {csi}"));
        value["failure_reason"] = json!(format!("failed {osc52} {bel}"));
        value["runs"][0]["outcome"]["text"] = json!(format!("did {c1_csi} work"));
        value["runs"][2]["failure_reason"] = json!(format!("out {cr} of reach"));
        value["output"]["entries"][0]["message"] = json!(format!("note {osc52}"));
        value["session"] = json!({
            "live": false,
            "status": {"failed": format!("stopped {csi}")},
            "execution_id": null,
            "started_at": null,
            "stopped_at": null,
        });
        value["configured_model"] = json!(format!("model {csi}"));
        value["latest_observed_model"] = json!(format!("observed {osc52}"));

        let text = run_report_text(&value);

        // No raw control character reaches the rendered report, whatever
        // field carried it.
        for raw in ["\x1b", "\u{9b}", "\u{7}", "\r"] {
            assert!(
                !text.contains(raw),
                "a raw control sequence reached the rendered report: {text:?}"
            );
        }
        // ... and each escaped shape is visible in the field that carried it,
        // so a person reading the report sees what the value holds.
        assert!(text.contains(r"\u{1b}[31mred\u{1b}[0m"), "{text:?}");
        assert!(text.contains(r"\u{1b}]52;c;aGVsbG8=\u{07}"), "{text:?}");
        assert!(text.contains(r"\u{9b}31m"), "{text:?}");
        assert!(text.contains(r"over\rwritten"), "{text:?}");
        assert!(text.contains(r"bel\u{07}led"), "{text:?}");
        // The readable words around the escapes survive.
        assert!(text.contains("did \\u{9b}31m work"), "{text:?}");
        assert!(text.contains("Model configured: model "), "{text:?}");
        assert!(text.contains("Model observed:   observed "), "{text:?}");
        assert!(text.contains("state failed (stopped "), "{text:?}");
    }

    /// Nothing a Task title, a provider error, a run's result or an output
    /// line carries may reach the report as itself, in a shape that changes
    /// how the report is read. The bidirectional controls U+202A–U+202E and
    /// the isolates U+2066–U+2069 reorder or isolate the characters printed
    /// after them — an RLO in a title can visually rewrite the labels and
    /// numbers of every line below — the directional marks U+200E/U+200F
    /// nudge neutral characters between runs of text, and the Unicode line
    /// and paragraph separators U+2028/U+2029 can break the report's own
    /// lines apart on a terminal that honors them. Every one becomes a
    /// visible `\u{xx}` escape, in every free-text field.
    #[test]
    fn direction_controls_cannot_reorder_or_rewrite_the_report() {
        // An RLO right after "Fix" would make what follows render mirrored;
        // the PDF ends it. An LRI with its PDI would visually hide what it
        // encloses. A mark between two words pulls the direction around.
        let mut value = sample();
        value["title"] = json!("Fix \u{202e}mirrored\u{202c}");
        value["ai_review"]["reason"] = json!("failed \u{200f}quietly");
        value["runs"][0]["outcome"]["text"] = json!("did \u{2066}isolated\u{2069} work");
        value["runs"][1]["outcome"]["text"] = json!("broken\u{2028}lines\u{2029}here");
        value["output"]["entries"][0]["message"] = json!("note \u{202a}left\u{202b}right");
        value["configured_model"] = json!("model \u{2068}popped\u{2067}");
        value["failure_reason"] = json!("gone \u{200e}missing");

        let text = run_report_text(&value);

        // No raw directional control, mark or separator reaches the rendered
        // report, in any field it was planted in.
        for raw in [
            '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}', '\u{202a}', '\u{202b}', '\u{202c}',
            '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            let raw: String = raw.into();
            assert!(
                !text.contains(raw.as_str()),
                "a raw direction control {raw:?} reached the report: {text:?}"
            );
        }
        // ... and each is visible as its own escape in the field that carried
        // it, so a person reading the report sees the report was shaped by an
        // untrusted value, not reordered by it.
        assert!(text.contains(r"Fix \u{202e}mirrored\u{202c}"), "{text:?}");
        assert!(text.contains(r"failed \u{200f}quietly"), "{text:?}");
        assert!(
            text.contains(r"did \u{2066}isolated\u{2069} work"),
            "{text:?}"
        );
        assert!(
            text.contains(r"broken\u{2028}lines\u{2029}here"),
            "{text:?}"
        );
        assert!(text.contains(r"note \u{202a}left\u{202b}right"), "{text:?}");
        assert!(text.contains(r"model \u{2068}popped\u{2067}"), "{text:?}");
        assert!(text.contains(r"gone \u{200e}missing"), "{text:?}");
    }

    /// The escaping exists for hostile shapes; ordinary characters, an accent
    /// included, are meant to survive whole. Zero-width joiners and
    /// non-joiners are ordinary shaping of text, not direction controls, so
    /// they survive too.
    #[test]
    fn ordinary_unicode_is_not_escaped() {
        let mut value = sample();
        value["title"] = json!("Fix the café éditeur — façade");
        value["runs"][0]["outcome"]["text"] = json!("café prêt 👍\u{200d}set");
        let text = run_report_text(&value);
        assert!(text.contains("Fix the café éditeur — façade"), "{text}");
        assert!(text.contains("café prêt 👍\u{200d}set"), "{text}");
        assert!(!text.contains(r"\u{"), "{text}");
    }

    #[test]
    fn shapes_the_instance_never_sends_degrade_defensively() {
        let text = run_report_text(&serde_json::json!({}));
        // No panic, and every degraded line is a specific placeholder, not a
        // vague one: the report is honest about exactly what was not sent.
        assert!(
            text.contains("Task ? \"(untitled)\" — provider ?\n"),
            "{text}"
        );
        assert!(
            text.contains("Model configured: provider default (saved on the Task)\n"),
            "{text}"
        );
        assert!(
            text.contains("Model observed:   none — no run has ever reported a model; an alias is a request, never evidence\n"),
            "{text}"
        );
        assert!(
            text.contains("Session:          this instance supervises no run state\n"),
            "{text}"
        );
        assert!(
            text.contains("AI review:        none on record\n"),
            "{text}"
        );
        assert!(text.contains("Failure:          none\n"), "{text}");
        assert!(
            text.contains("Runs:             0 described, 0 older not described\n"),
            "{text}"
        );
        assert!(
            text.contains("Activity:         0 milestone(s) echoed, 0 older not echoed\n"),
            "{text}"
        );
    }
}
