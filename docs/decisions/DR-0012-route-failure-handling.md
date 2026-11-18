# DR-0012: 経路の本当の失敗は次の経路でやり直さない。fork しない connect は ENAMETOOLONG、ディレクトリが無ければ ENOENT

- Status: Accepted
- Date: 2026-11-18

## Context

経路 ([DR-0001](DR-0001-long-socket-path-routes.md)) を順に試す時、ある経路のエラーが「その経路が使えない」なのか「bind / connect が本当に失敗した」なのかで、次へ進むかが変わる。fork を試さない `connect` / `connect_nonblocking` ([DR-0010](DR-0010-public-api.md)) が fork の手前まで来た時と、ディレクトリが無い時に、何を返すかも決める。

## Decision

- 経路が実際に bind / connect した結果のエラー (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) はそこで返し、次の経路を試さない。symlink の置き場の候補も同じで、ある置き場を通した bind / connect が失敗したら、次の置き場では試さない
- 次へ進むのは、その経路自体が使えないと分かった時だけ
  - /proc を使えない ([DR-0003](DR-0003-linux-proc-self-fd.md))
  - 広げた sockaddr をカーネルが受け付けない ([DR-0005](DR-0005-widened-sockaddr-on-macos.md))
  - symlink の置き場が安全でない・作れない ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md))
  - 長さが足りない
- **fork を試さない connect** (`connect` / `connect_nonblocking`) は、fork の経路の手前で `ENAMETOOLONG` を返す。ソケットは作らず、何も connect しない。専用のエラーや文言は作らない
- ディレクトリが無い時は、どの OS でも、ファイル名の長さによらず `ENOENT` を返す。ディレクトリを先に開いてから、ファイル名が収まるかを判定する

未実装: Linux 以外は fork の経路 (`route::posix::run`) と FreeBSD の `route::freebsd::run_long` がファイル名の sockaddr をディレクトリを開く前に組み立てるので、ディレクトリが無くファイル名が長すぎると `ENAMETOOLONG` になる。

## 理由

- 本当の失敗を次の経路でやり直すと、同じエラーを遅れて返すか、別の場所にソケットを作る
- `ENAMETOOLONG` は、パスが `sun_path` に入らない時の普通の connect と同じエラー。fork を使わないなら普通の connect と同じ立場なので、同じエラーにする
- ディレクトリが無いのに「パスが長すぎる」と読めるエラーを返さない。無いディレクトリ・開けないディレクトリは、その本当のエラーで返す

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 経路の失敗を次の経路で試し直す | 同じエラーを遅れて返すか、別の場所にソケットを作る |
| fork でしか届かない connect に専用のエラーや文言を作る | fork を使わない connect は普通の connect と同じ立場で、`ENAMETOOLONG` で足りる |

## 判断の出所

- fork を試さない connect は `ENAMETOOLONG`、専用のエラーは作らない: kawaz 裁定 2026-11-18
- ディレクトリが無い時はどの OS でも `ENOENT` にそろえる: 統括の裁定 2026-11-18
- 本当の失敗を次の経路でやり直さない: 出所の記録なし
