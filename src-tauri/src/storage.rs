//! RDP 证书指纹的 known_hosts 持久化。
//!
//! 文件位置：`<config_dir>/rustterm/rdp_known_hosts.json`。
//! 结构：`[ { host, port, fingerprint, subject, issuer, valid_from, valid_to, added_at } ]`。
//!
//! 与 SSH 的 known_hosts 分开存放：RDP 用 SHA256 指纹，SSH 用公钥 blob，
//! 两者的匹配语义不同，混在一起会互相干扰。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RdpCertificateMetadata {
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdpKnownHost {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    #[serde(flatten)]
    pub metadata: RdpCertificateMetadata,
    /// 记录写入时间（unix 秒）
    pub added_at: i64,
}

/// 查询结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownHostCheck {
    /// 主机已记录且指纹一致
    Match,
    /// 主机已记录但指纹变了
    HostSeen,
    /// 主机从未记录
    UnknownHost,
}

fn known_hosts_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "rustterm", "RustTerm")?;
    let dir = dirs.config_dir();
    std::fs::create_dir_all(dir).ok()?;
    Some(dir.join("rdp_known_hosts.json"))
}

/// 进程内文件锁，防止并发写冲突。
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn load_all() -> Vec<RdpKnownHost> {
    let Some(path) = known_hosts_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn store_all(list: &[RdpKnownHost]) {
    let Some(path) = known_hosts_path() else {
        return;
    };
    if let Ok(text) = serde_json::to_string_pretty(list) {
        let _ = std::fs::write(path, text);
    }
}

/// 查询主机是否已记录，以及指纹是否一致。
pub fn check_rdp_known_host(
    host: &str,
    port: u16,
    fingerprint: &str,
) -> Result<KnownHostCheck, String> {
    let _guard = FILE_LOCK.lock().map_err(|_| "known_hosts 锁被污染".to_string())?;
    let list = load_all();
    for entry in &list {
        if entry.host == host && entry.port == port {
            return Ok(if entry.fingerprint == fingerprint {
                KnownHostCheck::Match
            } else {
                KnownHostCheck::HostSeen
            });
        }
    }
    Ok(KnownHostCheck::UnknownHost)
}

/// 写入或更新一条主机记录。
pub fn upsert_rdp_known_host(
    host: &str,
    port: u16,
    fingerprint: &str,
    metadata: RdpCertificateMetadata,
) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().map_err(|_| "known_hosts 锁被污染".to_string())?;
    let mut list = load_all();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    match list
        .iter_mut()
        .find(|entry| entry.host == host && entry.port == port)
    {
        Some(existing) => {
            existing.fingerprint = fingerprint.to_string();
            existing.metadata = metadata;
            existing.added_at = now;
        }
        None => list.push(RdpKnownHost {
            host: host.to_string(),
            port,
            fingerprint: fingerprint.to_string(),
            metadata,
            added_at: now,
        }),
    }
    store_all(&list);
    Ok(())
}

/// 删除一条记录（供 UI"忘记此主机"用）。
pub fn remove_rdp_known_host(host: &str, port: u16) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().map_err(|_| "known_hosts 锁被污染".to_string())?;
    let mut list = load_all();
    list.retain(|entry| !(entry.host == host && entry.port == port));
    store_all(&list);
    Ok(())
}

/// 列出所有记录（供 UI 展示）。
pub fn list_rdp_known_hosts() -> Result<Vec<RdpKnownHost>, String> {
    let _guard = FILE_LOCK.lock().map_err(|_| "known_hosts 锁被污染".to_string())?;
    Ok(load_all())
}

/// 供界面展示的 known_hosts 文件路径。
pub fn rdp_known_hosts_path() -> Option<String> {
    known_hosts_path().map(|p| p.to_string_lossy().to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VncKnownHost {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub added_at: i64,
}

fn vnc_known_hosts_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "rustterm", "RustTerm")?;
    let dir = dirs.config_dir();
    std::fs::create_dir_all(dir).ok()?;
    Some(dir.join("vnc_known_hosts.json"))
}

fn load_vnc_all() -> Vec<VncKnownHost> {
    let Some(path) = vnc_known_hosts_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn check_vnc_known_host(
    host: &str,
    port: u16,
    fingerprint: &str,
) -> Result<KnownHostCheck, String> {
    let list = load_vnc_all();
    for entry in &list {
        if entry.host == host && entry.port == port {
            return Ok(if entry.fingerprint == fingerprint {
                KnownHostCheck::Match
            } else {
                KnownHostCheck::HostSeen
            });
        }
    }
    Ok(KnownHostCheck::UnknownHost)
}

pub fn upsert_vnc_known_host(host: &str, port: u16, fingerprint: &str) -> Result<(), String> {
    let mut list = load_vnc_all();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match list.iter_mut().find(|e| e.host == host && e.port == port) {
        Some(e) => {
            e.fingerprint = fingerprint.to_string();
            e.added_at = now;
        }
        None => list.push(VncKnownHost {
            host: host.to_string(),
            port,
            fingerprint: fingerprint.to_string(),
            added_at: now,
        }),
    }
    if let Some(path) = vnc_known_hosts_path() {
        if let Ok(text) = serde_json::to_string_pretty(&list) {
            let _ = std::fs::write(path, text);
        }
    }
    Ok(())
}