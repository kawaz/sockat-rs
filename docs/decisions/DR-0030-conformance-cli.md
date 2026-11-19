# DR-0030: 適合試験の CLI は example に置き、経路の情報は cfg フラグ `sockat_conformance` の後ろに隠したモジュールから取る

- Status: Accepted
- Date: 2026-11-19

## Context

- ハブの適合試験 ([ハブの docs/design/conformance.md](https://github.com/kawaz/sockat/blob/main/docs/design/conformance.md)) は、各言語の試験用 CLI を場面ごとに起こし、標準入力の JSON で 1 回 bind / connect させ、通った経路・飛ばした経路・失うもの・errno を標準出力の JSON で受け取る。cwd・環境変数・umask は場面ごとにハブのスクリプトが整える
- 既定の `bind` / `connect` は経路の情報を返さない。それは crate の中 (`route::run_reporting`) にしかない ([DR-0011](DR-0011-routes-not-exposed.md))
- CLI は公開しない (conformance.md、SPEC の「経路は公開 API に出さない」)

## Decision

### 隠したモジュール

- `src/conformance.rs` を `#[cfg(all(sockat_conformance, feature = "any"))]` と `#[doc(hidden)]` の後ろで `pub mod conformance` にする。中身は 2 つの関数
  - `conformance::bind(path) -> Result<AnyListener, AnyError>`: `bind` と同じ列 (置き場の symlink を試さない)
  - `conformance::connect(path) -> Result<AnyConnection, AnyError>`: `connect` と同じ列 (ブロックする、fork-direct を試さない)
- 戻り値は any 系の型 ([DR-0029](DR-0029-any-functions.md))。`losses`・`route`・`skipped` は any 系と同じ関数 (`bind_reporting` / `connect_reporting`) で作る
- `sockat_conformance` は Cargo の feature でなく rustc の `--cfg` (`RUSTFLAGS='--cfg sockat_conformance'`)。`Cargo.toml` の `[lints.rust] unexpected_cfgs` の check-cfg で宣言する

### CLI

- `examples/conformance.rs`。`[[example]]` に `required-features = ["any"]`
- cfg を立てない build (`cargo clippy --all-targets --all-features`、`cargo test` が example も build する) では、build の仕方を標準エラーに出して終了コード 2 で終わる `main` だけになる
- 入力の JSON は dev-dependency の `serde_json` で読む。出力は項目を conformance.md の順に並べて組み立て、文字列は `serde_json` でエスケープする
- `error`: errno のあるエラーは POSIX の errno の表で引いた名前 (同じ値の名前が 2 つある時は表の先の方。`EAGAIN`、`EOPNOTSUPP`、表に無い errno は `errno-<番号>`)。errno の無いエラーは `InvalidInput` なら `invalid-argument`、他は `general`
- bind が通ったら listener を閉じる (ソケットのファイルは消さない)。connect が通ったら `x` を 1 バイト送って閉じる
- 終了コード: 結果を出したら 0 (bind / connect の成否によらない)。connect の後に `x` を送れなかった時は結果を出して 1。標準入力を読めない時は標準出力に何も出さず 2

### 起こし方

- `just --justfile <このリポ>/justfile conformance` (どの cwd からでもよい)
- `conformance` は `conformance-build` (cfg を立てて `target/conformance` に build。`CARGO_TARGET_DIR` があればその下の `conformance`) の後、呼んだ cwd と環境のまま build 済みのバイナリを起こす (`[no-cd]`)。RUSTFLAGS が違うので target を分け、通常の build の成果物を作り直させない
- build 済みなら `conformance-build` は何も作らない (umask 0777、存在しない `TMPDIR` でも通り、`TMPDIR` に何も残さないことを確かめた)。場面の環境で build が走らないよう、ハブのスクリプトは場面を流す前に `just conformance-build` を 1 回呼ぶ
- `just check` は cfg を立てた clippy (`--all-targets --all-features`) も回す
- CLI 自体の試験はハブの適合試験が兼ねる (sockat-rs の試験には置かない。cfg を変えた build が要り、`just test` を遅くする)

### DR-0011 との関係

- feature にしない。feature は依存する crate の宣言から有効にでき、docs.rs の `all-features` で doc に出る。cfg フラグは build する人が RUSTFLAGS で立てた時だけ効き、Cargo の依存の宣言からは立てられない
- 通常の build にも docs.rs にも出ない。立てた時も `#[doc(hidden)]` で、何も約束しない (どの版で変えても消してもよい)
- 公開する crate には `src/conformance.rs` も入る (`lib.rs` が cfg の下で宣言するため)。`examples/` は `include` の外で入らない
- DR-0011 の「feature も cfg(test) 専用の公開口も作らない」の例外は、この口だけ

## 却下した案

| 案 | 却下の理由 |
|---|---|
| feature (`__conformance` 等) | 依存の宣言から有効にでき、docs.rs の `all-features` で doc に出る |
| 単体試験の中に標準入力を読む `#[ignore]` の試験を置き、`cargo test --lib -- --ignored --exact` で起こす | 公開の口は要らないが、libtest が標準出力に `running 1 test` と結果の行を出し、「標準出力に JSON を 1 つ」を満たさない |
| `src/bin/` のバイナリ | `cargo install sockat` で入る。経路の情報は結局 crate の中から取る口が要る |
| 公開の `bind_any` / `connect_any` で作る | 列が違う (`bind_any` は置き場の symlink、`connect_any` は fork-direct を足す) |
| JSON を手で読む | 入力の文字列のエスケープ (`\u0000`、サロゲート対) を正しく読む手間に見合わない。`serde_json` は dev-dependency で、利用者の依存を増やさない |

## 判断の出所

- 実装側の判断 2026-11-19 (ハブの conformance.md に従う)
