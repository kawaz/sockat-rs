//! The directories B and D and the symlinks E (hub SPEC-routes "B の作り方",
//! "D と E").
//!
//! * B is a real directory of this user in a place (`/tmp`,
//!   `$XDG_RUNTIME_DIR`, `$TMPDIR`, the socket's directory) or where
//!   `Options.base` says, reused across processes and never removed by
//!   sockat. Its fd is held for the life of the process (the
//!   `/proc/<pid>/fd/<B's fd>` spelling goes through it).
//! * D is a directory in B, one per (process, B), with a random name and mode
//!   0711. The process holds a shared `flock(2)` on it for its whole life, so
//!   [`sweep`] (and systemd-tmpfiles) can tell a live D from a dead one.
//! * E is a symlink: in D, to the socket's directory (link-dir, thread-cwd,
//!   at) or to the socket file (link-file); or directly in B for the plain
//!   connect's link-file.
//!
//! The Bs this process uses are kept in a registry and checked to still be
//! the directory held (the same `(st_dev, st_ino)` at B's spelling) before
//! each use. Of the places 1-3 the process holds one B (and one more for
//! `usable_cross_uid` when that is elsewhere, [`Held`]); a B in the socket's
//! directory or at `Options.base` is looked up by where it is.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::sys;

/// Where a B is, for the diagnostic `base` and the order of the places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Place {
    /// `/tmp/sockat-<uid>`.
    Tmp,
    /// `$XDG_RUNTIME_DIR/sockat`.
    Runtime,
    /// `$TMPDIR/sockat-<uid>`.
    Tmpdir,
    /// `<the socket's directory>/.sockat.<10 random>`, when none of the
    /// above can be made.
    SocketDir,
    /// The path the caller gave as `Options.base`.
    Given,
}

impl Place {
    /// The diagnostic `base` of a spelling through B's real path.
    pub(crate) fn real_name(self) -> &'static str {
        match self {
            Place::Tmp => "tmp",
            Place::Runtime => "runtime",
            Place::Tmpdir => "tmpdir",
            Place::SocketDir => "sockdir",
            Place::Given => "given",
        }
    }

    /// The diagnostic `base` of a spelling through `/proc/<pid>/fd/<B's fd>`:
    /// `proc` for a B in a temporary place (the spelling is the same
    /// wherever it is), the B itself (`sockdir`, `given`) otherwise (hub
    /// SPEC-routes "綴りの形").
    pub(crate) fn proc_name(self) -> &'static str {
        match self {
            Place::SocketDir => "sockdir",
            Place::Given => "given",
            Place::Tmp | Place::Runtime | Place::Tmpdir => "proc",
        }
    }

    /// The default name of B in this place.
    pub(crate) fn b_name(self) -> String {
        match self {
            Place::Runtime => "sockat".to_owned(),
            _ => format!("sockat-{}", sys::effective_uid()),
        }
    }
}

/// Length of every random name sockat draws: B in the socket's directory, D,
/// the E of thread-cwd / at and the E directly in B (hub
/// SPEC-routes "D と E").
pub(crate) const RANDOM_LEN: usize = 10;

/// How many names to try before giving up. Each try is a fresh random name,
/// so exhausting this means something other than bad luck.
pub(crate) const ATTEMPTS: usize = 16;

/// What B's name starts with in the socket's directory.
pub(crate) const SOCKDIR_PREFIX: &str = ".sockat.";

/// `RANDOM_LEN` random letters and digits (62 kinds) from the system's
/// secure generator.
pub(crate) fn random_name() -> io::Result<String> {
    const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut random = [0u8; RANDOM_LEN];
    sys::random_bytes(&mut random)?;
    // 256 is not a multiple of 62, so this is slightly biased; the name only
    // has to be hard to guess and to collide with, not uniform.
    Ok(random
        .iter()
        .map(|b| ALPHABET[*b as usize % 62] as char)
        .collect())
}

/// Whether `name` has the form of the names [`random_name`] draws:
/// `RANDOM_LEN` ASCII letters and digits. [`sweep`] takes only directories
/// named so (hub SPEC "`sweep`").
pub(crate) fn is_random_name(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.len() == RANDOM_LEN && name.iter().all(u8::is_ascii_alphanumeric)
}

/// `n` in base 36 (`0`-`9`, `a`-`z`): the name of the n-th E a D holds (`0`,
/// `1`, ..., `z`, `10`, ...; hub SPEC-routes "D と E"). Unique within D,
/// which is unique per (process, B), and short.
pub(crate) fn counted_name(mut n: u64) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::new();
    loop {
        out.insert(0, ALPHABET[(n % 36) as usize] as char);
        n /= 36;
        if n == 0 {
            return out;
        }
    }
}

/// Open the directory `path` without following a symlink at its last
/// component, to use as the base of `*at` calls and of `fchdir`: `O_PATH`
/// on Linux, `O_SEARCH` on Apple (`O_RDONLY` when the kernel refuses it on a
/// directory, `EISDIR` / `EINVAL`), `O_RDONLY` elsewhere. `O_PATH` and
/// `O_SEARCH` need no read permission on the directory.
pub(crate) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let open = |flags: libc::c_int| {
        OpenOptions::new()
            .read(true)
            .custom_flags(flags | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map(OwnedFd::from)
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return open(libc::O_PATH);
    #[cfg(target_vendor = "apple")]
    match open(libc::O_SEARCH) {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EISDIR | libc::EINVAL)) => {}
        searched => return searched,
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    open(0)
}

/// [`open_dir`] of `name` inside the directory `dir`.
fn open_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return sys::open_dir_at(dir, name, libc::O_PATH);
    #[cfg(target_vendor = "apple")]
    match sys::open_dir_at(dir, name, libc::O_SEARCH) {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EISDIR | libc::EINVAL)) => {}
        searched => return searched,
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    sys::open_dir_at(dir, name, libc::O_RDONLY)
}

/// `EPERM`: a check of a place or of `Options.base` failed.
fn refused() -> io::Error {
    io::Error::from_raw_os_error(libc::EPERM)
}

/// A poisoned lock only means a thread panicked while holding it; the data
/// (directories held by fds) stays consistent, so go on with it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// the check of the directories on the way to B
// ---------------------------------------------------------------------------

/// What the directories on the way to a place are like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chain {
    /// None of them can be changed by another user: each is owned by root or
    /// this user and either not writable by group and others or sticky (as
    /// OpenSSH's StrictModes judges a path). Otherwise another user could
    /// replace a directory on the way, and B with it.
    pub(crate) safe: bool,
    /// Each lets others search it (`o+x`), so another uid can follow a
    /// spelling through it.
    pub(crate) others_search: bool,
}

/// How often the way to a B is checked (hub SPEC-routes "B の作り方" 2 and
/// "実体の置き場").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    /// Once per process: the places 1-3 and the socket's directory.
    Once,
    /// On every call: `Options.base` (and `sweep_base`, whose path is
    /// checked as `Options.base` is), whose way may change between calls.
    EveryCall,
}

/// The [`Chain`] of every directory from the root down to `real`, a real path
/// (hub SPEC-routes "B の作り方" 2): looked up once per process for
/// [`Check::Once`], on every call for [`Check::EveryCall`].
///
/// # Errors
///
/// `ENOENT` when one is missing, `ENOTDIR` when one is not a directory.
pub(crate) fn chain(real: &Path, check: Check) -> io::Result<Chain> {
    static CHECKED: Mutex<Option<HashMap<PathBuf, Chain>>> = Mutex::new(None);
    if check == Check::Once {
        if let Some(c) = lock(&CHECKED).as_ref().and_then(|m| m.get(real)) {
            return Ok(*c);
        }
    }
    let mut c = Chain {
        safe: true,
        others_search: true,
    };
    let mut ancestor = PathBuf::new();
    for step in real.components() {
        ancestor.push(step);
        let m = std::fs::symlink_metadata(&ancestor)?;
        if !m.is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let owner_ok = m.uid() == 0 || m.uid() == sys::effective_uid();
        let unwritable = m.mode() & 0o022 == 0;
        let sticky = m.mode() & 0o1000 != 0;
        c.safe &= owner_ok && (unwritable || sticky);
        c.others_search &= m.mode() & 0o001 != 0;
    }
    if check == Check::Once {
        lock(&CHECKED)
            .get_or_insert_with(HashMap::new)
            .insert(real.to_path_buf(), c);
    }
    Ok(c)
}

/// Whether `p` is `/tmp` or `/var/tmp` or below one of them, which a process
/// with a private `/tmp` (systemd's `PrivateTmp`) does not share.
/// The comparison is by components, against each of them as written and as
/// its real path (macOS's `/tmp` is `/private/tmp`).
fn under_tmp(p: &Path) -> bool {
    ["/tmp", "/var/tmp"]
        .iter()
        .any(|t| p.starts_with(t) || std::fs::canonicalize(t).is_ok_and(|real| p.starts_with(real)))
}

// ---------------------------------------------------------------------------
// B
// ---------------------------------------------------------------------------

/// A B this process uses, held by its fd.
#[derive(Debug)]
pub(crate) struct Base {
    /// B's spelling: its real path (hub SPEC-routes "B の作り方" 1: the place
    /// is resolved first, and B is checked, opened and spelled by it).
    pub(crate) path: PathBuf,
    pub(crate) place: Place,
    /// The place as given, before it was resolved (`/tmp`, `$TMPDIR`, the
    /// parent of `Options.base` as given, the socket's directory's real
    /// path), which the registry finds B by.
    given_parent: PathBuf,
    fd: OwnedFd,
    /// `(st_dev, st_ino)` of B.
    pub(crate) id: (u64, u64),
    /// Whether every directory on the way to B lets others search, as judged
    /// when B was taken.
    way_search: bool,
    /// Whether B itself lets others search: judged when B was taken, and
    /// looked at again while it is `false` ([`Base::others_search`]).
    b_search: AtomicBool,
    /// Whether the place as given or B's real path goes through `/tmp` or
    /// `/var/tmp` (hub SPEC "綴りの性質": then `usable_with_private_tmp` is
    /// false).
    pub(crate) through_tmp: bool,
    /// This process's D in B, made when first needed.
    d: Mutex<Option<Arc<Dir>>>,
    /// The Ds of B let go because they were removed, kept with their fd and
    /// lock for the rest of the process ([`Base::dir`]).
    retired_d: Mutex<Vec<Arc<Dir>>>,
}

/// What [`Base::make`] does with a B already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Existing {
    /// Use it when it passes the checks; otherwise the name is taken
    /// ([`Unmade::Occupied`]) and the place cannot be used (the places 1-3).
    UseOrNext,
    /// Use it when it passes the checks; otherwise `EPERM` (`Options.base`).
    UseOrRefuse,
    /// Never: the name is random and must be new (the socket's directory).
    New,
    /// Make nothing: only find it (`sweep`).
    FindOnly,
}

/// Why [`Base::make`] gave no B.
#[derive(Debug)]
enum Unmade {
    /// Something already at B's name fails the checks (not a real directory,
    /// someone else's, writable by group or others): the name is taken
    /// (hub SPEC-routes "B の作り方" 5). The error is the one the check gives
    /// (`EPERM`, `ENOTDIR`), which `Options.base` returns.
    Occupied(io::Error),
    /// Anything else (the way to B, `mkdirat`, the mode, the open, B not
    /// letting others search when that is asked).
    Failed(io::Error),
}

impl From<io::Error> for Unmade {
    fn from(e: io::Error) -> Self {
        Unmade::Failed(e)
    }
}

impl From<Unmade> for io::Error {
    fn from(u: Unmade) -> Self {
        match u {
            Unmade::Occupied(e) | Unmade::Failed(e) => e,
        }
    }
}

impl Base {
    /// B's fd.
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Whether another uid can follow a spelling through B's real path: every
    /// directory on the way and B itself let others search (D is always
    /// 0711), as judged when B was taken (hub SPEC-routes "実体の置き場").
    ///
    /// When B itself was judged not to, a call for `usable_cross_uid`
    /// (`recheck`) looks at B's mode again by `fstat` of its fd, and keeps
    /// what it finds once B lets others search: two processes making B for
    /// the first time at once may take it between the other's `mkdirat`
    /// (0700) and its mode set to 0711, and that judgement must not stay.
    /// The way to B is not looked at again.
    pub(crate) fn others_search(&self, recheck: bool) -> bool {
        if !self.way_search {
            return false;
        }
        if self.b_search.load(Ordering::Relaxed) {
            return true;
        }
        if !recheck {
            return false;
        }
        let now = self
            .fd
            .try_clone()
            .and_then(|fd| std::fs::File::from(fd).metadata())
            .is_ok_and(|m| m.mode() & 0o001 != 0);
        if now {
            self.b_search.store(true, Ordering::Relaxed);
        }
        now
    }

    /// Whether B's spelling still names the directory held.
    fn current(&self) -> bool {
        let named = std::fs::symlink_metadata(&self.path).map(|m| (m.dev(), m.ino()));
        still_named(self.fd.as_fd(), self.id, named)
    }

    /// Make (or find) B named `name` in the directory `parent` (hub
    /// SPEC-routes "B の作り方"):
    ///
    /// 1. `parent` is resolved to its real path, which the rest uses;
    /// 2. every directory from the root down to it is checked ([`chain`]);
    /// 3. it is opened without following a symlink and
    ///    `mkdirat(parent, name, 0700)` makes B;
    /// 4. B is opened without following a symlink, `fstat` must show a
    ///    directory of this user, and its mode is set to 0711;
    /// 5. on `EEXIST`, what is there must be a directory of this user that
    ///    group and others cannot write to, and is used with its mode as it
    ///    is; otherwise the name is taken ([`Unmade::Occupied`]).
    ///
    /// `others_search` asks for a B another uid can search through: a way
    /// there that does not allow it is `EPERM` before anything is made, and
    /// so is a B already there without `o+x`.
    fn make(
        parent: &Path,
        name: &OsStr,
        place: Place,
        existing: Existing,
        others_search: bool,
    ) -> Result<Base, Unmade> {
        let parent_real = std::fs::canonicalize(parent)?;
        let check = match place {
            Place::Given => Check::EveryCall,
            _ => Check::Once,
        };
        let c = chain(&parent_real, check)?;
        if !c.safe || (others_search && !c.others_search) {
            return Err(Unmade::Failed(refused()));
        }
        let parent_fd = open_dir(&parent_real)?;
        let mut name = name.to_os_string();
        for _ in 0..ATTEMPTS {
            let created = match existing {
                Existing::FindOnly => false,
                _ => match sys::mkdir_at(parent_fd.as_fd(), &name, 0o700) {
                    Ok(()) => true,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        if existing == Existing::New {
                            name = socket_dir_name()?;
                            continue;
                        }
                        false
                    }
                    Err(e) => return Err(e.into()),
                },
            };
            let st = sys::stat_at(parent_fd.as_fd(), &name)?;
            // A symlink is not a real directory (EPERM: the check fails);
            // anything else that is not a directory is ENOTDIR.
            if st.is_symlink {
                return Err(Unmade::Occupied(refused()));
            }
            if !st.is_dir {
                return Err(Unmade::Occupied(io::Error::from_raw_os_error(
                    libc::ENOTDIR,
                )));
            }
            let ours = st.uid == sys::effective_uid() && (created || st.mode & 0o022 == 0);
            if !ours {
                return Err(Unmade::Occupied(refused()));
            }
            // Design rationale: the mode of a B just made is set by its name
            // before it is opened, rather than by `fchmod` on the fd after:
            // the fd is `O_PATH` on Linux (no `fchmod`), and a umask that took
            // the owner's search or read bit would keep it from being opened
            // at all. Nobody else can replace an entry of this user's in the
            // checked parent meanwhile (a sticky one lets only the owner
            // rename or remove it).
            let mode = if created {
                if let Err(e) = sys::chmod_at(parent_fd.as_fd(), &name, 0o711) {
                    let _ = sys::unlink_at(parent_fd.as_fd(), &name, true);
                    return Err(e.into());
                }
                0o711
            } else {
                st.mode
            };
            let fd = match open_dir_at(parent_fd.as_fd(), &name) {
                Ok(fd) => fd,
                Err(e) => {
                    if created {
                        let _ = sys::unlink_at(parent_fd.as_fd(), &name, true);
                    }
                    return Err(e.into());
                }
            };
            // What was opened must be a directory of this user and still be
            // what is at the name (`fstat` of the fd, `lstat` by the name).
            let m = std::fs::File::from(fd.try_clone()?).metadata()?;
            let id = (m.dev(), m.ino());
            let held = m.is_dir()
                && m.uid() == sys::effective_uid()
                && sys::id_at(parent_fd.as_fd(), &name).is_ok_and(|named| named == id);
            if !held {
                if created {
                    let _ = sys::unlink_at(parent_fd.as_fd(), &name, true);
                    return Err(Unmade::Failed(refused()));
                }
                return Err(Unmade::Occupied(refused()));
            }
            let b_search = mode & 0o001 != 0;
            if others_search && !b_search {
                return Err(Unmade::Failed(refused()));
            }
            let path = parent_real.join(&name);
            return Ok(Base {
                through_tmp: under_tmp(parent) || under_tmp(&path),
                path,
                place,
                given_parent: parent.to_path_buf(),
                fd,
                id,
                way_search: c.others_search,
                b_search: AtomicBool::new(b_search),
                d: Mutex::new(None),
                retired_d: Mutex::new(Vec::new()),
            });
        }
        Err(Unmade::Failed(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free name for B",
        )))
    }

    /// This process's D in B, made the first time it is asked for (and again
    /// when the one held is no longer at its name in B: the one let go is
    /// kept, fd and lock held, for the rest of the process, as a B let go
    /// is, [`retire`]).
    pub(crate) fn dir(&self) -> io::Result<Arc<Dir>> {
        let mut slot = lock(&self.d);
        if let Some(d) = slot.as_ref() {
            if still_named(d.fd.as_fd(), d.id, sys::id_at(self.fd.as_fd(), &d.name)) {
                return Ok(Arc::clone(d));
            }
        }
        let d = Arc::new(Dir::make(self.fd.as_fd())?);
        if let Some(old) = slot.replace(Arc::clone(&d)) {
            lock(&self.retired_d).push(old);
        }
        Ok(d)
    }

    /// The name of this process's D in B, when it has one.
    fn own_d(&self) -> Option<OsString> {
        lock(&self.d).as_ref().map(|d| d.name.clone())
    }
}

/// Whether the directory held by `fd` (whose `(st_dev, st_ino)` was `id`)
/// is still there and still what its name names (`named`, by `lstat`): it
/// has not been removed (`fstat` shows links) and the name gives the same
/// `(st_dev, st_ino)` (hub SPEC-routes "実体の置き場": a B or D removed, by
/// the OS's clean-up or by hand, is let go and made again by the next call).
/// The link count tells a removed directory whose number the file system has
/// already given to a new one at the name (a removed directory shows 0 links
/// on Linux; macOS's APFS keeps the count, so there only the name tells).
pub(crate) fn still_named(
    fd: BorrowedFd<'_>,
    id: (u64, u64),
    named: io::Result<(u64, u64)>,
) -> bool {
    let linked = fd
        .try_clone_to_owned()
        .and_then(|fd| std::fs::File::from(fd).metadata())
        .is_ok_and(|m| m.nlink() > 0);
    linked && named.is_ok_and(|n| n == id)
}

/// A random name for B in the socket's directory: `.sockat.<10 random>`
/// (hub SPEC-routes "B の作り方").
fn socket_dir_name() -> io::Result<OsString> {
    Ok(OsString::from(format!(
        "{SOCKDIR_PREFIX}{}",
        random_name()?
    )))
}

/// Every B this process holds.
struct Registry {
    all: Vec<Arc<Base>>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry { all: Vec::new() });

/// The Bs let go because they were removed (hub SPEC-routes "実体の置き場"):
/// kept, fd open, for the rest of the process. Closing the fd would let its
/// number go to another directory, and a live socket whose spelling goes
/// through `/proc/<pid>/fd/<that number>` (procB) would then name a place in
/// that directory.
static RETIRED: Mutex<Vec<Arc<Base>>> = Mutex::new(Vec::new());

/// Keep `b`, removed and let go, for the rest of the process ([`RETIRED`]).
fn retire(b: Arc<Base>) {
    lock(&RETIRED).push(b);
}

impl Registry {
    /// The B that `pick` chooses and that is still at its spelling; a B no
    /// longer there is let go, its fd kept open ([`retire`]).
    fn find(&mut self, pick: impl Fn(&Base) -> bool) -> Option<Arc<Base>> {
        let i = self.all.iter().position(|b| pick(b))?;
        if self.all[i].current() {
            return Some(Arc::clone(&self.all[i]));
        }
        retire(self.all.remove(i));
        None
    }

    /// The B at `Options.base` (`parent`, `name` as given).
    fn given(&mut self, parent: &Path, name: &OsStr) -> Option<Arc<Base>> {
        self.find(|b| {
            b.place == Place::Given && b.given_parent == parent && b.path.file_name() == Some(name)
        })
    }
}

/// The places 1-3 (hub SPEC-routes "実体の置き場"), in order: `/tmp`, then
/// `$XDG_RUNTIME_DIR` and `$TMPDIR`, each only when set to an absolute path.
/// A path already listed is not listed again.
pub(crate) fn places_from(
    tmpdir: Option<OsString>,
    runtime_dir: Option<OsString>,
) -> Vec<(Place, PathBuf)> {
    let set = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let mut out = vec![(Place::Tmp, PathBuf::from("/tmp"))];
    for (place, p) in [
        (Place::Runtime, set(runtime_dir)),
        (Place::Tmpdir, set(tmpdir)),
    ] {
        if let Some(p) = p {
            let same = |q: &PathBuf| q.components().eq(p.components());
            if !out.iter().any(|(_, q)| same(q)) {
                out.push((place, p));
            }
        }
    }
    out
}

/// The B of the places 1-3 a process holds (hub SPEC-routes "実体の置き場"):
/// the usual one ([`USUAL`]) and the one for `usable_cross_uid` ([`CROSS`]),
/// the same B when the usual one lets others search the way to it.
// Design rationale: carried by `route::Config` rather than kept only in a
// static, so that each test holds Bs of its own places while the tests share
// one process; outside the tests every call carries the one of the process
// (`Held::process`).
#[derive(Debug, Default)]
pub(crate) struct Held {
    slots: Mutex<[Option<Arc<Base>>; 2]>,
}

const USUAL: usize = 0;
const CROSS: usize = 1;

impl Held {
    /// The one this process holds.
    pub(crate) fn process() -> Arc<Held> {
        static PROCESS: std::sync::OnceLock<Arc<Held>> = std::sync::OnceLock::new();
        Arc::clone(PROCESS.get_or_init(Arc::default))
    }
}

/// The B in `slots[slot]` when it is still at its spelling; one no longer
/// there is let go, its fd kept open ([`retire`]).
fn held_in(slots: &mut [Option<Arc<Base>>; 2], slot: usize) -> Option<Arc<Base>> {
    let b = slots[slot].clone()?;
    if b.current() {
        return Some(b);
    }
    slots[slot] = None;
    lock(&REGISTRY).all.retain(|r| !Arc::ptr_eq(r, &b));
    retire(b);
    None
}

/// What [`automatic`] found.
pub(crate) enum Automatic {
    /// The process's B, and a spelling through it may fit.
    Base(Arc<Base>),
    /// The process's B is (or would be made) where the spelling is known not
    /// to fit: the routes through B are passed over, and nothing is made.
    TooLong,
    /// B can be made in none of the places; `occupied` when B's default
    /// name was taken in one of them (hub SPEC "skipped": the reason of the
    /// routes through B says so).
    Missing { occupied: bool },
}

/// The process's B of the places 1-3 (hub SPEC-routes "実体の置き場"): the
/// one it holds, else the first of `places` where B can be made, which it
/// then holds for the rest of its life. A place where it could not be made
/// is not remembered, and is tried again by the next call.
///
/// With `others_search` (`usable_cross_uid`), only a place where every
/// directory on the way and B itself let others search ("別の uid と
/// PrivateTmp"); that B is held apart from the usual one, and is the usual one
/// when that allows it. Whether a B allows it is judged when the process
/// takes it and kept, except that B itself found not to is looked at again
/// ([`Base::others_search`]).
///
/// When the spelling through that first place is known not to fit (`fits`
/// of B's real path there), B is not made, and the next place is not tried
/// either: the process's B does not depend on the length of a path.
///
/// A place whose default name is taken (something there fails the checks of
/// B) cannot be used: nothing is made in it, and the next place is tried
/// (hub SPEC-routes "B の作り方" 5).
pub(crate) fn automatic(
    places: &[(Place, PathBuf)],
    held: &Held,
    others_search: bool,
    fits: impl Fn(&Path) -> bool,
) -> Automatic {
    let mut slots = lock(&held.slots);
    let slot = if others_search { CROSS } else { USUAL };
    let found = |b: Arc<Base>| match fits(&b.path) {
        true => Automatic::Base(b),
        false => Automatic::TooLong,
    };
    if let Some(b) = held_in(&mut slots, slot) {
        return found(b);
    }
    let other = held_in(&mut slots, 1 - slot);
    let mut occupied = false;
    for (place, parent) in places {
        // The B held for the other slot is this place's B when it is here,
        // with what it allows as judged when it was taken (B itself looked
        // at again when judged not to).
        if let Some(b) = other
            .as_ref()
            .filter(|b| b.place == *place && b.given_parent == *parent)
        {
            if others_search && !b.others_search(true) {
                continue;
            }
            slots[slot] = Some(Arc::clone(b));
            return found(Arc::clone(b));
        }
        // What can be told without making anything: a place that is not
        // there, or that the way to does not pass the checks, is not where B
        // can be made.
        let Ok(real) = std::fs::canonicalize(parent) else {
            continue;
        };
        match chain(&real, Check::Once) {
            Ok(c) if c.safe && (c.others_search || !others_search) => {}
            _ => continue,
        }
        if !fits(&real.join(place.b_name())) {
            return Automatic::TooLong;
        }
        let name = OsString::from(place.b_name());
        match Base::make(parent, &name, *place, Existing::UseOrNext, others_search) {
            Ok(b) => {
                let b = Arc::new(b);
                lock(&REGISTRY).all.push(Arc::clone(&b));
                slots[slot] = Some(Arc::clone(&b));
                return Automatic::Base(b);
            }
            Err(Unmade::Occupied(_)) => occupied = true,
            Err(Unmade::Failed(_)) => {}
        }
    }
    Automatic::Missing { occupied }
}

/// The B in the socket's directory (`.sockat.<10 random>`), for when no place
/// 1-3 has one: one per socket's directory in the process. `real_dir` is the
/// socket's directory's real path.
pub(crate) fn in_socket_dir(real_dir: &Path) -> Option<Arc<Base>> {
    let mut registry = lock(&REGISTRY);
    if let Some(b) = registry.find(|b| b.place == Place::SocketDir && b.given_parent == real_dir) {
        return Some(b);
    }
    let name = socket_dir_name().ok()?;
    let b = Arc::new(Base::make(real_dir, &name, Place::SocketDir, Existing::New, false).ok()?);
    registry.all.push(Arc::clone(&b));
    Some(b)
}

/// `Options.base` split into its parent and B's name: it must be absolute and
/// end with a name (not `dir/`, `.`, `..` or `/`), else it is an argument
/// error (hub SPEC "エラー").
pub(crate) fn split_given(base: &Path) -> io::Result<(&Path, &OsStr)> {
    let refuse = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Options.base must be an absolute path ending with a name",
        )
    };
    if !base.is_absolute() {
        return Err(refuse());
    }
    crate::path::split(base).map_err(|_| refuse())
}

/// Check `Options.base` without making anything (hub SPEC-routes "B の作り方":
/// made before any route is tried, for a bind and a connect alike): the
/// directories on the way (looked at again on every call, "実体の置き場"),
/// and B itself when it is there. `others_search` is
/// `Options.usable_cross_uid` of a bind; for a B this process already holds,
/// whether others can search the way to it is the judgement made when it was
/// taken, B itself looked at again when judged not to
/// ([`Base::others_search`]). The errors are those of [`given`].
pub(crate) fn check_given(base: &Path, others_search: bool) -> io::Result<()> {
    let (parent, name) = split_given(base)?;
    let held = lock(&REGISTRY).given(parent, name);
    let parent_real = std::fs::canonicalize(parent)?;
    let c = chain(&parent_real, Check::EveryCall)?;
    // Whether others can search the way is the judgement made when B was
    // taken, for a B this process holds; whether the way is safe is checked
    // now.
    let way_search = held.as_ref().map_or(c.others_search, |b| b.way_search);
    if !c.safe || (others_search && !way_search) {
        return Err(refused());
    }
    let m = match std::fs::symlink_metadata(parent_real.join(name)) {
        Ok(m) => m,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
        Err(e) => return Err(e),
    };
    if m.file_type().is_symlink() {
        return Err(refused());
    }
    if !m.is_dir() {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    let ours = m.uid() == sys::effective_uid() && m.mode() & 0o022 == 0;
    let searchable = match &held {
        Some(b) => b.others_search(others_search),
        None => m.mode() & 0o001 != 0,
    };
    if !ours || (others_search && !searchable) {
        return Err(refused());
    }
    Ok(())
}

/// The B the caller gave as `Options.base` (hub SPEC "エラー"), made when it is
/// not there. `others_search` is `Options.usable_cross_uid` of a bind.
///
/// # Errors
///
/// `ENOENT` when a directory on the way is missing, `ENOTDIR` when B or one
/// on the way is not a directory, `EPERM` when a check fails (a directory on
/// the way another user can change, B someone else's or writable by group or
/// others; with `others_search`, a directory on the way or B others cannot
/// search; for a B this process holds, as judged when it was taken, B itself
/// looked at again when judged not to). Never
/// another place instead.
pub(crate) fn given(base: &Path, others_search: bool) -> io::Result<Arc<Base>> {
    let (parent, name) = split_given(base)?;
    let mut registry = lock(&REGISTRY);
    if let Some(b) = registry.given(parent, name) {
        if others_search && !b.others_search(true) {
            return Err(refused());
        }
        return Ok(b);
    }
    let b = Arc::new(Base::make(
        parent,
        name,
        Place::Given,
        Existing::UseOrRefuse,
        others_search,
    )?);
    registry.all.push(Arc::clone(&b));
    Ok(b)
}

// ---------------------------------------------------------------------------
// D and E
// ---------------------------------------------------------------------------

/// This process's D in a B, held by an fd with a shared `flock(2)`.
#[derive(Debug)]
pub(crate) struct Dir {
    pub(crate) name: OsString,
    fd: OwnedFd,
    id: (u64, u64),
    /// The number of the next counted E ([`counted_name`]).
    next: AtomicU64,
    /// The E the default binds share ([`Dir::shared`]).
    shared: Mutex<HashMap<PathBuf, OsString>>,
}

impl Dir {
    /// Make D in the B held by `b` (hub SPEC-routes "D の作り方"):
    ///
    /// 1. `mkdirat(b, <10 random>, 0700)`, a fresh name on `EEXIST`, at most
    ///    [`ATTEMPTS`] times;
    /// 2. mode 0711, then opened `O_RDONLY | O_DIRECTORY | O_NOFOLLOW |
    ///    O_CLOEXEC`, and `fstat` must show a directory of this user;
    /// 3. a shared `flock` on that fd, held as long as the process keeps D;
    /// 4. D's name in B must still be that directory: another process's
    ///    `sweep()` may have removed it between 1 and 3 (it was not locked
    ///    yet), and then the fd is closed and D made again.
    fn make(b: BorrowedFd<'_>) -> io::Result<Dir> {
        for _ in 0..ATTEMPTS {
            let name = OsString::from(random_name()?);
            match sys::mkdir_at(b, &name, 0o700) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            // Nothing but this process makes entries in B (its own), so what
            // is at the name is the directory just made; one that cannot be
            // made ready is removed again (it is empty).
            let opened = sys::chmod_at(b, &name, 0o711)
                .and_then(|()| open_d(b, &name))
                .and_then(|fd| {
                    let m = std::fs::File::from(fd.try_clone()?).metadata()?;
                    match m.is_dir() && m.uid() == sys::effective_uid() {
                        true => Ok((fd, m)),
                        false => Err(refused()),
                    }
                });
            let (fd, m) = match opened {
                Ok(done) => done,
                Err(e) => {
                    let _ = sys::unlink_at(b, &name, true);
                    return Err(e);
                }
            };
            if !sys::lock(fd.as_fd(), sys::Lock::Shared)? {
                continue;
            }
            let id = (m.dev(), m.ino());
            if sys::id_at(b, &name).ok() != Some(id) {
                continue;
            }
            return Ok(Dir {
                name,
                fd,
                id,
                next: AtomicU64::new(0),
                shared: Mutex::new(HashMap::new()),
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free name for D",
        ))
    }

    /// D's fd (the base of thread-cwd and at).
    #[cfg_attr(
        not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd"
        )),
        allow(dead_code)
    )]
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The E the default binds (those that return no `SocketFile`) share in
    /// D through link-dir: the real path of a socket's directory to the name
    /// of the counted E pointing to it, one per directory (hub SPEC-routes
    /// "link-dir"). The caller holds the lock while it looks an E up and
    /// makes one when there is none, so two binds at once never make two E
    /// for one directory; it is not held across the bind (a shared E is
    /// never removed, so no bind has to wait for another). A D made again
    /// starts with an empty table (the table is D's).
    pub(crate) fn shared(&self) -> MutexGuard<'_, HashMap<PathBuf, OsString>> {
        lock(&self.shared)
    }

    /// Whether the E named `name` in D is still a symlink pointing to
    /// `target` (`readlinkat` on D's fd): one removed, or replaced by
    /// something else, is not to be used again (hub SPEC-routes "link-dir").
    pub(crate) fn points_to(&self, name: &OsStr, target: &Path) -> bool {
        sys::read_link_at(self.fd.as_fd(), name).is_ok_and(|to| to == target)
    }

    /// The length of the name the next counted E gets (it only grows).
    pub(crate) fn next_e_len(&self) -> usize {
        counted_name(self.next.load(Ordering::Relaxed)).len()
    }

    /// Make the next counted E in D, pointing to `target`, kept until
    /// [`Link::cleanup`].
    pub(crate) fn link(self: &Arc<Self>, target: &Path) -> io::Result<Link> {
        for _ in 0..ATTEMPTS {
            let name = OsString::from(counted_name(self.next.fetch_add(1, Ordering::Relaxed)));
            match sys::symlink_at(target, self.fd.as_fd(), &name) {
                Ok(()) => {
                    return Ok(Link {
                        dir: Arc::clone(self),
                        name,
                        done: Mutex::new(false),
                        #[cfg(test)]
                        unlinks: AtomicU64::new(0),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free name for E",
        ))
    }
}

/// A counted E in D (the E of resolve and of a bind that keeps its
/// spelling). Dropping it leaves it in place: only [`Link::cleanup`] removes
/// it, once.
#[derive(Debug)]
pub(crate) struct Link {
    dir: Arc<Dir>,
    name: OsString,
    /// Whether E has been removed; held while it is being removed, so the
    /// removal is one at a time.
    done: Mutex<bool>,
    /// Test-only: how many times [`Link::cleanup`] has unlinked E.
    #[cfg(test)]
    pub(crate) unlinks: AtomicU64,
}

impl Link {
    /// E's name in D.
    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    /// `D/E`: the part of the spelling below B.
    pub(crate) fn below_b(&self) -> PathBuf {
        Path::new(&self.dir.name).join(&self.name)
    }

    /// Remove E; once it is removed, later calls do nothing. E already gone
    /// is not an error. A failure is returned, and a later call tries again
    /// (hub SPEC "remove と cleanup": a failure remembered would leave no way
    /// to clean up). Calls at once remove it one at a time.
    pub(crate) fn cleanup(&self) -> io::Result<()> {
        let mut done = lock(&self.done);
        if *done {
            return Ok(());
        }
        #[cfg(test)]
        self.unlinks.fetch_add(1, Ordering::Relaxed);
        match unlink_e(self.dir.fd.as_fd(), &self.name) {
            Err(e) if e.raw_os_error() != Some(libc::ENOENT) => Err(e),
            _ => {
                *done = true;
                Ok(())
            }
        }
    }
}

/// Remove the E named `name` in `dir` (D, or B for the plain connect's).
fn unlink_e(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_UNLINK_E.get() {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    sys::unlink_at(dir, name, false)
}

#[cfg(test)]
thread_local! {
    /// Test-only: `true` makes every removal of an E on this thread fail
    /// (`EPERM`, as for an immutable E) and leave the E, as a removal refused
    /// by the system would.
    // The initializer is `const`; clippy on illumos says otherwise of the
    // macro's expansion there.
    #[allow(clippy::missing_const_for_thread_local)]
    pub(crate) static FAIL_UNLINK_E: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// An E with a random name, removed when this is dropped: the E of
/// thread-cwd / at (in D) and of the plain connect's link-file (in B).
pub(crate) struct TempLink<'a> {
    dir: BorrowedFd<'a>,
    pub(crate) name: OsString,
}

impl<'a> TempLink<'a> {
    /// Make a symlink with a fresh random name in the directory `dir`,
    /// pointing to `target`.
    pub(crate) fn make(dir: BorrowedFd<'a>, target: &Path) -> io::Result<Self> {
        for _ in 0..ATTEMPTS {
            let name = OsString::from(random_name()?);
            match sys::symlink_at(target, dir, &name) {
                Ok(()) => return Ok(Self { dir, name }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free name for E",
        ))
    }
}

impl Drop for TempLink<'_> {
    fn drop(&mut self) {
        // Best effort: the call is made by then. Nobody else can create or
        // remove entries in B or D, so the name is still this E.
        let _ = unlink_e(self.dir, &self.name);
    }
}

/// `<proc>/<pid>/fd/<B's fd>` when it names B (the same `(st_dev, st_ino)`):
/// the procB form of B's spelling. `None` without a usable `/proc`, or when
/// it is not this process's (a `/proc` of another PID namespace).
pub(crate) fn proc_spelling(proc: &Path, b: &Base) -> Option<PathBuf> {
    let p = proc
        .join(sys::process_id().to_string())
        .join("fd")
        .join(b.fd.as_raw_fd().to_string());
    let m = std::fs::metadata(&p).ok()?;
    ((m.dev(), m.ino()) == b.id).then_some(p)
}

// ---------------------------------------------------------------------------
// sweep
// ---------------------------------------------------------------------------

/// Remove the D whose owner is gone (no `flock` held on it) in each B of the
/// places 1-3 by its default name that is there (hub SPEC "`sweep`"): none is
/// made, and the Bs in sockets' directories are left alone.
pub(crate) fn sweep(places: &[(Place, PathBuf)]) -> io::Result<()> {
    for (place, parent) in places {
        let name = OsString::from(place.b_name());
        // A place without B, or whose B fails the checks, has nothing of
        // this user's to sweep.
        if let Ok(b) = Base::make(parent, &name, *place, Existing::FindOnly, false) {
            sweep_in(&b)?;
        }
    }
    Ok(())
}

/// [`sweep`] of the B at `base` (an `Options.base`), which must pass the
/// checks of [`given`]. A parent or B that is not there has nothing to sweep:
/// that is success, and nothing is made (hub SPEC "`sweep`").
///
/// # Errors
///
/// Those of [`given`] finding it (never making it), other than its not being
/// there, and of listing it.
pub(crate) fn sweep_base(base: &Path) -> io::Result<()> {
    let (parent, name) = split_given(base)?;
    match Base::make(parent, name, Place::Given, Existing::FindOnly, false).map_err(io::Error::from)
    {
        Ok(b) => sweep_in(&b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The D of this process in a registered B at the same place as `b`.
fn own_d_at(b: &Base) -> Option<OsString> {
    lock(&REGISTRY)
        .all
        .iter()
        .find(|r| r.id == b.id)
        .and_then(|r| r.own_d())
}

/// Each directory of this user in `b` named in the form of a D, other than
/// this process's D, that an exclusive `flock` can be taken on has its
/// entries and then itself removed. Another process's live D keeps its
/// shared lock and is skipped. B is sockat's own, yet anything else in it is
/// left alone: only what has the form of a D is sockat's (hub SPEC "`sweep`").
fn sweep_in(b: &Base) -> io::Result<()> {
    let own = own_d_at(b);
    for entry in std::fs::read_dir(&b.path)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if !is_random_name(&name) || own.as_deref() == Some(name.as_os_str()) {
            continue;
        }
        let Ok(st) = sys::stat_at(b.fd.as_fd(), &name) else {
            continue;
        };
        if !st.is_dir || st.uid != sys::effective_uid() {
            continue;
        }
        let Ok(d) = sys::open_dir_at(b.fd.as_fd(), &name, libc::O_RDONLY) else {
            continue;
        };
        if !sys::lock(d.as_fd(), sys::Lock::Exclusive).unwrap_or(false) {
            continue;
        }
        let Ok(inside) = std::fs::read_dir(b.path.join(&name)) else {
            continue;
        };
        let inside: Vec<OsString> = inside.flatten().map(|e| e.file_name()).collect();
        // A D with anything but symlinks (E) in it is not one sockat left:
        // leave it.
        let only_links = inside
            .iter()
            .all(|e| sys::stat_at(d.as_fd(), e).is_ok_and(|st| st.is_symlink));
        if !only_links {
            continue;
        }
        for e in &inside {
            let _ = sys::unlink_at(d.as_fd(), e, false);
        }
        let _ = sys::unlink_at(b.fd.as_fd(), &name, true);
    }
    Ok(())
}

/// Open D (named `name` in the B held by `b`) to hold it.
fn open_d(b: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    #[cfg(test)]
    if let Some(tries) = FAIL_D.get() {
        FAIL_D.set(Some(tries + 1));
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    sys::open_dir_at(b, name, libc::O_RDONLY)
}

#[cfg(test)]
thread_local! {
    /// Test-only: `Some(n)` makes this thread's every D fail to open right
    /// after it is made and its mode set (`EACCES`), counting the tries in
    /// `n`. Per thread, so the tests running beside it are not touched.
    // The initializer is `const`; clippy on illumos says otherwise of the
    // macro's expansion there.
    #[allow(clippy::missing_const_for_thread_local)]
    pub(crate) static FAIL_D: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The number of the fd this process holds on the registered B at `path`
/// (test-only: stands in for `/proc/<pid>/fd/<B's fd>` where there is no
/// `/proc`).
#[cfg(test)]
pub(crate) fn registered_fd(path: &Path) -> Option<std::os::fd::RawFd> {
    lock(&REGISTRY)
        .all
        .iter()
        .find(|b| b.path == path)
        .map(|b| b.fd.as_raw_fd())
}

/// The number of the fd this process holds on its D in the registered B at
/// `path` (test-only).
#[cfg(test)]
pub(crate) fn registered_d_fd(path: &Path) -> Option<std::os::fd::RawFd> {
    let registry = lock(&REGISTRY);
    let b = registry.all.iter().find(|b| b.path == path)?;
    lock(&b.d).as_ref().map(|d| d.fd.as_raw_fd())
}
