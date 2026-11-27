// SPDX-License-Identifier: MPL-2.0
//! Reading one Task's PR Comment Review for the control channel
//! (`IpcRequest::GetPrCommentReview`).
//!
//! [`pr_comment_review`] is a pure function of a Task record: it takes the
//! record by shared reference and has no access to the analyzer, the agent
//! runner, the approval writers or storage, so a read cannot analyze, start
//! an agent, change an approval or edit the plan. It shows the same derived
//! delivery state the review dialog does: the lifecycle backfill from the
//! last Apply and the Task's PR-level reply records are applied to a copy of
//! the stored plan, never to the plan itself.

use slashit_ipc::pr_comment_review::{
    PrCommentReview, PrCommentReviewApply, PrCommentReviewComment, PrCommentReviewExecution,
    PrCommentReviewFinding, PrCommentReviewPlan, PrCommentReviewPublication,
};

use crate::domain::task::{PrCommentKind, PrReviewDecision, PrReviewPlan};
use crate::domain::Task;

/// One Task's PR Comment Review, as SlashIt holds it.
pub(crate) fn pr_comment_review(task: &Task) -> PrCommentReview {
    PrCommentReview {
        task_id: task.id.to_string(),
        task_title: task.title.clone(),
        task_pr_url: task.authoritative_pr_url().map(str::to_string),
        plan: task.pr_review_plan.as_ref().map(|stored| {
            let mut plan = stored.clone();
            plan.backfill_lifecycle_from_last_apply();
            super::apply_pr_reply_records(&mut plan, &task.pr_reply_records);
            plan_view(&plan)
        }),
    }
}

fn plan_view(plan: &PrReviewPlan) -> PrCommentReviewPlan {
    let identity = super::pull_request_identity(&plan.pr_url);
    let ids = finding_ids(plan);
    PrCommentReviewPlan {
        pr_url: plan.pr_url.clone(),
        repository: identity.as_ref().map(|(repo, _)| repo.clone()),
        pr_number: identity.map(|(_, number)| number),
        analyzed_head_oid: plan.analyzed_head_oid.clone(),
        analyzed_worktree_oid: plan.analyzed_worktree_oid.clone(),
        generated_at: plan.generated_at.to_rfc3339(),
        analyzed_at: plan.analyzed_at.map(|t| t.to_rfc3339()),
        review_decision: plan.review_decision.clone(),
        human_edit_revision: plan.human_edit_revision,
        findings: plan
            .items
            .iter()
            .zip(ids)
            .map(|(item, finding_id)| {
                let comment = item.comment_id.and_then(|id| plan.comment_for(id));
                let human_override = item
                    .proposed_change_override
                    .clone()
                    .filter(|text| !text.trim().is_empty());
                PrCommentReviewFinding {
                    finding_id,
                    comment_id: item.comment_id,
                    comment: comment.map(|c| PrCommentReviewComment {
                        kind: kind_name(&c.kind).to_string(),
                        author: c.author.clone(),
                        author_association: c.author_association.clone(),
                        body: c.body.clone(),
                        path: c.path.clone(),
                        line: c.line,
                        url: c.url.clone(),
                        created_at: c.created_at.map(|t| t.to_rfc3339()),
                        updated_at: c.updated_at.map(|t| t.to_rfc3339()),
                    }),
                    summary: item.summary.clone(),
                    reasoning: item.reasoning.clone(),
                    decision: decision_name(&item.decision).to_string(),
                    original_proposal: item.proposed_change.clone(),
                    human_override,
                    effective_instruction: item.effective_proposed_change().to_string(),
                    approved: item.approved,
                    selected_for_fix: item.approved && item.decision == PrReviewDecision::Fix,
                    user_note: Some(item.user_note.clone()).filter(|n| !n.trim().is_empty()),
                    execution: PrCommentReviewExecution {
                        fix_done: item.fix_done,
                        fix_uncommitted: item.fix_uncommitted,
                        fix_commit: item.fix_commit.clone(),
                        fix_before_tree: item.fix_effect.as_ref().map(|e| e.before_tree.clone()),
                        fix_after_tree: item.fix_effect.as_ref().map(|e| e.after_tree.clone()),
                        last_agent_summary: item.last_agent_summary.clone(),
                        last_error: item.last_error.clone(),
                    },
                    publication: PrCommentReviewPublication {
                        reply_posted: item.reply_posted,
                        reply_uncertain: item.reply_uncertain,
                        reply_comment_id: item.reply_comment_id,
                        reply_text: item.pr_reply_text.clone(),
                    },
                }
            })
            .collect(),
        last_apply: plan.last_apply.as_ref().map(|a| PrCommentReviewApply {
            applied_at: a.applied_at.to_rfc3339(),
            dry_run: a.dry_run,
            agent_summary: a.agent_summary.clone(),
            fixed_ids: a.fixed_ids.clone(),
            skipped_ids: a.skipped_ids.clone(),
            failed_ids: a.failed_ids.clone(),
            fix_errors: a.fix_errors.clone(),
            pushed: a.pushed,
            push_branch: a.push_branch.clone(),
            push_error: a.push_error.clone(),
            replies_posted: a.replies_posted,
            reply_errors: a.reply_errors.clone(),
        }),
    }
}

/// One id per item, in item order, derived only from the stored plan: the
/// comment's kind and GitHub id when the plan holds that comment once (kinds
/// number their comments separately, so the id alone is not an identity), the
/// bare id when it holds it ambiguously or not at all, and the item's 1-based
/// position when it names no comment. A repeat gets `#<n>` appended, so every
/// id is unique within the answer.
fn finding_ids(plan: &PrReviewPlan) -> Vec<String> {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    plan.items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let base = match item.comment_id {
                Some(id) => match plan.comment_for(id) {
                    Some(comment) => format!("{}:{id}", kind_name(&comment.kind)),
                    None => format!("comment:{id}"),
                },
                None => format!("item:{}", idx + 1),
            };
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                base
            } else {
                format!("{base}#{count}")
            }
        })
        .collect()
}

fn kind_name(kind: &PrCommentKind) -> &'static str {
    match kind {
        PrCommentKind::Inline => "inline",
        PrCommentKind::Review => "review",
        PrCommentKind::Conversation => "conversation",
    }
}

fn decision_name(decision: &PrReviewDecision) -> &'static str {
    match decision {
        PrReviewDecision::Fix => "fix",
        PrReviewDecision::Skip => "skip",
        PrReviewDecision::Question => "question",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::{FixEffect, PrReviewComment};
    use crate::test_helpers::create_test_pr_review_setup;

    fn fixture() -> Task {
        let (mut task, mut plan) = create_test_pr_review_setup();
        plan.analyzed_head_oid = Some("a".repeat(40));
        plan.items[0].proposed_change_override = Some("Delete it and its import.".into());
        plan.items[0].fix_done = true;
        plan.items[0].fix_commit = Some("c".repeat(40));
        plan.items[0].fix_effect = Some(FixEffect {
            before_tree: "1".repeat(40),
            after_tree: "2".repeat(40),
        });
        plan.items[0].reply_posted = true;
        plan.items[0].reply_comment_id = Some(9001);
        // A review comment that shares inline comment 101's number, and an
        // item with no comment, to pin the identity rules.
        plan.comments.push(PrReviewComment {
            id: Some(7),
            kind: PrCommentKind::Review,
            ..plan.comments[0].clone()
        });
        let mut review_item = plan.items[1].clone();
        review_item.comment_id = Some(7);
        plan.items.push(review_item);
        let mut loose = plan.items[1].clone();
        loose.comment_id = None;
        plan.items.push(loose.clone());
        plan.items.push(loose);
        task.pr_review_plan = Some(plan);
        task
    }

    /// Two reads of the same stored review name every finding alike, and the
    /// ids are unique and derived from the comment's identity.
    #[test]
    fn finding_ids_are_stable_across_reads() {
        let task = fixture();
        let first = pr_comment_review(&task);
        let second = pr_comment_review(&task);
        assert_eq!(first, second);
        let ids: Vec<_> = first
            .plan
            .as_ref()
            .unwrap()
            .findings
            .iter()
            .map(|f| f.finding_id.as_str())
            .collect();
        assert_eq!(ids, ["inline:101", "inline:102", "review:7", "item:4", "item:5"]);
        // A JSON round trip, which is what the CLI reads, keeps them.
        let wire = serde_json::to_value(&first).unwrap();
        let back: PrCommentReview = serde_json::from_value(wire).unwrap();
        assert_eq!(back, first);
    }

    #[test]
    fn ids_repeat_only_with_a_suffix() {
        let mut task = fixture();
        let plan = task.pr_review_plan.as_mut().unwrap();
        let again = plan.items[0].clone();
        plan.items.push(again);
        let view = pr_comment_review(&task);
        let last = view.plan.unwrap().findings.pop().unwrap();
        assert_eq!(last.finding_id, "inline:101#2");
    }

    #[test]
    fn a_finding_shows_proposal_override_and_evidence() {
        let task = fixture();
        let view = pr_comment_review(&task);
        assert_eq!(view.task_pr_url.as_deref(), Some("https://github.com/test-org/test-repo/pull/42"));
        let plan = view.plan.unwrap();
        assert_eq!(plan.pr_number, Some(42));
        assert_eq!(plan.repository.as_deref(), Some("test-org/test-repo"));
        assert_eq!(plan.analyzed_head_oid, Some("a".repeat(40)));
        let fix = &plan.findings[0];
        assert_eq!(fix.comment.as_ref().unwrap().body, "This variable is unused.");
        assert_eq!(fix.original_proposal, "Delete the variable.");
        assert_eq!(fix.human_override.as_deref(), Some("Delete it and its import."));
        assert_eq!(fix.effective_instruction, "Delete it and its import.");
        assert_eq!(fix.decision, "fix");
        assert!(fix.approved && fix.selected_for_fix);
        assert_eq!(fix.execution.fix_commit, Some("c".repeat(40)));
        assert_eq!(fix.execution.fix_after_tree, Some("2".repeat(40)));
        assert!(fix.publication.reply_posted);
        assert_eq!(fix.publication.reply_comment_id, Some(9001));
        let skip = &plan.findings[1];
        assert_eq!(skip.decision, "skip");
        assert!(!skip.approved && !skip.selected_for_fix);
        assert_eq!(skip.human_override, None);
    }

    #[test]
    fn a_task_never_analyzed_has_no_plan() {
        let (task, _) = create_test_pr_review_setup();
        assert!(pr_comment_review(&task).plan.is_none());
    }
}
