# AGENTS.md — sockat の作業ルール

sockat は、`sun_path` の上限を超えるパスでも Unix domain socket を bind / connect できる Rust のライブラリ (公開 crate)。短いパスは std にそのまま渡し、長いパスだけを OS ごとの経路で短い綴りにして渡す。

言語によらない振る舞いの正本はハブの [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) (kawaz/sockat) で、sockat-rs はその Rust 実装。

## 必読

- [docs/decisions/DR-0001](docs/decisions/DR-0001-long-socket-path-routes.md) — 経路と試す順番の総論。各経路の安全性の考え方・却下した案・未確定の点は、そこからリンクした経路ごとの DR にある。経路に手を入れる前に、総論と触る経路の DR を読む
- [docs/decisions/INDEX.md](docs/decisions/INDEX.md) — 設計判断 (DR) の一覧
- [docs/MANUAL-ja.md](docs/MANUAL-ja.md) — 利用者に約束している振る舞い (std と違う点、エラー)

## 構成

| パス | 中身 |
|---|---|
| `src/lib.rs` | 公開 API (`bind` / `connect`) と crate doc |
| `src/route.rs` | 経路の順番 (OS ごとの `run`) と各経路 |
| `src/path.rs` | パスの分割・相対パスの計算 (ファイルシステムに触らない) |
| `src/sys.rs` | `unsafe` の境界。libc を直接呼ぶのはここだけ |
| `src/tests.rs` | 経路を強制する単体試験 |
| `tests/` | 公開 API の結合試験、tokio との連携試験 |

## 守ること

- **振る舞いを変える時は、先にハブの SPEC を直す。** sockat-rs の docs と実装はその後に追従させる
- **`unsafe` は `src/sys.rs` にだけ書く。** `just lint-unsafe` が他のファイルの `unsafe` を落とす。`sys.rs` の関数は安全に呼べる形で公開し、各 `unsafe` に `SAFETY:` を添える
- **std が受け付けるパス (OS ごとの `sun_path` − 1 バイト以内) には何も足さない。** std の関数をそのまま呼ぶ ([DR-0002](docs/decisions/DR-0002-std-handles-paths-it-accepts.md))
- **経路の本当の失敗は次の経路でやり直さない。** 次へ進むのは経路自体が使えない時だけ ([DR-0012](docs/decisions/DR-0012-route-failure-handling.md))
- **fork した子の区間は async-signal-safe な呼び出しだけ。** allocation・lock・Rust の destructor を通さない。sockaddr は fork の前に親が組み立てる
- **経路は公開 API に出さない。** 試験は crate 内で各経路の関数を呼ぶ ([DR-0011](docs/decisions/DR-0011-routes-not-exposed.md))
- 公開 API・doc・エラーの文言は英語。DR と日本語の docs は日本語

## 試験

- `just test` (macOS)、`just test-linux` (docker で Linux)。経路は OS で違うので、変更したら両方で回す
- 一時ディレクトリはランダム名で作り、試験の終わりに消す (`tempfile`)。長さを決めたいので、基本は `/tmp` の下に作る
- cwd やシグナルの扱いのようにプロセス全体に効く設定が要る試験は、試験バイナリを子プロセスで起こして中で行う (`run_child`)。試験は並行して走るので、同じプロセスで変えない
- fork の経路の子はプロセスの全 fd の複製を一瞬持つ。並行する試験の fork が、閉じたはずの listener を生かすことがある。「誰も listen していないソケット」が要る試験は、別プロセスで作る (`stale_socket`)
- 各試験の直前に「何を確かめ、なぜそうなるべきか」をコメントで書く。DR の番号だけで済ませない

## VCS と push

- jj colocate。commit は `jj commit -m "..." <files...>` でパスを指定する
- push は `just push` だけ (lint・試験・翻訳ペアの鮮度・version の上げ忘れを検査してから push する)
- README と docs/MANUAL は日本語版 (`*-ja.md`) が正本。英語版は日本語版を直した同じ変更で直す
- `src/` を変えたら `just bump-version` で version を上げる
