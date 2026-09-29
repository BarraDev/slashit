# Where SlashIt stores things

SlashIt keeps your repositories pristine by default. Nothing is written inside
a project unless you ask for it.

This document explains what is stored, where, and why the boundary sits where
it does. The code that enforces it is `src-tauri/src/config/paths.rs`; no other
module is permitted to build a state path by hand.

## The problem this replaced

Path construction used to be scattered across the backend. Four modules each
called `ProjectDirs::from("com", "barradev", "slashit-app")` independently,
worktrees were created as siblings of the repository, and registering a
workspace created an empty `.slashit/` directory in the user's folder that
nothing ever wrote to.

That last one is the clearest symptom: nobody owned the question "where does
this belong?", so a directory appeared in every registered project for no
reason. Routing every path through one module makes that class of mistake
reviewable instead of invisible.

## State classification

Exactly one category may live inside a user's project.

| Category | Examples | Location | Relocatable |
|---|---|---|---|
| Shareable project state | kanban board, task list | project state dir | **yes** |
| Secrets | `AgentConfig::api_key`, daemon tokens | `config_dir`, mode 0600 | no |
| Machine-local | PTY scrollback, terminal sessions | `data_dir` | no |
| Logs | agent transcripts, PR helper logs | `data_dir/logs` | no |
| Runtime | daemon socket, PID file | `runtime_dir` | no |
| Cache | derived, regenerable data | `cache_dir` | no |

Secrets are called out because `config.toml` carries `AgentConfig::api_key`.
If that file were relocatable, an API key would land in a user's git history.
It is written atomically with mode 0600, and the mode is set on the temporary
file *before* the rename, so it is never briefly world-readable at its final
path.

The categories are not a matter of taste. Runtime files must not survive a
reboot, logs must not be committed, and caches must be safe to delete — none
of which is true of a directory inside a git repository.

## Choosing where the board lives

Per project, in Settings → Storage:

- **Outside the project** (default) —
  `<data_dir>/projects/<project-key>/<project-id>/`.
  Your repository is untouched.
- **Inside the project** — `<repo>/.slashit/<project-id>/`. Opt in when you
  want the board committed and shared with your team.
- **Detect automatically** — use `.slashit/` if it already exists and has
  content, otherwise stay outside.

The repository-derived key groups state by checkout. The project UUID below
that root keeps separate SlashIt projects attached to one repository from
sharing, overwriting, or deleting the same board file.

Projects saved before this setting existed load as *Detect automatically*
rather than the type's own `External` default. An existing install may already
have a populated `.slashit/`, and silently switching it to external storage
would look like data loss.

An empty `.slashit/` does not count as opting in. That is precisely the dead
directory older versions created, and treating it as a signal would resurrect
the bug.

## Project keys

External state is keyed by

```text
<sanitised-directory-name>-<first 8 hex of sha256(absolute path)>
```

The hash is taken over the **path**, not over a git remote:

- a fork and its upstream share a remote, but they are different checkouts and
  must not collide;
- repositories with no remote still need a key;
- two checkouts of the same repository (`app` and `app-review`) must not share
  worktrees or task state.

The trade-off is that moving a project directory changes its key. The stored
key travels with the project rather than being re-derived on every load, so a
moved project keeps its state until you migrate it deliberately. Re-deriving
would silently orphan it.

## Migrating

Changing the setting never moves files as a side effect. Selecting an option
builds a plan — real file counts, byte totals and any conflicting paths — and
only an explicit confirmation runs it.

The migration itself is: lock, recover any leftover state from a previous,
interrupted migration to this exact destination, re-plan under the lock (the
plan shown for confirmation is only a preview — a conflict that appears after
it, including one recovery itself just surfaced, is caught here, not silently
resolved in the source's favor), copy the source to a staging directory,
verify the copied source file-by-file, copy in any non-conflicting
destination-only data, retire the destination, atomically rename staging into
its place, and **only then** delete the source and the retired destination.
It is therefore safe across filesystems and idempotent. An interruption
before the retire-and-swap step leaves the source and destination fully
untouched — staging is pure scratch. An interruption during that step is the
one window where the retired destination is briefly the only surviving copy
of its data; `StateMigrator::recover` resolves that window deterministically
instead of treating every in-progress directory as equally disposable, and
`migrate` calls it itself, under the lock and before planning, so retrying a
migration after a crash can never silently start over and ignore what was
stranded. The file-by-file check only covers the copied source; carrying over
destination-only data has no separate verification pass, so its safety comes
from that copy failing closed on any error, not from a checksum.

If both locations hold data, the default is to refuse and list the conflicts.
Guessing which copy of a kanban board is the good one is not a decision this
code is entitled to make. When you choose, the copy that loses is archived
beside itself, never deleted. Any copy failure while carrying destination-only
data into staging aborts the whole migration rather than dropping data
silently.

Task files migrate lazily. Reads fall back to the old location and startup
sweeps it, so an upgrade shows every task it showed before; the move happens on
the next save, after the new copy is on disk.

## Worktrees

SlashIt-managed worktrees live at

```text
<data_dir>/worktrees/<project-key>/<branch>
```

They used to be created as siblings of the repository (`<repo>.<branch>`),
which cluttered the folder holding your repositories.

Branch names may contain `/`, which cannot appear in a single path component.
Sanitising alone would make `feat/login` and `feat-login` collide, so when
sanitisation actually changes the name a short hash of the original branch is
appended. Names that need no sanitisation stay readable.

Nothing lands inside the project. The unavoidable exception is
`.git/worktrees/<name>`, which git itself maintains inside `.git/` and which is
invisible to the working tree.

SlashIt creates every worktree itself, with `git worktree add`. No other tool
is consulted, and having [worktrunk](https://github.com/max-sixty/worktrunk)
(`wt`) installed or not makes no difference. Earlier versions delegated
placement to `wt` when it was installed, but always with its hooks turned off
(`--no-verify`, `--no-hooks`), so no Worktrunk hook ever ran for a SlashIt
task either way.

`[worktree] placement` in `config.toml` accepts `"managed"` (written by
default) and `"auto"` (what earlier versions wrote), and both mean SlashIt's
own root. A configuration that still says `"auto"` needs no change. Going back
to an earlier version with it, though, would delegate to `wt` again on a
machine that has it; set `"managed"` first if that matters.

### Where a new task branch starts

An ordinary task branch starts at an exact commit resolved from local refs,
never at whatever the primary checkout has checked out, and SlashIt never
fetches to find it. When `refs/remotes/origin/HEAD` names a fetched branch of
`origin` (as `git clone` or `git remote set-head origin` records it), the
branch starts at the commit `refs/remotes/origin/<default>` names. In a
Jujutsu-colocated repository where that ref is missing, Jujutsu's `trunk()`
alias is used instead, but only when it is exactly `<branch>@origin` and that
branch has been fetched. Otherwise the branch starts at the project's local
base branch, `refs/heads/<base>`, recorded on the project when it was
registered or initialized, or chosen in Settings > Repository. A repository
with no remote at all runs tasks this way. With none of these the task is
refused, saying what to do: choose a base branch, or use Detect default
branch (`git remote set-head origin --auto`, run only when asked). See
[product-model.md](product-model.md#base-of-a-tasks-branch).

The branch is created with no upstream, whatever `branch.autoSetupMerge`
says, so SlashIt never points a bare `git push` or `git pull` in a task's
checkout at the default branch. A push refspec the user configures, such as
`remote.origin.push=HEAD:refs/heads/main`, still applies there as anywhere
else. SlashIt's own push, when it opens the pull request, records
`origin/<branch>` as the upstream. The pull request targets the default branch
the task was started from.

### Recording a new checkout

A task's checkout, its branch, and where that branch started are written to
the task file before SlashIt reports the checkout or starts an agent in it.
If that write fails, the Worktree command fails and the task does not start.
The board keeps showing what the file says, and a branch and worktree created
for that attempt are removed again, so the next attempt creates them afresh
and records where they started. Left in place, they would be adopted
unrecorded, and an adopted checkout has no known start. The removal is never
forced. A checkout that gained commits, modified or untracked files, or an
initialized submodule in the meantime is kept, as is one that is locked, and
the error names it. Ignored files are not protected: they are removed with the
checkout, as `git worktree remove` does. A worktree added for a branch that already existed, or an
existing worktree SlashIt adopted, is left as it is. After such a failure the
queue waits a minute before starting the same task again, rather than
creating and removing a checkout on every poll while the storage keeps
refusing writes.

### Adoption

An existing worktree is always adopted rather than recreated. Before creating
or reattaching anything, SlashIt checks the managed path, then the legacy
sibling path, then any other path git has registered for exactly the task's
branch, such as one `wt` placed under its own template, which stays where it
is. The repository's own checkout is never adopted: neither the Project's
path nor, when the Project is itself a linked worktree, the main worktree git
lists first. A task whose branch is checked out there is refused and told
where. An adopted worktree records no starting commit or origin, since its
`HEAD` is not where the branch started; the task keeps whatever it had
recorded.

A task that records no branch is given `task-<its whole id>`, so no two tasks
are given the same name. A task keeps the branch it records, whatever its
form: earlier versions used only the first eight hex digits of the id
(`task-<8 hex>`), two tasks can record the same such name, and git cannot say
which task a branch or worktree of that name belongs to. Before reattaching,
adopting, resuming or creating anything, SlashIt refuses the task, naming the
other task, when another task in the same git repository records the branch.
A task that records no branch is also refused while a local branch of the
`task-<8 hex>` name earlier versions would have given it exists and no task
in that repository records it: it may hold an earlier start of this task
whose record was never saved, or another task's work, and SlashIt neither
adopts it by prefix nor starts the task beside it. The refusal says how to
hand it to the task (`git branch -m task-<8 hex> task-<id>`, then, if no
worktree has it checked out, `git worktree add <directory> task-<id>`) or to
move it out of the way. Projects whose repositories are the same directory, or
linked worktrees of one repository, count as the same repository. A task whose
recorded worktree path no longer resolves is re-pointed at the worktree's
current location before the reference is treated as stale, unless another
task in the same repository records the same branch. Then neither task is
re-pointed: both keep the reference they had and say why. The previous
behaviour cleared the reference, stranding the branch and any uncommitted work
in it.

### A checkout whose directory is gone

A directory that is gone is not by itself a removed checkout. Git may still
register it, and while it does the checkout's path, and the branch it has
checked out, stay in use: `git worktree add` refuses both. SlashIt reads
`git worktree list --porcelain -z` and looks the checkout up twice, by the
path the task records and by the task's branch. The path finds a checkout
whose `HEAD` is detached, which holds no branch; the branch finds one git
recorded under a spelling of the path that can no longer be resolved to the
task's, such as through a symlink to a drive that is not mounted. Symlinks
are resolved along the path, including one whose target is gone, because git
records paths with symlinks resolved. Removal and startup decide on the same
two lookups:

- Nothing registered, by path or by branch: the checkout is gone. Removal
  reports it removed, and startup clears the task's reference.
- Registered and not locked (git calls it prunable): removal runs its
  ordinary `git worktree remove`, which for a directory that is gone only
  drops git's record of it, and reports the checkout removed once git no
  longer lists it. Startup clears the task's reference. Acquiring the task's
  checkout later finds such a registration of its branch, or at the path it
  is about to use. One that has a branch checked out is dropped the same way
  before a new checkout is added: every commit made in it is still on that
  branch. That removal is not forced, but it would still act on a directory
  that reappeared at that path between the check and git running. It refuses
  one holding modified or untracked files, so only ignored files are exposed.
  Git treats an unlocked registration of a missing directory as garbage:
  `git worktree prune` removes it, and git's documentation says to lock a
  checkout on a drive that is not always mounted. One whose `HEAD` is
  detached is never dropped automatically, because its `HEAD` and reflog live
  in the registration and may be the only thing naming commits made there.
  Acquisition refuses instead, and the task says which commit the detached
  `HEAD` is at, how to keep it (`git branch <name> <commit>`), and to run
  `git worktree prune` afterwards.
- Registered and locked, by either lookup: git refuses to clear it, and so
  does SlashIt. Removal fails and changes nothing, so the task keeps its
  reference, and startup keeps the reference too. Both name the lock, its
  reason and the branch it holds, and how to lift it: mount the drive the
  checkout is on, or, if it is gone for good, run `git worktree unlock <path>`
  in the repository. The next removal or start then converges, and startup
  drops the notice once the lock is gone, or once the checkout is found again
  where it was recorded or somewhere else. SlashIt never unlocks or forces
  anything.
- Git cannot be asked, or cannot say: removal fails and startup keeps the
  reference. That includes a repository directory that exists but is no
  longer a usable Git repository: the task keeps its reference, and cannot be
  finished or deleted, until the repository is fixed. Reading the listing
  needs git 2.36 or newer, for `-z`; with an older git a checkout whose
  directory was already gone cannot be confirmed removed. Removing a
  checkout that is there needs no listing: a `git worktree remove` that
  succeeds takes its registration down itself.

A task whose cleanup was interrupted is not cleared on these terms. Startup
reconciles it only when its directory is gone and git registers nothing for
it by path or by branch; otherwise the task stays quarantined until the
cleanup is asked for again, and says so, naming the lock if there is one.

Startup runs no git command that changes anything.

## Disk usage

Settings > Storage shows how much space SlashIt uses and how full the disk
holding its data directory is. Nothing is deleted, cleaned or pruned because
of it. The code is `src-tauri/src/storage_accounting/`; what each total means
is documented on `domain::storage_usage`.

When that disk is critically low, SlashIt pauses new task executions
(`queue/start_guard.rs`). Each start asks the filesystem afresh with one
`statvfs`-style call rather than reading the last measurement, and a check
that fails pauses new work too. Queued tasks stay queued and start on a later
scheduler pass once space returns. Warning pauses nothing, and running tasks,
AI reviews, Human Review decisions and pull request work are never stopped
or refused by it.

Only SlashIt's own roots are measured: the config, data, cache and runtime
directories above. Every path comes from `AppPaths`; nothing a task or board
records is walked on its say-so. Task Checkouts are found by listing
`<data_dir>/worktrees/<project-key>/` and then matched against the paths
tasks record, so a recorded path cannot point the walk anywhere else.
Checkouts recorded elsewhere (adopted, or at the legacy sibling path) are
counted but not measured. Repositories, boards kept inside a project, git's
own data in `.git/`, and shared tool caches are not measured either.

What each thing is:

- A checkout exactly one task records, under the worktree root of that
  task's own project, is that task's, and its files are *task files*. Any
  other directory under the worktree root, including one two tasks record
  or one a task of another project records, is *unrecognized*, as is any
  entry in SlashIt's directories that no version of SlashIt writes.
- Inside a checkout a task owns, a top-level `target/` or `dist/` is
  *rebuildable* only when it is a real directory on the same filesystem,
  is not a repository of its own, git reports it as ignored, and git tracks
  nothing inside it. The name alone never qualifies. Git is asked with
  `core.fsmonitor` off and optional locks off, so the question starts no
  hook or daemon and writes nothing, and each answer is bounded in time.
- Rebuildable output *could be freed* only while no agent (an execution,
  an AI review or a pull request helper) is attached to the task and the
  task is not In Progress, in AI Review or queued. What the user runs in a
  checkout themselves is not known. Nothing frees it today.
- A checkout whose cleanup was interrupted is measured but neither broken
  down nor offered, because a removal may be taking it apart.

Links are not followed, with a guarantee that differs by platform:

- On Unix the walk goes by directory handle, not by path. From SlashIt's
  root down, each directory is opened relative to its parent with
  `O_NOFOLLOW`, each entry is stat'ed relative to that handle, and a
  directory must still be the one that was listed (same device and inode)
  when it is opened. A directory swapped for a link, or for another
  directory, while the walk is running is reported as changed, never
  measured through its new target. A directory the walk has already
  opened is walked as that object, even if it is renamed out of the tree
  meanwhile. The walk does not cross into another filesystem.
- On Windows the walk goes by path, as the standard library does. It
  enters no entry it sees as a reparse point (junctions, mounted volumes,
  cloud placeholders), and refuses a reparse point between SlashIt's root
  and the item. After listing a directory it checks the path again and
  discards the listing if the path is no longer a plain directory or its
  creation time, where the filesystem reports one, has changed. That
  catches many replacements, but it is not a check of identity: creation
  time can be missing or set to anything. A directory swapped and
  restored between the listing and that check, or replaced by one that
  looks the same, is not caught, and its entries can be counted as the
  original's. Closing that needs handle-relative enumeration.

Which item a byte is attributed to, and whether it counts as rebuildable,
is decided by path at one moment; a tree rearranged during a measurement
can be attributed differently. On Unix no link or path swap can steer the
walk out of the tree below SlashIt's root, though a directory already
opened is followed as that object wherever it is moved. None of this
makes a measurement a basis for deleting anything: a cleanup must
establish what it removes for itself.

The measurement counts allocated space where the
platform reports it (Unix) and file length otherwise (Windows), counting
a hard-linked file once per walk on Unix; a file linked into two items is
counted in each. An entry that cannot be read makes the
item *partial*, and one that cannot be measured at all is listed as such,
never as zero bytes. So does an item that is itself a link (a worktree
root moved to another disk, say) or that holds another filesystem: what it
points to is not measured, and the totals say they are lower bounds. The
links and mounts not followed are counted and shown.

Measuring runs only when asked, off the UI thread, one measurement at a
time: a Refresh that arrives while one is running waits for it instead of
starting another, and the previous result stays visible meanwhile.

Disk pressure is Warning below the larger of 15% of the filesystem and
120 GiB available, and Critical below the larger of 5% and 40 GiB, where the
40 GiB floor is capped at 20% of the filesystem: a 100 GiB disk is Critical
below 20 GiB, not 40. Critical pauses new task executions, and Settings >
Storage and that pause read the same threshold.

## Directories by platform

| Root | Linux | macOS | Windows |
|---|---|---|---|
| config | `~/.config/slashit-app` | `~/Library/Application Support/com.barradev.slashit-app` | `%APPDATA%\barradev\slashit-app\config` |
| data | `~/.local/share/slashit-app` | same as config | `%APPDATA%\barradev\slashit-app\data` |
| runtime | `$XDG_RUNTIME_DIR/slashit-app` | `<tmp>/slashit-<uid>` | `<tmp>\slashit-<username>` |

The `$XDG_RUNTIME_DIR` check itself is not Linux-only — the resolver checks it
on every platform and only falls back to the qualified temp path when it is
unset or empty. The table lists the fallback for macOS and Windows because
that is what an unmodified install actually has; if something in your
environment sets `XDG_RUNTIME_DIR` there too (a Nix shell, a container
toolchain, dotfiles sourced from a Linux setup), the runtime directory and PID
file follow that instead.

Config, data and cache come from the `directories` crate. Runtime does not:
`AppPaths` defers to `slashit_ipc::endpoint::runtime_dir()`.

That is not a stylistic preference. `ProjectDirs::runtime_dir()` returns
`None` off Linux, so its fallback — a fixed `<tmp>/slashit` — was the *normal*
path on macOS and Windows rather than an edge case, and it was wrong twice
over. It named a different directory than the one the IPC crate actually binds
in, so `socket_path()` and `pid_file()` described a directory nothing had ever
listened in; and an unqualified name in a world-writable temp directory can be
pre-created by another local user before SlashIt gets there. One resolver, in
the crate that does the binding, is the only arrangement in which the socket
and the PID file cannot drift apart.

The qualifier differs by platform because the thing it has to disambiguate
does: a uid where the runtime directory is a filesystem path, the username on
Windows, where named pipes live in a machine-global namespace. On Windows the
control channel *is* a named pipe (`\\.\pipe\slashit-<username>`) rather than
a file, so the runtime directory there holds only the PID file.

The application identifier `("com", "barradev", "slashit-app")` is frozen.
Changing its third component would orphan every existing user's projects and
tasks. It is independent of every Rust package and binary name in the
workspace — `slashit-ui`, `slashit-frontend`, `slashit`, `slashit-ipc`,
`slashitd` — so renaming any of those is safe; renaming this is not.
