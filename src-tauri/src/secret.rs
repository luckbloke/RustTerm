//! 已保存的 SSH 密码。
//!
//! ## 威胁模型（重要）
//!
//! 本模块把密码交给**操作系统凭据库**：Windows 是凭据管理器（DPAPI 加密，
//! 绑定当前用户账户），macOS 是 Keychain，Linux 是 Secret Service。
//! 因此：
//!
//! - **能防住**：`sessions.json` 被拷到别的机器、混进备份或云同步、被别的
//!   用户账户读取——凭据库里的密文离开原账户就解不开。
//! - **防不住**：以同一用户身份运行的恶意程序。任何能自动解密的东西，
//!   同权限的攻击者都能解密，这是这类方案的固有边界，不是实现缺陷。
//!
//! 密码**绝不写入** `sessions.json`；那个文件里只留一个"是否记住"的标记。

use serde::Serialize;

/// 凭据库里每条记录的服务名。加版本号便于将来整体迁移。
const SERVICE: &str = "rustterm";

/// 凭据条目的用户名（即凭据库里的 key）。
///
/// 用 `user@host:port` 而不是 `name`：会话名称可改、可重名，
/// 而"谁能连上哪台机器"只由这三者决定。
fn account(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

/// 凭据库当前是否可用。前端据此禁用"记住密码"选项。
#[derive(Debug, Clone, Serialize)]
pub struct StoreStatus {
    pub available: bool,
    /// 不可用时的原因，直接给用户看（英文技术描述）。
    pub detail: Option<String>,
}

pub fn store_status() -> StoreStatus {
    match keyring::Entry::store_status() {
        Ok(()) => StoreStatus { available: true, detail: None },
        Err(e) => StoreStatus { available: false, detail: Some(e.to_string()) },
    }
}

/// 保存密码。
pub fn save(user: &str, host: &str, port: u16, password: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, &account(user, host, port))
        .map_err(|e| format!("secret-store-unavailable:{e}"))?;
    entry
        .set_password(password)
        .map_err(|e| format!("secret-write-failed:{e}"))
}

/// 读取密码。没有记录时返回 `Ok(None)`（不是错误）。
pub fn load(user: &str, host: &str, port: u16) -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(SERVICE, &account(user, host, port))
        .map_err(|e| format!("secret-store-unavailable:{e}"))?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        // 凭据不存在是正常情况：用户没勾选"记住密码"
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("secret-read-failed:{e}")),
    }
}

/// 删除密码。已不存在也算成功。
pub fn delete(user: &str, host: &str, port: u16) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, &account(user, host, port))
        .map_err(|e| format!("secret-store-unavailable:{e}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("secret-delete-failed:{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_key_uses_connection_identity() {
        // 会话名可改、可重名，凭据键必须由连接三元组决定
        assert_eq!(account("root", "10.0.0.1", 22), "root@10.0.0.1:22");
        assert_eq!(account("me", "example.com", 2222), "me@example.com:2222");
        // 同一主机的不同端口、不同用户必须落成不同键
        assert_ne!(account("a", "h", 22), account("a", "h", 2222));
        assert_ne!(account("a", "h", 22), account("b", "h", 22));
    }

    #[test]
    fn store_status_is_queryable_without_panicking() {
        // 这里不断言是否可用：CI 可能没有凭据库。
        // 只保证查询本身不会 panic，且两处调用结论一致。
        let first = store_status();
        let second = store_status();
        assert_eq!(first.available, second.available);
    }
}
