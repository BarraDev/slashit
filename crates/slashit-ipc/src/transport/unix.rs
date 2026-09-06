//! Unix domain socket transport. Linux and macOS.
//!
//! The **directory** is the access control. Permissions on the socket file
//! itself can only be applied after `bind()` returns, which leaves a window in
//! which the socket exists at its final path with the process umask; a 0700
//! parent closes that window because nobody else can traverse into it. The
//! 0600 on the socket is belt and braces for the case where the directory is
//! ever relaxed.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

use super::{Accepted, IpcStream, TransportAuth};
use crate::endpoint::Endpoint;

#[derive(Debug)]
pub struct UnixListenerTransport {
    listener: UnixListener,
    path: PathBuf,
    ino: u64,
    /// The instance lease, held for as long as this listener exists.
    ///
    /// Never read or written — only kept alive, because the kernel releases
    /// its `flock` when the descriptor closes. That is what makes it a
    /// crash-safe statement of "a live process owns this endpoint": a hard
    /// kill releases it with no stale artifact to reason about, whereas a
    /// lockfile's mere existence would need a staleness rule, and a PID file
    /// would need one that cannot be made correct.
    _lease: std::fs::File,
}

impl UnixListenerTransport {
    pub async fn bind(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            prepare_socket_dir(parent)?;
        }

        // Acquired before the reclaim-then-bind sequence below, which is a
        // check-then-act with no atomicity of its own, and then held for the
        // listener's whole lifetime rather than released at the end of this
        // function. Holding it is what makes owning this endpoint a lease a
        // live process has, instead of merely a socket file that happens to
        // exist. Non-blocking, so a second instance is told immediately that
        // it lost rather than waiting behind the winner.
        let lease = acquire_claim_lock(&path.with_extension("lock"))?;

        claim_socket(path).await?;

        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let ino = std::fs::metadata(path)?.ino();

        Ok(Self {
            listener,
            path: path.to_path_buf(),
            ino,
            _lease: lease,
        })
    }

    pub async fn accept(&mut self) -> io::Result<Accepted> {
        let (stream, _addr) = self.listener.accept().await?;
        Ok(Accepted {
            stream: IpcStream::new(stream),
            // Reaching a socket inside a 0700 directory required being the
            // owning user.
            transport_auth: TransportAuth::OsVerifiedOwner,
            endpoint: self.endpoint(),
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix {
            path: self.path.clone(),
        }
    }
}

impl Drop for UnixListenerTransport {
    /// Remove the socket so the next start does not have to decide whether a
    /// leftover file is stale — but only if it is still *this* socket.
    /// `ClaimLock` serializes two processes racing to bind at startup, but it
    /// cannot protect a socket that was already bound before either process
    /// started shutting down: without the inode check, a process that loses a
    /// race and later exits could unlink the *newer* process's live socket
    /// (whose bind it never even contended with) out from under it, since a
    /// path match alone doesn't say which process's file it still is.
    ///
    /// Best-effort: a failure here is not worth failing shutdown over, and
    /// `claim_socket` handles a leftover on the next start anyway.
    fn drop(&mut self) {
        match std::fs::metadata(&self.path) {
            Ok(meta) if meta.ino() != self.ino => return,
            _ => {}
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serializes the stale-socket-reclaim-then-bind sequence across processes.
///
/// `claim_socket` followed by `UnixListener::bind` is a check-then-act
/// sequence with no atomicity of its own: two processes started together can
/// both observe the same stale socket, and the second to run `remove_file`
/// can delete the first's freshly bound (live) socket instead of the stale
/// one it actually saw, leaving the first holding a listener nothing can
/// reach. `flock` is released automatically when the returned file's
/// descriptor closes — including on a crash — so, unlike a lockfile
/// existence check, holding this lock never itself needs a staleness rule.
/// The caller only needs to keep the returned `File` alive for as long as the
/// lock must be held; its contents are never read or written.
fn acquire_claim_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    flock_exclusive(&file)?;
    Ok(file)
}

fn flock_exclusive(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // No `libc` dependency in this crate; declaring the one symbol needed
    // avoids pulling one in just for this.
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    // Safety: `file`'s fd is open and valid for the duration of this call.
    //
    // Non-blocking: the lease is now held for the listener's whole lifetime,
    // so a blocking acquisition would park a losing instance until the winner
    // exited instead of telling it immediately that it lost. `EWOULDBLOCK` is
    // reported as `AddrInUse` so every caller sees one "someone else already
    // owns this endpoint" error, whichever of the two guards detected it.
    let ret = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if ret != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another SlashIt instance is already listening",
            ));
        }
        return Err(err);
    }
    Ok(())
}

/// Create the socket directory with owner-only permissions.
///
/// Only creates when nothing exists yet at `dir` — an already-existing entry
/// (from `$XDG_RUNTIME_DIR/slashit-app`, or a leftover in the world-writable
/// temp-dir fallback) is never chmod'd on trust alone; it goes through
/// [`ensure_safe_runtime_dir`] first, the same as a freshly created one.
fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    ensure_safe_runtime_dir(dir)
}

/// Refuse to trust a pre-existing runtime directory just because something is
/// there. `create_dir`-then-chmod only covers a directory this process just
/// made; a directory that already existed could be a symlink planted to
/// redirect the chmod (and the eventual socket) somewhere else, a leftover
/// owned by a different user, or one left in an over-permissive mode by an
/// older SlashIt version. This is the single authoritative check for both the
/// `$XDG_RUNTIME_DIR` and temp-dir-fallback cases — both funnel through
/// [`prepare_socket_dir`], so the invariant only needs to live once.
fn ensure_safe_runtime_dir(dir: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "refusing to use {}: it is a symlink, not a real directory",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "refusing to use {}: not a directory",
            dir.display()
        )));
    }
    if meta.uid() != current_uid() {
        return Err(io::Error::other(format!(
            "refusing to use {}: not owned by the current user",
            dir.display()
        )));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn current_uid() -> u32 {
    // No `libc` dependency in this crate; declaring the one symbol needed
    // avoids pulling one in just for this.
    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }
    // Safety: geteuid takes no arguments and always succeeds.
    unsafe { libc_geteuid() }
}

/// Decide whether an existing socket file may be replaced.
///
/// A socket left behind by a crash must be cleared, but one belonging to a
/// live instance must not: removing it and binding again silently steals every
/// subsequent client connection from the running app. Connecting is the only
/// reliable liveness test — a bound socket accepts, an orphaned inode refuses.
async fn claim_socket(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    match UnixStream::connect(path).await {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another SlashIt instance is already listening on {}",
                path.display()
            ),
        )),
        Err(_) => std::fs::remove_file(path),
    }
}

pub async fn connect(path: &Path) -> io::Result<IpcStream> {
    Ok(IpcStream::new(UnixStream::connect(path).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn binding_creates_an_owner_only_socket_and_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("run");
        let path = dir.join("slashit.sock");

        let listener = UnixListenerTransport::bind(&path).await.unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "socket dir mode {dir_mode:o}");

        let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(sock_mode, 0o600, "socket mode {sock_mode:o}");

        drop(listener);
    }

    #[tokio::test]
    async fn a_live_owner_cannot_be_displaced() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("slashit.sock");

        let _first = UnixListenerTransport::bind(&path).await.unwrap();

        let second = UnixListenerTransport::bind(&path).await;
        let err = second.expect_err("a second bind must be refused");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(err.to_string().contains("already listening"));
    }

    #[tokio::test]
    async fn a_stale_socket_file_is_reclaimed() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("slashit.sock");

        // Bind and drop: the file is removed by Drop, so recreate it as the
        // orphaned inode a crash would leave behind.
        {
            let _l = UnixListenerTransport::bind(&path).await.unwrap();
        }
        // Binding and immediately dropping the std listener leaves the inode
        // behind with nothing accepting on it, which is what a crash leaves.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "precondition: a leftover socket file exists");

        // Nothing is listening on it any more, so it may be reclaimed.
        let listener = UnixListenerTransport::bind(&path).await;
        assert!(
            listener.is_ok(),
            "a stale socket must be reclaimed, got {:?}",
            listener.err()
        );
    }

    #[tokio::test]
    async fn drop_does_not_remove_a_socket_that_is_no_longer_its_own() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("slashit.sock");

        let listener = UnixListenerTransport::bind(&path).await.unwrap();

        // Simulate what a racing second process leaves behind: this path now
        // holds a different socket (a different inode) than the one this
        // instance bound, as if another process had reclaimed and rebound it
        // between this instance's bind and its eventual drop.
        std::fs::remove_file(&path).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(
            path.exists(),
            "precondition: a different socket now occupies the path"
        );

        drop(listener);

        assert!(
            path.exists(),
            "Drop must not remove a socket it did not itself bind"
        );
    }

    #[tokio::test]
    async fn dropping_the_listener_removes_the_socket() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("slashit.sock");

        {
            let _l = UnixListenerTransport::bind(&path).await.unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "Drop should clean the socket up");
    }

    #[test]
    fn prepare_socket_dir_creates_a_fresh_directory_at_0700() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("runtime");

        prepare_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn prepare_socket_dir_accepts_and_tightens_an_existing_owned_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("runtime");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        prepare_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "an over-permissive but owned directory is tightened, not rejected");
    }

    #[test]
    fn prepare_socket_dir_refuses_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("elsewhere");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("runtime");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = prepare_socket_dir(&link).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[test]
    fn prepare_socket_dir_refuses_a_regular_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime");
        std::fs::write(&path, b"not a directory").unwrap();

        let err = prepare_socket_dir(&path).unwrap_err();
        assert!(err.to_string().contains("not a directory"));
    }

    // Rejecting a directory owned by a different uid is exercised by
    // `ensure_safe_runtime_dir`'s `meta.uid() != current_uid()` check, but a
    // unit test cannot fabricate a directory owned by another user without
    // root — that branch is verified by code inspection only.

    #[tokio::test]
    async fn a_local_peer_is_os_verified() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("slashit.sock");
        let mut listener = UnixListenerTransport::bind(&path).await.unwrap();

        let client = tokio::spawn({
            let path = path.clone();
            async move { connect(&path).await }
        });

        let accepted = listener.accept().await.unwrap();
        assert_eq!(accepted.transport_auth, TransportAuth::OsVerifiedOwner);
        assert_eq!(accepted.endpoint, Endpoint::Unix { path });

        client.await.unwrap().unwrap();
    }
}
