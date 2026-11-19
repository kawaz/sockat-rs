# DR-0031: 既定の操作ごとに後始末・診断つきの `_ex` を置き、最終手段を使うかを `LastResort` で選ぶ

- Status: Accepted
- Date: 2026-11-19

## Context

- 既定の bind / connect は、標準と同じに振る舞い、安全でない綴りを残さない経路だけを使う ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))。そのため届かない環境が残る (bind: D をどこにも作れず、ソケットのディレクトリも開けない時。connect: fork でしか届かない時)
- 届かせる手はあるが、どれも何かを失う (他の利用者が作り直せる綴り、fork の副作用)
- 長いパスでは、カーネルが覚える綴りが元のパスでない。元のパスで、自分が作ったファイルだけを消す口が要る。listener は tokio 等へ move されるので、消す口は閉じる対象を持てない
- 言語によらない仕様はハブの [SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) の「公開する操作」「失うもの」と [ハブ DR-0015](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0015-ex-with-last-resort-and-diagnostics.md)。この DR はその Rust の形

## Decision

### 公開する関数と型

```rust
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener>;
pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream>;
pub fn connect_nonblocking<P: AsRef<Path>>(path: P) -> io::Result<UnixStream>;

pub fn bind_ex<P: AsRef<Path>>(path: P, last_resort: LastResort) -> Result<ExListener, ExError>;
pub fn connect_ex<P: AsRef<Path>>(path: P, last_resort: LastResort) -> Result<ExStream, ExError>;
pub fn connect_nonblocking_ex<P: AsRef<Path>>(path: P, last_resort: LastResort) -> Result<ExStream, ExError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LastResort { #[default] Avoid, Allow }

#[non_exhaustive] #[derive(Debug)]
pub struct ExListener {
    pub listener: UnixListener,
    pub file: SocketFile,
    pub losses: Vec<Loss>,
    pub route: &'static str,
    pub skipped: Vec<Skipped>,
}

#[non_exhaustive] #[derive(Debug)]
pub struct ExStream {
    pub stream: UnixStream,
    pub losses: Vec<Loss>,
    pub route: &'static str,
    pub skipped: Vec<Skipped>,
}

#[non_exhaustive] #[derive(Debug)]
pub struct ExError {
    pub error: io::Error,
    pub route: Option<&'static str>,
    pub skipped: Vec<Skipped>,
}
impl From<ExError> for io::Error { /* error をそのまま返す */ }

#[non_exhaustive] #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Loss { Respelled, Forked, RecreatableSpelling, CwdRelative }

#[non_exhaustive] #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Skipped { pub name: &'static str, pub reason: &'static str }

#[derive(Debug)]
pub struct SocketFile { /* realpath, (st_dev, st_ino) */ }
impl SocketFile { pub fn discard(self) -> io::Result<()>; }
```

- 既定の 3 つ (`bind`、`connect`、`connect_nonblocking`) は std の型だけを返し、最終手段を使わない
- `_ex` は既定の操作ごとに 1 つ。`LastResort::Avoid` (既定) なら既定の関数と同じ経路を同じ順に試し、`LastResort::Allow` ならその後に最終手段を試す

| 関数 | 最終手段 | 失うもの |
|---|---|---|
| `bind_ex` | symlink-dir (`route::links::bind_through`): 置き場にソケットのディレクトリを指す symlink を作り、`<置き場>/sockat.XXXXXXXXXX/<ファイル名>` で bind | `Respelled`。共有の置き場では加えて `RecreatableSpelling` |
| `connect_ex`、`connect_nonblocking_ex` | fork-cwd-direct (`route::fork_beside`) | `Forked` |

- bind の fork-cwd と symlink-file は既定の経路で、最終手段ではない
- symlink-dir の置き場は `XDG_RUNTIME_DIR`、`TMPDIR` (どちらも空でない絶対パスの時)、`/tmp` (同じ綴りは 1 回。`Config::dir_link_places`)。持ち主だけが書ける置き場を候補の順に試し、次に共有の置き場を候補の順に試す
- crate の feature は持たない。`_ex` と型はいつも公開する

### 戻り値

- `losses` は通った経路で失ったものの並び (`lib::losses`)

| 経路 | bind | connect |
|---|---|---|
| standard | 相対パス (abstract の名前を除く) なら `CwdRelative`、他は空 | 空 |
| symlink-file、D を通す経路、`-sockdir` (fork-cwd 以外) | `Respelled` | 空 |
| fork-cwd、fork-cwd-sockdir | `Respelled`、`Forked` | |
| symlink-dir (持ち主だけが書ける置き場) | `Respelled` | |
| symlink-dir (共有の置き場) | `Respelled`、`RecreatableSpelling` | |
| proc-direct・proc-held・relative・widened・at-direct | | 空 |
| fork-cwd-direct | | `Forked` |

- `route` は通った経路の名前 (ハブ SPEC の「経路の名前」、`Route::name`)。`skipped` は試して使えなかった経路の並び (試した順)。長いパスでは最初が `standard`
- `skipped` に入るのは、その呼び出しで試す順の列 (ハブ SPEC の「試す順番」と、connect では OS と `/proc` を使えるかで決まる「connect の順」の 1 行。`LastResort::Allow` ならその最後に最終手段) にある経路だけで、その列の順に並ぶ。`/proc` を使えない Linux では proc・proc-sockdir・proc-direct・proc-held を入れず、Linux の bind は symlink-file を入れず、`LastResort::Avoid` の connect は fork-cwd-direct を入れない
- `route` より前に試した経路はすべて使えなかったもの (実際に bind / connect して失敗したら、そのエラーを返して次を試さない)。「使えたか」は持たない
- `Loss` の名前は約束で、足すだけ。`as_str` と `Display` はハブ SPEC の名前。`route`・`skipped` の名前と `reason` は診断用で、中身を約束しない
- 失敗した時は `ExError`: `error` は既定の関数と同じ `io::Error`、`route` は実際に bind / connect して失敗した経路の名前 (短いパスで std が失敗した時は `standard`。どの経路も使えない `ENAMETOOLONG` と、経路の前に断った時は `None`)、`skipped` は試して使えなかった経路。経路の前に断った時 (NUL、名前で終わらない、ソケットのディレクトリが無い・ディレクトリでない・realpath を引けない) は `skipped` も空
- `From<ExError> for io::Error` は中の `io::Error` をそのまま返す。`Display` と `source` は中の `io::Error` のもの

### SocketFile

- `bind_ex` は bind が通った直後、経路の中で listener を返す前に、ソケットのファイルの realpath と `(st_dev, st_ino)` を覚える (`route::run_reporting` が bind の時に `route::Bound` を取る)
  - std に渡す長さのパス: std の bind が返った直後に `realpath(3)` と `lstat(2)`
  - D を通す経路と D 無しの形: R が残っている間に、bind と同じ綴り `R/<ファイル名>` で D (またはソケットのディレクトリ) の fd から `fstatat(AT_SYMLINK_NOFOLLOW)`。パスは R の先 (ソケットのディレクトリの realpath) とファイル名
  - symlink-file と symlink-dir: ソケットのディレクトリの realpath とファイル名を `lstat`
- 覚えられなければ listener を閉じ、ファイルは消さずにエラーを返す (通った経路の名前を `route` に付ける)
- `discard()` は realpath を `lstat` し、`(st_dev, st_ino)` が同じ時だけ `unlink` する。違う物があれば触らず、何も無ければ (`ENOENT` / `ENOTDIR`) 何もしない。どちらも `Ok(())`。確かめられない時とそれ以外の `unlink` の失敗はエラー
- `discard()` は listener を閉じない。`SocketFile` を drop しても何もしない

### 飛ばした経路の集め方

- 1 回の呼び出しの状態 `route::Target` が `skipped` を持ち、経路が使えないと判断した所で `Target::skip(名前, 理由)` を呼ぶ。名前と理由は `route::names` の定数
- D を通す経路は、その OS と状態の手段だけを、D を作る前に分かる理由 (`route::means_check`) と D を作った後に分かる理由と合わせて、試す順に記録する
- 記録は長いパスの呼び出しのたびに行う (`route::Reached::skipped`)。読むのは `_ex` と試験だけ

## 理由

- `LastResort::Allow` を渡す手間が「失うものを受け入れる」という意思表示になる (ハブ DR-0015)
- 引数を `bool` でなく enum にする: 呼ぶ所で `bind_ex(path, true)` と書くと何を選んだか読めない。ハブ DR-0015 で選べるのは 1 つだけと決まっているので、項目を足せる options の struct にはしない (足せる形は、経路を組み合わせる口を作らない決まりと合わない)。`Default` を `Avoid` にして「既定は使わない」を型で表す
- 型名を `ExListener` / `ExStream` / `ExError` にする: `_ex` の関数の戻り値であることが名前で分かり、std の `UnixListener` / `UnixStream` と並ぶ
- 戻り値を `#[non_exhaustive]` の公開フィールドの struct にする: 失うものの名前や項目を後で足しても既存の利用者を壊さず、`let ExListener { listener, file, .. } = ...;` と分解して listener と `SocketFile` を別々に持ち出せる
- `bind_ex` の戻り値に `SocketFile` を入れる (ハブ DR-0015 の表): 後始末と診断を 1 つの関数にまとめ、別の後始末つきの関数を増やさない
- エラーを `io::Error` のまま返さず `ExError` にする: `io::Error::new` で包むと `raw_os_error()` が `None` になる
- feature を持たない: `_ex` の引数で最終手段を選ぶので、feature で隠す理由が無い。feature を外すと、試験・clippy・docs.rs を全 feature と feature 無しで分けて回す必要も無くなる

悪い面:

- `bind_ex` は短いパスでも realpath と `lstat` を引くので、`bind` より少し重い
- 戻り値が `io::Result` でないので、`io::Result` を返す関数の中では `?` で `io::Error` に変わり、`skipped` は失われる (要るなら `ExError` のまま扱う)
- symlink-dir の綴りは D を通す経路より長いので、ファイル名が長すぎる時には役に立たない。届くのは D も D 無しの形も使えない時だけ

## 却下した案

| 案 | 却下の理由 |
|---|---|
| any 系 (`bind_any`、`connect_any` 系) を feature `any` の後ろに置く | ハブ DR-0015 で不採用 (API が増え、推奨コードと結び付けにくい) |
| 最終手段を `bool` の引数で選ぶ | 呼ぶ所で意味が読めない |
| 最終手段を options の struct で選ぶ | 項目を足せる形が、経路を組み合わせる口を作らない決まり (ハブ DR-0015) と合わない |
| `bind_ex` を `(UnixListener, SocketFile)` のまま残し、診断を別の関数で返す | ハブ DR-0015 は `bind_ex` 1 つで後始末と診断を返す |
| エラーを `io::Error::new` で包み、`skipped` を中に入れる | `raw_os_error()` が `None` になり、errno で分岐する利用者のコードが壊れる |
| 経路の名前を enum で返す | 経路の追加・変更が破壊的変更になる。名前は診断用 |

## 判断の出所

- any 系を作らず既定の操作ごとに `_ex` を置く、引数は最終手段を使うかの 1 つだけ、最終手段 (bind は symlink-dir、connect は fork-cwd-direct)、置き場の順、`losses`・`route`・`skipped` を返す、失敗時も `skipped` と失敗した経路を付け errno を保つ、`discard` は消すだけ: kawaz 裁定 (ハブ DR-0015、2026-11-19)
- 経路の名前 (fork-cwd、fork-cwd-sockdir、fork-cwd-direct、symlink-file、symlink-dir): kawaz 裁定 (ハブ DR-0016、2026-11-19)
- `LastResort` の enum、型名 (`ExListener`、`ExStream`、`ExError`)、feature を外す、`#[non_exhaustive]` と公開フィールド: 実装側の判断 2026-11-19
- `SocketFile` の決まり (bind の直後に覚える、覚えられなければエラー、同じ時だけ消す、確かめられない時はエラー): ハブ SPEC の「公開する操作」
