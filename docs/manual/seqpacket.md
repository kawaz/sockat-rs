# Use SEQPACKET

> English | [日本語](./seqpacket-ja.md)

sockat has no functions of its own for SEQPACKET. Hand the path made by [Resolve only the path](./resolve.md) to a SEQPACKET socket you make yourself. Clean up as on the resolve page.

- The side that listens (binds): hand the path made by `resolve_for_bind` to the bind ([Bind with std](./resolve.md#bind-with-std))
- The side that connects: hand the path made by `resolve_for_connect` to the connect ([Connect to the same peer many times](./resolve.md#connect-to-the-same-peer-many-times))
