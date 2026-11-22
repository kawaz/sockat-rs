# 他言語への長い Unix ソケットパス対応 (2026-11-18 調査)

## 読み方・調査範囲

- 対象は pathname 型の Unix domain socket の bind / listen / connect。基準は Rust 版の設計 ([DR-0001](../decisions/archive/DR-0001-long-socket-path-routes.md))
- 「確」は公開ソース・公式文書・issue 本文・手元の実測で確かめたもの。「見込み」は API の組み合わせからの推論。「未確認」は確かめられなかったもの
- 表の「○」は言語の標準 API で実現できる、「△」は native extension / FFI / 生のシステムコールに頼る、「×」は標準の返却型を保つ公開 API が見当たらない
- 需要の件数は GitHub の issue 検索の結果数 (issue と PR を含む、2026-11-18 時点) で、利用者数ではない。調査中に検索 API の回数上限に達したので、取れなかったものを 0 件とはしない

## Rust (基準の既存版)

| 観点 | 結果 |
|---|---|
| 標準の失敗 | `UnixListener::bind` / `UnixStream::connect` は `bytes.len() >= sun_path.len()` を `InvalidInput` にする。Linux 107、macOS / BSD 103 バイトまで ([std](https://github.com/rust-lang/rust/blob/master/library/std/src/os/unix/net/addr.rs)) |
| 需要の実例 | [rust-lang/rust#160684](https://github.com/rust-lang/rust/issues/160684): macOS のカーネルは 253 バイトを受け付けるのに std が 104 で止める。[PR #161149](https://github.com/rust-lang/rust/pull/161149) で上限の変更案 (調査時点で未 merge)。[Nix#13462](https://github.com/NixOS/nix/issues/13462) は cwd を移して短い名前で bind する例 (検索結果のみ) |
| 件数 | `repo:rust-lang/rust "sun_path"` 11 件、`repo:tokio-rs/tokio "sun_path"` 3 件 |
| 回避策と不足 | `/tmp` に移す、相対パス、cwd を変える等の個別処理。[Fedora の議論](https://www.mail-archive.com/devel@lists.fedoraproject.org/msg202022.html)。std の上限引き上げだけでは 253 バイト超、Linux、symlink / fork の経路は解決しない |
| 需要 | 強 |

std が macOS で 253 バイトまで受け付けるようになったら、sockat の「std が受け付けるパスは std に任せる」閾値を追従させる。

## Go

| 観点 | 結果 |
|---|---|
| 標準の失敗 | `net.Listen("unix", ...)` / `net.Dial` は `syscall.SockaddrUnix` で長さを判定し `EINVAL` ([Linux](https://github.com/golang/go/blob/master/src/syscall/syscall_linux.go)、[BSD](https://github.com/golang/go/blob/master/src/syscall/syscall_bsd.go))。[golang/go#6895](https://github.com/golang/go/issues/6895) はエラーの分かりにくさの報告 |
| 需要の実例 | [golang/go#62614](https://github.com/golang/go/issues/62614): `t.TempDir()` の下の 114 バイトのパスが macOS / FreeBSD / OpenBSD の 104 に当たる (本文確認)。同 issue に go-plugin の報告も |
| 件数 | `repo:golang/go "sun_path"` 12 件、`repo:golang/go "too long" "socket" "TempDir"` 3 件 |
| 回避策と不足 | [josharian/sockpath](https://github.com/josharian/sockpath) は試験用の短い名前を返すだけで、本番の既存パスで bind / connect はできない |
| 経路と返却型 | ディレクトリの fd を保持でき、/proc・相対パス・symlink は組める見込み。bindat と macOS の広げた sockaddr は `golang.org/x/sys/unix` と生のシステムコールが要る。fd から標準型は `net.FileListener` / `net.FileConn` で作れる ([ソース](https://github.com/golang/go/blob/master/src/net/file.go))。runtime の都合で生の fork は使えない |
| 配布・CI | module path `github.com/kawaz/sockat-go` は proxy で 404 (未公開)。tag を push すれば公開。CI は ubuntu / macos の matrix で `go test ./...` |
| 需要 | 強 |

## JavaScript / TypeScript

| Runtime | 標準の振る舞い | 使える経路と標準型 | 需要 |
|---|---|---|---|
| Node.js | macOS の Node 26.13.1 の実測で、160〜253 バイトの listen / connect が成功し 255 で `EINVAL` (libuv が macOS で広げた sockaddr を使う、[pipe.c](https://github.com/libuv/libuv/blob/v1.x/src/unix/pipe.c))。Linux の Node 22 で長いパスが切り詰められて listen が成功扱いになる報告あり ([agenthook#439](https://github.com/Jesuso/agenthook/issues/439))。OS と版で境界が違う | symlink と相対パスを `net.Server` / `net.Socket` に渡す形は見込み。/proc・bindat・fork は native addon が要る | 強 |
| Bun | macOS の Bun 1.4.4 の実測で 160 バイト成功、210 で `EINVAL`。[Bun#47407](https://github.com/oven-sh/bun/issues/47407) | symlink と相対パスは見込み。listen 済みの fd から `Bun.Listener` を作れるかは未確認 | 中 |
| Deno | macOS の Deno 2.9.8 の実測で 101 バイト可、160 は `path must be shorter than SUN_LEN` | symlink と相対パスを `Deno.listen` に渡す形は見込み。FFI の fd を標準の listener に取り込む API は見当たらない | 弱 |

| 観点 | 結果 |
|---|---|
| 需要の実例 | [Nuxt#35275](https://github.com/nuxt/nuxt/issues/35275)、[Playwright#40878](https://github.com/microsoft/playwright/issues/40878)、[playwright-cli#474](https://github.com/microsoft/playwright-cli/issues/474) (macOS の深い一時ディレクトリ)、[undici#5978](https://github.com/nodejs/undici/pull/5978) (Node の版による切り詰め)。どれも Node の `net.Server` の障害とは限らない |
| 件数 | 検索 API の上限で未取得 |
| 回避策と不足 | `TMPDIR=/tmp`、専用ディレクトリ、Linux の abstract socket、個別の `/tmp` への退避。どれも元のパスにソケットを作れない |
| 配布・CI | npm の `sockat` は 404 (未公開)。npm の trusted publishing は既存 package の設定画面が要るので、初回の公開方法は別に確かめる。1 つの npm package で Node / Bun / Deno を兼ねられるかは未確認 |

## Python

| 観点 | 結果 |
|---|---|
| 標準の失敗 | CPython の [socketmodule.c](https://github.com/python/cpython/blob/main/Modules/socketmodule.c) が `path.len >= sizeof sun_path` で `OSError("AF_UNIX path too long")` (errno なし)。Linux 107、macOS 103 バイトまで |
| 需要の実例 | [CPython#93852](https://github.com/python/cpython/issues/93852) (長い TMPDIR の下の試験)、[PR#93914](https://github.com/python/cpython/pull/93914) (試験用に cwd にソケットを作る)、[#132124](https://github.com/python/cpython/issues/132124)・[#140734](https://github.com/python/cpython/issues/140734)・[#149527](https://github.com/python/cpython/issues/149527) (multiprocessing / forkserver)。[mitogen#855](https://github.com/mitogen-hq/mitogen/issues/855) は検索結果のみ |
| 件数 | CPython の関連 issue は少なくとも上の 4 件 (全体の件数は誤ヒットが多く使わない) |
| 回避策と不足 | [multiprocessing/util.py](https://github.com/python/cpython/blob/main/Lib/multiprocessing/util.py) は長い tempdir から `/tmp` に退避するが、multiprocessing の外は救わない |
| 経路と返却型 | `os.open` (Linux は `O_PATH`) で fd を保持でき、/proc・相対パス・symlink は標準ライブラリで組める見込み。`socket.socket(fileno=fd)` で標準型に取り込める。bindat と広げた sockaddr は ctypes / native。fork は子の区間を async-signal-safe にするため native が要る |
| 配布・CI | PyPI の `sockat` は 404。PyPI の pending trusted publisher で新規 project を OIDC で初回公開できる (名前の予約にはならない) |
| 需要 | 強 |

## Ruby

| 観点 | 結果 |
|---|---|
| 標準の失敗 | [unixsocket.c](https://github.com/ruby/ruby/blob/master/ext/socket/unixsocket.c) が `sizeof(sun_path) < RSTRING_LEN(path)` で `ArgumentError("too long unix socket path")`。ちょうど同じ長さはここでは断らないので、境界は未確定 |
| 需要の実例 | [ruby/debug#982](https://github.com/ruby/debug/issues/982)、[vscode-rdbg#23](https://github.com/ruby/vscode-rdbg/issues/23) (macOS の `/var/folders/.../T`)、[CocoaPods#11640](https://github.com/CocoaPods/CocoaPods/issues/11640)、[passenger#2397](https://github.com/phusion/passenger/issues/2397)、[fpm#2052](https://github.com/jordansissel/fpm/issues/2052) (どれも検索結果のみ) |
| 件数 | `repo:ruby/ruby "too long unix socket path"` 2 件 |
| 回避策と不足 | [ruby/ruby#7749](https://github.com/ruby/ruby/pull/7749) は試験の一時ディレクトリを短くするだけ |
| 経路と返却型 | ディレクトリの fd と `File.symlink` で `UNIXServer.new` / `UNIXSocket.new` に短い綴りを渡せる見込み。`for_fd` で標準型に取り込めるかは未確認。fork は native で再評価 |
| 配布・CI | RubyGems の `sockat` は 404。RubyGems の trusted publishing は新規 gem の pending publisher に対応 |
| 需要 | 中 |

## ほかの候補

| 言語 | 標準の振る舞い・現場 | 実装の論点 | 評価 |
|---|---|---|---|
| Java / JVM | [OpenJDK](https://github.com/openjdk/jdk/blob/master/src/java.base/unix/native/libnio/ch/UnixDomainSockets.c) は `sizeof(sun_path)-2` を超えると `SocketException("Unix domain path too long")` (Rust std より 1 バイト厳しい) | 相対パスと symlink は `UnixDomainSocketAddress.of` に渡せる。自前の fd を標準の channel に載せる方法は未確認 | 中 |
| .NET | [UnixDomainSocketEndPoint](https://github.com/dotnet/runtime/blob/main/src/libraries/System.Net.Sockets/src/System/Net/Sockets/UnixDomainSocketEndPoint.cs) は上限超で `ArgumentOutOfRangeException`。[vs-servicehub#709](https://github.com/microsoft/vs-servicehub/pull/709) は macOS の深い TMPDIR での失敗 | symlink は標準の `Socket` に渡せる。自前の fd も `Socket(SafeSocketHandle)` で取り込める | 強 (問題)・中 (ライブラリ) |
| PHP | [sockets.c](https://github.com/php/php-src/blob/master/ext/sockets/sockets.c) は `>= sizeof(sun_path)` で `ValueError` | symlink 経由は見込み。fd の取り込みは未確認 | 未確認 |
| Swift、Zig、Elixir / Erlang、Perl | 未確認 | 未確認 | 未確認 |

## 言語 × 経路 (実現の見込み。検証済みではない)

| 言語 | Linux /proc | FreeBSD bindat | 相対パス | macOS 広げた sockaddr | symlink | fork | fd → 標準型 |
|---|---|---|---|---|---|---|---|
| Rust (既存) | ○ | ○ | ○ | ○ | ○ | ○ | ○ |
| Go | ○ | △ | ○ | △ | ○ | × (runtime) | ○ `net.FileListener` / `FileConn` |
| Node | △ addon | △ addon | ○ | ○ (libuv が既に扱う) | ○ | △ addon | △ 未検証 |
| Bun | △ FFI | △ FFI | ○ | △ FFI | ○ | △ FFI | △ listen は未確認 |
| Deno | △ FFI | △ FFI | ○ | △ FFI | ○ | △ FFI | × |
| Python | ○ | △ | ○ | △ | ○ | △ native | ○ `socket(fileno=)` |
| Ruby | ○ | △ | ○ | △ | ○ | △ native | ? `for_fd` |
| Java | ? | △ JNI | ○ | △ JNI | ○ | △ JNI | ? |
| .NET | ○ | △ P/Invoke | ○ | △ P/Invoke | ○ | △ native | ○ `Socket(SafeSocketHandle)` |
| PHP | ? | △ | ○ | △ | ○ | △ | ? |

## 作る順の提案

| 順 | 言語 | 問題の需要 | 根拠 |
|---|---|---|---|
| 1 | Python | 強 | 標準ライブラリ自身が何度も対処し、利用側の事故も多い。標準ライブラリだけで /proc・相対パス・symlink を組める |
| 2 | Go | 強 | 言語本体の `TempDir` の issue が明確。`net.FileListener` / `FileConn` で標準型に載せられる |
| 3 | Node.js | 強 | ツール横断で報告が多い。macOS の Node は 253 バイトまで既に通るので、253 超・Linux・元のパスの維持に絞る |
| 4 | Ruby | 中 | 開発ツール・gem での報告が複数。境界と `for_fd` を先に確かめる |
| 5 | .NET | 強 (問題) | 具体的な失敗あり。標準の `Socket` に fd を渡せる |
| 6 | Bun | 中 | 本体の issue あり。経路と上限が未判明 |
| 7 | Deno | 弱 | 失敗は実測したが利用者の報告が乏しい |
| 保留 | Java、PHP、Swift、Zig、Elixir / Erlang、Perl | 未確認 | 戻り型と公開 API、利用者の報告を調べてから |

試作で確かめること: macOS の深い TMPDIR、Linux の /proc あり / なし、104 / 108 バイトの境界、symlink を作れない環境、`getsockname` の結果、標準型として使えること、指定したパスにソケットが実際にできること。
