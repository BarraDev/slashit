//! Strip the `name` section from the frontend module in a Trunk output
//! directory, in place, and prove nothing else changed.
//!
//! ```text
//! strip-wasm-names <dist-dir>          strip, verify, replace
//! strip-wasm-names --check <dist-dir>  fail unless stripped and pinned
//! ```
//!
//! Stripping also moves the Subresource Integrity pin in `dist/index.html` to
//! the stripped module; nothing else in the page changes.
//!
//! Only `scripts/build-acceptance-app.sh` should need to run this. See
//! `slashit_acceptance::wasm_names` for what is and is not allowed to change.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use slashit_acceptance::wasm_names;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, dist] if flag == "--check" => check(Path::new(dist)),
        [dist] if !dist.starts_with('-') => strip(Path::new(dist)),
        _ => bail!("usage: strip-wasm-names [--check] <dist-dir>"),
    }
}

/// The one `*_bg.wasm` Trunk wrote. More than one means a stale build is
/// mixed in, and guessing which one the page loads is how the wrong file gets
/// tested.
fn frontend_module(dist: &Path) -> Result<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dist).with_context(|| format!("reading {}", dist.display()))? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with("_bg.wasm"))
        {
            found.push(path);
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no *_bg.wasm in {}; did `trunk build` run?", dist.display()),
        many => bail!("more than one *_bg.wasm in {}: {many:?}", dist.display()),
    }
}

/// Replace, never truncate-and-write: an interrupted run must leave either
/// the old file or the new one, not half of one. A staging file left behind
/// by an interruption is gone after the next build: it starts with
/// `trunk build`, which recreates `dist/`.
fn replace(path: &Path, contents: &[u8]) -> Result<()> {
    let mut staged = path.as_os_str().to_owned();
    staged.push(".stripping");
    let staged = PathBuf::from(staged);
    std::fs::write(&staged, contents).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, path).with_context(|| format!("replacing {}", path.display()))
}

fn strip(dist: &Path) -> Result<()> {
    let path = frontend_module(dist)?;
    let page = dist.join("index.html");
    let original = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let html =
        std::fs::read_to_string(&page).with_context(|| format!("reading {}", page.display()))?;

    let stripped = wasm_names::strip(&original)?;
    wasm_names::verify(&original, &stripped)?;
    let updated = wasm_names::replace_integrity(&html, &original, &stripped)?;

    replace(&path, &stripped)?;
    replace(&page, updated.as_bytes())?;

    // Verify what is on disk, not what was in memory.
    let written = std::fs::read(&path).with_context(|| format!("re-reading {}", path.display()))?;
    let report = wasm_names::verify(&original, &written)?;
    let written_html =
        std::fs::read_to_string(&page).with_context(|| format!("re-reading {}", page.display()))?;
    ensure!(
        wasm_names::pins(&written_html, &written),
        "{} does not pin the stripped module",
        page.display()
    );
    println!("{}", path.display());
    println!("{report}");
    println!(
        "{}: integrity pin moved to {}",
        page.display(),
        wasm_names::integrity(&written)
    );
    Ok(())
}

fn check(dist: &Path) -> Result<()> {
    let path = frontend_module(dist)?;
    let module = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        !wasm_names::has_name_section(&module)?,
        "{} still has a `{}` section",
        path.display(),
        wasm_names::NAME_SECTION
    );
    let page = dist.join("index.html");
    let html =
        std::fs::read_to_string(&page).with_context(|| format!("reading {}", page.display()))?;
    ensure!(
        wasm_names::pins(&html, &module),
        "{} does not pin {} with its integrity hash",
        page.display(),
        path.display()
    );
    println!(
        "{}: no `{}` section, pinned by index.html",
        path.display(),
        wasm_names::NAME_SECTION
    );
    Ok(())
}
