# DR-0012: 経路の本当の失敗は次の経路でやり直さない。どの経路も使えなければ ENAMETOOLONG、ディレクトリが無ければ ENOENT

- Status: Accepted
- Date: 2026-11-18

## Context

経路 ([DR-0032](DR-0032-resolve-through-persistent-base.md)) を順に試す時、ある経路のエラーが「その経路が使えない」なのか「bind / connect が本当に失敗した」なのかで、次へ進むかが変わる。どの経路も使えない時と、ディレクトリが無い時に、何を返すかも決める。

## Decision

- 経路が実際に bind / connect した結果のエラー (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) はそこで返し、次の経路を試さない。link-dir・link-file で bind して失敗した時は、作った E を消してから返す
- 次へ進むのは、その経路自体が使えないと分かった時だけ
  - 綴りが `sun_path` に収まらない
  - B・D・E を作れない (置き場の確かめに外れた、作れない、名前を使い切った、乱数が取れない)
  - スレッド cwd が使えない: スレッドを作れない、seccomp の filter が掛かっている (Linux)、`unshare` が断られた、`pthread_fchdir_np` が無い・失敗した
  - `/proc` を使えない
  - ソケットのディレクトリの fd が要る経路 (proc-direct、at-direct、thread-cwd-direct) で、そのディレクトリを開けない
  - 広げた sockaddr をカーネルが受け付けない ([DR-0005](DR-0005-widened-sockaddr-on-macos.md))
  - 相対パスの経路で、作った綴りのディレクトリが元と `(st_dev, st_ino)` で一致しない、または cwd が既に変わっている ([DR-0004](DR-0004-relative-path-from-cwd.md))
  - link-file の bind が失敗し、bind が symlink を辿るか分からない OS で元のパスに物が無い (カーネルが辿らなかったとみなす。E は消す)
- どの経路も使えなければ `ENAMETOOLONG`。専用のエラーや文言は作らない
- `Options.base` の確かめに外れた時は、経路を試す前に `ENOENT` / `ENOTDIR` / `EPERM` (自動の置き場に落とさない)
- ディレクトリが無い時は、どの OS でも、ファイル名の長さによらず `ENOENT` を返す。最初にソケットのディレクトリを `stat` で引き、その後でファイル名が収まるかを判定する。ディレクトリでない物があれば `ENOTDIR`。続けて realpath を取り、引けなければそのエラーを返す

試験: `a_missing_directory_is_enoent_before_any_route`、`a_failed_bind_through_link_dir_removes_its_e_and_is_not_retried`、`a_failed_link_file_bind_is_the_failure`、`link_file_told_at_run_time_is_passed_over_when_not_followed`、`a_connect_failure_is_not_retried`、`options_base_that_fails_its_check_fails_before_any_route`。

## 理由

- 本当の失敗を次の経路でやり直すと、同じエラーを遅れて返すか、別の場所にソケットを作る
- `ENAMETOOLONG` は、パスが `sun_path` に入らない時の普通の bind / connect と同じエラー
- ディレクトリが無いのに「パスが長すぎる」と読めるエラーを返さない。無いディレクトリは、その本当のエラーで返す

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 経路の失敗を次の経路で試し直す | 同じエラーを遅れて返すか、別の場所にソケットを作る |
| ソケットのディレクトリを最初に開き、開けなければそのエラーを返す | `O_RDONLY` で開く OS では読み取り権限の無いディレクトリを開けず、B を通す経路なら届くパスを断る。開くのは fd が要る経路だけにする |

## 判断の出所

- ディレクトリが無い時はどの OS でも `ENOENT` にそろえる: 統括の裁定 2026-11-18
- ソケットのディレクトリは `stat` で引き、fd が要る経路だけが開く: ハブの SPEC (2026-11-19)
- link-dir・link-file の bind の失敗で E を消す、`Options.base` の確かめは経路の前: ハブの SPEC (2026-11-21)
- 本当の失敗を次の経路でやり直さない: 出所の記録なし
