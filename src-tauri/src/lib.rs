mod hostkey;
mod menu;
mod portscan;
mod pty;
mod secret;
mod sftp;
mod ssh;

use hostkey::HostKeyPolicy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, Mutex as StdMutex};
use tauri::Emitter;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// 端口扫描的取消标志表。前端点"停止"时按 scanId 找到对应标志置位。
static SCAN_CANCELS: std::sync::OnceLock<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>> =
    std::sync::OnceLock::new();

fn scan_cancels() -> &'static Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>> {
    SCAN_CANCELS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn scan_cancel_register(id: &str, flag: Arc<std::sync::atomic::AtomicBool>) {
    scan_cancels().lock().unwrap().insert(id.to_string(), flag);
}

pub(crate) fn scan_cancel_unregister(id: &str) {
    scan_cancels().lock().unwrap().remove(id);
}

pub(crate) fn scan_cancel_fire(id: &str) {
    if let Some(flag) = scan_cancels().lock().unwrap().get(id) {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 在 Windows 上以不弹控制台窗口的方式启动子进程。
///
/// 默认情况下 `Command` 启动控制台程序会分配一个新的控制台窗口，
/// 于是 `powershell`/`reg`/`ping` 每次调用都闪一下黑框。
/// `CREATE_NO_WINDOW`（0x08000000）让子进程从一开始就不分配控制台。
///
/// 非 Windows 平台是空操作——那里的进程启动本来就不会弹窗。
#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

pub struct AppState {
    pub sessions: Mutex<HashMap<String, pty::PtyHandle>>,
    pub ssh_sessions: Mutex<HashMap<String, ssh::SshHandle>>,
    pub cancel_flags: Mutex<HashMap<String, Arc<sftp::TransferCancel>>>,
    /// 主机密钥校验策略，由前端持久化后同步进来。
    pub host_key_policy: Mutex<HostKeyPolicy>,
}

pub struct XServerState(pub StdMutex<Option<Child>>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TunnelRecord {
    pub id: String,
    pub session_id: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

/// 隧道记录连同它的监听任务句柄。
/// 只保留记录会让转发器一直运行，删除后本地端口永远不会释放。
pub struct Tunnel {
    pub record: TunnelRecord,
    pub task: tokio::task::JoinHandle<()>,
}

pub struct TunnelState(pub Mutex<Vec<Tunnel>>);

// ---------- PTY ----------
#[tauri::command]
fn pty_spawn(app: tauri::AppHandle, state: tauri::State<AppState>, cols: u16, rows: u16) -> Result<String, String> {
    let id = format!("pty-{}", uuid_like());
    let handle = pty::spawn_local(app.clone(), id.clone(), cols, rows).map_err(|e| e.to_string())?;
    state.sessions.lock().unwrap().insert(id.clone(), handle);
    Ok(id)
}

#[tauri::command]
fn pty_write(state: tauri::State<AppState>, session_id: String, data: Vec<u8>) -> Result<(), String> {
    {
        let map = state.sessions.lock().unwrap();
        if let Some(handle) = map.get(&session_id) {
            return handle.write(&data).map_err(|e| e.to_string());
        }
    }
    let map = state.ssh_sessions.lock().unwrap();
    match map.get(&session_id) {
        Some(handle) => handle.write(&data).map_err(|e| e.to_string()),
        // 会话在写入的瞬间被关闭属于正常竞态，静默忽略。
        None => Ok(()),
    }
}

/// PTY 与 SSH 会话共用同一个 resize 入口：前端不需要区分会话类型。
#[tauri::command]
fn pty_resize(state: tauri::State<AppState>, session_id: String, cols: u16, rows: u16) -> Result<(), String> {
    {
        let map = state.sessions.lock().unwrap();
        if let Some(handle) = map.get(&session_id) {
            return handle.resize(cols, rows).map_err(|e| e.to_string());
        }
    }
    let map = state.ssh_sessions.lock().unwrap();
    match map.get(&session_id) {
        Some(handle) => handle.resize(cols, rows).map_err(|e| e.to_string()),
        // 会话在调整尺寸的瞬间被关闭属于正常竞态，静默忽略。
        None => Ok(()),
    }
}

#[tauri::command]
fn pty_close(state: tauri::State<AppState>, session_id: String) -> Result<(), String> {
    state.sessions.lock().unwrap().remove(&session_id);
    state.ssh_sessions.lock().unwrap().remove(&session_id);
    Ok(())
}

// ---------- SSH ----------
#[tauri::command]
async fn ssh_connect(
    app: tauri::AppHandle, state: tauri::State<'_, AppState>,
    host: String, port: u16, user: String, password: String, cols: u16, rows: u16,
) -> Result<String, String> {
    let id = format!("ssh-{}", uuid_like());
    let policy = *state.host_key_policy.lock().unwrap();
    let handle = ssh::connect(app.clone(), id.clone(), policy, &host, port, &user, &password, cols, rows)
        .await.map_err(|e| e.to_string())?;
    state.ssh_sessions.lock().unwrap().insert(id.clone(), handle);
    Ok(id)
}

#[tauri::command]
async fn ssh_connect_key(
    app: tauri::AppHandle, state: tauri::State<'_, AppState>,
    host: String, port: u16, user: String,
    key_path: String, passphrase: Option<String>, cols: u16, rows: u16,
) -> Result<String, String> {
    let id = format!("ssh-{}", uuid_like());
    let policy = *state.host_key_policy.lock().unwrap();
    let handle = ssh::connect_key(app.clone(), id.clone(), policy, &host, port, &user, &key_path, passphrase.as_deref(), cols, rows)
        .await.map_err(|e| e.to_string())?;
    state.ssh_sessions.lock().unwrap().insert(id.clone(), handle);
    Ok(id)
}

#[tauri::command]
async fn ssh_connect_jump(
    app: tauri::AppHandle, state: tauri::State<'_, AppState>,
    jump_host: String, jump_port: u16, jump_user: String, jump_password: String,
    host: String, port: u16, user: String, password: String,
    cols: u16, rows: u16,
) -> Result<String, String> {
    let id = format!("ssh-{}", uuid_like());
    let policy = *state.host_key_policy.lock().unwrap();
    let handle = ssh::connect_jump(
        app.clone(), id.clone(), policy,
        &jump_host, jump_port, &jump_user, &jump_password,
        &host, port, &user, &password,
        cols, rows,
    ).await.map_err(|e| e.to_string())?;
    state.ssh_sessions.lock().unwrap().insert(id.clone(), handle);
    Ok(id)
}

// ---------- 主机密钥校验 ----------
#[tauri::command]
fn set_host_key_policy(state: tauri::State<'_, AppState>, policy: HostKeyPolicy) {
    *state.host_key_policy.lock().unwrap() = policy;
}

#[tauri::command]
fn get_host_key_policy(state: tauri::State<'_, AppState>) -> HostKeyPolicy {
    *state.host_key_policy.lock().unwrap()
}

/// 供界面展示：当前使用的 known_hosts 文件路径。
#[tauri::command]
fn known_hosts_file() -> Option<String> {
    hostkey::known_hosts_path().map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
fn setup_x11(state: tauri::State<AppState>, session_id: String) -> Result<(), String> {
    let map = state.ssh_sessions.lock().unwrap();
    let h = map.get(&session_id).ok_or("session-not-found")?;
    let cmd = b"export DISPLAY=$(grep nameserver /etc/resolv.conf | awk '{print $2}'):1.0\n";
    h.write(cmd).map_err(|e| e.to_string())
}

// ---------- SFTP ----------
#[tauri::command]
async fn sftp_list_dir(state: tauri::State<'_, AppState>, session_id: String, path: String) -> Result<Vec<sftp::RemoteEntry>, String> {
    let sftp = { let map = state.ssh_sessions.lock().unwrap(); let h = map.get(&session_id).ok_or("session-not-found")?; h.sftp.clone() };
    sftp.list_dir(&path).await.map_err(|e| e.to_string())
}

/// 取出会话的 SFTP 句柄。
fn sftp_of(state: &tauri::State<'_, AppState>, session_id: &str) -> Result<Arc<sftp::SftpHandle>, String> {
    let map = state.ssh_sessions.lock().unwrap();
    let handle = map.get(session_id).ok_or("session-not-found")?;
    Ok(handle.sftp.clone())
}

/// 注册取消标志 → 执行传输 → 无论成功、失败还是取消都注销标志。
///
/// 四个传输命令原来各写一遍这套流程，且只有正常返回才会 remove：
/// 一旦传输报错，标志就永久留在表里。
async fn with_cancel_flag<F, Fut>(
    state: &tauri::State<'_, AppState>,
    transfer_id: &str,
    run: F,
) -> Result<(), String>
where
    F: FnOnce(Arc<sftp::TransferCancel>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let flag = sftp::TransferCancel::new();
    state.cancel_flags.lock().unwrap().insert(transfer_id.to_string(), flag.clone());
    let result = run(flag).await.map_err(|e| e.to_string());
    state.cancel_flags.lock().unwrap().remove(transfer_id);
    result
}

#[tauri::command]
async fn sftp_download(app: tauri::AppHandle, state: tauri::State<'_, AppState>, session_id: String, transfer_id: String, remote: String, local: String) -> Result<(), String> {
    let sftp = sftp_of(&state, &session_id)?;
    let lp = std::path::PathBuf::from(&local);
    with_cancel_flag(&state, &transfer_id, |flag| {
        let (sftp, app, sid, remote) = (sftp.clone(), app.clone(), session_id.clone(), remote.clone());
        async move { sftp.download_with_cancel(app, sid, &remote, &lp, flag).await }
    }).await
}

#[tauri::command]
async fn sftp_download_resume(app: tauri::AppHandle, state: tauri::State<'_, AppState>, session_id: String, transfer_id: String, remote: String, local: String) -> Result<(), String> {
    let sftp = sftp_of(&state, &session_id)?;
    let lp = std::path::PathBuf::from(&local);
    with_cancel_flag(&state, &transfer_id, |flag| {
        let (sftp, app, sid, remote) = (sftp.clone(), app.clone(), session_id.clone(), remote.clone());
        async move { sftp.download_resume(app, sid, &remote, &lp, flag).await }
    }).await
}

#[tauri::command]
async fn sftp_upload(app: tauri::AppHandle, state: tauri::State<'_, AppState>, session_id: String, transfer_id: String, local: String, remote: String) -> Result<(), String> {
    let sftp = sftp_of(&state, &session_id)?;
    let lp = std::path::PathBuf::from(&local);
    with_cancel_flag(&state, &transfer_id, |flag| {
        let (sftp, app, sid, remote) = (sftp.clone(), app.clone(), session_id.clone(), remote.clone());
        async move { sftp.upload_with_cancel(app, sid, &lp, &remote, flag).await }
    }).await
}

#[tauri::command]
async fn sftp_upload_resume(app: tauri::AppHandle, state: tauri::State<'_, AppState>, session_id: String, transfer_id: String, local: String, remote: String) -> Result<(), String> {
    let sftp = sftp_of(&state, &session_id)?;
    let lp = std::path::PathBuf::from(&local);
    with_cancel_flag(&state, &transfer_id, |flag| {
        let (sftp, app, sid, remote) = (sftp.clone(), app.clone(), session_id.clone(), remote.clone());
        async move { sftp.upload_resume(app, sid, &lp, &remote, flag).await }
    }).await
}

/// 上传一个本地路径：是目录就递归上传，否则按单文件上传。
///
/// `transfer_id` 由前端传入，用于把这次上传登记进取消表——
/// 目录上传耗时最长，没有它用户无法中止。
/// 兼容旧调用：不传时自动生成，但那样就无法被取消了。
#[tauri::command]
async fn sftp_upload_path(
    app: tauri::AppHandle, state: tauri::State<'_, AppState>,
    session_id: String, local: String, remote: String,
    transfer_id: Option<String>,
) -> Result<(), String> {
    let sftp = sftp_of(&state, &session_id)?;
    let lp = std::path::PathBuf::from(&local);
    let id = transfer_id.unwrap_or_else(|| format!("upload-path-{}", uuid_like()));
    if lp.is_dir() {
        with_cancel_flag(&state, &id, |flag| {
            let (sftp, app, sid, remote) = (sftp.clone(), app.clone(), session_id.clone(), remote.clone());
            async move { sftp.upload_dir(app, sid, &lp, &remote, flag).await }
        }).await
    } else {
        with_cancel_flag(&state, &id, |flag| {
            let (sftp, app, sid, remote) = (sftp.clone(), app.clone(), session_id.clone(), remote.clone());
            async move { sftp.upload_with_cancel(app, sid, &lp, &remote, flag).await }
        }).await
    }
}

/// 界面上的"取消"按钮。标记为用户主动取消，
/// 传输层据此决定是否删除没传完的目标文件（网络中断则保留，供续传）。
#[tauri::command]
fn cancel_transfer(state: tauri::State<AppState>, transfer_id: String) -> Result<(), String> {
    let map = state.cancel_flags.lock().unwrap();
    if let Some(flag) = map.get(&transfer_id) {
        flag.request_user_cancel();
    }
    Ok(())
}

#[tauri::command]
async fn sftp_delete(state: tauri::State<'_, AppState>, session_id: String, path: String, is_dir: bool) -> Result<(), String> {
    let sftp = { let map = state.ssh_sessions.lock().unwrap(); let h = map.get(&session_id).ok_or("session-not-found")?; h.sftp.clone() };
    sftp.remove(&path, is_dir).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn sftp_mkdir(state: tauri::State<'_, AppState>, session_id: String, path: String) -> Result<(), String> {
    let sftp = { let map = state.ssh_sessions.lock().unwrap(); let h = map.get(&session_id).ok_or("session-not-found")?; h.sftp.clone() };
    sftp.mkdir(&path).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn sftp_rename(state: tauri::State<'_, AppState>, session_id: String, old_path: String, new_path: String) -> Result<(), String> {
    let sftp = { let map = state.ssh_sessions.lock().unwrap(); let h = map.get(&session_id).ok_or("session-not-found")?; h.sftp.clone() };
    sftp.rename(&old_path, &new_path).await.map_err(|e| e.to_string())
}

// ---------- 会话持久化 ----------
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedSession {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub color: String,
    /// 是否已把密码存进系统凭据库。
    /// 注意这里只存标记，**密码本身绝不写入 sessions.json**。
    #[serde(default)]
    pub save_password: bool,
}

fn sessions_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "rustterm", "RustTerm")?;
    let dir = dirs.config_dir();
    std::fs::create_dir_all(dir).ok()?;
    Some(dir.join("sessions.json"))
}

fn load_sessions() -> Vec<SavedSession> {
    let Some(p) = sessions_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&p) else { return Vec::new() };
    serde_json::from_str(&text).unwrap_or_default()
}

fn store_sessions(list: &[SavedSession]) {
    let Some(p) = sessions_path() else { return };
    if let Ok(text) = serde_json::to_string_pretty(list) {
        let _ = std::fs::write(p, text);
    }
}

#[tauri::command]
fn save_session_full(
    host: String, port: u16, user: String, group: String, color: String,
    save_password: bool, password: Option<String>,
) -> Result<(), String> {
    let mut list = load_sessions();
    let name = format!("{user}@{host}");

    // 凭据库操作先做：失败就不要把"已保存"写进配置，否则界面会撒谎。
    if save_password {
        match password {
            // 用户提供了新密码 → 写入凭据库
            Some(pw) if !pw.is_empty() => secret::save(&user, &host, port, &pw)?,
            // 没提供新密码，但此前已存过 → 保持不变
            _ => {
                let already = list.iter().any(|s| s.host == host && s.user == user && s.port == port && s.save_password);
                if !already {
                    return Err("secret-password-required".into());
                }
            }
        }
    } else if list.iter().any(|s| s.host == host && s.user == user && s.port == port && s.save_password) {
        // 用户取消勾选 → 顺手清掉凭据，避免留下无人引用的密码
        secret::delete(&user, &host, port)?;
    }

    // 保存会话本身是 upsert：早前实现遇到已存在的就直接返回，
    // 结果用户改分组/颜色/记住密码都无效。
    match list.iter_mut().find(|s| s.host == host && s.user == user && s.port == port) {
        Some(existing) => {
            existing.group = group;
            existing.color = color;
            existing.save_password = save_password;
            existing.name = name;
        }
        None => list.push(SavedSession {
            name, host, port, user, group, color, save_password,
        }),
    }
    store_sessions(&list);
    Ok(())
}

#[tauri::command]
fn list_sessions() -> Vec<SavedSession> { load_sessions() }

#[tauri::command]
fn delete_session(host: String, port: u16, user: String) -> Result<(), String> {
    let mut list = load_sessions();
    // 一并清掉凭据库里的密码，否则会留下再也访问不到的孤儿记录
    if list.iter().any(|s| s.host == host && s.user == user && s.port == port && s.save_password) {
        let _ = secret::delete(&user, &host, port);
    }
    list.retain(|s| !(s.host == host && s.user == user && s.port == port));
    store_sessions(&list);
    Ok(())
}

// ---------- 密码凭据库 ----------
#[tauri::command]
fn secret_store_status() -> secret::StoreStatus {
    secret::store_status()
}

#[tauri::command]
fn save_secret(host: String, port: u16, user: String, password: String) -> Result<(), String> {
    secret::save(&user, &host, port, &password)
}

/// 读取已保存的密码。没有记录返回 `None`，这是正常情况而非错误。
#[tauri::command]
fn get_secret(host: String, port: u16, user: String) -> Result<Option<String>, String> {
    secret::load(&user, &host, port)
}

#[tauri::command]
fn delete_secret(host: String, port: u16, user: String) -> Result<(), String> {
    secret::delete(&user, &host, port)
}

#[tauri::command]
fn import_putty_sessions() -> Result<Vec<SavedSession>, String> {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("reg");
        cmd.args(["query", r"HKCU\Software\SimonTatham\PuTTY\Sessions", "/s"]);
        hide_console(&mut cmd);
        let out = cmd.output().map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut list = Vec::new();
        let mut cur_host = String::new();
        for line in text.lines() {
            if line.contains("HostName") {
                if let Some(v) = line.split("REG_SZ").nth(1) { cur_host = v.trim().to_string(); }
            }
            if line.contains("UserName") {
                if let Some(v) = line.split("REG_SZ").nth(1) {
                    let user = v.trim().to_string();
                    if !cur_host.is_empty() {
                        list.push(SavedSession {
                            name: format!("{}@{}", user, cur_host),
                            host: cur_host.clone(),
                            port: 22,
                            user,
                            group: String::new(),
                            color: String::new(),
                            save_password: false,
                        });
                    }
                }
            }
        }
        let mut existing = load_sessions();
        for s in &list {
            if !existing.iter().any(|e| e.host == s.host && e.user == s.user) {
                existing.push(s.clone());
            }
        }
        store_sessions(&existing);
        Ok(list)
    }
    #[cfg(not(windows))] { Ok(Vec::new()) }
}

#[tauri::command]
fn export_sessions(path: String) -> Result<(), String> {
    let list = load_sessions();
    let json = serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

#[tauri::command]
fn import_sessions(path: String) -> Result<usize, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let imported: Vec<SavedSession> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut existing = load_sessions();
    let mut count = 0;
    for s in imported {
        if !existing.iter().any(|e| e.host == s.host && e.user == s.user && e.port == s.port) {
            existing.push(s); count += 1;
        }
    }
    store_sessions(&existing);
    Ok(count)
}

// ---------- X server ----------
// 只返回机器可读的状态码，文案交给前端 i18n。
// 原先在这里返回中文串，英文界面下状态栏会冒出中文。
#[tauri::command]
fn xserver_start(state: tauri::State<XServerState>) -> Result<(), String> {
    let mut guard = state.0.lock().unwrap();
    if guard.is_some() { return Ok(()); }
    let candidates = [r"C:\Program Files\VcXsrv\vcxsrv.exe", r"C:\Program Files (x86)\VcXsrv\vcxsrv.exe"];
    for path in candidates {
        if std::path::Path::new(path).exists() {
            let child = Command::new(path)
                .args([":1", "-multiwindow", "-clipboard", "-wgl", "-ac"])
                .spawn()
                .map_err(|e| format!("xserver-spawn-failed:{e}"))?;
            *guard = Some(child);
            return Ok(());
        }
    }
    Err("xserver-not-found".into())
}

#[tauri::command]
fn xserver_stop(state: tauri::State<XServerState>) -> Result<(), String> {
    let mut guard = state.0.lock().unwrap();
    if let Some(mut child) = guard.take() { let _ = child.kill(); }
    Ok(())
}

// ---------- Tunneling ----------
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TunnelSpec { pub local_port: u16, pub remote_host: String, pub remote_port: u16 }

#[tauri::command]
async fn tunnel_start(state: tauri::State<'_, AppState>, tunnel_state: tauri::State<'_, TunnelState>, session_id: String, spec: TunnelSpec) -> Result<u16, String> {
    use tokio::net::TcpListener;
    let client = {
        let map = state.ssh_sessions.lock().unwrap();
        let handle = map.get(&session_id).ok_or("session-not-found")?;
        handle.client.clone()
    };
    let listener = TcpListener::bind(("127.0.0.1", spec.local_port)).await.map_err(|e| e.to_string())?;
    let actual_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let remote_host = spec.remote_host.clone();
    let remote_port = spec.remote_port;
    let sid = session_id.clone();

    // 保存 JoinHandle，删除隧道时才能真正停掉监听、释放本地端口。
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut incoming, peer)) = listener.accept().await else { break };
            let client = client.clone();
            let rh = remote_host.clone();
            let sid = sid.clone();
            tokio::spawn(async move {
                match client.channel_open_direct_tcpip(&rh, remote_port as u32, "127.0.0.1", 0).await {
                    Ok(chan) => {
                        let mut stream = chan.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut incoming, &mut stream).await;
                    }
                    Err(e) => eprintln!("tunnel {sid} -> {peer} failed: {e}"),
                }
            });
        }
    });

    let record = TunnelRecord {
        id: format!("tn-{}", uuid_like()), session_id,
        local_port: actual_port, remote_host: spec.remote_host, remote_port: spec.remote_port,
    };
    tunnel_state.0.lock().unwrap().push(Tunnel { record, task });
    Ok(actual_port)
}

#[tauri::command]
fn list_tunnels(state: tauri::State<TunnelState>) -> Vec<TunnelRecord> {
    state.0.lock().unwrap().iter().map(|t| t.record.clone()).collect()
}

#[tauri::command]
fn remove_tunnel(state: tauri::State<TunnelState>, id: String) -> Result<(), String> {
    let mut tunnels = state.0.lock().unwrap();
    let mut removed = Vec::new();
    tunnels.retain(|t| {
        if t.record.id == id { removed.push(t.task.abort_handle()); false } else { true }
    });
    drop(tunnels);
    // 中止监听循环，listener 随之释放。
    for handle in removed { handle.abort(); }
    Ok(())
}

// ---------- Telnet ----------
#[tauri::command]
async fn telnet_connect(app: tauri::AppHandle, state: tauri::State<'_, AppState>, host: String, port: u16) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;
    let id = format!("telnet-{}", uuid_like());
    let stream = TcpStream::connect((host.as_str(), port)).await.map_err(|e| e.to_string())?;
    let (mut read, write) = stream.into_split();
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let handle = pty::PtyHandle::from_tcp(write);
    state.sessions.lock().unwrap().insert(id.clone(), handle);
    let app_out = app.clone();
    let id_out = id.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => { let _ = app_out.emit("pty:close", serde_json::json!({ "sessionId": id_out })); break; }
                Ok(n) => { let _ = app_out.emit("pty:data", serde_json::json!({ "sessionId": id_out, "data": &buf[..n] })); }
            }
        }
    });
    Ok(id)
}

// ---------- 工具 ----------
#[tauri::command]
fn path_exists(path: String) -> bool { std::path::Path::new(&path).exists() }

#[tauri::command]
fn file_version(path: String) -> Result<String, String> {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Item '{}').VersionInfo.FileVersion", path.replace('\'', "''")),
        ]);
        hide_console(&mut cmd);
        let out = cmd.output().map_err(|e| e.to_string())?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // 空版本由前端按"未安装/未知"处理。
        Ok(version)
    }
    #[cfg(not(windows))] { Ok(String::new()) }
}

/// 用系统默认程序打开 URL 或可执行文件。
///
/// Windows 上走 `explorer.exe`，它不会解释 shell 元字符；
/// 原实现经由 `cmd /C start` 打开用户提供的字符串，存在命令注入面。
#[tauri::command]
fn open_external(program: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        Command::new("explorer.exe").arg(&program).spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open").arg(&program).spawn().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn exit_app(app: tauri::AppHandle) { app.exit(0); }

/// 前端切换语言后调用，重建原生菜单栏。
///
/// 菜单文本在 Rust 侧生成，前端改不了它；不重建的话
/// 界面是中文而菜单栏一直是英文。
#[tauri::command]
fn set_menu_language(app: tauri::AppHandle, lang: String) -> Result<(), String> {
    menu::set_english(lang == "en");
    let rebuilt = menu::build_with(&app).map_err(|e| e.to_string())?;
    app.set_menu(rebuilt).map_err(|e| e.to_string())?;
    Ok(())
}

/// 生成会话/隧道 ID。
///
/// 原实现只取纳秒时间戳，同一纳秒内创建的两个会话会拿到相同 ID，
/// 后者插入 HashMap 时直接覆盖前者，导致前一个会话的句柄被丢弃。
/// 这里叠加一个单调递增计数器，保证进程内唯一。
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{seq:x}")
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            sessions: Mutex::new(HashMap::new()),
            ssh_sessions: Mutex::new(HashMap::new()),
            cancel_flags: Mutex::new(HashMap::new()),
            host_key_policy: Mutex::new(HostKeyPolicy::default()),
        })
        .manage(XServerState(StdMutex::new(None)))
        .manage(TunnelState(Mutex::new(Vec::new())))
        .invoke_handler(tauri::generate_handler![
            pty_spawn, pty_write, pty_resize, pty_close,
            ssh_connect, ssh_connect_key, ssh_connect_jump, setup_x11,
            sftp_list_dir, sftp_download, sftp_download_resume,
            sftp_upload, sftp_upload_resume, sftp_upload_path,
            sftp_delete, sftp_mkdir, sftp_rename, cancel_transfer,
            save_session_full, list_sessions, delete_session,
            secret_store_status, save_secret, get_secret, delete_secret,
            import_putty_sessions, export_sessions, import_sessions,
            xserver_start, xserver_stop,
            tunnel_start, list_tunnels, remove_tunnel,
            telnet_connect, portscan::port_scan,portscan::cancel_scan,
            path_exists, file_version, open_external, exit_app, set_menu_language,
            set_host_key_policy, get_host_key_policy, known_hosts_file,
        ])
        .setup(|app| {
            let built = menu::build(app)?;
            app.set_menu(built)?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            let id = event.id().as_ref();
            match id {
                "term-exit" => app.exit(0),
                _ => { let _ = app.emit("menu:action", id); }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}