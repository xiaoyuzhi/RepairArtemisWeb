// ============================================================================
// RepairArtemisWeb
//   iSecure VMS OpenAPI artemis-web / artemis-portal 组件修复工具
//
//   功能复刻自 Repair-ArtemisWeb.ps1:
//     * 定位 OpenAPI 安装目录与内置 node.exe;
//     * 检查前置组件 (artemis 网关 9016 / redis / postgresql / minio / nginx);
//     * 对 artemis-web / artemis-portal 逐个修复:
//         - 服务运行中            -> 跳过 (除非 --reinstall);
//         - 服务停止              -> 直接启动, 端口起来即完成;
//         - 未安装/启动失败/端口未起 -> 卸载(service.uninstall.js)
//                                     -> 重装(service.install.js) -> 启动;
//     * 等待端口监听 + HTTP 健康检查, 输出汇总。
//
//   特性: 零第三方依赖 (纯 Rust 标准库 + 少量 Win32 FFI),
//         带参数运行 = 命令行模式, 不带参数 = 中文交互式菜单。
//
//   开发者: 余志强    QQ: 379008610
//   主页:   https://github.com/xiaoyuzhi
//   版权:   Copyright (c) 2026 余志强 (Yu Zhiqiang). All rights reserved.
// ============================================================================

use std::env;
use std::ffi::c_void;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const DEFAULT_OPENAPI_ROOT: &str =
    r"C:\Program Files (x86)\iSecure VMS\VSM Servers\OpenAPI\artemis";
const SEARCH_BASE: &str = r"C:\Program Files (x86)\iSecure VMS";

// ---------------------------- Win32 FFI (无 crate) --------------------------
#[repr(C)]
struct SYSTEMTIME {
    w_year: u16,
    w_month: u16,
    w_day_of_week: u16,
    w_day: u16,
    w_hour: u16,
    w_minute: u16,
    w_second: u16,
    w_milliseconds: u16,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut SYSTEMTIME);
    fn GetStdHandle(n_std_handle: u32) -> *mut c_void;
    fn GetConsoleMode(h_console_output: *mut c_void, lp_mode: *mut u32) -> i32;
    fn SetConsoleMode(h_console_output: *mut c_void, dw_mode: u32) -> i32;
    fn SetConsoleOutputCP(w_code_page_id: u32) -> i32;
    fn SetConsoleCP(w_code_page_id: u32) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn IsUserAnAdmin() -> i32;
}

const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // -11
const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // -10
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const CP_UTF8: u32 = 65001;

static COLORED: AtomicBool = AtomicBool::new(true);

/// 启用控制台 ANSI 颜色; 若 stdout 非终端则关闭颜色。
fn enable_vt() {
    unsafe {
        // 控制台代码页统一为 UTF-8 (中文字符串以 UTF-8 输出, 中文 Windows
        // 默认按 GBK 解码会乱码)。
        SetConsoleOutputCP(CP_UTF8);
        SetConsoleCP(CP_UTF8);

        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() {
            COLORED.store(false, Ordering::Relaxed);
            return;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(h, &mut mode) == 0 {
            COLORED.store(false, Ordering::Relaxed); // 输出被重定向
        } else {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// stdin 是否为控制台 (双击/终端运行 = true; 管道/重定向 = false)。
fn stdin_is_console() -> bool {
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        if h.is_null() {
            return false;
        }
        let mut mode: u32 = 0;
        GetConsoleMode(h, &mut mode) != 0
    }
}

/// 交互式运行时, 退出前暂停等待回车, 防止控制台窗口一闪而过。
/// 仅在 stdin 是控制台时生效, 不影响管道/脚本调用。
fn pause_if_console() {
    if !stdin_is_console() {
        return;
    }
    print!("\n按回车键退出...");
    let _ = io::stdout().flush();
    let _ = io::stdin().read_line(&mut String::new());
}

fn now_ts() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        GetLocalTime(&mut t);
    }
    format!("{:02}:{:02}:{:02}", t.w_hour, t.w_minute, t.w_second)
}

// --------------------------------- 日志 -------------------------------------
#[derive(Clone, Copy)]
enum Level {
    Info,
    Ok,
    Warn,
    Err,
    Step,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Ok => "OK",
            Level::Warn => "WARN",
            Level::Err => "ERR",
            Level::Step => "STEP",
        }
    }
    fn color(self) -> &'static str {
        match self {
            Level::Info => "\x1b[90m",
            Level::Ok => "\x1b[32m",
            Level::Warn => "\x1b[33m",
            Level::Err => "\x1b[31m",
            Level::Step => "\x1b[36m",
        }
    }
}

fn log(level: Level, msg: &str) {
    let text = format!("[{}] [{}] {}", now_ts(), level.tag(), msg);
    if COLORED.load(Ordering::Relaxed) {
        println!("{}{}\x1b[0m", level.color(), text);
    } else {
        println!("{}", text);
    }
    let _ = io::stdout().flush();
}

// -------------------------------- 进程助手 ----------------------------------
fn run_status(prog: &str, args: &[&str]) -> i32 {
    match Command::new(prog).args(args).status() {
        Ok(s) => s.code().unwrap_or(-1),
        Err(_) => -1,
    }
}

fn run_output(prog: &str, args: &[&str], cwd: Option<&Path>) -> (i32, String, String) {
    let mut c = Command::new(prog);
    c.args(args);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    match c.output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(_) => (-1, String::new(), String::new()),
    }
}

fn run_node_logged(node_exe: &Path, script: &Path, cwd: Option<&Path>, prefix: &str) {
    let mut c = Command::new(node_exe);
    c.arg(script);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = match c.output() {
        Ok(o) => o,
        Err(e) => {
            log(Level::Warn, &format!("无法执行 node.exe: {}", e));
            return;
        }
    };
    let code = out.status.code().unwrap_or(-1);
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
    if code != 0 {
        log(Level::Warn, &format!("  [{}] 脚本退出码: {}", prefix, code));
    }
}

// --------------------------- 端口 / HTTP 检查 ------------------------------
/// 用 netstat 判断本地端口是否处于 LISTENING (兼容中/英文输出)。
fn port_netstat(port: u16) -> bool {
    let (_c, out, _e) = run_output("netstat", &["-ano"], None);
    for line in out.lines() {
        let mut toks = line.split_whitespace();
        let _proto = toks.next();
        let Some(local) = toks.next() else { continue };
        let Some(idx) = local.rfind(':') else { continue };
        let Ok(p) = local[idx + 1..].trim_end_matches(']').parse::<u16>() else {
            continue;
        };
        if p != port {
            continue;
        }
        if toks.clone().any(|t| t.contains("LISTENING") || t.contains("LISTEN")) {
            return true;
        }
    }
    false
}

/// 端口监听判定: netstat 优先, TCP 连通测试兜底 (防止状态字被本地化)。
fn port_listening(port: u16) -> bool {
    if port_netstat(port) {
        return true;
    }
    let addr: SocketAddr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

/// HTTP 健康检查: 只要求有 HTTP 响应 (200/302/404 均算服务已起), 连接拒绝才算失败。
fn http_up(port: u16, pathname: &str) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let mut s = match TcpStream::connect_timeout(&addr, Duration::from_secs(8)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(8)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(8)));
    let req = format!(
        "GET /{}/ HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUser-Agent: RepairArtemisWeb\r\nConnection: close\r\n\r\n",
        pathname, port
    );
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    if s.flush().is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    match s.read(&mut buf) {
        Ok(n) if n > 0 => buf[..n].windows(4).any(|w| w == b"HTTP"),
        _ => false,
    }
}

// ------------------------------ 服务管理 (sc.exe) --------------------------
#[derive(Clone, Copy, PartialEq, Debug)]
enum SvcState {
    Missing,
    Stopped,
    Running,
    Other(u32),
}

/// sc.exe query 返回码非 0 = 服务不存在 (错误 1060);
/// STATE 行的数值: 1=STOPPED 2=START_PENDING 3=STOP_PENDING 4=RUNNING ...
fn service_state(name: &str) -> SvcState {
    let (code, out, _e) = run_output("sc.exe", &["query", name], None);
    if code != 0 {
        return SvcState::Missing;
    }
    for line in out.lines() {
        let line = line.trim();
        if line.starts_with("STATE") {
            if let Some(rest) = line.splitn(2, ':').nth(1) {
                let num = rest
                    .split_whitespace()
                    .next()
                    .and_then(|x| x.parse::<u32>().ok())
                    .unwrap_or(0);
                return match num {
                    1 => SvcState::Stopped,
                    4 => SvcState::Running,
                    n => SvcState::Other(n),
                };
            }
        }
    }
    SvcState::Other(0)
}

fn start_service(name: &str) {
    run_status("sc.exe", &["start", name]);
}

fn stop_service(name: &str) {
    run_status("sc.exe", &["stop", name]);
}

fn delete_service(name: &str) {
    run_status("sc.exe", &["delete", name]);
}

// ------------------------------ 目录 / 文件定位 ----------------------------
/// 在 base 下递归查找名为 artemis 的目录 (取第一个, 模拟 PS 枚举)。
fn find_artemis_dir(base: &Path) -> Option<PathBuf> {
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                let named = p
                    .file_name()
                    .map(|n| n.to_string_lossy().eq_ignore_ascii_case("artemis"))
                    .unwrap_or(false);
                if named {
                    return Some(p);
                }
                stack.push(p);
            }
        }
    }
    None
}

/// 在 base 下递归查找全部 node.exe, 取 FullName 字典序最大者
/// (对应 PS: Sort-Object FullName -Descending | Select -First 1)。
fn find_node_exe(base: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let ft = e.file_type().unwrap_or(p.metadata().map(|m| m.file_type()).ok()?);
            if ft.is_dir() {
                stack.push(p);
            } else if p
                .file_name()
                .map(|n| n.to_string_lossy().eq_ignore_ascii_case("node.exe"))
                .unwrap_or(false)
            {
                found.push(p);
            }
        }
    }
    found.sort_by(|a, b| b.to_string_lossy().cmp(&a.to_string_lossy()));
    found.into_iter().next()
}

// ------------------------------ config.properties --------------------------
struct CompConfig {
    service_name: Option<String>,
    port: Option<u16>,
}

/// 解析 config.properties 中的 service.name / server.port
/// (键中的 "." 在 PS 正则里是转义的字面点)。
fn read_config(dir: &Path) -> CompConfig {
    let mut cfg = CompConfig {
        service_name: None,
        port: None,
    };
    let raw = match fs::read(dir.join("config.properties")) {
        Ok(b) => b,
        Err(_) => return cfg,
    };
    let content = String::from_utf8_lossy(&raw).into_owned();
    for line in content.lines() {
        let line = line.trim_start();
        if cfg.service_name.is_none() {
            if let Some(rest) = line.strip_prefix("service.name") {
                if let Some(v) = kv_value(rest) {
                    cfg.service_name = Some(v);
                }
            }
        }
        if cfg.port.is_none() {
            if let Some(rest) = line.strip_prefix("server.port") {
                if let Some(v) = kv_value(rest) {
                    cfg.port = v.parse::<u16>().ok();
                }
            }
        }
    }
    cfg
}

/// 从 "= value ..." 剩余部分提取第一个非空白 token。
fn kv_value(after_key: &str) -> Option<String> {
    let rest = after_key.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let v = rest.split_whitespace().next()?;
    Some(v.to_string())
}

// --------------------------------- 修复流程 ---------------------------------
fn print_wrapper_tail(comp_dir: &Path, svc_name: &str) {
    let wrapper = comp_dir
        .join("daemon")
        .join(format!("{}.wrapper.log", svc_name));
    if wrapper.is_file() {
        log(Level::Err, "--- wrapper.log 最后 10 行 ---");
        if let Ok(content) = fs::read_to_string(&wrapper) {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(10);
            for l in &lines[start..] {
                log(Level::Err, &format!("  {}", l));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn repair_component(
    name: &str,
    root: &Path,
    node_exe: &Path,
    reinstall: bool,
    check_only: bool,
    issues: &mut u32,
) {
    let comp_dir = root.join("bin").join(name).join(name);
    let install_js = comp_dir.join("service.install.js");
    let uninstall_js = comp_dir.join("service.uninstall.js");

    log(Level::Step, &format!("---- 组件 [{}] ----", name));
    if !comp_dir.join("koa-app.js").is_file() {
        log(
            Level::Err,
            &format!("组件目录不存在或缺文件: {}", comp_dir.display()),
        );
        *issues += 1;
        return;
    }

    let cfg = read_config(&comp_dir);
    let svc_name = cfg
        .service_name
        .clone()
        .unwrap_or_else(|| name.to_string());
    let port = cfg.port.unwrap_or_else(|| {
        if name == "artemis-portal" {
            9018
        } else {
            9017
        }
    });
    log(Level::Info, &format!("服务名={} 端口={}", svc_name, port));

    let mut state = service_state(&svc_name);

    // ---- 仅检查 ----
    if check_only {
        match state {
            SvcState::Running => {
                let up = port_listening(port);
                let http = http_up(port, name);
                let st = if up && http {
                    "端口监听, HTTP 正常"
                } else if up {
                    "端口监听, HTTP 异常"
                } else {
                    "端口未监听"
                };
                if up && http {
                    log(Level::Ok, &format!("服务运行中, {}", st));
                } else {
                    log(Level::Err, &format!("服务运行中, {}", st));
                    *issues += 1;
                }
            }
            other => {
                let st = match other {
                    SvcState::Missing => "未安装".to_string(),
                    s => format!("{:?}", s),
                };
                log(Level::Err, &format!("服务未运行 (Status={})", st));
                *issues += 1;
            }
        }
        return;
    }

    // ---- 修复 ----
    let mut need_reinstall = reinstall || state == SvcState::Missing;

    if !need_reinstall && state != SvcState::Running {
        log(Level::Info, "服务存在但未运行, 先尝试直接启动...");
        start_service(&svc_name);
        thread::sleep(Duration::from_secs(5));
        state = service_state(&svc_name);
        if state == SvcState::Running && port_listening(port) {
            log(Level::Ok, &format!("启动成功, 端口 {} 已监听。", port));
        } else {
            log(Level::Warn, "直接启动失败或端口未监听, 转入重装流程。");
            need_reinstall = true;
        }
    }

    if need_reinstall {
        if state != SvcState::Missing {
            log(Level::Info, &format!("卸载旧服务 {} ...", svc_name));
            run_node_logged(node_exe, &uninstall_js, None, "uninstall");
            stop_service(&svc_name);
            delete_service(&svc_name);
            thread::sleep(Duration::from_secs(3));
        }
        log(Level::Info, "重装服务 (重建 daemon 守护进程)...");
        run_node_logged(node_exe, &install_js, Some(&comp_dir), "install");

        // service.install.js 安装后通常会自动 start, 这里再兜底启动一次
        thread::sleep(Duration::from_secs(5));
        let st = service_state(&svc_name);
        if st != SvcState::Missing && st != SvcState::Running {
            start_service(&svc_name);
        }
    }

    // 等待端口监听 (最多 60 秒)
    let mut ok = false;
    for _ in 0..12 {
        if port_listening(port) {
            ok = true;
            break;
        }
        thread::sleep(Duration::from_secs(5));
    }
    let http = ok && http_up(port, name);

    if ok && http {
        log(
            Level::Ok,
            &format!(
                "[{}] 修复成功: 服务运行中, 端口 {} 监听, HTTP 响应正常。",
                name, port
            ),
        );
    } else if ok {
        log(
            Level::Warn,
            &format!(
                "[{}] 端口 {} 已监听, 但 HTTP 检查未通过 (可能仍在初始化, 稍后刷新页面确认)。",
                name, port
            ),
        );
        *issues += 1;
    } else {
        log(
            Level::Err,
            &format!(
                "[{}] 修复失败: 端口 {} 未监听。请查看 daemon\\wrapper.log 与 log.txt。",
                name, port
            ),
        );
        print_wrapper_tail(&comp_dir, &svc_name);
        *issues += 1;
    }
}

// ------------------------------- 主流程 ------------------------------------
#[derive(Clone, Copy, PartialEq)]
enum Action {
    Repair,
    CheckOnly,
    Reinstall,
}

fn run_flow(action: Action, root_arg: Option<String>) -> i32 {
    let mut issues: u32 = 0;
    let check_only = action == Action::CheckOnly;
    let reinstall = action == Action::Reinstall;

    // == 1. 定位安装目录 ==
    let mut root = PathBuf::from(root_arg.unwrap_or_else(|| DEFAULT_OPENAPI_ROOT.to_string()));
    if !root.join("bin").is_dir() {
        log(
            Level::Err,
            &format!("未找到 OpenAPI 目录: {}", root.display()),
        );
        if let Some(alt) = find_artemis_dir(Path::new(SEARCH_BASE)) {
            root = alt;
            log(Level::Warn, &format!("自动定位到: {}", root.display()));
        } else {
            log(Level::Err, "本机未安装 iSecure VMS OpenAPI 组件, 退出。");
            return 1;
        }
    }
    log(Level::Ok, &format!("OpenAPI 根目录: {}", root.display()));

    // == 2. 定位 node.exe ==
    let node_dir = root
        .parent()
        .map(|p| p.join("nodejs"))
        .unwrap_or_else(|| root.join("..").join("nodejs"));
    let node = match find_node_exe(&node_dir) {
        Some(n) => n,
        None => {
            log(Level::Err, "未找到内置 node.exe (OpenAPI\\nodejs\\), Web 组件无法运行。");
            return 1;
        }
    };
    log(Level::Ok, &format!("node.exe: {}", node.display()));

    // == 3. 前置组件检查 (缺失仅告警) ==
    log(Level::Step, "---- 前置组件检查 ----");
    const PREREQ: &[(u16, &str)] = &[
        (9016, "artemis 网关(Java)"),
        (7019, "redis"),
        (5432, "postgresql"),
        (9000, "minio"),
        (443, "nginx"),
    ];
    for (p, desc) in PREREQ {
        if port_listening(*p) {
            log(Level::Ok, &format!("{} 端口 {}: 正常监听", desc, p));
        } else {
            log(
                Level::Warn,
                &format!("{} 端口 {}: 未监听, web/portal 能启动但功能可能异常", desc, p),
            );
            issues += 1;
        }
    }

    // == 4. 组件修复 ==
    repair_component("artemis-web", &root, &node, reinstall, check_only, &mut issues);
    repair_component(
        "artemis-portal",
        &root,
        &node,
        reinstall,
        check_only,
        &mut issues,
    );

    // == 5. 汇总 ==
    log(Level::Step, "==== 汇总 ====");
    if check_only {
        log(
            Level::Info,
            &format!("诊断完成 (未做任何修改)。发现问题数: {}", issues),
        );
    } else {
        log(Level::Info, &format!("修复流程完成。异常项: {}", issues));
        log(
            Level::Info,
            "验证入口(本机): http://127.0.0.1:9017/artemis-web/  http://127.0.0.1:9018/artemis-portal/",
        );
        log(
            Level::Info,
            "经 nginx 443 对外: https://<本机IP>/artemis-web/  https://<本机IP>/artemis-portal/",
        );
    }
    if issues > 0 {
        1
    } else {
        0
    }
}

fn choose_action_interactive(admin: bool) -> Option<Action> {
    loop {
        println!();
        println!("============================================================");
        println!("  RepairArtemisWeb — iSecure VMS OpenAPI 组件修复工具");
        println!("============================================================");
        println!("  开发人: 余志强    QQ: 379008610    主页: https://github.com/xiaoyuzhi");
        if !admin {
            println!("  [!] 当前未以管理员身份运行: 选项 1 / 3 需要管理员权限");
            println!("      (可右键“以管理员身份运行”本程序)");
        }
        println!("  [1] 标准修复  自动修复未运行的 artemis-web / artemis-portal 服务");
        println!("  [2] 仅检查    只诊断不修改 (无需管理员权限)");
        println!("  [3] 强制重装  即使服务运行中, 也卸载重装 (需要管理员权限)");
        println!("  [0] 退出");
        print!("  请输入序号并回车 (直接回车默认 1): ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                log(Level::Warn, "无法读取交互输入, 按标准修复执行。");
                return Some(Action::Repair);
            }
            Ok(_) => {}
        }
        // 只保留 ASCII 字母/数字用于匹配, 兼容 UTF-16/杂散控制符等管道输入
        let key: String = line
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        match key.as_str() {
            "" => return Some(Action::Repair),
            "1" | "s" => return Some(Action::Repair),
            "2" | "c" => return Some(Action::CheckOnly),
            "3" | "r" => return Some(Action::Reinstall),
            "0" | "q" => {
                println!("已退出。");
                return None;
            }
            _ => println!("  无效输入, 请重新选择。"),
        }
    }
}

fn usage() {
    println!("RepairArtemisWeb — iSecure VMS OpenAPI artemis-web / artemis-portal 服务修复工具");
    println!();
    println!("用法:");
    println!("  RepairArtemisWeb.exe [选项]");
    println!();
    println!("选项:");
    println!("  -c, --check-only   仅诊断, 不做任何修改 (无需管理员权限)");
    println!("  -r, --reinstall    强制卸载并重装服务 (即使服务正在运行)");
    println!("  --root <目录>      指定 OpenAPI 根目录");
    println!("                     默认: {}", DEFAULT_OPENAPI_ROOT);
    println!("  -h, --help         显示此帮助");
    println!();
    println!("不带任何参数运行会进入中文交互式菜单。");
    println!();
    println!("开发人: 余志强    QQ: 379008610    主页: https://github.com/xiaoyuzhi");
    println!("版权: Copyright (c) 2026 余志强 (Yu Zhiqiang). All rights reserved.");
}

fn main() {
    enable_vt();

    let args: Vec<String> = env::args().skip(1).collect();
    let mut check_only = false;
    let mut reinstall = false;
    let mut root_arg: Option<String> = None;
    let mut has_cli = false;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].to_ascii_lowercase();
        match a.as_str() {
            "-c" | "--check-only" | "/c" | "check-only" => {
                check_only = true;
                has_cli = true;
            }
            "-r" | "--reinstall" | "/r" => {
                reinstall = true;
                has_cli = true;
            }
            "--root" | "-root" | "--openapi-root" | "/root" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("错误: {} 需要目录参数。", args[i - 1]);
                    usage();
                    std::process::exit(2);
                }
                root_arg = Some(args[i].clone());
                has_cli = true;
            }
            "-h" | "--help" | "/?" | "help" => {
                usage();
                std::process::exit(0);
            }
            _ => {
                eprintln!("错误: 未知参数 \"{}\"。", args[i]);
                usage();
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let admin = is_admin();

    // 有命令行参数 -> 命令行模式; 否则进入交互菜单。
    let action: Option<Action> = if has_cli {
        Some(if check_only {
            Action::CheckOnly
        } else if reinstall {
            Action::Reinstall
        } else {
            Action::Repair
        })
    } else {
        log(
            Level::Step,
            "==== iSecure VMS OpenAPI artemis-web/portal 修复工具 ====",
        );
        choose_action_interactive(admin)
    };

    // 菜单选择"0 退出"
    let Some(action) = action else {
        pause_if_console();
        std::process::exit(0);
    };

    if !admin && action != Action::CheckOnly {
        log(
            Level::Err,
            "本工具需要管理员权限(注册/启动 Windows 服务), 请以管理员身份运行。",
        );
        pause_if_console();
        std::process::exit(2);
    }

    let code = run_flow(action, root_arg);
    pause_if_console();
    std::process::exit(code);
}
