// SPDX-License-Identifier: MPL-2.0
//! Analyze builds the PR Comment Review from GitHub and the triage helper.
//!
//! A Task with no stored review plan is analyzed through the rendered menu, the
//! path a person takes. Analyze reads the pull request's reviews, conversation
//! comments, inline comments and review threads from the fake `gh`, runs the
//! triage helper once for the collaborators' comments and once for everyone
//! else's, and stores the plan it parses. The journey checks the requests GitHub
//! received, the comment identities and items the stored plan keeps, and the
//! approval panel. It then restarts: reopening shows the stored plan and runs no
//! analysis. No public write is made. Replies are published by
//! `pr_comment_review`.

use super::human_review::{approve_and_open_the_pull_request, install_fakes};
use super::pr_comment_review::{
    ERROR, ITEM, ITEM_APPROVE, POST, REPLY, PR_NUMBER, REPOSITORY, await_count, board_plan, disabled, is_checked,
    item_part, open_dialog, publishes,
};
use super::*;
use slashit_acceptance::fake_agent::FakeAgent;
use slashit_acceptance::fake_gh::{FakeGh, ScriptedRead};

/// The findings the triage helper returns and the GitHub data behind them: one
/// review, one conversation comment, and three inline comments. The inline
/// comment in a resolved thread is not a finding, so it must not be listed.
const COLLABORATOR_INLINE: u64 = 2001;
const CONTRIBUTOR_INLINE: u64 = 2002;
const RESOLVED_INLINE: u64 = 2003;
const CONVERSATION: u64 = 3001;
const REVIEW: u64 = 4001;

const COLLABORATOR_TEXT: &str = "Rename this helper.";
const CONTRIBUTOR_TEXT: &str = "The error message is unclear.";
const RESOLVED_TEXT: &str = "Nit: trailing whitespace.";

/// Items the dialog shows after Analyze: one per finding above, none for the
/// resolved thread.
const ITEMS: usize = 4;

#[tokio::test(flavor = "multi_thread")]
async fn analyze_builds_the_review_plan_from_github_and_the_triage_helper() {
    let context = TestContext::new("pr_comment_analyze").expect("harness setup");
    let outcome = journey(&context).await;
    context.finish(outcome);
}

async fn journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    // Real work, so the branch holds a commit the pull request can be opened at.
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.point_origin_at_github()?;
    repository.serve_github_from_origin()?;

    let session = context.start_session("opened").await?;
    let outcome = approve_and_open_the_pull_request(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "opened", &outcome).await?;
    let (executed, pr_url) = outcome?;
    script_github(&gh, &pr_url)?;
    script_triage(&agent)?;

    let session = context.start_session("analyzed").await?;
    let outcome = analyzed(session.driver(), &agent, &gh, &executed, &root).await;
    context.close_session(session, "analyzed", &outcome).await?;
    outcome?;

    let session = context.start_session("reopened").await?;
    let outcome = reopened(session.driver(), &agent, &gh, &executed).await;
    context.close_session(session, "reopened", &outcome).await?;
    outcome
}

/// What GitHub answers for the pull request, scripted the way its API does: the
/// `gh pr view` answer names reviews and comments by node id, and the REST
/// lists give their numeric ids under the same node ids.
fn script_github(gh: &FakeGh, pr_url: &str) -> Result<()> {
    let head = pr_head(gh)?;
    let at = |fragment: &str| format!("{pr_url}{fragment}");
    gh.script_pr_view_of(
        1,
        &json!({
            "state": "OPEN",
            "number": 1,
            "mergeable": "MERGEABLE",
            "reviewDecision": "REVIEW_REQUIRED",
            "statusCheckRollup": [],
            "baseRefName": "main",
            "isCrossRepository": false,
            "headRefOid": head,
            "reviews": [{
                "id": "PRR_4001",
                "author": {"login": "reviewer-one"},
                "authorAssociation": "MEMBER",
                "body": "Overall, please rename the helper.",
                "state": "COMMENTED",
                "submittedAt": "2026-01-01T00:00:00Z",
                "url": at("#pullrequestreview-4001"),
            }],
            "comments": [{
                "id": "IC_3001",
                "author": {"login": "reviewer-two"},
                "authorAssociation": "CONTRIBUTOR",
                "body": "Could you add a changelog entry?",
                "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
                "url": at("#issuecomment-3001"),
            }],
        }),
    )?;
    gh.script_read(
        ScriptedRead::PullReviews,
        &json!([{
            "id": 4001,
            "node_id": "PRR_4001",
            "user": {"login": "reviewer-one"},
            "author_association": "MEMBER",
            "body": "Overall, please rename the helper.",
            "state": "COMMENTED",
        }]),
    )?;
    gh.script_read(
        ScriptedRead::IssueComments,
        &json!([{
            "id": 3001,
            "node_id": "IC_3001",
            "user": {"login": "reviewer-two"},
            "author_association": "CONTRIBUTOR",
            "body": "Could you add a changelog entry?",
            "html_url": at("#issuecomment-3001"),
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }]),
    )?;
    let inline = |id: u64, login: &str, association: &str, body: &str| {
        json!({
            "id": id,
            "node_id": format!("PRRC_{id}"),
            "user": {"login": login},
            "author_association": association,
            "body": body,
            "path": "src/lib.rs",
            "line": 10,
            "html_url": at(&format!("#discussion_r{id}")),
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        })
    };
    gh.script_read(
        ScriptedRead::PullComments,
        &json!([
            inline(COLLABORATOR_INLINE, "reviewer-one", "MEMBER", COLLABORATOR_TEXT),
            inline(CONTRIBUTOR_INLINE, "reviewer-two", "CONTRIBUTOR", CONTRIBUTOR_TEXT),
            inline(RESOLVED_INLINE, "outside-contributor", "CONTRIBUTOR", RESOLVED_TEXT),
        ]),
    )?;
    // The thread of the resolved comment is the only one resolved, so only its
    // comment is left out of the findings.
    let thread = |id: u64, resolved: bool| {
        json!({
            "isResolved": resolved,
            "isOutdated": false,
            "comments": {"totalCount": 1, "nodes": [{"fullDatabaseId": id.to_string()}]},
        })
    };
    gh.script_read(
        ScriptedRead::ReviewThreads,
        &json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                thread(COLLABORATOR_INLINE, false),
                thread(CONTRIBUTOR_INLINE, false),
                thread(RESOLVED_INLINE, true),
            ],
        }}}}}),
    )
}

/// The triage the helper returns, in the single-line JSON its prompt asks for.
/// Each run keeps only the items for its own comments, so one answer serves both.
fn script_triage(agent: &FakeAgent) -> Result<()> {
    let answer = json!({"items": [
        {"comment_id": COLLABORATOR_INLINE, "summary": "Rename the helper", "decision": "fix",
         "reasoning": "The name does not say what the helper does.", "proposed_change": "Rename the helper."},
        {"comment_id": CONTRIBUTOR_INLINE, "summary": "Clarify the error message", "decision": "fix",
         "reasoning": "The message does not name the value that failed.", "proposed_change": "Name the value in the message."},
        {"comment_id": CONVERSATION, "summary": "Ask about the changelog", "decision": "question",
         "reasoning": "Which release should the entry go under?", "proposed_change": ""},
        {"comment_id": REVIEW, "summary": "Overall review", "decision": "skip",
         "reasoning": "The inline comments already cover it.", "proposed_change": ""},
    ]});
    // A directory, because the harness reads every file in the marker directory
    // as an invocation record.
    let directory = agent.marker_dir().join(".review-triage");
    std::fs::create_dir_all(&directory).with_context(|| format!("could not create {}", directory.display()))?;
    let path = directory.join("answer");
    std::fs::write(&path, answer.to_string()).with_context(|| format!("could not write {}", path.display()))
}

/// The head the pull request was opened at, which GitHub reports as its head.
fn pr_head(gh: &FakeGh) -> Result<String> {
    let path = gh.dir().join("pr-heads").join(PR_NUMBER);
    Ok(std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?
        .trim()
        .to_string())
}

/// The triage prompts the helper was given, one per batch.
fn triage_prompts(agent: &FakeAgent) -> Result<Vec<String>> {
    Ok(agent
        .invocations()?
        .into_iter()
        .filter_map(|invocation| invocation.prompt)
        .filter(|prompt| prompt.starts_with("# PR Review Triage"))
        .collect())
}

/// How many calls `gh` made to the reads Analyze makes for the reviews.
fn review_reads(gh: &FakeGh) -> Result<usize> {
    let endpoint = format!("repos/{REPOSITORY}/pulls/{PR_NUMBER}/reviews?per_page=100");
    Ok(gh.invocations()?.iter().filter(|args| args.contains(&endpoint)).count())
}

fn item_row(comment: u64) -> String {
    format!("{ITEM}[data-comment-id=\"{comment}\"]")
}

/// Waits for the analyzed items. A visible error ends the wait at once, with its
/// text, so a failed analysis is never mistaken for a slow one.
async fn await_analysis(driver: &WebDriver) -> Result<()> {
    let started = Instant::now();
    loop {
        if count(driver, ERROR).await? > 0 {
            let shown = text_of(driver, ERROR).await?.unwrap_or_default();
            bail!("Analyze failed: {shown:?}");
        }
        let found = count(driver, ITEM).await?;
        if found == ITEMS {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the analysis shows {found} items, expected {ITEMS}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn analyzed(
    driver: &WebDriver,
    agent: &FakeAgent,
    gh: &FakeGh,
    executed: &ExecutedTask,
    root: &Path,
) -> Result<()> {
    // Nothing is seeded: a plan already on the board would be shown, not analyzed.
    if board_plan(root, &executed.id).is_ok() {
        bail!("the Task holds a review plan before Analyze ran");
    }
    let before = gh.invocations()?.len();
    open_dialog(driver, executed).await?;
    await_analysis(driver).await?;

    // Analyze asked GitHub for every list it needs, before triage ran.
    let reads = gh.invocations()?;
    for endpoint in [
        format!("repos/{REPOSITORY}/pulls/{PR_NUMBER}/reviews?per_page=100"),
        format!("repos/{REPOSITORY}/issues/{PR_NUMBER}/comments?per_page=100"),
        format!("repos/{REPOSITORY}/pulls/{PR_NUMBER}/comments?per_page=100"),
    ] {
        if !reads.iter().any(|args| args.contains(&endpoint)) {
            bail!("Analyze did not read {endpoint}");
        }
    }
    if !reads.iter().any(|args| args.iter().any(|arg| arg == "graphql")) {
        bail!("Analyze did not read the review threads");
    }

    // One triage run per batch: the collaborators' comments, then everyone else's.
    let prompts = triage_prompts(agent)?;
    if prompts.len() != 2 {
        bail!("Analyze ran the triage helper {} times, expected one run per batch", prompts.len());
    }
    let collaborators = prompts
        .iter()
        .find(|prompt| prompt.contains(COLLABORATOR_TEXT))
        .context("no triage run was given the collaborators' comments")?;
    if collaborators.contains(CONTRIBUTOR_TEXT) {
        bail!("the collaborators' triage run was given a contributor's comment");
    }

    // The stored plan holds each comment under its own kind and numeric id, and
    // one item per finding, approved only where a collaborator wrote the comment.
    let plan = board_plan(root, &executed.id)?;
    let head = pr_head(gh)?;
    if plan["analyzed_head_oid"].as_str() != Some(head.as_str()) {
        bail!("the plan is not bound to the pull request head {head}: {}", plan["analyzed_head_oid"]);
    }
    let mut comments = listed(&plan, "comments", |c| (c["id"].as_u64(), text(&c["kind"]), text(&c["author_association"])))?;
    let mut expected_comments: Vec<_> = [
        (COLLABORATOR_INLINE, "inline", "MEMBER"),
        (CONTRIBUTOR_INLINE, "inline", "CONTRIBUTOR"),
        (CONVERSATION, "conversation", "CONTRIBUTOR"),
        (REVIEW, "review", "MEMBER"),
    ]
    .into_iter()
    .map(|(id, kind, association)| (Some(id), Some(kind.to_string()), Some(association.to_string())))
    .collect();
    comments.sort();
    expected_comments.sort();
    if comments != expected_comments {
        bail!("the stored plan lists the comments {comments:?}, expected {expected_comments:?}");
    }
    let mut items = listed(&plan, "items", |i| (i["comment_id"].as_u64(), text(&i["decision"]), i["approved"].as_bool()))?;
    let mut expected_items: Vec<_> = [
        (COLLABORATOR_INLINE, "fix", true),
        (CONTRIBUTOR_INLINE, "fix", false),
        (CONVERSATION, "question", false),
        (REVIEW, "skip", false),
    ]
    .into_iter()
    .map(|(id, decision, approved)| (Some(id), Some(decision.to_string()), Some(approved)))
    .collect();
    items.sort();
    expected_items.sort();
    if items != expected_items {
        bail!("the stored plan's items are {items:?}, expected {expected_items:?}");
    }

    // The dialog shows the analyzed plan: the collaborator's fix is pre-approved,
    // the contributor's is not, and no reply exists before a fix is applied.
    if !is_checked(driver, &item_part(COLLABORATOR_INLINE, ITEM_APPROVE)).await? {
        bail!("the collaborator's fix is not approved in the analyzed plan");
    }
    if is_checked(driver, &item_part(CONTRIBUTOR_INLINE, ITEM_APPROVE)).await? {
        bail!("the contributor's fix is shown approved");
    }
    if count(driver, &item_row(RESOLVED_INLINE)).await? != 0 {
        bail!("the comment in a resolved thread is listed as a finding");
    }
    if count(driver, REPLY).await? != 0 {
        bail!("a reply is offered before any fix was applied");
    }
    if !disabled(driver, POST).await? {
        bail!("Post is enabled with nothing selected");
    }

    // Analyze reads and triages. It publishes nothing.
    if !gh.posts()?.is_empty() {
        bail!("Analyze published a reply: {:?}", gh.posts()?);
    }
    if gh.invocations()?.iter().skip(before).any(|args| publishes(args)) {
        bail!("Analyze made a write to GitHub");
    }
    Ok(())
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// Reads one list of the stored plan, mapping each entry with `key`. A plan
/// without the list is an error, not an empty result.
fn listed<T>(plan: &Value, field: &str, key: impl Fn(&Value) -> T) -> Result<Vec<T>> {
    Ok(plan[field]
        .as_array()
        .with_context(|| format!("the stored plan has no {field} list"))?
        .iter()
        .map(key)
        .collect())
}

/// After a restart the stored plan is shown as it was analyzed, and nothing runs
/// again: no triage run and no review read.
async fn reopened(driver: &WebDriver, agent: &FakeAgent, gh: &FakeGh, executed: &ExecutedTask) -> Result<()> {
    let triage_before = triage_prompts(agent)?.len();
    let reviews_before = review_reads(gh)?;
    open_dialog(driver, executed).await?;
    await_count(driver, ITEM, ITEMS).await?;
    if !is_checked(driver, &item_part(COLLABORATOR_INLINE, ITEM_APPROVE)).await? {
        bail!("the analyzed approval of the collaborator's fix did not survive a restart");
    }
    if triage_prompts(agent)?.len() != triage_before {
        bail!("reopening the stored plan ran the triage helper again");
    }
    if review_reads(gh)? != reviews_before {
        bail!("reopening the stored plan read the reviews from GitHub again");
    }
    if !gh.posts()?.is_empty() {
        bail!("reopening the stored plan published a reply: {:?}", gh.posts()?);
    }
    Ok(())
}
