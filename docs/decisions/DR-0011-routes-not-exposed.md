# DR-0011: どの経路を通ったかは利用者に見せず、試験は crate 内で各経路を強制する

- Status: Accepted
- Date: 2026-11-18

## Context

経路は OS と環境 (/proc の有無、seccomp の filter、`TMPDIR`、置き場に書けるか、カーネルが広げた sockaddr を受け付けるか) で決まる ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))。試験では各経路を確実に通したいが、そのための口を公開 API に出すと利用者の面が増える。

## Decision

- 経路 (`Route`) は crate 内の値で、公開 API は捨てる。`connect_any` 系が返すのは経路の名前でなく、fork の経路を通ったか (= サーバに見える peer の pid が終了したプロセスのものになったか) だけ ([DR-0010](DR-0010-public-api.md))
- 試験は crate 内の単体試験 (`src/tests.rs`) に置き、経路の順番 (`route::run_long`) に環境を差し替えた入力を渡すか、手段の関数 (`route::thread_cwd_op`、`route::fork_op`、`route::own::OwnDir` 等) を直接呼んで強制する
- 環境を読む部分は値 (`route::Config`) にして試験から差し替える
  - D の置き場 (`own_dir_places`。置き場の無い・作れない場合を作る)
  - symlink の置き場 (`link_places`)
  - スレッド cwd を試すか (`thread_cwd`。seccomp の filter 等で使えない環境の代わり)
  - Apple の OS で広げた sockaddr を受け付けないカーネル (`wide`)
  - Linux の /proc の場所 (`proc_self_fd`。試験は何も無い場所や空のディレクトリを渡して /proc の無い環境の代わりにし、本物の /proc には触らない)
- プロセス全体に効く設定 (cwd、umask、`SIGCHLD`) を変える試験は、試験のバイナリを子プロセスで起こして中で行う
- feature も cfg(test) 専用の公開口も作らない。例外はハブの適合試験の CLI の口だけで、cfg フラグ `sockat_conformance` の後ろに隠す ([DR-0030](DR-0030-conformance-cli.md))

## 理由

- 利用者が知る必要が無い。利用者に関係するのは fork の副作用が乗ったかだけで、それは `AnyConnection::forked` で返す
- 試験は crate 内で足りる

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 経路を公開 API で返す / 強制する feature を作る | 利用者が知る必要が無い。試験は crate 内で足りる |

## 判断の出所

- 実装側の判断 2026-11-18
