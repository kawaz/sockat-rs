//! The command `sockat` run as a process (hub DR-0018, docs/design/cli.md,
//! DR-0033).
//!
//! The places for B are a directory of each test's own: a debug build of the
//! command takes `SOCKAT_CLI_TEST_TMP` in place of `/tmp`, and `TMPDIR` and
//! `XDG_RUNTIME_DIR` are removed. A release build has no such knob, so the
//! tests that need it are ignored there.

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The longest path the kernel takes (`sun_path` less the NUL).
#[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))]
const LIMIT: usize = 107;
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "illumos")))]
const LIMIT: usize = 103;

/// Whether a bind follows a symlink at the last component here.
const BIND_FOLLOWS: bool = cfg!(any(target_vendor = "apple", target_os = "netbsd"));

/// A test's own directory under `/tmp` (short, so its paths are measured),
/// with `places/` standing in for `/tmp` and a directory whose path is too
/// long for the kernel.
struct Case {
    dir: TempDir,
}

impl Case {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("srt-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::create_dir(dir.path().join("places")).unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A directory whose path, with a file name, does not fit.
    fn long_dir(&self, letter: char) -> PathBuf {
        let d = self.path().join(letter.to_string().repeat(120));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The B the command makes in the places: `<places' real path>/sockat-<uid>`.
    fn b(&self) -> PathBuf {
        let places = fs::canonicalize(self.path().join("places")).unwrap();
        let uid = fs::metadata(self.path()).unwrap().uid();
        places.join(format!("sockat-{uid}"))
    }

    /// The command with the places in this case's directory.
    fn sockat(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_sockat"));
        c.env("SOCKAT_CLI_TEST_TMP", self.path().join("places"))
            .env_remove("TMPDIR")
            .env_remove("XDG_RUNTIME_DIR")
            .current_dir(self.path());
        c
    }

    fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Output {
        self.sockat().args(args).output().unwrap()
    }
}

/// The path printed (the line without its newline).
fn printed(out: &Output) -> PathBuf {
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = out
        .stdout
        .strip_suffix(b"\n")
        .expect("a line ending in a newline");
    assert!(!line.contains(&b'\n'), "one line only");
    PathBuf::from(OsStr::from_bytes(line))
}

/// A failure: the exit code, nothing on the standard output, and the message
/// on the standard error.
fn failed(out: &Output, code: i32) -> String {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// The length of the hash an E is named by: 128 bits in hex.
const HASH_LEN: usize = 32;

/// The first [`HASH_LEN`] lowercase hex digits of the SHA-256 of `p`'s bytes.
fn hash(p: &Path) -> String {
    let digest = Sha256::digest(p.as_os_str().as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..HASH_LEN]
        .to_owned()
}

fn len(p: &Path) -> usize {
    p.as_os_str().len()
}

/// A path the kernel takes is printed as it is, with nothing made: the
/// command's promise is a path that fits, not a short one (hub DR-0018
/// "目的", step 1), and the path itself fits up to the limit.
#[test]
fn a_path_that_fits_is_printed_as_it_is() {
    let c = Case::new();
    let short = c.path().join("s.sock");
    assert_eq!(printed(&c.run(&[&short])), short);
    // Exactly the limit: still the path itself.
    let name = "n".repeat(LIMIT - len(c.path()) - 1);
    let at_limit = c.path().join(&name);
    assert_eq!(len(&at_limit), LIMIT);
    assert_eq!(printed(&c.run(&[&at_limit])), at_limit);
    assert!(!c.b().exists(), "nothing is made for a path that fits");
}

/// One byte over the limit goes through B: the limit is `sun_path` less the
/// NUL, the same for every language the path is handed to (hub DR-0018 1).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn one_byte_over_the_limit_goes_through_b() {
    let c = Case::new();
    let fill = "d".repeat(LIMIT + 1 - len(c.path()) - "/s.sock".len() - 1);
    let dir = c.path().join(fill);
    fs::create_dir(&dir).unwrap();
    let over = dir.join("s.sock");
    assert_eq!(len(&over), LIMIT + 1);
    let real = fs::canonicalize(&dir).unwrap();
    let expected = c.b().join(hash(&real)).join("s.sock");
    assert_eq!(printed(&c.run(&[&over])), expected);
}

/// A long absolute path whose real path fits (it looks long only through a
/// symlink on the way) is printed as its real path, nothing made in the
/// places (route `standard`, no E); one whose real path does not fit either
/// goes through E (hub's answer on DR-0019 for the command: the real path is
/// tried for an absolute path too, `$PWD` only for a relative one).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_long_absolute_path_is_its_real_path_when_that_fits() {
    let c = Case::new();
    let real_dir = c.path().join("r");
    fs::create_dir(&real_dir).unwrap();
    let long_link = c.path().join("l".repeat(120));
    std::os::unix::fs::symlink(&real_dir, &long_link).unwrap();
    let original = long_link.join("s.sock");
    assert!(len(&original) > LIMIT);
    let out = c.run(&[OsStr::new("--json"), original.as_os_str()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let real = fs::canonicalize(&real_dir).unwrap().join("s.sock");
    assert_eq!(v["path"], real.to_str().unwrap());
    assert_eq!(v["route"], "standard");
    assert!(v.get("link").is_none() && v.get("target").is_none(), "{v}");
    assert!(!c.b().exists(), "nothing is made in the places");

    // The real path too long as well: through E.
    let long = c.long_dir('d');
    let out = c.run(&[long.join("s.sock")]);
    let real_long = fs::canonicalize(&long).unwrap();
    assert_eq!(printed(&out), c.b().join(hash(&real_long)).join("s.sock"));
}

/// The command run in `cwd` with `$PWD` set to `pwd`.
fn in_cwd<S: AsRef<OsStr>>(c: &Case, cwd: &Path, pwd: &Path, args: &[S]) -> Output {
    c.sockat()
        .current_dir(cwd)
        .env("PWD", pwd)
        .args(args)
        .output()
        .unwrap()
}

/// A relative path is made absolute before it is measured, since the
/// process the path is handed to may run in another directory (cli.md
/// "終了コードとエラー", hub DR-0019 "絶対パスの作り方と、その後"). The
/// first candidate is the real path (the socket's directory by realpath, the
/// file name after it): when it fits, it is the path, and nothing is made in
/// the places.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_relative_path_is_its_real_path_when_that_fits() {
    let c = Case::new();
    fs::create_dir(c.path().join("x")).unwrap();
    let real = fs::canonicalize(c.path()).unwrap();
    let out = in_cwd(&c, c.path(), c.path(), &["./x/../s.sock"]);
    assert_eq!(printed(&out), real.join("s.sock"));
    assert!(!c.b().exists(), "nothing is made in the places");
}

/// When the real path does not fit, the second candidate is `$PWD` with the
/// relative path after it, `.` and `..` folded by their letters: used when
/// `$PWD` is the cwd and the folded directory is the relative path's
/// directory (hub DR-0019 2-3). Here the cwd is reached through a short
/// symlink to a long directory, so only `$PWD`'s spelling fits; nothing is
/// made in the places.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_relative_path_is_spelled_from_pwd_when_only_that_fits() {
    let c = Case::new();
    let long = c.long_dir('d');
    let link = c.path().join("ln");
    std::os::unix::fs::symlink(&long, &link).unwrap();
    let out = in_cwd(&c, &link, &link, &["./s.sock"]);
    assert_eq!(printed(&out), link.join("s.sock"));
    assert!(!c.b().exists(), "nothing is made in the places");
}

/// `$PWD` that is not the cwd (another directory's `(st_dev, st_ino)`) is
/// not used: its spelling would name another place (hub DR-0019 2). With the
/// real path too long, the path goes through E.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_pwd_that_is_not_the_cwd_is_not_used() {
    let c = Case::new();
    let long = c.long_dir('d');
    let link = c.path().join("ln");
    std::os::unix::fs::symlink(&long, &link).unwrap();
    let elsewhere = c.path().join("e");
    fs::create_dir(&elsewhere).unwrap();
    let out = in_cwd(&c, &link, &elsewhere, &["s.sock"]);
    let real_long = fs::canonicalize(&long).unwrap();
    assert_eq!(printed(&out), c.b().join(hash(&real_long)).join("s.sock"));
}

/// `..` after a symlink is the parent of the symlink's target to the
/// kernel, so `$PWD`'s spelling folded by its letters names another
/// directory and is not a candidate (hub DR-0019 2, 案 E): the path goes
/// through E to the directory the kernel reaches, not to the folded one.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_folded_dotdot_that_names_another_directory_is_not_used() {
    let c = Case::new();
    let long = c.long_dir('d');
    fs::create_dir(long.join("sub")).unwrap();
    std::os::unix::fs::symlink(long.join("sub"), c.path().join("sym")).unwrap();
    let out = in_cwd(&c, c.path(), c.path(), &["sym/../s.sock"]);
    let real_long = fs::canonicalize(&long).unwrap();
    assert_eq!(printed(&out), c.b().join(hash(&real_long)).join("s.sock"));
}

/// When neither candidate fits, a relative path goes through E, as a long
/// absolute path does (hub DR-0019 4), even when it is short as given.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_relative_path_neither_candidate_fits_goes_through_e() {
    let c = Case::new();
    let long = c.long_dir('d');
    let out = in_cwd(&c, &long, &long, &["s.sock"]);
    let real_long = fs::canonicalize(&long).unwrap();
    assert_eq!(printed(&out), c.b().join(hash(&real_long)).join("s.sock"));
}

/// A path that does not end with a name (`dir/`, `dir/.`, `dir/..`, `..`)
/// is an error in the arguments whatever its length, as in the library (hub
/// SPEC "エラー"): it names no socket file to bind or connect to.
#[test]
fn a_path_not_ending_with_a_name_is_an_argument_error() {
    let c = Case::new();
    let long = c.long_dir('d');
    for p in ["/tmp/x/", "/tmp/x/.", "/tmp/x/..", "..", "x/", "/"] {
        let m = failed(&c.run(&[p]), 2);
        assert!(
            m.starts_with("sockat: ") && m.lines().count() == 2,
            "{p}: {m}"
        );
    }
    failed(&c.run(&[long.join("x").join("..")]), 2);
}

/// A long path is printed as `B/<hash of the directory's real path>/<file
/// name>`, E right in B pointing to the directory, and the path works for a
/// bind and a connect: the socket file is where the long path names (hub
/// DR-0018 2-5).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_long_path_goes_through_an_e_to_its_directory_for_bind_and_connect() {
    let c = Case::new();
    let long = c.long_dir('d');
    let original = long.join("s.sock");
    assert!(len(&original) > LIMIT);
    let path = printed(&c.run(&[&original]));
    let real = fs::canonicalize(&long).unwrap();
    let e = c.b().join(hash(&real));
    assert_eq!(path, e.join("s.sock"));
    assert!(len(&path) <= LIMIT);
    assert_eq!(fs::read_link(&e).unwrap(), real);

    let listener = UnixListener::bind(&path).unwrap();
    assert!(
        fs::symlink_metadata(&original)
            .unwrap()
            .file_type()
            .is_socket()
    );
    let mut client = UnixStream::connect(&path).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    client.write_all(b"ping").unwrap();
    let mut buf = [0; 4];
    server.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}

/// The same directory always gets the same E (the name is the hash of its
/// real path), so the same long path always gets the same path and E stays
/// one per directory; another directory gets another (hub DR-0018 4).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn the_same_directory_gets_the_same_e() {
    let c = Case::new();
    let long = c.long_dir('d');
    let first = printed(&c.run(&[long.join("a.sock")]));
    let again = printed(&c.run(&[long.join("a.sock")]));
    let other_name = printed(&c.run(&[long.join("b.sock")]));
    assert_eq!(first, again);
    assert_eq!(first.parent(), other_name.parent());
    let other_dir = printed(&c.run(&[c.long_dir('e').join("a.sock")]));
    assert_ne!(first.parent(), other_dir.parent());
    // One E per directory, nothing else.
    assert_eq!(fs::read_dir(c.b()).unwrap().count(), 2);
}

/// Something else at the name of the E (a link pointing elsewhere, changed
/// by hand, or a file that is not a link) is not replaced: the command fails
/// with exit code 1, naming that path and saying to remove it, and leaves it
/// as it is, so a path printed before never comes to name another place
/// unseen (hub DR-0018 4, cli.md "終了コードとエラー"). Once it is removed,
/// the same path is printed again.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn something_else_at_the_name_of_the_e_is_an_error_and_left_as_it_is() {
    let c = Case::new();
    let long = c.long_dir('d');
    let path = printed(&c.run(&[long.join("s.sock")]));
    let e = path.parent().unwrap().to_path_buf();

    // A link pointing elsewhere.
    fs::remove_file(&e).unwrap();
    std::os::unix::fs::symlink(c.path(), &e).unwrap();
    let m = failed(&c.run(&[long.join("s.sock")]), 1);
    assert!(m.starts_with("sockat: ") && m.lines().count() == 2, "{m}");
    assert!(
        m.contains(e.to_str().unwrap()) && m.contains("remove"),
        "{m}"
    );
    assert_eq!(fs::read_link(&e).unwrap(), c.path());

    // A file that is not a link.
    fs::remove_file(&e).unwrap();
    fs::write(&e, b"kept").unwrap();
    let m = failed(&c.run(&[long.join("s.sock")]), 1);
    assert!(
        m.contains(e.to_str().unwrap()) && m.contains("remove"),
        "{m}"
    );
    assert_eq!(fs::read(&e).unwrap(), b"kept");

    // Nothing else is made in B for either.
    let names: Vec<_> = fs::read_dir(c.b())
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    assert_eq!(names, [e.file_name().unwrap()]);

    // Removed: the same path again, the E pointing to the directory.
    fs::remove_file(&e).unwrap();
    assert_eq!(printed(&c.run(&[long.join("s.sock")])), path);
    assert_eq!(fs::read_link(&e).unwrap(), fs::canonicalize(&long).unwrap());
}

/// The length of a file name that does not fit through an E to its
/// directory but does through an E to the file.
fn too_long_for_the_directory(c: &Case) -> usize {
    LIMIT - (len(&c.b()) + 1 + HASH_LEN + 1) + 1
}

/// The warning for a path to the socket file given without `--for`: on
/// every OS it is not recommended for a bind, for the reason of the OS
/// (cli.md "ファイル名が長い時"). Where a bind does not follow a symlink the
/// bind fails; where it does, unlinking the path removes the symlink and
/// leaves the socket file.
const WARNING: &str = match BIND_FOLLOWS {
    false => {
        "this path is for connect; not recommended for bind (bind does not follow the symlink on this OS)"
    }
    true => {
        "this path is for connect; not recommended for bind (unlinking this path removes only the symlink, not the socket file)"
    }
};

/// The warning for a path to the socket file given for `--for bind` where a
/// bind follows a symlink.
const WARNING_BIND: &str = "unlinking this path removes only the symlink, not the socket file (remove the socket by its original path)";

/// The warning for a path through E given for `--for bind` with B in the
/// places, which the OS may clean up (cli.md "終了コードとエラー").
const WARNING_BASE: &str = "the OS may clean up the base directory: a long-running program given this path loses it once the symlink it goes through is removed (give --base a directory that is not cleaned up)";

/// The path printed, with a line on the standard error for each warning.
fn printed_with_warnings(out: &Output, warnings: &[&str]) -> PathBuf {
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        warnings
            .iter()
            .map(|w| format!("sockat: warning: {w}\n"))
            .collect::<String>()
    );
    let line = out
        .stdout
        .strip_suffix(b"\n")
        .expect("a line ending in a newline");
    assert!(!line.contains(&b'\n'), "one line only");
    PathBuf::from(OsStr::from_bytes(line))
}

/// A file name too long for the path through the directory's E gets the
/// path through an E to the file (kawaz CL-Q1 = b; hub DR-0018 6, cli.md
/// "ファイル名が長い時"). It works for a connect everywhere and for a bind
/// only where a bind follows a symlink (macOS, NetBSD), and even there its
/// unlink removes the symlink and leaves the socket file. Without `--for`
/// it is printed for a connect (exit code 0) with one line on the standard
/// error saying it is not recommended for a bind and why, the standard
/// output holding the path alone. `--for connect` has no warning; `--for
/// bind` gets the path with the warning of the unlink where a bind follows
/// a symlink, and an error elsewhere, told before anything is made.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_long_file_name_gets_the_path_to_the_file() {
    let c = Case::new();
    let long = c.long_dir('d');
    let name = "n".repeat(too_long_for_the_directory(&c));
    let original = long.join(&name);
    let real = fs::canonicalize(&long).unwrap().join(&name);
    let e = c.b().join(hash(&real));

    if !BIND_FOLLOWS {
        let message = failed(
            &c.run(&[
                OsStr::new("--for"),
                OsStr::new("bind"),
                original.as_os_str(),
            ]),
            1,
        );
        assert!(message.starts_with("sockat: "), "{message}");
        assert!(message.contains("bind"), "{message}");
        assert!(!c.b().exists(), "no B is made for an error");
    }

    // Without `--for`: the path for a connect, with the warning on every OS.
    assert_eq!(printed_with_warnings(&c.run(&[&original]), &[WARNING]), e);

    // For a connect: the E to the file, to a socket bound at the long path.
    let listener = sockat::bind(&original).unwrap();
    let out = c.run(&[
        OsStr::new("--for"),
        OsStr::new("connect"),
        original.as_os_str(),
    ]);
    assert_eq!(printed(&out), e);
    assert_eq!(fs::read_link(&e).unwrap(), real);
    let _client = UnixStream::connect(&e).unwrap();
    listener.accept().unwrap();
    drop(listener);
    fs::remove_file(&original).unwrap();

    // For a bind: the same E where a bind follows it, an error elsewhere.
    let out = c.run(&[
        OsStr::new("--for"),
        OsStr::new("bind"),
        original.as_os_str(),
    ]);
    if BIND_FOLLOWS {
        assert_eq!(
            printed_with_warnings(&out, &[WARNING_BIND, WARNING_BASE]),
            e
        );
        let _listener = UnixListener::bind(&e).unwrap();
        assert!(
            fs::symlink_metadata(&original)
                .unwrap()
                .file_type()
                .is_socket()
        );
    } else {
        let message = failed(&out, 1);
        assert!(message.contains("bind"), "{message}");
    }
}

/// `--for bind` with B chosen in the places gets the path with one line on
/// the standard error: the OS may clean the places up, so a long-running
/// program given the path loses it once the E it goes through is removed,
/// and a `--base` the OS does not clean up keeps it (cli.md "終了コードとエ
/// ラー").
/// With `--json` the warning goes into `warnings` instead. There is no such
/// warning when the base is given (`--base`, and `--base-fd` on Linux: who
/// chose it knows how long it lasts), for a path that goes through no E (the
/// path itself, or its real path), nor without `--for bind`.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn for_bind_warns_that_the_os_may_clean_the_base() {
    let c = Case::new();
    let long = c.long_dir('d');
    let original = long.join("s.sock");
    let e = c.b().join(hash(&fs::canonicalize(&long).unwrap()));
    let for_bind = |extra: &[&OsStr], path: &Path| {
        let mut args = vec![OsStr::new("--for"), OsStr::new("bind")];
        args.extend_from_slice(extra);
        args.push(path.as_os_str());
        c.run(&args)
    };

    // The places: through the E to the directory, with the warning.
    assert_eq!(
        printed_with_warnings(&for_bind(&[], &original), &[WARNING_BASE]),
        e.join("s.sock")
    );

    // `--json`: in `warnings`, nothing on the standard error.
    let out = for_bind(&[OsStr::new("--json")], &original);
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["route"], "link-dir");
    assert_eq!(v["warnings"], serde_json::json!([WARNING_BASE]));

    // Without `--for bind`: no warning.
    assert_eq!(printed(&c.run(&[&original])), e.join("s.sock"));
    let out = c.run(&[
        OsStr::new("--for"),
        OsStr::new("connect"),
        original.as_os_str(),
    ]);
    assert_eq!(printed(&out), e.join("s.sock"));

    // `--base`: no warning.
    let app = c.path().join("app");
    fs::create_dir(&app).unwrap();
    let base = app.join("sockat");
    let out = for_bind(&[OsStr::new("--base"), base.as_os_str()], &original);
    assert!(printed(&out).starts_with(fs::canonicalize(&app).unwrap()));

    // No E: the path itself, and a long path whose real path fits.
    let short = c.path().join("s.sock");
    assert_eq!(printed(&for_bind(&[], &short)), short);
    let real_dir = c.path().join("r");
    fs::create_dir(&real_dir).unwrap();
    let long_link = c.path().join("l".repeat(120));
    std::os::unix::fs::symlink(&real_dir, &long_link).unwrap();
    assert_eq!(
        printed(&for_bind(&[], &long_link.join("s.sock"))),
        fs::canonicalize(&real_dir).unwrap().join("s.sock")
    );

    // `--base-fd`: no warning.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;

        let held = c.path().join("held");
        fs::create_dir(&held).unwrap();
        fs::set_permissions(&held, fs::Permissions::from_mode(0o700)).unwrap();
        let dir = fs::File::open(&held).unwrap();
        let fd = dir.as_raw_fd().to_string();
        let pid = std::process::id().to_string();
        let out = for_bind(
            &[
                OsStr::new("--base-fd"),
                OsStr::new(&fd),
                OsStr::new("--base-pid"),
                OsStr::new(&pid),
            ],
            &original,
        );
        assert!(printed(&out).starts_with("/proc"));
    }
}

/// `--json` gives the members of the design: the path and its bytes, the
/// limit, the path as given and its bytes, the route, the E and its target
/// (not for `standard`), and the uses (cli.md "出力").
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn json_has_the_members_of_the_design() {
    let c = Case::new();
    let json = |out: Output| -> serde_json::Value {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // A warning goes into `warnings`, never to the standard error.
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let none = serde_json::json!([]);
    let long = c.long_dir('d');
    let real = fs::canonicalize(&long).unwrap();

    // link-dir, the path given relative (`original` is as given).
    let v = json(
        c.sockat()
            .current_dir(&long)
            .args(["--json", "s.sock"])
            .output()
            .unwrap(),
    );
    let e = c.b().join(hash(&real));
    let path = e.join("s.sock");
    assert_eq!(v["path"], path.to_str().unwrap());
    assert_eq!(v["bytes"], len(&path));
    assert_eq!(v["limit"], LIMIT);
    assert_eq!(v["original"], "s.sock");
    assert_eq!(v["original_bytes"], 6);
    assert_eq!(v["route"], "link-dir");
    assert_eq!(v["link"], e.to_str().unwrap());
    assert_eq!(v["target"], real.to_str().unwrap());
    assert_eq!(v["for"], serde_json::json!(["bind", "connect"]));
    assert_eq!(v["warnings"], none, "`warnings` is there even when empty");

    // standard: no E.
    let short = c.path().join("s.sock");
    let v = json(c.run(&[OsStr::new("--json"), short.as_os_str()]));
    assert_eq!(v["path"], short.to_str().unwrap());
    assert_eq!(v["route"], "standard");
    assert!(v.get("link").is_none() && v.get("target").is_none(), "{v}");
    assert_eq!(v["for"], serde_json::json!(["bind", "connect"]));
    assert_eq!(v["warnings"], none);

    // link-file: for a connect, and for a bind where a bind follows it.
    let name = "n".repeat(too_long_for_the_directory(&c));
    let v = json(c.run(&[
        OsStr::new("--json"),
        OsStr::new("--for"),
        OsStr::new("connect"),
        long.join(&name).as_os_str(),
    ]));
    assert_eq!(v["route"], "link-file");
    assert_eq!(v["target"], real.join(&name).to_str().unwrap());
    let uses = match BIND_FOLLOWS {
        true => serde_json::json!(["bind", "connect"]),
        false => serde_json::json!(["connect"]),
    };
    assert_eq!(v["for"], uses);
    assert_eq!(v["warnings"], none);

    // link-file without `--for`: the same path, with the warning in
    // `warnings` on every OS.
    let v = json(c.run(&[OsStr::new("--json"), long.join(&name).as_os_str()]));
    assert_eq!(v["route"], "link-file");
    assert_eq!(v["for"], uses);
    assert_eq!(v["warnings"], serde_json::json!([WARNING]));
}

/// Exit codes: 2 for an error in the arguments, 1 for a path that cannot be
/// made, with the cause and what to do in 1-2 lines on the standard error
/// and nothing on the standard output (cli.md "終了コードとエラー").
#[test]
fn exit_codes() {
    let c = Case::new();
    let long = c.long_dir('d');
    failed(&c.run(&["--no-such-option", "x"]), 2);
    failed(&c.run(&["--for", "send", "/tmp/x"]), 2);
    let m = failed(
        &c.run(&[
            OsStr::new("--base"),
            OsStr::new("rel"),
            long.join("s").as_os_str(),
        ]),
        2,
    );
    assert!(m.starts_with("sockat: ") && m.lines().count() == 2, "{m}");
    let m = failed(&c.run(&[long.join("..")]), 2);
    assert!(m.lines().count() == 2, "{m}");
    let m = failed(&c.run(&[long.join("missing").join("s.sock")]), 1);
    assert!(m.starts_with("sockat: ") && m.lines().count() == 2, "{m}");
    fs::write(long.join("file"), b"").unwrap();
    let m = failed(&c.run(&[long.join("file").join("s.sock")]), 1);
    assert!(m.starts_with("sockat: "), "{m}");
}

/// `sweep` removes a dead D in B and leaves the E that `resolve` put right
/// in B: its name (32 hex digits) is not the form of a D (10 letters and
/// digits), the only names sweep takes (hub DR-0018 "片付け", SPEC "sweep").
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn sweep_leaves_the_e() {
    let c = Case::new();
    let long = c.long_dir('d');
    let path = printed(&c.run(&[long.join("s.sock")]));
    let e = path.parent().unwrap().to_path_buf();
    let e_name = e.file_name().unwrap().as_bytes();
    assert!(e_name.len() == HASH_LEN && e_name.iter().all(|b| b.is_ascii_hexdigit()));
    // A D left by a process that has ended: a directory of the D's form
    // holding an E, with no lock on it.
    let dead = c.b().join("AbCdE12345");
    fs::create_dir(&dead).unwrap();
    std::os::unix::fs::symlink("/", dead.join("0")).unwrap();

    let out = c.run(&["sweep"]);
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());
    assert!(!dead.exists(), "the dead D is swept");
    assert_eq!(fs::read_link(&e).unwrap(), fs::canonicalize(&long).unwrap());
}

/// `--base` names B itself: it is made there (its parent must be there) and
/// the E is put right in it, and nothing is made in the places; a B whose
/// path leaves no room is refused before anything is made (cli.md "基本の
/// オプション", hub DR-0018).
#[test]
fn base_puts_the_e_in_the_given_b() {
    let c = Case::new();
    let long = c.long_dir('d');
    let parent = c.path().join("app");
    fs::create_dir(&parent).unwrap();
    let base = parent.join("sockat");
    let out = c.run(&[
        OsStr::new("--base"),
        base.as_os_str(),
        long.join("s.sock").as_os_str(),
    ]);
    let real = fs::canonicalize(&long).unwrap();
    let b = fs::canonicalize(&parent).unwrap().join("sockat");
    assert_eq!(printed(&out), b.join(hash(&real)).join("s.sock"));
    assert!(fs::metadata(&b).unwrap().is_dir());
    assert!(!c.b().exists(), "nothing is made in the places");

    // A B whose path is too long for any spelling: an error, B not made.
    let too_long = long.join("sockat");
    let out = c.run(&[
        OsStr::new("--base"),
        too_long.as_os_str(),
        long.join("s.sock").as_os_str(),
    ]);
    failed(&out, 1);
    assert!(!too_long.exists());

    // A missing parent: an error.
    let missing = c.path().join("no").join("sockat");
    let out = c.run(&[
        OsStr::new("--base"),
        missing.as_os_str(),
        long.join("s.sock").as_os_str(),
    ]);
    failed(&out, 1);
}

/// `--base` is not looked at when an absolute candidate fits (hub SPEC "相対
/// パスを絶対パスにする": a candidate goes to std without B, as a path that
/// fits as it is does): a long absolute path whose real path fits, and a
/// relative path, are printed as their real path with a `--base` whose parent
/// is missing (an error when it is looked at). Its form is still checked
/// first (an argument error), as the library checks `Options.base`'s.
#[test]
fn base_is_not_looked_at_when_a_candidate_fits() {
    let c = Case::new();
    let real_dir = c.path().join("r");
    fs::create_dir(&real_dir).unwrap();
    let long_link = c.path().join("l".repeat(120));
    std::os::unix::fs::symlink(&real_dir, &long_link).unwrap();
    let real = fs::canonicalize(&real_dir).unwrap().join("s.sock");
    let missing = c.path().join("no").join("sockat");
    let with_base = |cwd: &Path, path: &OsStr| {
        in_cwd(
            &c,
            cwd,
            cwd,
            &[OsStr::new("--base"), missing.as_os_str(), path],
        )
    };
    let original = long_link.join("s.sock");
    assert!(len(&original) > LIMIT);
    assert_eq!(printed(&with_base(c.path(), original.as_os_str())), real);
    assert_eq!(printed(&with_base(&real_dir, OsStr::new("s.sock"))), real);
    assert!(!missing.parent().unwrap().exists());
    let out = in_cwd(
        &c,
        &real_dir,
        &real_dir,
        &[
            OsStr::new("--base"),
            OsStr::new("rel"),
            OsStr::new("s.sock"),
        ],
    );
    failed(&out, 2);
}

/// With no arguments the command, and `resolve` and `completion`, print
/// their help and succeed, as `--help` does; `--help-all` has all of
/// `--help` (cli.md "呼び方"). `--version` prints the version.
#[test]
fn no_arguments_print_the_help() {
    let c = Case::new();
    let help = |args: &[&str]| {
        let out = c.run(args);
        assert!(out.status.success() && out.stderr.is_empty(), "{args:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let top = help(&[]);
    assert!(top.contains("Usage:") && top.contains("resolve") && top.contains("sweep"));
    assert_eq!(top, help(&["--help"]));
    let all = help(&["--help-all"]);
    for line in top.lines().filter(|l| !l.contains("--help")) {
        assert!(all.contains(line), "--help-all lacks {line:?}");
    }
    assert!(help(&["resolve"]).contains("--for"));
    assert!(help(&["completion"]).contains("zsh"));
    assert_eq!(
        help(&["--version"]),
        format!("sockat {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// `completion <shell>` prints the shell's completion script with the
/// subcommands and options in it (fish writes the long options as `-l json`).
#[test]
fn completion_prints_a_script() {
    let c = Case::new();
    for shell in ["bash", "zsh", "fish"] {
        let out = c.run(&["completion", shell]);
        assert!(out.status.success(), "{shell}");
        let script = String::from_utf8(out.stdout).unwrap();
        for word in [
            "sockat",
            "resolve",
            "sweep",
            "completion",
            "json",
            "base",
            "connect",
        ] {
            assert!(script.contains(word), "{shell} lacks {word}");
        }
    }
}

/// The first argument is a subcommand only when it names one; after an
/// option or `--` it is the path (cli.md "呼び方").
#[test]
fn a_subcommand_name_is_the_path_unless_first() {
    let c = Case::new();
    let real = fs::canonicalize(c.path()).unwrap();
    assert_eq!(printed(&c.run(&["--", "sweep"])), real.join("sweep"));
    assert_eq!(
        printed(&c.run(&["resolve", "resolve"])),
        real.join("resolve")
    );
    let out = c.run(&["--json", "resolve"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["original"], "resolve");
}

// ---------------------------------------------------------------------------
// the advanced options and the commands that try the routes (cli.md
// "進んだオプション", "手法を試すサブコマンド"; DR-0034)
// ---------------------------------------------------------------------------

/// Every route name of the SPEC ("経路の名前").
const ALL_ROUTES: [&str; 10] = [
    "standard",
    "link-dir",
    "link-file",
    "thread-cwd",
    "at",
    "proc-direct",
    "relative",
    "widened",
    "at-direct",
    "thread-cwd-direct",
];

/// The routes of a bind on this OS (hub SPEC "bind の順": link-file where a
/// bind may follow a symlink, thread-cwd on Linux and Apple, at on FreeBSD).
#[cfg(any(target_os = "linux", target_os = "android"))]
const BIND_ROUTES: &[&str] = &["standard", "link-dir", "thread-cwd"];
#[cfg(target_vendor = "apple")]
const BIND_ROUTES: &[&str] = &["standard", "link-dir", "link-file", "thread-cwd"];
#[cfg(target_os = "freebsd")]
const BIND_ROUTES: &[&str] = &["standard", "link-dir", "at"];
#[cfg(any(target_os = "openbsd", target_os = "illumos"))]
const BIND_ROUTES: &[&str] = &["standard", "link-dir"];
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "illumos"
)))]
const BIND_ROUTES: &[&str] = &["standard", "link-dir", "link-file"];

/// The routes of a connect on this OS (hub SPEC "connect の順", both rows of
/// Linux).
#[cfg(any(target_os = "linux", target_os = "android"))]
const CONNECT_ROUTES: &[&str] = &[
    "standard",
    "proc-direct",
    "relative",
    "thread-cwd-direct",
    "link-file",
];
#[cfg(target_vendor = "apple")]
const CONNECT_ROUTES: &[&str] = &[
    "standard",
    "relative",
    "widened",
    "thread-cwd-direct",
    "link-file",
];
#[cfg(target_os = "freebsd")]
const CONNECT_ROUTES: &[&str] = &["standard", "at-direct", "link-file"];
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
)))]
const CONNECT_ROUTES: &[&str] = &["standard", "relative", "link-file"];

/// Why thread-cwd and thread-cwd-direct cannot be used here, if they cannot:
/// a seccomp filter on Linux (docker's default profile installs one, and the
/// command inherits it), as the library tells (`seccomp filter`).
fn thread_cwd_refused() -> Option<&'static str> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return None;
    }
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mode = status
        .lines()
        .find_map(|l| l.strip_prefix("Seccomp:"))
        .map(str::trim);
    (mode != Some("0")).then_some("seccomp filter")
}

/// A listener at `p` (any length), made by std at a short path in the case
/// and renamed there, so a connect test does not lean on the bind routes.
fn listen_at(c: &Case, p: &Path) -> UnixListener {
    let short = c.path().join("l.sock");
    let l = UnixListener::bind(&short).unwrap();
    fs::rename(&short, p).unwrap();
    l.set_nonblocking(true).unwrap();
    l
}

/// The JSON of `bind` / `connect` and the exit code.
fn trial_json(out: &Output) -> (i32, serde_json::Value) {
    let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: stdout {}, stderr {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap(), v)
}

/// The names in `v`'s `skipped`.
fn skipped(v: &serde_json::Value) -> Vec<String> {
    v["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect()
}

fn is_socket(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_socket())
}

/// The `base` and `until` of a bind's link-dir in the case: B spelled
/// through `/proc/<pid>/fd/<B's fd>` where `/proc` names what an fd is open
/// on, else by its real path (hub SPEC-routes "綴りの形": procB is used when
/// `/proc/<pid>/fd/<fd>` and the fd's `fstat` are the same `(st_dev,
/// st_ino)`, whatever the OS). Told here as the library tells it, with a
/// directory this test opens.
fn link_dir_base() -> (&'static str, &'static str) {
    use std::os::fd::AsRawFd;

    let dir = fs::File::open("/").unwrap();
    let held = dir.metadata().unwrap();
    let named = fs::metadata(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        dir.as_raw_fd()
    ));
    match named.is_ok_and(|m| (m.dev(), m.ino()) == (held.dev(), held.ino())) {
        true => ("proc", "Process"),
        false => ("tmp", "Cleanup"),
    }
}

/// The E of the link-dir spelling `kernel` (`<B's spelling>/<D>/<E>/<name>`),
/// by B's real path in the case's places: how `left` names it.
fn e_of(c: &Case, kernel: &Path) -> PathBuf {
    let e = kernel.parent().unwrap();
    let d = e.parent().unwrap();
    c.b()
        .join(d.file_name().unwrap())
        .join(e.file_name().unwrap())
}

/// `bind` without `--route` binds by the library's order (hub SPEC "bind の
/// 順"): a long path goes past standard (too long) to link-dir, the first
/// route through B, here in the case's places (spelled through `/proc` where
/// it can be used). `kernel-path` is the spelling as the kernel wrote it, and
/// `left` names the E by its real path. The text shows one item a
/// line, `left` the E made, with where it points. Without `--keep` the
/// socket file and that E are removed after they are shown.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn bind_by_the_default_order() {
    let c = Case::new();
    let long = c.long_dir('d');
    let original = long.join("s.sock");
    let out = c.run(&[OsStr::new("bind"), original.as_os_str()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let item = |name: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("no {name} in {text}"))
            .to_owned()
    };
    let (base, until) = link_dir_base();
    assert_eq!(item("route"), "link-dir");
    assert_eq!(item("base"), base);
    assert!(
        item("spelling").starts_with(&format!(
            "Linked (removable, until {}",
            until.to_lowercase()
        )),
        "{text}"
    );
    assert!(item("skipped").starts_with("standard ("), "{text}");
    let kernel = PathBuf::from(item("kernel-path"));
    assert!(len(&kernel) <= LIMIT, "{text}");
    let real = fs::canonicalize(&long).unwrap();
    let (e, target) = item("left")
        .split_once(" -> ")
        .map(|(e, t)| (PathBuf::from(e), PathBuf::from(t)))
        .unwrap();
    assert_eq!(target, real);
    assert_eq!(e, e_of(&c, &kernel));
    assert!(!original.exists(), "the socket file is removed");
    assert!(fs::symlink_metadata(&e).is_err(), "the E is removed");
}

/// `--keep` leaves the socket file at the path and the E in the base, to be
/// looked at; without it both are gone (cli.md "手法を試すサブコマンド").
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn keep_leaves_the_socket_file_and_the_e() {
    let c = Case::new();
    let long = c.long_dir('d');
    let original = long.join("s.sock");
    let (code, v) = trial_json(&c.run(&[
        OsStr::new("bind"),
        OsStr::new("--json"),
        OsStr::new("--keep"),
        original.as_os_str(),
    ]));
    assert_eq!(code, 0, "{v}");
    let e = PathBuf::from(v["left"][0].as_str().unwrap());
    assert!(is_socket(&original));
    assert_eq!(fs::read_link(&e).unwrap(), fs::canonicalize(&long).unwrap());
    fs::remove_file(&original).unwrap();

    let (code, v) = trial_json(&c.run(&[
        OsStr::new("bind"),
        OsStr::new("--json"),
        original.as_os_str(),
    ]));
    assert_eq!(code, 0, "{v}");
    let e = PathBuf::from(v["left"][0].as_str().unwrap());
    assert!(!original.exists());
    assert!(fs::symlink_metadata(&e).is_err());
}

/// `--json` of `bind` and `connect` has the members of the conformance CLI
/// (`ok`, `error`, `route`, `base`, `spelling`, with its values) and the
/// command's (`skipped` with the reasons, `kernel_path`, `left`), for a call
/// that gets through and one that does not (cli.md "出力").
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn trial_json_has_the_members_of_the_design() {
    let c = Case::new();
    let long = c.long_dir('d');
    let original = long.join("s.sock");
    let (code, v) = trial_json(&c.run(&[
        OsStr::new("bind"),
        OsStr::new("--json"),
        OsStr::new("--route"),
        OsStr::new("standard,link-dir"),
        original.as_os_str(),
    ]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["error"], serde_json::Value::Null);
    assert_eq!(v["route"], "link-dir");
    let (base, until) = link_dir_base();
    assert_eq!(v["base"], base);
    assert_eq!(v["spelling"]["kind"], "Linked");
    assert_eq!(v["spelling"]["removable"], true);
    assert_eq!(v["spelling"]["until"], until);
    for attr in ["usable_cross_uid", "usable_with_private_tmp"] {
        assert!(v["spelling"][attr].is_boolean(), "{v}");
    }
    assert_eq!(skipped(&v), ["standard"]);
    assert!(v["skipped"][0]["reason"].is_string(), "{v}");
    let kernel = PathBuf::from(v["kernel_path"].as_str().unwrap());
    assert_eq!(kernel.file_name().unwrap(), "s.sock");
    assert_eq!(
        v["left"],
        serde_json::json!([e_of(&c, &kernel).to_str().unwrap()])
    );

    // Through to nobody: ok false, the errno's name, and nothing kept.
    let (code, v) = trial_json(&c.run(&[
        OsStr::new("connect"),
        OsStr::new("--json"),
        original.as_os_str(),
    ]));
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["ok"], false);
    assert!(
        matches!(v["error"].as_str(), Some("ENOENT" | "ECONNREFUSED")),
        "{v}"
    );
    assert!(
        v["route"].is_string(),
        "the route that failed for real: {v}"
    );
    assert_eq!(v["base"], serde_json::Value::Null);
    assert_eq!(v["spelling"], serde_json::Value::Null);
    assert_eq!(v["kernel_path"], serde_json::Value::Null);
    assert_eq!(v["left"], serde_json::json!([]));

    // A connect that gets through: no base, spelling or left.
    let l = listen_at(&c, &original);
    let (code, v) = trial_json(&c.run(&[
        OsStr::new("connect"),
        OsStr::new("--json"),
        original.as_os_str(),
    ]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["base"], serde_json::Value::Null);
    assert_eq!(v["spelling"], serde_json::Value::Null);
    assert_eq!(v["left"], serde_json::json!([]));
    assert_eq!(skipped(&v)[0], "standard");
    assert!(l.accept().is_ok(), "the connect reached the listener");
}

/// `bind --route <name>` binds by that route alone, for each route of a bind
/// on this OS, with the socket at the path: standard for a path that fits,
/// the others for a long one. thread-cwd is passed over where a seccomp
/// filter keeps the thread's cwd from moving, and then nothing is left to
/// try (exit code 1).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn bind_by_each_route_named() {
    let c = Case::new();
    let long = c.long_dir('d');
    for route in BIND_ROUTES {
        let original = match *route {
            "standard" => c.path().join("s.sock"),
            _ => long.join("s.sock"),
        };
        let out = c.run(&[
            OsStr::new("bind"),
            OsStr::new("--json"),
            OsStr::new("--keep"),
            OsStr::new("--route"),
            OsStr::new(route),
            original.as_os_str(),
        ]);
        let (code, v) = trial_json(&out);
        if route.starts_with("thread-cwd") {
            if let Some(why) = thread_cwd_refused() {
                assert_eq!(code, 1, "{v}");
                assert_eq!(v["skipped"][0]["reason"], why, "{v}");
                continue;
            }
        }
        assert_eq!(code, 0, "{route}: {v}");
        assert_eq!(v["route"], *route, "{v}");
        assert!(v["skipped"].as_array().unwrap().is_empty(), "{v}");
        let kind = match *route {
            "standard" => "Original",
            "link-dir" | "link-file" => "Linked",
            _ => "Dangling",
        };
        assert_eq!(v["spelling"]["kind"], kind, "{v}");
        assert!(is_socket(&original), "{route}: the socket is at the path");
        fs::remove_file(&original).unwrap();
    }
}

/// `connect --route <name>` connects by that route alone, for each route of
/// a connect on this OS, to the listener at the path: standard for a path
/// that fits, relative from the socket's directory, the others for a long
/// path from elsewhere.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn connect_by_each_route_named() {
    let c = Case::new();
    let long = c.long_dir('d');
    for route in CONNECT_ROUTES {
        let (original, cwd) = match *route {
            "standard" => (c.path().join("s.sock"), c.path().to_path_buf()),
            "relative" => (long.join("s.sock"), long.clone()),
            _ => (long.join("s.sock"), c.path().to_path_buf()),
        };
        let l = listen_at(&c, &original);
        let out = c
            .sockat()
            .current_dir(&cwd)
            .args([
                OsStr::new("connect"),
                OsStr::new("--json"),
                OsStr::new("--route"),
                OsStr::new(route),
                original.as_os_str(),
            ])
            .output()
            .unwrap();
        let (code, v) = trial_json(&out);
        if route.starts_with("thread-cwd") {
            if let Some(why) = thread_cwd_refused() {
                assert_eq!(code, 1, "{v}");
                assert_eq!(v["skipped"][0]["reason"], why, "{v}");
                fs::remove_file(&original).unwrap();
                continue;
            }
        }
        assert_eq!(code, 0, "{route}: {v}");
        assert_eq!(v["route"], *route, "{v}");
        assert!(
            l.accept().is_ok(),
            "{route}: the connect reached the listener"
        );
        fs::remove_file(&original).unwrap();
    }
}

/// A route the operation or this OS does not have is an error in the
/// arguments (exit code 2) before anything is tried, and the error names
/// the routes it has (cli.md "`--route` に渡す名前"); so is a name that is no
/// route. Nothing is made in the places.
#[test]
fn a_route_this_os_or_operation_lacks_is_an_argument_error() {
    let c = Case::new();
    let long = c.long_dir('d');
    let original = long.join("s.sock");
    for (op, routes) in [("bind", BIND_ROUTES), ("connect", CONNECT_ROUTES)] {
        for route in ALL_ROUTES
            .iter()
            .filter(|r| !routes.contains(r))
            .chain(&["fork"])
        {
            let out = c.run(&[
                OsStr::new(op),
                OsStr::new("--route"),
                OsStr::new(route),
                original.as_os_str(),
            ]);
            let message = failed(&out, 2);
            for usable in routes.iter().chain(&["default"]) {
                assert!(message.contains(usable), "{op} {route}: {message}");
            }
        }
    }
    // resolve takes the routes that give a path.
    for route in ["thread-cwd", "relative", "proc-direct"] {
        let out = c.run(&[
            OsStr::new("--route"),
            OsStr::new(route),
            original.as_os_str(),
        ]);
        let message = failed(&out, 2);
        assert!(message.contains("link-file"), "{message}");
    }
    assert!(!c.b().exists());
}

/// A route named that cannot be used for the path is passed over to the
/// next named (cli.md "`--route` に渡す名前"): standard for a long path, and
/// relative for a connect from a directory too far from the socket's; with
/// none left, exit code 1 with what was passed over.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_route_unusable_for_the_path_is_passed_over() {
    let c = Case::new();
    let long = c.long_dir('d');
    let original = long.join("s.sock");

    let (code, v) = trial_json(&c.run(&[
        OsStr::new("bind"),
        OsStr::new("--json"),
        OsStr::new("--route"),
        OsStr::new("standard"),
        original.as_os_str(),
    ]));
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"], "ENAMETOOLONG");
    assert_eq!(skipped(&v), ["standard"]);
    assert!(!original.exists());

    if CONNECT_ROUTES.contains(&"relative") {
        let l = listen_at(&c, &original);
        let (code, v) = trial_json(&c.run(&[
            OsStr::new("connect"),
            OsStr::new("--json"),
            OsStr::new("--route"),
            OsStr::new("relative,link-file"),
            original.as_os_str(),
        ]));
        assert_eq!(code, 0, "{v}");
        assert_eq!(v["route"], "link-file");
        assert_eq!(skipped(&v), ["relative"]);
        assert!(l.accept().is_ok());
    }

    // resolve: standard passed over, link-dir taken; none usable, exit 1.
    let out = c.run(&[
        OsStr::new("--route"),
        OsStr::new("standard,link-dir"),
        original.as_os_str(),
    ]);
    let real = fs::canonicalize(&long).unwrap();
    assert_eq!(printed(&out), c.b().join(hash(&real)).join("s.sock"));
    let m = failed(
        &c.run(&[
            OsStr::new("--route"),
            OsStr::new("standard"),
            original.as_os_str(),
        ]),
        1,
    );
    assert!(
        m.starts_with("sockat: ") && m.lines().count() == 2 && m.contains("standard"),
        "{m}"
    );
}

/// `resolve --route` makes the path by the routes named, in that order, even
/// for a path that fits as it is: link-dir gives `B/<hash>/<name>`, link-file
/// `B/<hash of the file>` (with the warning of a path for connect), and
/// `default` alone is no `--route` at all.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn resolve_by_the_routes_named() {
    let c = Case::new();
    let short = c.path().join("s.sock");
    let real = fs::canonicalize(c.path()).unwrap();
    let out = c.run(&[
        OsStr::new("--route"),
        OsStr::new("link-dir"),
        short.as_os_str(),
    ]);
    assert_eq!(printed(&out), c.b().join(hash(&real)).join("s.sock"));
    let out = c.run(&[
        OsStr::new("--route"),
        OsStr::new("link-file"),
        short.as_os_str(),
    ]);
    assert_eq!(
        printed_with_warnings(&out, &[WARNING]),
        c.b().join(hash(&real.join("s.sock")))
    );
    let out = c.run(&[
        OsStr::new("resolve"),
        OsStr::new("--route"),
        OsStr::new("link-file,link-dir"),
        OsStr::new("--for"),
        OsStr::new("bind"),
        short.as_os_str(),
    ]);
    match BIND_FOLLOWS {
        true => assert_eq!(
            printed_with_warnings(&out, &[WARNING_BIND, WARNING_BASE]),
            c.b().join(hash(&real.join("s.sock")))
        ),
        // link-file cannot serve a bind here: passed over to link-dir.
        false => assert_eq!(
            printed_with_warnings(&out, &[WARNING_BASE]),
            c.b().join(hash(&real)).join("s.sock")
        ),
    }
    let long = c.long_dir('d');
    for args in [vec!["--route", "default"], vec![]] {
        let out = c
            .sockat()
            .args(&args)
            .arg(long.join("s.sock"))
            .output()
            .unwrap();
        let real_long = fs::canonicalize(&long).unwrap();
        assert_eq!(printed(&out), c.b().join(hash(&real_long)).join("s.sock"));
    }
}

/// `--base` gives a bind's B as `Options.base` does (`base` is `given`).
#[test]
fn bind_with_base() {
    let c = Case::new();
    let long = c.long_dir('d');
    let app = c.path().join("app");
    fs::create_dir(&app).unwrap();
    let base = app.join("sockat");
    let (code, v) = trial_json(&c.run(&[
        OsStr::new("bind"),
        OsStr::new("--json"),
        OsStr::new("--base"),
        base.as_os_str(),
        long.join("s.sock").as_os_str(),
    ]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["base"], "given");
    let kernel = PathBuf::from(v["kernel_path"].as_str().unwrap());
    assert!(
        kernel.starts_with(fs::canonicalize(&app).unwrap().join("sockat"))
            || kernel.starts_with("/proc"),
        "{v}"
    );
    assert!(!c.b().exists(), "nothing is made in the places");
}

/// `--base-fd` and `--base-pid` are a pair: one alone is an error in the
/// arguments, and so is either with `--base` (cli.md "進んだオプション").
/// Where there is no `/proc` (all but Linux), they are an error in the
/// arguments too, saying so.
#[test]
fn base_fd_and_base_pid_are_a_pair() {
    let c = Case::new();
    let p = c.long_dir('d').join("s.sock");
    let pid = std::process::id().to_string();
    for args in [
        vec!["--base-fd", "3"],
        vec!["--base-pid", pid.as_str()],
        vec![
            "--base",
            "/tmp/x/sockat",
            "--base-fd",
            "3",
            "--base-pid",
            pid.as_str(),
        ],
    ] {
        let out = c.sockat().args(&args).arg(&p).output().unwrap();
        let m = failed(&out, 2);
        assert!(
            m.starts_with("sockat: --base") && m.lines().count() == 2,
            "{m}"
        );
    }
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        let out = c
            .sockat()
            .args(["--base-fd", "3", "--base-pid", pid.as_str()])
            .arg(&p)
            .output()
            .unwrap();
        let m = failed(&out, 2);
        assert!(m.contains("/proc") && m.contains("Linux"), "{m}");
    }
}

/// `--base-fd <N> --base-pid <PID>` puts E right in the directory that
/// process holds open by its fd N, and the path goes through
/// `/proc/<PID>/fd/<N>` (hub DR-0018 "置き場と持ち主の指定"): here this test
/// holds the directory open, the command opens it through `/proc` as another
/// process, and a bind and a connect by the path reach the socket at the long
/// path while the fd is held. Nothing is made in the places. A directory
/// others can write to is refused as `--base` refuses one (exit code 1).
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn base_fd_spells_the_base_through_proc() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let c = Case::new();
    let long = c.long_dir('d');
    let held = c.path().join("held");
    fs::create_dir(&held).unwrap();
    fs::set_permissions(&held, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = fs::File::open(&held).unwrap();
    let fd = dir.as_raw_fd().to_string();
    let pid = std::process::id().to_string();
    let out = c
        .sockat()
        .args([
            "--json",
            "--base-fd",
            fd.as_str(),
            "--base-pid",
            pid.as_str(),
        ])
        .arg(long.join("s.sock"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let real = fs::canonicalize(&long).unwrap();
    let e = PathBuf::from(format!("/proc/{pid}/fd/{fd}")).join(hash(&real));
    let path = e.join("s.sock");
    assert_eq!(v["path"], path.to_str().unwrap());
    assert_eq!(v["route"], "link-dir");
    assert_eq!(v["link"], e.to_str().unwrap());
    assert_eq!(fs::read_link(held.join(hash(&real))).unwrap(), real);
    let listener = UnixListener::bind(&path).unwrap();
    assert!(is_socket(&long.join("s.sock")));
    let _client = UnixStream::connect(&path).unwrap();
    listener.accept().unwrap();
    assert!(!c.b().exists(), "nothing is made in the places");

    fs::set_permissions(&held, fs::Permissions::from_mode(0o777)).unwrap();
    let out = c
        .sockat()
        .args(["--base-fd", fd.as_str(), "--base-pid", pid.as_str()])
        .arg(long.join("t.sock"))
        .output()
        .unwrap();
    let m = failed(&out, 1);
    assert!(m.starts_with("sockat: cannot use fd"), "{m}");
}

/// The advanced options and the commands that try the routes are not in
/// `--help`, and are in `--help-all` and in the completion scripts (cli.md
/// "呼び方": `--help` lists the basic ones; the help, the completion and the
/// implementation go together).
#[test]
fn the_advanced_ones_are_in_help_all_and_the_completion_only() {
    let c = Case::new();
    let text = |args: &[&str]| {
        let out = c.run(args);
        assert!(out.status.success() && out.stderr.is_empty(), "{args:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let short = text(&["--help"]);
    let all = text(&["--help-all"]);
    let lists = |help: &str, name: &str| help.lines().any(|l| l.starts_with(&format!("  {name} ")));
    for option in ["--route", "--base-fd", "--base-pid"] {
        assert!(!short.contains(option), "--help has {option}");
        assert!(all.contains(option), "--help-all lacks {option}");
        assert!(!text(&["resolve", "--help"]).contains(option));
        assert!(text(&["resolve", "--help-all"]).contains(option));
    }
    for command in ["bind", "connect"] {
        assert!(!lists(&short, command), "--help has {command}");
        assert!(lists(&all, command), "--help-all lacks {command}");
    }
    assert!(text(&["bind"]).contains("--keep"));
    assert!(text(&["connect", "--help"]).contains("--route"));
    for shell in ["bash", "zsh", "fish"] {
        let script = text(&["completion", shell]);
        for word in ["bind", "route", "base-fd", "base-pid", "keep", "link-dir"] {
            assert!(script.contains(word), "{shell} lacks {word}");
        }
    }
}

/// A B its owner can search and write but not read (mode 0311) is a B as
/// good as any (the library opens B with the search permission alone,
/// `O_PATH` on Linux, `O_SEARCH` on Apple), for `--base` and for `--base-fd`
/// alike: the two check B itself the same way (DR-0034). Here B is given by
/// `--base`, already there with that mode, and the E is put in it. (Root
/// reads any directory, so the test means more when not run as root.)
#[test]
fn base_takes_a_directory_without_read_permission() {
    use std::os::unix::fs::PermissionsExt;

    let c = Case::new();
    let long = c.long_dir('d');
    let app = c.path().join("app");
    fs::create_dir(&app).unwrap();
    let base = app.join("sockat");
    fs::create_dir(&base).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o311)).unwrap();
    let out = c.run(&[
        OsStr::new("--base"),
        base.as_os_str(),
        long.join("s.sock").as_os_str(),
    ]);
    let real = fs::canonicalize(&long).unwrap();
    let b = fs::canonicalize(&app).unwrap().join("sockat");
    assert_eq!(printed(&out), b.join(hash(&real)).join("s.sock"));
    assert_eq!(fs::read_link(b.join(hash(&real))).unwrap(), real);
}

/// The same B without read permission (0311), given by `--base-fd` (the
/// test holds it open, opened before the read permission is taken away):
/// the command takes it with the search permission alone, as `--base` does,
/// and makes E in it through its own fd of it; a bind and a connect by the
/// path printed reach the socket at the long path.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn base_fd_takes_a_directory_without_read_permission() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let c = Case::new();
    let long = c.long_dir('d');
    let held = c.path().join("held");
    fs::create_dir(&held).unwrap();
    fs::set_permissions(&held, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = fs::File::open(&held).unwrap();
    fs::set_permissions(&held, fs::Permissions::from_mode(0o311)).unwrap();
    let fd = dir.as_raw_fd().to_string();
    let pid = std::process::id().to_string();
    let out = c
        .sockat()
        .args(["--base-fd", fd.as_str(), "--base-pid", pid.as_str()])
        .arg(long.join("s.sock"))
        .output()
        .unwrap();
    let real = fs::canonicalize(&long).unwrap();
    let path = PathBuf::from(format!("/proc/{pid}/fd/{fd}"))
        .join(hash(&real))
        .join("s.sock");
    assert_eq!(printed(&out), path);
    assert_eq!(fs::read_link(held.join(hash(&real))).unwrap(), real);
    let listener = UnixListener::bind(&path).unwrap();
    assert!(is_socket(&long.join("s.sock")));
    let _client = UnixStream::connect(&path).unwrap();
    listener.accept().unwrap();
}

/// The JSON of `bind` and `connect` holds no path as given, only the paths
/// the kernel and the base give (`kernel_path`, `left`): a path as given
/// that is not UTF-8 gets JSON when those are UTF-8 (here link-dir's,
/// through B, the file name ASCII), and an error with nothing on the
/// standard output (exit code 1) when one is not (the file name not UTF-8,
/// in `kernel_path`), the socket file removed either way (DR-0034). Linux
/// only: macOS's file systems take no name that is not UTF-8.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn trial_json_with_a_path_not_utf8() {
    let c = Case::new();
    let parent = c.long_dir('d').join(OsStr::from_bytes(b"\xff\xfe"));
    fs::create_dir(&parent).unwrap();
    let ascii = parent.join("s.sock");
    let (code, v) = trial_json(&c.run(&[
        OsStr::new("bind"),
        OsStr::new("--json"),
        OsStr::new("--route"),
        OsStr::new("link-dir"),
        ascii.as_os_str(),
    ]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["route"], "link-dir");
    assert!(
        v["kernel_path"].as_str().unwrap().ends_with("/s.sock"),
        "{v}"
    );
    assert!(!ascii.exists());

    let not_utf8 = parent.join(OsStr::from_bytes(b"s\xff.sock"));
    let out = c.run(&[
        OsStr::new("bind"),
        OsStr::new("--json"),
        OsStr::new("--route"),
        OsStr::new("link-dir"),
        not_utf8.as_os_str(),
    ]);
    let m = failed(&out, 1);
    assert!(m.contains("UTF-8"), "{m}");
    assert!(
        fs::symlink_metadata(&not_utf8).is_err(),
        "the socket file is removed"
    );
}
