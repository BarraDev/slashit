//! Bearer tokens for transports the operating system does not access-control.
//!
//! A Unix socket in a 0700 directory and a named pipe with a restrictive
//! security descriptor are authenticated by the OS: reaching them already
//! proves you are the owning user. TCP proves nothing, so a loopback TCP
//! listener is only allowed to exist alongside a token.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::Path;
use subtle::ConstantTimeEq;

/// Bytes of entropy in a generated token.
///
/// 32 bytes is well past the point where guessing is the weakest link; the
/// token is at rest in a 0600 file, which is the actual exposure.
const TOKEN_BYTES: usize = 32;

/// A shared secret proving a TCP client is the same user as the server.
///
/// `Debug` and `Display` deliberately redact: tokens travel through error
/// paths and logs, and a token printed into a log file has outlived its file
/// permissions.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthToken(String);

impl AuthToken {
    /// Mint a fresh token from the OS random source.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(hex_encode(&bytes))
    }

    /// Adopt an existing token string.
    pub fn from_string(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The token as it appears on the wire.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Compare in constant time.
    ///
    /// A byte-by-byte comparison that returns early leaks the length of the
    /// matching prefix through timing, which over enough attempts recovers the
    /// token one byte at a time.
    pub fn matches(&self, candidate: &str) -> bool {
        let a = self.0.as_bytes();
        let b = candidate.as_bytes();
        // `ct_eq` requires equal lengths; the length itself is not secret.
        if a.len() != b.len() {
            return false;
        }
        a.ct_eq(b).into()
    }
}

impl fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

impl fmt::Display for AuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The credentials file: `credentials.toml` in the config directory.
///
/// Kept separate from `config.toml` so it can be locked to 0600 independently
/// and excluded from any future config export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipc_token: Option<AuthToken>,
}

impl Credentials {
    /// Read credentials, treating an absent file as empty.
    ///
    /// A malformed file is an error rather than a silent default: unlike a
    /// feature flag, silently forgetting a credential turns an authenticated
    /// listener into a broken one, and the operator should be told.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Write credentials with owner-only permissions.
    ///
    /// The temporary file is created inside the destination directory, at
    /// `0600` from the moment it is created, and tightened again *before* the
    /// rename in case it already existed — so the token never exists at its
    /// final path, or at the temporary path, with looser permissions.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let tmp = path.with_extension("toml.tmp");
        write_owner_only(&tmp, text.as_bytes())?;
        restrict_to_owner(&tmp)?;
        std::fs::rename(&tmp, path)
    }

    /// Return the stored token, minting and persisting one if absent.
    ///
    /// Idempotent: a second call returns the same token, so restarting the
    /// listener does not invalidate a client's saved credential.
    pub fn ensure_token(path: &Path) -> io::Result<AuthToken> {
        let mut creds = Self::load(path)?;
        if let Some(token) = &creds.ipc_token {
            return Ok(token.clone());
        }
        let token = AuthToken::generate();
        creds.ipc_token = Some(token.clone());
        creds.save(path)?;
        Ok(token)
    }
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Create (or truncate) `path` and write `contents`, `0600` from the instant
/// the file is created.
///
/// On Unix, setting the mode at `open()` time — rather than `write` then
/// `chmod` after — means the file never exists on disk at a looser,
/// umask-controlled mode, not even for the instant between the two calls.
#[cfg(unix)]
fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(not(unix))]
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    // Windows inherits the parent directory's ACL, and the config directory
    // already lives under the per-user profile. Clearing the read-only bit is
    // all that is meaningful here.
    let mut perms = std::fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms)
}

fn hex_encode(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            // Writing to a String is infallible.
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generated_tokens_are_unique_and_full_length() {
        let a = AuthToken::generate();
        let b = AuthToken::generate();
        assert_ne!(a.expose(), b.expose());
        assert_eq!(a.expose().len(), TOKEN_BYTES * 2);
        assert!(a.expose().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_token_matches_itself_and_nothing_else() {
        let token = AuthToken::generate();
        assert!(token.matches(token.expose()));
        assert!(!token.matches("wrong"));
        assert!(!token.matches(""));

        // A correct prefix must not match.
        let prefix = &token.expose()[..TOKEN_BYTES];
        assert!(!token.matches(prefix));
    }

    #[test]
    fn tokens_are_redacted_in_debug_and_display() {
        let token = AuthToken::from_string("s3cret-value");
        assert!(!format!("{token:?}").contains("s3cret"));
        assert!(!format!("{token}").contains("s3cret"));
    }

    #[test]
    fn credentials_are_redacted_in_debug() {
        let creds = Credentials {
            ipc_token: Some(AuthToken::from_string("s3cret-value")),
        };
        assert!(!format!("{creds:?}").contains("s3cret"));
    }

    #[test]
    fn an_absent_file_loads_as_empty() {
        let tmp = TempDir::new().unwrap();
        let creds = Credentials::load(&tmp.path().join("absent.toml")).unwrap();
        assert!(creds.ipc_token.is_none());
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_silent_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("credentials.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();
        assert!(Credentials::load(&path).is_err());
    }

    #[test]
    fn ensure_token_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("credentials.toml");

        let first = Credentials::ensure_token(&path).unwrap();
        let second = Credentials::ensure_token(&path).unwrap();
        assert_eq!(first.expose(), second.expose());
    }

    #[test]
    fn a_token_roundtrips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("credentials.toml");

        let token = Credentials::ensure_token(&path).unwrap();
        let loaded = Credentials::load(&path).unwrap();
        assert_eq!(loaded.ipc_token.unwrap().expose(), token.expose());
    }

    #[cfg(unix)]
    #[test]
    fn the_credentials_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("credentials.toml");

        Credentials::ensure_token(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "credentials must not be group- or world-readable, got {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
