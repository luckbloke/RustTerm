# RustTerm

基于 **Tauri 2 + Rust + xterm.js** 的 Windows 桌面终端工具，把 SSH / SFTP / Telnet 与端口转发等日常运维操作集中在一个界面里。

界面中英双语、浅色/深色双主题，支持多标签、分屏、多标签同步输入、断点续传、端口转发与 X11 转发。

---

## 目录

- [功能](#功能)
- [环境要求](#环境要求)
- [开发与构建](#开发与构建)
- [快捷键](#快捷键)
- [项目结构](#项目结构)
- [架构说明](#架构说明)
- [数据存储位置](#数据存储位置)
- [密码保存](#密码保存)
- [代码校验](#代码校验)
- [主机密钥校验](#主机密钥校验)
- [已知限制](#已知限制)
- [许可](#许可)

---

## 功能

### 会话

| 功能 | 说明 |
|---|---|
| 本地终端 | 通过 `portable-pty` 拉起 `powershell.exe`（Windows）/ `/bin/bash`（Unix），完整 PTY 语义 |
| SSH 密码登录 | `russh` 实现，支持自定义端口 |
| SSH 密钥登录 | 支持带口令的私钥 |
| 跳板机（Jump Host） | 经 `direct-tcpip` 通道连接目标主机，两段认证分别进行 |
| Telnet | 直连 TCP，不带协议协商 |
| 会话库 | 保存/删除/分组/着色，支持导入导出 JSON |
| 导入 PuTTY 会话 | 读取注册表 `HKCU\Software\SimonTatham\PuTTY\Sessions` |
| 最近会话 | 欢迎页展示，可搜索过滤，双击直连 |

### 终端

- **多标签**：每个标签拥有独立的 `Terminal` 实例与窗格，互不干扰；标签可拖动排序、重命名、着色、关闭其他。
- **分屏**：同一标签内并排两个会话（可本地 + 远端，或两台不同主机）。两窗格是相互独立的会话，输入不会互相广播。
- **同步输入（MultiExec）**：把多个标签加入同一同步组，一次输入同时下发到全组。
- **批量命令**：勾选若干标签，输入一条命令一次性执行。
- **查找**：`SearchAddon` 提供上一个/下一个与无匹配提示。
- **宏**：录制键盘输入（字节级）并回放，宏保存到 `localStorage`。
- **WebGL 渲染**：优先使用 `WebglAddon`，不可用或上下文丢失时自动回退到 DOM 渲染器。
- 字号缩放、回滚行数、光标闪烁可调。

### 文件传输（SFTP）

- 浏览远端目录、新建目录、重命名、删除；头部按钮为图标（悬停显示提示）。
- **隐藏文件默认不显示**，可用面板上的复选框切换；被隐藏的条目数会显示出来，全部隐藏时给出提示而不是让人误以为目录是空的。开关状态会记住。
- 上传 / 下载，**断点续传**（按已传字节数双向 seek 后续传）。
- 传输队列串行执行，进度条实时刷新；每完成一项即刷新列表。
- 取消时区分两种来源：
  - **用户主动取消**（点传输进度条上的 ✕）→ 删除没传完的目标文件。
  - **网络中断 / IO 错误** → 保留已传部分，供后续续传。
  - 例外：若目标文件在传输开始前就已存在（例如重复下载覆盖旧文件），用户取消时**不会**删除它——那是你原有的数据，程序不替你决定。
- **拖放上传**：从资源管理器把文件拖到 SFTP 面板即上传（走 Tauri 窗口级拖放事件取真实路径）。
- 上传目录：递归上传整个目录树。

### 网络与转发

- **端口转发**：`127.0.0.1:本地端口 → 远端主机:端口`，可列出与删除（删除会真正停掉监听任务）。
- **Ping**：调用系统 `ping` 并提取汇总行。
- **X11 转发**：启动 VcXsrv 并给当前 SSH 会话设置 `DISPLAY`。
- **包检查**：检测 VcXsrv / Xming / PuTTY / PowerShell 是否安装及版本号。
- **RDP / VNC**：不内置客户端，走系统 `mstsc` 或打开 TightVNC 下载页。

---

## 环境要求

| 依赖 | 版本 | 说明 |
|---|---|---|
| Node.js | ≥ 18 | 构建前端 |
| Rust | ≥ 1.77（stable） | 编译后端 |
| Windows | 10 / 11 | 主要目标平台；需要 [WebView2 运行时](https://developer.microsoft.com/microsoft-edge/webview2/)（Win11 已内置） |

可选组件（用到对应功能再装）：

- **VcXsrv** — X11 转发。默认从 `C:\Program Files\VcXsrv\vcxsrv.exe` 与 `C:\Program Files (x86)\VcXsrv\vcxsrv.exe` 查找。
- **TightVNC** — VNC 连接（本项目只负责打开下载页）。

> 仓库根目录下的 `vcxsrv-64.1.20.14.0.installer.exe` 是 VcXsrv 安装包，方便离线部署；它**不会**被打进应用包。

---

## 开发与构建

安装依赖：

```bash
npm install
```

### 开发模式（热重载）

```bash
npm run tauri dev
```

`tauri.conf.json` 的 `beforeDevCommand` 会自动执行 `npm run dev` 起 Vite（端口 1420，`strictPort`）。

只调试前端（不带 Tauri 运行时，`invoke` 会失败，仅适合调样式）：

```bash
npm run dev
```

### 生产构建

```bash
npm run tauri build
```

产物：`src-tauri/target/release/bundle/`。`beforeBuildCommand` 会先执行 `npm run build`（类型检查 + Vite 打包到 `dist/`）。

### 常用脚本

| 命令 | 作用 |
|---|---|
| `npm run dev` | 仅启动 Vite 开发服务器 |
| `npm run typecheck` | `tsc --noEmit` 类型检查 |
| `npm run check:i18n` | 校验中英词条是否对齐、引用是否完整 |
| `npm run build` | 类型检查 + 打包前端到 `dist/` |
| `npm run tauri dev` | 开发模式（前端 + Rust） |
| `npm run tauri build` | 出安装包 |

### 只校验 Rust

```bash
cd src-tauri
cargo test --lib    # 编译检查 + 主机密钥模块的单元测试
```

若 `src-tauri/target` 因权限被拒（多见于受限沙箱、只读挂载或 CI 缓存目录不可写），把构建产物指到别处即可：

```bash
CARGO_TARGET_DIR=/tmp/rustterm-target cargo test --lib
```

Windows PowerShell：

```powershell
$env:CARGO_TARGET_DIR = "$env:TEMP\rustterm-target"; cargo test --lib
```

注意用这种方式编译时，产物不在 `src-tauri/target` 下，`npm run tauri build` 仍然会使用默认目录；两者不要混用以免重复编译。

---

## 快捷键

| 快捷键 | 作用 |
|---|---|
| `Ctrl + T` | 新建本地终端 |
| `Ctrl + N` | 聚焦快速连接输入框 |
| `Ctrl + W` | 关闭当前标签 |
| `Ctrl + L` | 打开会话管理器 |
| `Ctrl + B` | 显示/隐藏侧栏 |
| `Ctrl + F` | 打开查找栏 |
| `Ctrl + D` | 切换浅色/深色主题 |
| `Ctrl + Tab` | 循环切换标签 |
| `F11` | 全屏切换 |
| `Esc` | 关闭当前对话框；无对话框时关闭查找栏 |

打开模态框时，以上快捷键**不会**穿透到主界面——键盘归属当前对话框。

**终端内右键**打开操作菜单：复制 / 粘贴 / 清屏 / 全选（按鼠标所在窗格生效，分屏时不会错对象）。

---

## 项目结构

```
.
├── README.md                  本文档
├── .gitignore                 版本控制忽略规则
├── index.html                 界面骨架；静态文案用 data-i18n* 标注，不写死语言
├── package.json
├── package-lock.json
├── tsconfig.json              noEmit：类型检查交给 tsc，打包交给 Vite
├── vite.config.ts
├── icon.png
├── vcxsrv-*.installer.exe     VcXsrv 安装包（可选组件，不会打进应用；有意提交，见下）
├── scripts
│   ├── check-i18n.mjs         中英词条对齐 + 引用完整性校验
│   └── verify-tab-logic.mjs   标签增删的下标运算回归验证
├── src
│   ├── main.ts                全部前端逻辑：标签、终端、SFTP、转发、快捷键
│   ├── i18n.ts                中英词条表 + t() / applyI18n() / errorText()
│   └── style.css              主题变量与全部样式
└── src-tauri
    ├── Cargo.toml
    ├── Cargo.lock             应用应提交，锁定依赖版本
    ├── build.rs
    ├── tauri.conf.json        窗口、CSP、打包配置
    ├── capabilities
    │   └── default.json       前端可调用的 Tauri 权限白名单
    ├── icons/                 应用图标
    └── src
        ├── main.rs            入口
        ├── lib.rs             Tauri 命令（会话表、SFTP、隧道、会话持久化…）
        ├── menu.rs            原生菜单栏（中英双语，运行时可按语言重建）
        ├── hostkey.rs         主机密钥校验策略与 known_hosts 读写
        ├── secret.rs          凭据库读写（Windows 凭据管理器 / Keychain / Secret Service）
        ├── pty.rs             本地 PTY / Telnet 的读写句柄
        ├── ssh.rs             SSH 连接、认证、跳板机、shell+SFTP 通道
        └── sftp.rs            目录、上传下载、断点续传
```

### 版本控制忽略规则

`.gitignore` 覆盖以下内容（已逐条在真实 git 仓库中验证）：

| 类别 | 忽略项 |
|---|---|
| 依赖 | `node_modules/` |
| 构建产物 | `dist/`、`src-tauri/target/`（本机实测约 **11 GB**，务必不要提交） |
| 生成文件 | `src-tauri/gen/schemas/`（`tauri-build` 每次编译重建） |
| 编译残留 | `src/**/*.js`、`src/**/*.js.map`（`tsconfig.json` 已开 `noEmit`，这是兜底） |
| 日志 | `*.log`、各包管理器的 debug 日志 |
| 覆盖率 | `coverage/`、`*.lcov`、`.nyc_output/` |
| 编辑器/系统 | `.vscode/`、`.idea/`、`Thumbs.db`、`Desktop.ini`、`.DS_Store` |
| 环境与密钥 | `.env`、`*.pem`、`*.key`、`id_rsa`、`id_ed25519`、`known_hosts` |
| 打包产物 | `*.msi`、`*.dmg`、`*.deb`、`*.rpm`、`*.AppImage` 等 |

**有意提交、不在忽略之列**：

- `vcxsrv-64.1.20.14.0.installer.exe`（约 41 MB）——离线部署用的 VcXsrv 安装包，README 已说明用途。若你不需要离线分发，可在 `.gitignore` 中追加 `vcxsrv-*.installer.exe` 并用 `git rm --cached` 把它移出版本库。
- `package-lock.json` 与 `src-tauri/Cargo.lock`——应用（而非库）应提交锁文件，以保证构建可复现。
- `src-tauri/icons/` 与 `.env.example`（若存在）——前者是打包必需资源，后者是环境变量模板，不含任何真实凭据。

> ⚠️ `.gitignore` 中的 `known_hosts`、`id_rsa`、`.env` 等条目是安全兜底：SSH 私钥与主机记录**绝不能**进入版本库。若这些文件曾被误提交，忽略规则不会自动移除它们，需要 `git rm --cached <文件>` 并从历史中清理。

---

---

## 架构说明

### 前端 ↔ 后端

前端只通过 `invoke()` 调用 Tauri 命令，后端通过事件把数据推回：

| 事件 | 载荷 | 用途 |
|---|---|---|
| `pty:data` | `{ sessionId, data: number[] }` | 终端输出 |
| `pty:close` | `{ sessionId }` | 会话结束 |
| `transfer:progress` | `{ sessionId, sent, total, label }` | 传输进度 |
| `transfer:done` | `{ sessionId, label }` | 传输完成 |
| `menu:action` | 菜单项 id | 原生菜单点击 |

后端错误以**错误码**形式返回（`auth-failed`、`key-auth-failed`、`session-not-found`、`cancelled` 等），由前端 `errorText()` 翻译成当前语言文案，避免在 Rust 里硬编码中文。

### 会话标识

每次 `pty_spawn` / `ssh_connect` / `telnet_connect` 生成唯一 id（纳秒时间戳 + 自增计数器）。`pty_write` / `pty_resize` / `pty_close` 只需 id，前端不必区分会话类型；`pty_resize` 会先查 PTY 表、再查 SSH 表。

### 每个标签一套终端

`Tab` 持有自己的 `Terminal`、`FitAddon`、包装层与窗格：

```
#terminal-panes
└── .term-pane                 包装层（只负责定位，不承载终端）
    ├── .term-pane-primary     主窗格 ← 主 Terminal
    └── .term-pane-secondary   第二窗格 ← 分屏 Terminal（仅分屏时存在）
```

包装层与窗格分成两层是刻意的：若把终端直接开在包装层上，分屏时第二窗格的包含块会变成已被缩窄的包装层，导致两个窗格错位重叠。分屏状态由包装层上的 `.two-up` 类控制，主窗格加 `.two-up-primary`。

标签关闭时按顺序释放：分屏 → 标记 `closed` → dispose 监听 → 通知后端关闭 → dispose 终端 → 移除 DOM，避免 dispose 期间触发的 `onResize` 再打后端。

### 国际化

- 所有面向用户的字符串都必须经 `t(key)` 取自 `src/i18n.ts`。
- `index.html` 的静态文案用 `data-i18n` / `data-i18n-title` / `data-i18n-placeholder` 标注，`applyI18n()` 统一写入。
- 原生菜单栏文案在 Rust 侧（`menu.rs` 的 `t(zh, en)`），前端切换语言时调用 `set_menu_language` 让后端重建菜单。
- 语言、主题首次启动分别跟随系统语言与默认浅色。

---

## 数据存储位置

| 数据 | 位置 |
|---|---|
| 已保存会话 | Windows：`%APPDATA%\rustterm\RustTerm\config\sessions.json`<br>macOS：`~/Library/Application Support/com.rustterm.RustTerm/sessions.json`<br>Linux：`~/.config/rustterm/RustTerm/sessions.json`<br>（由 `directories` crate 的 `ProjectDirs::from("com", "rustterm", "RustTerm")` 解析，另有 `config` 子目录） |
| **已保存的密码** | **不在任何文件里**——存放于系统凭据库，见[密码保存](#密码保存) |
| 语言 | `localStorage: rustterm.lang` |
| 主题 | `localStorage: rustterm.theme` |
| 字号 | `localStorage: rustterm.fontSize` |
| 宏 | `localStorage: rustterm.macros` |
| SFTP 隐藏文件开关 | `localStorage: rustterm.sftpShowHidden`（缺省不显示） |
| 主机密钥策略 | `localStorage: rustterm.hostKeyPolicy`（启动时同步给后端） |
| 上次打开的 SSH 标签 | `localStorage: rustterm.openTabs`（**仅记录标题**，重启后只做提示，不自动重连） |

`localStorage` 存放在 WebView2 的用户数据目录下（`%LOCALAPPDATA%\com.rustterm.app\EBWebView`），不随应用卸载自动清除。

> `sessions.json` 里**只有** `save_password: true/false` 这个标记，**绝不包含密码明文或密文**。想确认的话直接打开该文件即可。

---

## 密码保存

保存会话时可选择「记住密码」，密码会被写入**操作系统凭据库**：

| 平台 | 凭据库 |
|---|---|
| Windows | 凭据管理器（DPAPI 加密，绑定当前用户账户） |
| macOS | Keychain |
| Linux | Secret Service（GNOME Keyring / KWallet 等） |

凭据条目的键是 `user@host:port`（服务名 `rustterm`）——用连接三元组而非会话名，因为会话名可改、可重名。

### 威胁模型：请先读这一段

> **任何能被程序自动解密的东西，同一用户身份下的恶意程序也能解密。**

这不是实现缺陷，而是这类方案的固有边界。具体来说：

| | 说明 |
|---|---|
| ✅ **能防住** | `sessions.json` 被拷到别的机器、混进备份或云同步、被**其他用户账户**读取——凭据库里的密文离开原账户就解不开 |
| ❌ **防不住** | 以**同一用户**身份运行的恶意程序、内存转储、键盘记录 |

如果你需要更强的保证，只能改用每次启动输入主密码的方案（本项目未实现）——那仍然防不住同账户的内存抓取，只是提高门槛。

### 使用方式

- 保存会话（菜单「会话 → 保存当前会话」）时会询问是否记住密码；确认后立即写入凭据库，**不等连接成功**。
- 之后双击该会话连接时，密码框会**预填**已保存的密码，直接回车即可。保留这一步是为了让你有机会改用别的密码——凭据库读取本身不需要额外授权。
- 会话树里已存密码的条目会显示标记与「忘记密码」按钮：只清除凭据，保留会话配置。删除会话也会一并清除凭据。
- 凭据库不可用时（例如 Linux 未装 Secret Service），程序会在状态栏说明原因并跳过保存，**不会静默降级成明文**。

### Linux 额外依赖

Linux 上 `keyring` 使用 Secret Service（`zbus`）。若发行版未提供，需要先装：

```bash
# Debian / Ubuntu
sudo apt install libsecret-1-dev gnome-keyring

# Fedora
sudo dnf install libsecret-devel gnome-keyring
```

---

---

## 代码校验

提交前建议跑一遍：

```bash
npm run typecheck     # TypeScript 类型检查（含 noUnusedLocals）
npm run check:i18n    # 中英词条对齐 + 键引用完整性
node scripts/verify-tab-logic.mjs   # 标签下标运算回归
npm run build         # 前端打包
cd src-tauri && cargo test --lib    # Rust 编译检查 + 单元测试（权限受限时见上一节）
```

`scripts/check-i18n.mjs` 会检查：中英词条键集合是否一致、代码与 HTML 引用的键是否都存在、是否有未被引用的死词条。加文案时忘了补另一种语言，它会直接报出来。

---

## 已知限制

- **仅 Windows 经过实际验证**：`pty.rs` 与 `lib.rs` 有 Unix 分支，但 X11 转发、包检查、PuTTY 导入等逻辑是 Windows 专用（`#[cfg(windows)]`）；密码保存的 macOS / Linux 后端已按 `keyring` 的平台实现接通，但**未在真实机器上验证过**。
- **单窗口**：不支持多窗口或标签拖出成新窗口。
- **分屏上限为 2 个窗格**：不支持三栏及更复杂的布局。
- **无端到端测试**：`cargo test --lib` 只有主机密钥与凭据键的单测，`scripts/` 下有两个校验脚本；没有端到端与界面测试。

### 已解决（保留说明以免回退）

以下问题曾经存在，现已修复：

- ~~不校验主机密钥~~ → 见[主机密钥校验](#主机密钥校验)，可防中间人攻击。
- ~~密码只能每次手输~~ → 见[密码保存](#密码保存)，存入系统凭据库且不落盘。
- ~~目录上传不可取消、无进度~~ → 递归每层每块都检查取消标志，上传期间有进度与列表刷新。
- ~~后端返回硬编码中文~~ → X server 等命令改为返回状态码，文案统一走 i18n。
- ~~会话断开后标签仍可输入~~ → 主会话结束时标签标记为已断开（斜体+暗色），输入被明确拦截而非静默丢弃。
- ~~保存会话遇到已存在的记录会静默无效~~ → `save_session_full` 改为 upsert，分组/颜色/记住密码都能更新。

---

## 主机密钥校验

首次连接某台主机时，其公钥会被记入 `known_hosts`；**之后该主机密钥一旦变化，连接会被直接拒绝**——这是防御中间人攻击的关键。可在「设置 → 主机密钥校验」中切换三档策略：

| 策略 | 行为 | 适用场景 |
|---|---|---|
| `strict` | 只允许 `known_hosts` 中已有的主机，未知主机一律拒绝 | 安全要求高的环境 |
| `accept-new`（默认） | 未知主机**记录后继续**；密钥变更一律拒绝 | 日常使用 |
| `insecure` | 完全不校验，等价于早期版本行为 | 老设备、一次性排障 |

跳板机与目标主机**两段连接都会校验**。密钥变更时会提示 `known_hosts` 的行号，便于核实后手动更正。

校验使用 `~/.ssh/known_hosts`（Windows 为 `%USERPROFILE%\.ssh\known_hosts`），与系统 `ssh` 客户端共用同一份记录。当前路径可在设置面板中看到。

> 密钥变更时程序**不会**自动覆盖记录，必须你手动确认后修改——自动接受新密钥等于没有校验。

---

## 许可

MIT License（见"关于"对话框）。

第三方依赖：Tauri（MIT/Apache-2.0）、xterm.js（MIT）、russh（Apache-2.0）、russh-sftp、portable-pty、tokio 等，各自遵循其原始许可。
