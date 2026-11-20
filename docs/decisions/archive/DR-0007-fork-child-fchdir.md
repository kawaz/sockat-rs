# DR-0007: 最後の手として、ディレクトリに fchdir した子を fork する

- Status: Superseded by DR-0032
- Date: 2026-11-18

## Context

D を通す経路のスレッド cwd と /proc 版 ([DR-0027](DR-0027-own-directory-and-fallback-routes.md)) が使えない環境 (seccomp の filter が掛かった Linux、`pthread_fchdir_np` の無い libSystem、それらの無い NetBSD・OpenBSD・illumos 等) が残る。cwd はプロセスの全スレッドで共有されるので、呼び出し元のプロセスで `chdir` はできない。hyoui (DR-0041 決定 5) は、ディレクトリの fd に `fchdir` した子を fork してファイル名だけで bind / connect する。

## Decision

他の手段が全部使えない時の最後の手にする ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))。試すのは bind (fork-cwd、fork-cwd-sockdir) と、最終手段を使う `connect_ex` 系 (fork-cwd-direct) だけ ([DR-0010](DR-0010-public-api.md))。

| 要求 | 子が `fchdir` する fd | 綴り |
|---|---|---|
| bind | D | `R/<ファイル名>` |
| bind (D をどこにも作れない時) | ソケットのディレクトリ | `.sockat.XXXXXXXXXX/<ファイル名>` (R はソケットのディレクトリの中) |
| 最終手段を使う `connect_ex` 系 (connect の順の後) | ソケットのディレクトリ | ファイル名 |

1. 親がソケットを作り、ディレクトリの fd を持ち、sockaddr を組み立ててから fork する (ブロックしない connect では、ソケットを fork の前に `O_NONBLOCK` にする)
2. 子は `fchdir(dirfd)` → 綴りで `bind` / `connect` → `_exit(結果)`
3. 親はその pid だけを `waitpid` し、終了 status から結果を受け取る (0 が成功、1〜255 が errno)。それから R と D を片付ける

- bind の後の listen は必ず親がする。子が listen すると、クライアントに見えるサーバの peer が死んだ子になる ([DR-0025](DR-0025-fork-bind-listen-by-parent.md))
- 子の区間は async-signal-safe な呼び出し (`fchdir` / `bind` / `connect` / errno の読み出し / `_exit`) だけにする。allocation も lock も Rust の destructor も通さない
- **子の status を他者に刈り取られた時** (`SIGCHLD` が `SIG_IGN`、他のスレッドの `waitpid(-1)`。`waitpid` が `ECHILD` で失敗する) は、ソケットの状態から結果を読む。bind は `getsockname` にパスが入っているか、connect は `getpeername` が通るか。この場合 errno は失われ、失敗は一般のエラー (`Other`) になる (試験で確認)
- 綴りは標準の `sun_path` に入れる。Apple の OS の `connect_ex` 系の fork-cwd-direct で、カーネルが広げた sockaddr ([DR-0005](DR-0005-widened-sockaddr-on-macos.md)) を断っていなければ、103 バイトを超えるファイル名にそれを使う。bind は使わない

### ディレクトリの開き方と要る権限

| OS | ソケットのディレクトリの開き方 | 要る権限 |
|---|---|---|
| Apple の OS | `O_SEARCH` | 通常の bind / connect と同じ (検索、bind なら書き込みも) |
| Linux | `O_PATH` | 検索権限で足りる |
| その他の Unix | `O_RDONLY` | 読み取り権限も要る。開けなければこの経路は使えない |

- D は D を通す経路が開いた fd ([DR-0027](DR-0027-own-directory-and-fallback-routes.md)) をそのまま使う
- Apple の OS で `O_SEARCH` をディレクトリに受け付けない古いカーネルは `EISDIR` / `EINVAL` を返すとみなし、その時は `O_RDONLY` で開き直す (この分岐は手元のカーネルでは通らないので、試験では `O_SEARCH` で開いた結果を値として差し替えて通す)

### 副作用 (doc に書く)

相手から見える副作用は connect だけにある。bind は listen を親がするので、相手から見える副作用が無い ([DR-0025](DR-0025-fork-bind-listen-by-parent.md))。

- **connect では、サーバに見える peer の pid が、もう終了したプロセス (fork した子) の pid になる。** ソケット自体は呼び出し元が持っていて、普通に使える
  - uid / gid は呼び出し元と同じ
  - macOS の `LOCAL_PEERPID` は、呼び出し元が send すると呼び出し元の pid に変わる (試験 `local_peerpid_of_a_forked_connect_names_the_child_until_the_client_sends`)。Linux の `SO_PEERCRED` は終了した子の pid のままで、send しても変わらない (試験 `so_peercred_keeps_the_pid_of_a_forked_connector`)
  - 終了した pid の番号は、後で別のプロセスに再利用されうる。その pid からは、プロセスの情報も ppid も引けない
- 相手から見えない副作用 (bind でも connect でも)
  - **子は一瞬、プロセスの全 fd の複製を持つ** (fork の性質)。その間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる。閉じた listener の backlog にさらに接続が入りうる (並行する試験の fork が、別の試験の閉じたはずの listener を生かしたことがある)
  - 親に `SIGCHLD` が届く
  - `pthread_atfork` の handler が走る
  - 監視ツールに短命の子が見える
  - 他のスレッドの `waitpid(-1)` が子を刈り取りうる (その時はソケットの状態から結果を読む)

## 理由

- fork 後のソケットは親子で同じ open file description を指すので、子の bind / connect が親の fd にそのまま効く。`O_NONBLOCK` も open file description に付くので、子の connect も待たない
- cwd が変わるのは子だけで、親のスレッドは巻き込まれない。子の区間を async-signal-safe に限るので、マルチスレッドのプロセスから呼んでも POSIX の範囲で安全
- bind の子は bind するだけで listen は親なので、listener に相手から見える fork の副作用は残らない
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

## 判断の出所

- 最後の手として fork する (hyoui DR-0041 決定 5 を引き継ぐ): kawaz 指示 2026-11-18
- bind の fork は D の中の R を通す、D をどこにも作れない時は R をソケットのディレクトリに作る: kawaz 裁定 (ハブ DR-0012、2026-11-19)
- ディレクトリの開き方と要る権限: 統括の裁定 2026-11-18
- 上に無い細部: 出所の記録なし
