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

Not created: a Repository, a checkout, a branch, a pull request, a worker run,
an integration, or any Git object. Historical Git identifiers are not checked
against the importing repository.

### Commit order and failure results

Projects and Repositories are stored in `config.toml`. Each Project's Tasks are
stored in their own board file, and startup loads every board file present. A
single atomic write cannot cover both, so the commit runs in this order:

1. Stage the board beside its destination. Nothing is visible yet.
2. Commit the Project to `config.toml`. This is the commit point.
3. Publish the board with a no-clobber publication.
4. Insert the Tasks into memory.

Results:

- A failure before step 2 leaves nothing: the staged file is removed.
- A failure of step 3 before the board is visible reverts step 2. The apply
  reports `reverted`, and the target is unchanged.
- A failure of that revert leaves the Project recorded without its Tasks. The
  apply reports it as incomplete. This is the one state this version cannot
  undo. Delete that Project before importing again, and keep the bundle file.
- A board that is published but not confirmed durable is reported as an
  applied import with `durable: false`. The change is in place, so it is not
  reported as a failure.
- A repeated apply of the same bundle is refused by the identity rule, so an
  import cannot be committed twice.
- Two concurrent applies of one bundle serialize on the Project's lifecycle
  gate. Exactly one commits.

A crash between step 2 and step 3 is not covered by a test. Its result is the
same incomplete state as the revert failure above, and recovery is the same.

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

## Snapshot consistency

Export takes the Project's lifecycle gate in shared mode and every Task
lifecycle lease of the Project, then reads the Project, its Repository and its
Tasks from the in-memory maps under their read locks, and releases everything
before writing the file. A deletion of the Project, a run start or a lifecycle
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
  withdraw before anything can act on it;
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
- A crash between the Project commit and the board publication is not covered
  by a test (see Commit order above).
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
