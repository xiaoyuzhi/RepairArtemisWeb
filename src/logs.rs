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
}
