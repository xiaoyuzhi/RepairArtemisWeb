// ============================================================================
// RepairArtemisWeb (C++ 版)
//   iSecure VMS OpenAPI artemis-web / artemis-portal 组件修复工具
//
//   功能与 Rust 版 RepairArtemisWeb 完全一致:
//     * 定位 OpenAPI 安装目录与内置 node.exe;
//     * 检查前置组件 (artemis 网关 9016 / redis 7019 / postgresql 5432 /
//       minio 9000 / nginx 443);
//     * 对 artemis-web / artemis-portal 逐个修复:
//         - 服务运行中            -> 跳过 (除非 --reinstall);
//         - 服务停止              -> 直接启动, 端口起来即完成;
//         - 未安装/启动失败/端口未起 -> 卸载(service.uninstall.js)
//                                   -> 重装(service.install.js) -> 启动;
//     * 等待端口监听 + HTTP 健康检查, 输出汇总。
//
//   特性: 零第三方依赖(仅 Win32 API), 静态链接单 exe;
//         带参数运行 = 命令行模式, 不带参数 = 中文交互式菜单;
//         交互式会话结束时提示“按回车键退出”, 防止双击运行时窗口一闪而过。
//
//   开发者: 余志强    QQ: 379008610
//   主页:   https://github.com/xiaoyuzhi
//   版权:   Copyright (c) 2026 余志强 (Yu Zhiqiang). All rights reserved.
//
//   编译:
//     g++ -std=c++17 -O2 -static -municode RepairArtemisWeb.cpp \
//         -o RepairArtemisWeb.exe -lws2_32 -lshell32
// ============================================================================

#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <shellapi.h>

#include <algorithm>
#include <cstdarg>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <map>
#include <optional>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

namespace fs = std::filesystem;

static std::string ltrim(const std::string& s) {
    size_t b = s.find_first_not_of(" \t");
    return b == std::string::npos ? std::string() : s.substr(b);
}
static std::string rtrim(const std::string& s) {
    size_t e = s.find_last_not_of(" \t");
    return e == std::string::npos ? std::string() : s.substr(0, e + 1);
}
static std::string trim(const std::string& s) { return ltrim(rtrim(s)); }
static std::vector<std::string> split_lines(const std::string& text) {
    std::vector<std::string> out;
    size_t pos = 0;
    while (true) {
        size_t nl = text.find('\n', pos);
        if (nl == std::string::npos) {
            if (pos < text.size()) out.push_back(text.substr(pos));
            break;
        }
        out.push_back(text.substr(pos, nl - pos));
        pos = nl + 1;
    }
    for (std::string& s : out)
        if (!s.empty() && s.back() == '\r') s.pop_back();
    return out;
}
static bool starts_with(const std::string& s, const char* p) {
    size_t n = std::strlen(p);
    return s.size() >= n && s.compare(0, n, p) == 0;
}
static bool is_all_digits(const std::string& s) {
    if (s.empty()) return false;
    for (char c : s) if (c < '0' || c > '9') return false;
    return true;
}

// ----------------------------------------------------------------------------
// 前向声明 (本文件按 工具 -> 模型 -> 解析 -> 探针 -> 修复 组织, 存在跨段互调)
// ----------------------------------------------------------------------------
static std::wstring ascii_value_to_wide(const std::string& v);
static std::string read_file_narrow(const fs::path& p);
static std::map<std::string, std::string> parse_properties(const std::string& text);
static bool parse_status_line(const std::string& line, unsigned short& out);
static std::string ws2utf8(const std::wstring& w);

// ----------------------------------------------------------------------------
// 组件描述表 (与 Rust model::Kind / StatusSet / ComponentDef / COMPONENTS 对齐)
// ----------------------------------------------------------------------------
enum class Kind { Node, Prunsrv };

// spec §5: 不设"非 5xx 即活"的宽泛规则。
struct StatusSet {
    bool two, three, four_zero_four;
    bool accepts(unsigned short code) const {
        if (two && code >= 200 && code < 300) return true;
        if (three && code >= 300 && code < 400) return true;
        if (four_zero_four && code == 404) return true;
        return false;
    }
};
static const StatusSet SS_WEB     = { true, true, false };
static const StatusSet SS_GATEWAY = { true, true, true  };

struct ComponentDef {
    const char*   key;
    Kind          kind;
    const wchar_t* rel_dir;
    const wchar_t* present_marker;
    const wchar_t* default_svc;
    unsigned short default_port;
    const wchar_t* default_pathname;
    StatusSet     l2_ok;
    unsigned      start_timeout_secs;
};

// 顺序即修复顺序: 网关先于 web/portal。
static const ComponentDef COMPONENTS[3] = {
    { "artemis", Kind::Prunsrv, L"bin\\artemis", L"bin\\windows\\artemis.exe",
      L"artemis", 9016, L"/artemis", SS_GATEWAY, 90 },
    { "artemis-web", Kind::Node, L"bin\\artemis-web\\artemis-web", L"koa-app.js",
      L"artemis-web", 9017, L"/artemis-web", SS_WEB, 60 },
    { "artemis-portal", Kind::Node, L"bin\\artemis-portal\\artemis-portal", L"koa-app.js",
      L"artemis-portal", 9018, L"/artemis-portal", SS_WEB, 60 },
};

struct ResolvedComponent {
    const ComponentDef* def;
    fs::path              dir;
    std::wstring          svc_name;
    unsigned short        port;
    std::wstring          pathname;
    bool                  present;
    std::vector<std::string> warnings;
};

static std::string read_file_narrow(const fs::path& p) {
    std::ifstream f(p, std::ios::binary);
    if (!f) return std::string();
    return std::string((std::istreambuf_iterator<char>(f)),
                       std::istreambuf_iterator<char>());
}

// 把 "set KEY=VALUE" 归一成 properties 行后复用 parse_properties。
// 循环内不得早退, 首个不匹配行必须继续扫描。
static bool parse_server_name_from_bat(const std::string& text, std::string& out) {
    std::string props;
    size_t pos = 0;
    while (pos <= text.size()) {
        size_t nl = text.find('\n', pos);
        std::string raw = (nl == std::string::npos) ? text.substr(pos)
                                                    : text.substr(pos, nl - pos);
        pos = (nl == std::string::npos) ? text.size() + 1 : nl + 1;
        if (!raw.empty() && raw.back() == '\r') raw.pop_back();
        size_t b = raw.find_first_not_of(" \t");
        if (b == std::string::npos) continue;
        std::string t = raw.substr(b);
        if (t.compare(0, 4, "set ") != 0) continue;
        props += t.substr(4);
        props += '\n';
    }
    std::map<std::string, std::string> m = parse_properties(props);
    std::map<std::string, std::string>::iterator it = m.find("_ServerName");
    if (it == m.end()) return false;
    std::string v = it->second;
    while (!v.empty() && (v.back() == ' ' || v.back() == '\t')) v.pop_back();
    // 等价于 Rust 的 trim_matches('"'): 两端引号成对或单个都要去掉
    while (v.size() >= 2 && v.front() == '"' && v.back() == '"')
        v = v.substr(1, v.size() - 2);
    if (v.empty()) return false;
    out = v;
    return true;
}

static std::map<std::string, std::string> props_for(const ComponentDef& def,
                                                    const fs::path& dir) {
    const wchar_t* name = (def.kind == Kind::Node) ? L"config.properties"
                                                   : L"application.properties";
    return parse_properties(read_file_narrow(dir / name));
}

// 读配置得到 svc/port/pathname; 任一失败则用兜底值并记 Warn (spec §5)。
static ResolvedComponent resolve_component(const ComponentDef& def, const fs::path& root) {
    ResolvedComponent c;
    c.def = &def;
    c.dir = root / def.rel_dir;
    std::map<std::string, std::string> p = props_for(def, c.dir);

    if (def.kind == Kind::Prunsrv) {
        std::string bat = read_file_narrow(c.dir / L"bin" / L"__service.bat");
        std::string nm;
        if (!bat.empty() && parse_server_name_from_bat(bat, nm)) {
            c.svc_name = ascii_value_to_wide(nm);
        } else {
            c.warnings.push_back(std::string("未读到 _ServerName, 用兜底服务名 ") + def.key);
            c.svc_name = def.default_svc;
        }
    } else {
        std::map<std::string, std::string>::iterator it = p.find("service.name");
        if (it != p.end() && !it->second.empty()) {
            c.svc_name = ascii_value_to_wide(it->second);
        } else {
            c.warnings.push_back(std::string("未读到 service.name, 用兜底服务名 ") + def.key);
            c.svc_name = def.default_svc;
        }
    }

    std::map<std::string, std::string>::iterator pit = p.find("server.port");
    bool port_ok = false;
    c.port = def.default_port;
    if (pit != p.end()) {
        const std::string& s = pit->second;
        port_ok = !s.empty();
        for (char ch : s) if (ch < '0' || ch > '9') { port_ok = false; break; }
        if (port_ok) {
            unsigned long pv = std::strtoul(s.c_str(), nullptr, 10);
            port_ok = (pv > 0 && pv <= 65535);
            if (port_ok) c.port = (unsigned short)pv;
        }
    }
    if (!port_ok) {
        char buf[96];
        snprintf(buf, sizeof(buf), "未读到 server.port, 用兜底端口 %u", def.default_port);
        c.warnings.push_back(buf);
    }

    const char* key = (def.kind == Kind::Prunsrv) ? "server.context-path" : "server.pathname";
    std::map<std::string, std::string>::iterator tit = p.find(key);
    if (tit != p.end() && !tit->second.empty()) {
        std::string path = tit->second;
        while (path.size() > 1 && path.back() == '/') path.pop_back();  // 等价 trim_end_matches('/')
        c.pathname = ascii_value_to_wide(path);
    } else {
        c.pathname = def.default_pathname;
        c.warnings.push_back(std::string("未读到 ") + key + ", 用兜底路径 "
                             + ws2utf8(std::wstring(def.default_pathname)));
    }

    std::error_code ec;
    c.present = fs::is_regular_file(c.dir / def.present_marker, ec);
    return c;
}

// ----------------------------------------------------------------------------
// 三层探针结果与归因矩阵 (与 Rust model::L1/L2/L3/attribute 对齐, spec §6)
// ----------------------------------------------------------------------------
enum class L1 { Listening, NotListening };

enum class L2Kind { Ok, ServerError, Unexpected, NoHttpResponse };
struct L2 {
    L2Kind kind;
    unsigned short code;  // NoHttpResponse 时无意义
};

enum class L3Kind { Raw_, Ok, Bad, TlsUnavailable, NginxDown, Skipped };
struct L3 {
    L3Kind kind;
    unsigned short code;  // 仅 Ok/Bad 有意义
};

enum class VerdictAction { None_, RepairBackend, ReportOccupier, ShowLog, NginxAttrib, Unverifiable };

struct RouteVerdict {
    const char*   cause;
    VerdictAction action;
    unsigned char exit_contrib;  // 0/1/3
};

// 首次匹配, 顺序即优先级 (与 Rust 的 match 臂顺序逐条对应)。
// Rust 的 Option<L2>/Option<L3> 在此用 has2/has3 表达。
static RouteVerdict attribute(L1 l1, bool has2, L2 l2, bool has3, L3 l3) {
    if (l1 == L1::NotListening)
        return { "后端未监听端口", VerdictAction::RepairBackend, 1 };
    // 行3 端口被非 HTTP 进程占用
    if (has2 && l2.kind == L2Kind::NoHttpResponse)
        return { "端口已被非 HTTP 进程占用", VerdictAction::ReportOccupier, 1 };
    // 行4 5xx
    if (has2 && l2.kind == L2Kind::ServerError)
        return { "应用已启动但返回 5xx", VerdictAction::ShowLog, 1 };
    // 行7 非期望状态码
    if (has2 && l2.kind == L2Kind::Unexpected)
        return { "后端响应不符合期望状态码", VerdictAction::RepairBackend, 1 };
    // 行5/6 后端健康但转发层故障
    if (has2 && l2.kind == L2Kind::Ok && has3
        && (l3.kind == L3Kind::Bad || l3.kind == L3Kind::NginxDown))
        return { "后端健康, nginx 转发层故障", VerdictAction::NginxAttrib, 3 };
    // 防御: 调用方漏归类时不得把 Raw 当成正常, 也不能当成 nginx 故障
    if (has2 && l2.kind == L2Kind::Ok && has3 && l3.kind == L3Kind::Raw_)
        return { "端到端结果未经归类", VerdictAction::Unverifiable, 0 };
    // Review Focus #4: TLS 不可用不得判成后端或 nginx 故障
    if (has2 && l2.kind == L2Kind::Ok && has3 && l3.kind == L3Kind::TlsUnavailable)
        return { "端到端无法验证 (TLS 层不可用)", VerdictAction::Unverifiable, 0 };
    if (has2 && l2.kind == L2Kind::Ok)
        return { "正常", VerdictAction::None_, 0 };
    return { "后端未响应, 探针未取到结果", VerdictAction::RepairBackend, 1 };
}



// ----------------------------------------------------------------------------
// 全局
// ----------------------------------------------------------------------------
static bool g_colored = true;

static const wchar_t* DEFAULT_OPENAPI_ROOT =
    L"C:\\Program Files (x86)\\iSecure VMS\\VSM Servers\\OpenAPI\\artemis";
static const wchar_t* SEARCH_BASE = L"C:\\Program Files (x86)\\iSecure VMS";

// ----------------------------------------------------------------------------
// 控制台 VT / 管理员 / 交互判定
// ----------------------------------------------------------------------------
static void enable_vt() {
    // 控制台输出/输入代码页改为 UTF-8: 源文件中的中文字符串以 UTF-8 字节
    // 直接 printf 输出, 而中文 Windows 控制台默认按 GBK(936) 解码会乱码,
    // 因此启动时显式统一为 CP_UTF8(65001)。
    SetConsoleOutputCP(CP_UTF8);
    SetConsoleCP(CP_UTF8);

    HANDLE h = GetStdHandle(STD_OUTPUT_HANDLE);
    if (h == nullptr || h == INVALID_HANDLE_VALUE) {
        g_colored = false;
        return;
    }
    DWORD mode = 0;
    if (!GetConsoleMode(h, &mode)) {
        g_colored = false;  // stdout 被重定向
    } else {
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
}

static bool is_admin() {
    // 通过进程 Token 判断是否以管理员(高完整性)运行, 避免依赖 shell32 接口
    HANDLE hToken = nullptr;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &hToken)) return false;
    TOKEN_ELEVATION te;
    DWORD sz = 0;
    bool elevated = false;
    if (GetTokenInformation(hToken, TokenElevation, &te, sizeof te, &sz)) {
        elevated = te.TokenIsElevated != 0;
    }
    CloseHandle(hToken);
    return elevated;
}

static bool stdin_is_console() {
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (h == nullptr || h == INVALID_HANDLE_VALUE) return false;
    DWORD mode = 0;
    return GetConsoleMode(h, &mode) != 0;
}

static void pause_if_console() {
    if (!stdin_is_console()) return;
    std::printf("\n按回车键退出...");
    std::fflush(stdout);
    std::string s;
    std::getline(std::cin, s);
}

// ----------------------------------------------------------------------------
// 时间戳
// ----------------------------------------------------------------------------
static std::string now_ts() {
    SYSTEMTIME t;
    GetLocalTime(&t);
    char buf[16];
    std::snprintf(buf, sizeof buf, "%02u:%02u:%02u", t.wHour, t.wMinute, t.wSecond);
    return std::string(buf);
}

// ----------------------------------------------------------------------------
// 日志
// ----------------------------------------------------------------------------
enum class Level { Info, Ok, Warn, Err, Step };

static const char* level_tag(Level l) {
    switch (l) {
        case Level::Info: return "INFO";
        case Level::Ok:   return "OK";
        case Level::Warn: return "WARN";
        case Level::Err:  return "ERR";
        case Level::Step: return "STEP";
    }
    return "INFO";
}

static const char* level_color(Level l) {
    switch (l) {
        case Level::Info: return "\x1b[90m";
        case Level::Ok:   return "\x1b[32m";
        case Level::Warn: return "\x1b[33m";
        case Level::Err:  return "\x1b[31m";
        case Level::Step: return "\x1b[36m";
    }
    return "\x1b[0m";
}

static void log(Level l, const std::string& msg) {
    std::string text = "[" + now_ts() + "] [" + level_tag(l) + "] " + msg;
    if (g_colored) {
        std::printf("%s%s\x1b[0m\n", level_color(l), text.c_str());
    } else {
        std::printf("%s\n", text.c_str());
    }
    std::fflush(stdout);
}

// 兼容格式化版本: logf(Level::Ok, "端口 %d 正常", 9017)
static void logf(Level l, const char* fmt, ...) {
    char buf[2048];
    va_list ap;
    va_start(ap, fmt);
    std::vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    log(l, std::string(buf));
}

// ----------------------------------------------------------------------------
// 外部字节串(GBK/UTF-8 不确定)解码为 UTF-8 供日志显示
// ----------------------------------------------------------------------------
// 宽字符串直接转 UTF-8
static std::string ws2utf8(const std::wstring& w) {
    if (w.empty()) return std::string();
    int nl = WideCharToMultiByte(CP_UTF8, 0, w.data(), (int)w.size(), nullptr, 0,
                                 nullptr, nullptr);
    if (nl <= 0) return std::string();
    std::string out(nl, '\0');
    WideCharToMultiByte(CP_UTF8, 0, w.data(), (int)w.size(), &out[0], nl,
                        nullptr, nullptr);
    return out;
}

// 重载: 宽字符串参数直接转 UTF-8 (路径 / wstring 日志用)
static std::string decode_to_utf8(const std::wstring& w) { return ws2utf8(w); }

static std::string decode_to_utf8(const std::string& b) {
    if (b.empty()) return b;
    // 先按严格 UTF-8 尝试; 失败再按本机 ACP 解码
    int cp = CP_UTF8;
    int wl = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, b.data(),
                                 (int)b.size(), nullptr, 0);
    if (wl <= 0) {
        cp = CP_ACP;
        wl = MultiByteToWideChar(CP_ACP, 0, b.data(), (int)b.size(), nullptr, 0);
        if (wl <= 0) return b;
    }
    std::wstring w(wl, L'\0');
    MultiByteToWideChar(cp, cp == CP_UTF8 ? 0 : 0, b.data(), (int)b.size(),
                        &w[0], wl);
    int nl = WideCharToMultiByte(CP_UTF8, 0, w.data(), (int)w.size(), nullptr, 0,
                                 nullptr, nullptr);
    if (nl <= 0) return b;
    std::string out(nl, '\0');
    WideCharToMultiByte(CP_UTF8, 0, w.data(), (int)w.size(), &out[0], nl,
                        nullptr, nullptr);
    return out;
}

// ----------------------------------------------------------------------------
// 进程执行 (CreateProcess + 管道, 无窗口)
// ----------------------------------------------------------------------------
static std::wstring quote_arg(const std::wstring& s) {
    if (s.find_first_of(L" \t\"") == std::wstring::npos) return s;
    std::wstring q = L"\"";
    for (wchar_t c : s) {
        if (c == L'"') q += L"\\\"";
        else q += c;
    }
    q += L'"';
    return q;
}

struct RunRes {
    long exitCode;      // -1 = 未能启动
    std::string out;    // stdout + stderr 合并的原始字节
    bool launched;
};

static RunRes run_output(const std::wstring& prog,
                         const std::vector<std::wstring>& args,
                         const fs::path* cwd) {
    RunRes res{ -1, std::string(), false };

    std::wstring dirw;
    if (cwd) dirw = cwd->native();

    // 尝试的程序路径列表 (找不到时回退 System32)
    std::vector<std::wstring> cands;
    cands.push_back(prog);
    if (prog.find(L'\\') == std::wstring::npos &&
        prog.find(L'/') == std::wstring::npos) {
        cands.push_back(L"C:\\Windows\\System32\\" + prog);
    }

    for (const auto& app : cands) {
        res.launched = false;
        HANDLE hOutR = nullptr, hOutW = nullptr;
        SECURITY_ATTRIBUTES sa;
        sa.nLength = sizeof sa;
        sa.lpSecurityDescriptor = nullptr;
        sa.bInheritHandle = TRUE;
        if (!CreatePipe(&hOutR, &hOutW, &sa, 0)) return res;
        SetHandleInformation(hOutR, HANDLE_FLAG_INHERIT, 0);

        STARTUPINFOW si;
        std::memset(&si, 0, sizeof si);
        si.cb = sizeof si;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = hOutW;
        si.hStdError = hOutW;
        si.hStdInput = nullptr;  // 与 Rust Command::output 一致: stdin 关闭

        PROCESS_INFORMATION pi;
        std::memset(&pi, 0, sizeof pi);

        std::wstring cmd = quote_arg(app);
        for (const auto& a : args) {
            cmd += L' ';
            cmd += quote_arg(a);
        }

        BOOL ok = CreateProcessW(nullptr, &cmd[0], nullptr, nullptr, TRUE,
                                 CREATE_NO_WINDOW, nullptr,
                                 dirw.empty() ? nullptr : &dirw[0], &si, &pi);
        CloseHandle(hOutW);  // 父进程侧的写端立即关闭
        if (!ok) {
            CloseHandle(hOutR);
            continue;  // 尝试下一个候选路径
        }
        res.launched = true;

        // 读取合并输出 (单管道, 无死锁风险)
        std::string data;
        char buf[8192];
        DWORD n = 0;
        while (ReadFile(hOutR, buf, sizeof buf, &n, nullptr) && n > 0) {
            data.append(buf, n);
        }
        CloseHandle(hOutR);

        WaitForSingleObject(pi.hProcess, INFINITE);
        DWORD code = 0;
        GetExitCodeProcess(pi.hProcess, &code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);

        res.exitCode = (long)code;
        res.out = std::move(data);
        break;
    }
    return res;
}

static long run_status(const std::wstring& prog,
                       const std::vector<std::wstring>& args) {
    return run_output(prog, args, nullptr).exitCode;
}

static void run_node_logged(const fs::path& node_exe, const fs::path& script,
                            const fs::path* cwd, const std::string& prefix) {
    RunRes r = run_output(node_exe.native(), { script.native() }, cwd);
    if (!r.launched) {
        logf(Level::Warn, "无法执行 node.exe");
        return;
    }
    std::string text = decode_to_utf8(r.out);
    std::string line;
    for (size_t i = 0; i <= text.size(); ++i) {
        if (i == text.size() || text[i] == '\n' || text[i] == '\r') {
            if (!line.empty() && line.find_first_not_of(" \t") != std::string::npos) {
                log(Level::Info, "  [" + prefix + "] " + line);
            }
            line.clear();
            if (i < text.size() && text[i] == '\r' && i + 1 < text.size() && text[i + 1] == '\n') ++i;
        } else {
            line += text[i];
        }
    }
    if (r.exitCode != 0) {
        logf(Level::Warn, "  [%s] 脚本退出码: %ld", prefix.c_str(), r.exitCode);
    }
}

// ----------------------------------------------------------------------------
// 端口 / HTTP 检查
// ----------------------------------------------------------------------------
// 解析 netstat -ano 的一行; 处于 LISTENING 才返回 true 并写出端口与 PID
static bool netstat_listen_pid(const std::string& line,
                               unsigned short& port, unsigned long& pid) {
    std::istringstream ss(line);
    std::string proto, local, peer, state, pidtok;
    if (!(ss >> proto)) return false;
    if (proto != "TCP") return false;
    if (!(ss >> local >> peer >> state)) return false;
    if (state.find("LISTENING") == std::string::npos
        && state.find("LISTEN") == std::string::npos) return false;
    if (!(ss >> pidtok)) return false;
    if (!is_all_digits(pidtok)) return false;
    size_t colon = local.find_last_of(':');
    if (colon == std::string::npos) return false;
    std::string pstr = local.substr(colon + 1);
    if (!pstr.empty() && pstr.back() == ']') pstr.pop_back();
    if (!is_all_digits(pstr)) return false;
    unsigned long pv = std::strtoul(pstr.c_str(), nullptr, 10);
    if (pv == 0 || pv > 65535) return false;
    port = (unsigned short)pv;
    pid = std::strtoul(pidtok.c_str(), nullptr, 10);
    return true;
}

// 遍历 netstat 输出; want_pid=false 时命中即返回端口, true 时返回占用 PID
static bool scan_netstat(unsigned short want, unsigned short& got_port,
                         unsigned long& got_pid, bool want_pid) {
    RunRes r = run_output(L"netstat", { L"-ano" }, nullptr);
    for (const std::string& line : split_lines(r.out)) {
        unsigned short port; unsigned long pid;
        if (!netstat_listen_pid(line, port, pid)) continue;
        if (port != want) continue;
        if (want_pid) { got_pid = pid; return true; }
        got_port = port; return true;
    }
    return false;
}

static bool winsock_started = false;
static void ensure_winsock() {
    if (!winsock_started) {
        WSADATA wsa;
        WSAStartup(MAKEWORD(2, 2), &wsa);
        winsock_started = true;
    }
}

// TCP 回环连通测试 (超时毫秒)
static bool tcp_probe(unsigned short port, int timeoutMs) {
    ensure_winsock();
    SOCKET s = socket(AF_INET, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) return false;
    u_long nb = 1;
    ioctlsocket(s, FIONBIO, &nb);
    sockaddr_in a;
    std::memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);

    bool ok = false;
    int r = connect(s, (sockaddr*)&a, sizeof a);
    if (r == 0) {
        ok = true;
    } else if (WSAGetLastError() == WSAEWOULDBLOCK) {
        fd_set wf;
        FD_ZERO(&wf);
        FD_SET(s, &wf);
        timeval tv;
        tv.tv_sec = timeoutMs / 1000;
        tv.tv_usec = (timeoutMs % 1000) * 1000;
        int sel = select(0, nullptr, &wf, nullptr, &tv);
        if (sel == 1) {
            int err = 0;
            int len = sizeof err;
            getsockopt(s, SOL_SOCKET, SO_ERROR, (char*)&err, &len);
            ok = (err == 0);
        }
    }
    closesocket(s);
    return ok;
}

// L1: netstat 优先, TCP 连通测试兜底 (防状态字被本地化)
static bool port_listening(unsigned short port) {
    unsigned short got = 0; unsigned long dummy = 0;
    if (scan_netstat(port, got, dummy, false)) return true;
    return tcp_probe(port, 1000);
}

// spec §6 行3: 端口被非 HTTP 进程占用时报告 PID
static bool port_owner_pid(unsigned short port, unsigned long& pid) {
    unsigned short dummy = 0;
    return scan_netstat(port, dummy, pid, true);
}

// 5xx 优先归 ServerError, 其余按期望集合。spec §5: 不设"非 5xx 即活"。
static L2 classify(unsigned short code, const StatusSet& set) {
    L2 r;
    r.code = code;
    if (code >= 500 && code < 600) r.kind = L2Kind::ServerError;
    else if (set.accepts(code))    r.kind = L2Kind::Ok;
    else                           r.kind = L2Kind::Unexpected;
    return r;
}

// 明文 HTTP GET, 读到状态行为止。硬超时, 拿不到状态行 -> NoHttpResponse。
static L2 http_probe_against(unsigned short port, const std::string& pathname,
                             const StatusSet& set) {
    L2 fail; fail.kind = L2Kind::NoHttpResponse; fail.code = 0;
    ensure_winsock();
    SOCKET s = socket(AF_INET, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) return fail;
    int ms = 8000;
    setsockopt(s, SOL_SOCKET, SO_SNDTIMEO, (const char*)&ms, sizeof ms);
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, (const char*)&ms, sizeof ms);
    sockaddr_in a;
    std::memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
    std::string path = pathname.empty() ? "/" : pathname;
    char req[512];
    snprintf(req, sizeof req,
             "GET %s HTTP/1.1\r\nHost: 127.0.0.1:%u\r\n"
             "User-Agent: RepairArtemisWeb\r\nConnection: close\r\n\r\n",
             path.c_str(), port);
    if (send(s, req, (int)std::strlen(req), 0) == SOCKET_ERROR) {
        closesocket(s); return fail;
    }
    // 逐字节读到首个 CRLF 即停, 不等响应体
    std::string line;
    char prev = 0, c = 0;
    while (line.size() < 128) {
        int n = recv(s, &c, 1, 0);
        if (n <= 0) break;
        line.push_back(c);
        if (prev == '\r' && c == '\n') break;
        prev = c;
    }
    closesocket(s);
    unsigned short code = 0;
    if (!parse_status_line(line, code)) return fail;
    return classify(code, set);
}

// ----------------------------------------------------------------------------
// L3: 经本机 nginx 443 的端到端验收 (spec §6 L3 / 决策 D1b)
// 原 WinHTTP 方案被实测推翻, 详见 spec D1b。
// ----------------------------------------------------------------------------

// curl 的 -w 输出 -> L3。"000" 是 curl 拿不到任何 HTTP 响应时的固定输出。
static L3 parse_curl_output(bool exit_ok, const std::string& stdout_text) {
    L3 r; r.kind = L3Kind::TlsUnavailable; r.code = 0;
    std::string t = trim(stdout_text);
    if (t.empty() || !is_all_digits(t)) return r;
    unsigned long v = std::strtoul(t.c_str(), nullptr, 10);
    if (v == 0 || v > 65535 || !exit_ok) return r;
    r.kind = L3Kind::Raw_;
    r.code = (unsigned short)v;
    return r;
}

// host 必须是裸主机名/IP。首字符为 '-' 会被 curl 当成选项, 元字符一律拒绝。
static bool valid_e2e_host(const std::string& host) {
    if (host.empty() || host[0] == '-') return false;
    for (char c : host) {
        bool ok = std::isalnum((unsigned char)c) || c == '.' || c == '-' || c == ':';
        if (!ok) return false;
    }
    return true;
}

// 绝对路径定位 System32\curl.exe, 不依赖 PATH 查找 (避免被同名程序劫持)。
static bool curl_exe(fs::path& out) {
    wchar_t buf[4096];
    DWORD n = GetEnvironmentVariableW(L"WINDIR", buf, 4095);
    fs::path win = (n == 0 || n > 4095) ? fs::path(L"C:\\Windows") : fs::path(buf);
    fs::path p = win / L"System32" / L"curl.exe";
    std::error_code ec;
    if (!fs::is_regular_file(p, ec)) return false;
    out = p;
    return true;
}

// 443 未监听 -> NginxDown; 否则按期望集合归类。5xx 与非期望一律 Bad。
static L3 classify_l3(L3 raw, const StatusSet& set, bool nginx_up) {
    if (raw.kind == L3Kind::Raw_ && !nginx_up) { raw.kind = L3Kind::NginxDown; return raw; }
    if (raw.kind == L3Kind::TlsUnavailable && !nginx_up) { raw.kind = L3Kind::NginxDown; return raw; }
    if (raw.kind != L3Kind::Raw_) return raw;
    unsigned short c = raw.code;
    raw.kind = ((c >= 500 && c < 600) || !set.accepts(c)) ? L3Kind::Bad : L3Kind::Ok;
    return raw;
}

// 经 nginx 443 取真实状态码。curl 缺失或 host 非法 -> Skipped (降级, 不伪造)。
static L3 https_probe(const std::string& host, unsigned short port, const std::string& pathname) {
    L3 r; r.kind = L3Kind::TlsUnavailable; r.code = 0;
    fs::path exe;
    if (!curl_exe(exe)) { r.kind = L3Kind::Skipped; return r; }
    if (!valid_e2e_host(host)) { r.kind = L3Kind::Skipped; return r; }
    std::string url = "https://" + host + ":" + std::to_string(port) + pathname;
    RunRes res = run_output(exe.wstring(),
                            { L"-s", L"-k", L"-o", L"NUL", L"-m", L"10",
                              L"-w", L"%{http_code}", ascii_value_to_wide(url) },
                            nullptr);
    if (!res.launched) return r;
    return parse_curl_output(res.exitCode == 0, res.out);
}

// ----------------------------------------------------------------------------
// 服务管理 (sc.exe)
// ----------------------------------------------------------------------------
enum class SvcState { Missing, Stopped, Running, Other };

static SvcState service_state(const std::wstring& name) {
    RunRes r = run_output(L"sc.exe", { L"query", name }, nullptr);
    if (!r.launched || r.exitCode != 0) return SvcState::Missing;
    std::string out = r.out;
    size_t pos = 0;
    while (pos <= out.size()) {
        size_t nl = out.find('\n', pos);
        std::string line = (nl == std::string::npos) ? out.substr(pos)
                                                     : out.substr(pos, nl - pos);
        pos = (nl == std::string::npos) ? out.size() + 1 : nl + 1;
        // 去尾部 \r / 空白
        while (!line.empty() && (line.back() == '\r' || line.back() == '\n')) line.pop_back();
        size_t b = 0;
        while (b < line.size() && (line[b] == ' ' || line[b] == '\t')) ++b;
        if (line.compare(b, 5, "STATE") != 0) continue;
        size_t ci = line.find(':', b);
        if (ci == std::string::npos) continue;
        size_t p = ci + 1;
        while (p < line.size() && (line[p] == ' ' || line[p] == '\t')) ++p;
        size_t q = p;
        while (q < line.size() && line[q] >= '0' && line[q] <= '9') ++q;
        if (q == p) continue;
        unsigned long n = std::strtoul(line.substr(p, q - p).c_str(), nullptr, 10);
        switch (n) {
            case 1: return SvcState::Stopped;
            case 4: return SvcState::Running;
            default: return SvcState::Other;
        }
    }
    return SvcState::Other;
}

static void start_service(const std::wstring& name) {
    run_status(L"sc.exe", { L"start", name });
}

static void stop_service(const std::wstring& name) {
    run_status(L"sc.exe", { L"stop", name });
}

static void delete_service(const std::wstring& name) {
    run_status(L"sc.exe", { L"delete", name });
}

// ----------------------------------------------------------------------------
// 目录 / 文件定位
// ----------------------------------------------------------------------------
// 在 base 下递归查找名为 artemis 的目录 (取第一个, 模拟 PS 枚举)
static bool find_artemis_dir(const fs::path& base, fs::path& out) {
    std::vector<fs::path> stack;
    stack.push_back(base);
    while (!stack.empty()) {
        fs::path dir = stack.back();
        stack.pop_back();
        std::error_code ec;
        fs::directory_iterator it(dir, ec);
        if (ec) { ec.clear(); continue; }
        fs::directory_iterator end;
        for (; it != end; it.increment(ec)) {
            if (ec) { ec.clear(); break; }
            const fs::directory_entry& ent = *it;
            std::error_code ec2;
            fs::file_status st = ent.symlink_status(ec2);
            if (ec2 || !fs::is_directory(st)) continue;
            fs::path p = ent.path();
            if (_wcsicmp(p.filename().native().c_str(), L"artemis") == 0) {
                out = p;
                return true;
            }
            stack.push_back(p);
        }
    }
    return false;
}

// 在 base 下递归查找全部 node.exe, 取 FullName 字典序最大者
static bool find_node_exe(const fs::path& base, fs::path& out) {
    std::vector<std::wstring> found;
    std::vector<fs::path> stack;
    stack.push_back(base);
    while (!stack.empty()) {
        fs::path dir = stack.back();
        stack.pop_back();
        std::error_code ec;
        fs::directory_iterator it(dir, ec);
        if (ec) { ec.clear(); continue; }
        fs::directory_iterator end;
        for (; it != end; it.increment(ec)) {
            if (ec) { ec.clear(); break; }
            const fs::directory_entry& ent = *it;
            std::error_code ec2;
            fs::file_status st = ent.symlink_status(ec2);
            if (ec2) continue;
            fs::path p = ent.path();
            if (fs::is_directory(st)) {
                stack.push_back(p);
            } else if (_wcsicmp(p.filename().native().c_str(), L"node.exe") == 0) {
                found.push_back(p.native());
            }
        }
    }
    if (found.empty()) return false;
    std::sort(found.begin(), found.end(),
              [](const std::wstring& a, const std::wstring& b) { return a > b; });
    out = fs::path(found[0]);
    return true;
}

// ----------------------------------------------------------------------------
// config.properties 解析
// ----------------------------------------------------------------------------
struct CompConfig {
    std::optional<std::wstring> service_name;
    std::optional<unsigned short> port;
};

static std::string trim_ws(const std::string& s) {
    size_t b = 0, e = s.size();
    while (b < e && (s[b] == ' ' || s[b] == '\t' || s[b] == '\r')) ++b;
    while (e > b && (s[e - 1] == ' ' || s[e - 1] == '\t' || s[e - 1] == '\r')) --e;
    return s.substr(b, e - b);
}

// 将行内 ASCII 键值提取为宽字符串 (ASCII 直通)
static std::wstring ascii_value_to_wide(const std::string& v) {
    std::wstring w;
    w.reserve(v.size());
    for (unsigned char c : v) w.push_back((wchar_t)c);
    return w;
}

// ----------------------------------------------------------------------------
// 通用 properties / HTTP 状态行解析 (与 Rust model::parse_properties,
// logs::parse_status_line 逐条对齐)
// ----------------------------------------------------------------------------

// 跳过 `#` / `!` 注释行; 键取第一个 '=' 前并去尾空白; 值保留其余原样 (含后续 '=')
static std::map<std::string, std::string> parse_properties(const std::string& text) {
    std::map<std::string, std::string> m;
    size_t pos = 0;
    while (pos <= text.size()) {
        size_t nl = text.find('\n', pos);
        std::string raw = (nl == std::string::npos) ? text.substr(pos)
                                                    : text.substr(pos, nl - pos);
        if (!raw.empty() && raw.back() == '\r') raw.pop_back();
        size_t b = raw.find_first_not_of(" \t");
        if (b != std::string::npos) {
            std::string line = raw.substr(b);
            if (line[0] != '#' && line[0] != '!') {
                size_t eq = line.find('=');
                if (eq != std::string::npos) {
                    std::string key = trim_ws(line.substr(0, eq));
                    size_t vb = line.find_first_not_of(" \t", eq + 1);
                    std::string value =
                        (vb == std::string::npos) ? std::string() : line.substr(vb);
                    if (!key.empty()) m[key] = value;
                }
            }
        }
        if (nl == std::string::npos) break;
        pos = nl + 1;
    }
    return m;
}

// 仅接受 "HTTP/1.x <3位数字>" 形态; 成功时写入 out 并返回 true
static bool parse_status_line(const std::string& line, unsigned short& out) {
    size_t b = line.find_first_not_of(" \t");
    if (b == std::string::npos) return false;
    std::string t = line.substr(b);
    if (t.compare(0, 5, "HTTP/") != 0) return false;
    size_t sp = t.find_first_of(" \t", 5);
    if (sp == std::string::npos) return false;
    size_t b2 = t.find_first_not_of(" \t", sp + 1);
    if (b2 == std::string::npos) return false;
    size_t e2 = t.find_first_of(" \t", b2);
    std::string code =
        (e2 == std::string::npos) ? t.substr(b2) : t.substr(b2, e2 - b2);
    if (code.size() != 3) return false;
    for (char c : code)
        if (c < '0' || c > '9') return false;
    out = (unsigned short)std::stoi(code);
    return true;
}

// ----------------------------------------------------------------------------
// 日志尾部读取与故障特征归因 (与 Rust logs::tail_file / classify_log 对齐)
// ----------------------------------------------------------------------------
static const unsigned long long TAIL_LIMIT_BYTES = 64ull * 1024;

// 定位到尾部窗口后顺序读取, 避免整文件进内存。成功返回 true 并写入 out。
static bool tail_file(const fs::path& p, unsigned long long max, std::string& out) {
    out.clear();
    std::ifstream f(p, std::ios::binary | std::ios::ate);
    if (!f) return false;
    std::streamoff len = f.tellg();
    if (len < 0) return false;
    unsigned long long ulen = (unsigned long long)len;
    unsigned long long want = ulen < max ? ulen : max;
    if (want == 0) return true;
    f.seekg((std::streamoff)(ulen - want), std::ios::beg);
    out.resize((size_t)want);
    f.read(&out[0], (std::streamsize)want);
    std::streamsize got = f.gcount();
    out.resize(got > 0 ? (size_t)got : 0);
    return true;
}

// spec §8 特征表。顺序即优先级, 与 Rust SIGS 逐条一致; 未命中返回 nullptr。
static const char* classify_log(const std::string& text) {
    struct Sig { const char* k; const char* v; };
    static const Sig SIGS[] = {
        { "ECONNREFUSED",           "上游依赖拒绝连接" },
        { "Connection refused",     "上游依赖拒绝连接" },
        { "EADDRINUSE",             "端口冲突" },
        { "address already in use", "端口冲突" },
        { "OutOfMemoryError",       "JVM 堆不足" },
        { "heap size",              "JVM 堆不足" },
        { "ECONNRESET",             "与网关或数据库的连接被重置" },
    };
    for (const Sig& s : SIGS)
        if (text.find(s.k) != std::string::npos) return s.v;
    return nullptr;
}

static CompConfig read_config(const fs::path& dir) {
    CompConfig cfg;
    std::ifstream f(dir / L"config.properties", std::ios::binary);
    if (!f) return cfg;
    std::string content((std::istreambuf_iterator<char>(f)),
                        std::istreambuf_iterator<char>());
    // 去掉 UTF-8 BOM
    if (content.size() >= 3 && (unsigned char)content[0] == 0xEF &&
        (unsigned char)content[1] == 0xBB && (unsigned char)content[2] == 0xBF) {
        content = content.substr(3);
    }
    size_t pos = 0;
    while (pos <= content.size()) {
        size_t nl = content.find('\n', pos);
        std::string line = (nl == std::string::npos) ? content.substr(pos)
                                                     : content.substr(pos, nl - pos);
        pos = (nl == std::string::npos) ? content.size() + 1 : nl + 1;
        if (line.find('\r') != std::string::npos) line.pop_back();
        line = trim_ws(line);
        if (line.empty() || line[0] == '#' || line[0] == '!') continue;

        // 按 ASCII 键精确匹配 service.name / server.port
        const char* keys[] = { "service.name", "server.port" };
        for (int ki = 0; ki < 2; ++ki) {
            const char* key = keys[ki];
            size_t klen = std::strlen(key);
            if (line.compare(0, klen, key) != 0) continue;
            // 键后必须是 '=' (允许中间空白)
            size_t eq = line.find('=');
            if (eq == std::string::npos) continue;
            std::string kpart = trim_ws(line.substr(0, eq));
            if (kpart != key) continue;
            std::string vpart = trim_ws(line.substr(eq + 1));
            // 只取第一个 token
            size_t sp = vpart.find_first_of(" \t");
            if (sp != std::string::npos) vpart = vpart.substr(0, sp);
            if (vpart.empty()) continue;
            if (ki == 0 && !cfg.service_name) {
                cfg.service_name = ascii_value_to_wide(vpart);
            } else if (ki == 1 && !cfg.port) {
                bool dig = !vpart.empty();
                for (char c : vpart) if (c < '0' || c > '9') { dig = false; break; }
                if (dig) {
                    unsigned long p = std::strtoul(vpart.c_str(), nullptr, 10);
                    if (p > 0 && p <= 65535) cfg.port = (unsigned short)p;
                }
            }
        }
    }
    return cfg;
}

// ----------------------------------------------------------------------------
// nginx 配置只读解析 (与 Rust nginx::* 对齐, spec §7)
// 不修改、不 reload、不递归展开 include。
// ----------------------------------------------------------------------------
struct Upstream {
    std::string name, host;
    unsigned short port;
};

enum class RouteTargetKind { Upstream_, Dynamic, None_ };
struct Route {
    std::string path;
    RouteTargetKind target;
    std::string upstream;  // 仅 target == Upstream_ 时有值
};

struct NginxInfo {
    fs::path root;
    std::vector<Upstream> upstreams;
    std::vector<Route> routes;
    std::string artemis_mode;      // 空 = 未读到
    std::string artemis_mode_loc;

    // $artemis != "local" 时 /artemis* 落到 https_artemis_remote (127.0.0.1:443), 即 nginx 自身
    bool loopback_risk() const {
        if (artemis_mode.empty() || artemis_mode == "local") return false;
        for (const Upstream& u : upstreams)
            if (u.name == "https_artemis_remote" && u.port == 443) return true;
        return false;
    }
};

static std::string strip_comment(const std::string& line) {
    size_t h = line.find('#');
    return (h == std::string::npos) ? line : line.substr(0, h);
}




static std::vector<Upstream> parse_upstreams(const std::string& text) {
    std::vector<Upstream> out;
    std::vector<std::string> lines = split_lines(text);
    size_t i = 0;
    while (i < lines.size()) {
        std::string t = trim(strip_comment(lines[i]));
        ++i;
        if (!starts_with(t, "upstream")) continue;
        std::string rest = t.substr(8);
        if (!rest.empty()) {
            char f = ltrim(rest).empty() ? ' ' : ltrim(rest)[0];
            if (std::isalnum((unsigned char)f)) continue;
        }
        rest = ltrim(rest);
        std::string name;
        for (char c : rest) {
            if (c == '{' || c == ' ' || c == '\t') break;
            name += c;
        }
        if (name.empty()) continue;
        int depth = (int)std::count(t.begin(), t.end(), '{')
                  - (int)std::count(t.begin(), t.end(), '}');
        std::string body = rest;
        while (i < lines.size() && depth > 0) {
            std::string c = strip_comment(lines[i]);
            depth += (int)std::count(c.begin(), c.end(), '{')
                   - (int)std::count(c.begin(), c.end(), '}');
            body += "\n";
            body += c;
            ++i;
        }
        for (const std::string& bl : split_lines(body)) {
            std::string bt = trim(bl);
            if (!starts_with(bt, "server")) continue;
            std::string a = trim(rtrim(bt.substr(6)));
            if (!a.empty() && a.back() == ';') a.pop_back();
            a = trim(a);
            size_t colon = a.find_last_of(':');
            if (colon == std::string::npos) continue;
            std::string pstr = trim(a.substr(colon + 1));
            if (!is_all_digits(pstr)) continue;
            unsigned long pv = std::strtoul(pstr.c_str(), nullptr, 10);
            if (pv == 0 || pv > 65535) continue;
            Upstream u;
            u.name = name;
            u.host = trim(a.substr(0, colon));
            u.port = (unsigned short)pv;
            out.push_back(u);
            break;
        }
    }
    return out;
}

// 花括号配对计数提取 location 块 (Review Focus #3)。
static std::vector<Route> parse_locations(const std::string& text) {
    std::vector<Route> out;
    std::vector<std::string> lines = split_lines(text);
    size_t i = 0;
    while (i < lines.size()) {
        std::string t = trim(strip_comment(lines[i]));
        ++i;
        if (!starts_with(t, "location")) continue;
        std::string rest = t.substr(8);
        std::string lr = ltrim(rest);
        if (!lr.empty() && std::isalnum((unsigned char)lr[0])) continue;
        std::string path;
        {
            std::istringstream ss(rest);
            std::string tk;
            while (ss >> tk) {
                if (tk == "^~" || tk == "~" || tk == "~*" || tk == "="
                    || (!tk.empty() && tk[0] == '~'))
                    continue;
                while (!tk.empty() && tk.back() == '{') tk.pop_back();
                path = tk;
                break;
            }
        }
        if (path.empty()) continue;
        int depth = (int)std::count(t.begin(), t.end(), '{')
                  - (int)std::count(t.begin(), t.end(), '}');
        std::string body;
        while (i < lines.size() && depth > 0) {
            std::string c = strip_comment(lines[i]);
            depth += (int)std::count(c.begin(), c.end(), '{')
                   - (int)std::count(c.begin(), c.end(), '}');
            body += c;
            body += "\n";
            ++i;
        }
        Route r;
        r.path = path;
        r.target = RouteTargetKind::None_;
        for (const std::string& bl : split_lines(body)) {
            std::string bt = trim(bl);
            if (!starts_with(bt, "proxy_pass")) continue;
            std::string a = trim(rtrim(bt.substr(10)));
            if (!a.empty() && a.back() == ';') a.pop_back();
            a = trim(a);
            if (a.find('$') != std::string::npos) {
                r.target = RouteTargetKind::Dynamic;
                break;
            }
            size_t sl = a.find_last_of('/');
            if (sl != std::string::npos && sl + 1 < a.size()) {
                r.upstream = a.substr(sl + 1);
                r.target = RouteTargetKind::Upstream_;
                break;
            }
        }
        out.push_back(r);
    }
    return out;
}

static bool find_artemis_mode(const std::string& text, const std::string& file,
                              std::string& value, std::string& loc) {
    std::vector<std::string> lines = split_lines(text);
    for (size_t n = 0; n < lines.size(); ++n) {
        std::string t = trim(strip_comment(lines[n]));
        if (!starts_with(t, "set")) continue;
        std::istringstream ss(ltrim(t.substr(3)));
        std::string var, val;
        if (!(ss >> var)) continue;
        if (var != "$artemis") continue;
        std::string rest;
        std::getline(ss, rest);
        rest = trim(rest);
        if (!rest.empty() && rest.back() == ';') rest.pop_back();
        rest = trim(rest);
        while (rest.size() >= 2 && rest.front() == '"' && rest.back() == '"')
            rest = rest.substr(1, rest.size() - 2);
        value = rest;
        char b[64];
        snprintf(b, sizeof(b), "%s:%u", file.c_str(), (unsigned)(n + 1));
        loc = b;
        return true;
    }
    return false;
}

static std::vector<fs::path> confs_in(const fs::path& dir) {
    std::vector<fs::path> v;
    std::error_code ec;
    for (const auto& e : fs::directory_iterator(dir, ec)) {
        const fs::path p = e.path();
        if (fs::is_regular_file(p) && p.extension() == L".conf") v.push_back(p);
    }
    std::sort(v.begin(), v.end());
    return v;
}

// root 为 nginx 根目录 (含 conf/)。spec §7.3: 只扫 conf/、conf/Mode/、../ssl/ 三处。
static NginxInfo load_nginx_info(const fs::path& root) {
    NginxInfo info;
    info.root = root;
    fs::path conf = root / L"conf";
    std::vector<fs::path> files;
    for (const fs::path& p : confs_in(conf)) files.push_back(p);
    for (const fs::path& p : confs_in(conf / L"Mode")) files.push_back(p);
    for (const fs::path& p : confs_in(root / L".." / L"ssl")) files.push_back(p);

    std::string main_txt = read_file_narrow(conf / L"nginx.conf");
    if (!main_txt.empty()) {
        info.upstreams = parse_upstreams(main_txt);
        info.routes = parse_locations(main_txt);
    }
    for (const fs::path& f : files) {
        std::string text = read_file_narrow(f);
        if (text.empty()) continue;
        for (const Route& r : parse_locations(text)) {
            bool dup = false;
            for (const Route& e : info.routes) if (e.path == r.path) { dup = true; break; }
            if (!dup) info.routes.push_back(r);
        }
        if (info.artemis_mode.empty()) {
            std::string val, loc;
            if (find_artemis_mode(text, f.filename().string(), val, loc)) {
                info.artemis_mode = val;
                info.artemis_mode_loc = loc;
            }
        }
    }
    return info;
}

// 路由 -> 目标端口。remote 模式下 local 分支失效, 落到 remote upstream。
static bool resolve_route_port(const NginxInfo& info, const std::string& path,
                               unsigned short& out) {
    const Route* hit = nullptr;
    for (const Route& r : info.routes) if (r.path == path) { hit = &r; break; }
    if (!hit || hit->target != RouteTargetKind::Upstream_) return false;
    if (info.loopback_risk()) {
        for (const Upstream& u : info.upstreams)
            if (u.name == "https_artemis_remote") { out = u.port; return true; }
        return false;
    }
    for (const Upstream& u : info.upstreams)
        if (u.name == hit->upstream) { out = u.port; return true; }
    return false;
}

// 在 base 下查找含 conf/nginx.conf 的目录, 写入 out (nginx 根)。
static bool locate_nginx_conf(const fs::path& base, fs::path& out) {
    std::vector<fs::path> stack;
    std::error_code ec;
    stack.push_back(base);
    while (!stack.empty()) {
        fs::path dir = stack.back();
        stack.pop_back();
        for (const auto& e : fs::directory_iterator(dir, ec)) {
            if (ec) break;
            const fs::path p = e.path();
            std::error_code e2;
            if (!fs::is_directory(p, e2)) continue;
            if (fs::is_regular_file(p / L"conf" / L"nginx.conf", e2)) {
                out = p;
                return true;
            }
            stack.push_back(p);
        }
    }
    return false;
}

// ----------------------------------------------------------------------------
// 修复流程
// ----------------------------------------------------------------------------
static void print_wrapper_tail(const fs::path& comp_dir,
                               const std::wstring& svc_name) {
    fs::path wrapper = comp_dir / L"daemon" / (svc_name + L".wrapper.log");
    std::error_code ec;
    if (!fs::is_regular_file(wrapper, ec)) return;
    std::ifstream f(wrapper, std::ios::binary);
    if (!f) return;
    std::string content((std::istreambuf_iterator<char>(f)),
                        std::istreambuf_iterator<char>());
    std::vector<std::string> lines;
    size_t p = 0;
    while (p <= content.size()) {
        size_t nl = content.find('\n', p);
        std::string line = (nl == std::string::npos) ? content.substr(p)
                                                     : content.substr(p, nl - p);
        p = (nl == std::string::npos) ? content.size() + 1 : nl + 1;
        while (!line.empty() && line.back() == '\r') line.pop_back();
        lines.push_back(line);
    }
    if (lines.empty()) return;
    log(Level::Err, "--- wrapper.log 最后 10 行 ---");
    size_t start = lines.size() > 10 ? lines.size() - 10 : 0;
    for (size_t i = start; i < lines.size(); ++i) {
        if (!lines[i].empty()) {
            log(Level::Err, "  " + decode_to_utf8(lines[i]));
        }
    }
}

// 组件名 / 服务名 / 端口 (utf8 输出用)
static void repair_component(const char* name_utf8, const fs::path& root,
                             const fs::path& node_exe, bool reinstall,
                             bool check_only, unsigned int& issues) {
    fs::path comp_dir = root / L"bin" / name_utf8 / name_utf8;
    fs::path install_js = comp_dir / L"service.install.js";
    fs::path uninstall_js = comp_dir / L"service.uninstall.js";

    log(Level::Step, std::string("---- 组件 [") + name_utf8 + "] ----");
    std::error_code ec;
    if (!fs::is_regular_file(comp_dir / L"koa-app.js", ec)) {
        logf(Level::Err, "组件目录不存在或缺文件: %s",
             decode_to_utf8(comp_dir.native()).c_str());
        ++issues;
        return;
    }

    CompConfig cfg = read_config(comp_dir);
    std::wstring svc_name = cfg.service_name ? *cfg.service_name
                                             : ascii_value_to_wide(name_utf8);
    unsigned short port = cfg.port ? *cfg.port
                                   : (std::strcmp(name_utf8, "artemis-portal") == 0
                                          ? 9018
                                          : 9017);
    logf(Level::Info, "服务名=%s 端口=%u", decode_to_utf8(svc_name).c_str(),
         (unsigned)port);

    SvcState state = service_state(svc_name);

    // ---- 仅检查 ----
    if (check_only) {
        if (state == SvcState::Running) {
            bool up = port_listening(port);
            L2 hp = http_probe_against(port, std::string("/") + name_utf8, SS_WEB);
            bool http = (hp.kind == L2Kind::Ok);
            const char* st = (up && http) ? "端口监听, HTTP 正常"
                             : (up ? "端口监听, HTTP 异常" : "端口未监听");
            if (up && http) {
                logf(Level::Ok, "服务运行中, %s", st);
            } else {
                logf(Level::Err, "服务运行中, %s", st);
                ++issues;
            }
        } else {
            const char* st = (state == SvcState::Missing) ? "未安装" : "其他状态";
            logf(Level::Err, "服务未运行 (Status=%s)", st);
            ++issues;
        }
        return;
    }

    // ---- 修复 ----
    bool need_reinstall = reinstall || state == SvcState::Missing;

    if (!need_reinstall && state != SvcState::Running) {
        log(Level::Info, "服务存在但未运行, 先尝试直接启动...");
        start_service(svc_name);
        std::this_thread::sleep_for(std::chrono::seconds(5));
        state = service_state(svc_name);
        if (state == SvcState::Running && port_listening(port)) {
            logf(Level::Ok, "启动成功, 端口 %u 已监听。", (unsigned)port);
        } else {
            log(Level::Warn, "直接启动失败或端口未监听, 转入重装流程。");
            need_reinstall = true;
        }
    }

    if (need_reinstall) {
        if (state != SvcState::Missing) {
            logf(Level::Info, "卸载旧服务 %s ...", decode_to_utf8(svc_name).c_str());
            run_node_logged(node_exe, uninstall_js, nullptr, "uninstall");
            stop_service(svc_name);
            delete_service(svc_name);
            std::this_thread::sleep_for(std::chrono::seconds(3));
        }
        log(Level::Info, "重装服务 (重建 daemon 守护进程)...");
        run_node_logged(node_exe, install_js, &comp_dir, "install");

        // service.install.js 安装后通常会自动 start, 这里再兜底启动一次
        std::this_thread::sleep_for(std::chrono::seconds(5));
        SvcState st = service_state(svc_name);
        if (st != SvcState::Missing && st != SvcState::Running) {
            start_service(svc_name);
        }
    }

    // 等待端口监听 (最多 60 秒)
    bool ok = false;
    for (int i = 0; i < 12; ++i) {
        if (port_listening(port)) {
            ok = true;
            break;
        }
        std::this_thread::sleep_for(std::chrono::seconds(5));
    }
    L2 hp = http_probe_against(port, std::string("/") + name_utf8, SS_WEB);
    bool http = ok && (hp.kind == L2Kind::Ok);

    if (ok && http) {
        logf(Level::Ok,
             "[%s] 修复成功: 服务运行中, 端口 %u 监听, HTTP 响应正常。",
             name_utf8, (unsigned)port);
    } else if (ok) {
        logf(Level::Warn,
             "[%s] 端口 %u 已监听, 但 HTTP 检查未通过 (可能仍在初始化, 稍后刷新页面确认)。",
             name_utf8, (unsigned)port);
        ++issues;
    } else {
        logf(Level::Err,
             "[%s] 修复失败: 端口 %u 未监听。请查看 daemon\\wrapper.log 与 log.txt。",
             name_utf8, (unsigned)port);
        print_wrapper_tail(comp_dir, svc_name);
        ++issues;
    }
}

// ----------------------------------------------------------------------------
// 主流程
// ----------------------------------------------------------------------------
enum class Action { Repair = 1, CheckOnly = 2, Reinstall = 3 };

static int run_flow(Action action, const std::wstring& root_arg) {
    unsigned int issues = 0;
    bool check_only = action == Action::CheckOnly;
    bool reinstall = action == Action::Reinstall;

    // == 1. 定位安装目录 ==
    fs::path root = root_arg.empty() ? fs::path(DEFAULT_OPENAPI_ROOT)
                                     : fs::path(root_arg);
    std::error_code ec;
    if (!fs::is_directory(root / L"bin", ec)) {
        logf(Level::Err, "未找到 OpenAPI 目录: %s",
             decode_to_utf8(root.native()).c_str());
        fs::path alt;
        if (find_artemis_dir(fs::path(SEARCH_BASE), alt)) {
            root = alt;
            logf(Level::Warn, "自动定位到: %s",
                 decode_to_utf8(root.native()).c_str());
        } else {
            log(Level::Err, "本机未安装 iSecure VMS OpenAPI 组件, 退出。");
            return 1;
        }
    }
    logf(Level::Ok, "OpenAPI 根目录: %s", decode_to_utf8(root.native()).c_str());

    // == 2. 定位 node.exe ==
    fs::path node_dir;
    fs::path parent = root.parent_path();
    if (parent.empty()) node_dir = root / L".." / L"nodejs";
    else node_dir = parent / L"nodejs";

    fs::path node;
    if (!find_node_exe(node_dir, node)) {
        log(Level::Err, "未找到内置 node.exe (OpenAPI\\nodejs\\), Web 组件无法运行。");
        return 1;
    }
    logf(Level::Ok, "node.exe: %s", decode_to_utf8(node.native()).c_str());

    // == 3. 前置组件检查 (缺失仅告警) ==
    log(Level::Step, "---- 前置组件检查 ----");
    struct Prereq { unsigned short port; const char* desc; };
    static const Prereq PREREQ[] = {
        { 9016, "artemis 网关(Java)" },
        { 7019, "redis" },
        { 5432, "postgresql" },
        { 9000, "minio" },
        { 443, "nginx" },
    };
    for (const auto& pr : PREREQ) {
        if (port_listening(pr.port)) {
            logf(Level::Ok, "%s 端口 %u: 正常监听", pr.desc, (unsigned)pr.port);
        } else {
            logf(Level::Warn,
                 "%s 端口 %u: 未监听, web/portal 能启动但功能可能异常",
                 pr.desc, (unsigned)pr.port);
            ++issues;
        }
    }

    // == 4. 组件修复 ==
    repair_component("artemis-web", root, node, reinstall, check_only, issues);
    repair_component("artemis-portal", root, node, reinstall, check_only, issues);

    // == 5. 汇总 ==
    log(Level::Step, "==== 汇总 ====");
    if (check_only) {
        logf(Level::Info, "诊断完成 (未做任何修改)。发现问题数: %u", issues);
    } else {
        logf(Level::Info, "修复流程完成。异常项: %u", issues);
        log(Level::Info,
            "验证入口(本机): http://127.0.0.1:9017/artemis-web/  "
            "http://127.0.0.1:9018/artemis-portal/");
        log(Level::Info,
            "经 nginx 443 对外: https://<本机IP>/artemis-web/  "
            "https://<本机IP>/artemis-portal/");
    }
    return issues > 0 ? 1 : 0;
}

// 交互菜单: 返回 Action 值; -1 = 退出
static int choose_action_interactive(bool admin) {
    for (;;) {
        std::printf("\n");
        std::printf("============================================================\n");
        std::printf("  RepairArtemisWeb — iSecure VMS OpenAPI 组件修复工具\n");
        std::printf("============================================================\n");
        std::printf("  开发人: 余志强    QQ: 379008610    主页: https://github.com/xiaoyuzhi\n");
        if (!admin) {
            std::printf("  [!] 当前未以管理员身份运行: 选项 1 / 3 需要管理员权限\n");
            std::printf("      (可右键“以管理员身份运行”本程序)\n");
        }
        std::printf("  [1] 标准修复  自动修复未运行的 artemis-web / artemis-portal 服务\n");
        std::printf("  [2] 仅检查    只诊断不修改 (无需管理员权限)\n");
        std::printf("  [3] 强制重装  即使服务运行中, 也卸载重装 (需要管理员权限)\n");
        std::printf("  [0] 退出\n");
        std::printf("  请输入序号并回车 (直接回车默认 1): ");
        std::fflush(stdout);

        std::string line;
        if (!std::getline(std::cin, line)) {
            log(Level::Warn, "无法读取交互输入, 按标准修复执行。");
            return (int)Action::Repair;
        }
        // 只保留 ASCII 字母/数字用于匹配, 兼容 UTF-16/杂散控制符等管道输入
        std::string key;
        for (char c : line) {
            if ((c >= '0' && c <= '9') || (c >= 'a' && c <= 'z') ||
                (c >= 'A' && c <= 'Z')) key += c;
        }
        if (key.empty() || key == "1" || key == "s") return (int)Action::Repair;
        if (key == "2" || key == "c") return (int)Action::CheckOnly;
        if (key == "3" || key == "r") return (int)Action::Reinstall;
        if (key == "0" || key == "q") {
            std::printf("已退出。\n");
            std::fflush(stdout);
            return -1;
        }
        std::printf("  无效输入, 请重新选择。\n");
        std::fflush(stdout);
    }
}

static void usage() {
    std::printf("RepairArtemisWeb — iSecure VMS OpenAPI artemis-web / artemis-portal 服务修复工具\n");
    std::printf("\n");
    std::printf("用法:\n");
    std::printf("  RepairArtemisWeb.exe [选项]\n");
    std::printf("\n");
    std::printf("选项:\n");
    std::printf("  -c, --check-only   仅诊断, 不做任何修改 (无需管理员权限)\n");
    std::printf("  -r, --reinstall    强制卸载并重装服务 (即使服务正在运行)\n");
    std::printf("  --root <目录>      指定 OpenAPI 根目录\n");
    std::printf("                     默认: %s\n", DEFAULT_OPENAPI_ROOT);
    std::printf("  -h, --help         显示此帮助\n");
    std::printf("\n");
    std::printf("不带任何参数运行会进入中文交互式菜单。\n");
    std::printf("\n");
    std::printf("开发人: 余志强    QQ: 379008610    主页: https://github.com/xiaoyuzhi\n");
    std::printf("版权: Copyright (c) 2026 余志强 (Yu Zhiqiang). All rights reserved.\n");
}

static std::wstring lower_ascii(const std::wstring& s) {
    std::wstring r = s;
    for (wchar_t& c : r)
        if (c >= L'A' && c <= L'Z') c = c - L'A' + L'a';
    return r;
}

int wmain(int argc, wchar_t** argv) {
    enable_vt();

    std::vector<std::wstring> args;
    for (int i = 1; i < argc; ++i) args.push_back(argv[i]);

    bool check_only = false;
    bool reinstall = false;
    std::wstring root_arg;
    bool has_cli = false;

    for (size_t i = 0; i < args.size(); ++i) {
        std::wstring a = lower_ascii(args[i]);
        if (a == L"-c" || a == L"--check-only" || a == L"/c" ||
            a == L"check-only") {
            check_only = true;
            has_cli = true;
        } else if (a == L"-r" || a == L"--reinstall" || a == L"/r") {
            reinstall = true;
            has_cli = true;
        } else if (a == L"--root" || a == L"-root" || a == L"--openapi-root" ||
                   a == L"/root") {
            if (i + 1 >= args.size()) {
                logf(Level::Err, "错误: %s 需要目录参数。",
                     decode_to_utf8(args[i]).c_str());
                usage();
                return 2;
            }
            root_arg = args[i + 1];
            ++i;
            has_cli = true;
        } else if (a == L"-h" || a == L"--help" || a == L"/?" || a == L"help") {
            usage();
            return 0;
        } else {
            logf(Level::Err, "错误: 未知参数 \"%s\"。",
                 decode_to_utf8(args[i]).c_str());
            usage();
            return 2;
        }
    }

    bool admin = is_admin();

    // 有命令行参数 -> 命令行模式; 否则进入交互菜单。
    Action action;
    if (has_cli) {
        if (check_only) action = Action::CheckOnly;
        else if (reinstall) action = Action::Reinstall;
        else action = Action::Repair;
    } else {
        log(Level::Step,
            "==== iSecure VMS OpenAPI artemis-web/portal 修复工具 ====");
        int sel = choose_action_interactive(admin);
        if (sel < 0) {  // 菜单选择"0 退出"
            pause_if_console();
            return 0;
        }
        action = (Action)sel;
    }

    if (!admin && action != Action::CheckOnly) {
        log(Level::Err,
            "本工具需要管理员权限(注册/启动 Windows 服务), 请以管理员身份运行。");
        pause_if_console();
        return 2;
    }

    int code = run_flow(action, root_arg);
    pause_if_console();
    return code;
}
