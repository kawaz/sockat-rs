# DR-0034: コマンドの経路の名指し (`--route`、`bind`・`connect`) と持ち主の fd の置き場 (`--base-fd`・`--base-pid`)

- Status: Accepted
- Date: 2026-11-24

## Context

- ハブの [DR-0018](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0018-cli-resolve-with-reused-hashed-link.md) と [cli.md](https://github.com/kawaz/sockat/blob/main/docs/design/cli.md) の「進んだオプション」「手法を試すサブコマンド」が、`--route`、`--base-fd`・`--base-pid`、`sockat bind`・`sockat connect` を決めた。どれも `--help-all` にだけ載せ、名前も出力も約束しない
- 経路を名指しする口はライブラリの公開の API に置かない (ハブ DR-0017・DR-0018、[DR-0011](DR-0011-routes-not-exposed.md))。コマンドはライブラリの CLI 専用の入口 `sockat::__cli` ([DR-0033](DR-0033-cli-crate-and-internal-entry.md)) を使う
- ライブラリの試験は、経路の順 (`route::run`) に環境を差し替えた入力 (`route::Config`) を渡して各経路を強制する (DR-0011)。各経路は `Step` を返す関数 (`through` と `bind_through`、`link_file_bind`、`thread_cwd_bind`、`linux::proc_direct` 等) で、順はそれを並べて呼ぶ
- cli.md に書いていない細部 (名指しの時の順の扱い、出力の書式、エラーの終了コード、持ち主の fd の確かめ) をここで決める

## Decision

### ライブラリの側

- 経路の順の関数に、名指しの並びを渡す口を足す。どれも `pub(crate)` で、公開の API と doc に出る物は変えない

  | 物 | 中身 |
  |---|---|
  | `route::run_by` | `route::run` に名前の並び (`Option<&[&str]>`) を足した物。`None` は今の順 (`run` はこれに `None` を渡す) |
  | `route::run_named` | 名前の並びを、その順に 1 つずつ試す。各経路は今の順と同じ関数を呼ぶ |
  | `route::nameable` | その操作 (bind か connect) がその OS で取れる経路の名前。その操作の順の表 (ハブ SPEC「bind の順」「connect の順」) に並ぶ物で、環境によらない (Linux の connect は `/proc` の有る行と無い行の両方、proc-direct と relative) |
  | `route::DEFAULT` | 名前の並びの中で、その操作の順を指す名前 `default` |
  | `bind_by`・`connect_by` (lib.rs) | `bind_with`・`connect_with` に `route::Config` と名前の並びを足した物 |

- 入口 `sockat::__cli` に足す物 (DR-0033 の表に足す)

  | 物 | 中身 |
  |---|---|
  | `Op` | `Bind`・`Connect` |
  | `DEFAULT` | `route::DEFAULT` |
  | `routes(op)` | `route::nameable` |
  | `bind(places, path, options, routes)` | `bind_ex` と同じ (`SocketFile` を返す bind)。B の置き場は `places`、`routes` が `None` か `default` だけなら `bind_ex` そのもの |
  | `connect(places, path, options, routes)` | `connect_ex` と同じ (ブロックする stream) |
  | `open_held(&Path)` | `--base-fd` のディレクトリを開いて確かめる (下の「持ち主の fd の置き場」) |
  | `error_name(&io::Error)` | errno の C の名前 (ハブ conformance.md の `error` と同じ規則)。コマンドは `libc` を持たないので、ライブラリの側に置く |

### 名指しの順 (`run_named`)

- 並べた名前の順に試す。並べていない経路は試さない。`skipped` に入るのは試して使えなかった物だけ (試した順)
- 試した経路が使えない時 (このパスでは綴りが収まらない、この環境では使えない (seccomp、`/proc` を使えない、B を作れない等)) は、理由を `skipped` に残して次の名前に進む。経路で実際に bind / connect して失敗したら、そのエラーを返して次を試さない (今の順と同じ。ハブ SPEC「診断」)
- 並べた経路がどれも使えなければ `ENAMETOOLONG` (今の順の 7 と同じ)
- 標準ライブラリにそのまま渡せる短いパスでも、`standard` を並べていなければ経路を試す (名指しした経路が何を作るかを見るため)。`standard` は、パスそのものか絶対パスの候補 (今の順の 1。長い絶対パスの実パスの候補を含む)
- `default` は、そこにその操作の順を置く (`standard` の後に bind の順・connect の順)。`default` だけなら名指しの無い呼び出しと同じ (入口は `bind_ex`・`connect_ex` そのものを呼ぶ)
- bind の相対パスを相対のまま標準ライブラリに渡す (`CwdRelative`。ハブ SPEC「相対パスを絶対パスにする」の 5) のは、並べた経路がどれも使えず、並びに `standard` か `default` がある時だけ
- その操作かその OS に無い名前は `InvalidInput` (何も引かず、何も作らない)。コマンドは引数のパーサの段で止める (下)

### コマンドの `bind`・`connect`

- `bind` は入口の `bind` (= `bind_ex`) で bind し、結果を出した後、`--keep` が無ければ `SocketFile::remove()` (ソケットのファイルと E) を呼び、listener を閉じる。`remove()` が失敗したら、標準エラーに原因と手で消す物を出して終了コード 1
- 綴りが link-dir の時、E はその bind の数の E (`bind_ex` の物) で、既定の `bind` が使い回す E ではない
- コマンドのプロセスの D は残る (ライブラリは D を消さない)。コマンドが終われば持ち主の死んだ D なので、`sockat sweep` と OS の掃除で消える。`--keep` の E もその D の中にあり、D と一緒に消える
- `connect` は入口の `connect` (= `connect_ex`) で connect し、結果を出して閉じる
- `kernel_path` は、`bind` は listener の `getsockname(2)`、`connect` は stream の `getpeername(2)` のパス。ライブラリが試験で綴りを読むのと同じく、カーネルが書いたアドレスの `sun_path` を長さの中で最初の NUL までと読む (入口の `local_path`・`peer_path`)。std の `SocketAddr` は通さない (Rust 1.99 以前は OpenBSD でパスの最後の 1 バイトを落とす。rust-lang/rust#118925。CI の OpenBSD の VM の Rust は 1.98.1)。パスの無いアドレスは `null`
- `left` は、通った経路が link-dir か link-file の時の、その bind の E。E の実パス (B の実パス、D、E。入口の `link_path`) で出す。綴りが procB でも、`/proc/<pid>/fd/<fd>` はコマンドが終わると辿れないので使わない。表示の時点で symlink として在る時だけ載せる。D・B は載せない (どちらも `--keep` によらず残り、`remove()` で消す物でない)
- 既定の bind の link-dir の `base` は、`/proc/<pid>/fd/<fd>` が fd と同じ物を指す所では `proc` (ハブ SPEC-routes「綴りの形」。OS で決めない)。CI では Linux・NetBSD・illumos が `proc`、macOS・FreeBSD・OpenBSD が置き場の名前 (2026-11-24)
- 標準出力は、1 行に 1 項目 `名前: 値`。無い値は `-`

  | 場面 | 項目 |
  |---|---|
  | `bind` が通った | `route`、`base`、`kernel-path`、`spelling`、`skipped`、`left` (E ごとに 1 行、`<E> -> <指す先>`) |
  | `connect` が通った | `route`、`kernel-path`、`skipped` |
  | 通らなかった | `error` (`<errno の名前>: <エラーの文>`)、`route`、`skipped` |

  `spelling` は `Original`・`CwdRelative`・`Dangling`、`Linked (<removable か not removable>, <until cleanup か until process>, <usable cross uid か not ...>, <usable with private tmp か not ...>)`。`skipped` は `<名前> (<理由>)` を `, ` でつなぐ。理由はライブラリの `Skipped::reason`

- `--json` の項目は cli.md の順 (`ok`、`error`、`route`、`base`、`spelling`、`skipped`、`kernel_path`、`left`)。`spelling` は適合試験の CLI と同じ形。`skipped` は `{ "name", "reason" }` の並び。`connect` と失敗の時の `base`・`spelling` は `null`、`left` は空
- `--json` に出すパスは `kernel_path` と `left` だけで、渡されたパスは出さない (cli.md の項目に無い)。出すパスが UTF-8 でなければ、`resolve` と同じく JSON に書けないので、標準出力に何も出さず終了コード 1 (`bind` は、その後で `--keep` が無ければ片付ける)。渡されたパスが UTF-8 でなくても、出すパスが全部 UTF-8 なら JSON を出す (例: 親が UTF-8 でないディレクトリのソケットを link-dir の B の綴りで bind した時)
- 通らなかった時は、結果を標準出力に出して終了コード 1。ただしライブラリが errno の無い `InvalidInput` を返した時 (空のパス、NUL、名前で終わらない長いパス、`--base` の形) は引数の誤りとして、標準エラーに出して終了コード 2 (標準出力には何も出さない。`resolve` と同じ)
- `--route` の名前は、その操作とその OS の名前 (入口の `routes`) と `default` だけを引数のパーサ (clap の possible values) が受ける。他の名前は clap のエラー (終了コード 2) で、使える名前が `[possible values: …]` に並ぶ。`--for` の誤りと同じ形
- `--route` は `,` で区切っても、繰り返してもよい

### `resolve` の `--route`

- 名前は `standard`・`link-dir`・`link-file`・`default` (`default` は `standard,link-dir,link-file`)。並べた順に試す
- 綴りが収まらない経路は飛ばす。`--for bind` で bind が symlink を辿らない OS の `link-file` も、使えない経路として飛ばす (理由は `bind does not follow the symlink on this OS`)
- どれも使えなければ終了コード 1。文は `none of the routes named can be used for this path: <名前> (<理由>), …` と、次にすること (`name other routes, or leave out --route`)
- B を選ぶ時の見積もりは、並べた link-dir・link-file のどれかの綴りが収まるか (今は両方を見る)。B を作れない等のエラーは今と同じ文
- `--route` が無い時と `default` だけの時は、今の順と今のエラーの文のまま

### 持ち主の fd の置き場 (`--base-fd`・`--base-pid`)

- `resolve` だけが受ける (cli.md の `bind`・`connect` の呼び方に無い)
- `--base-fd` は 0 から `i32` の最大まで、`--base-pid` は 1 以上の整数。片方だけ、または `--base` と一緒は引数の誤り (終了コード 2)。文は `sockat: --base-fd needs --base-pid, …` / `--base-pid needs --base-fd, …` / `--base and --base-fd both give the base` と、次にすること
- Linux・Android 以外では、`/proc` が無いので引数の誤り (終了コード 2、`--base` を勧める)
- B を使う経路に来た時に、`/proc/<PID>/fd/<N>` を探す権限だけで開く (Linux は `O_PATH`。`--base` の B を開くのと同じで、持ち主が読めない B (mode 0311 等) も使える)。開けなければ終了コード 1 (別の uid、`hidepid`、そのプロセスや fd が無い)
- 開いたディレクトリを、`--base` の B 自身と同じに確かめる (入口の `open_held`): ディレクトリで、この利用者の物で、group と other が書けない。外れたら終了コード 1。途中の確かめは無い (途中はそのプロセスの fd で、他人が変えられない)
- `/proc/self/fd/<コマンドの fd>` と `/proc/<PID>/fd/<N>` の両方が、開いたディレクトリと同じ `(st_dev, st_ino)` を指すことを確かめる (ライブラリが `/proc` をそう確かめてから使うのと同じ。ハブ SPEC「用語」)。違えば終了コード 1
- E は `/proc/self/fd/<コマンドの fd>/<ハッシュ>` で作り、綴りは `/proc/<PID>/fd/<N>/<ハッシュ>/<名前>` (link-file は `/proc/<PID>/fd/<N>/<ハッシュ>`)。作り方と名前は `--base` の B と同じ (DR-0033「`resolve` の作り方」)。綴りが収まらなければ、`--base` と同じく何も作らずエラー
- 候補が収まる時 (`standard`) は fd を開かない (`--base` を確かめないのと同じ)

### `--help` と `--help-all`

- `--help-all` は、進んだオプションと手法を試すサブコマンドを見せた定義 (`command(true)`) の `--help` を出す。引数の `--` より前に `--help-all` があれば、その定義で引数を読む
- 進んだオプションは、見せる時は見出し `Advanced options` の下に置き、見せない定義では `hide` で隠す。`bind`・`connect` は見せない定義で `hide` にする
- 補完の定義は見せた定義から作る (進んだオプションとサブコマンドも補える)

理由:

- 名指しの順を今の順と同じ関数の並べ替えにすれば、名指しで試した経路は既定の順の中と同じ動きをする (確かめたい物が同じになる)。経路の関数を公開せず、入口に「名前の並びを渡せる `bind_ex`・`connect_ex`」を置くだけで済む
- 試した経路が環境で使えない時に飛ばすのは、経路の関数が「使えない」と「本当の失敗」を分けて返すのをそのまま使うため。理由は `skipped` に出るので、1 つだけ名指しして使えなければ、終了コード 1 と理由で分かる
- 短いパスで名指しの経路を試すのは、`--route link-dir` の目的が link-dir の綴りの性質を見ることだから。短いパスで黙って `standard` にすると、名指しが効かない
- `default` だけを名指しの無い呼び出しにそろえるのは、cli.md の「`--route` を付けないのと同じ」をライブラリの呼び出しそのもので守るため
- `bind` を `bind_ex` にするのは、cli.md が `remove()` と `cleanup()` で片付けると決めたため (既定の `bind` は `SocketFile` を返さない)
- `left` を E の実パスでライブラリから取るのは、綴りから求めると OS で結果が違うため。procB の綴り `/proc/<pid>/fd/<fd>/D/E` の realpath は、Linux・NetBSD では B の実パスになった。illumos では、コマンドが終わった後にそのパスで E が見つからなかった (`--keep` の試験が `ENOENT`、CI 2026-11-24)。D は自分の実パス (B の実パスと D の名前) を持つ (feature `__cli-internal` の時だけ。ライブラリの振る舞いは変えない)
- `InvalidInput` を終了コード 2 にするのは、`resolve` が同じ誤り (名前で終わらないパス、空、`--base` の形) を 2 にしているのとそろえるため
- `--route` の名前の誤りを clap に任せるのは、使える名前の一覧と補完の候補を同じ定義から作れるため (`--help`・補完・実装を同じ定義でそろえる)
- 持ち主の fd の B を確かめるのは、`--base` の B と同じ理由 (他人が B に書けると、E を差し替えて別のソケットに繋がせられる)。E をコマンドの fd を通して作るのは、確かめたディレクトリに作るため (持ち主がその間に fd を閉じて番号を使い回しても、確かめていない所に作らない)
- `--help-all` を見せた定義の `--help` にするのは、clap がサブコマンドを `--help` と長い help の両方から隠し、長い help はオプションの説明を次の行に置く書式に変えるため。見せた定義の `--help` なら、`--help` の行がそのまま `--help-all` に含まれる

悪い面:

- `--help-all` を見つけるのに、引数の解析の前に引数を一度見る (`--` より前の `--help-all`)。オプションの値として `--help-all` という文字列を渡す場合も、help として読む
- `--route` の名前の誤りの文は clap の形 (`error: invalid value …`) で、`sockat: ` で始まる他のエラーの文と形が違う (`--for` の誤りと同じ)
- 環境で使えない経路 (seccomp の下の thread-cwd 等) を名指しすると、引数の誤りでなく終了コード 1 になる。ハブ DR-0018 の「その環境で使えない手法は使えない理由つきのエラーにする」を、「`skipped` に理由が出る終了コード 1」で満たす読み方をした
- `left` は E の実パスを、D を作った時の B の実パスから作る。その後に B の置き場が rename されると、実際の場所と合わない (表示だけの話で、片付けは fd で行う)
- `bind` のたびにコマンドのプロセスの D が 1 つ残る (sweep まで)
- errno の名前の表が、適合試験の CLI (`examples/conformance.rs`) と入口の 2 か所にある (適合試験の CLI は公開の API だけを使う。DR-0030)
- 持ち主の fd の B は、B 自身の確かめが `--base` より緩い (途中の確かめが無い。途中は `/proc` の fd)。`hidepid` 等で別の uid の fd は開けないので、他人の B を使うことは無い

## Alternatives Considered

- 案 A: 経路の関数を入口に 1 つずつ公開し、コマンドが順を組む
  - 不採用理由: `Target`・`Step`・`Config` 等の内部の型を入口に出すことになり、入口の面が大きくなる。順の組み方 (B を 1 度だけ選ぶ、`skipped` の付け方、`CwdRelative` の扱い) をコマンドに写すことになる
- 案 B: 名指しを `route::Config` の差し替え (他の経路を使えなくする) で行う (試験と同じ形)
  - 不採用理由: 並べた順に試せない (順は OS の表で決まる)。差し替えの口は試験用で、使えなくした経路も `skipped` に入る
- 案 C: 環境で使えない経路の名指しを終了コード 2 にする
  - 不採用理由: 環境で使えるかは試すまで分からない物がある (`unshare` が拒まれる、B を作れない)。試す前に分かる物だけ 2 にすると、同じ「使えない」が 2 と 1 に分かれる
- 案 D: `--help-all` を clap の長い help (`HelpLong`) にし、サブコマンドの一覧は `after_long_help` の文で足す
  - 不採用理由: 長い help の書式が変わり、`--help` の行が `--help-all` にそのまま現れない。サブコマンドの一覧を手で書くことになる
- 案 E: `--base-fd` の B を確かめない (利用者が開いた物を信じる)
  - 不採用理由: `--base` の B と守る物が同じなのに確かめが無くなる

## Consequences

- 試験: ライブラリは `src/tests.rs` (名指しの順、短いパスでの名指し、使えない時、`default`、connect の link-file)。コマンドは `cli/tests/cli.rs` (`bind`・`connect` の既定の順と各経路の名指し、OS に無い名前、飛ばし、`--keep`、`--json` (UTF-8 でない親のパスを含む)、`resolve --route`、`--base-fd` (持ち主が読めない B を含み、同じ B を `--base` でも)、`--help-all`、補完)
- ハブ cli.md の利用者向けの説明 (`pages/`) に、`bind`・`connect` の出力の形を書くかはハブで決める

## 関連

- ハブ: [DR-0018](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0018-cli-resolve-with-reused-hashed-link.md)、[cli.md](https://github.com/kawaz/sockat/blob/main/docs/design/cli.md)、[conformance.md](https://github.com/kawaz/sockat/blob/main/docs/design/conformance.md)、[findings 2026-11-24](https://github.com/kawaz/sockat/blob/main/docs/findings/2026-11-24-cli-base-fd-and-vanished-link.md)
- [DR-0011](DR-0011-routes-not-exposed.md)、[DR-0030](DR-0030-conformance-cli.md)、[DR-0033](DR-0033-cli-crate-and-internal-entry.md)
