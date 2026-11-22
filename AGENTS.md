# AGENTS.md — sockat の作業ルール

sockat は、`sun_path` の上限を超えるパスでも Unix domain socket を bind / connect できる Rust のライブラリ (公開 crate)。短いパスは std にそのまま渡し、長いパスは置き場 B・プロセスの D・symlink E を通る短い綴りにして渡す。言語によらない振る舞いはハブ [kawaz/sockat](https://github.com/kawaz/sockat) の SPEC が正本。

## 必読

- ハブの [DR-0017](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0017-resolve-through-persistent-base.md)・[SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md)・[SPEC-routes](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md) — 経路と試す順、B・D・E の作り方、綴りの性質、エラー。経路に手を入れる前に読む
- [docs/decisions/DR-0032](docs/decisions/DR-0032-resolve-through-persistent-base.md) — それを Rust で作る形 (公開 API、B・D・E の持ち方、OS ごとの試す順)
- [docs/MANUAL-ja.md](docs/MANUAL-ja.md) — 利用者に約束している振る舞い (std と違う点、片付け、エラー)

## 構成

| パス | 中身 |
|---|---|
| `src/lib.rs` | 公開 API (stream の関数、`Options`・`Spelling`・`Bound`・`SocketFile`・resolve・`sweep`) と crate doc |
| `src/datagram.rs` | `SOCK_DGRAM` の公開 API |
| `src/route.rs` | 経路の順番 (OS ごと) と各経路、resolve |
| `src/base.rs` | 置き場 B・D・E (作り方、確かめ、プロセスの登録簿、`sweep`) |
| `src/path.rs` | パスの分割・相対パスの計算 (ファイルシステムに触らない) |
| `src/sys.rs` | `unsafe` の境界。libc を直接呼ぶのはここだけ |
| `src/tests.rs` | 経路を強制する単体試験 |
| `tests/` | 公開 API の結合試験、tokio との連携試験 |
| `examples/conformance.rs` | ハブの適合試験の CLI (DR-0030) |
| `src/cli_internal.rs` | コマンド専用の入口 `sockat::__cli` (feature `__cli-internal` の後ろ、doc に出さない、API でない。DR-0033) |
| `cli/` | コマンド `sockat` のパッケージ sockat-cli (`src/main.rs` 引数と出力、`src/resolve.rs` 綴りの作り方、`tests/cli.rs` 結合試験)。未公開 |

## 守ること

- **`unsafe` は `src/sys.rs` にだけ書く。** `just lint-unsafe` が他のファイルの `unsafe` を落とす。`sys.rs` の関数は安全に呼べる形で公開し、各 `unsafe` に `SAFETY:` を添える
- **std が受け付ける絶対パス (`sun_path` − 1 バイト以内) には何も足さない。** std の関数をそのまま呼び、環境変数も読まない (DR-0002)。bind 系と resolve の相対パスは絶対パスにしてから扱う (ハブ DR-0019、DR-0032)
- **経路の本当の失敗は次の経路でやり直さない。** 次へ進むのは経路自体が使えない時だけ。bind に失敗したら作った E を消してから返す (DR-0012)。既定の bind の link-dir の E だけは、失敗しても消さない (同じディレクトリの既定の bind が使い回す。DR-0032)
- **fork しない。** どの経路も呼び出し元のプロセスの cwd も呼び出し元のスレッドの cwd も変えない
- **経路は公開 API に出さない。** 返すのは診断の名前と `Spelling` だけ。試験は crate 内で各経路を強制する (DR-0011)
- **B と D は sockat が消さない。** 消すのは `sweep` / `sweep_base` (持ち主の死んだ D だけ)。E は `cleanup` / `remove`
- 公開 API・doc・エラーの文言は英語。DR と日本語の docs は日本語

## 試験

- `just test` (macOS)、`just test-linux` (docker、既定の seccomp)、`just test-linux-unconfined` (スレッド cwd を通す)、`just test-linux-user` (uid 1000)。経路は OS と環境で違うので、変更したら全部回す
- 一時ディレクトリはランダム名で作り、試験の終わりに消す (`tempfile`)。長さを決めたいので、基本は `/tmp` の下に作る。B の置き場は試験ごとの一時ディレクトリにする (`route::Config` の `places`。コマンドの試験は debug build のコマンドが読む `SOCKAT_CLI_TEST_TMP`)
- cwd や umask のようにプロセス全体に効く設定が要る試験と、終わったプロセスの D が要る試験は、試験バイナリを子プロセスで起こして中で行う (`run_child`)。試験は並行して走るので、同じプロセスで変えない
- 各試験の直前に「何を確かめ、なぜそうなるべきか」をコメントで書く。DR の番号だけで済ませない
- ハブの適合試験 (`just conformance` をハブで) も流す。手元と `--on linux`

## VCS と push

- jj colocate。commit は `jj commit -m "..." <files...>` でパスを指定する
- push は `just push` だけ (lint・試験・翻訳ペアの鮮度・version の上げ忘れを検査してから push する)
- README と docs/MANUAL は日本語版 (`*-ja.md`) が正本。英語版は日本語版を直した同じ変更で直す
- `src/` を変えたら `just bump-version` で version を上げる
