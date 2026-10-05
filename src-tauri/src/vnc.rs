use anyhow::{Context, Result};
use image::{codecs::jpeg::JpegEncoder, RgbImage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;
use vnc::{PixelFormat, VncConnector, VncEvent, X11Event, ClientKeyEvent, ClientMouseEvent};

/// VNC 会话句柄，供前端发送输入事件。
pub struct VncHandle {
    input_tx: tokio::sync::mpsc::UnboundedSender<X11Event>,
    cancel: Arc<AtomicBool>,
}

impl VncHandle {
    pub fn send_key(&self, keysym: u32, down: bool) -> Result<()> {
        let _ = self.input_tx.send(X11Event::KeyEvent(ClientKeyEvent {
            keycode: keysym,
            down,
        }));
        Ok(())
    }

    pub fn send_pointer(&self, x: u16, y: u16, buttons: u8) -> Result<()> {
        let _ = self.input_tx.send(X11Event::PointerEvent(ClientMouseEvent {
            position_x: x,
            position_y: y,
            bottons: buttons,
        }));
        Ok(())
    }

    pub fn close(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 连接到 VNC 服务器并启动事件循环。
pub async fn connect(
    app: AppHandle,
    session_id: String,
    host: &str,
    port: u16,
    password: &str,
) -> Result<VncHandle> {
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("无法连接 VNC 服务器 {host}:{port}"))?;

    let password = password.to_string();
    let vnc = VncConnector::new(tcp)
        .set_auth_method(async move { Ok(password) })
        .add_encoding(vnc::VncEncoding::Tight)
        .add_encoding(vnc::VncEncoding::Zrle)
        .add_encoding(vnc::VncEncoding::Raw)
        .allow_shared(true)
        .set_pixel_format(PixelFormat::bgra())
        .build()?
        .try_start()
        .await?
        .finish()?;

    let cancel = Arc::new(AtomicBool::new(false));
    let (input_tx, mut input_rx) = tokio::sync::mpsc::UnboundedSender::unbounded_channel();

    let cancel_clone = cancel.clone();
    let sid = session_id.clone();
    let app_out = app.clone();

    tokio::spawn(async move {
        // 当前帧缓冲
        let mut framebuffer: Option<RgbImage> = None;

        loop {
            tokio::select! {
                // 接收 VNC 服务器事件
                event = vnc.poll_event() => {
                    match event {
                        Ok(Some(VncEvent::SetResolution(screen))) => {
                            framebuffer = Some(RgbImage::new(
                                screen.width as u32,
                                screen.height as u32,
                            ));
                            let _ = app_out.emit("vnc:resolution", serde_json::json!({
                                "sessionId": sid,
                                "width": screen.width,
                                "height": screen.height,
                            }));
                        }
                        Ok(Some(VncEvent::RawImage(rect, data))) => {
                            if let Some(ref mut fb) = framebuffer {
                                blit_bgra(fb, &rect, &data);
                                emit_frame(&app_out, &sid, fb);
                            }
                        }
                        Ok(Some(VncEvent::JpegImage(rect, data))) => {
                            // JPEG 直接转发，前端解码更快
                            let _ = app_out.emit("vnc:jpeg", serde_json::json!({
                                "sessionId": sid,
                                "x": rect.x, "y": rect.y,
                                "width": rect.width, "height": rect.height,
                                "data": data,
                            }));
                        }
                        Ok(Some(VncEvent::Copy(dst, src))) => {
                            if let Some(ref mut fb) = framebuffer {
                                copy_rect(fb, &dst, &src);
                                emit_frame(&app_out, &sid, fb);
                            }
                        }
                        Ok(Some(VncEvent::Bell)) | Ok(None) => {}
                        Ok(Some(_)) => {}
                        Err(e) => {
                            eprintln!("VNC 错误: {e}");
                            break;
                        }
                    }
                }
                // 接收前端输入事件
                Some(ev) = input_rx.recv() => {
                    if let Err(e) = vnc.input(ev).await {
                        eprintln!("VNC 输入错误: {e}");
                    }
                }
                // 检查取消标志
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    if cancel_clone.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
        }

        let _ = app_out.emit("vnc:closed", serde_json::json!({ "sessionId": sid }));
    });

    Ok(VncHandle { input_tx, cancel })
}

/// 把 BGRA 数据写入 RGB 帧缓冲。
fn blit_bgra(fb: &mut RgbImage, rect: &vnc::Rect, data: &[u8]) {
    let w = rect.width as u32;
    let h = rect.height as u32;
    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;
            if idx + 3 < data.len() {
                let px = fb.get_pixel_mut(rect.x as u32 + x, rect.y as u32 + y);
                *px = image::Rgb([data[idx + 2], data[idx + 1], data[idx]]);
            }
        }
    }
}

/// 把 Copy 事件应用到帧缓冲。
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

/// 把帧缓冲编码为 JPEG 并发送给前端。
fn emit_frame(app: &AppHandle, sid: &str, fb: &RgbImage) {
    let mut jpeg = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut jpeg, 75);
    if encoder.encode(fb.as_raw(), fb.width(), fb.height(), image::ExtendedColorType::Rgb8).is_ok() {
        let _ = app.emit("vnc:frame", serde_json::json!({
            "sessionId": sid,
            "width": fb.width(),
            "height": fb.height(),
            "data": jpeg,
        }));
    }
}