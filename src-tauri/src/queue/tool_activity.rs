//! The tool calls a coding run's timeline keeps, and what each one says.
//!
//! A tool call reaches the timeline as its tool's name and one short detail:
//! a command's first line, a path relative to the task's checkout, a search
//! pattern. Only tools that say something about the work are kept; the
//! agent's own bookkeeping (plans, tool discovery, MCP servers and plugins)
//! is not.
//!
//! Nothing here is ever the tool's full input. A command keeps its first
//! line, a path outside the checkout keeps only its file name, and every
//! detail has anything that looks like a credential masked and URLs cut to
//! their path, so neither a secret nor the layout of the user's machine is
//! written into the task file.
//!
//! Calls are buffered per run in memory and reach the task only in the write
//! that ends the run (see [`crate::queue::TaskExecutor`]): the board never
//! shows a timeline row the task file does not have.

use chrono::{DateTime, Utc};
use slashit_activity::Entry;

use crate::domain::Task;

/// The tool calls one run has made so far, compacted and capped exactly as
/// they will be on the task.
#[derive(Debug, Default, Clone)]
pub struct RunTools {
    run: u32,
    entries: Vec<Entry>,
}

impl RunTools {
    pub fn new(run: u32) -> Self {
        Self { run, entries: Vec::new() }
    }

    pub fn run(&self) -> u32 {
        self.run
    }

    /// Record one tool call, if it is one the timeline keeps.
    pub fn push(&mut self, at: DateTime<Utc>, tool: &str, input: &serde_json::Value, working_dir: &str) {
        if let Some((name, detail)) = describe(tool, input, working_dir) {
            slashit_activity::record_tool(&mut self.entries, at, self.run, name, detail.as_deref());
        }
    }

    /// Append the calls to `task`'s timeline. Takes `&self` so a write that
    /// stages them and then fails leaves the buffer to the caller.
    pub fn apply_to(&self, task: &mut Task) {
        slashit_activity::append(&mut task.activity, self.entries.clone());
    }
}

/// The name and detail a tool call is shown with, or `None` for a tool the
/// timeline does not keep.
///
/// Every detail is free text the agent chose, so every one takes the same
/// path: reduced to the one field worth showing, with the checkout and home
/// directory written as `.` and `~`, then [`slashit_activity::sanitize`]d.
pub fn describe(tool: &str, input: &serde_json::Value, working_dir: &str) -> Option<(&'static str, Option<String>)> {
    let field = |key: &str| input.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let (name, detail) = match tool {
        "Bash" => ("Bash", field("command").map(first_line)),
        "Read" => ("Read", field("file_path").map(|p| display_path(p, working_dir))),
        "Edit" => ("Edit", field("file_path").map(|p| display_path(p, working_dir))),
        "MultiEdit" => ("Edit", field("file_path").map(|p| display_path(p, working_dir))),
        "Write" => ("Write", field("file_path").map(|p| display_path(p, working_dir))),
        "NotebookEdit" => ("Edit", field("notebook_path").map(|p| display_path(p, working_dir))),
        "Glob" => ("Glob", field("pattern").map(str::to_string)),
        "Grep" => ("Grep", field("pattern").map(str::to_string)),
        "WebFetch" => ("WebFetch", field("url").map(str::to_string)),
        "WebSearch" => ("WebSearch", field("query").map(str::to_string)),
        "Task" | "Agent" => ("Subagent", field("description").map(str::to_string)),
        _ => return None,
    };
    let detail = detail.map(|d| {
        slashit_activity::sanitize(&without_private_paths(&d, working_dir), slashit_activity::MAX_DETAIL_CHARS)
    });
    Some((name, detail.filter(|d| !d.is_empty())))
}

/// The first non-blank line of a shell command.
fn first_line(command: &str) -> String {
    command.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_string()
}

/// `text` with the task's checkout written as `.` and the home directory as
/// `~`, so a command that names absolute paths (`cd /home/me/... && ...`)
/// does not record the layout of the user's machine.
fn without_private_paths(text: &str, working_dir: &str) -> String {
    let root = working_dir.trim_end_matches('/');
    let text = if root.is_empty() { text.to_string() } else { text.replace(root, ".") };
    crate::domain::task::without_home_dir(&text)
}

/// A path as the timeline shows it: relative to the task's checkout when it
/// is inside it, otherwise only its file name.
fn display_path(path: &str, working_dir: &str) -> String {
    let root = working_dir.trim_end_matches('/');
    if let Some(inside) = path.strip_prefix(root).and_then(|rest| rest.strip_prefix('/')) {
        return inside.to_string();
    }
    if !path.starts_with('/') && !path.starts_with('~') && !path.contains(':') {
        return path.to_string();
    }
    path.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WD: &str = "/home/someone/.local/share/slashit/worktrees/task-1";

    fn detail(tool: &str, input: serde_json::Value) -> Option<(&'static str, Option<String>)> {
        describe(tool, &input, WD)
    }

    #[test]
    fn commands_keep_their_first_line_only() {
        assert_eq!(
            detail("Bash", json!({"command": "\n  cargo test -p slashit-ui\ncat secret.txt", "timeout": 5})),
            Some(("Bash", Some("cargo test -p slashit-ui".to_string())))
        );
        let long = "echo ".to_string() + &"x".repeat(500);
        let (_, d) = detail("Bash", json!({ "command": long })).unwrap();
        assert_eq!(d.unwrap().chars().count(), slashit_activity::MAX_DETAIL_CHARS);
    }

    #[test]
    fn paths_are_relative_to_the_checkout_or_just_a_file_name() {
        assert_eq!(
            detail("Read", json!({"file_path": format!("{WD}/src-tauri/src/queue/executor.rs")})),
            Some(("Read", Some("src-tauri/src/queue/executor.rs".to_string())))
        );
        assert_eq!(
            detail("Edit", json!({"file_path": "/home/someone/.ssh/config", "old_string": "a", "new_string": "b"})),
            Some(("Edit", Some("config".to_string())))
        );
        assert_eq!(
            detail("Write", json!({"file_path": "src/components/task_drawer.rs", "content": "all of it"})),
            Some(("Write", Some("src/components/task_drawer.rs".to_string())))
        );
    }

    #[test]
    fn bookkeeping_tools_are_not_kept() {
        for tool in ["TodoWrite", "ToolSearch", "ListMcpResourcesTool", "mcp__github__create_issue", "Skill"] {
            assert_eq!(detail(tool, json!({"anything": "x"})), None, "{tool}");
        }
    }

    #[test]
    fn a_tool_without_the_expected_field_is_kept_without_detail() {
        assert_eq!(detail("Bash", json!({})), Some(("Bash", None)));
        assert_eq!(detail("Grep", json!({"pattern": "   "})), Some(("Grep", None)));
    }

    #[test]
    fn credentials_in_commands_are_masked() {
        let cases = [
            ("GITHUB_TOKEN=ghp_abcdefghijklmnop gh pr list", "GITHUB_TOKEN=*** gh pr list"),
            ("export API_KEY=hunter2", "export API_KEY=***"),
            (
                "curl -H \"Authorization: Bearer abc.def\" https://api.example.com",
                "curl -H \"Authorization: *** ***\" https://api.example.com",
            ),
            ("mysql --password hunter2 -u root", "mysql --password *** -u root"),
            ("git clone https://user:pw@github.com/o/r.git", "git clone https://***@github.com/o/r.git"),
            ("echo sk-0123456789abcdef", "echo ***"),
            ("curl -u admin:hunter2 https://x", "curl -u *** https://x"),
            ("curl -H X-Api-Key:abc123 https://x", "curl -H X-Api-Key:*** https://x"),
            ("gh api -H 'Authorization: token abc' /user", "gh api -H 'Authorization: *** ***' /user"),
            ("cargo test -p slashit-ui", "cargo test -p slashit-ui"),
        ];
        for (input, expected) in cases {
            let (_, d) = detail("Bash", json!({ "command": input })).unwrap();
            assert_eq!(d.as_deref(), Some(expected), "{input}");
        }
    }

    #[test]
    fn authorization_headers_are_masked_however_they_are_written() {
        let cases = [
            ("curl -H \"Authorization:Bearer abc\" https://x", "curl -H \"Authorization:*** ***\" https://x"),
            ("curl -H 'Authorization:Basic dXNlcjpw' https://x", "curl -H 'Authorization:*** ***' https://x"),
            ("curl -H \"Authorization:token abc def\" https://x", "curl -H \"Authorization:*** *** ***\" https://x"),
            ("curl -H Authorization:Bearer abc https://x", "curl -H Authorization:*** *** https://x"),
            ("curl -H Authorization:token abc https://x", "curl -H Authorization:*** *** https://x"),
            ("curl -H Authorization: Bearer abc https://x", "curl -H Authorization: *** *** https://x"),
            ("curl -H Authorization: Basic abc https://x", "curl -H Authorization: *** *** https://x"),
            ("curl -H Authorization: token abc https://x", "curl -H Authorization: *** *** https://x"),
            ("curl -H \"Authorization: Basic abc\" https://x", "curl -H \"Authorization: *** ***\" https://x"),
        ];
        for (input, expected) in cases {
            let (_, d) = detail("Bash", json!({ "command": input })).unwrap();
            assert_eq!(d.as_deref(), Some(expected), "{input}");
            assert!(!d.unwrap().contains("abc"), "{input}");
        }
    }

    #[test]
    fn every_free_text_detail_is_sanitized() {
        let token = "ghp_0123456789abcdefghij";
        for (tool, input) in [
            ("WebSearch", json!({ "query": format!("why does {token} fail") })),
            ("Grep", json!({ "pattern": format!("GITHUB_TOKEN={token}") })),
            ("Grep", json!({ "pattern": token })),
            ("Glob", json!({ "pattern": format!("**/{token}/*.rs") })),
            ("Task", json!({ "description": format!("check the key {token}") })),
            ("Agent", json!({ "description": format!("use Bearer {token}") })),
            ("Read", json!({ "file_path": format!("{WD}/{token}.txt") })),
        ] {
            let (_, d) = detail(tool, input.clone()).unwrap();
            let d = d.unwrap();
            assert!(!d.contains(token), "{tool} {input}: {d}");
            assert!(d.contains("***"), "{tool} {input}: {d}");
        }
    }

    #[test]
    fn signed_urls_keep_neither_query_nor_fragment() {
        let url = "https://bucket.s3.amazonaws.com/o?X-Amz-Signature=abc123&X-Amz-Credential=AKIA0123456789ABCDEF#k";
        assert_eq!(
            detail("Bash", json!({ "command": format!("curl -o out \"{url}\"") })),
            Some(("Bash", Some("curl -o out \"https://bucket.s3.amazonaws.com/o\"".to_string())))
        );
        assert_eq!(
            detail("WebSearch", json!({ "query": format!("{url} expired") })),
            Some(("WebSearch", Some("https://bucket.s3.amazonaws.com/o expired".to_string())))
        );
    }

    #[test]
    fn ordinary_text_stays_useful() {
        for (tool, input, expected) in [
            ("WebSearch", json!({"query": "rust token parser basic usage"}), "rust token parser basic usage"),
            ("Task", json!({"description": "Explore the auth module"}), "Explore the auth module"),
            ("Grep", json!({"pattern": "fn record_tool"}), "fn record_tool"),
            ("Bash", json!({"command": "git log --author=rui -5"}), "git log --author=rui -5"),
            ("WebFetch", json!({"url": "https://docs.rs/chrono/latest/chrono/"}), "https://docs.rs/chrono/latest/chrono/"),
        ] {
            assert_eq!(detail(tool, input), Some((describe_name(tool), Some(expected.to_string()))), "{tool}");
        }
    }

    fn describe_name(tool: &str) -> &'static str {
        match tool {
            "Task" | "Agent" => "Subagent",
            "WebSearch" => "WebSearch",
            "Grep" => "Grep",
            "Bash" => "Bash",
            "WebFetch" => "WebFetch",
            _ => unreachable!(),
        }
    }

    #[test]
    fn commands_and_patterns_do_not_record_where_the_checkout_lives() {
        assert_eq!(
            detail("Bash", json!({"command": format!("cd {WD}/src && cargo build")})),
            Some(("Bash", Some("cd ./src && cargo build".to_string())))
        );
        assert_eq!(
            detail("Glob", json!({"pattern": format!("{WD}/**/*.rs")})),
            Some(("Glob", Some("./**/*.rs".to_string())))
        );
        if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()).filter(|h| h.len() > 1) {
            assert_eq!(
                detail("Grep", json!({"pattern": format!("{home}/notes")})),
                Some(("Grep", Some("~/notes".to_string())))
            );
        }
    }

    #[test]
    fn fetched_urls_lose_their_query_and_credentials() {
        assert_eq!(
            detail("WebFetch", json!({"url": "https://u:p@example.com/docs?token=abc#x", "prompt": "summarize"})),
            Some(("WebFetch", Some("https://***@example.com/docs".to_string())))
        );
    }

    #[test]
    fn a_run_buffers_compacted_calls_and_hands_them_to_the_task() {
        let mut tools = RunTools::new(3);
        let now = chrono::Utc::now();
        tools.push(now, "Bash", &json!({"command": "cargo test"}), WD);
        tools.push(now, "Bash", &json!({"command": "cargo test"}), WD);
        tools.push(now, "TodoWrite", &json!({"todos": []}), WD);
        tools.push(now, "Read", &json!({"file_path": format!("{WD}/Cargo.toml")}), WD);

        let mut task = crate::test_helpers::create_test_task("t");
        task.record_activity(slashit_activity::Kind::RunStarted { run: 3, addressing_feedback: false });
        tools.apply_to(&mut task);
        let kinds: Vec<_> = task.activity.iter().map(|e| e.kind.clone()).collect();
        assert_eq!(
            kinds[1..],
            [
                slashit_activity::Kind::ToolUsed { run: 3, tool: "Bash".into(), detail: Some("cargo test".into()), count: 2 },
                slashit_activity::Kind::ToolUsed { run: 3, tool: "Read".into(), detail: Some("Cargo.toml".into()), count: 1 },
            ]
        );
        assert!(task.activity.windows(2).all(|w| w[0].seq < w[1].seq));
    }
}
