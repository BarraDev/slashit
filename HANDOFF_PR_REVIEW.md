# Handoff: PR Review / Apply / Reply Flow

## Restart Prompt (paste at start of next session)

Continue the SlashIt PR review work. Read `HANDOFF_PR_REVIEW.md`. The
disappearing-modal bug is fixed at the root cause; the review flow is now
structured (per-comment decisions, Approve/Fix/Skip/Question, optional
auto-push and per-comment GitHub reply). End-to-end manual test against
`dictmagic/dictmagic-app` PR #60 (has 18 copilot reviewer comments) is the
remaining verification step. Watch backend logs in tmux pane
`slashit-app:1.3` for `[pr-review]` lines.

## What is built

### Bug: PR Review modal vanished mid-analysis (FIXED, root cause)

`pages/dashboard.rs` wrapped `<Kanban tasks=tasks.get()/>` in a reactive
`move ||` block. Any `tasks` update — auto-refresh-on-mount, the 5s
polling Effect, `on_pr_created` — re-ran the block, recreated the
`Kanban` component, and reset every internal `RwSignal::new(...)`
including `show_pr_review_modal`. The modal disappeared without a toast
and without ever rendering its error banner.

Fix:

- `Kanban` now takes `tasks: ReadSignal<Vec<Task>>` and
  `set_tasks: WriteSignal<Vec<Task>>` props (`components/kanban.rs:107-117`).
- `Dashboard` passes the signals directly (`pages/dashboard.rs:149-154`).
- `auto-refresh-on-mount` only fires `set_tasks_signal.update(...)` when
  `status` or `external_refs` actually changed
  (`components/kanban.rs:240-272`).
- `PrReviewModal` backdrop only closes when `!loading && !applying`.

### Structured review (PrReviewPlan)

Backend `domain/task.rs` adds:

- `PrReviewPlan { generated_at, pr_url, review_decision, comments,
  items, raw_plan, last_apply }` persisted on `Task.pr_review_plan`.
- `PrReviewComment { id, kind: Inline | Review | Conversation, author,
  body, path, line, url }`.
- `PrReviewItem { comment_id, summary, decision: Fix | Skip | Question,
  reasoning, proposed_change, approved }`.
- `PrReviewApplyResult { applied_at, agent_summary, fixed_ids,
  skipped_ids, pushed, push_branch, replies_posted, reply_errors }`.

Frontend `models/task.rs` mirrors these (`PrReviewDecisionKind` instead
of `PrReviewDecision` to avoid name conflicts).

### `analyze_pr_comments` → structured plan

`commands/pr.rs`:

- `fetch_pr_review_data` parses GitHub responses into structured
  `PrReviewComment`s (review-level + conversation + inline with
  path/line/id from `gh api repos/{repo}/pulls/{n}/comments`).
- Triage prompt demands strict JSON; `parse_review_items` extracts the
  first `{...}` block. On parse failure the modal shows the raw output.
- Empty plans (no comments) are NOT cached so reviewer comments arriving
  later trigger a fresh fetch.
- `eprintln!("[pr-review] analyze...")`, `... N comments fetched`,
  `... triage output: M chars`, `... parsed K items` go to stderr → tmux
  pane `slashit-app:1.3`.

### `address_pr_review` → apply + push + reply

Replaces the old free-form `address_pr_comments`.

Inputs: `task_id`, the edited `PrReviewPlan`, and
`AddressPrReviewOptions { auto_push: bool, auto_reply: bool }`.

Flow:

1. Filter `items` to those with `approved=true && decision=Fix`. Empty
   approved list → error.
2. Build a fix prompt that lists only the approved items + their
   `proposed_change` and `reasoning`. Run `claude` with edit tools.
3. `jj describe -m "task: ... (PR review fixes)"` + `jj git export`.
4. If `auto_push`: `push_branch(...)` (jj-aware, falls back to git push).
5. If `auto_reply`, for each approved item:
   - `comment_id` set → `gh api -X POST repos/{repo}/pulls/{n}/comments/{id}/replies -f body=...`.
   - Inline reply failed or no `comment_id` → `gh pr comment {url} --body ...`.
   - Reply text built from item: `[SlashIt agent — Fixed]\n\n{summary}\n\n{reasoning}\n\nChange: {proposed_change}`.
6. Save `last_apply` on `plan.pr_review_plan` so the modal's banner can
   show fixed/pushed/replies_posted/errors after the call returns.

### Modal UX

- Header: `PR Comment Review` + review-decision badge (`APPROVED`,
  `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, `Commented`).
- Collapsible "PR comments (N)" panel with raw bodies (kind, author,
  location).
- Per-item card:
  - checkbox **Approve** (defaults to true for `Fix`).
  - dropdown Fix / Skip / Question (changing decision auto-flips
    Approve).
  - editable `reasoning` (this is what gets posted as the reply on
    GitHub).
  - editable `proposed_change`.
- Footer toggles: Auto-push branch, Auto-reply on PR. Buttons:
  Re-analyze (force re-fetch), Close, Apply N approved.
- After apply: banner shows summary; PR state is refreshed automatically
  so the card moves to Done if the reviewer reacts.
- Empty state: `"No reviewer comments yet"` with a one-line explanation
  if `comments.is_empty()`.
- Stale empty caches (from before the "don't cache empty" fix) are
  ignored on open via `has_useful_cache` check.

## Real test cases

- `dictmagic/dictmagic-app` PR #60 (Bootstrap auth) — 18 inline
  comments from `copilot-pull-request-reviewer`. **Use this for the
  end-to-end test.**
- `dictmagic/dictmagic-app` PR #61 (Referral & Rewards System) —
  genuinely empty (verified via `gh` and GraphQL). Useful for the empty
  path.
- Local repo: `/home/ruiandrada/Repo/dictmagic/dictmagic-app` (jj +
  git colocated).
- The Referral task storage at
  `/home/ruiandrada/.config/slashit-app/tasks/b203c9b8-a0ae-404d-b41d-8dd6fdf95cce.toml`
  still has a leftover empty `pr_review_plan` from before the fix. The
  frontend ignores it, but it persists on disk until the next analyze
  writes a non-empty plan.

## Where logs land

`cargo tauri dev` runs in tmux pane `slashit-app:1.3`. Backend
`eprintln!` prefixes with `[pr-review]` for analyze + apply paths:

```
tmux capture-pane -t slashit-app:1.3 -p -S -200
```

Other slashit-app panes (1.1, 1.2) host claude sessions, not app
output.

## Verification

- `cargo check` (frontend) and
  `cargo check --manifest-path src-tauri/Cargo.toml` both pass with
  only pre-existing dead-code warnings.
- `cargo clippy` clean for changes introduced in this work; remaining
  warnings are unrelated.

## Open follow-ups

1. **End-to-end on PR #60**: open task, click `Review PR`, confirm modal
   shows the 18 copilot items, approve a Fix, click `Apply N approved`,
   verify push and replies posted. Watch `[pr-review]` lines in pane
   1.3.
2. **gh permissions**: `gh api .../comments/{id}/replies` needs PR-write
   scope. If it fails the code falls back to `gh pr comment`. Verify
   which path is taken on the user's token.
3. **Empty cache on disk** for the Referral task TOML: cosmetically
   ugly. Could be cleaned by writing a one-shot migration that drops
   `pr_review_plan` rows where `comments=[] && items=[]`.
4. **Per-item reply without agent run**: useful for Skip/Question items
   where the user just wants to post a reasoning back to the reviewer.
5. **Surface raw triage output even on success**: currently only shown
   when JSON parsing fails — could help debug parser misses.
6. Tray/AppIndicator quirks on Linux remain (separate issue).

## Other unchanged context

### GH007 private-email recovery

`get_pr_push_recovery` and `recover_private_email_and_create_pr` are
JJ-aware: `jj config set --repo user.email`, `jj metaedit -r <branch>
--author "Name <email>"`, `jj git export`, then retry push. Git
fallback rewrites only the branch tip with `git commit-tree` +
`git update-ref` guarded by the original SHA.

### PR Open column + auto Done on MERGED

- 8-column kanban with explicit `PR Open` between `Human Review` and
  `Done`.
- `link_pr_to_task` calls `fetch_pr_state`; if remote is `MERGED` the
  task auto-promotes to `Done`.
- `refresh_task_pr_state` Tauri command + per-card `Refresh` button.
- Auto-refresh-on-mount loop bubbles externally-merged PRs to Done
  without needing user action.

### Tray (Linux/AppIndicator)

- No menu → icon disappears in this user's environment.
- Attached menu → icon reappears.
- Left-click toggle unreliable when a menu is attached.
- Fallback: `Show/Hide SlashIt` menu item toggles the window.
