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

## 1. Terminology

These are JJ and development terms. They describe how SlashIt itself is
developed, not what the SlashIt application manages.

| Term | Meaning |
|---|---|
| JJ workspace | A working copy attached to the shared repository store. Each has a name and its own working-copy commit. `default` is the name of the canonical root's JJ workspace; it is not a bookmark. |
| Working-copy commit, `@` | The commit that holds the files on disk in the current JJ workspace. `jj` snapshots edits into it automatically; there is no staging step. `<name>@` addresses another JJ workspace's working-copy commit. |
| Change ID and commit ID | The change ID (letters `k` to `z`, such as `kxqlzmno`) stays the same when a change is edited, described, or rebased. The commit ID (hex) identifies one exact snapshot and changes on every rewrite. Record both when a snapshot matters. |
| Completed checkpoint | A described, validated change that `jj new` has left behind, so it sits at `@-` or lower. |
| Empty continuation | The empty, undescribed `@` that `jj new` creates on top of a completed checkpoint. |
| Bookmark | A named pointer to a commit; JJ's equivalent of a Git branch. `main` is the primary bookmark. A publication bookmark is the one pushed for a pull request. Bookmarks do not follow `@`, but they do follow rewrites of the commit they point to. |
| Canonical root workspace | The `default` JJ workspace at the canonical repository root (section 2). |
| Developer workspace | A JJ workspace under `../workspaces/<slug>` where one owner does one unit of work. |

None of these is a product concept. A Product Workspace is the SlashIt
container that groups Projects. A Git worktree is Git's own multi-checkout
mechanism, which this workflow does not use on the canonical `.git`
(section 7). A Task Checkout is the isolated working copy the application
creates for a Task; it is always a Git worktree, and a JJ workspace is not a
Task Checkout backend. See
[`docs/architecture/product-model.md`](architecture/product-model.md).

## 2. Repository roles

Four kinds of checkout exist. They look alike on disk but serve different
purposes.

| Role | Location | Purpose |
|---|---|---|
| Canonical coordination root | The canonical repository root | Holds the colocated `.git` and `.jj` store. Used to fetch and import, create developer workspaces, inspect publications, normalize after merges, and inspect the repository safely. Not a place for product edits. |
| JJ developer workspace | `../workspaces/<slug>` | Where all product edits happen. Shares the repository store with the root but has its own working copy, `@`, and `target/`. |
| Git-only disposable clone | `../workspaces/<slug>-git` | Rare. An independent `git clone` for tasks that need a real `.git` directory (section 7). |
| SlashIt product task worktrees | `<data_dir>/worktrees/<project-key>/<branch>` or a user-configured path | Created by the SlashIt application for the tasks it manages. They belong to the product, not to this development workflow. See [`docs/architecture/state-locations.md`](architecture/state-locations.md). |

The canonical root stays clean because it is shared. Every workspace's
changes, the Git refs, and the operation log all live in its store, so edits
made there are the easiest to confuse with someone else's work.

Its steady state is: `default@` is an empty, undescribed change whose parent
is `main`. Check it with:

```bash
mise exec -- jj --ignore-working-copy log -r 'default@ | main'
```

## 3. Directory layout

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

## 4. Starting a unit

A unit is one coherent piece of work that will most likely become one pull
request. All product edits happen in a JJ developer workspace, and
substantial work gets a dedicated one.

1. Update deliberately. The repository store is shared, so a fetch from any
   workspace updates all of them; this procedure runs it from the canonical
   root. `jj git fetch` changes
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
   both the directory and the workspace name. When the purpose is already
   known, pass the description now:

   ```bash
   mise exec -- jj workspace add ../workspaces/<slug> \
     --name <slug> -r main -m "<type>(<scope>): <summary>"
   ```

3. Enter the workspace and confirm the toolchain and the base:

   ```bash
   cd ../workspaces/<slug>
   mise exec -- jj --version
   mise exec -- jj log -r '@ | @-'
   ```

   If mise reports that the workspace's `mise.toml` is not trusted, run
   `mise trust` there.

Rules:

- One writer per workspace. A second agent that needs to write gets its own
  workspace.
- Know the base. Record the commit ID of `main` the workspace started from.
- Ordinary local work needs no bookmark. Do not create backup or archive
  bookmarks either. The operation log (`jj op log`) already records every
  prior state, and extra bookmarks clutter the shared repository.

## 5. JJ workspace lifecycle

### Preflight

Run this before the first mutation of a session or of a unit, and again after
anything that could have changed the answers (a fetch, another agent's
activity, a failed command). It is read-only in intent, but an ordinary `jj`
command snapshots the working copy, and in the colocated canonical root it also
imports and exports Git refs. So the flag depends on where you are:

- Canonical root: put `--ignore-working-copy` on every `jj` command below.
- Your own developer workspace: use ordinary commands. The snapshot is what
  makes step 3 true; with the flag, `@` shows the last snapshot and reports an
  edited change as empty.

```bash
# 1. Which kind of checkout is this? A JJ developer workspace has no .git; the
#    canonical root and a Git-only clone do.
pwd; test -e .git && echo "has .git" || echo "no .git"
mise exec -- jj workspace list

# 2. Ownership: is anyone else mutating this workspace? Recent operations
#    that are not yours mean it is not yours to edit.
mise exec -- jj op log --limit 5

# 3. Active revision and parent: what is @, is it empty, what is @-?
mise exec -- jj log -r '@ | @-'

# 4. Publication bookmark against the remote it will be pushed to. The local
#    bookmark must exist once, not be conflicted, and agree with <bookmark>@origin.
mise exec -- jj --ignore-working-copy bookmark list -a <bookmark>

# 5. The remote head as GitHub reports it, not as the last push printed it.
gh api repos/BarraDev/slashit/git/ref/heads/<bookmark> --jq .object.sha
```

Then confirm the authorization the next operation needs (section 13) and read
the `AGENTS.md` of every directory you are about to touch.

Stop under section 15 when the answers disagree with what you were told: you
are in the canonical root and about to edit product files, `@` holds edits you
did not make, the bookmark is conflicted or its change is divergent, or the
remote head is not the commit you expected.

### Work

Inside the workspace, use ordinary `jj` commands (`jj status`, `jj diff`,
`jj describe`, `jj new`, `jj split`, `jj squash`). Each workspace has its own
`@`, so these do not affect other workspaces' working copies.

Give a meaningful active change its description early, before or at the
start of the edits:

```bash
mise exec -- jj describe -m "<type>(<scope>): <summary>"
```

Describing is not committing. The files you edit are the content of `@`, so
`jj status` keeps listing them as working-copy changes after `jj describe`;
that is expected. A description is what makes the change identifiable in the
log, in other workspaces, and to reviewers.

### Complete a checkpoint

When the change is implemented and validated:

1. Make sure the description is still accurate.
2. Record its change ID and commit ID:

   ```bash
   mise exec -- jj log --no-graph -r @ -T 'change_id ++ " " ++ commit_id ++ "\n"'
   ```

3. Start an empty continuation:

   ```bash
   mise exec -- jj new
   ```

Afterwards the graph is:

```text
@   empty, undescribed continuation
@-  completed, described checkpoint
```

A developer workspace that is idle or waiting for review should look like
this: nothing unfinished sits in `@`, and further edits cannot silently land
in the completed change. This is a workflow boundary, not a technical lock.
The completed checkpoint can still be rewritten by any command that targets
it.

### Inspect from elsewhere

Every workspace's working-copy commit is addressable as `<name>@`:

```bash
mise exec -- jj --ignore-working-copy log -r '<slug>@ | <slug>@-'
mise exec -- jj workspace list
```

### Update onto a newer `main`

Before publication, rebasing is normal. Fetch (section 4), then rebase the
whole branch of work:

```bash
mise exec -- jj git fetch
mise exec -- jj rebase -b @ -o main
```

If an operation in another workspace rewrote this workspace's commit, `jj`
reports the working copy as stale. Run `mise exec -- jj workspace update-stale`
to bring it back in sync.

After publication, do not rebase to follow `main`. `-b @` rebases every
ancestor that is not on `main`, including the published checkpoint, so it
rewrites published history; the GitHub squash merge resolves the base. If a
rebase is genuinely needed, for example to resolve conflicts, it is a rewrite
of a published checkpoint and needs the owner's approval (section 13).

### Publish

Publication needs explicit approval (section 13). The publication bookmark
must point at the completed checkpoint, named by an explicit revision. `jj
bookmark create`, `set`, and `move` default to `@` when no revision is given,
and `@` is the empty continuation, so never rely on the default:

```bash
mise exec -- jj bookmark create <bookmark> -r <completed-change-id>
mise exec -- jj git push -b <bookmark> --dry-run
mise exec -- jj git push -b <bookmark>
```

The dry run must list only that bookmark, as `add` for a new one. `jj git
push -b` starts tracking a new bookmark automatically; the empty continuation
above the bookmark is not pushed. `jj` refuses to push a commit with no
description, but that is a safety net, not the publication rule.

JJ workspaces have no `.git`, so `gh` cannot discover the repository from
inside one. Pass the repository explicitly (`gh pr create -R BarraDev/slashit
...`) or run `gh` from the canonical root.

### Review corrections

A review correction becomes a child of the published checkpoint. The empty
continuation already is that child, so reuse it rather than creating another
one with `jj new <published-commit>`:

1. Confirm with `mise exec -- jj log -r '@ | @-'` that `@` is empty and `@-`
   is the published checkpoint, then describe the existing empty `@` before
   editing, for example
   `mise exec -- jj describe -m "fix(<scope>): <summary>"`.
2. Edit and validate. The published checkpoint below it is not touched.
3. Complete the checkpoint with `jj new`, as above.
4. With approval, prove ancestry and advance the bookmark explicitly:

   ```bash
   mise exec -- jj log -r '<bookmark>@origin::<correction-change-id>'
   mise exec -- jj bookmark move <bookmark> --to <correction-change-id>
   mise exec -- jj git push -b <bookmark> --dry-run
   mise exec -- jj git push -b <bookmark>
   ```

The first command must show the remote bookmark's commit, which is what was
published, as an ancestor of the correction. Do not check against the local
bookmark: it follows rewrites of its target, so it can still look like an
ancestor after the published commit was rewritten. `jj bookmark move`
refuses backward or sideways moves unless given `--allow-backwards`; do not
pass it here. The dry run must report `move forward` for that bookmark and
nothing else.

Step 4 is where a mistake becomes public, so before running it prove the
correction is what you believe it is:

- It changes something. For a source correction `mise exec -- jj diff -r
  <correction-change-id> --stat` must list the intended files. A correction
  with an empty diff is not a fix, whatever its description says, and a
  successful `jj describe`, commit or push proves nothing about content.
- Its parent is the published checkpoint:
  `mise exec -- jj log -r '<correction-change-id>- | <bookmark>@origin'` shows
  one commit.
- The tests ran against exactly this commit. Record
  `jj log --no-graph -r <correction-change-id> -T commit_id` before the run
  and again after it, and make sure `jj status` showed no edits on top.
- Edit the change where it lives. Editing the same change from a Git
  checkout or with Git porcelain makes it divergent (two commits, one change
  ID), and a push from the Git side can then publish a copy that lacks the
  content. Check for it with `mise exec -- jj log -r 'divergent()'`.

After the push, read GitHub back instead of trusting the push output:

```bash
scripts/verify-published-commit.sh BarraDev/slashit <bookmark> <full-commit-id> --path <changed-dir>/
```

It fails unless the remote head is exactly that commit, the commit changes
files, and it touches the given path. Equal commit IDs imply equal trees, so
the published tree is the tree that was tested. The script does not say the
hosted checks passed.

### Published-history policy

Before publication, rewriting changes (`describe`, `squash`, `split`,
`rebase`) is normal JJ work. Once a commit has been pushed as a publication
or reviewed, preserve it: corrections are child changes, and the bookmark
advances by fast-forward.

This is SlashIt policy, not a property `jj` enforces. The default
`immutable_heads()` covers `trunk()` (normally `main`), tags, and untracked
remote bookmarks; a pushed publication bookmark is tracked, so `jj` will
rewrite its commits if asked. `jj git push` also behaves like `git push
--force-with-lease`: it pushes a sideways or backward bookmark move without
any `--force` flag, labeling it `move sideways` or `move backward` in the dry
run. Rewriting a published checkpoint, or pushing anything other than `add`
or `move forward`, needs an explicit reason and the repository owner's
approval.

### Pull request shape

A pull request may contain the initial checkpoint and one or more corrective
checkpoints. Do not squash them locally just for appearance; the GitHub
squash merge produces the single commit on `main`.

### After the merge

1. Verify that the squash commit on `main` has the tree you expect, for
   example by comparing `git rev-parse <merge-commit>^{tree}` with the tree
   of the final checkpoint.
2. Confirm the developer workspace holds no unique work:

   ```bash
   mise exec -- jj --ignore-working-copy log -r '::<slug>@ ~ ::main'
   ```

   Every listed change must be the empty, undescribed `<slug>@` or one of the
   merged checkpoints.
3. From the canonical root, fetch the new `main` and move the root's empty
   `default` change onto it with `mise exec -- jj new main`, after
   confirming that `jj status` there shows no edits.
4. Retire the local publication bookmark with
   `mise exec -- jj bookmark forget <bookmark>`. Do not use `jj bookmark
   delete`, which schedules deletion of the remote branch on the next push.
5. Remove the workspace, from the canonical root:

   ```bash
   cd <project parent>/slashit-app
   mise exec -- jj workspace forget <slug>
   rm -rf <project parent>/workspaces/<slug>
   ```

Deleting the remote branch is a separate operation with its own approval
(section 13).

`jj workspace forget` stops tracking the workspace's working copy and does
not touch the directory on disk. It does not abandon described or non-empty
work; only an empty, undescribed working-copy commit is discarded. The `rm`
removes the directory and its `target/`. To discard unmerged work, run `jj
abandon` on it explicitly; do not rely on forgetting the workspace. The same
steps close a workspace whose work is being deliberately abandoned.

The merged checkpoints stay in the store as visible commits. While the remote
branch exists, `bookmark forget` leaves it as an untracked remote bookmark,
which makes those commits immutable. Both are expected.

### Retire remote branches

Deleting a remote branch requires the repository owner's explicit approval for
that deletion, every time (section 13). This procedure does not grant it, and an
approval given for other references does not carry over.

Deleting a remote branch changes local JJ state at the next `jj git fetch`.
JJ drops the remote-tracking bookmark and any local bookmark that tracked it,
and it abandons commits that nothing else reaches. A squash merge does not make
the pre-squash commits disposable: once the branch is gone, they exist only in
the local store. Run JJ through the pinned binary (`mise exec -- jj`). In the
canonical root, inspect with `--ignore-working-copy` as section 5 describes. That
flag reads the last snapshot, so it can miss files edited in a developer
workspace since its last JJ command; step 3 covers workspaces. Follow this order
for each deleted or to-be-deleted branch:

1. Record a baseline and keep it with the report: the visible history
   (`mise exec -- jj --ignore-working-copy log -r '::visible_heads()' --no-graph -T 'commit_id ++ "\n"'`),
   the workspace list (`mise exec -- jj --ignore-working-copy workspace list`),
   the bookmark list (`mise exec -- jj --ignore-working-copy bookmark list -a`),
   and `mise exec -- jj --ignore-working-copy log -r 'conflicts() | divergent()'`.
2. Inventory the name and record each head commit ID:
   `mise exec -- jj --ignore-working-copy bookmark list -a exact:<name> -T 'name ++ " " ++ if(remote, remote, "-") ++ " " ++ if(conflict, "CONFLICT", "ok") ++ " " ++ if(normal_target, normal_target.commit_id(), "none") ++ "\n"'`.
   Once the name is forgotten, the commit ID is the only way to refer to the
   head. If any row reads `CONFLICT`, stop before step 4 and report it: a
   conflicted bookmark has no single head to preserve. Note any divergent change.
3. Check each head against every workspace and against unpublished work. List
   the workspaces with `mise exec -- jj --ignore-working-copy workspace list`,
   then run `mise exec -- jj --ignore-working-copy log -r '((<head>:: | ::<head>) & ::<slug>@) ~ ::main'`
   for each `<slug>`, and `mise exec -- jj --ignore-working-copy log -r '(<head>:: ~ <head>) ~ ::main'`
   for unpublished descendants. A
   non-empty result protects the head. Do not forget or delete a protected head;
   record it in the report and wait for the repository owner's decision.
   A workspace's `@` is only as current as its last snapshot. For each workspace
   the query involves, its owner must confirm that the working copy is current:
   the owner runs `mise exec -- jj status` there and records the output. Do not run an
   ordinary JJ command in a workspace you do not own, because it snapshots that
   workspace. A head is protected if an unconfirmed workspace shows any relation
   to it.
4. Preserve the heads before anything is forgotten. Fetch them by SHA into a
   disposable bare repository (`git fetch <canonical .git> <sha>...`; verified
   with the pinned Git), name each with `git update-ref refs/archive/<name> <sha>`,
   and run `git -C <scratch-repo> bundle create <file> --all`, where the scratch
   repository is outside the canonical repository and holds only the archive
   refs. Run `git bundle verify`, restore the bundle into a second disposable repository,
   and compare each restored SHA and tree with the original. Stop if any check
   fails.
5. Forget the exact names: `mise exec -- jj bookmark forget --include-remotes exact:<name> ...`.
   Confirm first that the step 2 listing matches only the intended rows. Do not
   use `bookmark delete`, which schedules a remote deletion on the next push; do
   not use `abandon`; do not use glob patterns.
6. Delete the remote branch after step 5 and before any fetch. Do not fetch while
   a forgotten branch is still live on the remote: the fetch imports it back as
   an untracked `origin` bookmark. If that happened, repeat step 5 before
   deleting.
7. Fetch, then compare with the baseline from step 1: the visible history, the
   workspace list, the bookmarks you keep, and the conflicts and divergent
   changes. Any commit that leaves the visible history, or any new conflict, is
   a stop.

Keep other writers out of the store. Run `mise exec -- jj --ignore-working-copy op log --limit 3` before each step,
and stop if an operation you did not start appears. Rehearse a non-trivial
sequence in a disposable colocated repository with a local bare remote
(section 9), including a deleted branch whose commit only it reaches. A rehearsal
proves the mechanism, not your repository's state, so still run steps 1 to 7 on
the real one. Do not restore an earlier operation with `jj op restore` until you
have shown that nothing done since would be overwritten.

## 6. Colocated Git/JJ safety

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

## 7. Git-only exception

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

`jj` imports the refs and `HEAD` of every Git worktree registered in the
canonical `.git`, so a worktree made there shows up as a stray bookmark in
the shared store, and its `HEAD` moves under `jj`'s feet. Making one
"temporarily" is not harmless. `git worktree list` run in the canonical root
shows what is registered; report a worktree you did not create instead of
removing it.

`jj git init --colocate` inside a Git worktree is refused ("Cannot create a
colocated jj repo inside a Git worktree", checked with the pinned `jj`), so a
worktree cannot be converted into a JJ workspace. For a real `.git` use the
independent clone above, which is its own repository.

### When the preferred JJ operation fails

A failing `jj` command is information, not an obstacle to route around.
Switching to Git porcelain or to another kind of checkout leaves a state `jj`
did not make and a later import has to reconcile, which is how a correction
ends up published as an empty commit. Recover inside the workflow:

| Symptom | Do |
|---|---|
| `jj workspace add` fails | Read the error. Usually the path exists or the working directory is wrong. Fix that; do not fall back to `git worktree add`. |
| "working copy is stale" | `mise exec -- jj workspace update-stale`, then repeat the preflight. |
| Conflicted bookmark (`??`) or divergent change | Do not push. The workspace that owns the change decides which side is kept. Report it; resolve with `jj bookmark set` or `jj abandon` only if you own it. |
| A push dry run shows anything but `add` or `move forward` | Stop (section 15). Never retry with a force option. |
| A tool needs a real `.git` | The Git-only exception above: an independent clone, not a worktree and not a colocation. |
| `jj` itself misbehaves | Check `mise exec -- jj --version` against the pin, and reproduce in a disposable repository (section 9). Report the failure; do not improvise another VCS flow. |

If a Git-only step really is the only way forward, record why in the report.

## 8. Mise and toolchain policy

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

### Rust toolchain

The Rust version is pinned in one place, `rust-toolchain.toml` at the
repository root. rustup selects it for any `cargo` or `rustc` run inside a
checkout, including CI and the release workflow, so there is no `stable`
selector and no version literal in the workflows. Outside the repository
nothing changes: your global default toolchain is untouched.

- Install it with `rustup toolchain install` (no argument) from inside the
  checkout. By default rustup also installs it on first use.
- The file pins only the compiler (profile `minimal`). Components and targets
  are added by the jobs that need them (`clippy`, `wasm32-unknown-unknown`,
  the macOS release targets), so a checkout or job does not carry
  capabilities it does not use. Developers add `clippy` and `rustfmt`
  themselves (`rustup component add clippy rustfmt`); rustup applies them to
  the pinned toolchain.
- A `cargo` that bypasses the rustup proxies (a distribution-packaged Rust
  on `PATH` ahead of `~/.cargo/bin`) also bypasses the file. Check with
  `rustc --version` inside the checkout.
- This is a toolchain pin, not an MSRV declaration; `Cargo.toml` carries no
  `rust-version`.
- Bump it deliberately in its own pull request: change the `channel`, run the
  exact Clippy command from CI, and address any new lints or warnings.

## 9. Disposable JJ experiments

Checking how `jj` behaves, for example before documenting a workflow, belongs
in a throwaway repository, never in the shared store. The pinned `jj` comes
from the project `mise.toml`, but `mise -C <dir>` and `mise exec --cd <dir>`
run the command *in* `<dir>`. Pointing either at a SlashIt checkout to get
the pinned version makes every experimental command act on the real
repository.

Instead:

1. Resolve the pinned binary to an absolute path from a SlashIt checkout,
   then leave it. Give the experiment its own identity and an empty `jj`
   config, so neither personal settings nor any config file is involved:

   ```bash
   JJ_BIN=$(mise which jj)   # run inside a SlashIt checkout
   SANDBOX=$(mktemp -d)
   : >"$SANDBOX/config.toml"
   export JJ_CONFIG="$SANDBOX/config.toml"
   export JJ_USER="Sandbox" JJ_EMAIL="sandbox@example.invalid"
   git init --bare "$SANDBOX/remote.git"
   mkdir "$SANDBOX/repo" && cd "$SANDBOX/repo"
   pwd                       # must print the sandbox path
   "$JJ_BIN" git init --colocate .
   "$JJ_BIN" git remote add origin "file://$SANDBOX/remote.git"
   ```

2. Run every experimental command as `"$JJ_BIN" ...` with the sandbox as the
   current directory, and confirm `pwd` again after steps that change
   directory.
3. Before any experimental push, prove the remote belongs to the sandbox:
   `"$JJ_BIN" git remote list`, `git config --get-all remote.origin.url`,
   and `git config --get-all remote.origin.pushurl` must resolve inside
   `$SANDBOX` (a push URL may be absent), and
   `git config --get-regexp '^url\.'` must show no URL rewrite. Stop if any
   of them names GitHub, `BarraDev`, or a SlashIt checkout.
4. Delete the sandbox when the evidence is recorded.

Never use the production GitHub remote for workflow experiments.

## 10. Cargo target policy

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
  section 5 does this).
- Experiments with a different target layout, such as a shared target or a
  compiler cache, belong in a disposable workspace, not in project config.
- Cargo's registry and Git download caches under `~/.cargo` remain shared
  globally. Only build output is per workspace.

### Resource safety

Diagnose capacity before a large build, not after it fails:

```bash
df -h . "${TMPDIR:-/tmp}"; free -g
```

A `/tmp` mounted as tmpfs consumes memory, so leftovers there can starve a
build as surely as a full disk. Cap build parallelism (`cargo ... -j <n>`)
when memory is short.

Cleanup is limited to what the agent owns:

- Allowed without asking: the `target/` of your own workspace, and temporary
  files you created.
- Needs explicit authorization: anything else, including other workspaces and
  their `target/`, shared Cargo caches, `~/.cargo`, `~/.rustup` and installed
  Rust toolchains, Git worktrees you did not create, and any directory whose
  owner you cannot name. Removing a toolchain or cache to make room is not a
  routine diagnosis step; it breaks other workspaces' builds and may be slow
  or impossible to restore.
- Report leftovers you find (stale worktrees, large temporary trees) with
  their paths and sizes instead of removing them.

## 11. Agent and subagent model

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

## 12. Reports and sessions

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

## 13. Pull request and publication workflow

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
introduced the problem rather than in a follow-up PR, as a corrective child
change (section 5). Do not post transient agent-status comments; reply to
review threads with what changed.

**Evidence.** Hosted CI is evidence, not complete proof. For example, it does not run
tests marked `#[ignore]`, such as the PTY tests. Run the relevant
local checks too, and do not merge with a known blocker open.

**Authorization gates.**

| Operation | Gate |
|---|---|
| Edits, `jj` changes, builds, and tests in your own workspace | None |
| Creating a publication bookmark, pushing, and creating or editing PRs, issues, or comments | Explicit authorization for that publication |
| Abandoning or deleting work or workspaces you do not own | Explicit authorization from the owner |
| Merging, force-pushing or otherwise rewriting a published checkpoint, deleting remote branches or tags, and changing repository or GitHub settings | The repository owner's explicit approval, every time |

## 14. Merge gates and security checks

`main` has no branch protection or required status checks. The gates below are
owner policy, applied by whoever merges.

**Wait for green before merging.** Every functional check in `ci.yml`:

- Agent docs
- Lint and structural checks
- Backend and harness tests
- Frontend tests and build
- Compile check (windows-latest)
- Compile check (macos-latest)
- Durable write tests (ubuntu-22.04, macos-latest, windows-latest)
- Desktop acceptance / product shard 1, 2 and 3 (shard 2 also runs the harness suite)

**Advisory; may finish after the merge.**

- CodeQL (GitHub default setup: Actions, JavaScript/TypeScript and Rust, on
  pull requests, pushes to `main`, and weekly). A pending CodeQL run does not
  block a merge once the functional checks are green. A CodeQL check that has
  already finished red must be inspected before merging. A confirmed false
  positive is dismissed in the code scanning alert with a factual reason;
  do not rewrite otherwise-correct code, including test code, just to turn
  the check green.
- CodeRabbit.

**Asynchronous and periodic.** Review the repository's Security tab weekly or
once per development round:

- Dependabot alerts: triage Critical and High within one working day; batch
  Medium and Low into a periodic dependency update.
- Secret scanning alerts. Push protection is enabled, so a push containing a
  detected secret is rejected before it reaches GitHub.
- New CodeQL alerts on `main`, from post-merge and weekly runs. They stay
  actionable even though CodeQL does not gate the pull request.

## 15. Failure gates

Stop and report instead of guessing when:

- the exact base a unit was asked to start from has moved;
- a rebase or other command would rewrite a published checkpoint;
- the local publication bookmark and `<bookmark>@origin` differ before a
  correction is published;
- a developer workspace being cleaned up still holds unique work;
- a publication dry run shows anything other than `add` or `move forward`
  for the intended bookmark, or lists any other bookmark;
- a bookmark operation could schedule a remote deletion;
- it cannot be proven that removing a workspace loses nothing;
- a sandbox remote or push URL resolves outside the sandbox;
- the preflight (section 5) disagrees with what you were told;
- the next step would use a different VCS or checkout kind because a `jj`
  operation failed (section 7);
- reading the published branch back shows a head or content other than the
  one you tested.

## 16. Verification discipline

A claim of success needs evidence that is about the thing being claimed.
These substitutes are common and wrong:

| Substitute | Why it fails | Check instead |
|---|---|---|
| The last line of `cargo test ... \| tail` | The pipeline's status is `tail`'s. A failing run looks successful. | Run without the pipe and read `$?`, or use `set -o pipefail`, or write to a file and check the status first. |
| A passing run | It may have run another revision, or with uncommitted edits. | Record the commit ID before and after the run and keep `jj status` clean (section 5, Review corrections). |
| A successful push or commit | It proves a ref moved, not that the content is there. | `scripts/verify-published-commit.sh`. |
| A passing local merge of two branches | It validates that union only. Two unmerged pull requests are not integrated by it, and the merge that eventually lands may resolve differently. | Say "validated on a local union"; re-validate after the first PR merges, on the exact tree to be published. |
| An old CodeRabbit review | A review of an earlier commit says nothing about the head, and a rate-limited run is not a review. | Compare the review's `commit_id` with the PR head: `gh api repos/BarraDev/slashit/pulls/<n>/reviews --jq '.[] \| {user: .user.login, commit_id}'`. |
| "CI is green" while jobs run | A check with no conclusion is pending. | `gh pr view <n> -R BarraDev/slashit --json statusCheckRollup`; report pending checks as pending. |

Local tests show how the code behaves in the tests. They do not demonstrate
power-loss durability, behavior on an operating system other than the one they
ran on, or anything the hosted `ignore`d tests cover. State those limits in
the report instead of implying them.

## 17. Change size and review standards

These standards apply to backend, frontend, Tauri IPC, integration, and
architecture work alike. They refine the rules above; they do not replace them.

- **Callers first.** Before changing a shared function, type, or interface, find
  every caller and indirect reference: Tauri command names used by `invoke`,
  entries in the `generate_handler!` list, tests, and scripts. Fix the root
  cause once, in the shared code.
- **Simple before general.** Prefer an existing project mechanism, then plain
  code, over a new abstraction, wrapper, option, or dependency. Remove redundant
  code only after confirming it has no caller and protects no invariant.
- **Keep safeguards.** Code that looks redundant or unused can still enforce
  authorization (for example `GuiMutation` admission), durable persistence and
  write ordering, checkout ownership, provider leases, or recovery evidence.
  Establish its purpose before removing it, and never remove such code to
  shorten a diff.
- **Evidence for findings.** A review finding states a reproduction, a reachable
  failure path, or a violated invariant, and it is checked against the current
  code before it is reported.
- **Established behavior stability.** Treat established, previously released
  behavior as stable. Modify existing shared behavior only for a reproduced
  defect, a proven violated invariant, or an unavoidable requirement of an
  explicitly authorized feature. Before changing it, identify affected callers,
  established invariants, compatibility consequences, and narrower alternatives.
  Passing tests or AI reviewer agreement alone is not sufficient justification.
  Do not perform opportunistic refactoring or cleanup. If the required correction
  exceeds the authorized scope, stop and request an explicit decision.
- **Verify current state first.** Before proposing implementation work, check the
  finding against the current intended base commit and relevant existing tests.
  Historical documentation and older source revisions may explain past behavior,
  but are not evidence of current implementation. Explicitly identify when a
  reported defect has already been fixed.
- **Coverage by risk.** Choose regression tests by the correctness and safety
  risk of the change, not by a fixed count. Behavior that protects data,
  authorization, or ownership needs tests for its failure and negative paths.
  Comment-only and documentation-only changes need no test run beyond the
  documentation check.
- **Limitations.** Record a significant intentional limitation where the code or
  its nearest `AGENTS.md` describes it, with the condition that would justify
  revisiting it.
- **Gates stay authoritative.** A clean review verdict does not replace hosted
  CI, exact-commit validation, or the publication gates in sections 13 and 14.
