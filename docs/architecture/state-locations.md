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

- **Outside the project** (default) — `<data_dir>/projects/<project-key>/`.
  Your repository is untouched.
- **Inside the project** — `<repo>/.slashit/`. Opt in when you want the board
  committed and shared with your team.
- **Detect automatically** — use `.slashit/` if it already exists and has
  content, otherwise stay outside.

Projects saved before this setting existed load as *Detect automatically*
rather than the type's own `External` default. An existing install may already
have a populated `.slashit/`, and silently switching it to external storage
would look like data loss.

An empty `.slashit/` does not count as opting in. That is precisely the dead
directory older versions created, and treating it as a signal would resurrect
the bug.

## Project keys

External state is keyed by

```
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

The migration itself is: plan, lock, copy to a staging directory, verify,
atomically rename into place, and **only then** delete the source. It is
therefore safe across filesystems, idempotent, and recoverable if interrupted:
at no point does the only copy of your board not exist.

If both locations hold data, the default is to refuse and list the conflicts.
Guessing which copy of a kanban board is the good one is not a decision this
code is entitled to make. When you choose, the copy that loses is archived
beside itself, never deleted.

Task files migrate lazily. Reads fall back to the old location and startup
sweeps it, so an upgrade shows every task it showed before; the move happens on
the next save, after the new copy is on disk.

## Worktrees

SlashIt-managed worktrees live at

```
<data_dir>/worktrees/<project-key>/<branch>
```

They used to be created as siblings of the repository (`<repo>.<branch>`),
which cluttered the folder holding your repositories.

Branch names may contain `/`, which cannot appear in a single path component.
Sanitising alone would make `feat/login` and `feat-login` collide, so when
sanitisation actually changes the name a short hash of the original branch is
appended. Names that need no sanitisation stay readable.

### Worktrunk

If [worktrunk](https://github.com/max-sixty/worktrunk) (`wt`) is installed,
SlashIt delegates placement to it by default. `wt switch` accepts no target
path — the location comes from your own `worktree-path` template — and that is
the point: it is your tool and your hooks. Set `worktree.placement = "managed"`
in `config.toml` to take the decision back and always use SlashIt's own root.

Either way nothing lands inside the project. The unavoidable exception is
`.git/worktrees/<name>`, which git itself maintains inside `.git/` and which is
invisible to the working tree.

### Adoption

An existing worktree is always adopted rather than recreated. On upgrade,
SlashIt checks the managed path and then the legacy sibling path before
creating anything, and a task whose recorded worktree path no longer resolves
is re-pointed at the worktree's current location before the reference is
treated as stale. The previous behaviour cleared the reference, stranding the
branch and any uncommitted work in it.

## Directories by platform

| Root | Linux | macOS | Windows |
|---|---|---|---|
| config | `~/.config/slashit-app` | `~/Library/Application Support/com.barradev.slashit-app` | `%APPDATA%\barradev\slashit-app\config` |
| data | `~/.local/share/slashit-app` | same as config | `%APPDATA%\barradev\slashit-app\data` |
| runtime | `$XDG_RUNTIME_DIR/slashit-app` | temp dir | temp dir |

The application identifier `("com", "barradev", "slashit-app")` is frozen.
Changing its third component would orphan every existing user's projects and
tasks. It is independent of the binary name, so renaming an executable is safe;
renaming this is not.
