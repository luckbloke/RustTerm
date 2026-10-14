//! 纯 Rust 的 SPICE 客户端实现，基于 capsaicin。
//!
//! 对齐 vnc.rs 的设计：
//!   - 状态机（Connecting / Authenticating / Active / Reconnecting / Failed）
//!   - 二进制帧通道（Channel<InvokeResponseBody>），脏矩形 JPEG 增量
//!   - 输入批处理 + 节流
//!   - 帧节流 + 背压（VecDeque 丢旧帧）
//!   - spawn_blocking 编码，避免阻塞 runtime
//!   - 自动重连（指数退避）
//!   - 多 surface + 光标 + Mark 一致性提交

#![allow(dead_code)]

use anyhow::Result;
use capsaicin_client::{
    ClientEvent, CursorEvent, DisplayEvent, InputEvent, MouseMode, RegionPixels, SpiceClient,
    SurfaceFormat,
};
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, Mutex};
use tokio::time::Instant;

// ============================================================================
// 常量
// ============================================================================

const MAX_FRAMEBUFFER_WIDTH: u32 = 7680;
const MAX_FRAMEBUFFER_HEIGHT: u32 = 4320;
const MAX_INPUT_BATCH: usize = 256;
const MAX_PENDING_FRAMES: usize = 2;
const WORKER_COMMAND_CHANNEL_CAPACITY: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(8);
const FRAME_EMIT_INTERVAL: Duration = Duration::from_millis(16);
const INPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(8);
const MARK_COMMIT_TIMEOUT: Duration = Duration::from_millis(32);

// ============================================================================
// 状态与错误
// ============================================================================

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpiceState {
    Connecting,
    Authenticating,
    Active,
    Reconnecting,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpiceErrorKind {
    Transport,
    Authentication,
    Protocol,
    Internal,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpiceStateEvent {
    pub session_id: String,
    pub state: SpiceState,
    pub message: Option<String>,
    pub error_kind: Option<SpiceErrorKind>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpiceMouseModeEvent {
    pub session_id: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpiceCursorEvent {
    pub session_id: String,
    /// "set" | "from-cache" | "move" | "hide" | "invalidate-one" | "invalidate-all"
    pub kind: String,
    pub x: i32,
    pub y: i32,
    pub hot_x: u16,
    pub hot_y: u16,
    pub width: u16,
    pub height: u16,
    /// ARGB8888，base64 编码（仅 kind=set 时有值）
    pub pixels_base64: Option<String>,
    pub unique: u64,
    pub visible: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpiceSurfaceEvent {
    pub session_id: String,
    pub surface_id: u32,
    pub primary: bool,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpiceMonitorsEvent {
    pub session_id: String,
    pub max_allowed: u16,
    pub heads: Vec<SpiceHead>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpiceHead {
    pub id: u32,
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub flags: u32,
}

// ============================================================================
// 输入事件（与 capsaicin 的 InputEvent 对齐，serde tag 供前端用）
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum SpiceInputEvent {
    #[serde(rename = "key")]
    Key { scancode: u32, pressed: bool },

    #[serde(rename = "pointer-abs")]
    PointerAbs { x: u32, y: u32, buttons: u32, display: Option<u8> },

    #[serde(rename = "pointer-rel")]
    PointerRel { dx: i32, dy: i32, buttons: u32 },

    #[serde(rename = "mouse-press")]
    MousePress { button: u8, buttons: u32 },

    #[serde(rename = "mouse-release")]
    MouseRelease { button: u8, buttons: u32 },
}

// ============================================================================
// 连接配置
// ============================================================================

#[derive(Debug, Clone)]
pub struct SpiceConnectConfig {
    pub session_id: String,
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    pub view_only: bool,
    pub reconnect_enabled: bool,
    pub reconnect_max_attempts: u32,
    /// 是否在前端绘制 SPICE 光标精灵。false 时前端用 CSS cursor。
    pub cursor_enabled: bool,
}

// ============================================================================
// 帧打包（与 vnc.rs 一致）
// ============================================================================

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
// 帧缓冲（BGRA 存，因为 capsaicin 所有解码输出都是 BGRA）
// ============================================================================

struct SpiceFramebuffer {
    width: u32,
    height: u32,
    /// BGRA，每像素 4 字节，top-down
    bgra: Vec<u8>,
}

impl SpiceFramebuffer {
    fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0
            || height == 0
            || width > MAX_FRAMEBUFFER_WIDTH
            || height > MAX_FRAMEBUFFER_HEIGHT
        {
            anyhow::bail!("SPICE framebuffer {width}x{height} 超出范围");
        }
        let len = width as usize * height as usize * 4;
        Ok(Self {
            width,
            height,
            bgra: vec![0u8; len],
        })
    }

    fn blit_raw(
        &mut self,
        rect: &capsaicin_client::Rect,
        data: &[u8],
        stride: u32,
        format: SurfaceFormat,
    ) -> Result<()> {
        let rx = rect.left as i64;
        let ry = rect.top as i64;
        let rw = rect.width() as i64;
        let rh = rect.height() as i64;
        if rw <= 0 || rh <= 0 {
            return Ok(());
        }
        if rx < 0 || ry < 0 || rx + rw > self.width as i64 || ry + rh > self.height as i64 {
            anyhow::bail!("SPICE 矩形越界");
        }
        let bpp = format.bytes_per_pixel().unwrap_or(4) as usize;
        if bpp == 0 {
            return Ok(());
        }
        let stride = stride as usize;
        let need = (stride * (rh as usize - 1)) + (rw as usize * bpp);
        if data.len() < need {
            anyhow::bail!(
                "SPICE Raw 数据长度 {} < 期望 {} (stride={}, rw={}, rh={}, bpp={})",
                data.len(),
                need,
                stride,
                rw,
                rh,
                bpp
            );
        }

        let dst_stride = self.width as usize * 4;
        for row in 0..rh as usize {
            let src_start = row * stride;
            let dst_start = (ry as usize + row) * dst_stride + rx as usize * 4;
            // 把 src 像素统一转成 BGRA 写入 dst
            match (bpp, format) {
                (4, SurfaceFormat::Xrgb8888) | (4, SurfaceFormat::Argb8888) => {
                    // 已经是 BGRA / BGRX，直接拷
                    let bytes = rw as usize * 4;
                    self.bgra[dst_start..dst_start + bytes]
                        .copy_from_slice(&data[src_start..src_start + bytes]);
                }
                (2, SurfaceFormat::Rgb565) | (2, SurfaceFormat::Rgb555) => {
                    for col in 0..rw as usize {
                        let s = src_start + col * 2;
                        let v = u16::from_le_bytes([data[s], data[s + 1]]);
                        let (r, g, b) = if matches!(format, SurfaceFormat::Rgb565) {
                            let r = ((v >> 11) & 0x1f) as u8;
                            let g = ((v >> 5) & 0x3f) as u8;
                            let b = (v & 0x1f) as u8;
                            ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
                        } else {
                            let r = ((v >> 10) & 0x1f) as u8;
                            let g = ((v >> 5) & 0x1f) as u8;
                            let b = (v & 0x1f) as u8;
                            ((r << 3) | (r >> 2), (g << 3) | (g >> 2), (b << 3) | (b >> 2))
                        };
                        let d = dst_start + col * 4;
                        self.bgra[d] = b;
                        self.bgra[d + 1] = g;
                        self.bgra[d + 2] = r;
                        self.bgra[d + 3] = 0xff;
                    }
                }
                (1, SurfaceFormat::A8) => {
                    for col in 0..rw as usize {
                        let a = data[src_start + col];
                        let d = dst_start + col * 4;
                        self.bgra[d] = 0;
                        self.bgra[d + 1] = 0;
                        self.bgra[d + 2] = 0;
                        self.bgra[d + 3] = a;
                    }
                }
                _ => {
                    // 未知格式：直接 4 字节拷（尽力而为）
                    let bytes = rw as usize * 4;
                    if src_start + bytes <= data.len() {
                        self.bgra[dst_start..dst_start + bytes]
                            .copy_from_slice(&data[src_start..src_start + bytes]);
                    }
                }
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, rect: &capsaicin_client::Rect, color: u32) -> Result<()> {
        let px = color.to_le_bytes();
        let rx = rect.left as i64;
        let ry = rect.top as i64;
        let rw = rect.width() as i64;
        let rh = rect.height() as i64;
        if rw <= 0 || rh <= 0 {
            return Ok(());
        }
        if rx < 0 || ry < 0 || rx + rw > self.width as i64 || ry + rh > self.height as i64 {
            anyhow::bail!("SPICE SolidColor 矩形越界");
        }
        let stride = self.width as usize * 4;
        for row in 0..rh as usize {
            let dst_start = (ry as usize + row) * stride + rx as usize * 4;
            for col in 0..rw as usize {
                let i = dst_start + col * 4;
                self.bgra[i..i + 4].copy_from_slice(&px);
            }
        }
        Ok(())
    }

    fn copy_rect(
        &mut self,
        src_x: i32,
        src_y: i32,
        dest_rect: &capsaicin_client::Rect,
    ) -> Result<()> {
        let dx = dest_rect.left as i64;
        let dy = dest_rect.top as i64;
        let dw = dest_rect.width() as i64;
        let dh = dest_rect.height() as i64;
        if dw <= 0 || dh <= 0 {
            return Ok(());
        }
        if dx < 0 || dy < 0 || dx + dw > self.width as i64 || dy + dh > self.height as i64 {
            anyhow::bail!("SPICE CopyRect 目标越界");
        }
        let stride = self.width as usize * 4;
        let mut tmp = vec![0u8; dw as usize * dh as usize * 4];
        for row in 0..dh {
            let sy = src_y as i64 + row;
            if sy < 0 || sy >= self.height as i64 {
                continue;
            }
            for col in 0..dw {
                let sx = src_x as i64 + col;
                if sx < 0 || sx >= self.width as i64 {
                    continue;
                }
                let si = (sy as usize * stride) + sx as usize * 4;
                let di = (row as usize * dw as usize + col as usize) * 4;
                tmp[di..di + 4].copy_from_slice(&self.bgra[si..si + 4]);
            }
        }
        for row in 0..dh as usize {
            let dst_start = (dy as usize + row) * stride + dx as usize * 4;
            let row_bytes = dw as usize * 4;
            let src_start = row * row_bytes;
            self.bgra[dst_start..dst_start + row_bytes]
                .copy_from_slice(&tmp[src_start..src_start + row_bytes]);
        }
        Ok(())
    }

    /// 提取脏矩形区域的 BGRA → RGB（JPEG 需要）
    fn extract_rgb(&self, rect: (u32, u32, u32, u32)) -> (Vec<u8>, u32, u32) {
        let (x, y, w, h) = rect;
        let stride = self.width as usize * 4;
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for row in 0..h as usize {
            let start = (y as usize + row) * stride + x as usize * 4;
            let row_slice = &self.bgra[start..start + w as usize * 4];
            for px in row_slice.chunks_exact(4) {
                rgb.push(px[2]);
                rgb.push(px[1]);
                rgb.push(px[0]);
            }
        }
        (rgb, w, h)
    }
}

// ============================================================================
// 多 surface 状态
// ============================================================================

struct SurfaceState {
    fb: SpiceFramebuffer,
    primary: bool,
    format: SurfaceFormat,
}

// ============================================================================
// 会话
// ============================================================================

enum SpiceWorkerCommand {
    Input(SpiceInputEvent),
    FullRefresh,
}

pub struct SpiceSession {
    pub config: SpiceConnectConfig,
    state: Mutex<SpiceState>,
    message: Mutex<Option<String>>,
    error_kind: Mutex<Option<SpiceErrorKind>>,
    generation: AtomicU64,
    frame_attach_id: AtomicU64,
    frame_channel: Mutex<Option<(u64, Channel<InvokeResponseBody>)>>,
    pending_frames: Mutex<VecDeque<Vec<u8>>>,
    /// 当前活跃 surface 的快照，供 attach 时发全帧
    active_fb: Mutex<Option<SpiceFramebuffer>>,
    command_sender: Mutex<Option<mpsc::Sender<SpiceWorkerCommand>>>,
    cancel: AtomicBool,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    close_requested: AtomicBool,
}

pub struct SpiceSessionManager {
    sessions: Mutex<std::collections::HashMap<String, Arc<SpiceSession>>>,
}

impl SpiceSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub async fn create_session(
        self: &Arc<Self>,
        app: AppHandle,
        config: SpiceConnectConfig,
    ) -> Result<String> {
        let session_id = config.session_id.clone();
        let session = Arc::new(SpiceSession {
            config,
            state: Mutex::new(SpiceState::Connecting),
            message: Mutex::new(None),
            error_kind: Mutex::new(None),
            generation: AtomicU64::new(0),
            frame_attach_id: AtomicU64::new(0),
            frame_channel: Mutex::new(None),
            pending_frames: Mutex::new(VecDeque::with_capacity(MAX_PENDING_FRAMES)),
            active_fb: Mutex::new(None),
            command_sender: Mutex::new(None),
            cancel: AtomicBool::new(false),
            worker: Mutex::new(None),
            close_requested: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), session.clone());
        emit_state(&app, &session, SpiceState::Connecting, None, None);
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
        if session.active_fb.lock().await.is_some() {
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

    pub async fn send_input(
        &self,
        session_id: &str,
        events: Vec<SpiceInputEvent>,
    ) -> Result<()> {
        let session = self.get(session_id).await?;
        if session.config.view_only {
            anyhow::bail!("SPICE 只读模式禁止输入");
        }
        if events.len() > MAX_INPUT_BATCH {
            anyhow::bail!("SPICE 输入批次超过 {MAX_INPUT_BATCH}");
        }
        let sender = session
            .command_sender
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("SPICE worker 未运行"))?;
        for ev in events {
            sender
                .send(SpiceWorkerCommand::Input(ev))
                .await
                .map_err(|_| anyhow::anyhow!("SPICE worker 通道关闭"))?;
        }
        Ok(())
    }

    pub async fn close(&self, app: &AppHandle, session_id: &str) -> Result<()> {
        let Some(session) = self.sessions.lock().await.remove(session_id) else {
            return Ok(());
        };
        session.close_requested.store(true, Ordering::Release);
        session.cancel.store(true, Ordering::Release);
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
        session.frame_attach_id.fetch_add(1, Ordering::AcqRel);
        *session.frame_channel.lock().await = None;
        session.pending_frames.lock().await.clear();
        *session.active_fb.lock().await = None;
        set_state(&session, SpiceState::Disconnected, None, None).await;
        emit_state(app, &session, SpiceState::Disconnected, None, None);
        let _ = app.emit("spice:closed", serde_json::json!({ "sessionId": session_id }));
        Ok(())
    }

    async fn get(&self, session_id: &str) -> Result<Arc<SpiceSession>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("SPICE 会话 {session_id} 不存在"))
    }
}

impl Default for SpiceSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 会话 worker
// ============================================================================

async fn spawn_session_worker(app: AppHandle, session: Arc<SpiceSession>) {
    let session_for_task = session.clone();
    let handle = tokio::spawn(async move {
        run_session_worker(app, session_for_task).await;
    });
    *session.worker.lock().await = Some(handle);
}

async fn run_session_worker(app: AppHandle, session: Arc<SpiceSession>) {
    let mut attempt = 0u32;
    loop {
        if session.close_requested.load(Ordering::Acquire) || session.cancel.load(Ordering::Acquire)
        {
            return;
        }
        let generation = session.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let state = if attempt == 0 {
            SpiceState::Connecting
        } else {
            SpiceState::Reconnecting
        };
        set_state(&session, state.clone(), None, None).await;
        emit_state(&app, &session, state, None, None);

        match run_protocol_generation(&app, &session, generation).await {
            Ok(()) => return,
            Err((kind, message, retryable)) => {
                eprintln!(
                    "[SPICE] gen={generation} worker error kind={kind:?} retryable={retryable} msg={message}"
                );
                if session.close_requested.load(Ordering::Acquire)
                    || session.cancel.load(Ordering::Acquire)
                {
                    return;
                }
                if !retryable
                    || !session.config.reconnect_enabled
                    || attempt >= session.config.reconnect_max_attempts
                {
                    set_state(
                        &session,
                        SpiceState::Failed,
                        Some(message.clone()),
                        Some(kind),
                    )
                    .await;
                    emit_state(
                        &app,
                        &session,
                        SpiceState::Failed,
                        Some(message),
                        Some(kind),
                    );
                    return;
                }
                attempt += 1;
                let delay = reconnect_delay(attempt);
                let deadline = Instant::now() + delay;
                while Instant::now() < deadline {
                    if session.close_requested.load(Ordering::Acquire)
                        || session.cancel.load(Ordering::Acquire)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }
}

async fn run_protocol_generation(
    app: &AppHandle,
    session: &Arc<SpiceSession>,
    generation: u64,
) -> Result<(), (SpiceErrorKind, String, bool)> {
    // ---------- 1. 连接 ----------
    let addr = format!("{}:{}", session.config.host, session.config.port);
    let password = session.config.password.clone().unwrap_or_default();
    eprintln!("[SPICE] gen={generation} connecting to {addr} (pwd_len={})", password.len());

    set_state(session, SpiceState::Authenticating, None, None).await;
    emit_state(app, session, SpiceState::Authenticating, None, None);

    let mut client = tokio::time::timeout(
        CONNECT_TIMEOUT,
        SpiceClient::connect(&addr, &password),
    )
    .await
    .map_err(|_| {
        eprintln!("[SPICE] gen={generation} connect TIMEOUT after {:?}", CONNECT_TIMEOUT);
        (
            SpiceErrorKind::Transport,
            "SPICE 连接超时".to_string(),
            true,
        )
    })?
    .map_err(|e| {
        eprintln!("[SPICE] gen={generation} connect FAILED: {e}");
        (
            SpiceErrorKind::Transport,
            format!("SPICE 连接失败 {addr}: {e}"),
            true,
        )
    })?;
    eprintln!("[SPICE] gen={generation} connected OK");

    // ---------- 2. 事件循环 ----------
    let (command_tx, mut command_rx) =
        mpsc::channel::<SpiceWorkerCommand>(WORKER_COMMAND_CHANNEL_CAPACITY);
    *session.command_sender.lock().await = Some(command_tx);
    eprintln!("[SPICE] gen={generation} command_sender ready; state -> Active");

    set_state(session, SpiceState::Active, None, None).await;
    emit_state(app, session, SpiceState::Active, None, None);

    // 多 surface
    let mut surfaces: std::collections::HashMap<u32, SurfaceState> =
        std::collections::HashMap::new();
    let mut active_surface_id: Option<u32> = None;
    let mut dirty_rect: Option<(u32, u32, u32, u32)> = None;
    let mut pending_mark = false;
    let mut mark_deadline: Option<Instant> = None;

    // 输入
    let mut pending_inputs: Vec<SpiceInputEvent> = Vec::new();
    let mut last_input_flush = Instant::now();

    // 帧节流
    let mut last_emit = Instant::now() - FRAME_EMIT_INTERVAL;

    let mut ticker = tokio::time::interval(EVENT_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if session.close_requested.load(Ordering::Acquire)
            || session.cancel.load(Ordering::Acquire)
            || session.generation.load(Ordering::Acquire) != generation
        {
            return Ok(());
        }

        tokio::select! {
            event = client.next_event() => {
                let Some(event) = event else {
                    return Err((SpiceErrorKind::Transport, "SPICE 事件流关闭".to_string(), true));
                };
                match event {
                    ClientEvent::Display(DisplayEvent::Mode { width, height, .. }) => {
                        // 旧式 hint，如果还没有 surface 就临时建一个
                        if surfaces.is_empty() && width > 0 && height > 0 {
                            if let Ok(fb) = SpiceFramebuffer::new(width, height) {
                                surfaces.insert(0, SurfaceState {
                                    fb,
                                    primary: true,
                                    format: SurfaceFormat::Xrgb8888,
                                });
                                if active_surface_id.is_none() {
                                    active_surface_id = Some(0);
                                    dirty_rect = Some((0, 0, width, height));
                                    let _ = app.emit("spice:resolution", serde_json::json!({
                                        "sessionId": session.config.session_id,
                                        "surfaceId": 0,
                                        "width": width,
                                        "height": height,
                                    }));
                                }
                            }
                        }
                    }

                    ClientEvent::Display(DisplayEvent::SurfaceCreated { id, width, height, format, primary }) => {
                        match SpiceFramebuffer::new(width, height) {
                            Ok(fb) => {
                                surfaces.insert(id, SurfaceState { fb, primary, format });
                                let should_switch = match active_surface_id {
                                    None => true,
                                    Some(cur) => {
                                        primary && !surfaces.get(&cur).map(|s| s.primary).unwrap_or(false)
                                    }
                                };
                                if should_switch {
                                    active_surface_id = Some(id);
                                    dirty_rect = Some((0, 0, width, height));
                                    // 同步 active_fb
                                    if let Some(s) = surfaces.get(&id) {
                                        *session.active_fb.lock().await = Some(
                                            SpiceFramebuffer::new(s.fb.width, s.fb.height)
                                                .map(|mut nf| { nf.bgra.copy_from_slice(&s.fb.bgra); nf })
                                                .unwrap_or_else(|_| SpiceFramebuffer { width: s.fb.width, height: s.fb.height, bgra: s.fb.bgra.clone() }),
                                        );
                                    }
                                    let _ = app.emit("spice:resolution", serde_json::json!({
                                        "sessionId": session.config.session_id,
                                        "surfaceId": id,
                                        "width": width,
                                        "height": height,
                                    }));
                                    let _ = app.emit("spice:active-surface", SpiceSurfaceEvent {
                                        session_id: session.config.session_id.clone(),
                                        surface_id: id,
                                        primary,
                                        width,
                                        height,
                                    });
                                    session.pending_frames.lock().await.clear();
                                }
                            }
                            Err(e) => {
                                return Err((SpiceErrorKind::Protocol, e.to_string(), false));
                            }
                        }
                    }

                    ClientEvent::Display(DisplayEvent::SurfaceDestroyed { id }) => {
                        surfaces.remove(&id);
                        if active_surface_id == Some(id) {
                            let next = surfaces.iter()
                                .find(|(_, s)| s.primary)
                                .or_else(|| surfaces.iter().next())
                                .map(|(k, v)| (*k, v.fb.width, v.fb.height, v.primary));
                            if let Some((nid, w, h, primary)) = next {
                                active_surface_id = Some(nid);
                                dirty_rect = Some((0, 0, w, h));
                                if let Some(s) = surfaces.get(&nid) {
                                    *session.active_fb.lock().await = Some(
                                        SpiceFramebuffer { width: s.fb.width, height: s.fb.height, bgra: s.fb.bgra.clone() }
                                    );
                                }
                                let _ = app.emit("spice:resolution", serde_json::json!({
                                    "sessionId": session.config.session_id,
                                    "surfaceId": nid,
                                    "width": w,
                                    "height": h,
                                }));
                                let _ = app.emit("spice:active-surface", SpiceSurfaceEvent {
                                    session_id: session.config.session_id.clone(),
                                    surface_id: nid,
                                    primary,
                                    width: w,
                                    height: h,
                                });
                            } else {
                                active_surface_id = None;
                            }
                        }
                    }

                    ClientEvent::Display(DisplayEvent::MonitorsConfig { max_allowed, heads }) => {
                        let payload = SpiceMonitorsEvent {
                            session_id: session.config.session_id.clone(),
                            max_allowed,
                            heads: heads.iter().map(|h| SpiceHead {
                                id: h.id,
                                surface_id: h.surface_id,
                                width: h.width,
                                height: h.height,
                                x: h.x,
                                y: h.y,
                                flags: h.flags,
                            }).collect(),
                        };
                        let _ = app.emit("spice-monitors", payload);
                    }

                    ClientEvent::Display(DisplayEvent::Mark) => {
                        pending_mark = true;
                        mark_deadline = Some(Instant::now() + MARK_COMMIT_TIMEOUT);
                    }

                    ClientEvent::Display(DisplayEvent::Reset) => {
                        for s in surfaces.values_mut() {
                            s.fb.bgra.fill(0);
                        }
                        if let Some(id) = active_surface_id {
                            if let Some(s) = surfaces.get(&id) {
                                dirty_rect = Some((0, 0, s.fb.width, s.fb.height));
                            }
                        }
                    }

                    ClientEvent::Display(DisplayEvent::Region { surface_id, rect, pixels, surface_format }) => {
                        let Some(surface) = surfaces.get_mut(&surface_id) else { continue };
                        let res = match pixels {
                            RegionPixels::Raw { data, stride } => {
                                surface.fb.blit_raw(&rect, &data, stride, surface_format)
                            }
                            RegionPixels::SolidColor(color) => surface.fb.fill_solid(&rect, color),
                        };
                        if let Err(e) = res {
                            eprintln!("[SPICE] Region 处理失败: {e}");
                            continue;
                        }
                        if active_surface_id == Some(surface_id) {
                            let r = (
                                rect.left.max(0) as u32,
                                rect.top.max(0) as u32,
                                rect.width() as u32,
                                rect.height() as u32,
                            );
                            dirty_rect = Some(union_rect(dirty_rect, r));
                            // 更新 active_fb
                            let mut guard = session.active_fb.lock().await;
                            if let Some(afb) = guard.as_mut() {
                                if afb.width == surface.fb.width && afb.height == surface.fb.height {
                                    afb.bgra.copy_from_slice(&surface.fb.bgra);
                                } else {
                                    *guard = Some(SpiceFramebuffer {
                                        width: surface.fb.width,
                                        height: surface.fb.height,
                                        bgra: surface.fb.bgra.clone(),
                                    });
                                }
                            } else {
                                *guard = Some(SpiceFramebuffer {
                                    width: surface.fb.width,
                                    height: surface.fb.height,
                                    bgra: surface.fb.bgra.clone(),
                                });
                            }
                        }
                    }

                    ClientEvent::Display(DisplayEvent::CopyRect { surface_id, src_x, src_y, dest_rect }) => {
                        let Some(surface) = surfaces.get_mut(&surface_id) else { continue };
                        if let Err(e) = surface.fb.copy_rect(src_x, src_y, &dest_rect) {
                            eprintln!("[SPICE] CopyRect 处理失败: {e}");
                            continue;
                        }
                        if active_surface_id == Some(surface_id) {
                            let r = (
                                dest_rect.left.max(0) as u32,
                                dest_rect.top.max(0) as u32,
                                dest_rect.width() as u32,
                                dest_rect.height() as u32,
                            );
                            dirty_rect = Some(union_rect(dirty_rect, r));
                        }
                    }

                    ClientEvent::Display(DisplayEvent::StreamFrame { dest_rect, pixels, .. }) => {
                        // 直接把 MJPEG 解码后的 BGRA 画到活跃 surface
                        if let Some(id) = active_surface_id {
                            if let Some(surface) = surfaces.get_mut(&id) {
                                if let RegionPixels::Raw { data, stride } = pixels {
                                    let _ = surface.fb.blit_raw(
                                        &dest_rect, &data, stride, SurfaceFormat::Argb8888
                                    );
                                    let r = (
                                        dest_rect.left.max(0) as u32,
                                        dest_rect.top.max(0) as u32,
                                        dest_rect.width() as u32,
                                        dest_rect.height() as u32,
                                    );
                                    dirty_rect = Some(union_rect(dirty_rect, r));
                                }
                            }
                        }
                    }

                    ClientEvent::Display(DisplayEvent::StreamCreated { stream_id, codec, dest, src_width, src_height, .. }) => {
                        eprintln!(
                            "[SPICE] StreamCreated id={stream_id} codec={codec:?} dest=({},{},{}x{}) src={src_width}x{src_height}",
                            dest.left, dest.top, dest.width(), dest.height()
                        );
                    }

                    ClientEvent::Display(DisplayEvent::StreamDestroyed { stream_id }) => {
                        eprintln!("[SPICE] StreamDestroyed id={stream_id}");
                    }

                    ClientEvent::Display(DisplayEvent::UnhandledDraw { msg_type, size }) => {
                        eprintln!("[SPICE] UnhandledDraw type={msg_type} size={size}");
                    }

                    ClientEvent::Cursor(ev) => {
                        if session.config.cursor_enabled {
                            let payload = cursor_to_payload(&session.config.session_id, ev);
                            let _ = app.emit("spice-cursor", payload);
                        }
                    }

                    ClientEvent::MouseMode(mode) => {
                        let mode_str = match mode {
                            MouseMode::Client => "client",
                            MouseMode::Server => "server",
                        };
                        let _ = app.emit("spice-mouse-mode", SpiceMouseModeEvent {
                            session_id: session.config.session_id.clone(),
                            mode: mode_str.to_string(),
                        });
                    }

                    ClientEvent::Closed(err) => {
                        let msg = err.map(|e| e.to_string())
                            .unwrap_or_else(|| "SPICE 服务器关闭连接".to_string());
                        return Err((SpiceErrorKind::Transport, msg, true));
                    }
                }
            }

            command = command_rx.recv() => {
                let Some(cmd) = command else { return Ok(()) };
                match cmd {
                    SpiceWorkerCommand::Input(input) => {
                        pending_inputs.push(input);
                        if last_input_flush.elapsed() >= INPUT_FLUSH_INTERVAL {
                            for ev in pending_inputs.drain(..) {
                                if let Err(e) = send_spice_input(&client, ev).await {
                                    eprintln!("[SPICE] 输入发送失败: {e:?}");
                                }
                            }
                            last_input_flush = Instant::now();
                        }
                    }
                    SpiceWorkerCommand::FullRefresh => {
                        if let Some(id) = active_surface_id {
                            if let Some(s) = surfaces.get(&id) {
                                dirty_rect = Some((0, 0, s.fb.width, s.fb.height));
                            }
                        }
                    }
                }
            }

            _ = ticker.tick() => {
                // 刷新输入队列
                if !pending_inputs.is_empty() {
                    for ev in pending_inputs.drain(..) {
                        if let Err(e) = send_spice_input(&client, ev).await {
                            eprintln!("[SPICE] 输入发送失败: {e:?}");
                        }
                    }
                    last_input_flush = Instant::now();
                }

                // Mark 超时提交：避免服务器不发 Mark 时永远不刷新
                let mark_ready = pending_mark
                    || mark_deadline.map(|d| Instant::now() >= d).unwrap_or(false);
                if mark_ready {
                    pending_mark = false;
                    mark_deadline = None;
                }

                // 发帧：dirty 且（Mark 已到 或 超时）
                let should_emit = dirty_rect.is_some()
                    && mark_ready
                    && last_emit.elapsed() >= FRAME_EMIT_INTERVAL;
                if should_emit {
                    let rect = dirty_rect.take().unwrap();
                    if let Some(surface) = active_surface_id.and_then(|id| surfaces.get(&id)) {
                        let is_full = rect.2 >= surface.fb.width && rect.3 >= surface.fb.height;
                        let quality = if is_full { 45 } else { 75 };
                        let (rgb, w, h) = surface.fb.extract_rgb(rect);
                        let jpeg = tokio::task::spawn_blocking(move || {
                            let mut jpeg = Vec::new();
                            let mut enc = JpegEncoder::new_with_quality(&mut jpeg, quality);
                            enc.encode(&rgb, w, h, image::ExtendedColorType::Rgb8).ok()?;
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
                            last_emit = Instant::now();
                        }
                    }
                }
            }
        }
    }
}

fn cursor_to_payload(session_id: &str, ev: CursorEvent) -> SpiceCursorEvent {
    use base64::Engine;
    match ev {
        CursorEvent::Set { x, y, hot_x, hot_y, width, height, pixels, unique, cacheable: _, visible } => {
            let pixels_base64 = if pixels.is_empty() {
                None
            } else {
                Some(base64::engine::general_purpose::STANDARD.encode(&pixels))
            };
            SpiceCursorEvent {
                session_id: session_id.to_string(),
                kind: "set".to_string(),
                x, y, hot_x, hot_y, width, height,
                pixels_base64,
                unique,
                visible,
            }
        }
        CursorEvent::SetFromCache { x, y, unique, visible } => SpiceCursorEvent {
            session_id: session_id.to_string(),
            kind: "from-cache".to_string(),
            x, y, hot_x: 0, hot_y: 0, width: 0, height: 0,
            pixels_base64: None,
            unique,
            visible,
        },
        CursorEvent::Move { x, y } => SpiceCursorEvent {
            session_id: session_id.to_string(),
            kind: "move".to_string(),
            x, y, hot_x: 0, hot_y: 0, width: 0, height: 0,
            pixels_base64: None,
            unique: 0,
            visible: true,
        },
        CursorEvent::Hide => SpiceCursorEvent {
            session_id: session_id.to_string(),
            kind: "hide".to_string(),
            x: 0, y: 0, hot_x: 0, hot_y: 0, width: 0, height: 0,
            pixels_base64: None,
            unique: 0,
            visible: false,
        },
        CursorEvent::InvalidateOne { unique } => SpiceCursorEvent {
            session_id: session_id.to_string(),
            kind: "invalidate-one".to_string(),
            x: 0, y: 0, hot_x: 0, hot_y: 0, width: 0, height: 0,
            pixels_base64: None,
            unique,
            visible: true,
        },
        CursorEvent::InvalidateAll => SpiceCursorEvent {
            session_id: session_id.to_string(),
            kind: "invalidate-all".to_string(),
            x: 0, y: 0, hot_x: 0, hot_y: 0, width: 0, height: 0,
            pixels_base64: None,
            unique: 0,
            visible: true,
        },
    }
}

async fn send_spice_input(
    client: &SpiceClient,
    input: SpiceInputEvent,
) -> Result<(), (SpiceErrorKind, String, bool)> {
    let ev = match input {
        SpiceInputEvent::Key { scancode, pressed } => {
            if pressed {
                InputEvent::KeyDown(scancode)
            } else {
                InputEvent::KeyUp(scancode)
            }
        }
        SpiceInputEvent::PointerAbs { x, y, buttons, display } => InputEvent::MousePosition {
            x,
            y,
            buttons,
            display: display.unwrap_or(0),
        },
        SpiceInputEvent::PointerRel { dx, dy, buttons } => {
            InputEvent::MouseMotion { dx, dy, buttons }
        }
        SpiceInputEvent::MousePress { button, buttons } => {
            InputEvent::MousePress { button, buttons }
        }
        SpiceInputEvent::MouseRelease { button, buttons } => {
            InputEvent::MouseRelease { button, buttons }
        }
    };
    client.send_input(ev).await.map_err(|e| {
        (
            SpiceErrorKind::Transport,
            format!("SPICE send_input 失败: {e}"),
            true,
        )
    })
}

// ============================================================================
// 帧发送（与 vnc.rs 一致）
// ============================================================================

async fn queue_or_send_frame(session: &Arc<SpiceSession>, frame: Vec<u8>) {
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

async fn flush_pending_frames(session: &Arc<SpiceSession>, attach_id: u64) {
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

async fn send_full_frame(session: &Arc<SpiceSession>, attach_id: u64) -> Result<()> {
    let fb_guard = session.active_fb.lock().await;
    let Some(fb) = fb_guard.as_ref() else {
        return Ok(());
    };
    let w = fb.width;
    let h = fb.height;
    let bgra = fb.bgra.clone();
    drop(fb_guard);

    let jpeg = tokio::task::spawn_blocking(move || {
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for px in bgra.chunks_exact(4) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
        let mut jpeg = Vec::new();
        let mut enc = JpegEncoder::new_with_quality(&mut jpeg, 45);
        enc.encode(&rgb, w, h, image::ExtendedColorType::Rgb8).ok()?;
        Some(jpeg)
    })
    .await
    .ok()
    .flatten();

    let Some(jpeg) = jpeg else { return Ok(()) };
    let packet = pack_frame(0, 0, w as u16, h as u16, &jpeg);
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
    session: &SpiceSession,
    state: SpiceState,
    message: Option<String>,
    error_kind: Option<SpiceErrorKind>,
) {
    *session.state.lock().await = state;
    *session.message.lock().await = message;
    *session.error_kind.lock().await = error_kind;
}

fn emit_state(
    app: &AppHandle,
    session: &SpiceSession,
    state: SpiceState,
    message: Option<String>,
    error_kind: Option<SpiceErrorKind>,
) {
    let event_name = format!("spice-state-{}", session.config.session_id);
    eprintln!("[SPICE emit] {event_name} -> {state:?} msg={message:?} err={error_kind:?}");
    match app.emit(&event_name, SpiceStateEvent {
        session_id: session.config.session_id.clone(),
        state,
        message,
        error_kind,
    }) {
        Ok(_) => {}
        Err(e) => eprintln!("[SPICE emit] {event_name} FAILED: {e}"),
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