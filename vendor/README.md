# Vendored RDP Dependencies

This directory contains local copies of upstream crates used by NyaTerm's RDP stack. Keep patches small and documented so the crates can be refreshed without turning them into long-lived forks.

## ironrdp-client

- Upstream: https://github.com/Devolutions/IronRDP
- Crate/version: `ironrdp-client` `0.1.0`
- Why vendored: NyaTerm needs runtime hooks that are not exposed by the published client API.
- NyaTerm changes:
  - Adds an injectable server certificate verifier called after TLS/RDCleanPath certificate extraction and before RDP finalize.
  - Adds an injectable CLIPRDR backend factory so NyaTerm can provide a text-only clipboard bridge instead of the native clipboard backend.
  - Exposes the clipboard module for non-Windows builds when the clipboard feature is enabled.

## ironrdp-connector

- Upstream: https://github.com/Devolutions/IronRDP
- Crate/version: `ironrdp-connector` `0.10.0`
- Why vendored: It must stay in lockstep with the vendored IronRDP client and the current connector API used by NyaTerm.
- NyaTerm changes: no intentional feature patches in this round; keep local edits limited to compatibility fixes required by the client.

## picky

- Upstream: https://github.com/Devolutions/picky-rs
- Crate/version: `picky` `7.0.0-rc.25`
- Why vendored: Required by IronRDP/SSPI auth dependencies and pinned to match the vendored connector stack.
- NyaTerm changes: no intentional RDP behavior patches in this round.

## sspi

- Upstream: https://github.com/Devolutions/sspi-rs
- Crate/version: `sspi` `0.21.0`
- Why vendored: CredSSP/NLA support must remain compatible with the vendored IronRDP connector and pinned `picky` version.
- NyaTerm changes: no intentional RDP behavior patches in this round.

## vnc-rs

- Upstream: https://github.com/HsuJv/vnc-rs
- Crate/version: `vnc-rs` `0.5.3`
- Pinned revision: `ab684d009d767c968af2f7559576334038623124`
- License: `MIT OR Apache-2.0`; both upstream `LICENSE-MIT` and `LICENSE-APACHE` are preserved in `vnc-rs/`, together with upstream author attribution in its manifest.
- Why vendored: The published client parser contains network-reachable unsafe conversions, uninitialized buffers, panics, and unbounded server-controlled allocations that must be hardened before NyaTerm integration.
- NyaTerm changes:
  - Forbids unsafe code and replaces unsafe enum conversion/uninitialized buffers/copies with checked safe code.
  - Adds typed errors for unknown security types/results/encodings and unsupported messages.
  - Correctly validates the RFB 3.8 None `SecurityResult` and uses exact-length reads for failure strings.
  - Adds configurable limits for strings, dimensions, rectangles, encoded/decoded payloads, clipboard data, and channels.
  - Reduces internal channel capacity and adds deterministic protocol/parser regression tests.
  - Adds an explicit security policy so NyaTerm can fail closed for `none`, `vnc-auth`, or password-aware `auto`.
  - Adds RA2_256 (security type 129) with bounded RSA key exchange, AES-EAX encrypted records, username/password subtypes, and a pre-credential server-key verification callback used by NyaTerm's TOFU flow.
- Integration status: used by NyaTerm's VNC direct-TCP manager and React pane. Raw is the required fallback; compressed encodings must remain gated by fork tests and interoperability checks before being advertised.

## Update Method

1. Record the current NyaTerm patches with `git diff -- src-tauri/vendor/<crate>`.
2. Replace the target crate from the matching upstream tag or revision.
3. Reapply only the documented NyaTerm patches, preferring upstream APIs if they now exist.
4. Run `cargo update --manifest-path src-tauri/Cargo.toml -p <crate>` if dependency metadata changed.
5. Run `cargo fmt`, `cargo test`, and `cargo clippy` for `src-tauri/Cargo.toml`.
6. Update this README with the new version/revision and any changed local patches.
