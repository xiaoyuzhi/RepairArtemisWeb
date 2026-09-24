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
        L3::TlsUnavailable if !nginx_up => L3::NginxDown,
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

// ---- L3: 经本机 nginx 443 的端到端验收 (spec §6 L3 / 决策 D1b) --------------
//
// 原方案是 WinHTTP FFI, 执行期被实测推翻: 对本平台 nginx, WinHttpSendRequest 恒返回
// 12175, 而同一端点 curl.exe 返回 200; 服务器证书无 CN/无 SAN, WinHTTP 的 INVALID_CA
// 一类失败没有对应的忽略位。详见 spec D1b。

use crate::model::L3;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::process::Command;

/// curl 的 -w 输出 -> L3。"000" 是 curl 拿不到任何 HTTP 响应时的固定输出。
pub fn parse_curl_output(exit_ok: bool, stdout: &str) -> L3 {
    let code = stdout.trim();
    match code.parse::<u16>() {
        Ok(c) if c != 0 && exit_ok => L3::Raw(c),
        _ => L3::TlsUnavailable,
    }
}

/// host 必须是裸主机名/IP。首字符为 '-' 会被 curl 当成选项, 空白与元字符一律拒绝。
pub fn valid_e2e_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
}

#[link(name = "kernel32")]
extern "system" {
    fn GetSystemDirectoryW(buf: *mut u16, size: u32) -> u32;
}

/// 系统目录向内核要, 不读 %WINDIR%: 环境变量可被同一会话内的任何进程改写,
/// 那等于把"用绝对路径避免被同名程序劫持"这一层重新交还给环境 (spec D1b 的原意)。
fn system32_dir() -> Option<std::path::PathBuf> {
    unsafe {
        let mut buf = [0u16; 260];
        let mut n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32);
        if n == 0 {
            return None;
        }
        if n as usize > buf.len() {
            let mut big = vec![0u16; n as usize + 1];
            n = GetSystemDirectoryW(big.as_mut_ptr(), big.len() as u32);
            if n == 0 || n as usize > big.len() {
                return None;
            }
            return Some(std::path::PathBuf::from(OsString::from_wide(&big[..n as usize])));
        }
        Some(std::path::PathBuf::from(OsString::from_wide(&buf[..n as usize])))
    }
}

fn curl_exe() -> Option<std::path::PathBuf> {
    let p = system32_dir()?.join("curl.exe");
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// 经 nginx 443 取真实状态码。curl 缺失或 host 非法 -> Skipped (降级, 不伪造)。
pub fn https_probe(host: &str, port: u16, pathname: &str) -> L3 {
    let exe = match curl_exe() {
        Some(e) => e,
        None => return L3::Skipped,
    };
    if !valid_e2e_host(host) {
        return L3::Skipped;
    }
    let url = format!("https://{}:{}{}", host, port, pathname);
    // 绝对路径调用 + 不经 shell, 参数以数组传入, 无注入面
    let out = match Command::new(&exe)
        .args(["-s", "-k", "-o", "NUL", "-m", "10", "-w", "%{http_code}"])
        .arg(&url)
        .output()
    {
        Ok(o) => o,
        Err(_) => return L3::TlsUnavailable,
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    parse_curl_output(out.status.success(), &stdout)
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

    // ---- Task 7b: L3 改由 curl.exe 承载 (spec D1b) ----

    #[test]
    fn curl输出解析成L3() {
        assert_eq!(parse_curl_output(true, "200"), L3::Raw(200));
        assert_eq!(parse_curl_output(true, "302"), L3::Raw(302));
        assert_eq!(parse_curl_output(true, "502"), L3::Raw(502));
        // 末尾换行与空白不得影响判定
        assert_eq!(parse_curl_output(true, "200\n"), L3::Raw(200));
        // curl 拿不到任何 HTTP 响应时打印 000
        assert_eq!(parse_curl_output(true, "000"), L3::TlsUnavailable);
        assert_eq!(parse_curl_output(false, "000"), L3::TlsUnavailable);
        assert_eq!(parse_curl_output(true, ""), L3::TlsUnavailable);
        assert_eq!(parse_curl_output(true, "garbage"), L3::TlsUnavailable);
    }

    #[test]
    fn host必须白名单校验否则跳过() {
        // 以 '-' 开头会被 curl 当成命令行选项; 空白与 shell 元字符一律拒绝
        assert!(valid_e2e_host("127.0.0.1"));
        assert!(valid_e2e_host("localhost"));
        assert!(valid_e2e_host("192.168.1.10"));
        assert!(!valid_e2e_host("-o"));
        assert!(!valid_e2e_host("a b"));
        assert!(!valid_e2e_host(""));
        assert!(!valid_e2e_host("127.0.0.1;calc"));
        assert!(!valid_e2e_host("127.0.0.1/x"));
    }

    #[test]
    fn 端口四四三未监听时TLS失败也归为nginx未起() {
        // spec D1b: curl 不可用/握手失败时, 若 443 本就没起来, 结论应是 nginx 未就绪
        assert_eq!(classify_l3(L3::TlsUnavailable, &StatusSet::WEB, false), L3::NginxDown);
        // 但 --no-e2e 的 Skipped 不能被改写成结论
        assert_eq!(classify_l3(L3::Skipped, &StatusSet::WEB, false), L3::Skipped);
    }
}
