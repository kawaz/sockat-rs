# DR-0006: 安全な置き場の短い symlink を経由する経路

- Status: Accepted
- Date: 2026-11-18

## Context

一時ディレクトリに短い名前の symlink を作れば、ディレクトリの深さによらず短い綴りでカーネルに渡せる。symlink を作ってから bind / connect するまでに他人が差し替えられると、別の場所にソケットを作らされる。bind と connect で、最後の要素の symlink の辿り方が違う。

## Decision

### 置き場

- 候補は `TMPDIR`、`/tmp` の順
  - `TMPDIR` は空でなく絶対パスの時だけ候補に入れる (外すのは `TMPDIR` だけで、`/tmp` は試す)
  - `TMPDIR` が `/tmp` そのもの (`/tmp/` も含め、綴りが同じ) なら 1 回だけ試す。`/private/tmp` のように綴りの違う同じディレクトリは別の候補として扱う
- 候補を順に見て、最初に使えたもので bind / connect する。何かを作る前に symlink を通した綴りの長さを確かめ、std にそのまま渡せる長さ ([DR-0002](DR-0002-std-handles-paths-it-accepts.md)) 以内なら std に、macOS で広げた sockaddr ([DR-0005](DR-0005-widened-sockaddr-on-macos.md)) が使えるなら 253 以内でそれに渡す。長さが足りない候補には何も作らず次へ進む
- **他人が symlink を差し替えられない置き場だけを使う。** 置き場を stat し (symlink は辿る。macOS の `/tmp` は `/private/tmp` への symlink)、次のどちらかを満たす時だけ使う。満たさない候補には何も作らず次へ進む。例外は /proc を使える Linux / Android の connect 系で、安全でない候補を自分専用のディレクトリを fd で掴む形で使う ([DR-0023](DR-0023-unsafe-base-held-by-fd.md))
  - sticky bit が立っていて、持ち主が root か自分の実効 uid (`/tmp` がこれ)
  - 持ち主が自分の実効 uid で、group と other に書き込み権限が無い (`mode & 0o022 == 0`。macOS の `$TMPDIR` がこれ)
- 確かめるのは置き場そのもので、置き場を含む上位のディレクトリまでは見ない
- symlink を作れない時 (置き場が無い、書けない、サンドボックス、名前を使い切った、乱数が取れない) も次の候補へ進む。広げた sockaddr をカーネルが受け付けなかった時も、以降の候補は std にそのまま渡せる長さで見て続ける。候補が尽きたら fork の経路 ([DR-0007](DR-0007-fork-child-fchdir.md)) に進む
- symlink を作った後の bind / connect の失敗は、その結果を返し、次の候補で試し直さない ([DR-0012](DR-0012-route-failure-handling.md)。試験は後の候補のディレクトリの mtime が変わらないことで、そこに何も作られていないことを確かめる)

### symlink の先と、カーネルに渡す綴り

| 要求 | symlink の先 | 渡す綴り |
|---|---|---|
| bind | ソケットのディレクトリ | `<置き場>/sockat.XXXXXXXXXX/<ファイル名>` |
| connect | ソケットのファイル (元のパス) | `<置き場>/sockat.XXXXXXXXXX` |

- symlink の先は利用者のパスの綴りそのもの (相対パスは cwd を前に付ける)
- `/tmp` で届く範囲: connect は `/tmp/sockat.XXXXXXXXXX` (22 バイト) でディレクトリの深さにもファイル名の長さにもよらない。bind はファイル名が 84 バイト (`sun_path` が 104 の OS は 80、macOS は広げた sockaddr で 230) 以内

### 名前と後始末

- 名前は `sockat.` に乱数 10 文字 (英数 62 種)
- 乱数は OS の安全な乱数から取る: macOS・NetBSD・OpenBSD・DragonFly・illumos は `arc4random_buf(3)`、Linux・Android は `getrandom(2)` を `syscall(2)` で呼ぶ (Linux 3.17 より古いカーネルの `ENOSYS` では symlink の経路は使えないとみなす)、その他の Unix は `/dev/urandom`
- `symlink(2)` の `EEXIST` なら名前を引き直す (16 回まで。使い切ったらその候補は使えないとみなす)。名前の生成を差し替えて、衝突した名前を飛ばすこと、既存のものに触らないこと、回数で打ち切ることを試験する
- bind / connect が成功しても失敗しても、戻る時に symlink を消す (panic でも消す)。消すのに失敗しても無視する
- プロセスが途中で死ぬと symlink が残る。名前が `sockat.` で始まるので sockat の残骸と分かる

## 理由

- 置き場の条件で、symlink を作ってから bind / connect するまでに他人が symlink を差し替えられないことが、仮定でなく確かめた事実になる。sticky なディレクトリの中の名前を消したり rename したりできるのは、その名前の持ち主、ディレクトリの持ち主、root だけで、他人の sticky なディレクトリではその持ち主が差し替えられるので持ち主も見る。残るのは同じ uid か root による差し替えと、他人が書ける上位ディレクトリの下に置き場がある場合で、守る範囲の外
- `TMPDIR` が深すぎる時に `/tmp` で短くできる (`TMPDIR` が std にそのまま渡せる長さを超える環境で効く)。相対パスの `TMPDIR` は cwd に依存するので外す。綴りの違う同じディレクトリを別に扱うのは、綴りが短くなりうるから
- symlink の先が利用者のパスそのものなので、カーネルは元のパスと同じに解決する。socket の実体は元の場所にでき、置き場には何も残らない (試験で実機確認)。ディレクトリを先に引く限界 ([DR-0001](DR-0001-long-socket-path-routes.md)) も無い
- connect は最後の要素の symlink を辿る (macOS・Linux で確認) ので、ファイルを指す綴りがファイル名の長さによらず最短になる
- bind はソケットのファイルを指す形を使えない。Linux は最後の要素の symlink を辿らず `EADDRINUSE` で断る (試験 `linux_binds_not_through_a_link_to_the_file_but_connects_through_it`)
- `symlink(2)` は既存の名前を置き換えずに `EEXIST` で失敗するので、これが symlink の `O_EXCL` になる。C ライブラリの `getrandom` は glibc 2.25・bionic API 28 からなので `syscall(2)` で呼ぶ
- 消すのに失敗しても無視するのは、ソケットは既にできていて、失敗させても symlink は戻らないから
- 名前の接頭辞を `sockat.` にするのは、一時ファイルの掃除に拾わせるための `tmp.` より、持ち主が分かることを取るため

## 却下した案

| 案 | 却下の理由 |
|---|---|
| symlink の置き場の安全を、bind / connect の後に symlink の inode を比べて確かめる | rename で退かして戻す形を見抜けない。置き場の mode と持ち主を先に確かめる |
| connect でも、ソケットのファイルを指す symlink が使えない時にディレクトリを指す symlink を試す | ファイルを指す綴りの方が `/<ファイル名>` の分だけ常に短い。ディレクトリを指す形でだけ届くのは、パス全体が symlink の先の上限 (Linux 4096 バイト) を超える時だけで、そのパスは他のシステムコールでも開けない (macOS は 1024 バイトを超えるとどちらの形も `ENAMETOOLONG`。両方を実機で確認) |
| symlink の経路でソケットのパスが他人に読めることを注意書きする | bind 済みのソケットのパスは、Linux は誰でも読める /proc/net/unix、macOS は `netstat -f unix` で元々誰にでも見える (統括が実機で確認 2026-11-18)。std より後退していない |

## 判断の出所

- 置き場を `TMPDIR`、`/tmp` の順にする: kawaz 裁定 2026-11-18
- 置き場を使う前に安全か確かめる: kawaz 裁定 2026-11-18
- bind はディレクトリを、connect はファイルを指す: kawaz 承認 2026-11-18
- 名前 (`sockat.` の接頭辞): 統括の裁定 2026-11-18。kawaz は接頭辞を支持
- symlink の経路を macOS 以外にも使う: [DR-0008](DR-0008-freebsd-bindat-and-other-unix.md)
- 上に無い細部: 出所の記録なし
