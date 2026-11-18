# DR-0026: connect_any 系の戻り値は名前付きの struct `AnyConnection`、feature 名は `any`

- Status: Accepted
- Date: 2026-11-18

## Context

- `connect_any` / `connect_any_nonblocking` ([DR-0010](DR-0010-public-api.md)) は stream と「fork の経路を通ったか」を返す。組 `(UnixStream, bool)` では、2 番目が何かを doc で覚えておく必要がある
- ハブの仕様は、戻り値を標準の stream と `forked` を持つ名前付きの型にし、形は言語の慣習で決めるとする。Rust の feature 名は `any` (ハブ [DR-0007](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0007-connect-any-is-opt-in.md))

## Decision

```rust
#[derive(Debug)]
pub struct AnyConnection {
    pub stream: UnixStream,
    pub forked: bool,
}
```

- `connect_any` / `connect_any_nonblocking` は `io::Result<AnyConnection>` を返す
- フィールドは公開する。accessor は作らない
- derive は `Debug` だけ
- `AnyConnection` も 2 つの関数と同じく feature `any` の後ろに置く
- feature 名を `any` にする: `sockat = { version = "0.4", features = ["any"] }`。`bind_any` は作らない (`bind` は今のまま fork の経路を試す)
- API を壊す (戻り値と feature 名) ので 0.4.0 にする

## 理由

- 名前付きなら、組の何番目が何かを覚えずに読める
- 公開フィールド: 経路の結果を運ぶだけの値で、守る不変条件が無い。std の `process::Output` と同じく、`let AnyConnection { stream, forked } = sockat::connect_any(path)?;` と分解して stream を取り出せる。accessor にすると、stream を持ち出す口 (`into_stream` 等) を別に足す必要がある
- 名前: `connect_any` が返す接続で、feature `any` と揃う。`Connection` のような一般名と違い、利用者のコードの他の型と取り違えない
- derive が `Debug` だけなのは、`UnixStream` が `Clone` / `PartialEq` を持たないから
- feature `any` は「fork を受け入れる」1 つの意思表示で、`connect_any` 系だけを有効にする

悪い面:

- 公開フィールドなので、後でフィールドを足すと API を壊す

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 組 `(UnixStream, bool)` のまま | 2 番目が何かを覚えていないと読めない |
| フィールドを隠して accessor (`stream()`、`forked()`、`into_stream()`) にする | 不変条件の無い値に取り出し口を足すだけになる。分解できない |
| `#[non_exhaustive]` を付ける | 分解に `..` が要り、`forked` を見落とす書き方を許す。足す予定のフィールドは無い |
| feature 名を `connect-any` のままにする | ハブの仕様で `any` に決まった |

## 判断の出所

- 戻り値を `stream` と `forked` を持つ名前付きの型にする、feature 名を `any` にする、`bind_any` を作らない: kawaz 裁定 (ハブ DR-0007、2026-11-18)
- 型名 `AnyConnection`、公開フィールド、derive は `Debug` だけ、`#[non_exhaustive]` を付けない: 実装側の判断 2026-11-18
