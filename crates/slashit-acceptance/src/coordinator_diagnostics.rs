//! Sanitized diagnostics for a failed Coordinator acceptance journey.
//!
//! The CI failure artifact uploads `coordinator-diagnostics.json` and nothing
//! richer. Every value in it is a count, a boolean, or one of the fixed
//! words this module defines. Nothing here carries a prompt, a model answer,
//! a path, an environment value or Human content, and a trace line that does
//! not match the expected shape is recorded as `unrecognized` rather than
//! echoed. The vocabulary below is the whole of the permitted content.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

/// The file a failed Coordinator journey writes into its artifact directory.
pub const FILE_NAME: &str = "coordinator-diagnostics.json";

/// The directory the fixture writes in the marker directory. It is a directory
/// so `FakeAgent::invocations()` skips it. Its `runs` file holds one line per
/// Coordinator run: `position=<n> scripted=<word> structured=<word>`.
pub const TRACE_DIR: &str = ".coordinator-trace";
pub const TRACE_RUNS: &str = "runs";

pub const SCHEMA_VERSION: u64 = 1;

const SCRIPTED_WORDS: [&str; 3] = ["found", "missing", "not_applicable"];
const STRUCTURED_WORDS: [&str; 3] = ["object", "none", "not_applicable"];

/// One Coordinator run as the fixture recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub position: u64,
    pub scripted: &'static str,
    pub structured: &'static str,
}

/// The whole record. `rendered_proposal_cards` and `persisted_actions` are
/// `None` when the journey could not read them at the point of failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    /// Coordinator runs claimed in the marker directory.
    pub invocations: usize,
    pub runs: Vec<Run>,
    pub rendered_proposal_cards: Option<usize>,
    pub persisted_actions: Option<usize>,
}

impl Diagnostics {
    /// The JSON written to the artifact. The key set is fixed, so a test can
    /// prove no other field can appear.
    pub fn to_json(&self) -> Value {
        let runs: Vec<Value> = self
            .runs
            .iter()
            .map(|run| json!({ "position": run.position, "scripted": run.scripted, "structured": run.structured }))
            .collect();
        let mut map = Map::new();
        map.insert("schema_version".into(), json!(SCHEMA_VERSION));
        map.insert("invoked".into(), json!(self.invocations > 0));
        map.insert("invocations".into(), json!(self.invocations));
        map.insert("runs".into(), Value::Array(runs));
        map.insert("rendered_proposal_cards".into(), json!(self.rendered_proposal_cards));
        map.insert("persisted_actions".into(), json!(self.persisted_actions));
        Value::Object(map)
    }
}

/// Parses the fixture's trace. Each line is checked against the expected
/// shape; anything else becomes an `unrecognized` run, so a corrupted or
/// unexpected line is visible without its content being copied.
pub fn parse_trace(trace: &str) -> Vec<Run> {
    trace.lines().filter(|line| !line.trim().is_empty()).map(parse_line).collect()
}

fn parse_line(line: &str) -> Run {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let position = fields.first().and_then(|field| field.strip_prefix("position=")).and_then(|value| value.parse::<u64>().ok());
    let scripted = fields.get(1).and_then(|field| field.strip_prefix("scripted=")).and_then(|value| word(&SCRIPTED_WORDS, value));
    let structured = fields.get(2).and_then(|field| field.strip_prefix("structured=")).and_then(|value| word(&STRUCTURED_WORDS, value));
    match (position, scripted, structured, fields.len()) {
        (Some(position), Some(scripted), Some(structured), 3) => Run { position, scripted, structured },
        _ => Run { position: 0, scripted: "unrecognized", structured: "unrecognized" },
    }
}

/// Returns the vocabulary entry that equals `value`, so the stored value is
/// always a constant from this module and never the text from the trace.
fn word(vocabulary: &'static [&'static str], value: &str) -> Option<&'static str> {
    vocabulary.iter().copied().find(|known| *known == value)
}

/// Counts the Coordinator runs the fake agent claimed: the numbered
/// `.coordinator-<n>` directories. The output and trace entries are excluded
/// because their names are not numbers.
pub fn count_claims(marker_dir: &Path) -> Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir(marker_dir).with_context(|| format!("read {}", marker_dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let numbered = name.strip_prefix(".coordinator-").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
        if numbered && entry.file_type()?.is_dir() {
            count += 1;
        }
    }
    Ok(count)
}

/// Reads the trace from the marker directory. A missing trace means no
/// Coordinator run reached the structured-output step, which is itself the
/// finding, so it is an empty list and not an error.
pub fn read_trace(marker_dir: &Path) -> Result<String> {
    match std::fs::read_to_string(marker_dir.join(TRACE_DIR).join(TRACE_RUNS)) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read the Coordinator trace in {}", marker_dir.display())),
    }
}

/// Builds the record from the marker directory alone. The journey adds the
/// application-side counts it can read at the point of failure.
pub fn collect(marker_dir: &Path, rendered_proposal_cards: Option<usize>, persisted_actions: Option<usize>) -> Result<Diagnostics> {
    Ok(Diagnostics {
        invocations: count_claims(marker_dir)?,
        runs: parse_trace(&read_trace(marker_dir)?),
        rendered_proposal_cards,
        persisted_actions,
    })
}

/// Writes the record into the journey's artifact directory.
pub fn write(artifacts: &Path, diagnostics: &Diagnostics) -> Result<()> {
    let path = artifacts.join(FILE_NAME);
    std::fs::write(&path, serde_json::to_vec_pretty(&diagnostics.to_json())?)
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("slashit-coordinator-diag-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    #[test]
    fn a_valid_trace_keeps_only_the_known_words() {
        let runs = parse_trace("position=1 scripted=found structured=object\nposition=2 scripted=missing structured=none\n");
        assert_eq!(
            runs,
            vec![
                Run { position: 1, scripted: "found", structured: "object" },
                Run { position: 2, scripted: "missing", structured: "none" },
            ]
        );
    }

    #[test]
    fn a_line_that_is_not_the_expected_shape_is_unrecognized_and_never_echoed() {
        let runs = parse_trace("position=1 scripted=found structured=object extra=/home/someone/secret\nhello world\nposition=x scripted=found structured=object\nposition=3 scripted=unexpected structured=object\n");
        assert_eq!(runs.len(), 4);
        for run in &runs {
            assert_eq!((run.scripted, run.structured), ("unrecognized", "unrecognized"), "{run:?}");
        }
    }

    #[test]
    fn the_record_has_exactly_the_permitted_fields_and_vocabulary() {
        let diagnostics = Diagnostics {
            invocations: 2,
            runs: parse_trace("position=1 scripted=found structured=none\nposition=2 scripted=found structured=object\n"),
            rendered_proposal_cards: Some(0),
            persisted_actions: None,
        };
        let json = diagnostics.to_json();
        let mut keys: Vec<&str> = json.as_object().expect("object").keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["invocations", "invoked", "persisted_actions", "rendered_proposal_cards", "runs", "schema_version"],
        );
        let mut run_keys: Vec<&str> = json["runs"][0].as_object().expect("run object").keys().map(String::as_str).collect();
        run_keys.sort_unstable();
        assert_eq!(run_keys, ["position", "scripted", "structured"]);
        assert_eq!(json["invoked"], true);
        assert!(json["persisted_actions"].is_null(), "an uncaptured count is null, not zero");
        let text = json.to_string();
        for forbidden in ["prompt", "/home", "result", "token", "text"] {
            assert!(!text.contains(forbidden), "{forbidden} leaked into {text}");
        }
    }

    #[test]
    fn claims_count_only_numbered_coordinator_directories() {
        let dir = scratch("claims");
        for name in [".coordinator-1", ".coordinator-2", ".coordinator-output", ".coordinator-trace", ".coordinator-x", "other"] {
            std::fs::create_dir_all(dir.join(name)).expect("entry");
        }
        assert_eq!(count_claims(&dir).expect("count"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_trace_is_an_empty_record_not_an_error() {
        let dir = scratch("no-trace");
        assert_eq!(read_trace(&dir).expect("read"), "");
        let diagnostics = collect(&dir, None, None).expect("collect");
        assert!(diagnostics.runs.is_empty());
        assert_eq!(diagnostics.invocations, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs the real fixture with a `PATH` the test controls, so the `jq`
    /// behavior is set by the test and not by the runner.
    fn run_fixture(path: &str, marker: &Path) -> std::process::Output {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/claude");
        std::process::Command::new(script)
            .args(["-p", "--output-format", "stream-json", "--json-schema", "{}"])
            .env_clear()
            .env("PATH", path)
            .env(crate::fake_agent::MARKER_DIR_VAR, marker)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the fixture")
    }

    /// The stub directory first, so its `jq` wins, then the system tools the
    /// fixture needs for everything else.
    fn with_system_tools(stub: &Path) -> String {
        format!("{}:/usr/bin:/bin", stub.display())
    }

    /// A directory holding one executable `jq` that exits with `status`, or no
    /// `jq` at all when `status` is `None`.
    fn jq_directory(name: &str, status: Option<u8>) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch(name);
        if let Some(status) = status {
            let jq = dir.join("jq");
            std::fs::write(&jq, format!("#!/bin/sh\nexit {status}\n")).expect("jq stub");
            std::fs::set_permissions(&jq, std::fs::Permissions::from_mode(0o755)).expect("jq mode");
        }
        dir
    }

    #[test]
    fn a_json_schema_run_without_jq_fails_and_names_jq() {
        let dir = jq_directory("no-jq", None);
        let marker = scratch("no-jq-markers");
        let output = run_fixture(dir.to_str().expect("utf-8 path"), &marker);
        assert_eq!(output.status.code(), Some(96), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("jq is required"), "{output:?}");
        assert!(output.stdout.is_empty(), "no answer is emitted without jq");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&marker);
    }

    #[test]
    fn a_broken_jq_fails_the_run_instead_of_dropping_the_answer() {
        let dir = jq_directory("broken-jq", Some(3));
        let marker = scratch("broken-jq-markers");
        let output = run_fixture(&with_system_tools(&dir), &marker);
        assert_eq!(output.status.code(), Some(95), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("failed unexpectedly"), "{output:?}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&marker);
    }

    #[test]
    fn a_prose_or_malformed_answer_gets_no_structured_output_and_is_not_an_error() {
        // jq status 2 is what the real tool reports for text that is not JSON.
        let dir = jq_directory("prose-jq", Some(2));
        let marker = scratch("prose-jq-markers");
        let output = run_fixture(&with_system_tools(&dir), &marker);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("\"type\":\"result\""), "{stdout}");
        assert!(!stdout.contains("structured_output"), "prose must not become structured output: {stdout}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&marker);
    }

    /// Regression: the trace once broke `FakeAgent::invocations()`, which every
    /// journey that counts Worker runs calls. The reader must accept a marker
    /// directory that holds a written trace.
    #[test]
    fn the_fake_agent_reader_accepts_a_written_trace() {
        let root = scratch("reader");
        let agent = crate::fake_agent::FakeAgent::install(&root).expect("install the fake agent");
        let trace = agent.marker_dir().join(TRACE_DIR);
        std::fs::create_dir_all(&trace).expect("trace directory");
        std::fs::write(trace.join(TRACE_RUNS), "position=1 scripted=found structured=object\n").expect("trace");
        assert!(agent.invocations().expect("the reader accepts the trace").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_file_is_written_into_the_artifact_directory() {
        let dir = scratch("write");
        let diagnostics = Diagnostics { invocations: 0, runs: vec![], rendered_proposal_cards: None, persisted_actions: None };
        write(&dir, &diagnostics).expect("write");
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).expect("file");
        assert!(text.contains("\"schema_version\": 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
