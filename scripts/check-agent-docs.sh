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
#
# Exit status: 0 when every check passes, 1 when a check fails, 2 when the
# inputs cannot be read or understood (the check never passes by default).

set -Eeuo pipefail
export LC_ALL=C

readonly MANIFEST=docs/agent-docs.toml
readonly ROOT_DOC_MAX_LINES=250
readonly NESTED_DOC_MAX_LINES=150
readonly CODE_BLOCK_MAX_LINES=30

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
  arraykey["coverage"] = "roots"; arraykey["citations"] = "untracked"
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
    if (!(section in arraykey) && section != "modules") bad("unknown section [" section "]")
    else if (section in sections) bad("duplicate section [" section "]")
    sections[section] = 1
    next
  }

  if (section in arraykey) {
    if (line !~ /^[a-z]+[ \t]*=[ \t]*\[.*\]$/) { bad("expected: key = [\"...\", ...] on one line"); next }
    key = line; sub(/[ \t]*=.*/, "", key)
    if (key != arraykey[section]) { bad("unknown key \"" key "\" in [" section "]"); next }
    if ((section, key) in keys) { bad("duplicate key \"" key "\""); next }
    keys[section, key] = 1
    body = line; sub(/^[^[]*\[/, "", body); sub(/\][ \t]*$/, "", body)
    n = split(body, parts, ",")
    for (i = 1; i <= n; i++) {
      item = parts[i]; gsub(/^[ \t]+|[ \t]+$/, "", item)
      if (item == "" && i == n && n > 1) continue
      if (item !~ /^"[^"]*"$/) { bad("array items must be \"double-quoted\" paths"); continue }
      v = substr(item, 2, length(item) - 2)
      p = v
      if (key == "untracked" && !sub(/\/$/, "", p)) { bad("untracked entries are directories and end in /, got \"" v "\""); continue }
      if (!valid_path(p)) { bad("invalid path \"" v "\""); continue }
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

# --- Result -----------------------------------------------------------------

if [ "$problems" -gt 0 ]; then
  printf 'agent-docs: FAILED with %d problem(s)\n' "$problems" >&2
  exit 1
fi
printf 'agent-docs: OK (%d AGENTS.md, %d modules, %d Markdown files)\n' \
  "$(wc -l <"$tmp/agents" | tr -d ' ')" \
  "$(grep -c '^module	' "$tmp/manifest")" \
  "$(wc -l <"$tmp/markdown" | tr -d ' ')"
