//! `resolve`: the path to print for a socket path (hub DR-0018 "resolve の既
//! 定の綴り", docs/design/cli.md).
//!
//! 1. A path that fits the socket address limit (once made absolute) is the
//!    path itself.
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

use std::ffi::OsStr;
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
    /// The path given (made absolute).
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
}

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
}

/// The path to print for `path` (hub DR-0018 "resolve の既定の綴り").
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
    let absolute = absolute(path)?;
    if len(&absolute) <= entry::LIMIT {
        return Ok(Resolved {
            path: absolute,
            route: Route::Standard,
            link: None,
            bind: true,
        });
    }
    let (dir, name) = split(&absolute)?;
    let real_dir = socket_dir(dir)?;
    let name_len = name.len();
    let through_dir = |b: &Path| len(b) + 1 + HASH_LEN + 1 + name_len;
    let through_file = |b: &Path| len(b) + 1 + HASH_LEN;
    // The E pointing to the socket file is counted whatever the use, so B is
    // chosen the same way for every use, and a path that only it would fit
    // gets the error that says why it cannot be had for that use (no
    // `--for`, or a bind where a bind does not follow a symlink) rather than
    // that nothing fits.
    let fits = |b: &Path| through_dir(b) <= entry::LIMIT || through_file(b) <= entry::LIMIT;
    let b = choose_base(base, &env.places, fits, name_len)?;
    let b = b.path();
    if through_dir(b) <= entry::LIMIT {
        let e = link(b, &hash(&real_dir), &real_dir)?;
        return Ok(Resolved {
            path: e.join(name),
            route: Route::LinkDir,
            link: Some((e, real_dir)),
            bind: true,
        });
    }
    let room = entry::LIMIT.saturating_sub(len(b) + 1 + HASH_LEN + 1);
    if through_file(b) > entry::LIMIT {
        return Err(too_long(name_len, Some(room)));
    }
    let bind = entry::BIND_FOLLOWS_SYMLINK;
    match for_ {
        None if bind => {
            return Err(Failure::new(format!(
                "the file name is too long for a path that works for both bind and connect ({name_len} bytes; up to {room} here)\n  pass --for connect or --for bind to get a path for that use"
            )));
        }
        None => {
            return Err(Failure::new(format!(
                "the file name is too long for a path that works for both bind and connect ({name_len} bytes; up to {room} here)\n  pass --for connect to get a path to connect to (a bind does not follow a symlink on this system)"
            )));
        }
        Some(Use::Bind) if !bind => {
            return Err(Failure::new(format!(
                "the file name is too long for a path to bind on this system ({name_len} bytes; up to {room} here; a bind does not follow a symlink)\n  use a shorter file name, or a --base with a shorter path"
            )));
        }
        Some(_) => {}
    }
    let file = real_dir.join(name);
    let e = link(b, &hash(&file), &file)?;
    Ok(Resolved {
        path: e.clone(),
        route: Route::LinkFile,
        link: Some((e, file)),
        bind,
    })
}

/// B: the one at `--base` (checked before anything is made, and made only
/// when a spelling through it can fit), else the process's B of the places
/// 1-3.
fn choose_base(
    base: Option<&Path>,
    places: &entry::Places,
    fits: impl Fn(&Path) -> bool,
    name_len: usize,
) -> Result<entry::Base, Failure> {
    let Some(base) = base else {
        return match entry::automatic(places, fits) {
            entry::Found::Base(b) => Ok(b),
            entry::Found::TooLong => Err(too_long(name_len, None)),
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
        let room = entry::LIMIT.saturating_sub(real.as_os_str().len() + 1 + HASH_LEN + 1);
        return Err(too_long(name_len, Some(room)));
    }
    entry::given(base).map_err(refused)
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

/// `path` made absolute by the current directory put in front of it, as
/// written: `.` and `..` are not folded (hub SPEC: a path is not folded by
/// its letters; a `..` after a symlink goes to the symlink's target's parent).
fn absolute(path: &Path) -> Result<PathBuf, Failure> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| {
        Failure::new(format!(
            "cannot read the current directory: {e}\n  give an absolute path"
        ))
    })?;
    // `join` puts one `/` between them and changes nothing else.
    Ok(cwd.join(path))
}

/// The directory and the file name of an absolute path, split at its last
/// `/` (the library's split): it must end with a name, not `/`, `.` or `..`.
fn split(path: &Path) -> Result<(&Path, &OsStr), Failure> {
    let bytes = path.as_os_str().as_bytes();
    let (dir, name) = match bytes.iter().rposition(|&b| b == b'/') {
        Some(0) => (&b"/"[..], &bytes[1..]),
        Some(i) => (&bytes[..i], &bytes[i + 1..]),
        None => (&b"."[..], bytes),
    };
    if name.is_empty() || name == b"." || name == b".." {
        return Err(Failure::usage(format!(
            "the path must end with a file name (not '/', '.' or '..'): {}\n  give the socket file's path",
            path.display()
        )));
    }
    Ok((Path::new(OsStr::from_bytes(dir)), OsStr::from_bytes(name)))
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
