# SEQPACKET を使う

> [English](./seqpacket.md) | 日本語

sockat に SEQPACKET 専用の関数は無い。[パスだけを解決する (resolve)](./resolve-ja.md) で作ったパスを、自分で作った SEQPACKET のソケットに渡せばよい。片付けも resolve のページと同じ。

- 待ち受ける (bind する) 側: `resolve_for_bind` で作ったパスを bind に渡す ([標準ライブラリで bind する](./resolve-ja.md#標準ライブラリで-bind-する))
- つなぐ (connect する) 側: `resolve_for_connect` で作ったパスを connect に渡す ([同じ相手に何度もつなぐ](./resolve-ja.md#同じ相手に何度もつなぐ))
