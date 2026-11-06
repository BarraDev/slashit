// SPDX-License-Identifier: MPL-2.0
//! Linking a GitHub Issue to a Task: which web addresses name an Issue, and
//! when a Task already holds one.
//!
//! Every function here refuses rather than repairs. The network read that
//! confirms the Issue is in `commands::github`; nothing here asks GitHub.

use crate::domain::{ExternalRef, Task};

const ORIGIN: &str = "https://github.com/";

/// The only address form an Issue link accepts.
pub const ISSUE_ADDRESS_FORM: &str = "https://github.com/<owner>/<repo>/issues/<number>";

/// An Issue as its address names it: `repo` is `owner/name` and `number` is
/// positive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueAddress {
    pub repo: String,
    pub number: u32,
}

impl IssueAddress {
    /// The canonical address, rebuilt from the parts so that a stored link
    /// never carries a query, a fragment or stray whitespace.
    pub fn url(&self) -> String {
        format!("{ORIGIN}{}/issues/{}", self.repo, self.number)
    }
}

/// Read an Issue address.
///
/// Accepts exactly `https://github.com/<owner>/<repo>/issues/<number>`, with
/// surrounding whitespace trimmed. The scheme and host are matched exactly
/// (no `http`, no `www`, no other host). A query string or a fragment is
/// refused, not ignored. A pull request address is refused with its own
/// message. The number must be a positive decimal number; leading zeros are
/// accepted and dropped from the canonical address.
pub fn parse_issue_url(input: &str) -> Result<IssueAddress, String> {
    let form_error = || format!("Enter a GitHub Issue address of the form {ISSUE_ADDRESS_FORM}.");
    let Some(path) = input.trim().strip_prefix(ORIGIN) else {
        return Err(form_error());
    };
    if path.contains(['?', '#']) {
        return Err(
            "Remove the query string or fragment. Only the Issue address is linked.".to_string(),
        );
    }
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        [_, _, "pull", _] => Err(format!(
            "That is a pull request address. Link a GitHub Issue instead, of the form {ISSUE_ADDRESS_FORM}."
        )),
        [owner, repo, "issues", number] if valid_owner(owner) && valid_repo(repo) => Ok(IssueAddress {
            repo: format!("{owner}/{repo}"),
            number: parse_number(number)?,
        }),
        _ => Err(form_error()),
    }
}

/// A GitHub user or organization name: 1 to 39 letters, digits or hyphens,
/// not starting or ending with a hyphen.
fn valid_owner(owner: &str) -> bool {
    (1..=39).contains(&owner.len())
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !owner.starts_with('-')
        && !owner.ends_with('-')
}

/// A repository name: 1 to 100 letters, digits, `-`, `_` or `.`, and not `.`
/// or `..`.
fn valid_repo(repo: &str) -> bool {
    (1..=100).contains(&repo.len())
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && repo != "."
        && repo != ".."
}

fn parse_number(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(number) if number > 0 && text.bytes().all(|b| b.is_ascii_digit()) => Ok(number),
        _ => Err("The Issue number must be a positive whole number.".to_string()),
    }
}

/// Whether `task` already links this Issue. The repository is compared
/// without regard to case, as GitHub does.
pub fn holds_issue(task: &Task, issue: &IssueAddress) -> bool {
    task.external_refs.iter().any(|r| {
        matches!(r, ExternalRef::GithubIssue { repo, number, .. }
            if *number == issue.number && repo.eq_ignore_ascii_case(&issue.repo))
    })
}

/// Add the Issue to `task` as a GitHub Issue reference, recording the state
/// GitHub reported. Refuses a repeated link and changes nothing then. Existing
/// references, including legacy ones, are left as they are.
pub fn attach_github_issue(
    task: &mut Task,
    issue: &IssueAddress,
    state: &str,
) -> Result<(), String> {
    if holds_issue(task, issue) {
        return Err(format!(
            "This Task already links issue #{} in {}.",
            issue.number, issue.repo
        ));
    }
    task.external_refs.push(ExternalRef::GithubIssue {
        url: issue.url(),
        number: issue.number,
        repo: issue.repo.clone(),
        state: Some(state.to_string()),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_task;

    fn refused(input: &str) -> String {
        parse_issue_url(input).expect_err(input)
    }

    #[test]
    fn accepts_the_issue_address_and_rebuilds_it_canonically() {
        let issue = parse_issue_url("  https://github.com/Owner-1/Repo.name/issues/042  ").unwrap();
        assert_eq!(
            issue,
            IssueAddress {
                repo: "Owner-1/Repo.name".into(),
                number: 42
            }
        );
        assert_eq!(
            issue.url(),
            "https://github.com/Owner-1/Repo.name/issues/42"
        );
    }

    #[test]
    fn refuses_a_pull_request_address_with_its_own_message() {
        assert!(refused("https://github.com/o/r/pull/7").contains("pull request address"));
    }

    #[test]
    fn refuses_any_other_scheme_host_or_shape() {
        for input in [
            "http://github.com/o/r/issues/1",
            "https://www.github.com/o/r/issues/1",
            "https://gitlab.com/o/r/issues/1",
            "https://github.com.example.test/o/r/issues/1",
            "https://example.test/github.com/o/r/issues/1",
            "github.com/o/r/issues/1",
            "https://github.com/o/r/issues",
            "https://github.com/o/r/issues/1/",
            "https://github.com/o/r/issues/1/extra",
            "https://github.com/o/r/issue/1",
            "https://github.com/o/r/discussions/1",
            "https://github.com//r/issues/1",
            "https://github.com/-o/r/issues/1",
            "https://github.com/o/../issues/1",
            "https://github.com/o/./issues/1",
            "",
        ] {
            refused(input);
        }
    }

    #[test]
    fn refuses_a_query_or_a_fragment_rather_than_dropping_it() {
        assert!(
            refused("https://github.com/o/r/issues/1?ref=x").contains("query string or fragment")
        );
        assert!(refused("https://github.com/o/r/issues/1#issuecomment-9")
            .contains("query string or fragment"));
    }

    #[test]
    fn refuses_a_number_that_is_not_a_positive_decimal() {
        for input in [
            "https://github.com/o/r/issues/0",
            "https://github.com/o/r/issues/00",
            "https://github.com/o/r/issues/abc",
            "https://github.com/o/r/issues/+5",
            "https://github.com/o/r/issues/-5",
            "https://github.com/o/r/issues/",
            "https://github.com/o/r/issues/99999999999999999999",
        ] {
            refused(input);
        }
        assert!(refused("https://github.com/o/r/issues/0").contains("positive whole number"));
    }

    #[test]
    fn attach_records_the_issue_with_its_state_and_keeps_existing_references() {
        let mut task = create_test_task("linked");
        task.github_issue_url = Some("https://github.com/o/r/issues/3".into());
        task.external_refs.push(ExternalRef::GitlabIssue {
            url: "https://gitlab.com/o/r/-/issues/1".into(),
        });
        let issue = parse_issue_url("https://github.com/o/r/issues/3").unwrap();
        attach_github_issue(&mut task, &issue, "OPEN").unwrap();
        assert_eq!(
            task.external_refs,
            vec![
                ExternalRef::GitlabIssue {
                    url: "https://gitlab.com/o/r/-/issues/1".into()
                },
                ExternalRef::GithubIssue {
                    url: "https://github.com/o/r/issues/3".into(),
                    number: 3,
                    repo: "o/r".into(),
                    state: Some("OPEN".into()),
                },
            ]
        );
        assert_eq!(
            task.github_issue_url.as_deref(),
            Some("https://github.com/o/r/issues/3")
        );
    }

    #[test]
    fn attach_refuses_the_same_issue_twice_even_with_another_repository_case() {
        let mut task = create_test_task("dup");
        let first = parse_issue_url("https://github.com/Owner/Repo/issues/5").unwrap();
        attach_github_issue(&mut task, &first, "OPEN").unwrap();
        let again = parse_issue_url("https://github.com/owner/repo/issues/5").unwrap();
        assert!(holds_issue(&task, &again));
        let refusal = attach_github_issue(&mut task, &again, "CLOSED").unwrap_err();
        assert!(refusal.contains("already links issue #5"));
        assert_eq!(task.external_refs.len(), 1);
    }

    #[test]
    fn attach_allows_another_issue_of_the_same_repository() {
        let mut task = create_test_task("two");
        attach_github_issue(
            &mut task,
            &parse_issue_url("https://github.com/o/r/issues/1").unwrap(),
            "OPEN",
        )
        .unwrap();
        attach_github_issue(
            &mut task,
            &parse_issue_url("https://github.com/o/r/issues/2").unwrap(),
            "CLOSED",
        )
        .unwrap();
        assert_eq!(task.external_refs.len(), 2);
    }
}
