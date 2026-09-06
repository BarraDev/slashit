//! Runtime feature flags.
//!
//! Flags exist so work that is not finished can ship dark rather than sitting
//! on a long-lived branch. They are read from
//! [`AppPaths::feature_flags_file`](super::paths::AppPaths::feature_flags_file)
//! at startup and are deliberately forgiving: a missing, unreadable or
//! malformed file yields defaults with a warning, because failing to start
//! over a flag file would be worse than having no flags at all.
//!
//! # One registry, four layers
//!
//! [`REGISTRY`] is the single list of every flag this build knows about, with
//! its stable id, its description, the area that owns it, and its default.
//! Adding a flag means adding one entry there (plus the field it reads), not
//! editing a handful of `match` arms scattered across the module — that
//! duplication is exactly how a flag ends up settable but unreadable.
//!
//! A value is then resolved through four layers, highest priority first:
//!
//! 1. a command-line override, because the operator typing the command has the
//!    most immediate intent;
//! 2. an environment override (`SLASHIT_FEATURE_<UPPER_SNAKE_ID>`), which is
//!    how a wrapper script or a test harness speaks;
//! 3. the persisted `features.toml`, which is what the user last toggled in the
//!    UI;
//! 4. the registry default.
//!
//! [`ResolvedFlags`] keeps the deciding layer alongside the value, because "the
//! flag is on" is not an actionable answer when a user is asking why. That is
//! also what [`slashit_ipc::FeatureFlagInfo`] carries over IPC for `slashit
//! features`.

use super::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Prefix for the environment variable that overrides a flag.
///
/// `SLASHIT_FEATURE_DAEMON_MODE=1` overrides `daemon_mode`. The prefix is
/// mandatory so that a flag id can never collide with an unrelated variable
/// already in the user's shell.
pub const ENV_PREFIX: &str = "SLASHIT_FEATURE_";

// --- Registry ---------------------------------------------------------------

/// One flag as this build knows it, independent of any value.
///
/// `read` and `write` are function pointers rather than a string-keyed map so
/// that the persisted representation stays a plain struct with named fields —
/// which is what keeps `features.toml` readable and its serde surface stable —
/// while the registry remains the only place that enumerates flags.
#[derive(Debug, Clone, Copy)]
pub struct FeatureDef {
    /// Stable id. Used in `features.toml`, in the environment variable name,
    /// on the command line and over IPC, so it must never be renamed.
    pub id: &'static str,

    /// One line a user can act on, shown in the settings UI and by
    /// `slashit features`.
    pub description: &'static str,

    /// Subsystem the flag belongs to, so a long list can be grouped.
    pub area: &'static str,

    /// Who to ask when the flag misbehaves.
    pub owner: &'static str,

    /// Value when no layer says otherwise. Every flag defaults to `false`: a
    /// feature behind a flag is off unless the user asked for it.
    pub default: bool,

    read: fn(&FeatureFlags) -> bool,
    write: fn(&mut FeatureFlags, bool),
}

/// Every flag this build recognises.
///
/// This is the list to edit when adding a flag.
pub static REGISTRY: &[FeatureDef] = &[
    FeatureDef {
        id: "daemon_mode",
        description: "Run the queue and IPC server without a window.",
        area: "daemon",
        owner: "core",
        default: false,
        read: |flags: &FeatureFlags| flags.daemon_mode,
        write: |flags: &mut FeatureFlags, value: bool| flags.daemon_mode = value,
    },
    FeatureDef {
        id: "remote_access",
        description: "Accept IPC connections from outside this machine.",
        area: "ipc-security",
        owner: "core",
        default: false,
        read: |flags: &FeatureFlags| flags.remote_access,
        write: |flags: &mut FeatureFlags, value: bool| flags.remote_access = value,
    },
    FeatureDef {
        id: "auto_update",
        description: "Check for and install application updates from GitHub Releases.",
        area: "updater",
        owner: "core",
        // Off until the first non-draft, non-prerelease release exists. The
        // configured endpoint is `releases/latest/download/latest.json`, and
        // GitHub's "latest" excludes both drafts and prereleases, so with this
        // on by default every user would see a failing check for a feature
        // that cannot yet succeed. Turn it on with the first real release.
        default: false,
        read: |flags: &FeatureFlags| flags.auto_update,
        write: |flags: &mut FeatureFlags, value: bool| flags.auto_update = value,
    },
];

/// The flags actually in force at startup.
///
/// Deliberately *not* the same thing as [`FeatureFlags::load`]. `load` returns
/// what is persisted, which is only one of the four layers; this applies the
/// environment on top, so that what the application *does* matches what
/// `slashit features` and the settings UI *report*. A flag overridden by the
/// environment that nonetheless behaved as its persisted value would be the
/// worst of both worlds.
///
/// The daemon layers its command-line overrides on top of this.
///
/// Note the asymmetry with persistence: this resolved set must never be written
/// back to `features.toml`, or a temporary environment override would be baked
/// into the user's configuration. `set_feature_flag` persists the file
/// separately and then re-resolves.
pub fn resolve_startup_flags(paths: &AppPaths) -> FeatureFlags {
    let persisted = FeatureFlags::load(paths);
    let mut flags = persisted.clone();
    FeatureResolver::new()
        .with_config(persisted)
        .with_process_env()
        .resolve_and_report()
        .apply_to(&mut flags);
    flags
}

/// Look up a flag definition by its stable id.
pub fn definition(id: &str) -> Option<&'static FeatureDef> {
    REGISTRY.iter().find(|def| def.id == id)
}

/// Every recognised id, in registry order. Used to make "unknown flag" errors
/// actionable instead of merely correct.
pub fn valid_ids() -> Vec<&'static str> {
    REGISTRY.iter().map(|def| def.id).collect()
}

/// The environment variable that overrides `id`.
pub fn env_var_for(id: &str) -> String {
    format!("{ENV_PREFIX}{}", id.to_ascii_uppercase())
}

/// Read a boolean the way a human writes one on a command line.
///
/// Returns `None` rather than `false` for anything else, so that a typo such as
/// `SLASHIT_FEATURE_DAEMON_MODE=ture` is reported instead of quietly meaning
/// "off" — the failure mode where a user swears they enabled something.
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

// --- Sources and errors -----------------------------------------------------

/// Which layer decided a flag's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagSource {
    Default,
    Config,
    Env,
    Cli,
}

impl FlagSource {
    /// The wire spelling. [`slashit_ipc::FeatureFlagInfo::source`] is a string
    /// and these four values are what it is documented to carry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Config => "config",
            Self::Env => "env",
            Self::Cli => "cli",
        }
    }
}

impl fmt::Display for FlagSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an override came from. Only the two layers a user can get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Cli,
    Env,
}

impl From<Origin> for FlagSource {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::Cli => Self::Cli,
            Origin::Env => Self::Env,
        }
    }
}

/// Name the exact thing the user has to fix, not just the layer.
fn origin_phrase(origin: Origin, id: &str) -> String {
    match origin {
        Origin::Cli => "the command line".to_string(),
        Origin::Env => format!("environment variable {}", env_var_for(id)),
    }
}

/// A malformed override.
///
/// On the command line these are returned as errors and abort the run: the
/// operator is present and can retype. From the environment the same conditions
/// are collected as warnings and the layer is skipped, because refusing to start
/// over a stray variable in someone's shell profile is worse than ignoring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureError {
    /// An override named a flag this build does not have.
    UnknownFlag { origin: Origin, name: String },

    /// The value was not one of the accepted boolean spellings.
    UnparsableValue {
        origin: Origin,
        name: String,
        value: String,
    },

    /// A command-line override was not written as `name=value`.
    MalformedOverride { argument: String },
}

impl fmt::Display for FeatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFlag { origin, name } => write!(
                f,
                "unknown feature flag '{name}' from {}; valid flags are: {}",
                origin_phrase(*origin, name),
                valid_ids().join(", ")
            ),
            Self::UnparsableValue {
                origin,
                name,
                value,
            } => write!(
                f,
                "feature flag '{name}' from {} has value '{value}', which is not a boolean; use one of 1, 0, true, false, yes, no, on, off",
                origin_phrase(*origin, name)
            ),
            Self::MalformedOverride { argument } => write!(
                f,
                "feature override '{argument}' must be written as name=value; valid flags are: {}",
                valid_ids().join(", ")
            ),
        }
    }
}

impl std::error::Error for FeatureError {}

// --- Persisted flags --------------------------------------------------------

/// Flags recognised by this build, as persisted in `features.toml`.
///
/// Unknown keys are preserved in `extra` so that flipping a flag from a newer
/// build and then downgrading does not silently discard it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureFlags {
    /// Run the queue and IPC server without a window. Not implemented yet;
    /// the flag exists so the daemon work can land incrementally.
    pub daemon_mode: bool,

    /// Accept IPC connections from outside this machine.
    ///
    /// There is no implementation behind this and there deliberately will not
    /// be one until the model in `docs/architecture/ipc-security.md` is built.
    /// A remote `CreateTask` is a remote shell.
    pub remote_access: bool,

    /// Check for and install application updates.
    ///
    /// Off until a non-draft, non-prerelease GitHub release exists — see the
    /// registry entry. The updater plugin, its endpoint and its signing key are
    /// all configured; only the release is missing.
    pub auto_update: bool,

    /// Unrecognised keys, kept so a downgrade does not drop them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// Built from the registry rather than from `bool::default`, so that a flag
/// that ever ships defaulting to `true` cannot disagree with its registry
/// entry. The container-level `#[serde(default)]` routes missing keys here too.
impl Default for FeatureFlags {
    fn default() -> Self {
        // These seed values are placeholders; the registry loop below is what
        // actually decides each flag. Listing the fields explicitly rather than
        // using `..Default::default()` keeps this honest: adding a field to the
        // struct without adding its registry entry fails to compile here.
        let mut flags = Self {
            daemon_mode: false,
            remote_access: false,
            auto_update: false,
            extra: BTreeMap::new(),
        };
        for def in REGISTRY {
            (def.write)(&mut flags, def.default);
        }
        flags
    }
}

impl FeatureFlags {
    /// Load flags, falling back to defaults on any problem.
    pub fn load(paths: &AppPaths) -> Self {
        Self::load_from(&paths.feature_flags_file())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str(&contents) {
            Ok(flags) => flags,
            Err(e) => {
                eprintln!(
                    "[features] ignoring {}: {e}. Using defaults.",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, paths: &AppPaths) -> std::io::Result<()> {
        let path = paths.feature_flags_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    /// Look a flag up by name, for the frontend toggle.
    pub fn get(&self, name: &str) -> Option<bool> {
        definition(name).map(|def| (def.read)(self))
    }

    /// Set a flag by name. Returns false when the name is not recognised, so
    /// a typo is reported rather than silently stored in `extra`.
    pub fn set(&mut self, name: &str, enabled: bool) -> bool {
        match definition(name) {
            Some(def) => {
                (def.write)(self, enabled);
                true
            }
            None => false,
        }
    }

    /// Resolve these persisted flags against the process environment and
    /// describe the result for diagnostics.
    ///
    /// This is the one call the Tauri command and the IPC handler share, so
    /// `slashit features` and the settings UI can never disagree about which
    /// layer won.
    pub fn diagnostics(&self) -> Vec<slashit_ipc::FeatureFlagInfo> {
        FeatureResolver::new()
            .with_config(self.clone())
            .with_process_env()
            .resolve_and_report()
            .to_flag_info()
    }
}

// --- Resolution -------------------------------------------------------------

/// Collects the override layers, then resolves them in priority order.
///
/// The environment is injected rather than read implicitly so that tests can
/// exercise every parsing rule without mutating the process environment, which
/// is a data race in a multi-threaded test binary.
#[derive(Debug, Clone, Default)]
pub struct FeatureResolver {
    config: Option<FeatureFlags>,
    /// Already stripped of [`ENV_PREFIX`] and lowercased to a candidate id.
    env: Vec<(String, String)>,
    cli: Vec<(String, String)>,
}

impl FeatureResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supply the persisted layer, normally the result of [`FeatureFlags::load`].
    pub fn with_config(mut self, flags: FeatureFlags) -> Self {
        self.config = Some(flags);
        self
    }

    /// Supply an environment. Entries without [`ENV_PREFIX`] are ignored, so a
    /// whole environment can be handed over.
    pub fn with_env<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (key, value) in vars {
            let key = key.as_ref();
            // `get` rather than slicing: a variable name may begin with a
            // multi-byte character, and indexing off a char boundary panics.
            let Some(head) = key.get(..ENV_PREFIX.len()) else {
                continue;
            };
            if !head.eq_ignore_ascii_case(ENV_PREFIX) {
                continue;
            }
            let id = key[ENV_PREFIX.len()..].to_ascii_lowercase();
            if id.is_empty() {
                continue;
            }
            self.env.push((id, value.as_ref().to_string()));
        }
        self
    }

    /// Read the real environment.
    ///
    /// Uses `vars_os` because `std::env::vars` panics on a variable that is not
    /// valid UTF-8, and an unrelated variable elsewhere in the environment must
    /// not be able to take the application down.
    pub fn with_process_env(self) -> Self {
        let vars: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect();
        self.with_env(vars)
    }

    /// Supply command-line overrides that have already been parsed.
    pub fn with_cli_overrides<I, K>(mut self, overrides: I) -> Self
    where
        I: IntoIterator<Item = (K, bool)>,
        K: AsRef<str>,
    {
        for (name, value) in overrides {
            self.cli
                .push((name.as_ref().to_ascii_lowercase(), value.to_string()));
        }
        self
    }

    /// Parse `name=value` arguments, as passed by `--feature`.
    ///
    /// Unlike the environment layer this rejects rather than warns: the person
    /// who typed the argument is standing right there, and silently ignoring
    /// what they asked for would be worse than stopping.
    pub fn with_cli_args<I, S>(mut self, args: I) -> Result<Self, FeatureError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for arg in args {
            let arg = arg.as_ref();
            let Some((name, value)) = arg.split_once('=') else {
                return Err(FeatureError::MalformedOverride {
                    argument: arg.to_string(),
                });
            };
            let name = name.trim().to_ascii_lowercase();
            if definition(&name).is_none() {
                return Err(FeatureError::UnknownFlag {
                    origin: Origin::Cli,
                    name,
                });
            }
            if parse_bool(value).is_none() {
                return Err(FeatureError::UnparsableValue {
                    origin: Origin::Cli,
                    name,
                    value: value.to_string(),
                });
            }
            self.cli.push((name, value.to_string()));
        }
        Ok(self)
    }

    /// Resolve every flag. Pure: problems are collected, not printed.
    pub fn resolve(&self) -> ResolvedFlags {
        let mut flags: Vec<ResolvedFlag> = REGISTRY
            .iter()
            .map(|def| ResolvedFlag {
                def,
                value: def.default,
                source: FlagSource::Default,
            })
            .collect();
        let mut warnings = Vec::new();

        // The persisted struct cannot distinguish "absent" from "present and
        // equal to the default", so the config layer only claims a flag it
        // actually changes. The value is identical either way; only the
        // reported source would have been a guess.
        if let Some(config) = &self.config {
            for flag in &mut flags {
                let stored = (flag.def.read)(config);
                if stored != flag.def.default {
                    flag.value = stored;
                    flag.source = FlagSource::Config;
                }
            }
        }

        for (id, raw) in &self.env {
            apply_override(&mut flags, &mut warnings, Origin::Env, id, raw);
        }
        for (id, raw) in &self.cli {
            apply_override(&mut flags, &mut warnings, Origin::Cli, id, raw);
        }

        ResolvedFlags { flags, warnings }
    }

    /// Resolve, and put every problem on stderr.
    ///
    /// This is the variant production code calls; [`Self::resolve`] stays quiet
    /// so tests can assert on the warnings instead of scraping output.
    pub fn resolve_and_report(&self) -> ResolvedFlags {
        let resolved = self.resolve();
        resolved.report_warnings();
        resolved
    }
}

fn apply_override(
    flags: &mut [ResolvedFlag],
    warnings: &mut Vec<FeatureError>,
    origin: Origin,
    id: &str,
    raw: &str,
) {
    let Some(slot) = flags.iter_mut().find(|flag| flag.def.id == id) else {
        warnings.push(FeatureError::UnknownFlag {
            origin,
            name: id.to_string(),
        });
        return;
    };
    match parse_bool(raw) {
        Some(value) => {
            slot.value = value;
            slot.source = origin.into();
        }
        None => warnings.push(FeatureError::UnparsableValue {
            origin,
            name: id.to_string(),
            value: raw.to_string(),
        }),
    }
}

/// One flag after resolution, with the layer that decided it.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedFlag {
    pub def: &'static FeatureDef,
    pub value: bool,
    pub source: FlagSource,
}

/// Every flag as this process resolved it.
#[derive(Debug, Clone)]
pub struct ResolvedFlags {
    flags: Vec<ResolvedFlag>,
    warnings: Vec<FeatureError>,
}

impl ResolvedFlags {
    pub fn get(&self, id: &str) -> Option<bool> {
        self.find(id).map(|flag| flag.value)
    }

    /// Which layer decided `id`. The question a user actually asks when a flag
    /// is not what they expected.
    pub fn source(&self, id: &str) -> Option<FlagSource> {
        self.find(id).map(|flag| flag.source)
    }

    pub fn find(&self, id: &str) -> Option<&ResolvedFlag> {
        self.flags.iter().find(|flag| flag.def.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResolvedFlag> {
        self.flags.iter()
    }

    /// Overrides that were named but could not be honoured.
    pub fn warnings(&self) -> &[FeatureError] {
        &self.warnings
    }

    pub fn report_warnings(&self) {
        for warning in &self.warnings {
            eprintln!("[features] {warning}");
        }
    }

    /// Write the resolved values onto a persisted set, leaving `extra`
    /// untouched so a downgrade still cannot lose a newer build's key.
    ///
    /// Note that saving the result would persist whatever the environment or
    /// command line said this run, which is rarely what the user meant by
    /// setting a variable; keep the resolved copy in memory instead.
    pub fn apply_to(&self, flags: &mut FeatureFlags) {
        for flag in &self.flags {
            (flag.def.write)(flags, flag.value);
        }
    }

    /// The diagnostic view shipped over IPC and to the settings UI.
    pub fn to_flag_info(&self) -> Vec<slashit_ipc::FeatureFlagInfo> {
        self.flags
            .iter()
            .map(|flag| slashit_ipc::FeatureFlagInfo {
                name: flag.def.id.to_string(),
                value: flag.value,
                default: flag.def.default,
                source: flag.source.as_str().to_string(),
                description: flag.def.description.to_string(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn config_with(name: &str, value: bool) -> FeatureFlags {
        let mut flags = FeatureFlags::default();
        assert!(flags.set(name, value));
        flags
    }

    #[test]
    fn defaults_are_all_off() {
        let flags = FeatureFlags::default();
        assert!(!flags.daemon_mode);
        assert!(!flags.remote_access);
        // And the struct default must agree with the registry, since serde
        // fills missing keys from the former and resolution from the latter.
        for def in REGISTRY {
            assert!(!def.default, "{} must ship off", def.id);
            assert_eq!(flags.get(def.id), Some(def.default));
        }
    }

    #[test]
    fn the_registry_is_the_only_list_of_flags() {
        // Every id resolves to a definition and back through get/set, so a new
        // registry entry cannot be half-wired.
        let mut flags = FeatureFlags::default();
        for def in REGISTRY {
            assert!(definition(def.id).is_some());
            assert!(flags.set(def.id, true), "{} must be settable", def.id);
            assert_eq!(flags.get(def.id), Some(true));
            assert!(!def.area.is_empty() && !def.owner.is_empty());
        }
        assert!(valid_ids().contains(&"daemon_mode"));
        assert!(valid_ids().contains(&"remote_access"));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let tmp = TempDir::new().unwrap();
        let flags = FeatureFlags::load_from(&tmp.path().join("absent.toml"));
        assert_eq!(flags, FeatureFlags::default());
    }

    #[test]
    fn malformed_file_yields_defaults_instead_of_failing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("features.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();
        assert_eq!(FeatureFlags::load_from(&path), FeatureFlags::default());
    }

    #[test]
    fn roundtrips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        );

        let mut flags = FeatureFlags::default();
        assert!(flags.set("daemon_mode", true));
        flags.save(&paths).unwrap();

        assert_eq!(FeatureFlags::load(&paths), flags);
    }

    #[test]
    fn unknown_flag_names_are_rejected() {
        let mut flags = FeatureFlags::default();
        assert!(!flags.set("nonexistent", true));
        assert_eq!(flags.get("nonexistent"), None);
    }

    #[test]
    fn unknown_keys_survive_a_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("features.toml");
        std::fs::write(&path, "daemon_mode = true\nfrom_a_newer_build = true\n").unwrap();

        let flags = FeatureFlags::load_from(&path);
        assert!(flags.daemon_mode);
        assert!(flags.extra.contains_key("from_a_newer_build"));
    }

    #[test]
    fn unknown_table_and_scalar_values_survive_a_save_and_reload_roundtrip() {
        // `unknown_keys_survive_a_roundtrip` above only exercises `load_from`
        // against a hand-written file. This drives the actual `save` path
        // too — the one a real downgrade-then-upgrade cycle goes through —
        // with both an unknown table value and an unknown scalar value, since
        // TOML's own syntax rules (scalars before table headers at the same
        // level) make table-valued entries the riskier case to get wrong.
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        );

        let mut flags = FeatureFlags::default();
        let mut table = toml::map::Map::new();
        table.insert(
            "nested".to_string(),
            toml::Value::String("value".to_string()),
        );
        table.insert("count".to_string(), toml::Value::Integer(3));
        flags
            .extra
            .insert("future_table_flag".to_string(), toml::Value::Table(table));
        flags
            .extra
            .insert("future_scalar_flag".to_string(), toml::Value::Boolean(true));

        flags.save(&paths).unwrap();
        let reloaded = FeatureFlags::load(&paths);

        assert_eq!(
            reloaded.extra.get("future_table_flag"),
            flags.extra.get("future_table_flag"),
            "an unknown table-valued key must survive a save/reload cycle unchanged"
        );
        assert_eq!(
            reloaded.extra.get("future_scalar_flag"),
            flags.extra.get("future_scalar_flag"),
            "an unknown scalar-valued key must survive a save/reload cycle unchanged"
        );
        assert_eq!(
            reloaded, flags,
            "a full save/reload roundtrip must not lose or alter any unknown key"
        );
    }

    #[test]
    fn files_written_by_the_previous_format_still_load() {
        // The exact shape `toml::to_string_pretty` produced before the registry
        // existed. Renaming a key here would orphan every user's file.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("features.toml");
        std::fs::write(&path, "daemon_mode = true\nremote_access = false\n").unwrap();

        let flags = FeatureFlags::load_from(&path);
        assert!(flags.daemon_mode);
        assert!(!flags.remote_access);
        assert!(flags.extra.is_empty());
    }

    // --- Precedence ---------------------------------------------------------

    #[test]
    fn the_default_decides_when_nothing_overrides() {
        let resolved = FeatureResolver::new().resolve();
        assert_eq!(resolved.get("daemon_mode"), Some(false));
        assert_eq!(resolved.source("daemon_mode"), Some(FlagSource::Default));
        assert!(resolved.warnings().is_empty());
    }

    #[test]
    fn config_beats_the_default() {
        let resolved = FeatureResolver::new()
            .with_config(config_with("daemon_mode", true))
            .resolve();
        assert_eq!(resolved.get("daemon_mode"), Some(true));
        assert_eq!(resolved.source("daemon_mode"), Some(FlagSource::Config));
        // An untouched flag stays on its default.
        assert_eq!(resolved.source("remote_access"), Some(FlagSource::Default));
    }

    #[test]
    fn env_beats_config() {
        let resolved = FeatureResolver::new()
            .with_config(config_with("daemon_mode", true))
            .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", "off")]))
            .resolve();
        assert_eq!(resolved.get("daemon_mode"), Some(false));
        assert_eq!(resolved.source("daemon_mode"), Some(FlagSource::Env));
    }

    #[test]
    fn cli_beats_env() {
        let resolved = FeatureResolver::new()
            .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", "true")]))
            .with_cli_overrides([("daemon_mode", false)])
            .resolve();
        assert_eq!(resolved.get("daemon_mode"), Some(false));
        assert_eq!(resolved.source("daemon_mode"), Some(FlagSource::Cli));
    }

    #[test]
    fn cli_beats_env_beats_config_beats_default() {
        // All four layers disagree at once; the highest must win and must say
        // so.
        let all = FeatureResolver::new()
            .with_config(config_with("daemon_mode", true))
            .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", "false")]))
            .with_cli_args(["daemon_mode=on"])
            .unwrap()
            .resolve();
        assert_eq!(all.get("daemon_mode"), Some(true));
        assert_eq!(all.source("daemon_mode"), Some(FlagSource::Cli));

        // Drop the top layer and the next one takes over, and so on down.
        let without_cli = FeatureResolver::new()
            .with_config(config_with("daemon_mode", true))
            .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", "false")]))
            .resolve();
        assert_eq!(without_cli.get("daemon_mode"), Some(false));
        assert_eq!(without_cli.source("daemon_mode"), Some(FlagSource::Env));

        let without_env = FeatureResolver::new()
            .with_config(config_with("daemon_mode", true))
            .resolve();
        assert_eq!(without_env.get("daemon_mode"), Some(true));
        assert_eq!(without_env.source("daemon_mode"), Some(FlagSource::Config));

        let bare = FeatureResolver::new().resolve();
        assert_eq!(bare.get("daemon_mode"), Some(false));
        assert_eq!(bare.source("daemon_mode"), Some(FlagSource::Default));
    }

    #[test]
    fn one_override_does_not_disturb_the_other_flags() {
        let resolved = FeatureResolver::new()
            .with_env(env(&[("SLASHIT_FEATURE_REMOTE_ACCESS", "1")]))
            .resolve();
        assert_eq!(resolved.get("remote_access"), Some(true));
        assert_eq!(resolved.get("daemon_mode"), Some(false));
        assert_eq!(resolved.source("daemon_mode"), Some(FlagSource::Default));
    }

    // --- Environment parsing ------------------------------------------------

    #[test]
    fn every_documented_spelling_parses() {
        for truthy in ["1", "true", "TRUE", "True", "yes", "YES", "on", "ON", " on "] {
            assert_eq!(parse_bool(truthy), Some(true), "{truthy}");
            let resolved = FeatureResolver::new()
                .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", truthy)]))
                .resolve();
            assert_eq!(resolved.get("daemon_mode"), Some(true), "{truthy}");
            assert!(resolved.warnings().is_empty(), "{truthy}");
        }
        for falsy in ["0", "false", "FALSE", "False", "no", "NO", "off", "OFF", " off "] {
            assert_eq!(parse_bool(falsy), Some(false), "{falsy}");
            let resolved = FeatureResolver::new()
                .with_config(config_with("daemon_mode", true))
                .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", falsy)]))
                .resolve();
            assert_eq!(resolved.get("daemon_mode"), Some(false), "{falsy}");
            assert!(resolved.warnings().is_empty(), "{falsy}");
        }
    }

    #[test]
    fn a_garbage_env_value_is_reported_and_ignored_rather_than_read_as_off() {
        for garbage in ["ture", "", "2", "enabled", "sure"] {
            assert_eq!(parse_bool(garbage), None, "{garbage}");
        }

        let resolved = FeatureResolver::new()
            .with_config(config_with("daemon_mode", true))
            .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", "ture")]))
            .resolve();

        // The lower layer still decides: the typo must not read as "off".
        assert_eq!(resolved.get("daemon_mode"), Some(true));
        assert_eq!(resolved.source("daemon_mode"), Some(FlagSource::Config));
        assert_eq!(resolved.warnings().len(), 1);
        let message = resolved.warnings()[0].to_string();
        assert!(message.contains("daemon_mode"), "{message}");
        assert!(message.contains("ture"), "{message}");
        assert!(message.contains("SLASHIT_FEATURE_DAEMON_MODE"), "{message}");
    }

    #[test]
    fn unprefixed_variables_are_ignored() {
        let resolved = FeatureResolver::new()
            .with_env(env(&[
                ("DAEMON_MODE", "1"),
                ("PATH", "/usr/bin"),
                ("SLASHIT_FEATURE_", "1"),
            ]))
            .resolve();
        assert_eq!(resolved.get("daemon_mode"), Some(false));
        assert!(resolved.warnings().is_empty());
    }

    #[test]
    fn the_env_variable_name_is_the_upper_snake_id() {
        assert_eq!(env_var_for("daemon_mode"), "SLASHIT_FEATURE_DAEMON_MODE");
        assert_eq!(
            env_var_for("remote_access"),
            "SLASHIT_FEATURE_REMOTE_ACCESS"
        );
    }

    // --- Validation ---------------------------------------------------------

    #[test]
    fn an_unknown_env_flag_is_reported_with_the_valid_ids() {
        let resolved = FeatureResolver::new()
            .with_env(env(&[("SLASHIT_FEATURE_TIME_TRAVEL", "1")]))
            .resolve();

        assert_eq!(resolved.warnings().len(), 1);
        let message = resolved.warnings()[0].to_string();
        assert!(message.contains("time_travel"), "{message}");
        assert!(message.contains("daemon_mode"), "{message}");
        assert!(message.contains("remote_access"), "{message}");
        // The known flags are unaffected.
        assert_eq!(resolved.get("daemon_mode"), Some(false));
    }

    #[test]
    fn an_unknown_cli_flag_is_an_error() {
        let err = FeatureResolver::new()
            .with_cli_args(["time_travel=1"])
            .unwrap_err();
        assert_eq!(
            err,
            FeatureError::UnknownFlag {
                origin: Origin::Cli,
                name: "time_travel".to_string()
            }
        );
        let message = err.to_string();
        assert!(message.contains("time_travel"), "{message}");
        assert!(message.contains("daemon_mode"), "{message}");

        // And an unknown id that sneaks past the argument parser is still
        // reported rather than silently accepted.
        let resolved = FeatureResolver::new()
            .with_cli_overrides([("time_travel", true)])
            .resolve();
        assert_eq!(resolved.warnings().len(), 1);
        assert!(resolved.warnings()[0].to_string().contains("time_travel"));
    }

    #[test]
    fn a_bad_cli_argument_is_an_error() {
        assert_eq!(
            FeatureResolver::new()
                .with_cli_args(["daemon_mode"])
                .unwrap_err(),
            FeatureError::MalformedOverride {
                argument: "daemon_mode".to_string()
            }
        );
        assert_eq!(
            FeatureResolver::new()
                .with_cli_args(["daemon_mode=ture"])
                .unwrap_err(),
            FeatureError::UnparsableValue {
                origin: Origin::Cli,
                name: "daemon_mode".to_string(),
                value: "ture".to_string()
            }
        );
    }

    // --- Diagnostic view ----------------------------------------------------

    #[test]
    fn the_diagnostic_view_names_the_deciding_layer() {
        let resolved = FeatureResolver::new()
            .with_config(config_with("remote_access", true))
            .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", "yes")]))
            .resolve();

        let info = resolved.to_flag_info();
        assert_eq!(info.len(), REGISTRY.len());

        let daemon = info.iter().find(|i| i.name == "daemon_mode").unwrap();
        assert!(daemon.value);
        assert!(!daemon.default);
        assert_eq!(daemon.source, "env");
        assert!(!daemon.description.is_empty());

        let remote = info.iter().find(|i| i.name == "remote_access").unwrap();
        assert!(remote.value);
        assert_eq!(remote.source, "config");

        // And every layer spells itself the way the protocol documents.
        let cli = FeatureResolver::new()
            .with_cli_args(["daemon_mode=1"])
            .unwrap()
            .resolve()
            .to_flag_info();
        assert_eq!(
            cli.iter().find(|i| i.name == "daemon_mode").unwrap().source,
            "cli"
        );
        let bare = FeatureResolver::new().resolve().to_flag_info();
        assert_eq!(
            bare.iter().find(|i| i.name == "daemon_mode").unwrap().source,
            "default"
        );
        assert_eq!(FlagSource::Config.as_str(), "config");
    }

    #[test]
    fn applying_a_resolution_keeps_unknown_keys() {
        let mut flags = FeatureFlags::default();
        flags
            .extra
            .insert("from_a_newer_build".to_string(), toml::Value::Boolean(true));

        FeatureResolver::new()
            .with_env(env(&[("SLASHIT_FEATURE_DAEMON_MODE", "1")]))
            .resolve()
            .apply_to(&mut flags);

        assert!(flags.daemon_mode);
        assert!(flags.extra.contains_key("from_a_newer_build"));
    }

    #[test]
    fn the_process_environment_is_read_when_asked() {
        // The only test in this module that touches the real environment;
        // everything else injects a map, because `set_var` races with any
        // concurrent reader. `cargo test` runs this binary's tests as
        // parallel threads of one process, so this acquires the same lock
        // `crate::config::paths`'s own environment-mutating tests use — a
        // lock scoped to just this variable would not protect against that
        // other module's unlocked-from-here mutations of a *different*
        // variable, since concurrent `setenv`/`getenv` are undefined behavior
        // in the platform C library regardless of which variable each side
        // touches.
        let _guard = crate::config::paths::ENV_LOCK.blocking_lock();

        let key = env_var_for("daemon_mode");
        let previous = std::env::var(&key).ok();
        // SAFETY: the guard above makes this the only thread touching the
        // environment; restored below before it drops.
        unsafe { std::env::set_var(&key, "yes") };

        let resolved = FeatureResolver::new().with_process_env().resolve();

        match previous {
            // SAFETY: as above.
            Some(v) => unsafe { std::env::set_var(&key, v) },
            None => unsafe { std::env::remove_var(&key) },
        }

        assert_eq!(resolved.get("daemon_mode"), Some(true));
        assert_eq!(resolved.source("daemon_mode"), Some(FlagSource::Env));
    }
}
