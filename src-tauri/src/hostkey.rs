//! 主机密钥校验。
//!
//! 原实现里 `check_server_key` 恒返回 `true`，等于完全放弃身份校验，
//! 任何能劫持 TCP 的人都能冒充目标主机。这里按策略校验 `~/.ssh/known_hosts`。
//!
//! 只使用 russh 提供的 `_path` 系列函数：默认版本会自己去读 `$HOME`，
//! 在服务账户等环境下取不到，行为不可预测。

use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{check_known_hosts_path, PublicKey, PublicKeyOrCertificate};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostKeyPolicy {
    /// 未知主机直接拒绝，只允许 known_hosts 里已有的主机。
    Strict,
    /// 未知主机记录后继续；**密钥与记录不一致一律拒绝**。默认档。
    #[default]
    AcceptNew,
    /// 完全不校验，兼容老设备。等价于改造前的行为。
    Insecure,
}

/// known_hosts 的路径。
///
/// 不用 `russh::keys::known_hosts_path()`：它内部依赖 `env::home_dir()`，
/// 在受限环境（服务账户、容器）下会失败。这里显式取用户主目录。
pub fn known_hosts_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(Path::new(&home).join(".ssh").join("known_hosts"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 与已知记录一致。
    Trusted,
    /// 首次见到，已写入 known_hosts。
    Learned,
    /// 首次见到，但写入 known_hosts 失败（只告警，不阻断连接）。
    LearnedNotRecorded,
    /// 跳过校验。
    Skipped,
}

/// 校验失败的原因，会作为错误码回传给前端翻译。
#[derive(Debug)]
enum Failure {
    /// 已有记录但密钥变了，附 known_hosts 行号。
    Changed { line: usize },
    /// strict 策略下的未知主机。
    Unknown,
    /// 读不到 known_hosts 路径（取不到主目录）。
    NoHomeDir,
    /// 读取或写入 known_hosts 失败。
    Io(String),
}

impl Failure {
    fn code(&self) -> String {
        match self {
            Failure::Changed { line } => format!("host-key-changed:{line}"),
            Failure::Unknown => "host-key-unknown".to_string(),
            Failure::NoHomeDir => "host-key-no-home".to_string(),
            Failure::Io(detail) => format!("host-key-io:{detail}"),
        }
    }
}

/// 校验主机密钥。
pub fn verify(
    policy: HostKeyPolicy,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Result<Verdict, String> {
    if policy == HostKeyPolicy::Insecure {
        return Ok(Verdict::Skipped);
    }

    let path = known_hosts_path().ok_or_else(|| Failure::NoHomeDir.code())?;

    match check_known_hosts_path(host, port, key, &path) {
        // 命中已有记录
        Ok(true) => Ok(Verdict::Trusted),
        // 全新主机
        Ok(false) => match policy {
            HostKeyPolicy::Strict => Err(Failure::Unknown.code()),
            _ => {
                // 写入失败不阻断连接：accept-new 的语义是"先用起来、下次校验"，
                // 若因为 ~/.ssh 只读或磁盘满就让人连不上，体验上说不过去。
                // 这里保留结论供调用方记录日志。
                match learn_known_hosts_path(host, port, key, &path) {
                    Ok(()) => Ok(Verdict::Learned),
                    Err(e) => {
                        eprintln!(
                            "warn: could not record host key for {host}:{port} into {}: {e}",
                            path.display()
                        );
                        Ok(Verdict::LearnedNotRecorded)
                    }
                }
            }
        },
        // 已有记录但密钥不一致：无论哪一档都必须拒绝，这是防中间人的关键
        Err(russh::keys::Error::KeyChanged { line }) => Err(Failure::Changed { line }.code()),
        Err(e) => Err(Failure::Io(e.to_string()).code()),
    }
}

/// SSH 握手期间调用的校验入口。
///
/// `check_server_key` 运行在 tokio worker 上、握手过程内部，
/// 这里不能再弹窗询问用户（会阻塞握手且前端未必接得住），
/// 所以采用"策略 + 记录"的方式：未知主机按策略处理，密钥变更直接拒绝。
pub struct HostKeyChecker {
    policy: HostKeyPolicy,
    host: String,
    port: u16,
    /// 拒绝原因。`check_server_key` 只能返回 bool，原因经这里传给调用方。
    failure: Arc<Mutex<Option<String>>>,
}

impl HostKeyChecker {
    pub fn new(policy: HostKeyPolicy, host: &str, port: u16) -> Self {
        Self {
            policy,
            host: host.to_string(),
            port,
            failure: Arc::new(Mutex::new(None)),
        }
    }

    /// 供调用方在 `connect` 失败后取出具体原因。
    pub fn failure_reporter(&self) -> Arc<Mutex<Option<String>>> {
        self.failure.clone()
    }

    fn accept(&self, key: &PublicKeyOrCertificate) -> bool {
        // 证书模式：known_hosts 里记的是主机公钥，证书里带着同一把公钥。
        // `Certificate::public_key()` 返回的是 `&KeyData`，需要自己包成 PublicKey。
        let public = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => {
                PublicKey::new(cert.public_key().clone(), String::new())
            }
        };
        match verify(self.policy, &self.host, self.port, &public) {
            Ok(_) => true,
            Err(code) => {
                if let Ok(mut slot) = self.failure.lock() {
                    *slot = Some(code);
                }
                false
            }
        }
    }
}

impl russh::client::Handler for HostKeyChecker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(self.accept(key))
    }
}

/// 连接失败时把"主机密钥被拒"的具体原因提升出来，
/// 否则用户只会看到一句含糊的握手失败。
pub fn describe_failure(
    failure: &Arc<Mutex<Option<String>>>,
    fallback: impl FnOnce() -> String,
) -> String {
    failure
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_codes_are_stable() {
        assert_eq!(Failure::Unknown.code(), "host-key-unknown");
        assert_eq!(Failure::Changed { line: 7 }.code(), "host-key-changed:7");
        assert_eq!(Failure::NoHomeDir.code(), "host-key-no-home");
        assert!(Failure::Io("boom".into()).code().starts_with("host-key-io:"));
    }

    #[test]
    fn default_policy_is_accept_new() {
        assert_eq!(HostKeyPolicy::default(), HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn failure_reporter_round_trips() {
        let checker = HostKeyChecker::new(HostKeyPolicy::Strict, "example.com", 22);
        let reporter = checker.failure_reporter();
        assert!(reporter.lock().unwrap().is_none());
        *reporter.lock().unwrap() = Some("host-key-unknown".into());
        assert_eq!(
            describe_failure(&reporter, || "fallback".into()),
            "host-key-unknown"
        );
        // 无失败原因时回退到调用方提供的默认描述
        let empty: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert_eq!(describe_failure(&empty, || "fallback".into()), "fallback");
    }
}
