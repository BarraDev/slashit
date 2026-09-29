//! Measure how much disk a directory tree occupies, without following
//! anything out of it.
//!
//! The walk never follows a symbolic link, and on Windows never enters any
//! reparse point, which is what junctions and volume mount points are. On
//! Unix it does not descend into a directory on another device, so a
//! filesystem mounted inside a checkout is not charged to it. A file that
//! disappears mid-walk is simply not counted; any other entry that cannot be
//! read is counted as skipped, and the result says so rather than reading as
//! a smaller, complete number.
//!
//! Sizes are the space allocated on disk where the platform reports it
//! (Unix: `st_blocks`), and the file's length otherwise (Windows). On Unix a
//! file with several hard links inside one walk is counted once, which is
//! what Cargo's `target/` needs: it hard-links every final artifact from
//! `deps/`.

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

/// Bounds on one walk. A walk that reaches one stops and reports itself
/// partial.
#[derive(Debug, Clone, Copy)]
pub struct WalkLimits {
    pub max_entries: u64,
    pub max_depth: usize,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            // A large Rust workspace's `target/` is a few hundred thousand
            // entries.
            max_entries: 5_000_000,
            max_depth: 256,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeSize {
    pub bytes: u64,
    pub entries: u64,
    /// Entries that exist but could not be read, and directories the walk
    /// did not enter because of a limit.
    pub skipped: u64,
    /// Why the first skipped entry was skipped.
    pub first_skip_reason: Option<String>,
    /// Symbolic links and reparse points, counted at their own size and not
    /// followed.
    pub links_not_followed: u64,
    /// Directories on another filesystem, not entered.
    pub mounts_not_entered: u64,
    /// The root itself is a link, so nothing it points to was measured.
    pub root_is_link: bool,
    pub hit_limit: bool,
}

impl TreeSize {
    pub fn is_complete(&self) -> bool {
        self.skipped == 0 && !self.hit_limit
    }

    fn skip(&mut self, reason: String) {
        self.skipped = self.skipped.saturating_add(1);
        self.first_skip_reason.get_or_insert(reason);
    }

    fn add(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Unreadable(String),
}

/// Measure `root` and everything below it, leaving out the root's direct
/// children named in `exclude`.
///
/// `root` itself is not followed if it is a link: its own size is returned.
/// A missing `root` is [`WalkError::NotFound`], distinct from an empty one.
pub fn measure(root: &Path, exclude: &[&str], limits: WalkLimits) -> Result<TreeSize, WalkError> {
    walk(root, exclude, limits, &mut Hooks::default())
}

/// Called just before the walk lists a directory or reads an entry's
/// metadata, the two moments a path it has already seen may disappear under
/// it. Tests use them to pull a path away at exactly that moment.
#[derive(Default)]
struct Hooks<'a> {
    before_list: Option<&'a mut dyn FnMut(&Path)>,
    before_stat: Option<&'a mut dyn FnMut(&Path)>,
}

fn walk(root: &Path, exclude: &[&str], limits: WalkLimits, hooks: &mut Hooks<'_>) -> Result<TreeSize, WalkError> {
    let root_meta = fs::symlink_metadata(root).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => WalkError::NotFound,
        _ => WalkError::Unreadable(e.to_string()),
    })?;

    let mut size = TreeSize::default();
    let mut seen = HardLinks::default();
    size.entries = 1;
    size.add(allocated(&root_meta));
    if is_link(&root_meta) {
        size.links_not_followed = 1;
        size.root_is_link = true;
        return Ok(size);
    }
    if !root_meta.is_dir() {
        return Ok(size);
    }

    let root_device = device(&root_meta);
    let mut pending: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        if let Some(hook) = hooks.before_list.as_mut() {
            hook(&dir);
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // Gone since it was listed: nothing left to count. The root
            // gone since it was stat'ed is as absent as one never there.
            Err(e) if e.kind() == io::ErrorKind::NotFound && depth > 0 => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(WalkError::NotFound),
            Err(e) if depth == 0 => return Err(WalkError::Unreadable(e.to_string())),
            Err(e) => {
                size.skip(format!("{}: {e}", relative(root, &dir)));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    size.skip(format!("{}: {e}", relative(root, &dir)));
                    continue;
                }
            };
            if depth == 0 && exclude.iter().any(|name| entry.file_name() == *name) {
                continue;
            }
            if size.entries >= limits.max_entries {
                size.hit_limit = true;
                return Ok(size);
            }
            // Never follows a link: on every platform `DirEntry::metadata`
            // describes the entry itself.
            if let Some(hook) = hooks.before_stat.as_mut() {
                hook(&entry.path());
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    size.skip(format!("{}: {e}", relative(root, &entry.path())));
                    continue;
                }
            };
            size.entries += 1;
            if !seen.first_sighting(&meta) {
                continue;
            }
            size.add(allocated(&meta));
            if is_link(&meta) {
                size.links_not_followed += 1;
                continue;
            }
            if !meta.is_dir() {
                continue;
            }
            if device(&meta) != root_device {
                size.mounts_not_entered += 1;
                continue;
            }
            if depth + 1 >= limits.max_depth {
                size.hit_limit = true;
                size.skip(format!("{}: deeper than {} levels", relative(root, &entry.path()), limits.max_depth));
                continue;
            }
            pending.push((entry.path(), depth + 1));
        }
    }
    Ok(size)
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel.display().to_string()
    }
}

#[cfg(unix)]
fn allocated(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // POSIX leaves the unit unspecified, but Linux and macOS both count
    // `st_blocks` in 512-byte units whatever the filesystem's block size.
    meta.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated(meta: &Metadata) -> u64 {
    if meta.is_dir() {
        0
    } else {
        meta.len()
    }
}

#[cfg(unix)]
fn device(meta: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.dev())
}

/// Not available on stable Rust off Unix. On Windows a volume mounted inside
/// the tree is a reparse point, which [`is_link`] already refuses to enter.
#[cfg(not(unix))]
fn device(_meta: &Metadata) -> Option<u64> {
    None
}

#[cfg(windows)]
pub(super) fn is_link(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    // Every reparse point, not only the name surrogates `is_symlink` covers:
    // a junction, a mounted volume, and a cloud placeholder that would be
    // downloaded on access all qualify.
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(super) fn is_link(meta: &Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// Files already counted in this walk, by identity.
#[derive(Default)]
struct HardLinks {
    #[cfg(unix)]
    seen: HashSet<(u64, u64)>,
    #[cfg(not(unix))]
    _seen: HashSet<()>,
}

impl HardLinks {
    /// False when `meta` is another name for a file this walk already
    /// counted.
    #[cfg(unix)]
    fn first_sighting(&mut self, meta: &Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        if meta.is_dir() || meta.nlink() < 2 {
            return true;
        }
        self.seen.insert((meta.dev(), meta.ino()))
    }

    /// Link counts are not available on stable Rust off Unix, so each name
    /// is counted.
    #[cfg(not(unix))]
    fn first_sighting(&mut self, _meta: &Metadata) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; len]).unwrap();
    }

    #[test]
    fn a_missing_root_is_not_an_empty_one() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            measure(&tmp.path().join("absent"), &[], WalkLimits::default()),
            Err(WalkError::NotFound)
        ));
        let empty = measure(tmp.path(), &[], WalkLimits::default()).unwrap();
        assert!(empty.is_complete());
        assert_eq!(empty.entries, 1);
    }

    #[test]
    fn sizes_add_up_across_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a"), 10_000);
        write(&tmp.path().join("d/b"), 20_000);
        write(&tmp.path().join("d/e/c"), 30_000);
        let size = measure(tmp.path(), &[], WalkLimits::default()).unwrap();
        assert!(size.is_complete());
        assert_eq!(size.entries, 6);
        // Allocation rounds up to blocks, so the total is at least the
        // content and not wildly more.
        assert!(size.bytes >= 60_000, "{size:?}");
        assert!(size.bytes < 60_000 + 64 * 1024, "{size:?}");
    }

    #[test]
    fn excluded_top_level_entries_are_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/main.rs"), 4096);
        write(&tmp.path().join("target/big"), 1_000_000);
        write(&tmp.path().join("src/target/nested"), 1_000_000);
        let size = measure(tmp.path(), &["target"], WalkLimits::default()).unwrap();
        // Only the root's own `target` is excluded; a nested one is not.
        assert!(size.bytes >= 1_004_096 && size.bytes < 1_100_000, "{size:?}");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("huge"), 2_000_000);
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("small"), 100);
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("huge"), tmp.path().join("file-link")).unwrap();
        // A loop, which following would never finish.
        std::os::unix::fs::symlink(tmp.path(), tmp.path().join("loop")).unwrap();

        let size = measure(tmp.path(), &[], WalkLimits::default()).unwrap();
        assert!(size.is_complete());
        assert_eq!(size.links_not_followed, 3);
        assert!(size.bytes < 100_000, "{size:?}");

        // A root that is itself a link is measured as the link.
        let as_root = measure(&tmp.path().join("escape"), &[], WalkLimits::default()).unwrap();
        assert_eq!(as_root.links_not_followed, 1);
        assert!(as_root.root_is_link);
        assert!(!size.root_is_link);
        assert!(as_root.bytes < 100_000, "{as_root:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("deps/lib"), 1_000_000);
        fs::hard_link(tmp.path().join("deps/lib"), tmp.path().join("lib")).unwrap();
        let size = measure(tmp.path(), &[], WalkLimits::default()).unwrap();
        assert!(size.bytes < 1_100_000, "{size:?}");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_makes_the_walk_partial_not_smaller() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("ok/file"), 10_000);
        write(&tmp.path().join("locked/file"), 10_000);
        fs::set_permissions(tmp.path().join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let result = measure(tmp.path(), &[], WalkLimits::default());
        fs::set_permissions(tmp.path().join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let size = result.unwrap();
        // Root can read anything, so there is nothing to observe there.
        if unsafe { libc::geteuid() } != 0 {
            assert!(!size.is_complete());
            assert_eq!(size.skipped, 1);
            assert!(size.first_skip_reason.unwrap().starts_with("locked"));
        }
        assert!(size.bytes >= 10_000);
    }

    #[test]
    fn limits_stop_the_walk_and_say_so() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..20 {
            write(&tmp.path().join(format!("f{i}")), 10);
        }
        let size = measure(
            tmp.path(),
            &[],
            WalkLimits {
                max_entries: 5,
                max_depth: 256,
            },
        )
        .unwrap();
        assert!(size.hit_limit);
        assert!(!size.is_complete());
        assert_eq!(size.entries, 5);

        write(&tmp.path().join("a/b/c/d"), 10);
        let shallow = measure(
            tmp.path(),
            &[],
            WalkLimits {
                max_entries: 1_000,
                max_depth: 2,
            },
        )
        .unwrap();
        assert!(!shallow.is_complete());
    }

    #[test]
    fn paths_that_disappear_mid_walk_are_neither_counted_nor_errors() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("stays"), 10_000);
        write(&tmp.path().join("file-goes"), 1_000_000);
        write(&tmp.path().join("dir-goes/inner"), 1_000_000);

        // Listed by its parent, removed before it is stat'ed.
        let mut before_stat = |path: &Path| {
            if path.ends_with("file-goes") {
                fs::remove_file(path).unwrap();
            }
        };
        // Stat'ed and queued, removed before it is listed.
        let mut before_list = |path: &Path| {
            if path.ends_with("dir-goes") {
                fs::remove_dir_all(path).unwrap();
            }
        };
        let mut hooks = Hooks {
            before_list: Some(&mut before_list),
            before_stat: Some(&mut before_stat),
        };
        let size = walk(tmp.path(), &[], WalkLimits::default(), &mut hooks).unwrap();

        assert!(size.is_complete(), "{size:?}");
        assert!(size.bytes >= 10_000 && size.bytes < 100_000, "{size:?}");
    }
}
