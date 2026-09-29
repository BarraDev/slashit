//! Measure how much disk a directory tree occupies, without following
//! anything out of it.
//!
//! A walk starts at an *anchor*, one of SlashIt's own root directories,
//! taken to be wherever the system resolves it. Everything below the anchor
//! is reached without following a link:
//!
//! - On Unix every directory is opened relative to its parent's open
//!   handle with `O_NOFOLLOW | O_DIRECTORY`, every entry is stat'ed relative
//!   to that handle without following, and a directory's identity (device
//!   and inode) when opened must be the one it had when it was listed. A
//!   directory swapped for a link, or for another directory, between being
//!   listed and being opened is therefore never entered through that name:
//!   it is reported as skipped, "changed while it was being measured". The
//!   walk does not descend into a directory on another device.
//! - On Windows the walk goes by path, as the standard library does, and
//!   does not enter any entry it sees as a reparse point (junctions, mounted
//!   volumes, cloud placeholders, symbolic links). A directory replaced by a
//!   junction after it was seen is caught by checking it again once it has
//!   been listed, and its contents are then discarded as changed. A swap
//!   that is undone before that check is not caught; see
//!   `docs/architecture/state-locations.md`.
//!
//! A file that disappears mid-walk is simply not counted; any other entry
//! that cannot be read is counted as skipped, and the result says so rather
//! than reading as a smaller, complete number.
//!
//! Sizes are the space allocated on disk where the platform reports it
//! (Unix: `st_blocks`), and the file's length otherwise (Windows). On Unix a
//! file with several hard links inside one walk is counted once, which is
//! what Cargo's `target/` needs: it hard-links every final artifact from
//! `deps/`.
//!
//! None of this makes a measurement an authority to delete anything: it is
//! accounting. A later cleanup must establish what it removes itself.

use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, PathBuf};

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
    /// Entries that exist but could not be read, directories that changed
    /// while being measured, and directories the walk did not enter because
    /// of a limit.
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

const CHANGED: &str = "changed while it was being measured";

/// Measure `path` and everything below it, leaving out its direct children
/// named in `exclude`.
///
/// `anchor` is the SlashIt root `path` lies in, or `path` itself. Nothing
/// between the anchor and `path`, nor `path` itself, is followed if it is a
/// link: a link at `path` is measured as the link, and a link in between
/// makes the walk fail rather than measure somewhere else. A missing `path`
/// is [`WalkError::NotFound`], distinct from an empty one.
pub fn measure(anchor: &Path, path: &Path, exclude: &[&str], limits: WalkLimits) -> Result<TreeSize, WalkError> {
    walk(anchor, path, exclude, limits, &mut Hooks::default())
}

/// Called just before the walk opens a directory or reads an entry's
/// metadata, the two moments a path it has already seen may change under
/// it. Tests use them to change a path at exactly that moment.
#[derive(Default)]
struct Hooks<'a> {
    before_list: Option<&'a mut dyn FnMut(&Path)>,
    before_stat: Option<&'a mut dyn FnMut(&Path)>,
}

impl Hooks<'_> {
    fn list(&mut self, path: &Path) {
        if let Some(hook) = self.before_list.as_mut() {
            hook(path);
        }
    }

    fn stat(&mut self, path: &Path) {
        if let Some(hook) = self.before_stat.as_mut() {
            hook(path);
        }
    }
}

/// What the walk needs to know about one entry, whatever the platform.
struct Entry {
    allocated: u64,
    is_dir: bool,
    is_link: bool,
    /// Device, where the platform reports it.
    device: Option<u64>,
    /// Device and file number, where the platform reports them.
    identity: Option<(u64, u64)>,
    links: u64,
}

/// The components of `path` below `anchor`, each an ordinary name.
fn components_below<'p>(anchor: &Path, path: &'p Path) -> Result<Vec<&'p std::ffi::OsStr>, WalkError> {
    let rel = path
        .strip_prefix(anchor)
        .map_err(|_| WalkError::Unreadable(format!("{} is not below {}", path.display(), anchor.display())))?;
    rel.components()
        .map(|c| match c {
            Component::Normal(name) => Ok(name),
            _ => Err(WalkError::Unreadable(format!("{} is not a plain path below its root", path.display()))),
        })
        .collect()
}

/// Shared bookkeeping for one entry: count it once, and say whether the
/// walk should go into it.
fn count(size: &mut TreeSize, seen: &mut HardLinks, entry: &Entry, root_device: Option<u64>) -> bool {
    size.entries += 1;
    if !seen.first_sighting(entry) {
        return false;
    }
    size.add(entry.allocated);
    if entry.is_link {
        size.links_not_followed += 1;
        return false;
    }
    if !entry.is_dir {
        return false;
    }
    if entry.device != root_device {
        size.mounts_not_entered += 1;
        return false;
    }
    true
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel.display().to_string()
    }
}

/// Files already counted in this walk, by identity.
#[derive(Default)]
struct HardLinks {
    seen: HashSet<(u64, u64)>,
}

impl HardLinks {
    /// False when `entry` is another name for a file this walk already
    /// counted. Off Unix there is no identity to compare, so each name is
    /// counted.
    fn first_sighting(&mut self, entry: &Entry) -> bool {
        match entry.identity {
            Some(identity) if !entry.is_dir && entry.links > 1 => self.seen.insert(identity),
            _ => true,
        }
    }
}

#[cfg(unix)]
use unix::walk;

#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::fd::OwnedFd;
    use rustix::fs::{fstat, openat, statat, AtFlags, Dir, FileType, Mode, OFlags, Stat, CWD};
    use rustix::io::Errno;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::rc::Rc;

    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    /// Any of the integer types `stat` fields have across Unix platforms.
    fn wide<T: TryInto<u64>>(value: T) -> u64 {
        value.try_into().unwrap_or(0)
    }

    fn entry(st: &Stat) -> Entry {
        let kind = FileType::from_raw_mode(st.st_mode);
        Entry {
            // Linux and macOS both count `st_blocks` in 512-byte units,
            // whatever the filesystem's block size.
            allocated: wide(st.st_blocks).saturating_mul(512),
            is_dir: kind == FileType::Directory,
            is_link: kind == FileType::Symlink,
            device: Some(wide(st.st_dev)),
            identity: Some((wide(st.st_dev), wide(st.st_ino))),
            links: wide(st.st_nlink),
        }
    }

    fn error(e: Errno) -> io::Error {
        e.into()
    }

    /// A directory the walk has listed and will open later, through its
    /// parent's handle.
    struct Pending {
        parent: Rc<OwnedFd>,
        name: OsString,
        path: PathBuf,
        depth: usize,
        identity: Option<(u64, u64)>,
    }

    pub(super) fn walk(
        anchor: &Path,
        path: &Path,
        exclude: &[&str],
        limits: WalkLimits,
        hooks: &mut Hooks<'_>,
    ) -> Result<TreeSize, WalkError> {
        let components = components_below(anchor, path)?;
        let unreadable = |e: Errno| match e {
            Errno::NOENT => WalkError::NotFound,
            Errno::LOOP | Errno::NOTDIR => WalkError::Unreadable(CHANGED.to_string()),
            e => WalkError::Unreadable(error(e).to_string()),
        };

        // The directory holding `path`, and `path`'s own name in it. The
        // anchor's own path is resolved as the system resolves it; from
        // there on, nothing is followed.
        let (parent, name): (OwnedFd, &OsStr) = match components.split_last() {
            Some((last, between)) => {
                let mut dir = openat(CWD, anchor, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
                    .map_err(unreadable)?;
                for component in between {
                    dir = openat(&dir, *component, DIR_FLAGS, Mode::empty()).map_err(unreadable)?;
                }
                (dir, *last)
            }
            None => {
                let name = anchor.file_name().ok_or_else(|| {
                    WalkError::Unreadable(format!("{} has no name to measure", anchor.display()))
                })?;
                let parent = anchor.parent().unwrap_or(Path::new("/"));
                let dir = openat(CWD, parent, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
                    .map_err(unreadable)?;
                (dir, name)
            }
        };

        let root = entry(&statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(unreadable)?);
        let mut size = TreeSize::default();
        let mut seen = HardLinks::default();
        size.entries = 1;
        size.add(root.allocated);
        if root.is_link {
            size.links_not_followed = 1;
            size.root_is_link = true;
            return Ok(size);
        }
        if !root.is_dir {
            return Ok(size);
        }

        hooks.list(path);
        let root_fd = openat(&parent, name, DIR_FLAGS, Mode::empty()).map_err(unreadable)?;
        if entry(&fstat(&root_fd).map_err(unreadable)?).identity != root.identity {
            return Err(WalkError::Unreadable(CHANGED.to_string()));
        }

        let mut pending = Vec::new();
        let mut walk = Walk {
            root: path,
            exclude,
            limits,
            root_device: root.device,
            size: &mut size,
            seen: &mut seen,
            pending: &mut pending,
            hooks,
        };
        match walk.list(Rc::new(root_fd), path, 0) {
            Ok(Flow::Continue) => {}
            Ok(Flow::Stop) => return Ok(size),
            Err(e) => return Err(WalkError::Unreadable(e.to_string())),
        }

        while let Some(next) = walk.pending.pop() {
            walk.hooks.list(&next.path);
            let size = &mut *walk.size;
            let fd = match openat(&*next.parent, next.name.as_os_str(), DIR_FLAGS, Mode::empty()) {
                Ok(fd) => fd,
                // Gone since it was listed: nothing left to count.
                Err(Errno::NOENT) => continue,
                // No longer a directory at that name: a link, or a file.
                Err(Errno::LOOP | Errno::NOTDIR) => {
                    size.skip(format!("{}: {CHANGED}", relative(path, &next.path)));
                    continue;
                }
                Err(e) => {
                    size.skip(format!("{}: {}", relative(path, &next.path), error(e)));
                    continue;
                }
            };
            // Another directory moved into its place is not the one listed.
            match fstat(&fd) {
                Ok(st) if entry(&st).identity == next.identity => {}
                Ok(_) => {
                    size.skip(format!("{}: {CHANGED}", relative(path, &next.path)));
                    continue;
                }
                Err(e) => {
                    size.skip(format!("{}: {}", relative(path, &next.path), error(e)));
                    continue;
                }
            }
            match walk.list(Rc::new(fd), &next.path, next.depth) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Stop) => break,
                Err(e) => walk.size.skip(format!("{}: {e}", relative(path, &next.path))),
            }
        }
        Ok(size)
    }

    /// One walk in progress below `root`.
    struct Walk<'w, 'h> {
        root: &'w Path,
        exclude: &'w [&'w str],
        limits: WalkLimits,
        root_device: Option<u64>,
        size: &'w mut TreeSize,
        seen: &'w mut HardLinks,
        pending: &'w mut Vec<Pending>,
        hooks: &'w mut Hooks<'h>,
    }

    enum Flow {
        Continue,
        Stop,
    }

    impl Walk<'_, '_> {
    /// Count the entries of the open directory `fd`, and queue the
    /// directories among them.
    fn list(&mut self, fd: Rc<OwnedFd>, dir: &Path, depth: usize) -> io::Result<Flow> {
        let (root, exclude, limits) = (self.root, self.exclude, self.limits);
        let size = &mut *self.size;
        // Reads through a fresh handle to the same directory (`openat(fd,
        // ".")`), never by path.
        let entries = Dir::read_from(&*fd).map_err(error)?;
        for dirent in entries {
            let dirent = match dirent {
                Ok(dirent) => dirent,
                Err(e) => {
                    size.skip(format!("{}: {}", relative(root, dir), error(e)));
                    continue;
                }
            };
            let name = OsStr::from_bytes(dirent.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            if depth == 0 && exclude.iter().any(|excluded| name == *excluded) {
                continue;
            }
            if size.entries >= limits.max_entries {
                size.hit_limit = true;
                size.skip(format!("{}: more than {} entries", relative(root, dir), limits.max_entries));
                return Ok(Flow::Stop);
            }
            let path = dir.join(name);
            self.hooks.stat(&path);
            let st = match statat(&*fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => entry(&st),
                Err(Errno::NOENT) => continue,
                Err(e) => {
                    size.skip(format!("{}: {}", relative(root, &path), error(e)));
                    continue;
                }
            };
            if !count(size, self.seen, &st, self.root_device) {
                continue;
            }
            if depth + 1 >= limits.max_depth {
                size.hit_limit = true;
                size.skip(format!("{}: deeper than {} levels", relative(root, &path), limits.max_depth));
                continue;
            }
            self.pending.push(Pending {
                parent: fd.clone(),
                name: name.to_os_string(),
                path,
                depth: depth + 1,
                identity: st.identity,
            });
        }
        Ok(Flow::Continue)
    }
    }

    pub(in crate::storage_accounting) fn is_link_meta(meta: &std::fs::Metadata) -> bool {
        meta.file_type().is_symlink()
    }
}

#[cfg(not(unix))]
use portable::walk;

// Also compiled into Unix test builds, so its logic runs somewhere CI does.
#[cfg(any(not(unix), test))]
mod portable {
    use super::*;
    use std::fs::{self, Metadata};

    fn entry(meta: &Metadata) -> Entry {
        Entry {
            allocated: if meta.is_dir() { 0 } else { meta.len() },
            is_dir: meta.is_dir(),
            is_link: is_link_meta(meta),
            // Not available on stable Rust off Unix. A volume mounted inside
            // the tree is a reparse point, which is never entered.
            device: None,
            identity: None,
            links: 1,
        }
    }

    /// What a directory looked like when it was listed, to tell whether it
    /// is still the same one after being read.
    fn fingerprint(meta: &Metadata) -> (bool, bool, Option<std::time::SystemTime>) {
        (meta.is_dir(), is_link_meta(meta), meta.created().ok())
    }

    pub(super) fn walk(
        anchor: &Path,
        path: &Path,
        exclude: &[&str],
        limits: WalkLimits,
        hooks: &mut Hooks<'_>,
    ) -> Result<TreeSize, WalkError> {
        let components = components_below(anchor, path)?;
        // Every directory between the anchor and `path` must be a real one.
        let mut between = anchor.to_path_buf();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            between.push(component);
            match fs::symlink_metadata(&between) {
                Ok(meta) if meta.is_dir() && !is_link_meta(&meta) => {}
                Ok(_) => return Err(WalkError::Unreadable(CHANGED.to_string())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(WalkError::NotFound),
                Err(e) => return Err(WalkError::Unreadable(e.to_string())),
            }
        }

        let root_meta = fs::symlink_metadata(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => WalkError::NotFound,
            _ => WalkError::Unreadable(e.to_string()),
        })?;
        let root = entry(&root_meta);
        let mut size = TreeSize::default();
        let mut seen = HardLinks::default();
        size.entries = 1;
        size.add(root.allocated);
        if root.is_link {
            size.links_not_followed = 1;
            size.root_is_link = true;
            return Ok(size);
        }
        if !root.is_dir {
            return Ok(size);
        }

        let mut pending: Vec<(PathBuf, usize, Metadata)> = vec![(path.to_path_buf(), 0, root_meta)];
        while let Some((dir, depth, before)) = pending.pop() {
            hooks.list(&dir);
            let listing: Vec<fs::DirEntry> = match fs::read_dir(&dir) {
                Ok(entries) => entries.filter_map(Result::ok).collect(),
                Err(e) if e.kind() == io::ErrorKind::NotFound && depth > 0 => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(WalkError::NotFound),
                Err(e) if depth == 0 => return Err(WalkError::Unreadable(e.to_string())),
                Err(e) => {
                    size.skip(format!("{}: {e}", relative(path, &dir)));
                    continue;
                }
            };
            // Read by path, so check the directory is still the one that was
            // listed: a junction put in its place would have been followed.
            let unchanged = fs::symlink_metadata(&dir).is_ok_and(|after| fingerprint(&after) == fingerprint(&before));
            if !unchanged {
                if depth == 0 {
                    return Err(WalkError::Unreadable(CHANGED.to_string()));
                }
                size.skip(format!("{}: {CHANGED}", relative(path, &dir)));
                continue;
            }
            for dirent in listing {
                if depth == 0 && exclude.iter().any(|name| dirent.file_name() == *name) {
                    continue;
                }
                if size.entries >= limits.max_entries {
                    size.hit_limit = true;
                    size.skip(format!("{}: more than {} entries", relative(path, &dir), limits.max_entries));
                    return Ok(size);
                }
                let child = dirent.path();
                hooks.stat(&child);
                // Describes the entry itself, from the listing.
                let meta = match dirent.metadata() {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        size.skip(format!("{}: {e}", relative(path, &child)));
                        continue;
                    }
                };
                if !count(&mut size, &mut seen, &entry(&meta), root.device) {
                    continue;
                }
                if depth + 1 >= limits.max_depth {
                    size.hit_limit = true;
                    size.skip(format!("{}: deeper than {} levels", relative(path, &child), limits.max_depth));
                    continue;
                }
                pending.push((child, depth + 1, meta));
            }
        }
        Ok(size)
    }

    #[cfg(windows)]
    pub(in crate::storage_accounting) fn is_link_meta(meta: &Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        // Every reparse point, not only the name surrogates `is_symlink`
        // covers: a junction, a mounted volume, and a cloud placeholder that
        // would be downloaded on access all qualify.
        meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    pub(in crate::storage_accounting) fn is_link_meta(meta: &Metadata) -> bool {
        meta.file_type().is_symlink()
    }
}

/// Whether `meta` describes a link the walk would not follow.
pub(super) fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    return unix::is_link_meta(meta);
    #[cfg(not(unix))]
    return portable::is_link_meta(meta);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A root measured as its own anchor.
    fn measure_tree(root: &Path, exclude: &[&str], limits: WalkLimits) -> Result<TreeSize, WalkError> {
        measure(root, root, exclude, limits)
    }

    fn write(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; len]).unwrap();
    }

    #[test]
    fn a_missing_root_is_not_an_empty_one() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            measure_tree(&tmp.path().join("absent"), &[], WalkLimits::default()),
            Err(WalkError::NotFound)
        ));
        let empty = measure_tree(tmp.path(), &[], WalkLimits::default()).unwrap();
        assert!(empty.is_complete());
        assert_eq!(empty.entries, 1);
    }

    #[test]
    fn sizes_add_up_across_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a"), 10_000);
        write(&tmp.path().join("d/b"), 20_000);
        write(&tmp.path().join("d/e/c"), 30_000);
        let size = measure_tree(tmp.path(), &[], WalkLimits::default()).unwrap();
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
        let size = measure_tree(tmp.path(), &["target"], WalkLimits::default()).unwrap();
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

        let size = measure_tree(tmp.path(), &[], WalkLimits::default()).unwrap();
        assert!(size.is_complete());
        assert_eq!(size.links_not_followed, 3);
        assert!(size.bytes < 100_000, "{size:?}");

        // A root that is itself a link is measured as the link.
        let as_root = measure(tmp.path(), &tmp.path().join("escape"), &[], WalkLimits::default()).unwrap();
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
        let size = measure_tree(tmp.path(), &[], WalkLimits::default()).unwrap();
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
        let result = measure_tree(tmp.path(), &[], WalkLimits::default());
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
        let size = measure_tree(
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
        assert_eq!(size.skipped, 1);
        assert!(size.first_skip_reason.unwrap().contains("more than 5 entries"));

        write(&tmp.path().join("a/b/c/d"), 10);
        let shallow = measure_tree(
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
        let size = walk(tmp.path(), tmp.path(), &[], WalkLimits::default(), &mut hooks).unwrap();

        assert!(size.is_complete(), "{size:?}");
        assert!(size.bytes >= 10_000 && size.bytes < 100_000, "{size:?}");
    }
}


/// A directory swapped between being listed and being opened: the window
/// a pathname walk leaves open. Unix only, where the walk closes it; see the
/// module documentation for Windows.
#[cfg(all(test, unix))]
mod containment {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    const OUTSIDE: usize = 4 * 1024 * 1024;

    struct Trees {
        outside: tempfile::TempDir,
        tmp: tempfile::TempDir,
    }

    /// An owned tree with `sub/own` (64 KiB), and an outside tree with 4 MiB.
    fn trees() -> Trees {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), vec![7u8; OUTSIDE]).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/own"), vec![1u8; 64 * 1024]).unwrap();
        Trees { outside, tmp }
    }

    fn swap_for_link(path: &Path, target: &Path) {
        fs::rename(path, path.with_file_name("sub.moved")).unwrap();
        symlink(target, path).unwrap();
    }

    #[test]
    fn a_directory_swapped_for_a_link_after_it_was_listed_is_not_entered() {
        let t = trees();
        let mut swap = |path: &Path| {
            if path.ends_with("sub") {
                swap_for_link(path, t.outside.path());
            }
        };
        let mut hooks = Hooks { before_list: Some(&mut swap), before_stat: None };
        let size = walk(t.tmp.path(), t.tmp.path(), &[], WalkLimits::default(), &mut hooks).unwrap();

        assert!(size.bytes < OUTSIDE as u64, "the outside tree was counted: {size:?}");
        assert!(!size.is_complete(), "{size:?}");
        assert!(size.first_skip_reason.unwrap().contains(CHANGED));
    }

    #[test]
    fn a_directory_swapped_for_another_directory_is_not_counted_as_itself() {
        let t = trees();
        fs::create_dir(t.tmp.path().join("other")).unwrap();
        fs::write(t.tmp.path().join("other/big"), vec![2u8; OUTSIDE]).unwrap();
        let mut swap = |path: &Path| {
            if path.ends_with("sub") {
                fs::rename(path, path.with_file_name("sub.moved")).unwrap();
                fs::rename(path.with_file_name("other"), path).unwrap();
            }
        };
        let mut hooks = Hooks { before_list: Some(&mut swap), before_stat: None };
        let size = walk(t.tmp.path(), t.tmp.path(), &[], WalkLimits::default(), &mut hooks).unwrap();

        assert!(!size.is_complete(), "{size:?}");
        assert!(size.first_skip_reason.unwrap().contains(CHANGED));
    }

    #[test]
    fn a_measured_root_swapped_for_a_link_before_it_is_opened_fails() {
        let t = trees();
        let sub = t.tmp.path().join("sub");
        let mut swap = |path: &Path| {
            if path.ends_with("sub") {
                swap_for_link(path, t.outside.path());
            }
        };
        let mut hooks = Hooks { before_list: Some(&mut swap), before_stat: None };
        match walk(t.tmp.path(), &sub, &[], WalkLimits::default(), &mut hooks) {
            Err(WalkError::Unreadable(reason)) => assert!(reason.contains(CHANGED), "{reason}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_link_between_the_anchor_and_the_measured_path_is_not_followed() {
        let t = trees();
        fs::create_dir(t.outside.path().join("b")).unwrap();
        fs::write(t.outside.path().join("b/big"), vec![3u8; OUTSIDE]).unwrap();
        symlink(t.outside.path(), t.tmp.path().join("a")).unwrap();

        let result = measure(t.tmp.path(), &t.tmp.path().join("a/b"), &[], WalkLimits::default());
        assert!(matches!(result, Err(WalkError::Unreadable(_))), "{result:?}");
        // And a path that climbs out of the anchor is refused outright.
        let climbing = measure(t.tmp.path(), &t.tmp.path().join("../x"), &[], WalkLimits::default());
        assert!(matches!(climbing, Err(WalkError::Unreadable(_))), "{climbing:?}");
    }

    /// Races a real swapper against real walks. The assertion holds for
    /// every interleaving, so it cannot flake: it can only fail if some
    /// interleaving escapes.
    #[test]
    fn walks_racing_a_swapper_never_count_the_outside_tree() {
        let t = trees();
        let sub = t.tmp.path().join("sub");
        let parked = t.tmp.path().join("sub.real");
        let link = t.tmp.path().join("sub.link");
        symlink(t.outside.path(), &link).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let (sub, parked, link, stop) = (sub.clone(), parked.clone(), link.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // `sub` alternates between the real directory and a link
                    // to the outside tree; each step is one atomic rename.
                    let _ = fs::rename(&sub, &parked);
                    let _ = fs::rename(&link, &sub);
                    let _ = fs::rename(&sub, &link);
                    let _ = fs::rename(&parked, &sub);
                }
            })
        };
        for _ in 0..2000 {
            if let Ok(size) = measure(t.tmp.path(), t.tmp.path(), &[], WalkLimits::default()) {
                assert!(size.bytes < OUTSIDE as u64, "a walk escaped into the outside tree: {size:?}");
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        swapper.join().unwrap();
    }
}

/// The Windows walk's change detection, run on Unix with symbolic links
/// standing in for junctions: the same pathname-then-recheck logic, not the
/// Windows filesystem.
#[cfg(all(test, unix))]
mod portable_detection {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    const OUTSIDE: usize = 4 * 1024 * 1024;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), vec![7u8; OUTSIDE]).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/own"), vec![1u8; 64 * 1024]).unwrap();
        (outside, tmp)
    }

    #[test]
    fn a_directory_left_replaced_by_a_link_is_discarded_as_changed() {
        let (outside, tmp) = setup();
        let mut swap = |path: &Path| {
            if path.ends_with("sub") {
                fs::rename(path, path.with_file_name("sub.moved")).unwrap();
                symlink(outside.path(), path).unwrap();
            }
        };
        let mut hooks = Hooks { before_list: Some(&mut swap), before_stat: None };
        let size = portable::walk(tmp.path(), tmp.path(), &[], WalkLimits::default(), &mut hooks).unwrap();
        assert!(size.bytes < OUTSIDE as u64, "{size:?}");
        assert!(size.first_skip_reason.unwrap().contains(CHANGED));
    }
}
