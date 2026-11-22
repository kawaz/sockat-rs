//! `SOCK_DGRAM` sockets at a path of any length: std's [`UnixDatagram`]
//! functions that take a path, and [`unbound`] that goes with them.
//!
//! The routes and their order are those of the stream functions (see the
//! [crate documentation](crate)): a bind keeps its spelling, so a peer that
//! receives from this socket can reply to the address it reports while the
//! spelling can be followed ([`Spelling`](crate::Spelling)); a connect leaves
//! nothing.
//!
//! ```no_run
//! let path = "/a/very/long/directory/.../server.sock";
//! let server = sockat::datagram::bind(path)?;
//! let client = sockat::datagram::unbound()?;
//! sockat::datagram::connect(&client, path)?;
//! client.send(b"ping")?;
//! let mut buf = [0; 4];
//! server.recv(&mut buf)?;
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

use crate::route::{self, Connect, Request};
use crate::sys::SockType;
use crate::{Bound, ExError, Options, Report};

/// Create a datagram socket bound to `path`, like [`UnixDatagram::bind`] but
/// for a path of any length. As [`bind`](crate::bind): a relative path is
/// made absolute first (so a peer replying to the address it reports reaches
/// this socket from any cwd), the socket file is at `path`, and E stays until the process ends, shared with the other default
/// binds in the same directory ([`bind_ex`] cleans up an E of its own).
///
/// # Errors
///
/// Those of [`bind`](crate::bind).
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixDatagram> {
    let (socket, _, _) = crate::bind_with(
        SockType::Datagram,
        path.as_ref(),
        &Options::default(),
        false,
    )?;
    Ok(UnixDatagram::from(socket))
}

/// A datagram socket bound to nothing: std's [`UnixDatagram::unbound`], to
/// [`connect`] or send from.
///
/// # Errors
///
/// Those of `socket(2)`.
pub fn unbound() -> io::Result<UnixDatagram> {
    UnixDatagram::unbound()
}

/// Connect `socket` to the socket at `path`, like [`UnixDatagram::connect`]
/// but for a path of any length. The routes are those of
/// [`connect`](crate::connect); nothing is left behind.
///
/// # Errors
///
/// Those of `connect(2)` (`ENOENT`, `ECONNREFUSED`, ...), `ENAMETOOLONG` when
/// no route reaches the path, `InvalidInput` as for [`bind`](crate::bind).
pub fn connect<P: AsRef<Path>>(socket: &UnixDatagram, path: P) -> io::Result<()> {
    connect_ex(socket, path, Options::default())
        .map(drop)
        .map_err(io::Error::from)
}

/// [`bind`], with the [`SocketFile`](crate::SocketFile), what the spelling
/// is good for and how it was bound, as [`bind_ex`](crate::bind_ex).
///
/// # Errors
///
/// Those of [`bind_ex`](crate::bind_ex).
pub fn bind_ex<P: AsRef<Path>>(path: P, options: Options) -> Result<Bound<UnixDatagram>, ExError> {
    crate::bound(crate::bind_with(
        SockType::Datagram,
        path.as_ref(),
        &options,
        true,
    )?)
}

/// [`connect`], with how it connected ([`Report`]). `options` chooses B for
/// the link-file route ([`Options::base`]).
///
/// # Errors
///
/// Those of [`connect`] and of [`Options::base`], as an [`ExError`].
pub fn connect_ex<P: AsRef<Path>>(
    socket: &UnixDatagram,
    path: P,
    options: Options,
) -> Result<Report, ExError> {
    let req = Request::Connect(Connect::Datagram(socket));
    let r = route::run(req, path.as_ref(), &options, false, None)?;
    Ok(Report {
        route: r.route,
        skipped: r.skipped,
    })
}
