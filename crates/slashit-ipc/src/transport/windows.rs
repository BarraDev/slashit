//! Windows named pipe transport.
//!
//! The security properties match the Unix socket rather than TCP:
//!
//! - `first_pipe_instance(true)` on the initial instance makes creation fail
//!   with `ERROR_ACCESS_DENIED` if another process already owns the name. That
//!   is the Windows analogue of the connect-first liveness check: an instance
//!   cannot silently take over another instance's clients.
//! - `reject_remote_clients(true)` refuses connections arriving over SMB, so
//!   the pipe is genuinely local rather than a network endpoint that happens
//!   to be reachable by UNC path.
//! - Every instance is created with an explicit DACL granting access only to
//!   the creating user's SID and to `LocalSystem`, which is what makes the
//!   peer OS-authenticated. Windows' *default* named-pipe security descriptor
//!   (what `CreateNamedPipe` grants when no security attributes are passed)
//!   additionally grants full control to the built-in Administrators group —
//!   correct for a single-user desktop where the user's own account already
//!   is an administrator, but on a machine with more than one administrator
//!   account it would let a *different* admin connect and be treated as this
//!   pipe's owner. The explicit descriptor below closes that gap; see
//!   [`owner_only_security_attributes`].
//!
//! A named pipe server handles one client per instance, so the listener keeps
//! one instance waiting and creates the replacement as soon as a client
//! arrives. Without that, a second client connecting during dispatch would be
//! refused rather than queued.

use std::io;
use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

use super::{Accepted, IpcStream, TransportAuth};
use crate::endpoint::Endpoint;

/// Windows error code for "all pipe instances are busy".
const ERROR_PIPE_BUSY: i32 = 231;

/// Create a named pipe server instance restricted to the current user and
/// `LocalSystem`, in place of Windows' broader default DACL.
///
/// Every instance — the initial one and every replacement created on
/// `accept()` — must go through this, not just the first: a DACL applies to
/// the specific pipe instance `CreateNamedPipe` returns, not to the name, so
/// an instance created any other way would silently fall back to the
/// Administrators-inclusive default.
mod security {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const SDDL_REVISION_1: u32 = 1;

    /// The calling process's own user SID, as the textual form SDDL expects
    /// (`S-1-5-21-...`).
    ///
    /// Looked up explicitly rather than relying on the SDDL `OW` ("Owner
    /// Rights") alias: `OW` resolves to whatever the *new object's default
    /// owner* is, and for a process running with a UAC split token that
    /// default can be the Administrators group rather than the user's own
    /// SID — which would silently reintroduce the exact access this module
    /// exists to remove.
    fn current_user_sid() -> io::Result<String> {
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let _guard = scopeguard(token);

            let mut needed: u32 = 0;
            // First call is expected to fail with ERROR_INSUFFICIENT_BUFFER;
            // its only job is to report the buffer size to allocate.
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buf = vec![0u8; needed as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }

            // Safety: `buf` was sized and filled by `GetTokenInformation` for
            // exactly `TokenUser`, which is documented to return a `TOKEN_USER`.
            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            sid_to_string(token_user.User.Sid)
        }
    }

    /// # Safety
    /// `sid` must be a valid `PSID` for the duration of this call, as
    /// guaranteed by its caller holding the buffer it points into.
    unsafe fn sid_to_string(sid: *mut c_void) -> io::Result<String> {
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

        let mut wide_ptr: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut wide_ptr) == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = (0..).take_while(|&i| *wide_ptr.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide_ptr, len));
        LocalFree(wide_ptr as *mut c_void);
        Ok(s)
    }

    /// Closes `token` when dropped, so an early `?` return above cannot leak
    /// the handle.
    struct HandleGuard(HANDLE);
    fn scopeguard(h: HANDLE) -> HandleGuard {
        HandleGuard(h)
    }
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// A security descriptor built from an SDDL string, freed on drop.
    struct OwnedSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for OwnedSecurityDescriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    /// Everything `CreateNamedPipe` needs to restrict the instance it creates
    /// to the current user and `LocalSystem`.
    ///
    /// Kept alive as one value because the `SECURITY_ATTRIBUTES` the caller
    /// passes to `create_with_security_attributes_raw` borrows from the
    /// descriptor: dropping the descriptor first would leave a dangling
    /// `lpSecurityDescriptor` for the duration of that call.
    pub struct OwnerOnlyAttributes {
        _descriptor: OwnedSecurityDescriptor,
        attrs: SECURITY_ATTRIBUTES,
    }

    impl OwnerOnlyAttributes {
        /// Raw pointer to pass to
        /// `ServerOptions::create_with_security_attributes_raw`. Valid only
        /// for the lifetime of `self`.
        pub fn as_raw(&self) -> *mut c_void {
            &self.attrs as *const SECURITY_ATTRIBUTES as *mut c_void
        }
    }

    /// Build the owner-only security attributes for a new pipe instance.
    ///
    /// `SY` is `LocalSystem` — granted the same way root implicitly bypasses
    /// the Unix socket's directory permissions, not as a second trusted user.
    /// The explicit user SID, not the SDDL `OW` alias, is what actually
    /// restricts the pipe to this account; see [`current_user_sid`].
    pub fn owner_only_security_attributes() -> io::Result<OwnerOnlyAttributes> {
        let sid = current_user_sid()?;
        let sddl = format!("D:(A;;GA;;;SY)(A;;GA;;;{sid})");
        let mut wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();

        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // Safety: `wide` is a valid, NUL-terminated UTF-16 string for the
        // duration of this call; `descriptor` is only written on success.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_mut_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };

        Ok(OwnerOnlyAttributes {
            _descriptor: OwnedSecurityDescriptor(descriptor),
            attrs,
        })
    }
}

/// Create a pipe instance restricted to the current user and `LocalSystem`,
/// via `ServerOptions::create_with_security_attributes_raw` rather than
/// `create`, which would fall back to Windows' Administrators-inclusive
/// default DACL. Used for both the initial instance and every replacement —
/// a DACL applies per instance, not per name.
fn create_owner_only(
    options: &ServerOptions,
    name: &str,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let attrs = security::owner_only_security_attributes()?;
    // Safety: `attrs` outlives this call, and `CreateNamedPipe` only reads
    // through the security attributes pointer while creating the instance —
    // it does not retain the pointer afterward.
    unsafe { options.create_with_security_attributes_raw(name, attrs.as_raw()) }
}

#[derive(Debug)]
pub struct NamedPipeListenerTransport {
    name: String,
    /// The instance currently waiting for a client.
    pending: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl NamedPipeListenerTransport {
    pub fn bind(name: &str) -> io::Result<Self> {
        let pending = create_owner_only(
            // Refuse to start if another instance already owns the name,
            // rather than joining it and stealing half its connections.
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true),
            name,
        )
        .map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another SlashIt instance is already listening on {name}"),
                )
            } else {
                e
            }
        })?;

        Ok(Self {
            name: name.to_string(),
            pending,
        })
    }

    pub async fn accept(&mut self) -> io::Result<Accepted> {
        // Create the replacement *before* connecting `self.pending`, not
        // after. If replacement creation failed after a successful connect,
        // `self.pending` would be left connected but never handed to a
        // caller; the next `accept()`'s own `connect()` call on that same
        // instance then returns `ERROR_PIPE_CONNECTED` immediately without
        // ever delivering that client, and repeated failures here can run out
        // the accept loop's retry budget. Creating first means a failure
        // leaves `self.pending` exactly as it was, a clean state to retry
        // `accept()` again — and the next client is still never refused.
        let next = create_owner_only(ServerOptions::new().reject_remote_clients(true), &self.name)?;

        self.pending.connect().await?;

        let connected = std::mem::replace(&mut self.pending, next);

        Ok(Accepted {
            stream: IpcStream::new(connected),
            transport_auth: TransportAuth::OsVerifiedOwner,
            endpoint: self.endpoint(),
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::NamedPipe {
            name: self.name.clone(),
        }
    }
}

pub async fn connect(name: &str) -> io::Result<IpcStream> {
    // A pipe with every instance busy is transient, not a failure: the server
    // creates the replacement instance immediately after accepting. Retry
    // briefly rather than reporting the app as not running.
    let deadline = std::time::Duration::from_secs(2);
    let started = std::time::Instant::now();

    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => return Ok(IpcStream::new(client)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if started.elapsed() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("all instances of {name} stayed busy for {deadline:?}"),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::pipe_name;

    fn unique_name(tag: &str) -> String {
        format!("{}-test-{}-{}", pipe_name(), tag, std::process::id())
    }

    #[test]
    fn owner_only_security_attributes_can_be_built() {
        // Exercises the SID lookup and SDDL parse in isolation from pipe
        // creation, so a failure here points at the security descriptor
        // rather than at `CreateNamedPipe` itself.
        let attrs = security::owner_only_security_attributes()
            .expect("the current process always has a queryable token and user SID");
        assert!(!attrs.as_raw().is_null());
    }

    #[tokio::test]
    async fn a_live_owner_cannot_be_displaced() {
        let name = unique_name("owner");
        let _first = NamedPipeListenerTransport::bind(&name).unwrap();

        let second = NamedPipeListenerTransport::bind(&name);
        let err = second.expect_err("a second bind must be refused");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn a_local_peer_is_os_verified() {
        let name = unique_name("peer");
        let mut listener = NamedPipeListenerTransport::bind(&name).unwrap();

        let client = tokio::spawn({
            let name = name.clone();
            async move { connect(&name).await }
        });

        let accepted = listener.accept().await.unwrap();
        assert_eq!(accepted.transport_auth, TransportAuth::OsVerifiedOwner);
        assert_eq!(accepted.endpoint, Endpoint::NamedPipe { name });

        client.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_second_client_is_served_rather_than_refused() {
        // Regression guard for the one-instance-per-server pitfall: without
        // creating the replacement instance on accept, this second connect
        // fails with FILE_NOT_FOUND.
        let name = unique_name("queue");
        let mut listener = NamedPipeListenerTransport::bind(&name).unwrap();

        let first = tokio::spawn({
            let name = name.clone();
            async move { connect(&name).await }
        });
        let _accepted = listener.accept().await.unwrap();
        first.await.unwrap().unwrap();

        let second = tokio::spawn({
            let name = name.clone();
            async move { connect(&name).await }
        });
        let accepted = listener.accept().await.unwrap();
        assert_eq!(accepted.transport_auth, TransportAuth::OsVerifiedOwner);
        second.await.unwrap().unwrap();
    }
}
