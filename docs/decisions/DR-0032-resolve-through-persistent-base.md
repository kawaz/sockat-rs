# DR-0032: ハブ DR-0017 を Rust で作る形 (公開 API、B・D・E の持ち方、OS ごとの試す順)

- Status: Accepted
- Date: 2026-11-21

## Context

- ハブの [DR-0017](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0017-resolve-through-persistent-base.md) と [SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md)・[SPEC-routes](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md) が、長いパスを置き場 B・プロセスの D・symlink E を通る短い綴りで bind / connect すること、経路と試す順、綴りの性質 `Spelling`、関数の形 (F1)、`Options`、fork をしないことを決めた
- 言語によらない振る舞いはハブが正本で、この DR は Rust で決める所 (型、関数のシグネチャ、プロセスの中の状態の持ち方、`unsafe` の境界、試験の口) を記録する

## Decision

### 公開 API

```rust
// stream (crate 直下)
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener>;
pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream>;
pub fn connect_nonblocking<P: AsRef<Path>>(path: P) -> io::Result<UnixStream>;
pub fn bind_ex<P: AsRef<Path>>(path: P, options: Options) -> Result<Bound<UnixListener>, ExError>;
pub fn connect_ex<P: AsRef<Path>>(path: P, options: Options) -> Result<Connected<UnixStream>, ExError>;
pub fn connect_nonblocking_ex<P: AsRef<Path>>(path: P, options: Options) -> Result<Connected<UnixStream>, ExError>;

// datagram
pub mod datagram {
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixDatagram>;
    pub fn unbound() -> io::Result<UnixDatagram>;
    pub fn connect<P: AsRef<Path>>(socket: &UnixDatagram, path: P) -> io::Result<()>;
    pub fn bind_ex<P: AsRef<Path>>(path: P, options: Options) -> Result<Bound<UnixDatagram>, ExError>;
    pub fn connect_ex<P: AsRef<Path>>(socket: &UnixDatagram, path: P, options: Options) -> Result<Report, ExError>;
}

// resolve と置き場
pub fn resolve_for_bind<P: AsRef<Path>>(path: P, options: Options) -> Result<ForBind, ExError>;
pub fn resolve_for_connect<P: AsRef<Path>>(path: P, options: Options) -> Result<ForConnect, ExError>;
pub fn sweep() -> io::Result<()>;
pub fn sweep_base<P: AsRef<Path>>(base: P) -> io::Result<()>;
```

| 型 | 形 |
|---|---|
| `Options` | `pub usable_cross_uid: bool`、`pub base: Option<PathBuf>`。`Default` を持ち、`#[non_exhaustive]` にしない (`Options { base: Some(p), ..Default::default() }` と書けるように) |
| `Spelling` | `#[non_exhaustive]` の enum。`Original`、`CwdRelative`、`Linked { removable, until: Until, usable_cross_uid, usable_with_private_tmp }`、`Dangling` |
| `Until` | `Process`、`Cleanup` |
| `Bound<S>` | `socket`、`file: SocketFile`、`spelling`、`route: &'static str`、`base: Option<&'static str>`、`skipped: Vec<Skipped>` |
| `Connected<S>` | `socket`、`route`、`skipped` |
| `Report` | `route`、`skipped` |
| `ForBind` | `path`、`spelling`、`route`、`base`、`skipped` と、`cleanup(&self)`・`socket_file(&self) -> io::Result<SocketFile>` |
| `ForConnect` | `path`、`route`、`skipped` と `cleanup(&self)` |
| `SocketFile` | `remove(&self)`・`cleanup(&self)`。Drop では何もしない |
| `ExError` | `error: io::Error`、`route: Option<&'static str>`、`skipped`。`From<ExError> for io::Error`、`Display`・`source` は `io::Error` の物 |
| `Skipped` | `name`、`reason` (どちらも `&'static str`) |

- 返す構造体は `#[non_exhaustive]` で、フィールドを公開する (分解して受け取れる)
- `remove` と `cleanup` は `&self` で、内側の印で 1 回だけ働く。1 つの綴りの持ち主 (`ForBind` か `ForConnect`、`bind_ex` の `SocketFile`) と、`ForBind` の `socket_file()` が返す全ての `SocketFile` は、E と 1 つの lock を `Arc<Clean>` で共有し、ファイルと E を消す操作はどれもその lock を持って行う。複数のスレッドから、別々の `SocketFile` や `ForBind` を通して同時に呼ばれても、消す操作は同時に 1 つ (lock が `SocketFile` ごとだと、2 つが同時にファイルを bind した物と確かめて両方が unlink し、後の方がその間に bind された別のソケットを消しうる)。片付けはどこから呼んでも 1 回。E の印 (`base::Link`) も自分の lock を持ち、経路の中の片付けと合わせて E の unlink を同時に 1 つにする
- 利用者が呼ぶ `cleanup` は E の unlink の失敗 (`ENOENT` 以外) をエラーで返す。印を立てるのは消せた時 (と既に無い時) だけで、失敗した後の呼び出しはもう一度消しに行く
- `remove` は失敗しても `cleanup` まで行い、ファイルの側のエラーを先に、無ければ `cleanup` のエラーを返す。印を立てるのは両方が通った時だけで、失敗した後の呼び出しはもう一度確かめて消しに行く
- 呼び出しの中で消す E (bind が失敗した時・ソケットのファイルを覚えられなかった時の E、thread-cwd・at の E、既定の connect の E) は、消すのに失敗しても捨てる (`let _`、`TempLink` の Drop)。呼び出しの成否も元のエラーも変えない
- bind の後に覚えるソケットのファイルは、どの経路も `route::remembered_at` で引く: パスを `realpath` し (最後の要素の symlink も辿る。Apple・NetBSD の bind はそこを辿ってその先にソケットを作る)、その `lstat` を覚える。ソケットでなければ `ENOTSOCK` で覚えない。standard は元のパス、link-dir・link-file・thread-cwd・at はソケットのディレクトリの実パスとファイル名を引き、link-dir は加えて綴りの `stat` が同じ物かを確かめる。`ForBind::socket_file()` は綴りを同じ関数で引く
- 作らないもの: `datagram::send_to`・`send_to_ex` と `seqpacket` (ハブ SPEC の「未確定」)

### B・D・E の持ち方 (`src/base.rs`)

- プロセスの中の登録簿 (`Mutex<Vec<Arc<Base>>>`) が、このプロセスの B を持つ。B は fd で掴み、プロセスが終わるまで閉じない (close-on-exec)。使う前に、fd の `fstat` の `st_nlink` が 0 でなく、B の綴りの `lstat` が持っている `(st_dev, st_ino)` と同じかを確かめ、外れたら外して次に作り直す。D も同じ。消したディレクトリの `st_nlink` は Linux では 0 になるが macOS の APFS では 2 のままなので、名前の確かめは省かない
- 置き場 1〜3 の B は `Held` が、普段の B と `usable_cross_uid` の B の 2 つの枠で持つ (普段の B が他の uid に辿らせる時は同じ B)。一度持ったらプロセスの間それを使い、作れなかった置き場は覚えず次の呼び出しで試し直す。外では `route::Config` がプロセスの 1 つの `Held` を運ぶ
- 置き場の順に、何も作らずに分かること (置き場が無い、途中の確かめに外れる、`usable_cross_uid` で途中が other に x を持たない) で外れる置き場を飛ばし、最初に残った置き場で綴りの長さを見積もる。収まらなければ B を作らず、次の置き場にも移らない
- 他の uid に辿らせるか (other の x) は、B を持った時に判定して `Base` に覚え、後で見直さない。`Options.base` の安全の確かめ (途中、持ち主、他人が書けない) は呼び出しのたびに行い、other の x だけは持っている B の判定を使う
- D を作れなかった B は、その呼び出しの間 (`route::Target`) 覚え、D を使う経路で作り直さない。mode をそろえられない・開けない時は、作った空の D を消す
- ソケットのディレクトリの B は procB の綴りでだけ使う (`route::forms` が実パスの綴りを出さない)
- D は B ごとに 1 つ、`O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` の fd に `flock(LOCK_SH | LOCK_NB)` を掛けて持ち続ける。掛けた後に名前で引き直して同じ D かを確かめ、違えば作り直す
- 作った B と D は、作った直後に名前で `fchmodat(0711)` してから開いて確かめる
- resolve の E は D の中のカウンタ (36 進)、thread-cwd・at の E と B 直下の E は乱数 10 文字 (`arc4random_buf` / `getrandom`)
- procB の綴りは `/proc/<getpid()>/fd/<B の fd>`。その stat が B の fd と同じ `(st_dev, st_ino)` の時だけ使う
- `sweep`・`sweep_base` が見るのは、B の直下の、名前が D の形 (`base::is_random_name`: ASCII の英数字 10 文字) のディレクトリだけ。他の名前の物には触らない

### OS ごとの試す順

| OS | bind | connect |
|---|---|---|
| Linux / Android | standard、link-dir (procB → 実パス)、thread-cwd | standard、proc-direct (`/proc` を使えなければ relative)、thread-cwd-direct、link-file |
| Apple の OS | standard、link-dir、link-file、thread-cwd | standard、relative、widened、thread-cwd-direct、link-file |
| FreeBSD | standard、link-dir、at | standard、at-direct、link-file |
| NetBSD | standard、link-dir、link-file | standard、relative、link-file |
| OpenBSD、illumos | standard、link-dir | standard、relative、link-file |
| その他の Unix | standard、link-dir、link-file (実行時に見分ける) | standard、relative、link-file |

- link-dir は、`/proc` を使える所ではどの OS でも procB を先に試す (Linux 以外では `/proc/<pid>/fd` の確かめに通らず実パスだけになる)
- bind が symlink を辿るかは `cfg` で決める (Apple・NetBSD は辿る、Linux / Android・FreeBSD・OpenBSD・illumos は辿らない、他は実行時に見分ける)

### 試験の口

- 環境を読む部分は `route::Config` (置き場、持っている置き場 1〜3 の B (`Held`)、`/proc` の場所、thread-cwd、widened、bind が symlink を辿るか、ソケットのディレクトリの開き方、cwd の引き直しの間) にして、試験が差し替える。試験は `Held` を試験ごとに持ち、同じプロセスで並ぶ試験の B が重ならない
- D を整えられない場面は、試験だけの thread_local (`base::FAIL_D`) で、作った D を開くところを失敗させて作る (本物の失敗をそこで起こす手が無い)
- E を消せない場面は、試験だけの thread_local (`base::FAIL_UNLINK_E`) で、そのスレッドの E の unlink を `EPERM` にして作る。bind の直後にソケットのファイルが置き換わる場面は、試験だけの thread_local (`route::AFTER_BIND`) で、bind が通ってから覚えるまでの間に置き換えを入れて作る (どちらも呼び出しの途中に他のプロセスが割り込む形で、試験から外の手で起こせない)。E とソケットのファイルを消した回数は試験だけのフィールド (`Link::unlinks`・`Clean::file_unlinks`) で数え、並行の呼び出しで消す操作が 1 回だけかを確かめる
- プロセス全体に効く設定 (cwd、umask、実効 uid) と、終わったプロセスの D が要る試験は、試験のバイナリを子プロセスで起こす
- mode で止まる利用者として確かめる試験 (`Options.base` を作れない場面を 0500 のディレクトリで作る) は、root で走る時、子プロセスが setgroups・setgid・setuid で root を手放してから行う (`sys::drop_root`)
- カーネルが覚える綴りは、試験では std の `SocketAddr` を通さず、`getsockname` / `recvfrom` が書いたアドレスを NUL まで読む (`sys::local_address`・`sys::recv_from`)。std の読み方は Rust の版で違う (Rust 1.99 以前は OpenBSD でパスの最後の 1 バイトを落とす、rust-lang/rust#118925)
- `unsafe` は `src/sys.rs` だけ (`just lint-unsafe`)

理由:

- `Options` を `#[non_exhaustive]` にすると crate の外で構造体式も更新構文も書けず、setter が要る。0.x の間は項目を足すのを破壊的変更として受け入れ、Rust で普通の書き方を選ぶ
- `remove` を `self` を消費する形にすると、`ForBind` と `SocketFile` で E を共有できず、SPEC の「2 回目以降は何もしない」を型でなく所有で表すことになる
- 失敗した `cleanup` の後の呼び出しをもう一度消しに行かせるのは、印を先に立てると 2 回目が E を残したまま成功を返し、利用者に頼まれた片付けの失敗を隠すため
- ソケットでない物を覚えないのを、SPEC が求める最後の要素が symlink の時に限らずどの場合にも行うのは、`bind_ex` が bind した直後の名前にソケットでない物があれば置き換わった物で、覚えると `remove` がそれを消すため (`ForBind::socket_file()` と同じ確かめ)
- B・D はプロセスごとの状態 (ハブ DR-0017) なので、プロセス全体の登録簿に置く。試験は置き場を試験ごとの一時ディレクトリにするので、登録簿を共有しても B は重ならない
- 置き場 1〜3 の B は置き場によらずプロセスに 1 つなので、置き場で引く登録簿では表せない。プロセスに 1 つの `Held` を `Config` で運ぶと、外の振る舞いはそのままで、試験は試験ごとの `Held` で 1 つのプロセスを模せる
- 作った直後に名前で mode を変えるのは、Linux の B の fd が `O_PATH` (`fchmod` を受け付けない) で、umask が持ち主の読み・辿る権限を削ると `O_RDONLY` / `O_SEARCH` では開けないため。親は途中の確かめに通っていて、作った物を他人は付け替えられない (sticky な `/tmp` でも、消す・名前を変えるのは持ち主だけ)

## Alternatives Considered

- 案 A: `Options` に setter を付けて `#[non_exhaustive]` にする
  - 不採用理由: 2 項目に setter を置くと、ハブ DR-0017 が退けた builder の形に近づき、構造体式で足りる所に API が増える
- 案 B: B・D を呼び出しごとに引数で渡す (プロセスの状態を持たない)
  - 不採用理由: ハブ DR-0017 の「B はプロセスごとに 1 つ、D は (プロセス, B) ごとに 1 つ」を満たせない
- 案 C: SPEC-routes の順どおり、開いてから `fchmod` で mode を変える
  - 不採用理由: Linux の `O_PATH` の fd は `fchmod` を受け付けず、`/proc` 経由か `O_RDONLY` で開き直す分岐が要る。umask で開けない時は置き場を捨てることになる

## Consequences

- fork の経路、最終手段 (`LastResort`)、`Loss`・`losses`、`ExListener`・`ExStream`、`SocketFile::discard` は無い
- 既定の `bind` の E と、死んだプロセスの D は、OS の一時ディレクトリの掃除か `sweep()` まで残る。ソケットのディレクトリの B と、落とした B (`sockat-<uid>.<乱数>`) はどちらでも消えないことがある (MANUAL に書く)
- 試験は `src/tests.rs` (経路を強制する単体試験) と `tests/` (公開 API) に置く

## 関連

- ハブ: [DR-0017](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0017-resolve-through-persistent-base.md)、[SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md)、[SPEC-routes](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md)、[適合試験](https://github.com/kawaz/sockat/blob/main/docs/design/conformance.md)
- [DR-0002](DR-0002-std-handles-paths-it-accepts.md)、[DR-0004](DR-0004-relative-path-from-cwd.md)、[DR-0005](DR-0005-widened-sockaddr-on-macos.md)、[DR-0011](DR-0011-routes-not-exposed.md)、[DR-0012](DR-0012-route-failure-handling.md)、[DR-0015](DR-0015-own-socket-matches-std.md)、[DR-0030](DR-0030-conformance-cli.md)
