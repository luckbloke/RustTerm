//! 纯 Rust 的 RDP 客户端实现，基于 rdp-rs-2 0.1.2。
//!
//! 设计取舍：
//! - 不设 socket read timeout。因为 rdp-rs-2 的 read_exact 超时后不会回滚已读字节，
//!   会导致协议错位、连接崩溃。
//! - 每次 client.read 返回后，立刻处理 input_rx，让"移动时"的输入几乎无延迟。
//! - 静止时的输入延迟 = 服务器推流间隔，这是 rdp-rs-2 架构下的上限。
//! - 每 50ms 发一帧，把一次画面更新里的几百个 64x64 图块合并成一个 dirty_rect。
//! - read 出错后自动重连 3 次（1s / 2s / 4s，带抖动）。

use anyhow::Result;
use image::{codecs::jpeg::JpegEncoder, RgbImage};
use rdp::core::client::Connector;
use rdp::core::event::{BitmapEvent, KeyboardEvent, PointerButton, PointerEvent, RdpEvent};
use serde::Serialize;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};

/// RDP 会话状态。前端根据它更新状态栏。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RdpState {
    Connecting,
    Authenticating,
    Active,
    Reconnecting,
    Disconnected,
    Failed,
}

/// 失败原因分类。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RdpErrorKind {
    Transport,
    Authentication,
    Session,
    Internal,
}

pub struct RdpHandle {
    input_tx: std::sync::mpsc::Sender<RdpEvent>,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    need_full_redraw: Arc<AtomicBool>,
}

impl RdpHandle {
    pub fn send_pointer_by_name(&self, x: u16, y: u16, button: &str, down: bool) -> Result<()> {
        let btn = match button {
            "left" => PointerButton::Left,
            "right" => PointerButton::Right,
            "middle" => PointerButton::Middle,
            "none" => PointerButton::None,
            _ => PointerButton::None,
        };
        let _ = self.input_tx.send(RdpEvent::Pointer(PointerEvent {
            x, y, button: btn, down,
        }));
        Ok(())
    }

    pub fn send_key(&self, code: u16, down: bool) -> Result<()> {
        let _ = self.input_tx.send(RdpEvent::Key(KeyboardEvent { code, down }));
        Ok(())
    }

    /// 批量发送输入事件。事件用 JSON 描述，Rust 端解析成 RdpEvent。
    ///
    /// 前端格式：
    ///   [{ "kind": "pointer", "x": 100, "y": 200, "button": "left", "down": true },
    ///    { "kind": "key", "code": 30, "down": true }]
    pub fn send_input_batch(&self, events: Vec<serde_json::Value>) -> Result<()> {
        for ev in events {
            let kind = ev.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "pointer" => {
                    let x = ev.get("x").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    let y = ev.get("y").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    let button = ev.get("button").and_then(|v| v.as_str()).unwrap_or("none");
                    let down = ev.get("down").and_then(|v| v.as_bool()).unwrap_or(false);
                    let btn = match button {
                        "left" => PointerButton::Left,
                        "right" => PointerButton::Right,
                        "middle" => PointerButton::Middle,
                        "none" => PointerButton::None,
                        _ => PointerButton::None,
                    };
                    let _ = self.input_tx.send(RdpEvent::Pointer(PointerEvent {
                        x, y, button: btn, down,
                    }));
                }
                "key" => {
                    let code = ev.get("code").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    let down = ev.get("down").and_then(|v| v.as_bool()).unwrap_or(false);
                    let _ = self.input_tx.send(RdpEvent::Key(KeyboardEvent { code, down }));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 是否暂停帧传输。true = 暂停，false = 恢复。
    pub fn set_streaming(&self, paused: bool) -> Result<()> {
        let was_paused = self.paused.swap(paused, Ordering::Relaxed);
        if was_paused && !paused {
            // 从暂停恢复：重发全屏
            // 设置一个标志，让事件循环下一 tick 发全屏
            self.need_full_redraw.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn close(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 发送 RDP 状态事件给前端。
fn emit_state(
    app: &AppHandle,
    sid: &str,
    state: RdpState,
    message: Option<String>,
    error_kind: Option<RdpErrorKind>,
) {
    let _ = app.emit(
        "rdp:state",
        serde_json::json!({
            "sessionId": sid,
            "state": state,
            "message": message,
            "errorKind": error_kind,
        }),
    );
}

/// 根据错误信息分类。
fn classify_read_error<E: std::fmt::Debug>(e: &E) -> RdpErrorKind {
    let msg = format!("{e:?}").to_lowercase();
    if msg.contains("timed out")
        || msg.contains("connection reset")
        || msg.contains("broken pipe")
        || msg.contains("10054")
        || msg.contains("os error 10060")
    {
        RdpErrorKind::Transport
    } else if msg.contains("auth")
        || msg.contains("password")
        || msg.contains("credssp")
        || msg.contains("logon")
    {
        RdpErrorKind::Authentication
    } else if msg.contains("eof") || msg.contains("closed") {
        RdpErrorKind::Session
    } else {
        RdpErrorKind::Internal
    }
}

/// 重连延迟：1s → 2s → 4s，带 250ms 抖动。
fn reconnect_delay(attempt: u32) -> std::time::Duration {
    let base_ms = match attempt {
        0 | 1 => 1_000u64,
        2 => 2_000,
        3 => 4_000,
        _ => 8_000,
    };
    let jitter = rand::random::<u64>() % 250;
    std::time::Duration::from_millis(base_ms + jitter)
}

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

pub async fn connect(
    app: AppHandle,
    session_id: String,
    host: String,
    port: u16,
    domain: String,
    username: String,
    password: String,
    width: u16,
    height: u16,
    frame_channel: Channel<InvokeResponseBody>,
) -> Result<RdpHandle> {
    let cancel = Arc::new(AtomicBool::new(false));
    let (input_tx, input_rx) = std::sync::mpsc::channel::<RdpEvent>();

    let cancel_clone = cancel.clone();
    let sid = session_id.clone();
    let app_out = app.clone();
    let ch = frame_channel.clone();

    // 保存连接参数，重连时用
    let host_c = host.clone();
    let domain_c = domain.clone();
    let username_c = username.clone();
    let password_c = password.clone();

    let paused = Arc::new(AtomicBool::new(false));
    let paused_clone = paused.clone();
    let need_full_redraw = Arc::new(AtomicBool::new(false));
    let need_full_redraw_clone = need_full_redraw.clone();

    tokio::task::spawn_blocking(move || {
        eprintln!("[RDP] ========== 开始连接 ==========");
        eprintln!("[RDP] addr={host}:{port} user={username} domain={domain} size={width}x{height}");

        emit_state(
            &app_out,
            &sid,
            RdpState::Connecting,
            Some("正在连接 TCP".to_string()),
            None,
        );

        let addr = format!("{host}:{port}");
        let tcp = match TcpStream::connect(&addr) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[RDP] TCP 连接失败: {e}");
                emit_state(
                    &app_out,
                    &sid,
                    RdpState::Failed,
                    Some(format!("TCP 连接失败: {e}")),
                    Some(RdpErrorKind::Transport),
                );
                let _ = app_out.emit("rdp:closed", serde_json::json!({ "sessionId": sid }));
                return;
            }
        };
        eprintln!("[RDP] TCP 连接成功");

        emit_state(
            &app_out,
            &sid,
            RdpState::Authenticating,
            Some("正在认证".to_string()),
            None,
        );

        // 不设 read timeout。rdp-rs-2 的 read_exact 超时后不会回滚已读字节，
        // 会导致协议错位、连接崩溃。
        let mut connector = Connector::new()
            .screen(width, height)
            .credentials(domain_c.clone(), username_c.clone(), password_c.clone());

        eprintln!("[RDP] 开始 RDP 协议握手...");
        let mut client = match connector.connect(tcp) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[RDP] RDP 握手失败: {e:?}");
                emit_state(
                    &app_out,
                    &sid,
                    RdpState::Failed,
                    Some(format!("RDP 握手失败: {e:?}")),
                    Some(RdpErrorKind::Authentication),
                );
                let _ = app_out.emit("rdp:closed", serde_json::json!({ "sessionId": sid }));
                return;
            }
        };
        eprintln!("[RDP] ✓ RDP 协议握手完成，session_id = {sid}");

        let _ = app_out.emit(
            "rdp:resolution",
            serde_json::json!({
                "sessionId": sid,
                "width": width,
                "height": height,
            }),
        );

        let mut framebuffer = RgbImage::new(width as u32, height as u32);
        let input_rx = input_rx;
        let mut dirty_rect: Option<(u32, u32, u32, u32)> = None;
        let mut tick: u64 = 0;
        let mut read_count: u64 = 0;
        let mut bitmap_count: u64 = 0;
        let mut pointer_count: u64 = 0;
        let mut key_count: u64 = 0;

        // 上次发帧时间。每 50ms 才发一次，把一次画面更新里的
        // 几百个 64x64 图块合并成一个 dirty_rect。
        let mut last_emit = std::time::Instant::now() - std::time::Duration::from_secs(1);
                // 帧队列，最多 2 帧
        let mut frame_queue: std::collections::VecDeque<Vec<u8>> =
            std::collections::VecDeque::with_capacity(2);
        const MAX_FRAME_QUEUE: usize = 2;
        let emit_interval = std::time::Duration::from_millis(50);

        loop {
            if cancel_clone.load(Ordering::Relaxed) {
                eprintln!("[RDP] 收到取消信号");
                break;
            }

            // 从暂停恢复时，标记整屏为脏，下一帧发全屏
            if need_full_redraw_clone.swap(false, Ordering::Relaxed) {
                dirty_rect = Some((0, 0, width as u32, height as u32));
            }

            // 1. read 前处理一次输入。如果 read 即将阻塞，输入已经发出去了。
            while let Ok(ev) = input_rx.try_recv() {
                if let Err(e) = client.write(ev) {
                    eprintln!("[RDP] 发送输入失败: {e:?}");
                }
            }

            // 2. 读服务器事件（阻塞）。match event 按值。
            read_count += 1;
            let result = client.read(|event| match event {
                RdpEvent::Bitmap(bitmap) => {
                    bitmap_count += 1;
                    if bitmap_count <= 5 {
                        eprintln!(
                            "[RDP] ← Bitmap #{}: bpp={} compress={} dest=({},{})-({},{})",
                            bitmap_count,
                            bitmap.bpp,
                            bitmap.is_compress,
                            bitmap.dest_left,
                            bitmap.dest_top,
                            bitmap.dest_right,
                            bitmap.dest_bottom,
                        );
                    }
                    if bitmap.bpp != 32 {
                        return;
                    }
                    if let Err(e) = apply_bitmap(&mut framebuffer, bitmap, &mut dirty_rect) {
                        eprintln!("[RDP] apply_bitmap 失败: {e:?}");
                    }
                }
                RdpEvent::Pointer(_) => {
                    pointer_count += 1;
                }
                RdpEvent::Key(_) => {
                    key_count += 1;
                }
            });

            // 第一次 read 成功说明会话已激活
            if read_count == 1 && result.is_ok() {
                emit_state(&app_out, &sid, RdpState::Active, None, None);
            }

            if let Err(e) = result {
                eprintln!("[RDP] ✗ read 出错（第 {read_count} 次）: {e:?}");
                let kind = classify_read_error(&e);
                emit_state(
                    &app_out,
                    &sid,
                    RdpState::Failed,
                    Some(format!("连接中断: {e:?}")),
                    Some(kind),
                );

                // 认证失败 / 内部错误不重连
                if matches!(kind, RdpErrorKind::Authentication | RdpErrorKind::Internal) {
                    emit_state(
                        &app_out,
                        &sid,
                        RdpState::Disconnected,
                        Some("会话结束".to_string()),
                        Some(kind),
                    );
                    break;
                }

                // 自动重连 3 次
                let mut reconnected = false;
                for attempt in 1..=3u32 {
                    let delay = reconnect_delay(attempt);
                    eprintln!(
                        "[RDP] {} 秒后尝试第 {} 次重连",
                        delay.as_secs(),
                        attempt
                    );
                    emit_state(
                        &app_out,
                        &sid,
                        RdpState::Reconnecting,
                        Some(format!("第 {attempt}/3 次重连")),
                        None,
                    );
                    std::thread::sleep(delay);

                    // 重新建立 TCP + 握手
                    let addr = format!("{host_c}:{port}");
                    let new_tcp = match TcpStream::connect(&addr) {
                        Ok(t) => t,
                        Err(err) => {
                            eprintln!("[RDP] 第 {} 次重连 TCP 失败: {err}", attempt);
                            continue;
                        }
                    };

                    let mut new_connector = Connector::new()
                        .screen(width, height)
                        .credentials(
                            domain_c.clone(),
                            username_c.clone(),
                            password_c.clone(),
                        );

                    match new_connector.connect(new_tcp) {
                        Ok(new_client) => {
                            eprintln!("[RDP] 第 {} 次重连成功", attempt);
                            client = new_client;
                            framebuffer = RgbImage::new(width as u32, height as u32);
                            dirty_rect = None;
                            read_count = 0;
                            bitmap_count = 0;
                            tick = 0;
                            last_emit = std::time::Instant::now()
                                - std::time::Duration::from_secs(1);
                            emit_state(
                                &app_out,
                                &sid,
                                RdpState::Active,
                                Some("已重新连接".to_string()),
                                None,
                            );
                            reconnected = true;
                            break;
                        }
                        Err(err) => {
                            eprintln!("[RDP] 第 {} 次重连握手失败: {err:?}", attempt);
                        }
                    }
                }

                if reconnected {
                    continue;
                }

                emit_state(
                    &app_out,
                    &sid,
                    RdpState::Disconnected,
                    Some("重连失败，会话结束".to_string()),
                    Some(kind),
                );
                break;
            }

            // 3. read 一返回就立刻处理输入。
            // 鼠标移动时服务器持续推画面，read 反复返回，输入几乎无延迟。
            while let Ok(ev) = input_rx.try_recv() {
                if let Err(e) = client.write(ev) {
                    eprintln!("[RDP] read 后发送输入失败: {e:?}");
                }
            }

            // 4. 累积 50ms 后发一帧
            if last_emit.elapsed() >= emit_interval {
                if !paused_clone.load(Ordering::Relaxed) {
                    if let Some((x, y, w, h)) = dirty_rect.take() {
                        tick += 1;
                        // 编码 JPEG 并加入队列
                        if let Some(frame) = build_frame(&framebuffer, x, y, w, h) {
                            if frame_queue.len() >= MAX_FRAME_QUEUE {
                                frame_queue.pop_front();
                            }
                            frame_queue.push_back(frame);
                        }
                    }
                    // 队列里的帧一次性发出去
                    while let Some(frame) = frame_queue.pop_front() {
                        let _ = ch.send(InvokeResponseBody::Raw(frame));
                    }
                } else {
                    // 暂停时丢弃脏矩形，恢复时重发全屏
                    dirty_rect = None;
                }
                last_emit = std::time::Instant::now();
            }
        }

        eprintln!("[RDP] ========== 事件循环退出 ==========");
        eprintln!(
            "[RDP] 统计: read={read_count} bitmap={bitmap_count} \
             pointer={pointer_count} key={key_count} tick={tick}"
        );
        let _ = app_out.emit("rdp:closed", serde_json::json!({ "sessionId": sid }));
    });

        Ok(RdpHandle { input_tx, cancel, paused, need_full_redraw })
}

/// 把位图画到 framebuffer。dest_right/dest_bottom 是包含的，所以要 +1。
fn apply_bitmap(
    fb: &mut RgbImage,
    bitmap: BitmapEvent,
    dirty_rect: &mut Option<(u32, u32, u32, u32)>,
) -> Result<()> {
    let bpp = bitmap.bpp;
    let is_compress = bitmap.is_compress;
    let bmp_w = bitmap.width as u32;
    let dst_left = bitmap.dest_left as u32;
    let dst_top = bitmap.dest_top as u32;
    let dst_right = bitmap.dest_right as u32 + 1;
    let dst_bottom = bitmap.dest_bottom as u32 + 1;
    let raw_data = bitmap.data.clone();

    if bpp != 32 {
        return Ok(());
    }

    let data: Vec<u8> = if is_compress {
        bitmap
            .decompress()
            .map_err(|e| anyhow::anyhow!("RDP 解压失败: {e:?}"))?
    } else {
        raw_data
    };

    let dst_w = dst_right - dst_left;
    let dst_h = dst_bottom - dst_top;

    let fb_w = fb.width();
    let fb_h = fb.height();
    if dst_left >= fb_w || dst_top >= fb_h {
        return Ok(());
    }
    let w = dst_w.min(fb_w - dst_left);
    let h = dst_h.min(fb_h - dst_top);
    if w == 0 || h == 0 {
        return Ok(());
    }

    let src_row_bytes = (bmp_w * 4) as usize;
    for row in 0..h {
        let py = dst_top + row;
        let src_off = (row as usize) * src_row_bytes;
        if src_off + src_row_bytes > data.len() {
            break;
        }
        let src_row = &data[src_off..];
        for col in 0..w {
            let px = dst_left + col;
            let i = (col as usize) * 4;
            if i + 3 >= src_row.len() {
                break;
            }
            let pixel = fb.get_pixel_mut(px, py);
            *pixel = image::Rgb([src_row[i + 2], src_row[i + 1], src_row[i]]);
        }
    }

    let r = (dst_left, dst_top, w, h);
    *dirty_rect = Some(match *dirty_rect {
        None => r,
        Some(d) => {
            let x1 = d.0.min(r.0);
            let y1 = d.1.min(r.1);
            let x2 = (d.0 + d.2).max(r.0 + r.2);
            let y2 = (d.1 + d.3).max(r.1 + r.3);
            (x1, y1, x2 - x1, y2 - y1)
        }
    });

    Ok(())
}

/// 编码脏矩形区域为帧包。失败返回 None。
fn build_frame(fb: &RgbImage, x: u32, y: u32, w: u32, h: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 {
        return None;
    }
    let x = x.min(fb.width());
    let y = y.min(fb.height());
    let w = w.min(fb.width() - x);
    let h = h.min(fb.height() - y);
    if w == 0 || h == 0 {
        return None;
    }

    let sub = image::imageops::crop_imm(fb, x, y, w, h).to_image();
    let is_full = w >= fb.width() && h >= fb.height();
    let quality = if is_full { 30 } else { 50 };

    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, quality);
    encoder
        .encode(sub.as_raw(), sub.width(), sub.height(), image::ExtendedColorType::Rgb8)
        .ok()?;

    Some(pack_frame(x as u16, y as u16, w as u16, h as u16, &jpeg))
}