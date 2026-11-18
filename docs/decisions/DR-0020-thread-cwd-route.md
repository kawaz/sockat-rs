# DR-0020: スレッドだけ cwd を移す経路を symlink の後、fork の前に置く

- Status: Accepted
- Date: 2026-11-18

## Context

- fork の経路 ([DR-0007](DR-0007-fork-child-fchdir.md)) は副作用がある (サーバに見える peer の pid が終了した子の pid になる、子が全 fd の複製を持つ) ので、`connect` / `connect_nonblocking` は試さない ([DR-0010](DR-0010-public-api.md))
- そのため symlink の経路 ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md)) も使えない時、`connect` は `bind` より手前で `ENAMETOOLONG` になる
- Linux と Apple の OS には、プロセスの cwd を変えずに 1 つのスレッドだけ cwd を移す手段がある (Linux の `unshare(CLONE_FS)`、Apple の `pthread_fchdir_np`)

## Decision

| OS | やり方 |
|---|---|
| Linux / Android | 使い捨てのスレッドで `unshare(CLONE_FS)` → `fchdir(dirfd)` → ファイル名で bind / connect。スレッドは呼び出しと一緒に終わる |
| Apple の OS | 使い捨てのスレッドで `pthread_fchdir_np(dirfd)` → ファイル名で bind / connect。スレッドは呼び出しと一緒に終わるので cwd は戻さない |
| NetBSD / OpenBSD / illumos 等 | 無い |

- 順番は symlink の後、fork の前 ([DR-0001](DR-0001-long-socket-path-routes.md))。Linux は /proc の経路が使えない時の順に入る。/proc の経路が使える時は /proc だけを使う ([DR-0003](DR-0003-linux-proc-self-fd.md))
- `bind` / `connect` / `connect_nonblocking` / `connect_any` 系のどれも試す。`connect_any` 系の `bool` は「fork の経路を通ったか」のままで、この経路では `false`
- 呼び出し元のスレッドがソケットを作り (ブロックしない connect ではここで `O_NONBLOCK` にする)、使い捨てのスレッドに渡す。bind の後の listen は呼び出し元のスレッドがする
- ディレクトリの fd は fork の経路と同じもの: Apple は `O_SEARCH` (断られたら `O_RDONLY`)、Linux は /proc の経路のために開いた `O_PATH` の fd
- ファイル名は fork の経路と同じ sockaddr に入れる。標準の `sun_path` に入らない時、Apple でカーネルが広げた sockaddr を受け付ける間は広げた形 (253 バイトまで)。入らなければ `ENAMETOOLONG`
- 使えないとみなして次の経路 (fork、`connect` 系なら `ENAMETOOLONG`) に進むのは次の時
  - スレッドを作れない
  - `unshare` が断られた (どの errno でも。docker の既定の seccomp は `EPERM`)
  - `pthread_fchdir_np` が無い (実行時に `dlsym` で引いて見つからない)、または失敗した
- Linux の `fchdir` の失敗は使えない印にしない。fork の経路の子の `fchdir` と同じく、その errno を本当の失敗として返す
- bind / connect 自体の失敗はそのまま返し、fork の経路でやり直さない ([DR-0012](DR-0012-route-failure-handling.md))
- 試験から使えない時を作るため、この経路を試すかを `route::posix::Config` の値にする ([DR-0011](DR-0011-routes-not-exposed.md))。試験の外では常に試す
- `unsafe` は `src/sys.rs` の `op_in_dir_on_thread` の中だけ。スレッドの起動から bind / connect までを 1 つの関数にし、`unshare` が通る前に `fchdir` へ進む道を作らない

## 理由

- 副作用が無い: peer の pid は呼び出し元のまま (Linux の `SO_PEERCRED`、macOS の `LOCAL_PEERPID` を試験で確認)、fd の複製も無い、並行する他のスレッドの `getcwd` と相対パスの解決は変わらない (試験で確認)
- `connect` / `connect_nonblocking` の届く範囲が、Linux と Apple の OS で `connect_any` と同じ所まで広がる。bind も fork の副作用 (子が一瞬全 fd を持つ) を避けられる
- Apple でも使い捨てのスレッドにするのは、呼び出し元のスレッドで `pthread_fchdir_np` して戻す形だと、呼び出し元がそのスレッドに自分で設定していたスレッドの cwd を `pthread_fchdir_np(-1)` で消してしまい、戻すのに失敗した時はそのスレッドだけ別の cwd に残るから。Linux と同じ形にもそろう
- `pthread_fchdir_np` は公開ヘッダに無いので、リンクせず実行時に引く。将来の libSystem から消えても、読み込めない binary にならず、この経路が使えないだけになる
- `fchdir` は Linux 3.5 以降 `O_PATH` のディレクトリの fd を受け付ける (docker の `seccomp=unconfined` で、uid 1000 が読み取り権限の無い mode 0300 のディレクトリに bind / connect できることを確認)。macOS の `pthread_fchdir_np` も `O_SEARCH` の fd を受け付ける (macOS 26.5 で mode 0300 のディレクトリに bind / connect できることを試験で確認)
- docker の既定の seccomp は `unshare` を `EPERM` で断る (試験 `the_thread_cwd_route_comes_after_symlink_and_before_fork` がその時に fork の経路へ進むことを確かめる)。`seccomp=unconfined` では使える

## 却下した案

| 案 | 却下の理由 |
|---|---|
| Linux だけ足す | Apple の OS でも同じ副作用の無さが得られる。`pthread_fchdir_np` が消えた時は次へ進むだけで壊れない |
| 足さない | `connect` が fork の経路でしか届かない範囲が残る |
| Apple で呼び出し元のスレッドに `pthread_fchdir_np` して `-1` で戻す | 呼び出し元のスレッドの cwd の設定を消しうる。戻せなかった時にそのスレッドが別の cwd に残る |
| スレッドを使い回す (常駐させる) | 呼び出しごとに cwd を移し直す必要が残り、常駐のスレッドとその後始末を抱える。使い捨てなら cwd を戻す必要が無い |

## 未確定

- Android は compile もしていない。`unshare(CLONE_FS)` が Android の seccomp で通るかは確かめていない
- macOS 以外の Apple の OS (iOS 等) に `pthread_fchdir_np` があるかは確かめていない (iOS の clippy だけ)。無ければ次へ進む
- 使い捨てのスレッドを作るコスト (呼び出しごとに 1 本) は測っていない

## 判断の出所

- 経路を足す、順番、どの要求でも試す、使えない時は次へ: kawaz 裁定 (ハブ [DR-0005](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0005-thread-cwd-route.md)、HB-Q4、2026-11-18)
- Apple でも使い捨てのスレッドにする、Linux の `fchdir` の失敗を本当の失敗とする: 実装側の判断 2026-11-18 (ハブの SPEC は「戻すか、スレッドを捨てる」のどちらも認める)
