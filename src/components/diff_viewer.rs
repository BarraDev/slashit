use leptos::prelude::*;

const DIFF_DEFAULT_COLLAPSED_KEY: &str = "slashit_diff_default_collapsed";

fn read_default_collapsed() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(DIFF_DEFAULT_COLLAPSED_KEY).ok().flatten())
        .map(|v| v == "true")
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
struct DiffFile {
    path: String,
    additions: usize,
    deletions: usize,
    lines: Vec<DiffLine>,
}

#[derive(Clone, Debug)]
struct DiffLine {
    kind: LineKind,
    content: String,
}

#[derive(Clone, Debug, PartialEq)]
enum LineKind {
    Addition,
    Deletion,
    Context,
    Header,
}

fn parse_diff(raw: &str) -> Vec<DiffFile> {
    // Group lines into one block per `diff --git` header first, then derive
    // that file's path from the whole block rather than from the header
    // line alone. `diff --git a/X b/Y` has no delimiter between X and Y
    // that survives a path containing the literal text " b/" (or, for the
    // fallback below, " a/") — see `extract_path`'s doc for the contract
    // this is derived from, reproduced directly against a real `git` binary
    // (git 2.55.0) rather than assumed.
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in raw.lines() {
        if line.starts_with("diff --git") {
            blocks.push(Vec::new());
        }
        if let Some(block) = blocks.last_mut() {
            block.push(line);
        }
    }

    blocks
        .into_iter()
        .map(|block| {
            let path = extract_path(&block);
            let mut file = DiffFile {
                path,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            };
            for line in block.iter().skip(1) {
                if line.starts_with("@@") || line.starts_with("+++") || line.starts_with("---") {
                    file.lines.push(DiffLine {
                        kind: LineKind::Header,
                        content: line.to_string(),
                    });
                } else if line.starts_with('+') {
                    file.additions += 1;
                    file.lines.push(DiffLine {
                        kind: LineKind::Addition,
                        content: line.to_string(),
                    });
                } else if line.starts_with('-') {
                    file.deletions += 1;
                    file.lines.push(DiffLine {
                        kind: LineKind::Deletion,
                        content: line.to_string(),
                    });
                } else {
                    file.lines.push(DiffLine {
                        kind: LineKind::Context,
                        content: line.to_string(),
                    });
                }
            }
            file
        })
        .collect()
}

/// One file block's display path, derived from git's actual output contract
/// rather than the `diff --git` line's own ambiguous shape.
///
/// Confirmed directly against a real `git` binary (git 2.55.0, this
/// project's own backend invokes plain `git diff <base>..HEAD` with no
/// extra flags — see `src-tauri/src/worktree/manager.rs`), across
/// modifications, adds, deletes, renames, spaces, the literal substrings
/// " a/" and " b/", quoted (C-style-escaped) paths, binary diffs and
/// mode-only changes:
///
/// - Whenever a block is a genuine rename (the `a/` and `b/` sides differ),
///   git always also emits a `rename from `/`rename to ` line naming the old
///   and new paths in full, to end of line, with nothing after — no
///   counterexample found. Copy detection is not enabled by this backend's
///   `git diff` invocation (no `-C`/`--find-copies`), so `copy from`/
///   `copy to` cannot currently appear; the identical handling below is
///   kept anyway, at negligible cost, since it is the same real git output
///   shape and its absence would be a silent trap if that invocation ever
///   changed.
/// - `+++ b/<path>` / `--- a/<path>` also always run to end of line, with
///   nothing after but a possible single trailing tab byte, which git adds
///   whenever the path contains whitespace, and which is stripped here.
/// - Both of the above are only absent for a binary diff (`Binary files
///   ... differ`) or a mode-only change with no content diff — and in
///   every such real case tested, the `diff --git` line's two halves were
///   identical. That is the one remaining case: parsing the `diff --git`
///   line itself only ever needs to disambiguate two *equal* halves, never
///   two different ones, which is exploited directly below rather than
///   guessed at with another delimiter heuristic.
/// - `core.quotePath` (default on) C-quotes a path containing a control
///   byte (always) or a non-ASCII byte (at its default setting), wrapping
///   it in `"..."` with backslash/octal escapes — on every line kind above,
///   including `diff --git` itself. Unquoted otherwise.
fn extract_path(block: &[&str]) -> String {
    // Bounded to the header region (before the first hunk), matching where
    // git actually places every line kind checked below: an added line
    // whose own text happens to start with "+++ " or "rename to " must
    // never be mistaken for one of these headers.
    let header: Vec<&&str> = block.iter().take_while(|l| !l.starts_with("@@")).collect();

    for line in &header {
        if let Some(rest) = line.strip_prefix("rename to ") {
            return unquote_path(rest);
        }
    }
    for line in &header {
        if let Some(rest) = line.strip_prefix("copy to ") {
            return unquote_path(rest);
        }
    }
    for line in &header {
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest.trim_end() != "/dev/null" {
                return unquote_prefixed_path(rest, "b/");
            }
        }
    }
    for line in &header {
        if let Some(rest) = line.strip_prefix("--- ") {
            if rest.trim_end() != "/dev/null" {
                return unquote_prefixed_path(rest, "a/");
            }
        }
    }
    if let Some(line) = block.first() {
        if let Some(path) = extract_equal_halves(line) {
            return path;
        }
    }
    // Last resort. Not expected to be reached against any real git output
    // this analysis covered — kept only so a case it missed degrades to the
    // pre-existing behavior rather than to "unknown".
    block
        .first()
        .and_then(|line| line.split(" b/").last())
        .unwrap_or("unknown")
        .to_string()
}

/// The path after a `rename to `/`copy to ` prefix. No `a/`/`b/` prefix of
/// its own to strip — confirmed directly against real git output.
fn unquote_path(rest: &str) -> String {
    match rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => unquote_c_style(inner),
        None => rest.trim_end_matches('\t').to_string(),
    }
}

/// The path after a `+++ `/`--- ` prefix, with the `a/`/`b/` git itself
/// always adds stripped back off (present inside the quotes too, when
/// quoted, since the whole `"a/path"` is one C-quoted unit).
fn unquote_prefixed_path(rest: &str, want_prefix: &str) -> String {
    // Trimmed first, not only in the unquoted fallback: git's
    // disambiguating trailing tab (added whenever the path itself contains
    // whitespace) is appended after the closing quote too, so `rest` can
    // be `"b/caf\303\251.txt"\t` — stripping the closing `"` before
    // trimming the tab would find `\t` where a `"` was expected and
    // silently fall through to the unquoted branch, returning the raw
    // quoted-and-escaped text unparsed. Confirmed against real git output
    // for a quoted, whitespace-containing path.
    let rest = rest.trim_end_matches('\t');
    let unprefixed = match rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => unquote_c_style(inner),
        None => rest.to_string(),
    };
    unprefixed
        .strip_prefix(want_prefix)
        .map(str::to_string)
        .unwrap_or(unprefixed)
}

/// `diff --git a/X b/Y` itself, for the one case with no other
/// disambiguating line (a binary diff or a mode-only change) — every real
/// case reaching here has X == Y, so the line's total length alone
/// determines where one ends and the other begins, with no dependence on
/// what characters X contains.
fn extract_equal_halves(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if let Some(after_quote) = rest.strip_prefix('"') {
        let end = find_unescaped_quote(after_quote)?;
        let first = unquote_c_style(&after_quote[..end]);
        let remainder = after_quote[end + 1..].trim_start();
        let second_raw = remainder.strip_prefix('"')?;
        let second_end = find_unescaped_quote(second_raw)?;
        let second = unquote_c_style(&second_raw[..second_end]);
        let a = first.strip_prefix("a/").unwrap_or(&first);
        let b = second.strip_prefix("b/").unwrap_or(&second);
        (a == b).then(|| b.to_string())
    } else {
        let tail = rest.strip_prefix("a/")?;
        let bytes = tail.as_bytes();
        let len = bytes.len().checked_sub(3)?;
        if len % 2 != 0 {
            return None;
        }
        let half = len / 2;
        if !tail.is_char_boundary(half) || !tail.is_char_boundary(half + 3) {
            return None;
        }
        (&tail[half..half + 3] == " b/" && tail[..half] == tail[half + 3..])
            .then(|| tail[..half].to_string())
    }
}

/// The index of the first `"` in `s` that is not the second byte of a
/// backslash escape. Git only ever escapes a literal `\` or `"` as a
/// 2-byte sequence, or another byte as a 4-byte `\NNN` octal sequence —
/// skipping exactly one extra byte after any backslash correctly steps
/// over either shape without needing to know which one it is.
fn find_unescaped_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Git's own C-style quoting, reversed: `\\`, `\"`, `\t`, `\n`, `\r` as
/// named escapes, `\NNN` (3-digit octal) as a raw byte — confirmed
/// directly against real git output (a non-ASCII filename produces exactly
/// the `\303\251`-shaped octal this decodes). Escaped bytes are assembled
/// before the final UTF-8 conversion, since an octal escape names one byte
/// of a multi-byte sequence, not one character.
fn unquote_c_style(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'"' => {
                    out.push(b'"');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                d @ b'0'..=b'7'
                    if bytes.len() >= i + 4
                        && (b'0'..=b'7').contains(&bytes[i + 2])
                        && (b'0'..=b'7').contains(&bytes[i + 3]) =>
                {
                    // A byte only has 256 values, so real git never emits
                    // above `\377` — but this text does not have to be real
                    // git output by the time it reaches a parser, so the
                    // multiply stays checked rather than trusting that: an
                    // out-of-range 3-digit escape (`\777`) would otherwise
                    // overflow `u8` and panic under this project's default
                    // dev-profile overflow checks.
                    let val = (d - b'0') as u32 * 64
                        + (bytes[i + 2] - b'0') as u32 * 8
                        + (bytes[i + 3] - b'0') as u32;
                    match u8::try_from(val) {
                        Ok(byte) => {
                            out.push(byte);
                            i += 4;
                        }
                        Err(_) => {
                            out.push(bytes[i]);
                            i += 1;
                        }
                    }
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Normalize a `git diff --stat` blob to one entry per line. Some callers
/// concatenate the stat into a single line; split on the trailing summary so
/// it renders cleanly in a <pre>.
fn normalize_stat(stat: &str) -> String {
    if stat.contains('\n') {
        return stat.to_string();
    }
    let mut out = String::with_capacity(stat.len() + 16);
    let bytes = stat.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        out.push(c);
        i += 1;
        if (c == '+' || c == '-') && i < bytes.len() && bytes[i] == b' ' {
            let rest = &stat[i..];
            let trimmed = rest.trim_start();
            let starts_path_or_totals = trimmed.chars().next().map(|ch| {
                ch.is_ascii_alphanumeric() || ch == '_' || ch == '/' || ch == '.'
            }).unwrap_or(false);
            if starts_path_or_totals {
                out.push('\n');
                while i < bytes.len() && bytes[i] == b' ' { i += 1; }
            }
        }
    }
    out
}

#[component]
pub fn DiffViewer(
    diff: String,
    #[prop(optional)] stat: Option<String>,
) -> impl IntoView {
    let files = parse_diff(&diff);
    let total_additions: usize = files.iter().map(|f| f.additions).sum();
    let total_deletions: usize = files.iter().map(|f| f.deletions).sum();
    let file_count = files.len();

    if file_count == 0 {
        return view! {
            <div class="diff-viewer">
                <div class="flex flex-col items-center justify-center py-12 text-center">
                    <svg class="w-12 h-12 text-white/10 mb-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                    </svg>
                    <p class="text-white/40 text-sm">"No changes detected"</p>
                    <p class="text-white/20 text-xs mt-1">"The working directory matches the last commit"</p>
                </div>
            </div>
        }.into_any();
    }

    let default_collapsed = read_default_collapsed();
    let file_signals: Vec<RwSignal<bool>> = (0..file_count)
        .map(|_| RwSignal::new(default_collapsed))
        .collect();

    let signals_for_collapse = file_signals.clone();
    let collapse_all = move |_| {
        for s in &signals_for_collapse { s.set(true); }
    };
    let signals_for_expand = file_signals.clone();
    let expand_all = move |_| {
        for s in &signals_for_expand { s.set(false); }
    };

    let show_stat = RwSignal::new(false);
    let stat_text = stat.as_ref().map(|s| normalize_stat(s));
    let has_stat = stat_text.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false);

    view! {
        <div class="diff-viewer space-y-3">
            // Summary header — compact, with global actions
            <div class="rounded-lg bg-white/[0.03] border border-white/[0.06]">
                <div class="flex items-center gap-3 px-3 py-2 flex-wrap">
                    <span class="text-sm text-white/70 font-medium">
                        {file_count} " file" {if file_count != 1 { "s" } else { "" }} " changed"
                    </span>
                    <span class="text-sm text-emerald-400 font-mono">{"+"}{total_additions}</span>
                    <span class="text-sm text-red-400 font-mono">{"-"}{total_deletions}</span>

                    <div class="ml-auto flex items-center gap-1">
                        <button
                            class="px-2 py-1 text-xs text-white/60 hover:text-white/90 hover:bg-white/[0.06] rounded transition-colors"
                            on:click=expand_all
                            title="Expand all files"
                        >"Expand all"</button>
                        <button
                            class="px-2 py-1 text-xs text-white/60 hover:text-white/90 hover:bg-white/[0.06] rounded transition-colors"
                            on:click=collapse_all
                            title="Collapse all files"
                        >"Collapse all"</button>
                        {has_stat.then(|| view! {
                            <button
                                class="px-2 py-1 text-xs text-white/60 hover:text-white/90 hover:bg-white/[0.06] rounded transition-colors"
                                on:click=move |_| show_stat.update(|v| *v = !*v)
                                title="Toggle diffstat"
                            >
                                {move || if show_stat.get() { "Hide diffstat" } else { "Show diffstat" }}
                            </button>
                        })}
                    </div>
                </div>

                {stat_text.map(|s| view! {
                    <Show when=move || show_stat.get()>
                        <div class="border-t border-white/[0.06] px-3 py-2 overflow-x-auto">
                            <pre class="text-xs font-mono leading-5 text-white/60 m-0 whitespace-pre">
                                {s.clone()}
                            </pre>
                        </div>
                    </Show>
                })}
            </div>

            // File sections
            {files.into_iter().enumerate().map(|(idx, file)| {
                let path = file.path.clone();
                let adds = file.additions;
                let dels = file.deletions;
                let collapsed = file_signals[idx];

                view! {
                    <div class="rounded-lg border border-white/[0.06] overflow-hidden">
                        <button
                            class="w-full flex items-center gap-2 px-3 py-2 bg-white/[0.03] hover:bg-white/[0.05] transition-colors text-left"
                            on:click=move |_| collapsed.update(|v| *v = !*v)
                        >
                            <span class="text-xs text-white/40">{move || if collapsed.get() { "▶" } else { "▼" }}</span>
                            <span class="text-sm font-mono text-white/80 truncate">{path.clone()}</span>
                            <span class="ml-auto flex items-center gap-2 text-xs">
                                <span class="text-emerald-400">{"+"}{adds}</span>
                                <span class="text-red-400">{"-"}{dels}</span>
                            </span>
                        </button>

                        <Show when=move || !collapsed.get()>
                            <div class="overflow-x-auto">
                                <pre class="text-xs font-mono leading-5 p-0 m-0">
                                    {file.lines.iter().map(|line| {
                                        let (bg, text_color) = match line.kind {
                                            LineKind::Addition => ("bg-emerald-500/10", "text-emerald-300"),
                                            LineKind::Deletion => ("bg-red-500/10", "text-red-300"),
                                            LineKind::Header => ("bg-blue-500/10", "text-blue-300"),
                                            LineKind::Context => ("", "text-white/50"),
                                        };
                                        let content = line.content.clone();
                                        view! {
                                            <div class=format!("px-3 min-h-[1.25rem] {} {}", bg, text_color)>
                                                {content}
                                            </div>
                                        }
                                    }).collect_view()}
                                </pre>
                            </div>
                        </Show>
                    </div>
                }
            }).collect_view()}
        </div>
    }.into_any()
}

#[cfg(test)]
mod tests {
    use super::{normalize_stat, parse_diff};

    fn only_path(raw: &str) -> String {
        let files = parse_diff(raw);
        assert_eq!(
            files.len(),
            1,
            "expected exactly one file: {:?}",
            files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        files[0].path.clone()
    }

    #[test]
    fn an_ordinary_modification_is_parsed() {
        // Real `git diff` output (git 2.55.0).
        let raw = "diff --git a/ordinary.txt b/ordinary.txt\nindex a29bdeb..c0d0fb4 100644\n--- a/ordinary.txt\n+++ b/ordinary.txt\n@@ -1 +1,2 @@\n line1\n+line2\n";
        assert_eq!(only_path(raw), "ordinary.txt");
    }

    #[test]
    fn an_added_file_is_parsed_from_the_b_side() {
        let raw = "diff --git a/new_file.txt b/new_file.txt\nnew file mode 100644\nindex 0000000..d5a09df\n--- /dev/null\n+++ b/new_file.txt\n@@ -0,0 +1 @@\n+brand new\n";
        assert_eq!(only_path(raw), "new_file.txt");
    }

    #[test]
    fn a_deleted_file_is_parsed_from_the_a_side() {
        let raw = "diff --git a/to_delete.txt b/to_delete.txt\ndeleted file mode 100644\nindex de98044..0000000\n--- a/to_delete.txt\n+++ /dev/null\n@@ -1,3 +0,0 @@\n-a\n-b\n-c\n";
        assert_eq!(only_path(raw), "to_delete.txt");
    }

    #[test]
    fn a_path_containing_spaces_is_parsed_whole() {
        // Real git also appends a disambiguating trailing tab to `+++`
        // whenever the path contains whitespace; the parser must strip it.
        let raw = "diff --git a/path with spaces.txt b/path with spaces.txt\nnew file mode 100644\nindex 0000000..ce01362\n--- /dev/null\n+++ b/path with spaces.txt\t\n@@ -0,0 +1 @@\n+hello\n";
        assert_eq!(only_path(raw), "path with spaces.txt");
    }

    #[test]
    fn a_path_containing_the_literal_substring_space_b_slash_is_parsed_whole() {
        // The confirmed bug: real `git diff` output for a new file inside a
        // directory literally named `weird b`. The old
        // `line.split(" b/").last()` parser returned "file.txt", silently
        // dropping the `weird b/` prefix.
        let raw = "diff --git a/weird b/file.txt b/weird b/file.txt\nnew file mode 100644\nindex 0000000..a6f054c\n--- /dev/null\n+++ b/weird b/file.txt\t\n@@ -0,0 +1 @@\n+content b\n";
        assert_eq!(only_path(raw), "weird b/file.txt");
    }

    #[test]
    fn a_path_containing_the_literal_substring_space_a_slash_is_parsed_whole() {
        let raw = "diff --git a/weird a/file.txt b/weird a/file.txt\nnew file mode 100644\nindex 0000000..e4542d7\n--- /dev/null\n+++ b/weird a/file.txt\t\n@@ -0,0 +1 @@\n+content a\n";
        assert_eq!(only_path(raw), "weird a/file.txt");
    }

    #[test]
    fn a_renamed_file_uses_the_new_path_from_rename_to() {
        let raw = "diff --git a/old name.txt b/new name.txt\nsimilarity index 100%\nrename from old name.txt\nrename to new name.txt\n";
        assert_eq!(only_path(raw), "new name.txt");
    }

    #[test]
    fn a_rename_whose_paths_contain_space_a_slash_and_space_b_slash_uses_rename_to_whole() {
        // The case that actually motivated this unit: a rename from a
        // "weird a" directory into a "weird b" directory, both containing
        // spaces. The old parser's `split(" b/").last()` on the `diff --git`
        // header truncated this to "from weird a file.txt", dropping the
        // `weird b/` destination prefix entirely.
        let raw = "diff --git a/weird a/file.txt b/weird b/from weird a file.txt\nsimilarity index 100%\nrename from weird a/file.txt\nrename to weird b/from weird a file.txt\n";
        assert_eq!(only_path(raw), "weird b/from weird a file.txt");
    }

    #[test]
    fn a_renamed_and_edited_file_still_uses_the_new_path() {
        let raw = "diff --git a/big_orig.txt b/big_renamed.txt\nsimilarity index 95%\nrename from big_orig.txt\nrename to big_renamed.txt\nindex 2cce9ac..a22d2c0 100644\n--- a/big_orig.txt\n+++ b/big_renamed.txt\n@@ -2,7 +2,7 @@ line number 1\n line 2\n-line number 5 old\n+line number 5 MODIFIED\n line 6\n";
        assert_eq!(only_path(raw), "big_renamed.txt");
    }

    #[test]
    fn a_quoted_non_ascii_path_is_unquoted() {
        // core.quotePath (default on) octal-escapes a non-ASCII byte. Real
        // byte sequence git 2.55.0 wrote for "café.txt".
        let raw = "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\nnew file mode 100644\nindex 0000000..d95f3ad\n--- /dev/null\n+++ \"b/caf\\303\\251.txt\"\n@@ -0,0 +1 @@\n+content\n";
        assert_eq!(only_path(raw), "café.txt");
    }

    #[test]
    fn a_quoted_path_with_gits_own_disambiguating_trailing_tab_is_still_unquoted() {
        // git appends a trailing tab to `---`/`+++` whenever the path
        // contains whitespace, including when the path is also C-quoted for
        // a non-ASCII byte. Real byte-for-byte output from git 2.55.0 for a
        // modified file named "notes café final.txt".
        let raw = "diff --git \"a/notes caf\\303\\251 final.txt\" \"b/notes caf\\303\\251 final.txt\"\nindex 626799f..8c1384d 100644\n--- \"a/notes caf\\303\\251 final.txt\"\t\n+++ \"b/notes caf\\303\\251 final.txt\"\t\n@@ -1 +1 @@\n-v1\n+v2\n";
        assert_eq!(only_path(raw), "notes café final.txt");
    }

    #[test]
    fn an_out_of_range_octal_escape_does_not_panic() {
        // Deliberately crafted, not real git output (a byte only has 256
        // values, so real git never emits above `\377`): the property under
        // test is "does not crash", not a specific decode.
        let raw = "diff --git \"a/x\\777y\" \"b/x\\777y\"\nnew file mode 100644\n";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 1, "must still produce one file, not panic");
    }

    #[test]
    fn a_binary_new_file_whose_path_contains_space_b_slash_has_no_plus_plus_plus_line_and_still_parses() {
        // Real `git diff` output for a new binary file: no `+++`/`---`
        // line at all, exercising the `diff --git` line's own equal-halves
        // fallback rather than the `+++` priority path above.
        let raw = "diff --git a/weird b/bin.dat b/weird b/bin.dat\nnew file mode 100644\nindex 0000000..0f49c4a\nBinary files /dev/null and b/weird b/bin.dat differ\n";
        assert_eq!(only_path(raw), "weird b/bin.dat");
    }

    #[test]
    fn a_mode_only_change_has_no_hunk_and_still_parses() {
        // Real `git diff` output for a chmod with no content change: no
        // hunk, no `+++`/`---` line either.
        let raw = "diff --git a/ordinary.txt b/ordinary.txt\nold mode 100644\nnew mode 100755\n";
        assert_eq!(only_path(raw), "ordinary.txt");
    }

    #[test]
    fn a_quoted_embedded_newline_path_is_unquoted_without_breaking_file_counting() {
        // core.quotePath always C-quotes a control byte, named-escaped
        // (`\n`), regardless of core.quotePath's own setting. A path
        // containing a raw newline byte (legal in a Linux filename) must
        // not be mistaken for a second file's header.
        let raw = "diff --git \"a/evil\\n+FAKE.txt\" \"b/evil\\n+FAKE.txt\"\nnew file mode 100644\nindex 0000000..637f034\n--- /dev/null\n+++ \"b/evil\\n+FAKE.txt\"\n@@ -0,0 +1 @@\n+content\n";
        let files = parse_diff(raw);
        assert_eq!(
            files.len(),
            1,
            "the quoted header must not be mistaken for two files"
        );
        assert_eq!(files[0].path, "evil\n+FAKE.txt");
    }

    #[test]
    fn multiple_files_in_one_patch_are_each_counted_and_pathed_correctly() {
        let raw = "diff --git a/one.txt b/one.txt\nindex 1..2 100644\n--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/two.txt b/two.txt\nnew file mode 100644\nindex 0..3 100644\n--- /dev/null\n+++ b/two.txt\n@@ -0,0 +1 @@\n+c\n";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "one.txt");
        assert_eq!(files[1].path, "two.txt");
    }

    #[test]
    fn content_lines_that_look_like_patch_headers_are_not_misclassified() {
        // An added/removed line whose own text happens to start with
        // "diff --git", "+++ " or "rename to " must stay inside its file's
        // block, counted as content, not treated as a new file or as
        // metadata that overrides the real header's path.
        let raw = "diff --git a/one.txt b/one.txt\nindex 1..2 100644\n--- a/one.txt\n+++ b/one.txt\n@@ -1,2 +1,3 @@\n line1\n+diff --git a/fake.txt b/fake.txt\n+rename to nope.txt\n";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 1, "content lines must not start new blocks");
        assert_eq!(files[0].path, "one.txt");
        assert_eq!(files[0].additions, 2);
    }

    #[test]
    fn preserves_already_multiline_stat() {
        let input = "a.txt | 5 ++---\nb.txt | 2 +-\n 2 files changed, 4 insertions(+), 3 deletions(-)\n";
        assert_eq!(normalize_stat(input), input);
    }

    #[test]
    fn splits_single_line_stat_into_one_entry_per_file() {
        let input = "src-tauri/locales/de.json | 5 ++- src-tauri/locales/en.json | 5 ++- src-tauri/src/tray.rs | 73 ++++++++-- 10 files changed, 86 insertions(+), 72 deletions(-)";
        let out = normalize_stat(input);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "got: {:?}", lines);
        assert_eq!(lines[0], "src-tauri/locales/de.json | 5 ++-");
        assert_eq!(lines[1], "src-tauri/locales/en.json | 5 ++-");
        assert_eq!(lines[2], "src-tauri/src/tray.rs | 73 ++++++++--");
        assert_eq!(lines[3], "10 files changed, 86 insertions(+), 72 deletions(-)");
    }

    #[test]
    fn does_not_split_inside_a_filename_with_plus_or_dash() {
        let input = "a+b.txt | 3 ++- c-d.txt | 1 +";
        let out = normalize_stat(input);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("a+b.txt"));
        assert!(out.contains("c-d.txt"));
    }
}

/// Modal wrapper for viewing diffs
#[component]
pub fn DiffModal(
    show: RwSignal<bool>,
    diff: Signal<String>,
    stat: Signal<String>,
    title: String,
) -> impl IntoView {
    view! {
        <Show when=move || show.get()>
            {
                let title = title.clone();
                let diff_val = diff.get();
                let stat_val = stat.get();
                view! {
                    <div
                        class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
                        on:click=move |_| show.set(false)
                    >
                        <div
                            class="bg-[#0e0e16] border border-white/[0.08] rounded-2xl w-[90vw] max-w-4xl max-h-[85vh] flex flex-col shadow-2xl"
                            on:click=move |e| e.stop_propagation()
                        >
                            <div class="flex items-center justify-between px-5 py-3 border-b border-white/[0.06]">
                                <h2 class="text-sm font-medium text-white/80">{title.clone()}</h2>
                                <button
                                    class="text-white/40 hover:text-white/80 transition-colors text-lg"
                                    on:click=move |_| show.set(false)
                                >
                                    "×"
                                </button>
                            </div>
                            <div class="overflow-y-auto p-4">
                                <DiffViewer
                                    diff=diff_val
                                    stat=stat_val
                                />
                            </div>
                        </div>
                    </div>
                }
            }
        </Show>
    }
}
