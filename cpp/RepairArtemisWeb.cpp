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
#include <optional>
#include <string>
#include <thread>
#include <vector>

namespace fs = std::filesystem;

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
static bool port_netstat(unsigned short port) {
    RunRes r = run_output(L"netstat", { L"-ano" }, nullptr);
    if (!r.launched) return false;
    std::string out = r.out;
    size_t pos = 0;
    while (pos <= out.size()) {
        size_t nl = out.find('\n', pos);
        std::string line = (nl == std::string::npos) ? out.substr(pos)
                                                     : out.substr(pos, nl - pos);
        pos = (nl == std::string::npos) ? out.size() + 1 : nl + 1;

        std::vector<std::string> toks;
        size_t p = 0;
        while (p < line.size()) {
            while (p < line.size() && (line[p] == ' ' || line[p] == '\t' || line[p] == '\r')) ++p;
            size_t q = p;
            while (q < line.size() && line[q] != ' ' && line[q] != '\t' && line[q] != '\r') ++q;
            if (q > p) toks.push_back(line.substr(p, q - p));
            p = q;
        }
        if (toks.size() < 2) continue;

        std::string local = toks[1];
        size_t ci = local.rfind(':');
        if (ci == std::string::npos) continue;
        std::string portstr = local.substr(ci + 1);
        while (!portstr.empty() && portstr.back() == ']') portstr.pop_back();
        if (portstr.empty()) continue;
        unsigned long pnum = 0;
        bool isdig = true;
        for (char c : portstr) if (c < '0' || c > '9') { isdig = false; break; }
        if (!isdig) continue;
        pnum = std::strtoul(portstr.c_str(), nullptr, 10);
        if (pnum != port) continue;

        bool listening = false;
        for (size_t k = 2; k < toks.size(); ++k) {
            if (toks[k].find("LISTENING") != std::string::npos ||
                toks[k].find("LISTEN") != std::string::npos) {
                listening = true;
                break;
            }
        }
        if (listening) return true;
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

// 端口监听判定: netstat 优先, TCP 连通测试兜底
static bool port_listening(unsigned short port) {
    if (port_netstat(port)) return true;
    return tcp_probe(port, 1000);
}

// HTTP 健康检查: 只要求有 HTTP 响应 (200/302/404 均算服务已起)
static bool http_up(unsigned short port, const std::string& pathname) {
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

    int r = connect(s, (sockaddr*)&a, sizeof a);
    bool connected = false;
    if (r == 0) {
        connected = true;
    } else if (WSAGetLastError() == WSAEWOULDBLOCK) {
        fd_set wf;
        FD_ZERO(&wf);
        FD_SET(s, &wf);
        timeval tv{ 8, 0 };
        int sel = select(0, nullptr, &wf, nullptr, &tv);
        if (sel == 1) {
            int err = 0;
            int len = sizeof err;
            getsockopt(s, SOL_SOCKET, SO_ERROR, (char*)&err, &len);
            connected = (err == 0);
        }
    }
    if (!connected) {
        closesocket(s);
        return false;
    }
    // 恢复阻塞, 设置读写超时
    nb = 0;
    ioctlsocket(s, FIONBIO, &nb);
    DWORD tmo = 8000;
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, (const char*)&tmo, sizeof tmo);
    setsockopt(s, SOL_SOCKET, SO_SNDTIMEO, (const char*)&tmo, sizeof tmo);

    char req[512];
    std::snprintf(req, sizeof req,
                  "GET /%s/ HTTP/1.1\r\nHost: 127.0.0.1:%u\r\n"
                  "User-Agent: RepairArtemisWeb\r\nConnection: close\r\n\r\n",
                  pathname.c_str(), (unsigned)port);
    int sr = send(s, req, (int)std::strlen(req), 0);
    if (sr <= 0) {
        closesocket(s);
        return false;
    }
    char buf[512];
    int nr = recv(s, buf, sizeof buf, 0);
    closesocket(s);
    if (nr <= 0) return false;
    std::string resp(buf, nr);
    return resp.find("HTTP") != std::string::npos;
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
            bool http = http_up(port, name_utf8);
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
    bool http = ok && http_up(port, name_utf8);

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
