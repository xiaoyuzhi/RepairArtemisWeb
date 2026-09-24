// ============================================================================
// logs — HTTP 状态行解析、日志尾部读取与故障特征归因 (纯逻辑, 可单测)
// ============================================================================

/// 从 HTTP 状态行取状态码。仅接受 `HTTP/1.x <3位数字>` 形态。
pub fn parse_status_line(line: &str) -> Option<u16> {
    let rest = line.trim_start().strip_prefix("HTTP/")?;
    let (_ver, after) = rest.split_once(char::is_whitespace)?;
    let code = after.split_whitespace().next()?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    code.parse().ok()
}

// ---- 日志尾部读取与特征归因 (spec §8) --------------------------------------

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// spec §8: 单次上限 64KB, 禁止整文件读入。
pub const TAIL_LIMIT_BYTES: u64 = 64 * 1024;

/// 定位到尾部窗口后顺序读取, 避免整文件进内存。
pub fn tail_file(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let want = len.min(max) as usize;
    if want == 0 {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(len - want as u64))?;
    let mut buf = vec![0u8; want];
    let mut filled = 0usize;
    while filled < want {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// spec §8 特征表。命中即给结论; 未命中返回 None, 不编造。
/// 顺序即优先级, 两版必须逐条一致。
pub fn classify_log(text: &str) -> Option<&'static str> {
    const SIGS: &[(&str, &str)] = &[
        ("ECONNREFUSED", "上游依赖拒绝连接"),
        ("Connection refused", "上游依赖拒绝连接"),
        ("EADDRINUSE", "端口冲突"),
        ("address already in use", "端口冲突"),
        ("OutOfMemoryError", "JVM 堆不足"),
        ("heap size", "JVM 堆不足"),
        ("ECONNRESET", "与网关或数据库的连接被重置"),
    ];
    SIGS.iter().find(|(k, _)| text.contains(k)).map(|(_, v)| *v)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn 解析常见状态行() {
        assert_eq!(parse_status_line("HTTP/1.1 200 OK"), Some(200));
        assert_eq!(parse_status_line("HTTP/1.0 302 Found"), Some(302));
        assert_eq!(parse_status_line("HTTP/1.1 404 Not Found\r"), Some(404));
        assert_eq!(parse_status_line("HTTP/1.1 502 Bad Gateway"), Some(502));
    }

    #[test]
    fn 非状态行一律返回None() {
        // 回归: 旧实现只找 "HTTP" 四个字节, 这些全会被误判为健康
        assert_eq!(parse_status_line(""), None);
        assert_eq!(parse_status_line("<html>HTTP is fine</html>"), None);
        assert_eq!(parse_status_line("HTTP/1.1 20 OK"), None);
        assert_eq!(parse_status_line("HTTP/1.1 20000"), None);
        assert_eq!(parse_status_line("ICE/1.0 200"), None);
    }

    // ---- Task 5: 反向 tail 与特征归因 ----

    #[test]
    fn 反向tail不超过上限且取到尾部() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("raw_t5_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big.log");
        {
            let mut w = std::fs::File::create(&f).unwrap();
            for i in 0..20000 {
                writeln!(w, "line {:06} padding padding padding padding", i).unwrap();
            }
        }
        let got = tail_file(&f, 4096).unwrap();
        assert!(got.len() as u64 <= 4096, "读到 {} 字节, 超过上限", got.len());
        let s = String::from_utf8_lossy(&got);
        assert!(s.contains("line 019999"), "尾部内容缺失");
        assert!(!s.contains("line 000000"), "不该读到头部");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 空文件与小于上限的文件都安全() {
        let dir = std::env::temp_dir().join(format!("raw_t5b_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("e.log");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(tail_file(&empty, 4096).unwrap().len(), 0);
        let small = dir.join("s.log");
        std::fs::write(&small, b"hello\n").unwrap();
        assert_eq!(tail_file(&small, 4096).unwrap(), b"hello\n");
        assert!(tail_file(&dir.join("nope.log"), 4096).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 特征匹配给出结论而非甩锅() {
        let read = |n: &str| {
            std::fs::read_to_string(crate::model::fixture_path(n)).unwrap()
        };
        assert_eq!(classify_log(&read("err-econnrefused.log")), Some("上游依赖拒绝连接"));
        assert_eq!(classify_log(&read("err-eaddrinuse.log")), Some("端口冲突"));
        assert_eq!(classify_log("GET /artemis-web/ 200 1ms"), None);
    }
}
