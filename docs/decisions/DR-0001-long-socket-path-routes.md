# DR-0001: sun_path を超える socket のパスに届く経路と、試す順番

- Status: Accepted (経路と試す順番、相対パスの作り方、Windows を対象外にすることは kawaz 指示 2026-11-18。symlink の名前、fork の経路の権限、FreeBSD の扱いは統括の裁定 2026-11-18。tokio・観測の仕組みは実装側の判断で、残りは「未確定」)、実装済み (未公開)
- Date: 2026-11-18

## Context

Unix domain socket のパスは `struct sockaddr_un` の `sun_path` に入れて `bind(2)` / `connect(2)` に渡す。`sun_path` は固定長の配列で、NUL 込みで macOS と BSD が 104 バイト、Linux が 108 バイトしかない。std の `UnixListener::bind` / `UnixStream::connect` はこれを超えるパスを断る。ファイルシステムの側にはそんな上限は無い。

上限に当たるのは、深い一時ディレクトリにソケットを置く時である。macOS の `$TMPDIR` はそれだけで 50 バイト前後あり、試験の枠組みやサンドボックスがさらに掘る (この作業をしたセッションの `$TMPDIR` は 113 バイトで、その下に何も置けない)。

先行の実装と回避策:

| どこ | やり方 | 足りない所 |
|---|---|---|
| hyoui (DR-0041 決定 5) | 上限を超えたら、ディレクトリの fd に `fchdir` した子を fork して、ファイル名だけで bind / connect する | 毎回 fork する。connect した側の pid がサーバから子に見える |
| llm-gateway の試験 | `/tmp` に短い symlink を張って経由する | 試験の中だけの手当てで、ライブラリになっていない |

上限が効くのは `bind` / `connect` に渡す `sun_path` 引数の長さで、ファイルシステム上のフルパスの長さではない。なので、短い別の綴りを渡せればよい。綴りの作り方は OS で違う。

## Decision

### 1. 100 バイト以内のパスは std にそのまま渡す

- パスが 100 バイト以内なら `UnixListener::bind` / `UnixStream::connect` をそのまま呼ぶ。何も足さない
- 100 は全プラットフォームで同じ値にする。両方の `sun_path` (104 / 108、NUL 込み) より小さいので、同じパスがどの OS でも同じ扱いになる

理由: 短いパスでは std と同じに振る舞うことが一番の約束で、そのためには短いパスに何も足さないのが確実である。

### 2. Linux は `/proc/self/fd/<dirfd>/<name>` に決め打つ

- ソケットを置くディレクトリを `O_PATH | O_DIRECTORY` で開き、`/proc/self/fd/<その fd>/<ファイル名>` を std に渡す。カーネルは `/proc/self/fd/<n>` をその fd が指すディレクトリとして解決するので、ソケットはそのディレクトリにできる (connect ならそこから引く)
- Linux ではこれ以外を試さない。ディレクトリ部分の長さに上限が無く、一時ファイルも fork も要らず、cwd も変えない
- `/proc/self/fd/<n>/<ファイル名>` が `sun_path` (107 バイト + NUL) に収まらない時 (ファイル名がおよそ 90 バイトを超える時) は `ENAMETOOLONG` にする。std に渡すと std 独自の `InvalidInput` になり、他の OS と食い違う
- `/proc` が無い時は、bind / connect の `ENOENT` が「ディレクトリが無い」と読めてしまうので、先に `/proc/self/fd/<n>` を確かめ、/proc が要ることを言うエラーにする (fork に倒すかは「未確定」)
- Android も同じ扱いにする (未検証)

理由: fork は使わない。Linux の `SO_PEERCRED` は connect した時点の pid を記録し、子が終了した後もその pid のままになる (試験 `so_peercred_keeps_the_pid_of_a_forked_connector` で実機確認)。サーバが peer の pid を見る用途を壊す。

### 3. macOS は 4 つの経路を順に試す

100 バイトを超えるパスは、次の順に試す。

| 順 | 経路 | 使う条件 |
|---|---|---|
| a | cwd からの相対パスを std に渡す | 相対パスが 100 バイト以内 |
| b | 広げた sockaddr (後述) で元のパス、無理なら相対パス | どちらかが 253 バイト以内 |
| c | `${TMPDIR:-/tmp}/sockat.XXXXXXXXXX` → ソケットのディレクトリ、の symlink を経由する | symlink を通したパスが 100 (b が使えるなら 253) 以内 |
| d | ディレクトリの fd に `fchdir` した子を fork する | 上が全部使えない時 |

**経路の失敗をどう扱うか。** 経路が実際に bind / connect した結果のエラー (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) はそこで返し、次の経路を試さない。次へ進むのは、その経路自体が使えないと分かった時だけである (広げた sockaddr をカーネルが受け付けない、symlink が作れない、長さが足りない)。理由: 本当の失敗を次の経路でやり直すと、同じエラーを遅れて返すか、別の場所にソケットを作る。

#### a. cwd からの相対パス

相対パスは 2 つの作り方を、この順に試す (kawaz 指示)。

1. **cwd の下にあるなら、前を削る。** パスが cwd の綴りで始まっていれば (区切り単位で比べる。`/a/b` は `/a/bc` の接頭辞ではない)、それを削った残りを使う。cwd の綴りの候補は `getcwd(3)` の値と `$PWD` である。残りは cwd から下へ降りるだけで、元のパスと同じ symlink を同じ順に通るので、解決の結果が元のパスと一致する。realpath も要らない
   - `$PWD` は環境変数なので嘘をつきうる (`chdir` が更新しない、誰でも書ける)。`stat($PWD)` と `stat(".")` の `(st_dev, st_ino)` が一致した時だけ使う。`getcwd` の値はそのまま使う
2. **1 で 100 バイト以内にならない時だけ、両方を実パスに揃えて `..` で上る。** ソケットのディレクトリを `realpath(3)` で実パスにし、`getcwd` の実パス (macOS で `cd /var` すると `/private/var` が返る) との差から、`..` で上ってから下る綴りを作る。両方が実パスなら、どの `..` も実ディレクトリから上るので、カーネルの解決と一致する
   - 書かれたままのパスで比べると、`/var` と `/private/var` のように片方が symlink 経由だと共通部分を見落とす。パスの途中の symlink の先で `..` を使うと、symlink の先の親に着いてしまう

2 つのうち短い方を使う。使う前に、相対パスのディレクトリと元のディレクトリの `(st_dev, st_ino)` が同じことを確かめる (cwd が消されている、他のスレッドが変えた、を弾く)。bind / connect の後に cwd の `(st_dev, st_ino)` が変わっていないことも確かめ、変わっていたら結果を捨ててエラーにする。

- 相対パスは呼んだ時点の cwd に依存し、cwd はプロセスの全スレッドで共有される。他のスレッドが途中で `chdir` すると、別のディレクトリで bind / connect したことになる。上の事後確認で見えるのは、その間に cwd が変わったままの場合だけで、変えて戻した場合は見えない。保証の範囲は「bind / connect の間に cwd を変えないプロセス」で、doc にそう書く
- 事後確認で弾いた bind が作ったソケットファイルは消さない。どこにできたかが分からないので、消そうとすると別のファイルを消しうる
- 元のパスが相対パスの時は、この経路は使わない (元のパス自体が cwd 基準で、短くする手が無い)

#### b. 広げた sockaddr (最大 253 バイト)

- XNU は `bind(2)` / `connect(2)` で `SOCK_MAXADDRLEN` (255) バイトまでのアドレスを受け付け (`getsockaddr`)、`sun_len - offsetof(sun_path)` バイトをパスとして取る (`unp_bind` / `unp_connect`、それが 255 未満であること)。長さと family の 2 バイトを除いて 253 バイトのパスが、NUL 無しで入る。libuv が #5047 で同じ形を使っている
- `sun_len` とアドレス長はどちらも `2 + パスの長さ` にする
- 253 バイトで bind / connect が通り、socket がそのパスにできることを実機で確かめた (試験 `the_widened_sockaddr_carries_253_bytes`)
- カーネルがこの形を受け付けない印は `EINVAL` と `ENAMETOOLONG` とし、それなら c に進む (将来の macOS が受け付けなくなった場合の逃げ道。今の macOS では起きない)

#### c. 短い symlink を経由する

- 置き場は `${TMPDIR:-/tmp}`。`TMPDIR` が空か未設定なら `/tmp`。相対パスの `TMPDIR` は cwd に依存するので、この経路を使わない
- 何かを作る前に、`<置き場>/sockat.XXXXXXXXXX/<ファイル名>` の長さを確かめる。100 以内なら std に、b が使えるなら 253 以内で広げた sockaddr に渡す
- symlink はソケットのディレクトリへのリンクにする。カーネルはリンクを辿ってディレクトリ側にソケットを作るので、socket の実体は元のディレクトリにでき、置き場には何も残らない (試験で実機確認)
- 名前は `sockat.` に乱数 10 文字 (英数 62 種、`arc4random_buf`)。`symlink(2)` は既存の名前を置き換えずに `EEXIST` で失敗するので、これが symlink の `O_EXCL` になる。`EEXIST` なら名前を引き直す (16 回まで。使い切ったら c は使えないとみなし d に進む)。名前の生成を差し替えて、衝突した名前を飛ばすこと、既存のものに触らないこと、回数で打ち切ることを試験した
- bind / connect が成功しても失敗しても、戻る時に symlink を消す (panic でも消す)。消すのに失敗しても無視する (ソケットは既にできていて、失敗させても symlink は戻らない)
- symlink を作れない時 (置き場が無い、書けない、サンドボックス) は d に進む
- symlink を作ってから bind / connect するまでの間は、symlink の置き換えに対して安全とみなす: `/tmp` は sticky で他人は消せず、macOS の `$TMPDIR` は本人専用。プロセスが途中で死ぬと symlink が残る。名前が `sockat.` で始まるので、sockat の残骸と分かる (統括の裁定。一時ファイルの掃除に拾わせるための `tmp.` より、持ち主が分かることを取った)

#### d. fork した子に `fchdir` させる (hyoui DR-0041 決定 5 を引き継ぐ)

1. 親がソケットを作り、ディレクトリを開いた fd を持ち、sockaddr を組み立ててから fork する
2. 子は `fchdir(dirfd)` → ファイル名で `bind` / `connect` → `_exit(結果)`
3. 親はその pid だけを `waitpid` し、終了 status から結果を受け取る (0 が成功、1〜255 が errno)

- fork 後のソケットは親子で同じ open file description を指すので、子の bind / connect が親の fd にそのまま効く
- cwd が変わるのは子だけで、親のスレッドは巻き込まれない。子の区間は async-signal-safe な呼び出し (`fchdir` / `bind` / `connect` / errno の読み出し / `_exit`) だけで、allocation も lock も Rust の destructor も通らない。マルチスレッドのプロセスから呼んでも POSIX の範囲で安全
- **子の status を他者に刈り取られた時** (`SIGCHLD` が `SIG_IGN`、他のスレッドの `waitpid(-1)`。`waitpid` が `ECHILD` で失敗する) は、ソケットの状態から結果を読む。bind は `getsockname` にパスが入っているか、connect は `getpeername` が通るか。この場合 errno は失われ、失敗は一般のエラーになる (試験で確認)
- ファイル名は標準の `sun_path` に入れる。b が使える時はファイル名にも広げた sockaddr を使う (ファイル名が 103 バイトを超える稀な場合)

副作用 (doc に書く):

- **connect した pid がサーバから子に見える。** macOS の `LOCAL_PEERPID` は子の pid を返し、親が send すると親の pid に変わる (試験 `local_peerpid_of_a_forked_connect_names_the_child_until_the_client_sends` で実機確認)。uid / gid はどちらでも同じ
- **子は一瞬、プロセスの全 fd の複製を持つ** (fork の性質)。その間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる。閉じた listener の backlog にさらに接続が入りうる。試験でこれを踏んだ (並行する試験の fork が、別の試験の閉じたはずの listener を生かしていた)
- macOS ではディレクトリを `O_SEARCH` で開く。検索権限だけで開け、その fd に `fchdir` できる (macOS 26.5.2 で確認: 読み取り権限の無い mode 0300 のディレクトリは `O_RDONLY` では `EACCES` で開けないが、`O_SEARCH` なら開けて bind できる。mode 0100 では通常の bind と同じく bind が `EACCES`。`O_EXEC` 単独ではディレクトリに `EISDIR`)。なので通常の bind / connect と同じ権限で足りる。`O_SEARCH` をディレクトリに受け付けない古いカーネルは `EISDIR` / `EINVAL` を返すとみなし、その時は `O_RDONLY` で開き直す (この分岐は手元のカーネルでは通らない)
- 他の Unix の fork と FreeBSD の `bindat` はディレクトリを `O_RDONLY` で開くので、読み取り権限も要る

### 4. FreeBSD は `bindat(2)` / `connectat(2)`、それ以外の Unix は fork

- FreeBSD 10 以降には、ディレクトリの fd を基準にする `bindat` / `connectat` がある。Linux の /proc と同じく、fd を基準にファイル名で bind / connect でき、cwd も一時ファイルも fork も要らない。これを使う (統括の裁定。初回の CI で落ちたら、fork に倒さずに原因を直す)
  - libc crate に宣言が無いので、FreeBSD の `<sys/socket.h>` の宣言を自前で書く
  - 広げた sockaddr は FreeBSD では使わない。libuv が #5047 の後に「FreeBSD で uv_pipe_bind が壊れた」(#5134) と報告されており、XNU と同じ前提は置けない
- NetBSD / OpenBSD / illumos など、ディレクトリの fd を基準にする primitive が無い Unix は、100 バイトを超えたら fork する (POSIX だけで成り立つ手がこれしか無い)。macOS の a〜c は、試せる環境が無いので入れない

理由: fork より副作用の無い手がある OS では、そちらを使う。

### 5. Windows は対象外

- Unix 以外は `compile_error!` で止める (kawaz 裁定)
- 将来入れるとしたら、相対パスで短くする経路だけになる見込み。そのための作り込み (抽象化や型の分岐) は今はしない

### 6. API は std の型を返す同期関数 2 つ

- `sockat::bind(path) -> io::Result<UnixListener>`、`sockat::connect(path) -> io::Result<UnixStream>`。std の `bind` / `connect` と同じ形で、置き換えるだけで使える
- 自前で作るソケットは std と同じ設定にする: close-on-exec、macOS では `SO_NOSIGPIPE`、listen の backlog は std と同じ値 (macOS / Linux / FreeBSD / OpenBSD は -1、他は `SOMAXCONN`)
- ソケットファイルは listener を閉じても消さない (std と同じ)。消すのは利用者で、元のパスで `remove_file` すればよい (unlink には `sun_path` の上限が無い)

### 7. tokio は依存に入れない

- 戻り値が std の型なので、`set_nonblocking(true)` してから `tokio::net::UnixListener::from_std` / `UnixStream::from_std` で渡せる。この手順を crate doc の例に書き、doctest で compile を確かめ、結合試験 (`tests/tokio_interop.rs`) で実際に送受信する
- tokio は dev-dependency にだけ入れる。feature で任意にもしない: 変換は 2 行で済み、feature にすると tokio の版を crate が縛る

### 8. どの経路を通ったかは利用者に見せない

- 経路 (`Route`) は crate 内の値で、公開 API は捨てる
- 試験は crate 内の単体試験 (`src/tests.rs`) に置き、各経路の関数を直接呼んで強制する。macOS の経路の順番の分岐 (広げた sockaddr を受け付けないカーネル、symlink の置き場) は、環境を読む部分を値 (`Config`) にして試験から差し替える。feature も cfg(test) 専用の公開口も要らない

### 9. カーネルが返すアドレスは元のパスではない

- `local_addr()` / `peer_addr()` (`getsockname` / `getpeername`) は、実際に bind に使った綴りを返す: 相対パス、`/proc/self/fd/...`、もう無い symlink、ファイル名だけ。macOS の広げた sockaddr では std のバッファ (106 バイト) で切れたパスになる (試験で確認、panic はしない)
- 利用者には元のパスを持ってもらう。doc にそう書く

### 10. 共通の限界: ディレクトリを先に引く

- /proc・相対パス・`bindat`・fork の経路は、先にディレクトリを引いて (開く、または実パスにする) から、ファイル名で bind / connect する。その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる (connect ならそこを見る)。呼んだ瞬間のパスの意味とずれうる
- symlink の経路は利用者のパスをそのまま symlink の先にするので、この点は元のパスと同じに振る舞う

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 常に fork する (hyoui の方式をそのまま) | fork の副作用 (peer の pid、fd の複製) を、避けられる経路があるのに全員に負わせる |
| 親のプロセスで `chdir` する | cwd は全スレッドで共有され、他のスレッドの相対パスの解決を巻き込む |
| Linux でも相対パス・symlink・fork を試す | /proc の経路が長さの制約無しで通るので要らない。fork は `SO_PEERCRED` の pid を壊す |
| 子プロセスに bind させて fd を `SCM_RIGHTS` で受け取る | fork と同じ副作用に加え、受け渡しの手順が増える。fork 後の fd 共有で足りる |
| 相対パスを書かれたままのパスの比較だけで作る | `/var` と `/private/var` のような違いで短くならない。cwd の接頭辞を削る手 (区切り単位、`$PWD` は stat で照合) と、実パスに揃える手の 2 段にした |
| FreeBSD でも広げた sockaddr を使う | XNU と同じ前提を置けない (libuv #5134)。`bindat` の方が副作用も無い |
| tokio を feature で任意にする | 変換は利用者側の 2 行で済む。feature にすると crate が tokio の版を縛る |
| 経路を公開 API で返す / 強制する feature を作る | 利用者が知る必要が無い。試験は crate 内で足りる |

## 未確定

- Linux で /proc が無い時に fork に倒すか (docs/QUESTIONS.md の SK-Q1。今はエラー。fork すると `SO_PEERCRED` の pid が子になる)
- `TMPDIR` が長すぎる時に `/tmp` も試すか (docs/QUESTIONS.md の SK-Q2。今は `${TMPDIR:-/tmp}` の 1 か所だけ。この作業をしたセッションのように `TMPDIR` が 100 を超える環境では、symlink の経路は広げた sockaddr 頼みになる)
- FreeBSD の `bindat` / `connectat` は compile の確認だけで、実機で動かしていない。CI の FreeBSD の job (VM) が初回の確認になる
- Android は compile もしていない。macOS 以外の Apple OS は iOS の clippy (試験コード込み) だけ確かめた
