// ============================================================================
// probe — 探针 I/O 边界 (spec §6)。L1 端口 / L2 直连 HTTP。L3 见 Task 7。
// ============================================================================

use crate::logs::parse_status_line;
use crate::model::{L2, StatusSet};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Review Focus #5: 连接与读各 8s 硬超时, 不得永久阻塞。
const TIMEOUT: Duration = Duration::from_secs(8);

/// 解析 netstat -ano 的一行; 处于 LISTENING 才返回 (本地端口, PID)。
pub fn netstat_listen_pid(line: &str) -> Option<(u16, u32)> {
    let mut toks = line.split_whitespace();
    if toks.next()? != "TCP" {
        return None;
    }
    let local = toks.next()?;
    toks.next()?; // 外部地址
    let state = toks.next()?;
    if !(state.contains("LISTENING") || state.contains("LISTEN")) {
        return None;
    }
    let pid = toks.next()?.parse::<u32>().ok()?;
    let idx = local.rfind(':')?;
    let port = local[idx + 1..].trim_end_matches(']').parse::<u16>().ok()?;
    Some((port, pid))
}

fn netstat_lines() -> Vec<String> {
    match std::process::Command::new("netstat").arg("-ano").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).lines().map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

fn tcp_connect_ok(port: u16) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, TIMEOUT).is_ok()
}

/// L1: netstat 优先, TCP 连通兜底 (防状态字被本地化)。
pub fn port_listening(port: u16) -> bool {
    netstat_lines()
        .iter()
        .any(|l| netstat_listen_pid(l).map(|(p, _)| p == port).unwrap_or(false))
        || tcp_connect_ok(port)
}

/// spec §6 行3: 端口被非 HTTP 进程占用时报告 PID。
pub fn port_owner_pid(port: u16) -> Option<u32> {
    netstat_lines().iter().find_map(|l| {
        netstat_listen_pid(l).and_then(|(p, pid)| if p == port { Some(pid) } else { None })
    })
}

/// 5xx 优先归 ServerError, 其余按期望集合。spec §5: 不设"非 5xx 即活"。
pub fn classify(code: u16, set: &StatusSet) -> L2 {
    if (500..600).contains(&code) {
        L2::ServerError(code)
    } else if set.accepts(code) {
        L2::Ok(code)
    } else {
        L2::Unexpected(code)
    }
}

/// 明文 HTTP GET, 读到状态行为止。硬超时。
pub fn http_probe_against(port: u16, pathname: &str, set: &StatusSet) -> L2 {
    use std::io::{Read, Write};
    let addr: SocketAddr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return L2::NoHttpResponse,
    };
    let mut s = match TcpStream::connect_timeout(&addr, TIMEOUT) {
        Ok(s) => s,
        Err(_) => return L2::NoHttpResponse,
    };
    let _ = s.set_write_timeout(Some(TIMEOUT));
    let _ = s.set_read_timeout(Some(TIMEOUT));
    let path = if pathname.is_empty() { "/" } else { pathname };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUser-Agent: RepairArtemisWeb\r\nConnection: close\r\n\r\n",
        path, port
    );
    if s.write_all(req.as_bytes()).is_err() || s.flush().is_err() {
        return L2::NoHttpResponse;
    }
    // 逐字节读到首个 CRLF 即停, 不等响应体
    let mut acc: Vec<u8> = Vec::with_capacity(64);
    let mut b = [0u8; 1];
    while acc.len() < 128 {
        match s.read(&mut b) {
            Ok(0) => break,
            Ok(_) => {
                acc.push(b[0]);
                if acc.len() >= 2 && acc[acc.len() - 2..] == [b'\r', b'\n'] {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let first = String::from_utf8_lossy(&acc).lines().next().unwrap_or("").to_string();
    match parse_status_line(&first) {
        Some(c) => classify(c, set),
        None => L2::NoHttpResponse,
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::model::{L2, StatusSet};

    #[test]
    fn netstat行解析只认LISTENING并取PID() {
        assert_eq!(
            netstat_listen_pid(
                "  TCP    0.0.0.0:9017           0.0.0.0:0            LISTENING       8460"
            ),
            Some((9017, 8460))
        );
        assert_eq!(
            netstat_listen_pid(
                "  TCP    [::]:9018               [::]:0               LISTENING       8448"
            ),
            Some((9018, 8448))
        );
        assert_eq!(
            netstat_listen_pid(
                "  TCP    0.0.0.0:9017            127.0.0.1:5          ESTABLISHED     999"
            ),
            None
        );
        assert_eq!(
            netstat_listen_pid("Active Internet connections (only servers):"),
            None
        );
        assert_eq!(netstat_listen_pid("  UDP    0.0.0.0:9017            *:*                            1"), None);
    }

    #[test]
    fn 状态码按期望集合归类() {
        let web = StatusSet::WEB;
        let gw = StatusSet::GATEWAY;
        assert_eq!(classify(200, &web), L2::Ok(200));
        assert_eq!(classify(302, &web), L2::Ok(302));
        assert_eq!(classify(404, &web), L2::Unexpected(404));
        assert_eq!(classify(404, &gw), L2::Ok(404)); // spec §5: 网关接受 404
        assert_eq!(classify(403, &gw), L2::Unexpected(403)); // 不设"非5xx即活"
        assert_eq!(classify(500, &gw), L2::ServerError(500));
        assert_eq!(classify(502, &web), L2::ServerError(502));
    }
}
