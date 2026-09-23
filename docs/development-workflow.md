# Development Workflow

This document explains how maintainers and coding agents develop SlashIt:
where work happens, which tools to use, and how changes become pull requests.
[`AGENTS.md`](../AGENTS.md) holds the short normative rules; this document
explains the procedure behind them and why they exist.

External contributors do not need any of this. Forking the repository and
using plain Git, as described in [`CONTRIBUTING.md`](../CONTRIBUTING.md), is
fully supported.

Commands below were checked against `jj` 0.42.0, the version pinned in the
project `mise.toml`.

Paths are given relative to the canonical repository root. Its parent
directory, the *project parent*, holds the canonical repository, developer
workspaces, and local private state. Where the project parent lives on disk
is up to each maintainer.

## 1. Repository roles

Four kinds of checkout exist. They look alike on disk but serve different
purposes.

| Role | Location | Purpose |
|---|---|---|
| Canonical coordination root | The canonical repository root | Holds the colocated `.git` and `.jj` store. Used to fetch, create workspaces, and inspect. Its `default` workspace stays an empty change on `main`. |
| JJ developer workspace | `../workspaces/<slug>` | Where normal work happens. Shares the repository store with the root but has its own working copy, `@`, and `target/`. |
| Git-only disposable clone | `../workspaces/<slug>-git` | Rare. An independent `git clone` for tasks that need a real `.git` directory (section 6). |
| SlashIt product task worktrees | `<data_dir>/worktrees/<project-key>/<branch>` or a user-configured path | Created by the SlashIt application for the tasks it manages. They belong to the product, not to this development workflow. See [`docs/architecture/state-locations.md`](architecture/state-locations.md). |

The canonical root stays clean because it is shared. Every workspace's
changes, the Git refs, and the operation log all live in its store, so edits
made there are the easiest to confuse with someone else's work.

## 2. Directory layout

```text
<project parent>/
  slashit-app/          canonical repository root (this repository)
  workspaces/           JJ developer workspaces and Git-only clones
    <slug>/
    <slug>-git/
  slashit-private/      local maintainer state; not a Git repository, never pushed
    reports/            one report per substantial unit of work
    archive/            retired local material kept for reference
    handoff-queue/      handoff notes between sessions
```

This layout is the maintainer and agent convention. External contributors do
not need to reproduce it; an ordinary clone or fork works.

`slashit-private/` is local, untracked maintainer state, not part of the
repository or of its public contract. `workspaces/` and `slashit-private/` sit
outside the repository, so no ignore rule is needed for them. Handoff notes
belong in `slashit-private/handoff-queue/`. Some agent tooling instead writes a
`handoff-queue/` directory at a checkout's root, so `.gitignore` also ignores
`/handoff-queue/` there to keep such notes from ever becoming tracked.

## 3. Starting a unit

A unit is one coherent piece of work that will most likely become one pull
request.

1. Update deliberately from the canonical root. `jj git fetch` changes
   repository state, so run it on purpose rather than as a side effect.
   Then confirm where `main` is:

   ```bash
   cd <project parent>/slashit-app
   mise exec -- jj git fetch
   mise exec -- jj --ignore-working-copy log -r 'main | main@origin'
   ```

   If the fetch moved `main`, move the root's empty `default` change onto it
   with `mise exec -- jj new main`, after confirming that `jj status` shows
   no edits there.

2. Create the workspace from `main`. Pick a short slug that describes the
   work, such as `terminal-resize` or `dev-workflow-contract`, and use it for
   both the directory and the workspace name:

   ```bash
   mise exec -- jj workspace add ../workspaces/<slug> \
     --name <slug> -r main -m "<type>(<scope>): <summary>"
   ```

3. Enter the workspace and confirm the toolchain:

   ```bash
   cd ../workspaces/<slug>
   mise exec -- jj --version
   ```

   If mise reports that the workspace's `mise.toml` is not trusted, run
   `mise trust` there.

Rules:

- One writer per workspace. A second agent that needs to write gets its own
  workspace.
- Do not create backup or archive bookmarks. The operation log
  (`jj op log`) already records every prior state, and extra bookmarks clutter
  the shared repository.

## 4. JJ workspace lifecycle

**Work.** Inside the workspace, use ordinary `jj` commands (`jj status`,
`jj diff`, `jj describe`, `jj new`, `jj split`, `jj squash`). Each workspace
has its own `@`, so these do not affect other workspaces' working copies.

**Inspect from elsewhere.** Every workspace's working-copy commit is
addressable as `<name>@`:

```bash
mise exec -- jj --ignore-working-copy log -r '<slug>@'
mise exec -- jj workspace list
```

**Update onto a newer `main`.** Fetch, then rebase the whole branch of work:

```bash
mise exec -- jj git fetch
mise exec -- jj rebase -b @ -o main
```

If an operation in another workspace rewrote this workspace's commit, `jj`
reports the working copy as stale. Run `mise exec -- jj workspace update-stale`
to bring it back in sync.

**Publish.** Create a bookmark only when publication is actually approved, not
as a local save point:

```bash
mise exec -- jj bookmark create <bookmark> -r @
mise exec -- jj git push -b <bookmark> --dry-run
mise exec -- jj git push -b <bookmark>
```

`jj git push -b` starts tracking a new bookmark automatically. Pushing is a
public mutation; see section 11.

JJ workspaces have no `.git`, so `gh` cannot discover the repository from
inside one. Pass the repository explicitly (`gh pr create -R BarraDev/slashit
...`) or run `gh` from the canonical root.

**Close.** When the work has merged, has been safely published, or is being
deliberately abandoned, remove the workspace from the canonical repository
root:

```bash
mise exec -- jj workspace forget <slug>
rm -rf ../workspaces/<slug>
```

`jj workspace forget` stops tracking the workspace's working copy and does
not touch the directory on disk. It does not abandon described or non-empty
work; only an empty, undescribed working-copy commit is discarded. The `rm` removes
the directory and its `target/`. To discard unmerged work, run `jj abandon` on
it explicitly; do not rely on forgetting the workspace.

## 5. Colocated Git/JJ safety

The canonical root is colocated: `.jj` and `.git` describe the same
repository. To keep them consistent, `jj` automatically imports Git refs and
`HEAD` at the start of a command and exports its own changes back to Git
afterward.

As a result, a command that looks read-only, such as `jj log` or `jj status`,
can still:

- snapshot the working copy into a new commit;
- import moved Git refs and record a new operation;
- export bookmarks to Git refs.

For genuine inspection, especially in the canonical root while another agent
may be working, pass `--ignore-working-copy`:

```bash
mise exec -- jj --ignore-working-copy log
mise exec -- jj --ignore-working-copy diff -r <rev> --stat
```

This skips the working-copy snapshot and the automatic Git import that goes
with it. It has limits:

- the view can be stale: files on disk and Git refs changed outside `jj` are
  not reflected;
- it does not make a mutating subcommand safe. `jj --ignore-working-copy
  rebase ...` still rewrites commits.

Read-only Git plumbing is also acceptable for inspection: `git rev-parse`,
`git cat-file`, `git ls-tree`, `git diff-tree`, `git ls-remote`. Git porcelain
that moves refs, `HEAD`, or the index (`git commit`, `git checkout`,
`git switch`, `git reset`, `git rebase`, `git stash`) is not part of the
normal workflow; it bypasses `jj` and forces a later import to reconcile the
difference.

## 6. Git-only exception

Some tasks genuinely need a real `.git` directory:

- dogfooding SlashIt's own Git worktree backend;
- reproducing the behavior of an external plain-Git contributor;
- `git bisect`;
- third-party tools that require `.git`.

For these, create a disposable independent clone from the canonical
repository root:

```bash
git clone https://github.com/BarraDev/slashit.git ../workspaces/<slug>-git
```

Delete the clone when the task is done.

Never run `git worktree add` against the canonical `.git`. Git worktrees
register themselves in the canonical repository's `.git/worktrees/`, add refs
and `HEAD`s that `jj` then imports, and tie a throwaway experiment to the
shared store. The same applies to pointing a SlashIt development build at the
canonical root as a project: the product would create its task worktrees
against the canonical `.git`. Use a `<slug>-git` clone as the project instead.

JJ developer workspaces have no `.git` directory. This is expected.

## 7. Mise and toolchain policy

Project tools come from the project `mise.toml`. Today it pins one tool:

```toml
[tools]
jj = "0.42.0"
```

Run it as `mise exec -- jj ...` so every workspace and every agent uses the
same version. A global `jj`, especially one tracking `latest`, can behave
differently from the pinned version while both operate on the same shared
repository store.

Rules:

- Do not add a tool to `mise.toml` for the convenience of one agent or one
  session.
- A new shared tool needs a stated reason in its pull request and must be
  exercised by CI.
- No lockfile is required beyond the exact version pin.

## 8. Cargo target policy

Policy: **per-workspace target**. Each workspace builds into its own
`target/` at the workspace root. `.cargo/config.toml` sets
`target-dir = "target"`, which resolves relative to the workspace. Do not
set `CARGO_TARGET_DIR` in your shell or agent environment: it overrides the
config file and silently turns every workspace's builds into a shared target.

Why:

- The acceptance harness (`crates/slashit-acceptance`) locates the application
  binary at `target/debug/slashit-ui` relative to the workspace root.
- Concurrent agents in different workspaces must not overwrite each other's
  application binaries and build artifacts.
- A shared target directory would also share Cargo's build lock, so one
  agent's build would block another's.

Operational rules:

- Keep roughly two or three active mutating workspaces; each `target/` is
  large.
- Delete the workspace's `target/` when closing the workspace (the `rm -rf` in
  section 4 does this).
- Experiments with a different target layout, such as a shared target or a
  compiler cache, belong in a disposable workspace, not in project config.
- Cargo's registry and Git download caches under `~/.cargo` remain shared
  globally. Only build output is per workspace.

## 9. Agent and subagent model

- **One mutating owner per workspace.** The owner is the only process that
  edits files, runs `jj` mutations, or builds there.
- **Read-only parallel reviewers** are useful for bounded, independent
  questions. They may read a shared workspace but must not write to it or
  start builds that contend with the owner's.
- **No nested subagent trees.** The main agent spawns subagents; subagents do
  not spawn their own.
- **The main agent re-verifies.** Subagent output is evidence. Before a claim
  drives a decision, the main agent checks it against the code or tool
  output.
- **Adversarial review is proportional.** Use an independent adversarial
  reviewer for high-blast-radius or publication-shaping changes, not for
  routine edits.
- **Do not poll.** Check CI and review bots when a decision depends on them,
  not on a loop.
- **Stop at genuine decisions.** Product choices, scope changes, and anything
  requiring authorization go back to the maintainer instead of being guessed.

## 10. Reports and sessions

Each substantial unit ends with one authoritative report in
`../slashit-private/reports/`, named
`<YYYY-MM-DD>-<slug>.md`. A report records the starting state, what changed,
how it was validated, and the next action. An optional bounded worklog may sit
beside it. Full chat transcripts are not kept.

Reports are local, private evidence. They are not committed and are not
linked from public text. When a report contains a lesson that should outlive
it, rewrite that lesson into real project documentation (this file,
`AGENTS.md`, or an architecture doc).

Start a fresh agent session for each substantial unit. Long sessions
accumulate stale assumptions, and the report is the handoff.

## 11. Pull request and publication workflow

**Boundaries.** A pull request is a semantic, causal delivery unit: one
coherent change a reviewer can understand and a future reader can find. Do not
open one PR per review finding, and do not bundle unrelated changes.

**Stacks.** Stack PRs, for example with `gh stack`, only when a later change
genuinely depends on an earlier one. Every PR in a stack must be independently
truthful: its title, body, and diff describe what that PR alone does, and it
builds and passes its checks at its own tip. Settle the shape of a stack
before creating it; reshaping a published stack is expensive.

**Durable text.** PR titles and bodies, commit messages, and source comments
are read long after the work ends. Describe the problem, the change, and how
it was verified. Do not include internal work-tracking labels, session
narration, temporary verification SHAs, private report paths, or AI
attribution. See "No AI Attribution" in `AGENTS.md`.

**Review findings.** Where practical, fix a review finding in the PR that
introduced the problem rather than in a follow-up PR. Do not post transient
agent-status comments; reply to review threads with what changed.

**Evidence.** Hosted CI is evidence, not complete proof. For example, it does not run
tests marked `#[ignore]`, such as the PTY tests. Run the relevant
local checks too, and do not merge with a known blocker open.

**Authorization gates.**

| Operation | Gate |
|---|---|
| Edits, `jj` changes, builds, and tests in your own workspace | None |
| Creating a publication bookmark, pushing, and creating or editing PRs, issues, or comments | Explicit authorization for that publication |
| Abandoning or deleting work or workspaces you do not own | Explicit authorization from the owner |
| Merging, force-pushing, deleting remote branches or tags, and changing repository or GitHub settings | The repository owner's explicit approval, every time |
