# AGENTS.md (slashit-acceptance)

Binding rules for changing the desktop acceptance harness and its journeys.
[README.md](README.md) is authoritative for how to run the harness, its
configuration, and its failure artifacts. Do not copy it here; when a rule
below and the README disagree, fix whichever is wrong in the same change.
Paths below are relative to this crate.

## Journeys and the shard manifest

- Every new product journey (under `tests/product_acceptance/`) must appear
  exactly once in [shard-manifest.txt](shard-manifest.txt), in shard `1`, `2`
  or `3`. No other shard number is valid.
- Renaming, moving or removing a journey changes the manifest in the same
  checkpoint. A missing, stale or duplicated entry fails the lint job.
- Enumerate the names with the command in the manifest's header. Rebalance
  shards with hosted journey timings; the checker proves assignment, not
  balance.
- Run `scripts/check-acceptance-shards.sh` before publishing a checkpoint that
  touches journeys or the manifest.
- The harness suite (`tests/acceptance.rs`) is not sharded and is not listed in
  the manifest.

## Building and running

- Run `scripts/build-acceptance-app.sh` before any desktop journey. A plain
  `cargo build`, `cargo test -p slashit-ui` or `cargo clippy` replaces
  `target/debug/slashit-ui` with a binary that has no embedded frontend. Rebuild
  after any such command, and after any frontend change. Never test against a
  binary you have not rebuilt from the source you are checking.
- Run journeys with `scripts/run-desktop-acceptance.sh --exact <name>` or
  `--shard <N>`. Journeys run one at a time.

## Isolation and process ownership

- A journey uses only its own state root and its own dynamic ports. Never read
  or write the developer's real SlashIt state, the real home configuration, or
  a fixed port. Port allocation lives in `src/ports.rs`.
- Start every provider and application process through the harness's process
  group (`src/process.rs`), so teardown can signal the whole group. Do not leave
  a child running after the journey ends; the harness verifies this.
- `claude` and `gh` are fixtures (`src/fake_agent.rs`, `src/fake_gh.rs`). A
  journey must not reach a real model provider, GitHub, or the CodeRabbit
  service.

## Waiting and assertions

- Wait on a condition with a deadline, never on a fixed sleep. Readiness is
  observed, for example a visible element or a listed task.
- A failing check names the step that failed and the observed value, so the
  failure is diagnosable without rerunning it.

## Evidence

- A hosted shard result is evidence only for the revision CI built. Name the
  commit a local run used.
- Failure artifacts (screenshots, page source, provider logs) stay local. Only
  the allowlist in the README is uploaded.

## Platform

The runtime is implemented for Linux only. Do not add platform-specific code
outside the `#[cfg]` gates the README describes.
