use anyhow::{Context, Result};
use ironrdp::connector::{self, Credentials, DesktopSize};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_session::image::DecodedImage;
use ironrdp_tokio::TokioFramed;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;

/// RDP 会话句柄。
pub struct RdpHandle {
    cancel: Arc<AtomicBool>,
}

impl RdpHandle {
    pub fn close(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 连接到 RDP 服务器并处理图形更新。
pub async fn connect(
    app: AppHandle,
    session_id: String,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    width: u16,
    height: u16,
) -> Result<RdpHandle> {
    let config = connector::Config {
        credentials: Credentials::UsernamePassword {
            username: username.to_string(),
            password: password.to_string(),
        },
        domain: None,
        enable_tls: true,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        desktop_size: DesktopSize { width, height },
        client_name: "RustTerm".to_string(),
        client_build: 0,
        platform: MajorPlatformType::WINDOWS,
        ..Default::default()
    };

    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("无法连接 RDP 服务器 {host}:{port}"))?;

    let client_addr = tcp.local_addr()?;
    let mut framed = TokioFramed::new(tcp);
    let mut connector = connector::ClientConnector::new(config, client_addr);

    // 开始连接
    let should_upgrade = ironrdp_async::connect_begin(&mut framed, &mut connector).await?;

    // TLS 升级
    if should_upgrade {
        // 简化：实际需要完整的 TLS 配置
        // 此处仅为代码骨架，实际集成需处理证书验证
        anyhow::bail!("RDP TLS 升级需要完整配置，请参考 IronRDP 文档");
    }

    let connection_result = ironrdp_async::connect_finalize(connector, framed).await?;

    let mut image = DecodedImage::new(
        PixelFormat::RgbA32,
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    );

    let mut active_stage = ActiveStage::new(connection_result);

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();
    let sid = session_id.clone();
    let app_out = app.clone();

    // 注意：IronRDP 的异步 API 需要处理 Send 约束
    // 如果遇到 Send 错误，参考 GitHub Issue #542 的解决方案
    tokio::spawn(async move {
        let mut framed = connection_result.framed;
        loop {
            if cancel_clone.load(Ordering::Relaxed) {
                break;
            }

            // 读取 PDU
            let (action, payload) = match framed.read_pdu().await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("RDP 读取错误: {e}");
                    break;
                }
            };

            // 处理 PDU
            match active_stage.process(&mut image, action, &payload) {
                Ok(outputs) => {
                    for output in outputs {
                        match output {
                            ActiveStageOutput::GraphicsUpdate(_) => {
                                emit_rdp_frame(&app_out, &sid, &image);
                            }
                            ActiveStageOutput::Terminate(reason) => {
                                eprintln!("RDP 会话终止: {reason:?}");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    eprintln!("RDP 处理错误: {e}");
                    break;
                }
            }
        }

        let _ = app_out.emit("rdp:closed", serde_json::json!({ "sessionId": sid }));
    });

    Ok(RdpHandle { cancel })
}

/// 把 RDP 帧缓冲编码为 JPEG 并发送给前端。
fn emit_rdp_frame(app: &AppHandle, sid: &str, image: &DecodedImage) {
    let width = image.width() as u32;
    let height = image.height() as u16;
    let rgba = image.data();

    // RGBA → RGB
    let mut rgb = Vec::with_capacity((width * height as u32 * 3) as usize);
    for chunk in rgba.chunks(4) {
        if chunk.len() >= 3 {
            rgb.extend_from_slice(&chunk[..3]);
        }
    }

    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75);
    if encoder
        .encode(&rgb, width, height as u32, image::ExtendedColorType::Rgb8)
        .is_ok()
    {
        let _ = app.emit(
            "rdp:frame",
            serde_json::json!({
                "sessionId": sid,
                "width": width,
                "height": height,
                "data": jpeg,
            }),
        );
    }
}