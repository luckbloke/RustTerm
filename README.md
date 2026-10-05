# RustTerm

A Windows desktop terminal built on **Tauri 2 + Rust + xterm.js**, bringing SSH / SFTP / Telnet and port forwarding together into one interface.

Bilingual UI (Chinese / English), light and dark themes, multi-tab, split panes, synchronized input across tabs, resumable transfers, port forwarding, and X11 forwarding.

---

## Table of Contents

- [Features](#features)
- [Requirements](#requirements)
- [Development and Build](#development-and-build)
- [Keyboard Shortcuts](#keyboard-shortcuts)
- [Project Structure](#project-structure)
- [Architecture](#architecture)
- [Data Storage Locations](#data-storage-locations)
- [Password Storage](#password-storage)
- [Code Checks](#code-checks)
- [Host Key Verification](#host-key-verification)
- [Known Limitations](#known-limitations)
- [License](#license)

---

## Features

### Sessions

| Feature | Description |
|---|---|
| Local terminal | Launches `powershell.exe` (Windows) / `/bin/bash` (Unix) via `portable-pty`, with full PTY semantics |
| SSH password login | Implemented with `russh`, custom port supported |
| SSH key login | Passphrase-protected private keys supported |
| Jump host | Connects to the target through a `direct-tcpip` channel; the two authentication stages are performed separately |
| Telnet | Direct TCP, no protocol negotiation |
| Session library | Save / delete / group / color-code; import and export as JSON |
| Import PuTTY sessions | Reads from the registry key `HKCU\Software\SimonTatham\PuTTY\Sessions` |
| Recent sessions | Shown on the welcome page, searchable, double-click to connect |

### Terminal

- **Multi-tab**: each tab owns an independent `Terminal` instance and pane, with no interference between them; tabs can be dragged to reorder, renamed, color-coded, or closed in bulk.
- **Split view**: two sessions side by side within one tab (local + remote, or two different hosts). The two panes are independent sessions — input is not broadcast between them.
- **Synchronized input (MultiExec)**: put multiple tabs into the same sync group and send one keystroke to all of them at once.
- **Batch commands**: select several tabs and run one command across all of them.
- **Find**: `SearchAddon` provides previous/next navigation and a "no match" notice.
- **Macros**: record keyboard input (byte-level) and replay it; macros are stored in `localStorage`.
- **WebGL rendering**: prefers `WebglAddon`, and automatically falls back to the DOM renderer when unavailable or when the context is lost.
- Adjustable font size, scrollback lines, and cursor blink.

### File Transfer (SFTP)

- Browse remote directories, create folders, rename, delete; header buttons are icons (hover for tooltips).
- **Hidden files are not shown by default**; toggle with the checkbox on the panel. The number of hidden entries is displayed, and when everything is hidden a hint is shown instead of making the directory look empty. The toggle state is remembered.
- Upload / download, with **resumable transfers** (seek on both ends by the number of bytes already transferred).
- Transfers run serially through a queue with a live progress bar; the list refreshes after each item completes.
- Cancellation distinguishes two sources:
  - **User-initiated cancel** (clicking ✕ on the transfer progress bar) → deletes the incomplete target file.
  - **Network interruption / IO error** → keeps the partial data so it can be resumed later.
  - Exception: if the target file already existed before the transfer began (e.g. re-downloading over an old file), a user cancel does **not** delete it — that is your existing data, and the program will not make that decision for you.
- **Drag-and-drop upload**: drag a file from Explorer onto the SFTP panel to upload it (uses Tauri's window-level drag-drop event to obtain the real path).
- Directory upload: recursively uploads an entire directory tree.

### Networking and Forwarding

- **Port forwarding**: `127.0.0.1:local_port → remote_host:port`; tunnels can be listed and deleted (deletion actually stops the listener task).
- **Ping**: invokes the system `ping` and extracts the summary lines.
- **X11 forwarding**: launches VcXsrv and sets `DISPLAY` for the current SSH session.
- **Package check**: detects whether VcXsrv / Xming / PuTTY / PowerShell are installed, along with their version numbers.
- **RDP / VNC**: no built-in client; uses the system `mstsc` or opens the TightVNC download page.

---

## Requirements

| Dependency | Version | Notes |
|---|---|---|
| Node.js | ≥ 18 | For building the frontend |
| Rust | ≥ 1.77 (stable) | For compiling the backend |
| Windows | 10 / 11 | Primary target platform; requires the [WebView2 runtime](https://developer.microsoft.com/microsoft-edge/webview2/) (bundled with Windows 11) |

Optional components (install only if you need the corresponding feature):

- **VcXsrv** — X11 forwarding. Searched by default at `C:\Program Files\VcXsrv\vcxsrv.exe` and `C:\Program Files (x86)\VcXsrv\vcxsrv.exe`.
- **TightVNC** — VNC connections (this project only opens the download page).

> The `vcxsrv-64.1.20.14.0.installer.exe` at the repository root is the VcXsrv installer, provided for offline deployment; it is **not** bundled into the application.

---

## Development and Build

Install dependencies:

```bash
npm install
```

### Development mode (hot reload)

```bash
npm run tauri dev
```

The `beforeDevCommand` in `tauri.conf.json` automatically starts Vite on port 1420 with `strictPort`.

Frontend only (without the Tauri runtime — `invoke` will fail; useful for styling work only):

```bash
npm run dev
```

### Production build

```bash
npm run tauri build
```

Output: `src-tauri/target/release/bundle/`. The `beforeBuildCommand` runs `npm run build` first (type check + Vite bundle to `dist/`).

### Common scripts

| Command | Purpose |
|---|---|
| `npm run dev` | Starts the Vite dev server only |
| `npm run typecheck` | Runs `tsc --noEmit` for type checking |
| `npm run check:i18n` | Verifies that Chinese and English strings are aligned and complete |
| `npm run build` | Type check + bundle the frontend into `dist/` |
| `npm run tauri dev` | Development mode (frontend + Rust) |
| `npm run tauri build` | Produces the installer |

### Rust-only checks

```bash
cd src-tauri
cargo test --lib    # compile check + unit tests for the host key module
```

If `src-tauri/target` is inaccessible (common in sandboxed environments, read-only mounts, or non-writable CI cache directories), point the build artifacts elsewhere:

```bash
CARGO_TARGET_DIR=/tmp/rustterm-target cargo test --lib
```

Windows PowerShell:

```powershell
$env:CARGO_TARGET_DIR = "$env:TEMP\rustterm-target"; cargo test --lib
```

Note that with this approach the artifacts are not under `src-tauri/target`, while `npm run tauri build` still uses the default directory; do not mix the two, or you will end up recompiling.

---

## Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl + T` | New local terminal |
| `Ctrl + N` | Focus the quick-connect input |
| `Ctrl + W` | Close the current tab |
| `Ctrl + L` | Open the session manager |
| `Ctrl + B` | Show / hide the sidebar |
| `Ctrl + F` | Open the find bar |
| `Ctrl + D` | Toggle light / dark theme |
| `Ctrl + Tab` | Cycle through tabs |
| `F11` | Toggle fullscreen |
| `Esc` | Close the current dialog; close the find bar when no dialog is open |

When a modal is open, the shortcuts above do **not** pass through to the main UI — the keyboard belongs to the current dialog.

**Right-click inside a terminal** opens an action menu: copy / paste / clear / select all (acting on the pane under the cursor, so the correct pane is targeted in split view).

---

## Project Structure

```
.
├── README.md                  This document
├── .gitignore                 Version control ignore rules
├── index.html                 UI skeleton; static text is tagged with data-i18n*, no hard-coded language
├── package.json
├── package-lock.json
├── tsconfig.json              noEmit: type checking is done by tsc, bundling by Vite
├── vite.config.ts
├── icon.png
├── vcxsrv-*.installer.exe     VcXsrv installer (optional component, not bundled into the app; intentionally committed — see below)
├── scripts
│   ├── check-i18n.mjs         Verifies Chinese/English string alignment + reference completeness
│   └── verify-tab-logic.mjs   Regression check for tab index arithmetic
├── src
│   ├── main.ts                All frontend logic: tabs, terminal, SFTP, forwarding, shortcuts
│   ├── i18n.ts                Chinese/English string table + t() / applyI18n() / errorText()
│   └── style.css              Theme variables and all styles
└── src-tauri
    ├── Cargo.toml
    ├── Cargo.lock             Applications should commit this to lock dependency versions
    ├── build.rs
    ├── tauri.conf.json        Window, CSP, and bundling configuration
    ├── capabilities
    │   └── default.json       Whitelist of Tauri permissions callable from the frontend
    ├── icons/                 Application icons
    └── src
        ├── main.rs            Entry point
        ├── lib.rs             Tauri commands (session tables, SFTP, tunnels, session persistence…)
        ├── menu.rs            Native menu bar (bilingual, rebuilt at runtime per language)
        ├── hostkey.rs         Host key verification policy and known_hosts read/write
        ├── secret.rs          Credential store read/write (Windows Credential Manager / Keychain / Secret Service)
        ├── pty.rs             Read/write handle for local PTY / Telnet
        ├── ssh.rs             SSH connection, authentication, jump host, shell + SFTP channels
        └── sftp.rs            Directory listing, upload/download, resumable transfers
```

### Version control ignore rules

`.gitignore` covers the following (each verified in a real git repository):

| Category | Ignored |
|---|---|
| Dependencies | `node_modules/` |
| Build artifacts | `dist/`, `src-tauri/target/` (measured at roughly **11 GB** on this machine — never commit it) |
| Generated files | `src-tauri/gen/schemas/` (rebuilt by `tauri-build` on every compile) |
| Compilation leftovers | `src/**/*.js`, `src/**/*.js.map` (`tsconfig.json` already sets `noEmit`; this is a safety net) |
| Logs | `*.log`, debug logs from various package managers |
| Coverage | `coverage/`, `*.lcov`, `.nyc_output/` |
| Editor / system | `.vscode/`, `.idea/`, `Thumbs.db`, `Desktop.ini`, `.DS_Store` |
| Environment and secrets | `.env`, `*.pem`, `*.key`, `id_rsa`, `id_ed25519`, `known_hosts` |
| Packaging output | `*.msi`, `*.dmg`, `*.deb`, `*.rpm`, `*.AppImage`, etc. |

**Intentionally committed, not ignored**:

- `vcxsrv-64.1.20.14.0.installer.exe` (~41 MB) — the VcXsrv installer for offline deployment, documented in the README. If you do not need offline distribution, add `vcxsrv-*.installer.exe` to `.gitignore` and remove it from the repository with `git rm --cached`.
- `package-lock.json` and `src-tauri/Cargo.lock` — applications (as opposed to libraries) should commit lock files to ensure reproducible builds.
- `src-tauri/icons/` and `.env.example` (if present) — the former is required for packaging, and the latter is an environment variable template containing no real credentials.

> ⚠️ Entries such as `known_hosts`, `id_rsa`, and `.env` in `.gitignore` are safety nets: SSH private keys and host records must **never** enter version control. If these files were ever committed by mistake, ignore rules will not remove them automatically — you must `git rm --cached <file>` and clean them out of the history.

---

## Architecture

### Frontend ↔ Backend

The frontend only calls Tauri commands via `invoke()`; the backend pushes data back through events:

| Event | Payload | Purpose |
|---|---|---|
| `pty:data` | `{ sessionId, data: number[] }` | Terminal output |
| `pty:close` | `{ sessionId }` | Session ended |
| `transfer:progress` | `{ sessionId, sent, total, label }` | Transfer progress |
| `transfer:done` | `{ sessionId, label }` | Transfer complete |
| `menu:action` | menu item id | Native menu click |

Backend errors are returned as **error codes** (`auth-failed`, `key-auth-failed`, `session-not-found`, `cancelled`, etc.) and translated by the frontend's `errorText()` into the current language, avoiding hard-coded Chinese in Rust.

### Session identifiers

Each `pty_spawn` / `ssh_connect` / `telnet_connect` generates a unique id (nanosecond timestamp + auto-incrementing counter). `pty_write` / `pty_resize` / `pty_close` need only the id, so the frontend does not have to distinguish session types; `pty_resize` checks the PTY table first, then the SSH table.

### One terminal per tab

Each `Tab` holds its own `Terminal`, `FitAddon`, wrapper, and pane:

```
#terminal-panes
└── .term-pane                 Wrapper (positioning only; does not host the terminal)
    ├── .term-pane-primary     Primary pane ← primary Terminal
    └── .term-pane-secondary   Secondary pane ← split Terminal (only exists when split)
```

The wrapper and the pane are deliberately two layers: if the terminal were opened directly on the wrapper, the containing block of the second pane in split view would become the already-narrowed wrapper, causing the two panes to overlap and misalign. Split state is controlled by the `.two-up` class on the wrapper, with `.two-up-primary` on the primary pane.

Closing a tab releases resources in order: split → mark `closed` → dispose listeners → notify the backend to close → dispose the terminal → remove the DOM node, avoiding an `onResize` triggered during dispose from hitting the backend again.

### Internationalization

- All user-facing strings must come from `src/i18n.ts` via `t(key)`.
- Static text in `index.html` is tagged with `data-i18n` / `data-i18n-title` / `data-i18n-placeholder`, and `applyI18n()` writes them all at once.
- Native menu text lives on the Rust side (`t(zh, en)` in `menu.rs`); when the frontend switches language it calls `set_menu_language` to have the backend rebuild the menu.
- On first launch, language follows the system locale and the theme defaults to light.

---

## Data Storage Locations

| Data | Location |
|---|---|
| Saved sessions | Windows: `%APPDATA%\rustterm\RustTerm\config\sessions.json`<br>macOS: `~/Library/Application Support/com.rustterm.RustTerm/sessions.json`<br>Linux: `~/.config/rustterm/RustTerm/sessions.json`<br>(resolved by `directories`' `ProjectDirs::from("com", "rustterm", "RustTerm")`, with an additional `config` subdirectory) |
| **Saved passwords** | **Not in any file** — stored in the OS credential store, see [Password Storage](#password-storage) |
| Language | `localStorage: rustterm.lang` |
| Theme | `localStorage: rustterm.theme` |
| Font size | `localStorage: rustterm.fontSize` |
| Macros | `localStorage: rustterm.macros` |
| SFTP hidden-files toggle | `localStorage: rustterm.sftpShowHidden` (hidden by default) |
| Host key policy | `localStorage: rustterm.hostKeyPolicy` (synced to the backend on startup) |
| Last open SSH tabs | `localStorage: rustterm.openTabs` (**titles only**; used as a hint after restart, no auto-reconnect) |

`localStorage` lives in the WebView2 user data directory (`%LOCALAPPDATA%\com.rustterm.app\EBWebView`) and is not removed when the application is uninstalled.

> `sessions.json` contains **only** the `save_password: true/false` flag — **never** a password in plaintext or encrypted form. Open the file to verify.

---

## Password Storage

When saving a session you can choose "remember password"; the password is written into the **operating system credential store**:

| Platform | Credential store |
|---|---|
| Windows | Credential Manager (DPAPI-encrypted, bound to the current user account) |
| macOS | Keychain |
| Linux | Secret Service (GNOME Keyring / KWallet, etc.) |

The credential entry key is `user@host:port` (service name `rustterm`) — using the connection triple rather than the session name, since session names can be edited or duplicated.

### Threat model: read this first

> **Anything a program can decrypt automatically can also be decrypted by a malicious program running under the same user identity.**

This is not an implementation flaw but an inherent boundary of this kind of scheme. Specifically:

| | Description |
|---|---|
| ✅ **Protected against** | `sessions.json` being copied to another machine, ending up in backups or cloud sync, or being read by **another user account** — the ciphertext in the credential store cannot be decrypted outside the original account |
| ❌ **Not protected against** | Malicious programs running as **the same user**, memory dumps, keyloggers |

If you need stronger guarantees, the only option is a master-password prompt on every launch (not implemented here) — and even that does not protect against memory scraping under the same account; it merely raises the bar.

### Usage

- When saving a session (menu "Sessions → Save current session"), you are asked whether to remember the password; on confirmation it is written to the credential store immediately, **not** waiting for the connection to succeed.
- Afterwards, double-clicking that session to connect **pre-fills** the password field — just press Enter. This step is kept so you have the chance to use a different password; reading from the credential store itself requires no extra authorization.
- Entries in the session tree that have a saved password show a marker and a "Forget password" button: this clears only the credential and keeps the session configuration. Deleting a session also clears its credential.
- When the credential store is unavailable (e.g. Linux without Secret Service), the program explains why in the status bar and skips saving — it **never** silently degrades to plaintext.

### Extra dependencies on Linux

On Linux, `keyring` uses Secret Service (`zbus`). If your distribution does not provide it, install:

```bash
# Debian / Ubuntu
sudo apt install libsecret-1-dev gnome-keyring

# Fedora
sudo dnf install libsecret-devel gnome-keyring
```

---

## Code Checks

Before committing, run:

```bash
npm run typecheck     # TypeScript type check (includes noUnusedLocals)
npm run check:i18n    # Chinese/English alignment + key reference completeness
node scripts/verify-tab-logic.mjs   # Tab index arithmetic regression
npm run build         # Frontend bundle
cd src-tauri && cargo test --lib    # Rust compile check + unit tests (see previous section if permissions are restricted)
```

`scripts/check-i18n.mjs` verifies: whether the Chinese and English key sets match, whether every key referenced by code and HTML actually exists, and whether there are unused dead strings. If you add a string and forget to add the other language, it will report it directly.

---

## Known Limitations

- **Only Windows has been actually verified**: `pty.rs` and `lib.rs` have Unix branches, but X11 forwarding, package check, and PuTTY import are Windows-specific (`#[cfg(windows)]`); the macOS / Linux backends for password storage are wired up per `keyring`'s platform implementations but have **not been verified on real machines**.
- **Single window**: no multi-window support and no dragging a tab out into a new window.
- **Split view limited to 2 panes**: no three-column or more complex layouts.
- **No end-to-end tests**: `cargo test --lib` only covers unit tests for host keys and credential keys, and there are two verification scripts under `scripts/`; there are no end-to-end or UI tests.

### Resolved (kept for reference to avoid regressions)

The following issues existed previously and have been fixed:

- ~~Host key not verified~~ → see [Host Key Verification](#host-key-verification); now protects against man-in-the-middle attacks.
- ~~Password had to be typed every time~~ → see [Password Storage](#password-storage); stored in the system credential store and never written to disk.
- ~~Directory upload could not be cancelled and had no progress~~ → every level and every chunk in the recursion checks the cancel flag; uploads now show progress and refresh the list.
- ~~Backend returned hard-coded Chinese~~ → commands such as the X server now return status codes, and all text goes through i18n.
- ~~Tabs remained writable after a session disconnected~~ → when the primary session ends, the tab is marked as disconnected (italic + dimmed) and input is explicitly rejected instead of silently dropped.
- ~~Saving a session silently did nothing when a record already existed~~ → `save_session_full` is now an upsert, so group / color / remember-password all update.

---

## Host Key Verification

When connecting to a host for the first time, its public key is recorded in `known_hosts`; **if that host key ever changes afterwards, the connection is rejected outright** — this is the key defense against man-in-the-middle attacks. Three policies can be selected under "Settings → Host key verification":

| Policy | Behavior | Use case |
|---|---|---|
| `strict` | Only hosts already present in `known_hosts` are allowed; unknown hosts are rejected | High-security environments |
| `accept-new` (default) | Unknown hosts are **recorded and then allowed**; key changes are always rejected | Everyday use |
| `insecure` | No verification at all, equivalent to earlier versions | Legacy devices, one-off troubleshooting |

Both the jump host and the target host **are verified in each connection**. When a key changes, the line number in `known_hosts` is reported so you can verify and correct it manually.

Verification uses `~/.ssh/known_hosts` (on Windows, `%USERPROFILE%\.ssh\known_hosts`), sharing the same record file as the system `ssh` client. The current path is shown in the settings panel.

> When a key changes, the program **does not** overwrite the record automatically; you must confirm and modify it manually — auto-accepting new keys is equivalent to no verification at all.

---

## License

MIT License (see the "About" dialog).

Third-party dependencies: Tauri (MIT/Apache-2.0), xterm.js (MIT), russh (Apache-2.0), russh-sftp, portable-pty, tokio, and others, each under its original license.