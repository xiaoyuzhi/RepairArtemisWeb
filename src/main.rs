// ============================================================================
// RepairArtemisWeb
//   iSecure VMS OpenAPI 组件修复与三层归因诊断工具
//   (基础流程复刻自 Repair-ArtemisWeb.ps1, 探针与归因为 0.2.0 新增)
//
//   * 定位 OpenAPI 安装目录与内置 node.exe;
//   * 检查前置组件 (redis / postgresql / minio / nginx);
//   * 对 artemis(Java 网关 9016) / artemis-web(9017) / artemis-portal(9018)
//     逐条做三层探针 (L1 端口 / L2 直连状态码 / L3 经本机 nginx 443),
//     按归因矩阵给出结论, 再按组件类型执行修复阶梯:
//       - node 服务   : 卸载 -> 重装 -> 启动 (原有流程);
//       - prunsrv 网关: 默认只 restart, 卸载重装需 --reinstall 且交互确认;
//   * nginx 配置只读归因 (不修改、不 reload), 识别 $artemis 回环配置;
//   * 修复失败时 tail 该组件类型的日志并按特征给出结论;
//   * 输出路由归因表, 退出码区分后端异常(1) 与 nginx 转发层故障(3)。
//
//   特性: 零第三方依赖 (纯 Rust 标准库 + 少量 Win32 FFI),
//         带参数运行 = 命令行模式, 不带参数 = 中文交互式菜单。
//
//   开发者: 余志强    QQ: 379008610
//   主页:   https://github.com/xiaoyuzhi
//   版权:   Copyright (c) 2026 余志强 (Yu Zhiqiang). All rights reserved.
// ============================================================================

mod logs;
mod model;
mod nginx;
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
        // 这里只覆盖 L1+L2, 不写"修复成功": 端到端 (L3) 在下面的归因表里才判定 (spec §13.3)
        &format!(
            "[{}] 端口 {} 已监听, 直连结论: {} (端到端见路由归因表)",
            rc.def.key, rc.port, verdict.cause
        ),
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

/// 报告表的一行: (路由, 目标端口, L1, L2, L3)。L2/L3 为 None 表示未探或跳过。
pub type RouteRow = (String, String, model::L1, Option<model::L2>, Option<model::L3>);

pub fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut i = 0;
    macro_rules! need {
        ($f:expr) => {{
            if i + 1 >= args.len() {
                return Err(format!("{} 需要参数值。", $f));
            }
            i += 1;
            args[i].clone()
        }};
    }
    while i < args.len() {
        let a = args[i].to_ascii_lowercase();
        match a.as_str() {
            "-c" | "--check-only" | "/c" | "check-only" => o.check_only = true,
            "-r" | "--reinstall" | "/r" => o.reinstall = true,
            "--yes" | "/yes" => o.assume_yes = true,
            "--no-e2e" | "/no-e2e" => o.e2e = false,
            "--root" | "-root" | "--openapi-root" | "/root" => o.root = Some(need!("--root")),
            "--nginx-root" => o.nginx_root = Some(need!("--nginx-root")),
            "--e2e-host" => o.e2e_host = need!("--e2e-host"),
            "--components" => {
                let v = need!("--components");
                let list: Vec<String> = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
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
            "-h" | "--help" | "/?" | "help" => return Err(HELP.to_string()),
            _ => return Err(format!("未知参数 \"{}\"。", args[i])),
        }
        i += 1;
    }
    Ok(o)
}

/// parse_args 用它表示"该打印帮助", 避免拿错误码字符串做判断。
const HELP: &str = "\u{0}HELP";

/// spec §9 报告表。每行: 路由 / 目标端口 / L1 / L2 / L3 / 结论。
pub fn render_route_table(rows: &[RouteRow]) -> String {
    use model::{L1, L2, L3};
    let mut s = String::new();
    s.push_str("路由              目标    L1      L2直连      L3经nginx   结论\n");
    for (path, target, l1, l2, l3) in rows {
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
            "{:<16}  {:<7}  {:<7}  {:<11}  {:<11}  {}\n",
            path, target, l1s, l2s, l3s, v.cause
        ));
    }
    s
}

fn run_flow(opts: &Options) -> i32 {
    let mut issues: u32 = 0;
    let mut exit_max: u8 = 0;

    // == 1. 定位安装目录 ==
    let mut root = PathBuf::from(opts.root.clone().unwrap_or_else(|| {
        DEFAULT_OPENAPI_ROOT.to_string()
    }));
    if !root.join("bin").is_dir() {
        log(Level::Err, &format!("未找到 OpenAPI 目录: {}", root.display()));
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
    // 9016 不在表里: 它现在是被修组件, 不再当前置告警
    log(Level::Step, "---- 前置组件检查 ----");
    const PREREQ: &[(u16, &str)] = &[
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

    // == 4. nginx 归因 (只读, spec §7) ==
    let nginx_root = match &opts.nginx_root {
        Some(s) => Some(PathBuf::from(s)),
        None => nginx::locate_nginx_conf(Path::new(SEARCH_BASE)),
    };
    match &nginx_root {
        Some(nr) => {
            let info = nginx::load_nginx_info(nr);
            log(Level::Ok, &format!("nginx 根目录: {}", info.root.display()));
            // 只报 /artemis* 相关路由: 全量 proxy_pass 有上百条动态路由, 会淹没归因结论
            let artemis_routes: Vec<&nginx::Route> = info
                .routes
                .iter()
                .filter(|r| r.path.starts_with("/artemis"))
                .collect();
            for r in &artemis_routes {
                let tgt = match &r.target {
                    // 端口是归因的关键: 回环时这里会显示 443 而不是后端端口
                    nginx::RouteTarget::Upstream(n) => match nginx::resolve_route_port(&info, &r.path) {
                        Some(p) => format!("upstream {}:{}", n, p),
                        None => format!("upstream {} (未定义)", n),
                    },
                    nginx::RouteTarget::Dynamic => "动态路由 (静态不可判定)".into(),
                    nginx::RouteTarget::None => "无 proxy_pass".into(),
                };
                log(Level::Info, &format!("  location {} -> {}", r.path, tgt));
            }
            log(
                Level::Info,
                &format!(
                    "  nginx 配置共 {} 条 location, 其中 /artemis* {} 条",
                    info.routes.len(),
                    artemis_routes.len()
                ),
            );
            if let (Some(m), Some(loc)) = (&info.artemis_mode, &info.artemis_mode_loc) {
                log(Level::Info, &format!("  set $artemis = \"{}\"  ({})", m, loc));
            }
            if info.loopback_risk() {
                log(
                    Level::Err,
                    "$artemis 非 \"local\": /artemis* 会回环到 nginx 自身 (https_artemis_remote=127.0.0.1:443), 后端服务无需重启",
                );
                exit_max = exit_max.max(3);
                issues += 1;
            }
        }
        None => {
            log(Level::Warn, "未定位到 nginx, L3 端到端与 nginx 归因跳过。");
        }
    }

    // == 5. 组件修复 ==
    let mut selected: Vec<model::ResolvedComponent> = Vec::new();
    for key in &opts.components {
        let def = match model::COMPONENTS.iter().find(|c| c.key == *key) {
            Some(d) => d,
            None => continue, // parse_args 已挡掉未知组件
        };
        let rc = model::resolve_component(def, &root);
        let contrib =
            repair_component(&rc, &node, opts.reinstall, opts.check_only, opts.assume_yes, &mut issues);
        exit_max = exit_max.max(contrib);
        selected.push(rc);
    }

    // == 6. 三层复测 + 报告表 ==
    let nginx_up = probe::port_listening(443);
    let mut rows: Vec<RouteRow> = Vec::new();
    for rc in &selected {
        if !rc.present {
            continue;
        }
        let l1 = probe::port_listening(rc.port);
        let l2 = if l1 {
            Some(probe::http_probe_against(rc.port, &rc.pathname, &rc.def.l2_ok))
        } else {
            None
        };
        let l3 = if !opts.e2e {
            Some(model::L3::Skipped)
        } else {
            let raw = probe::https_probe(&opts.e2e_host, 443, &format!("{}/", rc.pathname));
            Some(probe::classify_l3(raw, &rc.def.l2_ok, nginx_up))
        };
        rows.push((
            rc.pathname.clone(),
            format!("{}", rc.port),
            if l1 { model::L1::Listening } else { model::L1::NotListening },
            l2,
            l3,
        ));
    }
    if !rows.is_empty() {
        log(Level::Step, "==== 路由归因表 ====");
        for line in render_route_table(&rows).lines() {
            log(Level::Info, line);
        }
    }
    for (path, target, l1, l2, l3) in &rows {
        let v = model::attribute(*l1, *l2, *l3);
        exit_max = exit_max.max(v.exit_contrib);
        if v.exit_contrib == 0 {
            continue;
        }
        let hint = v.action.next_step();
        // 端口在听但没 HTTP 响应: 把占用者 PID 一起报出来, 否则用户只能自己 netstat
        let occupier = match l2 {
            Some(model::L2::NoHttpResponse) => target
                .parse::<u16>()
                .ok()
                .and_then(probe::port_owner_pid)
                .map(|pid| format!("占用进程 PID={}", pid)),
            _ => None,
        };
        let msg = match (&occupier, hint) {
            (Some(p), h) if !h.is_empty() => format!("{}: {} | {}", path, p, h),
            (Some(p), _) => format!("{}: {}", path, p),
            (None, h) if !h.is_empty() => format!("{}: {}", path, h),
            _ => continue,
        };
        log(Level::Warn, &msg);
    }

    // == 7. 汇总 ==
    log(Level::Step, "==== 汇总 ====");
    // spec §13.3: 只有 L1+L2+L3 全部通过才说"修复成功"
    let all_pass = !rows.is_empty()
        && rows
            .iter()
            .all(|(_, _, l1, l2, l3)| model::attribute(*l1, *l2, *l3).exit_contrib == 0);
    if !opts.check_only && all_pass {
        log(Level::Ok, "修复成功: 全部路由的 L1/L2/L3 三层均通过。");
    }
    if opts.check_only {
        log(Level::Info, &format!("诊断完成 (未做任何修改)。异常项: {}", issues));
    } else {
        log(Level::Info, &format!("修复流程完成。异常项: {}", issues));
    }
    if issues > 0 && exit_max < 3 {
        1
    } else {
        exit_max as i32
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
        println!("  [1] 标准修复  三层探针 + 按类型修复 (Java 网关默认只 restart)");
        println!("  [2] 仅检查    三层探针 + nginx 归因, 只读, 无需管理员");
        println!("  [3] 强制重装  含网关 uninstall/install, 需交互确认");
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
    println!("RepairArtemisWeb — iSecure VMS OpenAPI 组件修复与三层归因诊断工具");
    println!();
    println!("用法:");
    println!("  RepairArtemisWeb.exe [选项]");
    println!();
    println!("选项:");
    println!("  -c, --check-only   仅诊断, 不做任何修改 (无需管理员权限)");
    println!("  -r, --reinstall    允许卸载并重装服务 (含 Java 网关)");
    println!("      --components <a,b>  只处理指定组件, 默认 artemis,artemis-web,artemis-portal");
    println!("      --root <目录>       指定 OpenAPI 根目录");
    println!("                          默认: {}", DEFAULT_OPENAPI_ROOT);
    println!("      --nginx-root <目录> 指定 nginx 根目录 (覆盖自动定位)");
    println!("      --no-e2e            跳过 L3 端到端验收 (无 nginx / 离线环境)");
    println!("      --e2e-host <主机>   L3 目标主机, 默认 127.0.0.1");
    println!("      --yes               跳过网关重装的交互确认");
    println!("  -h, --help         显示此帮助");
    println!();
    println!("退出码: 0 无异常 · 1 存在异常或修复失败 · 2 参数错误 · 3 后端健康但 nginx 转发层故障");
    println!();
    println!("不带任何参数运行会进入中文交互式菜单。");
    println!();
    println!("开发人: 余志强    QQ: 379008610    主页: https://github.com/xiaoyuzhi");
    println!("版权: Copyright (c) 2026 余志强 (Yu Zhiqiang). All rights reserved.");
}

fn main() {
    enable_vt();

    let args: Vec<String> = env::args().skip(1).collect();
    let opts = if args.is_empty() {
        match choose_action_interactive(is_admin()) {
            Some(a) => Options {
                check_only: a == Action::CheckOnly,
                reinstall: a == Action::Reinstall,
                ..Default::default()
            },
            None => {
                pause_if_console();
                std::process::exit(0);
            }
        }
    } else {
        match parse_args(&args) {
            Ok(o) => o,
            Err(e) if e == HELP => {
                usage();
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("错误: {}", e);
                usage();
                pause_if_console();
                std::process::exit(2);
            }
        }
    };

    if !is_admin() && !opts.check_only {
        log(
            Level::Err,
            "本工具需要管理员权限(注册/启动 Windows 服务), 请以管理员身份运行。",
        );
        pause_if_console();
        std::process::exit(2);
    }

    let code = run_flow(&opts);
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

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 参数解析覆盖新选项() {
        let o = parse_args(&argv(&[
            "--check-only",
            "--components",
            "artemis-web,artemis-portal",
            "--no-e2e",
            "--nginx-root",
            "D:/x",
        ]))
        .unwrap();
        assert!(o.check_only);
        assert_eq!(
            o.components,
            vec!["artemis-web".to_string(), "artemis-portal".to_string()]
        );
        assert!(!o.e2e);
        assert_eq!(o.nginx_root.as_deref(), Some("D:/x"));
        assert_eq!(o.e2e_host, "127.0.0.1");
    }

    #[test]
    fn 默认组件表含三组件() {
        let o = parse_args(&argv(&["--root", "C:/r"])).unwrap();
        assert_eq!(
            o.components,
            vec!["artemis".to_string(), "artemis-web".to_string(), "artemis-portal".to_string()]
        );
        assert_eq!(o.root.as_deref(), Some("C:/r"));
        assert!(!o.check_only && !o.reinstall && o.e2e);
    }

    #[test]
    fn 未知参数与缺值报错不panic() {
        assert!(parse_args(&argv(&["--bogus"])).is_err());
        assert!(parse_args(&argv(&["--components"])).is_err());
        assert!(parse_args(&argv(&["--root"])).is_err());
        // 未知组件名必须报错, 不能静默忽略
        assert!(parse_args(&argv(&["--components", "artemis-webb"])).is_err());
        assert!(parse_args(&argv(&["--components", " , "])).is_err());
    }

    #[test]
    fn 报告表把不一致行显式标出() {
        use model::{L1, L2, L3};
        let t = render_route_table(&[
            (
                "/artemis-web".into(),
                "9017".into(),
                L1::Listening,
                Some(L2::Ok(200)),
                Some(L3::Bad(502)),
            ),
            (
                "/artemis".into(),
                "9016".into(),
                L1::NotListening,
                None,
                None,
            ),
        ]);
        assert!(t.contains("/artemis-web"), "{}", t);
        assert!(t.contains("502"), "必须显示实际状态码: {}", t);
        assert!(t.contains("后端健康") || t.contains("nginx"), "必须给结论: {}", t);
        assert!(t.contains("未监听"), "{}", t);
        assert!(t.contains("9017"), "必须显示目标端口: {}", t);
    }
}
