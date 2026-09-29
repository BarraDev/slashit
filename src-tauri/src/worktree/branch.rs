/// Refuses a task branch name that is not safe to hand to `git`, `jj` or `gh`.
///
/// `Task.branch_name` is read back from `tasks.toml`, and for a board stored in
/// the project that file is whatever the last commit made it, so the value is
/// untrusted even though SlashIt writes `task-<task id>` itself. Every function
/// that passes a recorded branch to a process checks it first: the PR commands,
/// and the worktree acquisition in [`super::WorktreeManager`]. A leading `-`
/// would be parsed as an option (`git push -u origin --mirror` deletes every
/// remote branch the local repository lacks; `git worktree add <dest> -Bvictim`
/// force-resets `victim`), and revision, revset or glob syntax would select
/// commits or bookmarks other than the task's own. The accepted shape is a
/// valid Git branch name made only of ASCII letters, digits, `.`, `_`, `/` and
/// `-`: that covers every name SlashIt generates, and no such name contains
/// quoting, revision or revset operators, or glob metacharacters. A refused
/// value is reported, never rewritten into something else.
pub fn checked_task_branch(branch: &str) -> Result<&str, String> {
    checked_branch(branch).map_err(|why| {
        format!(
            "The task's recorded branch {branch:?} {why}, so SlashIt will not pass it to \
             git, jj or gh. Check the task's board file for an unexpected change."
        )
    })
}

/// [`checked_task_branch`]'s rules for a project's base branch
/// (`domain::ProjectBase`), which is read back from the configuration file
/// and chosen from the repository's own branch names.
pub fn checked_base_branch(branch: &str) -> Result<&str, String> {
    checked_branch(branch).map_err(|why| {
        format!(
            "The branch {branch:?} {why}, so SlashIt will not use it as the project's base \
             branch. Choose another branch in Settings > Repository."
        )
    })
}

/// Whether `name` is a branch SlashIt names after a task: `task-<task id>`,
/// or `task-<first 8 hex digits>` as earlier versions named them. Such a
/// branch is never offered as a project's base.
pub fn looks_like_task_branch(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("task-") else {
        return false;
    };
    let hex8 = |s: &str| s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit());
    hex8(rest) || uuid::Uuid::try_parse(rest).is_ok_and(|id| id.hyphenated().to_string() == rest)
}

/// Why `branch` is not a plain branch name, if it is not.
fn checked_branch(branch: &str) -> Result<&str, &'static str> {
    let refuse = |why: &'static str| Err(why);

    if branch.is_empty() {
        return refuse("is empty");
    }
    if branch.starts_with('-') {
        return refuse("starts with `-`");
    }
    if !branch
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    {
        return refuse("contains characters other than letters, digits, `.`, `_`, `/` and `-`");
    }
    // The rules of `git check-ref-format --branch` that the character set
    // above leaves open.
    let bad_component = branch
        .split('/')
        .any(|c| c.is_empty() || c.starts_with('.') || c.ends_with(".lock"));
    if branch == "HEAD" || branch.contains("..") || branch.ends_with('.') || bad_component {
        return refuse("is not a valid Git branch name");
    }
    Ok(branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Only the names SlashIt gives task branches count as task branches;
    /// a person's branch that merely starts with `task-` does not.
    #[test]
    fn task_branch_names_are_recognised_exactly() {
        let generated = super::super::WorktreeManager::branch_for_task(Uuid::new_v4());
        assert!(looks_like_task_branch(&generated));
        assert!(looks_like_task_branch("task-c0ffee11"));
        for other in ["task-login-page", "task-c0ffee1", "task-", "main-task-c0ffee11", "trunk"] {
            assert!(!looks_like_task_branch(other), "{other}");
        }
        assert!(checked_base_branch("-x").unwrap_err().contains("project's base branch"));
    }

    #[test]
    fn checked_task_branch_accepts_the_names_slashit_generates() {
        for _ in 0..64 {
            let generated = super::super::WorktreeManager::branch_for_task(Uuid::new_v4());
            assert_eq!(checked_task_branch(&generated), Ok(generated.as_str()));
        }
        // `task-<8 hex>` is what earlier versions wrote, and a board may
        // still carry it.
        for branch in ["task-abcd1234", "feature/login", "fix_1.2-rc", "task-"] {
            assert_eq!(checked_task_branch(branch), Ok(branch), "{branch}");
        }
    }

    #[test]
    fn checked_task_branch_refuses_options_revsets_globs_and_invalid_refs() {
        for branch in [
            "", "--mirror", "-f", "--receive-pack=touch x", "mutable()", "a|b", "glob:*",
            "exact:main", "task*", "task?", "a b", "a\nb", "a\"b", "a\\b", "a~1", "a^",
            "a@{1}", "a..b", "a//b", "/a", "a/", ".a", "a/.b", "a.lock", "a.lock/b", "a.",
            "HEAD", "tâsk", "-Bvictim", "-m", "HEAD~1", "HEAD^", "@{-1}", "main:path",
        ] {
            assert!(checked_task_branch(branch).is_err(), "{branch:?} must be refused");
        }
    }
}
