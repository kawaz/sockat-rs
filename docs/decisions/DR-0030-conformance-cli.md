# DR-0030: 適合試験の CLI は example に置き、公開の `bind_ex` / `connect_ex` を最終手段を使わずに呼んで経路の情報を出す

- Status: Accepted
- Date: 2026-11-19

## Context

- ハブの適合試験 ([ハブの docs/design/conformance.md](https://github.com/kawaz/sockat/blob/main/docs/design/conformance.md)) は、各言語の試験用 CLI を場面ごとに起こし、標準入力の JSON で 1 回 bind / connect させ、通った経路・飛ばした経路・失うもの・errno を標準出力の JSON で受け取る。cwd・環境変数・umask は場面ごとにハブのスクリプトが整える
- CLI が試すのは既定の bind の経路の列と既定の connect の経路の列 (最終手段を使わない `_ex` と同じ。conformance.md)
- 公開の `bind_ex` / `connect_ex` は `LastResort::Avoid` で既定の関数と同じ列を試し、`losses`・`route`・`skipped` を返す ([DR-0031](DR-0031-ex-functions.md))
- CLI は公開しない (conformance.md)

## Decision

- `examples/conformance.rs`。`sockat::bind_ex(path, LastResort::Avoid)` と `sockat::connect_ex(path, LastResort::Avoid)` (ブロックする) を呼ぶ。crate の中に CLI 専用の口は置かない
- 入力の JSON は dev-dependency の `serde_json` で読む。出力は項目を conformance.md の順に並べて組み立て、文字列は `serde_json` でエスケープする
- `error`: errno のあるエラーは POSIX の errno の表で引いた名前 (同じ値の名前が 2 つある時は表の先の方。`EAGAIN`、`EOPNOTSUPP`、表に無い errno は `errno-<番号>`)。errno の無いエラーは `InvalidInput` なら `invalid-argument`、他は `general`
- bind が通ったら listener を閉じる (ソケットのファイルは消さない。`SocketFile` は使わない)。connect が通ったら `x` を 1 バイト送って閉じる
- 終了コード: 結果を出したら 0 (bind / connect の成否によらず、connect の後に `x` を送れなかった時も。届いたかはハブのスクリプトが見る)。標準入力を読めない時は標準出力に何も出さず 2 (ハブ conformance.md 445c1670)

### 起こし方

- `just --justfile <このリポ>/justfile conformance` (どの cwd からでもよい)
- `conformance` は `conformance-build` (`cargo build --example conformance`。target は `CARGO_TARGET_DIR` があればそこ、無ければ `target`) の後、呼んだ cwd と環境のまま build 済みのバイナリを起こす (`[no-cd]`)
- ハブのスクリプトは場面を流す前に各言語の build (`just conformance-build`) を 1 回呼ぶ
- CLI 自体の試験はハブの適合試験が兼ねる (sockat-rs の試験には置かない)。`cargo test` と `cargo clippy --all-targets` は example も build する

## 却下した案

| 案 | 却下の理由 |
|---|---|
| crate の中の隠したモジュールから経路の情報を取る (feature や cfg フラグの後ろ) | 公開の `_ex` が同じ列で同じ情報を返すので、隠した口は要らない |
| 単体試験の中に標準入力を読む `#[ignore]` の試験を置き、`cargo test --lib -- --ignored --exact` で起こす | libtest が標準出力に `running 1 test` と結果の行を出し、「標準出力に JSON を 1 つ」を満たさない |
| `src/bin/` のバイナリ | `cargo install sockat` で入る |
| JSON を手で読む | 入力の文字列のエスケープ (`\u0000`、サロゲート対) を正しく読む手間に見合わない。`serde_json` は dev-dependency で、利用者の依存を増やさない |

## 判断の出所

- 実装側の判断 2026-11-19 (ハブの conformance.md に従う)
