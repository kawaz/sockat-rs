# DR-0033: コマンド `sockat` は workspace の別パッケージ sockat-cli に置き、B の作り方は feature の後ろの doc に出さない入口でライブラリから使う

- Status: Proposed (形はハブと合わせ済み。sockat-cli の公開は kawaz の裁定待ち)
- Date: 2026-11-22

## Context

- ハブの [DR-0018](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0018-cli-resolve-with-reused-hashed-link.md) と [設計文書 cli.md](https://github.com/kawaz/sockat/blob/main/docs/design/cli.md) が、OS の上限 (sockaddr に入る長さ) に収まる綴りを出すコマンド `sockat` を Rust で作り、このリポに置くことを決めた。目的は短くすることでなく、bind / connect がエラーにならない綴りを出すこと。crate の分け方と名前はこのリポで決める
- コマンドは引数のパーサと SHA-256 を要る。ライブラリの利用者の依存にそれを足さない (DR-0018「作る物と置き場所」)
- `resolve` の B の作り方と確かめはライブラリと同じにする (DR-0018「`resolve` の既定の綴り」3、ハブ SPEC-routes「B の作り方」)。それは `src/base.rs` の `pub(crate)` の部分で、別のパッケージからは見えない
- 今回作るのは `resolve` (省略形 `sockat <パス>`)、`sweep`、`completion`、`--help` / `--help-all` / `--version`。`--base-fd` / `--base-pid`、`--route`、手法を試す `bind` / `connect` は後で作る

## Decision

### パッケージの分け方

- リポを cargo の workspace にする (`[workspace] members = [".", "cli"]`)。ライブラリ `sockat` はリポ直下のまま (crates.io に出している位置と `include` を変えない)
- コマンドは `cli/` のパッケージ `sockat-cli`、bin の名前は `sockat` (公開すれば `cargo install sockat-cli`)
- 依存はパッケージごとに分ける

  | パッケージ | 依存 |
  |---|---|
  | `sockat` | `libc` のまま (増やさない) |
  | `sockat-cli` | `sockat` (path、feature `__cli-internal`)、`clap`、`clap_complete`、`sha2` 0.10。試験に `serde_json`・`tempfile` |

- `sha2` は 0.10 にする (0.11 は Rust 1.88 を求め、MSRV 1.85 を保てない)。他の依存は 1.85 で通る (`cargo +1.85 check --workspace`)
- `sockat-cli` は `publish = false`。release.yml は `cargo publish -p sockat` でライブラリだけを出す。公開は kawaz の裁定の後に外す
- `sockat-cli` の version はライブラリと別に持つ (0.1.0 から)。ライブラリは path 依存に `version = "=x.y.z"` を添えて固定し、`just bump-version` がライブラリの version を上げる同じ commit でこの固定も動かす

### CLI 専用の入口

- ライブラリに、既定で無効の feature `__cli-internal` (依存を足さない) の後ろで、`#[doc(hidden)]` の `pub mod __cli` (`src/cli_internal.rs`) を置く。公開の API (doc に出る物、semver の約束) は変えない
- 入口は API でない。何も約束せず、どのリリースでも黙って変わる・消える。module の doc と Cargo.toml の feature の注記に「CLI 専用、使わない」と書く
- feature の名前は先頭を `__` にして内部用と分かるようにする。cargo は先頭の `_` と途中の `-` を許し、crates.io は ASCII の英数字と `_`・`-`・`+` を許す
- 入口に置くのは、コマンドがライブラリと同じ B を作り確かめるのに要る物と、綴りの長さ・名前に要る定数と乱数だけ。経路を選ぶ口は置かない

  | 物 | 中身 |
  |---|---|
  | `LIMIT` | 綴りの上限 (`sun_path` − 1。Linux・Android・illumos は 107、Apple と BSD は 103)。ライブラリの `route::DIRECT_MAX` |
  | `BIND_FOLLOWS_SYMLINK` | bind が最後の要素の symlink を辿ると分かっている OS か (Apple・NetBSD)。分からない OS は `false` |
  | `Places` | 置き場 1〜3 (`from_env`: `/tmp`・`$XDG_RUNTIME_DIR`・`$TMPDIR`。`with_tmp`: `/tmp` を差し替えた物、試験用) |
  | `automatic(places, fits)` | プロセスの B (ライブラリの `base::automatic`。見積もりの `fits` に外れる置き場では作らず、次の置き場にも移らない) |
  | `check_base_form`・`check_given`・`given` | `Options.base` と同じ形の検査、作らない確かめ、作る (`base::split_given`・`check_given`・`given`) |
  | `sweep(places)` | ライブラリの `sweep()` と同じ (`base::sweep`。`places` を `from_env` にすれば `sweep()` そのもの) |
  | `random_name` | ライブラリの乱数の名前 (英数字 10 文字) |

- ライブラリの側の変更は、この module と、置き場 1 を差し替えられる `base::places_with_tmp` (`places_from` はそれに `/tmp` を渡す)、`route::FOLLOWS` を `pub(crate)` にすることだけ。ライブラリの振る舞いは変えない

### `resolve` の作り方 (ハブ DR-0018 の手順 1〜7 を Rust で)

- 相対パスは、短くても cwd (`getcwd`) を前に付けて絶対パスにしてから、手順 1 の長さで分ける (出した綴りを別の cwd のプロセスが使う)。前に付ける時に `.` や `..` を字句で畳まない (`PathBuf::join` は間に `/` を 1 つ置くだけ)。`--json` の `original` は渡されたままの文字列
- 手順 1 の長さは `sun_path` − 1 (`LIMIT`)。それ以下のパスは何も作らずにそのまま出す
- パスの分け方はライブラリの `path::split` と同じ (最後の `/` で分け、名前が空・`.`・`..` なら引数の誤り)
- 手順 3 の B は、`--base` があればその B (入口の `check_given` で作らずに確かめ、綴りが収まると見積もれた時だけ `given` で作る)、無ければ入口の `automatic`。見積もりは「`B/<ハッシュ>/<名前>` か `B/<ハッシュ>` が収まる」で、用途によらない (B の選び方を用途で変えず、ファイルを指す E だけが収まる時は「収まらない」でなく、その用途で使えない理由 (`--for` が無い、bind が symlink を辿らない OS) をエラーにするため)
- 手順 4・6 の E は B の直下。名前は指す先 (ディレクトリの E はソケットのディレクトリの realpath、ファイルの E はその後ろにファイル名を付けた物) のバイト列の SHA-256 の 16 進小文字の先頭 12 文字。`symlink` が `EEXIST` なら `readlink` で指す先を比べ、同じなら使い、違えば `<ハッシュ>.<乱数 10 文字>` に作って `rename` で置き換える (失敗したら作った物を消す)
- E の名前 (16 進 12 文字) も置き換えの途中の名前 (`.` を含む) も D の形 (英数字 10 文字) でないので、`sweep` は触らない。ライブラリの `sweep` は変えない (ハブが SPEC の「置き場に残る物」「sweep」に CLI の E を足す)
- 手順 6 は、`--for connect` ならどの OS でも、`--for bind` は `BIND_FOLLOWS_SYMLINK` の OS だけ出す。`--for` が無ければエラーで `--for` を求める (cli.md の CL-Q1)。bind が symlink を辿らない OS では、エラーの文は `--for connect` だけを勧める
- `--json` の `for` は、ディレクトリを指す綴りと `standard` は `["bind", "connect"]`、ファイルを指す綴りは bind が辿る OS で `["bind", "connect"]`、他で `["connect"]`
- `--json` のパスが UTF-8 でない時は、JSON に書けないのでエラー (終了コード 1) にし、`--json` 無しを勧める。書き換えた文字列を出さない
- JSON は依存を足さずに手で書く (メンバーの順を cli.md の例どおりにし、文字列は `"`・`\`・制御文字を escape する)。試験は `serde_json` で読んで確かめる
- `sweep` は入口の `sweep(places)`。`places` は `Places::from_env()` で、ライブラリの `sweep()` と同じ置き場を見る
- 標準出力は綴り 1 行 (または JSON) だけ。失敗は標準エラーに `sockat: <原因>` と次にすることの 1〜2 行、終了コードは綴りを作れない時 1、引数の誤り 2

### 引数のパーサ

- `clap` を builder の API で使う (derive の proc-macro の依存を足さない)。補完の定義は `clap_complete`
- 理由: kawaz の CLI の好み (ロングオプションだけ、オプションを位置引数の後ろにも置ける、`--` の区切り、サブコマンド、補完の定義を出す) を全部満たし、`--help` と補完の定義を同じ定義から作れる (実装・`--help`・補完の 3 者がずれない)。MSRV 1.85 を保つ
- `sockat <パス>` は、トップレベルに `resolve` と同じ引数を持たせ、`args_conflicts_with_subcommands` と `subcommand_negates_reqs` で、最初の引数がサブコマンドの名前の時だけサブコマンドにする (`sockat --json resolve`・`sockat -- sweep` の `resolve`・`sweep` はパス)。補完もトップレベルでパスを補える
- `-h`・`-V` と `help` サブコマンドは消し、`--help` (`HelpShort`)・`--help-all` (`HelpLong`)・`--version` をロングオプションで置く。進んだオプションは `hide_short_help` で `--help` から隠して `--help-all` にだけ出す (今は無いので両者は同じ中身)
- 引数なしは、トップレベルと、引数を取るサブコマンド (`resolve`・`completion`) で `--help` と同じ物を標準出力に出して終了コード 0 (clap の `arg_required_else_help` は標準エラーに出して 2 で終わるので、使わずに先に見分ける)

### 試験の置き場

- 試験はコマンドをプロセスとして起こす (`cli/tests/cli.rs`)。B は試験ごとの一時ディレクトリに作り、共有の `/tmp/sockat-<uid>` を作らない・頼らない
- debug build のコマンドだけが環境変数 `SOCKAT_CLI_TEST_TMP` を置き場 1 (`/tmp`) の代わりに読む (`#[cfg(debug_assertions)]`)。release build (`cargo install`) にはこの口が無い。release build で回す試験ではこれに頼る試験を `ignore` にする

理由:

- workspace の別パッケージにすれば、パーサと SHA-256 はライブラリの依存に現れず、ライブラリの利用者の build にも Cargo.lock にも入らない
- B の作り方と確かめ (途中の確かめ、`mkdirat`・`fchmodat`・`O_NOFOLLOW`、占拠の扱い、プロセスの B の持ち方) は細かく、`*at` の呼び出しに `unsafe` が要る。コマンドに写すと 2 つの実装がずれ、`unsafe` を `src/sys.rs` の外に置くことになる。入口から使えば 1 つの実装のまま
- 入口を公開の API にすると、B の作り方が約束になり、ハブ DR-0017 の「経路を選ぶ口は作らない」と [DR-0011](DR-0011-routes-not-exposed.md) の方針に反する。feature の後ろの doc に出さない module なら、約束を増やさずに別のパッケージから使える
- 版を `=` で固定するのは、入口が semver に縛られず、ライブラリの patch の版で変わりうるため。同じリポの path 依存なので固定は今の版と常に同じで、`just bump-version` が動かす
- E を B の fd でなくパスで作る (`symlink`・`readlink`・`rename`) のは、コマンドが `unsafe` を持たず std に `*at` の呼び出しが無いため。B はライブラリが確かめた自分のディレクトリ (途中を他人が変えられず、B は自分の物で group と他人が書けない) なので、その間に他人がパスの指す物を差し替えられない
- 置き場の差し替えを debug build に閉じるのは、release build に文書に無い口を残さないため

悪い面:

- feature `__cli-internal` は crates.io の feature の一覧と `cargo add` の表示に出る (実測: `cargo add --dry-run` が `- __cli-internal` を並べた)。名前と注記で「使わない」を示すだけで、使うことは止められない
- ライブラリの version を上げるたびに `cli/Cargo.toml` の固定も動く。コマンドを公開した後は、ライブラリだけを直したリリースでもコマンドを出し直さないと、公開されたコマンドは古いライブラリのまま
- 試験の置き場の口は debug build にしか無いので、`cargo test --release` ではそれに頼る試験が `ignore` になる
- ファイルを指す E だけが収まり、その用途で使えない時 (`--for` が無い、bind が symlink を辿らない OS の `--for bind`) は、エラーにする前に B を作る (B は残す物なので害は無いが、何も出さずに B ができる)
- 補完はトップレベルのパスとオプションと、サブコマンドの名前を同じ位置に並べる (パスの補完の候補にサブコマンドの名前が混ざる)
- 上限に収まる短いパスは何も確かめずにそのまま出す (`/tmp/x/` のような名前で終わらないパスもそのまま。ライブラリが std にそのまま渡すのと同じ)

## Alternatives Considered

- 案 A: ライブラリのパッケージに bin を足し、パーサと SHA-256 を optional の依存と feature (`cli`) で入れる
  - 不採用理由: ライブラリの manifest に CLI の依存が並び、公開する crate に CLI のコードと試験が入る。CLI だけの変更でもライブラリの version を上げることになる。`cargo install sockat --features cli` と feature を覚えさせる
- 案 B: B の作り方をライブラリの公開の API に足す
  - 不採用理由: B の作り方が semver の約束になり、経路と B を API に出さない方針 (DR-0011、ハブ DR-0017) に反する
- 案 C: コマンドに B の作り方と確かめを写す
  - 不採用理由: 2 つの実装がずれる。`*at` の呼び出しに `unsafe` が要り、`unsafe` を `src/sys.rs` に閉じられない
- 案 D: コマンドを別のリポに置く
  - 不採用理由: ハブ DR-0018 がこのリポに置くと決めた。入口と固定の版を同じ変更で動かせない
- 案 E: ライブラリへの依存を `^x.y` で書く
  - 不採用理由: 入口は semver に縛られないので、新しい patch の版で壊れうる
- 案 F: 引数を手で読む、または軽いパーサ (`lexopt`・`pico-args` 等)
  - 不採用理由: `--help` と補完の定義を別に書いて合わせ続けることになる (好みの「実装・`--help`・補完を同時に追従」を手で守る)
- 案 G: 試験の置き場の差し替えを release build でも読む
  - 不採用理由: 文書に無い口が利用者の手元に残る

## Consequences

- コマンドの試験は `cli/tests/cli.rs`。`just test` と `just test-linux*`、CI の各 OS の VM は `--workspace` で回す。clippy と MSRV の check も workspace を見て、clippy は feature なしのライブラリも別に見る。`just lint-unsafe` は `cli/` と `examples/` も見る
- README (日英) と MANUAL にコマンドの節を足すのは公開の後 (ハブ DR-0018 の Consequences)
- `sockat-cli` を公開する時は、`publish = false` を外し、release.yml でライブラリの後に出す (固定の版のライブラリが crates.io に先に要る)

## 関連

- ハブ: [DR-0018](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0018-cli-resolve-with-reused-hashed-link.md)、[cli.md](https://github.com/kawaz/sockat/blob/main/docs/design/cli.md)、[SPEC-routes「B の作り方」](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md#b-の作り方)、[DR-0017](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0017-resolve-through-persistent-base.md)
- [DR-0011](DR-0011-routes-not-exposed.md)、[DR-0016](DR-0016-crate-distribution.md)、[DR-0017](DR-0017-release-flow.md)、[DR-0032](DR-0032-resolve-through-persistent-base.md)
