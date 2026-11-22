# SEQPACKET を使う

> [English](./seqpacket.md) | 日本語

sockat に SEQPACKET 専用の関数は無い。[パスだけを解決する (resolve)](./resolve-ja.md) で作った短いパスを、自分で作った SEQPACKET のソケットの bind / connect に渡せばよい。片付けも resolve のページと同じ。
