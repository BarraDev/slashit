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

# Check G. Mutations that rename the production ClaudeRunner and then leave a
# look-alike behind must still fail: only a real item counts.
say() { printf '%s\n' "$2" | tr '@' '\140' >>"$1"; } # @ stands for a backtick
edit() { sed "$1" "$2" >"$2.edit" && mv "$2.edit" "$2"; }
export -f say edit
runner=src-tauri/src/agents/runner.rs
gone="edit 's/^pub struct ClaudeRunner {/pub struct ClaudeProcess {/' $runner"
stale='\[G\] src-tauri/src/agents/AGENTS.md:[0-9]+: `ClaudeRunner` names nothing defined in crate slashit-ui'

probe "cited symbol removed" 1 '\[G\] src-tauri/src/AGENTS.md:[0-9]+: `NullEventSink` names nothing defined' \
  'edit "/^pub struct NullEventSink;$/d" src-tauri/src/events.rs'
probe "cited symbol renamed" 1 "$stale" "$gone"
probe "cited enum variant renamed in a dependency crate" 1 '`CreateTask` names nothing defined in crate slashit-ui or its dependency slashit-ipc' \
  'edit "s/^    CreateTask {/    NewTask {/" crates/slashit-ipc/src/protocol.rs'
probe "uncited symbol renamed" 0 'agent-docs: OK' \
  'edit "s/^pub const PROTOCOL_VERSION/pub const WIRE_VERSION/" crates/slashit-ipc/src/protocol.rs'
probe "uncited file removed" 0 'agent-docs: OK' 'rm crates/slashit-ipc/src/framing.rs'
probe "new file in a documented module" 0 'agent-docs: OK' \
  'printf "pub struct Helper;\n" >src-tauri/src/agents/helper.rs'
probe "function body changed" 0 'agent-docs: OK' \
  'edit "s/self.events.get().cloned().unwrap_or_else(events::null_sink)/events::null_sink()/" src-tauri/src/lib.rs'
probe "function signature changed" 0 'agent-docs: OK' \
  'edit "s/pub fn events(&self) -> events::SharedEventSink {/pub fn events(\&self, _verbose: bool) -> events::SharedEventSink {/" src-tauri/src/lib.rs'
probe "cited symbol moved to another file" 0 'agent-docs: OK' \
  "edit '/^pub struct ClaudeRunner {/,/^}/d' $runner; printf 'pub struct ClaudeRunner {}\n' >src-tauri/src/agents/process.rs; printf 'pub mod process;\n' >>src-tauri/src/agents/mod.rs"
probe "stale citation corrected" 0 'agent-docs: OK' \
  "$gone; edit 's/ClaudeRunner/ClaudeProcess/' src-tauri/src/agents/AGENTS.md"
probe "same name defined twice" 1 '`ClaudeRunner` is ambiguous in crate slashit-ui.*agents/runner.rs:[0-9]+ \(agents::runner::ClaudeRunner\), src-tauri/src/queue/pool.rs:1 \(queue::pool::ClaudeRunner\); qualify' \
  'printf "pub struct ClaudeRunner;\n" >src-tauri/src/queue/pool.rs; printf "mod pool;\n" >>src-tauri/src/queue/mod.rs'
probe "ambiguous citation qualified" 0 'agent-docs: OK' \
  'printf "pub struct ClaudeRunner;\n" >src-tauri/src/queue/pool.rs; printf "mod pool;\n" >>src-tauri/src/queue/mod.rs; edit "s/\`ClaudeRunner\`/\`runner::ClaudeRunner\`/" src-tauri/src/agents/AGENTS.md'
probe "unqualified AgentRole is ambiguous" 1 '`AgentRole` is ambiguous.*agents::roles::AgentRole.*domain::mcp::AgentRole' \
  'edit "s/roles::AgentRole/AgentRole/" src-tauri/src/agents/AGENTS.md'
probe "qualified citation whose item is gone" 1 '`roles::AgentRole` names nothing defined' \
  'edit "s/^pub enum AgentRole {/pub enum AgentKind {/" src-tauri/src/agents/roles.rs'
probe "qualifier that is not the parent module" 1 '`agents::AgentRole` names nothing defined' \
  'edit "s/roles::AgentRole/agents::AgentRole/" src-tauri/src/agents/AGENTS.md'
probe "trait method removed from one impl while other impls keep it" 1 '`DaemonControl::show_window` names nothing defined' \
  'edit "s/^    fn show_window(&self) -> Result<(), String> {/    fn show_window_gone(\&self) -> Result<(), String> {/" src-tauri/src/daemon.rs'
probe "variant cited through the wrong enum" 1 '`ClaudeEvent::CreateTask` names nothing defined' \
  'say src-tauri/src/AGENTS.md "See @ClaudeEvent::CreateTask@."'
probe "fully qualified path from the wrong crate" 1 '`slashit_ui_lib::endpoint::runtime_dir\(\)` names nothing defined' \
  'edit "s/slashit_ipc::endpoint::runtime_dir/slashit_ui_lib::endpoint::runtime_dir/" src-tauri/src/AGENTS.md'
probe "same name only in an unrelated crate" 1 "$stale" \
  "$gone; printf 'pub struct ClaudeRunner;\n' >>crates/slashit-cli/src/main.rs; printf 'pub struct ClaudeRunner;\n' >>src/app.rs"
probe "root AGENTS.md resolves across every crate" 1 '\[G\] AGENTS.md:[0-9]+: `JjStatus` is ambiguous in any crate' \
  'say AGENTS.md "See @JjStatus@."'
probe "doc comments are not definitions" 1 "$stale" \
  "$gone; printf '/// pub struct ClaudeRunner;\n//! pub struct ClaudeRunner {}\n' >>$runner"
probe "line and nested block comments are not definitions" 1 "$stale" \
  "$gone; printf '// pub struct ClaudeRunner;\n/* a /* nested */\npub struct ClaudeRunner;\n*/\n' >>$runner"
probe "string and raw-string fixtures are not definitions" 1 "$stale" \
  "$gone; printf 'const A: &str = \"\npub struct ClaudeRunner; \\\\\" }\n\";\nconst B: &str = r#\"\npub struct ClaudeRunner {} \"quoted\"\n\"#;\n' >>$runner"
probe "a character literal does not hide a real definition" 0 'agent-docs: OK' \
  "$gone; printf 'const Q: char = \047\"\047;\nconst B: char = \047{\047;\nfn f<\047a>(_: &\047a str) {}\npub struct ClaudeRunner;\n' >>$runner"
probe "an inline cfg(test) module is not a definition" 1 "$stale" \
  "$gone; printf '#[cfg(test)]\nmod tests {\n    pub struct ClaudeRunner;\n}\n' >>$runner"
probe "a file module declared under cfg(test) is not a definition" 1 "$stale" \
  "$gone; printf '#[cfg(all(test, unix))]\nmod fixtures;\n' >>src-tauri/src/agents/mod.rs; printf 'pub struct ClaudeRunner;\n' >src-tauri/src/agents/fixtures.rs"
probe "tests.rs and tests/ are not definitions" 1 "$stale" \
  "$gone; mkdir src-tauri/src/agents/tests; printf 'pub struct ClaudeRunner;\n' >src-tauri/src/agents/tests/fake.rs; printf 'pub struct ClaudeRunner;\n' >src-tauri/src/agents/tests.rs; printf 'pub struct ClaudeRunner;\n' >src-tauri/tests/fake.rs"
probe "a file marked #![cfg(test)] is not a definition" 1 "$stale" \
  "$gone; printf 'pub mod fixtures;\n' >>src-tauri/src/agents/mod.rs; printf '#![cfg(test)]\npub struct ClaudeRunner;\n' >src-tauri/src/agents/fixtures.rs"
probe "tests.rs is not a definition even without cfg(test)" 1 "$stale" \
  "$gone; printf 'mod tests;\n' >>src-tauri/src/agents/mod.rs; printf 'pub struct ClaudeRunner;\n' >src-tauri/src/agents/tests.rs"
probe "a tests/ module is not a definition even without cfg(test)" 1 "$stale" \
  "$gone; printf 'mod tests;\n' >>src-tauri/src/agents/mod.rs; mkdir src-tauri/src/agents/tests; printf 'pub struct ClaudeRunner;\n' >src-tauri/src/agents/tests/mod.rs"
probe "items inside function bodies and macros are not definitions" 1 "$stale" \
  "$gone; printf 'fn helper() {\n    struct ClaudeRunner;\n}\nmacro_rules! fake {\n    () => { pub struct ClaudeRunner; };\n}\n' >>$runner"
probe "a source file with a space in its name is read and left out" 1 "$stale" \
  "$gone; printf 'pub struct ClaudeRunner;\n' >'src-tauri/src/agents/a b.rs'"
probe "unreadable Rust stops the check" 2 'cannot follow the Rust source of src-tauri/src/agents/runner.rs' \
  "printf 'const BROKEN: &str = r#\"never closed;\n' >>$runner"
probe "snake_case, constants and fenced code are not citations" 0 'agent-docs: OK' \
  'say src/AGENTS.md "Calls @no_such_fn()@ with @NO_SUCH_CONST@."; printf "%s\n" "\`\`\`rust" "let x = NoSuchType::new();" "\`\`\`" >>src/AGENTS.md'
probe "crate-relative citation" 1 '`crate::AppState`: crate, self and super are not supported' \
  'say src-tauri/src/AGENTS.md "See @crate::AppState@."'
probe "allowlisted external" 0 'agent-docs: OK' 'say src/AGENTS.md "Compare @TypeId@."'
probe "external-looking citation not allowlisted" 1 '\[G\] src/AGENTS.md:[0-9]+: `HashMap<K, V>` names nothing defined in crate slashit-frontend' \
  'say src/AGENTS.md "Use a @HashMap<K, V>@."'
probe "allowlisted name that SlashIt defines" 1 '`AppState` is listed as external in docs/agent-docs.toml but SlashIt defines it' \
  'edit "s/^external = \[/external = [\"AppState\", /" docs/agent-docs.toml'
probe "allowlisted name nothing cites" 1 'external citation "HashMap" is not cited' \
  'edit "s/^external = \[/external = [\"HashMap\", /" docs/agent-docs.toml'
probe "duplicate external entry" 2 'duplicate value "TypeId"' \
  'edit "s/^external = \[/external = [\"TypeId\", /" docs/agent-docs.toml'
probe "wildcard external entry" 2 'external entries are exact names or paths' \
  'edit "s/^external = \[/external = [\"tauri::*\", /" docs/agent-docs.toml'
probe "unknown [citations] key" 2 'unknown key "externals"' \
  'edit "s/^external = /externals = /" docs/agent-docs.toml'
probe "documented command renamed in code" 1 '\[G\] src-tauri/src/jj/AGENTS.md:[0-9]+: Tauri command `git_export` is listed under "Tauri Commands Exposed" but is not registered' \
  'edit "s/^            git_export,$/            git_export_all,/" src-tauri/src/lib.rs; edit "s/fn git_export(/fn git_export_all(/" src-tauri/src/commands/jj.rs'
probe "command defined but no longer registered" 1 'Tauri command `abandon_change` is listed' \
  'edit "/^            abandon_change,$/d" src-tauri/src/lib.rs'
probe "registration commented out or mentioned elsewhere" 1 'Tauri command `abandon_change` is listed' \
  'edit "s|^            abandon_change,$|            // abandon_change,|" src-tauri/src/lib.rs; printf "fn uses() { let _ = commands::jj::abandon_change; }\n" >>src-tauri/src/lib.rs'
probe "unrelated command unregistered" 0 'agent-docs: OK' 'edit "/^            greet,$/d" src-tauri/src/lib.rs'
probe "handler list reformatted" 0 'agent-docs: OK' \
  'edit "/^            describe_change,$/d" src-tauri/src/lib.rs; edit "s/^            new_change,$/            new_change, commands::jj::describe_change,/" src-tauri/src/lib.rs'
probe "second generate_handler! list" 2 'expected one tauri::generate_handler!\[...\] in src-tauri/src/lib.rs, found 2' \
  'printf "fn more() { tauri::generate_handler![greet]; }\n" >>src-tauri/src/lib.rs'
probe "a cfg(test) item is not a production definition" 1 '`ClaudeRunner` names only agents::runner::ClaudeRunner .*compiled only under cfg\(test\)' \
  "$gone; printf '#[cfg(test)]\npub struct ClaudeRunner;\n' >>$runner"
probe "a method in a cfg(test) impl is not a production definition" 1 '`AppState::events\(\)` names only AppState::events .*cfg\(test\)' \
  'edit "s/pub fn events(&self)/pub fn events_now(\&self)/" src-tauri/src/lib.rs; printf "#[cfg(test)]\nimpl AppState {\n    pub fn events(&self) -> u8 { 0 }\n}\n" >>src-tauri/src/lib.rs'
probe "a cfg(all(test, not(...))) module is not a definition" 1 "$stale" \
  "$gone; printf '#[cfg(all(test, not(target_os = \"windows\")))]\nmod win_tests {\n    pub struct ClaudeRunner;\n}\n' >>$runner"
probe "a cfg(any(test, ...)) item is a definition" 0 'agent-docs: OK' \
  "$gone; printf '#[cfg(any(test, feature = \"x\"))]\npub struct ClaudeRunner;\n' >>$runner"
probe "a cfg(test) module loaded through #[path] is not a definition" 1 "$stale" \
  "$gone; printf '#[cfg(test)]\n#[path = \"runner_fixtures.rs\"]\nmod fixtures;\n' >>$runner; printf 'pub struct ClaudeRunner;\n' >src-tauri/src/agents/runner_fixtures.rs"
probe "a file no crate root reaches is not a definition" 1 '`JjManager` names nothing defined' \
  'edit "/^mod jj;$/d" src-tauri/src/lib.rs'
probe "a binary is not part of the library crate" 1 '`JjStatus` names nothing defined' \
  'edit "s/^pub struct JjStatus {/pub struct JjState {/" src-tauri/src/jj/manager.rs; printf "struct JjStatus;\n" >>src-tauri/src/bin/slashitd.rs'
probe "a moved item still re-exported under its old path" 1 '`roles::AgentRole` names nothing defined .*items named AgentRole: .*agents::kinds::AgentRole.*re-exports do not count' \
  'edit "s/^pub enum AgentRole {/pub enum AgentRoleOld {/" src-tauri/src/agents/roles.rs; printf "pub use super::kinds::AgentRole;\n" >>src-tauri/src/agents/roles.rs; printf "pub mod kinds;\n" >>src-tauri/src/agents/mod.rs; printf "pub enum AgentRole {}\n" >src-tauri/src/agents/kinds.rs'
probe "a name inside generic arguments is checked" 1 '\[G\] src-tauri/src/AGENTS.md:[0-9]+: `RoadmapState` names nothing defined' \
  'edit "s/^pub struct RoadmapState {/pub struct RoadmapStore {/" src-tauri/src/commands/roadmap.rs'
probe "test_only citation" 1 '`RecordingEventSink` names only events::RecordingEventSink .*cfg\(test\)' \
  'edit "s/^test_only = .*/test_only = []/" docs/agent-docs.toml'
probe "test_only entry that names a production item" 1 '`NullEventSink` names events::NullEventSink .*which is not test-only' \
  'edit "s/^test_only = \[/test_only = [\"NullEventSink\", /" docs/agent-docs.toml'
probe "test_only entry nothing cites" 1 'test_only citation "HashMap" is not a cited test-only item' \
  'edit "s/^test_only = \[/test_only = [\"HashMap\", /" docs/agent-docs.toml'
probe "external entry rooted at a SlashIt crate" 1 'external citation "slashit_ipc::Gone" starts with the SlashIt crate slashit_ipc' \
  'edit "s/^external = \[/external = [\"slashit_ipc::Gone\", /" docs/agent-docs.toml'
probe "external name SlashIt now defines suggests the full path" 1 '`TypeId` is listed as external .*queue::Key::TypeId.*full path \(such as `std::any::TypeId`\)' \
  'printf "enum Key {\n    TypeId,\n    Name,\n}\n" >>src-tauri/src/queue/mod.rs'
probe "ambiguity hint lists every qualified form" 1 '`ClaudeRunner` is ambiguous.*qualify the citation as one of `runner::ClaudeRunner`, `pool::ClaudeRunner`' \
  'printf "pub struct ClaudeRunner;\n" >src-tauri/src/queue/pool.rs; printf "mod pool;\n" >>src-tauri/src/queue/mod.rs'
probe "commands heading in another case, with a subsection" 1 'Tauri command `abandon_change` is listed' \
  'edit "s/^## Tauri Commands Exposed$/## Tauri commands exposed/" src-tauri/src/jj/AGENTS.md; edit "s/^- \`abandon_change\`/### Mutating\n\n- \`abandon_change\`/" src-tauri/src/jj/AGENTS.md; edit "/^            abandon_change,$/d" src-tauri/src/lib.rs'
probe "prose in the commands section is not a command" 0 'agent-docs: OK' \
  'say src-tauri/src/jj/AGENTS.md "These run @jj@ as a subprocess."; edit "/^## Pattern$/d" src-tauri/src/jj/AGENTS.md'
probe "workspace and dotted-table path dependencies keep their scope" 0 'agent-docs: OK' \
  'printf "\n[workspace.dependencies]\nslashit-ipc = { path = \"crates/slashit-ipc\" }\n" >>Cargo.toml; edit "s|^slashit-ipc = { path = \"../crates/slashit-ipc\" }$|slashit-ipc = { workspace = true }|" src-tauri/Cargo.toml; grep -q "workspace = true" src-tauri/Cargo.toml; edit "s|^slashit-ipc = { path = \"../slashit-ipc\" }$|[dependencies.slashit-ipc]\npath = \"../slashit-ipc\"|" crates/slashit-cli/Cargo.toml; grep -q "^\[dependencies.slashit-ipc\]" crates/slashit-cli/Cargo.toml'

printf '%d/%d probes passed\n' "$((n - failures))" "$n"
[ "$failures" -eq 0 ]
