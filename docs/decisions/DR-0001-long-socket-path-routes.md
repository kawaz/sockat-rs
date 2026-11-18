# DR-0001: sun_path を超える socket のパスに届く経路と、試す順番

- Status: Accepted
- Date: 2026-11-18

## Context

Unix domain socket のパスは `struct sockaddr_un` の `sun_path` に入れて `bind(2)` / `connect(2)` に渡す。`sun_path` は固定長で、NUL 込みで macOS と BSD が 104 バイト、Linux が 108 バイトしかない。std の `UnixListener::bind` / `UnixStream::connect` はこれを超えるパスを断るが、ファイルシステムの側にはそんな上限は無い。深い一時ディレクトリ (macOS の `$TMPDIR` はそれだけで 50 バイト前後、試験の枠組みやサンドボックスがさらに掘る) にソケットを置くとすぐ当たる。

上限が効くのは `bind` / `connect` に渡す `sun_path` 引数の長さで、ファイルシステム上のフルパスの長さではない。同じ場所を指す短い綴りを渡せればよく、その作り方は OS で違う。

| 先行の実装と回避策 | やり方 | 足りない所 |
|---|---|---|
| hyoui (DR-0041 決定 5) | 上限を超えたら、ディレクトリの fd に `fchdir` した子を fork して、ファイル名だけで bind / connect する | 毎回 fork する。connect した側の pid がサーバから子に見える |
| llm-gateway の試験 | `/tmp` に短い symlink を張って経由する | 試験の中だけの手当てで、ライブラリになっていない |

## Decision

- std が受け付けるパスは std にそのまま渡す ([DR-0002](DR-0002-std-handles-paths-it-accepts.md))
- それを超えるパスは、OS ごとに次の順に試し、最初に使えた経路で bind / connect する

  | OS | 試す順番 |
  |---|---|
  | Linux / Android (/proc を使える) | /proc |
  | Linux / Android (/proc を使えない) | 相対パス → symlink → スレッド cwd → fork |
  | FreeBSD | bindat |
  | macOS / iOS 等 Apple の OS | 相対パス → 広げた sockaddr → symlink → スレッド cwd → fork |
  | NetBSD / OpenBSD / illumos / その他の Unix | 相対パス → symlink → fork |
  | Windows | 対象外 ([DR-0009](DR-0009-windows-out-of-scope.md)) |

- NUL を含むパスは、長さを見る前に `InvalidInput` にする。Linux / Android の先頭の NUL (abstract namespace) は対象外で std にそのまま渡す ([DR-0021](DR-0021-nul-is-invalid-at-any-length.md))
- **順番の方針: 副作用の無い安い経路から試し、fork は最後に回す。** ディレクトリの fd を基準にできる primitive (Linux の /proc、FreeBSD の `bindat`) が使える時は、それだけを使う
- どの経路も呼び出し元のプロセスの cwd を変えない
- fork の経路を試すのは `bind` と `connect_any` 系だけ ([DR-0010](DR-0010-public-api.md))
- 経路が使えないのか、経路で本当に失敗したのかの扱いは [DR-0012](DR-0012-route-failure-handling.md)

| 経路 | 使う OS | DR |
|---|---|---|
| /proc (`/proc/self/fd/<fd>/<ファイル名>`) | Linux / Android | [DR-0003](DR-0003-linux-proc-self-fd.md) |
| 相対パス (cwd から) | FreeBSD 以外 | [DR-0004](DR-0004-relative-path-from-cwd.md) |
| 広げた sockaddr (253 バイト) | Apple の OS | [DR-0005](DR-0005-widened-sockaddr-on-macos.md) |
| symlink (安全な置き場の短い symlink を経由) | FreeBSD 以外 | [DR-0006](DR-0006-symlink-in-safe-temp-dir.md) |
| スレッド cwd (使い捨てのスレッドだけ cwd を移す) | Linux / Android、Apple の OS | [DR-0020](DR-0020-thread-cwd-route.md) |
| fork (`fchdir` した子が bind / connect) | FreeBSD 以外 | [DR-0007](DR-0007-fork-child-fchdir.md) |
| bindat (`bindat(2)` / `connectat(2)`) | FreeBSD | [DR-0008](DR-0008-freebsd-bindat-and-other-unix.md) |

### 全経路に共通の限界: ディレクトリを先に引く

- /proc・相対パス・bindat・スレッド cwd・fork の経路は、先にディレクトリを引いて (開く、または実パスにする) から、ファイル名で bind / connect する
- その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる (connect ならそこを見る)。呼んだ瞬間のパスの意味とずれうる
- 広げた sockaddr と symlink の経路は元のパスをそのままカーネルに解決させるので、この点は元のパスと同じに振る舞う

カーネルが返すアドレスが元のパスでないことは [DR-0013](DR-0013-kernel-returned-address.md)。

## 理由

- fork は副作用がある (サーバに見える peer の pid が終了した子の pid になる、子が一瞬全 fd の複製を持つ。[DR-0007](DR-0007-fork-child-fchdir.md))。避けられる経路があるなら、そちらで届かせる
- ふつうの環境 (`/tmp` が書ける) では symlink の経路までで届く ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md))。Linux と Apple の OS では、それも使えない時にスレッド cwd の経路が副作用無しで届く ([DR-0020](DR-0020-thread-cwd-route.md))。fork が要るのは、それらも使えない時だけになる

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 常に fork する (hyoui の方式をそのまま) | fork の副作用 (peer の pid、fd の複製) を、避けられる経路があるのに全員に負わせる |

## 未確定

- Android は compile もしていない。macOS 以外の Apple OS は iOS の clippy (試験コード込み) だけ確かめた
- NetBSD・OpenBSD・illumos の振る舞いは CI の VM の試験で確かめる ([DR-0019](DR-0019-ci-targets.md))。手元では NetBSD・illumos の clippy (試験コード込み) まで。OpenBSD は rustup に std が無く、手元では compile も確かめていない

## 判断の出所

- 経路と試す順番: kawaz 指示 2026-11-18
- スレッド cwd の経路を symlink の後、fork の前に足す: kawaz 裁定 (ハブ DR-0005、2026-11-18)
- 副作用の無い安い経路から試し、fork は最後: kawaz 裁定 2026-11-18
- 上に無い細部: 出所の記録なし
