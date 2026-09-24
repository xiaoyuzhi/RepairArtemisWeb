# RepairArtemisWeb 三层探针与 nginx 归因 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 RepairArtemisWeb 从"两个 node 组件 + 弱 HTTP 判定"升级为"三组件（含 9016 Java 网关）+ 三层探针 + nginx 只读归因"，使其不再在"后端健康但转发层故障"时误报成功。

**Architecture:** 纯逻辑（组件表、properties/nginx 解析、归因矩阵、日志反向 tail）下沉到可单测的模块，I/O（netstat/TCP/HTTP/WinHTTP/sc.exe/服务脚本）留在边界外。归因是纯函数 `attribute(l1,l2,l3)`，探针结果由调用方注入。Rust 与 C++ 两版结构逐条对齐。

**Tech Stack:** Rust 2021（仅 `std` + Win32 FFI）、C++17（g++/MSYS2，仅 Win32 API）、WinHTTP（`winhttp.dll`，端到端 HTTPS）、`tests/fixtures/` 文本夹具。

**Spec:** `docs/superpowers/specs/2026-09-24-artemis-three-layer-probe-design.md`

## Global Constraints

- 零第三方依赖：Rust 只用 `std` + `#[link]` FFI；C++ 只用 Win32 API 与标准库。不得引入任何 crate 或外部可执行文件。（spec N4）
- 单文件免运行库：C++ 保持 `-static`，Rust 保持 `.cargo/config.toml` 的静态 CRT。
- 平台下限 Windows 7+；中文输出不得乱码（控制台 UTF-8 自适应逻辑保持）。
- 两版函数命名一一对应：Rust `snake_case` ↔ C++ 同名 `snake_case`，类型名一致（`ComponentDef`/`StatusSet`/`L1`/`L2`/`L3`/`RouteVerdict`/`NginxInfo`）。
- C++ 转写契约（每个任务的 "C++ 同步" 步骤依赖此条，不算占位符）：Rust 步骤给出的完整函数体即规范实现，C++ 按以下固定替换转写 —— `String`→`std::string`、路径→`fs::path`/`std::wstring`、`Option<T>`→`bool` 出参或哨兵值（注释标明用哪个）、`Vec<T>`→`std::vector<T>`、`HashMap`→`std::map<std::string,std::string>`、`&str` 字面量保持 `const char*`、`format!`→`logf`/`snprintf`。C++ 无异常穿越函数边界。仅当某步骤显式给出 C++ 代码时以该代码为准（WinHTTP、prunsrv 两处属于显式给出）。
- C++ 版不引入测试框架；其正确性由 Task 10 的真机 `--check-only` 逐行 diff 验收。（spec §11）
- 退出码：`0` 无异常 · `1` 异常/修复失败 · `2` 参数错误 · `3` 后端健康但 nginx 转发层故障。（spec D3）
- 提交信息用中文，风格对齐 `git log`（如"新增 README 项目说明文档"）。

## Review Focus

spec 未逐条规定、但使用本工具的人最先撞上的输入形态。每行在对应任务里被测试钉住。

1. **properties 值里含 `=` 与空格** —— `spring.datasource.url = jdbc:postgresql://127.0.0.1:5432/artemis` 必须解析成完整 URL，不能只取到 `jdbc:postgresql://127.0.0.1:5432/artemis` 之前的片段或在第二个 `=` 处截断。（Task 1）
2. **注释行不得误匹配** —— `#server.port = 6100` 与 `# service.name = %SERVICE_NAME%` 绝不能成为生效值；否则端口会被读成 6100，探针全错。（Task 1）
3. **nginx `location` 块内嵌套花括号** —— `if (...) { ... }` 会让朴素的"找下一个 `}`"提前收尾，把后面 `location` 的 `proxy_pass` 归到错误路由。必须做花括号配对计数。（Task 4）
4. **WinHTTP 在 Win7 只有 TLS 1.0 可用 / 握手失败** —— 必须归为"L3 无法验证"并 Warn，**不得**当成后端故障、也不得触发重装。（Task 7 + Task 3 的 `L3::TlsUnavailable` 分支）
5. **端口被非 HTTP 进程占用** —— `http_probe` 必须带硬超时（连接与读各 8s），拿不到状态行时返回 `NoHttpResponse` 而不是永久阻塞，归因走"报占用 PID、不自动修"。（Task 6 + Task 3）

---

## 文件结构

Rust 侧从单个 `src/main.rs` 拆出纯逻辑模块，使归因与解析可单测；C++ 侧保持单文件（无测试框架，拆分无收益）。

| 文件 | 职责 |
| --- | --- |
| `src/model.rs` | 组件描述表、`StatusSet`、`L1/L2/L3`、`attribute()` 归因纯函数、`parse_properties` |
| `src/logs.rs` | `parse_status_line`、反向分块 tail、`classify_log` 特征匹配 |
| `src/nginx.rs` | `parse_upstreams`、`parse_locations`、`find_artemis_mode`、`resolve_route_port`、nginx 根定位 |
| `src/probe.rs` | L1/L2/L3 的实际 I/O：`port_listening`、`http_probe`、`https_probe`(WinHTTP) |
| `src/main.rs` | CLI 解析、交互菜单、服务控制（sc.exe / `__service.bat`）、修复编排、报告输出 |
| `tests/fixtures/*` | 真实脱敏文本夹具 |
| `cpp/RepairArtemisWeb.cpp` | 上述全部的单文件镜像实现 |
| `cpp/build.bat` | 追加 `-lwinhttp` |

`src/main.rs` 顶部新增 `mod model; mod logs; mod nginx; mod probe;`，并把现有 `port_listening` / `run_output` 等按需迁移；迁移时保持 `main.rs` 对它们的调用点改名为 `probe::` / `model::`。

---

## Task 1: properties 与状态行解析（纯函数 + 夹具）

**Files:**
- Create: `src/model.rs`
- Create: `src/logs.rs`
- Create: `tests/fixtures/artemis-web.config.properties`
- Create: `tests/fixtures/artemis-gateway.application.properties`
- Modify: `src/main.rs:1-32`（加 `mod` 声明）
- Test: `src/model.rs` 与 `src/logs.rs` 内的 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `model::parse_properties(&str) -> std::collections::HashMap<String,String>`
- Produces: `logs::parse_status_line(&str) -> Option<u16>`
- Produces: `model::fixture_path(&str) -> PathBuf`（测试用，拼 `CARGO_MANIFEST_DIR/tests/fixtures`）

- [ ] **Step 1: 建两个夹具文件**

`tests/fixtures/artemis-web.config.properties`（从真机 `bin/artemis-web/artemis-web/config.properties` 脱敏抄录，保留全部注释形态）：

```properties
# node服务
server.host = localhost
#server.port = 6100
server.port = 9017
server.pathname = /artemis-web

# 接口地址
java.pathname = /artemis
java.origin = http://127.0.0.1:9016

log.path = ../../../logs/artemis-web

# service配置
#service.name = %SERVICE_NAME%
service.name = artemis-web

# 连接超时单位秒
connection.waittime = 300
platform = iSecure
```

`tests/fixtures/artemis-gateway.application.properties`（含 Review Focus #1 的带 `=` 值）：

```properties
version=3.3.1
spring.application.name = artemis
server.context-path=/artemis
server.port=9016
server.ssl.enabled=false
spring.datasource.url = jdbc:postgresql://127.0.0.1:5432/artemis
#server.port=19016
```

- [ ] **Step 2: 写失败测试**

`src/model.rs`：

```rust
use std::collections::HashMap;
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

/// 测试辅助：定位仓库内夹具。
pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[cfg(test)]
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
        assert_eq!(p.get("service.name").map(|s| s.as_str()), Some("artemis-web"));
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
        assert_eq!(p.get("server.context-path").map(|s| s.as_str()), Some("/artemis"));
        assert_eq!(p.get("server.port").map(|s| s.as_str()), Some("9016"));
    }

    #[test]
    fn 无等号与空键行被忽略() {
        let p = parse_properties("lonely\n=novalue\n  =also\nok=1");
        assert_eq!(p.get("ok").map(|s| s.as_str()), Some("1"));
        assert_eq!(p.len(), 1);
    }
}
```

`src/logs.rs`：

```rust
/// 从 HTTP 状态行取状态码。仅接受 `HTTP/1.x <3位数字>` 形态。
pub fn parse_status_line(line: &str) -> Option<u16> {
    let rest = line.trim_start().strip_prefix("HTTP/")?;
    let (_ver, code) = rest.split_once(char::is_whitespace)?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    code.parse().ok()
}

#[cfg(test)]
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
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -30`
Expected: 编译错误 `unresolved import crate::model`（`mod` 未声明）或 `parse_status_line` 未定义。

- [ ] **Step 4: 在 `src/main.rs` 顶部声明模块**

在 `src/main.rs:22` 之前（文件头注释块之后、`use std::env;` 之前）插入：

```rust
mod logs;
mod model;
mod nginx;
mod probe;
```

本任务先创建 `src/nginx.rs` 与 `src/probe.rs` 两个空壳（仅 `// 见 Task 4 / Task 6`），否则 `mod` 声明编译不过。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 5 passed`

- [ ] **Step 6: C++ 同步**

在 `cpp/RepairArtemisWeb.cpp:611`（`struct CompConfig` 之前）新增 `parse_properties` 与 `parse_status_line`，按转写契约实现：`std::map<std::string,std::string>`，注释判定用 `line[0]=='#'||line[0]=='!'`，`find('=')` 语义与 Rust 一致（值保留后续 `=`）。C++ 无单测，靠 Task 10 diff 验收。

- [ ] **Step 7: 提交**

```bash
git add src/model.rs src/logs.rs src/nginx.rs src/probe.rs src/main.rs tests/fixtures/ cpp/RepairArtemisWeb.cpp
git commit -m "新增 properties 与 HTTP 状态行解析, 替换原先仅匹配 HTTP 字样的弱判定基础"
```

---

## Task 2: 组件描述表与解析

**Files:**
- Modify: `src/model.rs`
- Create: `tests/fixtures/artemis-service.bat`
- Test: `src/model.rs::tests`

**Interfaces:**
- Consumes: `model::parse_properties`
- Produces: `model::Kind`、`model::StatusSet`、`model::ComponentDef`、`model::COMPONENTS`、`model::ResolvedComponent`、`model::resolve_component(&ComponentDef, &Path) -> ResolvedComponent`、`model::parse_server_name_from_bat(&str) -> Option<String>`

- [ ] **Step 1: 写失败测试**

在 `src/model.rs` 的 `mod tests` 追加：

```rust
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
}

#[test]
fn 网关用application_properties的端口与上下文() {
    let root = fixture_path("fake-root").parent().unwrap().to_path_buf();
    // 直接喂文本, 不依赖真机目录
    let p = parse_properties(
        &fs::read_to_string(fixture_path("artemis-gateway.application.properties")).unwrap(),
    );
    assert_eq!(p.get("server.port").unwrap().parse::<u16>().unwrap(), 9016);
    assert_eq!(p.get("server.context-path").unwrap(), "/artemis");
    let _ = root;
}

#[test]
fn 组件表覆盖三组件且kind正确() {
    assert_eq!(COMPONENTS.len(), 3);
    assert_eq!(COMPONENTS[0].key, "artemis");
    assert!(matches!(COMPONENTS[0].kind, Kind::Prunsrv));
    assert_eq!(COMPONENTS[1].key, "artemis-web");
    assert!(matches!(COMPONENTS[1].kind, Kind::Node));
    assert_eq!(COMPONENTS[2].default_port, 9018);
    // spec §5: 网关启动更慢
    assert!(COMPONENTS[0].start_timeout_secs > COMPONENTS[1].start_timeout_secs);
}
```

`tests/fixtures/artemis-service.bat`：从真机 `bin/artemis/bin/__service.bat` 抄前 6 行（`@echo off` / `::Server Register Info` / `set _ServerName=artemis` / `set _DisplayName=artemis` / `set _Description=artemis` / `set _Startup=auto`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `cannot find function parse_server_name_from_bat in this scope`

- [ ] **Step 3: 写实现**

`src/model.rs` 追加：

```rust
use std::path::Path;

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
pub fn parse_server_name_from_bat(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim_start();
        let rest = t
            .strip_prefix("set _ServerName=")
            .or_else(|| t.strip_prefix("set _ServerName ="))?;
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
        Kind::Prunsrv => {
            let bat = dir.join("bin").join("__service.bat");
            fs::read_to_string(&bat)
                .ok()
                .and_then(|t| parse_server_name_from_bat(&t))
                .unwrap_or_else(|| {
                    warnings.push(format!("未读到 _ServerName, 用兜底服务名 {}", def.default_svc));
                    def.default_svc.to_string()
                })
        }
        Kind::Node => p
            .get("service.name")
            .cloned()
            .unwrap_or_else(|| {
                warnings.push(format!("未读到 service.name, 用兜底服务名 {}", def.default_svc));
                def.default_svc.to_string()
            }),
    };

    let port = match def.kind {
        Kind::Prunsrv => p.get("server.port"),
        Kind::Node => p.get("server.port"),
    }
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
```

需要 `use std::fs;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 8 passed`

- [ ] **Step 5: C++ 同步**

在 `cpp/RepairArtemisWeb.cpp:611` 处用 `Kind`/`StatusSet`/`ComponentDef`/`COMPONENTS`（`static const ComponentDef COMPONENTS[3] = {...}`）/`ResolvedComponent` 替换原 `CompConfig`，`resolve_component` 按转写契约实现（`std::wstring` 路径拼接，`warnings` 为 `std::vector<std::string>`）。删除 `CompConfig` 与 `read_config`（`src/main.rs:391-434` 的对应物），其唯一调用点 `repair_component` 在 Task 9 改造。

- [ ] **Step 6: 提交**

```bash
git add src/model.rs tests/fixtures/ cpp/RepairArtemisWeb.cpp
git commit -m "新增三组件描述表与配置解析, 纳入 9016 Java 网关"
```

---

## Task 3: 归因矩阵纯函数

**Files:**
- Modify: `src/model.rs`
- Test: `src/model.rs::tests`

**Interfaces:**
- Consumes: `model::StatusSet`
- Produces: `model::L1`、`model::L2`、`model::L3`、`model::VerdictAction`、`model::RouteVerdict`、`model::attribute(L1, Option<L2>, Option<L3>) -> RouteVerdict`

- [ ] **Step 1: 写失败测试（表驱动，覆盖 spec §6 全部 7 行 + Review Focus #4）**

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `cannot find type L1 in this scope`

- [ ] **Step 3: 写实现**

```rust
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
            v!("端口已被非 HTTP 进程占用", VerdictAction::ReportOccupier, 1)
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
        (Some(L2::Ok(_)), Some(L3::TlsUnavailable)) => {
            v!("端到端无法验证 (TLS 层不可用)", VerdictAction::Unverifiable, 0)
        }
        // L2 Ok + (Ok | Skipped), 以及 L2 未知的兜底
        (Some(L2::Ok(_)), _) => v!("正常", VerdictAction::None, 0),
        (None, _) => v!("后端未响应, 探针未取到结果", VerdictAction::RepairBackend, 1),
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 10 passed`

- [ ] **Step 5: C++ 同步**

`enum class L1/L2/L3/VerdictAction` + `struct RouteVerdict` + `attribute()`。C++ 无 `Option`，用 `bool has_l2, bool has_l3` 加值参数：`RouteVerdict attribute(L1 l1, bool has2, L2 l2, bool has3, L3 l3)`，语义与 Rust 分支顺序逐条对应（把 Rust 的 `match` 翻成同序 `if` 链，注释标 "首次匹配, 顺序即优先级"）。

- [ ] **Step 6: 提交**

```bash
git add src/model.rs cpp/RepairArtemisWeb.cpp
git commit -m "新增三层探针归因矩阵纯函数, 后端健康时不再因端到端失败重启后端"
```

---

## Task 4: nginx 配置只读解析

**Files:**
- Modify: `src/nginx.rs`（当前为空壳）
- Create: `tests/fixtures/nginx-local/conf/nginx.conf`
- Create: `tests/fixtures/nginx-local/conf/Mode/nginx_artemis.conf`
- Create: `tests/fixtures/nginx-local/ssl/artemis.conf`
- Create: `tests/fixtures/nginx-remote/conf/nginx.conf`
- Create: `tests/fixtures/nginx-remote/conf/Mode/nginx_artemis.conf`
- Create: `tests/fixtures/nginx-remote/ssl/artemis.conf`
- Test: `src/nginx.rs::tests`

**Interfaces:**
- Produces: `nginx::Upstream`、`nginx::RouteTarget`、`nginx::Route`、`nginx::ArtemisMode`、`nginx::NginxInfo`、`nginx::parse_upstreams(&str) -> Vec<Upstream>`、`nginx::parse_locations(&str) -> Vec<Route>`、`nginx::find_artemis_mode(&str, &str) -> Option<ArtemisMode>`、`nginx::load_nginx_info(&Path) -> NginxInfo`、`nginx::resolve_route_port(&NginxInfo, &str) -> Option<u16>`、`nginx::locate_nginx_conf(&Path) -> Option<PathBuf>`

- [ ] **Step 1: 建夹具**

`tests/fixtures/nginx-local/conf/nginx.conf` —— 真机 `Web Service/nginx/conf/nginx.conf` 的 artemis 相关最小切片，保留 `include` 与嵌套 `if` 形态：

```nginx
http {
    include       mime.types;
    upstream http_artemis {
        server 127.0.0.1:9016;
    }
    upstream http_artemis_web {
        server 127.0.0.1:9017;
    }
    upstream http_artemis_portal {
        server 127.0.0.1:9018;
    }
    upstream https_artemis_remote {
        server 127.0.0.1:443;
    }
    server {
        listen       80;
        listen       443 ssl;
        server_name  localhost;
        include ../../ssl/interencrypt.conf;
        include ../../ssl/artemis.conf;
        include Mode/*.conf;
        location / {
            root   ../www;
        }
        location ^~ /artemis-portal {
            if ($scheme = "http") {
                break;
            }
            if ($artemis = "local") {
                proxy_pass http://http_artemis_portal;
            }
            if ($artemis = "remote") {
                proxy_pass https://https_artemis_remote;
            }
            autoindex off;
        }
    }
}
```

`tests/fixtures/nginx-local/conf/Mode/nginx_artemis.conf` —— 真机原样，含 `/artemis` 与 `/artemis-web` 两条：

```nginx
location ^~ /artemis {
    if ($scheme = "http") {
        break;
    }
    if ($artemis = "local") {
        proxy_pass http://http_artemis;
    }
    if ($artemis = "remote") {
        proxy_pass https://https_artemis_remote;
    }
    autoindex off;
}

location ^~ /artemis-web {
    if ($scheme = "http") {
        break;
    }
    if ($artemis = "local") {
        proxy_pass http://http_artemis_web;
    }
    if ($artemis = "remote") {
        proxy_pass https://https_artemis_remote;
    }
    autoindex off;
}
```

`tests/fixtures/nginx-local/ssl/artemis.conf` 内容一行：`set $artemis "local";`
`tests/fixtures/nginx-remote/ssl/artemis.conf` 内容一行：`set $artemis "remote";`
`tests/fixtures/nginx-remote/conf/nginx.conf` 与 `conf/Mode/nginx_artemis.conf` 从 local 版逐字节复制。

- [ ] **Step 2: 写失败测试（含 Review Focus #3 嵌套花括号）**

`src/nginx.rs` 追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fixture_path;
    use std::fs;

    fn root(name: &str) -> PathBuf {
        fixture_path("").join(name)
    }

    #[test]
    fn 提取upstream端口() {
        let text = fs::read_to_string(root("nginx-local").join("conf").join("nginx.conf")).unwrap();
        let ups = parse_upstreams(&text);
        let find = |n: &str| ups.iter().find(|u| u.name == n).map(|u| u.port);
        assert_eq!(find("http_artemis"), Some(9016));
        assert_eq!(find("http_artemis_web"), Some(9017));
        assert_eq!(find("http_artemis_portal"), Some(9018));
        assert_eq!(find("https_artemis_remote"), Some(443));
    }

    #[test]
    fn 嵌套花括号不得提前结束location块() {
        // Review Focus #3: 块内 if (...) { } 会让朴素"找下一个 }"提前收尾
        let text = fs::read_to_string(
            root("nginx-local").join("conf").join("Mode").join("nginx_artemis.conf"),
        ).unwrap();
        let routes = parse_locations(&text);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, "/artemis");
        assert_eq!(routes[1].path, "/artemis-web");
        // 归因取 local 分支的 upstream, 不是 remote 那条
        assert_eq!(routes[1].target, RouteTarget::Upstream("http_artemis_web".into()));
    }

    #[test]
    fn 变量型proxy_pass标记为动态不报错() {
        let routes = parse_locations("location /a {\n proxy_pass $scheme://$cookie_x;\n}\n");
        assert_eq!(routes[0].target, RouteTarget::Dynamic);
    }

    #[test]
    fn 读取artemis_mode并给出文件行号() {
        let p = root("nginx-local").join("ssl").join("artemis.conf");
        let m = find_artemis_mode(&fs::read_to_string(&p).unwrap(), "artemis.conf").unwrap();
        assert_eq!(m.value, "local");
        assert_eq!(m.loc, "artemis.conf:1");
        let p2 = root("nginx-remote").join("ssl").join("artemis.conf");
        assert_eq!(
            find_artemis_mode(&fs::read_to_string(&p2).unwrap(), "x").unwrap().value,
            "remote"
        );
    }

    #[test]
    fn 整仓加载后能解析出三条路由的目标端口() {
        let info = load_nginx_info(&root("nginx-local"));
        assert_eq!(info.artemis_mode.as_deref(), Some("local"));
        assert_eq!(resolve_route_port(&info, "/artemis-web"), Some(9017));
        assert_eq!(resolve_route_port(&info, "/artemis-portal"), Some(9018));
        assert_eq!(resolve_route_port(&info, "/artemis"), Some(9016));
        assert_eq!(resolve_route_port(&info, "/nope"), None);
    }

    #[test]
    fn remote模式下明确报回环() {
        let info = load_nginx_info(&root("nginx-remote"));
        assert_eq!(info.artemis_mode.as_deref(), Some("remote"));
        // local 分支失效 -> 落到 https_artemis_remote = 127.0.0.1:443 = nginx 自身
        assert_eq!(resolve_route_port(&info, "/artemis-web"), Some(443));
        assert!(info.loopback_risk(), "remote 模式必须被识别为回环");
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -25`
Expected: `cannot find function parse_upstreams in this scope`

- [ ] **Step 4: 写实现**

`src/nginx.rs` 全文替换：

```rust
//! nginx 配置只读解析 (spec §7)。不修改、不 reload、不递归展开 include。
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::model::fixture_path;

#[derive(Debug, PartialEq)]
pub struct Upstream {
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, PartialEq, Clone)]
pub enum RouteTarget {
    Upstream(String),
    /// proxy_pass 目标含变量, 静态不可判定
    Dynamic,
    None,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Route {
    pub path: String,
    pub target: RouteTarget,
}

#[derive(Debug, PartialEq)]
pub struct ArtemisMode {
    pub value: String,
    pub loc: String,
}

#[derive(Debug, Default)]
pub struct NginxInfo {
    pub root: PathBuf,
    pub upstreams: Vec<Upstream>,
    pub routes: Vec<Route>,
    pub artemis_mode: Option<String>,
    pub artemis_mode_loc: Option<String>,
}

impl NginxInfo {
    /// `$artemis != "local"` 时 /artemis* 落到 https_artemis_remote (127.0.0.1:443), 即 nginx 自身。
    pub fn loopback_risk(&self) -> bool {
        self.artemis_mode.as_deref().map(|m| m != "local").unwrap_or(false)
            && self.upstreams.iter().any(|u| u.name == "https_artemis_remote" && u.port == 443)
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

pub fn parse_upstreams(text: &str) -> Vec<Upstream> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let t = strip_comment(lines[i]).trim().to_string();
        i += 1;
        let Some(rest) = t.strip_prefix("upstream") else { continue };
        let rest = rest.trim_start();
        let name: String = rest.chars().take_while(|c| *c != '{' && !c.is_whitespace()).collect();
        if name.is_empty() {
            continue;
        }
        let mut depth = t.matches('{').count() as i32 - t.matches('}').count() as i32;
        let mut body = String::from(rest);
        while i < lines.len() && depth > 0 {
            let c = strip_comment(lines[i]);
            depth += c.matches('{').count() as i32 - c.matches('}').count() as i32;
            body.push('\n');
            body.push_str(c);
            i += 1;
        }
        for bl in body.lines() {
            let bt = bl.trim();
            if let Some(args) = bt.strip_prefix("server") {
                let a = args.trim().trim_end_matches(';').trim().to_string();
                if let Some((h, p)) = a.rsplit_once(':') {
                    if let Ok(port) = p.trim().parse::<u16>() {
                        out.push(Upstream { name: name.clone(), host: h.trim().to_string(), port });
                        break;
                    }
                }
            }
        }
    }
    out
}

/// 花括号配对计数提取 location 块 (Review Focus #3)。
pub fn parse_locations(text: &str) -> Vec<Route> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let t = strip_comment(lines[i]).trim().to_string();
        i += 1;
        let Some(rest) = t.strip_prefix("location") else { continue };
        if rest.chars().next().map(|c| c.is_alphanumeric()).unwrap_or(false) {
            continue; // 避免命中 "locationFoo"
        }
        let mut path = String::new();
        for tk in rest.split_whitespace() {
            if tk == "^~" || tk == "~" || tk == "~*" || tk == "=" || tk.starts_with('~') {
                continue; // 修饰符
            }
            path = tk.trim_end_matches('{').to_string();
            break;
        }
        if path.is_empty() {
            continue;
        }
        let mut depth = t.matches('{').count() as i32 - t.matches('}').count() as i32;
        let mut body = String::new();
        while i < lines.len() && depth > 0 {
            let c = strip_comment(lines[i]);
            depth += c.matches('{').count() as i32 - c.matches('}').count() as i32;
            body.push_str(c);
            body.push('\n');
            i += 1;
        }
        // 取 local 分支的 proxy_pass: 首个字面 http(s):// 目标
        let mut target = RouteTarget::None;
        for bl in body.lines() {
            let bt = bl.trim();
            if let Some(args) = bt.strip_prefix("proxy_pass") {
                let a = args.trim().trim_end_matches(';').trim().to_string();
                if a.contains('$') {
                    target = RouteTarget::Dynamic;
                    break;
                }
                if let Some(name) = a.rsplit('/').next() {
                    if !name.is_empty() {
                        target = RouteTarget::Upstream(name.to_string());
                        break;
                    }
                }
            }
        }
        out.push(Route { path, target });
    }
    out
}

pub fn find_artemis_mode(text: &str, file: &str) -> Option<ArtemisMode> {
    for (n, line) in text.lines().enumerate() {
        let t = strip_comment(line).trim();
        let Some(rest) = t.strip_prefix("set") else { continue };
        let rest = rest.trim_start();
        if let Some((var, val)) = rest.split_once(char::is_whitespace) {
            if var == "$artemis" {
                let v = val.trim().trim_end_matches(';').trim().trim_matches('"').to_string();
                return Some(ArtemisMode { value: v, loc: format!("{}:{}", file, n + 1) });
            }
        }
    }
    None
}

fn confs_in(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().map(|x| x == "conf").unwrap_or(false) {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

/// root 为 nginx 根目录 (含 conf/)。spec §7.3: 只扫 conf/、conf/Mode/、../ssl/ 三处。
pub fn load_nginx_info(root: &Path) -> NginxInfo {
    let mut info = NginxInfo { root: root.to_path_buf(), ..Default::default() };
    let conf = root.join("conf");
    let mut files: Vec<PathBuf> = Vec::new();
    files.extend(confs_in(&conf));
    files.extend(confs_in(&conf.join("Mode")));
    files.extend(confs_in(&root.join("..").join("ssl")));
    if let Ok(main) = std::fs::read_to_string(conf.join("nginx.conf")) {
        info.upstreams = parse_upstreams(&main);
        info.routes = parse_locations(&main);
    }
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        for r in parse_locations(&text) {
            if !info.routes.iter().any(|e| e.path == r.path) {
                info.routes.push(r);
            }
        }
        if info.artemis_mode.is_none() {
            let rel = f.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
            if let Some(m) = find_artemis_mode(&text, &rel) {
                info.artemis_mode = Some(m.value);
                info.artemis_mode_loc = Some(m.loc);
            }
        }
    }
    info
}

/// 路由 -> 目标端口。remote 模式下 local 分支失效, 落到 remote upstream。
pub fn resolve_route_port(info: &NginxInfo, path: &str) -> Option<u16> {
    let r = info.routes.iter().find(|r| r.path == path)?;
    let name = match &r.target {
        RouteTarget::Upstream(n) => n.clone(),
        RouteTarget::Dynamic | RouteTarget::None => return None,
    };
    if info.loopback_risk() {
        return info.upstreams.iter().find(|u| u.name == "https_artemis_remote").map(|u| u.port);
    }
    info.upstreams.iter().find(|u| u.name == name).map(|u| u.port)
}

/// 在 base 下查找含 conf/nginx.conf 的目录, 返回该目录 (nginx 根)。
pub fn locate_nginx_conf(base: &Path) -> Option<PathBuf> {
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if p.join("conf").join("nginx.conf").is_file() {
                return Some(p);
            }
            stack.push(p);
        }
    }
    None
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -25`
Expected: `test result: ok. 16 passed`

若 `嵌套花括号不得提前结束location块` 失败，检查 `parse_locations` 中 depth 起点：`location ^~ /artemis {` 行应得 `depth == 1`（有 `{` 无 `}`）。

- [ ] **Step 6: C++ 同步**

`src/nginx.rs` 全部函数按转写契约落到 `cpp/RepairArtemisWeb.cpp`（放在原 `read_config` 位置附近）。要点：`strip_prefix` 用 `starts_with` + `substr`；`Option` 用 `bool` 出参；`NginxInfo::loopback_risk()` 用 `bool loopback_risk(const NginxInfo&)` 自由函数；`rsplit_once(':')` 用 `find_last_of(':')`。

- [ ] **Step 7: 提交**

```bash
git add src/nginx.rs tests/fixtures/ cpp/RepairArtemisWeb.cpp
git commit -m "新增 nginx 路由与 artemis 模式只读解析, 可识别 remote 回环故障"
```

---

## Task 5: 日志反向 tail 与特征归因

**Files:**
- Modify: `src/logs.rs`
- Create: `tests/fixtures/err-econnrefused.log`
- Create: `tests/fixtures/err-eaddrinuse.log`
- Test: `src/logs.rs::tests`

**Interfaces:**
- Produces: `logs::TAIL_LIMIT_BYTES: u64`、`logs::tail_file(&Path, u64) -> io::Result<Vec<u8>>`、`logs::classify_log(&str) -> Option<&'static str>`

- [ ] **Step 1: 建夹具**

`tests/fixtures/err-econnrefused.log`：

```
[2026-09-24T21:16:10.758] [ERROR] debug - request to http://127.0.0.1:9016/artemis failed
  Error: connect ECONNREFUSED 127.0.0.1:9016
      at TCPConnectWrap.afterConnect [as oncomplete] (net.js:1106:14)
```

`tests/fixtures/err-eaddrinuse.log`：

```
Error: listen EADDRINUSE: address already in use :::9017
    at Server.setupListenHandle [as _listen2] (net.js:1279:14)
```

- [ ] **Step 2: 写失败测试**

`src/logs.rs` 的 `mod tests` 追加：

```rust
#[test]
fn 反向tail不超过上限且取到尾部() {
    use std::io::Write;
    let dir = std::env::temp_dir().join("raw_tail_probe_t5");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("big.log");
    {
        let mut w = std::fs::File::create(&f).unwrap();
        for i in 0..20000 {
            writeln!(w, "line {:06} padding padding padding padding", i).unwrap();
        }
    }
    let got = tail_file(&f, 4096).unwrap();
    assert!(got.len() as u64 <= 4096);
    let s = String::from_utf8_lossy(&got);
    assert!(s.contains("line 019999"), "尾部内容缺失");
    assert!(!s.contains("line 000000"), "不该读到头部");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 空文件与小于上限的文件都安全() {
    let dir = std::env::temp_dir().join("raw_tail_probe_t5b");
    std::fs::create_dir_all(&dir).unwrap();
    let empty = dir.join("e.log");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(tail_file(&empty, 4096).unwrap().len(), 0);
    let small = dir.join("s.log");
    std::fs::write(&small, b"hello\n").unwrap();
    assert_eq!(tail_file(&small, 4096).unwrap(), b"hello\n");
    assert!(tail_file(&dir.join("nope.log"), 4096).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 特征匹配给出结论而非甩锅() {
    let a = std::fs::read_to_string(crate::model::fixture_path("err-econnrefused.log")).unwrap();
    assert_eq!(classify_log(&a), Some("上游依赖拒绝连接"));
    let b = std::fs::read_to_string(crate::model::fixture_path("err-eaddrinuse.log")).unwrap();
    assert_eq!(classify_log(&b), Some("端口冲突"));
    assert_eq!(classify_log("GET /artemis-web/ 200 1ms"), None);
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `cannot find function tail_file in this scope`

- [ ] **Step 4: 写实现**

`src/logs.rs` 追加：

```rust
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// spec §8: 单次上限 64KB, 禁止整文件读入。
pub const TAIL_LIMIT_BYTES: u64 = 64 * 1024;

/// 从文件尾反向读取至多 max 字节。
pub fn tail_file(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let want = len.min(max) as usize;
    if want == 0 {
        return Ok(Vec::new());
    }
    let start = len - want as u64;
    f.seek(SeekFrom::Start(start))?;
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
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 19 passed`

- [ ] **Step 6: C++ 同步**

`tail_file` 用 `std::ifstream`（`ate` 打开 + `tellg()` 求长 + `seekg(start)` + `read()`）；`classify_log` 用同一张 `SIGS` 表配 `std::string::find != npos`。

- [ ] **Step 7: 提交**

```bash
git add src/logs.rs tests/fixtures/ cpp/RepairArtemisWeb.cpp
git commit -m "新增日志反向尾部读取与故障特征归因, 替换整文件读取"
```

---

## Task 6: L2 直连探针与 node.exe 定位修正

**Files:**
- Modify: `src/probe.rs`（当前为空壳）
- Modify: `src/main.rs:224-285`（删除 `port_netstat` / `port_listening` / `http_up`）
- Modify: `src/main.rs:362-388`（`find_node_exe` 的 `ok()?` 早退）
- Test: `src/probe.rs::tests`

**Interfaces:**
- Consumes: `logs::parse_status_line`、`model::{L2, StatusSet}`
- Produces: `probe::netstat_listen_pid(&str) -> Option<(u16, u32)>`、`probe::port_listening(u16) -> bool`、`probe::port_owner_pid(u16) -> Option<u32>`、`probe::classify(u16, &StatusSet) -> L2`、`probe::http_probe_against(u16, &str, &StatusSet) -> L2`

- [ ] **Step 1: 写失败测试**

`src/probe.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{L2, StatusSet};

    #[test]
    fn netstat行解析只认LISTENING并取PID() {
        assert_eq!(
            netstat_listen_pid("  TCP    0.0.0.0:9017           0.0.0.0:0            LISTENING       8460"),
            Some((9017, 8460))
        );
        assert_eq!(
            netstat_listen_pid("  TCP    [::]:9018               [::]:0               LISTENING       8448"),
            Some((9018, 8448))
        );
        assert_eq!(
            netstat_listen_pid("  TCP    0.0.0.0:9017            127.0.0.1:5          ESTABLISHED     999"),
            None
        );
        assert_eq!(netstat_listen_pid("Active Internet connections (only servers):"), None);
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `cannot find function netstat_listen_pid in this scope`

- [ ] **Step 3: 写实现**

`src/probe.rs` 全文替换：

```rust
//! 探针 I/O 边界 (spec §6)。L1 端口 / L2 直连 HTTP。L3 见 Task 7。
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 21 passed`

- [ ] **Step 5: 删除旧探针并修 `find_node_exe`**

`src/main.rs` 删除 `port_netstat`（`src/main.rs:225-244`）、`port_listening`（`src/main.rs:246-256`）、`http_up`（`src/main.rs:258-285`）。`src/main.rs` 内所有调用点改为 `probe::port_listening` / `probe::http_probe_against`（临时用 `let _ = 0;` 占位会编译失败，本步骤与 Step 6 的 `run_flow` 改造在同一提交内完成；若需保持中间可编译，先临时加 `use probe::{port_listening, http_probe_against as http_up_compat};`）。

`src/main.rs:374` 早退修正：

```rust
// 修改前: 碰到一个读不出 file_type 的条目就结束整个搜索并返回 None
let ft = e.file_type().unwrap_or(p.metadata().map(|m| m.file_type()).ok()?);
// 修改后: 跳过该条目, 继续遍历
let ft = match e.file_type() {
    Ok(t) => t,
    Err(_) => match p.metadata() {
        Ok(m) => m.file_type(),
        Err(_) => continue,
    },
};
```

- [ ] **Step 6: 编译验证**

Run: `cargo build 2>&1 | tail -10`
Expected: 若 `repair_component` 仍引用已删函数而报错，属预期 —— Task 9 统一改造；本任务只需 `cargo test --bin RepairArtemisWeb` 全绿即可提交，但 **Task 9 结束前不得发布**。

- [ ] **Step 7: C++ 同步**

替换 `cpp/RepairArtemisWeb.cpp:337-493`（`port_netstat` / `port_listening` / `http_up`）为 `netstat_listen_pid` / `port_listening` / `port_owner_pid` / `classify` / `http_probe_against`。`http_probe` 复用现有 `ensure_winsock()`，用 `send`/`recv` 逐字节至 `\r\n`，并设 `SO_RCVTIMEO`/`SO_SNDTIMEO` 为 8000ms。

- [ ] **Step 8: 提交**

```bash
git add src/probe.rs src/main.rs cpp/RepairArtemisWeb.cpp
git commit -m "L2 直连探针按期望状态码归类, 修正 node.exe 定位遇错即停"
```

---

## Task 7: L3 端到端 HTTPS 探针（WinHTTP）

**Files:**
- Modify: `src/probe.rs`
- Modify: `src/model.rs`（`L3` 加 `Raw` 变体 + `attribute` 防御分支）
- Modify: `cpp/build.bat:11`
- Modify: `cpp/RepairArtemisWeb.cpp`
- Test: `src/probe.rs::tests`（仅 `classify_l3` 纯函数部分）

**Interfaces:**
- Consumes: `model::{L3, StatusSet}`、`logs::parse_status_line`
- Produces: `probe::https_probe(&str, u16, &str) -> L3`、`probe::classify_l3(L3, &StatusSet, bool) -> L3`

- [ ] **Step 1: 写失败测试**

`src/probe.rs::tests` 追加：

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `cannot find function classify_l3 in this scope`

- [ ] **Step 3: 给 `L3` 加 `Raw` 并在 `attribute` 补防御分支**

`src/model.rs` 的 `enum L3` 增加：

```rust
    /// 探针原始输出, 待 classify_l3 归类
    Raw(u16),
```

`attribute` 的 match 中，在 `(Some(L2::Ok(_)), Some(L3::Bad(_)))` 分支之后插入：

```rust
        // 防御: 调用方漏归类时不得把 Raw 当成正常或当成 nginx 故障
        (Some(L2::Ok(_)), Some(L3::Raw(_))) => {
            v!("端到端结果未经归类", VerdictAction::Unverifiable, 0)
        }
```

- [ ] **Step 4: 写 `classify_l3` 与 WinHTTP 探针**

`src/probe.rs` 追加：

```rust
use crate::model::L3;
use std::ffi::c_void;

/// 443 未监听 -> NginxDown; 否则按期望集合归类。5xx 与非期望一律 Bad。
pub fn classify_l3(raw: L3, set: &StatusSet, nginx_up: bool) -> L3 {
    match raw {
        L3::Raw(c) if !nginx_up => L3::NginxDown,
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
const WT_OPT_SECURE_PROTOCOLS: u32 = 84;
const WT_QUERY_STATUS_CODE: u32 = 19;
const WT_QUERY_FLAG_NUMBER: u32 = 0x2000;
const SEC_IGNORE_ALL: u32 = 0x0200 | 0x1000 | 0x2000 | 0x2000;
const PROTOCOLS_ALL: u32 = 0x0080 | 0x0200 | 0x0800 | 0x2000;

fn u16z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 经 nginx 443 的端到端验收 (spec §6 L3)。
/// 任何 TLS/连接层失败一律返回 TlsUnavailable, 不得伪装成 Bad —— 见 Review Focus #4。
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
        // Win7 默认仅 TLS1.0; 平台不支持的位会被拒绝, 失败则回退不设置 (Review Focus #4)
        let mut proto: u32 = PROTOCOLS_ALL;
        WinHttpSetOption(req, WT_OPT_SECURE_PROTOCOLS, &mut proto as *mut u32 as *mut c_void, 4);
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
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 23 passed`

- [ ] **Step 6: C++ 侧与构建**

`cpp/RepairArtemisWeb.cpp` 顶部 `#include <winhttp.h>`，实现同名 `https_probe` / `classify_l3`（`L3` 用 `enum class` + `unsigned short code` 成员的结构体承载，注释标明与 Rust `L3::Raw` 对应）。`cpp/build.bat` 第 11 行改为：

```bat
    -o RepairArtemisWeb.exe -lws2_32 -lshell32 -ladvapi32 -lwinhttp
```

Run: `cargo build --release 2>&1 | tail -5 && cd cpp && cmd //c build.bat 2>&1 | tail -5`
Expected: Rust `Finished release profile`；C++ `BUILD OK: RepairArtemisWeb.exe`

- [ ] **Step 7: 真机手工验证（本机即健康参照）**

Run: `./target/release/RepairArtemisWeb.exe --check-only 2>&1 | tail -30`
Expected: 三条 artemis 路由的 L3 列均为 `200`/`302`。若显示"端到端无法验证"，排查本机 nginx 443 与 TLS 协议位，**不得**通过放宽判定来消除告警。

- [ ] **Step 8: 提交**

```bash
git add src/probe.rs src/model.rs cpp/RepairArtemisWeb.cpp cpp/build.bat
git commit -m "新增 WinHTTP 端到端 HTTPS 探针, 握手失败归为无法验证而非后端故障"
```

---

## Task 8: 修复动作按 kind 分派

**Files:**
- Modify: `src/main.rs`（新增 `repair_node` / `repair_prunsrv`，替换 `repair_component` 的 `src/main.rs:453-598`）
- Test: `src/main.rs::tests`（仅纯逻辑：修复阶梯决策）

**Interfaces:**
- Consumes: `model::{ComponentDef, Kind, ResolvedComponent}`、`probe::port_listening`、`logs::{tail_file, classify_log, TAIL_LIMIT_BYTES}`
- Produces: `main::plan_repair(Kind, SvcState, bool /*l1*/, bool /*reinstall*/) -> Vec<RepairStep>`、`main::RepairStep`、`main::repair_node(...)`、`main::repair_prunsrv(...)`、`main::dump_component_logs(&ResolvedComponent)`

- [ ] **Step 1: 写失败测试（阶梯决策是纯逻辑，必须可测）**

`src/main.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use model::Kind;

    #[test]
    fn 网关默认只到restart重装需显式授权() {
        // spec §6 Prunsrv 阶梯 + D2
        let s = plan_repair(Kind::Prunsrv, SvcState::Running, false, false);
        assert_eq!(s, vec![RepairStep::PrunsrvRestart]);
        let s = plan_repair(Kind::Prunsrv, SvcState::Running, false, true);
        assert_eq!(s, vec![RepairStep::PrunsrvRestart, RepairStep::PrunsrvReinstall]);
        // 服务在跑但端口已起 -> 什么都不做
        assert_eq!(plan_repair(Kind::Prunsrv, SvcState::Running, true, true), Vec::new());
        // Missing -> install, 不是 restart
        assert_eq!(
            plan_repair(Kind::Prunsrv, SvcState::Missing, false, false),
            vec![RepairStep::PrunsrvInstall]
        );
        // Stopped -> 仅 sc start
        assert_eq!(
            plan_repair(Kind::Prunsrv, SvcState::Stopped, false, false),
            vec![RepairStep::ScStart]
        );
    }

    #[test]
    fn node阶梯保持既有行为() {
        use RepairStep::*;
        assert_eq!(
            plan_repair(Kind::Node, SvcState::Missing, false, false),
            vec![NodeUninstall, ScStop, ScDelete, NodeInstall, ScStart]
        );
        assert_eq!(plan_repair(Kind::Node, SvcState::Stopped, false, false), vec![ScStart]);
        assert_eq!(plan_repair(Kind::Node, SvcState::Running, true, false), Vec::new());
        // Running 但端口未起 -> 走重装 (spec §1 行1 的成因)
        assert_eq!(
            plan_repair(Kind::Node, SvcState::Running, false, false),
            vec![NodeUninstall, ScStop, ScDelete, NodeInstall, ScStart]
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `cannot find function plan_repair in this scope`

- [ ] **Step 3: 写阶梯决策与执行**

`src/main.rs` 中，在 `SvcState` 定义之后新增：

```rust
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RepairStep {
    ScStart,
    ScStop,
    ScDelete,
    NodeUninstall,
    NodeInstall,
    PrunsrvInstall,
    PrunsrvReinstall,
    PrunsrvRestart,
}

/// 纯函数: 输入 (kind, 服务状态, L1 是否监听, 是否 --reinstall), 输出阶梯。
/// spec §6。网关比 node 保守一级 (D2)。
fn plan_repair(kind: Kind, state: SvcState, l1_up: bool, reinstall: bool) -> Vec<RepairStep> {
    use RepairStep::*;
    match kind {
        Kind::Node => {
            if state == SvcState::Running && l1_up && !reinstall {
                return Vec::new();
            }
            match state {
                SvcState::Stopped => vec![ScStart],
                SvcState::Running if l1_up => vec![NodeUninstall, ScStop, ScDelete, NodeInstall, ScStart],
                _ => vec![NodeUninstall, ScStop, ScDelete, NodeInstall, ScStart],
            }
        }
        Kind::Prunsrv => {
            if state == SvcState::Running && l1_up && !reinstall {
                return Vec::new();
            }
            match state {
                SvcState::Missing => vec![PrunsrvInstall],
                SvcState::Stopped => vec![ScStart],
                SvcState::Running if reinstall => vec![PrunsrvRestart, PrunsrvReinstall],
                SvcState::Running => vec![PrunsrvRestart],
                _ => vec![PrunsrvRestart],
            }
        }
    }
}
```

执行函数（替换 `src/main.rs:453-598` 的 `repair_component`）：

```rust
/// 跑 `__service.bat {install|restart|uninstall}`。脚本自身会 CD 到 bin/artemis。
fn run_prunsrv_bat(dir: &Path, verb: &str, prefix: &str) {
    let bat = dir.join("bin").join("__service.bat");
    if !bat.is_file() {
        log(Level::Err, &format!("未找到 {}", bat.display()));
        return;
    }
    let mut c = Command::new("cmd");
    c.args(["/C", &bat.to_string_lossy(), verb]);
    if let Ok(out) = c.output() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for l in text.lines() {
            if !l.trim().is_empty() {
                log(Level::Info, &format!("  [{}] {}", prefix, l.trim_end()));
            }
        }
        let code = out.status.code().unwrap_or(-1);
        if code != 0 {
            log(Level::Warn, &format!("  [{}] 退出码 {}", prefix, code));
        }
    }
}

/// spec §8: 失败时按 kind tail 正确的日志文件, 并给特征结论。
fn dump_component_logs(c: &ResolvedComponent) {
    let files: Vec<PathBuf> = match c.def.kind {
        Kind::Node => {
            let d = c.dir.join("daemon");
            let mut v = vec![
                d.join(format!("{}.err.log", c.svc_name)),
                d.join(format!("{}.out.log", c.svc_name)),
                d.join(format!("{}.wrapper.log", c.svc_name)),
            ];
            if let Ok(t) = fs::read_to_string(c.dir.join("config.properties")) {
                if let Some(p) = model::parse_properties(&t).get("log.path").cloned() {
                    v.push(c.dir.join(p));
                }
            }
            v
        }
        Kind::Prunsrv => {
            let d = c.dir.join("logs");
            match fs::read_dir(&d) {
                Ok(rd) => rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .map(|n| n.to_string_lossy().starts_with("artemis"))
                            .unwrap_or(false)
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
    };
    let mut all = String::new();
    for f in files.iter().filter(|p| p.is_file()) {
        match logs::tail_file(f, logs::TAIL_LIMIT_BYTES) {
            Ok(b) => {
                let s = String::from_utf8_lossy(&b).into_owned();
                log(Level::Err, &format!("--- {} (尾部 {}B) ---", f.file_name().unwrap().to_string_lossy(), b.len()));
                for l in s.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev() {
                    log(Level::Err, &format!("  {}", l));
                }
                all.push_str(&s);
            }
            Err(_) => {}
        }
    }
    match logs::classify_log(&all) {
        Some(why) => log(Level::Err, &format!("日志特征归因: {}", why)),
        None => log(Level::Info, "日志未命中已知故障特征。"),
    }
}

fn wait_for_port(c: &ResolvedComponent) -> bool {
    let deadline = c.def.start_timeout_secs;
    let mut waited = 0u64;
    while waited < deadline {
        if probe::port_listening(c.port) {
            return true;
        }
        thread::sleep(Duration::from_secs(5));
        waited += 5;
    }
    probe::port_listening(c.port)
}

/// 返回该组件对异常计数的贡献: 0 正常, 1 已修/异常, 3 nginx 层
#[allow(clippy::too_many_arguments)]
fn repair_component(
    c: &ResolvedComponent,
    root: &Path,
    node_exe: &Path,
    reinstall: bool,
    check_only: bool,
    assume_yes: bool,
    issues: &mut u32,
) -> u8 {
    log(Level::Step, &format!("---- 组件 [{}] ----", c.def.key));
    if !c.present {
        log(Level::Err, &format!("组件不存在: {}", c.dir.join(c.def.present_marker).display()));
        *issues += 1;
        return 1;
    }
    for w in &c.warnings {
        log(Level::Warn, w);
    }
    let state = service_state(&c.svc_name);
    let l1 = probe::port_listening(c.port);
    log(
        Level::Info,
        &format!("服务名={} 端口={} 状态={:?} L1={}", c.svc_name, c.port, state, l1),
    );

    if check_only {
        let l2 = if l1 {
            probe::http_probe_against(c.port, &c.pathname, &c.def.l2_ok)
        } else {
            model::L2::NoHttpResponse
        };
        let v = model::attribute(
            if l1 { model::L1::Listening } else { model::L1::NotListening },
            l1.then_some(l2),
            None,
        );
        log(if v.exit_contrib == 0 { Level::Ok } else { Level::Err }, &v.cause);
        *issues += v.exit_contrib.max(0) as u32;
        return v.exit_contrib;
    }

    let steps = plan_repair(c.def.kind, state, l1, reinstall);
    if steps.is_empty() {
        log(Level::Ok, "无需修复。");
        return 0;
    }
    if c.def.kind == Kind::Prunsrv
        && steps.contains(&RepairStep::PrunsrvReinstall)
        && !assume_yes
        && !confirm(&format!(
            "即将卸载并重装 Java 网关服务 {} (影响整个 OpenAPI 平台), 确认? [y/N]",
            c.svc_name
        ))
    {
        log(Level::Warn, "用户取消网关重装。");
        *issues += 1;
        return 1;
    }

    for st in steps {
        match st {
            RepairStep::ScStart => {
                log(Level::Info, &format!("sc start {}", c.svc_name));
                start_service(&c.svc_name);
            }
            RepairStep::ScStop => stop_service(&c.svc_name),
            RepairStep::ScDelete => delete_service(&c.svc_name),
            RepairStep::NodeUninstall => {
                run_node_logged(node_exe, &c.dir.join("service.uninstall.js"), None, "uninstall")
            }
            RepairStep::NodeInstall => {
                run_node_logged(node_exe, &c.dir.join("service.install.js"), Some(&c.dir), "install")
            }
            RepairStep::PrunsrvInstall => run_prunsrv_bat(&c.dir, "install", "prunsrv"),
            RepairStep::PrunsrvRestart => run_prunsrv_bat(&c.dir, "restart", "prunsrv"),
            RepairStep::PrunsrvReinstall => {
                run_prunsrv_bat(&c.dir, "uninstall", "prunsrv");
                thread::sleep(Duration::from_secs(3));
                run_prunsrv_bat(&c.dir, "install", "prunsrv");
            }
        }
        let _ = root;
    }

    let ok = wait_for_port(c);
    if !ok {
        log(Level::Err, &format!("[{}] 修复失败: 端口 {} 未监听。", c.def.key, c.port));
        dump_component_logs(c);
        *issues += 1;
        return 1;
    }
    log(Level::Ok, &format!("[{}] 端口 {} 已监听。", c.def.key, c.port));
    0
}

/// 交互确认; 非交互 (stdin 非控制台) 时一律返回 false, 不做危险动作。
fn confirm(prompt: &str) -> bool {
    if !stdin_is_console() {
        return false;
    }
    print!("{} ", prompt);
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 25 passed`

- [ ] **Step 5: C++ 同步**

`plan_repair` / `RepairStep` / `run_prunsrv_bat` / `dump_component_logs` / `confirm` 按转写契约落到 `cpp/RepairArtemisWeb.cpp:686-827`（替换 `print_wrapper_tail` 与 `repair_component`）。`run_prunsrv_bat` 用 `run_output(L"cmd.exe", {L"/C", bat.wstring(), verb}, ...)`；`confirm` 用 `_get_wstdin` + `GetConsoleMode` 判交互。

- [ ] **Step 6: 提交**

```bash
git add src/main.rs cpp/RepairArtemisWeb.cpp
git commit -m "修复动作按组件类型分派, 网关默认只 restart 且重装需确认"
```

---

## Task 9: 流程组装、报告表与 CLI

**Files:**
- Modify: `src/main.rs`（`run_flow` `src/main.rs:608-699`、`choose_action_interactive` `src/main.rs:701-743`、`usage` `src/main.rs:745-762`、`main` `src/main.rs:764-845`）
- Test: `src/main.rs::tests`

**Interfaces:**
- Consumes: 前 8 个任务全部产物
- Produces: `main::Options`、`main::parse_args(&[String]) -> Result<Options, String>`、`main::render_route_table(&[(String, L1, Option<L2>, Option<L3>)] ) -> String`

- [ ] **Step 1: 写失败测试（参数解析 + 报告表渲染）**

`src/main.rs::tests` 追加：

```rust
#[test]
fn 参数解析覆盖新选项() {
    let o = parse_args(&[
        "--check-only".into(),
        "--components".into(),
        "artemis-web,artemis-portal".into(),
        "--no-e2e".into(),
        "--nginx-root".into(),
        "D:/x".into(),
    ])
    .unwrap();
    assert!(o.check_only);
    assert_eq!(o.components, vec!["artemis-web".to_string(), "artemis-portal".to_string()]);
    assert!(!o.e2e);
    assert_eq!(o.nginx_root.as_deref(), Some("D:/x"));
    assert_eq!(o.e2e_host, "127.0.0.1");
}

#[test]
fn 默认组件表含三组件() {
    let o = parse_args(&["--root".into(), "C:/r".into()]).unwrap();
    assert_eq!(o.components, vec!["artemis", "artemis-web", "artemis-portal"]);
    assert!(!o.check_only && !o.reinstall && o.e2e);
}

#[test]
fn 未知参数与缺值报错不panic() {
    assert!(parse_args(&["--bogus"]).is_err());
    assert!(parse_args(&["--components"]).is_err());
    assert!(parse_args(&["--root"]).is_err());
}

#[test]
fn 报告表把不一致行显式标出() {
    use model::{L1, L2, L3};
    let t = render_route_table(&[
        ("/artemis-web".into(), L1::Listening, Some(L2::Ok(200)), Some(L3::Bad(502))),
        ("/artemis".into(), L1::NotListening, None, None),
    ]);
    assert!(t.contains("/artemis-web"), "{}", t);
    assert!(t.contains("502"), "必须显示实际状态码: {}", t);
    assert!(t.contains("后端健康") || t.contains("nginx"), "必须给结论: {}", t);
    assert!(t.contains("未监听"), "{}", t);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `cannot find function parse_args in this scope`

- [ ] **Step 3: 写 `Options` 与 `parse_args`**

```rust
#[derive(Debug, PartialEq)]
pub struct Options {
    pub check_only: bool,
    pub reinstall: bool,
    pub root: Option<String>,
    pub components: Vec<String>,
    pub nginx_root: Option<String>,
    pub e2e: bool,
    pub e2e_host: String,
    pub assume_yes: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            check_only: false,
            reinstall: false,
            root: None,
            components: model::COMPONENTS.iter().map(|c| c.key.to_string()).collect(),
            nginx_root: None,
            e2e: true,
            e2e_host: "127.0.0.1".to_string(),
            assume_yes: false,
        }
    }
}

pub fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].to_ascii_lowercase();
        let mut need = |flag: &str| -> Result<String, String> {
            if i + 1 >= args.len() {
                return Err(format!("{} 需要参数值。", flag));
            }
            i += 1;
            Ok(args[i].clone())
        };
        match a.as_str() {
            "-c" | "--check-only" | "/c" => o.check_only = true,
            "-r" | "--reinstall" | "/r" => o.reinstall = true,
            "--yes" => o.assume_yes = true,
            "--no-e2e" => o.e2e = false,
            "--root" | "-root" | "--openapi-root" | "/root" => o.root = Some(need("--root")?),
            "--nginx-root" => o.nginx_root = Some(need("--nginx-root")?),
            "--e2e-host" => o.e2e_host = need("--e2e-host")?,
            "--components" => {
                let v = need("--components")?;
                let list: Vec<String> =
                    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                if list.is_empty() {
                    return Err("--components 不能为空。".to_string());
                }
                for k in &list {
                    if !model::COMPONENTS.iter().any(|c| c.key == *k) {
                        return Err(format!("未知组件 \"{}\"。", k));
                    }
                }
                o.components = list;
            }
            "-h" | "--help" | "/?" | "help" => return Err("__HELP__".to_string()),
            _ => return Err(format!("未知参数 \"{}\"。", args[i])),
        }
        i += 1;
    }
    Ok(o)
}
```

闭包 `need` 捕获 `i` 的可变借用与 `args` 冲突，改为内联宏以避免借用错误：

```rust
    macro_rules! need {
        ($f:expr) => {{
            if i + 1 >= args.len() {
                return Err(format!("{} 需要参数值。", $f));
            }
            i += 1;
            args[i].clone()
        }};
    }
```

并把 `need("--root")?` 全部替换为 `need!("--root")`（宏内已含边界判断，不再 `?`）。

- [ ] **Step 4: 写 `render_route_table`**

```rust
/// spec §7 输出形态。每行: 路由 / 目标端口 / L1 / L2 / L3 / 结论。
pub fn render_route_table(rows: &[(String, model::L1, Option<model::L2>, Option<model::L3>)]) -> String {
    use model::{L1, L2, L3};
    let mut s = String::new();
    s.push_str("路由              目标              L1      L2直连    L3经nginx   结论\n");
    for (path, l1, l2, l3) in rows {
        let v = model::attribute(*l1, *l2, *l3);
        let l1s = if *l1 == L1::Listening { "监听" } else { "未监听" };
        let l2s = match l2 {
            Some(L2::Ok(c)) => format!("{}", c),
            Some(L2::ServerError(c)) => format!("{}!", c),
            Some(L2::Unexpected(c)) => format!("{}!", c),
            Some(L2::NoHttpResponse) => "无HTTP响应".into(),
            None => "-".into(),
        };
        let l3s = match l3 {
            Some(L3::Ok(c)) => format!("{}", c),
            Some(L3::Bad(c)) => format!("{}!", c),
            Some(L3::Raw(c)) => format!("{}?", c),
            Some(L3::TlsUnavailable) => "TLS不可用".into(),
            Some(L3::NginxDown) => "443不可达".into(),
            Some(L3::Skipped) | None => "-".into(),
        };
        s.push_str(&format!(
            "{:<16}  {:<15}  {:<6}  {:<9}  {:<11}  {}\n",
            path, "-", l1s, l2s, l3s, v.cause
        ));
    }
    s
}
```

- [ ] **Step 5: 改造 `run_flow` 与 `main`**

`run_flow(opts: &Options) -> i32`，按 spec §6/§7 顺序：

1. 定位 OpenAPI root（保留现有逻辑与 `find_artemis_dir` 兜底）。
2. 定位 node.exe（`find_node_exe`，Task 6 已修）。
3. 前置组件检查（保留 `PREREQ`，但 9016 从表中移除——它现在是被修组件，不再算前置告警）。
4. 定位 nginx 根：`opts.nginx_root` 优先，否则 `nginx::locate_nginx_conf(Path::new(SEARCH_BASE))`；失败则 Warn "未定位到 nginx, L3 与 nginx 归因跳过"。
5. `load_nginx_info` → 若 `loopback_risk()` 立即 `log(Err, "$artemis = \"remote\", /artemis* 将回环到 nginx 自身 (loc), 后端服务无需重启")`，并把整体退出码抬到 3。
6. 按 `opts.components` 顺序 `resolve_component` + `repair_component`。
7. 修复模式下对每个组件复测 L1/L2，再跑 L3：`probe::classify_l3(probe::https_probe(&opts.e2e_host, 443, &format!("{}/", c.pathname)), &c.def.l2_ok, probe::port_listening(443))`；`opts.e2e == false` 时用 `L3::Skipped`。
8. `render_route_table` 结果整体 `log(Step, ...)` 输出（按 `\n` 拆行）。
9. 汇总退出码 = 各路由 `exit_contrib` 的最大值；`issues > 0` 且最大值 < 3 时返回 1；否则返回该最大值。

`main` 改为：

```rust
    let args: Vec<String> = env::args().skip(1).collect();
    let opts = if args.is_empty() {
        // 交互菜单
        match choose_action_interactive(is_admin()) {
            Some(a) => Options { check_only: a == Action::CheckOnly, reinstall: a == Action::Reinstall, ..Default::default() },
            None => { pause_if_console(); std::process::exit(0) }
        }
    } else {
        match parse_args(&args) {
            Ok(o) => o,
            Err(e) if e == "__HELP__" => { usage(); std::process::exit(0) }
            Err(e) => { eprintln!("错误: {}", e); usage(); std::process::exit(2) }
        }
    };
    if !is_admin() && !opts.check_only {
        log(Level::Err, "本工具需要管理员权限(注册/启动 Windows 服务), 请以管理员身份运行。");
        pause_if_console();
        std::process::exit(2);
    }
    let code = run_flow(&opts);
    pause_if_console();
    std::process::exit(code);
```

`usage()` 补 spec §9 的 5 个新参数与退出码 3 说明。交互菜单文案改为 spec §9 的三条。

- [ ] **Step 6: 跑测试与真机冒烟**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -20`
Expected: `test result: ok. 29 passed`

Run: `cargo build --release 2>&1 | tail -3 && ./target/release/RepairArtemisWeb.exe --check-only 2>&1 | tail -25; echo "exit=$?"`
Expected: 路由表三行 L1=监听、L2 为 200/302、L3 为 200；`exit=0`

Run: `./target/release/RepairArtemisWeb.exe --components artemis-web --check-only 2>&1 | grep -c "/artemis-web"`
Expected: `1`（只报一条路由，spec §13.4）

- [ ] **Step 7: C++ 同步**

`Options`/`parse_args`/`render_route_table`/`run_flow` 按转写契约落到 `cpp/RepairArtemisWeb.cpp:834-1052`。`parse_args` 的 `Result` 用 `bool parse_args(..., Options&, std::string& err)`。

- [ ] **Step 8: 提交**

```bash
git add src/main.rs cpp/RepairArtemisWeb.cpp
git commit -m "组装三层探针与 nginx 归因流程, 新增组件选择与端到端参数"
```

---

## Task 10: 文档、版本与双版本一致性验收

**Files:**
- Modify: `README.md:1-140`
- Modify: `Cargo.toml:3`
- Modify: `cpp/RepairArtemisWeb.cpp`（`usage()` 文案同步）

- [ ] **Step 1: 更新 README 功能段**

`README.md:9-16` 的"检查前置组件"与"逐个修复"改为：

```markdown
2. **检查前置组件**：redis / postgresql / minio / nginx 是否就绪；
3. **三层探针 + 逐个修复** `artemis`(网关 9016) / `artemis-web`(9017) / `artemis-portal`(9018)：

   | 层级 | 判据 |
   | --- | --- |
   | L1 | 端口是否监听 |
   | L2 | 直连状态码是否落在该组件期望集合（网关接受 302/404，web/portal 要求 2xx/3xx） |
   | L3 | 经本机 nginx 443 访问 `/<pathname>/` 的实际状态码 |

   网关为 prunsrv 服务，默认只 `restart`；卸载重装需 `--reinstall` 且交互确认。
```

新增"归因矩阵"小节，粘贴 spec §6 的 7 行表；新增"退出码"小节列出 0/1/2/3。

`README.md:34-39` 的验证入口补一句：

```markdown
> 若本机直连 9017 正常但 `https://<IP>/artemis-web/` 仍 502，属 nginx 转发层问题，
> 用 `--check-only` 查看路由归因表，不要反复重启后端服务。
```

新增"双版本一致性验证"小节：

```markdown
## 双版本一致性验证

C++ 版无自动化测试。发布前在任意一台已部署平台上分别运行两版：

```bat
RepairArtemisWeb-rs.exe --check-only > out-rs.txt 2>&1
RepairArtemisWeb-cpp.exe --check-only > out-cpp.txt 2>&1
fc out-rs.txt out-cpp.txt
```

时间戳前缀必然不同，比对时忽略每行 `[HH:MM:SS]`；其余文本须逐行一致。
```

- [ ] **Step 2: 版本号**

`Cargo.toml:3` 改为 `version = "0.2.0"`。

- [ ] **Step 3: 全量回归**

Run: `cargo test --bin RepairArtemisWeb 2>&1 | tail -10 && cargo build --release 2>&1 | tail -3 && cd cpp && cmd //c build.bat 2>&1 | tail -3`
Expected: 测试全绿；两版均构建成功。

- [ ] **Step 4: spec §13 验收逐条核对**

- §13.1：临时 `sc stop artemis-web` 后跑 `--check-only`，确认该路由 L1 列为"未监听"且出现在待修列表，而非仅 Warn。
- §13.2：`cargo test --bin RepairArtemisWeb nginx::tests::remote模式下明确报回环 -- --exact` 通过。
- §13.3：确认只有 L1+L2+L3 全通过才打印"修复成功"（`grep -n "修复成功" src/main.rs` 逐处核对判定条件）。
- §13.4：Step 6 的 `--components artemis-web` 冒烟已覆盖。
- §13.5：非管理员 PowerShell 跑 `--check-only`，退出码不得为 2。

- [ ] **Step 5: 双版本 diff**

按 Step 1 新增章节在本机执行 `fc`，逐行核对；差异须能归因到时间戳或明确记录为缺陷。

- [ ] **Step 6: 提交**

```bash
git add README.md Cargo.toml src/ cpp/
git commit -m "更新 README 与版本号, 补充三层探针归因矩阵与双版本一致性验证"
```

---

## 计划自审记录

**1. spec 覆盖核对**：spec §5 组件表→Task 2；§6 探针与矩阵→Task 3/6/7；§6 修复分派→Task 8；§7 nginx 归因→Task 4；§8 日志证据→Task 5；§9 CLI 与退出码→Task 9；§10 顺带修正→Task 5（tail）、Task 6（`find_node_exe`、pathname 来自配置）；§11 测试→各任务测试步骤 + Task 10 Step 5；§13 验收→Task 10 Step 4。无遗漏。

**2. 占位符**：无 TBD/TODO/"类似 Task N"。`src/nginx.rs` 的 `#[cfg(test)] use crate::model::fixture_path;` 是真实代码而非占位。

**3. 类型一致性**：`L1/L2/L3/StatusSet/RouteVerdict/VerdictAction/ResolvedComponent/NginxInfo/RepairStep/Options` 在各任务间签名一致；`attribute(L1, Option<L2>, Option<L3>)` 在 Task 3 定义、Task 8/9 调用处签名一致；`http_probe_against(port, pathname, &StatusSet)` 在 Task 6 定义、Task 8 调用一致。

**4. 已知的执行期风险（不掩盖）**：
- Task 6 Step 5 删除 `http_up` 后、Task 9 改造 `run_flow` 前，`src/main.rs` 可能编译不过。计划在该步给了保持可编译的临时手段，并要求 Task 9 结束前不发布。若执行者按任务顺序提交，**允许 Task 6 的提交处于"测试全绿但 `cargo build` 报错"状态**，因为 `cargo test` 会编译整个 crate —— 实际上不会。因此执行 Task 6 时若 `cargo test` 因 `repair_component` 引用旧函数而失败，须把 Task 8/9 的 `repair_component` 改造提前到 Task 6 一并做，或在本任务内先加兼容 shim。这是任务切分上的真实耦合，执行时按此处理，不要靠放宽测试来通过。
- `parse_args` 的闭包借用冲突在 Step 3 内已给出宏替代方案，执行时直接采用宏版本。
