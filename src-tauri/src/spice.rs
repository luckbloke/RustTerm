//! 纯 Rust 的 SPICE 客户端实现，基于 capsaicin。
//!
//! capsaicin 是纯 Rust 的 SPICE 协议实现，不依赖 libspice 或 glib。
//! 它连接真实 QEMU SPICE 服务器，解码显示帧，并通过 inputs 通道
//! 转发键盘和鼠标事件。

use anyhow::{Context, Result};
use capsaicin_client::{
    ClientEvent, DisplayEvent, InputEvent, MouseMode, RegionPixels, SpiceClient,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

/// SPICE 会话句柄，供前端发送输入事件和关闭会话。
pub struct SpiceHandle {
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    cancel: Arc<AtomicBool>,
}

impl SpiceHandle {
    /// 发送键盘事件。scancode 是 PC-AT set-1 扫描码。
    pub fn send_key(&self, scancode: u32, down: bool) -> Result<()> {
        let event = if down {
            InputEvent::KeyDown(scancode)
        } else {
            InputEvent::KeyUp(scancode)
        };
        let _ = self.input_tx.send(event);
        Ok(())
    }

    /// 发送鼠标绝对位置（client 模式）。
    pub fn send_mouse_position(&self, x: u32, y: u32, buttons: u32) -> Result<()> {
        let _ = self.input_tx.send(InputEvent::MousePosition {
            x,
            y,
            buttons,
            display: 0,
        });
        Ok(())
    }

    /// 发送鼠标相对移动（server 模式）。
    pub fn send_mouse_motion(&self, dx: i32, dy: i32, buttons: u32) -> Result<()> {
        let _ = self.input_tx.send(InputEvent::MouseMotion { dx, dy, buttons });
        Ok(())
    }

    /// 鼠标按键按下。
    pub fn send_mouse_press(&self, button: u8, buttons: u32) -> Result<()> {
        let _ = self.input_tx.send(InputEvent::MousePress { button, buttons });
        Ok(())
    }

    /// 鼠标按键释放。
    pub fn send_mouse_release(&self, button: u8, buttons: u32) -> Result<()> {
        let _ = self.input_tx.send(InputEvent::MouseRelease { button, buttons });
        Ok(())
    }

    pub fn close(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 连接到 SPICE 服务器并启动事件循环。
pub async fn connect(
    app: AppHandle,
    session_id: String,
    host: &str,
    port: u16,
    password: &str,
) -> Result<SpiceHandle> {
    let addr = format!("{host}:{port}");
    let mut client = SpiceClient::connect(&addr, password)
        .await
        .with_context(|| format!("无法连接 SPICE 服务器 {addr}"))?;

    let cancel = Arc::new(AtomicBool::new(false));
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();

    let cancel_clone = cancel.clone();
    let sid = session_id.clone();
    let app_out = app.clone();

    tokio::spawn(async move {
        // 当前帧缓冲。SurfaceCreated 之前为 None。
        // (width, height, RGBA 字节)
        let mut framebuffer: Option<(u32, u32, Vec<u8>)> = None;
        // 主 surface id（通常是 0）
        let mut primary_surface: Option<u32> = None;

        loop {
            if cancel_clone.load(Ordering::Relaxed) {
                break;
            }

            tokio::select! {
                // 接收 SPICE 服务器事件
                event = client.next_event() => {
                    let Some(event) = event else { break };
                    match event {
                        ClientEvent::Display(DisplayEvent::SurfaceCreated {
                            id, width, height, primary, ..
                        }) => {
                            if primary || primary_surface.is_none() {
                                primary_surface = Some(id);
                                framebuffer = Some((
                                    width,
                                    height,
                                    vec![0u8; (width as usize) * (height as usize) * 4],
                                ));
                                let _ = app_out.emit("spice:resolution", serde_json::json!({
                                    "sessionId": sid,
                                    "width": width,
                                    "height": height,
                                }));
                            }
                        }

                        ClientEvent::Display(DisplayEvent::Region {
                            surface_id, rect, pixels, ..
                        }) => {
                            if Some(surface_id) != primary_surface {
                                continue;
                            }
                            if let Some((w, h, ref mut buf)) = framebuffer {
                                match pixels {
                                    RegionPixels::Raw { data, stride } => {
                                        blit_raw(buf, w, h, &rect, &data, stride);
                                    }
                                    RegionPixels::SolidColor(color) => {
                                        fill_solid(buf, w, h, &rect, color);
                                    }
                                }
                                emit_frame(&app_out, &sid, w, h, buf);
                            }
                        }

                        ClientEvent::Display(DisplayEvent::CopyRect {
                            surface_id, src_x, src_y, dest_rect,
                        }) => {
                            if Some(surface_id) != primary_surface {
                                continue;
                            }
                            if let Some((w, h, ref mut buf)) = framebuffer {
                                copy_rect(buf, w, h, src_x, src_y, &dest_rect);
                                emit_frame(&app_out, &sid, w, h, buf);
                            }
                        }

                        ClientEvent::MouseMode(mode) => {
                            let mode_str = match mode {
                                MouseMode::Client => "client",
                                MouseMode::Server => "server",
                            };
                            let _ = app_out.emit("spice:mouse-mode", serde_json::json!({
                                "sessionId": sid,
                                "mode": mode_str,
                            }));
                        }

                        ClientEvent::Closed(_) => {
                            let _ = app_out.emit("spice:closed", serde_json::json!({ "sessionId": sid }));
                            break;
                        }

                        _ => {}
                    }
                }

                // 接收前端输入事件
                Some(input_evt) = input_rx.recv() => {
                    if let Err(e) = client.send_input(input_evt).await {
                        eprintln!("SPICE 输入错误: {e}");
                    }
                }
            }
        }

        let _ = app_out.emit("spice:closed", serde_json::json!({ "sessionId": sid }));
    });

    Ok(SpiceHandle { input_tx, cancel })
}

/// 把 Raw 像素数据写入帧缓冲。
///
/// capsaicin 的 `RegionPixels::Raw { data, stride }` 提供原始像素，
/// `stride` 是每行字节数。这里按 BGRA（每像素 4 字节）处理。
fn blit_raw(
    buf: &mut [u8],
    width: u32,
    height: u32,
    rect: &capsaicin_client::Rect,
    data: &[u8],
    stride: u32,
) {
    let rx = rect.left as u32;
    let ry = rect.top as u32;
    let rw = rect.width() as u32;
    let rh = rect.height() as u32;
    let stride = stride as usize;

    for row in 0..rh {
        let dst_y = ry + row;
        if dst_y >= height {
            break;
        }
        for col in 0..rw {
            let dst_x = rx + col;
            if dst_x >= width {
                break;
            }
            let src_idx = (row as usize * stride) + (col as usize * 4);
            let dst_idx = ((dst_y as usize * width as usize) + dst_x as usize) * 4;
            if src_idx + 3 < data.len() && dst_idx + 3 < buf.len() {
                buf[dst_idx..dst_idx + 4].copy_from_slice(&data[src_idx..src_idx + 4]);
            }
        }
    }
}

/// 用纯色填充矩形区域。color 是 32 位 BGRA（小端）。
fn fill_solid(
    buf: &mut [u8],
    width: u32,
    height: u32,
    rect: &capsaicin_client::Rect,
    color: u32,
) {
    let px = color.to_le_bytes();
    let rx = rect.left as u32;
    let ry = rect.top as u32;
    let rw = rect.width() as u32;
    let rh = rect.height() as u32;

    for row in 0..rh {
        let dst_y = ry + row;
        if dst_y >= height {
            break;
        }
        for col in 0..rw {
            let dst_x = rx + col;
            if dst_x >= width {
                break;
            }
            let dst_idx = ((dst_y as usize * width as usize) + dst_x as usize) * 4;
            if dst_idx + 3 < buf.len() {
                buf[dst_idx..dst_idx + 4].copy_from_slice(&px);
            }
        }
    }
}

/// 处理 CopyRect：把帧缓冲中一块区域复制到另一块。
fn copy_rect(
    buf: &mut [u8],
    width: u32,
    height: u32,
    src_x: i32,
    src_y: i32,
    dest_rect: &capsaicin_client::Rect,
) {
    let dx = dest_rect.left as u32;
    let dy = dest_rect.top as u32;
    let dw = dest_rect.width() as u32;
    let dh = dest_rect.height() as u32;

    // 先读到临时缓冲，避免源和目标重叠时数据被破坏
    let mut tmp = Vec::with_capacity((dw * dh * 4) as usize);
    for row in 0..dh {
        let sy = src_y as u32 + row;
        for col in 0..dw {
            let sx = src_x as u32 + col;
            let idx = ((sy as usize * width as usize) + sx as usize) * 4;
            if idx + 3 < buf.len() {
                tmp.extend_from_slice(&buf[idx..idx + 4]);
            } else {
                tmp.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }

    for row in 0..dh {
        let dst_y = dy + row;
        if dst_y >= height {
            break;
        }
        for col in 0..dw {
            let dst_x = dx + col;
            if dst_x >= width {
                break;
            }
            let src_idx = ((row * dw + col) * 4) as usize;
            let dst_idx = ((dst_y as usize * width as usize) + dst_x as usize) * 4;
            if src_idx + 3 < tmp.len() && dst_idx + 3 < buf.len() {
                buf[dst_idx..dst_idx + 4].copy_from_slice(&tmp[src_idx..src_idx + 4]);
            }
        }
    }
}

/// 把帧缓冲编码为 JPEG 并发送给前端。
///
/// 不要每帧传原始 RGBA（1920×1080×4 ≈ 8MB/帧），会打爆 IPC。
/// JPEG 质量 75 通常能把一帧压到几十 KB。
fn emit_frame(app: &AppHandle, sid: &str, width: u32, height: u32, buf: &[u8]) {
    // BGRA → RGB
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for chunk in buf.chunks(4) {
        if chunk.len() >= 3 {
            rgb.push(chunk[2]); // R
            rgb.push(chunk[1]); // G
            rgb.push(chunk[0]); // B
        }
    }

    let mut jpeg = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75);
    if encoder
        .encode(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .is_ok()
    {
        let _ = app.emit(
            "spice:frame",
            serde_json::json!({
                "sessionId": sid,
                "width": width,
                "height": height,
                "data": jpeg,
            }),
        );
    }
}