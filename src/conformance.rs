//! Not part of sockat's API: the entry points of the conformance CLI
//! (`examples/conformance.rs`) that the hub's language-independent
//! conformance test runs. Built only with `--cfg sockat_conformance` and the
//! feature `any` (DR-0030); may change or go away in any release.
//!
//! Each tries the route order of the plain function (`bind` / `connect`)
//! once and reports it in the shape of the `any` functions: the route taken,
//! the routes passed over and what was given up, or the error with the route
//! that failed for real.

use std::path::Path;

use crate::route::{BindOptions, ConnectOptions};
use crate::{AnyConnection, AnyError, AnyListener};

/// [`crate::bind`]'s route order (without `bind_any`'s symlink), reported.
pub fn bind(path: &Path) -> Result<AnyListener, AnyError> {
    crate::bind_reporting(path, BindOptions { symlink: false })
}

/// [`crate::connect`]'s route order (blocking, without the fork route),
/// reported.
pub fn connect(path: &Path) -> Result<AnyConnection, AnyError> {
    let options = ConnectOptions {
        nonblocking: false,
        fork: false,
    };
    crate::connect_reporting(path, options)
}
