// SPDX-License-Identifier: MPL-2.0
//! `slashit pr-review`: a Task's PR Comment Review, printed as a summary or
//! exported as Markdown. Both are rendered here from the instance's answer
//! ([`PrCommentReview`]); the instance itself only reads.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use slashit_ipc::pr_comment_review::{PrCommentReview, PrCommentReviewFinding};

use crate::output::escape_terminal;

/// A short summary for a terminal: one block per finding.
pub fn summary_text(review: &PrCommentReview) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Task:  {} {}", review.task_id, escape_terminal(&review.task_title));
    let check = &review.head_check;
    let _ = write!(out, "Status: {}", check.state.as_str());
    if let Some(oid) = &check.observed_head_oid {
        let _ = write!(out, " (remote head {})", escape_terminal(oid));
    }
    if let Some(reason) = &check.reason {
        let _ = write!(out, " ({})", escape_terminal(&one_line(reason)));
    }
    let _ = writeln!(out);
    let Some(plan) = &review.plan else {
        let _ = writeln!(out, "No PR Comment Review has been analyzed for this Task.");
        return out;
    };
    let _ = writeln!(out, "PR:    {}", escape_terminal(&plan.pr_url));
    let _ = writeln!(out, "Head:  {}", plan.analyzed_head_oid.as_deref().unwrap_or("unknown"));
    let _ = writeln!(out, "Findings: {}", plan.findings.len());
    for f in &plan.findings {
        let _ = writeln!(out);
        let comment_id = f.comment_id.map_or_else(|| "none".to_string(), |id| id.to_string());
        let _ = writeln!(
            out,
            "  {}  comment {comment_id}  {}  {}",
            escape_terminal(&f.finding_id),
            f.decision,
            state_label(f)
        );
        let _ = writeln!(out, "    {}", escape_terminal(&one_line(&f.summary)));
        if f.human_override.is_some() {
            let _ = writeln!(out, "    (Human override in effect)");
        }
    }
    out
}

fn state_label(f: &PrCommentReviewFinding) -> String {
    let mut parts = vec![if f.approved { "approved" } else { "not approved" }];
    if f.execution.fix_done {
        parts.push("fixed");
    }
    if f.execution.last_error.is_some() {
        parts.push("failed");
    }
    if f.publication.reply_posted {
        parts.push("replied");
    } else if f.publication.reply_uncertain {
        parts.push("reply uncertain");
    }
    parts.join(", ")
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The review as a Markdown document. Text that came from reviewers or
/// agents is put in fenced blocks, so it cannot change the document's
/// structure.
pub fn markdown(review: &PrCommentReview) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# PR Comment Review: {}", one_line(&review.task_title));
    let _ = writeln!(out);
    let _ = writeln!(out, "- Task: `{}`", review.task_id);
    if let Some(url) = &review.task_pr_url {
        let _ = writeln!(out, "- Task pull request: <{url}>");
    }
    let _ = writeln!(out, "- Head check: {}", review.head_check.state.as_str());
    if let Some(oid) = &review.head_check.observed_head_oid {
        let _ = writeln!(out, "- Observed remote head: `{oid}`");
    }
    let Some(plan) = &review.plan else {
        let _ = writeln!(out);
        let _ = writeln!(out, "No PR Comment Review has been analyzed for this Task.");
        return out;
    };
    let _ = writeln!(out, "- Analyzed pull request: <{}>", plan.pr_url);
    if let (Some(repo), Some(number)) = (&plan.repository, plan.pr_number) {
        let _ = writeln!(out, "- Pull request: {repo}#{number}");
    }
    let _ = writeln!(out, "- Head revision: {}", code_or_unknown(plan.analyzed_head_oid.as_deref()));
    let _ = writeln!(out, "- Checkout revision: {}", code_or_unknown(plan.analyzed_worktree_oid.as_deref()));
    let _ = writeln!(out, "- Generated: {}", plan.generated_at);
    if let Some(at) = &plan.analyzed_at {
        let _ = writeln!(out, "- Analyzed: {at}");
    }
    if let Some(decision) = &plan.review_decision {
        let _ = writeln!(out, "- GitHub review decision: {decision}");
    }
    let _ = writeln!(out, "- Findings: {}", plan.findings.len());

    for f in &plan.findings {
        let _ = writeln!(out);
        let _ = writeln!(out, "## `{}`: {}", f.finding_id, one_line(&f.summary));
        let _ = writeln!(out);
        let _ = writeln!(out, "- Decision: {}", f.decision);
        let _ = writeln!(out, "- Approved: {}", yes_no(f.approved));
        let _ = writeln!(out, "- Selected for fix: {}", yes_no(f.selected_for_fix));
        if let Some(id) = f.comment_id.filter(|_| f.comment.is_some()) {
            let _ = writeln!(out, "- Comment id: {id}");
        }
        if let Some(c) = &f.comment {
            let mut author = format!("- Reviewer: {} ({}", one_line(&c.author), c.kind);
            if let Some(assoc) = &c.author_association {
                let _ = write!(author, ", {assoc}");
            }
            author.push(')');
            let _ = writeln!(out, "{author}");
            if let Some(path) = &c.path {
                let line = c.line.map(|l| format!(":{l}")).unwrap_or_default();
                let _ = writeln!(out, "- Location: `{}{line}`", one_line(path));
            }
            if let Some(url) = &c.url {
                let _ = writeln!(out, "- Comment: <{url}>");
            }
            section(&mut out, "Reviewer comment", &c.body);
        } else if let Some(id) = f.comment_id {
            let _ = writeln!(out, "- Comment id: {id} (not held uniquely by this analysis)");
        }
        section(&mut out, "Agent reasoning", &f.reasoning);
        section(&mut out, "Original proposal", &f.original_proposal);
        if let Some(text) = &f.human_override {
            section(&mut out, "Human override", text);
        }
        section(&mut out, "Effective instruction", &f.effective_instruction);
        if let Some(note) = &f.user_note {
            section(&mut out, "Human note", note);
        }

        let e = &f.execution;
        let _ = writeln!(out);
        let _ = writeln!(out, "### Execution");
        let _ = writeln!(out);
        let _ = writeln!(out, "- Fix done: {}", yes_no(e.fix_done));
        let _ = writeln!(out, "- Fix uncommitted: {}", yes_no(e.fix_uncommitted));
        let _ = writeln!(out, "- Fix commit: {}", code_or_unknown(e.fix_commit.as_deref()));
        if let (Some(before), Some(after)) = (&e.fix_before_tree, &e.fix_after_tree) {
            let _ = writeln!(out, "- Fix effect: `{before}` → `{after}`");
        }
        if let Some(s) = &e.last_agent_summary {
            section(&mut out, "Agent summary", s);
        }
        if let Some(err) = &e.last_error {
            section(&mut out, "Last error", err);
        }

        let p = &f.publication;
        let _ = writeln!(out);
        let _ = writeln!(out, "### Publication");
        let _ = writeln!(out);
        let _ = writeln!(out, "- Reply posted: {}", yes_no(p.reply_posted));
        if p.reply_uncertain {
            let _ = writeln!(out, "- Reply uncertain: yes");
        }
        if let Some(id) = p.reply_comment_id {
            let _ = writeln!(out, "- Reply comment id: {id}");
        }
        if let Some(text) = &p.reply_text {
            section(&mut out, "Reply text", text);
        }
    }

    if let Some(a) = &plan.last_apply {
        let _ = writeln!(out);
        let _ = writeln!(out, "## Last apply");
        let _ = writeln!(out);
        let _ = writeln!(out, "- Applied: {}{}", a.applied_at, if a.dry_run { " (dry run)" } else { "" });
        let _ = writeln!(out, "- Fixed: {:?}", a.fixed_ids);
        let _ = writeln!(out, "- Skipped: {:?}", a.skipped_ids);
        let _ = writeln!(out, "- Failed: {:?}", a.failed_ids);
        let _ = writeln!(out, "- Pushed: {}", yes_no(a.pushed));
        if let Some(branch) = &a.push_branch {
            let _ = writeln!(out, "- Push branch: `{}`", one_line(branch));
        }
        let _ = writeln!(out, "- Replies posted: {}", a.replies_posted);
        let mut errors: Vec<&str> = a.fix_errors.iter().map(String::as_str).collect();
        errors.extend(a.push_error.as_deref());
        errors.extend(a.reply_errors.iter().map(String::as_str));
        if !errors.is_empty() {
            section(&mut out, "Errors", &errors.join("\n"));
        }
        section(&mut out, "Agent summary", &a.agent_summary);
    }
    out
}

/// Write `markdown` to a new file at `path`. An existing path is never
/// replaced.
pub fn write_new_file(path: &Path, markdown: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(markdown.as_bytes())?;
    file.sync_all()
}

fn section(out: &mut String, title: &str, body: &str) {
    if body.trim().is_empty() {
        return;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "**{title}:**");
    let _ = writeln!(out);
    let fence = fence_for(body);
    let _ = writeln!(out, "{fence}text");
    let _ = writeln!(out, "{}", body.trim_end());
    let _ = writeln!(out, "{fence}");
}

/// A backtick fence longer than any backtick run in `body`.
fn fence_for(body: &str) -> String {
    let longest = body
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn code_or_unknown(value: Option<&str>) -> String {
    value.map_or_else(|| "unknown".to_string(), |v| format!("`{v}`"))
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slashit_ipc::pr_comment_review::*;

    fn review() -> PrCommentReview {
        PrCommentReview {
            task_id: "6f96fefa-0000-0000-0000-000000000003".into(),
            task_title: "Fix login".into(),
            task_pr_url: Some("https://github.com/o/r/pull/7".into()),
            head_check: PrCommentReviewHeadCheck {
                state: PrCommentReviewHeadState::Stale,
                observed_head_oid: Some("b".repeat(40)),
                reason: None,
            },
            plan: Some(PrCommentReviewPlan {
                pr_url: "https://github.com/o/r/pull/7".into(),
                repository: Some("o/r".into()),
                pr_number: Some(7),
                analyzed_head_oid: Some("a".repeat(40)),
                analyzed_worktree_oid: None,
                generated_at: "2026-11-27T00:00:00+00:00".into(),
                analyzed_at: None,
                review_decision: None,
                human_edit_revision: 1,
                findings: vec![PrCommentReviewFinding {
                    finding_id: "inline:101".into(),
                    comment_id: Some(101),
                    comment: Some(PrCommentReviewComment {
                        kind: "inline".into(),
                        author: "rev".into(),
                        author_association: Some("MEMBER".into()),
                        body: "Use ```this``` instead\n# not a heading".into(),
                        path: Some("src/lib.rs".into()),
                        line: Some(3),
                        url: None,
                        created_at: None,
                        updated_at: None,
                    }),
                    summary: "Rename".into(),
                    reasoning: "Clearer.".into(),
                    decision: "fix".into(),
                    original_proposal: "Rename it.".into(),
                    human_override: Some("Rename it to x.".into()),
                    effective_instruction: "Rename it to x.".into(),
                    approved: true,
                    selected_for_fix: true,
                    user_note: None,
                    execution: PrCommentReviewExecution {
                        fix_done: true,
                        fix_uncommitted: false,
                        fix_commit: Some("c".repeat(40)),
                        fix_before_tree: None,
                        fix_after_tree: None,
                        last_agent_summary: None,
                        last_error: None,
                    },
                    publication: PrCommentReviewPublication {
                        reply_posted: false,
                        reply_uncertain: false,
                        reply_comment_id: None,
                        reply_text: None,
                    },
                }],
                last_apply: None,
            }),
        }
    }

    #[test]
    fn markdown_carries_identity_proposal_override_and_evidence() {
        let md = markdown(&review());
        for expected in [
            "- Pull request: o/r#7",
            &format!("- Head revision: `{}`", "a".repeat(40)),
            "## `inline:101`: Rename",
            "**Original proposal:**",
            "**Human override:**",
            "Rename it to x.",
            &format!("- Fix commit: `{}`", "c".repeat(40)),
            "- Reply posted: no",
            "- Head check: stale",
            "- Comment id: 101",
        ] {
            assert!(md.contains(expected), "missing {expected:?} in:\n{md}");
        }
    }

    #[test]
    fn the_summary_shows_the_head_check_and_both_finding_ids() {
        let text = summary_text(&review());
        let status = format!("Status: stale (remote head {})", "b".repeat(40));
        assert!(text.contains(&status), "{text}");
        assert!(text.contains("inline:101  comment 101  fix"), "{text}");
    }

    /// A reviewer's backticks and headings stay inside their block.
    #[test]
    fn reviewer_text_is_fenced_beyond_its_own_backticks() {
        let md = markdown(&review());
        assert!(md.contains("````text\nUse ```this``` instead\n# not a heading\n````"), "{md}");
    }

    #[test]
    fn an_unanalyzed_task_says_so() {
        let mut r = review();
        r.plan = None;
        assert!(markdown(&r).contains("No PR Comment Review has been analyzed"));
        assert!(summary_text(&r).contains("No PR Comment Review has been analyzed"));
    }

    #[test]
    fn the_export_never_replaces_a_file() {
        // Under the workspace's own ignored build directory.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-scratch")
            .join(format!("slashit-pr-review-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("review.md");
        let _ = std::fs::remove_file(&path);
        write_new_file(&path, "first").unwrap();
        assert!(write_new_file(&path, "second").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
