//! 本地 X server（VcXsrv / Xming）管理。
//!
//! 职责：
//! 1. 自动发现已安装的 X server 可执行文件；
//! 2. 挑选一个空闲 display（TCP 6000+N 未被占用）；
//! 3. 启动进程并等待 X 协议真正就绪（完成一次 X11 握手），
//!    而不是"进程起来了就算好了"——X server 从启动到监听端口有几百毫秒到几秒的窗口，
//!    期间往远程写 DISPLAY 都会失败；
//! 4. 提供 DISPLAY 字符串（127.0.0.1:N.0）供 `setup_x11` 写入 SSH 会话；
//! 5. 停止时确保进程被杀掉、句柄被回收。
//!
//! 启动参数带 `-ac`（关闭访问控制）：这样远程 X 客户端无需 XAUTHORITY cookie
//! 就能连进来，省掉 X11 forwarding + xauth 的整套配置。
//!
//! 关于 VcXsrv 的进程模型：
//! VcXsrv 启动后会 fork 出真正的 X server 进程和剪贴板进程。
//! 只 kill 父进程会留下孤儿进程占着 6000+N 端口。
//! 因此 stop 用 taskkill /T 杀整个进程树；start 开头也清一次残留。

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

/// 一个运行中的 X server。
pub struct XServer {
    /// display 编号（X server 监听端口 = 6000 + display）。
    pub display: u32,
    /// 实际使用的可执行文件，用于日志与界面提示。
    pub program: String,
    process: Child,
}

impl XServer {
    /// 杀掉进程树并等待其退出。
    ///
    /// Windows 上用 taskkill /T /F /PID：VcXsrv 会 fork 子进程，
    /// 只 kill 父进程会留下孤儿占着端口。
    pub fn stop(&mut self) {
        let pid = self.process.id();
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .output();
        }
        #[cfg(not(windows))]
        {
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
    }
}

impl Drop for XServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// X server 就绪等待的总时长。
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// display 探测范围。:0 在 Windows 上会被系统/其它程序抢用，从 :1 开始。
const DISPLAY_RANGE: std::ops::RangeInclusive<u32> = 1..=20;

/// X 协议握手第 3 字节（index 2）的 major version。
///
/// X11 握手前 4 字节是：
///   buf[0] = 成功状态 (0x01)
///   buf[1] = padding   (0x00)
///   buf[2] = major version (0x0B = 11)
///   buf[3] = minor version (0x00)
const X_MAJOR_VERSION: u8 = 11;

/// 用户可配置的 X server 启动项。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XServerConfig {
    /// 可执行文件路径。
    pub program: String,
    /// 启动参数（不含 display 号）。
    #[serde(default)]
    pub args: Vec<String>,
}

fn config_path() -> Option<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "rustterm", "RustTerm")?;
    let dir = dirs.config_dir();
    std::fs::create_dir_all(dir).ok()?;
    Some(dir.join("xserver.json"))
}

/// 从配置目录读取 xserver.json；没有就返回 None。
fn load_user_config() -> Option<Vec<XServerConfig>> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 保存用户配置。
pub fn save_user_config(list: &[XServerConfig]) -> Result<(), String> {
    let path = config_path().ok_or("无法定位配置目录")?;
    let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// 返回当前生效的配置：用户配置优先，否则返回默认候选。
pub fn get_config() -> Vec<XServerConfig> {
    if let Some(list) = load_user_config() {
        if !list.is_empty() {
            return list;
        }
    }
    #[cfg(windows)]
    {
        DEFAULT_CANDIDATES
            .iter()
            .map(|(p, a)| XServerConfig {
                program: p.to_string(),
                args: a.iter().map(|s| s.to_string()).collect(),
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![XServerConfig {
            program: "Xvfb".to_string(),
            args: vec!["-screen".into(), "0".into(), "1024x768x24".into()],
        }]
    }
}

/// 候选 X server 程序：按优先级排列。
#[cfg(windows)]
const DEFAULT_CANDIDATES: &[(&str, &[&str])] = &[
    (
        r"C:\Program Files (x86)\Xming\Xming.exe",
        &["-multiwindow", "-clipboard", "-ac"],
    ),
    (
        r"C:\Program Files\Xming\Xming.exe",
        &["-multiwindow", "-clipboard", "-ac"],
    ),
    (
        r"C:\Program Files\VcXsrv\vcxsrv.exe",
        &["-multiwindow", "-clipboard", "-ac"],
    ),
    (
        r"C:\Program Files (x86)\VcXsrv\vcxsrv.exe",
        &["-multiwindow", "-clipboard", "-ac"],
    ),
];

/// 找到第一个存在的 X server 可执行文件，返回（路径, 启动参数模板）。
pub fn find_program() -> Option<(PathBuf, Vec<String>)> {
    // 1. 优先用户配置
    if let Some(list) = load_user_config() {
        for cfg in &list {
            let p = Path::new(&cfg.program);
            if p.is_file() {
                return Some((p.to_path_buf(), cfg.args.clone()));
            }
        }
    }

    // 2. 兜底默认候选
    let candidates: &[(&str, &[&str])] = {
        #[cfg(windows)]
        { DEFAULT_CANDIDATES }
        #[cfg(not(windows))]
        {
            &[(
                "Xvfb",
                &["-screen", "0", "1024x768x24"],
            )]
        }
    };
    for (path, extra_args) in candidates {
        let p = Path::new(path);
        let exists = if p.is_file() {
            true
        } else if let Some(base) = p.file_name() {
            std::env::var_os("PATH")
                .map(|dirs| {
                    std::env::split_paths(&dirs).any(|dir| dir.join(base).is_file())
                })
                .unwrap_or(false)
        } else {
            false
        };
        if exists {
            return Some((
                p.to_path_buf(),
                extra_args.iter().map(|s| s.to_string()).collect(),
            ));
        }
    }
    None
}

/// 探测一个空闲 display：6000+N 可绑定即可用。
///
/// 绑 `0.0.0.0` 而不是 `127.0.0.1`：VcXsrv 绑的是 `0.0.0.0`，
/// 用 `127.0.0.1` 检查会漏掉 VcXsrv 已占用的情况。
fn free_display() -> Option<u32> {
    for n in DISPLAY_RANGE {
        let port = (6000u32 + n) as u16;
        match std::net::TcpListener::bind(("0.0.0.0", port)) {
            Ok(listener) => {
                drop(listener);
                return Some(n);
            }
            Err(_) => {}
        }
    }
    None
}

/// X server 的 TCP 端口。
pub fn x11_port(display: u32) -> u16 {
    (6000u32 + display) as u16
}

/// 形如 `127.0.0.1:1.0` 的 DISPLAY 值。
pub fn display_name(display: u32) -> String {
    format!("127.0.0.1:{display}.0")
}

/// 连接到 X 端口并验证它真的在讲 X11 协议。
///
/// X 协议规定服务端在 TCP 连接建立后主动发送 12 字节：
/// `0x01, 0x00, 0x0B, 0x00, auth-string(8)`。
/// 第 3 字节（index 2）是 major version = 11。
/// 只验证 TCP 可达是不够的——那个端口可能被其它程序占用并应答非 X 数据。
///
/// 超时 3 秒：VcXsrv 启动后需要几秒才监听端口。500ms 太短会误判。
async fn x11_handshake_ok(port: u16) -> bool {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;
    use tokio::time::timeout;

    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    let mut buf = [0u8; 12];
    match timeout(Duration::from_secs(3), stream.read_exact(&mut buf)).await {
        Ok(Ok(_)) => {
            // 关键：major version 在 buf[2]，不是 buf[1]。
            // buf[1] 是 padding，恒为 0。
            eprintln!(
                "[xserver] 握手字节: {:02X} {:02X} {:02X} {:02X}",
                buf[0], buf[1], buf[2], buf[3]
            );
            buf[0] == 0x01 && buf[2] == X_MAJOR_VERSION
        }
        _ => false,
    }
}

/// 清理残留的 VcXsrv / Xming 进程。
///
/// RustTerm 被强杀、崩溃、或 Debug 重启时，XServer 的 Drop 不会执行，
/// VcXsrv 的子进程会留在后台占着 6000+N 端口。
/// 启动新实例前先清一次，避免 "listen() failed (10048)"。
#[cfg(windows)]
fn kill_leftover_xservers() {
    for image in ["vcxsrv.exe", "Xming.exe"] {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", image])
            .output();
    }
}

#[cfg(not(windows))]
fn kill_leftover_xservers() {}

/// 启动一个 X server 并阻塞（异步）到它真正就绪。
///
/// 错误码：
/// - `xserver-not-found`     没有安装任何已知 X server
/// - `xserver-no-free-display` 找不到空闲 display
/// - `xserver-spawn-failed:<io err>` 进程启动失败
/// - `xserver-ready-timeout` 超时未通过 X 协议握手（进程已被回收）
pub async fn start() -> Result<XServer, String> {
    // 先清残留，避免端口被上次的孤儿进程占着。
    kill_leftover_xservers();

    let (program, extra_args) = find_program().ok_or("xserver-not-found")?;
    let display = free_display().ok_or("xserver-no-free-display")?;
    eprintln!("[xserver] 尝试启动 display :{display}，程序 {program:?}");

    let mut cmd = std::process::Command::new(&program);
    cmd.arg(format!(":{display}")).args(&extra_args);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("xserver-spawn-failed:{e}"))?;
    eprintln!("[xserver] 进程已启动 pid={:?}", child.id());

    let port = x11_port(display);
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if x11_handshake_ok(port).await {
            eprintln!("[xserver] display :{display} 已就绪");
            return Ok(XServer {
                display,
                program: program.display().to_string(),
                process: child,
            });
        }
        // 进程在等待期间自己退出了（比如 VcXsrv 弹窗被用户关掉）
        if let Ok(Some(status)) = child.try_wait() {
            let _ = child.wait();
            eprintln!("[xserver] 进程提前退出: {status}");
            return Err("xserver-spawn-failed:process-exited-early".into());
        }
        if Instant::now() >= deadline {
            let pid = child.id();
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .output();
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill();
            }
            let _ = child.wait();
            eprintln!("[xserver] display :{display} 就绪超时");
            return Err("xserver-ready-timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}