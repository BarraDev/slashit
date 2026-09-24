#!/usr/bin/env bash
# Validate the structure of the agent documentation (AGENTS.md and CLAUDE.md).
#
# This runs locally and in CI. It is deterministic, edits no file, and uses
# only bash, POSIX awk and the version control tool that owns the checkout
# (in a JJ workspace, listing files snapshots the working copy, as every jj
# command does). It checks structure, not prose quality:
#
#   A. Every CLAUDE.md is exactly the pointer `@AGENTS.md` beside an AGENTS.md,
#      and every AGENTS.md has that CLAUDE.md beside it.
#   B. Every directory directly under a coverage root in docs/agent-docs.toml
#      is listed there once, as `documented` (has its own AGENTS.md) or as
#      `covered-by:<ancestor AGENTS.md>` (which names the directory). A
#      symlink to a directory counts as a module directory.
#   C. Repository paths that an AGENTS.md cites still exist. Recognized
#      citations, outside fenced code blocks only:
#        - inline and reference-style Markdown link targets, resolved as
#          Markdown does;
#        - `backticked` and **bold** tokens that end in `/` or in a known file
#          extension (`src/app.rs`, `queue/`, `lib.rs:42`). They resolve
#          against the doc's own directory or any directory above it.
#      Tokens with spaces, `<placeholders>`, globs, `...`, a `:` other than a
#      line number (URLs, Rust paths), no trailing `/` or extension
#      (`leptos/csr`, `origin/main`), or a leading `../` that leaves the
#      repository are not citations. HTML links and __underscore__ emphasis
#      are not recognized.
#   D. The "## Nested AGENTS.md" list in the root AGENTS.md names exactly the
#      nested AGENTS.md files that exist.
#   E. Size budgets: root AGENTS.md, nested AGENTS.md and each fenced code
#      block. A fenced directory tree (first line ends in `/`, every other
#      line is a `├──`/`└──` entry) is exempt from the code-block budget.
#   F. Product vocabulary in Markdown prose (code is exempt): retired or
#      misleading terms, and a sentence that names both a JJ workspace and a
#      Task Checkout without denying that one is the other ("is not",
#      "cannot", "never", ...). See docs/architecture/product-model.md.
#   G. Code identities an AGENTS.md names still exist, once. Recognized
#      citations, in `backticks` outside fenced code blocks, are an
#      UpperCamelCase name (`AppState`) or a Rust path (`roles::AgentRole`,
#      `IpcRequest::spawns_agent()`); a trailing `(...)` or `::*` is dropped,
#      and the names inside a trailing `<...>` are citations too. Bare
#      snake_case names are not citations. Each must match exactly one Rust
#      item (fn, struct, enum, variant, union, trait, type, const, static, or
#      an associated item) in the crate that owns the doc and its path
#      dependencies, or in any crate for a doc outside a crate. A path matches
#      when it is the tail of the item path at its definition (re-exports do
#      not count): `roles::AgentRole` is agents::roles::AgentRole, never
#      domain::mcp::AgentRole, and a name defined twice must be qualified.
#      Only files a crate root reaches through `mod name;` count. Items in
#      comments, strings, function bodies, macros, `tests/`, `tests.rs` and
#      cfg(test) modules never count; an item compiled only under cfg(test)
#      counts only for a citation listed under [citations] test_only, and
#      names SlashIt does not define are listed exactly under [citations]
#      external, both in docs/agent-docs.toml. Every command a list item
#      names under a "Tauri Commands Exposed" heading must be registered in
#      the tauri::generate_handler! list in src-tauri/src/lib.rs.
#
# Exit status: 0 when every check passes, 1 when a check fails, 2 when the
# inputs cannot be read or understood (the check never passes by default).

set -Eeuo pipefail
export LC_ALL=C

readonly MANIFEST=docs/agent-docs.toml
readonly ROOT_DOC_MAX_LINES=250
readonly NESTED_DOC_MAX_LINES=150
readonly CODE_BLOCK_MAX_LINES=30
# Check G. The one file whose tauri::generate_handler! list is authoritative.
readonly HANDLER_FILE=src-tauri/src/lib.rs

# Check F. Markdown files and sections where the retired terms are expected:
# the changelog is history, and the product model's "Former names" section
# exists to name them.
readonly VOCAB_SKIP_FILES='CHANGELOG.md'
readonly VOCAB_SKIP_SECTION_FILE='docs/architecture/product-model.md'
readonly VOCAB_SKIP_SECTION_HEADING='## Former names and compatibility'

die() {
  printf 'agent-docs: error: %s\n' "$*" >&2
  exit 2
}

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P) ||
  die "cannot resolve the repository root"
cd "$root"
[ -f "$MANIFEST" ] || die "$MANIFEST not found"
[ -f AGENTS.md ] || die "AGENTS.md not found at the repository root"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/agent-docs.XXXXXX") ||
  die "cannot create a temporary directory"
trap 'rm -rf "$tmp"' EXIT
# Any command failing unexpectedly means the checker could not do its job.
trap 'printf "agent-docs: error: internal failure at line %s\n" "$LINENO" >&2; exit 2' ERR

# --- The file set ------------------------------------------------------------
#
# Tracked files plus new, not-ignored ones, so a module added locally is
# checked before it is committed. A Git checkout (including a CI checkout,
# shallow or not) is listed with `git ls-files`; a JJ workspace, which has no
# `.git`, with `jj file list`, which snapshots the working copy first as every
# jj command does. The project pins jj in mise.toml, so prefer that jj.

vcs=
list_files() {
  local top
  if top=$(git rev-parse --show-toplevel 2>/dev/null) &&
    [ "$(cd "$top" && pwd -P)" = "$root" ]; then
    git -c core.quotePath=false ls-files --cached --others --exclude-standard
  elif [ -d .jj ]; then
    if command -v mise >/dev/null 2>&1 && [ -f mise.toml ]; then
      mise exec -- jj file list
    else
      jj file list
    fi
  else
    printf 'agent-docs: error: %s is neither a Git checkout nor a JJ workspace\n' "$root" >&2
    return 1
  fi
}

[ -d .jj ] && vcs=jj
if top=$(git rev-parse --show-toplevel 2>/dev/null) &&
  [ "$(cd "$top" && pwd -P)" = "$root" ]; then
  vcs=git
fi
list_files >"$tmp/listed" || die "cannot list repository files"

# grep, but only "no match" (status 1) is allowed to pass as empty output.
select_lines() { grep "$@" || [ $? -eq 1 ]; }

if grep -q '^"' "$tmp/listed"; then
  die "a file name needs quoting; rename it or extend this script"
fi
: >"$tmp/dirlinks"
while IFS= read -r f; do
  if [ -e "$f" ] || [ -L "$f" ]; then
    printf '%s\n' "$f"
    if [ -L "$f" ] && [ -d "$f" ]; then printf '%s\n' "$f" >>"$tmp/dirlinks"; fi
  elif [ "$vcs" = jj ]; then
    # jj lists only files present in the working copy; anything else means
    # the listing could not be read line by line (a newline in a name).
    die "jj listed \"$f\", which does not exist; rename the file"
  fi
  # `ls-files --cached` still lists a file deleted from the work tree.
done <"$tmp/listed" | sort -u >"$tmp/files"
[ -s "$tmp/files" ] || die "the repository file list is empty"

select_lines -E '(^|/)AGENTS\.md$' "$tmp/files" >"$tmp/agents"
select_lines -E '(^|/)CLAUDE\.md$' "$tmp/files" >"$tmp/claudes"
select_lines -E '\.md$' "$tmp/files" >"$tmp/markdown"
# awk would read `name=value` as an assignment, not a file name.
if grep -q '=' "$tmp/markdown"; then
  die "a Markdown file name contains '='; rename it or extend this script"
fi
while IFS= read -r f; do
  [ -f "$f" ] && [ -r "$f" ] || die "$f is not a readable file"
done <"$tmp/markdown"
agent_docs=()
while IFS= read -r f; do agent_docs+=("$f"); done <"$tmp/agents"
markdown_docs=()
while IFS= read -r f; do markdown_docs+=("$f"); done <"$tmp/markdown"

# Shared awk helpers, prepended to each check's program. `exists` and `isdir`
# are filled from the file set.
lib=$(cat <<'AWK'
function load_files(path,    f, d) {
  isdir[""] = 1
  while ((getline f < path) > 0) {
    exists[f] = 1
    d = f
    while (sub(/\/[^\/]*$/, "", d)) { exists[d] = 1; isdir[d] = 1 }
  }
  close(path)
}
function dirname(p) { if (!sub(/\/[^\/]*$/, "", p)) p = ""; return p }
function basename(p) { sub(/^.*\//, "", p); return p }
# Collapses `.` and `..`; returns ESCAPED when the path leaves the repository.
function normalize(p,    n, parts, out, i, k, res) {
  n = split(p, parts, "/"); k = 0
  for (i = 1; i <= n; i++) {
    if (parts[i] == "" || parts[i] == ".") continue
    if (parts[i] == "..") { if (k == 0) return ESCAPED; k--; continue }
    out[++k] = parts[i]
  }
  res = ""
  for (i = 1; i <= k; i++) res = res (i > 1 ? "/" : "") out[i]
  return res
}
# Code fences: the opening marker (``` or ~~~, possibly longer) or "".
function fence_marker(s) {
  if (!match(s, /^ *(```+|~~~+)/)) return ""
  s = substr(s, RSTART, RLENGTH); gsub(/ /, "", s)
  return s
}
function fence_closes(s, m) { return s ~ ("^ *" m "[" substr(m, 1, 1) "]* *$") }
function report(check, where, msg) {
  printf "agent-docs: [%s] %s: %s\n", check, where, msg
  problems++
}
BEGIN { ESCAPED = "\001escaped"; problems = 0 }
AWK
)

problems=0
count() { # Adds the problems a check printed (one per line) to the total.
  local n
  n=$(wc -l <"$1" | tr -d ' ')
  cat "$1"
  problems=$((problems + n))
}

# --- Manifest ----------------------------------------------------------------
#
# A deliberately narrow TOML subset. Anything outside it is an error rather
# than being skipped, so a typo cannot quietly drop a module from coverage.

awk -v out="$tmp/manifest" "$lib"'
function valid_path(p) {
  return p ~ /^[A-Za-z0-9_.-]+(\/[A-Za-z0-9_.-]+)*$/ && p !~ /(^|\/)\.\.?(\/|$)/
}
function bad(msg) { report("manifest", FILENAME ":" FNR, msg) }
BEGIN {
  arraykeys["coverage"] = " roots "; arraykeys["citations"] = " untracked external test_only "
  printf "" > out
}
{
  line = $0
  if (line ~ /\r/) { bad("carriage return; use LF line endings"); next }
  sub(/#.*/, "", line)
  gsub(/^[ \t]+|[ \t]+$/, "", line)
  if (line == "") next

  if (line ~ /^\[[a-z]+\]$/) {
    section = substr(line, 2, length(line) - 2)
    if (!(section in arraykeys) && section != "modules") bad("unknown section [" section "]")
    else if (section in sections) bad("duplicate section [" section "]")
    sections[section] = 1
    next
  }

  if (section in arraykeys) {
    if (line !~ /^[a-z_]+[ \t]*=[ \t]*\[.*\]$/) { bad("expected: key = [\"...\", ...] on one line"); next }
    key = line; sub(/[ \t]*=.*/, "", key)
    if (!index(arraykeys[section], " " key " ")) { bad("unknown key \"" key "\" in [" section "]"); next }
    if ((section, key) in keys) { bad("duplicate key \"" key "\""); next }
    keys[section, key] = 1
    body = line; sub(/^[^[]*\[/, "", body); sub(/\][ \t]*$/, "", body)
    n = split(body, parts, ",")
    for (i = 1; i <= n; i++) {
      item = parts[i]; gsub(/^[ \t]+|[ \t]+$/, "", item)
      if (item == "" && i == n && n > 1) continue
      if (item !~ /^"[^"]*"$/) { bad("array items must be \"double-quoted\" strings"); continue }
      v = substr(item, 2, length(item) - 2)
      p = v
      if (key == "external" || key == "test_only") {
        if (v !~ /^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$/) { bad(key " entries are exact names or paths such as \"TypeId\" or \"tauri::State\", got \"" v "\""); continue }
      } else if (key == "untracked" && !sub(/\/$/, "", p)) { bad("untracked entries are directories and end in /, got \"" v "\""); continue }
      else if (!valid_path(p)) { bad("invalid path \"" v "\""); continue }
      if ((key, v) in values) { bad("duplicate value \"" v "\""); continue }
      values[key, v] = 1
      printf "%s\t%s\n", key, v > out
    }
    next
  }

  if (section == "modules") {
    if (line !~ /^"[^"]*"[ \t]*=[ \t]*"[^"]*"$/) { bad("expected: \"<module dir>\" = \"documented\" or \"covered-by:<AGENTS.md>\""); next }
    mod = line; sub(/^"/, "", mod); sub(/".*/, "", mod)
    val = line; sub(/"$/, "", val); sub(/.*"/, "", val)
    if (!valid_path(mod)) { bad("invalid module path \"" mod "\""); next }
    if (mod in modules) { bad("duplicate module \"" mod "\" (first on line " modules[mod] ")"); next }
    modules[mod] = FNR
    if (val == "documented") { printf "module\t%s\tdocumented\t\t%d\n", mod, FNR > out; next }
    if (val ~ /^covered-by:/) {
      target = substr(val, 12)
      if (!valid_path(target) || target !~ /(^|\/)AGENTS\.md$/) { bad("covered-by must name an AGENTS.md path, got \"" target "\""); next }
      printf "module\t%s\tcovered-by\t%s\t%d\n", mod, target, FNR > out
      next
    }
    bad("unknown classification \"" val "\"")
    next
  }

  bad("entry outside a known section")
}
END {
  if (!(("coverage", "roots") in keys)) report("manifest", FILENAME, "missing [coverage] roots")
  if (!("modules" in sections)) report("manifest", FILENAME, "missing [modules]")
}
' "$MANIFEST" >"$tmp/out.manifest"
count "$tmp/out.manifest"
if [ "$problems" -gt 0 ]; then
  printf 'agent-docs: %s cannot be understood; nothing else was checked\n' "$MANIFEST" >&2
  exit 2
fi
grep -q '^roots	' "$tmp/manifest" || die "$MANIFEST declares no coverage roots"
grep -q '^module	' "$tmp/manifest" || die "$MANIFEST declares no modules"

# --- A. AGENTS.md / CLAUDE.md pairing ---------------------------------------

: >"$tmp/out.a"
while IFS= read -r f; do
  dir=$(dirname "$f")
  sibling=AGENTS.md
  [ "$dir" = . ] || sibling="$dir/AGENTS.md"
  if ! grep -Fxq -- "$sibling" "$tmp/agents"; then
    printf 'agent-docs: [A] %s: no sibling AGENTS.md\n' "$f" >>"$tmp/out.a"
  fi
  if ! { printf '@AGENTS.md\n' | cmp -s - "$f"; } &&
    ! { printf '@AGENTS.md' | cmp -s - "$f"; }; then
    printf 'agent-docs: [A] %s: must contain exactly "@AGENTS.md"; put documentation in AGENTS.md\n' "$f" >>"$tmp/out.a"
  fi
done <"$tmp/claudes"
while IFS= read -r f; do
  dir=$(dirname "$f")
  sibling=CLAUDE.md
  [ "$dir" = . ] || sibling="$dir/CLAUDE.md"
  if ! grep -Fxq -- "$sibling" "$tmp/claudes"; then
    printf 'agent-docs: [A] %s: no sibling CLAUDE.md containing "@AGENTS.md"\n' "$f" >>"$tmp/out.a"
  fi
done <"$tmp/agents"
count "$tmp/out.a"

# --- B. Module coverage -----------------------------------------------------

awk -F '\t' -v files="$tmp/files" -v dirlinks="$tmp/dirlinks" -v manifest="$MANIFEST" "$lib"'
# Does `doc` mention `name/` as a path segment (not as the tail of a longer
# name such as `.config/`)?
function mentions(doc, name,    line, rest, i, prev, found) {
  found = 0
  while (!found && (getline line < doc) > 0) {
    rest = line
    while ((i = index(rest, name "/")) > 0) {
      prev = i > 1 ? substr(rest, i - 1, 1) : ""
      if (prev !~ /[A-Za-z0-9_.-]/) { found = 1; break }
      rest = substr(rest, i + 1)
    }
  }
  close(doc)
  return found
}
BEGIN { load_files(files) }
$1 == "roots" {
  root = $2
  if (!(root in isdir) || root == "") { report("B", manifest, "coverage root \"" root "\" is not a directory with files"); next }
  roots[root] = 1
  next
}
$1 == "module" { kind[$2] = $3; target[$2] = $4; line[$2] = $5 }
END {
  # Discover module directories: one level below each root, plus symlinks
  # to directories at that level.
  while ((getline f < files) > 0) {
    for (r in roots) {
      if (index(f, r "/") != 1) continue
      rest = substr(f, length(r) + 2)
      if (rest !~ /\//) continue
      sub(/\/.*/, "", rest)
      found[r "/" rest] = 1
    }
  }
  close(files)
  while ((getline f < dirlinks) > 0)
    if (dirname(f) in roots) found[f] = 1
  close(dirlinks)
  for (m in found)
    if (!(m in kind))
      report("B", m, "module directory has no entry in " manifest "; add \"" m "\" = \"covered-by:<an ancestor AGENTS.md that names it>\" or \"documented\"")
  for (m in kind) {
    where = manifest ":" line[m]
    parent = dirname(m)
    if (!(parent in roots)) { report("B", where, "\"" m "\" is not directly under a coverage root"); continue }
    if (!(m in found)) { report("B", where, "\"" m "\" does not exist"); continue }
    own = m "/AGENTS.md"
    if (kind[m] == "documented") {
      if (!(own in exists)) report("B", where, "\"" m "\" is documented but has no AGENTS.md")
      continue
    }
    t = target[m]
    if (own in exists) { report("B", where, "\"" m "\" has its own AGENTS.md; classify it as documented"); continue }
    if (!(t in exists)) { report("B", where, "covered-by target " t " does not exist"); continue }
    tdir = dirname(t)
    if (tdir != "" && index(m, tdir "/") != 1) { report("B", where, t " is not in an ancestor directory of " m); continue }
    if (!mentions(t, basename(m))) report("B", where, t " does not mention \"" basename(m) "/\"")
  }
}
' "$tmp/manifest" | sort >"$tmp/out.b"
count "$tmp/out.b"

# --- C. Cited paths ---------------------------------------------------------

select_lines '^untracked	' "$tmp/manifest" | cut -f2 >"$tmp/untracked"

awk -v files="$tmp/files" -v untracked="$tmp/untracked" "$lib"'
BEGIN {
  load_files(files)
  while ((getline u < untracked) > 0) skip[u] = 1
  close(untracked)
  ext = "[A-Za-z0-9_-]\\.(rs|md|toml|ya?ml|sh|json|html|css|lock|txt|js|ts)$"
}
# Entries end in "/", so this matches whole path segments only.
function is_untracked(p,    u) {
  for (u in skip) if (index(p, u) == 1 || p "/" == u) return 1
  return 0
}
# A token that is plausibly a repository path, per the rules at the top.
function path_like(t) {
  if (t !~ /^[A-Za-z0-9_.\/-]+$/) return 0
  if (index(t, "...") || t ~ /^\// || t ~ /\/\//) return 0
  return t ~ /\/$/ || t ~ ext
}
function check_token(doc, n, t,    d, r, want_dir) {
  sub(/(:[0-9]+(-[0-9]+)?|#L[0-9]+(-L?[0-9]+)?)$/, "", t)
  if (!path_like(t) || is_untracked(t)) return
  want_dir = t ~ /\/$/
  if (t ~ /^\.\.?\//) {
    r = normalize(dirname(doc) "/" t)
    if (r == ESCAPED) return
    if (r in exists && (!want_dir || r in isdir)) return
    report("C", doc ":" n, "cited path \"" t "\" does not exist")
    return
  }
  d = dirname(doc)
  while (1) {
    r = normalize((d == "" ? "" : d "/") t)
    if (r in exists && (!want_dir || r in isdir)) return
    if (d == "") break
    d = dirname(d)
  }
  report("C", doc ":" n, "cited path \"" t "\" does not exist")
}
function check_link(doc, n, t,    r) {
  if (t ~ /^</) { sub(/^</, "", t); sub(/>.*/, "", t) }
  else sub(/[ \t].*/, "", t)
  if (t ~ /^[A-Za-z][A-Za-z0-9+.-]*:/) return
  sub(/#.*/, "", t); sub(/\?.*/, "", t)
  if (t == "") return
  r = t ~ /^\// ? normalize(t) : normalize(dirname(doc) "/" t)
  if (r == ESCAPED) { report("C", doc ":" n, "link \"" t "\" leaves the repository"); return }
  if (!(r in exists)) report("C", doc ":" n, "link target \"" t "\" does not exist")
}
FNR == 1 { fence = "" }
{
  if (fence != "") { if (fence_closes($0, fence)) fence = ""; next }
  if ((fence = fence_marker($0)) != "") next

  if (match($0, /^ ? ? ?\[[^]]+\]:[ \t]+/)) {
    check_link(FILENAME, FNR, substr($0, RSTART + RLENGTH))
    next
  }
  line = $0; plain = ""
  while (match(line, /`[^`]+`/)) {
    check_token(FILENAME, FNR, substr(line, RSTART + 1, RLENGTH - 2))
    plain = plain substr(line, 1, RSTART - 1) " "
    line = substr(line, RSTART + RLENGTH)
  }
  plain = plain line
  rest = plain
  while (match(rest, /\*\*[^*]+\*\*/)) {
    t = substr(rest, RSTART + 2, RLENGTH - 4)
    if (t ~ /\/$/ || t ~ ext) check_token(FILENAME, FNR, t)
    rest = substr(rest, RSTART + RLENGTH)
  }
  rest = plain
  while (match(rest, /\]\([^)]*\)/)) {
    check_link(FILENAME, FNR, substr(rest, RSTART + 2, RLENGTH - 3))
    rest = substr(rest, RSTART + RLENGTH)
  }
}
' "${agent_docs[@]}" | sort >"$tmp/out.c"
count "$tmp/out.c"

# --- D. Root nested-doc list ------------------------------------------------

grep -vx 'AGENTS.md' "$tmp/agents" >"$tmp/nested" || true
awk -v nested="$tmp/nested" "$lib"'
BEGIN { while ((getline f < nested) > 0) actual[f] = 1; close(nested) }
/^## / { insection = ($0 == "## Nested AGENTS.md"); if (insection) seen = 1; next }
insection && /^[-*] / {
  if (!match($0, /^[-*] `[^`]+`/)) { report("D", FILENAME ":" FNR, "list entries must start with a `path/AGENTS.md`"); next }
  p = substr($0, 4, RLENGTH - 4)
  if (p in listed) report("D", FILENAME ":" FNR, p " is listed twice")
  listed[p] = FNR
}
END {
  if (!seen) { report("D", FILENAME, "no \"## Nested AGENTS.md\" section"); exit }
  for (p in listed)
    if (!(p in actual)) report("D", FILENAME ":" listed[p], "lists " p ", which does not exist")
  for (p in actual)
    if (!(p in listed)) report("D", FILENAME, p " exists but is not listed under \"## Nested AGENTS.md\"")
}
' AGENTS.md | sort >"$tmp/out.d"
count "$tmp/out.d"

# --- E. Size budgets --------------------------------------------------------

awk -v root_max="$ROOT_DOC_MAX_LINES" -v nested_max="$NESTED_DOC_MAX_LINES" \
  -v block_max="$CODE_BLOCK_MAX_LINES" "$lib"'
function close_block() {
  if (lines > block_max && !tree)
    report("E", FILENAME ":" start, "code block has " lines " lines (budget " block_max "); link to the source instead")
}
function end_doc(    max) {
  if (doc == "") return
  if (fence != "") report("E", doc ":" start, "code block is never closed")
  max = doc == "AGENTS.md" ? root_max : nested_max
  if (total > max) report("E", doc, total " lines (budget " max ")")
}
FNR == 1 { end_doc(); doc = FILENAME; fence = ""; total = 0 }
{ total++ }
fence != "" {
  if (fence_closes($0, fence)) { close_block(); fence = ""; next }
  lines++
  if (lines == 1) tree = $0 ~ /\/[ \t]*(#.*)?$/
  else if ($0 !~ /^[ \t]*$/ && !index($0, "\342\224\234\342\224\200\342\224\200 ") && !index($0, "\342\224\224\342\224\200\342\224\200 ")) tree = 0
  next
}
(fence = fence_marker($0)) != "" { start = FNR; lines = 0; tree = 0; next }
END { end_doc() }
' "${agent_docs[@]}" >"$tmp/out.e"
count "$tmp/out.e"

# --- F. Product vocabulary --------------------------------------------------

awk -v skip_files="$VOCAB_SKIP_FILES" -v skip_file="$VOCAB_SKIP_SECTION_FILE" \
  -v skip_heading="$VOCAB_SKIP_SECTION_HEADING" "$lib"'
BEGIN {
  n = split(skip_files, s, " ")
  for (i = 1; i <= n; i++) skipped[s[i]] = 1
  retired["meta[- ]?workspaces?"] = "\"meta-workspace\" is retired; say Workspace"
  retired["meta[- ]folders?"] = "\"meta folder\" is retired; say Workspace root"
  retired["task[- ]workspaces?"] = "say Task Checkout, not \"task workspace\""
  retired["isolated workspaces?"] = "say Task Checkout, not \"isolated workspace\""
  jj = "(^|[^a-z])(jj|jujutsu) workspaces?([^a-z]|$)"
  checkout = "task checkouts?|checkout backends?"
  denied = "(^|[^a-z])((is|are|was|were|can|could|does|do|will|may) not|cannot|never|no longer)([^a-z]|$)|n(\047|\342\200\231)t([^a-z]|$)"
}
# One paragraph or list item, checked a sentence at a time.
function flush_unit(    rest, sentence) {
  rest = unit
  while (rest != "") {
    if (match(rest, /[.!?;]( |$)/)) {
      sentence = substr(rest, 1, RSTART); rest = substr(rest, RSTART + RLENGTH)
    } else { sentence = rest; rest = "" }
    if (sentence ~ jj && sentence ~ checkout && sentence !~ denied) {
      report("F", ufile ":" ustart, "names a JJ workspace and a Task Checkout together without saying one is not the other; every Task Checkout is a Git worktree")
      break
    }
  }
  unit = ""
}
FNR == 1 { flush_unit(); fence = ""; insection = 0 }
FILENAME in skipped { next }
FILENAME == skip_file && /^#+ / {
  if ($0 == skip_heading) insection = 1
  else if ($0 ~ /^##? /) insection = 0
}
insection { next }
fence != "" { if (fence_closes($0, fence)) fence = ""; next }
(fence = fence_marker($0)) != "" { flush_unit(); next }
/^[ \t]*$/ || /^#/ || /^ *([-*+]|[0-9]+\.) / { flush_unit() }
/^[ \t]*$/ { next }
{
  # Retired terms are checked in prose only: inline code names identifiers,
  # slugs and commands. The JJ claim reads code too (a `jj` workspace).
  line = $0; prose = ""
  while (match(line, /`[^`]+`/)) {
    prose = prose substr(line, 1, RSTART - 1) " code "
    line = substr(line, RSTART + RLENGTH)
  }
  prose = tolower(prose line)
  for (re in retired)
    if (prose ~ ("(^|[^a-z])" re "([^a-z]|$)")) report("F", FILENAME ":" FNR, retired[re])
  if (/^#/) next
  text = tolower($0)
  gsub(/[*`]/, "", text)
  if (unit == "") { ufile = FILENAME; ustart = FNR }
  unit = unit " " text
}
END { flush_unit() }
' "${markdown_docs[@]}" | sort >"$tmp/out.f"
count "$tmp/out.f"

# --- G. Cited code identities -----------------------------------------------
#
# The crates, from each Cargo.toml in the file set: "crate dir name package"
# and "dep dir depdir" for each path dependency.

select_lines '^external	' "$tmp/manifest" | cut -f2 >"$tmp/externals"
select_lines -E '(^|/)Cargo\.toml$' "$tmp/files" >"$tmp/cargo"
awk "$lib"'
function quoted(s) { sub(/^[^"]*"/, "", s); sub(/".*/, "", s); return s }
function add_dep(p, from) {
  p = normalize((from == "" ? "" : from "/") p)
  if (p != ESCAPED) deps[++ndeps] = p
}
# Path dependencies declared once in [workspace.dependencies], for members
# that say `name = { workspace = true }` or `name.workspace = true`.
BEGIN {
  while ((getline line < "Cargo.toml") > 0) {
    sub(/^[ \t]+/, "", line)
    if (line ~ /^\[/) { ws = line ~ /^\[workspace\.dependencies\]/; continue }
    if (ws && match(line, /(^|[,{ \t])path[ \t]*=[ \t]*"[^"]*"/)) {
      n = line; sub(/[ \t.=].*/, "", n)
      wspath[n] = quoted(substr(line, RSTART, RLENGTH))
    }
  }
  close("Cargo.toml")
}
{
  toml = $0; dir = dirname(toml); section = ""; pkg = ""; libname = ""; ndeps = 0
  while ((getline line < toml) > 0) {
    sub(/^[ \t]+/, "", line)
    if (line ~ /^\[/) {
      section = line; sub(/[ \t]*(#.*)?$/, "", section)
      # [dependencies.name] and [target.<cfg>.dependencies.name] tables.
      table = ""
      if (section ~ /^\[(target\..*\.)?dependencies\.[A-Za-z0-9_-]+\]$/) {
        table = section; sub(/^.*dependencies\./, "", table); sub(/\]$/, "", table)
      }
      continue
    }
    if (section == "[package]" && line ~ /^name[ \t]*=[ \t]*"/) pkg = quoted(line)
    if (section == "[lib]" && line ~ /^name[ \t]*=[ \t]*"/) libname = quoted(line)
    if (table != "") {
      if (match(line, /^path[ \t]*=[ \t]*"[^"]*"/)) add_dep(quoted(line), dir)
      else if (line ~ /^workspace[ \t]*=[ \t]*true/ && (table in wspath)) add_dep(wspath[table], "")
      continue
    }
    if (section != "[dependencies]" && section !~ /^\[target\..*\.dependencies\]$/) continue
    if (match(line, /(^|[,{ \t])path[ \t]*=[ \t]*"[^"]*"/)) add_dep(quoted(substr(line, RSTART, RLENGTH)), dir)
    else if (line ~ /workspace[ \t]*=[ \t]*true/) {
      n = line; sub(/[ \t.=].*/, "", n)
      if (n in wspath) add_dep(wspath[n], "")
    }
  }
  close(toml)
  if (pkg == "") next
  name = libname != "" ? libname : pkg
  gsub(/-/, "_", name)
  printf "crate\t%s\t%s\t%s\n", dir, name, pkg
  for (i = 1; i <= ndeps; i++) printf "dep\t%s\t%s\n", dir, deps[i]
}
' "$tmp/cargo" >"$tmp/crates"

# The Rust files to index, as "crate modpath file". A file belongs to the
# crate with the longest src/ directory above it; its module path follows the
# file layout. Test files are left out.
awk -F '\t' -v files="$tmp/files" '
$1 == "crate" { crate[($2 == "" ? "" : $2 "/") "src"] = $3 }
END {
  while ((getline f < files) > 0) {
    if (f !~ /\.rs$/) continue
    best = ""
    for (s in crate) if (index(f, s "/") == 1 && length(s) > length(best)) best = s
    if (best == "") continue
    rel = substr(f, length(best) + 2)
    if (rel ~ /(^|\/)tests\// || rel ~ /(^|\/)tests\.rs$/) continue
    sub(/\.rs$/, "", rel); sub(/(^|\/)(mod|lib|main)$/, "", rel)
    path = crate[best]
    if (rel != "") { gsub(/\//, "::", rel); path = path "::" rel }
    printf "%s\t%s\t%s\n", crate[best], path, f
  }
  close(files)
}
' "$tmp/crates" >"$tmp/sources"

# The definition index. A small Rust lexer drops comments and the contents
# of string and character literals; a parser then follows item nesting so
# that only items reachable by path are recorded:
#   def  name  path  modpath  file:line  kind  test  filemod
#   mod  modpath  filemod   (a `mod name;` that loads another file)
#   testmod  modpath        (a module compiled only under cfg(test))
# `test` is 1 for an item compiled only under cfg(test), or inside an impl,
# trait or enum that is.
# Any file that does not end back at the top level, outside every literal
# and comment, stops the check: an index built on a misread file could pass
# a citation it should fail.
: >"$tmp/hcode"
awk -v sources="$tmp/sources" -v hfile="$HANDLER_FILE" -v hcode="$tmp/hcode" '
function lex(s,    out, c, i, h) {
  out = ""
  while (s != "") {
    if (st == "block") {
      if (!match(s, /\/\*|\*\//)) return out
      if (substr(s, RSTART, 2) == "/*") bdepth++
      else if (--bdepth == 0) st = ""
      s = substr(s, RSTART + 2)
      continue
    }
    if (st == "str") {
      if (!match(s, /^([^"\\]|\\.)*"/)) return out
      s = substr(s, RLENGTH + 1); st = ""; out = out "\"\""
      continue
    }
    if (st == "raw") {
      if (!(i = index(s, rawend))) return out
      s = substr(s, i + length(rawend)); st = ""; out = out "\"\""
      continue
    }
    if (!match(s, LEXSTART)) return out s
    out = out substr(s, 1, RSTART - 1)
    c = substr(s, RSTART, 2)
    if (c == "//") return out
    if (c == "/*") { st = "block"; bdepth = 1; s = substr(s, RSTART + 2); continue }
    c = substr(c, 1, 1)
    s = substr(s, RSTART + 1)
    if (c == "\"") {
      # r"...", r#"..."#, br"..." and cr"..." are raw strings.
      if (match(out, /[bc]?r#*$/) &&
        (RSTART == 1 || substr(out, RSTART - 1, 1) !~ /[A-Za-z0-9_]/)) {
        h = substr(out, RSTART); sub(/^[^#]*/, "", h)
        out = substr(out, 1, RSTART - 1)
        st = "raw"; rawend = "\"" h
      } else st = "str"
      continue
    }
    # A character literal, or a lifetime or loop label.
    if (substr(s, 1, 1) == "\\") {
      if ((i = index(substr(s, 3), Q))) s = substr(s, i + 3)
      out = out " 0 "; continue
    }
    if (substr(s, 2, 1) == Q) { s = substr(s, 3); out = out " 0 "; continue }
    if (substr(s, 1, 1) > "~" && (i = index(s, Q)) && i <= 5) {
      s = substr(s, i + 1); out = out " 0 "; continue
    }
    if (match(s, /^[A-Za-z_][A-Za-z0-9_]*/)) s = substr(s, RLENGTH + 1)
    out = out " "
  }
  return out
}
# Block kinds: mod, impl, trait and enum hold items or variants; skip is any
# other block (bodies, struct fields, macros, cfg(test) modules).
function push(kind, name, test) {
  d++; bk[d] = kind; bn[d] = name; pd[d] = 0; hdr[d] = ""; pend[d] = ""
  ptest[d] = 0; expv[d] = (kind == "enum"); bt[d] = bt[d - 1] || test
  mp[d] = mp[d - 1] (kind == "mod" ? "::" name : "")
}
function pop() { if (d > 0) d--; else underflow = 1 }
function container(    k) {
  for (k = d; k > 0; k--) {
    if (bk[k] == "impl" || bk[k] == "trait" || bk[k] == "enum") return bn[k] "::"
    if (bk[k] == "mod") return ""
  }
  return ""
}
function def(name, kind, test) {
  if (name == "_") return
  printf "def\t%s\t%s\t%s\t%s:%d\t%s\t%d\t%s\n", name, mp[d] "::" container() name, mp[d], file, lno, kind, bt[d] || test, filemod
}
# cfg(test) or cfg(all(..., test, ...)): compiled only when testing.
function is_test_cfg(a,    n, i, c, depth, arg) {
  if (a == "cfg(test)") return 1
  if (a !~ /^cfg\(all\(.*\)\)$/) return 0
  a = substr(a, 9, length(a) - 10) ","
  depth = 0; arg = ""; n = length(a)
  for (i = 1; i <= n; i++) {
    c = substr(a, i, 1)
    if (c == "(") depth++
    else if (c == ")") depth--
    if (c == "," && depth == 0) { if (arg == "test") return 1; arg = "" }
    else arg = arg c
  }
  return 0
}
function token(t,    k) {
  if (attr) {
    if (t == "[") attr++
    else if (t == "]" && --attr == 0) {
      if (is_test_cfg(attrtext)) {
        if (attrinner) print "testmod\t" mp[d]
        else ptest[d] = 1
      }
      return
    }
    attrtext = attrtext t
    return
  }
  if (t == "#") { attrwait = 1; return }
  if (attrwait) {
    k = attrwait; attrwait = 0
    if (t == "!" && k == 1) { attrwait = 2; return }
    if (t == "[") { attr = 1; attrtext = ""; attrinner = (k == 2); return }
  }
  if (hdr[d] != "") { header(t); return }
  if (t == "{") { ptest[d] = 0; push("skip"); return }
  if (t == "}") { pop(); ptest[d] = 0; return }
  if (t == "(" || t == "[") { pd[d]++; return }
  if (t == ")" || t == "]") { if (pd[d] > 0) pd[d]--; return }
  if (pd[d] > 0) return
  if (bk[d] == "enum") {
    if (t == ",") expv[d] = 1
    else if (expv[d] && t ~ /^[A-Za-z_]/) { expv[d] = 0; def(t, "variant", ptest[d]) }
    ptest[d] = 0
    return
  }
  if (bk[d] != "mod" && bk[d] != "impl" && bk[d] != "trait") return
  if (pend[d] != "") {
    k = pend[d]; pend[d] = ""
    if (k == "const" && t ~ /^(fn|unsafe|async|extern)$/) { token(t); return }
    if (k == "static" && t == "mut") { pend[d] = k; return }
    if (t !~ /^[A-Za-z_][A-Za-z0-9_]*$/) { token(t); return }
    hdr[d] = k; hname[d] = t; htest[d] = ptest[d]; ptest[d] = 0
    if (k != "mod") def(t, k, htest[d])
    return
  }
  if (t ~ /^(fn|struct|enum|union|trait|type|const|static|mod)$/) { pend[d] = t; return }
  if (t == "impl") { hdr[d] = "impl"; iangle[d] = 0; ilast[d] = ""; htest[d] = ptest[d]; ptest[d] = 0; return }
  if (t == "use") { hdr[d] = "use"; ptest[d] = 0; return }
  if (t == ";") ptest[d] = 0
}
# Tokens between an item name (or `impl`) and the `{` or `;` that ends it.
# An impl block is named after its self type: the last name outside generic
# arguments, after `for` when there is one.
function header(t,    k) {
  k = hdr[d]
  if (k == "use") { if (t == ";") hdr[d] = ""; return }
  if (k == "impl") {
    if (t == "<") iangle[d]++
    else if (t == ">") iangle[d]--
    else if (iangle[d] == 0 && t == "for") ilast[d] = ""
    else if (iangle[d] == 0 && t == "where") iangle[d] = -1000
    else if (t == "{" && iangle[d] <= 0) { hdr[d] = ""; push("impl", ilast[d], htest[d]) }
    else if (iangle[d] == 0 && t ~ /^[A-Za-z_][A-Za-z0-9_]*$/ && t !~ /^(dyn|mut|const|unsafe)$/) ilast[d] = t
    return
  }
  if (t == "(" || t == "[") { pd[d]++; return }
  if (t == ")" || t == "]") { if (pd[d] > 0) pd[d]--; return }
  if (t == "{") {
    if (pd[d] > 0 || k == "const" || k == "static" || k == "type") { push("skip"); return }
    hdr[d] = ""
    if (k == "mod") push(htest[d] ? "skip" : "mod", hname[d])
    else if (k == "enum" || k == "trait") push(k, hname[d], htest[d])
    else push("skip")
    return
  }
  if (t == ";" && pd[d] == 0) {
    hdr[d] = ""
    if (k == "mod") printf "%s\t%s\t%s\n", htest[d] ? "testmod" : "mod", mp[d] "::" hname[d], filemod
  }
}
function tokens(code,    t) {
  while (match(code, /r#[A-Za-z_][A-Za-z0-9_]*|[A-Za-z_][A-Za-z0-9_]*|::|->|=>|[^ \t]/)) {
    t = substr(code, RSTART, RLENGTH)
    code = substr(code, RSTART + RLENGTH)
    if (t ~ /^r#/) t = substr(t, 3)
    if (t !~ /^[0-9]/) token(t)
  }
}
BEGIN {
  Q = "\047"; LEXSTART = "/[/*]|[\"\047]"
  while ((getline rec < sources) > 0) {
    split(rec, r, "\t"); file = r[3]; filemod = r[2]
    d = 0; bk[0] = "mod"; mp[0] = r[2]; bt[0] = 0; pd[0] = 0; hdr[0] = ""; pend[0] = ""; ptest[0] = 0
    st = ""; attr = 0; attrwait = 0; lno = 0; underflow = 0
    while ((getline line < file) > 0) {
      lno++
      code = lex(line)
      if (file == hfile) print code > hcode
      tokens(code)
    }
    close(file)
    if (st != "" || d != 0 || attr || hdr[0] != "" || underflow)
      printf "desync\t%s\n", file
  }
  close(sources)
}
' >"$tmp/index"
if grep -q '^desync	' "$tmp/index"; then
  die "cannot follow the Rust source of $(grep '^desync	' "$tmp/index" | cut -f2 | tr '\n' ' ')(unbalanced braces or an unterminated literal); Check G cannot index it"
fi

# Resolve the citations.
select_lines '^test_only	' "$tmp/manifest" | cut -f2 >"$tmp/test_only"
if ! awk -v crates="$tmp/crates" -v defs="$tmp/index" -v externals="$tmp/externals" \
  -v test_only="$tmp/test_only" -v hcode="$tmp/hcode" -v hfile="$HANDLER_FILE" \
  -v manifest="$MANIFEST" -v stats="$tmp/stats.g" "$lib"'
# A citation without its trailing `(...)`, `<...>` or `::*`. The names in a
# stripped `<...>` are kept in generic[] to be checked on their own.
function citation(t,    prev, g, n, i, part) {
  ngeneric = 0
  do {
    prev = t
    sub(/\([^()]*\)$/, "", t)
    if (t ~ />$/ && index(t, "<")) {
      g = substr(t, index(t, "<"))
      t = substr(t, 1, index(t, "<") - 1)
      gsub(/[^A-Za-z0-9_:]+/, " ", g)
      n = split(g, part, " ")
      for (i = 1; i <= n; i++) if (is_citation(part[i])) generic[++ngeneric] = part[i]
    }
    sub(/::\*$/, "", t)
  } while (t != prev)
  return t
}
function is_citation(t) {
  return t ~ /^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)+$/ || (t ~ /^[A-Z][A-Za-z0-9]*$/ && t ~ /[a-z]/)
}
# Sets inscope[] to the crates a doc may cite and returns their description:
# the crate that owns the doc plus its path dependencies, or every crate.
function scope_of(doc,    dir, c, best, s, i) {
  dir = dirname(doc); best = ESCAPED
  for (c in cname) {
    if (c == "" && !(dir == "src" || index(dir, "src/") == 1)) continue
    if (c != "" && dir != c && index(dir, c "/") != 1) continue
    if (best == ESCAPED || length(c) > length(best)) best = c
  }
  for (c in inscope) delete inscope[c]
  if (best == ESCAPED) {
    for (c in cname) inscope[cname[c]] = 1
    return "any crate"
  }
  inscope[cname[best]] = 1
  s = "crate " cpkg[best]
  for (i = 1; i <= ndep[best]; i++) {
    inscope[cname[dep[best, i]]] = 1
    s = s (i == 1 ? " or its dependency " : ", ") cpkg[dep[best, i]]
  }
  return s
}
# Fills hit[] with the in-scope items whose path ends with the citation, one
# per path (cfg variants of one item share a path), taking test-only items
# only when no other item matches. Returns their count; hittest says which.
function resolve(cite,    last, i, k, c, seen, pass) {
  last = cite; sub(/.*::/, "", last)
  for (pass = 0; pass <= 1; pass++) {
    nhit = 0; hittest = pass
    for (i = 1; i <= nbyname[last]; i++) {
      k = byname[last, i]
      if (dtest[k] != pass) continue
      c = canon[k]; sub(/::.*/, "", c)
      if (!(c in inscope)) continue
      c = "::" canon[k]
      if (substr(c, length(c) - length(cite) - 1) != "::" cite) continue
      if ((canon[k], cite, pass) in seen) continue
      seen[canon[k], cite, pass] = 1
      hit[++nhit] = k
    }
    if (nhit) return nhit
  }
  return 0
}
function short(k,    p) { p = canon[k]; sub(/^[^:]*::/, "", p); return p }
function hits(    i, s) {
  s = ""
  for (i = 1; i <= nhit; i++) s = s (i > 1 ? ", " : "") loc[hit[i]] " (" short(hit[i]) ")"
  return s
}
# The shortest qualified form of each hit: the item and its parent.
function qualifiers(    i, s, n, seg) {
  s = ""
  for (i = 1; i <= nhit; i++) {
    n = split(canon[hit[i]], seg, "::")
    s = s (i > 1 ? ", " : "") "`" seg[n - 1] "::" seg[n] "`"
  }
  return s
}
function check_symbol(where, doc, raw, t,    scope, n, last, root) {
  scope = scope_of(doc)
  root = t; sub(/::.*/, "", root)
  if (t in external) {
    used[t] = 1
    if (resolve(t) > 0)
      report("G", where, "`" raw "` is listed as external in " manifest " but SlashIt defines it: " hits() "; cite the item SlashIt does not define by its full path (such as `std::any::TypeId`) and list that instead")
    return
  }
  if (t ~ /(^|::)(crate|self|super|Self)(::|$)/) {
    report("G", where, "`" raw "`: crate, self and super are not supported; qualify with module or type names, such as `module::Name`")
    return
  }
  n = resolve(t)
  if (n > 1) {
    report("G", where, "`" raw "` is ambiguous in " scope ": " hits() "; qualify the citation as one of " qualifiers())
    return
  }
  if (n == 1 && !hittest) {
    if (t in testonly) report("G", where, "`" raw "` names " short(hit[1]) " (" loc[hit[1]] "), which is not test-only; remove it from [citations] test_only in " manifest)
    return
  }
  if (n == 1) {
    if (t in testonly) { usedtest[t] = 1; return }
    report("G", where, "`" raw "` names only " short(hit[1]) " (" loc[hit[1]] "), which is compiled only under cfg(test); cite a production item, or list the citation under [citations] test_only in " manifest)
    return
  }
  last = t; sub(/.*::/, "", last)
  if (t != last && resolve(last) > 0)
    report("G", where, "`" raw "` names nothing defined in " scope "; items named " last ": " hits() " (cite the defining module; re-exports do not count)")
  else if (root in crate_names)
    report("G", where, "`" raw "` names nothing defined in " scope "; correct the citation")
  else
    report("G", where, "`" raw "` names nothing defined in " scope "; correct the citation, or add it to [citations] external in " manifest " if SlashIt does not define it")
}
# The names registered by the one tauri::generate_handler![...] in hfile.
function load_handlers(    line, all, rest, n, i, c, depth, body, parts, np, j, part) {
  loaded = 1; all = ""
  while ((getline line < hcode) > 0) all = all " " line
  close(hcode)
  n = 0; rest = all
  while (match(rest, /generate_handler[ \t]*![ \t]*\[/)) { n++; rest = substr(rest, RSTART + RLENGTH) }
  if (n != 1) fatal("expected one tauri::generate_handler![...] in " hfile ", found " n)
  match(all, /generate_handler[ \t]*![ \t]*\[/)
  rest = substr(all, RSTART + RLENGTH)
  depth = 1; body = ""
  for (i = 1; i <= length(rest); i++) {
    c = substr(rest, i, 1)
    if (c == "[") depth++
    else if (c == "]" && --depth == 0) break
    body = body (depth == 1 ? c : " ")
  }
  if (depth != 0) fatal("the tauri::generate_handler! list in " hfile " is not closed")
  gsub(/[#!\]]/, " ", body)
  np = split(body, parts, ",")
  for (j = 1; j <= np; j++) {
    part = parts[j]; gsub(/[ \t]/, "", part); sub(/.*::/, "", part)
    if (part != "") registered[part] = 1
  }
}
function fatal(msg) { printf "agent-docs: error: %s\n", msg; failed = 1; exit 2 }
BEGIN {
  while ((getline line < crates) > 0) {
    split(line, f, "\t")
    if (f[1] == "crate") { cname[f[2]] = f[3]; cpkg[f[2]] = f[4]; crate_names[f[3]] = 1; reach[f[3]] = 1 }
    else dep[f[2], ++ndep[f[2]]] = f[3]
  }
  close(crates)
  for (c in ndep)
    for (i = 1; i <= ndep[c]; i++)
      if (!(dep[c, i] in cname)) fatal(c "/Cargo.toml: path dependency " dep[c, i] " is not a crate in this repository")
  while ((getline line < defs) > 0) {
    split(line, f, "\t")
    if (f[1] == "testmod") testmod[f[2]] = 1
    else if (f[1] == "mod") { nm++; mchild[nm] = f[2]; mparent[nm] = f[3] }
    else { nd++; dname[nd] = f[2]; canon[nd] = f[3]; dmod[nd] = f[4]; loc[nd] = f[5]; dtest[nd] = f[7]; dfile[nd] = f[8] }
  }
  close(defs)
  # A file counts only when a crate root reaches it through `mod name;`
  # declarations: an orphaned file, or one loaded under cfg(test) or from a
  # `#[path]` elsewhere, does not.
  do {
    grew = 0
    for (i = 1; i <= nm; i++)
      if (!(mchild[i] in reach) && (mparent[i] in reach)) { reach[mchild[i]] = 1; grew = 1 }
  } while (grew)
  for (k = 1; k <= nd; k++) {
    if (!(dfile[k] in reach)) continue
    m = dmod[k]; skip = 0
    do if (m in testmod) skip = 1; while (!skip && sub(/::[^:]*$/, "", m))
    if (!skip) byname[dname[k], ++nbyname[dname[k]]] = k
  }
  while ((getline line < externals) > 0) {
    external[line] = 1
    root = line; sub(/::.*/, "", root)
    if (root in crate_names) report("G", manifest, "external citation \"" line "\" starts with the SlashIt crate " root "; it cannot be external")
  }
  close(externals)
  while ((getline line < test_only) > 0) testonly[line] = 1
  close(test_only)
}
FNR == 1 { fence = ""; commands = 0 }
{
  if (fence != "") { if (fence_closes($0, fence)) fence = ""; next }
  if ((fence = fence_marker($0)) != "") next
  # "Tauri Commands Exposed" (any case) runs to the next heading of its level
  # or above; its list items name registered commands.
  if (match($0, /^#+ /)) {
    level = RLENGTH - 1
    if (commands && level <= clevel) commands = 0
    h = tolower($0); sub(/^#+ +/, "", h); sub(/[ #]*$/, "", h)
    if (h == "tauri commands exposed") { commands = 1; clevel = level }
  }
  if (commands && match($0, /^ *[-*+] `[^`]+`/)) {
    t = citation(substr($0, index($0, "`") + 1, RSTART + RLENGTH - index($0, "`") - 2))
    if (t ~ /^[a-z_][a-z0-9_]*$/) {
      if (!loaded) load_handlers()
      ncommands++
      if (!(t in registered))
        report("G", FILENAME ":" FNR, "Tauri command `" t "` is listed under \"Tauri Commands Exposed\" but is not registered in tauri::generate_handler! in " hfile)
    }
  }
  line = $0
  while (match(line, /`[^`]+`/)) {
    raw = substr(line, RSTART + 1, RLENGTH - 2)
    line = substr(line, RSTART + RLENGTH)
    t = citation(raw)
    if (is_citation(t)) { ncited++; check_symbol(FILENAME ":" FNR, FILENAME, raw, t) }
    for (i = 1; i <= ngeneric; i++) { ncited++; check_symbol(FILENAME ":" FNR, FILENAME, generic[i], generic[i]) }
  }
}
END {
  if (failed) exit 2
  for (t in external)
    if (!(t in used)) report("G", manifest, "external citation \"" t "\" is not cited by any AGENTS.md; remove it")
  for (t in testonly)
    if (!(t in usedtest)) report("G", manifest, "test_only citation \"" t "\" is not a cited test-only item; remove it")
  printf "%d %d\n", ncited, ncommands > stats
}
' "${agent_docs[@]}" >"$tmp/g.raw"; then
  grep '^agent-docs: error:' "$tmp/g.raw" >&2 || printf 'agent-docs: error: Check G could not run\n' >&2
  exit 2
fi
sort "$tmp/g.raw" >"$tmp/out.g"
count "$tmp/out.g"

# --- Result -----------------------------------------------------------------

if [ "$problems" -gt 0 ]; then
  printf 'agent-docs: FAILED with %d problem(s)\n' "$problems" >&2
  exit 1
fi
read -r cited commands <"$tmp/stats.g"
printf 'agent-docs: OK (%d AGENTS.md, %d modules, %d Markdown files, %d code citations, %d Tauri commands)\n' \
  "$(wc -l <"$tmp/agents" | tr -d ' ')" \
  "$(grep -c '^module	' "$tmp/manifest")" \
  "$(wc -l <"$tmp/markdown" | tr -d ' ')" \
  "$cited" "$commands"
