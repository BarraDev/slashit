#!/usr/bin/env bash
# Self-test for scripts/check-agent-docs.sh.
#
# Copies the repository's files into a throwaway Git repository, breaks one
# rule at a time in a fresh copy, and asserts that the checker fails with the
# expected check (or, for the allowed cases, still passes). The repository
# itself is never modified.

set -euo pipefail
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$root"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/agent-docs-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

if top=$(git rev-parse --show-toplevel 2>/dev/null) && [ "$(cd "$top" && pwd -P)" = "$root" ]; then
  git ls-files --cached --others --exclude-standard >"$tmp/list"
elif command -v mise >/dev/null 2>&1 && [ -f mise.toml ]; then
  mise exec -- jj file list >"$tmp/list"
else
  jj file list >"$tmp/list"
fi

base="$tmp/base"
mkdir "$base"
while IFS= read -r f; do
  [ -e "$f" ] || continue
  mkdir -p "$base/$(dirname "$f")"
  cp -p "$f" "$base/$f"
done <"$tmp/list"
git -C "$base" init -q

failures=0
n=0

nested_entry() { printf -- '- `%s` -- test\n' "$1" >>AGENTS.md; }
export -f nested_entry

# probe <name> <expected exit> <expected output pattern or -> <shell mutation>
#
# A mutation that cannot be applied fails the probe, and the remaining probes
# still run. It runs in a child shell with errexit so that a failure anywhere
# in it counts (errexit is ignored inside a `||` list in this shell). In
# `a && b; c`, a failing `a` does not stop the mutation; separate steps that
# can fail with `;` or a newline.
probe() {
  local name=$1 want_status=$2 want_output=$3 mutation=$4 dir status
  n=$((n + 1))
  dir=$(mktemp -d "$tmp/probe.XXXXXX")
  cp -R "$base/." "$dir"
  status=0
  bash -eo pipefail -c 'cd "$1" && eval "$2"' mutation "$dir" "$mutation" >"$dir.setup" 2>&1 || status=$?
  if [ "$status" -ne 0 ]; then
    printf 'FAIL %s: mutation setup failed (exit %s)\n' "$name" "$status"
    sed 's/^/    /' "$dir.setup"
    failures=$((failures + 1))
    return 0
  fi
  (cd "$dir/src" && ../scripts/check-agent-docs.sh) >"$dir.out" 2>&1 || status=$?
  if [ "$status" -ne "$want_status" ] ||
    { [ "$want_output" != - ] && ! grep -Eq -- "$want_output" "$dir.out"; }; then
    printf 'FAIL %s: exit %s, wanted %s matching %s\n' "$name" "$status" "$want_status" "$want_output"
    sed 's/^/    /' "$dir.out"
    failures=$((failures + 1))
  else
    printf 'ok   %s\n' "$name"
  fi
}

# The harness itself: a failing mutation is reported as a probe failure, is
# counted, and does not stop the run. Checked in a separate process with
# errexit on, as in this script (errexit is ignored inside a command
# substitution used as a condition), so that the expected failure does not
# count against this run.
n=$((n + 1))
export -f probe
export tmp base
out=$(bash -euo pipefail -c 'failures=0 n=0
  probe "setup that fails" 0 - "false; : >never-reached"
  if compgen -G "$tmp/probe.*/never-reached" >/dev/null; then echo "mutation ran on after a failure"; fi
  printf "continued with %s failure(s)\n" "$failures"' 2>&1) || true
if grep -q '^FAIL setup that fails: mutation setup failed' <<<"$out" &&
  ! grep -q 'mutation ran on' <<<"$out" &&
  grep -qx 'continued with 1 failure(s)' <<<"$out"; then
  printf 'ok   %s\n' "a failing mutation setup is reported and the run continues"
else
  printf 'FAIL %s\n' "a failing mutation setup is reported and the run continues"
  sed 's/^/    /' <<<"$out"
  failures=$((failures + 1))
fi

probe "unchanged tree passes" 0 'agent-docs: OK' ':'
probe "CLAUDE.md with content" 1 '\[A\] src/CLAUDE.md: must contain' \
  'printf "extra\n" >>src/CLAUDE.md'
probe "AGENTS.md without CLAUDE.md" 1 '\[A\] src/AGENTS.md: no sibling CLAUDE.md' \
  'rm src/CLAUDE.md'
probe "CLAUDE.md without AGENTS.md" 1 '\[A\] docs/CLAUDE.md: no sibling AGENTS.md' \
  'printf "@AGENTS.md\n" >docs/CLAUDE.md'
probe "unlisted module" 1 '\[B\] src-tauri/src/newmod: module directory has no entry' \
  'mkdir src-tauri/src/newmod && : >src-tauri/src/newmod/mod.rs'
probe "unlisted module with a space" 1 '\[B\] src/new module: module directory has no entry' \
  'mkdir "src/new module" && : >"src/new module/a b.rs"'
probe "manifest entry for a missing module" 1 '"src-tauri/src/ghost" does not exist' \
  'printf "\"src-tauri/src/ghost\" = \"documented\"\n" >>docs/agent-docs.toml'
probe "documented module without AGENTS.md" 1 'is documented but has no AGENTS.md' \
  'mkdir src/newmod && : >src/newmod/x.rs && printf "\"src/newmod\" = \"documented\"\n" >>docs/agent-docs.toml'
probe "covered-by doc that does not name the module" 1 'does not mention "newmod/"' \
  'mkdir src/newmod && : >src/newmod/x.rs && printf "\"src/newmod\" = \"covered-by:src/AGENTS.md\"\n" >>docs/agent-docs.toml'
probe "covered-by doc that is not an ancestor" 1 'is not in an ancestor directory' \
  'mkdir src/newmod && : >src/newmod/x.rs && printf "\"src/newmod\" = \"covered-by:src-tauri/src/AGENTS.md\"\n" >>docs/agent-docs.toml'
probe "duplicate manifest entry" 2 'duplicate module "src/newmod"' \
  'printf "\"src/newmod\" = \"documented\"\n\"src/newmod\" = \"documented\"\n" >>docs/agent-docs.toml'
probe "malformed manifest entry" 2 'expected: "<module dir>"' \
  "printf \"'src/newmod' = 'documented'\n\" >>docs/agent-docs.toml"
probe "unknown classification" 2 'unknown classification "covered"' \
  'printf "\"src/extra\" = \"covered\"\n" >>docs/agent-docs.toml'
probe "untracked entry without a trailing slash" 2 'untracked entries are directories' \
  'sed "s|^untracked = .*|untracked = [\"src\"]|" docs/agent-docs.toml >m && mv m docs/agent-docs.toml'
probe "symlinked module directory" 1 '\[B\] src/linked: module directory has no entry' \
  'mkdir -p other && : >other/a.rs && ln -s ../other src/linked'
probe "missing coverage roots" 2 'missing \[coverage\] roots' \
  'grep -v "^roots" docs/agent-docs.toml >m && mv m docs/agent-docs.toml'
probe "stale bold file-list entry" 1 '\[C\] src/AGENTS.md:[0-9]+: cited path "gone.rs"' \
  'printf -- "- **gone.rs** - removed\n" >>src/AGENTS.md'
probe "stale backticked path" 1 'cited path "src-tauri/src/jj/backend.rs"' \
  'printf "See \`src-tauri/src/jj/backend.rs\`.\n" >>src/AGENTS.md'
probe "stale path with a line number" 1 'cited path "src/gone.rs" does not exist' \
  'printf "See \`src/gone.rs:42\`.\n" >>src/AGENTS.md'
probe "stale reference-style link" 1 'link target "docs/nope.md" does not exist' \
  'printf "\n[nope]: docs/nope.md\n" >>AGENTS.md'
probe "stale Markdown link" 1 'link target "docs/nope.md" does not exist' \
  'printf "See [nope](docs/nope.md).\n" >>AGENTS.md'
probe "commands, globs, URLs and placeholders are not citations" 0 'agent-docs: OK' \
  'printf "%s\n" "\`cargo test -p x\` \`src/*.rs\` \`https://x.test/a.rs\` \`../workspaces/<slug>\` \`a/.../b\` \`lib.rs::run()\` \`leptos/csr\` \`origin/main\` \`.rs\`" "\`\`\`" "cat missing/file.rs" "\`\`\`" >>src/AGENTS.md'
probe "unlisted nested AGENTS.md" 1 '\[D\].*src/pages/AGENTS.md exists but is not listed' \
  'printf "# AGENTS.md\n" >src/pages/AGENTS.md && printf "@AGENTS.md\n" >src/pages/CLAUDE.md'
probe "listed nested AGENTS.md that does not exist" 1 '\[D\].*lists src/pages/AGENTS.md, which does not exist' \
  'nested_entry src/pages/AGENTS.md'
probe "nested AGENTS.md over budget" 1 '\[E\] src/AGENTS.md: [0-9]+ lines \(budget 150\)' \
  'i=0; while [ $i -lt 150 ]; do echo x; i=$((i + 1)); done >>src/AGENTS.md'
probe "root AGENTS.md over budget" 1 '\[E\] AGENTS.md: [0-9]+ lines \(budget 250\)' \
  'i=0; while [ $i -lt 250 ]; do echo x; i=$((i + 1)); done >>AGENTS.md'
probe "code block over budget" 1 '\[E\] src/AGENTS.md:[0-9]+: code block has 31 lines' \
  '{ echo "\`\`\`rust"; i=0; while [ $i -lt 31 ]; do echo "let x = 1;"; i=$((i + 1)); done; echo "\`\`\`"; } >>src/AGENTS.md'
probe "unclosed code block" 1 'code block is never closed' \
  'printf "\`\`\`\nfn main() {}\n" >>src/AGENTS.md'
probe "four-backtick fence holding a three-backtick fence" 0 'agent-docs: OK' \
  'printf "%s\n" "\`\`\`\`md" "\`\`\`" "see missing/file.rs" "\`\`\`" "\`\`\`\`" >>src/AGENTS.md'
probe "long directory tree is exempt" 0 'agent-docs: OK' \
  '{ echo "\`\`\`"; echo "tree/"; i=0; while [ $i -lt 40 ]; do echo "├── f$i.rs"; i=$((i + 1)); done; echo "└── end.rs"; echo "\`\`\`"; } >>src/AGENTS.md'
probe "retired term meta-workspace" 1 '\[F\] README.md:[0-9]+: "meta-workspace" is retired' \
  'printf "The meta-workspace groups projects.\n" >>README.md'
probe "plural retired terms" 1 '\[F\] README.md:[0-9]+: "meta folder" is retired' \
  'printf "\nAll meta folders are listed.\n" >>README.md'
probe "retired term inside inline code is exempt" 0 'agent-docs: OK' \
  'printf "\nThe developer workspace \`../workspaces/meta-workspace\` is unrelated.\n" >>docs/development-workflow.md'
probe "task workspace" 1 '\[F\] docs/releasing.md:[0-9]+: say Task Checkout' \
  'printf "Each task gets a task workspace.\n" >>docs/releasing.md'
probe "isolated workspace" 1 '\[F\] src/AGENTS.md:[0-9]+: say Task Checkout' \
  'printf "Agents run in an isolated workspace.\n" >>src/AGENTS.md'
probe "JJ workspace claimed as a Task Checkout" 1 '\[F\] README.md:[0-9]+: names a JJ workspace and a Task Checkout' \
  'printf "\nA Task Checkout can also be a\n\`jj\` workspace, if you use Jujutsu.\n" >>README.md'
probe "JJ workspace claim after an unrelated negation" 1 'names a JJ workspace and a Task Checkout' \
  'printf "\nThis is not about Workspaces. A task checkout can live in a jj workspace, no setup needed.\n" >>README.md'
probe "JJ workspace and Task Checkout in separate sentences" 0 'agent-docs: OK' \
  'printf "\nEach JJ workspace keeps its own target directory. A Task Checkout is a Git worktree.\n" >>docs/development-workflow.md'
probe "JJ workspace denied as a Task Checkout" 0 'agent-docs: OK' \
  'printf "\nA JJ workspace is not a Task Checkout backend.\n" >>README.md'
probe "developer JJ workspace wording" 0 'agent-docs: OK' \
  'printf "\nCreate a JJ workspace for each change; Cargo workspace members build together.\n" >>docs/development-workflow.md'
probe "retired terms in CHANGELOG.md" 0 'agent-docs: OK' \
  'printf -- "- Renamed the meta-workspace and meta folder; removed the task workspace.\n" >>CHANGELOG.md'
probe "retired terms outside the Former names section" 1 '\[F\] docs/architecture/product-model.md' \
  'printf "\n## Later\n\nThe meta folder is back.\n" >>docs/architecture/product-model.md'

printf '%d/%d probes passed\n' "$((n - failures))" "$n"
[ "$failures" -eq 0 ]
