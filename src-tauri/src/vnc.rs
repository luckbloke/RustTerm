//! 纯 Rust 的 VNC 客户端实现，基于 vnc-rs。

use anyhow::Result;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use vnc::{
    ClientKeyEvent, ClientMouseEvent, PixelFormat, Rect, Screen, VncClient, VncConnector,
    VncEncoding, VncError, VncEvent, VncLimits, VncSecurityPolicy, VncServerKey, X11Event,
};

const MAX_FRAMEBUFFER_WIDTH: u16 = 7680;
const MAX_FRAMEBUFFER_HEIGHT: u16 = 4320;
const MAX_VNC_CLIPBOARD_BYTES: usize = 1024 * 1024;
const MAX_INPUT_BATCH: usize = 256;
const MAX_PENDING_FRAMES: usize = 2;
const WORKER_COMMAND_CHANNEL_CAPACITY: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const SERVER_KEY_PROMPT_TIMEOUT: Duration = Duration::from_mins(1);
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(8);
const FRAME_EMIT_INTERVAL: Duration = Duration::from_millis(16);
const UPDATE_REQUEST_INTERVAL: Duration = Duration::from_millis(16);

// ============================================================================
// 状态与错误
// ============================================================================

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VncState {
    Connecting,
    Authenticating,
    Negotiating,
    Active,
    Reconnecting,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VncErrorKind {
    Transport,
    Authentication,
    Protocol,
    Encoding,
    Clipboard,
    Internal,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VncStateEvent {
    pub session_id: String,
    pub state: VncState,
    pub message: Option<String>,
    pub error_kind: Option<VncErrorKind>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VncServerKeyVerifyEvent {
    pub request_id: String,
    pub session_id: String,
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub key_bits: u32,
    pub known_host_status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VncClipboardEvent {
    pub session_id: String,
    pub text: String,
}

// ============================================================================
// 输入事件
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum VncInputEvent {
    #[serde(rename = "key")]
    Key {
        keysym: u32,
        pressed: bool,
    },
    #[serde(rename = "pointer")]
    Pointer {
        x: u16,
        y: u16,
        #[serde(rename = "buttonMask")]
        button_mask: u8,
    },
    #[serde(rename = "release-all-keys")]
    ReleaseAllKeys,
}

// ============================================================================
// 连接配置
// ============================================================================

#[derive(Debug, Clone)]
pub struct VncConnectConfig {
    pub session_id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    /// "auto" | "none" | "vnc-auth"
    pub security_mode: String,
    /// "fit" | "full" | "1:1"（前端处理，后端不感知）
    pub scale_mode: String,
    pub clipboard_enabled: bool,
    pub reconnect_enabled: bool,
    pub reconnect_max_attempts: u32,
    pub shared: bool,
    pub view_only: bool,
}

// ============================================================================
// 像素解码
// ============================================================================

/// 按 RFB 协议从原始字节里解出一个像素。
///
/// [RFC6143, 7.4] 定义了 PixelFormat：
///   bits_per_pixel: 8 / 16 / 32
///   big_endian_flag: 多字节像素在线上是否用大端
///   true_color_flag: 1 = 真彩，0 = 调色板（不支持）
///   red_max/green_max/blue_max: 每种分量的最大值（通常 255，也可能 31/63）
///   red_shift/green_shift/blue_shift: 分量在像素值里的位移
fn decode_pixel(bytes: &[u8], pf: &PixelFormat) -> (u8, u8, u8) {
    // ---- 32bpp 硬编码 ----
    //
    // 服务器报告的 PixelFormat 是 rshift=16/gshift=8/bshift=0、小端，
    // 但不同 VNC 服务器在 32bpp 下实际发的字节顺序不统一（BGRA / RGBA / ARGB）。
    // 与其依赖 shift 的通用逻辑，不如直接试字节顺序。
    //
    // 下面这行是 BGRA。如果画面颜色反了，依次换成 RGBA / ARGB 试。
    if pf.bits_per_pixel == 32 {
        // 顺序 A：BGRA（小端，最常见）
        return (bytes[2], bytes[1], bytes[0]);

        // 顺序 B：RGBA
        //return (bytes[0], bytes[1], bytes[2]);

        // 顺序 C：ARGB
        //return (bytes[1], bytes[2], bytes[3]);
    }

    // ---- 8/16bpp 走通用逻辑 ----
    let value: u32 = match pf.bits_per_pixel {
        8 => u32::from(bytes[0]),
        16 => {
            if pf.big_endian_flag != 0 {
                u32::from(u16::from_be_bytes([bytes[0], bytes[1]]))
            } else {
                u32::from(u16::from_le_bytes([bytes[0], bytes[1]]))
            }
        }
        _ => return (0, 0, 0),
    };

    if pf.true_color_flag == 0 {
        return (0, 0, 0);
    }

    let r = (value >> pf.red_shift) & u32::from(pf.red_max);
    let g = (value >> pf.green_shift) & u32::from(pf.green_max);
    let b = (value >> pf.blue_shift) & u32::from(pf.blue_max);

    let scale = |v: u32, max: u16| -> u8 {
        if max == 0 {
            0
        } else {
            ((v * 255) / u32::from(max)) as u8
        }
    };

    (
        scale(r, pf.red_max),
        scale(g, pf.green_max),
        scale(b, pf.blue_max),
    )
}

// ============================================================================
// 帧缓冲
// ============================================================================

struct VncFramebuffer {
    width: u16,
    height: u16,
    rgb: Vec<u8>,
}

impl VncFramebuffer {
    fn new(width: u16, height: u16) -> Result<Self> {
        if width == 0
            || height == 0
            || width > MAX_FRAMEBUFFER_WIDTH
            || height > MAX_FRAMEBUFFER_HEIGHT
        {
            anyhow::bail!("VNC framebuffer {width}x{height} 超出范围");
        }
        let len = usize::from(width) * usize::from(height) * 3;
        Ok(Self {
            width,
            height,
            rgb: vec![0u8; len],
        })
    }

    /// 按 PixelFormat 解码服务器原始字节，写入 RGB 帧缓冲。
    fn blit_raw(&mut self, rect: Rect, data: &[u8], pf: &PixelFormat) -> Result<()> {
        let bpp = (pf.bits_per_pixel / 8) as usize;
        if bpp == 0 {
            anyhow::bail!("VNC bits_per_pixel 为 0");
        }

        let rx = rect.x as usize;
        let ry = rect.y as usize;
        let rw = rect.width as usize;
        let rh = rect.height as usize;

        if rw == 0 || rh == 0 || rx + rw > self.width as usize || ry + rh > self.height as usize {
            anyhow::bail!("VNC 矩形超出 framebuffer 边界");
        }

        let src_stride = rw * bpp;
        let dst_stride = self.width as usize * 3;
        let expected = src_stride * rh;
        if data.len() < expected {
            anyhow::bail!(
                "VNC 数据长度 {} 小于期望 {}（bpp={}, rw={}, rh={}）",
                data.len(),
                expected,
                pf.bits_per_pixel,
                rw,
                rh
            );
        }

        for row in 0..rh {
            let src_start = row * src_stride;
            let dst_start = (ry + row) * dst_stride + rx * 3;
            let src_row = &data[src_start..src_start + src_stride];
            let dst_row = &mut self.rgb[dst_start..dst_start + rw * 3];

            for col in 0..rw {
                let px = &src_row[col * bpp..col * bpp + bpp];
                let (r, g, b) = decode_pixel(px, pf);
                dst_row[col * 3] = r;
                dst_row[col * 3 + 1] = g;
                dst_row[col * 3 + 2] = b;
            }
        }
        Ok(())
    }

    /// framebuffer 内一块复制到另一块（CopyRect 编码）。
    fn copy_rect(&mut self, dst: Rect, src: Rect) -> Result<()> {
        let w = src.width.min(dst.width) as usize;
        let h = src.height.min(dst.height) as usize;
        if w == 0 || h == 0 {
            return Ok(());
        }
        let stride = self.width as usize * 3;
        let mut tmp = vec![0u8; w * 3];
        for y in 0..h {
            let src_y = src.y as usize + y;
            let src_x = src.x as usize;
            let dst_y = dst.y as usize + y;
            let dst_x = dst.x as usize;
            let src_start = src_y * stride + src_x * 3;
            tmp.copy_from_slice(&self.rgb[src_start..src_start + w * 3]);
            let dst_start = dst_y * stride + dst_x * 3;
            self.rgb[dst_start..dst_start + w * 3].copy_from_slice(&tmp);
        }
        Ok(())
    }
}

/// 帧包格式（和 RDP 一致，前端统一解析）：
///   byte 0-1  : x
///   byte 2-3  : y
///   byte 4-5  : width
///   byte 6-7  : height
///   byte 8-11 : jpeg 字节数
///   byte 12.. : jpeg 数据
fn pack_frame(x: u16, y: u16, w: u16, h: u16, jpeg: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + jpeg.len());
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf.extend_from_slice(&w.to_le_bytes());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
    buf.extend_from_slice(jpeg);
    buf
}

fn union_rect(a: Option<(u32, u32, u32, u32)>, b: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
    match a {
        None => b,
        Some(d) => {
            let x1 = d.0.min(b.0);
            let y1 = d.1.min(b.1);
            let x2 = (d.0 + d.2).max(b.0 + b.2);
            let y2 = (d.1 + d.3).max(b.1 + b.3);
            (x1, y1, x2 - x1, y2 - y1)
        }
    }
}

// ============================================================================
// 会话
// ============================================================================

enum VncWorkerCommand {
    Input(VncInputEvent),
    Clipboard(String),
    FullRefresh,
}

struct VncServerKeyPending {
    payload: VncServerKeyVerifyEvent,
    session_id: String,
    generation: u64,
    responder: oneshot::Sender<bool>,
}

pub struct VncSession {
    pub config: VncConnectConfig,
    state: Mutex<VncState>,
    message: Mutex<Option<String>>,
    error_kind: Mutex<Option<VncErrorKind>>,
    generation: AtomicU64,
    frame_attach_id: AtomicU64,
    frame_channel: Mutex<Option<(u64, Channel<InvokeResponseBody>)>>,
    pending_frames: Mutex<VecDeque<Vec<u8>>>,
    framebuffer: Mutex<Option<VncFramebuffer>>,
    pixel_format: Mutex<Option<PixelFormat>>,
    command_sender: Mutex<Option<mpsc::Sender<VncWorkerCommand>>>,
    cancel_sender: Mutex<Option<watch::Sender<bool>>>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    close_requested: AtomicBool,
    pending_server_keys: Arc<Mutex<std::collections::HashMap<String, VncServerKeyPending>>>,
}

pub struct VncSessionManager {
    sessions: Mutex<std::collections::HashMap<String, Arc<VncSession>>>,
    pending_server_keys: Arc<Mutex<std::collections::HashMap<String, VncServerKeyPending>>>,
}

impl VncSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(std::collections::HashMap::new()),
            pending_server_keys: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    pub async fn create_session(
        self: &Arc<Self>,
        app: AppHandle,
        config: VncConnectConfig,
    ) -> Result<String> {
        let session_id = config.session_id.clone();
        let session = Arc::new(VncSession {
            config,
            state: Mutex::new(VncState::Connecting),
            message: Mutex::new(None),
            error_kind: Mutex::new(None),
            generation: AtomicU64::new(0),
            frame_attach_id: AtomicU64::new(0),
            frame_channel: Mutex::new(None),
            pending_frames: Mutex::new(VecDeque::with_capacity(MAX_PENDING_FRAMES)),
            framebuffer: Mutex::new(None),
            pixel_format: Mutex::new(None),
            command_sender: Mutex::new(None),
            cancel_sender: Mutex::new(None),
            worker: Mutex::new(None),
            close_requested: AtomicBool::new(false),
            pending_server_keys: self.pending_server_keys.clone(),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), session.clone());
        emit_state(&app, &session, VncState::Connecting, None, None);
        spawn_session_worker(app, session).await;
        Ok(session_id)
    }

    pub async fn attach_frame_channel(
        &self,
        session_id: &str,
        channel: Channel<InvokeResponseBody>,
    ) -> Result<()> {
        let session = self.get(session_id).await?;
        let attach_id = session.frame_attach_id.fetch_add(1, Ordering::AcqRel) + 1;
        *session.frame_channel.lock().await = Some((attach_id, channel));
        if session.framebuffer.lock().await.is_some() {
            session.pending_frames.lock().await.clear();
        } else {
            flush_pending_frames(&session, attach_id).await;
        }
        send_full_frame(&session, attach_id).await?;
        Ok(())
    }

    pub async fn detach_frame_channel(&self, session_id: &str) -> Result<()> {
        let session = self.get(session_id).await?;
        session.frame_attach_id.fetch_add(1, Ordering::AcqRel);
        *session.frame_channel.lock().await = None;
        Ok(())
    }

    pub async fn send_input(&self, session_id: &str, events: Vec<VncInputEvent>) -> Result<()> {
        let session = self.get(session_id).await?;
        if session.config.view_only {
            anyhow::bail!("VNC 只读模式禁止输入");
        }
        if events.len() > MAX_INPUT_BATCH {
            anyhow::bail!("VNC 输入批次超过 {MAX_INPUT_BATCH}");
        }
        let sender = session
            .command_sender
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("VNC worker 未运行"))?;
        for ev in events {
            sender
                .send(VncWorkerCommand::Input(ev))
                .await
                .map_err(|_| anyhow::anyhow!("VNC worker 通道关闭"))?;
        }
        Ok(())
    }

    pub async fn set_clipboard_text(&self, session_id: &str, text: String) -> Result<()> {
        let session = self.get(session_id).await?;
        if !session.config.clipboard_enabled {
            anyhow::bail!("VNC 剪贴板已禁用");
        }
        if text.len() > MAX_VNC_CLIPBOARD_BYTES {
            anyhow::bail!("VNC 剪贴板文本超过 1MB");
        }
        if !text.chars().all(|c| u32::from(c) <= 0xff) {
            anyhow::bail!("VNC 剪贴板只支持 Latin-1");
        }
        send_worker_command(&session, VncWorkerCommand::Clipboard(text)).await
    }

    pub async fn close(&self, app: &AppHandle, session_id: &str) -> Result<()> {
        let Some(session) = self.sessions.lock().await.remove(session_id) else {
            return Ok(());
        };
        session.close_requested.store(true, Ordering::Release);
        stop_worker(&session).await;
        self.cancel_pending_server_keys_for_session(session_id).await;
        session.frame_attach_id.fetch_add(1, Ordering::AcqRel);
        *session.frame_channel.lock().await = None;
        session.pending_frames.lock().await.clear();
        *session.framebuffer.lock().await = None;
        set_state(&session, VncState::Disconnected, None, None).await;
        emit_state(app, &session, VncState::Disconnected, None, None);
        let _ = app.emit("vnc:closed", serde_json::json!({ "sessionId": session_id }));
        Ok(())
    }

    pub async fn respond_server_key(&self, request_id: &str, accepted: bool) -> Result<()> {
        let Some(pending) = self.pending_server_keys.lock().await.remove(request_id) else {
            anyhow::bail!("没有待决的 VNC server key 请求: {request_id}");
        };
        let _ = pending.responder.send(accepted);
        Ok(())
    }

    async fn cancel_pending_server_keys_for_session(&self, session_id: &str) {
        let mut pending = self.pending_server_keys.lock().await;
        let ids: Vec<String> = pending
            .iter()
            .filter(|(_, p)| p.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(p) = pending.remove(&id) {
                let _ = p.responder.send(false);
            }
        }
    }

    async fn get(&self, session_id: &str) -> Result<Arc<VncSession>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("VNC 会话 {session_id} 不存在"))
    }
}

impl Default for VncSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 会话 worker
// ============================================================================

async fn spawn_session_worker(app: AppHandle, session: Arc<VncSession>) {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    *session.cancel_sender.lock().await = Some(cancel_tx);
    let session_for_task = session.clone();
    let handle = tokio::spawn(async move {
        run_session_worker(app, session_for_task, cancel_rx).await;
    });
    *session.worker.lock().await = Some(handle);
}

async fn run_session_worker(
    app: AppHandle,
    session: Arc<VncSession>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut attempt = 0u32;
    loop {
        if session.close_requested.load(Ordering::Acquire) || *cancel_rx.borrow() {
            return;
        }
        let generation = session.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let state = if attempt == 0 {
            VncState::Connecting
        } else {
            VncState::Reconnecting
        };
        set_state(&session, state.clone(), None, None).await;
        emit_state(&app, &session, state, None, None);

        match run_protocol_generation(&app, &session, generation, &mut cancel_rx).await {
            Ok(()) => return,
            Err((kind, message, retryable)) => {
                if session.close_requested.load(Ordering::Acquire) || *cancel_rx.borrow() {
                    return;
                }
                if !retryable
                    || !session.config.reconnect_enabled
                    || attempt >= session.config.reconnect_max_attempts
                {
                    set_state(
                        &session,
                        VncState::Failed,
                        Some(message.clone()),
                        Some(kind),
                    )
                    .await;
                    emit_state(
                        &app,
                        &session,
                        VncState::Failed,
                        Some(message),
                        Some(kind),
                    );
                    return;
                }
                attempt += 1;
                let delay = reconnect_delay(attempt);
                let sleep = tokio::time::sleep(delay);
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {},
                    changed = cancel_rx.changed() => {
                        if changed.is_err() || *cancel_rx.borrow() { return; }
                    }
                }
            }
        }
    }
}

async fn run_protocol_generation(
    app: &AppHandle,
    session: &Arc<VncSession>,
    generation: u64,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<(), (VncErrorKind, String, bool)> {
    // ---------- 1. TCP 连接 ----------
    let tcp = tokio::select! {
        result = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((session.config.host.as_str(), session.config.port)),
        ) => {
            result
                .map_err(|_| (VncErrorKind::Transport, "VNC 连接超时".to_string(), true))?
                .map_err(|e| (VncErrorKind::Transport, format!("VNC TCP 连接失败: {e}"), true))?
        }
        _ = cancel_rx.changed() => return Ok(()),
    };

    let std_tcp = tcp
        .into_std()
        .map_err(|e| (VncErrorKind::Transport, format!("into_std 失败: {e}"), true))?;
    let std_tcp_for_request = std_tcp
        .try_clone()
        .map_err(|e| (VncErrorKind::Transport, format!("try_clone 失败: {e}"), true))?;
    let tcp_for_vnc = TcpStream::from_std(std_tcp)
        .map_err(|e| (VncErrorKind::Transport, format!("from_std(vnc) 失败: {e}"), true))?;
    let tcp_for_request = TcpStream::from_std(std_tcp_for_request)
        .map_err(|e| (VncErrorKind::Transport, format!("from_std(req) 失败: {e}"), true))?;

    // ---------- 2. 认证 ----------
    set_state(session, VncState::Authenticating, None, None).await;
    emit_state(app, session, VncState::Authenticating, None, None);

    let password = session.config.password.clone().unwrap_or_default();
    let has_password = session.config.password.is_some();
    let auth_password = password.clone();

    let app_for_verifier = app.clone();
    let session_for_verifier = session.clone();
    let pending_for_verifier = session.pending_server_keys.clone();

    let connector = VncConnector::new(tcp_for_vnc)
        .set_auth_method(async move { Ok(auth_password) })
        .set_credentials_available(has_password)
        .set_username(session.config.username.clone())
        .set_security_policy(security_policy(&session.config.security_mode))
        // 强制 pixel format
        .set_pixel_format(PixelFormat::bgra())
        // 客户端通过 SetPixelFormat 事件动态适配。
        .add_encoding(VncEncoding::Zrle)
        .add_encoding(VncEncoding::Tight)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .allow_shared(session.config.shared)
        .set_limits(VncLimits {
            max_framebuffer_width: MAX_FRAMEBUFFER_WIDTH,
            max_framebuffer_height: MAX_FRAMEBUFFER_HEIGHT,
            max_framebuffer_pixels: usize::from(MAX_FRAMEBUFFER_WIDTH)
                * usize::from(MAX_FRAMEBUFFER_HEIGHT),
            max_clipboard_bytes: MAX_VNC_CLIPBOARD_BYTES,
            max_rectangles_per_update: 1024,
            max_encoded_payload_bytes: 64 * 1024 * 1024,
            max_decoded_payload_bytes: usize::from(MAX_FRAMEBUFFER_WIDTH)
                * usize::from(MAX_FRAMEBUFFER_HEIGHT)
                * 4,
            channel_capacity: 32,
            ..VncLimits::default()
        })
        .set_server_key_verifier(move |key| {
            let app = app_for_verifier.clone();
            let session = session_for_verifier.clone();
            let pending = pending_for_verifier.clone();
            async move { verify_vnc_server_key(&app, &session, generation, &pending, key).await }
        })
        .build()
        .map_err(classify_vnc_error)?;

    let handshake = tokio::select! {
        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.try_start()) => {
            result
                .map_err(|_| (VncErrorKind::Transport, "VNC 握手超时".to_string(), true))?
                .map_err(classify_vnc_error)?
        }
        _ = cancel_rx.changed() => return Ok(()),
    };
    let client = handshake.finish().map_err(classify_vnc_error)?;

    // ---------- 3. 事件循环 ----------
    set_state(session, VncState::Negotiating, None, None).await;
    emit_state(app, session, VncState::Negotiating, None, None);

    let (command_tx, mut command_rx) =
        mpsc::channel::<VncWorkerCommand>(WORKER_COMMAND_CHANNEL_CAPACITY);
    *session.command_sender.lock().await = Some(command_tx);

    let mut req_writer = tokio::io::BufWriter::new(tcp_for_request);
    let mut pressed_keys: Vec<u32> = Vec::new();
    let mut pending_full_request = true;
    let mut last_request = tokio::time::Instant::now() - UPDATE_REQUEST_INTERVAL;
    let mut last_emit = tokio::time::Instant::now() - FRAME_EMIT_INTERVAL;
    let mut pending_inputs: Vec<VncInputEvent> = Vec::new();
    let mut last_input_flush = tokio::time::Instant::now();
    let mut dirty_rect: Option<(u32, u32, u32, u32)> = None;
    let mut ticker = tokio::time::interval(EVENT_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if session.generation.load(Ordering::Acquire) != generation {
            let _ = client.close().await;
            return Ok(());
        }

        tokio::select! {
            changed = cancel_rx.changed() => {
                let _ = changed;
                release_pressed_keys(&client, &mut pressed_keys).await;
                let _ = client.close().await;
                return Ok(());
            }

            event = client.poll_event() => {
                match event {
                    Ok(Some(ev)) => {
                        handle_vnc_event(app, session, generation, ev, &mut dirty_rect).await?
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let (kind, msg, retry) = classify_vnc_error(e);
                        return Err((kind, msg, retry));
                    }
                }
            }

            command = command_rx.recv() => {
                let Some(cmd) = command else {
                    let _ = client.close().await;
                    return Ok(());
                };
                match cmd {
                    VncWorkerCommand::Input(input) => {
                        pending_inputs.push(input);
                        if last_input_flush.elapsed() >= Duration::from_millis(8) {
                            for ev in pending_inputs.drain(..) {
                                send_vnc_input(&client, ev, &mut pressed_keys).await?;
                            }
                            last_input_flush = tokio::time::Instant::now();
                        }
                    }
                    VncWorkerCommand::Clipboard(text) => {
                        let _ = client.input(X11Event::CopyText(text)).await;
                    }
                    VncWorkerCommand::FullRefresh => {
                        pending_full_request = true;
                    }
                }
            }

            _ = ticker.tick() => {
                // 定期发 FramebufferUpdateRequest
                let (has_fb, fb_w, fb_h) = {
                    let fb = session.framebuffer.lock().await;
                    match fb.as_ref() {
                        Some(f) => (true, f.width, f.height),
                        None => (false, 0, 0),
                    }
                };
                if has_fb
                    && (pending_full_request || last_request.elapsed() >= UPDATE_REQUEST_INTERVAL)
                {
                    let incremental = !pending_full_request;
                    let msg: [u8; 10] = [
                        3,
                        if incremental { 1 } else { 0 },
                        0, 0,
                        0, 0,
                        (fb_w >> 8) as u8, (fb_w & 0xff) as u8,
                        (fb_h >> 8) as u8, (fb_h & 0xff) as u8,
                    ];
                    if req_writer.write_all(&msg).await.is_err() {
                        return Err((
                            VncErrorKind::Transport,
                            "发送 FramebufferUpdateRequest 失败".to_string(),
                            true,
                        ));
                    }
                    let _ = req_writer.flush().await;
                    pending_full_request = false;
                    last_request = tokio::time::Instant::now();
                }

                // 刷新输入队列
                if !pending_inputs.is_empty() {
                    for ev in pending_inputs.drain(..) {
                        send_vnc_input(&client, ev, &mut pressed_keys).await?;
                    }
                }

                // 发帧：脏矩形编码放到 spawn_blocking
                if dirty_rect.is_some() && last_emit.elapsed() >= FRAME_EMIT_INTERVAL {
                    let rect = dirty_rect.take().unwrap();
                    let fb_snapshot = {
                        let fb = session.framebuffer.lock().await;
                        fb.as_ref().map(|f| {
                            let stride = f.width as usize * 3;
                            let row_bytes = rect.2 as usize * 3;
                            let mut sub = Vec::with_capacity(row_bytes * rect.3 as usize);
                            for row in 0..rect.3 as usize {
                                let start = (rect.1 as usize + row) * stride + rect.0 as usize * 3;
                                sub.extend_from_slice(&f.rgb[start..start + row_bytes]);
                            }
                            (sub, rect.2, rect.3, f.width, f.height)
                        })
                    };
                    if let Some((sub, w, h, fb_w, fb_h)) = fb_snapshot {
                        let is_full = w >= u32::from(fb_w) && h >= u32::from(fb_h);
                        let quality = if is_full { 45 } else { 75 };
                        let jpeg = tokio::task::spawn_blocking(move || {
                            let mut jpeg = Vec::new();
                            let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, quality);
                            encoder
                                .encode(&sub, w, h, image::ExtendedColorType::Rgb8)
                                .ok()?;
                            Some(jpeg)
                        })
                        .await
                        .ok()
                        .flatten();
                        if let Some(jpeg) = jpeg {
                            let packet = pack_frame(
                                rect.0 as u16,
                                rect.1 as u16,
                                rect.2 as u16,
                                rect.3 as u16,
                                &jpeg,
                            );
                            queue_or_send_frame(session, packet).await;
                            last_emit = tokio::time::Instant::now();
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// 事件处理
// ============================================================================

async fn handle_vnc_event(
    app: &AppHandle,
    session: &Arc<VncSession>,
    generation: u64,
    event: VncEvent,
    dirty_rect: &mut Option<(u32, u32, u32, u32)>,
) -> Result<(), (VncErrorKind, String, bool)> {
    if session.generation.load(Ordering::Acquire) != generation {
        return Ok(());
    }
    match event {
        VncEvent::SetResolution(Screen { width, height }) => {
            let fb = VncFramebuffer::new(width, height)
                .map_err(|e| (VncErrorKind::Protocol, e.to_string(), false))?;
            *session.framebuffer.lock().await = Some(fb);
            *dirty_rect = Some((0, 0, width as u32, height as u32));
            let _ = app.emit("vnc:resolution", serde_json::json!({
                "sessionId": session.config.session_id,
                "width": width,
                "height": height,
            }));
        }

        VncEvent::SetPixelFormat(pf) => {
            eprintln!(
                "[VNC] 服务器像素格式: bpp={} depth={} big_endian={} true_color={} \
                 rmax={} gmax={} bmax={} rshift={} gshift={} bshift={}",
                pf.bits_per_pixel,
                pf.depth,
                pf.big_endian_flag,
                pf.true_color_flag,
                pf.red_max,
                pf.green_max,
                pf.blue_max,
                pf.red_shift,
                pf.green_shift,
                pf.blue_shift,
            );
            *session.pixel_format.lock().await = Some(pf);
        }

        VncEvent::RawImage(rect, data) => {
            let pf = session
                .pixel_format
                .lock()
                .await
                .clone()
                .unwrap_or_else(PixelFormat::rgba);

             // === 诊断日志 ===
            eprintln!(
                "[VNC] RawImage rect=({},{},{}x{}) data.len={} pf.bpp={} rshift={} gshift={} bshift={} big_endian={}",
                rect.x, rect.y, rect.width, rect.height,
                data.len(),
                pf.bits_per_pixel, pf.red_shift, pf.green_shift, pf.blue_shift, pf.big_endian_flag
            );
            // ================

            let mut fb_guard = session.framebuffer.lock().await;
            if fb_guard.is_none() {
                let w = rect.x.checked_add(rect.width).ok_or((
                    VncErrorKind::Protocol,
                    "VNC 矩形水平越界".to_string(),
                    false,
                ))?;
                let h = rect.y.checked_add(rect.height).ok_or((
                    VncErrorKind::Protocol,
                    "VNC 矩形垂直越界".to_string(),
                    false,
                ))?;
                *fb_guard = Some(
                    VncFramebuffer::new(w, h)
                        .map_err(|e| (VncErrorKind::Protocol, e.to_string(), false))?,
                );
            }
            let fb = fb_guard.as_mut().expect("framebuffer 已初始化");
            fb.blit_raw(rect, &data, &pf)
                .map_err(|e| (VncErrorKind::Protocol, e.to_string(), false))?;
            *dirty_rect = Some(union_rect(
                *dirty_rect,
                (
                    rect.x as u32,
                    rect.y as u32,
                    rect.width as u32,
                    rect.height as u32,
                ),
            ));
            drop(fb_guard);
            if !matches!(*session.state.lock().await, VncState::Active) {
                set_state(session, VncState::Active, None, None).await;
                emit_state(app, session, VncState::Active, None, None);
            }
        }

        VncEvent::Copy(dst, src) => {
            let mut fb_guard = session.framebuffer.lock().await;
            if let Some(fb) = fb_guard.as_mut() {
                fb.copy_rect(dst, src)
                    .map_err(|e| (VncErrorKind::Protocol, e.to_string(), false))?;
                *dirty_rect = Some(union_rect(
                    *dirty_rect,
                    (
                        dst.x as u32,
                        dst.y as u32,
                        dst.width as u32,
                        dst.height as u32,
                    ),
                ));
            }
        }

        VncEvent::JpegImage(rect, data) => {
            // Tight 编码的 JPEG 帧。直接转发给前端。
            // 这块区域在 framebuffer 里没更新，后续全屏会不一致，
            // 但只在服务器用 Tight 编码时才发生，且不会破坏其他区域。
            eprintln!(
                "[VNC] JpegImage rect=({},{},{}x{}) data.len={}",
                rect.x, rect.y, rect.width, rect.height, data.len()
            );
            let packet = pack_frame(
                rect.x as u16,
                rect.y as u16,
                rect.width as u16,
                rect.height as u16,
                &data,
            );
            queue_or_send_frame(session, packet).await;
        }

        VncEvent::Text(text) => {
            if session.config.clipboard_enabled && text.len() <= MAX_VNC_CLIPBOARD_BYTES {
                let _ = app.emit(
                    format!("vnc-clipboard-{}", session.config.session_id).as_str(),
                    VncClipboardEvent {
                        session_id: session.config.session_id.clone(),
                        text,
                    },
                );
            }
        }

        VncEvent::Error(msg) => {
            return Err((VncErrorKind::Protocol, msg, false));
        }

        VncEvent::SetCursor(_, _) => {}

        VncEvent::Bell => {}
        _ => {}
    }
    Ok(())
}

async fn send_vnc_input(
    client: &VncClient,
    input: VncInputEvent,
    pressed_keys: &mut Vec<u32>,
) -> Result<(), (VncErrorKind, String, bool)> {
    eprintln!("[VNC] send_vnc_input: {:?}", input);
    match input {
        VncInputEvent::Key { keysym, pressed } => {
            if pressed {
                if !pressed_keys.contains(&keysym) {
                    pressed_keys.push(keysym);
                }
            } else {
                pressed_keys.retain(|k| *k != keysym);
            }
            client
                .input(X11Event::KeyEvent(ClientKeyEvent {
                    keycode: keysym,
                    down: pressed,
                }))
                .await
                .map_err(classify_vnc_error)?;
        }
        VncInputEvent::Pointer {
            x,
            y,
            button_mask,
        } => {
            client
                .input(X11Event::PointerEvent(ClientMouseEvent {
                    position_x: x,
                    position_y: y,
                    bottons: button_mask,
                }))
                .await
                .map_err(classify_vnc_error)?;
        }
        VncInputEvent::ReleaseAllKeys => {
            release_pressed_keys(client, pressed_keys).await;
        }
    }
    Ok(())
}

async fn release_pressed_keys(client: &VncClient, pressed_keys: &mut Vec<u32>) {
    for keysym in pressed_keys.drain(..) {
        let _ = client
            .input(X11Event::KeyEvent(ClientKeyEvent {
                keycode: keysym,
                down: false,
            }))
            .await;
    }
}

// ============================================================================
// Server key 验证
// ============================================================================

async fn verify_vnc_server_key(
    app: &AppHandle,
    session: &Arc<VncSession>,
    generation: u64,
    pending_map: &Arc<Mutex<std::collections::HashMap<String, VncServerKeyPending>>>,
    key: VncServerKey,
) -> std::result::Result<(), VncError> {
    if session.generation.load(Ordering::Acquire) != generation
        || session.close_requested.load(Ordering::Acquire)
    {
        return Err(VncError::Ra2ServerKeyRejected(
            "stale VNC generation".to_string(),
        ));
    }

    let fingerprint = vnc_key_fingerprint(&key);
    let known_status = crate::storage::check_vnc_known_host(
        &session.config.host,
        session.config.port,
        &fingerprint,
    )
    .map_err(VncError::Ra2ServerKeyRejected)?;

    use crate::storage::KnownHostCheck;
    if known_status == KnownHostCheck::Match {
        return Ok(());
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    let payload = VncServerKeyVerifyEvent {
        request_id: request_id.clone(),
        session_id: session.config.session_id.clone(),
        host: session.config.host.clone(),
        port: session.config.port,
        fingerprint: fingerprint.clone(),
        key_bits: key.bits(),
        known_host_status: match known_status {
            KnownHostCheck::Match => "match",
            KnownHostCheck::HostSeen => "changed",
            KnownHostCheck::UnknownHost => "unknown",
        }
        .to_string(),
    };
    pending_map.lock().await.insert(
        request_id.clone(),
        VncServerKeyPending {
            payload: payload.clone(),
            session_id: session.config.session_id.clone(),
            generation,
            responder: tx,
        },
    );
    let _ = app.emit("vnc-server-key-verify", payload);

    let response = tokio::time::timeout(SERVER_KEY_PROMPT_TIMEOUT, rx).await;
    pending_map.lock().await.remove(&request_id);
    let _ = app.emit(
        "vnc-server-key-verify-resolved",
        serde_json::json!({ "requestId": request_id }),
    );

    let Ok(Ok(accepted)) = response else {
        return Err(VncError::Ra2ServerKeyRejected("验证超时".to_string()));
    };
    if !accepted {
        return Err(VncError::Ra2ServerKeyRejected("用户拒绝".to_string()));
    }
    let _ = crate::storage::upsert_vnc_known_host(
        &session.config.host,
        session.config.port,
        &fingerprint,
    );
    Ok(())
}

fn vnc_key_fingerprint(key: &VncServerKey) -> String {
    format!("SHA256:{}", hex::encode(Sha256::digest(key.encoded())))
}

// ============================================================================
// 帧发送
// ============================================================================

async fn queue_or_send_frame(session: &Arc<VncSession>, frame: Vec<u8>) {
    let attach_id = session.frame_attach_id.load(Ordering::Acquire);
    let sent = {
        let mut channel = session.frame_channel.lock().await;
        match channel.as_ref() {
            Some((id, ch)) if *id == attach_id => {
                if ch.send(InvokeResponseBody::Raw(frame.clone())).is_ok() {
                    true
                } else {
                    if channel.as_ref().is_some_and(|(i, _)| *i == attach_id) {
                        *channel = None;
                    }
                    false
                }
            }
            _ => false,
        }
    };
    if sent {
        return;
    }
    let mut pending = session.pending_frames.lock().await;
    while pending.len() >= MAX_PENDING_FRAMES {
        pending.pop_front();
    }
    pending.push_back(frame);
}

async fn flush_pending_frames(session: &Arc<VncSession>, attach_id: u64) {
    let channel = session.frame_channel.lock().await;
    let Some((id, ch)) = channel.as_ref() else {
        return;
    };
    if *id != attach_id {
        return;
    }
    let mut pending = session.pending_frames.lock().await;
    while let Some(frame) = pending.pop_front() {
        if ch.send(InvokeResponseBody::Raw(frame)).is_err() {
            break;
        }
    }
}

async fn send_full_frame(session: &Arc<VncSession>, attach_id: u64) -> Result<()> {
    let fb_guard = session.framebuffer.lock().await;
    let Some(fb) = fb_guard.as_ref() else {
        return Ok(());
    };
    let w = fb.width;
    let h = fb.height;
    let rgb = fb.rgb.clone();
    drop(fb_guard);

    let jpeg = tokio::task::spawn_blocking(move || {
        let mut jpeg = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, 45);
        encoder
            .encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8)
            .ok()?;
        Some(jpeg)
    })
    .await
    .ok()
    .flatten();

    let Some(jpeg) = jpeg else {
        return Ok(());
    };
    let packet = pack_frame(0, 0, w, h, &jpeg);
    let mut channel = session.frame_channel.lock().await;
    if let Some((id, ch)) = channel.as_ref() {
        if *id == attach_id {
            let _ = ch.send(InvokeResponseBody::Raw(packet));
        }
    }
    Ok(())
}

// ============================================================================
// 状态
// ============================================================================

async fn set_state(
    session: &VncSession,
    state: VncState,
    message: Option<String>,
    error_kind: Option<VncErrorKind>,
) {
    *session.state.lock().await = state;
    *session.message.lock().await = message;
    *session.error_kind.lock().await = error_kind;
}

fn emit_state(
    app: &AppHandle,
    session: &VncSession,
    state: VncState,
    message: Option<String>,
    error_kind: Option<VncErrorKind>,
) {
    let _ = app.emit(
        format!("vnc-state-{}", session.config.session_id).as_str(),
        VncStateEvent {
            session_id: session.config.session_id.clone(),
            state,
            message,
            error_kind,
        },
    );
}

async fn send_worker_command(session: &Arc<VncSession>, cmd: VncWorkerCommand) -> Result<()> {
    let sender = session
        .command_sender
        .lock()
        .await
        .clone()
        .ok_or_else(|| anyhow::anyhow!("VNC worker 未运行"))?;
    sender
        .send(cmd)
        .await
        .map_err(|_| anyhow::anyhow!("VNC worker 通道关闭"))
}

async fn stop_worker(session: &Arc<VncSession>) {
    if let Some(cancel) = session.cancel_sender.lock().await.take() {
        let _ = cancel.send(true);
    }
    *session.command_sender.lock().await = None;
    if let Some(mut worker) = session.worker.lock().await.take() {
        if tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, &mut worker)
            .await
            .is_err()
        {
            worker.abort();
            let _ = worker.await;
        }
    }
}

// ============================================================================
// 错误分类
// ============================================================================

fn security_policy(mode: &str) -> VncSecurityPolicy {
    match mode {
        "none" => VncSecurityPolicy::NoneOnly,
        "vnc-auth" => VncSecurityPolicy::VncAuthOnly,
        _ => VncSecurityPolicy::Auto,
    }
}

fn classify_vnc_error(error: VncError) -> (VncErrorKind, String, bool) {
    match error {
        VncError::NoPassword | VncError::WrongPassword => (
            VncErrorKind::Authentication,
            "VNC 认证失败".to_string(),
            false,
        ),
        VncError::CredentialTooLong { .. } | VncError::Ra2ServerKeyRejected(_) => {
            (VncErrorKind::Authentication, error.to_string(), false)
        }
        VncError::UnsupportedSecurityType
        | VncError::RequiredSecurityTypeUnavailable(_)
        | VncError::InvalidSecurityType(_) => (
            VncErrorKind::Authentication,
            format!("VNC 服务器要求不支持的安全类型: {error}"),
            false,
        ),
        VncError::InvalidEncoding(_) | VncError::InvalidImageData => {
            (VncErrorKind::Encoding, error.to_string(), false)
        }
        VncError::IoError(ref source)
            if source.kind() == std::io::ErrorKind::InvalidData =>
        {
            (VncErrorKind::Protocol, error.to_string(), false)
        }
        VncError::IoError(_) => (VncErrorKind::Transport, error.to_string(), true),
        VncError::LimitExceeded { .. }
        | VncError::InvalidDimensions
        | VncError::IntegerOverflow(_)
        | VncError::WrongPixelFormat
        | VncError::WrongServerMessage
        | VncError::InvalidSecurityResult(_)
        | VncError::SecurityFailure(_)
        | VncError::InvalidRa2KeyLength { .. }
        | VncError::InvalidRa2PublicKey
        | VncError::InvalidRa2EncryptedRandomLength { .. }
        | VncError::InvalidRa2RandomLength(_)
        | VncError::InvalidRa2Subtype(_)
        | VncError::Ra2ServerKeyVerifierRequired
        | VncError::InvalidRa2RecordLimit(_)
        | VncError::Ra2Crypto(_)
        | VncError::ConnectError
        | VncError::NoEncoding
        | VncError::ClientNotRunning => (VncErrorKind::Protocol, error.to_string(), false),
        VncError::General(_) => (VncErrorKind::Internal, error.to_string(), true),
        _ => (VncErrorKind::Internal, error.to_string(), true),
    }
}

fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs(match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        5 => 15,
        _ => 30,
    })
}

// ============================================================================
// 公共 API
// ============================================================================

pub fn load_saved_vnc_config(host: String, port: u16, password: String) -> VncConnectConfig {
    VncConnectConfig {
        session_id: uuid::Uuid::new_v4().to_string(),
        host,
        port,
        username: String::new(),
        password: Some(password),
        security_mode: "auto".to_string(),
        scale_mode: "fit".to_string(),
        clipboard_enabled: true,
        reconnect_enabled: true,
        reconnect_max_attempts: 5,
        shared: true,
        view_only: false,
    }
}