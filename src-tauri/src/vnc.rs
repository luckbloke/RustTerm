//! 纯 Rust 的 VNC 客户端实现，基于 vnc-rs。
//!
//! 关键点：
//! 1. vnc-rs 不暴露 FramebufferUpdateRequest，自己按 RFB 协议发。
//! 2. vnc-rs 的 input() 不保证立即 flush，鼠标键盘也自己编码原始字节发。
//! 3. tokio::net::TcpStream 没有 try_clone()，通过 into_std() 中转。
//! 4. 帧数据用 Tauri v2 的 ipc::Channel<InvokeResponseBody> 走真二进制。
//! 5. 只编码变化区域（脏矩形），编码放到 spawn_blocking。
//! 6. 请求间隔 16ms（约 60fps），发帧节流 16ms。

use anyhow::{Context, Result};
use image::{codecs::jpeg::JpegEncoder, RgbImage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use vnc::{VncConnector, VncEvent};

pub struct VncHandle {
    input_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    cancel: Arc<AtomicBool>,
}

impl VncHandle {
    pub fn send_key(&self, keysym: u32, down: bool) -> Result<()> {
        let mut msg = Vec::with_capacity(8);
        msg.push(4);
        msg.push(if down { 1 } else { 0 });
        msg.push(0);
        msg.push(0);
        msg.extend_from_slice(&keysym.to_be_bytes());
        let _ = self.input_tx.send(msg);
        Ok(())
    }

    pub fn send_pointer(&self, x: u16, y: u16, buttons: u8) -> Result<()> {
        let mut msg = Vec::with_capacity(6);
        msg.push(5);
        msg.push(buttons);
        msg.extend_from_slice(&x.to_be_bytes());
        msg.extend_from_slice(&y.to_be_bytes());
        let _ = self.input_tx.send(msg);
        Ok(())
    }

    pub fn close(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

type Rect = (u32, u32, u32, u32);

fn union_rect(a: Option<Rect>, b: Rect) -> Rect {
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

/// 帧数据包：
///   byte 0-1   : x
///   byte 2-3   : y
///   byte 4-5   : width
///   byte 6-7   : height
///   byte 8-11  : jpeg 字节数 (u32)
///   byte 12..  : jpeg 数据
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
    host: &str,
    port: u16,
    password: &str,
    frame_channel: Channel<InvokeResponseBody>,
) -> Result<VncHandle> {
    eprintln!("[VNC] TcpStream::connect {host}:{port}");
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("TCP 连接失败 {host}:{port}"))?;

    let std_tcp = tcp.into_std().context("into_std 失败")?;
    let std_tcp_for_request = std_tcp.try_clone().context("try_clone 失败")?;

    let tcp_for_vnc = TcpStream::from_std(std_tcp).context("from_std (vnc) 失败")?;
    let tcp_for_request =
        TcpStream::from_std(std_tcp_for_request).context("from_std (request) 失败")?;

    let password = password.to_string();

    let connector = VncConnector::new(tcp_for_vnc)
        .set_auth_method(async move { Ok(password) })
        .add_encoding(vnc::VncEncoding::Raw)
        .allow_shared(true)
        .build()
        .context("VncConnector::build 失败")?;

    let started = connector
        .try_start()
        .await
        .context("try_start 失败（握手/认证阶段）")?;
    let vnc = started.finish().context("finish 失败（读 ServerInit）")?;

    eprintln!("[VNC] 连接建立完成，session_id = {session_id}");

    let cancel = Arc::new(AtomicBool::new(false));
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let cancel_clone = cancel.clone();
    let sid = session_id.clone();
    let app_out = app.clone();

    tokio::spawn(async move {
        let mut framebuffer: Option<RgbImage> = None;
        let mut dirty = false;
        let mut dirty_rect: Option<Rect> = None;
        let mut tick_count: u64 = 0;
        let mut raw_count: u64 = 0;
        let mut other_count: u64 = 0;

        // 请求间隔 16ms（约 60fps）
        let mut last_request = tokio::time::Instant::now();
        let request_interval = std::time::Duration::from_millis(16);

        // 发帧节流到 16ms
        let mut last_emit = tokio::time::Instant::now() - std::time::Duration::from_secs(1);
        let emit_interval = std::time::Duration::from_millis(16);

        let mut req_writer = tokio::io::BufWriter::new(tcp_for_request);
        let mut pending_full_request = true;

        // ticker 16ms，和 request_interval 一致
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(16));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                event = vnc.poll_event() => {
                    match event {
                        Ok(Some(VncEvent::SetResolution(screen))) => {
                            eprintln!("[VNC] SetResolution {}x{}", screen.width, screen.height);
                            framebuffer = Some(RgbImage::new(
                                screen.width as u32,
                                screen.height as u32,
                            ));
                            dirty = true;
                            dirty_rect = Some((0, 0, screen.width as u32, screen.height as u32));
                            pending_full_request = true;
                            let _ = app_out.emit("vnc:resolution", serde_json::json!({
                                "sessionId": sid,
                                "width": screen.width,
                                "height": screen.height,
                            }));
                        }
                        Ok(Some(VncEvent::RawImage(rect, data))) => {
                            raw_count += 1;
                            if raw_count <= 3 {
                                let nonzero = data.iter().filter(|&&b| b != 0).count();
                                eprintln!(
                                    "[VNC] RawImage #{raw_count} rect={:?} len={} nonzero={}",
                                    rect, data.len(), nonzero
                                );
                            } else if raw_count % 500 == 0 {
                                eprintln!("[VNC] RawImage 累计 {}", raw_count);
                            }
                            if let Some(ref mut fb) = framebuffer {
                                blit_bgra(fb, &rect, &data);
                                dirty = true;
                                let r: Rect = (
                                    rect.x as u32,
                                    rect.y as u32,
                                    rect.width as u32,
                                    rect.height as u32,
                                );
                                dirty_rect = Some(union_rect(dirty_rect, r));
                            }
                        }
                        Ok(Some(VncEvent::JpegImage(rect, data))) => {
                            let packet = pack_frame(
                                rect.x as u16,
                                rect.y as u16,
                                rect.width as u16,
                                rect.height as u16,
                                &data,
                            );
                            let _ = frame_channel.send(InvokeResponseBody::Raw(packet));
                        }
                        Ok(Some(VncEvent::Copy(dst, src))) => {
                            if let Some(ref mut fb) = framebuffer {
                                copy_rect(fb, &dst, &src);
                                dirty = true;
                                let r: Rect = (
                                    dst.x as u32,
                                    dst.y as u32,
                                    dst.width as u32,
                                    dst.height as u32,
                                );
                                dirty_rect = Some(union_rect(dirty_rect, r));
                            }
                        }
                        Ok(Some(VncEvent::Bell)) | Ok(None) => {}
                        Ok(Some(other)) => {
                            other_count += 1;
                            if other_count <= 10 {
                                eprintln!("[VNC] 其他事件 #{other_count}: {:?}", other);
                            }
                        }
                        Err(e) => {
                            eprintln!("[VNC] poll_event 出错，退出循环: {e}");
                            break;
                        }
                    }
                }

                Some(msg) = input_rx.recv() => {
                    if let Err(e) = req_writer.write_all(&msg).await {
                        eprintln!("[VNC] 发送输入事件失败: {e}");
                        break;
                    }
                    if let Err(e) = req_writer.flush().await {
                        eprintln!("[VNC] flush 输入事件失败: {e}");
                        break;
                    }
                }

                _ = ticker.tick() => {
                    tick_count += 1;
                    if cancel_clone.load(Ordering::Relaxed) {
                        eprintln!("[VNC] 收到取消信号，退出循环");
                        break;
                    }

                    // 定期发送 FramebufferUpdateRequest
                    if let Some(ref fb) = framebuffer {
                        let should_request = pending_full_request
                            || last_request.elapsed() >= request_interval;
                        if should_request {
                            let incremental = !pending_full_request;
                            let w = fb.width().min(u16::MAX as u32) as u16;
                            let h = fb.height().min(u16::MAX as u32) as u16;
                            let msg: [u8; 10] = [
                                3,
                                if incremental { 1 } else { 0 },
                                0, 0,
                                0, 0,
                                (w >> 8) as u8, (w & 0xff) as u8,
                                (h >> 8) as u8, (h & 0xff) as u8,
                            ];
                            if let Err(e) = req_writer.write_all(&msg).await {
                                eprintln!("[VNC] 发送 FramebufferUpdateRequest 失败: {e}");
                                break;
                            }
                            if let Err(e) = req_writer.flush().await {
                                eprintln!("[VNC] flush 失败: {e}");
                                break;
                            }
                            if pending_full_request {
                                eprintln!("[VNC] 发送全屏请求 {}x{}", w, h);
                                pending_full_request = false;
                            }
                            last_request = tokio::time::Instant::now();
                        }
                    }

                    // 发帧：脏矩形 + 16ms 节流
                    if dirty && last_emit.elapsed() >= emit_interval {
                        if let (Some(ref fb), Some((x, y, w, h))) = (&framebuffer, dirty_rect) {
                            let fb_clone = fb.clone();
                            let channel_clone = frame_channel.clone();
                            let current_tick = tick_count;
                            tokio::task::spawn_blocking(move || {
                                emit_frame_region(
                                    &channel_clone,
                                    &fb_clone,
                                    x, y, w, h,
                                    current_tick,
                                );
                            });
                        }
                        dirty = false;
                        dirty_rect = None;
                        last_emit = tokio::time::Instant::now();
                    }
                }
            }
        }

        eprintln!(
            "[VNC] 事件循环退出。总 RawImage={raw_count}, 其他={other_count}, tick={tick_count}"
        );
        let _ = app_out.emit("vnc:closed", serde_json::json!({ "sessionId": sid }));
    });

    Ok(VncHandle { input_tx, cancel })
}

/// 把 BGRA 数据写入 RGB 帧缓冲。按行处理，避免逐像素 get_pixel_mut。
fn blit_bgra(fb: &mut RgbImage, rect: &vnc::Rect, data: &[u8]) {
    let w = rect.width as u32;
    let h = rect.height as u32;
    let fb_w = fb.width();
    let fb_h = fb.height();
    let row_bytes = (w * 4) as usize;

    for y in 0..h {
        let py = rect.y as u32 + y;
        if py >= fb_h {
            continue;
        }
        let src_row = &data[(y as usize) * row_bytes..];
        for x in 0..w {
            let px = rect.x as u32 + x;
            if px >= fb_w {
                continue;
            }
            let i = (x as usize) * 4;
            if i + 3 >= src_row.len() {
                break;
            }
            let pixel = fb.get_pixel_mut(px, py);
            *pixel = image::Rgb([src_row[i + 2], src_row[i + 1], src_row[i]]);
        }
    }
}

fn copy_rect(fb: &mut RgbImage, dst: &vnc::Rect, src: &vnc::Rect) {
    let w = src.width.min(dst.width) as u32;
    let h = src.height.min(dst.height) as u32;
    let mut tmp = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let px = fb.get_pixel(src.x as u32 + x, src.y as u32 + y);
            tmp.push(*px);
        }
    }
    for y in 0..h {
        for x in 0..w {
            let px = &tmp[(y * w + x) as usize];
            *fb.get_pixel_mut(dst.x as u32 + x, dst.y as u32 + y) = *px;
        }
    }
}

/// 只编码脏矩形区域，打包成二进制通过 Channel 发送。
fn emit_frame_region(
    channel: &Channel<InvokeResponseBody>,
    fb: &RgbImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    tick: u64,
) {
    if w == 0 || h == 0 {
        return;
    }
    let x = x.min(fb.width());
    let y = y.min(fb.height());
    let w = w.min(fb.width() - x);
    let h = h.min(fb.height() - y);
    if w == 0 || h == 0 {
        return;
    }

    let sub = image::imageops::crop_imm(fb, x, y, w, h).to_image();

    let is_full = w >= fb.width() && h >= fb.height();
    let quality = if is_full { 40 } else { 75 };

    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, quality);
    if encoder
        .encode(
            sub.as_raw(),
            sub.width(),
            sub.height(),
            image::ExtendedColorType::Rgb8,
        )
        .is_err()
    {
        return;
    }

    if tick <= 5 {
        eprintln!(
            "[VNC] tick#{} 区域 {}x{} @({},{}) q={} JPEG {} 字节",
            tick, w, h, x, y, quality, jpeg.len()
        );
    }

    let packet = pack_frame(x as u16, y as u16, w as u16, h as u16, &jpeg);
    let _ = channel.send(InvokeResponseBody::Raw(packet));
}