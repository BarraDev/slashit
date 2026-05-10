# Handoff: Human Review / PR Review Flow

## Restart Prompt (paste at start of next session)

Continue the SlashIt Human Review / PR review work. Read `HANDOFF_PR_REVIEW.md`,
inspect the current diff and the live app. Top priority is the open bug:
**`Review PR` modal in the new `PR Open` column shows "Analyzing PR comments..."
and then disappears with no error toast and no error banner.** Reproduce against
task `Referral & Rewards System` (PR #61 on `dictmagic/dictmagic-app`,
branch `task-c10b482c`) and find why `show_pr_review_modal` is being reset
or why the spawn_local future is being cancelled before it can populate
`pr_review_error` / `pr_review_plan`.

## User Goal

The Human Review / PR Open flow should:

- Show open PRs in their own `PR Open` column (not in Done).
- Auto-move the task to Done only when GitHub reports `MERGED`.
- Let the agent triage PR comments without editing.
- Ask the user to confirm/edit the proposed fix plan before applying changes.
- Disable Serena/MCP only for the PR review helper, not for normal task agents.
- Recover from `GH007` private-email push rejection only after explicit user confirmation.

## Real Test Case

- Project: `dictmagic-app`
- Task: `Referral & Rewards System`
- Task id: `c10b482c-b610-4e73-b7b6-6cae00b433c5`
- Branch: `task-c10b482c`
- Issue: `https://github.com/dictmagic/dictmagic-app/issues/21`
- PR: `https://github.com/dictmagic/dictmagic-app/pull/61` (open at last check)
- Local repo: `/home/ruiandrada/Repo/dictmagic/dictmagic-app` (JJ + Git colocated)
- Storage: `/home/ruiandrada/.config/slashit-app/tasks/b203c9b8-a0ae-404d-b41d-8dd6fdf95cce.toml`

## Open Bug — Top Priority

**Symptom:** Click `Review PR` on a card in the `PR Open` column. Modal
opens, shows the "Analyzing PR comments..." spinner, and then closes by
itself without a toast and without ever rendering the error banner or
the textarea content.

Possible causes to investigate:

- The outer modal div has `on:click` that calls `show.set(false)` when
  `!applying.get()`. Loading is set to true but `applying` stays false,
  so any backdrop click during loading dismisses the modal. If a
  reactive re-render or focus shift fires a synthetic click, that would
  explain the disappearance.
- The auto-refresh-on-mount loop in `kanban.rs` (around lines 240-261)
  fans out `refresh_task_pr_state` calls. If the spawn_local from
  `analyze_pr_comments` is in the same task pool and one of those
  re-renders the parent in a way that recreates Kanban, signals like
  `show_pr_review_modal` would reset to false. Confirm by adding a
  `console::log` on every `show_pr_review_modal.set(false)`.
- `analyze_pr_comments` could be returning `Err` very fast for this PR
  (e.g. `gh pr view --json reviews,comments,reviewDecision` failing on
  this repo's auth) and `pr_review_error` is being set, but the modal
  also got dismissed before the banner could render.
- If parent component re-runs `<Kanban tasks=... />` whenever
  `set_tasks_signal` propagates, the local `RwSignal::new(false)` for
  `show_pr_review_modal` is recreated. Verify by checking the parent
  page (`pages/dashboard.rs` or wherever Kanban is mounted).

Suggested next steps:

1. Add temporary `web_sys::console::log_1` lines before every
   `show_pr_review_modal.set(false)` and at the start/end of
   `on_analyze_pr_comments`'s spawn_local block to confirm the order of
   operations.
2. Tighten the modal backdrop dismiss: only close on `!loading.get() &&
   !applying.get()`, so the user can't lose progress mid-analysis and
   stray clicks during loading are ignored.
3. Make sure `pr_review_error` actually renders: open the dev tools
   inspector and look for the red banner element in the DOM right
   before the modal disappears.

## What Was Implemented (current diff)

### PR Comment Review reliability

- `src-tauri/src/commands/pr.rs:524-538` — `analyze_pr_comments` now
  returns `Err(...)` (instead of `Ok("")`) when the Claude CLI produces
  no transcript. The frontend renders that as a red banner inside the
  modal, never as plan text.
- `src/components/kanban.rs:139` — new `pr_review_error: RwSignal<Option<String>>`.
- `src/components/kanban.rs:240-275` — `on_analyze_pr_comments` now
  populates `pr_review_error` on `Err` and on empty `Ok`. Toasts kept
  as a complement.
- `src/components/kanban.rs:680-690` — `PrReviewModal` accepts an
  `error` prop and renders a red banner above the textarea when set.

### Disable Serena/MCP for PR review helper

- `src-tauri/src/agents/runner.rs:9-22, 95-99` — added
  `disable_mcp: bool` to `ClaudeRunConfig`. When `true`, runner passes
  `--strict-mcp-config` to the `claude` CLI without any
  `--mcp-config` files, so all MCP servers (project + user, including
  Serena) are skipped.
- `src-tauri/src/commands/pr.rs:741` — PR review helper sets
  `disable_mcp: true`.
- `src-tauri/src/queue/executor.rs:446, 733, 860` — task agents keep
  `disable_mcp: false` (MCP normal).

### `PR Open` column + auto Done on MERGED

- `src/components/kanban.rs:23, 84-93` — `COLUMNS` now has 8 entries;
  new `PR Open` column with `TaskStatus::PrCreated` between
  `Human Review` and `Done`.
- `src/components/kanban.rs:278, 837/863, 862, 929, 990` — removed all
  the `PrCreated || Done` aliasing in stats, drag/drop position, and
  same-column logic. `PrCreated` is now a real, separate column.
- `src/pages/insights.rs:33` — Done count no longer includes `PrCreated`.
- `src-tauri/src/commands/pr.rs:887-918` — `link_pr_to_task` now calls
  `fetch_pr_state` (gh) before saving and sets the task to
  `TaskStatus::Done` if the remote state is `MERGED`, else
  `PrCreated`. The `ExternalRef::GithubPr.state` field is also kept
  in sync.
- `src-tauri/src/commands/pr.rs:935-988` — new
  `refresh_task_pr_state(task_id)` Tauri command. Reads PR state via
  `gh pr view --json state` and returns the updated `Task`. Registered
  in `src-tauri/src/lib.rs:340`.
- `src/services/pr_service.rs:108-112` — frontend wrapper
  `refresh_task_pr_state`.
- `src/components/kanban.rs:240-261` — Kanban auto-refreshes PR state
  on mount for every task that has a linked PR (so tasks merged
  externally bubble to Done).
- `src/components/kanban.rs:1585-1610` — new `Refresh` button on every
  card with a PR. Calls `refresh_task_pr_state` and surfaces a success
  toast ("PR merged — moved to Done") if the task transitioned.

### GH007 private-email recovery (already in branch)

- `get_pr_push_recovery` and
  `recover_private_email_and_create_pr` (`src-tauri/src/commands/pr.rs`)
  are JJ-aware: they detect `jj root` and use
  `jj config set --repo user.email`, `jj metaedit -r <branch>
  --author "Name <email>"`, and `jj git export` before retrying.
- Git fallback rewrites only the branch tip with `git commit-tree`
  + `git update-ref`, guarded by the original SHA.
- Frontend modal triggered from the context-menu `Find/Create PR` when
  the error contains `GH007` or `private email address`.
- `push_branch` uses `jj git push --allow-new --bookmark <branch>`
  when the repo is JJ-backed.

## Verification

```bash
cargo check                         # frontend (Leptos UI) — passes with warnings only
cargo check --manifest-path src-tauri/Cargo.toml   # backend — passes with warnings only
```

Warnings are mostly pre-existing dead-code in unused service files.

End-to-end verification still pending against a JJ-backed repo:

- Confirm `jj metaedit -r <branch> --author ...` works on the actual
  task branch/bookmark name.
- Confirm `jj git push --allow-new --bookmark <branch>` reaches GitHub
  with the rewritten author email.
- Reproduce and fix the disappearing PR Comment Review modal (top
  priority, see Open Bug section).

## UX Notes

- `Sync PR` on cards only finds/links existing candidates. It does not
  create a PR.
- `Find/Create PR` (context menu) is the explicit create path.
- `Refresh` (card button, when has_pr) re-checks PR state via `gh` and
  moves to Done if MERGED.
- `Review PR` (card button, when has_pr) triages PR comments and asks
  for plan approval before applying fixes.
- The modal textarea is the literal `approved_plan` sent to the agent.
  Anything in there will be acted on, so error and "no plan" messages
  must never live inside the textarea.

## Tray Note (unchanged)

Linux/AppIndicator behavior is awkward:

- Attaching no menu makes the icon disappear in this user's environment.
- Restoring an attached menu makes it reappear.
- Left-click toggle may not be reliable on Linux when a menu is attached.
- Current fallback: a `Show/Hide SlashIt` menu item toggles the window.
