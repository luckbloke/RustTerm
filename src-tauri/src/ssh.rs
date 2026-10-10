use crate::hostkey::{describe_failure, HostKeyChecker, HostKeyPolicy};
use crate::sftp::SftpHandle;
use anyhow::Result;
use russh::client::{self, Handle};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use std::borrow::Cow;

pub struct SshHandle {
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    pub sftp: Arc<SftpHandle>,
    pub client: Arc<Handle<HostKeyChecker>>,
}

impl SshHandle {
    pub fn write(&self, data: &[u8]) -> Result<()> {
        let _ = self.input_tx.send(data.to_vec());
        Ok(())
    }
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let _ = self.resize_tx.send((cols, rows));
        Ok(())
    }
}

/// 建立到主机的 SSH 连接，并在握手期间校验主机密钥。
///
/// `failure` 用来把"主机密钥被拒"的具体原因带出握手过程：
/// `check_server_key` 只能返回 bool，若不额外记录，用户只会看到
/// 一句含糊的握手失败，无法区分是密钥变了还是网络问题。
async fn connect_target<T>(
    policy: HostKeyPolicy,
    target: T,
    host: &str,
    port: u16,
    failure: &Arc<Mutex<Option<String>>>,
) -> Result<Handle<HostKeyChecker>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // 显式配置：在默认安全算法之后，追加旧算法兜底
    let mut config = client::Config::default();
    {
        let p = &mut config.preferred;

        // 1. 旧 KEX：DH group1 / group14 (SHA-1)
        //    把它们追加到末尾，现代算法优先，旧算法兜底
        // kex
        let mut kex: Vec<russh::kex::Name> = p.kex.iter().copied().collect();
        kex.push(russh::kex::DH_G1_SHA1);
        kex.push(russh::kex::DH_G14_SHA1);
        kex.push(russh::kex::DH_GEX_SHA1);
        p.kex = Cow::Owned(kex);

        // cipher
        let mut cipher: Vec<russh::cipher::Name> = p.cipher.iter().copied().collect();
        cipher.push(russh::cipher::AES_128_CBC);
        cipher.push(russh::cipher::AES_192_CBC);
        cipher.push(russh::cipher::AES_256_CBC);
        cipher.push(russh::cipher::TRIPLE_DES_CBC);
        p.cipher = Cow::Owned(cipher);

        // mac
        let mut mac: Vec<russh::mac::Name> = p.mac.iter().copied().collect();
        mac.push(russh::mac::HMAC_SHA1);
        p.mac = Cow::Owned(mac);


        // 4. 旧主机密钥算法：ssh-rsa (SHA-1)
        // key —— 用 cloned 而不是 copied
        let mut key: Vec<russh::keys::Algorithm> = p.key.iter().cloned().collect();
        key.push(russh::keys::Algorithm::Rsa { hash: None }); // None = SHA-1
        p.key = Cow::Owned(key);
    }

    let config = Arc::new(config);
    let checker = HostKeyChecker::new(policy, host, port);
    let reporter = checker.failure_reporter();
    let result = client::connect_stream(config, target, checker).await;
    if result.is_err() {
        if let Ok(reason) = reporter.lock() {
            if let Ok(mut slot) = failure.lock() {
                *slot = reason.clone();
            }
        }
    }
    Ok(result?)
}

/// 连接失败时区分"主机密钥问题"与普通网络/协议错误。
fn connection_error(
    host: &str,
    port: u16,
    e: anyhow::Error,
    failure: &Arc<Mutex<Option<String>>>,
) -> anyhow::Error {
    let message = describe_failure(failure, || format!("connect-failed:{host}:{port}"));
    anyhow::anyhow!("{message} ({e})")
}

async fn open_shell_and_sftp(
    app: AppHandle,
    id: String,
    handle: Handle<HostKeyChecker>,
    cols: u16,
    rows: u16,
) -> Result<SshHandle> {
    let mut channel: Channel<client::Msg> = handle.channel_open_session().await?;
    channel.request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[]).await?;
    channel.request_shell(false).await?;

    let sftp_channel = handle.channel_open_session().await?;
    sftp_channel.request_subsystem(true, "sftp").await?;
    let sftp_session = russh_sftp::client::SftpSession::new(sftp_channel.into_stream()).await?;
    let sftp = Arc::new(SftpHandle::new(sftp_session));

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let client = Arc::new(handle);

    let app_out = app.clone();
    let id_out = id.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) => {
                            let _ = app_out.emit("pty:data", serde_json::json!({ "sessionId": id_out, "data": data.to_vec() }));
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                            let _ = app_out.emit("pty:close", serde_json::json!({ "sessionId": id_out }));
                            break;
                        }
                        _ => {}
                    }
                }
                Some(data) = input_rx.recv() => { let _ = channel.data(&data[..]).await; }
                Some((c, r)) = resize_rx.recv() => { let _ = channel.window_change(c as u32, r as u32, 0, 0).await; }
            }
        }
    });

    Ok(SshHandle { input_tx, resize_tx, sftp, client })
}

pub async fn connect(
    app: AppHandle, id: String,
    policy: HostKeyPolicy,
    host: &str, port: u16, user: &str, password: &str,
    cols: u16, rows: u16,
) -> Result<SshHandle> {
    let failure = Arc::new(Mutex::new(None));
    let target = tokio::net::TcpStream::connect((host, port)).await?;
    let mut handle = connect_target(policy, target, host, port, &failure)
        .await
        .map_err(|e| connection_error(host, port, e, &failure))?;
    let auth = handle.authenticate_password(user, password).await?;
    if !auth.success() { anyhow::bail!("auth-failed"); }
    open_shell_and_sftp(app, id, handle, cols, rows).await
}

pub async fn connect_key(
    app: AppHandle, id: String,
    policy: HostKeyPolicy,
    host: &str, port: u16, user: &str,
    key_path: &str, passphrase: Option<&str>,
    cols: u16, rows: u16,
) -> Result<SshHandle> {
    let failure = Arc::new(Mutex::new(None));
    let target = tokio::net::TcpStream::connect((host, port)).await?;
    let mut handle = connect_target(policy, target, host, port, &failure)
        .await
        .map_err(|e| connection_error(host, port, e, &failure))?;
    let key = load_secret_key(key_path, passphrase)?;
    let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);
    let auth = handle.authenticate_publickey(user, key_with_alg).await?;
    if !auth.success() {
        anyhow::bail!("key-auth-failed");
    }
    open_shell_and_sftp(app, id, handle, cols, rows).await
}

/// 通过跳板机连接目标主机。
///
/// 两段连接都做主机密钥校验：跳板机本身也可能被冒充。
pub async fn connect_jump(
    app: AppHandle, id: String,
    policy: HostKeyPolicy,
    jump_host: &str, jump_port: u16, jump_user: &str, jump_password: &str,
    host: &str, port: u16, user: &str, password: &str,
    cols: u16, rows: u16,
) -> Result<SshHandle> {
    // 1. 连跳板机
    let jump_failure = Arc::new(Mutex::new(None));
    let jump_stream = tokio::net::TcpStream::connect((jump_host, jump_port)).await?;
    let mut jump = connect_target(policy, jump_stream, jump_host, jump_port, &jump_failure)
        .await
        .map_err(|e| connection_error(jump_host, jump_port, e, &jump_failure))?;
    let auth = jump.authenticate_password(jump_user, jump_password).await?;
    if !auth.success() { anyhow::bail!("jump-auth-failed"); }
    let jump = Arc::new(jump);

    // 2. 通过跳板机开 direct-tcpip 通道到目标主机
    let chan = jump
        .channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0)
        .await?;

    // 3. 在通道上建立到目标主机的 SSH 连接（同样校验目标主机密钥）
    let target_failure = Arc::new(Mutex::new(None));
    let mut target = connect_target(policy, chan.into_stream(), host, port, &target_failure)
        .await
        .map_err(|e| connection_error(host, port, e, &target_failure))?;
    let auth = target.authenticate_password(user, password).await?;
    if !auth.success() { anyhow::bail!("target-auth-failed"); }

    open_shell_and_sftp(app, id, target, cols, rows).await
}
