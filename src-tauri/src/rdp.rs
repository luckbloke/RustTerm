//! 纯 Rust 的 RDP 客户端实现，基于 rdp-rs-2 0.1.2。
//!
//! 设计取舍：
//! - 不设 socket read timeout。因为 rdp-rs-2 的 read_exact 超时后不会回滚已读字节，
//!   会导致协议错位、连接崩溃。
//! - 每次 client.read 返回后，立刻处理 input_rx，让"移动时"的输入几乎无延迟。
//! - 静止时的输入延迟 = 服务器推流间隔，这是 rdp-rs-2 架构下的上限。
//! - 每 50ms 发一帧，把一次画面更新里的几百个 64x64 图块合并成一个 dirty_rect。

use anyhow::Result;
use image::{codecs::jpeg::JpegEncoder, RgbImage};
use rdp::core::client::Connector;
use rdp::core::event::{BitmapEvent, KeyboardEvent, PointerButton, PointerEvent, RdpEvent};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};

pub struct RdpHandle {
    input_tx: std::sync::mpsc::Sender<RdpEvent>,
    cancel: Arc<AtomicBool>,
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

    pub fn close(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
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

    tokio::task::spawn_blocking(move || {
        eprintln!("[RDP] ========== 开始连接 ==========");
        eprintln!("[RDP] addr={host}:{port} user={username} domain={domain} size={width}x{height}");

        let addr = format!("{host}:{port}");
        let tcp = match TcpStream::connect(&addr) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[RDP] TCP 连接失败: {e}");
                let _ = app_out.emit("rdp:closed", serde_json::json!({ "sessionId": sid }));
                return;
            }
        };
        eprintln!("[RDP] TCP 连接成功");

        // 不设 read timeout。rdp-rs-2 的 read_exact 超时后不会回滚已读字节，
        // 会导致协议错位、连接崩溃。
        let mut connector = Connector::new()
            .screen(width, height)
            .credentials(domain, username, password);

        eprintln!("[RDP] 开始 RDP 协议握手...");
        let mut client = match connector.connect(tcp) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[RDP] RDP 握手失败: {e:?}");
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
        let emit_interval = std::time::Duration::from_millis(50);

        loop {
            if cancel_clone.load(Ordering::Relaxed) {
                eprintln!("[RDP] 收到取消信号");
                break;
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

            if let Err(e) = result {
                eprintln!("[RDP] ✗ read 出错（第 {read_count} 次）: {e:?}");
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
                if let Some((x, y, w, h)) = dirty_rect.take() {
                    tick += 1;
                    emit_region(&ch, &framebuffer, x, y, w, h);
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

    Ok(RdpHandle { input_tx, cancel })
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

fn emit_region(
    channel: &Channel<InvokeResponseBody>,
    fb: &RgbImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
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
    let quality = if is_full { 30 } else { 50 };

    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, quality);
    match encoder.encode(sub.as_raw(), sub.width(), sub.height(), image::ExtendedColorType::Rgb8) {
        Ok(_) => {
            // 减少日志噪音：只在区域较大时打印
            if w >= 512 || h >= 512 {
                eprintln!(
                    "[RDP] emit_region: {}x{} @({},{}) q={} jpeg={}字节",
                    w, h, x, y, quality, jpeg.len()
                );
            }
            let packet = pack_frame(x as u16, y as u16, w as u16, h as u16, &jpeg);
            let _ = channel.send(InvokeResponseBody::Raw(packet));
        }
        Err(e) => {
            eprintln!("[RDP] JPEG 编码失败: {e:?}");
        }
    }
}