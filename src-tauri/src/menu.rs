use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    App, AppHandle, Wry,
};

/// 界面语言是否已切到英文。原生菜单栏由 Rust 构建，
/// 前端切换语言时通过 `set_menu_language` 通知这里重建。
static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_english(english: bool) {
    ENGLISH.store(english, Ordering::Relaxed);
}

fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// 取当前语言下的文案。
fn t(zh: &'static str, en: &'static str) -> &'static str {
    if is_english() { en } else { zh }
}

/// 供 `setup` 使用：此时只有 `&App`。
pub fn build(app: &App) -> tauri::Result<Menu<Wry>> {
    build_with(app.handle())
}

/// 构建菜单栏。
///
/// 菜单项的 Runtime 参数固定为 `Wry`，因此这里直接收 `&AppHandle<Wry>`，
/// 启动时的 `setup` 和运行时切换语言共用同一份定义。
pub fn build_with(handle: &AppHandle<Wry>) -> tauri::Result<Menu<Wry>> {
    // 终端
    let new_local = MenuItem::with_id(handle, "term-new-local", t("启动本地终端", "Start local terminal"), true, Some("Ctrl+T"))?;
    let new_ssh = MenuItem::with_id(handle, "term-new-ssh", t("新建 SSH 会话", "New SSH session"), true, Some("Ctrl+N"))?;
    let close_tab = MenuItem::with_id(handle, "term-close-tab", t("关闭当前标签", "Close current tab"), true, Some("Ctrl+W"))?;
    let sep1 = PredefinedMenuItem::separator(handle)?;
    let exit = MenuItem::with_id(handle, "term-exit", t("退出", "Exit"), true, Some("Alt+F4"))?;
    let terminal_menu = Submenu::with_items(
        handle, t("终端", "Terminal"), true,
        &[&new_local, &new_ssh, &close_tab, &sep1, &exit],
    )?;

    // 会话
    let save_session = MenuItem::with_id(handle, "sess-save", t("保存当前会话", "Save current session"), true, None::<&str>)?;
    let open_library = MenuItem::with_id(handle, "sess-library", t("会话管理器…", "Session manager..."), true, Some("Ctrl+L"))?;
    let sep2 = PredefinedMenuItem::separator(handle)?;
    let import_putty = MenuItem::with_id(handle, "sess-import-putty", t("导入 PuTTY 会话", "Import PuTTY sessions"), true, None::<&str>)?;
    let export_sessions = MenuItem::with_id(handle, "sess-export", t("导出会话…", "Export sessions..."), true, None::<&str>)?;
    let import_sessions = MenuItem::with_id(handle, "sess-import", t("导入会话…", "Import sessions..."), true, None::<&str>)?;
    let sessions_menu = Submenu::with_items(
        handle, t("会话", "Sessions"), true,
        &[
            &save_session,
            &open_library,
            &sep2,
            &import_putty,
            &export_sessions,
            &import_sessions,
        ],
    )?;

    // 视图
    let toggle_sidebar = MenuItem::with_id(handle, "view-sidebar", t("显示/隐藏侧栏", "Show/Hide sidebar"), true, Some("Ctrl+B"))?;
    let toggle_sftp = MenuItem::with_id(handle, "view-sftp", t("显示/隐藏 SFTP 面板", "Show/Hide SFTP panel"), true, None::<&str>)?;
    let sep3 = PredefinedMenuItem::separator(handle)?;
    let zoom_in = MenuItem::with_id(handle, "view-zoom-in", t("放大", "Zoom in"), true, Some("Ctrl+="))?;
    let zoom_out = MenuItem::with_id(handle, "view-zoom-out", t("缩小", "Zoom out"), true, Some("Ctrl+-"))?;
    let zoom_reset = MenuItem::with_id(handle, "view-zoom-reset", t("重置字号", "Reset zoom"), true, Some("Ctrl+0"))?;
    let sep4 = PredefinedMenuItem::separator(handle)?;
    let toggle_fullscreen = MenuItem::with_id(handle, "view-fullscreen", t("全屏", "Fullscreen"), true, Some("F11"))?;
    let reset_layout = MenuItem::with_id(handle, "view-reset-layout", t("重置布局", "Reset layout"), true, None::<&str>)?;
    let view_menu = Submenu::with_items(
        handle, t("视图", "View"), true,
        &[
            &toggle_sidebar,
            &toggle_sftp,
            &sep3,
            &zoom_in,
            &zoom_out,
            &zoom_reset,
            &sep4,
            &toggle_fullscreen,
            &reset_layout,
        ],
    )?;

    // X 服务
    let x_start = MenuItem::with_id(handle, "x-start", t("启动 X 服务", "Start X server"), true, None::<&str>)?;
    let x_stop = MenuItem::with_id(handle, "x-stop", t("停止 X 服务", "Stop X server"), true, None::<&str>)?;
    let x_menu = Submenu::with_items(handle, t("X 服务", "X server"), true, &[&x_start, &x_stop])?;

    // 工具
    let t_ssh = MenuItem::with_id(handle, "tools-ssh", t("SSH 客户端", "SSH client"), true, None::<&str>)?;
    let t_sftp = MenuItem::with_id(handle, "tools-sftp", t("SFTP 客户端", "SFTP client"), true, None::<&str>)?;
    let t_telnet = MenuItem::with_id(handle, "tools-telnet", t("Telnet 客户端", "Telnet client"), true, None::<&str>)?;
    let t_rdp = MenuItem::with_id(handle, "tools-rdp", t("RDP 客户端（mstsc）", "RDP client (mstsc)"), true, None::<&str>)?;
    let t_vnc = MenuItem::with_id(handle, "tools-vnc", t("VNC 客户端（TightVNC）", "VNC client (TightVNC)"), true, None::<&str>)?;
    let sep5 = PredefinedMenuItem::separator(handle)?;
    let t_ping = MenuItem::with_id(handle, "tools-ping", t("Ping 主机", "Ping host"), true, None::<&str>)?;
    let t_packages = MenuItem::with_id(handle, "tools-packages", t("检查已装组件", "Check packages"), true, None::<&str>)?;
    let t_portscan = MenuItem::with_id(handle, "tools-portscan", t("端口扫描", "Port scan"), true, None::<&str>)?;
    let t_menu = Submenu::with_items(
        handle, t("工具", "Tools"), true,
        &[&t_ssh, &t_sftp, &t_telnet, &t_rdp, &t_vnc, &sep5, &t_ping, &t_portscan, &t_packages],
    )?;

    // 设置
    let s_general = MenuItem::with_id(handle, "set-general", t("常规设置…", "General settings..."), true, None::<&str>)?;
    let s_terminal = MenuItem::with_id(handle, "set-terminal", t("终端设置…", "Terminal settings..."), true, None::<&str>)?;
    let s_menu = Submenu::with_items(
        handle, t("设置", "Settings"), true,
        &[&s_general, &s_terminal],
    )?;

    // 宏
    let m_record = MenuItem::with_id(handle, "macro-record", t("录制宏", "Record macro"), true, None::<&str>)?;
    let m_play = MenuItem::with_id(handle, "macro-play", t("回放宏", "Play macro"), true, None::<&str>)?;
    let m_menu = Submenu::with_items(handle, t("宏", "Macros"), true, &[&m_record, &m_play])?;

    // 帮助
    let help_about = MenuItem::with_id(handle, "help-about", t("关于 RustTerm", "About RustTerm"), true, None::<&str>)?;
    let help_menu = Submenu::with_items(handle, t("帮助", "Help"), true, &[&help_about])?;

    Menu::with_items(
        handle,
        &[
            &terminal_menu,
            &sessions_menu,
            &view_menu,
            &x_menu,
            &t_menu,
            &s_menu,
            &m_menu,
            &help_menu,
        ],
    )
}
