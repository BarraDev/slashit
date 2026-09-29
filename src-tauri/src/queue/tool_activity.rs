//! The tool calls a coding run's timeline keeps, and what each one says.
//!
//! A tool call reaches the timeline as its tool's name and one short detail:
//! a command's first line, a path relative to the task's checkout, a search
//! pattern. Only tools that say something about the work are kept; the
//! agent's own bookkeeping (plans, tool discovery, MCP servers and plugins)
//! is not.
//!
//! Nothing here is ever the tool's full input. A command keeps its first
//! line, with anything that looks like a credential masked, and a path
//! outside the checkout keeps only its file name, so neither a secret nor the
//! layout of the user's machine is written into the task file.
//!
//! Calls are buffered per run in memory and reach the task only in the write
//! that ends the run (see [`crate::queue::TaskExecutor`]): the board never
//! shows a timeline row the task file does not have.

use chrono::{DateTime, Utc};
use slashit_activity::{one_line, Entry};

use crate::domain::Task;

/// The longest detail kept for a tool call, in characters.
const MAX_DETAIL_CHARS: usize = 120;

/// The tool calls one run has made so far, compacted and capped exactly as
/// they will be on the task.
#[derive(Debug, Default)]
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

    /// Append the calls to `task`'s timeline.
    pub fn apply_to(self, task: &mut Task) {
        slashit_activity::append(&mut task.activity, self.entries);
    }
}

/// The name and detail a tool call is shown with, or `None` for a tool the
/// timeline does not keep.
pub fn describe(tool: &str, input: &serde_json::Value, working_dir: &str) -> Option<(&'static str, Option<String>)> {
    let field = |key: &str| input.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let (name, detail) = match tool {
        "Bash" => ("Bash", field("command").map(|c| without_private_paths(&first_command_line(c), working_dir))),
        "Read" => ("Read", field("file_path").map(|p| display_path(p, working_dir))),
        "Edit" => ("Edit", field("file_path").map(|p| display_path(p, working_dir))),
        "MultiEdit" => ("Edit", field("file_path").map(|p| display_path(p, working_dir))),
        "Write" => ("Write", field("file_path").map(|p| display_path(p, working_dir))),
        "NotebookEdit" => ("Edit", field("notebook_path").map(|p| display_path(p, working_dir))),
        "Glob" => ("Glob", field("pattern").map(|p| without_private_paths(p, working_dir))),
        "Grep" => ("Grep", field("pattern").map(|p| without_private_paths(p, working_dir))),
        "WebFetch" => ("WebFetch", field("url").map(without_query)),
        "WebSearch" => ("WebSearch", field("query").map(str::to_string)),
        "Task" | "Agent" => ("Subagent", field("description").map(str::to_string)),
        _ => return None,
    };
    Some((name, detail.map(|d| one_line(&d, MAX_DETAIL_CHARS)).filter(|d| !d.is_empty())))
}

/// The first non-blank line of a shell command, with credentials masked.
fn first_command_line(command: &str) -> String {
    let line = command.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    redact(line)
}

/// `text` with the task's checkout written as `.` and the home directory as
/// `~`, so a command that names absolute paths (`cd /home/me/... && ...`)
/// does not record the layout of the user's machine.
fn without_private_paths(text: &str, working_dir: &str) -> String {
    let root = working_dir.trim_end_matches('/');
    let mut out = if root.is_empty() { text.to_string() } else { text.replace(root, ".") };
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) {
        let home = home.trim_end_matches('/');
        if home.len() > 1 {
            out = out.replace(home, "~");
        }
    }
    out
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

/// A URL without its query, fragment or credentials, any of which can carry
/// a token.
fn without_query(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    mask_url_credentials(url)
}

/// Words that mark an assignment, flag or header as carrying a secret.
const SECRET_WORDS: &[&str] = &[
    "token", "secret", "password", "passwd", "pwd", "apikey", "api_key", "api-key", "auth", "credential",
    "cookie", "private_key", "access_key",
];

/// Prefixes of well-known credential formats.
const SECRET_PREFIXES: &[&str] = &[
    "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "glpat-", "sk-", "xoxb-", "xoxp-", "xoxa-", "AKIA",
];

const MASK: &str = "***";

fn names_secret(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|s| word.contains(s))
}

/// Mask what looks like a credential in one shell command line: the value
/// of an assignment or flag whose name mentions one, the word after
/// `Bearer`/`Basic`, a token in a well-known format, and credentials in a
/// URL. Best effort by construction -- which is why the rest of the input is
/// never kept at all.
pub fn redact(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    // After `-u`/`--user`: masked when it is `user:password`.
    let mut user_next = false;
    // Inside a quoted header whose name mentions a secret: every word up to
    // the closing quote is its value.
    let mut in_secret_header = false;
    for word in line.split_whitespace() {
        let closes_quote = word.ends_with(['"', '\'']);
        if in_secret_header {
            out.push(if closes_quote { format!("{MASK}{}", &word[word.len() - 1..]) } else { MASK.to_string() });
            in_secret_header = !closes_quote;
            continue;
        }
        if mask_next {
            out.push(MASK.to_string());
            mask_next = false;
            continue;
        }
        if std::mem::take(&mut user_next) && word.contains(':') {
            out.push(MASK.to_string());
            continue;
        }
        let bare = word.trim_matches(|c| c == '"' || c == '\'');
        let lower = bare.to_ascii_lowercase();
        if lower == "bearer" || lower == "basic" || lower == "token" {
            out.push(word.to_string());
            mask_next = true;
            continue;
        }
        if let Some((name, _)) = bare.split_once('=') {
            if names_secret(name) {
                out.push(format!("{name}={MASK}"));
                continue;
            }
        }
        // `curl -u user:secret`: the value after a user flag carries the
        // password when it has one.
        if matches!(bare, "-u" | "--user") {
            out.push(word.to_string());
            user_next = true;
            continue;
        }
        if bare.starts_with('-') && names_secret(bare) {
            out.push(word.to_string());
            mask_next = true;
            continue;
        }
        if let Some((name, value)) = bare.split_once(':') {
            if !bare.contains("://") && names_secret(name) {
                if value.is_empty() {
                    // `"Authorization: Bearer x"`: the value is in the
                    // following words.
                    out.push(word.to_string());
                    in_secret_header = word.starts_with(['"', '\'']) && !closes_quote;
                    mask_next = !in_secret_header;
                } else {
                    out.push(format!("{name}:{MASK}"));
                }
                continue;
            }
        }
        if SECRET_PREFIXES.iter().any(|p| bare.starts_with(p) && bare.len() > p.len() + 8) {
            out.push(MASK.to_string());
            continue;
        }
        out.push(mask_url_credentials(word));
    }
    out.join(" ")
}

fn mask_url_credentials(word: &str) -> String {
    let Some(scheme_end) = word.find("://") else {
        return word.to_string();
    };
    let rest = &word[scheme_end + 3..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    match rest[..host_end].rfind('@') {
        Some(at) => format!("{}{MASK}@{}", &word[..scheme_end + 3], &rest[at + 1..]),
        None => word.to_string(),
    }
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
        assert_eq!(d.unwrap().chars().count(), MAX_DETAIL_CHARS);
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
