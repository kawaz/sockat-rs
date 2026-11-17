//! The `unsafe` boundary. Every libc call that needs `unsafe` lives in this file
//! (`just lint-unsafe` fails if `unsafe` appears anywhere else), and every
//! function exported from here is safe to call.
//!
//! Contents:
//!
//! * [`SockAddr`] — a `sockaddr_un` built by hand. On Apple targets it can also
//!   hold the "widened" form whose `sun_path` runs past the 104 bytes of
//!   `struct sockaddr_un`, up to `SOCK_MAXADDRLEN` (255) bytes in total.
//! * [`socket`] / [`bind`] / [`connect`] / [`listen`] — a stream socket set up the
//!   same way std sets up the sockets behind `UnixListener` / `UnixStream`
//!   (optionally `O_NONBLOCK`, which std cannot connect with).
//! * [`op_in_dir_forked`] — bind / connect relative to a directory fd in a forked
//!   child that `fchdir`s (the last resort on Apple targets, on Linux without
//!   /proc, and on Unix targets without a dirfd-relative primitive); returns the
//!   child's pid.
//! * `op_at` — `bindat(2)` / `connectat(2)` (FreeBSD).

// FreeBSD reaches long paths through bindat / connectat, so the fork route is
// used there only by the tests.
#![cfg_attr(all(not(test), target_os = "freebsd"), allow(dead_code))]

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

/// What to do with a socket and an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// `bind(2)` followed by `listen(2)`.
    Bind,
    /// `connect(2)`.
    Connect,
}

/// Offset of `sun_path` in `struct sockaddr_un` (2 on every supported target).
const SUN_PATH_OFFSET: usize = std::mem::offset_of!(libc::sockaddr_un, sun_path);

/// Size of the `sun_path` field of `struct sockaddr_un` (104 on BSDs and Apple,
/// 108 on Linux). A path fits the standard form when it is shorter than this,
/// leaving room for the terminating NUL as std does.
pub(crate) const SUN_PATH_CAP: usize = std::mem::size_of::<libc::sockaddr_un>() - SUN_PATH_OFFSET;

/// Longest path the widened `sockaddr_un` carries on Apple targets.
///
/// XNU accepts a socket address of up to `SOCK_MAXADDRLEN` (255) bytes in
/// `bind(2)` / `connect(2)` (`getsockaddr` refuses longer ones) and takes
/// `sun_len - offsetof(sun_path)` bytes as the path, requiring that to be less
/// than `SOCK_MAXADDRLEN` (`unp_bind` / `unp_connect`). With the length byte and
/// the family byte in front, 255 - 2 = 253 bytes of path fit, without a NUL
/// (the kernel terminates its own copy). libuv uses the same layout (#5047).
#[cfg(target_vendor = "apple")]
pub(crate) const WIDE_PATH_MAX: usize = libc::SOCK_MAXADDRLEN as usize - SUN_PATH_OFFSET;

/// Bytes of storage for [`SockAddr`]: the widest address any target here builds.
const ADDR_STORAGE: usize = 256;

/// A Unix domain socket address built for `bind(2)` / `connect(2)`.
///
/// It is plain bytes so the forked child of [`op_in_dir_forked`] can pass it to
/// the kernel without touching the allocator.
#[repr(C, align(8))]
pub(crate) struct SockAddr {
    storage: [u8; ADDR_STORAGE],
    len: libc::socklen_t,
}

impl SockAddr {
    /// The standard form: `path` must be shorter than [`SUN_PATH_CAP`], and the
    /// address carries a terminating NUL (the same bytes std's `sockaddr_un`
    /// builds).
    ///
    /// # Errors
    ///
    /// `InvalidInput` when `path` is empty or contains a NUL byte,
    /// `ENAMETOOLONG` when it does not fit.
    pub(crate) fn standard(path: &OsStr) -> io::Result<Self> {
        let bytes = check_bytes(path)?;
        if bytes.len() >= SUN_PATH_CAP {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        Ok(Self::encode(bytes, SUN_PATH_OFFSET + bytes.len() + 1))
    }

    /// The widened form (Apple only): up to [`WIDE_PATH_MAX`] bytes of path,
    /// no terminating NUL, `sun_len` and the address length both set to
    /// `2 + path length`.
    ///
    /// # Errors
    ///
    /// `InvalidInput` when `path` is empty or contains a NUL byte,
    /// `ENAMETOOLONG` when it is longer than [`WIDE_PATH_MAX`].
    #[cfg(target_vendor = "apple")]
    pub(crate) fn wide(path: &OsStr) -> io::Result<Self> {
        let bytes = check_bytes(path)?;
        if bytes.len() > WIDE_PATH_MAX {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        Ok(Self::encode(bytes, SUN_PATH_OFFSET + bytes.len()))
    }

    fn encode(path: &[u8], len: usize) -> Self {
        // The header is taken from a `sockaddr_un` built through its own fields
        // rather than by guessing byte offsets (Linux has a 2-byte `sun_family`
        // and no `sun_len`; the BSDs have a 1-byte `sun_len` then a 1-byte
        // family).
        // SAFETY: `sockaddr_un` is plain old data; zeroed is a valid value.
        let mut un: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        un.sun_family = libc::AF_UNIX as libc::sa_family_t;
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        {
            // At most 2 + WIDE_PATH_MAX = 255 on Apple and 2 + 104 on FreeBSD.
            un.sun_len = len as u8;
        }
        // SAFETY: reads the first `offsetof(sun_path)` bytes of a fully
        // initialised `sockaddr_un`, which is shorter than the struct.
        let header =
            unsafe { std::slice::from_raw_parts((&raw const un).cast::<u8>(), SUN_PATH_OFFSET) };
        let mut storage = [0u8; ADDR_STORAGE];
        storage[..SUN_PATH_OFFSET].copy_from_slice(header);
        storage[SUN_PATH_OFFSET..SUN_PATH_OFFSET + path.len()].copy_from_slice(path);
        Self {
            storage,
            len: len as libc::socklen_t,
        }
    }

    fn as_ptr(&self) -> *const libc::sockaddr {
        self.storage.as_ptr().cast()
    }
}

fn check_bytes(path: &OsStr) -> io::Result<&[u8]> {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is empty",
        ));
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path must not contain a NUL byte",
        ));
    }
    Ok(bytes)
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// A new `AF_UNIX` / `SOCK_STREAM` socket, set up like the ones std creates:
/// close-on-exec everywhere, and `SO_NOSIGPIPE` on Apple targets (std sets it
/// there so a write to a closed peer returns `EPIPE` instead of raising
/// SIGPIPE). `nonblocking` makes it `O_NONBLOCK` as well.
pub(crate) fn socket(nonblocking: bool) -> io::Result<OwnedFd> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        let nonblock = if nonblocking { libc::SOCK_NONBLOCK } else { 0 };
        // SAFETY: plain syscall; the returned fd is owned by the OwnedFd below.
        let fd = cvt(unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | nonblock,
                0,
            )
        })?;
        // SAFETY: `fd` is a fresh fd nobody else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    {
        // SAFETY: plain syscall; the returned fd is owned by the OwnedFd below.
        let fd = cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) })?;
        // SAFETY: `fd` is a fresh fd nobody else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: fcntl on an fd we own.
        cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        if nonblocking {
            // SAFETY: fcntl on an fd we own.
            let flags = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
            // SAFETY: fcntl on an fd we own.
            cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
        }
        #[cfg(target_vendor = "apple")]
        {
            let one: libc::c_int = 1;
            // SAFETY: setsockopt with a pointer to a live c_int and its size.
            cvt(unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&raw const one).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            })?;
        }
        Ok(fd)
    }
}

/// `bind(2)`.
pub(crate) fn bind(sock: BorrowedFd<'_>, addr: &SockAddr) -> io::Result<()> {
    // SAFETY: `addr` points to `addr.len` initialised bytes for the call.
    cvt(unsafe { libc::bind(sock.as_raw_fd(), addr.as_ptr(), addr.len) }).map(drop)
}

/// `connect(2)`.
///
/// Like std's `UnixStream::connect`, an `EINTR` is returned as is rather than
/// retried (retrying a connect that was interrupted is not portable).
pub(crate) fn connect(sock: BorrowedFd<'_>, addr: &SockAddr) -> io::Result<()> {
    // SAFETY: `addr` points to `addr.len` initialised bytes for the call.
    cvt(unsafe { libc::connect(sock.as_raw_fd(), addr.as_ptr(), addr.len) }).map(drop)
}

/// `listen(2)` with the backlog std's `UnixListener::bind` uses on the target.
pub(crate) fn listen(sock: BorrowedFd<'_>) -> io::Result<()> {
    // std passes -1 ("the system maximum") where the kernel caps it, and
    // SOMAXCONN elsewhere.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_vendor = "apple"
    ))]
    const BACKLOG: libc::c_int = -1;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_vendor = "apple"
    )))]
    const BACKLOG: libc::c_int = libc::SOMAXCONN;
    // SAFETY: plain syscall on a borrowed fd.
    cvt(unsafe { libc::listen(sock.as_raw_fd(), BACKLOG) }).map(drop)
}

/// `bind(2)` or `connect(2)` on `sock` with `addr`.
pub(crate) fn op(op: Op, sock: BorrowedFd<'_>, addr: &SockAddr) -> io::Result<()> {
    match op {
        Op::Bind => bind(sock, addr),
        Op::Connect => connect(sock, addr),
    }
}

/// Whether `sock` is bound to a pathname (`getsockname(2)` returns a non-empty
/// `sun_path`).
fn is_bound(sock: BorrowedFd<'_>) -> io::Result<bool> {
    let mut storage = [0u8; ADDR_STORAGE];
    let mut len = ADDR_STORAGE as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes into `storage`.
    cvt(unsafe { libc::getsockname(sock.as_raw_fd(), storage.as_mut_ptr().cast(), &mut len) })?;
    Ok(len as usize > SUN_PATH_OFFSET && storage[SUN_PATH_OFFSET] != 0)
}

/// Whether `sock` is connected (`getpeername(2)` succeeds).
fn is_connected(sock: BorrowedFd<'_>) -> io::Result<bool> {
    let mut storage = [0u8; ADDR_STORAGE];
    let mut len = ADDR_STORAGE as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes into `storage`.
    let rc = unsafe { libc::getpeername(sock.as_raw_fd(), storage.as_mut_ptr().cast(), &mut len) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENOTCONN) {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Bind or connect `sock` to `addr` resolved relative to the directory `dir`,
/// by forking a child that `fchdir`s into `dir` and does the call there.
///
/// After `fork(2)` the parent's and the child's `sock` refer to the same open
/// file description, so the child's `bind(2)` / `connect(2)` takes effect on
/// the parent's socket; nothing has to be passed back but the result. Only the
/// child changes its working directory, so no thread of the caller ever sees a
/// different cwd.
///
/// The child runs nothing but `fchdir(2)`, `bind(2)` / `connect(2)`, a read of
/// `errno`, and `_exit(2)`: all async-signal-safe, no allocation, no locks, no
/// Rust destructors. `addr` is built by the parent before the fork. This keeps
/// the call within what POSIX allows after `fork(2)` in a multi-threaded
/// process. The child reports `0` for success or the `errno` (clamped to
/// 1..=255) as its exit status.
///
/// The parent waits for that pid only (it never reaps other children). If the
/// status cannot be collected because something else reaped the child first
/// (`SIGCHLD` set to `SIG_IGN`, or a `waitpid(-1)` elsewhere in the process;
/// `waitpid` then fails with `ECHILD`), the outcome is read off the socket
/// itself: bound / connected means success.
///
/// On success it returns the child's pid: the process that made the call, and
/// so the one a server's peer-credential query names for a connect (Linux's
/// `SO_PEERCRED` for good, macOS's `LOCAL_PEERPID` until this process sends).
/// The child has exited by then (or was reaped elsewhere); the pid is a record
/// of who connected, not a process to signal.
pub(crate) fn op_in_dir_forked(
    op: Op,
    sock: BorrowedFd<'_>,
    dir: BorrowedFd<'_>,
    addr: &SockAddr,
) -> io::Result<libc::pid_t> {
    let sock_raw = sock.as_raw_fd();
    let dir_raw = dir.as_raw_fd();
    let addr_ptr = addr.as_ptr();
    let addr_len = addr.len;
    // SAFETY: fork(2) itself can always be called. The child below only makes
    // async-signal-safe calls and `_exit`s, so it never touches a lock or the
    // allocator state the parent's other threads held at the moment of the fork.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Child.
        // SAFETY: fchdir / bind / connect / _exit are async-signal-safe; the fd
        // numbers and the address bytes were prepared by the parent before the
        // fork and are only read here. `last_os_error` only reads errno (no
        // allocation: the Os variant of io::Error is a plain integer).
        unsafe {
            if libc::fchdir(dir_raw) != 0 {
                libc::_exit(errno_status());
            }
            let rc = match op {
                Op::Bind => libc::bind(sock_raw, addr_ptr, addr_len),
                Op::Connect => libc::connect(sock_raw, addr_ptr, addr_len),
            };
            if rc != 0 {
                libc::_exit(errno_status());
            }
            libc::_exit(0);
        }
    }
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waits for the child we forked; `status` is a local.
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r == pid {
            break;
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return outcome_from_socket(op, sock).map(|()| pid),
            _ => return Err(err),
        }
    }
    if !libc::WIFEXITED(status) {
        return Err(io::Error::other(
            "the child process that binds / connects the socket did not exit normally",
        ));
    }
    match libc::WEXITSTATUS(status) {
        0 => Ok(pid),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

fn errno_status() -> libc::c_int {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(255)
        .clamp(1, 255)
}

fn outcome_from_socket(op: Op, sock: BorrowedFd<'_>) -> io::Result<()> {
    let done = match op {
        Op::Bind => is_bound(sock)?,
        Op::Connect => is_connected(sock)?,
    };
    if done {
        Ok(())
    } else {
        Err(io::Error::other(
            "the child process that binds / connects the socket was reaped elsewhere \
             (SIGCHLD ignored, or waitpid(-1) in another thread) and the socket is not \
             bound / connected",
        ))
    }
}

/// `bindat(2)` / `connectat(2)` with `addr` resolved relative to `dir`
/// (FreeBSD 10+).
#[cfg(target_os = "freebsd")]
pub(crate) fn op_at(
    op: Op,
    sock: BorrowedFd<'_>,
    dir: BorrowedFd<'_>,
    addr: &SockAddr,
) -> io::Result<()> {
    // The libc crate does not declare these; the prototypes are those of
    // FreeBSD's <sys/socket.h>.
    unsafe extern "C" {
        fn bindat(
            fd: libc::c_int,
            s: libc::c_int,
            addr: *const libc::sockaddr,
            addrlen: libc::socklen_t,
        ) -> libc::c_int;
        fn connectat(
            fd: libc::c_int,
            s: libc::c_int,
            name: *const libc::sockaddr,
            namelen: libc::socklen_t,
        ) -> libc::c_int;
    }
    // SAFETY: `addr` points to `addr.len` initialised bytes for the call; the
    // fds are borrowed and live.
    let rc = unsafe {
        match op {
            Op::Bind => bindat(dir.as_raw_fd(), sock.as_raw_fd(), addr.as_ptr(), addr.len),
            Op::Connect => connectat(dir.as_raw_fd(), sock.as_raw_fd(), addr.as_ptr(), addr.len),
        }
    };
    cvt(rc).map(drop)
}

/// Fill `buf` with random bytes from the kernel (`arc4random_buf(3)`, which
/// never fails and never blocks).
#[cfg(target_vendor = "apple")]
pub(crate) fn random_bytes(buf: &mut [u8]) {
    // SAFETY: writes exactly `buf.len()` bytes into `buf`.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) }
}

/// `LOCAL_PEERPID` of a connected socket (Apple). Test-only: used to record
/// which process the server sees as the peer.
#[cfg(all(test, target_vendor = "apple"))]
pub(crate) fn local_peer_pid(sock: BorrowedFd<'_>) -> io::Result<libc::pid_t> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `pid`.
    cvt(unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    })?;
    Ok(pid)
}

/// `SO_PEERCRED` pid of a connected socket (Linux). Test-only.
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
pub(crate) fn peer_cred_pid(sock: BorrowedFd<'_>) -> io::Result<libc::pid_t> {
    // SAFETY: `ucred` is plain old data; zeroed is a valid value.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`.
    cvt(unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    })?;
    Ok(cred.pid)
}

/// Set `SIGCHLD` to `SIG_IGN`, which makes the kernel reap children on its own
/// (`waitpid` then fails with `ECHILD`). Test-only, and only for a child
/// process of the test binary: the disposition is process-wide.
#[cfg(test)]
pub(crate) fn ignore_sigchld() {
    // SAFETY: installs SIG_IGN, no handler code runs.
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
    }
}

/// Whether `fd` is `O_NONBLOCK`. Test-only (the connect option tests run on
/// Linux and Apple).
#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_vendor = "apple")
))]
pub(crate) fn is_nonblocking(fd: BorrowedFd<'_>) -> io::Result<bool> {
    // SAFETY: fcntl F_GETFL on a borrowed fd.
    let flags = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
    Ok(flags & libc::O_NONBLOCK != 0)
}

/// `listen(2)` again with `backlog`, which both Linux and XNU apply to a
/// socket that is already listening. Test-only: shrinks the backlog of a
/// listener so it can be filled.
#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_vendor = "apple")
))]
pub(crate) fn relisten(sock: BorrowedFd<'_>, backlog: libc::c_int) -> io::Result<()> {
    // SAFETY: plain syscall on a borrowed fd.
    cvt(unsafe { libc::listen(sock.as_raw_fd(), backlog) }).map(drop)
}
