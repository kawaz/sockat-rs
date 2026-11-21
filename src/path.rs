//! Pure path arithmetic: splitting a socket path into its directory and file
//! name, and spelling an absolute path relative to a working directory.
//! Nothing here touches the file system.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
#[cfg(any(test, not(target_os = "freebsd")))]
use std::path::{Component, PathBuf};

/// Length of `path` in bytes, which is what `sun_path` limits.
pub(crate) fn len(path: &Path) -> usize {
    path.as_os_str().as_bytes().len()
}

/// Whether `path` is a name in Linux's abstract socket namespace, one that
/// starts with a NUL byte (Linux, Android). It names no file, so none of
/// sockat's routes applies to it; it is handed to std as is at any length.
/// Always `false` on other targets, which have no such namespace.
pub(crate) fn is_abstract(path: &Path) -> bool {
    cfg!(any(target_os = "linux", target_os = "android"))
        && path.as_os_str().as_bytes().first() == Some(&0)
}

/// `InvalidInput` when `path` is empty (hub SPEC "短いパス": an argument
/// error).
///
/// Refused before std sees it, although std would take it on Linux: there a
/// bind with an empty path autobinds to an unnamed abstract address, a socket
/// the caller did not name (and no file `bind_ex` could remember). Like a
/// NUL ([`refuse_nul`]), an exception to handing std every path it accepts.
/// Checked before [`is_abstract`] (an empty path names nothing at all).
pub(crate) fn refuse_empty(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is empty",
        ));
    }
    Ok(())
}

/// `InvalidInput` when `path` contains a NUL byte, at any length.
///
/// The kernel takes a socket path up to its first NUL, so a path with a NUL
/// inside would put the socket (or look it up) at the part before it, not at
/// the path given. Refused before anything is created. A leading NUL on Linux
/// ([`is_abstract`]) is to be dealt with before this.
pub(crate) fn refuse_nul(path: &Path) -> io::Result<()> {
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path must not contain a NUL byte",
        ));
    }
    Ok(())
}

/// Split a socket path into the directory that holds the socket and the
/// socket's file name, by the bytes after the last `/`.
///
/// The split is done on the bytes rather than through [`Path::components`],
/// because `components` drops a trailing `/` and `.`: `a/b/` and `a/b/.` would
/// both come out as file name `b`, while the kernel would resolve them as the
/// directory `a/b` itself. Such paths do not name a socket file, so they are
/// refused rather than silently retargeted.
///
/// | path | directory | file name |
/// |---|---|---|
/// | `/d/e/s.sock` | `/d/e` | `s.sock` |
/// | `/s.sock` | `/` | `s.sock` |
/// | `s.sock` | `.` | `s.sock` |
/// | `d//s.sock` | `d/` | `s.sock` |
/// | `d/`, `d/.`, `d/..`, `..` | (error) | |
///
/// # Errors
///
/// `InvalidInput` when the last segment is empty, `.` or `..`.
pub(crate) fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let bytes = path.as_os_str().as_bytes();
    let (dir, name) = match bytes.iter().rposition(|&b| b == b'/') {
        Some(0) => (&b"/"[..], &bytes[1..]),
        Some(i) => (&bytes[..i], &bytes[i + 1..]),
        None => (&b"."[..], bytes),
    };
    if name.is_empty() || name == b"." || name == b".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path must end with a file name (not '/', '.' or '..')",
        ));
    }
    Ok((Path::new(OsStr::from_bytes(dir)), OsStr::from_bytes(name)))
}

/// The part of `target` below the directory `prefix`, when `target` starts
/// with `prefix` component by component (`/a/b` is a prefix of `/a/b/c` but
/// not of `/a/bc`).
///
/// The result only descends from `prefix` along `target`'s own components, so
/// it resolves through the same symlinks `target` does. Returns `None` when
/// `prefix` is not a proper prefix (including `target == prefix`, which leaves
/// no file name).
#[cfg(any(test, not(target_os = "freebsd")))]
pub(crate) fn below(target: &Path, prefix: &Path) -> Option<PathBuf> {
    if !target.is_absolute() || !prefix.is_absolute() {
        return None;
    }
    let rest = target.strip_prefix(prefix).ok()?;
    if rest.as_os_str().is_empty() {
        return None;
    }
    Some(rest.to_path_buf())
}

/// Spell the absolute path `target` relative to the absolute directory `cwd`:
/// climb with `..` out of the part of `cwd` that `target` does not share, then
/// descend along the rest of `target`.
///
/// This is component arithmetic and is only meaningful when both inputs are
/// real paths (no symlinks, `.` or `..`): `cwd` as `getcwd(3)` reports it and
/// `target` with its directory passed through `realpath(3)`. Then each `..`
/// climbs out of a real directory, which is what the kernel does, and the
/// result names the same file as `target`. The caller still confirms that the
/// result names the same directory before using it (the cwd can change, or be
/// stale).
///
/// Returns `None` when either path is not absolute, or when `target` is `cwd`
/// itself or one of its ancestors (there would be no file name left).
#[cfg(any(test, not(target_os = "freebsd")))]
pub(crate) fn relative(target: &Path, cwd: &Path) -> Option<PathBuf> {
    if !target.is_absolute() || !cwd.is_absolute() {
        return None;
    }
    let target: Vec<Component<'_>> = target.components().collect();
    let cwd: Vec<Component<'_>> = cwd.components().collect();
    let common = target
        .iter()
        .zip(cwd.iter())
        .take_while(|(t, c)| t == c)
        .count();
    if common >= target.len() {
        return None;
    }
    let mut rel = PathBuf::new();
    for _ in common..cwd.len() {
        rel.push("..");
    }
    for c in &target[common..] {
        rel.push(c.as_os_str());
    }
    Some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A NUL anywhere in the path is an argument error: the kernel would stop
    // reading the path at it and create (or look up) the socket at the part
    // before it, somewhere the caller did not name. That holds for a NUL in
    // the directory part, in the file name, at the end and at the start (the
    // Linux abstract namespace, a leading NUL, is set aside by the caller with
    // is_abstract before this is asked). A path without one is left alone, the
    // empty path included (refuse_empty judges that one).
    #[test]
    fn refuse_nul_refuses_a_nul_anywhere() {
        for bad in [&b"/d\0/s.sock"[..], b"/d/s\0.sock", b"/d/s.sock\0", b"\0s"] {
            let p = Path::new(OsStr::from_bytes(bad));
            let e = refuse_nul(p).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{p:?}");
        }
        for good in ["/d/s.sock", "s.sock", ""] {
            refuse_nul(Path::new(good)).unwrap();
        }
    }

    // On Linux and Android a path whose first byte is a NUL names a socket in
    // the abstract namespace, not a file: it is told apart by that first byte
    // only (a NUL later in the path is not one). Elsewhere there is no abstract
    // namespace and no path is one.
    #[test]
    fn is_abstract_is_a_leading_nul_on_linux_only() {
        let linux = cfg!(any(target_os = "linux", target_os = "android"));
        let p = |b: &'static [u8]| Path::new(OsStr::from_bytes(b));
        assert_eq!(is_abstract(p(b"\0name")), linux);
        assert_eq!(is_abstract(p(b"\0")), linux);
        assert!(!is_abstract(p(b"/d/\0name")));
        assert!(!is_abstract(p(b"name")));
        assert!(!is_abstract(p(b"")));
    }

    // `split` decides which directory a long path's socket is created in and
    // under which name, for every route that does not hand the whole path to
    // the kernel (link-dir, thread-cwd, at, proc-direct, at-direct,
    // thread-cwd-direct). A wrong split creates the socket somewhere else than
    // the path says, so each row of the table in its doc comment is pinned
    // here.
    #[test]
    fn split_takes_the_bytes_after_the_last_slash() {
        let cases: &[(&str, &str, &str)] = &[
            // Ordinary absolute and relative paths.
            ("/d/e/s.sock", "/d/e", "s.sock"),
            ("d/s.sock", "d", "s.sock"),
            // A socket directly under the root keeps "/" as its directory
            // (an empty directory would open the cwd instead).
            ("/s.sock", "/", "s.sock"),
            // A bare file name lives in the working directory.
            ("s.sock", ".", "s.sock"),
            // A doubled slash is left in the directory part; the kernel treats
            // "d/" and "d" alike, so the split does not need to normalise it.
            ("d//s.sock", "d/", "s.sock"),
            // `..` inside the directory part is kept verbatim for the kernel to
            // resolve (normalising it lexically would be wrong across symlinks).
            ("/a/../s.sock", "/a/..", "s.sock"),
        ];
        for &(path, dir, name) in cases {
            let (d, n) = split(Path::new(path)).unwrap();
            assert_eq!((d, n), (Path::new(dir), OsStr::new(name)), "{path}");
        }
    }

    // A path whose last segment is empty, "." or ".." names a directory, not a
    // socket file. `Path::file_name` would map "d/" and "d/." to "d"; binding
    // that would create a socket named after the directory in its parent, which
    // is not what the path says. Refusing keeps long paths behaving like the
    // kernel does for short ones (where such a bind fails).
    #[test]
    fn split_refuses_paths_that_do_not_end_with_a_file_name() {
        for path in ["d/", "d/.", "d/..", "..", ".", "/", ""] {
            let err = split(Path::new(path)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{path:?}");
        }
    }

    // `below` cuts a spelling of the cwd off the front of the path. The cut is
    // by components: a byte prefix would turn `/w/abc/s` with cwd `/w/ab` into
    // `c/s`, which names `/w/ab/c/s`, a different file.
    #[test]
    fn below_cuts_the_prefix_by_components() {
        // Below the cwd: the rest of the path, unchanged.
        assert_eq!(
            below(Path::new("/w/a/s.sock"), Path::new("/w")),
            Some(PathBuf::from("a/s.sock"))
        );
        // Directly in the cwd: the file name.
        assert_eq!(
            below(Path::new("/w/s.sock"), Path::new("/w")),
            Some(PathBuf::from("s.sock"))
        );
        // A trailing slash on the prefix is not a component, so it does not
        // stop the match.
        assert_eq!(
            below(Path::new("/w/s.sock"), Path::new("/w/")),
            Some(PathBuf::from("s.sock"))
        );
        // `/w/ab` is a byte prefix of `/w/abc/...` but not a component prefix.
        assert_eq!(below(Path::new("/w/abc/s.sock"), Path::new("/w/ab")), None);
        // Not below at all.
        assert_eq!(below(Path::new("/x/s.sock"), Path::new("/w")), None);
        // The path is the prefix itself: no file name left.
        assert_eq!(below(Path::new("/w"), Path::new("/w")), None);
        // Relative inputs have no defined relation to the cwd here.
        assert_eq!(below(Path::new("w/s"), Path::new("/w")), None);
    }

    // `relative` is what lets a long absolute path fit `sun_path` as a short
    // relative one when the socket is near the working directory. Each row
    // pins one shape of the climb-then-descend spelling.
    #[test]
    fn relative_climbs_out_of_the_unshared_part_of_cwd() {
        let cases: &[(&str, &str, &str)] = &[
            // Below the cwd: no climbing.
            ("/w/a/s.sock", "/w", "a/s.sock"),
            // Directly in the cwd: just the file name.
            ("/w/s.sock", "/w", "s.sock"),
            // A sibling subtree: climb one, descend.
            ("/w/a/s.sock", "/w/b", "../a/s.sock"),
            // Nothing shared but the root: climb all the way up.
            ("/x/s.sock", "/w/b", "../../x/s.sock"),
            // cwd is the root.
            ("/x/s.sock", "/", "x/s.sock"),
            // `.` and doubled slashes in the target are not components, so they
            // do not stop the shared prefix.
            ("/w/./a//s.sock", "/w", "a/s.sock"),
            // The arithmetic is purely by components: a `..` in the target is
            // not interpreted (callers pass real paths, which have none), it
            // just ends the shared prefix.
            ("/w/../w/s.sock", "/w", "../w/s.sock"),
        ];
        for &(target, cwd, want) in cases {
            assert_eq!(
                relative(Path::new(target), Path::new(cwd)),
                Some(PathBuf::from(want)),
                "{target} from {cwd}"
            );
        }
    }

    // Only absolute inputs have a well-defined relation to the cwd, and the
    // result must still end in a file name.
    #[test]
    fn relative_is_none_when_it_cannot_name_a_file_below() {
        // A relative target is already relative to the cwd.
        assert_eq!(relative(Path::new("a/s"), Path::new("/w")), None);
        // A relative cwd is not what getcwd(3) returns.
        assert_eq!(relative(Path::new("/a/s"), Path::new("w")), None);
        // The target is the cwd or an ancestor of it: nothing left to descend to.
        assert_eq!(relative(Path::new("/w"), Path::new("/w")), None);
        assert_eq!(relative(Path::new("/w"), Path::new("/w/x")), None);
    }
}
