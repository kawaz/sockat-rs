# DR-0012: 経路の本当の失敗は次の経路でやり直さない。fork しない connect は ENAMETOOLONG、ディレクトリが無ければ ENOENT

- Status: Accepted
- Date: 2026-11-18

## Context

経路 ([DR-0027](DR-0027-own-directory-and-fallback-routes.md)) を順に試す時、ある経路のエラーが「その経路が使えない」なのか「bind / connect が本当に失敗した」なのかで、次へ進むかが変わる。fork を試さない `connect` / `connect_nonblocking` ([DR-0010](DR-0010-public-api.md)) が fork の手前まで来た時と、ディレクトリが無い時に、何を返すかも決める。

## Decision

- 経路が実際に bind / connect した結果のエラー (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) はそこで返し、次の手段も、次の置き場も、次の経路も試さない
- 次へ進むのは、その経路自体が使えないと分かった時だけ
  - 長さが足りない
  - 中心の経路で D の置き場に D か R を作れない (置き場を開けない、D を作れない、名前を使い切った、乱数が取れない、確認が通らない、0700 にそろえられない、`symlinkat` が失敗した。次の置き場へ)
  - 中心の経路の手段が使えない: スレッドを作れない、seccomp の filter が掛かっている (Linux)、`unshare` が断られた、`pthread_fchdir_np` が無い・失敗した、`/proc/self/fd/<fd>` を引けない
  - ソケットのディレクトリの fd が要る経路 (/proc、connectat、fork) で、そのディレクトリを開けない
  - /proc を使えない
  - D の中のファイルへの symlink (/proc を使える Linux の connect 系) で、D か `s` を作れない
  - 広げた sockaddr をカーネルが受け付けない ([DR-0005](DR-0005-widened-sockaddr-on-macos.md))
  - symlink の置き場が安全でない・作れない ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md))
  - 相対パスの経路で、作った綴りのディレクトリが元と `(st_dev, st_ino)` で一致しない、または cwd が既に変わっている ([DR-0004](DR-0004-relative-path-from-cwd.md))
- **fork を試さない connect** (`connect` / `connect_nonblocking`) は、予備の経路の後、fork の経路の手前で `ENAMETOOLONG` を返す。ファイルシステムに何も作らず、何も connect しない。専用のエラーや文言は作らない。中心の経路や予備の経路で実際に connect した失敗 (`ENOENT`、`ECONNREFUSED` 等) は、`ENAMETOOLONG` に変えずにそのまま返す
- ディレクトリが無い時は、どの OS でも、ファイル名の長さによらず `ENOENT` を返す。最初にソケットのディレクトリを `stat` で引き、その後でファイル名が収まるかを判定する。ディレクトリでない物があれば `ENOTDIR`。続けて realpath を取り、引けなければそのエラーを返す ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))

試験: `a_missing_directory_is_enoent_even_when_the_name_fits_no_sockaddr` (254 バイトのファイル名、bind と各 connect、置き場なしで最後の経路まで進める場合も)、`a_failure_in_the_central_route_is_not_retried` (中心の経路の失敗をそのまま返し、後の置き場と symlink の置き場に何も作らない)、`the_thread_cwd_means_comes_first_and_its_failures_are_final`、`a_failure_through_the_symlink_is_reported_and_not_retried`。

## 理由

- 本当の失敗を次の経路でやり直すと、同じエラーを遅れて返すか、別の場所にソケットを作る
- `ENAMETOOLONG` は、パスが `sun_path` に入らない時の普通の connect と同じエラー。fork を使わないなら普通の connect と同じ立場なので、同じエラーにする
- ディレクトリが無いのに「パスが長すぎる」と読めるエラーを返さない。無いディレクトリは、その本当のエラーで返す

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 経路の失敗を次の経路で試し直す | 同じエラーを遅れて返すか、別の場所にソケットを作る |
| fork でしか届かない connect に専用のエラーや文言を作る | fork を使わない connect は普通の connect と同じ立場で、`ENAMETOOLONG` で足りる |
| ソケットのディレクトリを最初に開き、開けなければそのエラーを返す | `O_RDONLY` で開く OS では読み取り権限の無いディレクトリを開けず、中心の経路なら届くパスを断る。開くのは fd が要る経路だけにする |

## 判断の出所

- fork を試さない connect は `ENAMETOOLONG`、専用のエラーは作らない: kawaz 裁定 2026-11-18
- ディレクトリが無い時はどの OS でも `ENOENT` にそろえる: 統括の裁定 2026-11-18
- ソケットのディレクトリは `stat` で引き、fd が要る経路だけが開く: ハブの SPEC (2026-11-19)
- 本当の失敗を次の経路でやり直さない: 出所の記録なし
