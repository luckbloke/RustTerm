//! RDP 剪贴板重定向（文本 + 文件双向）。
//!
//! 实现 `CliprdrBackend`，负责：
//! - 本地剪贴板变化时通知远端（轮询）
//! - 响应远端对文本/文件的请求
//! - 接收远端推来的文本/文件，落到缓存目录并写回本地剪贴板
//!
//! 与 NyaTerm 主项目的差异：
//! - 不使用 `TransferController`（本项目没有这套），进度直接发 `transfer:progress` 事件
//! - 不用 `ChunkedFetch`（它把整个文件读到内存），改为流式写盘

use ironrdp::cliprdr::backend::{
    ClipboardMessage, ClipboardMessageProxy, CliprdrBackend, CliprdrBackendFactory,
};
use ironrdp::cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardFormatName,
    ClipboardGeneralCapabilityFlags, FileContentsFlags, FileContentsRequest,
    FileContentsResponse, FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse, PackedFileList, FORMAT_NAME_FILE_LIST,
};
use ironrdp::core::impl_as_any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter};

/// 剪贴板文本大小上限（16MB）。
const MAX_CLIPBOARD_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// 单次文件分块大小（1MB）。
const FILE_CHUNK_SIZE: u32 = 1024 * 1024;
/// 一次剪贴板文件列表最多多少个文件。
const MAX_FILE_COUNT: usize = 100_000;
/// 单个文件大小上限（2GB）。
const MAX_SINGLE_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;
/// 一次剪贴板操作所有文件总大小上限（4GB）。
const MAX_TOTAL_SIZE: u64 = 4 * 1024 * 1024 * 1024;
/// 轮询本地剪贴板的间隔。
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(750);
/// 远端文件传输的超时时间。
const REMOTE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
/// 缓存目录保留时长。
const CACHE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

// ============================================================================
// 数据结构
// ============================================================================

/// 本地文件列表快照中的一个文件。
#[derive(Debug)]
struct LocalFileEntry {
    path: PathBuf,
    size: u64,
    is_directory: bool,
}

/// 本地剪贴板文件列表快照。
#[derive(Debug)]
struct LocalFileSnapshot {
    descriptors: Vec<FileDescriptor>,
    entries: Vec<LocalFileEntry>,
}

/// 待从远端下载的文件。
#[derive(Debug)]
struct RemoteFileSpec {
    descriptor_index: i32,
    path: PathBuf,
    expected_size: Option<u64>,
}

/// 单个正在下载的文件。
#[derive(Debug)]
struct ActiveRemoteFile {
    spec: RemoteFileSpec,
    file: File,
    offset: u64,
    pending: PendingResponse,
}

/// 当前等待远端响应的类型。
#[derive(Debug, Clone, Copy)]
enum PendingResponse {
    Size,
    Range { requested: u32 },
}

/// 一次完整的远端→本地传输。
#[derive(Debug)]
struct RemoteGeneration {
    root: PathBuf,
    clipboard_paths: Vec<PathBuf>,
    pending_files: VecDeque<RemoteFileSpec>,
    active_files: HashMap<u32, ActiveRemoteFile>,
    clip_data_id: Option<u32>,
    total_size: u64,
    transferred: u64,
    file_count: u64,
    completed_files: u64,
    last_activity: Instant,
}

/// 已完成的传输，保留缓存目录供用户粘贴。
#[derive(Debug)]
struct CompletedGeneration {
    root: PathBuf,
    file_list_hash: u64,
}

// ============================================================================
// 桥接核心
// ============================================================================

pub(crate) struct RdpClipboardBridge {
    app: AppHandle,
    pub(crate) session_id: String,
    /// 是否允许文件剪贴板（前端配置）。
    file_enabled: bool,
    /// 协商后远端是否支持文件。
    file_transfer_available: AtomicBool,
    shutdown: AtomicBool,
    watcher_started: AtomicBool,
    proxy: Mutex<Option<Arc<Mutex<Box<dyn ClipboardMessageProxy>>>>>,
    /// 上次本地剪贴板文本的哈希，用于回环抑制。
    last_text_hash: Mutex<Option<u64>>,
    /// 上次本地文件列表的哈希。
    last_file_hash: Mutex<Option<u64>>,
    /// 当前本地文件快照，远端请求文件内容时从这里读。
    current_snapshot: Mutex<Option<Arc<LocalFileSnapshot>>>,
    /// 被远端 lock 的快照（data_id → snapshot）。
    locked_snapshots: Mutex<HashMap<u32, Arc<LocalFileSnapshot>>>,
    /// 正在进行的远端→本地传输。
    remote_generation: Mutex<Option<RemoteGeneration>>,
    /// 已完成的远端→本地传输，缓存目录保留。
    completed_generation: Mutex<Option<CompletedGeneration>>,
    /// 下一个 stream_id（每个文件内容请求用）。
    next_stream_id: AtomicU32,
}

impl std::fmt::Debug for RdpClipboardBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdpClipboardBridge")
            .field("session_id", &self.session_id)
            .field("file_enabled", &self.file_enabled)
            .field("shutdown", &self.shutdown.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl RdpClipboardBridge {
    pub(crate) fn new(app: AppHandle, session_id: String, file_enabled: bool) -> Self {
        let cache_root = clipboard_cache_root();
        let _ = cleanup_stale_cache(&cache_root, CACHE_RETENTION);
        Self {
            app,
            session_id,
            file_enabled,
            file_transfer_available: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            watcher_started: AtomicBool::new(false),
            proxy: Mutex::new(None),
            last_text_hash: Mutex::new(None),
            last_file_hash: Mutex::new(None),
            current_snapshot: Mutex::new(None),
            locked_snapshots: Mutex::new(HashMap::new()),
            remote_generation: Mutex::new(None),
            completed_generation: Mutex::new(None),
            next_stream_id: AtomicU32::new(1),
        }
    }

    fn set_proxy(&self, proxy: Arc<Mutex<Box<dyn ClipboardMessageProxy>>>) {
        if let Ok(mut current) = self.proxy.lock() {
            *current = Some(proxy);
        }
    }

    fn send(&self, message: ClipboardMessage) {
        let proxy = self.proxy.lock().ok().and_then(|p| p.clone());
        if let Some(proxy) = proxy {
            if let Ok(proxy) = proxy.lock() {
                proxy.send_clipboard_message(message);
            }
        }
    }

    /// 主动通知远端本地剪贴板有文本可用。
    pub(crate) fn notify_text_available(&self) -> Result<(), String> {
        if read_clipboard_text_blocking()
            .filter(|t| !t.is_empty() && clipboard_text_within_limit(t))
            .is_none()
        {
            return Ok(());
        }
        self.send(ClipboardMessage::SendInitiateCopy(vec![
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
        ]));
        Ok(())
    }

    /// 前端调用：本地剪贴板文本变化时通知远端。
    pub(crate) fn mark_text_written_from_remote(&self, text: &str) {
        if let Ok(mut last) = self.last_text_hash.lock() {
            *last = Some(stable_hash(text));
        }
    }

    pub(crate) fn start_watcher(self: &Arc<Self>) {
        if self.watcher_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let bridge = self.clone();
        std::thread::spawn(move || {
            while !bridge.shutdown.load(Ordering::SeqCst) {
                bridge.poll_local_clipboard();
                bridge.expire_remote_generation();
                std::thread::sleep(CLIPBOARD_POLL_INTERVAL);
            }
        });
    }

    fn poll_local_clipboard(&self) {
        // 优先检查文件
        if self.file_transfer_available.load(Ordering::SeqCst) {
            if let Some(paths) = read_clipboard_file_list_blocking() {
                if !paths.is_empty() {
                    let hash = file_list_fingerprint(&paths);
                    let published_by_remote = self
                        .completed_generation
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().map(|g| g.file_list_hash))
                        == Some(hash);
                    if published_by_remote {
                        if let Ok(mut last) = self.last_file_hash.lock() {
                            *last = Some(hash);
                        }
                        return;
                    }
                    self.release_completed_generation();
                    let changed = self
                        .last_file_hash
                        .lock()
                        .map(|mut last| {
                            let changed = *last != Some(hash);
                            *last = Some(hash);
                            changed
                        })
                        .unwrap_or(false);
                    if changed {
                        match build_local_snapshot(&paths) {
                            Ok(snapshot) => {
                                let snapshot = Arc::new(snapshot);
                                if let Ok(mut cur) = self.current_snapshot.lock() {
                                    *cur = Some(snapshot.clone());
                                }
                                self.send(ClipboardMessage::SendInitiateFileCopy(
                                    snapshot.descriptors.clone(),
                                ));
                            }
                            Err(e) => tracing::warn!(
                                session_id = %self.session_id,
                                error = %e,
                                "忽略不安全的本地剪贴板文件列表"
                            ),
                        }
                    }
                    return;
                }
            }
        }

        self.release_completed_generation();
        // 无文件：检查文本
        if let Some(text) = read_clipboard_text_blocking() {
            if !clipboard_text_within_limit(&text) {
                return;
            }
            let hash = stable_hash(&text);
            let changed = self
                .last_text_hash
                .lock()
                .map(|mut last| {
                    let changed = *last != Some(hash);
                    *last = Some(hash);
                    changed
                })
                .unwrap_or(false);
            if changed {
                self.send(ClipboardMessage::SendInitiateCopy(vec![
                    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
                ]));
            }
        }
    }

    pub(crate) fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.cancel_remote_generation("RDP 剪贴板断开", true);
        if let Ok(mut locks) = self.locked_snapshots.lock() {
            locks.clear();
        }
    }

    // --- 远端→本地 ---

    fn begin_remote_generation(
        &self,
        files: &[FileDescriptor],
        clip_data_id: Option<u32>,
    ) -> Result<Vec<FileContentsRequest>, String> {
        self.cancel_remote_generation("远端剪贴板变化", true);
        let generation_id = uuid::Uuid::new_v4().to_string();
        let root = clipboard_cache_root().join(&generation_id);
        let prepared = prepare_remote_files(files, &root)?;

        if let Err(e) = fs::create_dir_all(&root) {
            let _ = fs::remove_dir_all(&root);
            return Err(format!("创建剪贴板缓存目录失败: {e}"));
        }
        for dir in &prepared.directories {
            if let Err(e) = fs::create_dir_all(dir) {
                let _ = fs::remove_dir_all(&root);
                return Err(format!("创建剪贴板目录失败: {e}"));
            }
        }

        let file_count = u64::try_from(prepared.files.len()).unwrap_or(u64::MAX);
        let total_size = prepared.files.iter().filter_map(|f| f.expected_size).sum();

        let mut generation = RemoteGeneration {
            root,
            clipboard_paths: prepared.clipboard_paths,
            pending_files: prepared.files.into(),
            active_files: HashMap::new(),
            clip_data_id,
            total_size,
            transferred: 0,
            file_count,
            completed_files: 0,
            last_activity: Instant::now(),
        };

        let requests = match self.pump_remote_generation(&mut generation) {
            Ok(reqs) => reqs,
            Err(e) => {
                self.fail_remote_generation(generation, &e);
                return Err(e);
            }
        };

        if generation.pending_files.is_empty() && generation.active_files.is_empty() {
            self.complete_remote_generation(generation)?;
        } else if let Ok(mut cur) = self.remote_generation.lock() {
            *cur = Some(generation);
        } else {
            let e = "RDP 剪贴板 generation 锁被污染".to_string();
            self.fail_remote_generation(generation, &e);
            return Err(e);
        }
        Ok(requests)
    }

    fn pump_remote_generation(
        &self,
        generation: &mut RemoteGeneration,
    ) -> Result<Vec<FileContentsRequest>, String> {
        let mut requests = Vec::new();
        // 最多 2 个并发下载
        while generation.active_files.len() < 2 {
            let Some(spec) = generation.pending_files.pop_front() else {
                break;
            };
            if let Some(parent) = spec.path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("创建剪贴板父目录失败: {e}"))?;
            }
            let file = File::create(&spec.path)
                .map_err(|e| format!("创建剪贴板文件失败: {e}"))?;

            if spec.expected_size == Some(0) {
                generation.completed_files = generation.completed_files.saturating_add(1);
                continue;
            }

            let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
            let (pending, request) = match spec.expected_size {
                Some(size) => {
                    let requested = u32::try_from(size.min(u64::from(FILE_CHUNK_SIZE)))
                        .unwrap_or(FILE_CHUNK_SIZE);
                    (
                        PendingResponse::Range { requested },
                        FileContentsRequest {
                            stream_id,
                            index: spec.descriptor_index,
                            flags: FileContentsFlags::RANGE,
                            position: 0,
                            requested_size: requested,
                            data_id: generation.clip_data_id,
                        },
                    )
                }
                None => (
                    PendingResponse::Size,
                    FileContentsRequest {
                        stream_id,
                        index: spec.descriptor_index,
                        flags: FileContentsFlags::SIZE,
                        position: 0,
                        requested_size: 8,
                        data_id: generation.clip_data_id,
                    },
                ),
            };
            generation.active_files.insert(
                stream_id,
                ActiveRemoteFile {
                    spec,
                    file,
                    offset: 0,
                    pending,
                },
            );
            requests.push(request);
        }
        Ok(requests)
    }

    fn handle_remote_response(
        &self,
        response: &FileContentsResponse<'_>,
    ) -> Result<Vec<FileContentsRequest>, String> {
        let mut cur = self
            .remote_generation
            .lock()
            .map_err(|_| "RDP 剪贴板 generation 锁被污染".to_string())?;
        let Some(mut generation) = cur.take() else {
            return Ok(Vec::new());
        };
        generation.last_activity = Instant::now();

        let result = (|| -> Result<Vec<FileContentsRequest>, String> {
            if response.is_error() {
                return Err("远端拒绝了剪贴板文件请求".to_string());
            }
            let stream_id = response.stream_id();
            let mut active = generation
                .active_files
                .remove(&stream_id)
                .ok_or_else(|| "收到未知的剪贴板流响应".to_string())?;

            match active.pending {
                PendingResponse::Size => {
                    let size = response
                        .data_as_size()
                        .map_err(|e| format!("剪贴板文件大小响应无效: {e}"))?;
                    if size > MAX_SINGLE_FILE_SIZE {
                        return Err(format!("远端文件超过单文件上限: {size} 字节"));
                    }
                    active.spec.expected_size = Some(size);
                    generation.total_size = generation.total_size.saturating_add(size);
                    if generation.total_size > MAX_TOTAL_SIZE {
                        return Err("远端剪贴板总大小超过上限".to_string());
                    }
                }
                PendingResponse::Range { requested } => {
                    let data = response.data();
                    let expected = active
                        .spec
                        .expected_size
                        .ok_or_else(|| "剪贴板文件大小未知".to_string())?;
                    if data.is_empty()
                        || data.len() > usize::try_from(requested).unwrap_or(usize::MAX)
                        || active.offset.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX))
                            > expected
                    {
                        return Err("远端返回了无效的剪贴板文件范围".to_string());
                    }
                    active
                        .file
                        .write_all(data)
                        .map_err(|e| format!("写入剪贴板缓存失败: {e}"))?;
                    let written = u64::try_from(data.len()).unwrap_or(0);
                    active.offset = active.offset.saturating_add(written);
                    generation.transferred = generation.transferred.saturating_add(written);

                    // 发进度
                    let _ = self.app.emit(
                        "transfer:progress",
                        serde_json::json!({
                            "sessionId": self.session_id,
                            "sent": generation.transferred,
                            "total": generation.total_size,
                            "label": "RDP 剪贴板",
                        }),
                    );
                }
            }

            let expected = active.spec.expected_size.unwrap_or(0);
            let mut requests = Vec::new();
            if active.offset >= expected {
                active
                    .file
                    .flush()
                    .map_err(|e| format!("刷新剪贴板缓存失败: {e}"))?;
                generation.completed_files = generation.completed_files.saturating_add(1);
            } else {
                let remaining = expected.saturating_sub(active.offset);
                let requested = u32::try_from(remaining.min(u64::from(FILE_CHUNK_SIZE)))
                    .unwrap_or(FILE_CHUNK_SIZE);
                active.pending = PendingResponse::Range { requested };
                requests.push(FileContentsRequest {
                    stream_id,
                    index: active.spec.descriptor_index,
                    flags: FileContentsFlags::RANGE,
                    position: active.offset,
                    requested_size: requested,
                    data_id: generation.clip_data_id,
                });
                generation.active_files.insert(stream_id, active);
            }
            requests.extend(self.pump_remote_generation(&mut generation)?);
            Ok(requests)
        })();

        match result {
            Err(e) => {
                drop(cur);
                self.fail_remote_generation(generation, &e);
                Err(e)
            }
            Ok(requests) => {
                if generation.pending_files.is_empty() && generation.active_files.is_empty() {
                    drop(cur);
                    self.complete_remote_generation(generation)?;
                } else {
                    *cur = Some(generation);
                }
                Ok(requests)
            }
        }
    }

    fn complete_remote_generation(&self, generation: RemoteGeneration) -> Result<(), String> {
        if let Err(e) = write_clipboard_file_list_blocking(&generation.clipboard_paths) {
            self.fail_remote_generation(generation, &e);
            return Err(e);
        }
        let hash = file_list_fingerprint(&generation.clipboard_paths);
        if let Ok(mut last) = self.last_file_hash.lock() {
            *last = Some(hash);
        }
        self.release_completed_generation();
        if let Ok(mut done) = self.completed_generation.lock() {
            *done = Some(CompletedGeneration {
                root: generation.root.clone(),
                file_list_hash: hash,
            });
        }
        let _ = self.app.emit(
            "transfer:done",
            serde_json::json!({
                "sessionId": self.session_id,
                "label": "RDP 剪贴板",
            }),
        );
        Ok(())
    }

    fn fail_remote_generation(&self, generation: RemoteGeneration, error: &str) {
        cleanup_generation(&generation);
        let _ = self.app.emit(
            "transfer:error",
            serde_json::json!({
                "sessionId": self.session_id,
                "label": "RDP 剪贴板",
                "error": error,
            }),
        );
    }

    fn cancel_remote_generation(&self, reason: &str, emit: bool) {
        let generation = self
            .remote_generation
            .lock()
            .ok()
            .and_then(|mut g| g.take());
        if let Some(generation) = generation {
            cleanup_generation(&generation);
            if emit {
                let _ = self.app.emit(
                    "transfer:cancelled",
                    serde_json::json!({
                        "sessionId": self.session_id,
                        "label": "RDP 剪贴板",
                        "reason": reason,
                    }),
                );
            }
        }
    }

    fn expire_remote_generation(&self) {
        let expired = self
            .remote_generation
            .lock()
            .ok()
            .and_then(|g| {
                g.as_ref()
                    .map(|g| g.last_activity.elapsed() >= REMOTE_TRANSFER_TIMEOUT)
            })
            .unwrap_or(false);
        if expired {
            self.cancel_remote_generation("RDP 剪贴板传输超时", true);
        }
    }

    fn release_completed_generation(&self) {
        let completed = self
            .completed_generation
            .lock()
            .ok()
            .and_then(|mut g| g.take());
        if let Some(c) = completed {
            let _ = fs::remove_dir_all(c.root);
        }
    }

    // --- 本地→远端 ---

    fn snapshot_for_request(&self, request: &FileContentsRequest) -> Option<Arc<LocalFileSnapshot>> {
        if let Some(data_id) = request.data_id {
            return self
                .locked_snapshots
                .lock()
                .ok()
                .and_then(|m| m.get(&data_id).cloned());
        }
        self.current_snapshot.lock().ok().and_then(|s| s.clone())
    }

    fn lock_snapshot(&self, data_id: LockDataId) {
        let cur = self
            .current_snapshot
            .lock()
            .ok()
            .and_then(|s| s.clone());
        if let Some(s) = cur {
            if let Ok(mut locks) = self.locked_snapshots.lock() {
                locks.insert(data_id.0, s);
            }
        }
    }

    fn unlock_snapshot(&self, data_id: LockDataId) {
        if let Ok(mut locks) = self.locked_snapshots.lock() {
            locks.remove(&data_id.0);
        }
    }

    fn cancel_for_locks(&self, locks: &[LockDataId]) {
        let should_cancel = self
            .remote_generation
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|g| g.clip_data_id))
            .is_some_and(|id| locks.iter().any(|l| l.0 == id));
        if should_cancel {
            self.cancel_remote_generation("RDP 剪贴板锁过期", true);
        }
    }
}

// ============================================================================
// 工厂 + 代理 + 后端
// ============================================================================

pub(crate) struct RdpClipboardBackendFactory {
    bridge: Arc<RdpClipboardBridge>,
    proxy: Arc<Mutex<Box<dyn ClipboardMessageProxy>>>,
}

impl RdpClipboardBackendFactory {
    pub(crate) fn new(
        bridge: Arc<RdpClipboardBridge>,
        proxy: Box<dyn ClipboardMessageProxy>,
    ) -> Self {
        let proxy = Arc::new(Mutex::new(proxy));
        bridge.set_proxy(proxy.clone());
        Self { bridge, proxy }
    }

    pub(crate) fn from_input_sender(
        bridge: Arc<RdpClipboardBridge>,
        input_sender: ironrdp::client::rdp::RdpInputSender,
    ) -> Self {
        Self::new(bridge, Box::new(RdpClipboardMessageProxy { input_sender }))
    }
}

#[derive(Debug)]
struct RdpClipboardMessageProxy {
    input_sender: ironrdp::client::rdp::RdpInputSender,
}

impl ClipboardMessageProxy for RdpClipboardMessageProxy {
    fn send_clipboard_message(&self, message: ClipboardMessage) {
        if self.input_sender.send_clipboard(message).is_err() {
            tracing::debug!("RDP 剪贴板通道已关闭");
        }
    }
}

impl CliprdrBackendFactory for RdpClipboardBackendFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(RdpClipboardBackend {
            bridge: self.bridge.clone(),
            proxy: self.proxy.clone(),
            negotiated: ClipboardGeneralCapabilityFlags::empty(),
        })
    }
}

struct RdpClipboardBackend {
    bridge: Arc<RdpClipboardBridge>,
    proxy: Arc<Mutex<Box<dyn ClipboardMessageProxy>>>,
    negotiated: ClipboardGeneralCapabilityFlags,
}

impl std::fmt::Debug for RdpClipboardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdpClipboardBackend")
            .field("session_id", &self.bridge.session_id)
            .field("negotiated", &self.negotiated)
            .finish()
    }
}

impl_as_any!(RdpClipboardBackend);

impl RdpClipboardBackend {
    fn send(&self, message: ClipboardMessage) {
        if let Ok(proxy) = self.proxy.lock() {
            proxy.send_clipboard_message(message);
        }
    }
}

impl CliprdrBackend for RdpClipboardBackend {
    fn temporary_directory(&self) -> &str {
        ".nyaterm-rdp-cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        let mut caps = ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES;
        if self.bridge.file_enabled {
            caps |= ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
                | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA;
        }
        caps
    }

    fn on_ready(&mut self) {
        let _ = self.bridge.notify_text_available();
        self.bridge.start_watcher();
    }

    fn on_request_format_list(&mut self) {
        // 优先文件；没有文件再发文本
        let has_files = self.bridge.file_enabled
            && read_clipboard_file_list_blocking()
                .is_some_and(|p| !p.is_empty());
        if has_files {
            self.send(ClipboardMessage::SendInitiateCopy(vec![
                ClipboardFormat::new(ClipboardFormatId::CF_HDROP)
                    .with_name(ClipboardFormatName::FILE_LIST),
            ]));
        } else if read_clipboard_text_blocking()
            .is_some_and(|t| !t.is_empty() && clipboard_text_within_limit(&t))
        {
            self.send(ClipboardMessage::SendInitiateCopy(vec![
                ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            ]));
        } else {
            self.send(ClipboardMessage::SendInitiateCopy(Vec::new()));
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        self.negotiated = capabilities;
        self.bridge.file_transfer_available.store(
            self.bridge.file_enabled
                && capabilities.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED),
            Ordering::SeqCst,
        );
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // 远端复制了内容：如果包含文件格式就请求文件列表，否则请求文本
        if self.bridge.file_transfer_available.load(Ordering::SeqCst) {
            if let Some(file_fmt) = available_formats.iter().find(|f| {
                f.name()
                    .is_some_and(|n| n.value() == FORMAT_NAME_FILE_LIST)
            }) {
                self.send(ClipboardMessage::SendInitiatePaste(file_fmt.id()));
                return;
            }
        }
        if available_formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT)
        {
            self.send(ClipboardMessage::SendInitiatePaste(
                ClipboardFormatId::CF_UNICODETEXT,
            ));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // 只支持文本
        if request.format != ClipboardFormatId::CF_UNICODETEXT {
            self.send(ClipboardMessage::SendFormatData(
                OwnedFormatDataResponse::new_error(),
            ));
            return;
        }
        let response = match read_clipboard_text_blocking() {
            Some(text) if clipboard_text_within_limit(&text) => {
                OwnedFormatDataResponse::new_unicode_string(&text)
            }
            _ => OwnedFormatDataResponse::new_error(),
        };
        self.send(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            return;
        }
        let Ok(text) = response.to_unicode_string() else {
            return;
        };
        if !clipboard_text_within_limit(&text) {
            tracing::warn!(session_id = %self.bridge.session_id, "忽略过大的 RDP 剪贴板文本");
            return;
        }
        self.bridge.mark_text_written_from_remote(&text);
        std::thread::spawn(move || {
            let _ = write_clipboard_text_blocking(text);
        });
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let response = serve_local_file_request(&self.bridge, &request)
            .unwrap_or_else(|_| FileContentsResponse::new_error(request.stream_id));
        self.send(ClipboardMessage::SendFileContentsResponse(response));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        match self.bridge.handle_remote_response(&response) {
            Ok(requests) => {
                for req in requests {
                    self.send(ClipboardMessage::SendFileContentsRequest(req));
                }
            }
            Err(e) => tracing::warn!(
                session_id = %self.bridge.session_id,
                error = %e,
                "RDP 剪贴板文件下载失败"
            ),
        }
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        self.bridge.lock_snapshot(data_id);
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        self.bridge.unlock_snapshot(data_id);
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        match self.bridge.begin_remote_generation(files, clip_data_id) {
            Ok(requests) => {
                for req in requests {
                    self.send(ClipboardMessage::SendFileContentsRequest(req));
                }
            }
            Err(e) => tracing::warn!(
                session_id = %self.bridge.session_id,
                error = %e,
                "拒绝 RDP 剪贴板文件列表"
            ),
        }
    }

    fn on_outgoing_locks_expired(&mut self, clip_data_ids: &[LockDataId]) {
        self.bridge.cancel_for_locks(clip_data_ids);
    }

    fn on_outgoing_locks_cleared(&mut self, clip_data_ids: &[LockDataId]) {
        self.bridge.cancel_for_locks(clip_data_ids);
    }
}

// ============================================================================
// 远端文件列表准备
// ============================================================================

struct PreparedRemoteFiles {
    directories: Vec<PathBuf>,
    files: Vec<RemoteFileSpec>,
    clipboard_paths: Vec<PathBuf>,
}

fn prepare_remote_files(files: &[FileDescriptor], root: &Path) -> Result<PreparedRemoteFiles, String> {
    if files.is_empty() || files.len() > MAX_FILE_COUNT {
        return Err("远端剪贴板文件数无效".to_string());
    }
    let mut seen = HashSet::new();
    let mut directories = Vec::new();
    let mut regular = Vec::new();
    let mut top_level = HashSet::new();

    for (index, descriptor) in files.iter().enumerate() {
        let relative = descriptor_relative_path(descriptor)?;
        let key = relative.to_string_lossy().to_lowercase();
        if !seen.insert(key) {
            return Err("远端剪贴板包含重复路径".to_string());
        }
        if let Some(Component::Normal(c)) = relative.components().next() {
            top_level.insert(c.to_os_string());
        }
        let target = root.join(&relative);
        let is_dir = descriptor
            .attributes
            .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY));
        if is_dir {
            directories.push(target);
        } else {
            regular.push(RemoteFileSpec {
                descriptor_index: i32::try_from(index)
                    .map_err(|_| "远端剪贴板索引过大".to_string())?,
                path: target,
                expected_size: descriptor.file_size,
            });
        }
    }
    directories.sort_by_key(|p| p.components().count());
    let mut top_level = top_level.into_iter().collect::<Vec<_>>();
    top_level.sort();
    let clipboard_paths = top_level.iter().map(|n| root.join(n)).collect::<Vec<_>>();

    Ok(PreparedRemoteFiles {
        directories,
        files: regular,
        clipboard_paths,
    })
}

fn descriptor_relative_path(descriptor: &FileDescriptor) -> Result<PathBuf, String> {
    let mut result = PathBuf::new();
    let mut parts = Vec::new();
    if let Some(parent) = descriptor.relative_path.as_deref() {
        parts.extend(parent.split(['/', '\\']));
    }
    parts.push(&descriptor.name);
    for part in parts {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\0')
            || part.contains(':')
            || part.ends_with([' ', '.'])
            || is_windows_device_name(part)
        {
            return Err(format!("远端剪贴板路径组件不安全: {part}"));
        }
        result.push(part);
    }
    if result.is_absolute()
        || result
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("远端剪贴板路径不是相对路径".to_string());
    }
    Ok(result)
}

/// 检测 Windows 保留设备名（CON/PRN/AUX/NUL/COM1..9/LPT1..9）。
fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit()
        && stem.as_bytes()[3] != b'0')
}

// ============================================================================
// 本地文件快照
// ============================================================================

fn build_local_snapshot(paths: &[PathBuf]) -> Result<LocalFileSnapshot, String> {
    if paths.is_empty() || paths.len() > MAX_FILE_COUNT {
        return Err("本地剪贴板文件数无效".to_string());
    }
    let mut descriptors = Vec::new();
    let mut entries = Vec::new();
    let mut names = HashSet::new();
    for selected in paths {
        let meta = fs::symlink_metadata(selected)
            .map_err(|e| format!("检查剪贴板路径失败: {e}"))?;
        if meta.file_type().is_symlink() {
            return Err("RDP 剪贴板不暴露符号链接".to_string());
        }
        let canonical = selected
            .canonicalize()
            .map_err(|e| format!("解析剪贴板路径失败: {e}"))?;
        let name = selected
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "剪贴板路径没有有效文件名".to_string())?;
        if !names.insert(name.to_lowercase()) {
            return Err("本地剪贴板包含重复根名称".to_string());
        }
        append_local_entry(&canonical, Path::new(name), &mut descriptors, &mut entries)?;
    }
    Ok(LocalFileSnapshot { descriptors, entries })
}

fn append_local_entry(
    path: &Path,
    relative: &Path,
    descriptors: &mut Vec<FileDescriptor>,
    entries: &mut Vec<LocalFileEntry>,
) -> Result<(), String> {
    if descriptors.len() >= MAX_FILE_COUNT {
        return Err("本地剪贴板文件过多".to_string());
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|e| format!("检查剪贴板文件失败: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("RDP 剪贴板不暴露符号链接".to_string());
    }
    let meta = fs::metadata(path).map_err(|e| format!("检查剪贴板文件失败: {e}"))?;
    if !meta.is_file() && !meta.is_dir() {
        return Err("RDP 剪贴板不暴露特殊文件".to_string());
    }
    let name = relative
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "剪贴板文件名非 UTF-8".to_string())?;
    let parent = relative
        .parent()
        .filter(|p| !p.as_os_str().is_empty());
    let parent_wire = parent.map(path_to_wire).transpose()?;

    let is_dir = meta.is_dir();
    let size = if is_dir { 0 } else { meta.len() };
    let mut d = FileDescriptor::new(name).with_attributes(if is_dir {
        ClipboardFileAttributes::DIRECTORY
    } else {
        ClipboardFileAttributes::NORMAL | ClipboardFileAttributes::ARCHIVE
    });
    if !is_dir {
        d = d.with_file_size(size);
    }
    if let Some(p) = parent_wire {
        d = d.with_relative_path(p);
    }
    descriptors.push(d);
    entries.push(LocalFileEntry {
        path: path.to_path_buf(),
        size,
        is_directory: is_dir,
    });

    if is_dir {
        let mut children = fs::read_dir(path)
            .map_err(|e| format!("读取剪贴板目录失败: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取剪贴板目录失败: {e}"))?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            append_local_entry(
                &child.path(),
                &relative.join(child.file_name()),
                descriptors,
                entries,
            )?;
        }
    }
    Ok(())
}

fn path_to_wire(path: &Path) -> Result<String, String> {
    path.components()
        .map(|c| match c {
            Component::Normal(v) => v
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| "剪贴板路径非 UTF-8".to_string()),
            _ => Err("剪贴板路径不是相对路径".to_string()),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("\\"))
}

// ============================================================================
// 本地文件请求服务
// ============================================================================

fn serve_local_file_request(
    bridge: &RdpClipboardBridge,
    request: &FileContentsRequest,
) -> Result<FileContentsResponse<'static>, String> {
    request.flags.validate().map_err(str::to_string)?;
    let snapshot = bridge
        .snapshot_for_request(request)
        .ok_or_else(|| "剪贴板快照已失效".to_string())?;
    serve_snapshot_file_request(&snapshot, request)
}

fn serve_snapshot_file_request(
    snapshot: &LocalFileSnapshot,
    request: &FileContentsRequest,
) -> Result<FileContentsResponse<'static>, String> {
    let index = usize::try_from(request.index).map_err(|_| "文件索引无效".to_string())?;
    let entry = snapshot
        .entries
        .get(index)
        .ok_or_else(|| "文件索引超出剪贴板快照".to_string())?;

    if request.flags.contains(FileContentsFlags::SIZE) {
        if request.position != 0 || request.requested_size != 8 {
            return Err("剪贴板 SIZE 请求无效".to_string());
        }
        return Ok(FileContentsResponse::new_size_response(
            request.stream_id,
            entry.size,
        ));
    }

    if entry.is_directory
        || request.requested_size == 0
        || request.requested_size > FILE_CHUNK_SIZE
        || request.position > entry.size
    {
        return Err("剪贴板 RANGE 请求无效".to_string());
    }

    let remaining = entry.size.saturating_sub(request.position);
    let read_size = usize::try_from(remaining.min(u64::from(request.requested_size)))
        .map_err(|_| "剪贴板范围过大".to_string())?;
    let mut file = File::open(&entry.path)
        .map_err(|e| format!("打开剪贴板文件失败: {e}"))?;
    file.seek(SeekFrom::Start(request.position))
        .map_err(|e| format!("定位剪贴板文件失败: {e}"))?;
    let mut data = vec![0u8; read_size];
    file.read_exact(&mut data)
        .map_err(|e| format!("读取剪贴板文件失败: {e}"))?;

    Ok(FileContentsResponse::new_data_response(request.stream_id, data))
}

// ============================================================================
// 剪贴板读写
// ============================================================================

fn read_clipboard_text_blocking() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok()
}

pub(crate) fn write_clipboard_text_blocking(text: String) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("无法打开剪贴板: {e}"))?;
    cb.set_text(text).map_err(|e| format!("写入剪贴板失败: {e}"))
}

fn read_clipboard_file_list_blocking() -> Option<Vec<PathBuf>> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get().file_list().ok()
}

fn write_clipboard_file_list_blocking(paths: &[PathBuf]) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("无法打开剪贴板: {e}"))?;
    cb.set()
        .file_list(paths)
        .map_err(|e| format!("发布剪贴板文件失败: {e}"))
}

fn clipboard_text_within_limit(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_TEXT_BYTES
}

fn stable_hash<T: Hash + ?Sized>(value: &T) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

fn file_list_fingerprint(paths: &[PathBuf]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new();
    for path in paths {
        path.hash(&mut h);
        if let Ok(m) = fs::metadata(path) {
            m.len().hash(&mut h);
            m.is_dir().hash(&mut h);
            m.modified().ok().hash(&mut h);
        }
    }
    h.finish()
}

// ============================================================================
// 缓存目录
// ============================================================================

fn cleanup_generation(generation: &RemoteGeneration) {
    let _ = fs::remove_dir_all(&generation.root);
}

fn clipboard_cache_root() -> PathBuf {
    directories::ProjectDirs::from("com", "rustterm", "RustTerm")
        .map(|dirs| dirs.cache_dir().join("rdp-clipboard"))
        .unwrap_or_else(|| std::env::temp_dir().join("rustterm-rdp-clipboard"))
}

fn cleanup_stale_cache(root: &Path, retention: Duration) -> Result<(), String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(());
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= retention);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}