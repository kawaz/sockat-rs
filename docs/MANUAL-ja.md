# sockat マニュアル

> [English](./MANUAL.md) | 日本語

sockat の使い方と、長いパスにどう届くか、何が残りどう片付けるか、長いパスで std と違う点のリファレンス。言語によらない振る舞いの正本は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) で、sockat-rs はその Rust 実装。Rust で決めた所は [DR-0032](./decisions/DR-0032-resolve-through-persistent-base.md) にある。

## 使い方

```toml
[dependencies]
sockat = "0.5"
```

std の関数の代わりに sockat の関数を使う。長さによらないパスを取り、std の型を返す。

| 関数 | 対応する std | 戻り値 |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listen 済み) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (無し) | `io::Result<UnixStream>` (非ブロッキング) |
| `sockat::datagram::bind(path)` | `UnixDatagram::bind(path)` | `io::Result<UnixDatagram>` |
| `sockat::datagram::unbound()` | `UnixDatagram::unbound()` | `io::Result<UnixDatagram>` |
| `sockat::datagram::connect(&sock, path)` | `sock.connect(path)` | `io::Result<()>` |

後始末と診断が要る時は、それぞれの `_ex` を使う ([後始末と診断つきの _ex](#後始末と診断つきの-_ex))。綴りを自分で std や他のライブラリに渡す時は resolve を使う ([resolve](#resolve))。

```rust
use std::io::{Read, Write};

let path = "/とても/深い/ディレクトリ/.../server.sock";
let listener = sockat::bind(path)?;
let mut client = sockat::connect(path)?;
let (mut server, _) = listener.accept()?;
client.write_all(b"ping")?;
let mut buf = [0; 4];
server.read_exact(&mut buf)?;
```

- std が受け付ける長さのパスは std の関数をそのまま呼ぶ。何も足さず、環境変数も読まない (`connect_nonblocking` は std に無いので、std と同じアドレスで自前のソケットを connect する)
- ソケットファイルは渡したパスにできる。listener を閉じても消えない (std と同じ)
- 自前で作るソケットは std と同じ設定 (close-on-exec、std が付ける OS (macOS / iOS、FreeBSD、NetBSD、DragonFly) では `SO_NOSIGPIPE`、std と同じ listen の backlog)
- fork はしない。どの経路も、プロセスの cwd も呼び出し元のスレッドの cwd も変えない

## connect と connect_nonblocking

- **`connect`** は std の `UnixStream::connect` の代わりに使う。ブロックする
- **`connect_nonblocking`** は async ランタイムで使う。ソケットを connect の前に非ブロッキングにし、そのまま返す。tokio の `UnixStream::from_std` にそのまま渡せる。相手の backlog が満杯なら待たずに失敗する (Linux は `EAGAIN`、macOS は `ECONNREFUSED`)。Unix domain の connect は呼び出しの中で終わるか失敗し、sockat の後片付けは unlink だけなので、待つものは無い

connect はプロセスの中にも置き場にも何も残さない。

## 長いパスへの届き方

std が受け付ける長さ (`sun_path` − 1 バイト) までのパスは std にそのまま渡す ([standard](#standard))。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS 等 Apple の OS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それより長いパスは、元のパスと同じ所を指す短い綴りでカーネルに渡す。カーネルは bind の綴りをソケットの寿命の間覚え、`getsockname` / `getpeername` で返す。上のライブラリはその綴りでソケットのファイルを消し、DGRAM の受け手はその綴りへ返信する。だから bind の綴りは、後で辿っても同じ所を指し続ける物にする。

### 置き場 B・D・E

| | 中身 | 作る単位 | 残る間 |
|---|---|---|---|
| B | 置き場の中のこの利用者のディレクトリ (mode 0711)。`/tmp/sockat-<uid>`、無理なら `$XDG_RUNTIME_DIR/sockat`、`$TMPDIR/sockat-<uid>`、最後にソケットのディレクトリの `.sockat.<乱数 10 文字>` | 利用者ごと。プロセスをまたいで使い回す | sockat は消さない |
| D | B の中の乱数 10 文字の名前のディレクトリ (mode 0711) | プロセスと B ごとに 1 つ | プロセスが共有ロック (`flock`) を掛けた fd を持ち続ける。死んだプロセスの D は OS の一時ディレクトリの掃除か [`sweep`](#sweep-と-sweep_base) で消える |
| E | D の中の symlink。ソケットのディレクトリを指す物 (link-dir) か、ソケットのファイルを指す物 (link-file) | bind と resolve ごと | `cleanup` / `remove` まで。既定の `bind` の E はプロセスが終わった後、D と一緒に消える |

- B の置き場は、realpath した実パスで確かめ・綴る (macOS の `/tmp` の B は `/private/tmp/sockat-<uid>`)
- B は、ルートから置き場までのディレクトリが全部「持ち主が root か自分で、group も other も書けないか、sticky」で、B 自身が「実ディレクトリで、持ち主が自分で、group も other も書けない」時だけ使う。既定の名前の B が外れたら `sockat-<uid>.<乱数 10 文字>` を作る。置き場に書けなければ次の置き場へ
- `$XDG_RUNTIME_DIR`・`$TMPDIR` が空か相対パスなら、その置き場は使わない
- 「自分」と名前の `<uid>` は実効 uid
- 1 つのプロセスが使う置き場 1〜3 の B は 1 つ (`usable_cross_uid` の時だけ、もう 1 つ使いうる)。ソケットのディレクトリの B はディレクトリごと
- 一度持った B は、プロセスの間それを使う。作れなかった置き場は、次の呼び出しで試し直す
- 持っている B・D が消されていたら (fd の `st_nlink` が 0、または名前で引いた物が fd と違う)、次の呼び出しで作り直す
- 綴りが収まらないと先に分かる時は、B を作らない。次の置き場にも移らず、B を通る経路を飛ばす (プロセスの B はパスの長さでは変わらない)
- D を作れなければ、その呼び出しでは D を使う経路を飛ばす。別の B は探さない
- ソケットのディレクトリの B は procB の綴りでだけ使う
- [`Options::base`](#options) を渡すと、B はその場所になる

B は `/proc/<pid>/fd/<B の fd>` (procB。`/proc` を使える時) か、B の実パスで綴る。procB の綴りは同じ uid・同じ PID 名前空間のプロセスだけが辿れ、プロセスが終われば辿れない。

### OS ごとに試す順番

最初に使えた経路で bind / connect する。経路が実際に bind / connect して失敗したら (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等)、そのエラーを返して次を試さない。

bind (stream・DGRAM):

| OS | 順 |
|---|---|
| Linux / Android | standard → link-dir (procB → B の実パス) → thread-cwd |
| macOS / iOS 等 Apple の OS | standard → link-dir → link-file → thread-cwd |
| FreeBSD | standard → link-dir → at |
| NetBSD | standard → link-dir → link-file |
| OpenBSD、illumos | standard → link-dir |
| その他の Unix | standard → link-dir → link-file (bind が symlink を辿るかを実行時に見分ける) |

connect (stream・DGRAM):

| OS | 順 |
|---|---|
| Linux / Android (`/proc` を使える) | standard → proc-direct → thread-cwd-direct → link-file |
| Linux / Android (`/proc` を使えない) | standard → relative → thread-cwd-direct → link-file |
| macOS / iOS 等 Apple の OS | standard → relative → widened → thread-cwd-direct → link-file |
| FreeBSD | standard → at-direct → link-file |
| NetBSD、OpenBSD、illumos、その他の Unix | standard → relative → link-file |

### 経路

#### standard

std にそのまま渡す。綴りは元のパス (絶対パスは `Original`、相対パスは `CwdRelative`)。

#### link-dir

D の中に、ソケットのディレクトリの実パスを指す E を作り、`B/D/E/<ファイル名>` で bind する。綴りで unlink すると途中の E を辿ってソケットのファイルが消える。

#### link-file

D の中に、ソケットのファイル (相対パスなら cwd を前に付けた物) を指す E を作り、`B/D/E` で bind する。綴りにファイル名が出ないので、ファイル名の長さによらず届く。bind が最後の要素の symlink を辿る OS (macOS、NetBSD) だけで使う。綴りで unlink すると E が消え、ソケットのファイルは残る。

connect の link-file は D を作らず、B の直下に乱数の名前の E を作って `B/E` で connect し、すぐ E を消す。

#### thread-cwd

綴りが収まらない時の bind (Linux / Android、Apple の OS)。sockat が起こしたスレッドだけが cwd を D に移し (Linux は `unshare(CLONE_FS)` と `fchdir`、Apple は `pthread_fchdir_np`)、D の中のすぐ消す E を通して `E/<ファイル名>` で bind する。綴りは片付け済みの E を含み、どこにも届かない (`Dangling`)。Linux で seccomp の filter が掛かっている時 (docker の既定の profile 等) は使わない (filter が `unshare` をプロセスの kill で断ることがあるため)。

#### at

FreeBSD の thread-cwd の代わり。スレッドを使わず `bindat(2)` に D の fd を渡す。

#### proc-direct

`/proc/self/fd/<ソケットのディレクトリの fd>/<ファイル名>` で connect する (Linux)。ファイル名はおよそ 90 バイトまで。

#### relative

cwd からの相対パスで connect する。パスの文字列を書き直すだけで cwd は変えない。cwd の綴りを前から削り、それで足りなければ実パスに揃えて `..` で上る綴りを作る。connect の後に cwd が変わっていたら、結果を捨てて `Other` のエラーを返す。

#### widened

Apple の OS で、元のパス (無理なら相対パス) を 253 バイトまで入る広げた sockaddr で connect する。bind には使わない (できたソケットのアドレスが `sockaddr_un` に入らず、読む側を壊す)。

#### at-direct

FreeBSD で `connectat(2)` にソケットのディレクトリの fd とファイル名を渡す。

#### thread-cwd-direct

sockat が起こしたスレッドだけが cwd をソケットのディレクトリに移し、ファイル名で connect する (Linux / Android、Apple の OS)。D も E も作らない。

### どこまで届くか

綴りに入るファイル名の長さのおよその上限 (B が `/tmp` にある時)。

| 経路 | Linux | macOS |
|---|---|---|
| link-dir (procB) | 74〜77 | (無し) |
| link-dir (B の実パス) | 77 (`/tmp/sockat-1000/`) | 66 (`/private/tmp/sockat-501/`) |
| link-file | (無し) | 長さによらない |
| thread-cwd | 96 | 92 |
| connect | 長さによらない (link-file) | 長さによらない (link-file。253 バイトまでは widened) |

Linux の bind は、ファイル名がおよそ 96 バイトを超えるか、seccomp の filter の下でおよそ 77 バイトを超えると `ENAMETOOLONG` になる。OpenBSD・illumos の bind は link-dir だけ。パス全体は `PATH_MAX` (macOS 1024、Linux 4096) まで。

### NUL を含むパス

先頭以外に NUL を含むパスは、長さによらず何も作らずに `InvalidInput` を返す (カーネルは NUL の手前までしか読まず、別の場所にソケットができるため)。Linux / Android で先頭が NUL のパス (abstract namespace) は長さによらず std にそのまま渡す (std は `InvalidInput` を返す。abstract の名前は std の `SocketAddr::from_abstract_name` と `bind_addr` / `connect_addr` で扱う)。

## 綴りの性質 Spelling

`bind_ex` と `resolve_for_bind` は、bind の綴りを後で使えるかを `Spelling` で返す。分岐はこれで行う。

| 値 | 綴り | 経路 |
|---|---|---|
| `Original` | 元の絶対パス | standard |
| `CwdRelative` | 元の相対パス。cwd を変えると別の所を指す (std が元から持つ危険だが、知らせる) | standard |
| `Linked { .. }` | B・D・E を通り、元のパスと同じ所を指す | link-dir、link-file |
| `Dangling` | 片付け済みの E を含む。どこにも届かない | thread-cwd、at |

`Linked` の属性:

| 属性 | 中身 |
|---|---|
| `removable` | 綴りで unlink すると実体が消えるか。link-dir は true、link-file は false (E が消える) |
| `until` | `Until::Process` (procB: プロセスが終われば辿れない) か `Until::Cleanup` (B の実パス: E を片付けるまで) |
| `usable_cross_uid` | 別の uid のプロセスが辿れるか。procB は false。B の実パスは、途中と B 自身が全部 other に x を持てば true (`/tmp` の B) |
| `usable_with_private_tmp` | 自分か相手が `/tmp` を隔離していても (systemd の `PrivateTmp` 等) 相手が辿れるか。procB は true。B の実パスは、B が `/tmp`・`/var/tmp` の下なら false |

## Options

`_ex` と resolve は `sockat::Options` を取る。`Options::default()` は既定の関数と同じ。

| 項目 | 中身 |
|---|---|
| `usable_cross_uid: bool` | bind の綴りを別の uid も辿れる物にする。途中と B 自身が other に x を持つ置き場の B だけを使い、procB を使わない。満たせなければ thread-cwd / at (`Dangling`) に進む。B が他の uid に辿らせるかは、プロセスがその B を持った時に判定して覚え、後で見直さない (`Options::base` の B も)。connect 系は見ない |
| `base: Option<PathBuf>` | B そのものの場所 (例 `/tmp/my-app-1000/sockat`)。無ければ作り (mode 0711)、確かめに外れたら `ENOENT` / `ENOTDIR` / `EPERM` で、自動の置き場に落とさない。相対パスと、名前で終わらないパスは、パスの長さによらず `InvalidInput`。base の場所が symlink なら `EPERM`。確かめは長いパスで呼び出しのたびに経路を試す前に行い、B を作るのは B を使う経路に来た時だけ。bind 系も connect 系も使う。この B は sockat 専用で、他の物を置かない (`sweep_base` は D の形の名前のディレクトリだけを見る) |

```rust
let options = sockat::Options {
    base: Some("/tmp/my-app-1000/sockat".into()),
    ..Default::default()
};
let bound = sockat::bind_ex(path, options)?;
```

## 後始末と診断つきの _ex

| 関数 | 戻り値 |
|---|---|
| `sockat::bind_ex(path, options)` | `Result<Bound<UnixListener>, ExError>` |
| `sockat::connect_ex(path, options)` | `Result<Connected<UnixStream>, ExError>` |
| `sockat::connect_nonblocking_ex(path, options)` | 同上 (非ブロッキング) |
| `sockat::datagram::bind_ex(path, options)` | `Result<Bound<UnixDatagram>, ExError>` |
| `sockat::datagram::connect_ex(&sock, path, options)` | `Result<Report, ExError>` |

| 型 | 中身 |
|---|---|
| `Bound<S>` | `socket` (std の型)、`file: SocketFile`、`spelling: Spelling`、`route`、`base`、`skipped` |
| `Connected<S>` | `socket`、`route`、`skipped` |
| `Report` | `route`、`skipped` |
| `ExError` | `error: io::Error`、`route` (実際に bind / connect して失敗した経路。無ければ `None`)、`skipped`。`?` で `io::Error` に戻る |

`bind_ex` の listener は、短いパスでも閉じた時にソケットのファイルを消さない。消すのは `SocketFile::remove` だけ。

### 診断

| 項目 | 中身 |
|---|---|
| `route` | 通った経路の名前 |
| `base` | bind の綴りが通る B (`proc`・`tmp`・`runtime`・`tmpdir`・`sockdir`・`given`)。`given` は `Options::base` の B。B を通らない綴り (standard・thread-cwd・at) と connect では無い |
| `skipped` | 試して使えなかった経路 (`Skipped { name, reason }`) を試した順に、経路の名前ごとに 1 件。長いパスでは `standard` が先頭 |

`route`・`base`・`skipped` の名前と `reason` は診断用で、中身を約束しない (経路は増えたり変わったりする)。分岐は `Spelling` で行う。

## 片付け

### SocketFile

`bind_ex` は、bind したソケットのファイルの実パスと `(st_dev, st_ino)` を bind の直後に覚えた `SocketFile` を返す。

| メソッド | すること |
|---|---|
| `remove(&self)` | ソケットのファイルを 1 回だけ消し、`cleanup` もする。そのパスの `(st_dev, st_ino)` が覚えた物と同じ時だけ消す。置き換わっていた・既に無い時は消さずに `Ok(())`。失敗しても `cleanup` まで行い、ファイルの側のエラーを先に、無ければ `cleanup` のエラーを返す |
| `cleanup(&self)` | 綴りが通る E を 1 回だけ消す。綴りはどこにも届かなくなり、ソケットのファイルとソケットは残る。E を消せなければ (既に無い時を除く) `unlink` のエラーを返す |

- どちらも成功した後の呼び出しは何もしない。失敗した後の呼び出しはやり直す。複数のスレッドから同時に呼んでも、消す操作は同時に 1 つ (`ForBind` と、その `socket_file()` を何度か呼んで得た `SocketFile` どうしの間でも)。Drop では何もしない
- パスの最後の要素が symlink の時 (macOS・NetBSD の bind はそれを辿り、その先にソケットを作る)、覚えるのは辿った先のソケット。`remove` はそのソケットを消し、利用者の symlink は残す
- `remove` は listener を閉じない。閉じるのは利用者
- 約束は listener を開いている間の置き換えまで。閉じた後に消して作り直された物は、番号をすぐ使い回すファイルシステム (ext4、BSD の UFS / FFS) で見分けられない。`remove` は listener を閉じる前に呼ぶ
- `bind_ex` がソケットのファイルを覚えられない時は、listener を閉じ、E を片付け、ソケットのファイルは消さずにエラーを返す (確かめられないファイルを消すと、置き換わった物を消しうる)。bind した直後にそこ (symlink なら辿った先) にあるのがソケットでなければ、置き換わった物なので覚えず `ENOTSOCK`
- 呼び出しの中で消す E (bind が失敗した時・覚えられない時の E、thread-cwd・at の E、connect の後の E) は、消すのに失敗しても黙る。呼び出しの成否も元のエラーも変わらない。利用者が呼ぶ `cleanup` と `remove` だけが消す失敗を返す
- 相対パスの `bind_ex` は、パスを引く前と、bind と覚えるための realpath の後で cwd の `(st_dev, st_ino)` を比べ、違えば覚えられない時と同じに `Other` のエラーを返す
- 確かめてから消すまでの間に、ソケットのディレクトリに書ける者がファイルをすり替えるのは防げない (inode を指定して消す手段が POSIX に無い)

```rust
let bound = sockat::bind_ex(path, Default::default())?;
// ... bound.socket で accept する (tokio に渡してもよい) ...
bound.file.remove()?; // listener を閉じる前に
drop(bound.socket);
```

### 置き場に残る物

- 既定の `bind` は `SocketFile` を返さないので、その E はプロセスが終わるまで残り、D と一緒に OS の一時ディレクトリの掃除か `sweep` で消える。片付けを気にするなら `bind_ex` か resolve を使う。長いパスに何度も bind する長命のプロセス (セッションごとにソケットを作る daemon 等) は、既定の `bind` だと E がプロセスの終わりまで溜まるので、`bind_ex` の `remove()`・`cleanup()` で 1 つずつ片付ける
- connect は何も残さない (B を初めて作った時は、その B をプロセスの B として使い続ける)
- B は sockat が消さない。ソケットのディレクトリの B (`.sockat.<乱数>`) と、確かめに外れて作った B (`sockat-<uid>.<乱数>`) は `sweep` の対象外で、OS の掃除でも消えないことがある。ソケットのディレクトリの B は利用者のディレクトリに残る

OS の一時ディレクトリの掃除:

| 置き場 | 死んだ D | 生きている D |
|---|---|---|
| Linux の `/tmp` | systemd-tmpfiles が 10 日で消す | flock の掛かったディレクトリは飛ばされる (systemd v242 以上)。v242 より古い環境 (RHEL 8 等) では 10 日で消されうる |
| `$XDG_RUNTIME_DIR` | 最終ログアウトと再起動まで残る | 同左 |
| macOS の `/tmp` | symlink の入ったディレクトリは消えないので、再起動まで残る | 消えない |
| macOS の `$TMPDIR` | dirhelper が 3 日で消しうる | 同左 |
| FreeBSD・NetBSD・OpenBSD・illumos | 既定は消えないか、再起動で消える | 同左 |

生きている D が消された時 (手で消した時を含む) は、綴りが辿れなくなるだけで危険はなく、元のパスのソケットは働く。exec をまたぐと D のロックの fd が閉じ、引き継いだ listener が生きていても D と E は `sweep` や OS の掃除で消されうる。procB の綴りは exec の時点で辿れなくなる。

### sweep と sweep_base

- `sockat::sweep()` は、置き場 1〜3 の既定の名前の B (`/tmp/sockat-<uid>`、`$XDG_RUNTIME_DIR/sockat`、`$TMPDIR/sockat-<uid>`) のうち、あるものの中で、持ち主のプロセスが死んだ D (`flock(LOCK_EX | LOCK_NB)` が取れた D) を中の E ごと消す。B は作らない。確かめに外れた置き場・B は黙って飛ばす
- `sockat::sweep_base(path)` は、`Options::base` に渡した B に同じことをする。その B を管理するアプリが呼ぶ。B か親が無ければ何もせず成功する。形の誤りは `InvalidInput`、symlink は `EPERM`、ディレクトリでなければ `ENOTDIR`、他人の物・group / other が書ける B・途中が他人に書ける時は `EPERM`
- 見るのは B の直下の、名前が D の形 (ASCII の英数字 10 文字) のディレクトリだけ。B は sockat 専用で、他の名前の物には触らない
- 生きているプロセスの D (このプロセスの物を含む) は消さない。B の直下の E と、symlink でない物がある D には触らない
- 自動では呼ばれない (resolve や bind のたびにも、プロセスの初めにも、終了時にも B の中を一覧しない)。死んだ D を消すのは、呼んだ時の `sweep` と OS の掃除だけ
- 試験を何度も回す環境 (CI、繰り返しの検証) では、`sweep()` (`Options::base` の B は `sweep_base`) を 1 回呼べば死んだ D が溜まらない

## resolve

綴りを自分で std や他のライブラリに渡す人向け。綴りはただのパスなので、ソケットの型と操作を問わず使える。

| 関数 | 綴り | 戻り値 |
|---|---|---|
| `sockat::resolve_for_bind(path, options)` | link-dir の `B/D/E/<ファイル名>`、macOS と NetBSD では link-file の `B/D/E` も | `ForBind` (`path`、`spelling`、`route`、`base`、`skipped`、`cleanup()`、`socket_file()`) |
| `sockat::resolve_for_connect(path, options)` | link-file の `B/D/E` (ファイル名の長さによらない) | `ForConnect` (`path`、`route`、`skipped`、`cleanup()`) |

- std が受け付ける長さのパスはそのまま返す (`Original` / `CwdRelative`)
- 綴りが収まらない時 (thread-cwd・at でしか届かない長さ、B を作れない) は `ENAMETOOLONG`。その長さに届かせたい時は `bind` / `connect` を使う
- `ForBind::socket_file()` は、bind した後に呼ぶ。その時点で綴りを辿り、ソケットのファイルの実パスと `(st_dev, st_ino)` を覚えた `SocketFile` を返す (bind の前や片付けた後はエラー、ソケットでない物なら `ENOTSOCK`)。パスの最後の要素が symlink なら (macOS・NetBSD の bind が辿る)、辿った先のソケットを覚える。その `remove` はファイルを消して、`ForBind` と同じ E を片付ける。呼ばなければ消す口は無い
- `ForConnect` の綴りは何度でも connect に使え、最後に `cleanup()` する
- `resolve_for_connect` は `usable_cross_uid` を見ない

```rust
let spelled = sockat::resolve_for_bind(path, Default::default())?;
let listener = std::os::unix::net::UnixListener::bind(&spelled.path)?;
let file = spelled.socket_file()?;
// ...
file.remove()?;
drop(listener);

let to = sockat::resolve_for_connect(path, Default::default())?;
for _ in 0..3 {
    let stream = std::os::unix::net::UnixStream::connect(&to.path)?;
    // ...
}
to.cleanup()?;
```

## datagram

`sockat::datagram` は std の `UnixDatagram` の、パスを取る関数と `unbound` を写す。経路と順は stream と同じ。長いパスに bind した送り手には、綴りが辿れる間は受け手が `recv_from` のアドレスへ返信できる (`Spelling::Linked`)。OpenBSD では Rust 1.100 以上が要る ([長いパスで std と違う点](#長いパスで-std-と違う点) の「カーネルが返すアドレス」)。

```rust
let server = sockat::datagram::bind(server_path)?;
let client = sockat::datagram::unbound()?;
sockat::datagram::connect(&client, server_path)?;
client.send(b"ping")?;
```

`send_to` と SEQPACKET はまだ無い ([仕様の未確定](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#未確定))。長いパスへ `send_to` したい時は、`resolve_for_connect` の綴りを std の `send_to` に渡す。

## 長いパスで std と違う点

| 点 | 内容 |
|---|---|
| カーネルが返すアドレス | listener と accept したストリームの `local_addr()`、connect した側の `peer_addr()` は、元のパスでなく bind の綴りを返す。後で使えるかは `Spelling` で分かる。illumos では connect した側の `peer_addr()` が、connect に渡した綴りを返す。<br>OpenBSD では、Rust 1.99 以前の std の `local_addr()`・`peer_addr()`・`recv_from()` がパスの最後の 1 バイトを落とす (std の不具合で、sockat に関係なく短いパスでも同じ。[rust-lang/rust#118925](https://github.com/rust-lang/rust/issues/118925)、Rust 1.100.0 で修正)。カーネルが返すアドレスは欠けていない。std の `recv_from` のアドレスへ DGRAM を返信するには Rust 1.100 以上が要る |
| 運用の道具での見え方 | `ss -x`・`lsof`・`/proc/net/unix` に出るアドレスも bind の綴り。link-dir と thread-cwd・at はファイル名が最後に残り、link-file の綴りには出ない |
| listener を閉じた時 | std と同じく、閉じてもソケットのファイルは消えない |
| ディレクトリを先に引く | link-dir・thread-cwd・at の E はソケットのディレクトリの実パスを指し、proc-direct・at-direct・thread-cwd-direct はディレクトリを開いてからファイル名で渡す。その間の rename や symlink の張り替えは反映されない |
| 他のスレッドの chdir | relative は、間に cwd が変わったままなら `Other` のエラー。変えて戻した場合は見えない |
| 状態がアプリの置き場の外に増える | B・D・E が `/tmp`・`$XDG_RUNTIME_DIR`・`$TMPDIR` に残る ([置き場に残る物](#置き場に残る物)) |
| 生きている D が消されうる環境 | systemd v242 より古い Linux と、B が macOS の `$TMPDIR` にある時。綴りが辿れなくなるだけ |
| `PrivateTmp` の相手 | 相手の `/tmp` が隔離されていると、`/tmp` を通る綴りを相手は辿れない。Linux では procB を先に使うので、まず起きない。`usable_with_private_tmp` で分かる |
| 権限 | ソケットのディレクトリは、macOS では `O_SEARCH`、Linux では `O_PATH` で開くので、普通の bind / connect と同じ権限で足りる。他の Unix は `O_RDONLY` で開き、proc-direct・at-direct・thread-cwd-direct に読み取り権限も要る |

## 使ってはいけない書き方

sockat を使う時に、無関係なファイルを消したり、ファイルを消し損ねたりする書き方 ([sockat の仕様の「使ってはいけない書き方」](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#使ってはいけない書き方))。

| 書き方 | Rust での形 | なぜいけないか | 代わりに |
|---|---|---|---|
| `Spelling` を見ずに、カーネルが返す綴りでソケットのファイルを消す・`stat` する | `std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())` | link-file の綴りでは E が消えるだけでファイルは残る。`Dangling` の綴りはどこにも届かない | 元のパスで `std::fs::remove_file(path)`<br>`SocketFile::remove()` |
| 片付けた後やプロセスが終わった後に、古い綴りを使う | `cleanup()` の後に綴りで connect する | 片付けた後の `/tmp` の B は、他人が同じ名前で作り直しうる。procB の綴りはプロセスが終われば辿れない | 綴りは `until` の間だけ使う |
| 相対パスで bind し、cwd を変えた後にその相対パスで消す | `sockat::bind("app.sock")` の後の<br>`std::env::set_current_dir(..)` と<br>`std::fs::remove_file("app.sock")` | 別の場所のファイルに当たる (`CwdRelative`) | bind の前に `std::path::absolute` で絶対パスにする<br>`SocketFile::remove()` |
| listener を閉じればソケットのファイルが消えると当てにする | `drop(listener)` だけ | 消えない | `SocketFile::remove()`<br>元のパスで消す |
| `remove()` で listener も閉じると当てにする | `bound.file.remove()` だけ | `remove()` はファイルを消すだけ | listener は自分で閉じる |
| listener を閉じた後に `remove()` を呼ぶ | `drop(bound.socket); bound.file.remove()` | 閉じた後に作り直された別のファイルを見分けられないことがある | 閉じる前に呼ぶ |
| procB や `usable_cross_uid` が false の綴りを別の uid に辿らせる | | 辿れない | `Options { usable_cross_uid: true, .. }` を渡し、`Spelling` を確かめる |
| `route`・`base`・`skipped`・`reason` の文字列で分岐する | `if bound.route == "link-file"` | 診断用で、中身を約束しない | `Spelling` で分岐する |

避け方 (crate の doc の試験で動かしている):

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let path = std::path::absolute(path)?; // bind の前に絶対パスに
    let bound = sockat::bind_ex(&path, Default::default())?;
    if let sockat::Spelling::Linked { removable: false, .. } = bound.spelling {
        // local_addr() の綴りで消すと E が消えるだけ。元のパスか remove() で消す
    }
    // ... bound.socket で accept する ...
    bound.file.remove()?; // listener を閉じる前に
    drop(bound.socket);
    Ok(())
}
```

## エラー

| 状況 | エラー |
|---|---|
| ディレクトリが無い | `ENOENT` |
| ディレクトリの所にディレクトリでない物がある | `ENOTDIR` |
| 長いパスで、ソケットのディレクトリの実パスを引けない | そのエラー |
| connect 先が無い | `ENOENT` |
| bind 先に既にファイルがある | `EADDRINUSE` |
| 誰も listen していないソケットファイル | `ECONNREFUSED` |
| backlog が満杯 | Linux はブロックしない connect で `EAGAIN` (ブロックする connect は空くまで待つ)、macOS は `ECONNREFUSED` |
| どの経路も使えない (綴りが収まらない、手段が無い) | `ENAMETOOLONG` |
| resolve で綴りが収まらない | `ENAMETOOLONG` |
| `Options::base` の途中のディレクトリが無い | `ENOENT` |
| `Options::base` の場所がディレクトリでも symlink でもない | `ENOTDIR` |
| `Options::base` の場所が symlink (実ディレクトリでない) | `EPERM` |
| `Options::base` の持ち主・権限の確かめに外れた (`usable_cross_uid` の bind で、途中と base 自身が other に x を持たないことを含む) | `EPERM` |
| `Options::base` が相対パスか、名前で終わらない (パスの長さによらず、abstract の名前の判定より前) | `InvalidInput` |
| `Options::base` の確かめに通った後、B を作れない | 作れなかったエラー |
| std にそのまま渡せない長さのパスで、名前で終わらない (`dir/`、`dir/.`、`..` 等) | `InvalidInput` |
| NUL を含むパス (長さによらない) | `InvalidInput` |
| relative の途中、または相対パスの `bind_ex` の途中で cwd が変わった | `Other` |
| `bind_ex` が bind した直後、そこ (最後の要素が symlink なら辿った先) にソケットでない物がある。`socket_file()` が辿った先がソケットでない | `ENOTSOCK` |
| `cleanup` / `remove` で E を消せない (既に無い時を除く) | `unlink` のエラー |

- ディレクトリを先に引くので、`ENOENT` と `ENOTDIR` は長さの判定より前に決まり、経路は試されない (`ExError` の `route` は `None`、`skipped` は空)
- `Options::base` の確かめは経路の前に行い、外れたら `route` は `None`、`skipped` は空。確かめに通った後に B を作れなかった時は、`route` は `None`、`skipped` はそれまでに飛ばした経路
- `_ex` は、実際に bind / connect して失敗した経路を `ExError::route` に入れる
- macOS では backlog が満杯の時も `ECONNREFUSED` を返すので、`ECONNREFUSED` だけでは「誰も listen していないソケットファイル」と判定できない (std の connect でも同じ)

## async ランタイム

戻り値は std の型なので、std のソケットを受け取れるランタイムならどれでも使える。tokio の例:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` は相手を待たず、後片付けも unlink だけなので、runtime の worker から直接呼べる。`bind` も相手を待たない (thread-cwd ではスレッドの終わりを待つが、bind してすぐ終わる)。`bind_ex` の listener も同じく渡せて、`SocketFile` は手元に残る。

## 確かめていないこと

- Android は compile もしていない。Linux と同じ経路を使う。Android のアプリは seccomp の filter の下で動くので、スレッド cwd は使われない見込み
- NetBSD・OpenBSD・illumos・FreeBSD は CI の VM で試験を回す。peer の資格情報は確かめていない
- ブロックしない connect が backlog の満杯で返すエラーと、connect が呼び出しの中で終わることは、Linux と macOS でだけ確かめた
- macOS 以外の Apple の OS (iOS 等) は macOS と同じ経路を使うが、確かめたのは iOS の clippy だけ。`pthread_fchdir_np` があるか、bind が symlink を辿るかも確かめていない
- FreeBSD の connect で link-file が使えるか (connect は最後の要素の symlink を辿る前提)
