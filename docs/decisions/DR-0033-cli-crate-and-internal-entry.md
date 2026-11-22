# DR-0033: コマンド `sockat` は workspace の別パッケージ sockat-cli に置き、B の作り方は feature の後ろの doc に出さない入口でライブラリから使う

- Status: Accepted (kawaz 2026-11-22: sockat-cli を crates.io に公開してよい (ハブ RL-Q1 = a)。0.6.1 で公開済み (crates.io の初回は手元から publish し、trusted publisher を設定)。kawaz 指示 2026-11-22: Homebrew (kawaz/tap) でも配り、GitHub release に target ごとの binary とハッシュの検査ファイルを置く)
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
- `sockat-cli` の version はライブラリと同じ番号で動かす。ライブラリは path 依存に `version = "=x.y.z"` を添えて固定し、`just bump-version` がライブラリの version、`sockat-cli` の version、この固定を同じ commit で動かす
- release.yml は、ライブラリを出した後に `sockat-cli` を出す (固定の版のライブラリが crates.io に先に要る)。crates.io に `sockat-cli` がまだ無い時は出さずに知らせだけ残す (初回は kawaz の手元で publish し、その後に trusted publisher を設定する。trusted publishing の設定は crate がある時にしか作れない)

### 配り方

- コマンドは 2 つの経路で配る
  - crates.io: `cargo install sockat-cli` (手元で build する。Rust の動く OS ならどれでも)
  - Homebrew: `brew install kawaz/tap/sockat` (tap kawaz/homebrew-tap の Formula `sockat`。build 済みの binary を入れる。macOS と Linux)
- GitHub release に、target ごとの binary の tar.gz とハッシュの検査ファイルを置く

  | target | 作り方 | Formula |
  |---|---|---|
  | `aarch64-apple-darwin`<br>`x86_64-apple-darwin` | macos-latest で `cargo build`。Developer ID で署名 (hardened runtime、entitlement なし) し、notarize する | 使う |
  | `aarch64-unknown-linux-gnu`<br>`x86_64-unknown-linux-gnu` | cross (image は Ubuntu 16.04、glibc 2.23)。要る glibc は 2.18 (実測) | 使う |
  | `aarch64-unknown-linux-musl`<br>`x86_64-unknown-linux-musl` | cross。static link | 使わない |

- 置く物の名前は `sockat-<version>-<target>.tar.gz` (中身は `sockat`・`LICENSE`・`README.md`) と `SHA256SUMS` (全部の tar.gz の SHA-256、`sha256sum -c` の形)。tar.gz と `SHA256SUMS` に SLSA の build provenance の attestation を付ける
- Formula の desc は "Print a path to a Unix domain socket that fits the socket address limit"、test は `sockat --version`。補完 (bash・zsh・fish) は `sockat completion <shell>` の出力から入れる
- macOS の binary を署名しないまま配らない。署名の secrets が無ければ darwin の job が build の前に落ち、GitHub release も Formula も作らない

### release の流れ

```text
ci - check-version -+- build-release (6 targets) -----------+- release - update-homebrew
                    +- publish-crate (sockat, sockat-cli) -+
```

- build-release と publish-crate は並べて走らせる。release (tag と GitHub release) は両方が通った後に、binary と `SHA256SUMS` を付けて 1 度に作る。update-homebrew は release の後に Formula を作り直して tap に push する (deploy key の secret `HOMEBREW_TAP_DEPLOY_KEY`)
- publish-crate は crates.io に既にある版を出し直さない (その版の API が 200 を返せば飛ばす)。publish の後で build や release が落ちても、手動の起動か `gh run rerun --failed` でやり直して release まで進める
- update-homebrew だけが落ちた時は `gh run rerun --failed` でやり直す (手動の起動は、release 済みの版を check-version が止める)

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
  | `split` | パスをディレクトリとファイル名に分ける (`path::split`。名前で終わらなければ `InvalidInput`) |
  | `pwd_candidate` | 相対パスの `$PWD` の候補 (ライブラリの bind・resolve と同じ `route::pwd_candidate`。長さは見ない) |
  | `check_base_form`・`check_given`・`given` | `Options.base` と同じ形の検査、作らない確かめ、作る (`base::split_given`・`check_given`・`given`) |
  | `sweep(places)` | ライブラリの `sweep()` と同じ (`base::sweep`。`places` を `from_env` にすれば `sweep()` そのもの) |
  | `random_name` | ライブラリの乱数の名前 (英数字 10 文字) |

- ライブラリの側の変更は、この module と、置き場 1 を差し替えられる `base::places_with_tmp` (`places_from` はそれに `/tmp` を渡す)、`route::FOLLOWS` を `pub(crate)` にすることだけ。ライブラリの振る舞いは変えない。`$PWD` の候補はライブラリが bind・resolve で作る物 ([DR-0032](DR-0032-resolve-through-persistent-base.md)「相対パスを絶対パスにする」) をそのまま使う

### `resolve` の作り方 (ハブ DR-0018 の手順 1〜7 を Rust で)

- 相対パスは絶対パスにしてから扱う (出した綴りを別の cwd のプロセスが使う。ハブ [DR-0019](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0019-absolutize-relative-path-on-bind.md)「絶対パスの作り方と、その後」と cli.md「終了コードとエラー」)。同じ所を指す絶対パスの候補を、上限に収まる最初の物が見つかるまで順に見る。収まればそれを出し、置き場には何も作らない
  1. 実パス: ソケットのディレクトリを realpath で実パスにし、ファイル名を付ける
  2. `$PWD` からの綴り: `$PWD` が絶対パスで、`stat` の `(st_dev, st_ino)` が `.` と同じ時だけ、相対パスを文字列でつなぎ、`.` と `..` を字句で畳む。畳んでできたディレクトリの `(st_dev, st_ino)` が元の相対パスのディレクトリと同じ時だけ候補にする (`sym/..` はカーネルでは symlink の先の親で、字句で畳むと別のディレクトリになりうる)。作るのは入口の `pwd_candidate` (ライブラリと同じ関数)
- 候補が収まれば `--base` を確かめない (形の検査だけは引数の誤りとして先に行う。ライブラリが `Options.base` を候補の時に見ないのと同じ、ハブ SPEC「相対パスを絶対パスにする」)
- どちらも収まらなければ、B の直下のハッシュの E を通す (手順 2〜7)。ライブラリの DR-0019 の 5 (経路が使えない時に相対のまま標準に渡す) は、コマンドでは取らない。コマンドは相対の綴りを出さない (cli.md) ので、E も作れない時はエラー
- 上限を超える絶対パスにも実パスの候補を試す (途中の symlink で長く見えるだけのパスがある)。収まればそれを出し、置き場に何も作らない (`route` は `standard`、`link`・`target` は無い)。収まらなければ E を通す。`$PWD` の候補は相対パスだけ。上限以下の絶対パスは何も確かめずにそのまま出す
- 相対パスのソケットのディレクトリが無い時は、長さによらずエラー (終了コード 1、`ENOENT`)。実パスの候補も E も作れないため。絶対パスの短いパスは何も確かめずにそのまま出すので、そこは扱いが分かれる
- `--json` の `original` は渡されたままの文字列。候補を出した時の `route` は `standard` (ライブラリの DR-0019 の 3 で標準に渡す時の経路の名前と同じ)
- 手順 1 の長さは `sun_path` − 1 (`LIMIT`)。それ以下のパスは何も作らずにそのまま出す
- パスは入口の `split` (ライブラリの `path::split`) で分ける。名前で終わらないパス (`dir/`・`dir/.`・`dir/..`・`..`・`/`) は、短いパスでも長さによらず引数の誤り (終了コード 2)。ライブラリが長さによらず `InvalidInput` にするのと揃える
- 手順 3 の B は、`--base` があればその B (入口の `check_given` で作らずに確かめ、綴りが収まると見積もれた時だけ `given` で作る)、無ければ入口の `automatic`。見積もりは「`B/<ハッシュ>/<名前>` が収まる、またはファイルを指す E がその用途に使えて `B/<ハッシュ>` が収まる」(ファイルを指す E は、`--for connect` と `--for` 無しならどの OS でも、`--for bind` は bind が symlink を辿る OS だけ使える)。見積もりに外れると B を作らずに止まる。その時は、入口の `automatic` が最後に見積もりに渡した B のパス (作った B か、作るはずだった B) で、ファイルを指す E なら収まるかを見て、収まるなら (`--for bind` で bind が symlink を辿らない OS) 「この OS では bind に使える綴りを作れない」を、収まらないなら「収まらない」をエラーにする
- 手順 4・6 の E は B の直下。名前は指す先 (ディレクトリの E はソケットのディレクトリの realpath、ファイルの E はその後ろにファイル名を付けた物) のバイト列の SHA-256 の 16 進小文字の先頭 12 文字。`symlink` が `EEXIST` なら `readlink` で指す先を比べ、同じなら使い、違えば `<ハッシュ>.<乱数 10 文字>` に作って `rename` で置き換える (失敗したら作った物を消す)
- E の名前 (16 進 12 文字) も置き換えの途中の名前 (`.` を含む) も D の形 (英数字 10 文字) でないので、`sweep` は触らない。ライブラリの `sweep` は変えない (ハブが SPEC の「置き場に残る物」「sweep」に CLI の E を足す)
- 手順 6 は、`--for connect` ならどの OS でも、`--for bind` は `BIND_FOLLOWS_SYMLINK` の OS だけ出す (他の OS の `--for bind` は終了コード 1 のエラー)。`--for` が無ければ connect 用の綴りとして出し、終了コード 0 (kawaz 裁定 CL-Q1 = b、cli.md「ファイル名が長い時」)
  - ファイルを指す綴りは、どの OS でも bind には勧めない。`--for` 無しの時は、標準エラーに理由つきの警告を 1 行出す。標準出力は綴りだけ
    - bind が symlink を辿らない OS: `sockat: warning: this path is for connect; not recommended for bind (bind does not follow the symlink on this OS)`。`for` は `["connect"]`
    - 辿る OS (macOS・NetBSD): bind はできるが、この綴りで unlink しても symlink が消えるだけでソケットのファイルが残る (`removable: false`)。`sockat: warning: this path is for connect; not recommended for bind (unlinking this path removes only the symlink, not the socket file)`。`for` は `["bind", "connect"]`
  - `--for connect` の時は警告を出さない
  - `--for bind` で、bind が symlink を辿る OS では綴りを出し、`sockat: warning: unlinking this path removes only the symlink, not the socket file (remove the socket by its original path)` を出す (cli.md に例が無いので決めた文)。辿らない OS では終了コード 1
  - `--json` の時は標準エラーに出さず、`warnings` (文の配列) に `sockat: warning: ` を除いた同じ文を入れる。`warnings` はどの `--json` の出力にも置き、警告が無ければ空の配列 (cli.md の表の「無ければ空」)
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
- Linux の gnu の binary を 2 つとも cross で作るのは、要る glibc を低く揃えるため。ubuntu-latest (24.04、glibc 2.39) で直に build すると `__libc_start_main@GLIBC_2.34` 等が入り、glibc 2.34 より古い Linux (Ubuntu 20.04・Debian 11・RHEL 8) で動かない (実測: 24.04 で build した binary の要る版は最大 2.34、cross の image で build した物は最大 2.18)
- musl の binary を置くのは、static link で glibc の版にも有無にも依らないため (Alpine、2.18 より古い glibc)。Alpine (aarch64、docker の既定の seccomp) で workspace の試験が全部通り、cross で作った binary が glibc の無い busybox の上で動く (aarch64・x86_64) ことを確かめた
- Formula の Linux は gnu の binary にする (Homebrew の Linux は glibc の環境。kawaz の他の Formula と同じ)
- build と publish を並べるのは、build が publish に依らないため。Apple の側 (notarize の同意の期限切れ等) で build が落ちても、ライブラリの release は止まらない。tag は両方の後なので、publish が落ちれば tag は残らない
- publish を出し直さないのは、publish の後の失敗 (build、release) をやり直す時に、出した版の publish で止まらないため
- FreeBSD・NetBSD・OpenBSD・illumos の binary は置かない。cross 0.2.5 で FreeBSD と illumos (x86_64 だけ) は build できたが、NetBSD は link で落ち (image に `libexecinfo` が無い)、OpenBSD は cross が無い (Rust の tier 3)。CI の VM は試験を回すが、配る binary を動かす所が無い。build の job が 1 つ落ちれば release が止まるので、確かめられない target を増やさない。これらの OS は `cargo install sockat-cli` で入る

悪い面:

- feature `__cli-internal` は crates.io の feature の一覧と `cargo add` の表示に出る (実測: `cargo add --dry-run` が `- __cli-internal` を並べた)。名前と注記で「使わない」を示すだけで、使うことは止められない
- ライブラリだけを直したリリースでも、コマンドの version が上がり出し直しになる (中身はライブラリの差だけ)。固定の版のライブラリを使うので、出し直さないと公開されたコマンドは古いライブラリのまま
- 試験の置き場の口は debug build にしか無いので、`cargo test --release` ではそれに頼る試験が `ignore` になる
- エラーの文を選ぶのに、入口の `automatic` が見積もりの関数に渡したパスを覚えて使う (`automatic` は見積もりに外れた B のパスを返さない)。`automatic` が見積もりを呼ぶ回数や順が変わると、エラーの文がずれうる (綴りと終了コードは変わらない)
- 補完はトップレベルのパスとオプションと、サブコマンドの名前を同じ位置に並べる (パスの補完の候補にサブコマンドの名前が混ざる)
- 絶対パスの短いパスは何も確かめずにそのまま出すが、相対パスは短くてもディレクトリを引くので、ディレクトリが無いとエラーになる (相対パスだけ扱いが厳しい)
- 深い cwd (realpath が PATH_MAX を超える) の相対パスは、候補も E も作れずエラーになる。ライブラリなら DR-0019 の 5 で相対のまま標準に渡せる所
- Apple の側で build が落ちると、crates.io に出た版に tag と GitHub release が無いまま、直してやり直すまで残る
- 配る binary は release の前に 1 度も動かさない (build するだけ)。cross の image の toolchain は CI の試験の build と別物
- cross 0.2.5 の image は古い (Ubuntu 16.04〜20.04)。Rust が要る glibc やリンクする物を増やすと、gnu や musl の build も NetBSD のように落ちうる
- update-homebrew だけが落ちた時は、同じ run の `gh run rerun --failed` でしかやり直せない

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
- 案 H: x86_64 の gnu の binary を ubuntu-latest で直に build する (kawaz/hyoui の形)
  - 不採用理由: glibc 2.34 以上が要り、aarch64 (cross、2.18) と要る版がずれる
- 案 I: Formula の Linux に musl の binary を使う
  - 不採用理由: gnu の binary も 2.18 まで動き、Homebrew の Linux の環境で足りる。kawaz の他の Formula と形を揃える
- 案 J: binary の build を crates.io の publish の後に置く
  - 不採用理由: 署名・notarize の都合でライブラリの release まで止まる。publish を出し直さないので、並べてもやり直しは効く
- 案 K: FreeBSD・illumos の binary も cross で作って置く
  - 不採用理由: 配る binary を動かして確かめる所が無く、落ちれば release 全体を止める (「理由」の BSD の項)

## Consequences

- コマンドの試験は `cli/tests/cli.rs`。`just test` と `just test-linux*`、CI の各 OS の VM は `--workspace` で回す。clippy と MSRV の check も workspace を見て、clippy は feature なしのライブラリも別に見る。`just lint-unsafe` は `cli/` と `examples/` も見る
- README (日英) と MANUAL にコマンドの節を足すのは公開の後 (ハブ DR-0018 の Consequences)

## 関連

- ハブ: [DR-0018](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0018-cli-resolve-with-reused-hashed-link.md)、[DR-0019](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0019-absolutize-relative-path-on-bind.md)、[cli.md](https://github.com/kawaz/sockat/blob/main/docs/design/cli.md)、[SPEC-routes「B の作り方」](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md#b-の作り方)、[DR-0017](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0017-resolve-through-persistent-base.md)
- [DR-0011](DR-0011-routes-not-exposed.md)、[DR-0016](DR-0016-crate-distribution.md)、[DR-0017](DR-0017-release-flow.md)、[DR-0032](DR-0032-resolve-through-persistent-base.md)
