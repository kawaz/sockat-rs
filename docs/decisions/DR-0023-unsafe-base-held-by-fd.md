# DR-0023: /proc を使える Linux の connect は、安全でない置き場に自分専用のディレクトリを作り fd で掴んで symlink の経路に使う

- Status: Accepted
- Date: 2026-11-18

## Context

- Linux で seccomp の filter が掛かり (スレッド cwd の経路が使えない、[DR-0020](DR-0020-thread-cwd-route.md))、`TMPDIR` も `/tmp` も安全な置き場でなく ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md))、ファイル名が `/proc` の綴りに収まらない時 ([DR-0003](DR-0003-linux-proc-self-fd.md))、bind は fork で届くが `connect` / `connect_nonblocking` は `ENAMETOOLONG` になる (hyoui の実測)
- 安全でない置き場に名前で辿る symlink を置くと、他人がそれを差し替えられる

## Decision

`/proc` を使える Linux / Android の connect 系 (`connect`、`connect_nonblocking`、`connect_any` 系) では、symlink の経路で安全でない置き場の候補を飛ばさず、fd で掴む形で使う。

| 置き場の候補 | 形 |
|---|---|
| 安全 | 置き場に直接 symlink ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md)) |
| 安全でない (`/proc` を使える Linux / Android の connect 系) | fd で掴む形 |
| 安全でない (それ以外) | 何も作らず次の候補へ |

- 試す順: `TMPDIR` が安全なら symlink → 安全でなければ fd で掴む形 → `/tmp` が安全なら symlink → 安全でなければ fd で掴む形 → スレッド cwd → fork (bind と `connect_any` 系だけ)。`TMPDIR` が `/tmp` と同じなら 1 回だけ
- fd で掴む形 (`route::posix::HeldDir`)
  0. 何かを作る前に `/proc` を使えるか確かめ、使えなければ何も作らずに次へ (下の判定)
  1. 置き場に mode 0700 のディレクトリを作る。名前と衝突の扱いは symlink と同じ (`sockat.` + 乱数 10 文字、`EEXIST` で引き直し 16 回で打ち切り)
  2. `O_PATH | O_DIRECTORY | O_NOFOLLOW` で開き (ディレクトリに権限が無くても開ける)、`fstat` で「ディレクトリ・持ち主が実効 uid・group と other に権限が無い (`mode & 0o077 == 0`)」を確かめる。通ったら `chmod("/proc/self/fd/<fd>", 0700)` で持ち主の権限をそろえる (`O_PATH` の fd には `fchmod` できない。/proc の場所は Config の値)
  3. fd の中に `symlinkat` で、ソケットのファイル (元のパス。相対パスなら cwd を前に付ける) を指す `s` を作る
  4. 綴り `/proc/self/fd/<fd>/s` を渡す
  5. 片付け: `s` は自分の `symlinkat` が作った時だけ `unlinkat(fd, "s")` で消す。ディレクトリは、手順 2 の確認が通っていて、かつその名前を `lstat` した `(st_dev, st_ino)` が fd と一致する時だけ `rmdir`。fd を閉じる。connect が成功しても失敗しても、panic でもする。片付けの失敗は無視する
- `/proc` を使えるかは、ソケットのディレクトリの fd で `/proc/self/fd/<fd>` を引けるかで決める (/proc の経路と同じ判定で、経路を選ぶ前に 1 度)。ハブの SPEC は置き場の fd で引くと書くが、同じプロセスの同じ `/proc` を見るので結果は変わらない。使えない時は安全でない置き場に何も作らない (試験 `without_proc_a_place_that_is_not_safe_is_passed_over` が置き場の mtime で確かめる)
- bind は使わない。bind の綴り (`/proc/self/fd/<fd>/s/<ファイル名>`) は /proc の経路の綴り (`/proc/self/fd/<fd>/<ファイル名>`) より `s/` の分長く、それが収まるなら /proc の経路が先に通る。2 つの fd の桁数が違う時だけ例外がありうるが、そのためには足さない
- 使えないとみなして次の候補へ: ディレクトリを作れない、開けない、`fstat` の確認が通らない、`symlinkat` が失敗した、綴りが標準の `sun_path` に収まらない
- 確認が通らなかったディレクトリ (他人が置いた物) は、中身も含めて使わず、消さない (中に `s` があっても消さない)。この 2 つの状態は型と値で持つ: `HeldDir` の値は確認が通った時だけ作り、`s` を作ったかは値の `linked` で持つ。Drop はその値の分だけ片付ける。`symlinkat` が失敗した時と綴りが収まらない時は、作ったディレクトリを片付けてから次へ
- connect した結果の失敗はそのまま返し、次の候補・経路で試さない ([DR-0012](DR-0012-route-failure-handling.md))
- `symlinkat` / `unlinkat` は `src/sys.rs` に置く

## 理由

- 名前でなく fd で辿るので、sticky でない置き場で他人がディレクトリを rename して差し替えても影響を受けない (試験 `a_held_directory_renamed_away_is_still_used_and_its_replacement_is_not_removed`: rename した後も fd の綴りで届き、名前に置かれた別のディレクトリは消さない)
- 作ってから開くまでのすり替えは `O_NOFOLLOW` と `fstat` の確認で弾き、すり替えた物の中身にも触らない (試験 `a_replaced_directory_is_left_with_its_contents`: 他人の `s` と別のファイルの入ったディレクトリに差し替えても、中身がそのまま残る。確認の前に値を作り `s` を必ず消す形に壊すと落ちる) (試験 `a_directory_replaced_before_it_is_held_is_not_used`: mode の違うディレクトリ、別のディレクトリへの symlink、ふつうのファイル、root で他人に chown したディレクトリ)
- connect の綴りはファイル名の長さにも置き場の深さにもよらず収まる (`/proc/self/fd/<fd>/s` はおよそ 20 バイト)
- 渡された `TMPDIR` で済むならそれが一番良いので、`TMPDIR` を `/tmp` より先に使い切る
- group と other の権限だけを見て、持ち主の権限を後からそろえるのは、umask が持ち主のビットを削る環境 (umask 0277 なら作ったディレクトリは 0500、0477 なら読み取りも無い 0300) で毎回確認に落ちて残骸が溜まらないため。umask は group と other のビットを足せないので、そこに権限があれば作った物ではない。setuid / setgid / sticky も見ないので、親の setgid のように作る側が決められない bit で使えなくならない
- `O_PATH` で開くのは、読み取り権限の無いディレクトリ (umask 0477) も開けるため。`O_PATH` の fd には `fchmod` できないので `/proc/self/fd/<fd>` を `chmod` する。この綴りは fd が掴んだディレクトリそのものに効くので、名前で触る時の差し替えの心配が無い。`fstat`、`symlinkat`、`unlinkat` は `O_PATH` の fd で動く (Linux 7.2 の docker で、片付けと rename の試験が通ることで確認)
- 試験 `a_umask_that_takes_the_owners_bits_does_not_stop_the_held_directory` は umask 0277 と 0477 の子プロセスで届き片付くことを確かめる。root は権限を無視するので、効くのは他の uid で走らせた時 (docker で `setpriv --reuid=1000`)。そこで `chmod` を外すと、また `O_PATH` を外すと落ちることを確かめた

悪い面:

- `/proc` が要る。`/proc` の無い Linux と他の Unix では使えない。Apple の OS はスレッド cwd の経路で届くので足さない
- 途中でプロセスが死ぬとディレクトリが残る (名前の `sockat.` で残骸と分かる)
- 作った後に開けなかった時 (fd が尽きた等) は、名前で辿るディレクトリが自分の物か確かめられないので消さずに残る

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 何もしない | bind は届くのに connect は届かないソケットができる |
| 安全でない置き場に名前で辿るディレクトリや symlink を置く | sticky でない置き場ですり替えられる |
| bind にも使う | bind の綴りは /proc の経路の綴りより長く、/proc の経路が長さで通らない時はほぼ収まらない |

## 未確定

- Android は compile もしていない
- 持ち主の違うディレクトリへのすり替えは root (docker) でだけ確かめた。uid 1000 では他人に chown できないので、その場合は飛ばす (他の観点は uid 1000 でも通ることを確かめた)

## 判断の出所

- 安全でない置き場を fd で掴む形で使う、順番、connect 系だけ、片付けの条件: kawaz 裁定 (ハブ [DR-0008](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0008-unsafe-base-held-by-fd.md)、2026-11-18)。bind に効かないこと: 統括の裁定 2026-11-18
- 何かを作る前に `/proc` を確かめる、group と other の権限で判定し 0700 にそろえる、`O_PATH` で開き `/proc/self/fd/<fd>` を `chmod` する: ハブの SPEC (統括の裁定 2026-11-18、sockat-go と sockat-rs の指摘)
- 自分が作った `s` だけ消し、確認が通ったディレクトリだけ `rmdir` する (確認に落ちた物は中身も含めて触らない): ハブの SPEC (sockat-py の指摘、2026-11-18)
- 開けなかったディレクトリを消さない: 実装側の判断 2026-11-18
