//! The `unsafe` boundary. Every libc call that needs `unsafe` lives in this file
//! (`just lint-unsafe` fails if `unsafe` appears anywhere else), and every
//! function exported from here is safe to call.
//!
//! Contents:
//!
//! * [`SockAddr`] — a `sockaddr_un` built by hand. On Apple targets it can also
//!   hold the "widened" form whose `sun_path` runs past the 104 bytes of
//!   `struct sockaddr_un`, up to `SOCK_MAXADDRLEN` (255) bytes in total.
//! * [`socket`] / [`bind`] / [`connect`] / [`listen`] — a stream or datagram
//!   socket set up the same way std sets up the sockets behind `UnixListener`
//!   / `UnixStream` / `UnixDatagram` (optionally `O_NONBLOCK`, which std
//!   cannot connect with).
//! * `op_in_dir_on_thread` — bind / connect relative to a directory fd on a
//!   thread of its own whose working directory alone is moved there (Linux:
//!   `unshare(CLONE_FS)` and `fchdir`; Apple: `pthread_fchdir_np`).
//! * `op_at` — `bindat(2)` / `connectat(2)` (FreeBSD).
//! * [`mkdir_at`] / [`open_dir_at`] / [`id_at`] / [`stat_at`] / [`chmod_at`] /
//!   [`symlink_at`] / [`unlink_at`] — the `*at(2)` calls that make, check and
//!   clean up the directories B and D and the symlinks E, through directory
//!   fds rather than names.
//! * [`lock`] — `flock(2)` on D, which keeps it alive while the process
//!   holds it.
//! * `random_bytes` — the random part of the names sockat creates.
//! * `effective_uid` / [`process_id`] — the owner B and D must have, and the
//!   pid in a `/proc/<pid>/fd/<fd>` spelling.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

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

/// Length of the `sun_path` array of `struct sockaddr_un` (104 on BSDs and
/// Apple, 108 on Linux and illumos). A path fits the standard form when it is
/// shorter than this, leaving room for the terminating NUL: the same test std
/// makes (`bytes.len() >= addr.sun_path.len()` is refused).
pub(crate) const SUN_PATH_CAP: usize = sun_path_len(|un| &un.sun_path);

/// The `N` of the array that `field` picks out of a `sockaddr_un`, read off its
/// type at compile time.
const fn sun_path_len<const N: usize>(
    _field: fn(&libc::sockaddr_un) -> &[libc::c_char; N],
) -> usize {
    N
}

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

/// The type of a socket: `SOCK_STREAM` or `SOCK_DGRAM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SockType {
    /// `SOCK_STREAM` (std's `UnixListener` / `UnixStream`).
    Stream,
    /// `SOCK_DGRAM` (std's `UnixDatagram`).
    Datagram,
}

impl SockType {
    fn raw(self) -> libc::c_int {
        match self {
            SockType::Stream => libc::SOCK_STREAM,
            SockType::Datagram => libc::SOCK_DGRAM,
        }
    }
}

/// A new `AF_UNIX` socket of `ty`, set up like the ones std creates:
/// close-on-exec everywhere, and `SO_NOSIGPIPE` where std sets it (see
/// [`no_sigpipe`]). `nonblocking` makes it `O_NONBLOCK` as well.
pub(crate) fn socket(ty: SockType, nonblocking: bool) -> io::Result<OwnedFd> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        let nonblock = if nonblocking { libc::SOCK_NONBLOCK } else { 0 };
        // SAFETY: plain syscall; the returned fd is owned by the OwnedFd below.
        let fd = cvt(unsafe {
            libc::socket(libc::AF_UNIX, ty.raw() | libc::SOCK_CLOEXEC | nonblock, 0)
        })?;
        // SAFETY: `fd` is a fresh fd nobody else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        no_sigpipe(fd.as_fd())?;
        Ok(fd)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    {
        // SAFETY: plain syscall; the returned fd is owned by the OwnedFd below.
        let fd = cvt(unsafe { libc::socket(libc::AF_UNIX, ty.raw(), 0) })?;
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
        no_sigpipe(fd.as_fd())?;
        Ok(fd)
    }
}

/// `SO_NOSIGPIPE` where std sets it on its Unix sockets (Apple, FreeBSD,
/// NetBSD, DragonFly), so a write to a closed peer returns `EPIPE` instead of
/// raising SIGPIPE, as with std's sockets. Nothing elsewhere.
fn no_sigpipe(sock: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        let one: libc::c_int = 1;
        // SAFETY: setsockopt with a pointer to a live c_int and its size.
        cvt(unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const one).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        })?;
    }
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    let _ = sock;
    Ok(())
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
        target_os = "freebsd",
        target_os = "openbsd",
        target_vendor = "apple"
    ))]
    const BACKLOG: libc::c_int = -1;
    #[cfg(not(any(
        target_os = "linux",
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

/// What came of a call that first has to move a thread's working directory
/// ([`op_in_dir_on_thread`]).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[derive(Debug)]
pub(crate) enum OnThread<T> {
    /// The thread's working directory was moved and the call was made; this
    /// is its outcome.
    Done(io::Result<T>),
    /// The thread's working directory could not be moved on its own, so the
    /// call was not made: no thread could be started, a seccomp filter is in
    /// effect so `unshare(CLONE_FS)` is not called, or it was refused (Linux),
    /// or `pthread_fchdir_np` is missing or failed (Apple). The error says
    /// which.
    Unusable(io::Error),
}

/// Bind or connect `sock` to `addr` resolved relative to the directory `dir`,
/// on a thread started for this call alone whose working directory, and only
/// its own, is moved into `dir`. The thread ends with the call.
///
/// The working directory is otherwise shared by every thread of the process,
/// so it is detached for this thread first:
///
/// * Linux / Android: `unshare(CLONE_FS)` gives the thread its own copy of the
///   working directory (and of the root and the umask), then `fchdir(dir)`
///   moves that copy. Not when a seccomp filter is in effect on the thread
///   ([`seccomp_mode`] not 0, or not readable): a filter may answer `unshare`
///   by killing the process with SIGSYS rather than with an error (systemd's
///   `SystemCallFilter=` does by default), so `unshare` is not called at all
///   and the route counts as unusable. The check is made on the started thread
///   itself, right before the `unshare`, since filters are per thread (a
///   thread inherits its creator's, and one can be added to a single thread).
///   `fchdir` is reached only after the `unshare` succeeded,
///   so a refused `unshare` can never move the cwd of the process. A failing
///   `fchdir` (no search permission, say) is the real outcome, as it would be
///   for a bind / connect through the path;
/// * Apple: `pthread_fchdir_np(dir)` sets a working directory for the calling
///   thread alone (the `__pthread_fchdir` system call). It is not a public API
///   of libSystem, so it is looked up when needed; missing, or failing, it
///   makes this route unusable. The thread ends right
///   after the call, so its working directory is never set back.
///
/// Both leave the working directory of every other thread, and of the
/// process, as it was. The call is made by this process (the peer pid a server
/// sees is this process's), on the caller's own socket, and no fd is copied.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub(crate) fn op_in_dir_on_thread(
    op: Op,
    sock: BorrowedFd<'_>,
    dir: BorrowedFd<'_>,
    addr: &SockAddr,
) -> OnThread<()> {
    std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .spawn_scoped(scope, || op_in_dir_on_this_thread(op, sock, dir, addr))
        {
            Ok(thread) => thread
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            Err(e) => OnThread::Unusable(e),
        }
    })
}

/// The body of [`op_in_dir_on_thread`], run on the thread it starts (and only
/// there: on any other thread it would leave that thread in `dir`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn op_in_dir_on_this_thread(
    op: Op,
    sock: BorrowedFd<'_>,
    dir: BorrowedFd<'_>,
    addr: &SockAddr,
) -> OnThread<()> {
    match seccomp_mode() {
        Ok(0) => {}
        Ok(mode) => {
            return OnThread::Unusable(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "a seccomp filter is in effect (mode {mode}), so unshare(CLONE_FS) is not \
                     called: the filter may kill the process for it"
                ),
            ));
        }
        Err(e) => {
            return OnThread::Unusable(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the seccomp mode cannot be read ({e}), so unshare(CLONE_FS) is not called: \
                     a filter may be in effect"
                ),
            ));
        }
    }
    // SAFETY: plain syscall without pointers; it only detaches this thread's
    // working directory, root and umask from the rest of the process.
    if unsafe { libc::unshare(libc::CLONE_FS) } != 0 {
        return OnThread::Unusable(io::Error::last_os_error());
    }
    // SAFETY: fchdir on a borrowed, live fd. After the unshare above it moves
    // this thread's working directory only.
    if unsafe { libc::fchdir(dir.as_raw_fd()) } != 0 {
        return OnThread::Done(Err(io::Error::last_os_error()));
    }
    OnThread::Done(self::op(op, sock, addr))
}

/// The seccomp mode of the calling thread (`prctl(PR_GET_SECCOMP)`): 0 without
/// seccomp, 2 with a filter (Linux, Android). An error means the mode is not
/// known: a kernel without seccomp answers `EINVAL`, and so does an x86_64
/// binary emulated on an arm64 Linux (docker on Apple silicon, observed under
/// seccomp and without it alike).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn seccomp_mode() -> io::Result<libc::c_int> {
    // SAFETY: PR_GET_SECCOMP takes no further arguments and only reports the
    // calling thread's mode.
    cvt(unsafe { libc::prctl(libc::PR_GET_SECCOMP) })
}

/// The body of [`op_in_dir_on_thread`], run on the thread it starts (and only
/// there: on any other thread it would leave that thread in `dir`).
#[cfg(target_vendor = "apple")]
fn op_in_dir_on_this_thread(
    op: Op,
    sock: BorrowedFd<'_>,
    dir: BorrowedFd<'_>,
    addr: &SockAddr,
) -> OnThread<()> {
    let Some(pthread_fchdir_np) = pthread_fchdir_np() else {
        return OnThread::Unusable(io::Error::new(
            io::ErrorKind::Unsupported,
            "pthread_fchdir_np is not available",
        ));
    };
    // SAFETY: `pthread_fchdir_np` is libSystem's `int pthread_fchdir_np(int)`
    // (see `pthread_fchdir_np` below), called with a borrowed, live fd. It sets
    // the working directory of this thread only.
    if unsafe { pthread_fchdir_np(dir.as_raw_fd()) } != 0 {
        return OnThread::Unusable(io::Error::last_os_error());
    }
    OnThread::Done(self::op(op, sock, addr))
}

/// libSystem's `int pthread_fchdir_np(int fd)`, looked up at run time, or
/// `None` when this libSystem does not have it.
///
/// It is exported by libsystem_pthread (checked on macOS 26.5) but declared in
/// no public header, so it may disappear from a future release; looking it up
/// rather than linking against it turns that into "this route cannot be used"
/// instead of a binary that does not load.
#[cfg(target_vendor = "apple")]
fn pthread_fchdir_np() -> Option<unsafe extern "C" fn(libc::c_int) -> libc::c_int> {
    // SAFETY: dlsym only looks the NUL-terminated name up in the loaded images.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"pthread_fchdir_np".as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: the symbol of that name in libSystem is the function
    // `int pthread_fchdir_np(int fd)` (libpthread's `pthread_fchdir_np`, a thin
    // wrapper of the `__pthread_fchdir` system call returning 0 or -1 with
    // errno set), so the pointer has this type.
    Some(unsafe {
        std::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn(libc::c_int) -> libc::c_int>(
            symbol,
        )
    })
}

/// Whether [`op_in_dir_on_thread`] can be used as far as can be told before
/// anything is made for it: on Linux / Android no seccomp filter on the
/// calling thread ([`seccomp_mode`] reads 0; a thread started from it inherits
/// its filters), on Apple `pthread_fchdir_np` found. What only the started
/// thread can tell (whether it starts, whether `unshare` is refused, whether
/// `pthread_fchdir_np` fails) [`op_in_dir_on_thread`] reports as
/// [`OnThread::Unusable`].
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn thread_cwd_available() -> bool {
    seccomp_mode().ok() == Some(0)
}

/// See the Linux version.
#[cfg(target_vendor = "apple")]
pub(crate) fn thread_cwd_available() -> bool {
    pthread_fchdir_np().is_some()
}

/// Set the working directory of the calling thread alone to `dir`
/// (`pthread_fchdir_np`), Apple. Test-only: stands in for a caller that keeps
/// a working directory of its own on its thread, which sockat must leave as
/// it is. Only for a thread started by the test, which ends with it.
#[cfg(all(test, target_vendor = "apple"))]
pub(crate) fn set_thread_cwd(dir: BorrowedFd<'_>) -> io::Result<()> {
    let f = pthread_fchdir_np().ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
    // SAFETY: see op_in_dir_on_this_thread; `dir` is a borrowed, live fd.
    cvt(unsafe { f(dir.as_raw_fd()) }).map(drop)
}

/// Give the calling thread a working directory of its own
/// (`unshare(CLONE_FS)`), Linux. Test-only, for the same stand-in as the
/// Apple `set_thread_cwd`: after it, `std::env::set_current_dir` moves this
/// thread alone. Only for a thread started by the test, and only where no
/// seccomp filter is in effect (the caller checks [`seccomp_mode`]).
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
pub(crate) fn unshare_fs() -> io::Result<()> {
    // SAFETY: plain syscall without pointers; it only detaches this thread's
    // working directory, root and umask from the rest of the process.
    cvt(unsafe { libc::unshare(libc::CLONE_FS) }).map(drop)
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

/// `name` as a C string for a system call, or `InvalidInput` when it holds a
/// NUL byte (which the call would take as the end of the name).
fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// `mkdirat(2)`: create the directory `name` inside the directory `dir` with
/// `mode` (less the umask, as `mkdir(2)` does). An existing `name` of any kind
/// is `EEXIST`, never reused.
pub(crate) fn mkdir_at(dir: BorrowedFd<'_>, name: &OsStr, mode: u32) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: `name` is NUL-terminated and lives for the call; `dir` is a
    // borrowed, live fd.
    cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) }).map(drop)
}

/// `openat(2)` of the directory `name` inside the directory `dir`, with
/// `flags` (the access mode, `O_PATH` or `O_SEARCH`) plus `O_DIRECTORY`,
/// `O_NOFOLLOW` and `O_CLOEXEC`: a symlink at `name` is refused (`ELOOP`;
/// `ENOTDIR` with `O_DIRECTORY` on Linux, `EMLINK` on FreeBSD, `EFTYPE` on
/// NetBSD), and so is anything but a directory.
pub(crate) fn open_dir_at(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    flags: libc::c_int,
) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    let flags = flags | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated and lives for the call; `dir` is a
    // borrowed, live fd. No mode is passed: O_CREAT is not among the flags.
    let fd = cvt(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) })?;
    // SAFETY: `fd` is a fresh fd nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `(st_dev, st_ino)` of what the raw fd `fd` is open on (`fstat(2)`), or
/// `EBADF` when `fd` is not open. Test-only: the tests look at an fd the
/// crate holds (B's, D's) without taking it, which `/dev/fd` cannot do on
/// every OS (FreeBSD and NetBSD show only 0 to 2 there without fdescfs).
#[cfg(test)]
pub(crate) fn fd_id(fd: std::os::fd::RawFd) -> io::Result<(u64, u64)> {
    // SAFETY: `stat` is plain old data; zeroed is a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fstat` reads no memory of ours but `st`, into which the kernel
    // writes at most one `stat`; a closed or invalid fd is `EBADF`, not
    // undefined behaviour.
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    #[allow(clippy::unnecessary_cast)]
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// `(st_dev, st_ino)` of `name` inside the directory `dir`, without following
/// a symlink at `name` (`fstatat(2)` with `AT_SYMLINK_NOFOLLOW`). The values
/// are widened to `u64` as std's `MetadataExt::dev` / `ino` widen them, so
/// the two compare.
pub(crate) fn id_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<(u64, u64)> {
    let name = c_name(name)?;
    // SAFETY: `stat` is plain old data; zeroed is a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated and lives for the call; the kernel
    // writes at most one `stat` into `st`; `dir` is a borrowed, live fd.
    cvt(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    #[allow(clippy::unnecessary_cast)]
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// What [`stat_at`] reports of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatAt {
    /// Whether it is a directory (`S_ISDIR`).
    pub(crate) is_dir: bool,
    /// Whether it is a symlink (`S_ISLNK`).
    pub(crate) is_symlink: bool,
    /// The permission bits and the set-id / sticky bits (`st_mode & 0o7777`).
    pub(crate) mode: u32,
    /// The owner (`st_uid`).
    pub(crate) uid: libc::uid_t,
}

/// The type, mode and owner of `name` inside the directory `dir`, without
/// following a symlink at `name` (`fstatat(2)` with `AT_SYMLINK_NOFOLLOW`).
pub(crate) fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<StatAt> {
    let name = c_name(name)?;
    // SAFETY: `stat` is plain old data; zeroed is a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated and lives for the call; the kernel
    // writes at most one `stat` into `st`; `dir` is a borrowed, live fd.
    cvt(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    // `mode_t` is u16 on Apple and the BSDs, u32 elsewhere.
    #[allow(clippy::unnecessary_cast)]
    let mode = st.st_mode as u32;
    #[allow(clippy::unnecessary_cast)]
    let dir_type = libc::S_IFDIR as u32;
    #[allow(clippy::unnecessary_cast)]
    let type_mask = libc::S_IFMT as u32;
    #[allow(clippy::unnecessary_cast)]
    let link_type = libc::S_IFLNK as u32;
    Ok(StatAt {
        is_dir: mode & type_mask == dir_type,
        is_symlink: mode & type_mask == link_type,
        mode: mode & 0o7777,
        uid: st.st_uid,
    })
}

/// `symlinkat(2)`: create the symlink `name` inside the directory `dir`,
/// pointing to `target`. An existing `name` is `EEXIST`, never replaced.
pub(crate) fn symlink_at(
    target: &std::path::Path,
    dir: BorrowedFd<'_>,
    name: &OsStr,
) -> io::Result<()> {
    let target = c_name(target.as_os_str())?;
    let name = c_name(name)?;
    // SAFETY: both strings are NUL-terminated and live for the call; `dir` is
    // a borrowed, live fd.
    cvt(unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) }).map(drop)
}

/// Set this thread's `errno` to 0. Where the way to reach it is not known,
/// nothing is done (a stale `errno` may then be taken for an error).
fn clear_errno() {
    #[cfg(target_os = "linux")]
    // SAFETY: returns this thread's errno, valid for the thread's life.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(target_os = "android")]
    // SAFETY: as above.
    unsafe {
        *libc::__errno() = 0;
    }
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    // SAFETY: as above.
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
    // SAFETY: as above.
    unsafe {
        *libc::__errno() = 0;
    }
    #[cfg(any(target_os = "illumos", target_os = "solaris"))]
    // SAFETY: as above.
    unsafe {
        *libc::___errno() = 0;
    }
}

/// The names in the directory `dir` (but `.` and `..`), listed through the
/// fd: `openat(dir, ".")` opens it again for reading (`dir` may be `O_PATH`
/// or `O_SEARCH`, which cannot be read), and `fdopendir` / `readdir` list
/// it. `EACCES` when it cannot be read.
pub(crate) fn read_dir_at(dir: BorrowedFd<'_>) -> io::Result<Vec<std::ffi::OsString>> {
    let dot = c_name(OsStr::new("."))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: `dot` is NUL-terminated and lives for the call; `dir` is a
    // borrowed, live fd. No mode is passed: O_CREAT is not among the flags.
    let fd = cvt(unsafe { libc::openat(dir.as_raw_fd(), dot.as_ptr(), flags) })?;
    // SAFETY: `fd` is a fresh fd of a directory opened for reading; on
    // success the stream owns it (closedir closes it), on failure it is
    // closed here.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: `fd` is ours and not owned by any stream.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut names = Vec::new();
    let result = loop {
        // errno is thread-local; 0 tells the end of the stream (NULL, errno
        // 0) from an error (NULL, errno set).
        clear_errno();
        // SAFETY: `stream` is a live directory stream used by this thread
        // alone.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break match io::Error::last_os_error().raw_os_error() {
                None | Some(0) => Ok(()),
                Some(e) => Err(io::Error::from_raw_os_error(e)),
            };
        }
        // SAFETY: `entry` points to a dirent valid until the next readdir on
        // this stream; `d_name` is NUL-terminated within it.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let name = name.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
        }
    };
    // SAFETY: `stream` is live and closed once here (it closes `fd`).
    unsafe { libc::closedir(stream) };
    result.map(|()| names)
}

/// `readlinkat(2)`: what the symlink `name` inside the directory `dir`
/// points to. `EINVAL` when `name` is not a symlink. A target that fills the
/// buffer is read again with a larger one, so it is never cut short.
pub(crate) fn read_link_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<std::path::PathBuf> {
    let name = c_name(name)?;
    let mut buf: Vec<u8> = vec![0; 256];
    loop {
        // SAFETY: `name` is NUL-terminated and lives for the call; the kernel
        // writes at most `buf.len()` bytes into `buf`; `dir` is a borrowed,
        // live fd.
        let n = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(std::path::PathBuf::from(std::ffi::OsString::from_vec(buf)));
        }
        buf.resize(buf.len() * 2, 0);
    }
}

/// `unlinkat(2)`: remove `name` inside the directory `dir`, a directory
/// (`AT_REMOVEDIR`, which must be empty) when `remove_dir`, otherwise anything
/// else (a symlink itself, not what it points to).
pub(crate) fn unlink_at(dir: BorrowedFd<'_>, name: &OsStr, remove_dir: bool) -> io::Result<()> {
    let name = c_name(name)?;
    let flags = if remove_dir { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `name` is NUL-terminated and lives for the call; `dir` is a
    // borrowed, live fd.
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(drop)
}

/// `fchmodat(2)` without flags: set the mode of `name` inside the directory
/// `dir` to `mode` (a symlink at `name` is followed: the callers use it only
/// on an entry they made a moment ago in a directory nobody else can change
/// entries of, after checking it by an fd opened `O_NOFOLLOW`).
///
/// Design rationale: by name rather than `fchmod(2)` on the fd, because on
/// Linux the fd is `O_PATH` (which `fchmod` refuses with `EBADF`), and an
/// `O_RDONLY` fd needs the owner's read bit, which a umask may have taken.
pub(crate) fn chmod_at(dir: BorrowedFd<'_>, name: &OsStr, mode: u32) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: `name` is NUL-terminated and lives for the call; `dir` is a
    // borrowed, live fd.
    cvt(unsafe { libc::fchmodat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t, 0) })
        .map(drop)
}

/// How [`lock`] locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lock {
    /// `LOCK_SH | LOCK_NB`: the lock a live process holds on its D.
    Shared,
    /// `LOCK_EX | LOCK_NB`: taken by `sweep()` only when no process holds D.
    Exclusive,
}

/// `flock(2)` on `fd` without waiting: `Ok(true)` when the lock was taken,
/// `Ok(false)` when another open file description holds a conflicting one
/// (`EWOULDBLOCK`). The lock lasts as long as the open file description.
pub(crate) fn lock(fd: BorrowedFd<'_>, how: Lock) -> io::Result<bool> {
    let op = match how {
        Lock::Shared => libc::LOCK_SH,
        Lock::Exclusive => libc::LOCK_EX,
    } | libc::LOCK_NB;
    loop {
        // SAFETY: plain syscall on a borrowed, live fd.
        if unsafe { libc::flock(fd.as_raw_fd(), op) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) => return Ok(false),
            _ => return Err(e),
        }
    }
}

/// The effective uid of this process (`geteuid(2)`, which cannot fail).
pub(crate) fn effective_uid() -> libc::uid_t {
    // SAFETY: plain syscall without arguments; it always succeeds.
    unsafe { libc::geteuid() }
}

/// The pid of this process (`getpid(2)`, which cannot fail), read at each
/// call (a forked child has its own).
pub(crate) fn process_id() -> libc::pid_t {
    // SAFETY: plain syscall without arguments; it always succeeds.
    unsafe { libc::getpid() }
}

/// Fill `buf` with random bytes from the system's secure generator:
/// `arc4random_buf(3)`, which never fails and never blocks.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "illumos"
))]
pub(crate) fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    // SAFETY: writes exactly `buf.len()` bytes into `buf`.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) };
    Ok(())
}

/// Fill `buf` with random bytes from the kernel: `getrandom(2)` with no flags
/// (the urandom source; it waits only while the kernel's pool is not yet
/// initialised, early in boot).
///
/// Made through `syscall(2)` rather than the C library's `getrandom`, which
/// glibc has only since 2.25 and bionic since API 28; the system call itself
/// is there since Linux 3.17. On an older kernel this fails with `ENOSYS`.
/// `EINTR` and a short read are retried.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let rest = &mut buf[filled..];
        // SAFETY: the kernel writes at most `rest.len()` bytes into `rest`.
        let n = unsafe {
            libc::syscall(
                libc::SYS_getrandom,
                rest.as_mut_ptr(),
                rest.len(),
                0 as libc::c_uint,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        filled += n as usize;
    }
    Ok(())
}

/// Fill `buf` with random bytes from `/dev/urandom`: on the remaining Unix
/// targets, which are not checked for a C-library generator one by one.
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "illumos",
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd"
)))]
pub(crate) fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

/// The path in an address the kernel wrote into `storage` with length `len`:
/// the bytes of `sun_path` within that length up to the first NUL (unix(7):
/// `strnlen(sun_path, len - offsetof(sun_path))`), so a length that counts
/// the NUL, one that does not, and one of the whole structure all read the
/// same path. Test-only, for [`local_address`] and [`recv_from`] (and the
/// test of this reading itself).
#[cfg(test)]
pub(crate) fn path_in(storage: &[u8], len: usize) -> std::path::PathBuf {
    let end = len.min(storage.len());
    let raw = storage.get(SUN_PATH_OFFSET..end).unwrap_or_default();
    let path = raw.split(|&b| b == 0).next().unwrap_or_default();
    OsStr::from_bytes(path).into()
}

/// `getsockname(2)` for `sock`: the length the kernel reports and the path in
/// it ([`path_in`]), asked with room for the widest address any target here
/// builds ([`ADDR_STORAGE`] bytes).
///
/// Test-only: the tests check the spelling the kernel keeps, read as the
/// kernel wrote it rather than through std's `SocketAddr`, whose reading
/// differs by Rust version (Rust 1.99 and earlier drop the last byte of the
/// path on OpenBSD, rust-lang/rust#118925, fixed in 1.100.0). The length
/// tells whether a bound address fits a `sockaddr_un` (Apple).
#[cfg(test)]
pub(crate) fn local_address(sock: BorrowedFd<'_>) -> io::Result<(usize, std::path::PathBuf)> {
    let mut storage = [0u8; ADDR_STORAGE];
    let mut len = ADDR_STORAGE as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes into `storage`.
    cvt(unsafe { libc::getsockname(sock.as_raw_fd(), storage.as_mut_ptr().cast(), &mut len) })?;
    Ok((len as usize, path_in(&storage, len as usize)))
}

/// `recvfrom(2)` on `sock` into `buf`: the number of bytes received and the
/// path of the sender's address as the kernel wrote it ([`path_in`]).
/// Test-only, for the same reason as [`local_address`]: the reply goes to the
/// address the kernel gives, not to std's reading of it.
#[cfg(test)]
pub(crate) fn recv_from(
    sock: BorrowedFd<'_>,
    buf: &mut [u8],
) -> io::Result<(usize, std::path::PathBuf)> {
    let mut storage = [0u8; ADDR_STORAGE];
    let mut len = ADDR_STORAGE as libc::socklen_t;
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf` and at
    // most `len` bytes into `storage`.
    let n = unsafe {
        libc::recvfrom(
            sock.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            storage.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((n as usize, path_in(&storage, len as usize)))
}

/// Give up root for good in this process: no supplementary groups, then the
/// group and user ids `id` (`setgroups(2)`, `setgid(2)`, `setuid(2)`, which
/// set the real, effective and saved ids, and on Linux drop the
/// capabilities). Test-only, and only for a child process of the test binary
/// run by root: the ids are process-wide (glibc and musl apply them to every
/// thread).
#[cfg(test)]
pub(crate) fn drop_root(id: libc::uid_t) -> io::Result<()> {
    // SAFETY: an empty list; the pointer is not read.
    cvt(unsafe { libc::setgroups(0, std::ptr::null()) })?;
    // SAFETY: plain syscalls without pointers.
    cvt(unsafe { libc::setgid(id as libc::gid_t) })?;
    // SAFETY: as above.
    cvt(unsafe { libc::setuid(id) }).map(drop)
}

/// Set the file mode creation mask of the process (`umask(2)`, which cannot
/// fail). Test-only, and only for a child process of the test binary: the mask
/// is process-wide.
#[cfg(test)]
pub(crate) fn set_umask(mask: u32) {
    // SAFETY: plain syscall without pointers.
    unsafe {
        libc::umask(mask as libc::mode_t);
    }
}

/// Set this process's effective uid to `uid` (`seteuid(2)`), leaving the
/// real uid as it is. Test-only, and only for a child process of the test
/// binary run by root: the uid is process-wide.
#[cfg(test)]
pub(crate) fn set_effective_uid(uid: libc::uid_t) -> io::Result<()> {
    // SAFETY: plain syscall without pointers.
    cvt(unsafe { libc::seteuid(uid) }).map(drop)
}

/// The real uid of this process (`getuid(2)`). Test-only.
#[cfg(test)]
pub(crate) fn real_uid() -> libc::uid_t {
    // SAFETY: plain syscall without arguments; it always succeeds.
    unsafe { libc::getuid() }
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

/// Whether `SO_NOSIGPIPE` is set on `sock`. Test-only: checks that sockat's
/// own sockets carry it wherever std's do.
#[cfg(all(
    test,
    any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )
))]
pub(crate) fn so_nosigpipe(sock: BorrowedFd<'_>) -> io::Result<bool> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt into a live c_int whose size is passed in `len`.
    cvt(unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw mut value).cast(),
            &raw mut len,
        )
    })?;
    Ok(value != 0)
}
