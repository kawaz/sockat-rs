//! The sample code of the user manual (`docs/manual/*.md`), run for real.
//!
//! Each module holds one code block of the manual as it is written there,
//! followed by the test that runs it at a path well past `sun_path`. The
//! comment above each module names the page and the section; the Japanese
//! page (`*-ja.md`) and the English one carry the same code. When a sample
//! changes, change it here and in both pages.
//!
//! The samples use the default places for B (`/tmp/sockat-<uid>` first),
//! which the tests of every sockat implementation share: a test here only
//! adds its own D and E there, as `tests/public_api.rs` does.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread;

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("srt-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// `<dir>/d…/<name>`, about 300 bytes: past `sun_path` on every OS, and
/// within what the link routes reach, so resolve gives a spelling for it.
fn long_path(dir: &Path, name: &str) -> PathBuf {
    let mut d = dir.to_path_buf();
    while d.as_os_str().len() < 280 {
        d.push("d".repeat(60));
    }
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(name);
    assert!(p.as_os_str().as_bytes().len() > 200);
    p
}

/// A server for the connect samples: answers each connection with the line
/// it sent. Bound before this returns, so a connect after it reaches it.
fn echo_server(path: &Path) -> UnixListener {
    let listener = sockat::bind(path).unwrap();
    let accepting = listener.try_clone().unwrap();
    thread::spawn(move || {
        for mut stream in accepting.incoming().flatten() {
            thread::spawn(move || {
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line)?;
                stream.write_all(line.as_bytes())
            });
        }
    });
    listener
}

/// Sends `line` on a fresh connection to `path` and reads the answer.
fn ask_line(path: &Path, line: &str) -> String {
    let mut stream = sockat::connect(path).unwrap();
    stream.write_all(line.as_bytes()).unwrap();
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer).unwrap();
    answer
}

/// Whether this std drops the last byte of the addresses it reads (Rust 1.99
/// and earlier on OpenBSD, rust-lang/rust#118925): a reply to the address
/// from std's `recv_from` cannot arrive there. Measured as in
/// `tests/public_api.rs`.
fn std_drops_last_byte() -> bool {
    let t = tmp();
    let p = t.path().join("s.sock");
    let l = UnixListener::bind(&p).unwrap();
    let read = l.local_addr().unwrap();
    read.as_pathname().unwrap() != p
}

// docs/manual/bind-ja.md「待ち受ける」, bind.md "Listen"
mod bind_listen {
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::thread;

    fn echo(mut stream: UnixStream) -> io::Result<()> {
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        stream.write_all(line.as_bytes())
    }

    fn start(path: &Path) -> io::Result<()> {
        let listener = sockat::bind(path)?;
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || echo(stream));
            }
        });
        Ok(())
    }

    // start binds before it returns and serves in its own thread: a client
    // connecting at the long path gets its line back, twice over (the
    // accept loop goes on).
    #[test]
    fn listens_at_a_long_path() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        start(&path).unwrap();
        assert_eq!(super::ask_line(&path, "one\n"), "one\n");
        assert_eq!(super::ask_line(&path, "two\n"), "two\n");
    }
}

// docs/manual/bind-ja.md「tokio で待ち受ける」, bind.md "Listen with tokio"
mod bind_tokio {
    use std::io;
    use std::path::Path;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    async fn echo(mut stream: UnixStream) -> io::Result<()> {
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).await?;
        stream.write_all(line.as_bytes()).await
    }

    async fn start(path: &Path) -> io::Result<()> {
        let listener = sockat::bind(path)?;
        listener.set_nonblocking(true)?;
        let listener = UnixListener::from_std(listener)?;
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(echo(stream));
            }
        });
        Ok(())
    }

    // The listener goes to tokio and is served on the runtime; a client of
    // tokio's own at the long path (through sockat's nonblocking connect)
    // gets its line back.
    #[tokio::test]
    async fn listens_with_tokio_at_a_long_path() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        start(&path).await.unwrap();
        let stream = sockat::connect_nonblocking(&path).unwrap();
        let mut stream = UnixStream::from_std(stream).unwrap();
        stream.write_all(b"one\n").await.unwrap();
        let mut answer = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut answer)
            .await
            .unwrap();
        assert_eq!(answer, "one\n");
    }
}

// docs/manual/bind-ja.md「終わる時にソケットのファイルを消す」, bind.md
// "Remove the socket file when done"
mod bind_remove {
    use std::io;
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    fn run(path: &Path, serve: impl FnOnce(&UnixListener) -> io::Result<()>) -> io::Result<()> {
        let bound = sockat::bind_ex(path, sockat::Options::default())?;
        let served = serve(&bound.socket);
        bound.file.remove()?;
        drop(bound.socket);
        served
    }

    // While serve runs the socket file is at the long path and reachable;
    // once run returns it is gone, whether serve succeeded or failed (and
    // serve's error is what run returns).
    #[test]
    fn the_socket_file_goes_when_run_returns() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        run(&path, |listener| {
            assert!(path.exists());
            let _client = sockat::connect(&path)?;
            listener.accept().map(drop)
        })
        .unwrap();
        assert!(!path.exists());

        let e = run(&path, |_| Err(io::Error::other("serve failed"))).unwrap_err();
        assert_eq!(e.to_string(), "serve failed");
        assert!(!path.exists());
    }
}

// docs/manual/bind-ja.md「置き場を自分で決める」, bind.md "Choose the base
// yourself"
mod bind_base {
    use std::io;
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    fn listen(path: &Path, app_dir: &Path) -> io::Result<UnixListener> {
        let base = app_dir.join("sockat");
        sockat::sweep_base(&base)?;
        let options = sockat::Options {
            base: Some(base),
            ..Default::default()
        };
        Ok(sockat::bind_ex(path, options)?.socket)
    }

    // The bind at the long path is reachable, and whatever B it needed is
    // the one under app_dir (made on the first use: sweep_base before it
    // finds nothing and makes nothing). A second listen at another path
    // sweeps the same B and leaves this process's live D, so both stay
    // reachable.
    #[test]
    fn listens_through_the_given_base() {
        let t = super::tmp();
        let app_dir = t.path().join("app");
        std::fs::create_dir(&app_dir).unwrap();
        let one = super::long_path(&t.path().join("x"), "one.sock");
        let two = super::long_path(&t.path().join("y"), "two.sock");
        let _l1 = listen(&one, &app_dir).unwrap();
        let _l2 = listen(&two, &app_dir).unwrap();
        sockat::connect(&one).unwrap();
        sockat::connect(&two).unwrap();
        for entry in std::fs::read_dir(&app_dir).unwrap() {
            assert_eq!(entry.unwrap().file_name(), "sockat");
        }
    }
}

// docs/manual/bind-ja.md「置き場に残った物を片付ける」, bind.md "Clear what
// was left in the bases"
mod bind_sweep {
    use std::io;
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    fn listen(path: &Path) -> io::Result<UnixListener> {
        sockat::sweep()?;
        sockat::bind(path)
    }

    // sweep before the bind succeeds and leaves the live D of this process
    // (other tests bind through it at the same time): the bind after it and
    // a connect to it work.
    #[test]
    fn sweeps_then_listens() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let _listener = listen(&path).unwrap();
        sockat::connect(&path).unwrap();
    }
}

// docs/manual/connect-ja.md「つなぐ」, connect.md "Connect"
mod connect_connect {
    use std::io::{self, BufRead, BufReader, Write};
    use std::path::Path;

    fn ask(path: &Path, question: &str) -> io::Result<String> {
        let mut stream = sockat::connect(path)?;
        stream.write_all(question.as_bytes())?;
        let mut answer = String::new();
        BufReader::new(&stream).read_line(&mut answer)?;
        Ok(answer)
    }

    // A server at the long path answers the line ask sends.
    #[test]
    fn connects_to_a_long_path() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let _server = super::echo_server(&path);
        assert_eq!(ask(&path, "one\n").unwrap(), "one\n");
    }
}

// docs/manual/connect-ja.md「tokio でつなぐ」, connect.md "Connect with tokio"
mod connect_tokio {
    use std::io;
    use std::path::Path;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    async fn ask(path: &Path, question: &str) -> io::Result<String> {
        let stream = sockat::connect_nonblocking(path)?;
        let mut stream = UnixStream::from_std(stream)?;
        stream.write_all(question.as_bytes()).await?;
        let mut answer = String::new();
        BufReader::new(&mut stream).read_line(&mut answer).await?;
        Ok(answer)
    }

    // From the runtime, a server at the long path answers the line ask
    // sends.
    #[tokio::test]
    async fn connects_with_tokio_to_a_long_path() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let _server = super::echo_server(&path);
        assert_eq!(ask(&path, "one\n").await.unwrap(), "one\n");
    }
}

// docs/manual/connect-ja.md「bind と同じ置き場を使う」, connect.md "Use the
// same base as the bind"
mod connect_base {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    fn connect(path: &Path, app_dir: &Path) -> io::Result<UnixStream> {
        let options = sockat::Options {
            base: Some(app_dir.join("sockat")),
            ..Default::default()
        };
        Ok(sockat::connect_ex(path, options)?.socket)
    }

    // The connect through the application's base reaches a server at the
    // long path, and makes nothing under app_dir but that B.
    #[test]
    fn connects_through_the_given_base() {
        use std::io::{BufRead, BufReader, Write};
        let t = super::tmp();
        let app_dir = t.path().join("app");
        std::fs::create_dir(&app_dir).unwrap();
        let path = super::long_path(&t.path().join("x"), "app.sock");
        let _server = super::echo_server(&path);
        let mut stream = connect(&path, &app_dir).unwrap();
        stream.write_all(b"one\n").unwrap();
        let mut answer = String::new();
        BufReader::new(&stream).read_line(&mut answer).unwrap();
        assert_eq!(answer, "one\n");
        for entry in std::fs::read_dir(&app_dir).unwrap() {
            assert_eq!(entry.unwrap().file_name(), "sockat");
        }
    }
}

// docs/manual/connect-ja.md「どう届いたかを見る」, connect.md "See how it was
// reached"
mod connect_diagnostics {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    fn connect(path: &Path) -> io::Result<UnixStream> {
        let connected = sockat::connect_ex(path, sockat::Options::default())
            .inspect_err(|e| eprintln!("failed: route {:?}, skipped {:?}", e.route, e.skipped))?;
        eprintln!("route {}, skipped {:?}", connected.route, connected.skipped);
        Ok(connected.socket)
    }

    // Both ways are logged: a server at the long path is reached, and with
    // nothing there the error comes back as the io::Error of the connect
    // (ENOENT).
    #[test]
    fn reports_both_ways() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let missing = super::long_path(t.path(), "none.sock");
        let _server = super::echo_server(&path);
        connect(&path).unwrap();
        let e = connect(&missing).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
    }
}

// docs/manual/resolve-ja.md「std で bind する」, resolve.md "Bind with std"
mod resolve_bind {
    use std::io;
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    fn listen(path: &Path) -> io::Result<(UnixListener, sockat::SocketFile)> {
        let spelled = sockat::resolve_for_bind(path, sockat::Options::default())?;
        let listener = match UnixListener::bind(&spelled.path) {
            Ok(listener) => listener,
            Err(e) => {
                spelled.cleanup()?;
                return Err(e);
            }
        };
        let file = spelled.socket_file()?;
        Ok((listener, file))
    }

    fn stop(listener: UnixListener, file: sockat::SocketFile) -> io::Result<()> {
        file.remove()?;
        drop(listener);
        Ok(())
    }

    // std binds the spelling; a client at the original long path reaches
    // it, and stop leaves no socket file at the path. A bind that fails
    // (another socket at the path) returns its own error, EADDRINUSE, after
    // cleaning up the spelling, and the other socket's file stays.
    #[test]
    fn std_listens_at_the_spelling() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let (listener, file) = listen(&path).unwrap();
        assert!(path.exists());
        sockat::connect(&path).unwrap();
        listener.accept().unwrap();
        stop(listener, file).unwrap();
        assert!(!path.exists());

        let _other = super::echo_server(&path);
        let e = listen(&path).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EADDRINUSE));
        assert!(path.exists());
    }
}

// docs/manual/resolve-ja.md「同じ相手に何度もつなぐ」, resolve.md "Connect
// to the same peer many times"
mod resolve_connect {
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    fn ask(to: &Path, question: &str) -> io::Result<String> {
        let mut stream = UnixStream::connect(to)?;
        stream.write_all(question.as_bytes())?;
        let mut answer = String::new();
        BufReader::new(&stream).read_line(&mut answer)?;
        Ok(answer)
    }

    fn ask_all(path: &Path, questions: &[&str]) -> io::Result<Vec<String>> {
        let to = sockat::resolve_for_connect(path, sockat::Options::default())?;
        let answers = questions.iter().map(|q| ask(&to.path, q)).collect();
        to.cleanup()?;
        answers
    }

    // Every question goes through the one spelling to the server at the
    // long path and gets its answer.
    #[test]
    fn std_connects_through_the_spelling_many_times() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let _server = super::echo_server(&path);
        let answers = ask_all(&path, &["one\n", "two\n", "three\n"]).unwrap();
        assert_eq!(answers, ["one\n", "two\n", "three\n"]);
    }
}

// docs/manual/resolve-ja.md「パスを取るライブラリに渡す」, resolve.md "Hand
// it to a library that takes a path"
mod resolve_tokio {
    use std::io;
    use std::path::Path;
    use tokio::net::{UnixListener, UnixStream};

    async fn listen(path: &Path) -> io::Result<(UnixListener, sockat::SocketFile)> {
        let spelled = sockat::resolve_for_bind(path, sockat::Options::default())?;
        let listener = match UnixListener::bind(&spelled.path) {
            Ok(listener) => listener,
            Err(e) => {
                spelled.cleanup()?;
                return Err(e);
            }
        };
        let file = spelled.socket_file()?;
        Ok((listener, file))
    }

    async fn connect(path: &Path) -> io::Result<UnixStream> {
        let to = sockat::resolve_for_connect(path, sockat::Options::default())?;
        let stream = UnixStream::connect(&to.path).await;
        to.cleanup()?;
        stream
    }

    // tokio binds and connects by the spellings; the two meet at the long
    // path, and removing the SocketFile leaves no socket file there.
    #[tokio::test]
    async fn tokio_binds_and_connects_by_the_spellings() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let (listener, file) = listen(&path).await.unwrap();
        let _client = connect(&path).await.unwrap();
        listener.accept().await.unwrap();
        file.remove().unwrap();
        assert!(!path.exists());

        let _other = super::echo_server(&path);
        let e = listen(&path).await.unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EADDRINUSE));
    }
}

// docs/manual/datagram-ja.md「受ける」, datagram.md "Receive"
mod datagram_receive {
    use std::io;
    use std::os::unix::net::UnixDatagram;
    use std::path::Path;

    fn open(path: &Path) -> io::Result<UnixDatagram> {
        sockat::datagram::bind(path)
    }

    fn receive(socket: &UnixDatagram) -> io::Result<Vec<u8>> {
        let mut buf = [0; 1024];
        let n = socket.recv(&mut buf)?;
        Ok(buf[..n].to_vec())
    }

    // A datagram sent to the long path arrives at the socket open bound.
    #[test]
    fn receives_at_a_long_path() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let socket = open(&path).unwrap();
        let sender = sockat::datagram::unbound().unwrap();
        sockat::datagram::connect(&sender, &path).unwrap();
        sender.send(b"one").unwrap();
        assert_eq!(receive(&socket).unwrap(), b"one");
    }
}

// docs/manual/datagram-ja.md「送る」, datagram.md "Send"
mod datagram_send {
    use std::io;
    use std::path::Path;

    fn send(path: &Path, message: &[u8]) -> io::Result<()> {
        let socket = sockat::datagram::unbound()?;
        sockat::datagram::connect(&socket, path)?;
        socket.send(message)?;
        Ok(())
    }

    // What send sends arrives at a socket bound at the long path.
    #[test]
    fn sends_to_a_long_path() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let socket = sockat::datagram::bind(&path).unwrap();
        send(&path, b"one").unwrap();
        let mut buf = [0; 8];
        let n = socket.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"one");
    }
}

// docs/manual/datagram-ja.md「返事をもらう」, datagram.md "Get a reply"
mod datagram_reply {
    use std::io;
    use std::os::unix::net::UnixDatagram;
    use std::path::Path;

    fn ask(path: &Path, my_path: &Path, question: &[u8]) -> io::Result<Vec<u8>> {
        let socket = sockat::datagram::bind(my_path)?;
        sockat::datagram::connect(&socket, path)?;
        socket.send(question)?;
        let mut buf = [0; 1024];
        let n = socket.recv(&mut buf)?;
        Ok(buf[..n].to_vec())
    }

    fn answer(socket: &UnixDatagram) -> io::Result<()> {
        let mut buf = [0; 1024];
        let (n, from) = socket.recv_from(&mut buf)?;
        if let Some(from) = from.as_pathname() {
            socket.send_to(&buf[..n], from)?;
        }
        Ok(())
    }

    // Both sides at long paths: answer replies to the address recv_from
    // gives (the spelling of ask's bind), and the reply arrives. Left out
    // where std drops the last byte of the addresses it reads (the reply
    // would go nowhere and ask would wait for ever), and where the default
    // bind's spelling cannot be followed (not Linked), the two cases the
    // manual's sentence names.
    #[test]
    fn the_reply_reaches_a_sender_at_a_long_path() {
        if super::std_drops_last_byte() {
            eprintln!(
                "reply not tried: this std drops the last byte of the addresses it reads \
                 (rust-lang/rust#118925, fixed in Rust 1.100.0)"
            );
            return;
        }
        let t = super::tmp();
        let probe = super::long_path(&t.path().join("p"), "probe.sock");
        let probe = sockat::datagram::bind_ex(&probe, sockat::Options::default()).unwrap();
        let linked = matches!(probe.spelling, sockat::Spelling::Linked { .. });
        probe.file.remove().unwrap();
        if !linked {
            eprintln!(
                "reply not tried: the bind's spelling is {:?}",
                probe.spelling
            );
            return;
        }
        let path = super::long_path(&t.path().join("s"), "server.sock");
        let my_path = super::long_path(&t.path().join("c"), "client.sock");
        let server = sockat::datagram::bind(&path).unwrap();
        let replier = std::thread::spawn(move || answer(&server));
        assert_eq!(ask(&path, &my_path, b"one").unwrap(), b"one");
        replier.join().unwrap().unwrap();
    }
}

// docs/manual/datagram-ja.md「長いパスへ send_to する」, datagram.md
// "send_to a long path"
mod datagram_send_to {
    use std::io;
    use std::os::unix::net::UnixDatagram;
    use std::path::Path;

    fn send_to(socket: &UnixDatagram, path: &Path, message: &[u8]) -> io::Result<()> {
        let to = sockat::resolve_for_connect(path, sockat::Options::default())?;
        let sent = socket.send_to(message, &to.path);
        to.cleanup()?;
        sent.map(drop)
    }

    // An unbound socket's send_to by the spelling reaches the socket at the
    // long path.
    #[test]
    fn sends_to_a_long_path_by_the_spelling() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        let receiver = sockat::datagram::bind(&path).unwrap();
        let socket = sockat::datagram::unbound().unwrap();
        send_to(&socket, &path, b"one").unwrap();
        let mut buf = [0; 8];
        let n = receiver.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"one");
    }
}

// docs/manual/datagram-ja.md「終わる時にソケットのファイルを消す」,
// datagram.md "Remove the socket file when done"
mod datagram_remove {
    use std::io;
    use std::os::unix::net::UnixDatagram;
    use std::path::Path;

    fn run(path: &Path, serve: impl FnOnce(&UnixDatagram) -> io::Result<()>) -> io::Result<()> {
        let bound = sockat::datagram::bind_ex(path, sockat::Options::default())?;
        let served = serve(&bound.socket);
        bound.file.remove()?;
        drop(bound.socket);
        served
    }

    // While serve runs, a datagram sent to the long path arrives; once run
    // returns the socket file is gone.
    #[test]
    fn the_socket_file_goes_when_run_returns() {
        let t = super::tmp();
        let path = super::long_path(t.path(), "app.sock");
        run(&path, |socket| {
            let sender = sockat::datagram::unbound()?;
            sockat::datagram::connect(&sender, &path)?;
            sender.send(b"one")?;
            let mut buf = [0; 8];
            let n = socket.recv(&mut buf)?;
            assert_eq!(&buf[..n], b"one");
            Ok(())
        })
        .unwrap();
        assert!(!path.exists());
    }
}

// Every ```rust block of the manual's pages is in this file, as consecutive
// lines, so the code a reader copies is the code the tests above run. The
// manual and this file are written by hand, and nothing else ties them
// together: a sample changed on one side only would otherwise go unnoticed.
// Leading whitespace is ignored (a block sits one level deep in its module
// here) and so are blank lines.
#[test]
fn every_sample_of_the_manual_is_here() {
    const PAGES: [(&str, &str); 10] = [
        ("bind-ja.md", include_str!("../docs/manual/bind-ja.md")),
        ("bind.md", include_str!("../docs/manual/bind.md")),
        (
            "connect-ja.md",
            include_str!("../docs/manual/connect-ja.md"),
        ),
        ("connect.md", include_str!("../docs/manual/connect.md")),
        (
            "seqpacket-ja.md",
            include_str!("../docs/manual/seqpacket-ja.md"),
        ),
        ("seqpacket.md", include_str!("../docs/manual/seqpacket.md")),
        (
            "resolve-ja.md",
            include_str!("../docs/manual/resolve-ja.md"),
        ),
        ("resolve.md", include_str!("../docs/manual/resolve.md")),
        (
            "datagram-ja.md",
            include_str!("../docs/manual/datagram-ja.md"),
        ),
        ("datagram.md", include_str!("../docs/manual/datagram.md")),
    ];
    fn lines(text: &str) -> Vec<&str> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect()
    }
    let here = lines(include_str!("manual_samples.rs"));
    let mut blocks = 0;
    for (page, text) in PAGES {
        let mut rest = text;
        while let Some(start) = rest.find("```rust\n") {
            let body = &rest[start + "```rust\n".len()..];
            let end = body.find("```").expect("an unclosed code block");
            let block = lines(&body[..end]);
            assert!(
                here.windows(block.len()).any(|w| w == block.as_slice()),
                "a sample of docs/manual/{page} is not in tests/manual_samples.rs:\n{}",
                block.join("\n")
            );
            blocks += 1;
            rest = &body[end + 3..];
        }
    }
    assert!(blocks > 0);
}
