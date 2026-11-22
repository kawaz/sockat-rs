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

/// The first 12 lowercase hex digits of the SHA-256 of `p`'s bytes.
fn hash(p: &Path) -> String {
    let digest = Sha256::digest(p.as_os_str().as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..12]
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

/// A relative path is made absolute by the current directory before it is
/// measured, since the process the path is handed to may run elsewhere; `.`
/// and `..` are kept as written (hub SPEC: a path is not folded by its
/// letters, a `..` after a symlink is the symlink's target's parent).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_relative_path_is_made_absolute_by_the_current_directory() {
    let c = Case::new();
    let real = fs::canonicalize(c.path()).unwrap();
    let out = c.run(&["./x/../s.sock"]);
    let mut expected = real.as_os_str().as_bytes().to_vec();
    expected.extend_from_slice(b"/./x/../s.sock");
    assert_eq!(printed(&out).as_os_str().as_bytes(), &expected[..]);

    // Short as given, too long once absolute: through B.
    let long = c.long_dir('d');
    let out = c
        .sockat()
        .current_dir(&long)
        .arg("s.sock")
        .output()
        .unwrap();
    let real_long = fs::canonicalize(&long).unwrap();
    assert_eq!(printed(&out), c.b().join(hash(&real_long)).join("s.sock"));
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

/// An E pointing elsewhere (replaced by hand, or left by another directory
/// of the same hash) is replaced by one pointing to the directory, renamed
/// over it, and nothing else is left in B (hub DR-0018 4).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn an_e_pointing_elsewhere_is_replaced() {
    let c = Case::new();
    let long = c.long_dir('d');
    let path = printed(&c.run(&[long.join("s.sock")]));
    let e = path.parent().unwrap().to_path_buf();
    fs::remove_file(&e).unwrap();
    std::os::unix::fs::symlink(c.path(), &e).unwrap();
    assert_eq!(printed(&c.run(&[long.join("s.sock")])), path);
    assert_eq!(fs::read_link(&e).unwrap(), fs::canonicalize(&long).unwrap());
    let names: Vec<_> = fs::read_dir(c.b())
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    assert_eq!(names, [e.file_name().unwrap()]);
}

/// The length of a file name that does not fit through an E to its
/// directory but does through an E to the file.
fn too_long_for_the_directory(c: &Case) -> usize {
    LIMIT - (len(&c.b()) + 1 + 12 + 1) + 1
}

/// A file name too long for the path through the directory's E needs
/// `--for`: the path through an E to the file works for a connect
/// everywhere, for a bind only where a bind follows a symlink (macOS,
/// NetBSD); without `--for` it is an error asking for it rather than a path
/// that may not work (hub DR-0018 6, cli.md CL-Q1).
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the places for B can be moved only in a debug build"
)]
fn a_long_file_name_needs_for() {
    let c = Case::new();
    let long = c.long_dir('d');
    // B is made by the first call.
    printed(&c.run(&[long.join("s.sock")]));
    let name = "n".repeat(too_long_for_the_directory(&c));
    let original = long.join(&name);
    let real = fs::canonicalize(&long).unwrap().join(&name);
    let e = c.b().join(hash(&real));

    let message = failed(&c.run(&[&original]), 1);
    assert!(message.starts_with("sockat: "), "{message}");
    assert!(message.contains("--for connect"), "{message}");
    assert_eq!(message.contains("--for bind"), BIND_FOLLOWS, "{message}");
    assert_eq!(message.lines().count(), 2, "{message}");

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
        assert_eq!(printed(&out), e);
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
        serde_json::from_slice(&out.stdout).unwrap()
    };
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

    // standard: no E.
    let short = c.path().join("s.sock");
    let v = json(c.run(&[OsStr::new("--json"), short.as_os_str()]));
    assert_eq!(v["path"], short.to_str().unwrap());
    assert_eq!(v["route"], "standard");
    assert!(v.get("link").is_none() && v.get("target").is_none(), "{v}");
    assert_eq!(v["for"], serde_json::json!(["bind", "connect"]));

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
/// in B: its name (12 hex digits) is not the form of a D (10 letters and
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
    assert!(e_name.len() == 12 && e_name.iter().all(|b| b.is_ascii_hexdigit()));
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
