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
| `Spelling` | `#[non_exhaustive]` の enum。`Original`、`CwdRelative`、`Linked { removable, until: Until, usable_cross_uid, usable_with_private_tmp }`、`Dangling`。`Linked` の variant も `#[non_exhaustive]` にし、属性を足しても利用者の pattern (`Linked { removable, .. }`) を壊さない (enum の `#[non_exhaustive]` は variant を足す時にしか効かない) |
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
- 呼び出しの中で消す E (bind が失敗した時・ソケットのファイルを覚えられなかった時の E。既定の bind の link-dir の E は除く。thread-cwd・at の E、既定の connect の E) は、消すのに失敗しても捨てる (`let _`、`TempLink` の Drop)。呼び出しの成否も元のエラーも変えない
- bind の後に覚えるソケットのファイルは、どの経路も `route::remembered_at` で引く: パスを `realpath` し (最後の要素の symlink も辿る。Apple・NetBSD の bind はそこを辿ってその先にソケットを作る)、その `lstat` を覚える。ソケットでなければ `ENOTSOCK` で覚えない。引く場所はハブ SPEC「remove」の覚える場所: standard は bind の後に元のパス、link-file は bind の後に E が指すパス (元のパスを絶対パスにした物、`Target::absolute`)、link-dir・thread-cwd・at は E が指すソケットのディレクトリの実パス (`Target::real_dir`、経路を組む前に引いた物) とファイル名を引き、元のパスを引き直さない。link-dir は加えて綴りの `stat` が同じ物かを確かめる。`ForBind::socket_file()` は綴りを同じ関数で引く。abstract の名前 (Linux / Android) はファイルでないので引かず、`route::Kept::Abstract` (ファイルを持たない `SocketFile`、`remove` はファイルを消さず E の片付けだけ) にする (ハブ SPEC「remove」)。std はパスで渡した abstract の名前を bind しないので、`bind_ex` は std の `InvalidInput` を返し、ファイルを持たない `SocketFile` を返すのは `ForBind::socket_file()` (利用者が std の `bind_addr` で bind した後) だけ
- std に渡す前に引数の誤り (`InvalidInput`、`route` は `None`、`skipped` は空) にする物は、先頭以外の NUL (ハブ DR-0006) と空のパス (ハブ SPEC「短いパス」)。どちらも長さによらず、全ての関数の入口 (`route::run`・`route::resolve`) で、abstract の名前の判定より前に断る (`path::refuse_empty`・`path::refuse_nul`)。`Options.base` の形の検査 (相対パス、NUL を含む、名前で終わらない。`base::split_given`) はその前
- 作らないもの: `datagram::send_to`・`send_to_ex` と `seqpacket` (ハブ SPEC の「未確定」)

### B・D・E の持ち方 (`src/base.rs`)

- プロセスの中の登録簿 (`Mutex<Vec<Arc<Base>>>`) が、このプロセスの B を持つ。B は fd で掴み、プロセスが終わるまで閉じない (close-on-exec)。使う前に、fd の `fstat` の `st_nlink` が 0 でなく、B の綴りの `lstat` が持っている `(st_dev, st_ino)` と同じかを確かめ、外れたら外して次に作り直す。D も同じ。外した B は fd を開けたまま `base::RETIRED` に、外した D は fd と flock を持ったまま B の `retired_d` に、プロセスの間持ち続ける (閉じると番号が別のディレクトリに使い回され、procB の綴りでその番号を通る生きているソケットの綴りが別のディレクトリに当たる。ハブ SPEC-routes「実体の置き場」)。消したディレクトリの `st_nlink` は Linux では 0 になるが macOS の APFS では 2 のままなので、名前の確かめは省かない
- 置き場 1〜3 の B は `Held` が、普段の B と `usable_cross_uid` の B の 2 つの枠で持つ (普段の B が他の uid に辿らせる時は同じ B)。一度持ったらプロセスの間それを使い、作れなかった置き場は覚えず次の呼び出しで試し直す。外では `route::Config` がプロセスの 1 つの `Held` を運ぶ
- 置き場 1〜3 は `base::places_from` が環境変数から作り、同じパスか、realpath が同じディレクトリ (`metadata` の `(st_dev, st_ino)` が同じ。macOS の `TMPDIR=/private/tmp` と `/tmp`) の置き場を重ねて並べない。無い置き場はパスだけで見分ける。途中の確かめは、置き場 1〜3 とソケットのディレクトリは実パスごとにプロセスで最初の 1 回 (`base::Check::Once`)、`Options.base` は呼び出しのたび (`Check::EveryCall`) (ハブ SPEC-routes「B の作り方」2)
- 置き場の順に、何も作らずに分かること (置き場が無い、途中の確かめに外れる、`usable_cross_uid` で途中が other に x を持たない) で外れる置き場を飛ばし、最初に残った置き場で綴りの長さを見積もる。収まらなければ B を作らず、次の置き場にも移らない
- 置き場 1〜3 の既定の名前に、B の確かめ (実ディレクトリ・持ち主が自分・他人が書けない) に外れる物が既にある時 (占拠)、`Base::make` は `Unmade::Occupied` を返し、`base::automatic` はその置き場に何も作らず次の置き場へ進む。その呼び出しで占拠された置き場を飛ばした時は `automatic` が `route::Target` の `occupied` を立て、B を使う経路 (link-dir、link-file、thread-cwd・at) の `skipped` の `reason` に占拠を書き添える (`route::names::with_occupied`: B を持てない時は `"base occupied"`、綴りが収まらない時は `"spelling too long; base occupied"` 等。B に関わらない理由はそのまま)。前の呼び出しで次の置き場の B を持った後の呼び出しでは書き添えない。`Options.base` の B の占拠は今まで通りそのエラー (`EPERM` / `ENOTDIR`)
- 他の uid に辿らせるか (other の x) は、B を持った時に、途中の判定 (`Base::way_search`) と B 自身の判定 (`Base::b_search`) に分けて `Base` に覚える。途中の判定は後で見直さない。B 自身が other の x を持たないと覚えた時は、`usable_cross_uid` の呼び出しのたびに B の fd の `fstat` で見直し、持っていればそれを覚える (`Base::others_search`)。`Options.base` の安全の確かめ (途中、持ち主、他人が書けない) は呼び出しのたびに行い、other の x だけは持っている B の判定 (と見直し) を使う
- D を作れなかった B は、その呼び出しの間 (`route::Target`) 覚え、D を使う経路で作り直さない。mode をそろえられない・開けない時は、作った空の D を消す
- ソケットのディレクトリの B は procB の綴りでだけ使う (`route::forms` が実パスの綴りを出さない)
- D は B ごとに 1 つ、`O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` の fd に `flock(LOCK_SH | LOCK_NB)` を掛けて持ち続ける。掛けた後に名前で引き直して同じ D かを確かめ、違えば作り直す
- 作った B と D は、作った直後に名前で `fchmodat(0711)` してから開いて確かめる
- resolve の E は D の中のカウンタ (36 進)、thread-cwd・at の E と B 直下の E は乱数 10 文字 (`arc4random_buf` / `getrandom`)
- 既定の bind (`bind`・`datagram::bind`。中では `route::run` の `remember` が false の bind) の link-dir の E は、D ごとの表 `Dir::shared` (`Mutex<HashMap<PathBuf, OsString>>`: ソケットのディレクトリの realpath → カウンタの E の名前) で (D, realpath) ごとに 1 つにして使い回す (`route::link_dir_shared`)。表は D が持つので、D を作り直すと空から始まる
  - 使う前に D の fd を基準に `readlinkat` (`sys::read_link_at`) し、失敗するか指す先が realpath と違えば、新しい E を作って表の値を置き換える
  - 表の lock は E を引く・作って表に入れる間だけ持ち、bind の間は持たない。新しい E は bind の前に表に入れ、bind に失敗しても消さない (その呼び出しで作った物も)。同じディレクトリへの同時の既定の bind は lock の中で同じ E を引くので、E は 1 つのまま
  - 使い回す E は `Reached.link` に入れない (誰も片付けない)。`bind_ex` と resolve と link-file の E は呼び出しごとのまま
  - 試験だけの差し替え口は持たない。試験は試験ごとの `Held` で D を分け、E を手で消す・別の所を指す symlink に置き換えて、作り直しを確かめる
- procB の綴りは `/proc/<getpid()>/fd/<B の fd>`。その stat が B の fd と同じ `(st_dev, st_ino)` の時だけ使う
- `/proc` を使えるかは、どれも `route::names_fd` (`<proc>/…/fd/<fd>` の stat と fd の fstat の `(st_dev, st_ino)` が同じか) で決める (ハブ SPEC「用語」)。connect の行の選択は `linux::probe` が `/` を開いた fd で `/proc/self/fd` を、proc-direct は呼び出しごとにソケットのディレクトリの fd で `/proc/self/fd` を、procB の綴りの長さの見積もり (`route::proc_prefix_estimate`) は B を開く前に `/` を開いた fd で `/proc/<pid>/fd` を確かめる
- `sweep`・`sweep_base` が見るのは、B の直下の、名前が D の形 (`base::is_random_name`: ASCII の英数字 10 文字) のディレクトリだけ。他の名前の物には触らない。B と D の中は fd から列挙し (`sys::read_dir_at`: `openat(fd, ".")` と `fdopendir`)、D に排他ロックを取った後に名前の `(st_dev, st_ino)` がロックした fd と同じかを確かめてから消す。`sweep()` は読めない B (`EACCES`) を黙って飛ばす (`sweep_base` はエラーを返す)。外から D を置き換える場面は、試験だけの thread_local (`base::AFTER_SWEEP_LOCK`) でロックの直後に作る

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
- E を消せない場面は、試験だけの thread_local (`base::FAIL_UNLINK_E`) で、そのスレッドの E の unlink を `EPERM` にして作る。bind の直後にソケットのファイルが置き換わる場面は、試験だけの thread_local (`route::AFTER_BIND`) で、bind が通ってから覚えるまでの間に置き換えを入れて作る (どちらも呼び出しの途中に他のプロセスが割り込む形で、試験から外の手で起こせない)。元のパスの途中の symlink を経路を組んだ後に張り替える場面は、bind の前を `Config` の cwd の引き直しの間 (ディレクトリの実パスを引いた後、経路の前に呼ばれる) で、bind の後を `route::AFTER_BIND` で作る。E とソケットのファイルを消した回数は試験だけのフィールド (`Link::unlinks`・`Clean::file_unlinks`) で数え、並行の呼び出しで消す操作が 1 回だけかを確かめる
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
- 既定の bind の link-dir の E を、失敗した呼び出しで作った物も含めて消さないのは、同時に別の既定の bind がその E を引いて bind しているかもしれないため (ハブ SPEC-routes「経路の失敗の扱い」)。消さないので、bind の間 lock を持って既定の bind どうしを順番にする必要が無い。残った E は次の既定の bind が使う
- 空のパスを std に渡さないのは、[DR-0002](DR-0002-std-handles-paths-it-accepts.md) の「std が受け付けるパスには何も足さない」の例外 (先頭以外の NUL と同じ)。Linux の std は空のパスの bind を名前の無い abstract のアドレスに autobind し、それは呼び出し側が頼んだソケットのファイルではない (`bind_ex` が覚えるファイルも無い)。macOS の std は `EINVAL` を返すので、errno の無い `InvalidInput` に変わるが `ErrorKind` は同じ
- B 自身の other の x だけを見直すのは、2 つのプロセスが同時に B を初めて作ると、片方の `mkdirat` (0700) と `fchmodat` (0711) の間にもう片方がその B を開いて 0700 と覚えうるため (ハブ SPEC-routes「実体の置き場」)。見直しは B の fd の `fstat` 1 回で、B を作り直さない。持っていると分かった後は覚えて見直さない (持っていると覚えた B を見直さないのと同じ)
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
- 既定の `bind` の E と、死んだプロセスの D は、OS の一時ディレクトリの掃除か `sweep()` まで残る。ソケットのディレクトリの B はどちらでも消えないことがある (MANUAL に書く)
- 試験は `src/tests.rs` (経路を強制する単体試験) と `tests/` (公開 API) に置く

## 関連

- ハブ: [DR-0017](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0017-resolve-through-persistent-base.md)、[SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md)、[SPEC-routes](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md)、[適合試験](https://github.com/kawaz/sockat/blob/main/docs/design/conformance.md)
- [DR-0002](DR-0002-std-handles-paths-it-accepts.md)、[DR-0004](DR-0004-relative-path-from-cwd.md)、[DR-0005](DR-0005-widened-sockaddr-on-macos.md)、[DR-0011](DR-0011-routes-not-exposed.md)、[DR-0012](DR-0012-route-failure-handling.md)、[DR-0015](DR-0015-own-socket-matches-std.md)、[DR-0030](DR-0030-conformance-cli.md)
