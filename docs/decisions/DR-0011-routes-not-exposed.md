# DR-0011: どの経路を通ったかは診断の名前だけで返し、試験は crate 内で各経路を強制する

- Status: Accepted
- Date: 2026-11-18

## Context

経路は OS と環境 (`/proc` の有無、seccomp の filter、置き場に書けるか、カーネルが広げた sockaddr を受け付けるか) で決まる ([DR-0032](DR-0032-resolve-through-persistent-base.md))。試験では各経路を確実に通したいが、そのための口を公開 API に出すと利用者の面が増える。

## Decision

- 経路は crate 内の値で、公開 API に出すのは `_ex` と resolve が返す診断 (`route`・`base`・`skipped` の名前) と、綴りの性質 `Spelling` だけ。経路を選ぶ・強制する口は作らない
- 試験は crate 内の単体試験 (`src/tests.rs`) に置き、経路の順番 (`route::run`・`route::resolve`) に環境を差し替えた入力を渡して強制する
- 環境を読む部分は値 (`route::Config`) にして試験から差し替える
  - B の置き場 (`places`。試験ごとの一時ディレクトリにして、置き場の確かめに外れる場合や作れない場合も作る)
  - `/proc` の場所 (`proc`。何も無い場所を渡して `/proc` の無い環境の代わりにし、`<それ>/<pid>/fd/<B の fd>` に B への symlink を置いて procB の綴りをどの OS でも通す)
  - スレッド cwd を試すか (`thread_cwd`。seccomp の filter 等で使えない環境の代わり)
  - Apple の OS で広げた sockaddr を試すか (`wide`)
  - bind が symlink を辿るか (`follows`。辿らない OS で実行時の見分けを通す)
  - ソケットのディレクトリの開き方 (`open_socket_dir`。開けないディレクトリの代わり)
  - 相対パスの bind で cwd を引き直す間 (`between_lookups`。他のスレッドの `chdir` の代わり)
- プロセス全体に効く設定 (cwd、umask) と、終わったプロセスの D が要る試験は、試験のバイナリを子プロセスで起こして中で行う
- feature も cfg(test) 専用の公開口も作らない。ハブの適合試験の CLI も公開の `_ex` を使う ([DR-0030](DR-0030-conformance-cli.md))

## 理由

- 利用者が分岐に使うのは綴りを後で使えるか (`Spelling`) で、経路の名前は診断でしかない
- 試験は crate 内で足りる

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 経路を公開 API で返す / 強制する feature を作る | 利用者が知る必要が無い。試験は crate 内で足りる |

## 判断の出所

- 実装側の判断 2026-11-18
