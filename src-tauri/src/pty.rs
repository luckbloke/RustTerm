use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

pub struct PtyHandle {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Option<Arc<Mutex<Box<dyn MasterPty + Send>>>>,
    tcp_writer: Option<Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>>,
}

impl PtyHandle {
    pub fn write(&self, data: &[u8]) -> Result<()> {
        if let Some(tcp) = &self.tcp_writer {
            let tcp = tcp.clone();
            let data = data.to_vec();
            tokio::spawn(async move {
                let mut w = tcp.lock().await;
                let _ = w.write_all(&data).await;
            });
        } else {
            let mut w = self.writer.lock().unwrap();
            w.write_all(data)?;
            w.flush()?;
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if let Some(m) = &self.master {
            let m = m.lock().unwrap();
            m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        }
        Ok(())
    }

    pub fn from_tcp(writer: Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>) -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(std::io::sink()))),
            master: None,
            tcp_writer: Some(writer),
        }
    }
}

pub fn spawn_local(app: AppHandle, id: String, cols: u16, rows: u16) -> Result<PtyHandle> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows, cols, pixel_width: 0, pixel_height: 0,
    })?;

    let shell = if cfg!(windows) { "powershell.exe" } else { "/bin/bash" };
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");

    if cfg!(windows) {
        // 必须写成一条完整语句：PowerShell 的续行符是反引号而不是反斜杠，
        // 用 "\" 续行会被解析成非法字符，每个本地终端启动时都会先报一次错。
        cmd.args([
            "-NoLogo", "-NoExit", "-Command",
            "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; [Console]::InputEncoding=[System.Text.Encoding]::UTF8; $OutputEncoding=[System.Text.Encoding]::UTF8",
        ]);
    }

    let _child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let master = pair.master;

    let writer = Arc::new(Mutex::new(writer));
    let master = Arc::new(Mutex::new(master));

    let app_out = app.clone();
    let id_out = id.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = app_out.emit(
                        "pty:close",
                        serde_json::json!({ "sessionId": id_out }),
                    );
                    break;
                }
                Ok(n) => {
                    let _ = app_out.emit(
                        "pty:data",
                        serde_json::json!({ "sessionId": id_out, "data": &buf[..n] }),
                    );
                }
            }
        }
    });

    Ok(PtyHandle {
        writer,
        master: Some(master),
        tcp_writer: None,
    })
}