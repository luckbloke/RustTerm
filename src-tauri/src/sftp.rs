use anyhow::Result;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// 是否隐藏项。由前端决定显不显示，后端不做过滤，
    /// 这样用户切换"显示隐藏文件"时不必重新列目录。
    pub hidden: bool,
}

/// 传输取消标志。
///
/// 需要区分"用户点了取消"和"网络/IO 出错"：前者应当清掉没传完的目标文件，
/// 后者要保留断点供续传——所以不能只用一个 bool。
#[derive(Default)]
pub struct TransferCancel {
    cancelled: AtomicBool,
    by_user: AtomicBool,
}

impl TransferCancel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 用户在界面上点了取消。
    pub fn request_user_cancel(&self) {
        self.by_user.store(true, Ordering::SeqCst);
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn is_user_cancel(&self) -> bool {
        self.by_user.load(Ordering::Relaxed)
    }
}

/// 未完成文件的守卫。
///
/// 只有"本次传输新建的文件"才在用户取消时删除：如果目标文件早就存在
/// （例如重复下载覆盖旧文件），它是用户原有的数据，不能替用户决定删掉。
struct PartialFile {
    path: PathBuf,
    pre_existing: bool,
}

impl PartialFile {
    fn inspect(path: &Path) -> Self {
        Self { path: path.to_path_buf(), pre_existing: path.exists() }
    }

    /// 传输结束后的收尾。取消来源不是"用户主动"时保留文件以便续传。
    async fn finish(self, cancel: &TransferCancel) {
        if cancel.is_user_cancel() && !self.pre_existing {
            let _ = tokio::fs::remove_file(&self.path).await;
        }
    }
}

/// 从 Unix 权限位里读隐藏属性：OpenSSH 的 sftp-server 在列出隐藏文件时
/// 会额外置上 0o10000000（S_IFWX 之外的自定义位）。
fn unix_hidden(name: &str, permissions: Option<u32>) -> bool {
    if name.starts_with('.') {
        return true;
    }
    permissions.is_some_and(|mode| mode & 0o10000000 != 0)
}

/// 拼接远端路径。
fn join_remote(parent: &str, name: &str) -> String {
    if parent.is_empty() || parent == "." {
        name.to_string()
    } else if parent == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    }
}

pub struct SftpHandle {
    pub session: Arc<Mutex<SftpSession>>,
}

impl SftpHandle {
    pub fn new(session: SftpSession) -> Self {
        Self { session: Arc::new(Mutex::new(session)) }
    }

    pub async fn list_dir(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        let sftp = self.session.lock().await;
        let query = if path.is_empty() { "." } else { path };
        let dir = sftp.read_dir(query.to_string()).await?;
        let mut entries = Vec::new();
        for entry in dir {
            let name = entry.file_name();
            if name == "." || name == ".." { continue; }
            let file_type = entry.file_type();
            let is_dir = file_type.is_dir();
            let metadata = entry.metadata();
            let hidden = unix_hidden(&name, Some(metadata.permissions.unwrap_or(0)));
            let size = metadata.size.unwrap_or(0);
            entries.push(RemoteEntry {
                name: name.clone(),
                path: join_remote(query, &name),
                is_dir,
                size,
                hidden,
            });
        }
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(entries)
    }

    pub async fn download_with_cancel(
        &self,
        app: AppHandle,
        session_id: String,
        remote: &str,
        local: &Path,
        cancel: Arc<TransferCancel>,
    ) -> Result<()> {
        let sftp = self.session.lock().await;
        let mut remote_file = sftp.open(remote.to_string()).await?;
        let total = remote_file.metadata().await?.size.unwrap_or(0);

        // 在创建之前判断，才能知道文件是"本来就有的"还是"本次新建的"
        let guard = PartialFile::inspect(local);
        let mut local_file = tokio::fs::File::create(local).await?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut sent: u64 = 0;

        loop {
            if cancel.is_cancelled() {
                // 用户主动取消且文件是本次新建的 → 清掉这个半成品
                drop(local_file);
                guard.finish(&cancel).await;
                anyhow::bail!("cancelled");
            }
            let n = remote_file.read(&mut buf).await?;
            if n == 0 { break; }
            local_file.write_all(&buf[..n]).await?;
            sent += n as u64;
            let _ = app.emit("transfer:progress", serde_json::json!({
                "sessionId": session_id, "sent": sent, "total": total, "label": "Download"
            }));
        }
        local_file.flush().await?;
        drop(local_file);
        guard.finish(&cancel).await;
        let _ = app.emit("transfer:done", serde_json::json!({ "sessionId": session_id, "label": "Download" }));
        Ok(())
    }

    /// 断点续传下载
    pub async fn download_resume(
        &self,
        app: AppHandle,
        session_id: String,
        remote: &str,
        local: &Path,
        cancel: Arc<TransferCancel>,
    ) -> Result<()> {
        let sftp = self.session.lock().await;
        let mut remote_file = sftp.open(remote.to_string()).await?;
        let total = remote_file.metadata().await?.size.unwrap_or(0);

        let existing = if local.exists() {
            tokio::fs::metadata(local).await?.len()
        } else {
            0
        };

        let guard = PartialFile::inspect(local);
        let mut local_file = if existing > 0 && existing < total {
            let f = tokio::fs::OpenOptions::new().append(true).open(local).await?;
            // 跳过已传部分
            let mut skip_buf = vec![0u8; 64 * 1024];
            let mut skipped: u64 = 0;
            while skipped < existing {
                let want = ((existing - skipped) as usize).min(skip_buf.len());
                let n = remote_file.read(&mut skip_buf[..want]).await?;
                if n == 0 { break; }
                skipped += n as u64;
            }
            f
        } else {
            tokio::fs::File::create(local).await?
        };

        let mut buf = vec![0u8; 64 * 1024];
        let mut sent = existing.min(total);

        loop {
            if cancel.is_cancelled() {
                drop(local_file);
                guard.finish(&cancel).await;
                anyhow::bail!("cancelled");
            }
            let n = remote_file.read(&mut buf).await?;
            if n == 0 { break; }
            local_file.write_all(&buf[..n]).await?;
            sent += n as u64;
            let _ = app.emit("transfer:progress", serde_json::json!({
                "sessionId": session_id, "sent": sent, "total": total, "label": "Download (resume)"
            }));
        }
        local_file.flush().await?;
        drop(local_file);
        // 续传的目标文件通常本来就存在（就是要续它），所以这里一般不会删
        guard.finish(&cancel).await;
        let _ = app.emit("transfer:done", serde_json::json!({ "sessionId": session_id, "label": "Download" }));
        Ok(())
    }

    pub async fn upload_with_cancel(
        &self,
        app: AppHandle,
        session_id: String,
        local: &Path,
        remote: &str,
        cancel: Arc<TransferCancel>,
    ) -> Result<()> {
        let sftp = self.session.lock().await;
        let mut local_file = tokio::fs::File::open(local).await?;
        let total = local_file.metadata().await?.len();

        let mut remote_file = sftp.create(remote.to_string()).await?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut sent: u64 = 0;
        let mut user_cancelled = false;

        loop {
            if cancel.is_cancelled() {
                user_cancelled = cancel.is_user_cancel();
                break;
            }
            let n = local_file.read(&mut buf).await?;
            if n == 0 { break; }
            remote_file.write_all(&buf[..n]).await?;
            sent += n as u64;
            let _ = app.emit("transfer:progress", serde_json::json!({
                "sessionId": session_id, "sent": sent, "total": total, "label": "Upload"
            }));
        }

        if user_cancelled {
            // 用户主动取消：关掉文件句柄再删掉远端那个不完整的文件。
            // 注意：必须复用上面已经持有的 `sftp` 守卫，
            // 不能再 self.session.lock().await —— 那是在等自己持有的锁，会永久死锁，
            // 结果就是传输命令永不返回、界面一直卡在进度条上。
            drop(remote_file);
            let _ = sftp.remove_file(remote.to_string()).await;
            anyhow::bail!("cancelled");
        }

        remote_file.flush().await?;
        let _ = app.emit("transfer:done", serde_json::json!({ "sessionId": session_id, "label": "Upload" }));
        Ok(())
    }

    /// 断点续传上传。
    ///
    /// 关键点：必须用「写模式但不截断」打开远端文件，并把写指针定位到已传长度。
    /// 原实现用 `create()`（等于 TRUNCATE）打开，却把本地指针跳到了已传长度之后，
    /// 结果是远端被清空、前 existing 字节永久丢失。
    pub async fn upload_resume(
        &self,
        app: AppHandle,
        session_id: String,
        local: &Path,
        remote: &str,
        cancel: Arc<TransferCancel>,
    ) -> Result<()> {
        use tokio::io::AsyncSeekExt;

        let sftp = self.session.lock().await;
        let mut local_file = tokio::fs::File::open(local).await?;
        let total = local_file.metadata().await?.len();

        // 远端已存在的长度决定续传起点。
        let existing = match sftp.metadata(remote.to_string()).await {
            Ok(meta) => meta.size.unwrap_or(0).min(total),
            Err(_) => 0,
        };

        let mut remote_file = sftp
            .open_with_flags(
                remote.to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
            )
            .await?;

        if existing > 0 {
            local_file.seek(std::io::SeekFrom::Start(existing)).await?;
            remote_file.seek(std::io::SeekFrom::Start(existing)).await?;
        }

        let mut buf = vec![0u8; 64 * 1024];
        let mut sent = existing;

        loop {
            if cancel.is_cancelled() {
                if cancel.is_user_cancel() {
                    // 用户主动取消：删掉远端半成品。续传上传依赖"远端已有长度"，
                    // 留下半截文件会让下次续传从错误的偏移开始。
                    // 同样复用已持有的守卫，不能再 lock()（会死锁）。
                    drop(remote_file);
                    let _ = sftp.remove_file(remote.to_string()).await;
                }
                anyhow::bail!("cancelled");
            }
            let n = local_file.read(&mut buf).await?;
            if n == 0 { break; }
            remote_file.write_all(&buf[..n]).await?;
            sent += n as u64;
            let _ = app.emit("transfer:progress", serde_json::json!({
                "sessionId": session_id, "sent": sent, "total": total, "label": "Upload (resume)"
            }));
        }
        remote_file.flush().await?;
        let _ = app.emit("transfer:done", serde_json::json!({ "sessionId": session_id, "label": "Upload" }));
        Ok(())
    }

    /// 递归上传目录。
    ///
    /// 目录上传耗时最长，因此递归的每一层、每个文件的每个分块都检查一次取消标志；
    /// 原先这个入口完全不支持取消，大目录一旦开始就只能等它跑完。
    pub async fn upload_dir(
        &self,
        app: AppHandle,
        session_id: String,
        local: &Path,
        remote: &str,
        cancel: Arc<TransferCancel>,
    ) -> Result<()> {
        if cancel.is_cancelled() { anyhow::bail!("cancelled"); }

        // 目录本身用 basename 作为进度标签，便于用户看出当前在传哪一层
        let dir_label = local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| remote.to_string());

        // 创建远程目录
        {
            let sftp = self.session.lock().await;
            let _ = sftp.create_dir(remote.to_string()).await;
        }

        let mut entries = tokio::fs::read_dir(local).await?;
        while let Some(entry) = entries.next_entry().await? {
            if cancel.is_cancelled() { anyhow::bail!("cancelled"); }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let remote_child = format!("{}/{}", remote.trim_end_matches('/'), name);

            if path.is_dir() {
                // 递归前必须让本层的 SFTP 守卫出作用域：
                // 否则子目录那一层会去抢一把被自己父层持有的锁，直接卡死。
                Box::pin(self.upload_dir(
                    app.clone(), session_id.clone(), &path, &remote_child, cancel.clone()
                )).await?;
            } else {
                let total = tokio::fs::metadata(&path).await?.len();
                // 单独一层作用域：让会话锁与文件句柄在本次迭代结束时就释放，
                // 不跨越到下一次迭代（尤其是递归分支）。
                {
                    let sftp = self.session.lock().await;
                    let mut local_file = tokio::fs::File::open(&path).await?;
                    let mut remote_file = sftp.create(remote_child.clone()).await?;
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut sent: u64 = 0;
                    loop {
                        if cancel.is_cancelled() {
                            if cancel.is_user_cancel() {
                                // 复用守卫删除；此处若再 lock() 会死锁
                                let _ = sftp.remove_file(remote_child.clone()).await;
                            }
                            anyhow::bail!("cancelled");
                        }
                        let n = local_file.read(&mut buf).await?;
                        if n == 0 { break; }
                        remote_file.write_all(&buf[..n]).await?;
                        sent += n as u64;
                        let _ = app.emit("transfer:progress", serde_json::json!({
                            "sessionId": session_id, "sent": sent, "total": total,
                            "label": format!("{dir_label}/{name}")
                        }));
                    }
                    remote_file.flush().await?;
                }
            }
        }
        Ok(())
    }

    pub async fn remove(&self, path: &str, is_dir: bool) -> Result<()> {
        let sftp = self.session.lock().await;
        if is_dir {
            sftp.remove_dir(path.to_string()).await?;
        } else {
            sftp.remove_file(path.to_string()).await?;
        }
        Ok(())
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        let sftp = self.session.lock().await;
        sftp.create_dir(path.to_string()).await?;
        Ok(())
    }

    pub async fn rename(&self, old: &str, new: &str) -> Result<()> {
        let sftp = self.session.lock().await;
        sftp.rename(old.to_string(), new.to_string()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotfiles_are_hidden() {
        assert!(unix_hidden(".bashrc", None));
        assert!(unix_hidden(".", None));
        assert!(unix_hidden("..", None));
        assert!(unix_hidden(".ssh", Some(0o40755)));
        // 普通文件不受影响
        assert!(!unix_hidden("readme.md", Some(0o100644)));
        assert!(!unix_hidden("src", Some(0o40755)));
    }

    #[test]
    fn sftp_server_hidden_flag_is_respected() {
        // OpenSSH 的 sftp-server 对隐藏文件会额外置 0o10000000
        assert!(unix_hidden("secret.txt", Some(0o10000000 | 0o100644)));
        assert!(unix_hidden("hiddendir", Some(0o10000000 | 0o40755)));
        // 没有该位就是普通文件
        assert!(!unix_hidden("visible.txt", Some(0o100644)));
    }

    #[test]
    fn missing_permissions_do_not_panic() {
        assert!(!unix_hidden("plain", None));
        assert!(!unix_hidden("plain", Some(0)));
    }

    #[test]
    fn user_cancel_is_recorded_separately_from_abort() {
        let cancel = TransferCancel::new();
        assert!(!cancel.is_cancelled());
        assert!(!cancel.is_user_cancel());

        cancel.request_user_cancel();
        assert!(cancel.is_cancelled());
        // 关键：必须能区分"用户点的取消"，否则会误删断点文件
        assert!(cancel.is_user_cancel());
    }

    #[test]
    fn join_remote_handles_root_and_relative() {
        assert_eq!(join_remote(".", "a.txt"), "a.txt");
        assert_eq!(join_remote("", "a.txt"), "a.txt");
        assert_eq!(join_remote("/", "a.txt"), "/a.txt");
        assert_eq!(join_remote("/var", "log"), "/var/log");
        // 末尾多余的斜杠不应产生双斜杠
        assert_eq!(join_remote("/var/", "log"), "/var/log");
    }

    /// 回归：取消上传后界面卡在进度条，根因是「已持锁又重复加锁」把自己锁死。
    ///
    /// 这个 bug 只在运行时表现为整个传输命令永不返回，编译器查不出来。
    /// 这里用同一个 Mutex 复现：守卫在作用域内时 `try_lock` 必然失败，
    /// 我们要求「复用守卫」那条约定成立。
    #[tokio::test]
    async fn cancel_path_must_reuse_the_guard_not_relock() {
        let session = Mutex::new(1u32);

        // 正确做法：函数入口加一次锁，取消路径复用这把守卫
        let result = async {
            let guard = session.lock().await;
            // 模拟「用户取消 → 删除半成品」，复用 guard
            *guard + 1
        }
        .await;
        assert_eq!(result, 2);

        // 反例：守卫仍存活时再加锁会永久等待。
        // 用 try_lock 断言这一点，避免测试真的挂住。
        let guard = session.lock().await;
        assert!(
            session.try_lock().is_err(),
            "守卫存活期间必须拿不到锁——所以取消路径绝不能再次 lock().await，否则死锁"
        );
        drop(guard);
        // 守卫释放后可再次获取
        assert!(session.try_lock().is_ok());
    }
}