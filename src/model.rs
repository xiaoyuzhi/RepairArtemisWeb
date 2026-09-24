// ============================================================================
// model — 组件描述表与配置解析 (纯逻辑, 可单测)
// ============================================================================

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 解析 Java/.NET 风格 properties。
/// 跳过以 `#` 或 `!` 开头的注释行；键与第一个 `=` 两侧空白剥离；
/// 值保留其余部分原样（含后续 `=`、`:`、空格）。
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

// ---- 组件描述表 (spec §5) --------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Node,
    Prunsrv,
}

/// 状态码期望集合。spec §5: 不设"非 5xx 即活"的宽泛规则。
#[derive(Clone, Copy)]
pub struct StatusSet {
    pub two: bool,
    pub three: bool,
    pub four_zero_four: bool,
}

impl StatusSet {
    pub const WEB: StatusSet = StatusSet { two: true, three: true, four_zero_four: false };
    pub const GATEWAY: StatusSet = StatusSet { two: true, three: true, four_zero_four: true };

    pub fn accepts(&self, code: u16) -> bool {
        (self.two && (200..300).contains(&code))
            || (self.three && (300..400).contains(&code))
            || (self.four_zero_four && code == 404)
    }
}

pub struct ComponentDef {
    pub key: &'static str,
    pub kind: Kind,
    /// 相对 OpenAPI root 的组件目录
    pub rel_dir: &'static str,
    /// 存在性判据: 相对组件目录的文件
    pub present_marker: &'static str,
    pub default_svc: &'static str,
    pub default_port: u16,
    pub default_pathname: &'static str,
    pub l2_ok: StatusSet,
    pub start_timeout_secs: u64,
}

/// spec §5 组件描述表。顺序即修复顺序: 网关先于 web/portal。
pub const COMPONENTS: &[ComponentDef] = &[
    ComponentDef {
        key: "artemis",
        kind: Kind::Prunsrv,
        rel_dir: "bin/artemis",
        present_marker: "bin/windows/artemis.exe",
        default_svc: "artemis",
        default_port: 9016,
        default_pathname: "/artemis",
        l2_ok: StatusSet::GATEWAY,
        start_timeout_secs: 90,
    },
    ComponentDef {
        key: "artemis-web",
        kind: Kind::Node,
        rel_dir: "bin/artemis-web/artemis-web",
        present_marker: "koa-app.js",
        default_svc: "artemis-web",
        default_port: 9017,
        default_pathname: "/artemis-web",
        l2_ok: StatusSet::WEB,
        start_timeout_secs: 60,
    },
    ComponentDef {
        key: "artemis-portal",
        kind: Kind::Node,
        rel_dir: "bin/artemis-portal/artemis-portal",
        present_marker: "koa-app.js",
        default_svc: "artemis-portal",
        default_port: 9018,
        default_pathname: "/artemis-portal",
        l2_ok: StatusSet::WEB,
        start_timeout_secs: 60,
    },
];

pub struct ResolvedComponent {
    pub def: &'static ComponentDef,
    pub dir: PathBuf,
    pub svc_name: String,
    pub port: u16,
    pub pathname: String,
    pub present: bool,
    pub warnings: Vec<String>,
}

/// 从 `__service.bat` 取 `set _ServerName=`。
/// 注意: 循环内不得用 `?` 早退, 否则首个不匹配行就会终止整个查找。
pub fn parse_server_name_from_bat(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("set") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix("_ServerName") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix('=') else { continue };
        let v = rest.trim().trim_matches('"');
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn props_for(def: &ComponentDef, dir: &Path) -> HashMap<String, String> {
    // Node: config.properties; Prunsrv: application.properties
    let name = match def.kind {
        Kind::Node => "config.properties",
        Kind::Prunsrv => "application.properties",
    };
    match fs::read_to_string(dir.join(name)) {
        Ok(t) => parse_properties(&t),
        Err(_) => HashMap::new(),
    }
}

/// 读配置得到 svc/port/pathname; 任一失败则用兜底值并记 Warn (spec §5)。
pub fn resolve_component(def: &'static ComponentDef, root: &Path) -> ResolvedComponent {
    let dir = root.join(def.rel_dir);
    let mut warnings = Vec::new();
    let p = props_for(def, &dir);

    let svc_name = match def.kind {
        Kind::Prunsrv => fs::read_to_string(dir.join("bin").join("__service.bat"))
            .ok()
            .and_then(|t| parse_server_name_from_bat(&t))
            .unwrap_or_else(|| {
                warnings.push(format!("未读到 _ServerName, 用兜底服务名 {}", def.default_svc));
                def.default_svc.to_string()
            }),
        Kind::Node => p.get("service.name").cloned().unwrap_or_else(|| {
            warnings.push(format!("未读到 service.name, 用兜底服务名 {}", def.default_svc));
            def.default_svc.to_string()
        }),
    };

    let port = p
        .get("server.port")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or_else(|| {
            warnings.push(format!("未读到 server.port, 用兜底端口 {}", def.default_port));
            def.default_port
        });

    let key = match def.kind {
        Kind::Prunsrv => "server.context-path",
        Kind::Node => "server.pathname",
    };
    let pathname = p
        .get(key)
        .cloned()
        .unwrap_or_else(|| {
            warnings.push(format!("未读到 {}, 用兜底路径 {}", key, def.default_pathname));
            def.default_pathname.to_string()
        })
        .trim_end_matches('/')
        .to_string();

    let present = dir.join(def.present_marker).is_file();
    ResolvedComponent { def, dir, svc_name, port, pathname, present, warnings }
}

// ---- 三层探针结果与归因矩阵 (spec §6) --------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum L1 {
    Listening,
    NotListening,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum L2 {
    /// 状态码落在该组件期望集合
    Ok(u16),
    /// 5xx: 应用在跑但内部出错
    ServerError(u16),
    /// 有 HTTP 响应, 不在期望集合也非 5xx
    Unexpected(u16),
    /// TCP 连上却拿不到状态行 -> 多半是别的进程占了这个端口
    NoHttpResponse,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum L3 {
    Ok(u16),
    /// 拿到了响应但不符合期望 (含 404: 说明没被转发而是当静态文件)
    Bad(u16),
    /// 探针原始输出, 待 probe::classify_l3 归类
    Raw(u16),
    /// TLS 层不可用, 端到端无法验证 (Win7 仅 TLS1.0 / 握手失败)
    TlsUnavailable,
    /// 443 端口不可达
    NginxDown,
    /// --no-e2e
    Skipped,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VerdictAction {
    None,
    RepairBackend,
    ReportOccupier,
    ShowLog,
    NginxAttrib,
    Unverifiable,
}

pub struct RouteVerdict {
    pub cause: &'static str,
    pub action: VerdictAction,
    /// 该路由对进程退出码的贡献 (0/1/3), 取多路由最大值
    pub exit_contrib: u8,
}

/// spec §6 归因矩阵。自上而下首次匹配。纯函数: 探针结果由调用方注入。
pub fn attribute(l1: L1, l2: Option<L2>, l3: Option<L3>) -> RouteVerdict {
    macro_rules! v {
        ($c:expr, $a:expr, $e:expr) => {
            RouteVerdict { cause: $c, action: $a, exit_contrib: $e }
        };
    }
    if l1 == L1::NotListening {
        return v!("后端未监听端口", VerdictAction::RepairBackend, 1);
    }
    match (l2, l3) {
        (Some(L2::NoHttpResponse), _) => {
            v!("端口已监听但无 HTTP 响应 (疑被非 HTTP 进程占用)", VerdictAction::ReportOccupier, 1)
        }
        (Some(L2::ServerError(_)), _) => {
            v!("应用已启动但返回 5xx", VerdictAction::ShowLog, 1)
        }
        (Some(L2::Unexpected(_)), _) => {
            v!("后端响应不符合期望状态码", VerdictAction::RepairBackend, 1)
        }
        (Some(L2::Ok(_)), Some(L3::Bad(_))) | (Some(L2::Ok(_)), Some(L3::NginxDown)) => {
            v!("后端健康, nginx 转发层故障", VerdictAction::NginxAttrib, 3)
        }
        // 防御: 调用方漏归类时不得把 Raw 当成正常, 也不能当成 nginx 故障
        (Some(L2::Ok(_)), Some(L3::Raw(_))) => {
            v!("端到端结果未经归类", VerdictAction::Unverifiable, 0)
        }
        (Some(L2::Ok(_)), Some(L3::TlsUnavailable)) => {
            v!("端到端无法验证 (TLS 层不可用)", VerdictAction::Unverifiable, 0)
        }
        (Some(L2::Ok(_)), _) => v!("正常", VerdictAction::None, 0),
        (None, _) => v!("后端未响应, 探针未取到结果", VerdictAction::RepairBackend, 1),
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

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

    // ---- Task 2: 组件描述表与解析 ----

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("raw_t2_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_tree(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
    }

    #[test]
    fn node组件从config_properties取服务名端口与路径() {
        let root = temp_root("node");
        let dir = "bin/artemis-web/artemis-web";
        write_tree(
            &root,
            &format!("{}/config.properties", dir),
            "server.port = 9017\nserver.pathname = /artemis-web/\nservice.name = artemis-web\n",
        );
        write_tree(&root, &format!("{}/koa-app.js", dir), "// app");
        let c = resolve_component(&COMPONENTS[1], &root);
        assert_eq!(c.svc_name, "artemis-web");
        assert_eq!(c.port, 9017);
        // 尾部斜杠必须剥掉, 否则拼出的探测路径会出现 //
        assert_eq!(c.pathname, "/artemis-web");
        assert!(c.present);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn 配置缺失时退回兜底值并逐项记录告警() {
        let root = temp_root("fallback");
        let c = resolve_component(&COMPONENTS[2], &root);
        assert_eq!(c.port, 9018);
        assert_eq!(c.pathname, "/artemis-portal");
        assert_eq!(c.svc_name, "artemis-portal");
        assert!(!c.present, "目录不存在时 present 必须为 false");
        // svc_name / port / pathname 三项都走了兜底
        assert_eq!(c.warnings.len(), 3, "{:?}", c.warnings);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn 网关从脚本与application_properties取值() {
        let root = temp_root("gw");
        write_tree(
            &root,
            "bin/artemis/application.properties",
            "server.port=9016\nserver.context-path=/artemis\n",
        );
        write_tree(&root, "bin/artemis/bin/__service.bat", "@echo off\nset _ServerName=artemis\n");
        write_tree(&root, "bin/artemis/bin/windows/artemis.exe", "");
        let c = resolve_component(&COMPONENTS[0], &root);
        assert_eq!(c.svc_name, "artemis");
        assert_eq!(c.port, 9016);
        assert_eq!(c.pathname, "/artemis");
        assert!(c.present);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn 网关不得被当成node组件处理() {
        // Task 8 的 plan_repair 按 kind 分派; 表格填错会让网关去跑 service.install.js
        assert!(matches!(COMPONENTS[0].kind, Kind::Prunsrv));
        assert!(matches!(COMPONENTS[1].kind, Kind::Node));
        assert!(matches!(COMPONENTS[2].kind, Kind::Node));
        // spec §5: 网关启动更慢, 等待窗口必须更长
        assert!(COMPONENTS[0].start_timeout_secs > COMPONENTS[1].start_timeout_secs);
    }

    #[test]
    fn 网关服务名取自脚本内变量() {
        let bat = "\
@echo off
::Server Register Info
set _ServerName=artemis
set _DisplayName=artemis
";
        assert_eq!(parse_server_name_from_bat(bat).as_deref(), Some("artemis"));
        assert_eq!(parse_server_name_from_bat("nope"), None);
        // 等号两侧带空格与引号包裹也是合法形态
        assert_eq!(
            parse_server_name_from_bat("set _ServerName = \"artemis2\"").as_deref(),
            Some("artemis2")
        );
    }

    #[test]
    fn 期望集合按组件区分() {
        // spec §5: 网关额外接受 404, web/portal 不接受
        assert!(StatusSet::GATEWAY.accepts(404));
        assert!(!StatusSet::WEB.accepts(404));
        assert!(StatusSet::WEB.accepts(200));
        assert!(StatusSet::WEB.accepts(302));
        // 不设"非 5xx 即活": 403 对两者都是失败
        assert!(!StatusSet::GATEWAY.accepts(403));
        assert!(!StatusSet::GATEWAY.accepts(500));
    }

    // ---- Task 3: 归因矩阵 ----
    // 注意: 不要 `use L2::*` 与 `use L3::*` 同时导入 —— 两者都有 `Ok` 变体会歧义;
    // `VerdictAction::None` 也会遮蔽 `Option::None`。一律用全限定路径。
    #[test]
    fn 归因矩阵逐行匹配spec() {
        use crate::model::{attribute, L1, L2, L3, VerdictAction as A};
        let cases: Vec<(L1, Option<L2>, Option<L3>, A, u8)> = vec![
            // 行1 正常
            (L1::Listening, Some(L2::Ok(200)), Some(L3::Ok(200)), A::None, 0),
            // 行2 后端未监听
            (L1::NotListening, None, None, A::RepairBackend, 1),
            // 行3 端口被非 HTTP 进程占用 (Review Focus #5)
            (L1::Listening, Some(L2::NoHttpResponse), None, A::ReportOccupier, 1),
            // 行4 5xx -> 出日志, 不重装
            (L1::Listening, Some(L2::ServerError(500)), None, A::ShowLog, 1),
            // 行5 后端健康但 nginx 转发层故障 -> 不得重启后端
            (L1::Listening, Some(L2::Ok(200)), Some(L3::Bad(502)), A::NginxAttrib, 3),
            // 行6 nginx 443 不可达
            (L1::Listening, Some(L2::Ok(302)), Some(L3::NginxDown), A::NginxAttrib, 3),
            // 行7 非期望状态码 -> 先修后端
            (L1::Listening, Some(L2::Unexpected(404)), None, A::RepairBackend, 1),
            // Review Focus #4: TLS 不可用不得判成后端或 nginx 故障
            (L1::Listening, Some(L2::Ok(200)), Some(L3::TlsUnavailable), A::Unverifiable, 0),
            // --no-e2e 时 L3 为 Skipped, 等价于未评估
            (L1::Listening, Some(L2::Ok(200)), Some(L3::Skipped), A::None, 0),
        ];
        for (l1, l2, l3, want_action, want_exit) in cases {
            let v = attribute(l1, l2, l3);
            assert_eq!(v.action, want_action, "l1={:?} l2={:?} l3={:?}", l1, l2, l3);
            assert_eq!(v.exit_contrib, want_exit);
            assert!(!v.cause.is_empty());
        }
    }

    #[test]
    fn 后端健康时绝不因端到端失败而重启后端() {
        // spec §6 核心不变式
        use crate::model::{attribute, L1, L2, L3, VerdictAction};
        for l3 in [L3::Bad(502), L3::Bad(404), L3::NginxDown, L3::TlsUnavailable, L3::Skipped] {
            let v = attribute(L1::Listening, Some(L2::Ok(200)), Some(l3));
            assert_ne!(v.action, VerdictAction::RepairBackend, "L3={:?} 触发了后端修复", l3);
        }
    }
}
