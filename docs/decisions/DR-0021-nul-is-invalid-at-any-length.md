# DR-0021: NUL を含むパスは長さによらず InvalidInput、Linux の先頭の NUL は std にそのまま渡す

- Status: Accepted
- Date: 2026-11-18

## Context

- std にそのまま渡せる長さのパスは std に渡す ([DR-0002](DR-0002-std-handles-paths-it-accepts.md))
- カーネルはソケットのパスを最初の NUL までしか読まない。NUL を含むパスを通すと、利用者の指定と別の場所 (NUL の手前まで) にソケットができうる
- Rust の std (`library/std/src/os/unix/net/addr.rs` の `sockaddr_un`) は、`bytes.contains(&0)` のパスを長さの判定より前に `InvalidInput` ("paths must not contain interior null bytes") で断る。先頭の NUL も断る。Linux の abstract namespace は `Path` でなく `SocketAddr::from_abstract_name` と `bind_addr` / `connect_addr` で扱う
- 長いパスは経路に回るので、NUL の判定を経路に任せると経路が先に動く。例えば B を通す経路は、置き場に B・D・E を作ってから、それを通した bind / connect で `InvalidInput` になる (試験 `paths_without_a_file_name_or_with_nul_are_invalid_input` は置き場に何も作られないことを確かめる)

## Decision

- 先頭以外に NUL を含むパスは、長さによらず `InvalidInput` を返し、何も作らない。長さを見る前、経路を試す前、環境変数を読む前に判定する (`path::refuse_nul`)
- Linux / Android で先頭が NUL のパスは abstract namespace の名前でファイルではないので、sockat の経路の対象外にする。長さによらず std にそのまま渡し、std の答え (今の std では `InvalidInput`) を返す (`path::is_abstract`)。ブロックしない connect は std と同じアドレスで自前のソケットを使う ([DR-0002](DR-0002-std-handles-paths-it-accepts.md)) ので、そのアドレスを組む所で同じく `InvalidInput` になる
- abstract namespace の無い OS (Linux / Android 以外) では、先頭の NUL も他の NUL と同じに、長さによらず `InvalidInput` にする
- std は NUL で切り詰めないので、README と MANUAL の「std と違う点」には書かない。MANUAL の長いパスへの届き方とエラーの表に、NUL の扱いと、Linux の abstract namespace を std の API で扱うことを書く

## 理由

- 利用者の指定と別の場所にソケットができるのを、どの長さでも防ぐ。std は元から NUL を断るので、短いパスの振る舞いは変わらない (種類は同じ `InvalidInput`。文言は sockat のもの)
- 長いパスでも経路より前に断るので、置き場に B・D・E を作らない
- Linux / Android 以外には abstract namespace が無く、先頭の NUL も「パスの中の NUL」と同じ

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 短いパスは NUL を含んでも std にそのまま渡す | 標準が NUL で切り詰める言語では別の場所にソケットができる (ハブ DR-0006 の却下)。Rust では std が断るので結果は同じだが、長さで扱いを分ける理由が無い |
| 長いパスの NUL は経路に任せる | 経路が D や E を作ってから断られる。どの経路の誰が断るかで文言も変わる |

## 判断の出所

- 先頭以外の NUL は長さによらず引数の誤り、Linux / Android の先頭の NUL は対象外で標準に渡す: kawaz 裁定 (ハブ [DR-0006](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0006-nul-is-invalid-at-any-length.md)、HB-Q5、2026-11-18)
- abstract namespace の無い OS で先頭の NUL も長さによらず `InvalidInput` にする: ハブの SPEC (統括 2026-11-18)
