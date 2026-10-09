//! AI Agent：调 LLM API，生成命令建议。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiMessage {
    pub role: String,      // "user" | "assistant" | "system"
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// 只读模式：为 true 时前端禁用写命令。
    #[serde(default)]
    pub read_only: bool,
    /// 对话历史最多保留多少条。
    #[serde(default = "default_max_history")]
    pub max_history: usize,
    /// 每次读终端上下文取最近多少行。
    #[serde(default = "default_context_lines")]
    pub context_lines: usize,
    /// 上下文每行最多多少字符，超出截断。
    #[serde(default = "default_context_max_line_len")]
    pub context_max_line_len: usize,
}

fn default_max_history() -> usize { 20 }
fn default_context_lines() -> usize { 50 }
fn default_context_max_line_len() -> usize { 200 }
fn default_max_tokens() -> u32 { 2048 }
fn default_temperature() -> f32 { 0.3 }

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            max_tokens: 2048,
            temperature: 0.3,
            read_only: false,
            max_history: 20,
            context_lines: 50,
            context_max_line_len: 200,
        }
    }
}

/// 系统提示词：告诉 AI 它是一个运维助手，输出要带命令块。
const SYSTEM_PROMPT: &str = r#"
你是 RustTerm 的终端运维助手。用户会在终端里操作服务器，你需要：
1. 用简洁的中文回答用户的问题。
2. 如果需要执行命令，把命令放在 ```bash 代码块里。
3. 命令要能在 Linux/macOS 的 bash 或 Windows 的 PowerShell 里运行。
4. 不要执行危险命令（rm -rf /、format、dd 等），如果用户要求，先警告。
5. 一次最多给 3 条命令，按顺序执行。

【Agent 模式】
当用户消息以 [Agent] 开头时，你进入多步任务模式：
- 每次回复只给 1-2 条命令，等用户反馈输出后再决定下一步。
- 命令放在 ```bash 代码块里。
- 命令执行后，用户会把输出作为下一条消息发给你。
- 当你认为任务已经完成，在回复末尾加上 [DONE]。
- 如果命令失败或输出异常，可以调整策略重试，但不要重复同一条命令超过 2 次。
- 最多 10 轮，超过就停止。
"#;

/// 发送对话，返回 AI 回复。
pub async fn chat(config: &AiConfig, history: &[AiMessage]) -> Result<String> {
    match config.provider.as_str() {
        "openai" => chat_openai(config, history).await,
        "anthropic" => chat_anthropic(config, history).await,
        "ollama" => chat_ollama(config, history).await,
        other => anyhow::bail!("不支持的 provider: {other}"),
    }
}

async fn chat_openai(config: &AiConfig, history: &[AiMessage]) -> Result<String> {
    let client = reqwest::Client::new();
    let mut messages = vec![serde_json::json!({
        "role": "system",
        "content": SYSTEM_PROMPT,
    })];
    for m in history {
        messages.push(serde_json::json!({
            "role": m.role,
            "content": m.content,
        }));
    }

    let body = serde_json::json!({
        "model": config.model,
        "messages": messages,
        "max_tokens": config.max_tokens,
        "temperature": config.temperature,
    });

    let resp = client
        .post(format!("{}/chat/completions", config.base_url))
        .header("Authorization", format!("Bearer {}", config.api_key))
        .json(&body)
        .send()
        .await
        .context("调用 OpenAI API 失败")?;

    let json: serde_json::Value = resp.json().await.context("解析响应失败")?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("响应格式异常: {json}"))?;
    Ok(content.to_string())
}

async fn chat_anthropic(config: &AiConfig, history: &[AiMessage]) -> Result<String> {
    let client = reqwest::Client::new();
    let mut messages = Vec::new();
    for m in history {
        if m.role == "system" { continue; }
        messages.push(serde_json::json!({
            "role": m.role,
            "content": m.content,
        }));
    }

    let body = serde_json::json!({
        "model": config.model,
        "system": SYSTEM_PROMPT,
        "messages": messages,
        "max_tokens": config.max_tokens,
    });

    let resp = client
        .post(format!("{}/v1/messages", config.base_url))
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("调用 Anthropic API 失败")?;

    let json: serde_json::Value = resp.json().await.context("解析响应失败")?;
    let content = json["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("响应格式异常: {json}"))?;
    Ok(content.to_string())
}

async fn chat_ollama(config: &AiConfig, history: &[AiMessage]) -> Result<String> {
    let client = reqwest::Client::new();
    let mut messages = vec![serde_json::json!({
        "role": "system",
        "content": SYSTEM_PROMPT,
    })];
    for m in history {
        messages.push(serde_json::json!({
            "role": m.role,
            "content": m.content,
        }));
    }

    let body = serde_json::json!({
        "model": config.model,
        "messages": messages,
        "stream": false,
    });

    let resp = client
        .post(format!("{}/api/chat", config.base_url))
        .json(&body)
        .send()
        .await
        .context("调用 Ollama 失败")?;

    let json: serde_json::Value = resp.json().await.context("解析响应失败")?;
    let content = json["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("响应格式异常: {json}"))?;
    Ok(content.to_string())
}

/// 从 AI 回复里提取 ```bash 代码块里的命令。
pub fn extract_commands(reply: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut in_block = false;
    let mut current = String::new();
    for line in reply.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_block {
                // 结束
                if !current.trim().is_empty() {
                    commands.push(current.trim().to_string());
                }
                current.clear();
                in_block = false;
            } else {
                in_block = true;
            }
            continue;
        }
        if in_block {
            current.push_str(line);
            current.push('\n');
        }
    }
    commands
}

/// 危险命令检测。
pub fn is_dangerous(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let patterns = [
        "rm -rf /", "rm -rf /*", "mkfs", "dd if=", "format ",
        ":(){ :|:& };:", "> /dev/sda", "chmod -r 777 /",
        "shutdown", "reboot", "init 0", "halt",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// 判断命令是否会修改系统状态。
pub fn is_write_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let patterns = [
        ">", ">>", "rm ", "mv ", "cp ", "mkdir ", "touch ",
        "chmod ", "chown ", "chgrp ",
        "apt ", "yum ", "dnf ", "pacman ", "brew ",
        "systemctl ", "service ", "kill ", "pkill ",
        "useradd", "usermod", "userdel", "passwd",
        "dd ", "mkfs", "fdisk", "parted",
        "sed -i", "tee ",
        "git clone", "pip install", "npm install",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// 判断 AI 回复是否表示任务完成。
pub fn is_task_done(reply: &str) -> bool {
    reply.contains("[DONE]")
}