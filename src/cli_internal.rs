//! The entry the `sockat` command (the `sockat-cli` package of this
//! repository) uses to make and check B as this library does. **Not part of
//! the API**: it is behind the feature `__cli-internal`, hidden from the
//! documentation, and may change or go in any release without notice. Do not
//! use it (DR-0033).
//!
//! It gives only what the command needs to have the same B as the library
//! (hub DR-0018 "resolve の既定の綴り" 3: B is made and checked as the
//! library does): the places, the process's B of the places 1-3, B at a
//! given path, `sweep` over the places, and the few constants, the split of
//! a path and the random names the command spells and names with, so they
//! are the library's own. No way to choose a route.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::base::{self, Automatic, Place};
use crate::route::{self, Follows};

/// The longest spelling the kernel takes: `sun_path` less the NUL (107 on
/// Linux, Android and illumos, 103 on Apple and the BSDs).
pub const LIMIT: usize = route::DIRECT_MAX;

/// Whether a bind is known to follow a symlink at the last component of its
/// path on this target (Apple, NetBSD). `false` where it is known not to, and
/// where it is not known.
pub const BIND_FOLLOWS_SYMLINK: bool = matches!(route::FOLLOWS, Follows::Yes);

/// The places 1-3 for B, in order (hub SPEC-routes "実体の置き場").
#[derive(Debug, Clone)]
pub struct Places(Vec<(Place, PathBuf)>);

impl Places {
    /// The places of this process: `/tmp`, `$XDG_RUNTIME_DIR`, `$TMPDIR`
    /// (those [`crate::sweep`] and the library's calls use).
    pub fn from_env() -> Self {
        Self(route::Config::from_env().places)
    }

    /// The places with `tmp` in place of `/tmp`, `$XDG_RUNTIME_DIR` and
    /// `$TMPDIR` as in the environment: for the command's tests, which must
    /// not use the `/tmp/sockat-<uid>` the user's processes share.
    pub fn with_tmp(tmp: PathBuf) -> Self {
        Self(base::places_with_tmp(
            tmp,
            std::env::var_os("TMPDIR"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        ))
    }
}

/// A B, held by its fd for as long as this lives (and, for the places 1-3,
/// for the rest of the process, as the library holds it).
#[derive(Debug, Clone)]
pub struct Base(Arc<base::Base>);

impl Base {
    /// B's real path, which spellings through it start with.
    pub fn path(&self) -> &Path {
        &self.0.path
    }
}

/// What [`automatic`] found.
#[derive(Debug)]
pub enum Found {
    /// The process's B, and `fits` held for it.
    Base(Base),
    /// The process's B is (or would be made) where `fits` does not hold:
    /// nothing is made, and the next place is not tried.
    TooLong,
    /// B can be made in none of the places. `occupied`: a place was passed
    /// over because its B's default name was taken by something that fails
    /// the checks.
    Missing {
        /// A default name was taken in a place passed over.
        occupied: bool,
    },
}

/// The process's B of the places 1-3, as the library's calls choose it (hub
/// SPEC-routes "実体の置き場" and "B の作り方"): the first place where B can
/// be made, unless `fits` (given B's real path, made or to be made) says a
/// spelling through it would not fit there.
pub fn automatic(places: &Places, fits: impl Fn(&Path) -> bool) -> Found {
    let occupied = std::cell::Cell::new(false);
    let held = base::Held::process();
    match base::automatic(&places.0, &held, false, fits, &occupied) {
        Automatic::Base(b) => Found::Base(Base(b)),
        Automatic::TooLong => Found::TooLong,
        Automatic::Missing => Found::Missing {
            occupied: occupied.get(),
        },
    }
}

/// A socket path split into its directory and file name, as the library
/// splits it (at the last `/`; `s.sock` is in `.`).
///
/// # Errors
///
/// `InvalidInput` when it does not end with a name (`dir/`, `dir/.`,
/// `dir/..`, `..`, `/`), which the library refuses whatever the length.
pub fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    crate::path::split(path)
}

/// The form of a B the caller names (as `Options.base`): absolute, without
/// NUL, ending with a name. Nothing is looked at on the file system.
///
/// # Errors
///
/// `InvalidInput` when it is not of that form.
pub fn check_base_form(base: &Path) -> io::Result<()> {
    base::split_given(base).map(|_| ())
}

/// The checks of `Options.base` without making anything: the form, the
/// directories on the way, and B itself when it is there.
///
/// # Errors
///
/// `InvalidInput` for the form, `ENOENT`, `ENOTDIR`, `EPERM` as for
/// `Options.base`.
pub fn check_given(base: &Path) -> io::Result<()> {
    base::check_given(base, false)
}

/// B at `base` (as `Options.base`), made when it is not there.
///
/// # Errors
///
/// Those of [`check_given`], and of making B.
pub fn given(base: &Path) -> io::Result<Base> {
    base::given(base, false).map(Base)
}

/// [`crate::sweep`] over `places`.
///
/// # Errors
///
/// Those of [`crate::sweep`].
pub fn sweep(places: &Places) -> io::Result<()> {
    base::sweep(&places.0)
}

/// A random name of 10 ASCII letters and digits from the system's secure
/// generator (the form of the names the library draws).
///
/// # Errors
///
/// When the system's generator fails.
pub fn random_name() -> io::Result<OsString> {
    base::random_name().map(OsString::from)
}
