// SPDX-License-Identifier: MPL-2.0
//! The XDG base-directory variables a terminal shell is started with.
//!
//! An isolated instance redirects `XDG_*_HOME` into its own state tree. A
//! terminal started from that environment would not run the owner's daily
//! shell: tools such as mise would resolve their data and caches inside the
//! redirected tree. For the variables the redirect moved, the shell gets the
//! daily default back. Everything else is left as the launcher set it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What the terminal child's environment does to one XDG variable.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum XdgChange {
    Set(OsString),
    Unset,
}

/// The rebased variables, with their daily default relative to `HOME`.
const VARIABLES: [(&str, &str); 4] = [
    ("XDG_CONFIG_HOME", ".config"),
    ("XDG_DATA_HOME", ".local/share"),
    ("XDG_CACHE_HOME", ".cache"),
    ("XDG_STATE_HOME", ".local/state"),
];

/// Decides which XDG variables the terminal child sets or unsets.
///
/// `get` reads the launcher's environment and `home` is `HOME`. The comparison
/// is lexical: no symbolic link is resolved and the filesystem is not touched.
/// `XDG_RUNTIME_DIR` is never returned, so the terminal keeps the instance's
/// runtime directory.
///
/// A variable is returned only when its value must change:
/// - `SLASHIT_TERMINAL_<NAME>` set and non-empty: the child gets exactly that value.
/// - unset: nothing is returned.
/// - set but empty: the child's variable is removed.
/// - absolute, inside the redirected state base, not already the daily default,
///   and the redirect is active: the child gets the daily default.
///
/// The redirect is active when the launcher's `XDG_STATE_HOME` is absolute and
/// differs from `$HOME/.local/state`. Any other value passes through unchanged.
pub(super) fn terminal_xdg_changes(
    get: impl Fn(&str) -> Option<OsString>,
    home: &Path,
) -> Vec<(&'static str, XdgChange)> {
    let daily_state = home.join(".local/state");
    let state_base = get("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|base| base.is_absolute() && *base != daily_state);

    VARIABLES
        .iter()
        .filter_map(|&(name, daily_relative)| {
            let hatch = format!("SLASHIT_TERMINAL_{name}");
            if let Some(value) = get(&hatch).filter(|value| !value.is_empty()) {
                return Some((name, XdgChange::Set(value)));
            }
            let value = get(name)?;
            if value.is_empty() {
                return Some((name, XdgChange::Unset));
            }
            let path = PathBuf::from(&value);
            let daily = home.join(daily_relative);
            let redirected = state_base
                .as_deref()
                .is_some_and(|base| path.is_absolute() && path.starts_with(base));
            (redirected && path != daily).then(|| (name, XdgChange::Set(daily.into_os_string())))
        })
        .collect()
}
