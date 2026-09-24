// ============================================================================
// model — 组件描述表与配置解析 (纯逻辑, 可单测)
// ============================================================================

use std::collections::HashMap;

/// 解析 Java/.NET 风格 properties。
/// 跳过以 `#` 或 `!` 开头的注释行；键与第一个 `=` 两侧空白剥离；
/// 值保留其余部分原样（含后续 `=`、`:`、空格）。
// Task 8 的 dump_component_logs 起被生产路径调用。
#[allow(dead_code)]
pub fn parse_properties(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim_end().to_string();
        let value = line[eq + 1..].trim_start().to_string();
        if key.is_empty() {
            continue;
        }
        m.insert(key, value);
    }
    m
}

/// 测试辅助：定位仓库内夹具。仅测试构建可见，不进生产二进制。
#[cfg(test)]
pub fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::fs;

    fn load(n: &str) -> HashMap<String, String> {
        parse_properties(&fs::read_to_string(fixture_path(n)).unwrap())
    }

    #[test]
    fn 注释行不得成为生效值() {
        let p = load("artemis-web.config.properties");
        assert_eq!(p.get("server.port").map(|s| s.as_str()), Some("9017"));
        assert_eq!(
            p.get("service.name").map(|s| s.as_str()),
            Some("artemis-web")
        );
        assert!(!p.contains_key("#server.port"));
        assert!(!p.contains_key("#service.name"));
    }

    #[test]
    fn 值中的等号与空格必须完整保留() {
        let p = load("artemis-gateway.application.properties");
        assert_eq!(
            p.get("spring.datasource.url").map(|s| s.as_str()),
            Some("jdbc:postgresql://127.0.0.1:5432/artemis")
        );
        assert_eq!(
            p.get("server.context-path").map(|s| s.as_str()),
            Some("/artemis")
        );
        assert_eq!(p.get("server.port").map(|s| s.as_str()), Some("9016"));
    }

    #[test]
    fn 无等号与空键行被忽略() {
        let p = parse_properties("lonely\n=novalue\n  =also\nok=1");
        assert_eq!(p.get("ok").map(|s| s.as_str()), Some("1"));
        assert_eq!(p.len(), 1);
    }
}
