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

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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
    pub(crate) fn name(self) -> &'static str {
        match self {
            Route::Standard => "standard",
            Route::LinkDir => "link-dir",
            Route::LinkFile => "link-file",
        }
    }
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

/// The warning for a path to the socket file given without `--for` where a
/// bind does not follow a symlink (cli.md "ファイル名が長い時").
const NOT_FOR_BIND: &str = "this path is for connect; not recommended for bind (bind does not follow the symlink on this OS)";

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

    fn usage(message: String) -> Self {
        Self {
            message,
            usage: true,
        }
    }
}

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
/// for a relative path).
pub(crate) fn resolve(
    path: &Path,
    for_: Option<Use>,
    base: Option<&Path>,
    env: &Env,
) -> Result<Resolved, Failure> {
    if path.as_os_str().is_empty() {
        return Err(Failure::usage(
            "the path is empty\n  give the socket's path".to_owned(),
        ));
    }
    if let Some(base) = base {
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
    let standard = |p: PathBuf| Resolved {
        path: p,
        route: Route::Standard,
        link: None,
        bind: true,
        warnings: Vec::new(),
    };
    if path.is_absolute() && len(path) <= entry::LIMIT {
        return Ok(standard(path.to_path_buf()));
    }
    let real_dir = socket_dir(dir)?;
    // The candidates of an absolute path naming the same place, the real path
    // first (hub DR-0019 2-3): the real path for any path too long as given
    // (an absolute one may look long only through a symlink on the way),
    // `$PWD`'s spelling for a relative one only.
    let real = real_dir.join(name);
    if len(&real) <= entry::LIMIT {
        return Ok(standard(real));
    }
    if !path.is_absolute() {
        if let Some(p) = from_pwd(path, dir, env.pwd.as_deref()) {
            if len(&p) <= entry::LIMIT {
                return Ok(standard(p));
            }
        }
    }
    through_base(&real_dir, name, for_, base, env)
}

/// The spelling of the relative `path` from `$PWD` (`pwd`), `.` and `..`
/// folded by their letters (hub DR-0019 2): only when `$PWD` is absolute and
/// is the cwd (the same `(st_dev, st_ino)` as `.`), and the folded directory
/// is the directory of `path` (`dir`) to the kernel. A `..` after a symlink
/// is the parent of the symlink's target to the kernel, which the folding
/// does not know, and then the folded spelling names another place.
fn from_pwd(path: &Path, dir: &Path, pwd: Option<&OsStr>) -> Option<PathBuf> {
    let pwd = Path::new(pwd?);
    if !pwd.is_absolute() {
        return None;
    }
    let id = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    if id(pwd)? != id(Path::new("."))? {
        return None;
    }
    let mut parts: Vec<&[u8]> = Vec::new();
    let joined = [pwd.as_os_str().as_bytes(), path.as_os_str().as_bytes()];
    for part in joined.iter().flat_map(|b| b.split(|&c| c == b'/')) {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut folded = Vec::new();
    for part in &parts {
        folded.push(b'/');
        folded.extend_from_slice(part);
    }
    let folded = PathBuf::from(OsStr::from_bytes(&folded));
    (id(folded.parent()?)? == id(dir)?).then_some(folded)
}

/// The path through an E right in B to the socket's directory (`real_dir`,
/// its real path), else to the socket file for the use asked for (hub
/// DR-0018 3-7).
fn through_base(
    real_dir: &Path,
    name: &OsStr,
    for_: Option<Use>,
    base: Option<&Path>,
    env: &Env,
) -> Result<Resolved, Failure> {
    let name_len = name.len();
    // The E pointing to the socket file is for a connect anywhere, and for a
    // bind only where a bind follows a symlink; without `--for` it is given
    // for a connect (kawaz CL-Q1 = b). B is made only when a path for this
    // use can fit through it, so the error of a bind the E to the file cannot
    // serve is told before anything is made.
    let file_usable = match for_ {
        Some(Use::Connect) | None => true,
        Some(Use::Bind) => entry::BIND_FOLLOWS_SYMLINK,
    };
    let usable = |b: &Path| {
        through_dir(b, name_len) <= entry::LIMIT || (file_usable && through_file(b) <= entry::LIMIT)
    };
    // The B (made or to be made) the library measured last: the one the
    // error is about when no path fits through it.
    let offered = RefCell::new(None::<PathBuf>);
    let fits = |b: &Path| {
        *offered.borrow_mut() = Some(b.to_path_buf());
        usable(b)
    };
    let b = choose_base(base, &env.places, fits)?;
    let Some(b) = b.filter(|b| usable(b.path())) else {
        return Err(match offered.take() {
            Some(b) => not_for_this_use(&b, name_len, for_),
            None => too_long(name_len, None),
        });
    };
    let b = b.path();
    if through_dir(b, name_len) <= entry::LIMIT {
        let e = link(b, &hash(real_dir), real_dir)?;
        return Ok(Resolved {
            path: e.join(name),
            route: Route::LinkDir,
            link: Some((e, real_dir.to_path_buf())),
            bind: true,
            warnings: Vec::new(),
        });
    }
    let file = real_dir.join(name);
    let e = link(b, &hash(&file), &file)?;
    Ok(Resolved {
        path: e.clone(),
        route: Route::LinkFile,
        link: Some((e, file)),
        bind: entry::BIND_FOLLOWS_SYMLINK,
        // Without `--for` the path is for a connect (kawaz CL-Q1 = b).
        warnings: match (for_, entry::BIND_FOLLOWS_SYMLINK) {
            (None, false) => vec![NOT_FOR_BIND],
            _ => Vec::new(),
        },
    })
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

/// B: the one at `--base` (checked before anything is made, and made only
/// when `fits` holds for it), else the process's B of the places 1-3.
/// `None` when `fits` does not hold for the B (nothing is made then).
fn choose_base(
    base: Option<&Path>,
    places: &entry::Places,
    fits: impl Fn(&Path) -> bool,
) -> Result<Option<entry::Base>, Failure> {
    let Some(base) = base else {
        return match entry::automatic(places, fits) {
            entry::Found::Base(b) => Ok(Some(b)),
            entry::Found::TooLong => Ok(None),
            entry::Found::Missing { occupied } => Err(Failure::new(format!(
                "no base directory could be made in /tmp, $XDG_RUNTIME_DIR or $TMPDIR{}\n  pass --base with a directory to make it in",
                match occupied {
                    true => " (a default name there is taken by something that fails the checks)",
                    false => "",
                }
            ))),
        };
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
    entry::given(base).map(Some).map_err(refused)
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

/// The E named `name` right in B (`b`, B's real path) pointing to `target`
/// (hub DR-0018 4): used when it is there and points to `target`, made when
/// it is not there, and replaced by one with a random name renamed over it
/// when it points elsewhere.
// Design rationale: E is made by its path in B rather than through B's fd
// (`symlinkat` / `renameat`): this package holds no `unsafe` and std has no
// `*at` call, and B is a directory of this user's the library has checked
// (no other user can change the directories on the way, B is this user's and
// not writable by group and others), so nobody else can swap what the path
// names meanwhile.
fn link(b: &Path, name: &str, target: &Path) -> Result<PathBuf, Failure> {
    let e = b.join(name);
    let failed = |err: io::Error| {
        Failure::new(format!(
            "cannot make the link {}: {err}\n  look at what is at that name in the base",
            e.display()
        ))
    };
    match std::os::unix::fs::symlink(target, &e) {
        Ok(()) => return Ok(e),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(failed(err)),
    }
    if std::fs::read_link(&e).is_ok_and(|to| to == target) {
        return Ok(e);
    }
    for _ in 0..ATTEMPTS {
        // `<hash>.<10 random>`: not the form of a D (10 letters and digits),
        // so `sweep` never takes it.
        let mut temp = name.to_owned().into_bytes();
        temp.push(b'.');
        temp.extend_from_slice(entry::random_name().map_err(failed)?.as_bytes());
        let temp = b.join(OsStr::from_bytes(&temp));
        match std::os::unix::fs::symlink(target, &temp) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(failed(err)),
        }
        return match std::fs::rename(&temp, &e) {
            Ok(()) => Ok(e),
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
