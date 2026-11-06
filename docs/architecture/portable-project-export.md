# Portable Project export and import preview (format v1)

Status: export and read-only import preview are implemented behind the IPC
protocol and the `slashit` CLI. Import apply is not implemented. Nothing in
this version creates, changes or activates a Project or Task on import.

Code: `src-tauri/src/portability/` (`bundle.rs` format, `export.rs` projection
and write, `preview.rs` validation against the target).

## Invocation

```text
slashit export <project-id> <new-file>
slashit import --preview <file>
```

Both are one IPC request each (`ExportProject`, `PreviewImport`). The CLI
turns a relative path into an absolute one first, because the instance writes
relative to its own working directory. `slashit import` without `--preview` is
refused.

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
transition therefore cannot interleave with the read. A Task text or
dependency edit that does not take a lease is not excluded by the leases; the
single read of the Task map is the only guarantee against it. Export does not
read the board files or the config file.

Lock order: project gate, then Task leases, then the shared maps. This matches
the existing Project deletion path.

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

A bundle that fails any rule is refused. A valid bundle is reported with:

- identity conflicts: Project id, Repository id, Repository remote, Task ids,
  and any unreadable target board (which makes a clean verdict impossible);
- `normalization`: a current approval in the bundle, which import must
  withdraw before anything can act on it;
- `git_references`: base commits and approved tips, carried as text and never
  recreated or checked;
- `excluded`: the omitted list;
- `importable`: true only when no conflict was found.

Preview reads only the file and the instance's in-memory maps. It never calls
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

- No import apply, no status normalization, no arrival bump, no Git
  reconciliation. Those are the next unit and need their own review.
- A file another process creates between the final existence check and the
  rename is replaced. A file written by a process that does not check first
  is not protected.
- Conversations are not exported in v1.
- Export reads the live instance. A CLI or desktop session must be running.
- A Task whose id, repository or remote matches the target is a conflict. The
  bundle never picks fresh identifiers.
