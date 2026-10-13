//! 纯 Rust 的 RDP 客户端实现，基于 IronRDP。

use anyhow::Result;
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use ironrdp::client::config::{
    ConfigBuilder as IronRdpConfigBuilder,
    Destination as IronRdpDestination,
    DirectTransport as IronRdpDirectTransport,
    ServerCertificateVerifier,
    ServerCertificateVerifyFuture,
    TransportKind as IronRdpTransportKind,
};
use ironrdp::client::rdp::{
    RdpClient as IronRdpClient,
    RdpInputEvent as IronRdpInputEvent,
    RdpOutputEvent,
};
use ironrdp::input::{
    Database as IronRdpInputDatabase,
    MouseButton as IronRdpMouseButton,
    MousePosition as IronRdpMousePosition,
    Operation as IronRdpInputOperation,
    Scancode as IronRdpScancode,
    WheelRotations as IronRdpWheelRotations,
};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::pdu::input::fast_path::{
    FastPathInputEvent as IronRdpFastPathInputEvent,
    KeyboardFlags as IronRdpKeyboardFlags,
};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use x509_cert::der::Decode as _;

use crate::rdp_clipboard::{RdpClipboardBackendFactory, RdpClipboardBridge};

const MAX_FRAME_QUEUE: usize = 2;
const FULL_REFRESH_INTERVAL_MS: u64 = 2000;
const MAX_RECONNECT_ATTEMPTS: u32 = 3;
const INPUT_BATCH_WINDOW_MS: u64 = 8;
const RDP_RIGHT_SHIFT_SCAN_CODE: u16 = 0x36;
const RDP_MIN_WIDTH: u16 = 640;
const RDP_MIN_HEIGHT: u16 = 480;
const RDP_MAX_WIDTH: u16 = 7680;
const RDP_MAX_HEIGHT: u16 = 4320;
const CERTIFICATE_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const MAX_CLIPBOARD_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// 前端发来的 RDP 输入事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum RdpInputEvent {
    #[serde(rename = "pointer")]
    Pointer { x: u16, y: u16, button: String, down: bool },
    #[serde(rename = "key")]
    Key {
        code: u16,
        down: bool,
        #[serde(default)]
        extended: bool,
    },
    #[serde(rename = "mouse-wheel")]
    MouseWheel {
        #[serde(rename = "deltaX", default)]
        delta_x: f64,
        #[serde(rename = "deltaY", default)]
        delta_y: f64,
        x: u16,
        y: u16,
    },
    #[serde(rename = "unicode")]
    Unicode { text: String },
    #[serde(rename = "release-all-keys")]
    ReleaseAllKeys,
    #[serde(rename = "resize")]
    Resize { width: u16, height: u16 },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RdpState {
    Connecting,
    CertificateVerification,
    Authenticating,
    Active,
    Reconnecting,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RdpErrorKind {
    Transport,
    Authentication,
    Session,
    Internal,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum RdpPointerEvent {
    #[serde(rename = "default")]
    Default { session_id: String },
    #[serde(rename = "hidden")]
    Hidden { session_id: String },
    #[serde(rename = "position")]
    Position { session_id: String, x: u16, y: u16 },
    #[serde(rename = "bitmap")]
    Bitmap {
        session_id: String,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba_base64: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpCertificateVerifyEvent {
    pub request_id: String,
    pub session_id: String,
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub subject: Option<String>,
    pub issuer: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub known_host_status: String,
}

pub struct RdpCertificatePending {
    pub session_id: String,
    pub responder: oneshot::Sender<RdpCertificateDecision>,
}

#[derive(Debug, Clone)]
pub struct RdpCertificateDecision {
    pub accepted: bool,
    pub remember: bool,
}

pub type RdpCertificatePendingMap =
    Arc<AsyncMutex<HashMap<String, RdpCertificatePending>>>;

#[derive(Debug, Clone)]
pub struct RdpConnectParams {
    pub session_id: String,
    pub host: String,
    pub port: u16,
    pub domain: String,
    pub username: String,
    pub password: String,
    pub width: u16,
    pub height: u16,
    pub display_mode: String,
    pub color_depth: u8,
    pub certificate_policy: String,
    /// "disabled" | "text-only" | "text-and-files"
    pub clipboard_mode: String,
}

type SharedFrameChannel = Arc<StdMutex<Option<Channel<InvokeResponseBody>>>>;
type SharedClipboardBridge = Arc<StdMutex<Option<Arc<RdpClipboardBridge>>>>;

pub struct RdpHandle {
    session_id: String,
    input_tx: tokio::sync::mpsc::UnboundedSender<RdpInputEvent>,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    need_full_redraw: Arc<AtomicBool>,
    frame_channel: SharedFrameChannel,
    resolution: Arc<StdMutex<(u16, u16)>>,
    pending_certificates: RdpCertificatePendingMap,
    clipboard_bridge: SharedClipboardBridge,
}

impl RdpHandle {
    pub fn send_pointer_by_name(&self, x: u16, y: u16, button: &str, down: bool) -> Result<()> {
        let _ = self.input_tx.send(RdpInputEvent::Pointer {
            x,
            y,
            button: button.to_string(),
            down,
        });
        Ok(())
    }

    pub fn send_key(&self, code: u16, down: bool) -> Result<()> {
        let _ = self.input_tx.send(RdpInputEvent::Key {
            code,
            down,
            extended: false,
        });
        Ok(())
    }

    pub fn send_input_batch(&self, events: Vec<RdpInputEvent>) -> Result<()> {
        for ev in events {
            let _ = self.input_tx.send(ev);
        }
        Ok(())
    }

    pub fn set_streaming(&self, paused: bool) -> Result<()> {
        let was_paused = self.paused.swap(paused, Ordering::Relaxed);
        if was_paused && !paused {
            self.need_full_redraw.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn attach_frame_channel(
        &self,
        channel: Option<Channel<InvokeResponseBody>>,
    ) -> Result<()> {
        let mut guard = self
            .frame_channel
            .lock()
            .map_err(|_| anyhow::anyhow!("frame channel lock is poisoned"))?;
        *guard = channel;
        Ok(())
    }

    pub fn resize(&self, width: u16, height: u16) -> Result<()> {
        if !(RDP_MIN_WIDTH..=RDP_MAX_WIDTH).contains(&width)
            || !(RDP_MIN_HEIGHT..=RDP_MAX_HEIGHT).contains(&height)
        {
            anyhow::bail!("RDP resize 超出支持范围");
        }
        {
            let mut current = self
                .resolution
                .lock()
                .map_err(|_| anyhow::anyhow!("resolution lock is poisoned"))?;
            if *current == (width, height) {
                return Ok(());
            }
            *current = (width, height);
        }
        let _ = self.input_tx.send(RdpInputEvent::Resize { width, height });
        Ok(())
    }

    /// 本地剪贴板文本变化时调用：通知远端有新内容可复制。
    pub fn set_clipboard_text(&self, text: String) -> Result<()> {
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            anyhow::bail!("剪贴板文本过大");
        }
        let guard = self
            .clipboard_bridge
            .lock()
            .map_err(|_| anyhow::anyhow!("clipboard bridge lock is poisoned"))?;
        let Some(bridge) = guard.as_ref() else {
            anyhow::bail!("剪贴板桥接不可用");
        };
        bridge.mark_text_written_from_remote(&text);
        bridge.notify_text_available().map_err(|e| anyhow::anyhow!(e))
    }

    pub fn close(&self) {
        self.cancel.store(true, Ordering::Relaxed);

        // 停掉剪贴板轮询线程
        if let Ok(guard) = self.clipboard_bridge.lock() {
            if let Some(bridge) = guard.as_ref() {
                bridge.stop();
            }
        }

        // 取消本会话所有待决证书请求
        if let Ok(mut map) = self.pending_certificates.try_lock() {
            let ids: Vec<String> = map
                .iter()
                .filter(|(_, p)| p.session_id == self.session_id)
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                if let Some(p) = map.remove(&id) {
                    let _ = p.responder.send(RdpCertificateDecision {
                        accepted: false,
                        remember: false,
                    });
                }
            }
        }
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

enum RdpInputAction {
    Operations(Vec<IronRdpInputOperation>),
    FastPath(IronRdpFastPathInputEvent),
    ReleaseAll,
    Resize { width: u16, height: u16 },
}

fn input_event_to_action(ev: RdpInputEvent) -> RdpInputAction {
    match ev {
        RdpInputEvent::Pointer { x, y, button, down } => {
            let mut ops = vec![IronRdpInputOperation::MouseMove(IronRdpMousePosition { x, y })];
            let mapped = match button.as_str() {
                "left" => Some(IronRdpMouseButton::Left),
                "middle" => Some(IronRdpMouseButton::Middle),
                "right" => Some(IronRdpMouseButton::Right),
                "back" => Some(IronRdpMouseButton::X1),
                "forward" => Some(IronRdpMouseButton::X2),
                _ => None,
            };
            if let Some(btn) = mapped {
                ops.push(if down {
                    IronRdpInputOperation::MouseButtonPressed(btn)
                } else {
                    IronRdpInputOperation::MouseButtonReleased(btn)
                });
            }
            RdpInputAction::Operations(ops)
        }
        RdpInputEvent::Key { code, down, extended } => {
            if !extended && code == RDP_RIGHT_SHIFT_SCAN_CODE {
                let flags = if down {
                    IronRdpKeyboardFlags::empty()
                } else {
                    IronRdpKeyboardFlags::RELEASE
                };
                return RdpInputAction::FastPath(
                    IronRdpFastPathInputEvent::KeyboardEvent(flags, code as u8),
                );
            }
            let scancode = IronRdpScancode::from_u8(extended, code as u8);
            RdpInputAction::Operations(vec![if down {
                IronRdpInputOperation::KeyPressed(scancode)
            } else {
                IronRdpInputOperation::KeyReleased(scancode)
            }])
        }
        RdpInputEvent::MouseWheel { delta_x, delta_y, x, y } => {
            let mut ops = vec![IronRdpInputOperation::MouseMove(IronRdpMousePosition { x, y })];
            if delta_x.abs() > 0.001 {
                ops.push(IronRdpInputOperation::WheelRotations(IronRdpWheelRotations {
                    is_vertical: false,
                    rotation_units: clamp_f64_to_i16(-delta_x),
                }));
            }
            if delta_y.abs() > 0.001 {
                ops.push(IronRdpInputOperation::WheelRotations(IronRdpWheelRotations {
                    is_vertical: true,
                    rotation_units: clamp_f64_to_i16(-delta_y),
                }));
            }
            RdpInputAction::Operations(ops)
        }
        RdpInputEvent::Unicode { text } => {
            let mut ops = Vec::new();
            for ch in text.chars() {
                ops.push(IronRdpInputOperation::UnicodeKeyPressed(ch));
                ops.push(IronRdpInputOperation::UnicodeKeyReleased(ch));
            }
            RdpInputAction::Operations(ops)
        }
        RdpInputEvent::ReleaseAllKeys => RdpInputAction::ReleaseAll,
        RdpInputEvent::Resize { width, height } => RdpInputAction::Resize { width, height },
    }
}

fn clamp_f64_to_i16(value: f64) -> i16 {
    if value.is_nan() {
        return 0;
    }
    value.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

fn client_build() -> u32 {
    env!("CARGO_PKG_VERSION")
        .split('.')
        .take(3)
        .fold(0_u32, |acc, part| {
            acc.saturating_mul(100)
                .saturating_add(part.parse::<u32>().unwrap_or(0))
        })
}

fn client_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "NyaTerm".to_string())
}

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

fn emit_pointer(app: &AppHandle, event: RdpPointerEvent) {
    let _ = app.emit("rdp:pointer", event);
}

fn classify_error<E: std::fmt::Debug>(e: &E) -> RdpErrorKind {
    let msg = format!("{e:?}").to_lowercase();
    if msg.contains("timed out")
        || msg.contains("connection reset")
        || msg.contains("broken pipe")
        || msg.contains("10054")
        || msg.contains("os error 10060")
        || msg.contains("eof")
        || msg.contains("closed")
    {
        RdpErrorKind::Transport
    } else if msg.contains("auth")
        || msg.contains("password")
        || msg.contains("credssp")
        || msg.contains("logon")
    {
        RdpErrorKind::Authentication
    } else {
        RdpErrorKind::Session
    }
}

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

/* ==========================================================================
   证书验证
   ========================================================================== */

#[derive(Clone)]
struct NyaTermRdpCertificateVerifier {
    app: AppHandle,
    session_id: String,
    policy: String,
    pending: RdpCertificatePendingMap,
}

impl std::fmt::Debug for NyaTermRdpCertificateVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NyaTermRdpCertificateVerifier")
            .field("session_id", &self.session_id)
            .field("policy", &self.policy)
            .finish()
    }
}

impl ServerCertificateVerifier for NyaTermRdpCertificateVerifier {
    fn verify_server_certificate<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        der: Vec<u8>,
    ) -> ServerCertificateVerifyFuture<'a> {
        Box::pin(async move { self.verify(host, port, &der).await })
    }
}

impl NyaTermRdpCertificateVerifier {
    async fn verify(&self, host: &str, port: u16, der: &[u8]) -> Result<(), String> {
        let fingerprint = rdp_certificate_fingerprint(der);
        let metadata = parse_rdp_certificate_metadata(der);
        let status = crate::storage::check_rdp_known_host(host, port, &fingerprint)
            .map_err(|e| format!("certificate rejected: {e}"))?;

        if certificate_policy_allows_without_prompt(&self.policy, status)? {
            return Ok(());
        }

        self.prompt_for_certificate(host, port, fingerprint, metadata, status)
            .await
    }

    async fn prompt_for_certificate(
        &self,
        host: &str,
        port: u16,
        fingerprint: String,
        metadata: crate::storage::RdpCertificateMetadata,
        status: crate::storage::KnownHostCheck,
    ) -> Result<(), String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            request_id.clone(),
            RdpCertificatePending {
                session_id: self.session_id.clone(),
                responder: tx,
            },
        );

        emit_state(
            &self.app,
            &self.session_id,
            RdpState::CertificateVerification,
            Some("正在验证 RDP 证书".to_string()),
            None,
        );

        let payload = RdpCertificateVerifyEvent {
            request_id: request_id.clone(),
            session_id: self.session_id.clone(),
            host: host.to_string(),
            port,
            fingerprint: fingerprint.clone(),
            subject: metadata.subject.clone(),
            issuer: metadata.issuer.clone(),
            valid_from: metadata.valid_from.clone(),
            valid_to: metadata.valid_to.clone(),
            known_host_status: match status {
                crate::storage::KnownHostCheck::Match => "match",
                crate::storage::KnownHostCheck::HostSeen => "changed",
                crate::storage::KnownHostCheck::UnknownHost => "unknown",
            }
            .to_string(),
        };
        let _ = self.app.emit("rdp-certificate-verify", payload);

        let decision = match tokio::time::timeout(CERTIFICATE_PROMPT_TIMEOUT, rx).await {
            Ok(Ok(d)) => d,
            _ => {
                self.pending.lock().await.remove(&request_id);
                return Err("certificate rejected".to_string());
            }
        };

        if !decision.accepted {
            return Err("certificate rejected".to_string());
        }
        if decision.remember {
            crate::storage::upsert_rdp_known_host(host, port, &fingerprint, metadata)
                .map_err(|e| format!("certificate rejected: {e}"))?;
        }
        Ok(())
    }
}

fn rdp_certificate_fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    format!("SHA256:{}", hex::encode(digest))
}

fn parse_rdp_certificate_metadata(der: &[u8]) -> crate::storage::RdpCertificateMetadata {
    let Ok(cert) = x509_cert::Certificate::from_der(der) else {
        return crate::storage::RdpCertificateMetadata::default();
    };
    crate::storage::RdpCertificateMetadata {
        subject: Some(cert.tbs_certificate.subject.to_string()),
        issuer: Some(cert.tbs_certificate.issuer.to_string()),
        valid_from: Some(cert.tbs_certificate.validity.not_before.to_string()),
        valid_to: Some(cert.tbs_certificate.validity.not_after.to_string()),
    }
}

fn certificate_policy_allows_without_prompt(
    policy: &str,
    status: crate::storage::KnownHostCheck,
) -> Result<bool, String> {
    use crate::storage::KnownHostCheck;
    match policy {
        "strict" => match status {
            KnownHostCheck::Match => Ok(true),
            KnownHostCheck::UnknownHost => Err("unknown certificate".to_string()),
            KnownHostCheck::HostSeen => Err("certificate fingerprint changed".to_string()),
        },
        "accept-temporarily" => Ok(true),
        _ => match status {
            KnownHostCheck::Match => Ok(true),
            _ => Ok(false),
        },
    }
}

pub async fn respond_certificate(
    pending: &RdpCertificatePendingMap,
    request_id: &str,
    accepted: bool,
    remember: bool,
) -> Result<(), String> {
    let Some(entry) = pending.lock().await.remove(request_id) else {
        return Err(format!("没有待决的证书请求: {request_id}"));
    };
    let _ = entry
        .responder
        .send(RdpCertificateDecision { accepted, remember });
    Ok(())
}

/* ==========================================================================
   连接
   ========================================================================== */

#[allow(clippy::too_many_arguments)]
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
    display_mode: String,
    color_depth: u8,
    certificate_policy: String,
    clipboard_mode: String,
    frame_channel: Channel<InvokeResponseBody>,
    pending_certificates: RdpCertificatePendingMap,
) -> Result<RdpHandle> {
    let shared_channel: SharedFrameChannel = Arc::new(StdMutex::new(Some(frame_channel)));
    let cancel = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let need_full_redraw = Arc::new(AtomicBool::new(false));
    let resolution = Arc::new(StdMutex::new((width, height)));
    let clipboard_bridge: SharedClipboardBridge = Arc::new(StdMutex::new(None));

    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<RdpInputEvent>();

    let sid = session_id.clone();
    let app_out = app.clone();
    let ch = shared_channel.clone();
    let cancel_c = cancel.clone();
    let paused_c = paused.clone();
    let need_full_redraw_c = need_full_redraw.clone();
    let pending_c = pending_certificates.clone();
    let clipboard_c = clipboard_bridge.clone();

    let params = RdpConnectParams {
        session_id: session_id.clone(),
        host: host.clone(),
        port,
        domain,
        username,
        password,
        width,
        height,
        display_mode,
        color_depth,
        certificate_policy,
        clipboard_mode,
    };

    eprintln!(
        "[RDP] 准备连接 {host}:{port} mode={} depth={} cert_policy={}",
        params.display_mode, params.color_depth, params.certificate_policy
    );

    let params_c = params.clone();
    tokio::spawn(async move {
        run_session_loop(
            app_out,
            sid,
            params_c,
            width,
            height,
            ch,
            cancel_c,
            paused_c,
            need_full_redraw_c,
            input_rx,
            pending_c,
            clipboard_c,
        )
        .await;
    });

    Ok(RdpHandle {
        session_id,
        input_tx,
        cancel,
        paused,
        need_full_redraw,
        frame_channel: shared_channel,
        resolution,
        pending_certificates,
        clipboard_bridge,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_session_loop(
    app: AppHandle,
    sid: String,
    params: RdpConnectParams,
    width: u16,
    height: u16,
    frame_channel: SharedFrameChannel,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    need_full_redraw: Arc<AtomicBool>,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<RdpInputEvent>,
    pending_certificates: RdpCertificatePendingMap,
    clipboard_bridge: SharedClipboardBridge,
) {
    let mut attempt: u32 = 0;
    let mut current_w = width;
    let mut current_h = height;

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        emit_state(&app, &sid, RdpState::Connecting, Some("正在连接".to_string()), None);

        let config = match build_config(&params, current_w, current_h, &app, &pending_certificates) {
            Ok(c) => c,
            Err(e) => {
                emit_state(
                    &app,
                    &sid,
                    RdpState::Failed,
                    Some(format!("配置失败: {e}")),
                    Some(RdpErrorKind::Internal),
                );
                let _ = app.emit("sessions-changed", ());
                break;
            }
        };

        let (output_sender, mut output_receiver) =
            ironrdp::client::output_channel::output_channel(2);
        let mut client = IronRdpClient::new(config, output_sender).with_desktop_updates();
        let input_sender = client.input_sender();

        // ===== 剪贴板桥接 =====
        // 按 clipboard_mode 决定是否挂 CLIPRDR 通道：
        // - "disabled"        → 不挂，零开销
        // - "text-only"       → 挂，但 client_capabilities 只声明文本
        // - "text-and-files"  → 挂，声明文件能力
        if params.clipboard_mode != "disabled" {
            let file_enabled = params.clipboard_mode == "text-and-files";
            let bridge = Arc::new(RdpClipboardBridge::new(
                app.clone(),
                sid.clone(),
                file_enabled,
            ));
            client = client.with_cliprdr_backend_factory(Box::new(
                RdpClipboardBackendFactory::from_input_sender(
                    bridge.clone(),
                    input_sender.clone(),
                ),
            ));
            if let Ok(mut guard) = clipboard_bridge.lock() {
                *guard = Some(bridge.clone());
            }
        }
        // =====================

        emit_state(
            &app,
            &sid,
            RdpState::Authenticating,
            Some("正在认证".to_string()),
            None,
        );

        let run_result = std::thread::Builder::new()
            .name(format!("rdp-client-{sid}"))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        eprintln!("[RDP] 无法创建 tokio runtime: {err}");
                        return;
                    }
                };
                runtime.block_on(client.run());
            });

        if let Err(err) = run_result {
            emit_state(
                &app,
                &sid,
                RdpState::Failed,
                Some(format!("无法启动客户端线程: {err}")),
                Some(RdpErrorKind::Internal),
            );
            let _ = app.emit("sessions-changed", ());
            break;
        }

        let mut last_emit = tokio::time::Instant::now() - std::time::Duration::from_secs(1);
        let emit_interval = std::time::Duration::from_millis(50);
        let mut input_db = IronRdpInputDatabase::new();
        let mut framebuffer: Option<FullFramebuffer> = None;
        let mut last_full_refresh: Option<tokio::time::Instant> = None;
        let mut dirty = false;
        let mut frame_queue: VecDeque<Vec<u8>> = VecDeque::with_capacity(MAX_FRAME_QUEUE);
        let mut pending_inputs: Vec<RdpInputEvent> = Vec::new();
        let mut last_input_flush = tokio::time::Instant::now();

        let mut failure: Option<(RdpErrorKind, String)> = None;
        let mut activated = false;

        loop {
            if cancel.load(Ordering::Relaxed) {
                return;
            }

            if need_full_redraw.swap(false, Ordering::Relaxed) {
                last_full_refresh = None;
                dirty = true;
            }

            tokio::select! {
                maybe_input = input_rx.recv() => {
                    let Some(ev) = maybe_input else { return; };
                    if let RdpInputEvent::Resize { width, height } = &ev {
                        current_w = *width;
                        current_h = *height;
                        let _ = input_sender.try_send(IronRdpInputEvent::Resize {
                            width: *width,
                            height: *height,
                            scale_factor: 100,
                            physical_size: None,
                        });
                        framebuffer = None;
                        last_full_refresh = None;
                        dirty = true;
                        continue;
                    }
                    pending_inputs.push(ev);
                    if last_input_flush.elapsed().as_millis() as u64 >= INPUT_BATCH_WINDOW_MS {
                        flush_inputs(&mut pending_inputs, &mut input_db, &input_sender);
                        last_input_flush = tokio::time::Instant::now();
                    }
                }

                maybe_out = output_receiver.recv() => {
                    let Some(out) = maybe_out else {
                        failure = Some((RdpErrorKind::Session, "RDP 输出通道关闭".to_string()));
                        break;
                    };
                    match out {
                        RdpOutputEvent::DesktopUpdate(update) => {
                            let (buffer, desktop_w, desktop_h, region) = update.into_parts();
                            let fb = framebuffer.get_or_insert_with(|| {
                                FullFramebuffer::new(desktop_w.get(), desktop_h.get())
                            });
                            fb.apply_region(&buffer, region);
                            dirty = true;
                            if !activated {
                                activated = true;
                                emit_state(&app, &sid, RdpState::Active, None, None);
                                let _ = app.emit("sessions-changed", ());
                            }
                        }
                        RdpOutputEvent::Image { buffer, width, height } => {
                            let w = width.get();
                            let h = height.get();
                            framebuffer = Some(FullFramebuffer::from_full(buffer, w, h));
                            dirty = true;
                            if !activated {
                                activated = true;
                                emit_state(&app, &sid, RdpState::Active, None, None);
                                let _ = app.emit("sessions-changed", ());
                            }
                        }
                        RdpOutputEvent::Terminated(reason) => {
                            failure = Some((classify_error(&reason), format!("会话终止: {reason:?}")));
                            break;
                        }
                        RdpOutputEvent::ConnectionFailure(err) => {
                            failure = Some((classify_error(&err), format!("连接失败: {err:?}")));
                            break;
                        }
                        RdpOutputEvent::PointerDefault => {
                            emit_pointer(&app, RdpPointerEvent::Default { session_id: sid.clone() });
                        }
                        RdpOutputEvent::PointerHidden => {
                            emit_pointer(&app, RdpPointerEvent::Hidden { session_id: sid.clone() });
                        }
                        RdpOutputEvent::PointerPosition { x, y } => {
                            emit_pointer(&app, RdpPointerEvent::Position { session_id: sid.clone(), x, y });
                        }
                        RdpOutputEvent::PointerBitmap(pointer) => {
                            let png = rgba_to_png(&pointer.bitmap_data, pointer.width, pointer.height);
                            if let Some(png) = png {
                                emit_pointer(&app, RdpPointerEvent::Bitmap {
                                    session_id: sid.clone(),
                                    width: pointer.width,
                                    height: pointer.height,
                                    hotspot_x: pointer.hotspot_x,
                                    hotspot_y: pointer.hotspot_y,
                                    rgba_base64: base64::engine::general_purpose::STANDARD.encode(&png),
                                });
                            }
                        }
                        _ => {}
                    }
                }

                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                    if !pending_inputs.is_empty()
                        && last_input_flush.elapsed().as_millis() as u64 >= INPUT_BATCH_WINDOW_MS
                    {
                        flush_inputs(&mut pending_inputs, &mut input_db, &input_sender);
                        last_input_flush = tokio::time::Instant::now();
                    }

                    if dirty
                        && last_emit.elapsed() >= emit_interval
                        && !paused.load(Ordering::Relaxed)
                    {
                        last_emit = tokio::time::Instant::now();
                        dirty = false;

                        if let Some(fb) = framebuffer.as_ref() {
                            let send_full = last_full_refresh
                                .map(|t| t.elapsed().as_millis() as u64 >= FULL_REFRESH_INTERVAL_MS)
                                .unwrap_or(true);

                            let snapshot = ImageSnapshot {
                                pixels: fb.pixels.clone(),
                                width: fb.width,
                                height: fb.height,
                            };

                            let jpeg_result = tokio::task::spawn_blocking(move || {
                                snapshot.encode_jpeg(50)
                            })
                            .await;

                            match jpeg_result {
                                Ok(Ok(packet)) => {
                                    if frame_queue.len() >= MAX_FRAME_QUEUE {
                                        frame_queue.pop_front();
                                    }
                                    frame_queue.push_back(packet);

                                    if let Ok(guard) = frame_channel.lock() {
                                        if let Some(ch) = guard.as_ref() {
                                            while let Some(frame) = frame_queue.pop_front() {
                                                let _ = ch.send(InvokeResponseBody::Raw(frame));
                                            }
                                        }
                                    }

                                    if send_full {
                                        last_full_refresh = Some(tokio::time::Instant::now());
                                    }
                                }
                                Ok(Err(err)) => eprintln!("[RDP] 帧编码失败: {err:?}"),
                                Err(err) => eprintln!("[RDP] 帧编码任务失败: {err:?}"),
                            }
                        }
                    }
                }
            }
        }

        // 清理：停掉剪贴板，清空 bridge 引用
        if let Ok(mut guard) = clipboard_bridge.lock() {
            if let Some(bridge) = guard.as_ref() {
                bridge.stop();
            }
            *guard = None;
        }

        drop(input_sender);
        framebuffer = None;
        last_full_refresh = None;
        dirty = true;
        frame_queue.clear();

        if cancel.load(Ordering::Relaxed) {
            emit_state(&app, &sid, RdpState::Disconnected, Some("会话已关闭".to_string()), None);
            let _ = app.emit("sessions-changed", ());
            break;
        }

        let Some((failed_kind, failed_message)) = failure else {
            emit_state(&app, &sid, RdpState::Disconnected, Some("会话结束".to_string()), None);
            let _ = app.emit("sessions-changed", ());
            break;
        };

        eprintln!("[RDP] 会话失败（{failed_kind:?}）: {failed_message}");

        if matches!(failed_kind, RdpErrorKind::Authentication | RdpErrorKind::Internal) {
            emit_state(&app, &sid, RdpState::Failed, Some(failed_message), Some(failed_kind));
            let _ = app.emit("sessions-changed", ());
            break;
        }

        attempt += 1;
        if attempt > MAX_RECONNECT_ATTEMPTS {
            emit_state(
                &app,
                &sid,
                RdpState::Failed,
                Some(format!("重连失败: {failed_message}")),
                Some(failed_kind),
            );
            let _ = app.emit("sessions-changed", ());
            break;
        }

        let delay = reconnect_delay(attempt);
        emit_state(
            &app,
            &sid,
            RdpState::Reconnecting,
            Some(format!(
                "第 {attempt}/{MAX_RECONNECT_ATTEMPTS} 次重连，{}ms 后",
                delay.as_millis()
            )),
            None,
        );
        tokio::time::sleep(delay).await;
    }

    let _ = app.emit("rdp:closed", serde_json::json!({ "sessionId": sid }));
}

fn flush_inputs(
    pending: &mut Vec<RdpInputEvent>,
    db: &mut IronRdpInputDatabase,
    sender: &ironrdp::client::rdp::RdpInputSender,
) {
    if pending.is_empty() {
        return;
    }
    let mut ops: Vec<IronRdpInputOperation> = Vec::with_capacity(pending.len());
    let mut release_all = false;

    for ev in pending.drain(..) {
        match input_event_to_action(ev) {
            RdpInputAction::Operations(mut o) => ops.append(&mut o),
            RdpInputAction::FastPath(fp) => {
                if !ops.is_empty() {
                    let fast = db.apply(std::mem::take(&mut ops));
                    if !fast.is_empty() {
                        let _ = sender.try_send(IronRdpInputEvent::FastPath(fast));
                    }
                }
                let _ = sender.try_send(IronRdpInputEvent::FastPath(
                    smallvec::smallvec![fp],
                ));
            }
            RdpInputAction::ReleaseAll => release_all = true,
            RdpInputAction::Resize { .. } => {}
        }
    }

    if !ops.is_empty() {
        let fast = db.apply(ops);
        if !fast.is_empty() {
            let _ = sender.try_send(IronRdpInputEvent::FastPath(fast));
        }
    }
    if release_all {
        let fast = db.release_all();
        if !fast.is_empty() {
            let _ = sender.try_send(IronRdpInputEvent::FastPath(fast));
        }
    }
}

fn rgba_to_png(rgba: &[u8], width: u16, height: u16) -> Option<Vec<u8>> {
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() < expected {
        return None;
    }
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba[..expected].to_vec())?;
    let mut png = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut png),
        image::ImageFormat::Png,
    )
    .ok()?;
    Some(png)
}

fn build_config(
    params: &RdpConnectParams,
    width: u16,
    height: u16,
    app: &AppHandle,
    pending: &RdpCertificatePendingMap,
) -> Result<ironrdp::client::config::Config> {
    let color_depth = match params.color_depth {
        16 => 16u32,
        24 => 24,
        32 => 32,
        _ => 32,
    };

    let verifier = Arc::new(NyaTermRdpCertificateVerifier {
        app: app.clone(),
        session_id: params.session_id.clone(),
        policy: params.certificate_policy.clone(),
        pending: pending.clone(),
    });

    let connector_host = params.host.clone();
    let connector_port = params.port;
    IronRdpConfigBuilder::new()
        .with_destination(IronRdpDestination::from_parts(params.host.clone(), params.port))
        .with_transport(IronRdpTransportKind::Direct)
        .with_username(params.username.clone())
        .with_domain(params.domain.clone())
        .with_password(params.password.clone())
        .with_desktop_width(width)
        .with_desktop_height(height)
        .with_desktop_scale_factor(100)
        .with_color_depth(color_depth)
        .with_credssp(true)
        .with_tls(true)
        .with_platform(MajorPlatformType::WINDOWS)
        .with_client_build(client_build())
        .with_client_dir("C:\\Windows\\System32\\mstscax.dll")
        .with_client_name(client_name())
        .with_server_certificate_verifier(verifier)
        .with_direct_transport_connector(Arc::new(move |_destination| {
            let host = connector_host.clone();
            let port = connector_port;
            Box::pin(async move {
                let stream = TcpStream::connect((host.as_str(), port))
                    .await
                    .map_err(|err| {
                        std::io::Error::new(
                            err.kind(),
                            format!("无法连接 RDP 服务器 {host}:{port}: {err}"),
                        )
                    })?;
                Ok(IronRdpDirectTransport {
                    stream: Box::new(stream),
                    local_addr: None,
                    peer_addr: None,
                })
            })
        }))
        .build()
        .map_err(|e| anyhow::anyhow!("RDP 配置失败: {e:?}"))
}

struct FullFramebuffer {
    pixels: Vec<u32>,
    width: u16,
    height: u16,
}

impl FullFramebuffer {
    fn new(width: u16, height: u16) -> Self {
        let len = (width as usize) * (height as usize);
        Self {
            pixels: vec![0u32; len],
            width,
            height,
        }
    }

    fn from_full(pixels: Vec<u32>, width: u16, height: u16) -> Self {
        Self {
            pixels,
            width,
            height,
        }
    }

    fn apply_region(&mut self, buffer: &[u32], region: InclusiveRectangle) {
        let rx = region.left as usize;
        let ry = region.top as usize;
        let rw = (region.right as usize).saturating_sub(rx).saturating_add(1);
        let rh = (region.bottom as usize).saturating_sub(ry).saturating_add(1);

        let fb_w = self.width as usize;
        let fb_h = self.height as usize;

        let copy_w = rw.min(fb_w.saturating_sub(rx));
        let copy_h = rh.min(fb_h.saturating_sub(ry));
        if copy_w == 0 || copy_h == 0 {
            return;
        }

        for row in 0..copy_h {
            let src_off = row * rw;
            let dst_off = (ry + row) * fb_w + rx;
            let src_row = &buffer[src_off..src_off + copy_w];
            self.pixels[dst_off..dst_off + copy_w].copy_from_slice(src_row);
        }
    }
}

struct ImageSnapshot {
    pixels: Vec<u32>,
    width: u16,
    height: u16,
}

impl ImageSnapshot {
    fn encode_jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        let pixel_count = (self.width as usize) * (self.height as usize);
        if self.pixels.len() < pixel_count {
            anyhow::bail!(
                "像素缓冲区过小: 期望 {pixel_count}, 实际 {}",
                self.pixels.len()
            );
        }

        let mut rgb = Vec::with_capacity(pixel_count * 3);
        for &p in self.pixels.iter().take(pixel_count) {
            let [_, r, g, b] = p.to_be_bytes();
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }

        let mut jpeg = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, quality);
        encoder.encode(
            &rgb,
            self.width as u32,
            self.height as u32,
            image::ExtendedColorType::Rgb8,
        )?;
        Ok(pack_frame(0, 0, self.width, self.height, &jpeg))
    }
}