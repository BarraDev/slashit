# Portable Project export and import preview (format v1)

Status: export, read-only import preview, and import apply are implemented
behind the IPC protocol and the `slashit` CLI. Apply creates one Project and
its Tasks as inert records. It creates no Repository and no runtime authority.
The desktop interface for import is not implemented.

Code: `src-tauri/src/portability/` (`bundle.rs` format, `export.rs` projection
and write, `preview.rs` validation against the target, `apply.rs` the import
commit).

## Invocation

```text
slashit export <project-id> <new-file>
slashit import --preview <file>
slashit import --apply <file> --checksum <sha256>
```

Each is one IPC request (`ExportProject`, `PreviewImport`, `ApplyImport`). The
CLI turns a relative path into an absolute one first, because the instance
writes relative to its own working directory. `--checksum` is the payload
checksum that `--preview` reports for the same file. An apply refuses any other
payload, so the user confirms exactly what was previewed.

## Apply

Apply validates the file again under the locks it commits with. It does not
trust an earlier preview. The identity rules are the preview's, so an apply
refuses whenever a preview would report a conflict.

Created, and kept as the bundle states them:

- The Project under its exported id. Its `repository_id` is empty, and its
  agent configuration is the default for its agent type. The exported model is
  kept. No command or credential is read from the bundle.
- Each Task under its exported id, with its dependencies, review history,
  timeline and Git identifiers as text. Runtime fields start at their defaults.

Changed, and reported in `normalization`:

- A current approval is withdrawn by closing its review arrival. The decision
  stays in the history and nothing is current.
- A Task in Human Review returns to Backlog, because a review wait needs the
  checkout the bundle does not carry.
- A reviewer model is trimmed, or cleared when it is empty or whitespace-only.
  Reported as `reviewer_model_trimmed` or `reviewer_model_cleared`. An absent
  model is stored as none and is not reported.

Kept as records, not verified: QA sign-off, review decisions and the timeline.
They show what the exporting instance recorded. They confer no authority,
because approvals are withdrawn and the apply creates no checkout.

Refused on read, in preview and apply alike: a Task whose reviewer model would
be rejected when it is created. An export of such a Task still succeeds. An
accepted reviewer model is stored as the parser returns it: an empty or
whitespace-only value stores no reviewer, and surrounding whitespace is removed.

Not created: a Repository, a checkout, a branch, a pull request, a worker run,
an integration, or any Git object. Historical Git identifiers are not checked
against the importing repository.

### Commit order and failure results

Projects and Repositories are stored in `config.toml`. Each Project's Tasks are
stored in their own board file, and startup loads every board file present. A
single atomic write cannot cover both, so the commit runs in this order:

1. Write the import record, `<id>.import-pending` beside `config.toml`. It
   tells a restart that an import of this Project is unfinished.
2. Stage the board beside its destination. Nothing is visible yet.
3. Commit the Project to `config.toml`. This is the commit point.
4. Publish the board with a no-clobber publication.
5. Remove the import record and insert the Tasks into memory. The record is
   removed only when the configuration entry and the board are both confirmed
   durable; otherwise it stays for the next start.

Results:

- A refusal before step 1 changes nothing. A record that cannot be written
  refuses the import, and nothing is committed.
- A failure before step 3 removes the record and leaves nothing behind.
- A failure of step 4 before the board is visible reverts step 3, removes the
  record, and the target is unchanged (`reverted`).
- A failure of that revert keeps the record and reports the import as
  incomplete. The next start settles it (see Recovery below). Until then the
  Project exists without Tasks, and no repository is linked, so it cannot run
  work.
- A board that is published but not confirmed durable is reported as an
  applied import with `durable: false`. The change is in place, so it is not
  reported as a failure. A record that is in place but not confirmed durable
  also gives `durable: false`.
- A repeated apply of the same bundle is refused by the identity rule, so an
  import cannot be committed twice.
- An apply is refused, and changes nothing, while any import record, Task board,
  configuration entry, or pending Task or Conversation cleanup for the same
  Project id is still on disk. This covers a Project held back by an earlier
  import: the preview sees only memory, so the disk is checked as well. A
  cleanup that is still owed would otherwise delete what the import created.
- Two concurrent applies of one bundle serialize on the Project's lifecycle
  gate. Exactly one commits.

### Recovery at startup

A record left behind marks an import that did not finish. At startup, before
any Project is used, and only with a configuration read without salvage:

- No Project and no board under the record: the import never committed. The
  record is removed.
- A Project with its board: the import completed. The Project is kept. The
  record is removed once the configuration is confirmed durable. On a
  filesystem that cannot synchronize a directory that never happens, so the
  record stays and the Project loads on every start until a start can confirm
  the configuration. The record is what holds back a Project whose entry or
  board a power loss could drop, so it is not removed on an unconfirmed start.
  The configuration's confirmation covers only the configuration's own
  directory. The record is also removed only after the directory of the Task
  board is synchronized at the same start. A board directory that cannot be
  synchronized keeps the record, and the Project loads on every start with a
  diagnostic until a start can synchronize it. A directory that cannot be
  synchronized at all keeps the record indefinitely; that is the visible,
  deferred cleanup the import prefers over deleting a board that might not be
  durable. What a clearing start proves: the configuration entry and the board's
  directory entry were each reported synchronized, and the board's bytes were
  synchronized when they were written. The tests simulate the directory answers
  with the durable-filesystem fault seam; they do not cut power.
- An interrupted import whose Project entry is removed, or whose record has no
  Project, is settled the same way: the record stays while the configuration
  is unconfirmed, because a removal written there may return after a power
  loss, and the record is what repeats it. An import whose id has a record
  stays refused until a start can confirm the configuration, which needs a
  filesystem that synchronizes directories.
- A Project without its board: the import was interrupted. Only this Project is
  removed from `config.toml`, so the bundle can be imported again. No other
  record or board is touched.
- A board with no Project in `config.toml` but with the record: the Project
  entry may have been lost after a non-durable commit. The board is held back
  (its Tasks are not loaded), and the record and board are kept. Nothing is
  adopted or deleted.

A configuration without a `projects` section is not authoritative, so a record
written before the first Project was saved waits until a configuration exists.
It is harmless: a retry overwrites it, and a later start settles it.

Records that cannot be trusted quarantine their Project instead:

- A record whose content does not name the Project in its file name (malformed,
  empty, or naming another Project), or whose file name is not a Project id, is
  not used to infer anything. The Project is kept out of this process together
  with its Tasks, and the record and board stay on disk for repair.
- When the configuration is not authoritative (it does not parse strictly, or
  has no `projects` section), every record's Project is quarantined the same way.
  Nothing is rewritten. The salvage reader recovers no Projects today, so what
  is held back is the Task board, which would otherwise load with no Project to
  own it.
- When the record directory cannot be listed, every Project is held back.
- A held-back Project keeps its `config.toml` entry through any later save in
  the same process, so a settlement or a Project save cannot drop it. Its
  Conversation and Task deletion records are not acted on while it is held back.

Once the record is repaired or the configuration loads completely, the next
start applies the rules above. Nothing is deleted for a quarantined Project.

Not covered: a Task board with neither a Project entry nor a record is still
loaded by startup with its Tasks, as before this change. Nothing in the import
path owns such a board, and it can block a later import with "a Task board
already exists". The import refuses and nothing is overwritten, but the board is
not adopted or removed automatically. Removing it needs a deliberate owner
decision; no automatic repair exists.

Covered by tests: the record write refusal, an unconfirmed record, a failed
revert followed by a restart through the production startup path, a record with
no Project, a record beside a completed board, an unrelated Project that
survives recovery, the three untrusted-record contents (each leaving the record
and board in place, the configuration unchanged, and the Tasks unloaded), a
completed import under a salvaged configuration (its Tasks not loaded, and
restored once the configuration loads), a board whose Project entry is missing,
a re-import refused over a held-back record, a Conversation deletion that is not
run for a held-back Project, a non-canonical record name, a held-back entry that
survives a sibling settlement and a later save, and an unconfirmed configuration
that keeps its record. Negative controls showed that the Task filter and the
held-back preservation are each required. The restart tests are not a real process kill: a crash
that loses unflushed page-cache data is not simulated.

## The file

One JSON object:

| Member | Meaning |
|---|---|
| `format` | Always `slashit.project-export` |
| `format_version` | `1`. A higher value is refused by name, before any strict parsing |
| `build` | Informational: the exporting build's version |
| `created_at` | Informational: when the export was written |
| `manifest` | Counts and the omitted-category list. Checked against the payload on read |
| `checksum` | SHA-256 (lowercase hex) of the exact bytes of `payload` as written |
| `payload` | The Project, its Repository identity, and its Tasks |

Files are created owner-only (`0600` on Unix), written atomically, and never
replace an existing path. A bundle is refused above 64 MiB. If the file is in
place but its directory entry could not be confirmed durable, the export
reports success with `durable: false`, so a retry is not needed and not
allowed by the no-clobber rule.

## What the payload carries

- Project: id, name, agent type, model, base branch name, timestamps.
- Repository: id, remote URL and type. The URL has credentials, query and
  fragment removed.
- Task: id, title, description, status, model, provider, review mode, reviewer
  model, planning mode, dependencies, categories and priority, subtasks,
  sequence number and position, external references, legacy issue and PR URLs,
  QA sign-off, Human Review arrivals and decisions, base commit as text,
  timeline, timestamps.
- Timeline: every entry except raw run output and tool calls. Free text is
  credential-redacted, and absolute machine paths are replaced with `<path>`.

## What is never exported

The manifest's `omitted` list is the authoritative version of this list.

- Credentials and environment: `api_key`, `env`, `command`, `args`.
- Runtime and execution authority: `worktree_path`, `branch_name`,
  `approved_execution`, `executed_approvals`, `new_attempt`,
  `worker_handoff`, `pending_republish`, `pr_create_unconfirmed`,
  `pr_publication`, `local_integration`, `cleanup_in_flight`,
  `checkout_admission`, `pr_review_plan`, and the Human Review delivery error.
- Locations and scope: the repository's local path, Project scope, and the
  state-location preference.
- Conversations, provider output (`RunResult`) and tool calls (`ToolUsed`,
  `ToolsOmitted`).
- Git content. No repository file is read or written.

## Refusal rules

Export is refused, and writes nothing, while any Task of the Project:

- is `queue`, `in_progress` or `ai_review`;
- has unresolved Worker recovery evidence, an unfinished restack, an
  interrupted cleanup, a pending local integration, an unconfirmed pull request
  creation, a pending or unknown publication, or a pending approval to start a
  run.

Every blocked Task is named in one refusal. `human_review` with a checkout is
allowed: a decision wait is not an active run.

## Lock order

Apply, Preview, Export, Task creation and Project deletion take the in-memory
locks in one order: the Project lifecycle gate, then Conversation and per-Task
lifecycle leases where the operation needs them, then Tasks, then Projects, then
Repositories. Leases are timed, so a wait for one ends in a refusal, not a hang. Project creation and Repository commands take one of these locks
at a time. A handler that took Projects before Tasks could hold Projects
while it waited for Tasks, and an Apply holding Tasks while it waited for
Projects would then deadlock with it. `ipc::handlers::import_lock_order` parks
Export and Preview at the Repositories lock and checks that they already hold
Tasks.

## Snapshot consistency

Export takes the Project's lifecycle gate in shared mode and every Task
lifecycle lease of the Project, then reads the Tasks, the Project and its
Repository from the in-memory maps under their read locks, in the lock order
above, and releases everything before writing the file. A deletion of the Project, a run start or a lifecycle
transition therefore cannot interleave with the read. Export does not read the
board files or the config file.

The guarantee is narrower than one atomic snapshot of the whole Project:

- The three map read locks are acquired one after another. A write that
  changes the Repository or the Project record without the Project gate can
  land between those acquisitions, so the Project and its Repository may come
  from different instants.
- A Task text or dependency edit that takes no lease is excluded only by the
  read lock on the Task map. The Tasks are read at one instant, which may differ
  from the instant of the Project and Repository records.
- The concurrent-write behavior has not been stress-tested. The claims above
  rest on lock-order analysis and barrier tests, not on a torn-read stress run.

Lock order: project gate, then Task leases, then the shared maps. This matches
the existing Project deletion path.

Publication never replaces a file: the bytes are staged beside the destination
and linked into place, and that link fails if the destination exists, including
a file created after the existence check.

## Preview

Preview validates the file in this order: header, format and version,
envelope, checksum, payload parse, manifest, then structural rules:

- A Task id appears once, and the dependency graph is acyclic and closed inside
  the bundle.
- No Task is in an active state.
- Every timeline entry parses as a known kind, and run output or tool calls are
  refused.
- Review entries are in order and do not name an arrival beyond the record's
  count.
- The repository remote is one plain token. A remote with user information is
  refused as unsafe, since a credential would sit there. The project base is a
  branch name (a leading dash is refused, so it can never read as an option).
- A Task's issue or pull request address must not contain `@` (refused as
  unsafe, on decode and on export). Export strips user information from the
  authority of a `://` address before the check, so that form is exported with
  the credential removed. Any other spelling that carries an `@` is refused, and
  nothing is written. A `://` address's host part must be a host with an optional
  numeric port (otherwise refused as malformed).
- Historical Git evidence (base commit, approved tip, pull request merge commit)
  is a 40- or 64-character hexadecimal identifier. The shape is checked, and
  nothing requires the commit to exist in the importing repository.

Export runs the same structural rules before it writes, so an export is refused
exactly when its own Preview would refuse it (for example, a Task depending on a
Task that is not in the Project).

A bundle that fails any rule is refused. A valid bundle is reported with:

- identity conflicts: Project id, Repository id, Repository remote, Task ids,
  and any unreadable target board (which makes a clean verdict impossible);
- `normalization`: a current approval in the bundle, which import must
  withdraw before anything can act on it, and the reviewer-model changes listed
  above; Apply reports the same list in its result;
- `git_references`: base commits and approved tips, carried as text and never
  recreated or checked;
- `excluded`: the omitted list;
- `importable`: true only when no conflict was found.

Preview reads only the file and the instance's in-memory maps. The file must be
a regular file: it is opened without blocking, so a named pipe is refused rather
than waited on, and it is read to at most 64 MiB plus one byte. It never calls
`Storage::load_config`, which can copy or rename a damaged `config.toml`
(verified by test), and it never loads a board. A test compares the config and
data trees byte for byte around a preview.

## Checksum and secrets

The checksum detects accidental damage and edits that did not also rewrite
the checksum. It is not authentication: anyone able to write the file can
write a matching checksum.

The bundle is not secret-free. The exclusions above remove known credential
fields and runtime authority. User-authored titles, descriptions, feedback and
provider-written reasons can still contain secrets, and the redaction pass is
best-effort. Treat an export as sensitive user data. Path scrubbing applies to
whitespace-delimited tokens that start with `/`; a path written inside a
longer unspaced fragment can survive.

## Limits and what is not done

- No Repository is recreated, so an imported Project cannot run work until a
  local checkout is linked to it. No Git reconciliation, restack, branch
  creation, worker resume, approval replay or pull request publication.
- No desktop interface. The CLI and IPC are the only import entry points.
- Export, Preview and Apply are refused to a peer the operating system has not
  identified, like the verbs that create agent work. Preview reads a
  caller-named file and Export writes one, so each refusal happens before the
  file is opened or created. Integration tests drive them through the assembled
  server over TCP with a valid token. The Apply test checks that nothing changed,
  the Preview test checks that the refusal does not depend on the file, and the
  Export test checks that no destination is created or overwritten. See
  `docs/architecture/ipc-security.md`.
- A repository remote in the scp form with a colon in its user part
  (`user:secret@host:path`) is refused on export and on decode. An ordinary
  `user@host:path` remote, such as `git@host:owner/repo.git`, is accepted.
- Forms that are not refused, and why. Each was reviewed and judged not to be a
  credential under Git's own parsing, so refusing them would also refuse valid
  remotes or addresses. They are documented, not sanitized:
  - A `://` remote whose text before the first `/` has a colon but no `@`
    (`https://user:s/ecret@host/path`). Git reads that text as host and port and
    rejects it because `s/ecret` is not a port (checked with `git ls-remote`), so
    it is not a usable credential. The export keeps the text as written. A colon
    in an authority is also how a valid port is written, so remotes are refused
    only when the authority has an `@`.
  - An scp-form remote with a percent-encoded colon (`deploy%3ASECRET@host:path`).
    Git does not percent-decode it before handing it to `ssh` (not checked beyond
    the hostname lookup). An scp-form remote with a `/` before its
    colon (`us/er:secret@host:path`) is read by Git as a local path.
- The Preview-to-Apply check is the checksum of the payload. It detects a file
  that changed, and it does not authenticate the file.
- Export refuses to replace any existing path, including one created after
  its check, but it is owner-only, not authenticated: another local process
  that runs as the same user can still read or change the file.
- The export needs a filesystem with hard links; on one without them the
  write fails closed and nothing is published.
- Conversations are not exported in v1.
- Export reads the live instance. A CLI or desktop session must be running.
- A Task whose id, repository or remote matches the target is a conflict. The
  bundle never picks fresh identifiers.
