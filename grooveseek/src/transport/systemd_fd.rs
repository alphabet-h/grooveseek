//! (計画 4 段 A) The listening socket systemd hands over, and nothing else.
//!
//! `groove serve --systemd-socket` takes the file descriptor a `.socket` unit
//! bound rather than binding an address of its own. This module is the whole
//! of that: it reads `LISTEN_PID` / `LISTEN_FDS`, checks the descriptor is
//! the kind of socket this server can serve on, and hands back an `std`
//! listener. [`crate::transport::http::run_http`] turns that into a tokio one.
//!
//! **By hand, against the `libc` this crate already has on `cfg(unix)`, rather
//! than through `libsystemd` or `listenfd`.** The reasoning is in ADR-0021
//! (`docs/decisions/0021-take-the-socket-you-were-given.md`).
//!
//! **What `sd_listen_fds(3)` requires, and this module does.** Compare
//! `$LISTEN_PID` with our own pid *first*, because a variable inherited from
//! a parent otherwise hands us somebody else's descriptor. Set `FD_CLOEXEC`
//! on what we keep. Never `shutdown(2)` the socket and never `unlink` its
//! path: the service manager keeps its own copy of the descriptor, and
//! `systemd.socket(5)` says a service "must not unlink the socket from a file
//! system".
//!
//! **The environment is left in place.** `std::env::remove_var` is `unsafe`
//! in edition 2024 and undefined behaviour while another thread reads the
//! environment. Instead [`take_listener`] can only succeed once per process,
//! which is what unsetting the variables was going to buy.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

/// The first descriptor a service manager passes (`SD_LISTEN_FDS_START`).
pub(crate) const LISTEN_FDS_START: RawFd = 3;

/// The socket `groove serve --systemd-socket` was handed.
#[derive(Debug)]
pub(crate) enum SystemdSocket {
    /// `AF_INET` / `AF_INET6`. Served exactly like a socket groove bound
    /// itself, peer check included.
    Tcp(std::net::TcpListener),
    /// `AF_UNIX`. No address and no port, which is what decides the admin
    /// peer rule ([`crate::transport::http::admin_peer_rule`]).
    Unix(std::os::unix::net::UnixListener),
}

/// `$LISTEN_PID` and `$LISTEN_FDS`, checked in the order `sd_listen_fds(3)`
/// checks them.
///
/// Taking the values as arguments rather than reading the environment is what
/// makes the table of refusals a unit test. The environment is read in
/// [`take_listener`] and nowhere else.
pub(crate) fn check_listen_env(
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    self_pid: u32,
) -> Result<()> {
    let Some(pid) = listen_pid.filter(|v| !v.is_empty()) else {
        bail!(
            "--systemd-socket was given but LISTEN_PID is not set, so this process was not started by a socket unit (own pid {self_pid})"
        );
    };
    let Ok(named) = pid.parse::<u32>() else {
        bail!(
            "--systemd-socket was given but LISTEN_PID is not a number: {pid} (own pid {self_pid})"
        );
    };
    if named != self_pid {
        bail!(
            "--systemd-socket was given but LISTEN_PID names {named}, not this process ({self_pid}); the variable was inherited from a parent and the descriptors it describes belong to that parent"
        );
    }
    let Some(fds) = listen_fds.filter(|v| !v.is_empty()) else {
        bail!("--systemd-socket was given but LISTEN_FDS is not set (own pid {self_pid})");
    };
    let Ok(count) = fds.parse::<u32>() else {
        bail!("--systemd-socket was given but LISTEN_FDS is not a number: {fds}");
    };
    if count != 1 {
        bail!(
            "--systemd-socket expects exactly one socket, but LISTEN_FDS is {count}; the socket unit must carry exactly one ListenStream="
        );
    }
    Ok(())
}

/// One `getsockopt` that reads an `int`, so the call sites do not each spell
/// the pointer cast.
fn getsockopt_int(fd: RawFd, level: libc::c_int, name: libc::c_int) -> Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` live across the call, `len` describes `value`,
    // and the descriptor is only read from.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            level,
            name,
            (&raw mut value).cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("getsockopt on the descriptor systemd passed");
    }
    Ok(value)
}

/// `SO_TYPE` first, then `SO_ACCEPTCONN`.
///
/// The order is not taste. A datagram socket can never be listening, so
/// checking `SO_ACCEPTCONN` first would report "not listening" for a
/// `ListenDatagram=` unit and hide the real mistake. The port is deliberately
/// not checked: `sd_listen_fds(3)` asks for checks "as loose as possible
/// without allowing incorrect setups", and says the port "matters little".
pub(crate) fn check_listening_stream(fd: RawFd) -> Result<()> {
    let ty = getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_TYPE)?;
    if ty != libc::SOCK_STREAM {
        bail!(
            "the descriptor systemd passed is socket type {ty}, not SOCK_STREAM ({want}); the socket unit needs ListenStream=, not ListenDatagram=",
            want = libc::SOCK_STREAM
        );
    }
    if getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN)? == 0 {
        bail!(
            "the descriptor systemd passed is not a listening socket (SO_ACCEPTCONN is 0); Accept=no together with ListenStream= is what makes it one"
        );
    }
    Ok(())
}

/// The address family, read from the descriptor rather than guessed from the
/// unit file.
fn socket_family(fd: RawFd) -> Result<libc::c_int> {
    // SAFETY: `sockaddr_storage` is plain data and zero is a valid pattern.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `storage` is large enough for any family and `len` says so.
    let rc = unsafe {
        libc::getsockname(
            fd,
            (&raw mut storage).cast::<libc::sockaddr>(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("getsockname on the descriptor systemd passed");
    }
    Ok(libc::c_int::from(storage.ss_family))
}

/// One `fcntl` that reads or writes a flag word.
fn fcntl(fd: RawFd, cmd: libc::c_int, arg: libc::c_int) -> Result<libc::c_int> {
    // SAFETY: `cmd` is one of F_GETFD / F_SETFD / F_GETFL / F_SETFL, each of
    // which takes an `int` argument or ignores it.
    let rc = unsafe { libc::fcntl(fd, cmd, arg) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error())
            .context("fcntl on the descriptor systemd passed");
    }
    Ok(rc)
}

/// `FD_CLOEXEC` so an exec does not leak the socket, `O_NONBLOCK` so tokio can
/// drive it. libsystemd sets the first for its callers; there is none here.
pub(crate) fn prepare_fd(fd: RawFd) -> Result<()> {
    let flags = fcntl(fd, libc::F_GETFD, 0)?;
    fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC)?;
    let status = fcntl(fd, libc::F_GETFL, 0)?;
    fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK)?;
    Ok(())
}

/// Descriptor 3 is adopted once per process.
///
/// Two listeners over one descriptor would close it twice, and the second
/// close can land on a descriptor the runtime has since handed to something
/// else. This is also what replaces unsetting `LISTEN_FDS`: the variables stay
/// in place (removing them is `unsafe` in edition 2024 and undefined behaviour
/// while another thread reads the environment), and the guarantee they were
/// going to buy is bought here instead.
///
/// The cell is a parameter so a test can measure the second call without
/// consuming the process-wide one.
pub(crate) fn claim(cell: &OnceLock<()>) -> Result<()> {
    cell.set(()).map_err(|()| {
        anyhow::anyhow!(
            "the socket systemd passed has already been adopted; groove takes it once per process"
        )
    })
}

static TAKEN: OnceLock<()> = OnceLock::new();

/// Check a descriptor and take ownership of it. Reads no environment.
///
/// The [`OwnedFd`] is the contract, not decoration. A caller cannot hand over
/// a descriptor it merely borrowed, which is the mistake that ends with two
/// owners closing one descriptor and the second `close(2)` landing on whatever
/// the runtime handed out in between. Every refusal below drops the `OwnedFd`
/// and so closes the descriptor exactly once, which is what lets a caller
/// recover from the error rather than leak it; on success it moves into the
/// listener and is closed there instead.
///
/// Both conversions are the safe `From<OwnedFd>` impls, so this function has no
/// `unsafe` at all. The one place the module builds an `OwnedFd` out of a raw
/// number is [`take_listener`], which runs `check_listening_stream` on the raw
/// number *before* it does — so that check runs twice per start. The
/// duplication is deliberate: this function has to hold for any caller on its
/// own, and two extra `getsockopt` calls once at start-up cost less than a
/// contract only the caller who remembers to pre-check satisfies.
pub(crate) fn adopt(fd: OwnedFd) -> Result<SystemdSocket> {
    check_listening_stream(fd.as_raw_fd())?;
    prepare_fd(fd.as_raw_fd())?;
    match socket_family(fd.as_raw_fd())? {
        libc::AF_UNIX => Ok(SystemdSocket::Unix(fd.into())),
        libc::AF_INET | libc::AF_INET6 => Ok(SystemdSocket::Tcp(fd.into())),
        other => bail!(
            "the socket systemd passed has address family {other}, which groove cannot serve; ListenStream= must name a filesystem path or an address and port"
        ),
    }
}

/// Claim, read the environment, adopt descriptor 3.
///
/// `shutdown(2)` is never called on the result and its path is never
/// unlinked: the service manager keeps its own copy of the descriptor, and
/// `systemd.socket(5)` says a service "must not unlink the socket from a file
/// system".
///
/// The claim is taken before the environment is read, so a *second* call
/// reports the double adoption rather than whatever `LISTEN_PID` says. The
/// first error is the real reason; there is nothing here to retry.
pub(crate) fn take_listener() -> Result<SystemdSocket> {
    claim(&TAKEN)?;
    check_listen_env(
        std::env::var("LISTEN_PID").ok().as_deref(),
        std::env::var("LISTEN_FDS").ok().as_deref(),
        std::process::id(),
    )?;
    // Refuse on the raw number, before anything here claims to own it.
    //
    // Up to this point the only thing saying descriptor 3 is ours is an
    // environment variable, and `LISTEN_PID=$$ LISTEN_FDS=1 groove serve
    // --systemd-socket` typed at a shell satisfies every check above while
    // descriptor 3 is whatever groove itself opened third. Refusing after the
    // `OwnedFd` exists would close that descriptor on the way out -- the
    // double close this module spent two commits removing, arriving through
    // the refusal path instead. `check_listening_stream` only reads the
    // descriptor through `getsockopt`, so it needs no ownership to run.
    check_listening_stream(LISTEN_FDS_START)?;
    // SAFETY: two separate facts, and neither stands in for the other.
    //
    // `claim` succeeded, so this is the first call in this process to reach
    // here and no `SystemdSocket` from an earlier one holds descriptor 3.
    // That, and only that, is what `claim` says.
    //
    // What says the descriptor is ours to take is the protocol together with
    // the two checks: `check_listen_env` confirmed `LISTEN_PID` names this
    // process rather than an ancestor whose environment we inherited, and
    // `check_listening_stream` confirmed descriptor 3 is a listening stream
    // socket rather than a file, a pipe, a datagram socket or a connected one.
    // A service manager following `sd_listen_fds(3)` passes exactly that and
    // hands this process no other user of it. The checks are as loose as that
    // page asks for, so what they cannot exclude is a listening stream socket
    // this process opened itself at descriptor 3 -- which is why they run
    // before the `OwnedFd` rather than after, leaving every shape they *can*
    // name to be refused without touching the descriptor.
    //
    // This is the only place in the module that builds an `OwnedFd` out of a
    // raw number.
    let fd = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START) };
    adopt(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// (試験 A-3) The check `sd_listen_fds(3)` makes first. A `LISTEN_PID`
    /// inherited from a parent names a process that is not us, and the
    /// descriptors it describes belong to that process -- adopting them is
    /// how a daemon ends up serving on somebody else's socket.
    #[test]
    fn a_listen_pid_that_is_not_our_own_is_refused_naming_both() {
        let err = check_listen_env(Some("999999"), Some("1"), 4242)
            .expect_err("LISTEN_PID from a parent must not be honoured");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("999999"),
            "the value read must be named: {msg}"
        );
        assert!(msg.contains("4242"), "our own pid must be named too: {msg}");
        assert!(msg.is_ascii(), "diagnostics stay ASCII: {msg}");
    }

    /// (試験 A-3) Absent and unparseable are separate refusals, and neither
    /// may fall through to "assume it is ours".
    #[test]
    fn a_missing_or_unreadable_listen_pid_is_refused() {
        for value in [None, Some("not-a-number"), Some("")] {
            let err = check_listen_env(value, Some("1"), 4242)
                .expect_err("an unusable LISTEN_PID is not an invitation")
                .to_string();
            assert!(err.contains("LISTEN_PID"), "must name the variable: {err}");
        }
    }

    /// (試験 A-4) Exactly one, because this design gives each socket unit one
    /// `ListenStream=`. Zero, several and "not a number" are all refusals,
    /// and each one names the count it read.
    #[test]
    fn listen_fds_must_be_exactly_one_and_the_refusal_names_what_it_read() {
        for value in ["0", "2", "7"] {
            let err = check_listen_env(Some("4242"), Some(value), 4242)
                .expect_err("only one descriptor is expected")
                .to_string();
            assert!(err.contains(value), "the count read must appear: {err}");
            assert!(err.contains("LISTEN_FDS"), "must name the variable: {err}");
        }
        for value in [None, Some("x")] {
            check_listen_env(Some("4242"), value, 4242)
                .expect_err("an unreadable LISTEN_FDS is not one descriptor");
        }
        check_listen_env(Some("4242"), Some("1"), 4242)
            .expect("one descriptor addressed to us is the whole point");
    }

    /// (試験 A-5) A socket nobody called `listen(2)` on cannot accept, and
    /// `SO_ACCEPTCONN` is how `sd_listen_fds(3)` says to notice.
    #[test]
    fn a_socket_that_is_not_listening_is_refused() {
        // SAFETY: an ordinary AF_INET stream socket; `OwnedFd` closes it.
        let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(
            raw >= 0,
            "socket(2) failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `raw` is a fresh descriptor this scope owns.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let err = check_listening_stream(owned.as_raw_fd())
            .expect_err("a socket without listen(2) is not a listener")
            .to_string();
        assert!(
            err.contains("SO_ACCEPTCONN"),
            "must say which check failed: {err}"
        );
    }

    /// (試験 A-6) `sd_listen_fds(3)` calls the datagram/stream distinction the
    /// one that "matters a lot". The order of the two checks is what makes
    /// this reachable: a datagram socket can never be listening, so checking
    /// `SO_ACCEPTCONN` first would report the wrong thing (plan decision 4).
    #[test]
    fn a_datagram_socket_is_refused_for_its_type_not_for_not_listening() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a UDP socket");
        let err = check_listening_stream(udp.as_raw_fd())
            .expect_err("ListenDatagram= is not what this server serves")
            .to_string();
        assert!(
            err.contains("SOCK_STREAM"),
            "must name the type wanted: {err}"
        );
        assert!(
            !err.contains("SO_ACCEPTCONN"),
            "the type check must come first, or A-6 cannot be told from A-5: {err}"
        );
    }

    /// (N-I1) The shape the pre-check in [`take_listener`] exists for: the
    /// descriptor is not a socket at all, because `LISTEN_FDS` was set by hand
    /// and descriptor 3 is whatever this process opened third -- a database
    /// handle, a log file. `getsockopt` answers `ENOTSOCK`, and the refusal has
    /// to be reachable while the descriptor is still a borrowed number, or the
    /// refusal closes a file somebody else is using.
    #[test]
    fn a_descriptor_that_is_not_a_socket_is_refused() {
        let dir = crate::test_support::unique_temp_path("g");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("f");
        let file = std::fs::File::create(&path).expect("a plain file");
        let err = check_listening_stream(file.as_raw_fd())
            .expect_err("a regular file is not a listening socket")
            .to_string();
        assert!(
            err.contains("getsockopt"),
            "must say which call failed: {err}"
        );
        assert!(err.is_ascii(), "diagnostics stay ASCII: {err}");
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shape that passes, so the two refusals above are not passing for
    /// some reason of their own.
    #[test]
    fn a_listening_stream_socket_passes() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a TCP listener");
        check_listening_stream(l.as_raw_fd()).expect("this is exactly the shape wanted");
    }

    /// (試験 A-11) libsystemd sets `FD_CLOEXEC` for the caller; there is no
    /// libsystemd here, so it is set explicitly. `O_NONBLOCK` is what tokio
    /// needs, and `systemd-socket-proxyd` sets it the same way right after it
    /// receives its descriptor.
    #[test]
    fn the_adopted_descriptor_is_cloexec_and_nonblocking() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a TCP listener");
        let fd = l.as_raw_fd();
        prepare_fd(fd).expect("setting the two flags must succeed");
        // SAFETY: reading the flags of a descriptor this scope owns.
        let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        // SAFETY: as above.
        let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(fd_flags >= 0 && status >= 0, "fcntl failed");
        assert_ne!(fd_flags & libc::FD_CLOEXEC, 0, "FD_CLOEXEC must be set");
        assert_ne!(status & libc::O_NONBLOCK, 0, "O_NONBLOCK must be set");
    }

    /// (試験 A-12) Two listeners over one descriptor would close it twice. The
    /// cell is a parameter so this can be measured without consuming the
    /// process-wide one (plan decision 5).
    #[test]
    fn the_descriptor_can_only_be_claimed_once() {
        let cell = std::sync::OnceLock::new();
        claim(&cell).expect("the first claim is the real one");
        let err = claim(&cell)
            .expect_err("a second adoption would close the descriptor twice")
            .to_string();
        assert!(err.is_ascii(), "diagnostics stay ASCII: {err}");
    }

    /// A refusal from inside [`adopt`] consumes the descriptor it was handed:
    /// the `OwnedFd` moved in, and dropping it on the way out is what closes
    /// it exactly once. The closing itself belongs to the type, so what this
    /// pins is the half that is this module's -- that `adopt` really does take
    /// ownership on the refusing path too, which a `RawFd` parameter left to a
    /// doc comment and to whoever wrote the caller.
    #[test]
    fn adopt_refuses_a_datagram_socket_and_consumes_the_descriptor() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a UDP socket");
        let err = adopt(OwnedFd::from(udp))
            .expect_err("ListenDatagram= is not what this server serves")
            .to_string();
        assert!(
            err.contains("SOCK_STREAM"),
            "must name the type wanted: {err}"
        );
        assert!(err.is_ascii(), "diagnostics stay ASCII: {err}");
    }

    /// The family gate. `AF_INET` and `AF_UNIX` are both served; anything else
    /// is refused by name, because `ListenStream=` can say things this server
    /// has no listener for.
    #[test]
    fn adopt_reads_the_family_from_the_descriptor() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind TCP");
        assert!(matches!(
            adopt(OwnedFd::from(tcp)).expect("an AF_INET listener"),
            SystemdSocket::Tcp(_)
        ));

        // Both names are as short as they can be, because an `AF_UNIX` address
        // is bounded by `sun_path` -- 108 bytes on Linux but 104 on macOS --
        // and `unique_temp_path` already spends a pid, a nanosecond timestamp
        // and a counter on top of `$TMPDIR`, which on a macOS runner is itself
        // a `/var/folders/...` path. The assert is here so the tight runner
        // says which limit it hit instead of failing inside `bind` as an
        // opaque "invalid argument".
        let dir = crate::test_support::unique_temp_path("g");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("s");
        // SAFETY: `sockaddr_un` is plain data and zero is a valid pattern; this
        // reads the length of its `sun_path` array for the target we are on.
        let sun: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        assert!(
            path.as_os_str().len() < sun.sun_path.len(),
            "the socket path must fit sun_path ({} bytes) with room for the NUL, but it is {} bytes: {}",
            sun.sun_path.len(),
            path.as_os_str().len(),
            path.display()
        );
        let ux = std::os::unix::net::UnixListener::bind(&path).expect("bind AF_UNIX");
        assert!(matches!(
            adopt(OwnedFd::from(ux)).expect("an AF_UNIX listener"),
            SystemdSocket::Unix(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
