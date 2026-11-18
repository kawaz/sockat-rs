# 退避した Decision Records

後続の DR に置き換えられた DR。各ファイルの `Status: Superseded by DR-NNNN` が正本で、現役の文書からはここも各ファイルも名指ししない (置き換え先を指す)。

| DR | 説明 | 置き換え先 |
|---|---|---|
| [DR-0001](DR-0001-long-socket-path-routes.md) | sun_path を超えるパスに届く経路と OS ごとの試す順番 (/proc → 相対パス → symlink → スレッド cwd → fork) | [DR-0027](../DR-0027-own-directory-and-fallback-routes.md) |
| [DR-0003](DR-0003-linux-proc-self-fd.md) | Linux は `/proc/self/fd/<dirfd>/<name>` を bind / connect に使い、使えない時だけ他の順 | [DR-0027](../DR-0027-own-directory-and-fallback-routes.md) |
| [DR-0008](DR-0008-freebsd-bindat-and-other-unix.md) | FreeBSD は bindat / connectat、dirfd 基準の primitive の無い Unix は相対パス → symlink → fork | [DR-0027](../DR-0027-own-directory-and-fallback-routes.md) |
| [DR-0020](DR-0020-thread-cwd-route.md) | スレッドだけ cwd を移す経路を symlink の後、fork の前に置く | [DR-0027](../DR-0027-own-directory-and-fallback-routes.md) |
| [DR-0023](DR-0023-unsafe-base-held-by-fd.md) | /proc を使える Linux の connect は安全でない置き場に自分専用のディレクトリを作り fd で掴む | [DR-0027](../DR-0027-own-directory-and-fallback-routes.md) |
| [DR-0024](DR-0024-apple-bind-without-widened-sockaddr.md) | Apple の OS の bind は広げた sockaddr を使わない | [DR-0027](../DR-0027-own-directory-and-fallback-routes.md) |
| [DR-0022](DR-0022-connect-any-behind-feature.md) | connect_any 系は feature `any` (既定で無効) の後ろ。docs.rs は全 feature、試験と CI は `--all-features` | [DR-0029](../DR-0029-any-functions.md) |
| [DR-0026](DR-0026-any-connection-and-feature-any.md) | connect_any 系の戻り値は公開フィールドの struct `AnyConnection { stream, forked }`。feature 名は `any` | [DR-0029](../DR-0029-any-functions.md) |
