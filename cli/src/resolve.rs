//! `resolve`: the path to print for a socket path (hub DR-0018 "resolve の既
//! 定の綴り", docs/design/cli.md).
//!
//! 1. An absolute path that fits the socket address limit is the path
//!    itself. Otherwise (and for any relative path) an absolute path to the
//!    same place is tried (hub DR-0019 "絶対パスの作り方と、その後"): its
//!    real path, else for a relative path its spelling from `$PWD` with `.`
//!    and `..` folded (when that names the same directory), whichever fits
//!    first; nothing is made for it.
//! 2. Otherwise the socket's directory is looked up (`ENOENT`, `ENOTDIR`).
//! 3. B is the process's B of the places 1-3 or the one at `--base`, made
//!    and checked by the library (`sockat::__cli`).
//! 4. Right in B, an E named by the hash of the directory's real path points
//!    to that directory: made when missing, replaced when it points elsewhere.
//! 5. `B/<hash>/<file name>` when it fits, for any use.
//! 6. Else `B/<hash>`, an E pointing to the socket file, for the use asked
//!    for (`--for`): a connect anywhere, a bind only where a bind follows a
//!    symlink.
//! 7. Else nothing fits.
//!
//! `--route` names which of those to try and in what order (`standard` for
//! 1, `link-dir` for 5, `link-file` for 6): one that cannot be used for the
//! path is passed over (DR-0034). `--base-fd` and `--base-pid` give B as a
//! directory another process holds open, spelled `/proc/<pid>/fd/<fd>`.

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sockat::__cli as entry;

/// What the path is to be used for (`--for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Use {
    Bind,
    Connect,
}

/// How the path printed was made (the JSON's `route`; diagnostics only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// The path given, or for a relative path an absolute path to the same
    /// place (no E).
    Standard,
    /// Through an E pointing to the socket's directory.
    LinkDir,
    /// Through an E pointing to the socket file.
    LinkFile,
}

impl Route {
    /// The order of `resolve` (hub DR-0018 1-7): what `--route default`, and
    /// no `--route`, try.
    pub(crate) const ORDER: [Route; 3] = [Route::Standard, Route::LinkDir, Route::LinkFile];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Route::Standard => "standard",
            Route::LinkDir => "link-dir",
            Route::LinkFile => "link-file",
        }
    }

    /// The route named `name` (`--route`), `None` for another name.
    pub(crate) fn named(name: &str) -> Option<Route> {
        Route::ORDER.into_iter().find(|r| r.name() == name)
    }
}

/// Where B is (`--base`, `--base-fd` and `--base-pid`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum BaseArg<'a> {
    /// The process's B of the places 1-3.
    Places,
    /// B itself at this path (`--base`, checked as `Options.base`).
    Dir(&'a Path),
    /// A directory the process `pid` holds open by its fd `fd`, spelled
    /// `/proc/<pid>/fd/<fd>` (Linux).
    Held { fd: u32, pid: u32 },
}

/// The path printed and how it was made.
#[derive(Debug)]
pub(crate) struct Resolved {
    pub(crate) path: PathBuf,
    pub(crate) route: Route,
    /// The E gone through and where it points (none for `Standard`).
    pub(crate) link: Option<(PathBuf, PathBuf)>,
    /// Whether the path works for a bind (it always works for a connect).
    pub(crate) bind: bool,
    /// What to warn of (cli.md "ファイル名が長い時"): on the standard error,
    /// or in the JSON's `warnings`.
    pub(crate) warnings: Vec<&'static str>,
}

/// The warning for a path to the socket file given without `--for` (cli.md
/// "ファイル名が長い時"): on every OS it is not recommended for a bind,
/// for the reason of the OS. Where a bind does not follow a symlink, the
/// bind fails; where it does, unlinking the path removes only the symlink
/// and leaves the socket file (the spelling is not removable).
const NOT_FOR_BIND: &str = match entry::BIND_FOLLOWS_SYMLINK {
    false => {
        "this path is for connect; not recommended for bind (bind does not follow the symlink on this OS)"
    }
    true => {
        "this path is for connect; not recommended for bind (unlinking this path removes only the symlink, not the socket file)"
    }
};

/// The warning for a path to the socket file given for `--for bind`, where
/// a bind follows a symlink: the bind works, its unlink does not remove the
/// socket file.
const UNLINK_LEAVES_THE_SOCKET: &str = "unlinking this path removes only the symlink, not the socket file (remove the socket by its original path)";

/// Why no path was printed: the message (the cause, then what to do next)
/// and whether it is an error in the arguments (exit code 2) rather than a
/// path that could not be made (exit code 1).
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) message: String,
    pub(crate) usage: bool,
}

impl Failure {
    fn new(message: String) -> Self {
        Self {
            message,
            usage: false,
        }
    }

    pub(crate) fn usage(message: String) -> Self {
        Self {
            message,
            usage: true,
        }
    }
}

/// Why a route named by `--route` was passed over: the path through it does
/// not fit.
const TOO_LONG: &str = "too long";

/// Why `link-file` was passed over for `--for bind` where a bind does not
/// follow a symlink.
const NOT_FOLLOWED: &str = "bind does not follow the symlink on this OS";

/// The length of the hash an E is named by (hub DR-0018 4).
const HASH_LEN: usize = 12;

/// How many random names to try for the E that replaces one pointing
/// elsewhere. Each is fresh, so running out means something else is wrong.
const ATTEMPTS: usize = 16;

/// What [`resolve`] reads from where it runs.
pub(crate) struct Env {
    pub(crate) places: entry::Places,
    /// `$PWD`.
    pub(crate) pwd: Option<OsString>,
}

/// The path to print for `path` (hub DR-0018 "resolve の既定の綴り", DR-0019
/// for a relative path), by the routes `routes` in that order when given
/// (`--route`), else by [`Route::ORDER`].
pub(crate) fn resolve(
    path: &Path,
    for_: Option<Use>,
    base: BaseArg<'_>,
    routes: Option<&[Route]>,
    env: &Env,
) -> Result<Resolved, Failure> {
    if path.as_os_str().is_empty() {
        return Err(Failure::usage(
            "the path is empty\n  give the socket's path".to_owned(),
        ));
    }
    if let BaseArg::Dir(base) = base {
        entry::check_base_form(base).map_err(|_| {
            Failure::usage(format!(
                "--base must be an absolute path ending with a name: {}\n  give the base directory itself, for example --base /tmp/my-app/sockat",
                base.display()
            ))
        })?;
    }
    // Whatever its length, as the library does (hub SPEC "エラー").
    let (dir, name) = entry::split(path).map_err(|_| {
        Failure::usage(format!(
            "the path must end with a file name (not '/', '.' or '..'): {}\n  give the socket file's path",
            path.display()
        ))
    })?;
    let order = routes.unwrap_or(&Route::ORDER);
    let name_len = name.len();
    // The E pointing to the socket file is for a connect anywhere, and for a
    // bind only where a bind follows a symlink; without `--for` it is given
    // for a connect (kawaz CL-Q1 = b). B is made only when a path through a
    // route named can fit through it, so the error of a bind the E to the
    // file cannot serve is told before anything is made.
    let file_usable = match for_ {
        Some(Use::Connect) | None => true,
        Some(Use::Bind) => entry::BIND_FOLLOWS_SYMLINK,
    };
    let dir_named = order.contains(&Route::LinkDir);
    let file_named = file_usable && order.contains(&Route::LinkFile);
    let usable = |b: &Path| {
        (dir_named && through_dir(b, name_len) <= entry::LIMIT)
            || (file_named && through_file(b) <= entry::LIMIT)
    };
    // The B (made or to be made) last measured: the one the error is about
    // when no path fits through it.
    let offered = RefCell::new(None::<PathBuf>);
    let fits = |b: &Path| {
        *offered.borrow_mut() = Some(b.to_path_buf());
        usable(b)
    };
    let mut real_dir: Option<PathBuf> = None;
    // B, chosen when the first route through it is tried (`None` in it when
    // no path through it fits: nothing is made then).
    let mut chosen: Option<Option<Chosen>> = None;
    let mut skipped: Vec<(Route, &'static str)> = Vec::new();
    for &route in order {
        if route == Route::Standard && path.is_absolute() && len(path) <= entry::LIMIT {
            return Ok(standard(path.to_path_buf()));
        }
        let real_dir = match &real_dir {
            Some(d) => d,
            None => real_dir.insert(socket_dir(dir)?),
        };
        match route {
            Route::Standard => match candidate(path, dir, name, real_dir, env) {
                Some(p) => return Ok(standard(p)),
                None => skipped.push((route, TOO_LONG)),
            },
            Route::LinkFile if !file_usable => skipped.push((route, NOT_FOLLOWED)),
            Route::LinkDir | Route::LinkFile => {
                let b = match &chosen {
                    Some(b) => b,
                    None => chosen.insert(choose_base(base, &env.places, fits)?),
                };
                let made = match (route, b) {
                    (Route::LinkDir, Some(b))
                        if through_dir(b.spelled(), name_len) <= entry::LIMIT =>
                    {
                        Some(through_dir_e(b, real_dir, name)?)
                    }
                    (Route::LinkFile, Some(b)) if through_file(b.spelled()) <= entry::LIMIT => {
                        Some(through_file_e(b, &real_dir.join(name), for_)?)
                    }
                    _ => None,
                };
                match made {
                    Some(resolved) => return Ok(resolved),
                    None => skipped.push((route, TOO_LONG)),
                }
            }
        }
    }
    Err(match (routes, offered.take()) {
        (Some(_), _) => none_usable(&skipped),
        (None, Some(b)) => not_for_this_use(&b, name_len, for_),
        (None, None) => too_long(name_len, None),
    })
}

/// The path given, or an absolute path naming the same place (no E).
fn standard(p: PathBuf) -> Resolved {
    Resolved {
        path: p,
        route: Route::Standard,
        link: None,
        bind: true,
        warnings: Vec::new(),
    }
}

/// An absolute path naming the same place as the long or relative `path`
/// (whose directory is `dir`, its real path `real_dir`) that fits, the real
/// path first (hub DR-0019 2-3): the real path for any path too long as given
/// (an absolute one may look long only through a symlink on the way),
/// `$PWD`'s spelling for a relative one only.
fn candidate(path: &Path, dir: &Path, name: &OsStr, real_dir: &Path, env: &Env) -> Option<PathBuf> {
    let real = real_dir.join(name);
    if len(&real) <= entry::LIMIT {
        return Some(real);
    }
    if path.is_absolute() {
        return None;
    }
    entry::pwd_candidate(path, dir, env.pwd.as_deref()).filter(|p| len(p) <= entry::LIMIT)
}

/// The path through an E right in B to the socket's directory (`real_dir`,
/// its real path): `B/<hash>/<file name>` (hub DR-0018 4-5).
fn through_dir_e(b: &Chosen, real_dir: &Path, name: &OsStr) -> Result<Resolved, Failure> {
    let e = link(b, &hash(real_dir), real_dir)?;
    Ok(Resolved {
        path: e.join(name),
        route: Route::LinkDir,
        link: Some((e, real_dir.to_path_buf())),
        bind: true,
        warnings: Vec::new(),
    })
}

/// The path through an E right in B to the socket file (`file`, by the real
/// path of its directory): `B/<hash>`, for the use asked for (hub DR-0018 6).
fn through_file_e(b: &Chosen, file: &Path, for_: Option<Use>) -> Result<Resolved, Failure> {
    let e = link(b, &hash(file), file)?;
    Ok(Resolved {
        path: e.clone(),
        route: Route::LinkFile,
        link: Some((e, file.to_path_buf())),
        bind: entry::BIND_FOLLOWS_SYMLINK,
        // Without `--for` the path is for a connect (kawaz CL-Q1 = b).
        warnings: match for_ {
            None => vec![NOT_FOR_BIND],
            Some(Use::Bind) => vec![UNLINK_LEAVES_THE_SOCKET],
            Some(Use::Connect) => Vec::new(),
        },
    })
}

/// None of the routes named by `--route` can be used for this path.
fn none_usable(skipped: &[(Route, &str)]) -> Failure {
    let tried: Vec<String> = skipped
        .iter()
        .map(|(r, why)| format!("{} ({why})", r.name()))
        .collect();
    Failure::new(format!(
        "none of the routes named can be used for this path: {}\n  name other routes, or leave out --route",
        tried.join(", ")
    ))
}

/// `B/<hash>/<file name>`'s length through B at `b`.
fn through_dir(b: &Path, name_len: usize) -> usize {
    len(b) + 1 + HASH_LEN + 1 + name_len
}

/// `B/<hash>`'s length through B at `b`.
fn through_file(b: &Path) -> usize {
    len(b) + 1 + HASH_LEN
}

/// No path for this use fits through B at `b`: the E to the socket file
/// would fit but cannot serve a bind here (`--for bind` where a bind does not
/// follow a symlink), or nothing fits at all.
fn not_for_this_use(b: &Path, name_len: usize, for_: Option<Use>) -> Failure {
    let room = entry::LIMIT.saturating_sub(through_dir(b, 0));
    if through_file(b) > entry::LIMIT || for_ != Some(Use::Bind) {
        return too_long(name_len, Some(room));
    }
    Failure::new(format!(
        "the file name is too long for a path to bind on this system ({name_len} bytes; up to {room} here; a bind does not follow a symlink)\n  use a shorter file name, or a --base with a shorter path"
    ))
}

/// A B the E are put right in: where it is spelled in the paths printed, and
/// where E is made in it.
// Design rationale: B held open by another process (`--base-fd`) is
// spelled through that process's fd, `/proc/<pid>/fd/<fd>`, which the command
// does not hold; E is made through the command's own fd of it,
// `/proc/self/fd/<fd>`, so it goes into the directory checked whatever that
// process does with its fd meanwhile.
enum Chosen {
    /// The process's B of the places 1-3, or the one at `--base`.
    Base(entry::Base),
    /// B another process holds open (`--base-fd`, `--base-pid`).
    #[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
    Held {
        spelled: PathBuf,
        made_through: PathBuf,
        /// The command's fd of B, which `made_through` names.
        _dir: std::fs::File,
    },
}

impl Chosen {
    /// B as the paths printed spell it.
    fn spelled(&self) -> &Path {
        match self {
            Chosen::Base(b) => b.path(),
            Chosen::Held { spelled, .. } => spelled,
        }
    }

    /// B as E is made in it.
    fn made_through(&self) -> &Path {
        match self {
            Chosen::Base(b) => b.path(),
            Chosen::Held { made_through, .. } => made_through,
        }
    }
}

/// B: the one at `--base` (checked before anything is made, and made only
/// when `fits` holds for it), the one `--base-fd` gives (used when `fits`
/// holds for its spelling), else the process's B of the places 1-3. `None`
/// when `fits` does not hold for the B (nothing is made then).
fn choose_base(
    base: BaseArg<'_>,
    places: &entry::Places,
    fits: impl Fn(&Path) -> bool,
) -> Result<Option<Chosen>, Failure> {
    let base = match base {
        BaseArg::Places => {
            return match entry::automatic(places, fits) {
                entry::Found::Base(b) => Ok(Some(Chosen::Base(b))),
                entry::Found::TooLong => Ok(None),
                entry::Found::Missing { occupied } => Err(Failure::new(format!(
                    "no base directory could be made in /tmp, $XDG_RUNTIME_DIR or $TMPDIR{}\n  pass --base with a directory to make it in",
                    match occupied {
                        true =>
                            " (a default name there is taken by something that fails the checks)",
                        false => "",
                    }
                ))),
            };
        }
        BaseArg::Held { fd, pid } => {
            let held = held_base(fd, pid)?;
            return Ok(fits(held.spelled()).then_some(held));
        }
        BaseArg::Dir(base) => base,
    };
    let refused = |e: io::Error| {
        Failure::new(format!(
            "cannot use --base {}: {e}\n  give a path whose parent exists and no other user can change, and that is a directory of yours others cannot write to, or is not there yet",
            base.display()
        ))
    };
    entry::check_given(base).map_err(refused)?;
    // The parent is there (checked), so its real path tells where B is.
    let parent = base.parent().unwrap_or(Path::new("/"));
    let real = std::fs::canonicalize(parent)
        .map_err(refused)?
        .join(base.file_name().unwrap_or_default());
    if !fits(&real) {
        return Ok(None);
    }
    entry::given(base)
        .map(|b| Some(Chosen::Base(b)))
        .map_err(refused)
}

/// The error of `--base-fd` on a system without `/proc`: one in the
/// arguments, as the option cannot be used there at all.
pub(crate) fn held_base_unsupported() -> Failure {
    Failure::usage(
        "--base-fd and --base-pid spell the base through /proc, which only Linux has\n  use --base instead".to_owned(),
    )
}

/// B the process `pid` holds open by its fd `fd` (hub DR-0018 "置き場と持ち
/// 主の指定"): opened through `/proc/<pid>/fd/<fd>` (another uid cannot) with
/// the search permission alone, checked as B itself is for `--base` (a
/// directory of this user that group and others cannot write to), and
/// spelled so. `/proc` must name what the
/// fds are open on, as the library uses it only so (hub SPEC "用語").
#[cfg(any(target_os = "linux", target_os = "android"))]
fn held_base(fd: u32, pid: u32) -> Result<Chosen, Failure> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let spelled = PathBuf::from(format!("/proc/{pid}/fd/{fd}"));
    let failed = |e: io::Error| {
        Failure::new(format!(
            "cannot use fd {fd} of process {pid} as the base ({}): {e}\n  give the fd of a directory of yours that the process holds open and group and others cannot write to",
            spelled.display()
        ))
    };
    let dir = entry::open_held(&spelled).map_err(failed)?;
    let made_through = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let held = dir.metadata().map_err(failed)?;
    let names_it = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| (m.dev(), m.ino()) == (held.dev(), held.ino()))
    };
    if !names_it(&made_through) || !names_it(&spelled) {
        return Err(Failure::new(format!(
            "/proc does not name the directory fd {fd} of process {pid} is open on\n  use --base instead"
        )));
    }
    Ok(Chosen::Held {
        spelled,
        made_through,
        _dir: dir,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn held_base(_fd: u32, _pid: u32) -> Result<Chosen, Failure> {
    Err(held_base_unsupported())
}

/// Nothing through B fits. `room`: how long a file name could be through B.
fn too_long(name_len: usize, room: Option<usize>) -> Failure {
    let room = match room {
        Some(r) => format!("; up to {r} here"),
        None => String::new(),
    };
    Failure::new(format!(
        "no path through the base fits the socket address limit ({} bytes) for this file name ({name_len} bytes{room})\n  use a --base with a shorter path, or a shorter file name",
        entry::LIMIT
    ))
}

/// The real path of the socket's directory `dir`, which must be there and be
/// a directory (hub SPEC-routes "共通").
fn socket_dir(dir: &Path) -> Result<PathBuf, Failure> {
    let failed = |e: io::Error| match e.kind() {
        io::ErrorKind::NotFound => Failure::new(format!(
            "the socket's directory is not there: {}\n  make the directory first",
            dir.display()
        )),
        _ => Failure::new(format!(
            "cannot look up the socket's directory {}: {e}",
            dir.display()
        )),
    };
    let m = std::fs::metadata(dir).map_err(failed)?;
    if !m.is_dir() {
        return Err(Failure::new(format!(
            "the socket's directory is not a directory: {}\n  give a path in a directory",
            dir.display()
        )));
    }
    std::fs::canonicalize(dir).map_err(failed)
}

/// The name of the E pointing to `target`: the first [`HASH_LEN`] lowercase
/// hex digits of the SHA-256 of its bytes (hub DR-0018 4).
fn hash(target: &Path) -> String {
    let digest = Sha256::digest(target.as_os_str().as_bytes());
    let mut hex = String::with_capacity(HASH_LEN);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
        if hex.len() >= HASH_LEN {
            break;
        }
    }
    hex.truncate(HASH_LEN);
    hex
}

/// The E named `name` right in B (`b`) pointing to `target` (hub DR-0018
/// 4): used when it is there and points to `target`, made when it is not
/// there, and replaced by one with a random name renamed over it when it
/// points elsewhere. Returns its path as B is spelled.
// Design rationale: E is made by its path in B rather than through B's fd
// (`symlinkat` / `renameat`): this package holds no `unsafe` and std has no
// `*at` call, and B is a directory of this user's the library has checked
// (no other user can change the directories on the way, B is this user's and
// not writable by group and others), so nobody else can swap what the path
// names meanwhile. B another process holds is made in through this command's
// own fd of it, which nobody else can change either.
fn link(b: &Chosen, name: &str, target: &Path) -> Result<PathBuf, Failure> {
    let spelled = b.spelled().join(name);
    let e = b.made_through().join(name);
    let failed = |err: io::Error| {
        Failure::new(format!(
            "cannot make the link {}: {err}\n  look at what is at that name in the base",
            spelled.display()
        ))
    };
    match std::os::unix::fs::symlink(target, &e) {
        Ok(()) => return Ok(spelled),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(failed(err)),
    }
    if std::fs::read_link(&e).is_ok_and(|to| to == target) {
        return Ok(spelled);
    }
    for _ in 0..ATTEMPTS {
        // `<hash>.<10 random>`: not the form of a D (10 letters and digits),
        // so `sweep` never takes it.
        let mut temp = name.to_owned().into_bytes();
        temp.push(b'.');
        temp.extend_from_slice(entry::random_name().map_err(failed)?.as_bytes());
        let temp = b.made_through().join(OsStr::from_bytes(&temp));
        match std::os::unix::fs::symlink(target, &temp) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(failed(err)),
        }
        return match std::fs::rename(&temp, &e) {
            Ok(()) => Ok(spelled),
            Err(err) => {
                let _ = std::fs::remove_file(&temp);
                Err(failed(err))
            }
        };
    }
    Err(failed(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the new link",
    )))
}

/// The length of `p` in bytes.
fn len(p: &Path) -> usize {
    p.as_os_str().len()
}
