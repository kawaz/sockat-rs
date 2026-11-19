# DR-0029: any 系 (bind_any、connect_any 系) は何かを失ってでも届かせる opt-in で、失ったものと通った経路・飛ばした経路を返す

- Status: Accepted
- Date: 2026-11-19

## Context

- 既定の bind / connect は、標準と同じに振る舞い、安全でない綴りを残さない経路だけを使う ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))。そのため届かない環境が残る
  - bind: D をどこにも作れず、ソケットのディレクトリも開けない (R を作れない) 時
  - connect: fork-direct でしか届かない時
- 届かせる手はあるが、どれも何かを失う (他の利用者が作り直せる綴り、fork の副作用)
- 言語によらない仕様はハブの [SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) の「公開する操作」と [ハブ DR-0013](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0013-any-means-reach-at-any-cost.md)。この DR はその Rust の形

## Decision

### 置き場と有効にする仕組み

- `bind_any`、`connect_any`、`connect_any_nonblocking` と、その戻り値の型 (`AnyListener`、`AnyConnection`、`AnyError`、`Loss`、`Skipped`) は crate feature `any` を有効にした時だけ公開する。feature は既定では無効
- 有効にする書き方: `sockat = { version = "0.4", features = ["any"] }`
- 経路の実装は feature によらずコンパイルする (`bind` も fork を使い、`connect` 系も同じ順番の関数を通る)。feature が切り替えるのは公開する 3 つの関数と 5 つの型だけ
- docs.rs は全 feature を有効にして build する (`[package.metadata.docs.rs] all-features = true`)。feature が要ることは、関数と型の doc の先頭の 1 文 (「Available on crate feature `any` only.」) と crate doc の Features の節で示す。`doc(cfg)` (nightly) は使わない
- feature を有効にしない時に存在しない item への intra-doc link を、他の item の doc から張らない (名前だけ書く)
- 試験と CI は `--all-features` で回し、feature 無しの clippy も `just check` で回す

### bind_any

- `bind` と同じ順に試し、使える経路が無い時 (`bind` が `ENAMETOOLONG` になる所) に最後に置き場の symlink (SPEC の symlink) を試す (`route::links::bind_through`)
- 置き場に `sockat.XXXXXXXXXX` の symlink をソケットのディレクトリの realpath に向けて作り、`<置き場>/sockat.XXXXXXXXXX/<ファイル名>` で bind して、すぐ消す
- 置き場の候補は `XDG_RUNTIME_DIR`、`TMPDIR` (どちらも空でない絶対パスの時)、`/tmp`。同じ綴りは 1 回 (`Config::bind_link_places`)
- 持ち主だけが書ける置き場 (持ち主が実効 uid、`mode & 0o022 == 0`) を候補の順に試し、次に共有の置き場 (sticky、持ち主が root か実効 uid) を候補の順に試す (`route::links::PlaceKind`)。どちらでもない、綴りが収まらない、symlink を作れない候補には何も作らず次へ
- 置き場の判定は connect の symlink と同じ関数 (`place_kind_of`) を使う。connect は両方を「安全な置き場」として候補の順に使う
- 経路は `Route::SymlinkBind { shared }` (名前は connect の `Route::Symlink` と同じ `symlink`)。`shared` は共有の置き場で bind したか

### 戻り値

```rust
pub fn bind_any<P: AsRef<Path>>(path: P) -> Result<AnyListener, AnyError>;
pub fn connect_any<P: AsRef<Path>>(path: P) -> Result<AnyConnection, AnyError>;
pub fn connect_any_nonblocking<P: AsRef<Path>>(path: P) -> Result<AnyConnection, AnyError>;

#[non_exhaustive]
pub struct AnyListener {
    pub listener: UnixListener,
    pub losses: Vec<Loss>,
    pub route: &'static str,
    pub skipped: Vec<Skipped>,
}

#[non_exhaustive]
pub struct AnyConnection {
    pub stream: UnixStream,
    pub losses: Vec<Loss>,
    pub route: &'static str,
    pub skipped: Vec<Skipped>,
}

#[non_exhaustive]
pub enum Loss { Respelled, Forked, RecreatableSpelling, CwdRelative }

#[non_exhaustive]
pub struct Skipped {
    pub name: &'static str,
    pub reason: &'static str,
}

#[non_exhaustive]
pub struct AnyError {
    pub error: io::Error,
    pub route: Option<&'static str>,
    pub skipped: Vec<Skipped>,
}
impl From<AnyError> for io::Error { /* error をそのまま返す */ }
```

- `losses` は通った経路で失ったものの並び (ハブ SPEC の「失うもの」の表の列どおり、`lib::losses`)。何も失わなければ空。`Loss::as_str` と `Display` は SPEC の名前 (`respelled` / `forked` / `recreatable-spelling` / `cwd-relative`)

| 経路 | bind | connect |
|---|---|---|
| standard | 相対パス (abstract の名前を除く) なら `CwdRelative`、他は空 | 空 |
| D を通す経路、`-sockdir` (fork 以外) | `Respelled` | 空 |
| fork、fork-sockdir | `Respelled`、`Forked` | |
| symlink (持ち主だけが書ける置き場) | `Respelled` | 空 (connect の symlink) |
| symlink (共有の置き場) | `Respelled`、`RecreatableSpelling` | |
| 予備の経路 | | 空 |
| fork-direct | | `Forked` |

- `route` は通った経路の名前 (SPEC の「経路の名前」、`Route::name`)。`skipped` は試して使えなかった経路の並び (試した順)。長いパスでは最初が `standard`
- `skipped` に入るのは、その呼び出しで試す順の列 (SPEC の「試す順番」と、OS と `/proc` を使えるかで決まる予備の経路の 1 行) にある経路だけ。`/proc` を使えない Linux では proc・proc-sockdir・proc-direct・proc-held を入れず、fork しない connect は fork-direct を入れない
- `route` より前に試した経路はすべて使えなかったもの (実際に bind / connect して失敗したら、そのエラーを返して次を試さない)。「使えたか」は持たない
- `Loss` の名前は約束で、足すだけ。`route`・`skipped` の名前と `reason` は診断用で、中身を約束しない (doc にそう書く)
- 失敗した時は `AnyError`: `error` は既定の関数と同じ `io::Error`、`route` は実際に bind / connect して失敗した経路の名前 (短いパスで std が失敗した時は `standard`。どの経路も使えない `ENAMETOOLONG` と、経路の前に断った時は `None`)、`skipped` は試して使えなかった経路。経路の前に断った時 (NUL、名前で終わらない、ソケットのディレクトリが無い・ディレクトリでない・realpath を引けない) は `skipped` も空 (長さの判定はディレクトリを引いた後なので、`standard` もまだ試していない)
- `From<AnyError> for io::Error` は中の `io::Error` をそのまま返す。`io::Result` を返す関数の中で `?` が使え、`raw_os_error()` の errno が残る。`Display` と `source` は中の `io::Error` のもの
- 4 つの型とも `#[non_exhaustive]` にする。フィールドは公開し、accessor は作らない
- derive: 戻り値の 2 つと `AnyError` は `Debug` だけ (`UnixListener` / `UnixStream` / `io::Error` が `Clone` / `PartialEq` を持たない)。`Loss` と `Skipped` は `Debug, Clone, Copy, PartialEq, Eq, Hash`

### 飛ばした経路の集め方

- 1 回の呼び出しの状態 `route::Target` が `skipped` を持ち、経路が使えないと判断した所で `Target::skip(名前, 理由)` を呼ぶ。経路の名前と理由は `route::names` の定数
- D を通す経路は、その OS と状態の手段だけを (`/proc` の手段は `/proc` を使える時だけ)、D を作る前に分かる理由 (綴りが収まらない、スレッド cwd が使えない) と一緒に持ち (`route::central_means`)、D を作った後に分かる理由 (スレッドが断られた、D の fd で測り直した `/proc` の綴りが収まらない) と合わせて、試す順に記録する。D をどこにも作れない時は D を通す経路を「no place for D」で記録してから `-sockdir` の経路に進む
- 記録は長いパスの呼び出しのたびに行う (`route::Reached::skipped`)。読むのは any 系と試験だけ

## 理由

- 有効にする手間が「失うものを受け入れる」という意思表示になる。普段の補完と doc の入口を散らかさない
- `#[non_exhaustive]`: 失うものの名前や戻り値の項目を後で足しても既存の利用者を壊さない。フィールドを公開したままなので、`let AnyConnection { stream, route, .. } = ...;` と分解して stream を取り出せる (accessor にすると、stream を持ち出す口を別に足す必要がある)
- 経路の名前と理由を `&'static str` にする: 中身を約束しないので、enum にして網羅の `match` を誘うより、表示と記録に向く形にする。失うものは約束なので enum にし、文字列は `as_str` / `Display` で SPEC の名前にそろえる
- エラーを `io::Error` のまま返さず `AnyError` にする: `io::Error::new` で包むと `raw_os_error()` が `None` になる。`AnyError` は `io::Error` を値で持ち、`From` で取り出すので errno が残る。`skipped` を別の口 (呼び出しの後に引く等) で渡す形は、呼び出しと結果の対応が崩れる
- 飛ばした経路を、判断した所で `Target` に記録する: 経路ごとの関数が「使えない」を返す形 (`Step::Next`) を変えずに済み、複数の経路をまとめて判断する所 (D を作れない時) も同じ口で記録できる
- `bind_any` の置き場に持ち主だけが書ける置き場を先に使う: 綴りを作り直せるのが自分だけなら、片付けた後の綴りは何にも当たらない

悪い面:

- 長いパスの呼び出しのたびに、any 系でなくても飛ばした経路を集める (数個の要素の `Vec`)
- 戻り値が `io::Result` でないので、`io::Result` を返す関数の中では `?` で `io::Error` に変わり、`skipped` は失われる (要るなら `AnyError` のまま扱う)
- `bind_any` の symlink の綴りは D を通す経路より長いので、ファイル名が長すぎる時には役に立たない。届くのは D も D 無しの形も使えない時だけ

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 戻り値を `#[non_exhaustive]` にしない | 失うものの名前や項目を後で足すと API を壊す |
| 失ったものを 1 つの段階 (enum の 1 値) で返す | ハブ DR-0013 で不採用 (標準の経路でも相対パスなら `cwd-relative` を知らせる等、並びでないと表せない) |
| エラーを `io::Error::new` で包み、`skipped` を中に入れる | `raw_os_error()` が `None` になり、errno で分岐する利用者のコードが壊れる |
| フィールドを隠して accessor にする | 不変条件の無い値に取り出し口を足すだけになり、分解できない |
| 経路の名前を enum で返す | 経路の追加・変更が破壊的変更になる。名前は診断用 |
| `Step::Next` に理由を持たせる | D を作れない時など複数の経路をまとめて飛ばす所で、1 つの値に収まらない |
| `bind_any` の置き場を持ち主だけが書ける置き場に限る | BSD 等で `/tmp` しか無いと届かない (ハブ DR-0013) |

## 未確定

- `bind_any` に後始末つきの形 (`_ex`) を置くか (ハブ DR-0013)

## 判断の出所

- any 系の意味、`bind_any` を Rust にも置く、置き場の順、`route`・`skipped` (`{ name, reason }`) を返す、`route`・`skipped` は診断用: kawaz 裁定 (ハブ DR-0013、SPEC e0357cff、2026-11-19)
- 失ったものを名前の並び `losses` (`respelled` / `forked` / `recreatable-spelling`) で返す、失敗時もエラーに `skipped` と失敗した経路を付け errno を保つ: kawaz 裁定 (ハブ DR-0013、SPEC f00f3217、2026-11-19)
- `skipped` に入れるのは試す順の列にある経路だけ、短いパスの std の失敗の `route` は `standard`: ハブ SPEC f72b68e7。経路の前の失敗は `skipped` も空: ハブ SPEC ead73a28
- `cwd-relative` を足す (D13-Q1)、abstract の名前には付けない: kawaz 裁定 (ハブ SPEC 7ab4e31e、8e938ad3、2026-11-19)
- any 系を feature `any` の後ろに置く、docs.rs は全 feature: kawaz 裁定 (ハブ DR-0013、2026-11-18)
- 型名 (`AnyListener`、`AnyConnection`、`AnyError`、`Loss`、`Skipped`)、`#[non_exhaustive]`、公開フィールド、`&'static str`、`Target::skip` で集める形、`AnyError` と `From<AnyError> for io::Error`: 実装側の判断 2026-11-19
