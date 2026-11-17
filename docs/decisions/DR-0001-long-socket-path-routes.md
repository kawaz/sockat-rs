# DR-0001: sun_path を超える socket のパスに届く経路と、試す順番

- Status: Accepted (経路と試す順番、相対パスの作り方、Windows を対象外にすることは kawaz 指示 2026-11-18。/proc が無い時の fork、symlink の置き場に /tmp を足すことは kawaz 裁定 2026-11-18。symlink の名前、fork の経路の権限、FreeBSD の扱いは統括の裁定 2026-11-18。設定を持つ connect の型 (ブロックしない、fork を試さない、子の pid) を足すことは kawaz 裁定 2026-11-18。tokio・観測の仕組みは実装側の判断で、残りは「未確定」)、0.1.0 で実装済み (設定を持つ connect は未実装)
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

### 2. Linux は `/proc/self/fd/<dirfd>/<name>`、/proc が無い時だけ fork

- ソケットを置くディレクトリを `O_PATH | O_DIRECTORY` で開き、`/proc/self/fd/<その fd>/<ファイル名>` を std に渡す。カーネルは `/proc/self/fd/<n>` をその fd が指すディレクトリとして解決するので、ソケットはそのディレクトリにできる (connect ならそこから引く)
- /proc がある時は、これ以外を試さない。ディレクトリ部分の長さに上限が無く、一時ファイルも fork も要らず、cwd も変えない
- `/proc/self/fd/<n>/<ファイル名>` が `sun_path` (107 バイト + NUL) に収まらない時 (ファイル名がおよそ 90 バイトを超える時) は `ENAMETOOLONG` にする。std に渡すと std 独自の `InvalidInput` になり、他の OS と食い違う
- **/proc が無い時は fork の経路 (3 d) に進む** (kawaz 裁定)。ディレクトリを開いた後に `/proc/self/fd/<n>` を stat し、引けなければ /proc の経路は使えないとみなす (/proc を mount していない chroot や最小のコンテナ)。決定 3 の「経路自体が使えない時だけ次へ」の扱いで、/proc の経路で実際に bind / connect した結果のエラーはそのまま返し、fork でやり直さない。/proc を確かめずに bind / connect すると、/proc が無いだけなのに「ディレクトリが無い」と読める `ENOENT` になる
  - fork の子は、/proc の経路のために開いた `O_PATH` の fd にそのまま `fchdir` する (Linux 3.5 以降、`O_PATH` のディレクトリの fd に `fchdir` できる)。なので /proc の経路と同じく、ディレクトリの検索権限で足りる (/proc の無い chroot で、uid 1000 が読み取り権限の無い mode 0300 のディレクトリに bind / connect できることを docker で確認した。`O_RDONLY` では `EACCES` で開けない。試験は docker の root で走り権限を確かめられないので、常設の試験は無い)
  - fork の経路では、ファイル名の長さの上限は fork の経路のもの (標準の `sun_path` に入ること) になる。/proc の綴りの上限 (およそ 90 バイト) は、その綴りを使わないので当てない
  - fork の副作用 (3 d) がこの時だけ乗る。特に **connect した時、サーバの `SO_PEERCRED` は終了した子の pid になり、クライアントが send しても変わらない** (試験 `without_proc_the_server_sees_the_forked_child_as_the_peer`)。/proc の無い環境で長いパスに connect した時だけの話で、doc に書く
- Android も同じ扱いにする (未検証)

理由: /proc がある時は fork を使わない。Linux の `SO_PEERCRED` は connect した時点の pid を記録し、子が終了した後もその pid のままになる (試験 `so_peercred_keeps_the_pid_of_a_forked_connector` で実機確認)。サーバが peer の pid を見る用途を壊す。/proc が無い時は他の Unix と同じ fork に倒し、bind / connect できないよりは、pid の副作用を負ってでも届く方を取る (kawaz 裁定)。

### 3. macOS は 4 つの経路を順に試す

100 バイトを超えるパスは、次の順に試す。

| 順 | 経路 | 使う条件 |
|---|---|---|
| a | cwd からの相対パスを std に渡す | 相対パスが 100 バイト以内 |
| b | 広げた sockaddr (後述) で元のパス、無理なら相対パス | どちらかが 253 バイト以内 |
| c | `<置き場>/sockat.XXXXXXXXXX` → ソケットのディレクトリ、の symlink を経由する。置き場は `TMPDIR`、`/tmp` の順の候補 | symlink を通したパスが 100 (b が使えるなら 253) 以内で、symlink を作れる候補がある |
| d | ディレクトリの fd に `fchdir` した子を fork する | 上が全部使えない時 |

**経路の失敗をどう扱うか。** 経路が実際に bind / connect した結果のエラー (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) はそこで返し、次の経路を試さない。次へ進むのは、その経路自体が使えないと分かった時だけである (広げた sockaddr をカーネルが受け付けない、symlink が作れない、長さが足りない)。理由: 本当の失敗を次の経路でやり直すと、同じエラーを遅れて返すか、別の場所にソケットを作る。

#### a. cwd からの相対パス

相対パスは 2 つの作り方を、この順に試す (kawaz 指示)。

1. **cwd の下にあるなら、前を削る。** パスが cwd の綴りで始まっていれば (区切り単位で比べる。`/a/b` は `/a/bc` の接頭辞ではない)、それを削った残りを使う。cwd の綴りの候補は `getcwd(3)` の値と `$PWD` である。残りは cwd から下へ降りるだけで、元のパスと同じ symlink を同じ順に通るので、解決の結果が元のパスと一致する。realpath も要らない
   - `$PWD` は環境変数なので嘘をつきうる (`chdir` が更新しない、誰でも書ける)。`stat($PWD)` と `stat(".")` の `(st_dev, st_ino)` が一致した時だけ使う。`getcwd` の値はそのまま使う
2. **1 で 100 バイト以内にならない時だけ、両方を実パスに揃えて `..` で上る。** ソケットのディレクトリを `realpath(3)` で実パスにし、`getcwd` の実パス (macOS で `cd /var` すると `/private/var` が返る) との差から、`..` で上ってから下る綴りを作る。両方が実パスなら、どの `..` も実ディレクトリから上るので、カーネルの解決と一致する
   - 書かれたままのパスで比べると、`/var` と `/private/var` のように片方が symlink 経由だと共通部分を見落とす。パスの途中の symlink の先で `..` を使うと、symlink の先の親に着いてしまう

2 つのうち短い方を使う。使う前に、相対パスのディレクトリと元のディレクトリの `(st_dev, st_ino)` が同じことを確かめる (cwd が消されている、他のスレッドが変えた、を弾く)。bind / connect の後に cwd の `(st_dev, st_ino)` が変わっていないことも確かめ、変わっていたら結果を捨ててエラーにする (試験は bind / connect を行う関数の中で cwd を変えて、他のスレッドの代わりにする)。

- 相対パスは呼んだ時点の cwd に依存し、cwd はプロセスの全スレッドで共有される。他のスレッドが途中で `chdir` すると、別のディレクトリで bind / connect したことになる。上の事後確認で見えるのは、その間に cwd が変わったままの場合だけで、変えて戻した場合は見えない。保証の範囲は「bind / connect の間に cwd を変えないプロセス」で、doc にそう書く
- 事後確認で弾いた bind が作ったソケットファイルは消さない。どこにできたかが分からないので、消そうとすると別のファイルを消しうる
- 元のパスが相対パスの時は、この経路は使わない (元のパス自体が cwd 基準で、短くする手が無い)

#### b. 広げた sockaddr (最大 253 バイト)

- XNU は `bind(2)` / `connect(2)` で `SOCK_MAXADDRLEN` (255) バイトまでのアドレスを受け付け (`getsockaddr`)、`sun_len - offsetof(sun_path)` バイトをパスとして取る (`unp_bind` / `unp_connect`、それが 255 未満であること)。長さと family の 2 バイトを除いて 253 バイトのパスが、NUL 無しで入る。libuv が #5047 で同じ形を使っている
- `sun_len` とアドレス長はどちらも `2 + パスの長さ` にする
- 253 バイトで bind / connect が通り、socket がそのパスにできることを実機で確かめた (試験 `the_widened_sockaddr_carries_253_bytes`)
- カーネルがこの形を受け付けない印は `EINVAL` と `ENAMETOOLONG` とし、それなら c に進む (将来の macOS が受け付けなくなった場合の逃げ道。今の macOS では起きない)

#### c. 短い symlink を経由する

- 置き場の候補は `TMPDIR`、`/tmp` の順 (kawaz 裁定)。`TMPDIR` は空でなく絶対パスの時だけ候補に入れる (相対パスは cwd に依存するので外す。外すのは `TMPDIR` だけで、`/tmp` は試す)。`TMPDIR` が `/tmp` そのもの (`/tmp/` も含め、綴りが同じ) なら 1 回だけ試す。`/private/tmp` のように綴りの違う同じディレクトリは別の候補として扱う (綴りが短くなりうるので)
- 候補を順に見て、最初に使えたもので bind / connect する。各候補では、何かを作る前に `<置き場>/sockat.XXXXXXXXXX/<ファイル名>` の長さを確かめる。100 以内なら std に、b が使えるなら 253 以内で広げた sockaddr に渡す。長さが足りない候補には何も作らず次の候補へ進む (`TMPDIR` が深すぎる時に `/tmp` で短くする。この作業をしたセッションのように `TMPDIR` が 100 バイトを超える環境で効く)
- 候補で symlink を作れない時 (置き場が無い、書けない、サンドボックス、名前を使い切った) も次の候補へ進む。広げた sockaddr をカーネルが受け付けなかった時も、以降の候補は 100 以内かで見て続ける。候補が尽きたら d に進む。symlink を作った後の bind / connect の失敗は、その結果を返し、次の候補で試し直さない (試験は後の候補のディレクトリの mtime が変わらないことで、そこに何も作られていないことを確かめる)
- symlink はソケットのディレクトリへのリンクにする。カーネルはリンクを辿ってディレクトリ側にソケットを作るので、socket の実体は元のディレクトリにでき、置き場には何も残らない (試験で実機確認)
- 名前は `sockat.` に乱数 10 文字 (英数 62 種、`arc4random_buf`)。`symlink(2)` は既存の名前を置き換えずに `EEXIST` で失敗するので、これが symlink の `O_EXCL` になる。`EEXIST` なら名前を引き直す (16 回まで。使い切ったらその候補は使えないとみなし、次の候補に進む)。名前の生成を差し替えて、衝突した名前を飛ばすこと、既存のものに触らないこと、回数で打ち切ることを試験した
- bind / connect が成功しても失敗しても、戻る時に symlink を消す (panic でも消す)。消すのに失敗しても無視する (ソケットは既にできていて、失敗させても symlink は戻らない)
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

- **connect した pid がサーバから子に見える。** macOS の `LOCAL_PEERPID` は子の pid を返し、親が send すると親の pid に変わる (試験 `local_peerpid_of_a_forked_connect_names_the_child_until_the_client_sends` で実機確認)。uid / gid はどちらでも同じ。/proc の無い Linux の `SO_PEERCRED` は子の pid のままで、send しても変わらない (決定 2)
- **子は一瞬、プロセスの全 fd の複製を持つ** (fork の性質)。その間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる。閉じた listener の backlog にさらに接続が入りうる。試験でこれを踏んだ (並行する試験の fork が、別の試験の閉じたはずの listener を生かしていた)
- macOS ではディレクトリを `O_SEARCH` で開く。検索権限だけで開け、その fd に `fchdir` できる (macOS 26.5.2 で確認: 読み取り権限の無い mode 0300 のディレクトリは `O_RDONLY` では `EACCES` で開けないが、`O_SEARCH` なら開けて bind できる。mode 0100 では通常の bind と同じく bind が `EACCES`。`O_EXEC` 単独ではディレクトリに `EISDIR`)。なので通常の bind / connect と同じ権限で足りる。`O_SEARCH` をディレクトリに受け付けない古いカーネルは `EISDIR` / `EINVAL` を返すとみなし、その時は `O_RDONLY` で開き直す (この分岐は手元のカーネルでは通らないので、試験では `O_SEARCH` で開いた結果を値として差し替えて通す)
- Linux の fork は、/proc の経路のために `O_PATH` で開いた fd に `fchdir` するので、検索権限で足りる (決定 2)
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
- ただし、connect が fork の経路を通ると、サーバから見える peer の pid がすぐに終わる子の pid になる (Linux の `SO_PEERCRED` は変わらず、macOS の `LOCAL_PEERPID` は親が send するまで)。peer の pid を監査と突き合わせる用途のために、`connect` とは別に、設定を持つ connect の型を 1 つ足す (kawaz 裁定 2026-11-18、0.1.0 の次の版)。設定は「ブロックしない」(Linux で相手の backlog が満杯の時に待たず `EAGAIN` を返す。tokio の connect と同じ) と「fork の経路を試さない」(他の経路が使えなければ、パスが `sun_path` に入らない時の普通の connect と同じく `ENAMETOOLONG`。fork を使わないなら普通の connect と同じ立場なので、専用のエラーや文言は作らない。kawaz 裁定)、出力は stream と、fork の経路を通った時にサーバに見える子の pid。設定と出力は独立した軸なので、関数を足すと組み合わせの数だけ増える。型 1 つにまとめた。返すのは経路の名前でなく、結果としてサーバに見えるもの (pid) なので、経路は見せないままである。bind は子が bind するだけで listen は親がするので、手当ては要らない
- 試験は crate 内の単体試験 (`src/tests.rs`) に置き、各経路の関数を直接呼んで強制する。macOS の経路の順番の分岐 (広げた sockaddr を受け付けないカーネル、symlink の置き場の候補) は、環境を読む部分を値 (`Config`) にして試験から差し替える。Linux の /proc の有無は、/proc の場所 (`/proc/self/fd`) を経路の順番の関数の引数にし、試験は何も無い場所や空のディレクトリを渡して /proc の無い環境の代わりにする (本物の /proc には触らない)。feature も cfg(test) 専用の公開口も要らない

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
| /proc がある Linux でも相対パス・symlink・fork を試す | /proc の経路が長さの制約無しで通るので要らない。fork は `SO_PEERCRED` の pid を壊す |
| 子プロセスに bind させて fd を `SCM_RIGHTS` で受け取る | fork と同じ副作用に加え、受け渡しの手順が増える。fork 後の fd 共有で足りる |
| 相対パスを書かれたままのパスの比較だけで作る | `/var` と `/private/var` のような違いで短くならない。cwd の接頭辞を削る手 (区切り単位、`$PWD` は stat で照合) と、実パスに揃える手の 2 段にした |
| FreeBSD でも広げた sockaddr を使う | XNU と同じ前提を置けない (libuv #5134)。`bindat` の方が副作用も無い |
| tokio を feature で任意にする | 変換は利用者側の 2 行で済む。feature にすると crate が tokio の版を縛る |
| 経路を公開 API で返す / 強制する feature を作る | 利用者が知る必要が無い。試験は crate 内で足りる |

## 未確定

- 決定 8 の設定を持つ connect の型の名前
- Android は compile もしていない。macOS 以外の Apple OS は iOS の clippy (試験コード込み) だけ確かめた
