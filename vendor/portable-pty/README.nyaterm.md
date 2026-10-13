# NyaTerm portable-pty patch

This directory starts from the crates.io release of `portable-pty 0.9.0`
(MIT license, see `LICENSE.md`). NyaTerm changes only its Windows ConPTY
loader and adds a test for creation failure.

The patch accepts an absolute path to a bundled `conpty.dll`, requires the
matching `OpenConsole.exe` host files, and falls back to `kernel32.dll` when
loading or `CreatePseudoConsole` fails. Each PTY retains its selected function
table for resize and close. The status API reports active backends, the last
successful backend, and whether fallback occurred.

When updating this crate, compare `src/win/psuedocon.rs` and `src/win/mod.rs`
against the new upstream release and rerun the Windows backend tests.
