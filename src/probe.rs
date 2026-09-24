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

/// 443 未监听 -> NginxDown; 否则按期望集合归类。5xx 与非期望一律 Bad。
pub fn classify_l3(raw: L3, set: &StatusSet, nginx_up: bool) -> L3 {
    match raw {
        L3::Raw(_) if !nginx_up => L3::NginxDown,
        L3::Raw(c) => {
            if (500..600).contains(&c) || !set.accepts(c) {
                L3::Bad(c)
            } else {
                L3::Ok(c)
            }
        }
        other => other,
    }
}

// ---- WinHTTP FFI (无 crate; 端到端 HTTPS 验收, spec §6 L3 / 决策 D1) --------
use crate::model::L3;
use std::ffi::c_void;

#[link(name = "winhttp")]
extern "system" {
    fn WinHttpOpen(ua: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> *mut c_void;
    fn WinHttpConnect(s: *mut c_void, server: *const u16, port: u16, reserved: u32) -> *mut c_void;
    fn WinHttpOpenRequest(c: *mut c_void, verb: *const u16, obj: *const u16, ver: *const u16, refr: *const u16, types: *const *const u16, flags: u32) -> *mut c_void;
    fn WinHttpSetOption(h: *mut c_void, opt: u32, val: *mut c_void, size: u32) -> i32;
    fn WinHttpSendRequest(r: *mut c_void, headers: *const u16, hlen: u32, data: *mut c_void, dlen: u32, total: u32, ctx: usize) -> i32;
    fn WinHttpReceiveResponse(r: *mut c_void, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(r: *mut c_void, level: u32, name: *const u16, buf: *mut c_void, buflen: *mut u32, idx: *mut u32) -> i32;
    fn WinHttpCloseHandle(h: *mut c_void) -> i32;
}

const WT_NO_PROXY: u32 = 1;
const WT_FLAG_SECURE: u32 = 0x0080_0000;
const WT_OPT_SECURITY_FLAGS: u32 = 31;
const WT_QUERY_STATUS_CODE: u32 = 19;
const WT_QUERY_FLAG_NUMBER: u32 = 0x2000;
// 忽略自签/域名不符/过期/用途不符: 目标是探状态码, 不是校验证书链
const SEC_IGNORE_ALL: u32 = 0x0200 | 0x1000 | 0x2000;

fn u16z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 经 nginx 443 的端到端验收 (spec §6 L3)。任何 TLS/连接层失败一律返回
/// TlsUnavailable, 不得伪装成 Bad —— 见 Review Focus #4。
///
/// 尚未通过验证 (阻塞中): 对本机 iSecure VMS 的 nginx (nginx.conf 第 284 行
/// `ssl_protocols TLSv1.1 TLSv1.2;` + `ssl_prefer_server_ciphers on`),
/// WinHttpSendRequest 一律返回 12175 ERROR_WINHTTP_SECURE_FAILURE, 即使已设置
/// WINHTTP_OPTION_SECURITY_FLAGS 的全部证书忽略位; 而同一端点 curl.exe 实测 200。
/// 对公网 TLS 站点本函数可正常收发, 故不是调用序列错误。
/// 见 ledger "Task 7 阻塞"。决策前不得接入 Task 9。
pub fn https_probe(host: &str, port: u16, pathname: &str) -> L3 {
    unsafe {
        let session = WinHttpOpen(
            u16z("RepairArtemisWeb").as_ptr(),
            WT_NO_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if session.is_null() {
            return L3::TlsUnavailable;
        }
        let conn = WinHttpConnect(session, u16z(host).as_ptr(), port, 0);
        let req = if conn.is_null() {
            std::ptr::null_mut()
        } else {
            WinHttpOpenRequest(
                conn,
                u16z("GET").as_ptr(),
                u16z(pathname).as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                WT_FLAG_SECURE,
            )
        };
        if req.is_null() {
            if !conn.is_null() {
                WinHttpCloseHandle(conn);
            }
            WinHttpCloseHandle(session);
            return L3::TlsUnavailable;
        }
        // 只忽略证书链相关的校验位: 我们要的是状态码, 不是信任判断。
        // 注意: 不设 WINHTTP_OPTION_SECURE_PROTOCOLS —— 实测设在 session 上会让
        // WinHttpSendRequest 一律返回 12175 (含对公网正规证书), 设在 request 上返回 12018。
        // 老平台 (Win7 默认仅 TLS1.0) 握手失败会如实落到 TlsUnavailable, 符合 Review Focus #4。
        let mut flags: u32 = SEC_IGNORE_ALL;
        WinHttpSetOption(req, WT_OPT_SECURITY_FLAGS, &mut flags as *mut u32 as *mut c_void, 4);

        let sent = WinHttpSendRequest(req, std::ptr::null(), 0, std::ptr::null_mut(), 0, 0, 0);
        let got = if sent == 0 { 0 } else { WinHttpReceiveResponse(req, std::ptr::null_mut()) };
        if got == 0 {
            WinHttpCloseHandle(req);
            WinHttpCloseHandle(conn);
            WinHttpCloseHandle(session);
            return L3::TlsUnavailable;
        }
        let mut code: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let ok = WinHttpQueryHeaders(
            req,
            WT_QUERY_STATUS_CODE | WT_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut code as *mut u32 as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
        );
        WinHttpCloseHandle(req);
        WinHttpCloseHandle(conn);
        WinHttpCloseHandle(session);
        if ok == 0 {
            return L3::TlsUnavailable;
        }
        L3::Raw(code as u16)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;    use crate::model::{L2, StatusSet};

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

    // ---- Task 7: L3 端到端归类 ----

    #[test]
    fn l3归类区分回环故障与nginx未起() {
        use crate::model::L3;
        let web = StatusSet::WEB;
        assert_eq!(classify_l3(L3::Raw(200), &web, true), L3::Ok(200));
        assert_eq!(classify_l3(L3::Raw(502), &web, true), L3::Bad(502));
        // spec §3: 经 nginx 后 404 = 没被转发, 属故障
        assert_eq!(classify_l3(L3::Raw(404), &web, true), L3::Bad(404));
        // Review Focus #4 相关: 443 不通时不得判成后端故障
        assert_eq!(classify_l3(L3::Raw(200), &web, false), L3::NginxDown);
        assert_eq!(classify_l3(L3::TlsUnavailable, &web, true), L3::TlsUnavailable);
    }

    #[test]
    fn 未归类的Raw不得被当成正常() {
        use crate::model::{attribute, L1, L2, L3, VerdictAction};
        let v = attribute(L1::Listening, Some(L2::Ok(200)), Some(L3::Raw(200)));
        assert_eq!(v.action, VerdictAction::Unverifiable);
    }
}
