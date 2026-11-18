# DR-0007: 最後の経路として、ディレクトリに fchdir した子を fork する

- Status: Accepted
- Date: 2026-11-18

## Context

相対パス・広げた sockaddr・symlink のどれでも届かない環境 (安全で symlink を作れる置き場が無いサンドボックス等、bind でファイル名が symlink の綴りに収まらない時) が残る。Linux と Apple の OS ではそこにスレッド cwd の経路 ([DR-0020](DR-0020-thread-cwd-route.md)) があるが、それも使えない環境 (seccomp の filter が掛かった Linux、`pthread_fchdir_np` の無い libSystem) と、それの無い Unix (NetBSD・OpenBSD・illumos 等) が残る。cwd はプロセスの全スレッドで共有されるので、呼び出し元のプロセスで `chdir` はできない。hyoui (DR-0041 決定 5) は、ディレクトリの fd に `fchdir` した子を fork してファイル名だけで bind / connect する。

## Decision

dirfd 基準の primitive の無い Unix と /proc の無い Linux で、他の経路 (Linux と Apple の OS ではスレッド cwd の経路も) が全部使えない時の最後の経路にする ([DR-0001](DR-0001-long-socket-path-routes.md))。試すのは `bind` と `connect_any` 系だけ ([DR-0010](DR-0010-public-api.md))。

1. 親がソケットを作り、ディレクトリを開いた fd を持ち、sockaddr を組み立ててから fork する (ブロックしない connect では、ソケットを fork の前に `O_NONBLOCK` にする)
2. 子は `fchdir(dirfd)` → ファイル名で `bind` / `connect` → `_exit(結果)`
3. 親はその pid だけを `waitpid` し、終了 status から結果を受け取る (0 が成功、1〜255 が errno)

- bind の後の listen は親がする
- 子の区間は async-signal-safe な呼び出し (`fchdir` / `bind` / `connect` / errno の読み出し / `_exit`) だけにする。allocation も lock も Rust の destructor も通さない
- **子の status を他者に刈り取られた時** (`SIGCHLD` が `SIG_IGN`、他のスレッドの `waitpid(-1)`。`waitpid` が `ECHILD` で失敗する) は、ソケットの状態から結果を読む。bind は `getsockname` にパスが入っているか、connect は `getpeername` が通るか。この場合 errno は失われ、失敗は一般のエラー (`Other`) になる (試験で確認)
- ファイル名は標準の `sun_path` に入れる。macOS で広げた sockaddr ([DR-0005](DR-0005-widened-sockaddr-on-macos.md)) が使える時はファイル名にもそれを使う (ファイル名が 103 バイトを超える稀な場合)

### ディレクトリの開き方と要る権限

| OS | 開き方 | 要る権限 |
|---|---|---|
| macOS | `O_SEARCH` | 通常の bind / connect と同じ (検索、bind なら書き込みも) |
| Linux | /proc の経路のために開いた `O_PATH` の fd ([DR-0003](DR-0003-linux-proc-self-fd.md)) | 検索権限で足りる |
| その他の Unix | `O_RDONLY` | 読み取り権限も要る |

- macOS で `O_SEARCH` をディレクトリに受け付けない古いカーネルは `EISDIR` / `EINVAL` を返すとみなし、その時は `O_RDONLY` で開き直す (この分岐は手元のカーネルでは通らないので、試験では `O_SEARCH` で開いた結果を値として差し替えて通す)

### 副作用 (doc に書く)

- **connect では、サーバに見える peer の pid が、もう終了したプロセス (fork した子) の pid になる。** ソケット自体は呼び出し元が持っていて、普通に使える
  - uid / gid は呼び出し元と同じ
  - macOS の `LOCAL_PEERPID` は、呼び出し元が send すると呼び出し元の pid に変わる (試験 `local_peerpid_of_a_forked_connect_names_the_child_until_the_client_sends`)。Linux の `SO_PEERCRED` は終了した子の pid のままで、send しても変わらない ([DR-0003](DR-0003-linux-proc-self-fd.md))
  - 終了した pid の番号は、後で別のプロセスに再利用されうる。その pid からは、プロセスの情報も ppid も引けない
- **子は一瞬、プロセスの全 fd の複製を持つ** (fork の性質)。その間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる。閉じた listener の backlog にさらに接続が入りうる (並行する試験の fork が、別の試験の閉じたはずの listener を生かしたことがある)

## 理由

- fork 後のソケットは親子で同じ open file description を指すので、子の bind / connect が親の fd にそのまま効く。`O_NONBLOCK` も open file description に付くので、子の connect も待たない
- cwd が変わるのは子だけで、親のスレッドは巻き込まれない。子の区間を async-signal-safe に限るので、マルチスレッドのプロセスから呼んでも POSIX の範囲で安全
- bind の子は bind するだけで listen は親なので、listener に fork の副作用は残らない
- macOS の `O_SEARCH` は検索権限だけで開け、その fd に `fchdir` できる (macOS 26.5.2 で確認: 読み取り権限の無い mode 0300 のディレクトリは `O_RDONLY` では `EACCES` で開けないが、`O_SEARCH` なら開けて bind できる。mode 0100 では通常の bind と同じく bind が `EACCES`。`O_EXEC` 単独ではディレクトリに `EISDIR`)
- Linux 3.5 以降、`O_PATH` のディレクトリの fd に `fchdir` できる。/proc の無い chroot で、uid 1000 が読み取り権限の無い mode 0300 のディレクトリに bind / connect できることを docker で確認した (`O_RDONLY` では `EACCES` で開けない)

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 子プロセスに bind させて fd を `SCM_RIGHTS` で受け取る | fork と同じ副作用に加え、受け渡しの手順が増える。fork 後の fd 共有で足りる |
| 親のプロセスで `chdir` する | cwd は全スレッドで共有され、他のスレッドの相対パスの解決を巻き込む |
| fork しない bind を設ける | bind の fork は子が bind するだけで listen は親なので、相手から見える副作用が無い。fork が禁じられた環境 (seccomp 等) では fork の失敗がそのままエラーで返る |

## 未確定

- NetBSD・OpenBSD・illumos で fork の経路で connect した時の peer の資格情報の振る舞いは確かめていない
- Linux の `O_PATH` の fd で権限が足りることは、/proc の無い環境では常設の試験が無い (試験は docker の root で走り、権限を確かめられない)

## 判断の出所

- 最後の経路として fork する (hyoui DR-0041 決定 5 を引き継ぐ): kawaz 指示 2026-11-18
- ディレクトリの開き方と要る権限: 統括の裁定 2026-11-18
- 上に無い細部: 出所の記録なし
