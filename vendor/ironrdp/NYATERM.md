# NyaTerm IronRDP snapshot

- Upstream: `https://github.com/Devolutions/IronRDP`
- Commit: `b149f500b85124c513646494335fb6cee525d897`
- Snapshot date: `2026-09-22`

The workspace is kept as one coherent source snapshot. NyaTerm-specific changes must be
documented here and kept narrowly scoped so a future upstream refresh can be rebased as a
single unit.

## Local changes

- `ironrdp-client` accepts an application-provided direct transport stream so NyaTerm proxy and
  jump-host connections do not bypass the shared network stack.
- `ironrdp-client` accepts an asynchronous post-handshake certificate verifier. This keeps
  NyaTerm's known-host prompt and fingerprint policy authoritative with the Windows native-TLS
  backend, whose upstream validation callback is intentionally unavailable.
