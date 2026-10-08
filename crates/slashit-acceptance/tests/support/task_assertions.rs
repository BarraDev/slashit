//! Filesystem and process-record assertions used by acceptance tests.

use anyhow::{bail, Result};
use slashit_acceptance::fake_agent;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(crate) fn assert_persisted(
    root: &Path,
    task_id: &str,
    title: &str,
    status: &str,
) -> Result<()> {
    let mut examined = 0usize;
    let mut found_as: Vec<String> = Vec::new();
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        examined += 1;
        // A state root holds more than the task store, and not every file in it
        // has to parse for this assertion to mean something.
        let Ok(document) = contents.parse::<toml::Value>() else {
            continue;
        };
        let Some(record) = task_record(&document, task_id) else {
            continue;
        };
        let recorded_title = record.get("title").and_then(toml::Value::as_str);
        let recorded = record.get("status").and_then(toml::Value::as_str);
        // One record carrying the id, the title and the settled status is the
        // whole claim. Others may exist — the store has had more than one
        // layout — and an older one lagging behind is not evidence that this
        // state was never written.
        if recorded_title == Some(title) && recorded == Some(status) {
            return Ok(());
        }
        found_as.push(format!(
            "{} records it as {recorded_title:?} with status {recorded:?}",
            file.display()
        ));
    }

    if found_as.is_empty() {
        bail!(
            "no file under {} holds a task record with id {} — nothing was persisted ({examined} \
             TOML files examined)",
            root.display(),
            task_id
        );
    }

    bail!(
        "task {} is persisted, but not as the state the application reported — expected the title \
         {:?} with status {status:?}, and {}",
        task_id,
        title,
        found_as.join("; ")
    )
}

/// The stored record for `id` in one parsed task store, if it holds one.
///
/// `Storage::save_project_tasks` writes a `version` and an array of tasks, so
/// one file routinely holds several. That is why this reads the array and
/// matches on the record's own `id` rather than looking for the values anywhere
/// in the document: a file can perfectly well contain the wanted id, the wanted
/// title and the wanted status while no single task has all three, and a proof
/// that cannot tell those apart is not proving the task was persisted.
pub(crate) fn task_record<'a>(
    document: &'a toml::Value,
    id: &str,
) -> Option<&'a toml::value::Table> {
    document
        .get("tasks")?
        .as_array()?
        .iter()
        .filter_map(toml::Value::as_table)
        .find(|record| record.get("id").and_then(toml::Value::as_str) == Some(id))
}

/// Every `.toml` under `root`, recursively.
pub(crate) fn toml_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory that vanished mid-walk is not this assertion's
            // problem; the state root has a live application's runtime in it.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "toml") {
                found.push(path);
            }
        }
    }
    Ok(found)
}

pub(crate) fn retry_among<'a>(
    runs: &'a [fake_agent::Invocation],
    failed_session: &OsStr,
) -> Result<&'a fake_agent::Invocation> {
    let retries: Vec<&fake_agent::Invocation> = runs
        .iter()
        .filter(|run| {
            matches!(
                run.flag("--session-id"),
                Some(session) if !session.is_empty() && session != failed_session
            )
        })
        .collect();

    match retries.as_slice() {
        [only] => Ok(only),
        [] => bail!(
            "no agent run carries a session of its own other than {failed_session:?}, so the \
             retry cannot be told apart from the attempt it was retrying"
        ),
        many => bail!(
            "{} agent runs carry a session other than the failed attempt's, expected exactly one \
             retry",
            many.len()
        ),
    }
}

#[cfg(all(test, not(feature = "run-acceptance")))]
mod tests {
    use super::*;

    struct ExecutedTask {
        id: String,
        title: String,
    }

    fn assert_persisted_for_task(root: &Path, task: &ExecutedTask, status: &str) -> Result<()> {
        super::assert_persisted(root, &task.id, &task.title, status)
    }
    use std::ffi::OsString;

    fn run(session: &str, working_dir: &str) -> fake_agent::Invocation {
        fake_agent::Invocation {
            working_dir: PathBuf::from(working_dir),
            args: ["-p", "--session-id", session]
                .iter()
                .map(OsString::from)
                .collect(),
            prompt: Some("do the work".to_string()),
        }
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slashit-persistence-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    /// A store written the way `Storage::save_project_tasks` writes one.
    fn store(root: &Path, records: &[(&str, &str, &str)]) -> PathBuf {
        let mut document = String::from("version = 1\n");
        for (id, title, status) in records {
            document.push_str(&format!(
                "\n[[tasks]]\nid = \"{id}\"\ntitle = \"{title}\"\nstatus = \"{status}\"\n"
            ));
        }
        let file = root.join("tasks.toml");
        std::fs::write(&file, document).expect("write the task store");
        file
    }

    fn executed(id: &str, title: &str) -> ExecutedTask {
        ExecutedTask {
            id: id.to_string(),
            title: title.to_string(),
        }
    }

    /// The state the journeys actually assert: one record holding all three.
    #[test]
    fn a_record_carrying_the_id_title_and_status_together_is_the_proof() {
        let root = scratch("settled");
        store(
            &root,
            &[
                ("11111111-aaaa", "some other task", "backlog"),
                ("22222222-bbbb", "the task under test", "human_review"),
            ],
        );

        assert!(assert_persisted_for_task(
            &root,
            &executed("22222222-bbbb", "the task under test"),
            "human_review"
        )
        .is_ok());
    }

    /// The proof this helper exists to make impossible. Neither record is the
    /// task the journey settled: one has its id under a different title and
    /// status, the other has the title and status under a different id. Read as
    /// loose strings the file contains every value being looked for, which is
    /// exactly why looking for loose strings proved nothing.
    #[test]
    fn values_split_across_two_records_are_not_a_persisted_task() {
        let root = scratch("split");
        store(
            &root,
            &[
                ("22222222-bbbb", "some other task", "backlog"),
                ("11111111-aaaa", "the task under test", "human_review"),
            ],
        );

        assert!(
            assert_persisted_for_task(
                &root,
                &executed("22222222-bbbb", "the task under test"),
                "human_review"
            )
            .is_err(),
            "a task's id, title and status were read from different records"
        );
    }

    /// Rejections that would otherwise look like the settled state.
    #[test]
    fn a_record_that_does_not_match_is_not_the_proof() {
        let root = scratch("mismatched");
        store(
            &root,
            &[("22222222-bbbb", "the task under test", "in_progress")],
        );

        for (case, id, status) in [
            (
                "the task was never written at all",
                "33333333-cccc",
                "human_review",
            ),
            (
                "the record settled at another status",
                "22222222-bbbb",
                "human_review",
            ),
        ] {
            assert!(
                assert_persisted_for_task(&root, &executed(id, "the task under test"), status)
                    .is_err(),
                "{case} was accepted as proof of the settled state"
            );
        }
    }

    /// What the runner produces for a `None` session: the flag is not passed at
    /// all, rather than passed with nothing after it.
    fn run_without_a_session(working_dir: &str) -> fake_agent::Invocation {
        fake_agent::Invocation {
            working_dir: PathBuf::from(working_dir),
            args: ["-p"].iter().map(OsString::from).collect(),
            prompt: Some("do the work".to_string()),
        }
    }

    /// The selection has to survive the records arriving in an order that says
    /// nothing about when they ran, because that is the only order the fixture
    /// guarantees. Reversed here on purpose: positional selection would return
    /// the failed attempt and every assertion downstream would still pass.
    #[test]
    fn the_retry_is_found_by_session_whatever_order_the_records_arrive_in() {
        let failed = OsString::from("session-of-the-failure");
        let shared_worktree = "/tmp/wt/task-abcd1234";

        for runs in [
            vec![
                run("session-of-the-failure", shared_worktree),
                run("session-of-the-retry", shared_worktree),
            ],
            vec![
                run("session-of-the-retry", shared_worktree),
                run("session-of-the-failure", shared_worktree),
            ],
        ] {
            let retry = retry_among(&runs, &failed).expect("one run is not the failed attempt");
            assert_eq!(
                retry.flag("--session-id"),
                Some(OsStr::new("session-of-the-retry")),
                "the retry was selected by position rather than by session"
            );
        }
    }

    /// Everything the retry is not, read together, because the ways this can go
    /// wrong are variations on one idea: a record the journey cannot show to be
    /// a second execution with an identity of its own. Each row would otherwise
    /// be selected and then satisfy every remaining assertion in the stage,
    /// since both attempts of a retried task share a worktree and a set of
    /// flags — which is what makes these quiet rather than loud.
    #[test]
    fn no_run_without_an_identity_of_its_own_is_taken_as_the_retry() {
        let failed = OsString::from("session-of-the-failure");
        let shared_worktree = "/tmp/wt/task-abcd1234";

        for (case, runs) in [
            (
                "a candidate that never reported a session",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run_without_a_session(shared_worktree),
                ],
            ),
            (
                "a candidate whose session is empty",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run("", shared_worktree),
                ],
            ),
            (
                "two runs reporting the failed attempt's own session",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run("session-of-the-failure", shared_worktree),
                ],
            ),
            (
                "more separately identified runs than the journey arranged",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run("session-of-the-retry", shared_worktree),
                    run("session-of-something-else", shared_worktree),
                ],
            ),
        ] {
            assert!(
                retry_among(&runs, &failed).is_err(),
                "{case} was accepted as the retry"
            );
        }
    }
}
