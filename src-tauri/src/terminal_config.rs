//! 本地终端（PTY）使用的 shell 程序配置。
//!
//! 默认值：Windows 用 `powershell.exe -NoLogo`，其它平台用 `$SHELL`。
//! 用户可以改成 `pwsh.exe`、`cmd.exe`、`wsl.exe`、`bash` 等。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        #[cfg(windows)]
        {
            Self {
                program: "powershell.exe".to_string(),
                args: vec![
                    "-NoLogo".to_string(),
                    "-NoExit".to_string(),
                    "-Command".to_string(),
                    "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; [Console]::InputEncoding=[System.Text.Encoding]::UTF8; $OutputEncoding=[System.Text.Encoding]::UTF8".to_string(),
                ],
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                program: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
                args: Vec::new(),
            }
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "rustterm", "RustTerm")?;
    let dir = dirs.config_dir();
    std::fs::create_dir_all(dir).ok()?;
    Some(dir.join("terminal.json"))
}

/// 进程内缓存，避免每次 spawn 都读文件。
static CACHE: Mutex<Option<TerminalConfig>> = Mutex::new(None);

pub fn load() -> TerminalConfig {
    if let Ok(guard) = CACHE.lock() {
        if let Some(cfg) = guard.as_ref() {
            return cfg.clone();
        }
    }
    let cfg = load_from_disk();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(cfg.clone());
    }
    cfg
}

fn load_from_disk() -> TerminalConfig {
    let Some(path) = config_path() else {
        return TerminalConfig::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return TerminalConfig::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(cfg: &TerminalConfig) -> Result<(), String> {
    let path = config_path().ok_or("无法定位配置目录".to_string())?;
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(cfg.clone());
    }
    Ok(())
}