// SPDX-License-Identifier: MPL-2.0
//! Which channel each published version belongs to, read from
//! `.github/release-channels.txt`.
//!
//! The file is the one declaration of a release's channel. `scripts/release-
//! preflight.sh` reads it to set the GitHub `prerelease` flag, and
//! `build.rs` reads it so that every build knows its own channel. A running
//! app therefore reports the channel it was built as, not the channel the
//! user has selected, and needs no `.git` directory at runtime.
//!
//! This module uses only `std`, because `build.rs` compiles it as well. The
//! rules here must match the preflight script's rules.

use std::fmt;

/// A publication channel: what a build is, or what an update preference selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseChannel {
    Stable,
    Prerelease,
}

impl ReleaseChannel {
    /// The spelling the declaration file, the persisted preference and the
    /// wire format all use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Prerelease => "prerelease",
        }
    }

    /// Exact, case-sensitive match on the spelling above.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "stable" => Some(Self::Stable),
            "prerelease" => Some(Self::Prerelease),
            _ => None,
        }
    }
}

/// A published application version: plain `MAJOR.MINOR.PATCH`, nothing else.
///
/// Suffixes, build metadata and a fourth component are not versions this
/// project publishes, so they are not representable here. Ordering is by the
/// three numbers, which is the order the updater compares in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlainVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl PlainVersion {
    /// Parse exactly `MAJOR.MINOR.PATCH`: decimal digits, no leading zeros
    /// except a lone `0`, and no other characters.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = plain_component(parts.next()?)?;
        let minor = plain_component(parts.next()?)?;
        let patch = plain_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self { major, minor, patch })
    }
}

fn plain_component(part: &str) -> Option<u64> {
    let digits_only = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let no_leading_zero = part == "0" || !part.starts_with('0');
    if !digits_only || !no_leading_zero {
        return None;
    }
    part.parse().ok()
}

impl fmt::Display for PlainVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One line of the declaration file: a version and the channel it ships on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declaration {
    pub version: PlainVersion,
    pub channel: ReleaseChannel,
}

/// Read the declaration file's text. Blank lines and `#` comments are skipped.
///
/// Every other line must be `<version> <channel>`. The lines must name
/// versions in strictly increasing order, so a version appears once and
/// published versions only go up.
pub fn parse_declarations(text: &str) -> Result<Vec<Declaration>, String> {
    let mut declarations: Vec<Declaration> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        let [version_text, channel_text] = fields.as_slice() else {
            return Err(format!("line {trimmed:?} must be '<version> <channel>'"));
        };
        let version = PlainVersion::parse(version_text).ok_or_else(|| {
            format!("{version_text:?} is not a plain MAJOR.MINOR.PATCH version")
        })?;
        let channel = ReleaseChannel::parse(channel_text).ok_or_else(|| {
            format!("{version} is declared as {channel_text:?}; the channel must be 'prerelease' or 'stable'")
        })?;
        if let Some(previous) = declarations.last() {
            if version <= previous.version {
                return Err(format!(
                    "{version} is listed after {}; each line must be a later version",
                    previous.version
                ));
            }
        }
        declarations.push(Declaration { version, channel });
    }
    Ok(declarations)
}

/// The channel declared for `version`, if the file declares it.
pub fn channel_for(declarations: &[Declaration], version: PlainVersion) -> Option<ReleaseChannel> {
    declarations
        .iter()
        .find(|declaration| declaration.version == version)
        .map(|declaration| declaration.channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> PlainVersion {
        PlainVersion::parse(text).unwrap()
    }

    #[test]
    fn a_plain_version_is_exactly_three_numbers() {
        assert_eq!(PlainVersion::parse("0.0.1"), Some(version("0.0.1")));
        assert_eq!(PlainVersion::parse("10.20.300").unwrap().minor, 20);
        for bad in ["", "1", "1.2", "1.2.3.4", "1.2.3-1", "1.2.3-rc.1", "1.2.3+1", "v1.2.3", "01.2.3", "1.2.x", "1..3", " 1.2.3", "1.2.3 "] {
            assert_eq!(PlainVersion::parse(bad), None, "{bad:?} must not be a plain version");
        }
    }

    #[test]
    fn plain_versions_order_numerically_not_lexically() {
        assert!(version("0.0.10") > version("0.0.9"));
        assert!(version("0.1.0") > version("0.0.99"));
        assert!(version("1.0.0") > version("0.99.99"));
    }

    #[test]
    fn the_declared_channel_is_the_one_the_line_names() {
        let declarations = parse_declarations("# published\n0.0.1 prerelease\n\n0.0.2 stable\n").unwrap();
        assert_eq!(channel_for(&declarations, version("0.0.1")), Some(ReleaseChannel::Prerelease));
        assert_eq!(channel_for(&declarations, version("0.0.2")), Some(ReleaseChannel::Stable));
        assert_eq!(channel_for(&declarations, version("0.0.3")), None);
    }

    #[test]
    fn a_plain_zero_zero_one_is_a_prerelease_without_any_hyphen() {
        let declarations = parse_declarations("0.0.1 prerelease\n").unwrap();
        assert_eq!(channel_for(&declarations, version("0.0.1")), Some(ReleaseChannel::Prerelease));
    }

    #[test]
    fn a_missing_channel_is_not_inferred_from_the_version() {
        let declarations = parse_declarations("0.0.1 prerelease\n").unwrap();
        assert_eq!(channel_for(&declarations, version("0.0.2")), None);
    }

    #[test]
    fn malformed_declarations_are_refused() {
        let refused = [
            "0.0.1\n",
            "0.0.1 prerelease extra\n",
            "0.0.1 beta\n",
            "0.0.1 Stable\n",
            "0.0.1-1 prerelease\n",
            "0.0.1 prerelease\n0.0.1 stable\n",
            "0.0.2 stable\n0.0.1 prerelease\n",
        ];
        for text in refused {
            assert!(parse_declarations(text).is_err(), "{text:?} must be refused");
        }
    }

    #[test]
    fn channel_spellings_round_trip() {
        for channel in [ReleaseChannel::Stable, ReleaseChannel::Prerelease] {
            assert_eq!(ReleaseChannel::parse(channel.as_str()), Some(channel));
        }
        assert_eq!(ReleaseChannel::parse("beta"), None);
    }

    #[test]
    fn the_repository_declaration_parses_and_declares_the_app_version() {
        let text = include_str!("../../.github/release-channels.txt");
        let declarations = parse_declarations(text).expect("the repository's declaration file must parse");
        let app_version = version(env!("CARGO_PKG_VERSION"));
        assert!(channel_for(&declarations, app_version).is_some(), "{app_version} has no declared channel");
    }
}
