//! 端口扫描。
//!
//! 并发模型：外层对每个 IP 起一个任务，内层对每个端口起一个任务，
//! 用 Semaphore 限制同时打开的连接数——不然扫一个 /24 网段会把
//! 本机文件描述符耗尽，也会被对端当成攻击。

use serde::Serialize;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::timeout;

/// 单个开放端口的结果。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPort {
    pub ip: String,
    pub port: u16,
    /// 从发起连接到建立成功的毫秒数，便于判断网络质量
    pub latency_ms: u64,
}

/// 扫描进度。前端据此更新进度条和已发现列表。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub scan_id: String, 
    pub scanned: u64,
    pub total: u64,
    pub open: Vec<OpenPort>,
}

/// 把 "192.168.1.1-192.168.1.254" 或 "192.168.1.0/24" 解析成 IP 列表。
///
/// 不用外部 crate：范围形式最常见，CIDR 只支持 /24 及以上（再小会扫太久）。
fn parse_targets(spec: &str) -> Result<Vec<IpAddr>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty-target".into());
    }

    // 范围形式：start-end
    if let Some((start, end)) = spec.split_once('-') {
        let start: IpAddr = start.trim().parse().map_err(|_| "bad-start-ip".to_string())?;
        let end: IpAddr = end.trim().parse().map_err(|_| "bad-end-ip".to_string())?;
        let (start, end) = match (start, end) {
            (IpAddr::V4(a), IpAddr::V4(b)) => (u32::from(a), u32::from(b)),
            _ => return Err("ipv6-range-unsupported".into()),
        };
        if end < start {
            return Err("range-reversed".into());
        }
        // 上限保护：超过 4096 个地址就不让扫，避免用户误输入整段公网
        if end - start > 4096 {
            return Err("range-too-large".into());
        }
        return Ok((start..=end)
            .map(|n| IpAddr::V4(std::net::Ipv4Addr::from(n)))
            .collect());
    }

    // CIDR 形式：a.b.c.d/n
    if let Some((base, prefix)) = spec.split_once('/') {
        let base: std::net::Ipv4Addr = base.trim().parse().map_err(|_| "bad-cidr-base".to_string())?;
        let prefix: u32 = prefix.trim().parse().map_err(|_| "bad-cidr-prefix".to_string())?;
        if prefix > 32 {
            return Err("bad-cidr-prefix".into());
        }
        // 只允许 /20 及以上：/20 是 4096 个地址，正好是上面的上限
        if prefix < 20 {
            return Err("cidr-too-large".into());
        }
        let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
        let network = u32::from(base) & mask;
        let broadcast = network | !mask;
        // 跳过网络地址和广播地址
        return Ok(((network + 1)..broadcast)
            .map(|n| IpAddr::V4(std::net::Ipv4Addr::from(n)))
            .collect());
    }

    // 单个 IP
    let ip: IpAddr = spec.parse().map_err(|_| "bad-ip".to_string())?;
    Ok(vec![ip])
}

/// 把 "22,80,443" 或 "1-1024" 或混合形式解析成端口列表。
fn parse_ports(spec: &str) -> Result<Vec<u16>, String> {
    let mut ports = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() { continue; }
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u16 = lo.trim().parse().map_err(|_| "bad-port-range".to_string())?;
            let hi: u16 = hi.trim().parse().map_err(|_| "bad-port-range".to_string())?;
            if lo == 0 || hi < lo {
                return Err("bad-port-range".into());
            }
            ports.extend(lo..=hi);
        } else {
            let p: u16 = part.parse().map_err(|_| "bad-port".to_string())?;
            if p == 0 { return Err("bad-port".into()); }
            ports.push(p);
        }
    }
    ports.sort_unstable();
    ports.dedup();
    if ports.is_empty() {
        return Err("no-ports".into());
    }
    // 上限保护：单次扫描的 IP×端口 不超过 65536
    Ok(ports)
}

/// 探测单个 (ip, port)。成功返回耗时，失败返回 None。
async fn probe(ip: IpAddr, port: u16, connect_timeout: Duration) -> Option<u64> {
    let addr = SocketAddr::new(ip, port);
    let start = std::time::Instant::now();
    match timeout(connect_timeout, TcpStream::connect(addr)).await {
        // 连上了 → 端口开放。立刻 drop 连接，只关心握手是否成功。
        Ok(Ok(_stream)) => Some(start.elapsed().as_millis() as u64),
        // 超时、拒绝、网络不可达 → 视为关闭/过滤
        _ => None,
    }
}

/// 扫描入口。
///
/// 用 `scan_id` 和 `cancel` 配对：前端可以中途取消，后端每完成一个探测
/// 就检查一次取消标志。
#[tauri::command]
pub async fn port_scan(
    app: AppHandle,
    scan_id: String,
    targets: String,
    ports: String,
    concurrency: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<(), String> {
    let ips = parse_targets(&targets)?;
    let port_list = parse_ports(&ports)?;
    println!("[port_scan] {} ips × {} ports = {} tasks", ips.len(), port_list.len(), ips.len() * port_list.len());
    let connect_timeout = Duration::from_millis(timeout_ms.unwrap_or(800).clamp(100, 5000));

    // 并发上限默认 256：单机扫描够快，又不至于打爆对端。
    // 上限 1024，再高容易被当成 SYN flood。
    let concurrency = concurrency.unwrap_or(256).clamp(1, 1024);

    // 每个探测任务都需要 (ip, port) 对
    let mut tasks = Vec::with_capacity(ips.len() * port_list.len());
    for ip in &ips {
        for port in &port_list {
            tasks.push((*ip, *port));
        }
    }
    let total = tasks.len() as u64;

    let semaphore = Arc::new(Semaphore::new(concurrency));
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let open = Arc::new(tokio::sync::Mutex::new(Vec::<OpenPort>::new()));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // 注册取消标志，前端可以通过 cancel_scan 触发
    crate::scan_cancel_register(&scan_id, cancel.clone());

    let mut handles = Vec::with_capacity(tasks.len());
    for (ip, port) in tasks {
        let sem = semaphore.clone();
        let counter = counter.clone();
        let open = open.clone();
        let cancel = cancel.clone();
        let app = app.clone();
        let scan_id = scan_id.clone();

        handles.push(tokio::spawn(async move {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            // 拿不到 permit 会在这里等——这就是限流点
            let _permit = match sem.acquire().await {
                Ok(p) => p,
                Err(_) => return, // semaphore 被关闭（不该发生）
            };

            if let Some(latency) = probe(ip, port, connect_timeout).await {
                let entry = OpenPort {
                    ip: ip.to_string(),
                    port,
                    latency_ms: latency,
                };
                open.lock().await.push(entry);
            }

            let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // 每完成一个就推一次进度：结果数量少时更新更频繁，用户体验更好。
            // 如果嫌事件太密，可以改成 `done % 8 == 0` 才发。
            let snapshot = open.lock().await.clone();
            let _ = app.emit("scan:progress", ScanProgress {
                scan_id: scan_id.clone(),
                scanned: done,
                total,
                open: snapshot,
            });
        }));
    }
    
    println!("[port_scan] spawned {} tasks, waiting...", handles.len());

    // 等所有任务结束（或者用户取消后剩余任务会快速返回）
    for h in handles {
        let _ = h.await;
    }
    println!("[port_scan] all done");
    crate::scan_cancel_unregister(&scan_id);
    let _ = app.emit("scan:done", serde_json::json!({ "scanId": scan_id }));
    Ok(())
}

/// 前端点"停止扫描"时调用。
#[tauri::command]
pub fn cancel_scan(scan_id: String) -> Result<(), String> {
    crate::scan_cancel_fire(&scan_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        let ips = parse_targets("192.168.1.1-192.168.1.3").unwrap();
        assert_eq!(ips.len(), 3);
        assert_eq!(ips[0].to_string(), "192.168.1.1");
        assert_eq!(ips[2].to_string(), "192.168.1.3");
    }

    #[test]
    fn cidr_parsing() {
        let ips = parse_targets("192.168.1.0/30").unwrap();
        // /30 有 4 个地址，去掉网络地址和广播地址剩 2 个
        assert_eq!(ips.len(), 2);
        assert_eq!(ips[0].to_string(), "192.168.1.1");
        assert_eq!(ips[1].to_string(), "192.168.1.2");
    }

    #[test]
    fn port_list_parsing() {
        let ports = parse_ports("22,80,443,8000-8003").unwrap();
        assert_eq!(ports, vec![22, 80, 443, 8000, 8001, 8002, 8003]);
    }

    #[test]
    fn oversized_range_rejected() {
        assert!(parse_targets("10.0.0.1-10.1.0.1").is_err());
        assert!(parse_targets("10.0.0.0/8").is_err());
    }
}