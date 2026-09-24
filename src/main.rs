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

// Task 6-9 才把这些模块接入生产路径; 在此之前允许尚未被消费的条目。
// Task 10 收尾时必须移除该 allow 并确认无 dead_code 告警。
#[allow(dead_code)]
mod logs;
#[allow(dead_code)]
mod model;
#[allow(dead_code)]
mod nginx;
#[allow(dead_code)]
mod probe;

use std::env;
use std::ffi::c_void;
use std::fs;
use std::io::{self, Write};
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

// ------------------------------ 修复阶梯决策 --------------------------------
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
fn plan_repair(kind: model::Kind, state: SvcState, l1_up: bool, reinstall: bool) -> Vec<RepairStep> {
    use model::Kind::*;
    use RepairStep::*;
    match kind {
        Node => {
            if state == SvcState::Running && l1_up && !reinstall {
                return Vec::new();
            }
            match state {
                SvcState::Stopped => vec![ScStart],
                _ => vec![NodeUninstall, ScStop, ScDelete, NodeInstall, ScStart],
            }
        }
        Prunsrv => {
            // spec §6 步骤 4: --reinstall 只是"端口起不来时允许升级到重装",
            // 不能拿它去重装一个已经健康的网关 (D2 的保守一级正在这里)
            if state == SvcState::Running && l1_up {
                return Vec::new();
            }
            match state {
                SvcState::Missing => vec![PrunsrvInstall, ScStart],
                SvcState::Stopped => vec![ScStart],
                _ if reinstall => vec![PrunsrvRestart, PrunsrvReinstall],
                _ => vec![PrunsrvRestart],
            }
        }
    }
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
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => match p.metadata() {
                    Ok(m) => m.file_type(),
                    // 单个条目读不出类型就跳过, 不得中断整棵树的查找
                    Err(_) => continue,
                },
            };
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

// --------------------------------- 修复流程 ---------------------------------

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

/// spec §8: 失败时按 kind tail 正确的日志文件, 并给特征归因结论。
fn dump_component_logs(rc: &model::ResolvedComponent) {
    let files: Vec<PathBuf> = match rc.def.kind {
        model::Kind::Node => {
            let d = rc.dir.join("daemon");
            let mut v = vec![
                d.join(format!("{}.err.log", rc.svc_name)),
                d.join(format!("{}.out.log", rc.svc_name)),
                d.join(format!("{}.wrapper.log", rc.svc_name)),
            ];
            if let Ok(t) = fs::read_to_string(rc.dir.join("config.properties")) {
                if let Some(p) = model::parse_properties(&t).get("log.path").cloned() {
                    v.push(rc.dir.join(p));
                }
            }
            v
        }
        model::Kind::Prunsrv => {
            let d = rc.dir.join("logs");
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
        let b = match logs::tail_file(f, logs::TAIL_LIMIT_BYTES) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let s = String::from_utf8_lossy(&b).into_owned();
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        log(Level::Err, &format!("--- {} (尾部 {}B) ---", name, b.len()));
        let head: Vec<&str> = s.lines().rev().take(10).collect();
        for l in head.into_iter().rev() {
            log(Level::Err, &format!("  {}", l));
        }
        all.push_str(&s);
    }
    match logs::classify_log(&all) {
        Some(why) => log(Level::Err, &format!("日志特征归因: {}", why)),
        None => log(Level::Info, "日志未命中已知故障特征。"),
    }
}

/// 等到端口监听为止, 上限为该组件的 start_timeout。
fn wait_for_port(rc: &model::ResolvedComponent) -> bool {
    let deadline = rc.def.start_timeout_secs;
    let mut waited = 0u64;
    while waited < deadline {
        if probe::port_listening(rc.port) {
            return true;
        }
        thread::sleep(Duration::from_secs(5));
        waited += 5;
    }
    probe::port_listening(rc.port)
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

/// 返回该组件对异常计数的贡献: 0 正常, 1 异常, 3 nginx 层。
fn repair_component(
    rc: &model::ResolvedComponent,
    node_exe: &Path,
    reinstall: bool,
    check_only: bool,
    assume_yes: bool,
    issues: &mut u32,
) -> u8 {
    log(Level::Step, &format!("---- 组件 [{}] ----", rc.def.key));
    if !rc.present {
        log(
            Level::Err,
            &format!("组件不存在: {}", rc.dir.join(rc.def.present_marker).display()),
        );
        *issues += 1;
        return 1;
    }
    for w in &rc.warnings {
        log(Level::Warn, w);
    }
    let state = service_state(&rc.svc_name);
    let mut l1 = probe::port_listening(rc.port);
    log(
        Level::Info,
        &format!(
            "服务名={} 端口={} 状态={:?} L1={}",
            rc.svc_name, rc.port, state, l1
        ),
    );

    if check_only {
        let l2 = if l1 {
            Some(probe::http_probe_against(rc.port, &rc.pathname, &rc.def.l2_ok))
        } else {
            None
        };
        let v = model::attribute(
            if l1 { model::L1::Listening } else { model::L1::NotListening },
            l2,
            None,
        );
        log(
            if v.exit_contrib == 0 { Level::Ok } else { Level::Err },
            &v.cause,
        );
        *issues += v.exit_contrib as u32;
        return v.exit_contrib;
    }

    // spec §6 步骤 3: 服务在跑但端口未起, 先等到 start_timeout, 别急着 restart
    if state == SvcState::Running && !l1 {
        log(
            Level::Info,
            &format!("服务运行中但端口未监听, 等待至 {}s ...", rc.def.start_timeout_secs),
        );
        l1 = wait_for_port(rc);
    }

    let steps = plan_repair(rc.def.kind, state, l1, reinstall);
    if steps.is_empty() {
        log(Level::Ok, "无需修复。");
        return 0;
    }
    if rc.def.kind == model::Kind::Prunsrv
        && steps.contains(&RepairStep::PrunsrvReinstall)
        && !assume_yes
        && !confirm(&format!(
            "即将卸载并重装 Java 网关服务 {} (影响整个 OpenAPI 平台), 确认? [y/N]",
            rc.svc_name
        ))
    {
        log(Level::Warn, "用户取消网关重装。");
        *issues += 1;
        return 1;
    }

    for st in steps {
        match st {
            RepairStep::ScStart => {
                log(Level::Info, &format!("sc start {}", rc.svc_name));
                start_service(&rc.svc_name);
            }
            RepairStep::ScStop => stop_service(&rc.svc_name),
            RepairStep::ScDelete => delete_service(&rc.svc_name),
            RepairStep::NodeUninstall => {
                run_node_logged(node_exe, &rc.dir.join("service.uninstall.js"), None, "uninstall")
            }
            RepairStep::NodeInstall => run_node_logged(
                node_exe,
                &rc.dir.join("service.install.js"),
                Some(&rc.dir),
                "install",
            ),
            RepairStep::PrunsrvInstall => run_prunsrv_bat(&rc.dir, "install", "prunsrv"),
            RepairStep::PrunsrvRestart => run_prunsrv_bat(&rc.dir, "restart", "prunsrv"),
            RepairStep::PrunsrvReinstall => {
                run_prunsrv_bat(&rc.dir, "uninstall", "prunsrv");
                thread::sleep(Duration::from_secs(3));
                run_prunsrv_bat(&rc.dir, "install", "prunsrv");
            }
        }
    }

    if !probe::port_listening(rc.port) && !wait_for_port(rc) {
        log(
            Level::Err,
            &format!("[{}] 修复失败: 端口 {} 未监听。", rc.def.key, rc.port),
        );
        dump_component_logs(rc);
        *issues += 1;
        return 1;
    }
    let l2 = probe::http_probe_against(rc.port, &rc.pathname, &rc.def.l2_ok);
    let verdict = model::attribute(model::L1::Listening, Some(l2), None);
    log(
        if verdict.exit_contrib == 0 { Level::Ok } else { Level::Warn },
        &format!("[{}] 端口 {} 已监听; {}", rc.def.key, rc.port, verdict.cause),
    );
    *issues += verdict.exit_contrib as u32;
    verdict.exit_contrib
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
        if probe::port_listening(*p) {
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
    // Task 9 会按 --components 选项重组这一段, 这里先接上新的 resolve/repair 契约
    for def in model::COMPONENTS {
        let rc = model::resolve_component(def, &root);
        repair_component(&rc, &node, reinstall, check_only, false, &mut issues);
    }

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

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn node_exe取字典序最大者并递归遍历子树() {
        let dir = std::env::temp_dir().join(format!("raw_t6_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // 用 join 链构造路径, 避免字面量里的反斜杠被当成转义序列
        let a = dir.join("a").join("node-v10-win-x64");
        let b = dir.join("b").join("node-v14-win-x64");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("node.exe"), b"x").unwrap();
        fs::write(b.join("node.exe"), b"x").unwrap();
        assert_eq!(find_node_exe(&dir), Some(b.join("node.exe")));
        // 目录里一个 node.exe 都没有时必须返回 None, 而不是 panic
        let bare = std::env::temp_dir().join(format!("raw_t6b_{}", std::process::id()));
        fs::create_dir_all(&bare).unwrap();
        assert_eq!(find_node_exe(&bare), None);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&bare);
    }

    #[test]
    fn 网关默认只到restart重装需显式授权() {
        // spec §6 Prunsrv 阶梯 + D2
        let s = plan_repair(model::Kind::Prunsrv, SvcState::Running, false, false);
        assert_eq!(s, vec![RepairStep::PrunsrvRestart]);
        let s = plan_repair(model::Kind::Prunsrv, SvcState::Running, false, true);
        assert_eq!(
            s,
            vec![RepairStep::PrunsrvRestart, RepairStep::PrunsrvReinstall]
        );
        // 服务在跑但端口已起 -> 什么都不做
        assert_eq!(
            plan_repair(model::Kind::Prunsrv, SvcState::Running, true, true),
            Vec::new()
        );
        // Missing -> install 后再 sc start (spec §6 步骤 1)
        assert_eq!(
            plan_repair(model::Kind::Prunsrv, SvcState::Missing, false, false),
            vec![RepairStep::PrunsrvInstall, RepairStep::ScStart]
        );
        // Stopped -> 仅 sc start
        assert_eq!(
            plan_repair(model::Kind::Prunsrv, SvcState::Stopped, false, false),
            vec![RepairStep::ScStart]
        );
    }

    #[test]
    fn node阶梯保持既有行为() {
        use model::Kind::Node;
        use RepairStep::*;
        assert_eq!(
            plan_repair(Node, SvcState::Missing, false, false),
            vec![NodeUninstall, ScStop, ScDelete, NodeInstall, ScStart]
        );
        assert_eq!(plan_repair(Node, SvcState::Stopped, false, false), vec![ScStart]);
        assert_eq!(plan_repair(Node, SvcState::Running, true, false), Vec::new());
        // Running 但端口未起 -> 走重装 (spec §1 行1 的成因)
        assert_eq!(
            plan_repair(Node, SvcState::Running, false, false),
            vec![NodeUninstall, ScStop, ScDelete, NodeInstall, ScStart]
        );
    }
}
